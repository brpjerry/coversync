//! SQLite index of tracks and cover metadata, in two databases.
//!
//! The local one, in coversyncd's state directory, holds the Inbox: its
//! index, the files hidden from it, its setup, plus cover data and the
//! settings that belong to this installation. The Library's own one lives in
//! the Library folder (see [`LIBRARY_DB`]) and travels with it: the Library's
//! rules and its index, with paths relative to the folder. It is attached to
//! the same connection as `lib`, and temporary views give both collections'
//! tracks the same shape.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, Row, ToSql, params, params_from_iter};

use crate::appearance::AppearanceSetup;
use crate::autofill::{self, Autofill, TrackInfo};
use crate::inbox::{HiddenStats, InboxSetup};
use crate::model::{Album, AlbumDetail, Attention, Collection, KindCoverage, TagCoverage, Track};
use crate::rules::{self, LibrarySetup};
use crate::sync::SyncSetup;
use crate::tags::TrackTags;
use crate::{Error, Result};

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
CREATE TABLE IF NOT EXISTS tracks (
    id           INTEGER PRIMARY KEY,
    path         TEXT NOT NULL UNIQUE,
    root         TEXT NOT NULL,
    mtime        INTEGER NOT NULL,
    size         INTEGER NOT NULL,
    album_id     TEXT NOT NULL,
    album_dir    TEXT NOT NULL,
    title        TEXT NOT NULL,
    artist       TEXT NOT NULL,
    album        TEXT NOT NULL,
    album_artist TEXT,
    track_no     INTEGER,
    track_total  INTEGER,
    disc_no      INTEGER,
    disc_total   INTEGER,
    year         INTEGER,
    genre        TEXT,
    duration_ms  INTEGER NOT NULL,
    sample_rate  INTEGER,
    bit_depth    INTEGER,
    channels     INTEGER,
    bitrate      INTEGER,
    format       TEXT NOT NULL,
    tag_keys     TEXT NOT NULL DEFAULT '',
    collection   TEXT NOT NULL DEFAULT 'inbox',
    tag_values   TEXT NOT NULL DEFAULT '{}'
);
CREATE INDEX IF NOT EXISTS tracks_album ON tracks (album_id);
-- Inbox files already copied into the Library; kept by path so rescans keep them hidden.
CREATE TABLE IF NOT EXISTS hidden (
    path      TEXT PRIMARY KEY,
    hidden_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS covers (
    album_id  TEXT PRIMARY KEY,
    source    TEXT,
    color     TEXT,
    signature TEXT NOT NULL
);
"#;

/// The local database's file name, in the state directory.
pub const LOCAL_DB: &str = "inbox.sqlite3";

/// What the local database was called before the Library got its own.
const LEGACY_DB: &str = "library.sqlite3";

/// The Library's own database, relative to the Library folder.
pub const LIBRARY_DB: &str = ".coversync/library.sqlite3";

/// Track columns both databases store the same way. The Library's leaves
/// out `root` and `collection`, and its `path` and `album_dir` are relative
/// to the Library folder.
const DATA_COLUMNS: &[&str] = &[
    "mtime",
    "size",
    "album_id",
    "title",
    "artist",
    "album",
    "album_artist",
    "track_no",
    "track_total",
    "disc_no",
    "disc_total",
    "year",
    "genre",
    "duration_ms",
    "sample_rate",
    "bit_depth",
    "channels",
    "bitrate",
    "format",
    "tag_keys",
    "tag_values",
];

/// The Library database's tables, in the database attached as `schema`.
/// It keeps a rollback journal rather than a write-ahead log, so a copy of
/// the folder taken while coversyncd is stopped holds every change.
fn library_schema(schema: &str) -> String {
    format!(
        r#"
PRAGMA {schema}.journal_mode = DELETE;
CREATE TABLE IF NOT EXISTS {schema}.tracks (
    id           INTEGER PRIMARY KEY,
    path         TEXT NOT NULL UNIQUE,
    mtime        INTEGER NOT NULL,
    size         INTEGER NOT NULL,
    album_id     TEXT NOT NULL,
    album_dir    TEXT NOT NULL,
    title        TEXT NOT NULL,
    artist       TEXT NOT NULL,
    album        TEXT NOT NULL,
    album_artist TEXT,
    track_no     INTEGER,
    track_total  INTEGER,
    disc_no      INTEGER,
    disc_total   INTEGER,
    year         INTEGER,
    genre        TEXT,
    duration_ms  INTEGER NOT NULL,
    sample_rate  INTEGER,
    bit_depth    INTEGER,
    channels     INTEGER,
    bitrate      INTEGER,
    format       TEXT NOT NULL,
    tag_keys     TEXT NOT NULL DEFAULT '',
    tag_values   TEXT NOT NULL DEFAULT '{{}}',
    -- When the file joined the Library (Unix seconds): the import that
    -- wrote it, or for a file found there by a scan, its modification time.
    added_at     INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS {schema}.tracks_album ON tracks (album_id);
CREATE TABLE IF NOT EXISTS {schema}.settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#
    )
}

/// Library track ids are public as -(prefix * 2^32 + row id), with a prefix
/// chosen at random per Library (below 2^20, so ids stay exact in
/// JavaScript). An id from another Library then names no track here rather
/// than a different one, and never clashes with an Inbox id.
const ID_SPAN: i64 = 1 << 32;

/// The connection's temporary objects: the Library folder attached (no row
/// while there is none), and views that give Library tracks absolute paths
/// and public ids, alongside the Inbox's.
fn temp_schema() -> String {
    let data = DATA_COLUMNS.join(", ");
    let lib_data: Vec<String> = DATA_COLUMNS.iter().map(|c| format!("t.{c}")).collect();
    let lib_data = lib_data.join(", ");
    format!(
        "CREATE TEMP TABLE lib_root (
            root    TEXT NOT NULL,
            prefix  INTEGER NOT NULL,
            -- 1 when the Library's own database is attached; 0 while its
            -- folder is indexed in memory (no database yet, or none possible).
            stored  INTEGER NOT NULL,
            -- Why the folder can't hold its database, if it can't.
            problem TEXT
         );
         CREATE TEMP VIEW library_tracks AS
            SELECT -(r.prefix * {ID_SPAN} + t.id) AS id, r.root || '/' || t.path AS path,
                r.root AS root,
                CASE t.album_dir WHEN '' THEN r.root ELSE r.root || '/' || t.album_dir END
                    AS album_dir,
                'library' AS collection, {lib_data}, t.added_at
            FROM lib.tracks t JOIN temp.lib_root r;
         -- An Inbox file counts as added when it was last modified.
         CREATE TEMP VIEW inbox_tracks AS SELECT *, mtime AS added_at FROM main.tracks;
         CREATE TEMP VIEW indexed_tracks AS
            SELECT id, path, root, album_dir, collection, {data}, added_at
                FROM temp.inbox_tracks
            UNION ALL
            SELECT id, path, root, album_dir, collection, {data}, added_at
                FROM temp.library_tracks;"
    )
}

/// The tracks of `collection`, as a table or view with the local schema.
fn source(collection: Collection) -> &'static str {
    match collection {
        Collection::Inbox => "temp.inbox_tracks",
        Collection::Library => "temp.library_tracks",
    }
}

/// The Library folder attached, and how.
struct Attached {
    root: PathBuf,
    prefix: i64,
    stored: bool,
    problem: Option<String>,
}

fn attached(conn: &Connection) -> Result<Option<Attached>> {
    Ok(conn
        .query_row(
            "SELECT root, prefix, stored, problem FROM temp.lib_root",
            [],
            |r| {
                Ok(Attached {
                    root: PathBuf::from(r.get::<_, String>(0)?),
                    prefix: r.get(1)?,
                    stored: r.get(2)?,
                    problem: r.get(3)?,
                })
            },
        )
        .optional()?)
}

/// The Library folder whose tracks are indexed, if one is.
fn library_root(conn: &Connection) -> Result<Option<PathBuf>> {
    Ok(attached(conn)?.map(|a| a.root))
}

/// `folder` as the existing directory it names, symlinks resolved.
fn existing_dir(folder: Option<&str>) -> Option<PathBuf> {
    let dir = Path::new(folder?).canonicalize().ok()?;
    dir.is_dir().then_some(dir)
}

/// A random track id prefix for a new Library index (see [`ID_SPAN`]). An
/// index rebuilt in memory gets a new one too: its rows are numbered afresh,
/// so ids from before must not name its tracks.
fn new_prefix() -> i64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    1 + (h.finish() % ((1 << 20) - 1)) as i64
}

/// Index `dir` (or nothing, with no `dir`) in an empty in-memory database.
fn attach_memory(conn: &Connection, dir: Option<&Path>, problem: Option<&str>) -> Result<()> {
    let _ = conn.execute("DETACH DATABASE lib", []);
    conn.execute("ATTACH DATABASE ':memory:' AS lib", [])?;
    conn.execute_batch(&library_schema("lib"))?;
    conn.execute("DELETE FROM temp.lib_root", [])?;
    if let Some(dir) = dir {
        conn.execute(
            "INSERT INTO temp.lib_root (root, prefix, stored, problem) VALUES (?1, ?2, 0, ?3)",
            params![dir.to_string_lossy(), new_prefix(), problem],
        )?;
    }
    Ok(())
}

/// Attach `dir`'s own database as `lib`, creating it if the folder has none.
/// Tracks of `carry`, the same folder indexed in memory, move into a
/// database created now. Returns whether the database is new. On failure the
/// database attached before stays attached.
fn attach_file(conn: &Connection, dir: &Path, carry: Option<i64>) -> Result<bool> {
    let db = dir.join(LIBRARY_DB);
    if let Some(parent) = db.parent() {
        std::fs::create_dir_all(parent)?;
    }
    conn.execute("ATTACH DATABASE ?1 AS lib_new", [db.to_string_lossy()])?;
    let prepared = (|| -> Result<(i64, bool)> {
        conn.execute_batch(&library_schema("lib_new"))?;
        migrate_library(conn, "lib_new")?;
        // An older coversyncd, opening the folder elsewhere, adds files
        // without a time; date them by their modification time now.
        conn.execute(
            "UPDATE lib_new.tracks SET added_at = mtime WHERE added_at = 0",
            [],
        )?;
        // SQLite opens a file it can't write read-only without saying so;
        // writing the prefix (again) checks the file and its folder, which
        // holds the journal, take writes.
        if let Some(prefix) = get_setting::<i64>(conn, "lib_new", "track_id_prefix")? {
            put_setting(conn, "lib_new", "track_id_prefix", &prefix)?;
            return Ok((prefix, false));
        }
        let prefix = carry.unwrap_or_else(new_prefix);
        put_setting(conn, "lib_new", "track_id_prefix", &prefix)?;
        if carry.is_some() {
            conn.execute(
                "INSERT OR IGNORE INTO lib_new.tracks SELECT * FROM lib.tracks",
                [],
            )?;
        }
        Ok((prefix, true))
    })();
    let _ = conn.execute("DETACH DATABASE lib_new", []);
    let (prefix, created) = prepared?;
    conn.execute("DETACH DATABASE lib", [])?;
    if let Err(e) = conn.execute("ATTACH DATABASE ?1 AS lib", [db.to_string_lossy()]) {
        attach_memory(conn, Some(dir), Some(&e.to_string()))?;
        return Err(e.into());
    }
    conn.execute("DELETE FROM temp.lib_root", [])?;
    conn.execute(
        "INSERT INTO temp.lib_root (root, prefix, stored) VALUES (?1, ?2, 1)",
        params![dir.to_string_lossy(), prefix],
    )?;
    Ok(created)
}

