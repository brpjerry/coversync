//! AcoustID and MusicBrainz answers saved on disk, so an album identified
//! once is identified again (when it's imported again, say) without asking
//! either service. Each answer is kept as the JSON the service sent, under
//! the request that asked for it, so a newer build that reads more of an
//! answer still finds all of it. Answers are kept until a lookup asks the
//! services again (Look up again, in the import dialog), which replaces them.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use rusqlite::{Connection, OptionalExtension, params};

use crate::Result;

/// The cache's file name, in coversyncd's cache directory.
pub const FILE: &str = "lookups.sqlite3";

const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
CREATE TABLE IF NOT EXISTS answers (
    service  TEXT NOT NULL,
    request  TEXT NOT NULL,
    -- The answer's JSON, zlib-compressed.
    body     BLOB NOT NULL,
    saved_at INTEGER NOT NULL,
    PRIMARY KEY (service, request)
);
";

/// The web service an answer came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Service {
    AcoustId,
    MusicBrainz,
}

impl Service {
    fn as_str(self) -> &'static str {
        match self {
            Self::AcoustId => "acoustid",
            Self::MusicBrainz => "musicbrainz",
        }
    }
}

/// An answer saved earlier.
#[derive(Debug)]
pub struct Saved {
    pub body: String,
    /// When it was saved, in seconds since the Unix epoch.
    pub at: i64,
}

/// The earlier of two save times; None counts as "just asked".
pub fn oldest(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub struct LookupCache {
    conn: Mutex<Connection>,
}

impl LookupCache {
    /// Open the cache at `path`, creating it if need be.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// The cache at `path`, or if that can't be opened, one in memory that
    /// lasts until coversyncd stops.
    pub fn open_or_memory(path: &Path) -> Self {
        Self::open(path).unwrap_or_else(|e| {
            tracing::warn!(path = %path.display(), "lookups will only be saved until coversyncd stops: {e}");
            Self::in_memory()
        })
    }

    /// A cache in memory, forgotten when dropped.
    pub fn in_memory() -> Self {
        let conn = Connection::open_in_memory().expect("SQLite opens a database in memory");
        conn.execute_batch(SCHEMA)
            .expect("the lookup cache's schema applies");
        Self {
            conn: Mutex::new(conn),
        }
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The answer saved for `request`, if there is one that can be read.
    pub fn get(&self, service: Service, request: &str) -> Option<Saved> {
        let row = self
            .conn()
            .query_row(
                "SELECT body, saved_at FROM answers WHERE service = ?1 AND request = ?2",
                params![service.as_str(), request],
                |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)?)),
            )
            .optional()
            .inspect_err(|e| tracing::warn!("couldn't read a saved lookup: {e}"))
            .ok()??;
        let mut body = String::new();
        ZlibDecoder::new(row.0.as_slice())
            .read_to_string(&mut body)
            .inspect_err(|e| tracing::warn!("a saved lookup is damaged: {e}"))
            .ok()?;
        Some(Saved { body, at: row.1 })
    }

    /// Save `body` as the answer to `request`, replacing any saved before.
    /// Not saving only means asking again next time, so a failure is logged
    /// rather than returned.
    pub fn put(&self, service: Service, request: &str, body: &str) {
        let mut z = ZlibEncoder::new(Vec::new(), Compression::default());
        let saved = z
            .write_all(body.as_bytes())
            .and_then(|_| z.finish())
            .map_err(crate::Error::from)
            .and_then(|packed| {
                self.conn().execute(
                    "INSERT OR REPLACE INTO answers (service, request, body, saved_at)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![service.as_str(), request, packed, now()],
                )?;
                Ok(())
            });
        if let Err(e) = saved {
            tracing::warn!("couldn't save a lookup: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_are_kept_per_service_and_replaced() {
        let cache = LookupCache::in_memory();
        assert!(cache.get(Service::MusicBrainz, "r").is_none());
        cache.put(Service::MusicBrainz, "r", r#"{"id": "one"}"#);
        cache.put(Service::AcoustId, "r", "[]");
        let saved = cache.get(Service::MusicBrainz, "r").unwrap();
        assert_eq!(saved.body, r#"{"id": "one"}"#);
        assert!(saved.at > 0);
        assert_eq!(cache.get(Service::AcoustId, "r").unwrap().body, "[]");

        cache.put(Service::MusicBrainz, "r", r#"{"id": "two"}"#);
        assert_eq!(
            cache.get(Service::MusicBrainz, "r").unwrap().body,
            r#"{"id": "two"}"#
        );
    }

    #[test]
    fn answers_survive_reopening_and_damage_reads_as_missing() {
        let dir =
            std::env::temp_dir().join(format!("coversync-test-{}-lookups", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join(FILE);
        LookupCache::open(&path)
            .unwrap()
            .put(Service::MusicBrainz, "r", "{}");
        let cache = LookupCache::open(&path).unwrap();
        assert_eq!(cache.get(Service::MusicBrainz, "r").unwrap().body, "{}");

        cache
            .conn()
            .execute("UPDATE answers SET body = x'00ff'", [])
            .unwrap();
        assert!(cache.get(Service::MusicBrainz, "r").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_oldest_save_wins() {
        assert_eq!(oldest(None, None), None);
        assert_eq!(oldest(Some(5), None), Some(5));
        assert_eq!(oldest(None, Some(5)), Some(5));
        assert_eq!(oldest(Some(5), Some(3)), Some(3));
    }
}
