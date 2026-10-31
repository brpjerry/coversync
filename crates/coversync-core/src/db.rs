//! SQLite index of tracks and cover metadata.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::Result;
use crate::appearance::AppearanceSetup;
use crate::autofill::{self, Autofill, TrackInfo};
use crate::inbox::{HiddenStats, InboxSetup};
use crate::model::{Album, AlbumDetail, Attention, Collection, KindCoverage, TagCoverage, Track};
use crate::rules::{self, LibrarySetup};
use crate::sync::SyncSetup;
use crate::tags::TrackTags;

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

/// A file found on disk, with the stat data used to skip unchanged files.
#[derive(Debug, Clone)]
pub struct FileStat {
    pub path: PathBuf,
    pub root: PathBuf,
    pub mtime: i64,
    pub size: u64,
    pub collection: Collection,
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

fn summarize(id: &str, tracks: &[&Track], mtime: i64, cover: Option<&CoverRow>) -> Album {
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
        added_at: mtime,
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
    mtime: i64,
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
    Ok(())
}

impl Library {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Path -> (mtime, size) for every indexed file.
    /// Path -> (mtime, size, collection) for every indexed file.
    pub fn file_stats(&self) -> Result<HashMap<PathBuf, (i64, u64, Collection)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT path, mtime, size, collection FROM tracks")?;
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