/// Bring a Library database up to the current schema (its own `user_version`).
fn migrate_library(conn: &Connection, schema: &str) -> rusqlite::Result<()> {
    let version: i64 =
        conn.query_row(&format!("PRAGMA {schema}.user_version"), [], |r| r.get(0))?;
    if version < 1 {
        conn.execute_batch(&format!("PRAGMA {schema}.user_version = 1"))?;
    }
    if version < 2 {
        // v2 records when each file joined the Library. Earlier imports
        // wrote their files then, so their modification time stands in. One
        // transaction, so an interrupted upgrade is redone in full.
        let has_column = conn
            .prepare(&format!(
                "SELECT 1 FROM pragma_table_info('tracks', '{schema}') WHERE name = 'added_at'"
            ))?
            .exists([])?;
        let add = if has_column {
            String::new()
        } else {
            format!("ALTER TABLE {schema}.tracks ADD COLUMN added_at INTEGER NOT NULL DEFAULT 0;")
        };
        conn.execute_batch(&format!(
            "BEGIN;
             {add}
             UPDATE {schema}.tracks SET added_at = mtime WHERE added_at = 0;
             PRAGMA {schema}.user_version = 2;
             COMMIT;"
        ))
        .inspect_err(|_| {
            let _ = conn.execute_batch("ROLLBACK");
        })?;
    }
    Ok(())
}

/// A setting stored in `schema` ("main" or "lib"). A value this build can't
/// parse (say, from a newer one) reads as unset.
fn get_setting<T: serde::de::DeserializeOwned>(
    conn: &Connection,
    schema: &str,
    key: &str,
) -> Result<Option<T>> {
    Ok(raw_setting(conn, schema, key)?.and_then(|v| serde_json::from_str(&v).ok()))
}

fn raw_setting(conn: &Connection, schema: &str, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            &format!("SELECT value FROM {schema}.settings WHERE key = ?1"),
            [key],
            |r| r.get(0),
        )
        .optional()?)
}

fn put_setting<T: serde::Serialize>(
    conn: &Connection,
    schema: &str,
    key: &str,
    value: &T,
) -> Result<()> {
    let json = serde_json::to_string(value).expect("settings serialize");
    conn.execute(
        &format!(
            "INSERT INTO {schema}.settings (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value"
        ),
        params![key, json],
    )?;
    Ok(())
}

/// Library setup as the Library's own database stores it: everything but
/// the folder, which is where the database itself is.
fn own_setup(setup: &LibrarySetup) -> LibrarySetup {
    LibrarySetup {
        folder: None,
        ..setup.clone()
    }
}

/// A file found on disk, with the stat data used to skip unchanged files.
#[derive(Debug, Clone)]
pub struct FileStat {
    pub path: PathBuf,
    pub root: PathBuf,
    pub mtime: i64,
    pub size: u64,
    pub collection: Collection,
    /// For Library files an import just wrote: the import time, which
    /// replaces any stored `added_at`. `None` keeps the stored value, or for
    /// a file not indexed yet, starts it at `mtime`.
    pub added: Option<i64>,
}

/// SQL condition selecting tracks the Inbox shows (not hidden).
const VISIBLE: &str = "path NOT IN (SELECT path FROM hidden)";

/// A file ready to index: its stat data, album id, album directory and tags.
pub type IndexedFile = (FileStat, String, PathBuf, TrackTags);

/// An album's stored cover source and colour.
type CoverRow = (Option<String>, Option<String>);

/// Where an album's cover comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoverSource {
    File(PathBuf),
    Embedded(PathBuf),
}

impl CoverSource {
    fn encode(&self) -> String {
        match self {
            CoverSource::File(p) => format!("file:{}", p.display()),
            CoverSource::Embedded(p) => format!("embedded:{}", p.display()),
        }
    }

    fn decode(s: &str) -> Option<Self> {
        if let Some(p) = s.strip_prefix("file:") {
            Some(CoverSource::File(p.into()))
        } else {
            s.strip_prefix("embedded:")
                .map(|p| CoverSource::Embedded(p.into()))
        }
    }
}

/// Cover bookkeeping for one album, as the cover pass sees it.
pub struct CoverJob {
    pub album_id: String,
    pub album_dir: PathBuf,
    pub first_track: PathBuf,
    pub signature: String,
}

pub struct Library {
    conn: Mutex<Connection>,
}

fn track_from_row(row: &Row<'_>) -> rusqlite::Result<(Track, String)> {
    let path: String = row.get("path")?;
    let root: String = row.get("root")?;
    let rel = Path::new(&path)
        .strip_prefix(&root)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.clone());
    Ok((
        Track {
            id: row.get("id")?,
            album_id: row.get("album_id")?,
            title: row.get("title")?,
            artist: row.get("artist")?,
            album: row.get("album")?,
            album_artist: row.get("album_artist")?,
            track_no: row.get("track_no")?,
            track_total: row.get("track_total")?,
            disc_no: row.get("disc_no")?,
            disc_total: row.get("disc_total")?,
            year: row.get("year")?,
            genre: row.get("genre")?,
            duration_ms: row.get::<_, i64>("duration_ms")? as u64,
            sample_rate: row.get("sample_rate")?,
            bit_depth: row.get("bit_depth")?,
            channels: row.get("channels")?,
            bitrate: row.get("bitrate")?,
            format: row.get("format")?,
            rel_path: rel,
            collection: Collection::parse(&row.get::<_, String>("collection")?),
        },
        row.get("album_dir")?,
    ))
}

/// The value most tracks agree on, preferring earlier tracks on ties.
fn consensus<'a>(values: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for v in values {
        match counts.iter_mut().find(|(k, _)| *k == v) {
            Some((_, n)) => *n += 1,
            None => counts.push((v, 1)),
        }
    }
    counts
        .iter()
        .enumerate()
        .max_by_key(|(i, (_, n))| (*n, std::cmp::Reverse(*i)))
        .map(|(_, (k, _))| *k)
}

/// When an album counts as added, from its tracks' times: in the Inbox, its
/// newest file; in the Library, its first import.
fn album_added(collection: Collection, times: impl Iterator<Item = i64>) -> i64 {
    match collection {
        Collection::Inbox => times.max(),
        Collection::Library => times.min(),
    }
    .unwrap_or(0)
}

fn summarize(id: &str, tracks: &[&Track], added: i64, cover: Option<&CoverRow>) -> Album {
    let title = consensus(tracks.iter().map(|t| t.album.as_str())).unwrap_or_default();
    let album_artist = consensus(tracks.iter().filter_map(|t| t.album_artist.as_deref()));
    let artist = album_artist.map(str::to_string).unwrap_or_else(|| {
        let mut artists: Vec<&str> = tracks.iter().map(|t| t.artist.as_str()).collect();
        artists.sort_unstable();
        artists.dedup();
        if artists.len() > 3 {
            "Various Artists".to_string()
        } else {
            consensus(tracks.iter().map(|t| t.artist.as_str()))
                .unwrap_or_default()
                .to_string()
        }
    });
    Album {
        id: id.to_string(),
        title: title.to_string(),
        artist,
        year: tracks.iter().filter_map(|t| t.year).min(),
        genre: consensus(tracks.iter().filter_map(|t| t.genre.as_deref())).map(str::to_string),
        track_count: tracks.len() as u32,
        duration_ms: tracks.iter().map(|t| t.duration_ms).sum(),
        color: cover.and_then(|c| c.1.clone()),
        has_cover: cover.is_some_and(|c| c.0.is_some()),
        added_at: added,
        format: consensus(tracks.iter().map(|t| t.format.as_str()))
            .unwrap_or_default()
            .to_string(),
        collection: tracks.first().map(|t| t.collection).unwrap_or_default(),
        sort_values: HashMap::new(),
        filter_values: HashMap::new(),
        attention: None,
    }
}

/// Filter keys whose values come from the album summary, as the grid shows
/// it: the album artist (or the artist it falls back to) and the year.
const SUMMARY_FILTERS: &[&str] = &["album_artist", "date"];

/// Every value of `key` an album has, in the order found and without repeats;
/// a value holding several is split at `separator`. `tracks` are the
/// album's stored tag values.
fn filter_values<'a>(
    key: &str,
    album: &Album,
    tracks: impl Iterator<Item = &'a HashMap<String, String>>,
    separator: &str,
) -> Vec<String> {
    let found: Vec<String> = match key {
        "album_artist" => vec![album.artist.clone()],
        "date" => album.year.map(|y| y.to_string()).into_iter().collect(),
        _ => tracks.filter_map(|v| v.get(key).cloned()).collect(),
    };
    let mut out: Vec<String> = Vec::new();
    for value in &found {
        let parts: Vec<&str> = if separator.trim().is_empty() {
            vec![value]
        } else {
            value.split(separator).collect()
        };
        for part in parts.into_iter().map(str::trim).filter(|p| !p.is_empty()) {
            if !out.iter().any(|o| o == part) {
                out.push(part.to_string());
            }
        }
    }
    out
}

fn sort_tracks(tracks: &mut [Track]) {
    tracks.sort_by(|a, b| {
        (
            a.disc_no.unwrap_or(1),
            a.track_no.unwrap_or(u32::MAX),
            &a.rel_path,
        )
            .cmp(&(
                b.disc_no.unwrap_or(1),
                b.track_no.unwrap_or(u32::MAX),
                &b.rel_path,
            ))
    });
}

/// A track as album listings load it.
struct TrackRow {
    track: Track,
    dir: String,
    /// When the file was added, as its collection counts it.
    added: i64,
    /// Tag values as stored (JSON object).
    values: String,
    /// Comma-separated kinds the file has.
    keys: String,
}

/// The kinds each track of an album (`idx` into `rows`) would have once
/// imported: what it has, plus whatever `autofill` fills in.
fn after_autofill<'a>(
    rows: &[TrackRow],
    idx: &[usize],
    values: &[HashMap<String, String>],
    present: &[HashSet<&'a str>],
    autofill: Option<&Autofill>,
) -> Vec<HashSet<&'a str>> {
    let mut have: Vec<HashSet<&str>> = idx.iter().map(|i| present[*i].clone()).collect();
    if let Some(opts) = autofill {
        let name = |p: &str| p.rsplit('/').next().unwrap_or(p).to_string();
        let mut infos: Vec<TrackInfo> = idx
            .iter()
            .map(|i| {
                let rel = &rows[*i].track.rel_path;
                let file = name(rel);
                let parent = rel
                    .rsplit_once('/')
                    .map(|(dir, _)| name(dir))
                    .unwrap_or_default();
                TrackInfo {
                    stem: file
                        .rsplit_once('.')
                        .map(|(s, _)| s.to_string())
                        .unwrap_or(file),
                    folder: parent,
                    values: values[*i]
                        .iter()
                        .filter_map(|(k, v)| Some((rules::kind(k)?.id, v.clone())))
                        .collect(),
                }
            })
            .collect();
        let album_folder = name(&rows[idx[0]].dir);
        for (h, filled) in have
            .iter_mut()
            .zip(autofill::fill(opts, &album_folder, &mut infos))
        {
            h.extend(filled);
        }
    }
    have
}

