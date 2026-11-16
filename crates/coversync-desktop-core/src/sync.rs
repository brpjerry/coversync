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

use crate::{APP_ID, Problem};

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
        // A path that isn't valid UTF-8 has no place in JSON: [`path`]
        // turns those away before they get here.
        let mut text = serde_json::to_vec_pretty(&config)
            .map_err(|e| format!("{} can't be written: {e}.", file.display()))?;
        text.push(b'\n');
        write_whole(file, &text, 0o666).map_err(failed)
    }
}

/// A new, empty file in `dir`, for writing something whole before it's
/// put in place: under a short name of its own, so not another writer's
/// (two saves at once mustn't write into each other's), and not longer than
/// a name the folder could hold. `mode` is the most anyone may do with it.
pub fn temporary(dir: &Path, mode: u32) -> io::Result<(fs::File, PathBuf)> {
    use std::os::unix::fs::OpenOptionsExt;
    // A name left by a run that was cut short is passed over, not reused.
    for n in 0..64 {
        let path = dir.join(format!(".coversync-{}-{n}.tmp", std::process::id()));
        let made = fs::File::options()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&path);
        match made {
            Ok(file) => return Ok((file, path)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::ErrorKind::AlreadyExists.into())
}

/// Puts `bytes` at `file`, whole or not at all: written to a file beside
/// it and flushed, renamed over it, and the folder flushed too, so that a
/// power cut leaves the old contents or the new.
pub fn write_whole(file: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let dir = file
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let (mut out, tmp) = temporary(dir, mode)?;
    let written = out
        .write_all(bytes)
        .and_then(|()| out.sync_all())
        .and_then(|()| fs::rename(&tmp, file));
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
        return written;
    }
    // The rename is in the folder: flushed where the file system allows.
    if let Ok(folder) = fs::File::open(dir) {
        let _ = folder.sync_all();
    }
    Ok(())
}

/// A server's address as it's kept, and as the keychain knows it.
pub fn server_url(input: &str) -> Result<String, String> {
    Upstream::parse(input)
        .map(|upstream| upstream.url())
        .map_err(|e| e.to_string())
}

/// Whether the server the settings name is `server` (as [`server_url`]
/// writes one). The settings' may be spelled another way, by an older
/// build or by hand; one that isn't an address at all is no server's.
pub fn same_server(kept: &str, server: &str) -> bool {
    server_url(kept).is_ok_and(|kept| kept == server)
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
    /// The settings file the agent reads, when that can be told: the
    /// service has an environment of its own, which needn't be the app's
    /// (an app started with a settings folder of its own, say).
    fn settings_file(&self) -> Option<PathBuf> {
        None
    }
}

/// How long what was found out about the agent's settings file is gone by
/// before it's asked for again: the unit can be installed, or changed,
/// while the app runs, and an answer that couldn't be had is no answer.
const READS_FOR: std::time::Duration = std::time::Duration::from_secs(30);

/// The agent's systemd user unit, through `systemctl --user`.
#[derive(Default)]
pub struct Systemd {
    /// The settings file the unit's agent reads, and when that was asked.
    reads: std::sync::Mutex<Option<(std::time::Instant, PathBuf)>>,
}

/// The words of one of systemd's `Environment=` lines (`systemctl show -p
/// Environment`): separated by spaces, and one that has a space or a quote
/// in it is in double quotes, with a backslash before a quote or a
/// backslash.
fn environment(line: &str) -> Vec<String> {
    let line = line.trim_end_matches('\n');
    let line = line.strip_prefix("Environment=").unwrap_or(line);
    let (mut words, mut word, mut quoted, mut any) = (Vec::new(), String::new(), false, false);
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            '\\' => {
                // What follows stands for itself, but for the usual two.
                match chars.next() {
                    Some('n') => word.push('\n'),
                    Some('t') => word.push('\t'),
                    Some(other) => word.push(other),
                    None => {}
                }
                any = true;
            }
            c if c.is_whitespace() && !quoted => {
                if any {
                    words.push(std::mem::take(&mut word));
                    any = false;
                }
            }
            c => {
                word.push(c);
                any = true;
            }
        }
    }
    if any {
        words.push(word);
    }
    words
}

