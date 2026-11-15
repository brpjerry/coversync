//! What the server keeps about each device's sync, in the local database:
//! what the device holds (its inventory), the files in its folder that
//! coversync didn't put there, puts that failed, the epoch its marker must
//! carry, and its last session. Also the index of cover files made for
//! devices. It has a connection of its own, so a device's reports never
//! wait on a scan's or an import's work on the index.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use coversync_protocol::{Capabilities, Client, EndReport, Fact, Failure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::plan::Held;

/// Made after the login tables, as the triggers are on `devices`: they
/// remove a device's sync state with it, whichever build removes it.
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS sync_devices (
    device_id       INTEGER PRIMARY KEY,
    -- From the last reconcile of the device's folder; null before the first.
    epoch           TEXT,
    -- What the device last said about itself and its storage (JSON).
    client          TEXT,
    capabilities    TEXT,
    -- Asked for in Sync setup: a reconcile, a sync, removing other files.
    check_requested INTEGER NOT NULL DEFAULT 0,
    sync_requested  INTEGER NOT NULL DEFAULT 0,
    remove_other    INTEGER NOT NULL DEFAULT 0,
    -- The device's target as last worked out, and when it last changed.
    target          TEXT,
    changed_at      INTEGER,
    -- The last session's report (JSON), and when it ended.
    last            TEXT
);
-- What the server believes each device holds.
CREATE TABLE IF NOT EXISTS sync_inventory (
    device_id INTEGER NOT NULL,
    path      TEXT NOT NULL,
    hash      TEXT NOT NULL,
    size      INTEGER NOT NULL,
    PRIMARY KEY (device_id, path)
) WITHOUT ROWID;
-- Files a reconcile found that coversync didn't put there.
CREATE TABLE IF NOT EXISTS sync_other (
    device_id INTEGER NOT NULL,
    path      TEXT NOT NULL,
    size      INTEGER NOT NULL,
    PRIMARY KEY (device_id, path)
) WITHOUT ROWID;
-- Puts that failed, held back until their content changes or the user
-- retries them.
CREATE TABLE IF NOT EXISTS sync_failures (
    device_id INTEGER NOT NULL,
    path      TEXT NOT NULL,
    hash      TEXT NOT NULL,
    op        TEXT NOT NULL,
    error     TEXT NOT NULL,
    detail    TEXT,
    at        INTEGER NOT NULL,
    PRIMARY KEY (device_id, path, hash)
) WITHOUT ROWID;
-- Cover files made for devices, by the art's version and the width they
-- were made for (0: as it is). The files are in the cache, by hash.
CREATE TABLE IF NOT EXISTS sync_covers (
    version TEXT NOT NULL,
    width   INTEGER NOT NULL,
    hash    TEXT NOT NULL,
    size    INTEGER NOT NULL,
    ext     TEXT NOT NULL,
    PRIMARY KEY (version, width)
) WITHOUT ROWID;
CREATE TRIGGER IF NOT EXISTS sync_device_removed AFTER DELETE ON devices
BEGIN
    DELETE FROM sync_devices WHERE device_id = OLD.id;
    DELETE FROM sync_inventory WHERE device_id = OLD.id;
    DELETE FROM sync_other WHERE device_id = OLD.id;
    DELETE FROM sync_failures WHERE device_id = OLD.id;
END;
"#;

/// A device's last session, as Sync setup shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastSession {
    pub started_at: i64,
    pub ended_at: i64,
    pub reason: coversync_protocol::Reason,
    pub report: EndReport,
}

/// One device's sync state, apart from its files.
#[derive(Debug, Clone, Default)]
pub struct DeviceState {
    pub epoch: Option<String>,
    pub client: Client,
    pub capabilities: Capabilities,
    pub check_requested: bool,
    pub sync_requested: bool,
    pub remove_other: bool,
    pub target: Option<String>,
    pub changed_at: Option<i64>,
    pub last: Option<LastSession>,
}

/// A put that failed, as Sync setup lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FailedPut {
    pub path: String,
    pub hash: String,
    pub op: String,
    pub error: String,
    pub detail: Option<String>,
    pub at: i64,
}

/// A cover file made for devices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverFile {
    pub hash: String,
    pub size: u64,
    pub ext: String,
}

pub struct Store {
    conn: Mutex<Connection>,
}

fn json<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("sync state serializes")
}

