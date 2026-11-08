//! Login state in the local database: the password's hash, browser sessions,
//! paired devices and their tokens, and how devices may pair. It has a
//! connection of its own, so checking a request never waits for a scan's or
//! an import's work on the index.

use std::path::Path;
use std::sync::Mutex;

use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use base64::Engine;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const SCHEMA: &str = r#"
-- The one user's password, as an argon2id PHC string, from `coversyncd set-password`.
CREATE TABLE IF NOT EXISTS password (
    id     INTEGER PRIMARY KEY CHECK (id = 1),
    hash   TEXT NOT NULL,
    set_at INTEGER NOT NULL
);
-- Apps and devices, each with its own token; only the token's SHA-256 is kept.
-- A revoked device keeps its entry (and, with SYN-03, its settings) until it
-- is removed, so pairing again can replace it. The columns after revoked_at
-- came with pairing; `migrate` adds them to a table made before.
CREATE TABLE IF NOT EXISTS devices (
    id           INTEGER PRIMARY KEY,
    name         TEXT NOT NULL,
    scope        TEXT NOT NULL CHECK (scope IN ('manage', 'sync')),
    token_hash   BLOB NOT NULL UNIQUE,
    created_at   INTEGER NOT NULL,
    last_seen_at INTEGER,
    revoked_at   INTEGER,
    -- What the device said about itself when it paired, and since.
    platform     TEXT,
    hostname     TEXT,
    app_version  TEXT,
    -- The address its token was last used from.
    last_ip      TEXT,
    -- When its current token was issued: pairing again replaces it.
    paired_at    INTEGER
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
-- How devices pair: the CA certificate apps are told to trust (the path to
-- its public certificate), and whether pairing over plain http is allowed.
CREATE TABLE IF NOT EXISTS pairing_setup (
    id         INTEGER PRIMARY KEY CHECK (id = 1),
    ca_path    TEXT,
    allow_http INTEGER NOT NULL DEFAULT 0
);
"#;

/// Columns `devices` gained after SRV-01, which made the table without them.
const DEVICE_COLUMNS: &[(&str, &str)] = &[
    ("platform", "TEXT"),
    ("hostname", "TEXT"),
    ("app_version", "TEXT"),
    ("last_ip", "TEXT"),
    ("paired_at", "INTEGER"),
];

/// A session ends after this long without a request (90 days).
pub const SESSION_IDLE: i64 = 90 * 24 * 3600;
/// A session's last use is written at most this often (seconds).
const SESSION_REFRESH: i64 = 3600;
/// A device's last use is written at most this often (seconds).
const DEVICE_REFRESH: i64 = 60;

/// App tokens start with this, so they're easy to spot in logs and secret scanners.
pub const TOKEN_PREFIX: &str = "cst_";

/// What a device or session may do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    /// A password login: the only kind that may add, change or revoke
    /// devices. Neither a token nor a cookie traded for one is.
    pub fn password_login(&self) -> bool {
        self.via == Via::Password
    }
}

/// SHA-256 of a session id or token, as stored.
pub fn digest(secret: &str) -> [u8; 32] {
    Sha256::digest(secret.as_bytes()).into()
}

/// 32 random bytes, base64url without padding.
pub fn random_secret() -> rusqlite::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

/// A new app token: `cst_` and 32 random bytes in base64url.
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

/// A paired device, as Sync setup lists it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DeviceInfo {
    pub id: i64,
    pub name: String,
    pub scope: Scope,
    /// What the device said it runs on ("android", "linux", …).
    pub platform: Option<String>,
    pub hostname: Option<String>,
    pub app_version: Option<String>,
    pub created_at: i64,
    /// When its current token was issued.
    pub paired_at: i64,
    pub last_seen_at: Option<i64>,
    pub last_ip: Option<String>,
    pub revoked_at: Option<i64>,
}

const DEVICE_INFO: &str = "SELECT id, name, scope, platform, hostname, app_version, created_at,
        COALESCE(paired_at, created_at), last_seen_at, last_ip, revoked_at FROM devices";

