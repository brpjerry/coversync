//! The app's settings, in `config.json` in the app's config directory
//! (`~/.config/io.github.brpjerry.coversync/` on Linux), and the lock that
//! keeps a second process off them ([`ConfigLock`]).

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// How many recently shown servers keep their ports, besides the saved one.
pub const KEEP_RECENT: usize = 8;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// The server's URL, as the fallback page last saved it.
    pub server: Option<String>,
    /// The proxy's port for each server (by its URL), kept so each server
    /// has an origin of its own, the same in every run: its cache and the
    /// UI's settings in local storage are its own.
    pub ports: BTreeMap<String, u16>,
    /// The servers in `ports`, the most recently shown first.
    pub recent: Vec<String>,
    /// Ports given up: one taken by something else when its server started,
    /// or a server's dropped from `ports`. Their origins may still hold that
    /// server's cache, so no other server ever gets them.
    pub retired: Vec<u16>,
    /// The one port the first builds kept, for every server; read once.
    #[serde(skip_serializing)]
    port: Option<u16>,
}

impl Config {
    /// Records `url` as shown, on `port`: keeps the port for it (retiring the
    /// one it had, if another), and drops the ports of servers that are
    /// neither the saved one nor among the [`KEEP_RECENT`] most recently
    /// shown, retiring them too. Returns the servers dropped.
    pub fn shown(&mut self, url: &str, port: u16) -> Vec<String> {
        if let Some(old) = self.ports.insert(url.to_owned(), port)
            && old != port
        {
            self.retire(old);
        }
        self.recent.retain(|server| server != url);
        self.recent.insert(0, url.to_owned());
        let kept: Vec<&String> = self
            .recent
            .iter()
            .take(KEEP_RECENT)
            .chain(self.server.as_ref())
            .collect();
        let dropped: Vec<String> = self
            .ports
            .keys()
            .filter(|server| !kept.contains(server))
            .cloned()
            .collect();
        for server in &dropped {
            if let Some(port) = self.ports.remove(server) {
                self.retire(port);
            }
        }
        let ports = &self.ports;
        self.recent.retain(|server| ports.contains_key(server));
        dropped
    }

    /// The ports `url`'s server mustn't get: other servers' and retired
    /// ones.
    pub fn taken_except(&self, url: &str) -> Vec<u16> {
        self.ports
            .iter()
            .filter(|(server, _)| *server != url)
            .map(|(_, port)| *port)
            .chain(self.retired.iter().copied())
            .collect()
    }

    fn retire(&mut self, port: u16) {
        if !self.retired.contains(&port) {
            self.retired.push(port);
        }
    }

    /// Reads the first builds' single `port` into the saved server's, unless
    /// it has one already; otherwise it's retired. Says whether there was
    /// one: its origin held every server's cache.
    fn migrate(&mut self) -> bool {
        let Some(port) = self.port.take() else {
            return false;
        };
        match &self.server {
            Some(server) if !self.ports.contains_key(server) => {
                self.ports.insert(server.clone(), port);
            }
            _ if self.ports.values().any(|&kept| kept == port) => {}
            _ => self.retire(port),
        }
        // Servers from builds that kept no order come after the rest.
        let missing: Vec<String> = self
            .ports
            .keys()
            .filter(|server| !self.recent.contains(server))
            .cloned()
            .collect();
        self.recent.extend(missing);
        true
    }
}

pub struct Store {
    path: PathBuf,
    current: Mutex<Config>,
    migrated: bool,
}

impl Store {
    /// Reads `config.json` in `dir`. A missing or unreadable file gives the
    /// defaults.
    pub fn open(dir: PathBuf) -> Self {
        let path = dir.join("config.json");
        let mut current: Config = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|err| {
                tracing::warn!("ignoring {}: {err}", path.display());
                Config::default()
            }),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Config::default(),
            Err(err) => {
                tracing::warn!("couldn't read {}: {err}", path.display());
                Config::default()
            }
        };
        let migrated = current.migrate();
        Self {
            path,
            current: Mutex::new(current),
            migrated,
        }
    }

    pub fn get(&self) -> Config {
        self.current.lock().expect("config lock").clone()
    }

    /// Whether it held the first builds' single `port`, shared by every
    /// server: the webview's HTTP cache may hold their answers under one
    /// origin.
    pub fn had_shared_port(&self) -> bool {
        self.migrated
    }

    /// Changes the settings and writes them out, replacing the file whole.
    pub fn update<T>(&self, change: impl FnOnce(&mut Config) -> T) -> io::Result<T> {
        let mut current = self.current.lock().expect("config lock");
        let out = change(&mut current);
        let json = serde_json::to_vec_pretty(&*current).map_err(io::Error::other)?;
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        let partial = self
            .path
            .with_extension(format!("json.{}.partial", std::process::id()));
        fs::write(&partial, json)?;
        fs::rename(&partial, &self.path)?;
        Ok(out)
    }
}