/// The settings file an agent reads that is started with these
/// environments, each as `systemctl show -p Environment` prints it: the
/// unit's own, over the service manager's. None when neither says where
/// the home folder is.
fn agents_file(unit: &str, manager: &str) -> Option<PathBuf> {
    let pairs: Vec<String> = environment(unit)
        .into_iter()
        .chain(environment(manager))
        .collect();
    let get = |name: &str| {
        let prefix = format!("{name}=");
        pairs
            .iter()
            .find_map(|pair| pair.strip_prefix(&prefix).map(str::to_owned))
            .filter(|value| !value.is_empty())
    };
    get("HOME")?;
    Some(file_from(&get))
}

/// Whether two paths name the same settings file: the same name in the
/// same folder, however the folder is reached (a link to it, say). The
/// file needn't be there yet, and a save puts a new one in its place, so
/// it's the folder that's looked at.
pub fn same_file(a: &Path, b: &Path) -> bool {
    let place = |path: &Path| {
        let name = path.file_name()?.to_owned();
        let folder = path.parent().filter(|dir| !dir.as_os_str().is_empty())?;
        let folder = fs::canonicalize(folder).unwrap_or_else(|_| folder.to_owned());
        Some(folder.join(name))
    };
    a == b || matches!((place(a), place(b)), (Some(a), Some(b)) if a == b)
}

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

    fn settings_file(&self) -> Option<PathBuf> {
        let mut reads = self.reads.lock().expect("reads lock");
        if let Some((asked, file)) = &*reads
            && asked.elapsed() < READS_FOR
        {
            return Some(file.clone());
        }
        // Not kept when it couldn't be had: asked again the next time.
        let unit = systemctl(&["show", UNIT, "-p", "Environment"]).ok()?;
        let manager = systemctl(&["show", "-p", "Environment"]).ok()?;
        let file = agents_file(&unit, &manager)?;
        *reads = Some((std::time::Instant::now(), file.clone()));
        Some(file)
    }
}

/// What the app's settings page shows of sync.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Overview {
    /// Whether the agent is installed at all.
    pub installed: bool,
    /// The server the window shows; none on the fallback page.
    pub server: Option<String>,
    /// Whether this computer is paired with that server: the agent syncs
    /// as the app's device.
    pub paired: bool,
    /// The server sync is set up with, which may be another one.
    pub syncs_with: Option<String>,
    /// The folder the copy is in. Only ever shown on this computer.
    pub folder: Option<String>,
    /// Whether sync is on: the agent starts with the session.
    pub on: bool,
    /// Whether the agent is running now.
    pub running: bool,
    /// What the agent last said it was doing.
    pub status: String,
    /// The agent ended because the server no longer takes this computer's
    /// token.
    pub unpaired: bool,
    /// The settings file the agent reads, when it isn't this window's: the
    /// app was started with a settings folder of its own, and the agent
    /// on this computer goes by another. Sync isn't this window's to set.
    pub agent_reads: Option<String>,
}

/// Sync as the app's settings page works it: the settings file and the
/// agent's service.
pub struct Panel {
    file: PathBuf,
    service: std::sync::Arc<dyn Service>,
}

impl Panel {
    pub fn new(file: PathBuf, service: std::sync::Arc<dyn Service>) -> Self {
        Self { file, service }
    }

    /// What to show, for a window showing `server` (as [`server_url`]
    /// writes it) that this computer is, or isn't, `paired` with.
    /// The settings file the agent reads, if it isn't the one this panel
    /// writes.
    fn others(&self) -> Option<PathBuf> {
        self.service
            .settings_file()
            .filter(|theirs| !same_file(theirs, &self.file))
    }