fn device_info(r: &rusqlite::Row) -> rusqlite::Result<DeviceInfo> {
    Ok(DeviceInfo {
        id: r.get(0)?,
        name: r.get(1)?,
        scope: Scope::parse(&r.get::<_, String>(2)?),
        platform: r.get(3)?,
        hostname: r.get(4)?,
        app_version: r.get(5)?,
        created_at: r.get(6)?,
        paired_at: r.get(7)?,
        last_seen_at: r.get(8)?,
        last_ip: r.get(9)?,
        revoked_at: r.get(10)?,
    })
}

/// A device pairing with a code: what the browser chose for it, and what it
/// said about itself.
pub struct NewDevice<'a> {
    pub name: &'a str,
    pub scope: Scope,
    /// The device whose entry it takes over.
    pub replace: Option<i64>,
    pub platform: Option<&'a str>,
    pub hostname: Option<&'a str>,
    pub app_version: Option<&'a str>,
    pub ip: Option<&'a str>,
}

/// How devices pair.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingSetup {
    /// The public certificate of the CA that signed the server's
    /// certificate, for Android apps to trust; none for a publicly trusted one.
    pub ca_path: Option<String>,
    /// Pairing over plain http is allowed, for a network that is encrypted
    /// already (Tailscale, WireGuard).
    pub allow_http: bool,
}

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    /// Open the login tables in the local database at `path`, creating them
    /// if need be.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let mut conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        migrate(&mut conn)?;
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

    /// Note that a device's token was used, from `ip`; `app_version`, when
    /// the app sent one, replaces the one noted before.
    pub fn touch_device(
        &self,
        id: i64,
        now: i64,
        ip: Option<&str>,
        app_version: Option<&str>,
    ) -> rusqlite::Result<()> {
        self.conn().execute(
            "UPDATE devices SET last_seen_at = ?2, last_ip = COALESCE(?3, last_ip),
                 app_version = COALESCE(?4, app_version)
             WHERE id = ?1 AND (last_seen_at IS NULL OR last_seen_at < ?2)",
            params![id, now, ip, app_version],
        )?;
        Ok(())
    }

    /// Every device: those in use, then the revoked ones, each in the order
    /// they were added.
    pub fn devices(&self) -> rusqlite::Result<Vec<DeviceInfo>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "{DEVICE_INFO} ORDER BY revoked_at IS NOT NULL, id"
        ))?;
        stmt.query_map([], device_info)?.collect()
    }

    pub fn device_info(&self, id: i64) -> rusqlite::Result<Option<DeviceInfo>> {
        self.conn()
            .query_row(&format!("{DEVICE_INFO} WHERE id = ?1"), [id], device_info)
            .optional()
    }

    /// Issue a token to a device that redeemed a pairing code. Returns the
    /// device's id and the token; only the token's hash is kept. A device
    /// that replaces another takes over its entry, revoked or not: the id
    /// and whatever hangs off it stay, while the old token and the sessions
    /// traded for it end. If that entry has gone meanwhile, the device is
    /// added as a new one.
    pub fn pair_device(&self, new: &NewDevice, now: i64) -> rusqlite::Result<(i64, String)> {
        let token = new_token()?;
        let hash = &digest(&token)[..];
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut replaced = None;
        if let Some(id) = new.replace {
            let found = tx.execute(
                "UPDATE devices SET name = ?2, scope = ?3, token_hash = ?4, platform = ?5,
                     hostname = ?6, app_version = ?7, last_ip = ?8, last_seen_at = ?9,
                     paired_at = ?9, revoked_at = NULL
                 WHERE id = ?1",
                params![
                    id,
                    new.name,
                    new.scope.as_str(),
                    hash,
                    new.platform,
                    new.hostname,
                    new.app_version,
                    new.ip,
                    now
                ],
            )?;
            tx.execute("DELETE FROM sessions WHERE device_id = ?1", [id])?;
            replaced = (found == 1).then_some(id);
        }
        let id = match replaced {
            Some(id) => id,
            None => {
                tx.execute(
                    "INSERT INTO devices (name, scope, token_hash, created_at, platform,
                         hostname, app_version, last_ip, last_seen_at, paired_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?4, ?4)",
                    params![
                        new.name,
                        new.scope.as_str(),
                        hash,
                        now,
                        new.platform,
                        new.hostname,
                        new.app_version,
                        new.ip
                    ],
                )?;
                tx.last_insert_rowid()
            }
        };
        tx.commit()?;
        Ok((id, token))
    }

    /// Revoke a device: its token and the sessions traded for it stop
    /// working at once. Its entry stays, marked revoked. Returns whether
    /// there is such a device.
    pub fn revoke_device(&self, id: i64, now: i64) -> rusqlite::Result<bool> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let found = tx.execute(
            "UPDATE devices SET revoked_at = COALESCE(revoked_at, ?2) WHERE id = ?1",
            params![id, now],
        )?;
        tx.execute("DELETE FROM sessions WHERE device_id = ?1", [id])?;
        tx.commit()?;
        Ok(found == 1)
    }

    /// Forget a device, revoking it if it wasn't. Returns whether there was
    /// such a device.
    pub fn remove_device(&self, id: i64) -> rusqlite::Result<bool> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM sessions WHERE device_id = ?1", [id])?;
        let found = tx.execute("DELETE FROM devices WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(found == 1)
    }

    /// Rename or re-scope a device. A new scope applies to its token and the
    /// sessions traded for it from their next request. Returns whether there
    /// is such a device.
    pub fn update_device(
        &self,
        id: i64,
        name: Option<&str>,
        scope: Option<Scope>,
    ) -> rusqlite::Result<bool> {
        let found = self.conn().execute(
            "UPDATE devices SET name = COALESCE(?2, name), scope = COALESCE(?3, scope)
             WHERE id = ?1",
            params![id, name, scope.map(Scope::as_str)],
        )?;
        Ok(found == 1)
    }

    pub fn pairing_setup(&self) -> rusqlite::Result<PairingSetup> {
        Ok(self
            .conn()
            .query_row(
                "SELECT ca_path, allow_http FROM pairing_setup WHERE id = 1",
                [],
                |r| {
                    Ok(PairingSetup {
                        ca_path: r.get(0)?,
                        allow_http: r.get(1)?,
                    })
                },
            )
            .optional()?
            .unwrap_or_default())
    }

    pub fn set_pairing_setup(&self, setup: &PairingSetup) -> rusqlite::Result<()> {
        self.conn().execute(
            "INSERT INTO pairing_setup (id, ca_path, allow_http) VALUES (1, ?1, ?2)
             ON CONFLICT (id) DO UPDATE SET ca_path = excluded.ca_path,
                 allow_http = excluded.allow_http",
            params![setup.ca_path, setup.allow_http],
        )?;
        Ok(())
    }

    /// Add a device with a new token, as pairing does, but not used yet.
    #[cfg(test)]
    pub fn add_device(
        &self,
        name: &str,
        scope: Scope,
        now: i64,
    ) -> rusqlite::Result<(i64, String)> {
        let new = NewDevice {
            name,
            scope,
            replace: None,
            platform: None,
            hostname: None,
            app_version: None,
            ip: None,
        };
        let (id, token) = self.pair_device(&new, now)?;
        self.forget_last_seen(id)?;
        Ok((id, token))
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
}

