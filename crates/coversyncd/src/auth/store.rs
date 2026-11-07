//! Login state in the local database: the password's hash, browser sessions
//! and app tokens. It has a connection of its own, so checking a request
//! never waits for a scan's or an import's work on the index.

use std::path::Path;
use std::sync::Mutex;

use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use base64::Engine;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::Serialize;
use sha2::{Digest, Sha256};

const SCHEMA: &str = r#"
-- The one user's password, as an argon2id PHC string, from `coversyncd set-password`.
CREATE TABLE IF NOT EXISTS password (
    id     INTEGER PRIMARY KEY CHECK (id = 1),
    hash   TEXT NOT NULL,
    set_at INTEGER NOT NULL
);
-- Apps and devices, each with its own token; only the token's SHA-256 is kept.
CREATE TABLE IF NOT EXISTS devices (
    id           INTEGER PRIMARY KEY,
    name         TEXT NOT NULL,
    scope        TEXT NOT NULL CHECK (scope IN ('manage', 'sync')),
    token_hash   BLOB NOT NULL UNIQUE,
    created_at   INTEGER NOT NULL,
    last_seen_at INTEGER,
    revoked_at   INTEGER
);
-- Browser sessions, by their id's SHA-256. One traded for an app's token
-- names that device; it ends when the device is revoked.
CREATE TABLE IF NOT EXISTS sessions (
    id_hash    BLOB PRIMARY KEY,
    device_id  INTEGER,
    scope      TEXT NOT NULL CHECK (scope IN ('manage', 'sync')),
    created_at INTEGER NOT NULL,
    seen_at    INTEGER NOT NULL
);
"#;

/// A session ends after this long without a request (90 days).
pub const SESSION_IDLE: i64 = 90 * 24 * 3600;
/// A session's last use is written at most this often (seconds).
const SESSION_REFRESH: i64 = 3600;
/// A device's last use is written at most this often (seconds).
const DEVICE_REFRESH: i64 = 60;

/// App tokens start with this, so they're easy to spot in logs and secret scanners.
pub const TOKEN_PREFIX: &str = "cst_";

/// What a device or session may do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// Everything a password login can do.
    Manage,
    /// Only syncing: its own file list and files (SYN-04).
    Sync,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Manage => "manage",
            Scope::Sync => "sync",
        }
    }

    fn parse(s: &str) -> Scope {
        // Anything unexpected gets the narrower scope.
        if s == "manage" {
            Scope::Manage
        } else {
            Scope::Sync
        }
    }
}

/// How a request proved who it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Via {
    /// A browser session from a password login.
    Password,
    /// An app's token, sent as `Authorization: Bearer`.
    Token,
    /// A browser session traded for an app's token (Android's WebView).
    TokenSession,
}

/// Who a request comes from, as the login guard found. Handlers can take it
/// as `Extension<Access>`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Access {
    pub via: Via,
    pub scope: Scope,
    /// The device whose token this is, or was traded for.
    pub device: Option<i64>,
}

impl Access {
    /// A password login: the only kind that may add, revoke or re-scope
    /// devices (SRV-02). Neither a token nor a cookie traded for one is.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "device management (SRV-02) checks it")
    )]
    pub fn password_login(&self) -> bool {
        self.via == Via::Password
    }
}

/// SHA-256 of a session id or token, as stored.
pub fn digest(secret: &str) -> [u8; 32] {
    Sha256::digest(secret.as_bytes()).into()
}

/// 32 random bytes, base64url without padding.
fn random_secret() -> rusqlite::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

/// A new app token: `cst_` and 32 random bytes in base64url.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "pairing (SRV-02) issues tokens")
)]
pub fn new_token() -> rusqlite::Result<String> {
    Ok(format!("{TOKEN_PREFIX}{}", random_secret()?))
}

/// Hash a password for storage: argon2id with the crate's defaults (19 MiB,
/// two passes), as a PHC string that carries its salt and parameters.
pub fn hash_password(password: &str) -> anyhow::Result<String> {
    let hash = Argon2::default()
        .hash_password(password.as_bytes())
        .map_err(|e| anyhow::anyhow!("hashing the password: {e}"))?;
    Ok(hash.to_string())
}

