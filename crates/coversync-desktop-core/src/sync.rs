//! Background sync on this computer, as the app and the sync agent
//! (`coversync run`, in `coversync-cli`) share it.
//!
//! What's kept here is which server the computer syncs with, the folder the
//! copy is in, and (only for a computer without a keychain) a file holding
//! its token. They are in `sync.json`, in the app's settings folder
//! ([`Config`]), where the app's settings page writes them and `coversync
//! setup` does without the app. The folder is never sent to the server: it
//! knows what a device holds, not where.
//!
//! The agent runs as a systemd user service ([`Service`]), which the app
//! turns on and off; its line of status is what the settings page shows.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

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
        let mut text = serde_json::to_vec_pretty(&config).expect("settings serialize");
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

/// A folder to sync into, as an absolute path. It has to be there already:
/// a folder made here by mistake (a disk that isn't mounted) would get the
/// whole Library.
pub fn folder(input: &Path) -> Result<PathBuf, String> {
    let path = std::path::absolute(input)
        .map_err(|e| format!("{} isn't a usable path: {e}.", input.display()))?;
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

/// The agent's systemd user unit.
pub const UNIT: &str = "coversync-agent.service";
/// The agent's exit status when this computer isn't paired with the server
/// (any more): the unit doesn't start it again.
pub const EXIT_UNPAIRED: i32 = 3;

/// How the agent's service is.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ServiceState {
    /// Whether the unit is there at all (the `coversync-agent` package).
    pub installed: bool,
    /// Started with the session.
    pub enabled: bool,
    /// Running now.
    pub active: bool,
    /// What the agent last said it was doing.
    pub status: String,
    /// The status it ended with, if it ended by itself: see
    /// [`EXIT_UNPAIRED`].
    pub exit: Option<i32>,
}

/// The agent as a service: what the settings page turns on and off.
pub trait Service: Send + Sync {
    fn state(&self) -> ServiceState;
    /// Turns the agent on (started now, and with every session) or off.
    fn turn(&self, on: bool) -> Result<(), String>;
    /// Starts a running agent again, to take up new settings. One that
    /// isn't running stays off.
    fn restart(&self) -> Result<(), String>;
}

/// The agent's systemd user unit, through `systemctl --user`.
pub struct Systemd;

fn systemctl(args: &[&str]) -> Result<String, String> {
    let output = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .map_err(|e| format!("systemctl couldn't be run: {e}."))?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let said = String::from_utf8_lossy(&output.stderr);
    let said = said.trim();
    Err(if said.is_empty() {
        format!("systemctl {} failed.", args.join(" "))
    } else {
        said.to_owned()
    })
}

/// Reads `systemctl show`'s `Name=value` lines.
fn read_state(shown: &str) -> ServiceState {
    let value = |name: &str| {
        shown
            .lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix('='))
            .unwrap_or_default()
    };
    let active = matches!(value("ActiveState"), "active" | "activating" | "reloading");
    ServiceState {
        installed: value("LoadState") == "loaded",
        enabled: matches!(value("UnitFileState"), "enabled" | "enabled-runtime"),
        active,
        status: value("StatusText").to_owned(),
        // The last run's, which says nothing while one is under way.
        exit: (!active && value("ActiveState") == "failed")
            .then(|| value("ExecMainStatus").parse().ok())
            .flatten(),
    }
}

impl Service for Systemd {
    fn state(&self) -> ServiceState {
        let properties = [
            "show",
            UNIT,
            "-p",
            "LoadState",
            "-p",
            "UnitFileState",
            "-p",
            "ActiveState",
            "-p",
            "StatusText",
            "-p",
            "ExecMainStatus",
        ];
        systemctl(&properties)
            .map(|shown| read_state(&shown))
            .unwrap_or_default()
    }

    fn turn(&self, on: bool) -> Result<(), String> {
        if on {
            // One that gave up (it wasn't paired) is given another go.
            let _ = systemctl(&["reset-failed", UNIT]);
            systemctl(&["enable", "--now", UNIT]).map(|_| ())
        } else {
            systemctl(&["disable", "--now", UNIT]).map(|_| ())
        }
    }

    fn restart(&self) -> Result<(), String> {
        systemctl(&["try-restart", UNIT]).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Bugs caught: 0, False positives: 0
    #[test]
    fn the_services_state_is_read_from_systemctl() {
        let running = "LoadState=loaded\nActiveState=active\nUnitFileState=enabled\n\
                       StatusText=Syncing: 3 of 9 files, 0.1 of 0.4 GB, 80 MB/s.\nExecMainStatus=0\n";
        assert_eq!(
            read_state(running),
            ServiceState {
                installed: true,
                enabled: true,
                active: true,
                status: "Syncing: 3 of 9 files, 0.1 of 0.4 GB, 80 MB/s.".into(),
                exit: None,
            }
        );
        // Ended because the server refused the token: its status stays.
        let refused = "LoadState=loaded\nActiveState=failed\nUnitFileState=enabled\n\
                       StatusText=The server no longer accepts this computer.\nExecMainStatus=3\n";
        let state = read_state(refused);
        assert!(!state.active && state.enabled);
        assert_eq!(state.exit, Some(EXIT_UNPAIRED));
        // Off: stopped by hand, which is no failure.
        let off = "LoadState=loaded\nActiveState=inactive\nUnitFileState=disabled\n\
                   StatusText=\nExecMainStatus=0\n";
        assert_eq!(
            read_state(off),
            ServiceState {
                installed: true,
                ..Default::default()
            }
        );
        // The package isn't installed, or there's no systemd to ask.
        let missing = "LoadState=not-found\nActiveState=inactive\nUnitFileState=\n";
        assert_eq!(read_state(missing), ServiceState::default());
        assert_eq!(read_state(""), ServiceState::default());
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("coversync-sync-{name}-{}", std::process::id()));
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
