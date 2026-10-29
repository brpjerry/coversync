# coversync

A music library manager and tag editor that runs as a small daemon and serves
its UI to the browser. It indexes an **Inbox** of unmanaged music and imports
albums into a **Library** it manages: copied (or moved) into a folder structure
you define, held to rules about tags and album art, with tags filled in or
corrected on the way. Syncing the Library to devices is planned.

Early days: single user, no authentication, used on a home server.

## Features

- **Browse**: Inbox and Library album grids with cover art, sidebar sort tabs,
  search, a player, and a tag editor. "Needs attention" lists albums missing
  tags or art the rules require.
- **Library setup**: required tags; album art rules (a cover file, embedded
  art or both, required or not, and the cover file's name); and the folder
  structure, a template such as
  `{album_artist}/{year} - {album}/{track_number} - {title}`.
  `{track_number}` is zero-padded and the extension is added. `{album:40}`
  keeps at most 40 characters of a value, cut at a word break when one is
  close. Tags a template uses count as required on import.
- **Import** (right-click an Inbox album): checks the rules and the template's
  tags, opens an editor for anything missing, then copies (or moves) the files
  into place with the new tags and art. In Copy mode the original is never
  written and is hidden from the Inbox afterwards.
- **Inbox setup**: Copy or Move, and autofill for empty tags that are safe to
  assume (album artist from artist, track numbers from file names, …).
- **Smart suggestions** (optional): the import dialog looks the album up on
  MusicBrainz, by Chromaprint fingerprints on AcoustID where it can and by
  title and artist otherwise, and offers MusicBrainz's tags to tick: values
  for empty tags and corrections for differing ones. **Prefer English**
  suggests romanized or English titles and artist names for releases in other
  scripts, and **Reverse first and last names** writes Chinese, Japanese and
  Korean names family name first.
- **Appearance**: which sort tabs the sidebar shows, and the cover size.

```text
crates/coversync-core   scanning (lofty), SQLite index, covers, tags, import, MusicBrainz
crates/coversyncd       axum HTTP API, with the web UI embedded in the binary
web/                    SvelteKit / Svelte 5 single-page app
tools/ui_smoke.py       headless Firefox (geckodriver) smoke test
packaging/arch/         PKGBUILD and systemd user unit for the pacman package
```

## Install on Arch Linux

Each [release](https://github.com/brpjerry/coversync/releases) has a pacman
package. Install it and start the daemon as a user service:

```sh
sudo pacman -U coversync-0.1.0-1-x86_64.pkg.tar.zst
sudo pacman -S --asdeps chromaprint                # optional: fpcalc for Smart suggestions
systemctl --user enable --now coversyncd
```

The service listens on <http://127.0.0.1:8770/>; choose the Inbox folder in
Inbox setup. To build the package from a checkout instead, run
`packaging/arch/build.sh` (it packages the current commit into `dist/`).

## Build and run

You need stable Rust and Node with pnpm. Smart suggestions also use `fpcalc`
(Chromaprint) for fingerprints and a free AcoustID application key from
<https://acoustid.org/new-application>; without them, albums are matched by
title and artist.

```sh
(cd web && pnpm install && pnpm build)   # the daemon embeds web/build
cargo build --release -p coversyncd
target/release/coversyncd --inbox ~/Music
```

Then open <http://127.0.0.1:8770/>. `coversyncd --help` lists the options:

- `--inbox` seeds the Inbox folder setting the first time; Inbox setup owns it
  afterwards.
- `--listen` (default `127.0.0.1:8770`). There is no authentication, so keep
  it on localhost or behind a reverse proxy that adds some.
- `--state-dir` and `--cache-dir`: the index database (default
  `$XDG_STATE_HOME/coversync`) and cover thumbnails (`$XDG_CACHE_HOME/coversync`).
  The AcoustID key is stored in the index database and never sent to the
  browser.
- `--allow-writes` lets the tag editor change Inbox files in place; it is off
  by default. Library files can always be edited. Writes go to a copy beside
  the file that is renamed into place, and every edit is validated before any
  file is touched.

To serve the UI under a path prefix behind a reverse proxy, build it with that
prefix: `(cd web && BASE_PATH=/coversync pnpm build)`.

### As a systemd user service

The Arch package installs this unit, with `/usr/bin/coversyncd`.

```ini
[Unit]
Description=coversync music library daemon
After=network-online.target

[Service]
ExecStart=%h/.local/bin/coversyncd --listen 127.0.0.1:8770
Restart=on-failure
StateDirectory=coversync
CacheDirectory=coversync
# Optional: a folder coversync must never change, such as an Inbox that
# another program syncs. Imports from it can then only copy.
#ReadOnlyPaths=/path/to/inbox
ProtectSystem=full
PrivateTmp=yes
NoNewPrivileges=yes

[Install]
WantedBy=default.target
```

## Develop

```sh
(cd web && pnpm dev)        # Vite on :5173, proxying /api to 127.0.0.1:8770
cargo test --workspace && cargo clippy --workspace --all-targets
(cd web && pnpm check)
tools/ui_smoke.py http://127.0.0.1:8770/ shots/     # read-only tour of the UI
```

The smoke test's other modes (`--save`, `--import`, `--suggest`, …, listed at
the top of the script) change settings or import albums: run them only against
a daemon with a scratch `--state-dir` and Library folder.

For every pull request and push to `main`, CI (`.github/workflows/ci.yml`) runs
`pnpm lint`, `pnpm check`, rustfmt, Clippy with warnings as errors and the
tests, and builds the pacman package.

### Release

1. Set the new version in `crates/coversyncd/Cargo.toml` and as `pkgver` in
   `packaging/arch/PKGBUILD` (with `pkgrel=1`), and merge that to `main`.
2. Tag that commit on `main` and push the tag:
   `git tag v0.2.0 && git push origin v0.2.0`.

The Release workflow checks that the tag matches the version, runs CI, and
publishes a GitHub release with the package and its SHA-256 checksum.