/// Add the [`DEVICE_COLUMNS`] a `devices` table made before them lacks, in
/// a write transaction, so `set-password` run while the service starts
/// can't add them twice.
fn migrate(conn: &mut Connection) -> rusqlite::Result<()> {
    fn missing(conn: &Connection) -> rusqlite::Result<Vec<(&'static str, &'static str)>> {
        let mut stmt = conn.prepare("SELECT name FROM pragma_table_info('devices')")?;
        let present: Vec<String> = stmt
            .query_map([], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        Ok(DEVICE_COLUMNS
            .iter()
            .copied()
            .filter(|(c, _)| !present.iter().any(|p| p == c))
            .collect())
    }
    if missing(conn)?.is_empty() {
        return Ok(());
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for (column, kind) in missing(&tx)? {
        tx.execute_batch(&format!("ALTER TABLE devices ADD COLUMN {column} {kind}"))?;
    }
    tx.commit()
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
        s.touch_device(id, 5, None, None).unwrap();
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
        s.touch_device(id, 1, None, None).unwrap();
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
        s.update_device(id, None, Some(Scope::Sync)).unwrap();
        assert_eq!(
            s.session(&session, 2).unwrap().unwrap().access.scope,
            Scope::Sync
        );
        s.revoke_device(id, 3).unwrap();
        assert!(s.session(&session, 4).unwrap().is_none());
    }

    fn phone(replace: Option<i64>) -> NewDevice<'static> {
        NewDevice {
            name: "Pixel 8",
            scope: Scope::Manage,
            replace,
            platform: Some("android"),
            hostname: Some("pixel-8"),
            app_version: Some("0.1.0"),
            ip: Some("192.168.1.20"),
        }
    }

    #[test]
    fn pairing_adds_a_device_and_replacing_keeps_its_id() {
        let s = store("pair");
        let (id, token) = s.pair_device(&phone(None), 100).unwrap();
        let d = s.device_info(id).unwrap().unwrap();
        assert_eq!(
            d,
            DeviceInfo {
                id,
                name: "Pixel 8".into(),
                scope: Scope::Manage,
                platform: Some("android".into()),
                hostname: Some("pixel-8".into()),
                app_version: Some("0.1.0".into()),
                created_at: 100,
                paired_at: 100,
                last_seen_at: Some(100),
                last_ip: Some("192.168.1.20".into()),
                revoked_at: None,
            }
        );
        let traded = s.create_device_session(id, Scope::Manage, 100).unwrap();
        let (other, _) = s.add_device("Desktop", Scope::Manage, 100).unwrap();

        // Reinstalled: a new token for the same entry, which ends the old
        // one and its sessions.
        let again = NewDevice {
            name: "Pixel 8 (new)",
            scope: Scope::Sync,
            app_version: Some("0.2.0"),
            ..phone(Some(id))
        };
        let (same, new_token) = s.pair_device(&again, 200).unwrap();
        assert_eq!(same, id);
        assert_ne!(new_token, token);
        assert!(s.device(&token, 201).unwrap().is_none());
        assert!(s.session(&traded, 201).unwrap().is_none());
        assert_eq!(s.device(&new_token, 201).unwrap().unwrap().id, id);
        let d = s.device_info(id).unwrap().unwrap();
        assert_eq!(
            (d.name.as_str(), d.scope, d.created_at, d.paired_at),
            ("Pixel 8 (new)", Scope::Sync, 100, 200)
        );
        assert_eq!(d.app_version.as_deref(), Some("0.2.0"));
        // The other device is untouched.
        assert!(s.device_info(other).unwrap().unwrap().revoked_at.is_none());

        // A revoked device comes back when it's replaced.
        s.revoke_device(id, 300).unwrap();
        assert!(s.device(&new_token, 301).unwrap().is_none());
        let (same, back) = s.pair_device(&phone(Some(id)), 400).unwrap();
        assert_eq!(same, id);
        assert!(s.device(&back, 401).unwrap().is_some());
        assert_eq!(s.device_info(id).unwrap().unwrap().revoked_at, None);

        // One removed meanwhile is added afresh.
        s.remove_device(id).unwrap();
        let (fresh, _) = s.pair_device(&phone(Some(id)), 500).unwrap();
        assert_ne!(fresh, id);
        assert_eq!(s.devices().unwrap().len(), 2);
    }

    #[test]
    fn revoked_devices_are_kept_until_removed() {
        let s = store("revoke");
        let (a, token) = s.add_device("A", Scope::Manage, 0).unwrap();
        let (b, _) = s.add_device("B", Scope::Sync, 0).unwrap();
        let session = s.create_device_session(a, Scope::Manage, 0).unwrap();
        assert!(s.revoke_device(a, 10).unwrap());
        assert!(s.device(&token, 11).unwrap().is_none());
        assert!(s.session(&session, 11).unwrap().is_none());
        // Revoking again keeps the first time.
        assert!(s.revoke_device(a, 20).unwrap());
        assert_eq!(s.device_info(a).unwrap().unwrap().revoked_at, Some(10));
        // Listed after the ones in use.
        let ids: Vec<i64> = s.devices().unwrap().iter().map(|d| d.id).collect();
        assert_eq!(ids, [b, a]);
        assert!(s.remove_device(a).unwrap());
        assert!(!s.remove_device(a).unwrap());
        assert!(!s.revoke_device(a, 30).unwrap());
        assert!(!s.update_device(a, Some("gone"), None).unwrap());
        assert!(s.update_device(b, Some("Player"), None).unwrap());
        let d = s.device_info(b).unwrap().unwrap();
        assert_eq!((d.name.as_str(), d.scope), ("Player", Scope::Sync));
    }

    #[test]
    fn using_a_token_notes_where_from() {
        let s = store("seen");
        let (id, _) = s.add_device("Phone", Scope::Manage, 0).unwrap();
        s.touch_device(id, 10, Some("192.168.1.5"), Some("0.1.0"))
            .unwrap();
        // Without a version, the last one stays.
        s.touch_device(id, 100, Some("10.0.0.7"), None).unwrap();
        let d = s.device_info(id).unwrap().unwrap();
        assert_eq!(d.last_seen_at, Some(100));
        assert_eq!(d.last_ip.as_deref(), Some("10.0.0.7"));
        assert_eq!(d.app_version.as_deref(), Some("0.1.0"));
    }

    #[test]
    fn pairing_setup_round_trips() {
        let s = store("pairing-setup");
        assert_eq!(s.pairing_setup().unwrap(), PairingSetup::default());
        let setup = PairingSetup {
            ca_path: Some("/home/me/.local/share/mkcert/rootCA.pem".into()),
            allow_http: true,
        };
        s.set_pairing_setup(&setup).unwrap();
        assert_eq!(s.pairing_setup().unwrap(), setup);
        s.set_pairing_setup(&PairingSetup::default()).unwrap();
        assert_eq!(s.pairing_setup().unwrap(), PairingSetup::default());
    }

    #[test]
    fn a_devices_table_from_before_pairing_gets_its_new_columns() {
        let dir =
            std::env::temp_dir().join(format!("coversyncd-store-migrate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("inbox.sqlite3");
        // The table as SRV-01 made it, with a device in it.
        let old = Connection::open(&path).unwrap();
        old.execute_batch(
            "CREATE TABLE devices (
                id INTEGER PRIMARY KEY, name TEXT NOT NULL,
                scope TEXT NOT NULL CHECK (scope IN ('manage', 'sync')),
                token_hash BLOB NOT NULL UNIQUE, created_at INTEGER NOT NULL,
                last_seen_at INTEGER, revoked_at INTEGER);",
        )
        .unwrap();
        let token = new_token().unwrap();
        old.execute(
            "INSERT INTO devices (name, scope, token_hash, created_at) VALUES ('Old', 'manage', ?1, 5)",
            [&digest(&token)[..]],
        )
        .unwrap();
        drop(old);
        let s = Store::open(&path).unwrap();
        let d = s.device(&token, 10).unwrap().unwrap();
        s.touch_device(d.id, 10, Some("192.168.1.9"), None).unwrap();
        let info = s.device_info(d.id).unwrap().unwrap();
        assert_eq!((info.paired_at, info.platform), (5, None));
        assert_eq!(info.last_ip.as_deref(), Some("192.168.1.9"));
        // Opening it again changes nothing.
        drop(s);
        Store::open(&path).unwrap();
    }
}