/// What an album still lacks after autofill, if anything.
fn attention(have: &[HashSet<&str>], required: &[&str]) -> Option<Attention> {
    let lacking = have
        .iter()
        .filter(|h| required.iter().any(|k| !h.contains(k)))
        .count() as u32;
    (lacking > 0).then(|| Attention {
        missing: required
            .iter()
            .filter(|k| have.iter().any(|h| !h.contains(*k)))
            .map(|k| k.to_string())
            .collect(),
        tracks: lacking,
    })
}

/// Bring an older database up to the current schema (tracked in `user_version`).
fn migrate(conn: &Connection) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version < 1 {
        // v1 records which tag kinds each file has. Existing rows predate it,
        // so mark them stale and let the next scan re-read every file.
        let has_column = conn
            .prepare("SELECT 1 FROM pragma_table_info('tracks') WHERE name = 'tag_keys'")?
            .exists([])?;
        if !has_column {
            conn.execute_batch(
                "ALTER TABLE tracks ADD COLUMN tag_keys TEXT NOT NULL DEFAULT '';
                 UPDATE tracks SET mtime = -1;",
            )?;
        }
        conn.execute_batch("PRAGMA user_version = 1")?;
    }
    if version < 2 {
        // v2 indexes the Library folder too; everything indexed before was Inbox.
        let has_column = conn
            .prepare("SELECT 1 FROM pragma_table_info('tracks') WHERE name = 'collection'")?
            .exists([])?;
        if !has_column {
            conn.execute_batch(
                "ALTER TABLE tracks ADD COLUMN collection TEXT NOT NULL DEFAULT 'inbox'",
            )?;
        }
        conn.execute_batch("PRAGMA user_version = 2")?;
    }
    if version < 3 {
        // v3 keeps every tag's value so any tag can be a sort tab; existing
        // rows predate it, so the next scan re-reads every file.
        let has_column = conn
            .prepare("SELECT 1 FROM pragma_table_info('tracks') WHERE name = 'tag_values'")?
            .exists([])?;
        if !has_column {
            conn.execute_batch(
                "ALTER TABLE tracks ADD COLUMN tag_values TEXT NOT NULL DEFAULT '{}';
                 UPDATE tracks SET mtime = -1;",
            )?;
        }
        conn.execute_batch("PRAGMA user_version = 3")?;
    }
    if version < 4 {
        // v4 moves the Library's index into the Library folder's own
        // database; the next scan reads its files into that one.
        conn.execute_batch(
            "DELETE FROM tracks WHERE collection = 'library';
             DELETE FROM covers WHERE album_id NOT IN (SELECT album_id FROM tracks);
             PRAGMA user_version = 4;",
        )?;
    }
    Ok(())
}

/// Once a Library's own database is attached: take its setup over the copy
/// kept locally, unless that copy holds edits saved while the database
/// wasn't open; a new database starts from the local copy.
fn adopt_or_store_setup(conn: &Connection, mirror: &LibrarySetup) -> Result<()> {
    let pending = get_setting::<bool>(conn, "main", PENDING)?.unwrap_or(false);
    let own = get_setting::<LibrarySetup>(conn, "lib", "library_setup")?;
    match own {
        Some(own) if !pending => {
            let setup = LibrarySetup {
                folder: mirror.folder.clone(),
                ..own
            };
            if setup != *mirror {
                tracing::info!("using the Library's own setup");
                put_setting(conn, "main", "library_setup", &setup)?;
            }
        }
        // One a newer build wrote is kept, even though this one can't read it.
        None if !pending && raw_setting(conn, "lib", "library_setup")?.is_some() => {}
        _ => put_setting(conn, "lib", "library_setup", &own_setup(mirror))?,
    }
    if pending {
        conn.execute("DELETE FROM main.settings WHERE key = ?1", [PENDING])?;
    }
    Ok(())
}

/// Local setting: Library setup was saved for the same folder while its
/// database wasn't open, so the local copy wins when it is.
const PENDING: &str = "library_setup_pending";

/// An upsert into `table` of the `head` columns, the data columns, then the
/// `kept` columns, which an existing row only takes when the parameter after
/// all of them is true.
fn upsert_sql(table: &str, head: &[&str], kept: &[&str]) -> String {
    let cols: Vec<&str> = head
        .iter()
        .chain(DATA_COLUMNS)
        .chain(kept)
        .copied()
        .collect();
    let marks: Vec<String> = (1..=cols.len()).map(|i| format!("?{i}")).collect();
    let replace = cols.len() + 1;
    let set: Vec<String> = cols
        .iter()
        .filter(|c| **c != "path")
        .map(|c| match kept.contains(c) {
            true => format!("{c} = CASE WHEN ?{replace} THEN excluded.{c} ELSE {c} END"),
            false => format!("{c} = excluded.{c}"),
        })
        .collect();
    format!(
        "INSERT INTO {table} ({}) VALUES ({}) ON CONFLICT (path) DO UPDATE SET {}",
        cols.join(", "),
        marks.join(", "),
        set.join(", ")
    )
}

impl Library {
    /// Open the local database in `state_dir`, taking over the one from
    /// before the Library had its own, then attach the Library folder's.
    pub fn open_in(state_dir: &Path) -> Result<Self> {
        let path = state_dir.join(LOCAL_DB);
        let legacy = state_dir.join(LEGACY_DB);
        let migrated = !path.exists() && legacy.exists();
        if migrated {
            // Fold the write-ahead log into the file first, so a single rename
            // moves everything and can't leave committed changes behind.
            let conn = Connection::open(&legacy)?;
            let mode: String = conn.query_row("PRAGMA journal_mode = DELETE", [], |r| r.get(0))?;
            drop(conn);
            if !mode.eq_ignore_ascii_case("delete") {
                return Err(Error::Invalid(format!(
                    "Couldn't checkpoint {} before renaming it (journal mode {mode}).",
                    legacy.display()
                )));
            }
            std::fs::rename(&legacy, &path)?;
            tracing::info!(from = %legacy.display(), to = %path.display(), "renamed the local database");
        }
        let lib = Self::open(&path)?;
        lib.refresh_library()?;
        if migrated
            && let Some(folder) = lib.library_setup()?.folder
            && Path::new(&folder).is_dir()
        {
            // That folder already was this installation's Library: keep its
            // index in its own database rather than re-reading it every start.
            if let Err(e) = lib.prepare_library(Path::new(&folder)) {
                tracing::warn!(folder = %folder, "couldn't create the Library database: {e}");
            }
        }
        Ok(lib)
    }

    /// Open a local database, with no Library folder attached yet (see
    /// [`Library::refresh_library`]).
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        conn.execute_batch(&temp_schema())?;
        attach_memory(&conn, None, None)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Follow Library setup's folder: attach its own database if it has one,
    /// or else index it in memory. The database is only created by an import
    /// (see [`Library::prepare_library`]), so choosing a folder by mistake
    /// leaves nothing behind in it. Run this exclusively with scans, imports
    /// and tag writes, since it can switch the Library they work on.
    pub fn refresh_library(&self) -> Result<()> {
        self.follow_library(false)
    }

    /// Before an import: create the Library folder and its database if need
    /// be, and fail, before anything is copied, if the database can't be kept
    /// there.
    pub fn prepare_library(&self, folder: &Path) -> Result<()> {
        std::fs::create_dir_all(folder)?;
        self.follow_library(true)?;
        let conn = self.conn();
        let here = folder.canonicalize()?;
        match attached(&conn)? {
            Some(a) if a.stored && a.root == here => Ok(()),
            Some(Attached {
                problem: Some(p), ..
            }) => Err(Error::Invalid(format!(
                "coversync can't keep the Library's database in {}: {p}",
                folder.display()
            ))),
            _ => Err(Error::Invalid(format!(
                "{} isn't the Library folder Library setup names.",
                folder.display()
            ))),
        }
    }

    /// Why the Library folder can't hold its database, if it can't: it is
    /// then indexed for this session only, and imports are refused.
    pub fn library_problem(&self) -> Result<Option<String>> {
        Ok(attached(&self.conn())?.and_then(|a| a.problem))
    }

    fn follow_library(&self, create: bool) -> Result<()> {
        let conn = self.conn();
        let mirror: LibrarySetup = get_setting(&conn, "main", "library_setup")?.unwrap_or_default();
        let target = existing_dir(mirror.folder.as_deref());
        let current = attached(&conn)?;
        let Some(dir) = target else {
            if current.is_some() {
                attach_memory(&conn, None, None)?;
            }
            return Ok(());
        };
        let has_db = dir.join(LIBRARY_DB).is_file();
        let carry = match current {
            Some(a) if a.root == dir => {
                // Indexed in memory: move to the folder's database once there
                // is one to open, or one is wanted. One that failed is tried
                // again, in case it has been fixed.
                if a.stored || !(create || has_db) {
                    return Ok(());
                }
                Some(a.prefix)
            }
            _ => None,
        };
        if create || has_db {
            match attach_file(&conn, &dir, carry) {
                Ok(created) => {
                    adopt_or_store_setup(&conn, &mirror)?;
                    tracing::info!(folder = %dir.display(), created, "Library database attached");
                    return Ok(());
                }
                Err(e) => {
                    tracing::warn!(folder = %dir.display(), "Library database unavailable: {e}");
                    let problem = e.to_string();
                    if carry.is_some() {
                        conn.execute("UPDATE temp.lib_root SET problem = ?1", [&problem])?;
                    } else {
                        attach_memory(&conn, Some(&dir), Some(&problem))?;
                    }
                    return Ok(());
                }
            }
        }
        attach_memory(&conn, Some(&dir), None)?;
        Ok(())
    }

    /// Tracks in a coversync Library database in `folder`, if it holds one.
    /// Reads it without attaching it, so any folder can be checked.
    pub fn library_in(folder: &Path) -> Option<u64> {
        let db = folder.join(LIBRARY_DB);
        if !db.is_file() {
            return None;
        }
        let count = Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .and_then(|c| c.query_row("SELECT COUNT(*) FROM tracks", [], |r| r.get::<_, i64>(0)));
        match count {
            Ok(n) => Some(n as u64),
            Err(e) => {
                tracing::debug!(db = %db.display(), "can't read Library database: {e}");
                None
            }
        }
    }

