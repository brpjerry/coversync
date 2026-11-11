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

/// The settings' version. 1: the HTTP cache the first builds' servers
/// shared (one port, so one origin) has been cleared.
pub const VERSION: u32 = 1;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Which of the changes behind [`VERSION`] are done; 0 for settings from
    /// before it.
    pub version: u32,
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
    /// The servers this computer is paired with, by URL: what each calls
    /// it. The tokens themselves are in the keychain, never here.
    pub paired: BTreeMap<String, Device>,
    /// The one port the first builds kept, for every server; read once.
    #[serde(skip_serializing)]
    port: Option<u16>,
}

/// This computer, as a server it's paired with lists it among its devices.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    /// The server's number for it.
    pub id: i64,
    /// The name it was given in Add device.
    pub name: String,
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
        let saved = self.server.as_ref();
        let kept: Vec<&String> = self
            .recent
            .iter()
            .filter(|server| Some(*server) != saved)
            .take(KEEP_RECENT)
            .chain(saved)
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

    /// Forgets `url`'s server: it's no longer the saved one, paired, or
    /// among those shown, and its port is retired.
    pub fn forget(&mut self, url: &str) {
        if self.server.as_deref() == Some(url) {
            self.server = None;
        }
        self.paired.remove(url);
        self.recent.retain(|server| server != url);
        if let Some(port) = self.ports.remove(url) {
            self.retire(port);
        }
    }

    fn retire(&mut self, port: u16) {
        if !self.retired.contains(&port) {
            self.retired.push(port);
        }
    }

    /// Reads what earlier builds wrote: the first builds' single `port` goes
    /// to the saved server, unless it has one already (then it's retired),
    /// and servers in `ports` that `recent` doesn't list (it came later) go
    /// at its end, so none is dropped as if unused.
    fn migrate(&mut self) {
        if let Some(port) = self.port.take() {
            match &self.server {
                Some(server) if !self.ports.contains_key(server) => {
                    self.ports.insert(server.clone(), port);
                }
                _ if self.ports.values().any(|&kept| kept == port) => {}
                _ => self.retire(port),
            }
        }
        let missing: Vec<String> = self
            .ports
            .keys()
            .filter(|server| !self.recent.contains(server))
            .cloned()
            .collect();
        self.recent.extend(missing);
    }

    /// Whether the webview's HTTP cache still has to be cleared once: the
    /// settings come from a build before [`VERSION`] 1.
    pub fn needs_cache_clear(&self) -> bool {
        self.version < 1
    }
}