    /// Refuses a change to sync from a window whose settings the agent
    /// doesn't read: it would show as done here and happen to another
    /// server and folder, or not at all.
    fn own(&self) -> Result<(), Problem> {
        match self.others() {
            None => Ok(()),
            Some(theirs) => Err(Problem::new(
                "Sync isn't this window's to set",
                format!(
                    "The sync agent on this computer reads {}, and this window was started with \
                     settings of its own ({}). Set sync up in the app started the usual way.",
                    theirs.display(),
                    self.file.display()
                ),
            )),
        }
    }

    pub fn overview(&self, server: Option<&str>, paired: bool) -> Result<Overview, Problem> {
        let config = self.config()?;
        let state = self.service.state();
        Ok(Overview {
            agent_reads: self.others().map(|theirs| theirs.display().to_string()),
            installed: state.installed,
            server: server.map(str::to_owned),
            // A token file is this computer's token for the server it was
            // set up with, and for no other.
            paired: paired
                || (config.token_file.is_some()
                    && matches!((&config.server, server), (Some(kept), Some(shown)) if same_server(kept, shown))),
            // As the window's server is written, so the page can tell
            // them apart by looking; one that isn't an address stays as it
            // is, to be seen.
            syncs_with: config.server.map(|kept| server_url(&kept).unwrap_or(kept)),
            folder: config.root.map(|root| root.display().to_string()),
            on: state.enabled,
            running: state.active,
            unpaired: state.exit == Some(EXIT_UNPAIRED),
            status: state.status,
        })
    }

    /// Turns sync on, with the server the window shows, or off.
    pub fn turn(&self, on: bool, server: Option<&str>, paired: bool) -> Result<(), Problem> {
        let cant = |why: &str| Problem::new("Sync can't be turned on yet", why);
        self.own()?;
        if !on {
            return self
                .service
                .turn(false)
                .map_err(|why| Problem::new("Sync couldn't be turned off", why));
        }
        let state = self.service.state();
        if !state.installed {
            return Err(cant(
                "The sync agent isn't installed on this computer (the coversync-agent package).",
            ));
        }
        let mut config = self.config()?;
        let Some(server) = server else {
            return Err(cant(
                "Open a server first: sync is with the server the app shows.",
            ));
        };
        // A token file is the token of the server it was set up with. For
        // another server it mustn't be used (that would hand this one the
        // other's sign-in): the app's own pairing is, from the keychain.
        let moving = !config
            .server
            .as_deref()
            .is_some_and(|kept| same_server(kept, server));
        let own_file = config.token_file.is_some() && !moving;
        if !paired && !own_file {
            return Err(cant(
                "This computer isn't paired with the server. Pair it first: Add device, in the \
                 server's Sync setup.",
            ));
        }
        if config.root.is_none() {
            return Err(cant("Choose the folder for the copy first."));
        }
        if moving {
            config.server = Some(server.to_owned());
            config.token_file = None;
            self.save(&config)?;
        }
        let started = self.service.turn(true).and_then(|()| {
            // One already running read the settings before they changed.
            if state.active {
                self.service.restart()
            } else {
                Ok(())
            }
        });
        started.map_err(|why| Problem::new("Sync couldn't be turned on", why))
    }

    /// Sets the folder the copy is kept in. It has to exist. A running
    /// agent starts again to take it up; what's in the old folder stays.
    pub fn set_folder(&self, chosen: &Path) -> Result<(), Problem> {
        self.own()?;
        let root = folder(chosen).map_err(|why| Problem::new("That folder can't be used", why))?;
        let mut config = self.config()?;
        if config.root.as_deref() != Some(root.as_path()) {
            config.root = Some(root);
            self.save(&config)?;
        }
        // Only an agent that's running has anything to take up, and only
        // one that's installed can be told. The same folder chosen again
        // still tells it: the last time may have got no further than the
        // settings.
        if !self.service.state().active {
            return Ok(());
        }
        self.service
            .restart()
            .map_err(|why| Problem::new("The sync agent couldn't be started again", why))
    }