    /// Path -> (mtime, size, collection) for every indexed file.
    pub fn file_stats(&self) -> Result<HashMap<PathBuf, (i64, u64, Collection)>> {
        let conn = self.conn();
        let mut stmt =
            conn.prepare("SELECT path, mtime, size, collection FROM temp.indexed_tracks")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                PathBuf::from(r.get::<_, String>(0)?),
                (
                    r.get(1)?,
                    r.get::<_, i64>(2)? as u64,
                    Collection::parse(&r.get::<_, String>(3)?),
                ),
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Store indexed files, each in its collection's database. Library files
    /// are stored relative to the folder they were found in, which must be
    /// the attached one: a scan of a folder Library setup has since moved
    /// away from leaves its files out.
    pub fn upsert(&self, batch: &[IndexedFile]) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let library = library_root(&tx)?;
        let mut attached: HashMap<&Path, bool> = HashMap::new();
        {
            let mut inbox = tx.prepare_cached(&upsert_sql(
                "main.tracks",
                &["path", "root", "album_dir", "collection"],
                &[],
            ))?;
            let mut lib = tx.prepare_cached(&upsert_sql(
                "lib.tracks",
                &["path", "album_dir"],
                &["added_at"],
            ))?;
            for (stat, album_id, album_dir, t) in batch {
                let title = t.title.clone().unwrap_or_else(|| {
                    stat.path
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default()
                });
                let artist = t.artist.clone().unwrap_or_else(|| "Unknown Artist".into());
                let album = t.album.clone().unwrap_or_else(|| {
                    album_dir
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default()
                });
                let size = stat.size as i64;
                let duration = t.duration_ms as i64;
                let keys = t.tag_keys.join(",");
                let values = serde_json::to_string(&t.values).expect("tag values serialize");
                let data: [&dyn ToSql; 21] = [
                    &stat.mtime,
                    &size,
                    album_id,
                    &title,
                    &artist,
                    &album,
                    &t.album_artist,
                    &t.track_no,
                    &t.track_total,
                    &t.disc_no,
                    &t.disc_total,
                    &t.year,
                    &t.genre,
                    &duration,
                    &t.sample_rate,
                    &t.bit_depth,
                    &t.channels,
                    &t.bitrate,
                    &t.format,
                    &keys,
                    &values,
                ];
                debug_assert_eq!(data.len(), DATA_COLUMNS.len());
                match stat.collection {
                    Collection::Inbox => {
                        let path = stat.path.to_string_lossy();
                        let root = stat.root.to_string_lossy();
                        let dir = album_dir.to_string_lossy();
                        let head: [&dyn ToSql; 4] = [&path, &root, &dir, &"inbox"];
                        inbox.execute(params_from_iter(head.iter().chain(&data)))?;
                    }
                    Collection::Library => {
                        let here = *attached.entry(stat.root.as_path()).or_insert_with(|| {
                            library.is_some() && stat.root.canonicalize().ok() == library
                        });
                        let (true, Ok(rel), Ok(rel_dir)) = (
                            here,
                            stat.path.strip_prefix(&stat.root),
                            album_dir.strip_prefix(&stat.root),
                        ) else {
                            tracing::debug!(path = %stat.path.display(), "not in the attached Library folder");
                            continue;
                        };
                        let path = rel.to_string_lossy();
                        let dir = rel_dir.to_string_lossy();
                        let head: [&dyn ToSql; 2] = [&path, &dir];
                        // An import's time replaces what a scan may have
                        // recorded; otherwise the first time recorded stays.
                        let added = stat.added.unwrap_or(stat.mtime);
                        let tail: [&dyn ToSql; 2] = [&added, &stat.added.is_some()];
                        lib.execute(params_from_iter(head.iter().chain(&data).chain(&tail)))?;
                    }
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Forget files, from whichever database holds them.
    pub fn remove_paths(&self, paths: &[PathBuf]) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let library = library_root(&tx)?;
        {
            let mut inbox = tx.prepare_cached("DELETE FROM main.tracks WHERE path = ?1")?;
            let mut lib = tx.prepare_cached("DELETE FROM lib.tracks WHERE path = ?1")?;
            for p in paths {
                inbox.execute([p.to_string_lossy()])?;
                if let Some(rel) = library.as_deref().and_then(|r| p.strip_prefix(r).ok()) {
                    lib.execute([rel.to_string_lossy()])?;
                }
            }
        }
        tx.execute(
            "DELETE FROM covers WHERE album_id NOT IN (SELECT album_id FROM temp.indexed_tracks)",
            [],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Visible tracks of a collection, with what album listings need beyond `Track`.
    fn all_tracks(&self, collection: Collection) -> Result<Vec<TrackRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT * FROM {} WHERE {VISIBLE}",
            source(collection)
        ))?;
        let rows = stmt.query_map([], |r| {
            let (track, dir) = track_from_row(r)?;
            Ok(TrackRow {
                track,
                dir,
                added: r.get("added_at")?,
                values: r.get("tag_values")?,
                keys: r.get("tag_keys")?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    fn covers(&self) -> Result<HashMap<String, CoverRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT album_id, source, color FROM covers")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Albums of a collection. `sort_keys` are tag kinds whose album-wide
    /// value (what most tracks agree on) each album should carry;
    /// `filter_keys` are kinds whose every value, on any track, it should
    /// carry, with values that hold several split at `separator`. Albums with
    /// tracks missing any `required` kind are flagged for attention, after
    /// `autofill` fills what it can (as an import would). `art::ART` among
    /// them requires album art.
    pub fn albums(
        &self,
        collection: Collection,
        sort_keys: &[&str],
        filter_keys: &[&str],
        separator: &str,
        required: &[&str],
        autofill: Option<&Autofill>,
    ) -> Result<Vec<Album>> {
        let rows = self.all_tracks(collection)?;
        let covers = self.covers()?;
        // Only parse the stored values when a sort tab, filter or autofill needs them.
        let needs_values = !sort_keys.is_empty()
            || filter_keys.iter().any(|k| !SUMMARY_FILTERS.contains(k))
            || (autofill.is_some() && !required.is_empty());
        let values: Vec<HashMap<String, String>> = if needs_values {
            rows.iter()
                .map(|r| serde_json::from_str(&r.values).unwrap_or_default())
                .collect()
        } else {
            Vec::new()
        };
        let present: Vec<HashSet<&str>> =
            rows.iter().map(|r| r.keys.split(',').collect()).collect();
        let mut groups: HashMap<&str, Vec<usize>> = HashMap::new();
        for (i, r) in rows.iter().enumerate() {
            groups.entry(r.track.album_id.as_str()).or_default().push(i);
        }
        let mut albums: Vec<Album> = groups
            .into_iter()
            .map(|(id, idx)| {
                let mut ts: Vec<&Track> = idx.iter().map(|i| &rows[*i].track).collect();
                ts.sort_by_key(|t| (t.disc_no.unwrap_or(1), t.track_no.unwrap_or(u32::MAX)));
                let added = album_added(collection, idx.iter().map(|i| rows[*i].added));
                let mut album = summarize(id, &ts, added, covers.get(id));
                for key in sort_keys {
                    let found = idx
                        .iter()
                        .filter_map(|i| values[*i].get(*key).map(String::as_str));
                    if let Some(v) = consensus(found) {
                        album.sort_values.insert(key.to_string(), v.to_string());
                    }
                }
                for key in filter_keys {
                    let found =
                        filter_values(key, &album, idx.iter().map(|i| &values[*i]), separator);
                    if !found.is_empty() {
                        album.filter_values.insert(key.to_string(), found);
                    }
                }
                if !required.is_empty() {
                    let mut have = after_autofill(&rows, &idx, &values, &present, autofill);
                    if album.has_cover {
                        for h in &mut have {
                            h.insert(crate::art::ART);
                        }
                    }
                    album.attention = attention(&have, required);
                }
                album
            })
            .collect();
        albums.sort_by(|a, b| {
            (a.artist.to_lowercase(), a.year, a.title.to_lowercase()).cmp(&(
                b.artist.to_lowercase(),
                b.year,
                b.title.to_lowercase(),
            ))
        });
        Ok(albums)
    }

    pub fn album(&self, id: &str) -> Result<Option<AlbumDetail>> {
        let (mut tracks, dir, added) = {
            let conn = self.conn();
            let mut stmt = conn.prepare(&format!(
                "SELECT * FROM temp.indexed_tracks WHERE album_id = ?1 AND {VISIBLE}"
            ))?;
            let mut dir = String::new();
            let mut added = Vec::new();
            let mut tracks = Vec::new();
            let mut rows = stmt.query([id])?;
            while let Some(r) = rows.next()? {
                let (t, d) = track_from_row(r)?;
                dir = d;
                added.push(r.get::<_, i64>("added_at")?);
                tracks.push(t);
            }
            (tracks, dir, added)
        };
        let Some(collection) = tracks.first().map(|t| t.collection) else {
            return Ok(None);
        };
        sort_tracks(&mut tracks);
        let covers = self.covers()?;
        let refs: Vec<&Track> = tracks.iter().collect();
        let added = album_added(collection, added.into_iter());
        let album = summarize(id, &refs, added, covers.get(id));
        Ok(Some(AlbumDetail { album, dir, tracks }))
    }

    pub fn track_path(&self, id: i64) -> Result<Option<PathBuf>> {
        Ok(self.track_root(id)?.map(|(path, _)| path))
    }

    /// A track's file and the folder it was found in.
    pub fn track_root(&self, id: i64) -> Result<Option<(PathBuf, PathBuf)>> {
        let conn = self.conn();
        let row = |r: &Row<'_>| {
            Ok((
                PathBuf::from(r.get::<_, String>(0)?),
                PathBuf::from(r.get::<_, String>(1)?),
            ))
        };
        if id >= 0 {
            return Ok(conn
                .query_row(
                    "SELECT path, root FROM main.tracks WHERE id = ?1",
                    [id],
                    row,
                )
                .optional()?);
        }
        // A Library id names its Library's prefix and the stored row (see
        // ID_SPAN); one from another Library names nothing here.
        let (prefix, row_id) = (-id / ID_SPAN, -id % ID_SPAN);
        Ok(conn
            .query_row(
                "SELECT r.root || '/' || t.path, r.root FROM lib.tracks t JOIN temp.lib_root r
                 WHERE r.prefix = ?1 AND t.id = ?2",
                [prefix, row_id],
                row,
            )
            .optional()?)
    }

    pub fn album_track_ids(&self, album_id: &str) -> Result<Vec<i64>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT id FROM temp.indexed_tracks WHERE album_id = ?1 AND {VISIBLE}"
        ))?;
        let rows = stmt.query_map([album_id], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Albums whose cover data is missing or stale.
    pub fn cover_jobs(&self) -> Result<Vec<CoverJob>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT t.album_id, t.album_dir, MIN(t.path),
                    MAX(t.mtime) || ':' || COUNT(*)
                        -- Library album ids survive a move; their cover paths don't.
                        || CASE t.collection WHEN 'library' THEN ':' || t.root ELSE '' END AS sig,
                    c.signature
             FROM temp.indexed_tracks t LEFT JOIN covers c ON c.album_id = t.album_id
             GROUP BY t.album_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                CoverJob {
                    album_id: r.get(0)?,
                    album_dir: PathBuf::from(r.get::<_, String>(1)?),
                    first_track: PathBuf::from(r.get::<_, String>(2)?),
                    signature: r.get(3)?,
                },
                r.get::<_, Option<String>>(4)?,
            ))
        })?;
        let mut jobs = Vec::new();
        for row in rows {
            let (job, existing) = row?;
            if existing.as_deref() != Some(job.signature.as_str()) {
                jobs.push(job);
            }
        }
        Ok(jobs)
    }

    pub fn set_cover(
        &self,
        album_id: &str,
        source: Option<&CoverSource>,
        color: Option<&str>,
        signature: &str,
    ) -> Result<()> {
        self.conn().execute(
            "INSERT INTO covers (album_id, source, color, signature) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (album_id) DO UPDATE SET source = excluded.source,
                color = excluded.color, signature = excluded.signature",
            params![album_id, source.map(CoverSource::encode), color, signature],
        )?;
        Ok(())
    }

