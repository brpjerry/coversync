//! What the agent keeps on this computer: which server it syncs with, the
//! folder the copy is in, and (only for a computer without a keychain) a
//! file holding its token. They are in `sync.json`, in the app's settings
//! folder, where the app's settings page writes them and `coversync setup`
//! does without the app.
//!
//! The folder is never sent to the server: it knows what a device holds,
//! not where.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use coversync_proxy::Upstream;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::APP_ID;

/// The format's version, for a later one to tell an older file by.
pub const VERSION: u32 = 1;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub version: u32,
    /// The server's address, as [`Upstream::url`] writes it: what the
    /// keychain files this computer's token under.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    /// The folder the copy of the Library is in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root: Option<PathBuf>,
    /// A file holding the token, for a computer with no keychain.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_file: Option<PathBuf>,
    /// What a newer build wrote and this one doesn't know: kept as it is.
    #[serde(flatten)]
    pub other: Map<String, Value>,
}

impl Config {
    /// The settings in `file`; none at all if there's no such file.
    pub fn load(file: &Path) -> Result<Self, String> {
        match fs::read(file) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| format!("{} can't be read: {e}.", file.display())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("{} can't be read: {e}.", file.display())),
        }
    }

    /// Writes the settings to `file`, whole or not at all.
    pub fn save(&self, file: &Path) -> Result<(), String> {
        let mut config = self.clone();
        config.version = VERSION;
        let failed = |e: io::Error| format!("{} can't be written: {e}.", file.display());
        let dir = file.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(dir).map_err(failed)?;
        // A path that isn't valid UTF-8 has no place in JSON: [`path`]
        // turns those away before they get here.
        let mut text = serde_json::to_vec_pretty(&config)
            .map_err(|e| format!("{} can't be written: {e}.", file.display()))?;
        text.push(b'\n');
        let tmp = file.with_extension("json.new");
        let mut out = fs::File::create(&tmp).map_err(failed)?;
        out.write_all(&text).map_err(failed)?;
        out.sync_all().map_err(failed)?;
        fs::rename(&tmp, file).map_err(failed)
    }
}

/// A server's address as it's kept, and as the keychain knows it.
pub fn server_url(input: &str) -> Result<String, String> {
    Upstream::parse(input)
        .map(|upstream| upstream.url())
        .map_err(|e| e.to_string())
}

/// A path to keep in the settings, made absolute. One that isn't valid
/// UTF-8 can't be written there (the file is JSON), nor remembered between
/// runs, so it's turned away.
pub fn path(input: &Path) -> Result<PathBuf, String> {
    let path = std::path::absolute(input)
        .map_err(|e| format!("{} isn't a usable path: {e}.", input.display()))?;
    if path.to_str().is_none() {
        return Err(format!(
            "{} can't be kept in the settings: its name isn't valid UTF-8.",
            path.display()
        ));
    }
    Ok(path)
}

