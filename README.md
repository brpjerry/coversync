# coversync

A music library manager and tag editor that runs as a small daemon and serves
its UI to the browser. It indexes an **Inbox** of unmanaged music and imports
albums into a **Library** it manages: copied (or moved) into a folder structure
you define, held to rules about tags and album art, with tags filled in or
corrected on the way. The server's side of syncing the Library to devices is
in place; the device apps come next.

Early days: a single user with a password (see [Signing in](#signing-in)),
used on a home server.

## Features

- **Browse**: Inbox and Library album grids with cover art, a sort menu in the
  top bar, search, a player, and a tag editor. "Recently added" means newest
  files first in the Inbox, and most recently imported first in the Library
  (the import time is kept in the Library's database, so tag edits and moves
  don't change it), or, with Inbox setup's Preserve modification date, in
  the Inbox's order. "Needs attention" lists albums missing tags or art the
  rules require, and albums **Incomplete** by their own numbering: track
  numbers below the last one (or up to the track total) that no track has,
  and discs below the last one there that no track is on. Discs past the
  last one aren't counted missing, since a disc total often counts a DVD or
  Blu-ray, and an album with an unnumbered track isn't judged (it could be
  any of the gaps). A total for the whole album on every disc counts at
  least the tracks it lacks even when the tags don't say which disc they're
  on. The album's page shows the missing numbers in its tracklist (between
  discs, when the numbering runs straight through them and the tags don't
  say which disc a gap is on). This needs no
  MusicBrainz; Smart suggestions (below) list what a release has that the
  album doesn't. The sidebar filters albums by tag:
  pick a tag such as Genre from its dropdown, search its values, and tick the
  ones to show. Ticked values of one tag widen the choice, values of several
  tags narrow it, and tags holding several values (split by Library setup's
  separator, and names at its phrases, see below) offer each.
- **Tag editor** (a Library album's Edit tags, or an Inbox album's Import
  without Smart suggestions; see Import): album-wide fields (album, album
  artist, date, genre) and each track's (number, disc, title, artist) up
  front (and on a Library album, Collection; see Collections), and
  **Show all tags** for every other tag the album has, grouped
  as in Library setup, with **Add a tag** for one it doesn't. An album-wide
  field shows the value every track shares; where they differ it says so,
  typing there sets every track, and **Per track** edits each track's own.
  Date shows the tag as written (`2010.08.25` stays as it is unless edited).
  Several values of one tag show joined by Library setup's separator. Listed
  read-only, and kept as they are by saves: tags coversync has no name for
  (`TITLE_JP`, `CDTOC`…), ones held as data or with a description (an
  iTunes `COMM:iTunNORM`), and the file's other tag blocks (ID3v2 in a FLAC,
  `RIFF INFO · IXYZ` beside a WAV's ID3v2). `TRACKNUM`, an old name for the
  track number, is dropped on saving a file that has a track number. An edit
  goes under the name the file has the tag under; one the format would drop
  (an MP4 compilation flag of "yes"), or a tag one file of the album can't
  hold, refuses the save before anything is written.
  `GET /api/albums/{id}/tags` gives each track's values by tag kind and
  those other tags; a save can send any tag kind per track (`kinds`).
  In the Library, saving puts every file of the album where Library
  setup's folder structure says with its tags as saved, edited or not: a
  new title renames a file, a new album title (or a tag the structure
  names a folder by) moves the album's files to another folder, and an
  album imported under an older structure moves to the current one.
  Saving a new structure moves nothing by itself: **Move files to fit**,
  under it in Library setup, moves every album that doesn't fit, as a save
  of each would (`POST /api/library/restructure` with `{}`), and lists
  which it couldn't move and why. When
  all of a folder's tracks go to one other folder, its cover images follow
  them, unless the name is taken there or audio stays behind. Folders left
  empty go. An album's folder in the Library holds what its import put
  there, its tracks and its cover: a save that would leave a folder
  holding anything else is refused. Moved tracks keep their ids and
  import dates, and a client that knew a track at its old path still
  finds it. The Library is supported on a local file system where names
  differ by case (ext4, btrfs, NTFS as Linux mounts it), not on a network
  share or FAT: a save that would move files is refused where they can't
  be renamed without risk of replacing one, or names fold. A save the
  structure couldn't place (a title cleared, two tracks on
  one path, a file already there or in the way of a folder, a file name
  starting with a dot, which scans skip, a folder name starting with
  `.coversync` in any case, a linked folder, another file system, one of
  those above, or moves before the Library's database exists, which its
  first import makes) is refused before anything is written; one whose files can't all be moved after the tags are written
  (a file appearing meanwhile) puts them all back and says so. Moves are
  recorded in the Library's database before they're made, and ones not
  yet indexed when coversyncd stops are put back when it next scans.
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
  close. A part in `< >` is optional: it's left out when a variable in it
  has no value, when it holds `{disc_number}` and the album has one disc, or
  when it holds `{track_number}` and the album is a single. The default,
  `{album_artist}/{album}/<{disc_number}-><{track_number} - >{title}`, names
  files `2-05 - Title`, `05 - Title` or `Title`; `<Disc {disc_number}>/`
  makes a disc folder only for albums with more than one. Every name it makes works on Windows, SMB shares and FAT32 or exFAT
  cards: tag values have `/ \ : * ? " < > |` replaced, the template's own
  text can't contain them, names Windows keeps for itself (CON, NUL, COM1,
  …) get a `_` in front, and no name ends in a dot or space. Tags a template
  uses outside `< >` count as required on import, but `{collection}`. The rules and the
  Library's index live in the Library folder itself (see
  [Moving the Library](#moving-the-library)).
- **Multiple values** (Library setup): the separator that joins several
  values of one tag (`;` by default), and phrases that join names, `feat.`,
  `ft.`, `featuring` and `(CV` to start with. In artist, album artist,
  composer and the other tags that name people (not sort names), a phrase
  between two names becomes the separator, both in what the import writes
  and in what MusicBrainz suggests: `Ado feat. Eve` becomes `Ado;Eve`, and
  `Hoshizora Rin (CV. Iida Riho)` becomes `Hoshizora Rin;Iida Riho`. Matching
  ignores case and full-width forms, takes a `:` or `.` right after a
  phrase along, drops the brackets around a phrase that opens one, and
  splits only between two names of the same value, never inside a word
  (accented letters count; Chinese, Japanese and Korean don't need spaces).
  A tag a file holds several times (two `ARTIST` comments, say) is read
  whole when one of them splits, so none is lost. Values typed in the
  import dialog are written as typed; the dialog says which tags it split. Filters split names at the phrases too, so an artist can be
  picked on its own in the Inbox as well.
- **Substitutions** (Library setup): regular expressions, each on the tags
  you pick, written as in vim: `s/pattern/replacement/flags`, with any ASCII
  punctuation but `\` in place of `/`. Patterns are Perl-style, as vim's
  `\v` (`(a|b)+`, `\d{4}`, `\b`), without look-around or back-references.
  Flags: `g` for every match rather than the first, `i` to ignore case. In
  the replacement, `&` and `\0` are the match, `\1`–`\9` its groups,
  `\u`/`\l` change the case of the next character and `\U`/`\L`
  everything up to `\E`. They run in order, after names are split, in what
  the import writes (the dialog says which tags they changed; typed values
  stay as typed) and in what MusicBrainz suggests. A tag holding several
  values (names, genres, release types, MusicBrainz artist IDs) has each
  changed on its own; other tags are changed whole. A list a file holds
  several times (two `ARTISTS` comments, say) is read whole when a rule
  would change it, so none is lost; any other tag is read by its first value, and a rule that changes
  it replaces the file's other copies, as a value typed in the import
  dialog does. `s/ \(Live\)$//` on the title turns `Song (Live)`
  into `Song`; `s/^(.+), The$/The \1/` on the artist turns `Beatles, The`
  into `The Beatles`. A rule that would leave a tag empty is skipped, and
  suggestions a rule makes invalid (a date that isn't one) aren't offered.
- **Collections** (under Library setup): groups of albums, all of a
  franchise say. The import dialog offers them for each album, None by
  default, and writes the one picked to the files' `COLLECTION` tag (a
  Vorbis comment, an ID3v2 `TXXX` frame, an MP4 `----:com.apple.iTunes`
  atom). A Library album's tag editor has a Collection dropdown of the same
  choices (and any collection its tracks are in already); saving another
  moves the album's files where the folder structure puts them. Collections
  are only picked, never typed. In the folder structure `{collection}` is
  always optional:
  `{collection}/{album_artist}/{album}/…` puts albums in none straight under
  `{album_artist}`.
- **Import** (an Inbox album's Import, or right-click it): Inbox files are
  read-only, so an album's tags are edited on its way into the Library. With
  Smart suggestions off, Import opens the album's tag editor, whose edits go
  to the Library copy; with them on, the import dialog offers MusicBrainz's
  tags instead. Then it checks the rules and the template's tags, opens the
  import dialog for anything missing (and the collection, if there are any),
  and copies (or moves) the files into place with the new tags and art. The
  original is never written: in Copy mode it is hidden from the Inbox
  afterwards, in Move mode deleted. A read-only original makes a copy
  coversync can still edit. Tags coversync has no name
  for stay as they were (`TITLE_JP`, `CDTOC`, a ripper's own), as do FLAC
  and Ogg ratings and the file's vendor string. Tags it does name are written its usual way:
  `ORGANIZATION` as `LABEL`, `TOTALTRACKS` as `TRACKTOTAL`, `TRACKNUMBER=01/12`
  as `TRACKNUMBER=1` and `TRACKTOTAL=12`, and ID3v2 tags as version 2.4. A file
  whose tags can't be written back as they are (an ID3v2 chapter located by
  byte offsets, say) is left alone and the import says why. Tag editor saves
  work the same way. FLAC is the Library's only lossless format: WAV, AIFF,
  WavPack, Monkey's Audio and ALAC files are converted to FLAC on import,
  with ffmpeg, and kept only if the FLAC's audio is the original's sample
  for sample (float or 32-bit audio FLAC can't hold exactly is refused,
  as is a file with more than one audio stream, or one ffmpeg can't decode
  without errors). Their tags carry over as far as Vorbis comments can
  hold them (tags lofty has no key for don't), and the dialog says which
  tracks are converted. coversync doesn't write APE tags (lofty can cut a
  file's audio rewriting one): Musepack and hybrid WavPack files (whose
  correction files ffmpeg can't read) can't be imported, and WavPack and
  Monkey's Audio files in the Library can't be edited; an MP3's APE tag
  (ReplayGain from MP3Gain, say) is left as it is.
- **Inbox setup**: Copy or Move; Preserve modification date, which gives an
  imported album its Inbox date (its newest file's modification time) as its
  date added instead of the import time, so the Library's Recently added
  keeps the Inbox's order (the import dialog shows the date it keeps);
  autofill for empty tags that are safe to assume (album artist from
  artist, track numbers from file names, …); and the uploads folder (see
  [Uploading albums](#uploading-albums)).
- **Uploads**: drop album folders (or files) on the Inbox, or pick them with
  the top bar's Upload button, and they go to the server's uploads folder and
  show up in the Inbox, folders such as `CD1/` and `CD2/` kept. A panel shows
  each upload's progress, with Cancel, and Show once its album is in the
  Inbox (see [Uploading albums](#uploading-albums)). Uploading from the
  desktop app, which resumes after the app restarts, is planned;
  `tools/upload.py` uploads a folder from a terminal.
- **Smart suggestions** (optional): the import dialog looks the album up on
  MusicBrainz, by Chromaprint fingerprints on AcoustID where it can, and
  otherwise by the barcode (or UPC) and ISRCs its files are tagged with, or
  its title and artist. An artist MusicBrainz credits under another name
  (汐れいら for "Ushio Reira") is found by its sort name or aliases. It
  offers MusicBrainz's tags to tick: values for empty tags and corrections
  for differing ones, and lists the release's tracks that none of the
  album's were paired with (a disc with none paired, such as a DVD, as one
  line): what the album lacks, if it's that release. **Prefer English**
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
- **Sync** (the server, the engine every device runs, and the
  [sync agent](#sync-agent) for desktops and laptops; the other device apps
  come next): devices pair with the
  server from Sync setup, each with a token of its own (see
  [Pairing devices](#pairing-devices)), and the sidebar's Sync section lists
  them. Each device gets the whole Library, with settings of its own in Sync
  setup: the Library's folders or its own, its art (a cover file, and the
  widest art), and when it syncs by itself. The sync API (`/api/sync/v1/`,
  its types in `crates/coversync-protocol`) plans every sync on the server:
  a device downloads what's new by content hash, several files at once, and
  renames what only moved, keeping no database of its own. The server keeps
  what each device holds, so a normal sync reads nothing back from the
  device. Sync setup shows each device's last sync, what's left, failed files
  and files in its folder coversync didn't put there, with **Sync now** and
  **Check files**. Sync setup can turn the section off. The sync engine
  (`crates/coversync-replica`) is the device's side, shared by every client:
  it runs the server's plans into a folder, several downloads at once, each
  checked against its hash as it arrives, resumed where it stopped, and put
  in place only once it's whole.
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
crates/coversync-protocol  the sync API's requests and replies, shared by the server and clients;
                        `cargo run -p coversync-protocol --example sync_client` syncs a folder
crates/coversync-replica  the sync engine every client runs; `cargo run -p coversync-replica
                        --example replica` syncs a folder with it
crates/coversync-cli    `coversync`, the command for a computer that keeps a copy of the Library:
                        `coversync run` is the background sync agent
crates/coversyncd       axum HTTP API, with the web UI embedded in the binary
crates/coversync-proxy  the desktop app's proxy to a server, on 127.0.0.1
crates/coversync-desktop-core  the desktop app's settings, servers and launches, apart from its window
web/                    SvelteKit / Svelte 5 single-page app
apps/desktop-linux/     Linux desktop app (Qt 6, Qt WebEngine), a window onto a server's UI; its own Cargo workspace
tools/ui_smoke.py       headless Firefox (geckodriver) smoke test
tools/player_stall.py   the player's buffering spinner while audio stalls, in headless Firefox
tools/upload.py         uploads an album folder through the upload API
tools/browser_upload.py the same through the web UI, in headless Firefox or Chromium, timed
packaging/arch/         PKGBUILD and systemd user units for the pacman packages
packaging/notices/      third-party notices for packages: Rust crates and the UI's npm packages
```

## Install on Arch Linux

Each [release](https://github.com/brpjerry/coversync/releases) has two
pacman packages: `coversync`, the server, and `coversync-agent`, which keeps
a copy of a server's Library on another computer (see
[Sync agent](#sync-agent)). Install the server and start it as a user
service:

```sh
sudo pacman -U coversync-0.1.0-1-x86_64.pkg.tar.zst
sudo pacman -S --asdeps chromaprint                # optional: fpcalc for Smart suggestions
sudo pacman -S --asdeps ffmpeg                     # optional: converts WAV, AIFF, WavPack, … to FLAC on import
systemctl --user enable --now coversyncd
coversyncd set-password                            # the password to sign in with
```

The service listens on <http://127.0.0.1:8770/>; sign in, then choose the
Inbox folder under Setup > Inbox (in the sidebar, under the tag filter). To build the
packages from a checkout instead, run `packaging/arch/build.sh` (it packages
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
- Tag edits are written only to Library files (but no APE tag is written,
  see Import). Writes go to a copy beside the file that is renamed into
  place, and every edit is validated before any file is touched.

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
  `sync` (syncing only: the sync API under `/api/sync/v1/`, `GET
  /api/session`, and its own `GET /api/devices/{id}/sync/settings`,
  `/sync/paths` and `/sync/state`; everything else is refused with 403).
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
only add files, never change any; they need a login, like the rest of the
API.

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
5. `GET /api/uploads/albums?path=PATH`, with the `path` finishing answered
   with, names the Inbox albums with files in that folder, once a scan has
   found them: `{"albums": [id, …]}`. A folder such as `CD1` uploaded on its
   own counts too, though its album is filed under the folder above.

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

**From the browser**, drop album folders or files on the Inbox, or use the
top bar's Upload button (**A folder…** or **Files…**). Each folder is one
upload, named after it, with the folders inside it kept; loose files go
together into one named `Upload`. Hidden files, and kinds uploads don't take,
are left out, and the panel says how many. The page sends several chunks at
once (up to 32 requests and 128 MiB over HTTP/2, four requests over HTTP/1.1,
which leaves room for the page's own), one upload after another without a
pause between them. If the connection drops or coversyncd restarts, it asks
how far each file got and sends the rest, for as long as the page stays open.
Once an upload is in the Inbox, the panel's Show opens its album.
A page can't read the files again after a reload, so leaving the page asks
first, and cancels the uploads under way on the server as it goes.

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
  nothing, every waiting code is cancelled. A code that was used, tried
  again (by a device whose answer was lost), isn't counted as a guess.
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
  it may have been seen on the way: its JSON `code`, and any code-shaped
  word in its first 4 KiB, however it's written and however many attempts
  its client has made. Such requests count against the limits on wrong
  codes. Sync setup can allow plain http for a network that is encrypted
  already (Tailscale, WireGuard).
- **CA certificate for apps.** For a server whose certificate a CA of your own
  signed (mkcert's, say), set Sync setup's CA certificate to the path of
  that CA's public certificate, such as `~/.local/share/mkcert/rootCA.pem`.
  coversyncd serves it at `GET /api/hello/ca`, and new codes put its SHA-256
  in the QR code (`ca=`, the fingerprint `openssl x509 -noout -fingerprint
  -sha256` prints, as lowercase hex), so the Android app can check the
  certificate and trust it for this server alone. A file holding a private
  key, such as `rootCA-key.pem` beside it, is refused, as is anything but
  one PEM certificate. Leave it empty for a publicly trusted certificate;
  the desktop app uses the system's certificates either way.

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
is no endpoint to ask for the changes since a given value: devices learn of
changes from the sync API's `target`, which the counter is part of. If a
Library's counter is ever lower than the highest one this
installation has seen it reach, as when its folder is restored from an older
backup, coversyncd gives it a new id, so clients that knew the later state
start over rather than trust it. A Library moved to another installation
keeps its id. An older coversyncd still opens a database a newer one has
upgraded, and the changes it makes are counted too.

Library tracks also keep a whole-file BLAKE3 digest (`blake3:` followed by
64 hexadecimal characters), with the size and nanosecond modification time
it covers. Imports copy tracks in parallel, preserving filesystem reflinks
where available and otherwise hashing as bytes stream. Files whose tags are
rewritten are hashed after Lofty finishes its seek-based writes. Tag edits
invalidate the old digest immediately and queue a rehash after writing.
Scans invalidate stale hashes and queue a separate parallel pass at low CPU and I/O priority on Linux.
That pass leaves scans, imports and tag edits free to run, rejects results
from superseded rows or another Library, and stops during daemon shutdown.
`/api/status` includes `hashes` with `running`, `files_total`, `files_done`,
`bytes_hashed`, `errors`, and `skipped` (files or rows changed during the pass).
A Library switch stops the previous pass at its next batch boundary. Publishing
a batch of hashes advances the Library's change counter too, while leaving the
album's tag-edit version
unchanged. The Inbox does not get file hashes. Files with modification times
outside the supported nanosecond range remain indexed, but have no digest until
their timestamp is representable. A Library indexed only in memory keeps its
hashes there too; an import persists both the index and its existing hashes.

For a scratch Library, `cargo run --release -p coversync-core --example
hash_bench -- STATE read` measures plain parallel reads; the `hash` mode
measures and stores missing digests. Use a scratch database and protect any
live source files from writes when benchmarking.

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

## Desktop app

The desktop app, `apps/desktop-linux`, shows a coversync server's UI in a
window of its own. It is for Linux, on Qt 6 and Qt WebEngine (which is
Chromium); there is none for Windows or macOS. Its settings, servers and
proxies are in `crates/coversync-desktop-core`, apart from the window, and
the page it shows without a server in `apps/desktop-linux/ui`. The UI comes
from the server, so the app doesn't need updating when the server does. Everything the window loads goes through
a small proxy in the app, on 127.0.0.1 (`crates/coversync-proxy`):

- Bodies stream both ways, so covers, playback and seeking, uploads and
  server-sent events work as in a browser.
- The server's path prefix stays: `https://n5pro.internal/coversync/` is
  served at `http://127.0.0.1:<port>/coversync/`.
- Each server gets its own port, kept from run to run, so each has an origin
  of its own in the window: its cached pages and covers, and the UI's
  settings (sort, sidebar), are its own. Ports are kept for the saved server
  and the 8 others most recently used; a server that drops out of those
  loses its cookies too, and its port never goes to another server. An
  address that's refused (below) keeps no cookies either. Either way, a
  kept server's origin keeps its cookies, whatever the prefix. One server's
  pages can't send requests through another's proxy.
- The proxy keeps the server's cookies itself, apart for each server, and
  sends them as a browser would. So a server's login works as in a browser,
  and one server's cookies never go to another. Persistent ones (a login)
  are kept in `cookies.json` in the app's config directory, readable only by
  you.
- Only the app's window can use it. The window opens the proxy with a secret
  made at each launch, which the proxy trades for a cookie, and requests
  without that cookie are refused.
- HTTPS servers are checked against the system's certificate store, so a LAN
  server with a certificate from your own CA (mkcert's, say) works once that
  CA is installed there.

When no server is set or it can't be reached, the window shows a page
bundled with the app, to try again or change the server. It also does when
the page's process crashes. A
new server entered there is used and saved only once it's checked: its `/api/hello`, which
coversync answers without a login, has to say it's coversync. A server too
old to have it, an error, or a redirect outside its address doesn't pass.
Neither does a server behind a login of its own in front (HTTP basic auth on
its reverse proxy, say), which answers 401 for `/api/hello`; `--server` or
`COVERSYNC_SERVER` still opens it, without the check, whether the app is
starting or already running. The address and the
ports are kept in `config.json` in the app's config directory
(`~/.config/io.github.brpjerry.coversync/`).
`--server URL` or `COVERSYNC_SERVER=URL` picks a server for one run without
saving it. One instance of the app runs at a time: launching it again
brings up its window, and shows the server given with `--server` (or
`COVERSYNC_SERVER`), if any; a server already showing isn't reloaded.
Links that would open a new window, such as MusicBrainz's, open in the
default browser.
A lock in the config directory keeps a second process off its settings
even where that handover can't work (it goes over a socket in
`$XDG_RUNTIME_DIR`); where the file system can't lock at all, the app warns
and runs without it.

There is no package for it yet. To build and run it on Arch Linux, you need Qt with Qt WebEngine, a C++ compiler and Rust. It uses Qt
6.9's API; it has been built and run only on Qt 6.12, with Qt WebEngine
6.11:

```sh
sudo pacman -S --needed base-devel qt6-base qt6-declarative qt6-webengine
cd apps/desktop-linux
cargo run                    # or cargo run -- --server https://n5pro.internal/coversync/
cargo build --release        # builds target/release/coversync-desktop
```

The window is Qt's, in C++ and QML (`cpp/`, `src/window.qml`), and the rest
is Rust; `build.rs` finds Qt through `qmake` and compiles both. It runs
natively on Wayland. One instance runs at a time: a later launch hands its
arguments to the first over a socket and ends. The fallback page is served
from `coversync://app/`, and reaches the app through a script in a script
world of its own, which a server's pages never see; the app checks where
each of its commands comes from. A debug build writes the pages' console to
stdout.

What the window takes from the desktop comes through its portal
(`xdg-desktop-portal`), as for GTK apps and browsers: light or dark, and the
file chooser behind a page's "Choose image". The app runs on Qt's
`xdgdesktopportal` platform theme for that, whatever `QT_QPA_PLATFORMTHEME`
says: a theme meant for Qt's widgets (qt6ct, say) would hand pages its own
palette instead of the desktop's preference.

The app can be paired with a server, which gives this computer an entry of
its own among the server's devices and a token of its own, in place of a
password login in the window. The device can be removed in the server's Sync
setup without changing the password.

- **By code:** after the address is checked, the app's page asks for a code
  from the server's Add device (or to sign in with the password instead, as
  in a browser).
- **By link:** Add device also shows a `coversync://pair?server=…&code=…`
  link. Opening it starts the app, or reaches the one running, which names
  the server and asks before it pairs. A link can come from anywhere, so
  nothing is sent before that. The link's `ca=` part is ignored: the app
  trusts the system's certificate store. A page in the window can't open
  such a link, or any other app's.
- **The token** is kept in the desktop's keychain (the Secret Service:
  GNOME Keyring, KeePassXC, KWallet), under the server's address, and goes
  with every request to that server. It is never written to `config.json`,
  `cookies.json` or a log. Without a keychain the app doesn't pair, and
  says why before a code is spent.
- **Pairing again** after a reinstall or with a new keychain: make the code
  with Replace an existing device, so the computer keeps its entry.
- **Removed on the server**, the app shows its own page again, saying so,
  with the choice to pair again or sign in with the password.
- **Forget this server**, on the app's page, removes the server's token from
  the keychain, its cookies and its port.

For the desktop to hand pairing links to the app, the app's desktop entry
(`apps/desktop-linux/io.github.brpjerry.coversync.desktop`) has to be
installed, with `coversync-desktop` on the `PATH`. There is no package that
does that yet; for a build of your own:

```sh
install -Dm644 apps/desktop-linux/io.github.brpjerry.coversync.desktop \
  ~/.local/share/applications/io.github.brpjerry.coversync.desktop
xdg-mime default io.github.brpjerry.coversync.desktop x-scheme-handler/coversync
```

The UI comes from the server, so it asks the app around it what that app
can do. It finds the app by a `Coversync-App` header the proxy adds to
`/api/hello`, and then asks once, at `POST /__coversync-app/v1/capabilities`
on its own origin, for a list of features; what depends on one shows only
when it's listed, so an older app works with a newer UI and the other way
round. The proxy answers that path itself and never forwards it
(`crates/coversync-proxy/src/app.rs`; the commands are in
`crates/coversync-desktop-core/src/bridge.rs`), and coversyncd serves nothing
there. Only a page of the server's own UI can ask: a request needs the
proxy's cookie, `Sec-Fetch-Site: same-origin`, the proxy's own `Origin` and a
JSON body, so a page from elsewhere, a frame of another origin or of none, or
another server's page in the same window is refused. The commands are safe for any script on the page:
none takes a file's path, gives out a sign-in, or changes the server the app
uses. In a browser there is no such header, and the UI never asks.

While its window is in the background, the app shows a finished scan, import
or suggestions lookup as a desktop notification. The UI sends these with the
web's own `Notification`, only in an app that lists `notify`; the app lets a
server's UI send them without asking, and hands them to the desktop's
notification service (`org.freedesktop.Notifications`), or to the portal in a
sandbox. Clicking one brings the window up, where the desktop allows that.

The app has a settings page of its own, for what's kept on the computer and
not on the server: **This app**, under Setup in the sidebar, or Ctrl+comma.
It opens over the server's page, in a view of its own, so the page under it
stays as it is and goes on playing. The server's UI opens it with one
command (`open-settings`), which it offers only in an app that lists
`settings`. So far the page holds sync:

- **Sync this computer** turns the [sync agent](#sync-agent) on and off
  (its systemd user unit, enabled and started, or disabled and stopped),
  for the server the window shows. It needs the `coversync-agent` package,
  the computer paired with the server, and a folder. The agent's own line
  of status shows under the switch.
- **Folder** is where the copy of the Library is kept, chosen with the
  desktop's folder chooser. It is written to the agent's settings on this
  computer and never sent to the server.
- What the computer gets and when it syncs by itself are the device's
  settings, in the server's Sync setup.
- An app started with a settings folder of its own (another
  `XDG_CONFIG_HOME`) can't set sync: the agent reads the usual one, and the
  page says so.

While sync is on, a track the computer holds a copy of is played from the
copy: the app's proxy answers the UI's request for its audio from the sync
folder, and the server isn't asked. Which tracks those are comes from the
server's inventory for this computer's device, asked again every five
minutes; a file is used only if it is in the folder itself with the size the
server has for it, and anything else is streamed as before
(`crates/coversync-desktop-core/src/local.rs`). A copy is told by its place
and its size, as the sync engine tells one, and isn't read through first: a
file changed on the computer without changing its length is played as it
is, until **Check files** in Sync setup finds it. The inventory is asked for
as the app's own device, so the app has to be paired with the server, not
signed in with the password. With sync off, nothing is played from the
copy, within ten seconds of turning it off, from the page or from a
terminal.

Qt WebEngine has no momentum for two-finger scrolling on Linux, so the app
carries a scroll on itself once the fingers lift (`cpp/momentum.h`, which
lists the environment variables that tune it; `COVERSYNC_MOMENTUM=0` turns it
off).

Qt WebEngine doesn't tell Chromium the display's refresh rate (QTBUG-123640),
so with Qt's own packages pages are drawn at 60 frames a second whatever the
display does. `patches/qt6-webengine-use-the-screens-refresh-rate.patch`
passes the rate on; it applies to Qt WebEngine 6.11.3, in `prepare()` of
Arch's `qt6-webengine` PKGBUILD, say. On a 120 Hz laptop, two-finger drags
over the album grid ran at 55 frames a second on Arch's package and 111 on
the patched one. Without the patch, `COVERSYNC_FRAME_RATE=unlimited` takes
Chromium's limit away, and scrolling then follows the display, but it is for
trying only: a view transition, which opening an album is, then never
finishes and the page stops being redrawn.

The app has its own Cargo workspace and lockfile, apart from the
repository's, so building and testing the rest doesn't need Qt.
`RUST_LOG=coversync_proxy=debug` also logs each request the proxy forwards;
warnings show either way. The fallback page has no build step.

## Sync agent

`coversync run` keeps a folder on a desktop or laptop in step with a server's
Library, in the background, with no app open. It runs the engine every
client runs (`crates/coversync-replica`); the command is in
`crates/coversync-cli`, and the `coversync-agent` package has it with a
systemd user unit:

```sh
sudo pacman -U coversync-agent-0.1.0-1-x86_64.pkg.tar.zst
coversync setup --server https://n5pro.internal/coversync --root ~/Music/Library
systemctl --user enable --now coversync-agent
systemctl --user status coversync-agent    # what it's doing, in a line
```

With the [desktop app](#desktop-app), the last three lines are its settings
page instead: choose the folder there and turn **Sync this computer** on.

- **Pairing.** A computer the desktop app has paired needs nothing more: the
  agent uses the app's token, from the keychain, so the computer is one
  device in Sync setup. Without the app, `coversync pair <server> <code>`
  takes a code from Add device and keeps the token in the keychain, or with
  `--token-file` in a file only you can read, for a computer with no
  keychain. With the app open, pair in its window: the keychain's token is
  the app's too, and `coversync pair` waits for it to be closed. A token
  file is one server's token: `coversync setup --server` to another server
  stops using it, and `coversync run --server` refuses to send it there.
- **What's kept on the computer** is the server, the folder and, if one is
  used, where the token file is: in
  `~/.config/io.github.brpjerry.coversync/sync.json`, which `coversync setup`
  shows and changes. The folder is never sent to the server. Everything else
  is the device's settings in the server's Sync setup.
- **When it syncs.** It keeps the server's event stream open and syncs once
  the Library has been quiet for the device's quiet period, at once for
  **Sync now**, when it starts and when the computer wakes. It also asks
  the server hourly, in case the stream dropped without a word, and when
  the network comes back or changes.
- **The device's conditions** in Sync setup hold for syncs it starts by
  itself: on unmetered networks only (as NetworkManager tells it), and
  while charging only (as UPower does: not on the battery). A sync under way
  stops when they stop holding, whether the network or the power changed or
  the device's settings did, and goes on when they hold again. **Sync
  now** is someone asking, and syncs whatever they say. A computer without
  NetworkManager or UPower counts as unmetered and on mains power. Until
  they have answered, a sync that depends on them waits, for ten seconds
  at most: a service that says nothing by then is taken as not there, and
  looked for again every minute.
- **The server or the network being away** is waited out. A sync that failed
  is tried again after 30 seconds, then at doubling intervals up to 10
  minutes (5 minutes up to an hour for trouble with the folder).
- **The folder has to exist.** One that isn't there, such as on a disk that
  isn't mounted, is waited for and never made. A folder that was synced
  before and has lost coversync's marker (`.coversync-replica/`) is taken
  for an empty mount point rather than a wish for a second copy: only
  **Sync now** fills it.
- **A token the server refuses** (the device was revoked or removed) ends
  the agent with exit status 3, and the unit doesn't start it again; pair
  again, then start it. Status 2 means nothing is set up yet.

## Develop

```sh
(cd web && pnpm dev)        # Vite on :5173, proxying /api to 127.0.0.1:8770
cargo test --workspace && cargo clippy --workspace --all-targets
(cd web && pnpm check && pnpm test)
COVERSYNC_PASSWORD=… tools/ui_smoke.py http://127.0.0.1:8770/ shots/   # read-only tour of the UI
COVERSYNC_PASSWORD=… tools/player_stall.py http://127.0.0.1:8770/       # the player's spinner through stalls
COVERSYNC_PASSWORD=… tools/upload.py http://127.0.0.1:8770/ ~/Music/Album   # upload a folder
COVERSYNC_PASSWORD=… tools/browser_upload.py http://127.0.0.1:8770/ ~/Music/Album   # the same through the UI, timed
```

A scratch daemon gets a password with
`coversyncd set-password --state-dir <scratch state dir>`. The smoke test
signs in with `COVERSYNC_PASSWORD` (or `--password`); `--auth` tries the login
page itself. Its other modes (`--save`, `--devices`, `--import`, `--suggest`,
…, listed at the top of the script) change settings, pair devices or import
albums: run them only against a daemon with a scratch `--state-dir` and
Library folder.

For every pull request and push to `main`, CI (`.github/workflows/ci.yml`) runs
`pnpm lint`, `pnpm check`, the UI's tests (`pnpm test`), rustfmt, Clippy with
warnings as errors and the tests, and builds the pacman package.

Changes to the desktop app or its proxy also run
`.github/workflows/desktop.yml`, which builds the app against Qt, in an Arch
Linux container, and runs rustfmt, Clippy and its tests.

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