fn parse<T: serde::de::DeserializeOwned + Default>(text: Option<String>) -> T {
    text.and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

impl Store {
    /// Open the sync tables in the local database at `path`. The login
    /// tables must be there already (`auth::Auth::open`).
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(10))?;
        // As the index's own connection: with a write-ahead log, a commit
        // needn't wait for the disk, which a hash pass or a sync can keep
        // busy for seconds at a time. A power cut can lose the last few
        // reports, which devices send again; it can't corrupt anything.
        conn.execute_batch("PRAGMA synchronous = NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn device(&self, id: i64) -> rusqlite::Result<DeviceState> {
        let found = self
            .conn()
            .query_row(
                "SELECT epoch, client, capabilities, check_requested, sync_requested,
                    remove_other, target, changed_at, last
                 FROM sync_devices WHERE device_id = ?1",
                [id],
                |r| {
                    Ok(DeviceState {
                        epoch: r.get(0)?,
                        client: parse(r.get(1)?),
                        capabilities: parse(r.get(2)?),
                        check_requested: r.get(3)?,
                        sync_requested: r.get(4)?,
                        remove_other: r.get(5)?,
                        target: r.get(6)?,
                        changed_at: r.get(7)?,
                        last: r
                            .get::<_, Option<String>>(8)?
                            .and_then(|t| serde_json::from_str(&t).ok()),
                    })
                },
            )
            .optional()?;
        Ok(found.unwrap_or_default())
    }

    /// Make sure the device has a row, so updates below find one.
    fn ensure(conn: &Connection, id: i64) -> rusqlite::Result<()> {
        conn.execute(
            "INSERT OR IGNORE INTO sync_devices (device_id) VALUES (?1)",
            [id],
        )?;
        Ok(())
    }

    /// What a starting session says about the device. A sync asked for in
    /// Sync setup is under way now.
    pub fn session_started(
        &self,
        id: i64,
        client: &Client,
        capabilities: &Capabilities,
    ) -> rusqlite::Result<()> {
        let conn = self.conn();
        Self::ensure(&conn, id)?;
        conn.execute(
            "UPDATE sync_devices SET client = ?2, capabilities = ?3, sync_requested = 0
             WHERE device_id = ?1",
            params![id, json(client), json(capabilities)],
        )?;
        Ok(())
    }

    /// Record the device's target, if it's another than last seen; returns
    /// when it last changed.
    pub fn saw_target(&self, id: i64, target: &str, now: i64) -> rusqlite::Result<i64> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO sync_devices (device_id, target, changed_at) VALUES (?1, ?2, ?3)
             ON CONFLICT (device_id) DO UPDATE SET target = excluded.target,
                 changed_at = excluded.changed_at
             WHERE target IS NOT excluded.target",
            params![id, target, now],
        )?;
        conn.query_row(
            "SELECT changed_at FROM sync_devices WHERE device_id = ?1",
            [id],
            |r| r.get(0),
        )
    }

    pub fn set_flag(&self, id: i64, flag: Flag, on: bool) -> rusqlite::Result<()> {
        let conn = self.conn();
        Self::ensure(&conn, id)?;
        let column = match flag {
            Flag::Check => "check_requested",
            Flag::Sync => "sync_requested",
            Flag::RemoveOther => "remove_other",
        };
        conn.execute(
            &format!("UPDATE sync_devices SET {column} = ?2 WHERE device_id = ?1"),
            params![id, on],
        )?;
        Ok(())
    }

    pub fn set_last(&self, id: i64, last: &LastSession) -> rusqlite::Result<()> {
        let conn = self.conn();
        Self::ensure(&conn, id)?;
        conn.execute(
            "UPDATE sync_devices SET last = ?2 WHERE device_id = ?1",
            params![id, json(last)],
        )?;
        Ok(())
    }

    /// The device's files: its inventory, other files, and held-back puts.
    pub fn files(&self, id: i64) -> rusqlite::Result<DeviceFiles> {
        let conn = self.conn();
        let inventory = conn
            .prepare_cached("SELECT path, hash, size FROM sync_inventory WHERE device_id = ?1")?
            .query_map([id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    Held {
                        hash: r.get(1)?,
                        size: r.get::<_, i64>(2)? as u64,
                    },
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;
        let other = conn
            .prepare_cached("SELECT path, size FROM sync_other WHERE device_id = ?1")?
            .query_map([id], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u64)))?
            .collect::<rusqlite::Result<_>>()?;
        let held_back = conn
            .prepare_cached("SELECT path, hash FROM sync_failures WHERE device_id = ?1")?
            .query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(DeviceFiles {
            inventory,
            other,
            held_back,
        })
    }

    /// Puts that failed on the device, newest first.
    pub fn failures(&self, id: i64, limit: usize) -> rusqlite::Result<(u64, Vec<FailedPut>)> {
        let conn = self.conn();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sync_failures WHERE device_id = ?1",
            [id],
            |r| r.get(0),
        )?;
        let list = conn
            .prepare_cached(
                "SELECT path, hash, op, error, detail, at FROM sync_failures
                 WHERE device_id = ?1 ORDER BY at DESC, path LIMIT ?2",
            )?
            .query_map(params![id, limit as i64], |r| {
                Ok(FailedPut {
                    path: r.get(0)?,
                    hash: r.get(1)?,
                    op: r.get(2)?,
                    error: r.get(3)?,
                    detail: r.get(4)?,
                    at: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok((count as u64, list))
    }

    /// Let the failed puts be tried again.
    pub fn clear_failures(&self, id: i64) -> rusqlite::Result<()> {
        self.conn()
            .execute("DELETE FROM sync_failures WHERE device_id = ?1", [id])?;
        Ok(())
    }

    /// Apply a device's report in one transaction. Applying the same report
    /// again changes nothing: puts and deletes say what's there now, and a
    /// move applies only while its source holds the content it names.
    pub fn apply_facts(
        &self,
        id: i64,
        done: &[Fact],
        failed: &[Failure],
        now: i64,
    ) -> rusqlite::Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        {
            let mut put = tx.prepare_cached(
                "INSERT INTO sync_inventory (device_id, path, hash, size) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (device_id, path) DO UPDATE SET hash = excluded.hash,
                     size = excluded.size",
            )?;
            let mut gone =
                tx.prepare_cached("DELETE FROM sync_inventory WHERE device_id = ?1 AND path = ?2")?;
            let mut gone_other =
                tx.prepare_cached("DELETE FROM sync_other WHERE device_id = ?1 AND path = ?2")?;
            let mut moved = tx.prepare_cached(
                "UPDATE OR REPLACE sync_inventory SET path = ?3
                 WHERE device_id = ?1 AND path = ?2 AND hash = ?4",
            )?;
            let mut unfailed =
                tx.prepare_cached("DELETE FROM sync_failures WHERE device_id = ?1 AND path = ?2")?;
            for fact in done {
                match fact {
                    Fact::Put { path, hash, size } => {
                        let Ok(size) = i64::try_from(*size) else {
                            continue;
                        };
                        put.execute(params![id, path, hash, size])?;
                        gone_other.execute(params![id, path])?;
                        unfailed.execute(params![id, path])?;
                    }
                    Fact::Move { from, path, hash } => {
                        // Whatever was at the path was replaced.
                        if moved.execute(params![id, from, path, hash])? > 0 {
                            gone_other.execute(params![id, path])?;
                        }
                    }
                    Fact::Delete { path } => {
                        gone.execute(params![id, path])?;
                        gone_other.execute(params![id, path])?;
                    }
                }
            }
            let mut fail = tx.prepare_cached(
                "INSERT OR REPLACE INTO sync_failures
                    (device_id, path, hash, op, error, detail, at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for f in failed {
                // Only puts are held back: other operations are planned
                // afresh from the inventory each time, and are cheap.
                if let Some(hash) = f.hash.as_deref().filter(|_| f.op == "put") {
                    fail.execute(params![id, f.path, hash, f.op, f.error, f.detail, now])?;
                }
            }
        }
        // Removing other files is asked for once: when none are left, files
        // a later check finds stay unless the user asks again.
        tx.execute(
            "UPDATE sync_devices SET remove_other = 0
             WHERE device_id = ?1 AND remove_other
                AND NOT EXISTS (SELECT 1 FROM sync_other WHERE device_id = ?1)",
            [id],
        )?;
        tx.commit()
    }

    /// Replace what the server believes the device holds, after a reconcile,
    /// under a new epoch.
    pub fn reconciled(
        &self,
        id: i64,
        inventory: &HashMap<String, Held>,
        other: &HashMap<String, u64>,
        epoch: &str,
    ) -> rusqlite::Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        Self::ensure(&tx, id)?;
        tx.execute("DELETE FROM sync_inventory WHERE device_id = ?1", [id])?;
        tx.execute("DELETE FROM sync_other WHERE device_id = ?1", [id])?;
        {
            let mut put = tx.prepare_cached(
                "INSERT INTO sync_inventory (device_id, path, hash, size) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (path, h) in inventory {
                put.execute(params![id, path, h.hash, h.size as i64])?;
            }
            let mut put = tx.prepare_cached(
                "INSERT INTO sync_other (device_id, path, size) VALUES (?1, ?2, ?3)",
            )?;
            for (path, size) in other {
                put.execute(params![id, path, *size as i64])?;
            }
        }
        // Removing other files was asked for the files listed before; a new
        // listing needs asking again.
        tx.execute(
            "UPDATE sync_devices SET epoch = ?2, check_requested = 0, remove_other = 0
             WHERE device_id = ?1",
            params![id, epoch],
        )?;
        tx.commit()
    }

    pub fn cover(&self, version: &str, width: u32) -> rusqlite::Result<Option<CoverFile>> {
        self.conn()
            .query_row(
                "SELECT hash, size, ext FROM sync_covers WHERE version = ?1 AND width = ?2",
                params![version, width],
                |r| {
                    Ok(CoverFile {
                        hash: r.get(0)?,
                        size: r.get::<_, i64>(1)? as u64,
                        ext: r.get(2)?,
                    })
                },
            )
            .optional()
    }

    /// Every cover file made, by (art version, width).
    pub fn covers(&self) -> rusqlite::Result<HashMap<(String, u32), CoverFile>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT version, width, hash, size, ext FROM sync_covers")?;
        stmt.query_map([], |r| {
            Ok((
                (r.get(0)?, r.get(1)?),
                CoverFile {
                    hash: r.get(2)?,
                    size: r.get::<_, i64>(3)? as u64,
                    ext: r.get(4)?,
                },
            ))
        })?
        .collect()
    }

    pub fn add_cover(&self, version: &str, width: u32, file: &CoverFile) -> rusqlite::Result<()> {
        self.conn().execute(
            "INSERT OR REPLACE INTO sync_covers (version, width, hash, size, ext)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![version, width, file.hash, file.size as i64, file.ext],
        )?;
        Ok(())
    }
}

/// A flag set from Sync setup.
#[derive(Debug, Clone, Copy)]
pub enum Flag {
    /// Reconcile the device's folder at its next session.
    Check,
    /// Someone asked for a sync.
    Sync,
    /// Remove the files in its folder that coversync didn't put there, once:
    /// it clears when none are left.
    RemoveOther,
}

/// A device's files, as plans need them.
#[derive(Debug, Default)]
pub struct DeviceFiles {
    pub inventory: HashMap<String, Held>,
    pub other: HashMap<String, u64>,
    pub held_back: HashSet<(String, String)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(name: &str) -> Store {
        let dir = std::env::temp_dir().join(format!(
            "coversyncd-sync-store-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("inbox.sqlite3");
        // The sync tables' trigger is on the login tables' devices.
        Connection::open(&path)
            .unwrap()
            .execute_batch("CREATE TABLE devices (id INTEGER PRIMARY KEY)")
            .unwrap();
        Store::open(&path).unwrap()
    }

    fn put(path: &str, hash: &str) -> Fact {
        Fact::Put {
            path: path.into(),
            hash: hash.into(),
            size: 1,
        }
    }

    fn mv(from: &str, path: &str, hash: &str) -> Fact {
        Fact::Move {
            from: from.into(),
            path: path.into(),
            hash: hash.into(),
        }
    }

    fn held(s: &Store) -> Vec<(String, String)> {
        let mut out: Vec<_> = s
            .files(1)
            .unwrap()
            .inventory
            .into_iter()
            .map(|(p, h)| (p, h.hash))
            .collect();
        out.sort();
        out
    }

    #[test]
    fn a_report_sent_twice_changes_nothing_the_second_time() {
        let s = store("replay");
        s.apply_facts(1, &[put("a", "h1"), put("b", "h2")], &[], 0)
            .unwrap();
        // a and b trade places through a temporary name; the reply is lost
        // and the device sends the same report again.
        let swap = [mv("a", "t", "h1"), mv("b", "a", "h2"), mv("t", "b", "h1")];
        for _ in 0..2 {
            s.apply_facts(1, &swap, &[], 0).unwrap();
            assert_eq!(
                held(&s),
                [("a".into(), "h2".into()), ("b".into(), "h1".into())]
            );
        }
        // A move whose source the server doesn't know of leaves its path as
        // it was.
        s.apply_facts(1, &[mv("gone", "a", "h9")], &[], 0).unwrap();
        assert_eq!(held(&s)[0], ("a".into(), "h2".into()));
    }
}