/// Whether `password` is the one `stored` was made from. A hash that can't
/// be read matches nothing.
pub fn verify_password(password: &str, stored: &str) -> bool {
    match PasswordHash::new(stored) {
        Ok(hash) => Argon2::default()
            .verify_password(password.as_bytes(), &hash)
            .is_ok(),
        Err(e) => {
            tracing::error!("the stored password hash can't be read: {e}");
            false
        }
    }
}

/// A session the guard found valid.
pub struct Session {
    pub access: Access,
    /// Its last use should be written down (see [`Store::touch_session`]).
    pub stale: bool,
    /// So should its device's, for one traded for a token (see
    /// [`Store::touch_device`]).
    pub device_stale: bool,
}

/// A device whose token the guard found valid.
pub struct Device {
    pub id: i64,
    pub scope: Scope,
    /// Its last use should be written down (see [`Store::touch_device`]).
    pub stale: bool,
}

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    /// Open the login tables in the local database at `path`, creating them
    /// if need be.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The stored password hash, read afresh each time, so a password set
    /// while the service runs counts at once.
    pub fn password_hash(&self) -> rusqlite::Result<Option<String>> {
        self.conn()
            .query_row("SELECT hash FROM password WHERE id = 1", [], |r| r.get(0))
            .optional()
    }

    /// Store a new password hash. Browsers signed in with the old password
    /// are signed out; app tokens and the sessions traded for them stay.
    /// Returns whether a password was replaced.
    pub fn set_password_hash(&self, hash: &str, now: i64) -> rusqlite::Result<bool> {
        let mut conn = self.conn();
        // Take the write lock first: a deferred transaction that read before
        // writing could fail outright when the running service writes between.
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let replaced = tx
            .query_row("SELECT 1 FROM password WHERE id = 1", [], |_| Ok(()))
            .optional()?
            .is_some();
        tx.execute(
            "INSERT INTO password (id, hash, set_at) VALUES (1, ?1, ?2)
             ON CONFLICT (id) DO UPDATE SET hash = excluded.hash, set_at = excluded.set_at",
            params![hash, now],
        )?;
        tx.execute("DELETE FROM sessions WHERE device_id IS NULL", [])?;
        tx.commit()?;
        Ok(replaced)
    }

    /// Start a password login's session, if `checked` (the hash the password
    /// was checked against) is still the stored one. Comparing and inserting
    /// in one statement orders it against a reset: before it, the reset ends
    /// the session; after it, nothing matches. Returns the session id for
    /// the cookie; only its hash is kept.
    pub fn create_password_session(
        &self,
        checked: &str,
        now: i64,
    ) -> rusqlite::Result<Option<String>> {
        let id = random_secret()?;
        let conn = self.conn();
        conn.execute(
            "DELETE FROM sessions WHERE seen_at <= ?1",
            [now - SESSION_IDLE],
        )?;
        let inserted = conn.execute(
            "INSERT INTO sessions (id_hash, device_id, scope, created_at, seen_at)
             SELECT ?1, NULL, 'manage', ?2, ?2 FROM password WHERE id = 1 AND hash = ?3",
            params![&digest(&id)[..], now, checked],
        )?;
        Ok((inserted == 1).then_some(id))
    }

    /// Start a session traded for a device's token, with its scope. Returns
    /// the session id for the cookie; only its hash is kept.
    pub fn create_device_session(
        &self,
        device: i64,
        scope: Scope,
        now: i64,
    ) -> rusqlite::Result<String> {
        let id = random_secret()?;
        let conn = self.conn();
        conn.execute(
            "DELETE FROM sessions WHERE seen_at <= ?1",
            [now - SESSION_IDLE],
        )?;
        conn.execute(
            "INSERT INTO sessions (id_hash, device_id, scope, created_at, seen_at)
             VALUES (?1, ?2, ?3, ?4, ?4)",
            params![&digest(&id)[..], device, scope.as_str(), now],
        )?;
        Ok(id)
    }

    /// The session with this id, unless it has been idle too long, ended,
    /// or its device revoked.
    pub fn session(&self, id: &str, now: i64) -> rusqlite::Result<Option<Session>> {
        let row = self
            .conn()
            .query_row(
                "SELECT s.device_id, s.scope, s.seen_at, d.scope, d.last_seen_at
                 FROM sessions s LEFT JOIN devices d ON d.id = s.device_id
                 WHERE s.id_hash = ?1 AND s.seen_at > ?2
                   AND (s.device_id IS NULL OR (d.id IS NOT NULL AND d.revoked_at IS NULL))",
                params![&digest(id)[..], now - SESSION_IDLE],
                |r| {
                    Ok((
                        r.get::<_, Option<i64>>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, Option<i64>>(4)?,
                    ))
                },
            )
            .optional()?;
        Ok(row.map(|(device, scope, seen, device_scope, device_seen)| {
            let mut scope = Scope::parse(&scope);
            // A device re-scoped since the trade takes its sessions along.
            if device_scope.is_some_and(|s| Scope::parse(&s) == Scope::Sync) {
                scope = Scope::Sync;
            }
            Session {
                access: Access {
                    via: if device.is_some() {
                        Via::TokenSession
                    } else {
                        Via::Password
                    },
                    scope,
                    device,
                },
                stale: seen <= now - SESSION_REFRESH,
                // Using a traded session counts as using the device.
                device_stale: device.is_some()
                    && device_seen.is_none_or(|s| s <= now - DEVICE_REFRESH),
            }
        }))
    }

    /// Note that a session was used, which keeps it from expiring.
    pub fn touch_session(&self, id_hash: &[u8; 32], now: i64) -> rusqlite::Result<()> {
        self.conn().execute(
            "UPDATE sessions SET seen_at = ?2 WHERE id_hash = ?1 AND seen_at < ?2",
            params![&id_hash[..], now],
        )?;
        Ok(())
    }

    /// End a session (logging out). Unknown ids are ignored.
    pub fn end_session(&self, id: &str) -> rusqlite::Result<()> {
        self.conn()
            .execute("DELETE FROM sessions WHERE id_hash = ?1", [&digest(id)[..]])?;
        Ok(())
    }

    /// The device this token belongs to, unless it has been revoked.
    pub fn device(&self, token: &str, now: i64) -> rusqlite::Result<Option<Device>> {
        self.conn()
            .query_row(
                "SELECT id, scope, last_seen_at FROM devices
                 WHERE token_hash = ?1 AND revoked_at IS NULL",
                [&digest(token)[..]],
                |r| {
                    let seen: Option<i64> = r.get(2)?;
                    Ok(Device {
                        id: r.get(0)?,
                        scope: Scope::parse(&r.get::<_, String>(1)?),
                        stale: seen.is_none_or(|s| s <= now - DEVICE_REFRESH),
                    })
                },
            )
            .optional()
    }

    /// Note that a device's token was used.
    pub fn touch_device(&self, id: i64, now: i64) -> rusqlite::Result<()> {
        self.conn().execute(
            "UPDATE devices SET last_seen_at = ?2
             WHERE id = ?1 AND (last_seen_at IS NULL OR last_seen_at < ?2)",
            params![id, now],
        )?;
        Ok(())
    }

    /// Add a device with a new token, which is returned; only its hash is
    /// kept. Pairing (SRV-02) issues tokens; until then only tests do.
    #[cfg(test)]
    pub fn add_device(
        &self,
        name: &str,
        scope: Scope,
        now: i64,
    ) -> rusqlite::Result<(i64, String)> {
        let token = new_token()?;
        let conn = self.conn();
        conn.execute(
            "INSERT INTO devices (name, scope, token_hash, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![name, scope.as_str(), &digest(&token)[..], now],
        )?;
        Ok((conn.last_insert_rowid(), token))
    }

    #[cfg(test)]
    pub fn revoke_device(&self, id: i64, now: i64) -> rusqlite::Result<()> {
        self.conn().execute(
            "UPDATE devices SET revoked_at = ?2 WHERE id = ?1",
            params![id, now],
        )?;
        Ok(())
    }

    #[cfg(test)]
    pub fn device_last_seen(&self, id: i64) -> rusqlite::Result<Option<i64>> {
        self.conn().query_row(
            "SELECT last_seen_at FROM devices WHERE id = ?1",
            [id],
            |r| r.get(0),
        )
    }

    #[cfg(test)]
    pub fn forget_last_seen(&self, id: i64) -> rusqlite::Result<()> {
        self.conn()
            .execute("UPDATE devices SET last_seen_at = NULL WHERE id = ?1", [id])?;
        Ok(())
    }

    #[cfg(test)]
    pub fn rescope_device(&self, id: i64, scope: Scope) -> rusqlite::Result<()> {
        self.conn().execute(
            "UPDATE devices SET scope = ?2 WHERE id = ?1",
            params![id, scope.as_str()],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(name: &str) -> Store {
        let dir =
            std::env::temp_dir().join(format!("coversyncd-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Store::open(&dir.join("inbox.sqlite3")).unwrap()
    }

    #[test]
    fn passwords_are_argon2id_and_checked() {
        let hash = hash_password("correct horse").unwrap();
        assert!(hash.starts_with("$argon2id$"), "{hash}");
        assert!(verify_password("correct horse", &hash));
        assert!(!verify_password("correct hors", &hash));
        assert!(!verify_password("correct horse", "not a hash"));
    }

    #[test]
    fn the_first_password_and_a_reset() {
        let s = store("password");
        assert_eq!(s.password_hash().unwrap(), None);
        assert!(!s.set_password_hash("$first", 10).unwrap());
        assert_eq!(s.password_hash().unwrap().as_deref(), Some("$first"));
        let browser = s.create_password_session("$first", 20).unwrap().unwrap();
        let (phone, token) = s.add_device("Phone", Scope::Manage, 20).unwrap();
        let traded = s.create_device_session(phone, Scope::Manage, 20).unwrap();
        assert!(s.set_password_hash("$second", 30).unwrap());
        assert_eq!(s.password_hash().unwrap().as_deref(), Some("$second"));
        // A reset signs browsers out, but leaves apps alone.
        assert!(s.session(&browser, 40).unwrap().is_none());
        assert!(s.session(&traded, 40).unwrap().is_some());
        assert!(s.device(&token, 40).unwrap().is_some());
    }

    fn sessions_count(s: &Store) -> i64 {
        s.conn()
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn a_reset_during_a_login_leaves_no_session() {
        let s = store("race");
        s.set_password_hash("$old", 0).unwrap();
        // A login reads the hash and checks the password against it, while
        // set-password commits a new one.
        let checked = s.password_hash().unwrap().unwrap();
        s.set_password_hash("$new", 1).unwrap();
        assert!(s.create_password_session(&checked, 2).unwrap().is_none());
        assert_eq!(sessions_count(&s), 0);
        // The other order: the session comes first, and the reset ends it.
        let early = s.create_password_session("$new", 3).unwrap().unwrap();
        s.set_password_hash("$newer", 4).unwrap();
        assert!(s.session(&early, 5).unwrap().is_none());
        assert_eq!(sessions_count(&s), 0);
    }

    #[test]
    fn a_reset_waits_for_another_writer() {
        let dir =
            std::env::temp_dir().join(format!("coversyncd-store-busy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("inbox.sqlite3");
        let s = Store::open(&path).unwrap();
        s.conn().execute_batch("PRAGMA journal_mode = WAL").unwrap();
        s.set_password_hash("$old", 0).unwrap();
        // The running service, in the middle of a write of its own.
        let service = Connection::open(&path).unwrap();
        service.execute_batch("BEGIN IMMEDIATE").unwrap();
        let (started, wait) = std::sync::mpsc::channel();
        let reset = std::thread::spawn(move || {
            started.send(()).unwrap();
            s.set_password_hash("$new", 1).map(|_| s)
        });
        wait.recv().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        service
            .execute("DELETE FROM sessions WHERE seen_at < 0", [])
            .unwrap();
        service.execute_batch("COMMIT").unwrap();
        let s = reset
            .join()
            .unwrap()
            .expect("the reset waits, then succeeds");
        assert_eq!(s.password_hash().unwrap().as_deref(), Some("$new"));
    }

    #[test]
    fn sessions_slide_and_expire() {
        let s = store("sessions");
        s.set_password_hash("$pw", 0).unwrap();
        let id = s.create_password_session("$pw", 1000).unwrap().unwrap();
        let fresh = s.session(&id, 1000 + 60).unwrap().unwrap();
        assert_eq!(fresh.access.via, Via::Password);
        assert!(fresh.access.password_login());
        assert!(!fresh.stale);
        let later = 1000 + SESSION_REFRESH + 1;
        assert!(s.session(&id, later).unwrap().unwrap().stale);
        s.touch_session(&digest(&id), later).unwrap();
        assert!(!s.session(&id, later).unwrap().unwrap().stale);
        // Idle from `later` for the whole period: gone.
        assert!(s.session(&id, later + SESSION_IDLE - 1).unwrap().is_some());
        assert!(s.session(&id, later + SESSION_IDLE).unwrap().is_none());
        assert!(s.session("someone else's", later).unwrap().is_none());
    }

    #[test]
    fn logging_out_ends_only_that_session() {
        let s = store("logout");
        s.set_password_hash("$pw", 0).unwrap();
        let a = s.create_password_session("$pw", 0).unwrap().unwrap();
        let b = s.create_password_session("$pw", 0).unwrap().unwrap();
        s.end_session(&a).unwrap();
        assert!(s.session(&a, 1).unwrap().is_none());
        assert!(s.session(&b, 1).unwrap().is_some());
    }

    #[test]
    fn tokens_are_stored_hashed_and_revocable() {
        let s = store("tokens");
        let (id, token) = s.add_device("Desktop", Scope::Manage, 0).unwrap();
        assert!(token.starts_with(TOKEN_PREFIX));
        assert_eq!(token.len(), TOKEN_PREFIX.len() + 43);
        let stored: Vec<u8> = s
            .conn()
            .query_row("SELECT token_hash FROM devices WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(stored, digest(&token));
        let d = s.device(&token, 0).unwrap().unwrap();
        assert_eq!((d.id, d.scope, d.stale), (id, Scope::Manage, true));
        s.touch_device(id, 5).unwrap();
        assert!(!s.device(&token, 10).unwrap().unwrap().stale);
        assert!(s.device(&token, 5 + DEVICE_REFRESH).unwrap().unwrap().stale);
        s.revoke_device(id, 20).unwrap();
        assert!(s.device(&token, 21).unwrap().is_none());
        assert!(s.device("cst_unknown", 21).unwrap().is_none());
    }

    #[test]
    fn traded_sessions_follow_their_device() {
        let s = store("traded");
        let (id, _) = s.add_device("Phone", Scope::Manage, 0).unwrap();
        let session = s.create_device_session(id, Scope::Manage, 0).unwrap();
        // Using the session counts as using the device.
        assert!(s.session(&session, 1).unwrap().unwrap().device_stale);
        s.touch_device(id, 1).unwrap();
        assert!(!s.session(&session, 2).unwrap().unwrap().device_stale);
        let found = s.session(&session, 1).unwrap().unwrap().access;
        assert_eq!(
            found,
            Access {
                via: Via::TokenSession,
                scope: Scope::Manage,
                device: Some(id)
            }
        );
        assert!(!found.password_login());
        s.rescope_device(id, Scope::Sync).unwrap();
        assert_eq!(
            s.session(&session, 2).unwrap().unwrap().access.scope,
            Scope::Sync
        );
        s.revoke_device(id, 3).unwrap();
        assert!(s.session(&session, 4).unwrap().is_none());
    }
}