/// A lock on the config directory for the whole run: `config.json` and
/// `cookies.json` are each written whole by one process. The single-instance
/// plugin usually keeps a second process out, but not without a D-Bus
/// session bus, or with another one.
pub struct ConfigLock(#[allow(dead_code)] File);

impl ConfigLock {
    pub fn take(dir: &Path) -> Result<Self, String> {
        fs::create_dir_all(dir)
            .map_err(|err| format!("Couldn't create {}: {err}", dir.display()))?;
        let path = dir.join("lock");
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|err| format!("Couldn't open {}: {err}", path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok(Self(file)),
            Err(fs::TryLockError::WouldBlock) => Err(format!(
                "coversync is already open with the settings in {}. Use that window, or \
                 close it before starting another.",
                dir.display()
            )),
            Err(fs::TryLockError::Error(err)) => {
                Err(format!("Couldn't lock {}: {err}", path.display()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(n: usize) -> String {
        format!("http://server-{n}/")
    }

    #[test]
    fn keeps_the_saved_server_and_the_recent_ones() {
        let mut config = Config {
            server: Some(server(0)),
            ..Config::default()
        };
        config.shown(&server(0), 1000);
        let mut dropped = Vec::new();
        for n in 1..=10 {
            dropped.extend(config.shown(&server(n), 1000 + n as u16));
        }
        assert_eq!(dropped, [server(1), server(2)]);
        assert_eq!(config.ports.len(), 1 + KEEP_RECENT);
        assert_eq!(
            config.ports.get(&server(0)),
            Some(&1000),
            "the saved one stays"
        );
        assert_eq!(config.recent.first(), Some(&server(10)));
        assert_eq!(
            config.retired,
            [1001, 1002],
            "dropped ports are never reused"
        );
        assert!(config.taken_except(&server(11)).contains(&1001));
    }

    #[test]
    fn a_server_moved_off_its_port_retires_it() {
        let mut config = Config::default();
        config.shown(&server(1), 1000);
        config.shown(&server(1), 2000);
        assert_eq!(config.ports.get(&server(1)), Some(&2000));
        assert_eq!(config.retired, [1000]);
        assert_eq!(config.taken_except(&server(2)), [2000, 1000]);
        assert_eq!(config.taken_except(&server(1)), [1000]);
    }

    fn open_with(name: &str, json: &str) -> (Store, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("coversync-config-{name}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("config.json"), json).unwrap();
        (Store::open(dir.clone()), dir)
    }

    #[test]
    fn reads_the_first_builds_port() {
        // Kept for the saved server.
        let (store, dir) = open_with("legacy", r#"{"server": "http://a/", "port": 4000}"#);
        assert!(store.had_shared_port());
        assert_eq!(store.get().ports.get("http://a/"), Some(&4000));
        assert_eq!(store.get().recent, ["http://a/"]);
        store.update(|_| ()).unwrap();
        let written = fs::read_to_string(dir.join("config.json")).unwrap();
        assert!(!written.contains("\"port\""), "{written}");
        assert!(!Store::open(dir.clone()).had_shared_port());
        fs::remove_dir_all(dir).unwrap();

        // An entry in `ports` wins, and the old port is retired.
        let (store, dir) = open_with(
            "legacy-ports",
            r#"{"server": "http://a/", "port": 4000, "ports": {"http://a/": 5000}}"#,
        );
        assert_eq!(store.get().ports.get("http://a/"), Some(&5000));
        assert_eq!(store.get().retired, [4000]);
        fs::remove_dir_all(dir).unwrap();

        let (store, dir) = open_with("current", r#"{"server": "http://a/"}"#);
        assert!(!store.had_shared_port());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn one_process_holds_the_lock() {
        let dir = std::env::temp_dir().join(format!("coversync-lock-{}", std::process::id()));
        let first = ConfigLock::take(&dir).unwrap();
        let second = ConfigLock::take(&dir).err().unwrap();
        assert!(second.contains("already open"), "{second}");
        drop(first);
        assert!(
            ConfigLock::take(&dir).is_ok(),
            "free once the first lets go"
        );
        fs::remove_dir_all(dir).unwrap();
    }
}