    fn config(&self) -> Result<Config, Problem> {
        Config::load(&self.file).map_err(|why| Problem::new("The sync settings can't be read", why))
    }

    fn save(&self, config: &Config) -> Result<(), Problem> {
        config
            .save(&self.file)
            .map_err(|why| Problem::new("The sync settings can't be saved", why))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    /// A service that does as it's told, and remembers what that was.
    #[derive(Default)]
    struct Fake {
        state: Mutex<ServiceState>,
        calls: Mutex<Vec<&'static str>>,
        fails: Mutex<Option<String>>,
        /// The settings file its agent reads, when it says.
        reads: Mutex<Option<PathBuf>>,
    }

    impl Fake {
        fn installed() -> Arc<Self> {
            let fake = Self::default();
            fake.state.lock().unwrap().installed = true;
            Arc::new(fake)
        }

        fn calls(&self) -> Vec<&'static str> {
            std::mem::take(&mut self.calls.lock().unwrap())
        }
    }

    impl Service for Fake {
        fn state(&self) -> ServiceState {
            self.state.lock().unwrap().clone()
        }

        fn turn(&self, on: bool) -> Result<(), String> {
            self.calls
                .lock()
                .unwrap()
                .push(if on { "on" } else { "off" });
            if let Some(why) = self.fails.lock().unwrap().clone() {
                return Err(why);
            }
            let mut state = self.state.lock().unwrap();
            state.enabled = on;
            state.active = on;
            Ok(())
        }

        fn restart(&self) -> Result<(), String> {
            self.calls.lock().unwrap().push("restart");
            match self.fails.lock().unwrap().clone() {
                Some(why) => Err(why),
                None => Ok(()),
            }
        }