    pub fn upsert(&self, batch: &[IndexedFile]) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO tracks (path, root, mtime, size, album_id, album_dir, title, artist,
                    album, album_artist, track_no, track_total, disc_no, disc_total, year, genre,
                    duration_ms, sample_rate, bit_depth, channels, bitrate, format, tag_keys,
                    collection, tag_values)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                    ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25)
                 ON CONFLICT (path) DO UPDATE SET root = excluded.root, mtime = excluded.mtime,
                    size = excluded.size, album_id = excluded.album_id,
                    album_dir = excluded.album_dir, title = excluded.title,
                    artist = excluded.artist, album = excluded.album,
                    album_artist = excluded.album_artist, track_no = excluded.track_no,
                    track_total = excluded.track_total, disc_no = excluded.disc_no,
                    disc_total = excluded.disc_total, year = excluded.year,
                    genre = excluded.genre, duration_ms = excluded.duration_ms,
                    sample_rate = excluded.sample_rate, bit_depth = excluded.bit_depth,
                    channels = excluded.channels, bitrate = excluded.bitrate,
                    format = excluded.format, tag_keys = excluded.tag_keys,
                    collection = excluded.collection, tag_values = excluded.tag_values",
            )?;
            for (stat, album_id, album_dir, t) in batch {
                let fallback_title = stat
                    .path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let fallback_album = album_dir
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                stmt.execute(params![
                    stat.path.to_string_lossy(),
                    stat.root.to_string_lossy(),
                    stat.mtime,
                    stat.size as i64,
                    album_id,
                    album_dir.to_string_lossy(),
                    t.title.clone().unwrap_or(fallback_title),
                    t.artist.clone().unwrap_or_else(|| "Unknown Artist".into()),
                    t.album.clone().unwrap_or(fallback_album),
                    t.album_artist,
                    t.track_no,
                    t.track_total,
                    t.disc_no,
                    t.disc_total,
                    t.year,
                    t.genre,
                    t.duration_ms as i64,
                    t.sample_rate,
                    t.bit_depth,
                    t.channels,
                    t.bitrate,
                    t.format,
                    t.tag_keys.join(","),
                    stat.collection.as_str(),
                    serde_json::to_string(&t.values).expect("tag values serialize"),
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn remove_paths(&self, paths: &[PathBuf]) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached("DELETE FROM tracks WHERE path = ?1")?;
            for p in paths {
                stmt.execute([p.to_string_lossy()])?;
            }
        }
        tx.execute(
            "DELETE FROM covers WHERE album_id NOT IN (SELECT album_id FROM tracks)",
            [],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Visible tracks of a collection, with what album listings need beyond `Track`.
    fn all_tracks(&self, collection: Collection) -> Result<Vec<TrackRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT * FROM tracks WHERE collection = ?1 AND {VISIBLE}"
        ))?;
        let rows = stmt.query_map([collection.as_str()], |r| {
            let (track, dir) = track_from_row(r)?;
            Ok(TrackRow {
                track,
                dir,
                mtime: r.get("mtime")?,
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
        let mut groups: HashMap<&str, (Vec<usize>, i64)> = HashMap::new();
        for (i, r) in rows.iter().enumerate() {
            let g = groups.entry(r.track.album_id.as_str()).or_default();
            g.0.push(i);
            g.1 = g.1.max(r.mtime);
        }
        let mut albums: Vec<Album> = groups
            .into_iter()
            .map(|(id, (idx, mtime))| {
                let mut ts: Vec<&Track> = idx.iter().map(|i| &rows[*i].track).collect();
                ts.sort_by_key(|t| (t.disc_no.unwrap_or(1), t.track_no.unwrap_or(u32::MAX)));
                let mut album = summarize(id, &ts, mtime, covers.get(id));
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
        let (mut tracks, dir, mtime) = {
            let conn = self.conn();
            let mut stmt = conn.prepare(&format!(
                "SELECT * FROM tracks WHERE album_id = ?1 AND {VISIBLE}"
            ))?;
            let mut dir = String::new();
            let mut mtime = 0;
            let mut tracks = Vec::new();
            let mut rows = stmt.query([id])?;
            while let Some(r) = rows.next()? {
                let (t, d) = track_from_row(r)?;
                dir = d;
                mtime = mtime.max(r.get::<_, i64>("mtime")?);
                tracks.push(t);
            }
            (tracks, dir, mtime)
        };
        if tracks.is_empty() {
            return Ok(None);
        }
        sort_tracks(&mut tracks);
        let covers = self.covers()?;
        let refs: Vec<&Track> = tracks.iter().collect();
        let album = summarize(id, &refs, mtime, covers.get(id));
        Ok(Some(AlbumDetail { album, dir, tracks }))
    }

    pub fn track_path(&self, id: i64) -> Result<Option<PathBuf>> {
        let conn = self.conn();
        Ok(conn
            .query_row("SELECT path FROM tracks WHERE id = ?1", [id], |r| {
                r.get::<_, String>(0)
            })
            .optional()?
            .map(PathBuf::from))
    }

    pub fn track_root(&self, id: i64) -> Result<Option<(PathBuf, PathBuf)>> {
        let conn = self.conn();
        Ok(conn
            .query_row("SELECT path, root FROM tracks WHERE id = ?1", [id], |r| {
                Ok((
                    PathBuf::from(r.get::<_, String>(0)?),
                    PathBuf::from(r.get::<_, String>(1)?),
                ))
            })
            .optional()?)
    }

    pub fn album_track_ids(&self, album_id: &str) -> Result<Vec<i64>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT id FROM tracks WHERE album_id = ?1 AND {VISIBLE}"
        ))?;
        let rows = stmt.query_map([album_id], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Albums whose cover data is missing or stale.
    pub fn cover_jobs(&self) -> Result<Vec<CoverJob>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT t.album_id, t.album_dir, MIN(t.path),
                    MAX(t.mtime) || ':' || COUNT(*) AS sig, c.signature
             FROM tracks t LEFT JOIN covers c ON c.album_id = t.album_id
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

    fn setting<T: serde::de::DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        let value: Option<String> = self
            .conn()
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| {
                r.get(0)
            })
            .optional()?;
        // A value this build can't parse (say, from a newer one) reads as unset.
        Ok(value.and_then(|v| serde_json::from_str(&v).ok()))
    }

    fn put_setting<T: serde::Serialize>(&self, key: &str, value: &T) -> Result<()> {
        let json = serde_json::to_string(value).expect("settings serialize");
        self.conn().execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![key, json],
        )?;
        Ok(())
    }

    pub fn library_setup(&self) -> Result<LibrarySetup> {
        // A setup saved by a newer build may name kinds this one doesn't know.
        let mut setup: LibrarySetup = self.setting("library_setup")?.unwrap_or_default();
        setup
            .rules
            .required_tags
            .retain(|id| rules::kind(id).is_some());
        Ok(setup)
    }

    pub fn set_library_setup(&self, setup: LibrarySetup) -> Result<LibrarySetup> {
        let setup = setup.normalize()?;
        self.put_setting("library_setup", &setup)?;
        Ok(setup)
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
                    (SELECT COUNT(DISTINCT t.album_id) FROM tracks t JOIN hidden h ON h.path = t.path)",
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
        assert_eq!(version, 3);
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
}
