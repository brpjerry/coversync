# coversync

A music library manager and tag editor that runs as a small daemon and serves
its UI to the browser. It indexes an **Inbox** of unmanaged music and imports
albums into a **Library** it manages: copied (or moved) into a folder structure
you define, held to rules about tags and album art, with tags filled in or
corrected on the way. Syncing the Library to devices is planned.

Early days: a single user with a password (see [Signing in](#signing-in)),
used on a home server.

## Features

- **Browse**: Inbox and Library album grids with cover art, a sort menu in the
  top bar, search, a player, and a tag editor. "Recently added" means newest
  files first in the Inbox, and most recently imported first in the Library
  (the import time is kept in the Library's database, so tag edits and moves
  don't change it). "Needs attention" lists albums missing tags or art the
  rules require. The sidebar filters albums by tag:
  pick a tag such as Genre from its dropdown, search its values, and tick the
  ones to show. Ticked values of one tag widen the choice, values of several
  tags narrow it, and tags holding several values (split by Library setup's
  separator) offer each.
- **Tag editor** (an album's Edit tags): album-wide fields and each track's.
  A save is refused, and nothing written, if the album changed after the
  editor loaded it: saved from another browser or app, or changed on disk
  and found by a scan, tracks added or removed included. The album then
  says it was changed elsewhere, as it does when a change arrives while you
  edit (see Live updates), and shows it as it is now, with the fields that
  changed marked and your unsaved edits on top: save again to apply them,
  or Reload to drop them. API clients get the same check by sending back
  the album's `version`, and its tracks' ids and paths, with a tag save
  (`PUT /api/albums/{id}/tags`), and a 409 with the album as it is now when
  it no longer matches; a save without them is written as before. The
  `album_changed` notice carries the album's new `version`.
- **Library setup**: required tags; album art rules (a cover file, embedded
  art or both, required or not, and the cover file's name); and the folder
  structure, a template such as
  `{album_artist}/{year} - {album}/{track_number} - {title}`.
  `{track_number}` is zero-padded and the extension is added. `{album:40}`
  keeps at most 40 characters of a value, cut at a word break when one is
  close. Tags a template uses count as required on import. The rules and the
  Library's index live in the Library folder itself (see
  [Moving the Library](#moving-the-library)).
- **Import** (right-click an Inbox album): checks the rules and the template's
  tags, opens an editor for anything missing, then copies (or moves) the files
  into place with the new tags and art. In Copy mode the original is never
  written and is hidden from the Inbox afterwards.
- **Inbox setup**: Copy or Move, autofill for empty tags that are safe to
  assume (album artist from artist, track numbers from file names, …), and
  the uploads folder (see [Uploading albums](#uploading-albums)).
- **Uploads**: albums sent to the server through the upload API land in the
  uploads folder and show up in the Inbox. Uploading from the browser and
  the desktop app is planned; `tools/upload.py` uploads a folder for now.
- **Smart suggestions** (optional): the import dialog looks the album up on
  MusicBrainz, by Chromaprint fingerprints on AcoustID where it can, and
  otherwise by the barcode (or UPC) and ISRCs its files are tagged with, or
  its title and artist. An artist MusicBrainz credits under another name
  (汐れいら for "Ushio Reira") is found by its sort name or aliases. It
  offers MusicBrainz's tags to tick: values for empty tags and corrections
  for differing ones. **Prefer English**
  suggests romanized or English titles and artist names for releases in other
  scripts, and **Reverse first and last names** writes Chinese, Japanese and
  Korean names family name first. AcoustID's and MusicBrainz's answers are
  saved, so an album identified before (one imported again, say) is only
  fingerprinted; the dialog says when the lookup was made, and **Look up
  again** asks both services afresh. Answers that found nothing aren't
  saved, so those are asked again each time. With **Save lookups** off,
  every album is looked up afresh, and no answer is read or saved.
- **Appearance**: which sorts the sort menu offers, which tags the sidebar
  filters by, and the cover size.
- **Sync** (planned): devices pair with the server from Sync setup, each with
  a token of its own (see [Pairing devices](#pairing-devices)), and the
  sidebar's Sync section lists them. Copying the Library to them comes later.
  Sync setup can turn the section off.
- **Live updates**: tag edits, imports and saved setups made in one browser,
  and scans, show in every other open browser as they happen, without
  reloading. They come from the daemon's event stream, `/api/events`; while
  it's down (the daemon restarting, say), the page polls instead, and
  catches up once the stream is back. The stream needs a login like the rest
  of the API, and ends within 20 seconds of its session ending (signing out,
  a password reset); signed out, the page waits, and opens it again once
  signed back in. Tag edits in progress are kept: if the
  album changes elsewhere meanwhile, the album says so and offers to reload,
  and saving puts the edits on top of the change (see Tag editor).
  Each tab showing coversync holds a connection open for the stream (one
  hidden for a few seconds lets it go). Over plain HTTP/1.1, as at
  <http://127.0.0.1:8770/>, browsers allow six connections to a server
  across all tabs, so several tabs side by side can stall each other; a
  reverse proxy serving HTTPS (HTTP/2) doesn't have that limit.

```text
crates/coversync-core   scanning (lofty), SQLite index, covers, tags, import, MusicBrainz
crates/coversyncd       axum HTTP API, with the web UI embedded in the binary
web/                    SvelteKit / Svelte 5 single-page app
tools/ui_smoke.py       headless Firefox (geckodriver) smoke test
tools/upload.py         uploads an album folder through the upload API
packaging/arch/         PKGBUILD and systemd user unit for the pacman package
packaging/notices/      third-party notices for packages: Rust crates and the UI's npm packages
```

## Install on Arch Linux

Each [release](https://github.com/brpjerry/coversync/releases) has a pacman
package. Install it and start the daemon as a user service:

```sh
sudo pacman -U coversync-0.1.0-1-x86_64.pkg.tar.zst
sudo pacman -S --asdeps chromaprint                # optional: fpcalc for Smart suggestions
systemctl --user enable --now coversyncd
coversyncd set-password                            # the password to sign in with
```

The service listens on <http://127.0.0.1:8770/>; sign in, then choose the
Inbox folder under Setup > Inbox (in the sidebar, under the tag filter). To build the
package from a checkout instead, run `packaging/arch/build.sh` (it packages
the current commit into `dist/`).

## Build and run

You need stable Rust and Node with pnpm. Smart suggestions also use `fpcalc`
(Chromaprint) for fingerprints and a free AcoustID application key from
<https://acoustid.org/new-application>; without them, albums are matched by
barcode, ISRCs, or title and artist.

```sh
(cd web && pnpm install && pnpm build)   # the daemon embeds web/build
cargo build --release -p coversyncd
target/release/coversyncd set-password
target/release/coversyncd --inbox ~/Music
```

Then open <http://127.0.0.1:8770/> and sign in. `coversyncd --help` lists the
options:

- `--inbox` seeds the Inbox folder setting the first time; Inbox setup owns it
  afterwards.
- `--listen` (default `127.0.0.1:8770`). coversyncd itself only speaks plain
  HTTP, so to reach it from other machines put a reverse proxy that adds HTTPS
  in front (see [Signing in](#signing-in)); otherwise the password and the
  session cookie cross the network unencrypted.
- `--state-dir` and `--cache-dir`: the local database, `inbox.sqlite3`
  (default `$XDG_STATE_HOME/coversync`), and cover thumbnails, MusicBrainz's
  cover images and the saved AcoustID and MusicBrainz lookups,
  `lookups.sqlite3` (`$XDG_CACHE_HOME/coversync`). The local database holds
  the Inbox's index, the Inbox files already imported, and this
  installation's settings: Inbox setup, Appearance, Sync setup, the Library
  folder's location and the AcoustID key, which is never sent to the browser. A database from before
  the Library had its own, `library.sqlite3`, is renamed on first start;
  rename it back before going back to an older release.
  Everything in the cache directory is rebuilt when missing.
- `--allow-writes` lets the tag editor change Inbox files in place; it is off
  by default. Library files can always be edited. Writes go to a copy beside
  the file that is renamed into place, and every edit is validated before any
  file is touched.

To serve the UI under a path prefix behind a reverse proxy, build it with that
prefix: `(cd web && BASE_PATH=/coversync pnpm build)`. The prefix is written
as it appears in the URL: ASCII, with anything else percent-encoded
(`/m%C3%BCsik`, not `/müsik`). The session cookie is then limited to that
prefix, so other sites on the same host never get it. The proxy has to pass
`/api/events` on as it comes rather than buffer it: Caddy does by itself, and
the stream asks nginx to with `X-Accel-Buffering: no`.

### Signing in

coversync has one user, with a password set on the server itself; there is
no way to set or reset it from the web.

```sh
coversyncd set-password                       # prompts twice, without echo
printf '%s\n' "$PASSWORD" | coversyncd set-password   # or one line on stdin
coversyncd set-password --state-dir /path/to/state    # a service with its own
```

It writes the password's argon2id hash to the local database in the state
directory, so give it the same `--state-dir` as the service if that has one.
It works whether the service runs or not: the service reads the hash at each
login. Running it again resets the password and signs out every browser
signed in with the old one; apps keep their tokens.

**Until a password is set, nothing but the login page can be opened.** The UI
says to run `coversyncd set-password`, and the API answers 401 with the code
`no_password`. Upgrading from a release without login therefore locks the UI
until the command is run on the server.

- **Browsers** sign in on the login page and get a session cookie,
  `coversync_session`: HttpOnly, SameSite=Lax, and Secure when the proxy
  reports HTTPS (`X-Forwarded-Proto: https`). It is limited to the UI's path,
  and the session ends after 90 days without use, or with **Sign out** in the
  sidebar. Covers and audio streams work with the cookie alone.
- **Apps** send a token, `Authorization: Bearer cst_…`, which pairing issues
  (see [Pairing devices](#pairing-devices)), one per device, each with a
  scope: `manage` (everything a browser can do, except managing devices) or
  `sync` (syncing only; refused with 403 for now, but for `GET /api/session`).
  `POST /api/login/token` trades a token for a session cookie, for an app's
  web view; that session has the device's scope and isn't a password login.
- **Everything under `/api` needs one or the other**, except `GET /api/hello`
  (the server's name, coversyncd's version and the API versions it speaks, for
  clients checking a URL), `GET /api/hello/ca` (the CA certificate for apps),
  logging in and out, and `POST /api/pair`. The UI's own files stay open:
  they are the app's source and hold no data.
- **Failed logins are limited.** Each client gets five tries, then one a
  minute; a client with a try left is always checked, so the right password
  always gets in. Overall, twenty failures are allowed, then one every six
  seconds; past that (an attack from many addresses), each attempt costs its
  client every try it has left (the right password gets one back), so each
  address gets one guess a minute, even sending its tries all at once.
  At most two passwords are checked at once. Behind a reverse proxy on the
  same machine, the client's address is taken from `X-Forwarded-For`, which
  is only trusted from loopback; loopback itself gets no other trust, since
  every request through the proxy comes from it.
- **Nothing a login is served is kept by shared caches**: API responses are
  `Cache-Control: private`, and `no-cache` unless they set their own policy.

### Uploading albums

Albums can be uploaded into the Inbox over the API, in chunks that survive a
dropped connection or a restart on either side. Each upload is one new folder
in the **uploads folder**: `Uploads` in the Inbox folder, unless Inbox setup
names another. Choose one outside the Inbox folder when coversyncd can't
write to the Inbox (a synced folder made read-only, say): it is scanned as
part of the Inbox, and its albums show up there like any other. The first
upload makes the uploads folder if the folder holding it is there. Uploads
only add files, never change any, so they need no `--allow-writes`; they
need a login, like the rest of the API.

Albums uploaded into a folder that is no longer the uploads folder (after
it's changed in Inbox setup) stay on disk but leave the Inbox at the next
scan: move them into the new uploads folder, or import them before
changing it.

1. `POST /api/uploads` with `{"folder": "Album", "files": [{"path":
   "CD1/01 Intro.flac", "size": 12345}, …]}` starts one and answers `201`
   with its `id`. Paths are relative, with `/` between folders, and no
   `.`, `..`, empty or hidden names. Only audio coversync reads, images, cue
   sheets, rip logs and similar files are taken (`GET /api/uploads` lists the
   extensions), at most 10,000 files. Two paths the disk takes for one
   (`a.flac` and `A.flac` where case doesn't count) are refused. The upload
   must fit on the disk, beside what the uploads already under way still
   need: if not, `507`. An uploads folder coversyncd can't write to gives
   `403`, saying so.
2. `PUT /api/uploads/{id}/files/{index}?offset=N` with the bytes as the body
   writes them into file `index` (counting from 0, in the order the files
   were listed) from byte `N`. Chunks
   can be any size (16 MiB is suggested), in any order, and several at once;
   sending one again is harmless.
3. `GET /api/uploads/{id}` gives each file's `ranges`, the byte ranges the
   server has as `[start, end)` pairs, so a client that lost track sends
   only the rest. A chunk cut short keeps the bytes that arrived.
4. `POST /api/uploads/{id}/finish` moves the folder into the uploads folder
   and starts a scan, so the album appears in the Inbox. If the folder's
   name is taken, it gets a number (`Album (2)`); nothing is ever
   overwritten. While bytes are missing it answers `409`, naming a file.
   Finishing carries on if the client hangs up, and asking again, even
   while it runs, gives the same answer, for a day (until coversyncd
   restarts).
   `DELETE /api/uploads/{id}` cancels.

Until it's finished, an upload is kept in `.coversync-uploads` in the uploads
folder, which the scanner skips. Chunks are written straight into the files
there at their offsets, and finishing is a rename, so nothing is copied
twice. Each file's name is flushed to disk when the upload starts, and its
contents once it's complete; finishing flushes what's left, then the move.
Progress is saved every few seconds. After a reboot, files not flushed yet
start over, as what was written to them may have been lost; so does a file
that's missing or shorter than its progress says. An upload no chunk reaches
for a day is removed, as are uploads in progress when the uploads folder
changes.

A reverse proxy in front must let request bodies of the chunk size through
and stream them. Caddy sets no body limit and streams by default; nginx's
`client_max_body_size` is 1 MB unless raised.

### Pairing devices

Each app or device gets its own token, which can be revoked without touching
the others, and only the browser ever sees the password.

1. In Sync setup, **Add device**: its name, and what it may do (**Manage and
   sync** for the desktop app or a phone, **Sync only** for a music player).
   The dialog shows a code such as `K7QM-4ZTD` and a QR code holding
   `coversync://pair?server=<url>&code=<code>[&ca=<sha256>]`, where the server
   URL is the page's own address (with its path prefix). The code works once,
   for 10 minutes, and closing the dialog cancels it.
2. The device checks the URL with `GET /api/hello`, then trades the code for
   its token: `POST /api/pair` with `{"code", "platform", "app_version",
   "hostname"}` (all but the code optional). The code may be typed in any
   case, with or without its hyphen. The answer holds `device_id`, `token`,
   `name`, `scope`, `server` (its name and version) and `library_id`. The
   dialog then shows the device, with **Not you? Revoke**.

- **Replace an existing device** (after reinstalling the app or rebuilding a
  computer) gives that device's entry the new token: its id, and the
  settings that will hang off it, stay; its old token, and the cookies
  traded for it, stop working. Removing the device meanwhile cancels the
  code. A removed device's id is never given to another.
- Sync setup lists the devices, with when and where each was last seen and
  its app version (the one it paired with, or the one an app sends in a
  `Coversync-App-Version` header). They can be renamed, re-scoped and
  revoked; a revoked token gets 401 at once, and a new scope applies at once
  to the token and the cookies traded for it. A revoked device stays listed,
  so pairing again can replace it, until it is removed.
- **Only a password login may add, change or revoke devices**, or change the
  pairing setup below. An app's token, or a cookie traded for one, can list
  them but gets 403 `password_required` for the rest, so a lost phone can't
  add devices or lock out the others.
- **Wrong codes are limited** like logins (five tries per client, then one a
  minute, counted apart from logins), and after 10 attempts that paired
  nothing, every waiting code is cancelled.
- **Tokens never cross the network in plain text.** `/api/pair` only answers
  requests that reached the reverse proxy on this machine over HTTPS
  (`X-Forwarded-Proto: https`, believed from loopback only), or that come
  straight from this machine with no proxy in between. A request a proxy
  forwarded over plain http is refused whatever client it names, so the
  proxy must send `X-Forwarded-Proto` (Caddy does; the last value is the one
  believed, as a proxy may add its own after a client's). Don't put a
  TCP-level forwarder on this machine in front of coversyncd (socat,
  `ssh -R`, some container port forwarders): it adds no headers, so its LAN
  clients look like this machine and would pair over plain http. Anything
  refused gets 403 `https_required`, and every code in it is cancelled, as
  it may have been seen on the way. Sync setup can allow plain http for a
  network that is encrypted already (Tailscale, WireGuard).
- **CA certificate for apps.** For a server whose certificate a CA of your own
  signed (mkcert's, say), set Sync setup's CA certificate to the path of
  that CA's public certificate, such as `~/.local/share/mkcert/rootCA.pem`.
  coversyncd serves it at `GET /api/hello/ca`, and new codes put its SHA-256
  in the QR code (`ca=`, the fingerprint `openssl x509 -noout -fingerprint
  -sha256` prints, as lowercase hex), so the Android app can check the
  certificate and trust it for this server alone. A file holding a private
  key, such as `rootCA-key.pem` beside it, is refused, as is anything but
  one PEM certificate. Leave it empty for a publicly trusted certificate;
  desktop apps use the system's certificates either way.

### Moving the Library

The Library keeps its own database in its folder,
`.coversync/library.sqlite3`: Library setup's rules, folder structure and
separator, and the index of its files, with paths relative to the folder. So
the folder moves as a whole, to another path or another machine, without the
Inbox:

1. Stop coversyncd, then copy the Library folder, keeping modification times
   (`rsync -a`), so its files don't need reading again.
2. On the other side, choose that folder in Library setup. coversync sees the
   Library it holds, and saving opens it with its own rules in place of the
   ones on screen.

The database is created by the first import, along with the folder if
needed. Until then the rules wait in the local database, and a folder that
already exists is indexed in memory, so choosing one by mistake leaves
nothing in it. A folder coversyncd can't write to is indexed the same way,
and Library setup says imports are off. Rules saved while the folder is away
(an unmounted disk, say) replace the Library's own when it comes back.

Stopping coversyncd before copying makes sure no import or scan is writing
to the database; it keeps a rollback journal rather than a write-ahead log,
so the folder alone holds every change. One coversyncd at a time should use
a Library folder, and anything that syncs the folder elsewhere (a backup, a
sync tool) can copy `.coversync` along with it.

The database also gives the Library a random id and a change counter, which
`/api/status` reports. The counter goes up once for each batch of changes
coversyncd commits: an import, a tag edit, or a scan that finds files added,
changed or removed (a large scan or import commits in several batches). The
tracks each batch wrote, and the files it removed, carry the new value. There
is no endpoint yet to ask for the changes since a given value; that comes
with sync. If a Library's counter is ever lower than the highest one this
installation has seen it reach, as when its folder is restored from an older
backup, coversyncd gives it a new id, so clients that knew the later state
start over rather than trust it. A Library moved to another installation
keeps its id. An older coversyncd still opens a database a newer one has
upgraded, and the changes it makes are counted too.

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
# another program syncs. Imports from it can then only copy, and uploads
# need an uploads folder outside it.
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
COVERSYNC_PASSWORD=… tools/ui_smoke.py http://127.0.0.1:8770/ shots/   # read-only tour of the UI
COVERSYNC_PASSWORD=… tools/upload.py http://127.0.0.1:8770/ ~/Music/Album   # upload a folder
```

A scratch daemon gets a password with
`coversyncd set-password --state-dir <scratch state dir>`. The smoke test
signs in with `COVERSYNC_PASSWORD` (or `--password`); `--auth` tries the login
page itself. Its other modes (`--save`, `--devices`, `--import`, `--suggest`,
…, listed at the top of the script) change settings, pair devices or import
albums: run them only against a daemon with a scratch `--state-dir` and
Library folder.

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

## License

Copyright (C) 2026 Jerry Shen

coversync is free software: you can redistribute it and/or modify it under the
terms of the GNU Affero General Public License as published by the Free
Software Foundation, either version 3 of the License, or (at your option) any
later version.

coversync is distributed in the hope that it will be useful, but WITHOUT ANY
WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A
PARTICULAR PURPOSE. See the GNU Affero General Public License in
[LICENSE](LICENSE) for more details.