/// A folder to sync into, as an absolute path. It has to be there already:
/// a folder made here by mistake (a disk that isn't mounted) would get the
/// whole Library.
pub fn folder(input: &Path) -> Result<PathBuf, String> {
    let path = path(input)?;
    match fs::metadata(&path) {
        Ok(meta) if meta.is_dir() => Ok(path),
        Ok(_) => Err(format!("{} isn't a folder.", path.display())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Err(format!(
            "There's no folder {}. Make it first.",
            path.display()
        )),
        Err(e) => Err(format!("{} can't be used: {e}.", path.display())),
    }
}

/// `$<var>` if it's an absolute path, or else `fallback` under the home
/// folder, as the XDG base directory specification has it.
fn base(var: &str, fallback: &str, get: &dyn Fn(&str) -> Option<String>) -> PathBuf {
    get(var)
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .unwrap_or_else(|| PathBuf::from(get("HOME").unwrap_or_default()).join(fallback))
}

fn from_env(var: &str) -> Option<String> {
    std::env::var(var).ok().filter(|value| !value.is_empty())
}

/// The settings file, in the app's settings folder
/// (`~/.config/io.github.brpjerry.coversync/sync.json`).
pub fn file() -> PathBuf {
    file_from(&from_env)
}

fn file_from(get: &dyn Fn(&str) -> Option<String>) -> PathBuf {
    base("XDG_CONFIG_HOME", ".config", get)
        .join(APP_ID)
        .join("sync.json")
}

/// What the agent remembers between runs
/// (`~/.local/state/io.github.brpjerry.coversync/sync-state.json`).
pub fn state_file() -> PathBuf {
    state_file_from(&from_env)
}

fn state_file_from(get: &dyn Fn(&str) -> Option<String>) -> PathBuf {
    base("XDG_STATE_HOME", ".local/state", get)
        .join(APP_ID)
        .join("sync-state.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("coversync-cli-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn settings_come_back_as_saved_with_what_this_build_doesnt_know() {
        let dir = scratch("config");
        let file = dir.join("deeper/sync.json");
        assert_eq!(Config::load(&file).unwrap(), Config::default());
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(
            &file,
            br#"{"version": 7, "server": "https://a.example/", "later": {"x": 1}}"#,
        )
        .unwrap();
        let mut config = Config::load(&file).unwrap();
        assert_eq!(config.server.as_deref(), Some("https://a.example/"));
        config.root = Some("/music".into());
        config.save(&file).unwrap();
        let again = Config::load(&file).unwrap();
        assert_eq!(again.version, VERSION);
        assert_eq!(again.root.as_deref(), Some(Path::new("/music")));
        assert_eq!(again.other["later"]["x"], 1);
        assert_eq!(again.token_file, None);
        // Nothing half-written is left beside it.
        assert_eq!(fs::read_dir(file.parent().unwrap()).unwrap().count(), 1);
        fs::write(&file, b"{").unwrap();
        assert!(Config::load(&file).is_err());
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn a_server_is_kept_under_the_address_the_app_uses() {
        assert_eq!(
            server_url("n5pro.internal/coversync").unwrap(),
            "https://n5pro.internal/coversync/"
        );
        assert_eq!(
            server_url("http://127.0.0.1:8781").unwrap(),
            "http://127.0.0.1:8781/"
        );
        assert!(server_url("").is_err());
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn a_folder_has_to_be_there() {
        let dir = scratch("folder");
        assert_eq!(folder(&dir).unwrap(), dir);
        assert!(
            folder(&dir.join("missing"))
                .unwrap_err()
                .contains("Make it")
        );
        fs::write(dir.join("file"), b"").unwrap();
        assert!(
            folder(&dir.join("file"))
                .unwrap_err()
                .contains("isn't a folder")
        );
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn a_path_that_json_cant_hold_is_turned_away_not_crashed_on() {
        use std::os::unix::ffi::OsStrExt;
        let dir = scratch("not-utf8");
        let odd = dir.join(std::ffi::OsStr::from_bytes(b"caf\xe9"));
        fs::create_dir(&odd).unwrap();
        assert!(path(&odd).unwrap_err().contains("UTF-8"));
        assert!(folder(&odd).unwrap_err().contains("UTF-8"));
        assert_eq!(path(&dir.join("café")).unwrap(), dir.join("café"));
        // One that got into the settings some other way fails the save,
        // and leaves the file as it was.
        let file = dir.join("sync.json");
        let good = Config {
            root: Some(dir.clone()),
            ..Default::default()
        };
        good.save(&file).unwrap();
        let bad = Config {
            root: Some(odd),
            ..Default::default()
        };
        assert!(bad.save(&file).is_err());
        assert_eq!(Config::load(&file).unwrap().root, Some(dir.clone()));
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn the_files_are_in_the_apps_folders() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |var: &str| {
                pairs
                    .iter()
                    .find(|(name, _)| *name == var)
                    .map(|(_, value)| value.to_string())
            }
        };
        let home = env(&[("HOME", "/home/a")]);
        assert_eq!(
            file_from(&home),
            Path::new("/home/a/.config/io.github.brpjerry.coversync/sync.json")
        );
        assert_eq!(
            state_file_from(&home),
            Path::new("/home/a/.local/state/io.github.brpjerry.coversync/sync-state.json")
        );
        let set = env(&[
            ("HOME", "/home/a"),
            ("XDG_CONFIG_HOME", "/c"),
            ("XDG_STATE_HOME", "relative"),
        ]);
        assert_eq!(
            file_from(&set),
            Path::new("/c/io.github.brpjerry.coversync/sync.json")
        );
        assert_eq!(
            state_file_from(&set),
            Path::new("/home/a/.local/state/io.github.brpjerry.coversync/sync-state.json")
        );
    }
}