pub struct Store {
    path: PathBuf,
    current: Mutex<Config>,
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
            // A first run: nothing from earlier builds to deal with.
            Err(err) if err.kind() == io::ErrorKind::NotFound => Config {
                version: VERSION,
                ..Config::default()
            },
            Err(err) => {
                tracing::warn!("couldn't read {}: {err}", path.display());
                Config::default()
            }
        };
        current.migrate();
        Self {
            path,
            current: Mutex::new(current),
        }
    }

    pub fn get(&self) -> Config {
        self.current.lock().expect("config lock").clone()
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
/// `cookies.json` are each written whole by one process. The app's
/// single-instance handover usually keeps a second process out, but not
/// where it can't work (no socket to hand a launch over).
pub struct ConfigLock(#[allow(dead_code)] Option<File>);

impl ConfigLock {
    /// Refuses only when another process holds the lock. Where locking
    /// itself fails (a file system without locks, a directory that can't be
    /// written), it warns and goes on without, as before the lock existed.
    pub fn take(dir: &Path) -> Result<Self, String> {
        let path = dir.join("lock");
        let file = fs::create_dir_all(dir).and_then(|()| {
            File::options()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&path)
        });
        let file = match file {
            Ok(file) => file,
            Err(err) => {
                tracing::warn!(
                    "couldn't open {}: {err}; going on without it",
                    path.display()
                );
                return Ok(Self(None));
            }
        };
        match file.try_lock() {
            Ok(()) => Ok(Self(Some(file))),
            Err(fs::TryLockError::WouldBlock) => Err(format!(
                "coversync is already open with the settings in {}. Use that window, or \
                 close it before starting another.",
                dir.display()
            )),
            Err(fs::TryLockError::Error(err)) => {
                tracing::warn!(
                    "couldn't lock {}: {err}; going on without it",
                    path.display()
                );
                Ok(Self(None))
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
    fn forgets_a_server_whole() {
        let mut config = Config {
            server: Some(server(0)),
            ..Config::default()
        };
        config.shown(&server(0), 4000);
        config.shown(&server(1), 4001);
        for n in [0, 1] {
            let device = Device {
                id: n as i64,
                name: "Laptop".into(),
            };
            config.paired.insert(server(n), device);
        }
        config.forget(&server(0));
        assert_eq!(config.server, None);
        assert_eq!(config.recent, [server(1)]);
        assert_eq!(config.ports.keys().collect::<Vec<_>>(), [&server(1)]);
        assert_eq!(config.paired.keys().collect::<Vec<_>>(), [&server(1)]);
        assert!(
            config.taken_except(&server(2)).contains(&4000),
            "its port goes to no other server"
        );
        // One it never knew: nothing changes.
        let before = serde_json::to_string(&config).unwrap();
        config.forget(&server(9));
        assert_eq!(serde_json::to_string(&config).unwrap(), before);
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
        assert!(store.get().needs_cache_clear());
        assert_eq!(store.get().ports.get("http://a/"), Some(&4000));
        assert_eq!(store.get().recent, ["http://a/"]);
        store.update(|_| ()).unwrap();
        let written = fs::read_to_string(dir.join("config.json")).unwrap();
        assert!(!written.contains("\"port\""), "{written}");
        assert!(
            Store::open(dir.clone()).get().needs_cache_clear(),
            "until the clear is recorded"
        );
        store.update(|config| config.version = VERSION).unwrap();
        assert!(!Store::open(dir.clone()).get().needs_cache_clear());
        fs::remove_dir_all(dir).unwrap();

        // An entry in `ports` wins, and the old port is retired.
        let (store, dir) = open_with(
            "legacy-ports",
            r#"{"server": "http://a/", "port": 4000, "ports": {"http://a/": 5000}}"#,
        );
        assert_eq!(store.get().ports.get("http://a/"), Some(&5000));
        assert_eq!(store.get().retired, [4000]);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn keeps_every_server_of_a_ports_only_config() {
        // What the build before `recent` wrote: `ports`, nothing else.
        let ports: Vec<String> = (1..=4)
            .map(|n| format!("\"{}\": {}", server(n), 4000 + n))
            .collect();
        let json = format!(
            r#"{{"server": "{}", "ports": {{{}}}}}"#,
            server(1),
            ports.join(", ")
        );
        let (store, dir) = open_with("ports-only", &json);
        let mut config = store.get();
        assert!(config.needs_cache_clear(), "its cache may be shared too");
        assert_eq!(config.recent.len(), 4);
        let dropped = config.shown(&server(2), 4002);
        assert!(dropped.is_empty(), "{dropped:?}");
        assert_eq!(config.ports.len(), 4);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_first_run_has_nothing_to_clear() {
        let dir = std::env::temp_dir().join(format!("coversync-config-new-{}", std::process::id()));
        assert!(!Store::open(dir).get().needs_cache_clear());
    }

    #[test]
    fn reopening_the_saved_server_keeps_eight_others() {
        let mut config = Config {
            server: Some(server(0)),
            ..Config::default()
        };
        for n in 0..=KEEP_RECENT {
            config.shown(&server(n), 1000 + n as u16);
        }
        assert_eq!(config.ports.len(), 1 + KEEP_RECENT);
        let dropped = config.shown(&server(0), 1000);
        assert!(dropped.is_empty(), "{dropped:?}");
        assert_eq!(config.ports.len(), 1 + KEEP_RECENT);
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
