//! The app's settings, in `config.json` in the app's config directory
//! (`~/.config/io.github.brpjerry.coversync/` on Linux).

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
    /// The proxy's port in the last run, to keep it (and with it the UI's
    /// settings in local storage) when it's free.
    pub port: Option<u16>,
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
        let current = match fs::read(&path) {
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