    /// A setting of this installation, kept in the local database.
    fn setting<T: serde::de::DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        get_setting(&self.conn(), "main", key)
    }

    fn put_setting<T: serde::Serialize>(&self, key: &str, value: &T) -> Result<()> {
        put_setting(&self.conn(), "main", key, value)
    }

    /// Library setup: the folder from the local database, everything else
    /// from the Library's own database once one is attached (the local
    /// database keeps a copy for while none is).
    pub fn library_setup(&self) -> Result<LibrarySetup> {
        let conn = self.conn();
        let mut setup: LibrarySetup =
            get_setting(&conn, "main", "library_setup")?.unwrap_or_default();
        if attached(&conn)?.is_some_and(|a| a.stored)
            && let Some(own) = get_setting::<LibrarySetup>(&conn, "lib", "library_setup")?
        {
            setup = LibrarySetup {
                folder: setup.folder,
                ..own
            };
        }
        // A setup saved by a newer build may name kinds this one doesn't know.
        setup
            .rules
            .required_tags
            .retain(|id| rules::kind(id).is_some());
        Ok(setup)
    }

    /// Save Library setup. Pointing it at another folder that already holds
    /// a coversync Library opens that Library with its own rules, which
    /// replace the ones given; the saved setup is returned. Like
    /// [`Library::refresh_library`], run it exclusively with scans, imports
    /// and tag writes.
    pub fn set_library_setup(&self, setup: LibrarySetup) -> Result<LibrarySetup> {
        let setup = setup.normalize()?;
        {
            let conn = self.conn();
            let before: Option<LibrarySetup> = get_setting(&conn, "main", "library_setup")?;
            put_setting(&conn, "main", "library_setup", &setup)?;
            let target = existing_dir(setup.folder.as_deref());
            let open = attached(&conn)?.filter(|a| a.stored).map(|a| a.root);
            if target.is_some() && open == target {
                put_setting(&conn, "lib", "library_setup", &own_setup(&setup))?;
            } else if before.is_some_and(|b| b.folder == setup.folder) {
                // The same folder, with its database not open (the disk is
                // away, say): these edits win over its rules once it is.
                put_setting(&conn, "main", PENDING, &true)?;
            } else {
                // Another folder: a Library already there keeps its own rules.
                conn.execute("DELETE FROM main.settings WHERE key = ?1", [PENDING])?;
            }
        }
        self.refresh_library()?;
        self.library_setup()
    }

    pub fn appearance(&self) -> Result<AppearanceSetup> {
        // Tabs saved by another build may name keys this one doesn't know.
        let saved: Option<AppearanceSetup> = self.setting("appearance")?;
        Ok(saved.and_then(|s| s.normalize().ok()).unwrap_or_default())
    }

    pub fn set_appearance(&self, setup: AppearanceSetup) -> Result<AppearanceSetup> {
        let setup = setup.normalize()?;
        self.put_setting("appearance", &setup)?;
        Ok(setup)
    }

    pub fn inbox_setup(&self) -> Result<InboxSetup> {
        Ok(self.setting("inbox_setup")?.unwrap_or_default())
    }

    /// The AcoustID application API key smart suggestions use, if one is set.
    pub fn acoustid_key(&self) -> Result<Option<String>> {
        self.setting("acoustid_key")
    }

    pub fn set_acoustid_key(&self, key: Option<&str>) -> Result<()> {
        match key {
            Some(k) => self.put_setting("acoustid_key", &k),
            None => {
                self.conn()
                    .execute("DELETE FROM settings WHERE key = 'acoustid_key'", [])?;
                Ok(())
            }
        }
    }

    pub fn set_inbox_setup(&self, setup: InboxSetup) -> Result<InboxSetup> {
        self.put_setting("inbox_setup", &setup)?;
        Ok(setup)
    }

    pub fn sync_setup(&self) -> Result<SyncSetup> {
        Ok(self.setting("sync_setup")?.unwrap_or_default())
    }

    pub fn set_sync_setup(&self, setup: SyncSetup) -> Result<SyncSetup> {
        self.put_setting("sync_setup", &setup)?;
        Ok(setup)
    }

    /// Hide Inbox files, e.g. once they have been copied into the Library.
    pub fn hide_paths(&self, paths: &[PathBuf], now: i64) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO hidden (path, hidden_at) VALUES (?1, ?2) ON CONFLICT (path) DO NOTHING",
            )?;
            for p in paths {
                stmt.execute(params![p.to_string_lossy(), now])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn hidden_stats(&self) -> Result<HiddenStats> {
        let conn = self.conn();
        Ok(conn.query_row(
            "SELECT (SELECT COUNT(*) FROM hidden),
                    (SELECT COUNT(DISTINCT t.album_id) FROM main.tracks t JOIN hidden h ON h.path = t.path)",
            [],
            |r| Ok(HiddenStats { files: r.get(0)?, albums: r.get(1)? }),
        )?)
    }

    /// Show every hidden file in the Inbox again. Returns how many were hidden.
    pub fn unhide_all(&self) -> Result<u32> {
        Ok(self.conn().execute("DELETE FROM hidden", [])? as u32)
    }

    /// How many tracks and whole albums have each rule tag kind.
    /// How common each kind is in the Inbox, counting what `autofill` would
    /// fill in on import.
    /// Inbox tracks with a value longer than its limit in `limits` (kind,
    /// characters), as (album id, track id), the most shortened first.
    pub fn overlong(&self, limits: &[(&str, usize)]) -> Result<Vec<(String, i64)>> {
        let mut out: Vec<(usize, String, i64)> = self
            .all_tracks(Collection::Inbox)?
            .into_iter()
            .filter_map(|r| {
                let values: HashMap<String, String> =
                    serde_json::from_str(&r.values).unwrap_or_default();
                let cut: usize = limits
                    .iter()
                    .filter_map(|(kind, max)| {
                        let len = values.get(*kind)?.trim().chars().count();
                        Some(len.saturating_sub(*max))
                    })
                    .sum();
                (cut > 0).then_some((cut, r.track.album_id, r.track.id))
            })
            .collect();
        out.sort_by_key(|(cut, _, _)| std::cmp::Reverse(*cut));
        Ok(out
            .into_iter()
            .map(|(_, album, track)| (album, track))
            .collect())
    }

    pub fn tag_coverage(&self, autofill: Option<&Autofill>) -> Result<TagCoverage> {
        let rows = self.all_tracks(Collection::Inbox)?;
        let values: Vec<HashMap<String, String>> = if autofill.is_some() {
            rows.iter()
                .map(|r| serde_json::from_str(&r.values).unwrap_or_default())
                .collect()
        } else {
            Vec::new()
        };
        let present: Vec<HashSet<&str>> =
            rows.iter().map(|r| r.keys.split(',').collect()).collect();
        let mut groups: HashMap<&str, Vec<usize>> = HashMap::new();
        for (i, r) in rows.iter().enumerate() {
            groups.entry(r.track.album_id.as_str()).or_default().push(i);
        }
        let index: HashMap<&str, usize> = rules::CATALOG
            .iter()
            .enumerate()
            .map(|(i, k)| (k.id, i))
            .collect();
        let n = rules::CATALOG.len();
        let mut track_counts = vec![0u32; n];
        // Per album: how many tracks it has, and per kind how many have it.
        let mut albums: HashMap<String, (u32, Vec<u32>)> = HashMap::new();
        for (album_id, idx) in &groups {
            let album = albums
                .entry(album_id.to_string())
                .or_insert_with(|| (0, vec![0; n]));
            for have in after_autofill(&rows, idx, &values, &present, autofill) {
                album.0 += 1;
                for i in have.iter().filter_map(|k| index.get(k)) {
                    track_counts[*i] += 1;
                    album.1[*i] += 1;
                }
            }
        }
        let covers = self.covers()?;
        let without_art = groups
            .keys()
            .filter(|id| covers.get(**id).is_none_or(|c| c.0.is_none()))
            .count() as u32;
        let mut album_counts = vec![0u32; n];
        let album_kinds = albums
            .into_iter()
            .map(|(id, (tracks, per_kind))| {
                let complete: Vec<u16> = (0..n)
                    .filter(|i| per_kind[*i] == tracks)
                    .map(|i| i as u16)
                    .collect();
                for i in &complete {
                    album_counts[*i as usize] += 1;
                }
                (id, complete)
            })
            .collect::<HashMap<_, _>>();
        Ok(TagCoverage {
            tracks: rows.len() as u32,
            albums: album_kinds.len() as u32,
            kinds: rules::CATALOG
                .iter()
                .enumerate()
                .map(|(i, k)| KindCoverage {
                    id: k.id,
                    tracks: track_counts[i],
                    albums: album_counts[i],
                })
                .collect(),
            album_kinds,
            without_art,
        })
    }

    pub fn cover_source(&self, album_id: &str) -> Result<Option<CoverSource>> {
        let conn = self.conn();
        let s: Option<Option<String>> = conn
            .query_row(
                "SELECT source FROM covers WHERE album_id = ?1",
                [album_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(s.flatten().as_deref().and_then(CoverSource::decode))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::Rules;
    use crate::tags::TrackTags;

    fn temp_db(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("coversync-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("library.sqlite3")
    }

    fn file(path: &str, album: &str, keys: &[&'static str]) -> IndexedFile {
        let stat = FileStat {
            path: path.into(),
            root: "/m".into(),
            mtime: 1,
            size: 1,
            collection: Collection::Inbox,
            added: None,
        };
        let tags = TrackTags {
            tag_keys: keys.to_vec(),
            format: "FLAC".into(),
            ..Default::default()
        };
        (stat, album.into(), PathBuf::from("/m/a"), tags)
    }

    #[test]
    fn migrating_v0_marks_rows_for_rescan() {
        let path = temp_db("migrate");
        let conn = Connection::open(&path).unwrap();
        // The v0 table had neither tag_keys nor collection.
        let v0 = SCHEMA
            .replace(",\n    tag_keys     TEXT NOT NULL DEFAULT ''", "")
            .replace(",\n    collection   TEXT NOT NULL DEFAULT 'inbox'", "")
            .replace(",\n    tag_values   TEXT NOT NULL DEFAULT '{}'", "");
        assert!(
            !v0.contains("tag_keys") && !v0.contains("collection") && !v0.contains("tag_values")
        );
        conn.execute_batch(&v0).unwrap();
        conn.execute(
            "INSERT INTO tracks (path, root, mtime, size, album_id, album_dir, title, artist, album,
                duration_ms, format) VALUES ('/m/a/1.flac', '/m', 5, 1, 'a', '/m/a', 't', 'a', 'b', 0, 'FLAC')",
            [],
        )
        .unwrap();
        drop(conn);
        let lib = Library::open(&path).unwrap();
        let stats = lib.file_stats().unwrap();
        let row = stats[Path::new("/m/a/1.flac")];
        assert_eq!((row.0, row.2), (-1, Collection::Inbox));
        let version: i64 = lib
            .conn()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 4);
    }

    #[test]
    fn albums_carry_requested_sort_values() {
        let lib = Library::open(&temp_db("sortvalues")).unwrap();
        let mut a = file("/m/a/1.flac", "a", &["label"]);
        a.3.values.insert("label", "Maltine".into());
        let mut b = file("/m/a/2.flac", "a", &["label"]);
        b.3.values.insert("label", "Maltine".into());
        let c = file("/m/b/1.flac", "b", &[]);
        lib.upsert(&[a, b, c]).unwrap();
        let albums = lib
            .albums(Collection::Inbox, &["label"], &[], "", &[], None)
            .unwrap();
        let get = |id: &str| albums.iter().find(|x| x.id == id).unwrap();
        assert_eq!(
            get("a").sort_values.get("label").map(String::as_str),
            Some("Maltine")
        );
        assert!(get("b").sort_values.is_empty());
        assert!(
            lib.albums(Collection::Inbox, &[], &[], "", &[], None)
                .unwrap()
                .iter()
                .all(|x| x.sort_values.is_empty())
        );
    }

    #[test]
    fn albums_carry_every_filter_value() {
        let lib = Library::open(&temp_db("filtervalues")).unwrap();
        let mut a = file("/m/a/1.flac", "a", &["genre", "artist"]);
        a.3.values.insert("genre", "Rock; Pop".into());
        a.3.values.insert("artist", "Kano".into());
        a.3.artist = Some("Kano".into());
        a.3.year = Some(2003);
        let mut b = file("/m/a/2.flac", "a", &["genre", "artist"]);
        b.3.values.insert("genre", "Pop;Jazz;".into());
        b.3.values.insert("artist", "Guest".into());
        b.3.artist = Some("Kano".into());
        b.3.year = Some(2005);
        let c = file("/m/b/1.flac", "b", &[]);
        lib.upsert(&[a, b, c]).unwrap();
        let keys = ["genre", "artist", "album_artist", "date"];
        let albums = lib
            .albums(Collection::Inbox, &[], &keys, ";", &[], None)
            .unwrap();
        let get = |id: &str| &albums.iter().find(|x| x.id == id).unwrap().filter_values;
        let values = |id: &str, key: &str| get(id).get(key).cloned().unwrap_or_default();
        // Every track's values, split at the separator, without repeats.
        assert_eq!(values("a", "genre"), ["Rock", "Pop", "Jazz"]);
        assert_eq!(values("a", "artist"), ["Kano", "Guest"]);
        // Album artist and year are the ones the grid shows.
        assert_eq!(values("a", "album_artist"), ["Kano"]);
        assert_eq!(values("a", "date"), ["2003"]);
        // An untagged album only has the artist the grid shows for it.
        assert_eq!(values("b", "album_artist"), ["Unknown Artist"]);
        assert_eq!(get("b").len(), 1);
        // Another separator leaves the values whole.
        let whole = lib
            .albums(Collection::Inbox, &[], &["genre"], " / ", &[], None)
            .unwrap();
        let a = whole.iter().find(|x| x.id == "a").unwrap();
        assert_eq!(a.filter_values["genre"], ["Rock; Pop", "Pop;Jazz;"]);
    }

    #[test]
    fn albums_missing_required_tags_need_attention() {
        let lib = Library::open(&temp_db("attention")).unwrap();
        lib.upsert(&[
            file("/m/a/1.flac", "a", &["title", "genre"]),
            file("/m/a/2.flac", "a", &["title"]),
            file("/m/b/1.flac", "b", &["title", "genre"]),
        ])
        .unwrap();
        let albums = lib
            .albums(Collection::Inbox, &[], &[], "", &["title", "genre"], None)
            .unwrap();
        let get = |id: &str| {
            albums
                .iter()
                .find(|x| x.id == id)
                .unwrap()
                .attention
                .clone()
        };
        assert_eq!(
            get("a"),
            Some(Attention {
                missing: vec!["genre".into()],
                tracks: 1
            })
        );
        assert_eq!(get("b"), None);
        // With autofill, a single-artist album gets its album artist assumed.
        let mut c = file("/m/c/01 Song.flac", "c", &["title", "artist"]);
        c.3.values.insert("artist", "Kano".into());
        c.3.values.insert("title", "Song".into());
        lib.upsert(&[c]).unwrap();
        let req = ["title", "album_artist"];
        let plain = lib
            .albums(Collection::Inbox, &[], &[], "", &req, None)
            .unwrap();
        let filled = lib
            .albums(
                Collection::Inbox,
                &[],
                &[],
                "",
                &req,
                Some(&Autofill::default()),
            )
            .unwrap();
        let flag =
            |list: &[Album], id: &str| list.iter().find(|x| x.id == id).unwrap().attention.clone();
        assert!(flag(&plain, "c").is_some());
        assert_eq!(flag(&filled, "c"), None);
        let none = lib
            .albums(Collection::Inbox, &[], &[], "", &[], None)
            .unwrap();
        assert!(none.iter().all(|x| x.attention.is_none()));
    }

    #[test]
    fn albums_without_art_need_attention_when_art_is_required() {
        let lib = Library::open(&temp_db("art")).unwrap();
        lib.upsert(&[
            file("/m/a/1.flac", "a", &["title"]),
            file("/m/b/1.flac", "b", &[]),
        ])
        .unwrap();
        let cover = CoverSource::File("/m/a/folder.jpg".into());
        lib.set_cover("a", Some(&cover), Some("#000000"), "sig")
            .unwrap();
        lib.set_cover("b", None, None, "sig").unwrap();
        let albums = lib
            .albums(
                Collection::Inbox,
                &[],
                &[],
                "",
                &["title", crate::art::ART],
                None,
            )
            .unwrap();
        let get = |id: &str| {
            albums
                .iter()
                .find(|x| x.id == id)
                .unwrap()
                .attention
                .clone()
        };
        assert_eq!(get("a"), None);
        assert_eq!(
            get("b"),
            Some(Attention {
                missing: vec!["title".into(), crate::art::ART.into()],
                tracks: 1
            })
        );
        assert_eq!(lib.tag_coverage(None).unwrap().without_art, 1);
    }

    #[test]
    fn coverage_counts_tracks_and_complete_albums() {
        let lib = Library::open(&temp_db("coverage")).unwrap();
        lib.upsert(&[
            file("/m/a/1.flac", "a", &["title", "genre"]),
            file("/m/a/2.flac", "a", &["title"]),
            file("/m/b/1.flac", "b", &["title", "genre"]),
        ])
        .unwrap();
        let cov = lib.tag_coverage(None).unwrap();
        let get = |id: &str| cov.kinds.iter().find(|k| k.id == id).unwrap();
        assert_eq!((cov.tracks, cov.albums), (3, 2));
        assert_eq!((get("title").tracks, get("title").albums), (3, 2));
        assert_eq!((get("genre").tracks, get("genre").albums), (2, 1));
        let genre = cov.kinds.iter().position(|k| k.id == "genre").unwrap() as u16;
        assert!(!cov.album_kinds["a"].contains(&genre));
        assert!(cov.album_kinds["b"].contains(&genre));
        // Autofill counts too: b's one track becomes track 1 of 1.
        let track = cov
            .kinds
            .iter()
            .position(|k| k.id == "track_number")
            .unwrap() as u16;
        assert!(!cov.album_kinds["b"].contains(&track));
        let filled = lib.tag_coverage(Some(&Autofill::default())).unwrap();
        assert_eq!(filled.tracks, 3);
        assert!(filled.album_kinds["b"].contains(&track));
    }

    #[test]
    fn setup_round_trips_normalized() {
        let lib = Library::open(&temp_db("setup")).unwrap();
        assert_eq!(lib.library_setup().unwrap(), LibrarySetup::default());
        let saved = lib
            .set_library_setup(LibrarySetup {
                rules: Rules {
                    required_tags: vec!["genre".into(), "title".into()],
                    ..Default::default()
                },
                ..Default::default()
            })
            .unwrap();
        assert_eq!(saved.rules.required_tags, ["title", "genre"]);
        assert_eq!(lib.library_setup().unwrap(), saved);
    }

    #[test]
    fn hidden_files_leave_the_inbox_until_unhidden() {
        let lib = Library::open(&temp_db("hidden")).unwrap();
        lib.upsert(&[
            file("/m/a/1.flac", "a", &["title"]),
            file("/m/a/2.flac", "a", &["title"]),
            file("/m/b/1.flac", "b", &["title"]),
        ])
        .unwrap();
        lib.hide_paths(
            &[
                "/m/a/1.flac".into(),
                "/m/a/2.flac".into(),
                "/m/gone.flac".into(),
            ],
            1,
        )
        .unwrap();
        assert_eq!(
            lib.hidden_stats().unwrap(),
            HiddenStats {
                files: 3,
                albums: 1
            }
        );
        assert_eq!(
            lib.albums(Collection::Inbox, &[], &[], "", &[], None)
                .unwrap()
                .len(),
            1
        );
        assert!(lib.album("a").unwrap().is_none());
        assert_eq!(lib.tag_coverage(None).unwrap().tracks, 1);
        // Hiding is by path, so it survives the file being re-indexed.
        lib.upsert(&[file("/m/a/1.flac", "a", &["title"])]).unwrap();
        assert_eq!(
            lib.albums(Collection::Inbox, &[], &[], "", &[], None)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(lib.unhide_all().unwrap(), 3);
        assert_eq!(
            lib.albums(Collection::Inbox, &[], &[], "", &[], None)
                .unwrap()
                .len(),
            2
        );
        assert_eq!(lib.hidden_stats().unwrap(), HiddenStats::default());
    }

    #[test]
    fn inbox_setup_defaults_to_copy_and_round_trips() {
        use crate::inbox::IngestMode;
        let lib = Library::open(&temp_db("inbox")).unwrap();
        assert_eq!(lib.inbox_setup().unwrap().ingest, IngestMode::Copy);
        lib.set_inbox_setup(InboxSetup {
            ingest: IngestMode::Move,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(lib.inbox_setup().unwrap().ingest, IngestMode::Move);
    }

    #[test]
    fn sync_setup_defaults_to_on_and_round_trips() {
        let lib = Library::open(&temp_db("sync")).unwrap();
        assert!(lib.sync_setup().unwrap().enabled);
        lib.set_sync_setup(SyncSetup { enabled: false }).unwrap();
        assert!(!lib.sync_setup().unwrap().enabled);
    }

    #[test]
    fn consensus_prefers_majority_then_first() {
        assert_eq!(consensus(["a", "b", "b"].into_iter()), Some("b"));
        assert_eq!(consensus(["a", "b"].into_iter()), Some("a"));
        assert_eq!(consensus(std::iter::empty()), None);
    }

    /// A fresh folder for a test, holding `parts` as empty folders.
    fn temp_dir(name: &str, parts: &[&str]) -> PathBuf {
        let dir = temp_db(name).parent().unwrap().canonicalize().unwrap();
        for p in parts {
            std::fs::create_dir_all(dir.join(p)).unwrap();
        }
        dir
    }

    /// A Library file at `rel` under `root`.
    fn library_file(root: &Path, rel: &str, album: &str, mtime: i64) -> IndexedFile {
        let path = root.join(rel);
        let dir = path.parent().unwrap().to_path_buf();
        let stat = FileStat {
            path,
            root: root.to_path_buf(),
            mtime,
            size: 1,
            collection: Collection::Library,
            added: None,
        };
        let tags = TrackTags {
            album: Some(album.into()),
            format: "FLAC".into(),
            ..Default::default()
        };
        let id = crate::scan::album_id_in(Collection::Library, root, &dir, Some(album));
        (stat, id, dir, tags)
    }

    fn with_folder(folder: &Path, required: &[&str]) -> LibrarySetup {
        LibrarySetup {
            folder: Some(folder.display().to_string()),
            rules: Rules {
                required_tags: required.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn library_tracks_live_in_the_library_folder() {
        let dir = temp_dir("libdb", &["state", "Lib"]);
        let root = dir.join("Lib");
        let lib = Library::open_in(&dir.join("state")).unwrap();
        lib.set_library_setup(with_folder(&root, &["title", "genre"]))
            .unwrap();
        lib.prepare_library(&root).unwrap();
        lib.upsert(&[
            library_file(&root, "A/1.flac", "A", 7),
            file("/m/a/1.flac", "a", &["title"]),
        ])
        .unwrap();

        let albums = lib
            .albums(Collection::Library, &[], &[], "", &[], None)
            .unwrap();
        assert_eq!(albums.len(), 1);
        let detail = lib.album(&albums[0].id).unwrap().unwrap();
        assert_eq!(detail.dir, root.join("A").display().to_string());
        let id = detail.tracks[0].id;
        assert!(id < -ID_SPAN, "Library ids carry a prefix: {id}");
        assert!(id > -(1 << 53), "and stay exact in JavaScript: {id}");
        assert_eq!(detail.tracks[0].rel_path, "A/1.flac");
        assert_eq!(lib.track_path(id).unwrap(), Some(root.join("A/1.flac")));
        assert_eq!(lib.album_track_ids(&albums[0].id).unwrap(), [id]);
        assert_eq!(
            lib.file_stats().unwrap()[&root.join("A/1.flac")],
            (7, 1, Collection::Library)
        );
        assert_eq!(
            lib.albums(Collection::Inbox, &[], &[], "", &[], None)
                .unwrap()
                .len(),
            1
        );

        // The local database holds only the Inbox; the Library's stores
        // paths relative to its folder, and its own setup.
        let local: i64 = lib
            .conn()
            .query_row("SELECT COUNT(*) FROM main.tracks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(local, 1);
        drop(lib);
        let own = Connection::open(root.join(LIBRARY_DB)).unwrap();
        let row: (String, String) = own
            .query_row("SELECT path, album_dir FROM tracks", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(row, ("A/1.flac".into(), "A".into()));
        let setup: String = own
            .query_row(
                "SELECT value FROM settings WHERE key = 'library_setup'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let setup: LibrarySetup = serde_json::from_str(&setup).unwrap();
        assert_eq!(setup.folder, None);
        assert_eq!(setup.rules.required_tags, ["title", "genre"]);
        assert_eq!(Library::library_in(&root), Some(1));
        assert_eq!(Library::library_in(&dir.join("state")), None);
    }

    #[test]
    fn library_albums_count_as_added_when_imported() {
        let dir = temp_dir("libadded", &["state", "Lib"]);
        let root = dir.join("Lib");
        let lib = Library::open_in(&dir.join("state")).unwrap();
        lib.set_library_setup(with_folder(&root, &[])).unwrap();
        lib.prepare_library(&root).unwrap();
        let imported = |rel: &str, album: &str, mtime: i64, at: i64| {
            let mut f = library_file(&root, rel, album, mtime);
            f.0.added = Some(at);
            f
        };
        let added = |title: &str| {
            let albums = lib
                .albums(Collection::Library, &[], &[], "", &[], None)
                .unwrap();
            let album = albums.iter().find(|a| a.title == title).unwrap();
            let detail = lib.album(&album.id).unwrap().unwrap();
            assert_eq!(detail.album.added_at, album.added_at);
            album.added_at
        };

        lib.upsert(&[imported("A/1.flac", "A", 100, 500)]).unwrap();
        assert_eq!(added("A"), 500);
        // A tag edit rewrites the file; the album was still added at the import.
        lib.upsert(&[library_file(&root, "A/1.flac", "A", 900)])
            .unwrap();
        assert_eq!(added("A"), 500);
        // A track imported into the album later doesn't make it new.
        lib.upsert(&[imported("A/2.flac", "A", 800, 800)]).unwrap();
        assert_eq!(added("A"), 500);
        // Files found in the folder by a scan count from their modification time.
        lib.upsert(&[library_file(&root, "B/1.flac", "B", 300)])
            .unwrap();
        assert_eq!(added("B"), 300);

        // The Inbox goes by its newest file.
        lib.upsert(&[file("/m/a/1.flac", "a", &[]), {
            let mut f = file("/m/a/2.flac", "a", &[]);
            f.0.mtime = 40;
            f
        }])
        .unwrap();
        let inbox = lib
            .albums(Collection::Inbox, &[], &[], "", &[], None)
            .unwrap();
        assert_eq!(inbox[0].added_at, 40);
    }

    #[test]
    fn an_older_library_database_gets_added_times_from_its_files() {
        let dir = temp_dir("libv1", &["state", "Lib"]);
        let root = dir.join("Lib");
        let lib = Library::open_in(&dir.join("state")).unwrap();
        lib.set_library_setup(with_folder(&root, &[])).unwrap();
        lib.prepare_library(&root).unwrap();
        lib.upsert(&[library_file(&root, "A/1.flac", "A", 70)])
            .unwrap();
        drop(lib);
        // As the first version of the Library database left it.
        let own = Connection::open(root.join(LIBRARY_DB)).unwrap();
        own.execute_batch(
            "ALTER TABLE tracks DROP COLUMN added_at;
             UPDATE tracks SET mtime = 70;
             PRAGMA user_version = 1;",
        )
        .unwrap();
        drop(own);

        let lib = Library::open_in(&dir.join("state")).unwrap();
        let albums = lib
            .albums(Collection::Library, &[], &[], "", &[], None)
            .unwrap();
        assert_eq!(albums[0].added_at, 70);
        let version: i64 = lib
            .conn()
            .query_row("PRAGMA lib.user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 2);
    }

    #[test]
    fn missing_added_times_are_filled_from_the_files() {
        let dir = temp_dir("libzero", &["state", "Lib"]);
        let root = dir.join("Lib");
        let lib = Library::open_in(&dir.join("state")).unwrap();
        lib.set_library_setup(with_folder(&root, &[])).unwrap();
        lib.prepare_library(&root).unwrap();
        lib.upsert(&[
            library_file(&root, "A/1.flac", "A", 70),
            library_file(&root, "B/1.flac", "B", 80),
        ])
        .unwrap();
        drop(lib);
        let added = |lib: &Library, title: &str| {
            lib.albums(Collection::Library, &[], &[], "", &[], None)
                .unwrap()
                .into_iter()
                .find(|a| a.title == title)
                .unwrap()
                .added_at
        };

        // An upgrade cut short after adding the column, and a row an older
        // coversyncd added without a time.
        let own = Connection::open(root.join(LIBRARY_DB)).unwrap();
        own.execute_batch(
            "UPDATE tracks SET added_at = 0 WHERE path = 'A/1.flac';
             PRAGMA user_version = 1;",
        )
        .unwrap();
        drop(own);
        let lib = Library::open_in(&dir.join("state")).unwrap();
        assert_eq!(added(&lib, "A"), 70);
        drop(lib);

        let own = Connection::open(root.join(LIBRARY_DB)).unwrap();
        own.execute("UPDATE tracks SET added_at = 0 WHERE path = 'B/1.flac'", [])
            .unwrap();
        drop(own);
        let lib = Library::open_in(&dir.join("state")).unwrap();
        assert_eq!(added(&lib, "B"), 80);
    }

    #[test]
    fn an_import_over_a_stale_row_takes_the_import_time() {
        let dir = temp_dir("libstale", &["state", "Lib"]);
        let root = dir.join("Lib");
        let lib = Library::open_in(&dir.join("state")).unwrap();
        lib.set_library_setup(with_folder(&root, &[])).unwrap();
        lib.prepare_library(&root).unwrap();
        lib.upsert(&[library_file(&root, "A/1.flac", "A", 70)])
            .unwrap();
        // The file was deleted by hand and the album imported again before
        // a scan noticed: the row is still there.
        let mut again = library_file(&root, "A/1.flac", "A", 900);
        again.0.added = Some(900);
        lib.upsert(&[again]).unwrap();
        let albums = lib
            .albums(Collection::Library, &[], &[], "", &[], None)
            .unwrap();
        assert_eq!(albums[0].added_at, 900);
    }

    #[test]
    fn a_moved_library_keeps_its_index_and_rules() {
        let dir = temp_dir("libmove", &["old", "new", "Lib"]);
        let root = dir.join("Lib");
        let lib = Library::open_in(&dir.join("old")).unwrap();
        lib.set_library_setup(with_folder(&root, &["title", "genre"]))
            .unwrap();
        lib.prepare_library(&root).unwrap();
        lib.upsert(&[library_file(&root, "A/1.flac", "A", 7)])
            .unwrap();
        let id = lib
            .albums(Collection::Library, &[], &[], "", &[], None)
            .unwrap()[0]
            .id
            .clone();
        drop(lib);

        // Another machine, where the folder has another path.
        let moved = dir.join("Moved");
        std::fs::rename(&root, &moved).unwrap();
        let lib = Library::open_in(&dir.join("new")).unwrap();
        let saved = lib
            .set_library_setup(with_folder(&moved, &["title"]))
            .unwrap();
        assert_eq!(saved.rules.required_tags, ["title", "genre"]);
        assert_eq!(saved.folder, Some(moved.display().to_string()));
        assert_eq!(lib.library_setup().unwrap(), saved);
        // Unchanged files keep their index rows, so a scan has nothing to re-read.
        assert_eq!(
            lib.file_stats().unwrap()[&moved.join("A/1.flac")],
            (7, 1, Collection::Library)
        );
        let albums = lib
            .albums(Collection::Library, &[], &[], "", &[], None)
            .unwrap();
        assert_eq!(albums[0].id, id);
        // A file added after the move joins the same album.
        lib.upsert(&[library_file(&moved, "A/2.flac", "A", 8)])
            .unwrap();
        let albums = lib
            .albums(Collection::Library, &[], &[], "", &[], None)
            .unwrap();
        assert_eq!((albums.len(), albums[0].track_count), (1, 2));

        // Later edits go to the Library's own setup.
        lib.set_library_setup(with_folder(&moved, &["artist"]))
            .unwrap();
        drop(lib);
        let lib = Library::open_in(&dir.join("old")).unwrap();
        let saved = lib
            .set_library_setup(with_folder(&moved, &["title"]))
            .unwrap();
        assert_eq!(saved.rules.required_tags, ["artist"]);
    }

    #[test]
    fn covers_are_found_again_after_the_library_moves() {
        let dir = temp_dir("libcovers", &["state", "Lib"]);
        let root = dir.join("Lib");
        let lib = Library::open_in(&dir.join("state")).unwrap();
        lib.set_library_setup(with_folder(&root, &[])).unwrap();
        lib.prepare_library(&root).unwrap();
        lib.upsert(&[library_file(&root, "A/1.flac", "A", 7)])
            .unwrap();
        let jobs = lib.cover_jobs().unwrap();
        assert_eq!(jobs.len(), 1);
        let source = CoverSource::File(root.join("A/cover.jpg"));
        lib.set_cover(&jobs[0].album_id, Some(&source), None, &jobs[0].signature)
            .unwrap();
        assert!(lib.cover_jobs().unwrap().is_empty());
        drop(lib);

        // Same installation, new path: same album id, so the cover row stays,
        // but it names the old folder and has to be looked at again.
        let moved = dir.join("Moved");
        std::fs::rename(&root, &moved).unwrap();
        let lib = Library::open_in(&dir.join("state")).unwrap();
        lib.set_library_setup(with_folder(&moved, &[])).unwrap();
        let jobs = lib.cover_jobs().unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].first_track, moved.join("A/1.flac"));
    }

    #[test]
    fn the_database_is_created_by_the_first_import() {
        let dir = temp_dir("libwait", &["state"]);
        let root = dir.join("Lib");
        let lib = Library::open_in(&dir.join("state")).unwrap();
        let saved = lib
            .set_library_setup(with_folder(&root, &["title", "genre"]))
            .unwrap();
        assert_eq!(lib.library_setup().unwrap(), saved);
        assert!(!root.exists(), "the folder is created on the first import");

        // A folder that exists is indexed in memory until then.
        std::fs::create_dir_all(root.join("A")).unwrap();
        lib.refresh_library().unwrap();
        assert!(!root.join(".coversync").exists());
        lib.upsert(&[library_file(&root, "A/1.flac", "A", 1)])
            .unwrap();
        let before = lib
            .album_track_ids(
                &lib.albums(Collection::Library, &[], &[], "", &[], None)
                    .unwrap()[0]
                    .id,
            )
            .unwrap();
        assert_eq!(before.len(), 1);

        // The import creates the database and keeps what was indexed.
        lib.prepare_library(&root).unwrap();
        assert!(root.join(LIBRARY_DB).is_file());
        assert_eq!(Library::library_in(&root), Some(1));
        assert_eq!(lib.library_setup().unwrap(), saved);
        let albums = lib
            .albums(Collection::Library, &[], &[], "", &[], None)
            .unwrap();
        assert_eq!(lib.album_track_ids(&albums[0].id).unwrap(), before);
        assert_eq!(albums[0].added_at, 1);
    }

    #[test]
    fn edits_saved_while_the_folder_is_away_win() {
        let dir = temp_dir("libaway", &["state", "other", "Lib"]);
        let root = dir.join("Lib");
        let away = dir.join("Lib.away");
        let lib = Library::open_in(&dir.join("state")).unwrap();
        lib.set_library_setup(with_folder(&root, &["title", "genre"]))
            .unwrap();
        lib.prepare_library(&root).unwrap();
        drop(lib);

        // The disk is unplugged; setup is edited and saved meanwhile.
        std::fs::rename(&root, &away).unwrap();
        let lib = Library::open_in(&dir.join("state")).unwrap();
        lib.set_library_setup(with_folder(&root, &["artist"]))
            .unwrap();
        std::fs::rename(&away, &root).unwrap();
        lib.refresh_library().unwrap();
        assert_eq!(lib.library_setup().unwrap().rules.required_tags, ["artist"]);
        drop(lib);
        // The Library's own database has them too.
        let other = Library::open_in(&dir.join("other")).unwrap();
        let opened = other.set_library_setup(with_folder(&root, &[])).unwrap();
        assert_eq!(opened.rules.required_tags, ["artist"]);
    }

    #[test]
    fn track_ids_of_another_library_name_nothing() {
        let dir = temp_dir("libids", &["state", "A", "B"]);
        let lib = Library::open_in(&dir.join("state")).unwrap();
        let mut ids = Vec::new();
        for name in ["A", "B"] {
            let root = dir.join(name);
            lib.set_library_setup(with_folder(&root, &[])).unwrap();
            lib.prepare_library(&root).unwrap();
            lib.upsert(&[library_file(&root, "x/1.flac", "x", 1)])
                .unwrap();
            let album = &lib
                .albums(Collection::Library, &[], &[], "", &[], None)
                .unwrap()[0];
            ids.push(lib.album_track_ids(&album.id).unwrap()[0]);
        }
        // Both are row 1 of their own database.
        assert_ne!(ids[0], ids[1]);
        assert_eq!(lib.track_path(ids[0]).unwrap(), None);
        assert_eq!(
            lib.track_path(ids[1]).unwrap(),
            Some(dir.join("B/x/1.flac"))
        );
        assert_eq!(lib.track_path(-1).unwrap(), None);
    }

    #[test]
    fn a_folder_that_cant_hold_the_database_is_indexed_in_memory() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("libro", &["state", "Lib"]);
        let root = dir.join("Lib");
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o555)).unwrap();
        if std::fs::create_dir(root.join("probe")).is_ok() {
            return; // Running as root: permissions don't stop writes.
        }
        let lib = Library::open_in(&dir.join("state")).unwrap();
        lib.set_library_setup(with_folder(&root, &[])).unwrap();
        lib.upsert(&[library_file(&root, "A/1.flac", "A", 1)])
            .unwrap();
        assert!(lib.prepare_library(&root).is_err());
        assert!(lib.library_problem().unwrap().is_some());
        // Still browsable this session.
        assert_eq!(
            lib.albums(Collection::Library, &[], &[], "", &[], None)
                .unwrap()
                .len(),
            1
        );
        // Once the folder is fixed, the next import gets its database.
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        lib.prepare_library(&root).unwrap();
        assert_eq!(lib.library_problem().unwrap(), None);
        assert_eq!(Library::library_in(&root), Some(1));
    }

    #[test]
    fn a_database_that_cant_be_written_is_retried_once_fixed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("libdbro", &["state", "Lib"]);
        let root = dir.join("Lib");
        let lib = Library::open_in(&dir.join("state")).unwrap();
        lib.set_library_setup(with_folder(&root, &[])).unwrap();
        lib.prepare_library(&root).unwrap();
        lib.upsert(&[library_file(&root, "A/1.flac", "A", 1)])
            .unwrap();
        drop(lib);

        let own = root.join(".coversync");
        let mode = |m| std::fs::set_permissions(&own, std::fs::Permissions::from_mode(m));
        let db_mode =
            |m| std::fs::set_permissions(root.join(LIBRARY_DB), std::fs::Permissions::from_mode(m));
        mode(0o555).unwrap();
        db_mode(0o444).unwrap();
        if std::fs::create_dir(own.join("probe")).is_ok() {
            return; // Running as root: permissions don't stop writes.
        }
        // SQLite would open it read-only; it is indexed in memory instead.
        let lib = Library::open_in(&dir.join("state")).unwrap();
        assert!(lib.library_problem().unwrap().is_some());
        lib.upsert(&[library_file(&root, "A/1.flac", "A", 1)])
            .unwrap();
        assert_eq!(lib.file_stats().unwrap().len(), 1);

        mode(0o755).unwrap();
        db_mode(0o644).unwrap();
        lib.refresh_library().unwrap();
        assert_eq!(lib.library_problem().unwrap(), None);
        assert_eq!(lib.file_stats().unwrap().len(), 1);
    }

    #[test]
    fn rebuilding_an_in_memory_index_does_not_retarget_old_ids() {
        let dir = temp_dir("libmemids", &["state", "Lib"]);
        let root = dir.join("Lib");
        let open = |rel: &str| {
            let lib = Library::open_in(&dir.join("state")).unwrap();
            lib.set_library_setup(with_folder(&root, &[])).unwrap();
            lib.upsert(&[library_file(&root, rel, "A", 1)]).unwrap();
            lib
        };
        let lib = open("A/1.flac");
        let album = &lib
            .albums(Collection::Library, &[], &[], "", &[], None)
            .unwrap()[0];
        let id = lib.album_track_ids(&album.id).unwrap()[0];
        drop(lib);

        // A different file is row 1 once the index is rebuilt.
        let lib = open("A/2.flac");
        assert_eq!(lib.track_path(id).unwrap(), None);
        assert!(!root.join(".coversync").exists());
    }

    #[test]
    fn files_of_another_library_folder_are_left_out() {
        let dir = temp_dir("libother", &["state", "Lib", "Other"]);
        let lib = Library::open_in(&dir.join("state")).unwrap();
        lib.set_library_setup(with_folder(&dir.join("Lib"), &[]))
            .unwrap();
        lib.upsert(&[library_file(&dir.join("Other"), "A/1.flac", "A", 1)])
            .unwrap();
        assert!(lib.file_stats().unwrap().is_empty());
        // Removing paths only touches the attached folder's rows.
        lib.upsert(&[library_file(&dir.join("Lib"), "A/1.flac", "A", 1)])
            .unwrap();
        lib.remove_paths(&[dir.join("Other/A/1.flac")]).unwrap();
        assert_eq!(lib.file_stats().unwrap().len(), 1);
        lib.remove_paths(&[dir.join("Lib/A/1.flac")]).unwrap();
        assert!(lib.file_stats().unwrap().is_empty());
    }

    #[test]
    fn the_old_local_database_is_taken_over() {
        let dir = temp_dir("legacy", &["state"]);
        let state = dir.join("state");
        let old = Library::open(&state.join(LEGACY_DB)).unwrap();
        old.set_sync_setup(SyncSetup { enabled: false }).unwrap();
        // A Library track indexed before the split.
        old.conn()
            .execute(
                "INSERT INTO main.tracks (path, root, mtime, size, album_id, album_dir, title,
                    artist, album, duration_ms, format, collection)
                 VALUES ('/l/a/1.flac', '/l', 5, 1, 'x', '/l/a', 't', 'a', 'b', 0, 'FLAC', 'library')",
                [],
            )
            .unwrap();
        old.conn().execute_batch("PRAGMA user_version = 3").unwrap();
        drop(old);

        let lib = Library::open_in(&state).unwrap();
        assert!(state.join(LOCAL_DB).is_file());
        for name in [LEGACY_DB, "library.sqlite3-wal"] {
            assert!(!state.join(name).exists(), "{name} is left behind");
        }
        assert!(!lib.sync_setup().unwrap().enabled);
        // The next scan reads the Library's files into its own database.
        assert!(lib.file_stats().unwrap().is_empty());
    }

    #[test]
    fn an_existing_library_gets_its_database_on_takeover() {
        let dir = temp_dir("legacylib", &["state", "Lib"]);
        let state = dir.join("state");
        let root = dir.join("Lib");
        let old = Library::open(&state.join(LEGACY_DB)).unwrap();
        old.put_setting("library_setup", &with_folder(&root, &["title", "genre"]))
            .unwrap();
        old.conn().execute_batch("PRAGMA user_version = 3").unwrap();
        drop(old);

        let lib = Library::open_in(&state).unwrap();
        assert!(root.join(LIBRARY_DB).is_file());
        assert_eq!(lib.library_problem().unwrap(), None);
        drop(lib);
        let other = Library::open_in(&dir.join("other")).unwrap();
        let opened = other.set_library_setup(with_folder(&root, &[])).unwrap();
        assert_eq!(opened.rules.required_tags, ["title", "genre"]);
    }
}
