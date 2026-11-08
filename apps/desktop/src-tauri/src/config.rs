//! The app's settings, in `config.json` in the app's config directory
//! (`~/.config/io.github.brpjerry.coversync/` on Linux). One instance of the
//! app runs at a time, so one process writes it.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// The server's URL, as the fallback page last saved it.
    pub server: Option<String>,
    /// The proxy's port for each server (by its URL), kept so each server
    /// has an origin of its own, the same in every run: its cache and the
    /// UI's settings in local storage are its own.
    pub ports: BTreeMap<String, u16>,
    /// The one port earlier builds kept, read into `ports` for the saved
    /// server.
    #[serde(skip_serializing)]
    port: Option<u16>,
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
            Err(err) if err.kind() == io::ErrorKind::NotFound => Config::default(),
            Err(err) => {
                tracing::warn!("couldn't read {}: {err}", path.display());
                Config::default()
            }
        };
        if let (Some(port), Some(server)) = (current.port.take(), current.server.clone()) {
            current.ports.entry(server).or_insert(port);
        }
        Self {
            path,
            current: Mutex::new(current),
        }
    }

    pub fn get(&self) -> Config {
        self.current.lock().expect("config lock").clone()
    }

    /// Changes the settings and writes them out, replacing the file whole.
    pub fn update(&self, change: impl FnOnce(&mut Config)) -> io::Result<()> {
        let mut current = self.current.lock().expect("config lock");
        change(&mut current);
        let json = serde_json::to_vec_pretty(&*current).map_err(io::Error::other)?;
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        // Its own name for each process, so two runs saving at once can't
        // mix their writes.
        let partial = self
            .path
            .with_extension(format!("json.{}.partial", std::process::id()));
        fs::write(&partial, json)?;
        fs::rename(&partial, &self.path)
    }
}