        fn settings_file(&self) -> Option<PathBuf> {
            self.reads.lock().unwrap().clone()
        }
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn sync_turns_on_only_once_theres_a_server_a_pairing_and_a_folder() {
        let dir = scratch("panel-on");
        let file = dir.join("sync.json");
        let service = Fake::installed();
        let panel = Panel::new(file.clone(), service.clone());
        let server = "https://music.example/";
        let why = |result: Result<(), Problem>| result.unwrap_err().message;

        assert!(why(panel.turn(true, None, false)).contains("Open a server"));
        assert!(why(panel.turn(true, Some(server), false)).contains("isn't paired"));
        assert!(why(panel.turn(true, Some(server), true)).contains("Choose the folder"));
        // Nothing was started, and nothing written, for any of those.
        assert!(service.calls().is_empty());
        assert!(!file.exists());

        let music = dir.join("music");
        assert!(
            panel.set_folder(&music).is_err(),
            "a folder that isn't there"
        );
        fs::create_dir(&music).unwrap();
        // With no agent running there's nobody to tell.
        panel.set_folder(&music).unwrap();
        assert!(service.calls().is_empty());

        panel.turn(true, Some(server), true).unwrap();
        assert_eq!(service.calls(), ["on"]);
        let overview = panel.overview(Some(server), true).unwrap();
        assert_eq!(
            overview,
            Overview {
                installed: true,
                server: Some(server.into()),
                paired: true,
                syncs_with: Some(server.into()),
                folder: Some(music.display().to_string()),
                on: true,
                running: true,
                status: String::new(),
                unpaired: false,
                agent_reads: None,
            }
        );
        // The file holds the server and the folder, and nothing else.
        let saved: serde_json::Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        assert_eq!(
            saved,
            serde_json::json!({ "version": 1, "server": server, "root": music })
        );

        // On for another server: the running agent starts again with it.
        let other = "https://other.example/";
        panel.turn(true, Some(other), true).unwrap();
        assert_eq!(service.calls(), ["on", "restart"]);
        assert_eq!(
            panel
                .overview(Some(server), true)
                .unwrap()
                .syncs_with
                .as_deref(),
            Some(other)
        );

        panel.turn(false, None, false).unwrap();
        assert_eq!(service.calls(), ["off"]);
        let overview = panel.overview(Some(server), true).unwrap();
        assert!(!overview.on && !overview.running);
        // Off keeps the folder and the server for next time.
        assert!(overview.folder.is_some() && overview.syncs_with.is_some());
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn a_folder_is_kept_whatever_the_agent_and_a_running_agent_is_told_until_it_hears() {
        let dir = scratch("panel-folder");
        let file = dir.join("sync.json");
        let (one, two) = (dir.join("one"), dir.join("two"));
        fs::create_dir_all(&one).unwrap();
        fs::create_dir_all(&two).unwrap();
        let folder_kept = || Config::load(&file).unwrap().root;

        // The agent isn't installed: the folder is kept all the same, and
        // nothing is asked of a service that isn't there.
        let missing = Arc::new(Fake::default());
        let panel = Panel::new(file.clone(), missing.clone());
        panel.set_folder(&one).unwrap();
        assert_eq!(folder_kept(), Some(one.clone()));
        assert!(missing.calls().is_empty());

        // Running: it's started again to take the folder up.
        let service = Fake::installed();
        service.turn(true).unwrap();
        service.calls();
        let panel = Panel::new(file.clone(), service.clone());
        panel.set_folder(&two).unwrap();
        assert_eq!(service.calls(), ["restart"]);

        // The restart fails: the folder is kept, and the failure said.
        *service.fails.lock().unwrap() = Some("Unit is busy.".into());
        let failed = panel.set_folder(&one).unwrap_err();
        assert_eq!(failed.message, "Unit is busy.");
        assert_eq!(folder_kept(), Some(one.clone()));
        assert_eq!(service.calls(), ["restart"]);
        // Choosing the same folder again tries the restart again: the
        // agent is still in the old one.
        *service.fails.lock().unwrap() = None;
        panel.set_folder(&one).unwrap();
        assert_eq!(service.calls(), ["restart"]);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn a_token_file_is_one_servers_and_isnt_taken_to_another() {
        let dir = scratch("panel-token-file");
        let file = dir.join("sync.json");
        let (a, b) = ("https://a.example/", "https://b.example/");
        let set_up = || {
            Config {
                server: Some(a.into()),
                root: Some(dir.clone()),
                token_file: Some(dir.join("a-token")),
                ..Default::default()
            }
            .save(&file)
            .unwrap()
        };
        let service = Fake::installed();
        let panel = Panel::new(file.clone(), service.clone());

        // For its own server the file counts as the pairing, however the
        // settings spell that server.
        Config {
            server: Some("a.example".into()),
            root: Some(dir.clone()),
            token_file: Some(dir.join("a-token")),
            ..Default::default()
        }
        .save(&file)
        .unwrap();
        assert!(panel.overview(Some(a), false).unwrap().paired);
        set_up();
        assert!(panel.overview(Some(a), false).unwrap().paired);
        panel.turn(true, Some(a), false).unwrap();
        assert!(Config::load(&file).unwrap().token_file.is_some());

        // For another server it doesn't: the app has to be paired there.
        assert!(!panel.overview(Some(b), false).unwrap().paired);
        let refused = panel.turn(true, Some(b), false).unwrap_err();
        assert!(refused.message.contains("isn't paired"), "{refused:?}");
        let kept = Config::load(&file).unwrap();
        assert_eq!(kept.server.as_deref(), Some(a), "nothing moved");
        assert!(kept.token_file.is_some());

        // Paired with the other server: sync moves there with the app's
        // own token, and a's file is let go of, never sent to b.
        panel.turn(true, Some(b), true).unwrap();
        let moved = Config::load(&file).unwrap();
        assert_eq!(moved.server.as_deref(), Some(b));
        assert_eq!(moved.token_file, None);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn a_window_with_settings_the_agent_doesnt_read_cant_set_sync() {
        let dir = scratch("panel-other-profile");
        fs::create_dir_all(dir.join("music")).unwrap();
        let (mine, theirs) = (dir.join("mine/sync.json"), dir.join("theirs/sync.json"));
        let service = Fake::installed();
        let panel = Panel::new(mine.clone(), service.clone());

        // The agent reads this window's file, or doesn't say: all as usual.
        *service.reads.lock().unwrap() = Some(mine.clone());
        assert_eq!(panel.overview(None, false).unwrap().agent_reads, None);
        panel.set_folder(&dir.join("music")).unwrap();
        // The same file reached through a link to its folder is the same.
        std::os::unix::fs::symlink(dir.join("mine"), dir.join("alias")).unwrap();
        *service.reads.lock().unwrap() = Some(dir.join("alias/sync.json"));
        assert_eq!(panel.overview(None, false).unwrap().agent_reads, None);

        // It reads another: the page is told which, and nothing is
        // changed or started from here.
        *service.reads.lock().unwrap() = Some(theirs.clone());
        let overview = panel.overview(Some("https://a/"), true).unwrap();
        assert_eq!(overview.agent_reads, Some(theirs.display().to_string()));
        let before = fs::read(&mine).unwrap();
        for refused in [
            panel.turn(true, Some("https://a/"), true),
            panel.turn(false, None, false),
            panel.set_folder(&dir),
        ] {
            let why = refused.unwrap_err();
            assert!(
                why.message.contains(&theirs.display().to_string()),
                "{why:?}"
            );
        }
        assert_eq!(fs::read(&mine).unwrap(), before);
        assert!(service.calls().is_empty());
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn the_agents_settings_file_follows_its_own_environment() {
        let manager = "Environment=HOME=/home/a LANG=C XDG_CONFIG_HOME=/home/a/.config\n";
        let default = Path::new("/home/a/.config/io.github.brpjerry.coversync/sync.json");
        assert_eq!(
            agents_file("Environment=", manager).as_deref(),
            Some(default)
        );
        assert_eq!(
            agents_file("", "Environment=HOME=/home/a\n").as_deref(),
            Some(default)
        );
        // The unit's own environment comes first.
        let unit =
            "Environment=XDG_CONFIG_HOME=/rig/config \"DBUS_SESSION_BUS_ADDRESS=unix:path=/x\"\n";
        assert_eq!(
            agents_file(unit, manager).as_deref(),
            Some(Path::new(
                "/rig/config/io.github.brpjerry.coversync/sync.json"
            ))
        );
        // Nothing to go by: not told, rather than guessed.
        assert_eq!(agents_file("", ""), None);

        // A folder with a space, a quote, a backslash and another script
        // in its name, as systemd prints one (taken from `systemctl show`).
        let odd = r#"Environment="XDG_CONFIG_HOME=/tmp/a b/ü\"q'\\x" PLAIN=/x/y"#;
        assert_eq!(
            environment(odd),
            [r#"XDG_CONFIG_HOME=/tmp/a b/ü"q'\x"#, "PLAIN=/x/y"]
        );
        assert_eq!(
            agents_file(odd, manager).as_deref(),
            Some(Path::new(
                r#"/tmp/a b/ü"q'\x/io.github.brpjerry.coversync/sync.json"#
            ))
        );
        assert_eq!(environment("Environment=\n"), Vec::<String>::new());
        assert_eq!(environment(r#"A=1  "B=" C="x y""#), ["A=1", "B=", "C=x y"]);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn a_settings_file_reached_another_way_is_the_same_file() {
        let dir = scratch("same-file");
        let real = dir.join("real");
        fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, dir.join("link")).unwrap();
        // Through a link to its folder, before the file is even there.
        assert!(same_file(
            &real.join("sync.json"),
            &dir.join("link/sync.json")
        ));
        assert!(same_file(&real.join("sync.json"), &real.join("sync.json")));
        // Another name, or another folder.
        assert!(!same_file(
            &real.join("sync.json"),
            &real.join("other.json")
        ));
        fs::create_dir_all(dir.join("elsewhere")).unwrap();
        assert!(!same_file(
            &real.join("sync.json"),
            &dir.join("elsewhere/sync.json")
        ));
        // Folders that aren't there are compared as written.
        assert!(same_file(
            Path::new("/nowhere/a/sync.json"),
            Path::new("/nowhere/a/sync.json")
        ));
        assert!(!same_file(
            Path::new("/nowhere/a/sync.json"),
            Path::new("/nowhere/b/sync.json")
        ));
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn the_server_sync_is_with_is_shown_as_the_windows_is_written() {
        let dir = scratch("panel-spelling");
        let file = dir.join("sync.json");
        let panel = Panel::new(file.clone(), Fake::installed());
        let kept = |server: &str| {
            Config {
                server: Some(server.into()),
                ..Default::default()
            }
            .save(&file)
            .unwrap();
            panel
                .overview(Some("https://music.example/"), true)
                .unwrap()
                .syncs_with
        };
        // Spelled another way in the file: the same server to the page.
        assert_eq!(
            kept("music.example").as_deref(),
            Some("https://music.example/")
        );
        assert_eq!(
            kept("https://music.example").as_deref(),
            Some("https://music.example/")
        );
        // What isn't an address at all is shown as it is.
        assert_eq!(kept("").as_deref(), Some(""));
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn sync_without_the_agent_says_so() {
        let dir = scratch("panel-missing");
        let service = Arc::new(Fake::default());
        let panel = Panel::new(dir.join("sync.json"), service.clone());
        let overview = panel.overview(Some("https://a/"), true).unwrap();
        assert!(!overview.installed && !overview.on);
        let refused = panel.turn(true, Some("https://a/"), true).unwrap_err();
        assert!(refused.message.contains("coversync-agent"), "{refused:?}");
        assert!(service.calls().is_empty());
        // What systemd says of a failure is passed on.
        let service = Fake::installed();
        *service.fails.lock().unwrap() = Some("Unit is masked.".into());
        let panel = Panel::new(dir.join("sync.json"), service);
        fs::create_dir_all(dir.join("music")).unwrap();
        panel.set_folder(&dir.join("music")).unwrap();
        let failed = panel.turn(true, Some("https://a/"), true).unwrap_err();
        assert_eq!(failed.message, "Unit is masked.");
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn an_agent_that_gave_up_on_its_token_is_told_apart() {
        let dir = scratch("panel-unpaired");
        let service = Fake::installed();
        {
            let mut state = service.state.lock().unwrap();
            state.enabled = true;
            state.exit = Some(EXIT_UNPAIRED);
            state.status = "The server no longer accepts this computer.".into();
        }
        let panel = Panel::new(dir.join("sync.json"), service);
        let overview = panel.overview(None, false).unwrap();
        assert!(overview.unpaired && overview.on && !overview.running);
        assert_eq!(
            overview.status,
            "The server no longer accepts this computer."
        );
    }

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
    fn a_file_is_put_in_place_whole_whatever_its_name_and_whatever_was_left() {
        let dir = scratch("whole");
        // A name as long as a folder can hold: the temporary beside it
        // must not need a longer one.
        let long = dir.join("t".repeat(255));
        write_whole(&long, b"one", 0o600).unwrap();
        assert_eq!(fs::read(&long).unwrap(), b"one");
        // A temporary left by a run cut short (this process's number, used
        // again) is passed over and left alone.
        let orphan = dir.join(format!(".coversync-{}-0.tmp", std::process::id()));
        fs::write(&orphan, b"left behind").unwrap();
        write_whole(&long, b"two", 0o600).unwrap();
        assert_eq!(fs::read(&long).unwrap(), b"two");
        assert_eq!(fs::read(&orphan).unwrap(), b"left behind");
        // Nothing of this save's own stays beside the file.
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 2);
        // A file that can't be put in place leaves no temporary either.
        let blocked = dir.join("folder");
        fs::create_dir(&blocked).unwrap();
        fs::write(blocked.join("in-it"), b"").unwrap();
        assert!(write_whole(&blocked, b"x", 0o600).is_err());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 3);
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
