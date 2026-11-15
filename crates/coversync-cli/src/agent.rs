//! The sync agent's loop: `coversync run`.
//!
//! It keeps the server's event stream open ([`coversync_replica::Watch`]),
//! and runs the engine when the watch says it's time: after the device's
//! quiet period, at once for Sync now, on waking, and when the hourly
//! status check finds work. What the server can't know is judged here:
//!
//! - **The device's auto-sync conditions** (Sync setup's "unmetered
//!   networks only" and "while charging"). A sync they don't allow waits
//!   until they do, and one under way stops when they stop holding. Sync
//!   now is someone asking, and goes ahead whatever they say.
//! - **Sleep and the network.** Waking starts a new watch, which checks the
//!   server at once; a network that came back gets a status check soon.
//! - **The folder.** One that isn't there (a disk not mounted) is waited
//!   for, and opened again when it's another folder than before. One that
//!   held coversync's marker before and has lost it is likelier a disk
//!   that isn't mounted than a wish for a second copy, so only Sync now
//!   fills it.
//! - **Failures.** A sync that failed is tried again, at growing
//!   intervals. A refused token ends the agent, unless the keychain holds
//!   a newer one (the app paired again).

use std::fmt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use coversync_proxy::Token;
use coversync_replica::{
    Again, AutoSync, CancellationToken, Engine, Error, FsStorage, Observer, Options, Outcome,
    Phase, Progress, Reason, Report, Server, Storage, Tls, Watch, WatchOptions,
};
use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use crate::system::{Conditions, Nudge, System};
use crate::tokens::Source;

/// How often a sync that's waiting (for the network, the power, the
/// folder) is looked at again: the server's settings can change without a
/// word that reaches here.
pub const LOOK_AGAIN: Duration = Duration::from_secs(30);
/// How long the keychain gets to answer (it may be asking the user).
const KEYCHAIN_TIMEOUT: Duration = Duration::from_secs(60);

/// What the agent runs with.
pub struct Settings {
    /// The server's address ([`crate::config::server_url`]'s form).
    pub server: String,
    /// The folder the copy is in.
    pub root: PathBuf,
    pub tokens: Source,
    /// Where the agent remembers that the folder has held its marker
    /// ([`crate::config::state_file`]); nowhere, in tests.
    pub state_file: Option<PathBuf>,
    pub watch: WatchOptions,
    pub options: Options,
    /// [`LOOK_AGAIN`], but for tests.
    pub look_again: Duration,
}

/// Why the agent gave up: nothing it can mend by itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stop {
    /// There is no token for the server here.
    NotPaired,
    /// The server no longer takes this computer's token.
    Refused,
    /// The settings can't be used.
    Setup(String),
}

impl fmt::Display for Stop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Stop::NotPaired => f.write_str(
                "This computer isn't paired with the server. Pair it in the coversync app, or \
                 with `coversync pair`.",
            ),
            Stop::Refused => f.write_str(
                "The server no longer accepts this computer: it was revoked or removed in Sync \
                 setup. Pair it again in the coversync app, or with `coversync pair`.",
            ),
            Stop::Setup(why) => f.write_str(why),
        }
    }
}

/// Where the agent says what it's doing, a line at a time.
pub trait Shows: Send + Sync {
    fn status(&self, text: &str);
}

/// Why a sync that's wanted waits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hold {
    /// Auto sync is off for this device: nothing waits, either.
    Off,
    Metered,
    Battery,
    /// The desktop hasn't said yet what the network or the power is.
    Finding,
    /// The folder was synced before and has lost coversync's marker.
    Emptied,
}

impl Hold {
    fn text(self) -> &'static str {
        match self {
            Hold::Off => "Auto sync is off for this device.",
            Hold::Metered => "Waiting for an unmetered network.",
            Hold::Battery => "Waiting for this computer to be plugged in.",
            Hold::Finding => "Finding out about the network and the power.",
            Hold::Emptied => {
                "The folder has lost its coversync files: is its disk mounted? Sync now, in \
                 Sync setup, fills it again."
            }
        }
    }
}

/// Whether a sync for `reason` may run now. `auto` is the device's
/// settings as last heard, `emptied` whether the folder lost its marker.
pub fn may_sync(
    reason: Reason,
    auto: Option<AutoSync>,
    now: Conditions,
    emptied: bool,
) -> Result<(), Hold> {
    // Someone asked: that's not auto sync.
    if reason == Reason::User {
        return Ok(());
    }
    let auto = auto.unwrap_or_default();
    // A condition the device sets holds until the desktop has said how
    // things are: not knowing isn't leave to go ahead.
    let held = |wanted: bool, state: Option<bool>, hold: Hold| match (wanted, state) {
        (false, _) | (true, Some(false)) => None,
        (true, Some(true)) => Some(hold),
        (true, None) => Some(Hold::Finding),
    };
    if !auto.enabled {
        Err(Hold::Off)
    } else if emptied {
        Err(Hold::Emptied)
    } else if let Some(hold) = held(auto.wifi_only, now.metered, Hold::Metered) {
        Err(hold)
    } else if let Some(hold) = held(auto.charging_only, now.on_battery, Hold::Battery) {
        Err(hold)
    } else {
        Ok(())
    }
}

/// The sync that's wanted, and when it may next be tried.
#[derive(Debug, Default)]
struct Pending {
    reason: Option<Reason>,
    /// Not before this: a failure's interval. None: as soon as it may.
    not_before: Option<Instant>,
}

impl Pending {
    /// A sync is wanted for `reason`, on top of any wanted already.
    fn want(&mut self, reason: Reason) {
        self.reason = Some(stronger(self.reason, reason));
    }

    /// The same sync again, `wait` from `now`.
    fn retry(&mut self, reason: Reason, wait: Duration, now: Instant) {
        self.want(reason);
        self.not_before = Some(now + wait);
    }

    /// Whatever it was waiting out may be over: as soon as it may.
    fn hurry(&mut self) {
        self.not_before = None;
    }

    fn clear(&mut self) {
        *self = Self::default();
    }

    /// The sync to try now, if there is one and its time has come. A
    /// deadline that has passed is done with: a sync that's then held for
    /// another reason is looked at again at the usual interval, not at a
    /// moment already gone.
    fn due(&mut self, now: Instant) -> Option<Reason> {
        if self.not_before.is_some_and(|at| at <= now) {
            self.not_before = None;
        }
        self.reason.filter(|_| self.not_before.is_none())
    }

    /// When to look again with nothing else heard: at the deadline, or
    /// after `interval` (a sync that's held, or settings that may have
    /// changed without a word).
    fn look_at(&self, now: Instant, interval: Duration) -> Instant {
        match (self.reason, self.not_before) {
            (Some(_), Some(at)) => at,
            _ => now + interval,
        }
    }
}

/// What a sync's report leaves to do by itself: how long to wait before
/// the same sync is tried again, if it is to be. `failures` counts the
/// tries in a row that got nowhere.
fn follow_up(report: &Report, failures: &mut u32) -> Option<Duration> {
    let moved_on = report.files + report.moved + report.deleted > 0;
    match (report.outcome, report.again) {
        (Some(Outcome::Complete), _) => {
            *failures = 0;
            None
        }
        // The engine asks for another go (the network or the server was
        // away, or the Library kept changing). Its interval is for a try
        // that got somewhere; ones that don't are spaced out.
        (_, Again::After(wait)) => {
            *failures = if moved_on { 0 } else { *failures + 1 };
            let doubled = wait.saturating_mul(1 << failures.saturating_sub(1).min(8));
            Some(doubled.min(Duration::from_secs(600).max(wait)))
        }
        // The folder failed it as a whole (read-only, an error writing):
        // nothing on the server will say when that's over.
        (Some(Outcome::Failed), _) => {
            *failures += 1;
            Some(backoff(&Error::Storage(String::new()), *failures))
        }
        // Out of room, files held back, or the server preparing files:
        // the next change, or someone, starts the next sync.
        _ => None,
    }
}

/// Of two reasons to sync, the one to give: someone asking comes first,
/// then waking (which has the folder checked), then the newer.
fn stronger(have: Option<Reason>, new: Reason) -> Reason {
    let rank = |r: Reason| match r {
        Reason::User => 2,
        Reason::Wake => 1,
        _ => 0,
    };
    match have {
        Some(old) if rank(old) > rank(new) => old,
        _ => new,
    }
}

/// How long to wait after the `failures`th failure in a row. The network
/// or the server being away mends itself, and soon; anything else (the
/// folder, a reply that made no sense) is tried now and then.
pub fn backoff(error: &Error, failures: u32) -> Duration {
    let (first, most) = if error.is_transient() {
        (30, 600)
    } else {
        (300, 3600)
    };
    let doubled = first << failures.saturating_sub(1).min(8);
    Duration::from_secs(doubled.min(most))
}

/// What a finished sync comes to, in a line.
fn summary(report: &Report) -> String {
    let did = {
        let mut parts = Vec::new();
        let mut count = |n: u64, one: &str, many: &str| {
            if n > 0 {
                parts.push(format!("{n} {}", if n == 1 { one } else { many }));
            }
        };
        count(report.files, "file downloaded", "files downloaded");
        count(report.moved, "moved", "moved");
        count(report.deleted, "removed", "removed");
        parts.join(", ")
    };
    let said = report.message.as_deref().filter(|m| !m.is_empty());
    match (report.outcome, said) {
        (Some(Outcome::Complete), _) if did.is_empty() => "Up to date.".to_owned(),
        (Some(Outcome::Complete), _) => format!("Up to date: {did}."),
        (_, Some(message)) => format!("Not everything is synced yet: {message}"),
        (_, None) if did.is_empty() => "Not everything is synced yet.".to_owned(),
        (_, None) => format!("Not everything is synced yet ({did} so far)."),
    }
}

fn wait_text(wait: Duration) -> String {
    match wait.as_secs() {
        s if s < 90 => format!("{s} seconds"),
        s => format!("{} minutes", (s + 30) / 60),
    }
}

/// The sync folder, open.
struct Folder {
    root: PathBuf,
    storage: Arc<FsStorage>,
    /// The device and inode, when it was opened, of the folder and of the
    /// engine's own folders in it, which the storage keeps open too.
    identity: [(u64, u64); 3],
}

/// The folder at `root` and the engine's own two in it, by device and
/// inode.
fn identity(root: &Path) -> std::io::Result<[(u64, u64); 3]> {
    let own = root.join(".coversync-replica");
    let of = |path: &Path| {
        let meta = std::fs::symlink_metadata(path)?;
        Ok::<_, std::io::Error>((meta.dev(), meta.ino()))
    };
    let folder = std::fs::metadata(root)?;
    Ok([
        (folder.dev(), folder.ino()),
        of(&own)?,
        of(&own.join("partial"))?,
    ])
}

impl Folder {
    /// Whether the path still leads to the folder that was opened, with
    /// the engine's own folders in it. A disk mounted or unmounted there
    /// since makes it another one; so does emptying it, which takes the
    /// folders the storage holds open away from under it.
    fn still_there(&self) -> bool {
        identity(&self.root).is_ok_and(|now| now == self.identity)
    }

    async fn has_marker(&self) -> bool {
        let storage = self.storage.clone();
        tokio::task::spawn_blocking(move || matches!(storage.read_marker(), Ok(Some(_))))
            .await
            .unwrap_or(false)
    }
}

/// What the agent remembers between runs: the folder it has seen
/// coversync's marker in, which a sync puts there as it starts.
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct State {
    version: u32,
    marked: Option<Marked>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Marked {
    server: String,
    root: PathBuf,
}

struct Memory {
    file: Option<PathBuf>,
    this: Marked,
    marked_before: bool,
    /// Whether the file says so too.
    saved: bool,
}

impl Memory {
    fn open(settings: &Settings) -> Self {
        let this = Marked {
            server: settings.server.clone(),
            root: settings.root.clone(),
        };
        let marked_before = settings
            .state_file
            .as_deref()
            .and_then(|file| std::fs::read(file).ok())
            .and_then(|bytes| serde_json::from_slice::<State>(&bytes).ok())
            .is_some_and(|state| state.marked.as_ref() == Some(&this));
        Self {
            file: settings.state_file.clone(),
            this,
            marked_before,
            saved: marked_before,
        }
    }

    /// The folder has coversync's marker in it. Written down until that
    /// works: a full disk today mustn't leave the next run without it.
    fn marked(&mut self) {
        self.marked_before = true;
        if self.saved {
            return;
        }
        let Some(file) = &self.file else { return };
        let state = State {
            version: 1,
            marked: Some(self.this.clone()),
        };
        let written = file
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| {
                std::fs::write(file, serde_json::to_vec(&state).expect("state serializes"))
            });
        match written {
            Ok(()) => self.saved = true,
            Err(e) => tracing::warn!("couldn't note the sync in {}: {e}", file.display()),
        }
    }
}

/// Progress, as a status line once a second at most.
struct Progressing {
    shows: Arc<dyn Shows>,
    began: Instant,
    last: AtomicU64,
}

impl Observer for Progressing {
    fn phase(&self, phase: Phase) {
        match phase {
            Phase::Listing | Phase::Sampling { .. } => self.shows.status("Checking the folder."),
            _ => {}
        }
    }

    fn progress(&self, p: &Progress) {
        let second = self.began.elapsed().as_secs();
        if self.last.swap(second, Ordering::Relaxed) == second || p.files_total == 0 {
            return;
        }
        self.shows.status(&format!(
            "Syncing: {} of {} files, {:.1} of {:.1} GB, {:.0} MB/s.",
            p.files,
            p.files_total,
            p.bytes as f64 / 1e9,
            p.bytes_total as f64 / 1e9,
            p.rate as f64 / 1e6,
        ));
    }
}

/// How one stretch with the folder and the token ended.
enum Ended {
    /// The agent was told to stop.
    Stopped,
    /// The folder is another one now, or gone: open it again. With the
    /// sync that was wanted, if one was.
    Reopen(Option<Reason>),
    /// The server refused the token.
    Refused,
}

/// Runs the agent until `shutdown`, or until it can't go on.
pub async fn run(
    settings: Settings,
    mut system: System,
    shows: Arc<dyn Shows>,
    shutdown: CancellationToken,
) -> Result<(), Stop> {
    let mut memory = Memory::open(&settings);
    let mut refused: Option<Token> = None;
    let mut carried: Option<Reason> = None;
    loop {
        let Some(token) = load_token(&settings, &*shows, &shutdown).await? else {
            return Ok(());
        };
        if refused.as_ref() == Some(&token) {
            return Err(Stop::Refused);
        }
        let Some(folder) = open_folder(&settings, &*shows, &shutdown).await else {
            return Ok(());
        };
        let server = Server {
            url: settings.server.clone(),
            token: coversync_replica::Token::new(token.expose()).ok_or(Stop::NotPaired)?,
            tls: Tls::System,
        };
        let engine = Engine::new(&server, folder.storage.clone(), settings.options.clone())
            .map_err(|e| Stop::Setup(format!("The agent couldn't start: {e}.")))?;
        let mut session = Session {
            settings: &settings,
            engine: &engine,
            folder: &folder,
            system: &mut system,
            shows: &shows,
            shutdown: &shutdown,
            memory: &mut memory,
            watch: engine.watch(settings.watch.clone()),
            conditions_gone: false,
            nudges_gone: false,
        };
        match session.run(carried.take()).await {
            Ended::Stopped => return Ok(()),
            Ended::Reopen(wanted) => carried = wanted,
            // Looked up again: a newer token (the app paired again) carries
            // on, the same one ends it.
            Ended::Refused => refused = Some(token),
        }
    }
}

/// The token, once the keychain gives an answer; None when told to stop
/// first.
async fn load_token(
    settings: &Settings,
    shows: &dyn Shows,
    shutdown: &CancellationToken,
) -> Result<Option<Token>, Stop> {
    let mut wait = Duration::from_secs(30);
    loop {
        let load = tokio::time::timeout(KEYCHAIN_TIMEOUT, settings.tokens.load(&settings.server));
        let why = tokio::select! {
            _ = shutdown.cancelled() => return Ok(None),
            loaded = load => match loaded {
                Ok(Ok(Some(token))) => return Ok(Some(token)),
                Ok(Ok(None)) => return Err(Stop::NotPaired),
                Ok(Err(why)) => why,
                Err(_) => "The keychain didn't answer.".to_owned(),
            },
        };
        // A keychain that's locked or not up yet (the agent starts with
        // the session) is asked again.
        tracing::warn!("{why} Asking again in {}", wait_text(wait));
        shows.status(&why);
        tokio::select! {
            _ = shutdown.cancelled() => return Ok(None),
            _ = tokio::time::sleep(wait) => {}
        }
        wait = (wait * 2).min(Duration::from_secs(600));
    }
}

/// The folder, once it's there; None when told to stop first.
async fn open_folder(
    settings: &Settings,
    shows: &dyn Shows,
    shutdown: &CancellationToken,
) -> Option<Folder> {
    let root = settings.root.as_path();
    let mut said = None;
    loop {
        let path = root.to_owned();
        let opened = tokio::task::spawn_blocking(move || {
            // Opening makes the engine's own folders, which are part of
            // what's told apart later.
            let storage = FsStorage::open(&path)?;
            Ok::<_, std::io::Error>((storage, identity(&path)?))
        })
        .await
        .unwrap_or_else(|e| Err(std::io::Error::other(e)));
        let why = match opened {
            Ok((storage, identity)) => {
                return Some(Folder {
                    root: root.to_owned(),
                    storage: Arc::new(storage),
                    identity,
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => format!(
                "Waiting for the folder {}: it isn't there. Is its disk mounted?",
                root.display()
            ),
            Err(e) => format!("The folder {} can't be used: {e}.", root.display()),
        };
        if said.as_ref() != Some(&why) {
            tracing::warn!("{why}");
            shows.status(&why);
            said = Some(why);
        }
        tokio::select! {
            _ = shutdown.cancelled() => return None,
            _ = tokio::time::sleep(settings.look_again) => {}
        }
    }
}

struct Session<'a> {
    settings: &'a Settings,
    engine: &'a Engine,
    folder: &'a Folder,
    system: &'a mut System,
    shows: &'a Arc<dyn Shows>,
    shutdown: &'a CancellationToken,
    memory: &'a mut Memory,
    watch: Watch,
    /// The desktop's channels, once closed, are never asked again.
    conditions_gone: bool,
    nudges_gone: bool,
}

/// What a sync that ran leaves to do.
enum Next {
    /// Nothing until the watch speaks.
    Rest,
    /// The same sync again, this long from now.
    Retry(Reason, Duration),
    /// The same sync, once conditions allow.
    Wait(Reason),
    End(Ended),
}

impl Session<'_> {
    async fn run(&mut self, carried: Option<Reason>) -> Ended {
        let mut pending = Pending {
            reason: carried,
            not_before: None,
        };
        let mut failures = 0u32;
        // Why it waits, as last said.
        let mut held: Option<Hold> = None;
        // Whether the status says auto sync is off.
        let mut off_shown = false;
        loop {
            // Auto sync is switched in Sync setup, and the watch hears of
            // it without giving a reason to sync: the status says which it
            // is. (Turned on again, the watch asks the server for what was
            // left meanwhile.)
            let off = self.watch.auto().is_some_and(|auto| !auto.enabled);
            if pending.reason.is_none() && off != off_shown {
                off_shown = off;
                self.shows.status(if off {
                    Hold::Off.text()
                } else {
                    "Auto sync is on again."
                });
            }
            let now = Instant::now();
            // A sync the agent starts by itself keeps to the device's
            // settings, so it waits for a new watch (after waking, or with
            // the folder opened again) to have heard them: a server that
            // can't say what they are can't be synced with either.
            let settled = self.watch.auto().is_some();
            if let Some(reason) = pending.due(now)
                && (reason == Reason::User || settled)
            {
                if !self.folder.still_there() {
                    tracing::info!("the folder isn't the one that was opened: opening it again");
                    return Ended::Reopen(pending.reason);
                }
                let marked = self.folder.has_marker().await;
                if marked {
                    self.memory.marked();
                }
                let emptied = self.memory.marked_before && !marked;
                let conditions = *self.system.conditions.borrow();
                match may_sync(reason, self.watch.auto(), conditions, emptied) {
                    Ok(()) => {
                        held = None;
                        pending.clear();
                        let (next, queued) = self.sync(reason, &mut failures).await;
                        match next {
                            Next::Rest => {}
                            Next::Retry(reason, wait) => {
                                pending.retry(reason, wait, Instant::now());
                            }
                            Next::Wait(reason) => pending.want(reason),
                            Next::End(ended) => return ended,
                        }
                        // What the watch said meanwhile. Someone asking
                        // doesn't wait out a failure's interval.
                        if let Some(queued) = queued {
                            pending.want(queued);
                            if queued == Reason::User {
                                pending.hurry();
                            }
                        }
                        continue;
                    }
                    Err(Hold::Off) => {
                        pending.clear();
                        held = None;
                        off_shown = true;
                        self.shows.status(Hold::Off.text());
                    }
                    Err(hold) => {
                        if held != Some(hold) {
                            tracing::info!("{}", hold.text());
                            self.shows.status(hold.text());
                            held = Some(hold);
                        }
                    }
                }
            }
            // Wait for word: from the watch, the desktop, or the clock.
            let look = pending.look_at(Instant::now(), self.settings.look_again);
            tokio::select! {
                _ = self.shutdown.cancelled() => return Ended::Stopped,
                trigger = self.watch.next() => match trigger {
                    Ok(trigger) => {
                        tracing::debug!(?trigger, "time to sync");
                        pending.want(trigger.reason);
                        // The server is there, and has something.
                        pending.hurry();
                    }
                    Err(Error::Unauthorized) => return Ended::Refused,
                    Err(e) => {
                        // The watch waits out everything else itself.
                        tracing::warn!("watching the server: {e}");
                        tokio::select! {
                            _ = self.shutdown.cancelled() => return Ended::Stopped,
                            _ = tokio::time::sleep(self.settings.look_again) => {}
                        }
                    }
                },
                changed = self.system.conditions.changed(), if !self.conditions_gone => {
                    self.conditions_gone = changed.is_err();
                }
                nudge = self.system.nudges.recv(), if !self.nudges_gone => match nudge {
                    Some(nudge) => {
                        self.nudged(nudge);
                        // Whatever kept a sync from working may be over.
                        pending.hurry();
                    }
                    None => self.nudges_gone = true,
                },
                _ = tokio::time::sleep_until(look) => {}
            }
        }
    }

    fn nudged(&mut self, nudge: Nudge) {
        match nudge {
            // A new watch: its first check is the device waking, and its
            // stream a new connection (the old one died in its sleep).
            Nudge::Woke => {
                tracing::info!("woke from sleep: checking the server");
                self.watch = self.engine.watch(self.settings.watch.clone());
            }
            Nudge::NetworkBack => {
                tracing::debug!("the network is back: checking the server");
                self.watch.check_soon();
            }
        }
    }

    /// Runs one sync, listening on meanwhile. Returns what's next, and the
    /// reason for another sync if the watch gave one while this one ran.
    async fn sync(&mut self, reason: Reason, failures: &mut u32) -> (Next, Option<Reason>) {
        tracing::info!(?reason, "syncing");
        self.shows.status("Syncing.");
        let cancel = CancellationToken::new();
        let observer = Arc::new(Progressing {
            shows: self.shows.clone(),
            began: Instant::now(),
            last: AtomicU64::new(u64::MAX),
        });
        let sync = self.engine.sync(reason, observer, cancel.clone());
        tokio::pin!(sync);
        let mut queued: Option<Reason> = None;
        let mut stopping = false;
        let mut refused = false;
        let mut hold: Option<Hold> = None;
        let mut woke = false;
        let mut network_back = false;
        let result = loop {
            tokio::select! {
                result = &mut sync => break result,
                _ = self.shutdown.cancelled(), if !stopping => {
                    stopping = true;
                    cancel.cancel();
                }
                trigger = self.watch.next(), if !refused => match trigger {
                    Ok(trigger) => queued = Some(stronger(queued, trigger.reason)),
                    Err(Error::Unauthorized) => {
                        refused = true;
                        cancel.cancel();
                    }
                    Err(e) => tracing::debug!("watching the server: {e}"),
                },
                changed = self.system.conditions.changed(), if !self.conditions_gone => {
                    self.conditions_gone = changed.is_err();
                    let conditions = *self.system.conditions.borrow();
                    // An auto sync goes on only while its conditions hold.
                    // (Auto sync turned off lets the one under way finish.)
                    match may_sync(reason, self.watch.auto(), conditions, false) {
                        Err(why) if why != Hold::Off && hold.is_none() => {
                            tracing::info!("stopping the sync: {}", why.text());
                            hold = Some(why);
                            cancel.cancel();
                        }
                        _ => {}
                    }
                }
                nudge = self.system.nudges.recv(), if !self.nudges_gone => match nudge {
                    // The engine deals with its own connections; the watch
                    // is seen to once the sync is over.
                    Some(Nudge::Woke) => woke = true,
                    Some(Nudge::NetworkBack) => network_back = true,
                    None => self.nudges_gone = true,
                },
            }
        };
        if woke {
            self.nudged(Nudge::Woke);
        } else if network_back {
            self.nudged(Nudge::NetworkBack);
        }
        // However the sync ended, even stopped part-way: a folder it put
        // its marker in is one to tell from an empty mount point later.
        if self.folder.has_marker().await {
            self.memory.marked();
        }
        let cancelled = match &result {
            Ok(report) => report.outcome == Some(Outcome::Cancelled),
            Err(e) => matches!(e, Error::Cancelled),
        };
        let next = if stopping {
            Next::End(Ended::Stopped)
        } else if refused || matches!(result, Err(Error::Unauthorized)) {
            Next::End(Ended::Refused)
        } else if cancelled && hold.is_some() {
            // Stopped for the conditions: it waits for them.
            Next::Wait(reason)
        } else {
            match result {
                Ok(report) => {
                    tracing::info!(
                        outcome = ?report.outcome,
                        files = report.files,
                        bytes = report.bytes,
                        moved = report.moved,
                        deleted = report.deleted,
                        failed = report.failed,
                        preparing = report.preparing,
                        seconds = report.seconds,
                        again = ?report.again,
                        "synced"
                    );
                    self.shows.status(&summary(&report));
                    // Only a target fully reached is nothing new when the
                    // server tells of it again.
                    if report.outcome == Some(Outcome::Complete) {
                        self.watch.synced(report.target.as_deref());
                    }
                    match follow_up(&report, failures) {
                        Some(wait) => Next::Retry(reason, wait),
                        // The watch says when: the server's next target.
                        None => Next::Rest,
                    }
                }
                // The network changed under the sync, which is likely
                // what failed it, and is there again: once more, now.
                Err(e) if e.is_transient() && (woke || network_back) && *failures == 0 => {
                    *failures += 1;
                    tracing::info!("the sync failed as the network changed ({e}): trying again");
                    Next::Retry(reason, Duration::ZERO)
                }
                Err(e) => {
                    *failures += 1;
                    let wait = backoff(&e, *failures);
                    tracing::warn!("the sync failed: {e}; again in {}", wait_text(wait));
                    self.shows.status(&format!(
                        "The last sync failed: {e}. Trying again in {}.",
                        wait_text(wait)
                    ));
                    Next::Retry(reason, wait)
                }
            }
        };
        (next, queued)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AUTO: AutoSync = AutoSync {
        enabled: true,
        quiet_seconds: 60,
        wifi_only: true,
        charging_only: true,
    };
    const FINE: Conditions = Conditions::FINE;

    // Bugs caught: 0, False positives: 0
    #[test]
    fn auto_sync_keeps_to_the_devices_conditions() {
        let metered = Conditions::known(true, false);
        let battery = Conditions::known(false, true);
        for reason in [Reason::Event, Reason::Timer, Reason::Wake] {
            assert_eq!(may_sync(reason, Some(AUTO), FINE, false), Ok(()));
            assert_eq!(
                may_sync(reason, Some(AUTO), metered, false),
                Err(Hold::Metered)
            );
            assert_eq!(
                may_sync(reason, Some(AUTO), battery, false),
                Err(Hold::Battery)
            );
            assert_eq!(may_sync(reason, Some(AUTO), FINE, true), Err(Hold::Emptied));
            // Conditions the device doesn't set don't count.
            let any = AutoSync {
                wifi_only: false,
                charging_only: false,
                ..AUTO
            };
            assert_eq!(may_sync(reason, Some(any), metered, false), Ok(()));
            assert_eq!(may_sync(reason, Some(any), battery, false), Ok(()));
            // Off is off, whatever else holds.
            let off = AutoSync {
                enabled: false,
                ..AUTO
            };
            assert_eq!(may_sync(reason, Some(off), FINE, false), Err(Hold::Off));
            assert_eq!(may_sync(reason, Some(off), metered, true), Err(Hold::Off));
        }
        // Settings not heard yet are the server's defaults: unmetered only.
        assert_eq!(
            may_sync(Reason::Wake, None, metered, false),
            Err(Hold::Metered)
        );
        assert_eq!(may_sync(Reason::Wake, None, battery, false), Ok(()));
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn what_the_desktop_hasnt_said_yet_isnt_taken_as_fine() {
        let any = AutoSync {
            wifi_only: false,
            charging_only: false,
            ..AUTO
        };
        let network_only = AutoSync {
            charging_only: false,
            ..AUTO
        };
        for reason in [Reason::Event, Reason::Timer, Reason::Wake] {
            assert_eq!(
                may_sync(reason, Some(AUTO), Conditions::UNKNOWN, false),
                Err(Hold::Finding)
            );
            // Only what the device sets has to be known.
            assert_eq!(
                may_sync(reason, Some(any), Conditions::UNKNOWN, false),
                Ok(())
            );
            let power_unknown = Conditions {
                metered: Some(false),
                on_battery: None,
            };
            assert_eq!(
                may_sync(reason, Some(network_only), power_unknown, false),
                Ok(())
            );
            assert_eq!(
                may_sync(reason, Some(AUTO), power_unknown, false),
                Err(Hold::Finding)
            );
            // What is known to be in the way is said as that.
            let metered = Conditions {
                metered: Some(true),
                on_battery: None,
            };
            assert_eq!(
                may_sync(reason, Some(AUTO), metered, false),
                Err(Hold::Metered)
            );
        }
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn a_retry_whose_time_has_come_and_is_then_held_waits_the_usual_interval() {
        let (start, interval) = (Instant::now(), Duration::from_secs(30));
        let mut pending = Pending::default();
        assert_eq!(pending.due(start), None);
        // Idle: looked at again now and then all the same (the settings).
        assert_eq!(pending.look_at(start, interval), start + interval);

        pending.retry(Reason::Event, Duration::from_secs(60), start);
        assert_eq!(pending.due(start + Duration::from_secs(59)), None);
        assert_eq!(
            pending.look_at(start + Duration::from_secs(59), interval),
            start + Duration::from_secs(60)
        );
        // Its time has come. Say the network is metered, so it's held:
        // asked again, it's due each time, and the next look is an
        // interval away, never at the moment that has passed.
        let later = start + Duration::from_secs(61);
        assert_eq!(pending.due(later), Some(Reason::Event));
        assert_eq!(pending.look_at(later, interval), later + interval);
        assert_eq!(pending.due(later + interval), Some(Reason::Event));
        assert_eq!(
            pending.look_at(later + interval, interval),
            later + interval * 2
        );

        // Someone asking, or the network coming back, cuts a wait short.
        pending.retry(Reason::Event, Duration::from_secs(600), later);
        assert_eq!(pending.due(later), None);
        pending.want(Reason::User);
        pending.hurry();
        assert_eq!(pending.due(later), Some(Reason::User));
        pending.clear();
        assert_eq!(pending.due(later), None);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn a_report_that_got_nowhere_is_followed_up_at_growing_intervals() {
        let report = |outcome, again, files| Report {
            outcome: Some(outcome),
            again,
            files,
            ..Default::default()
        };
        let away = report(Outcome::Partial, Again::After(Duration::from_secs(30)), 0);
        let mut failures = 0;
        let waits: Vec<u64> = (0..7)
            .map(|_| follow_up(&away, &mut failures).unwrap().as_secs())
            .collect();
        assert_eq!(waits, [30, 60, 120, 240, 480, 600, 600]);
        // One that got somewhere starts over at the engine's interval.
        let some = report(Outcome::Partial, Again::After(Duration::from_secs(30)), 4);
        assert_eq!(
            follow_up(&some, &mut failures),
            Some(Duration::from_secs(30))
        );
        assert_eq!(failures, 0);

        // The folder failing the sync as a whole is tried again too, more
        // rarely: nothing else would.
        let broken = report(Outcome::Failed, Again::No, 0);
        let waits: Vec<u64> = (0..5)
            .map(|_| follow_up(&broken, &mut failures).unwrap().as_secs())
            .collect();
        assert_eq!(waits, [300, 600, 1200, 2400, 3600]);

        // Done, or waiting for something only a change or a person ends.
        assert_eq!(
            follow_up(&report(Outcome::Complete, Again::No, 9), &mut failures),
            None
        );
        assert_eq!(failures, 0);
        let full = report(Outcome::Partial, Again::No, 0);
        assert_eq!(follow_up(&full, &mut failures), None);
        let preparing = report(Outcome::Partial, Again::WhenChanged, 0);
        assert_eq!(follow_up(&preparing, &mut failures), None);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn sync_now_goes_ahead_whatever_the_conditions() {
        let worst = Conditions::known(true, true);
        let off = AutoSync {
            enabled: false,
            ..AUTO
        };
        assert_eq!(may_sync(Reason::User, Some(AUTO), worst, true), Ok(()));
        assert_eq!(
            may_sync(Reason::User, Some(AUTO), Conditions::UNKNOWN, true),
            Ok(())
        );
        assert_eq!(may_sync(Reason::User, Some(off), worst, true), Ok(()));
        assert_eq!(may_sync(Reason::User, None, worst, true), Ok(()));
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn someone_asking_outranks_waking_which_outranks_the_rest() {
        assert_eq!(stronger(None, Reason::Timer), Reason::Timer);
        assert_eq!(stronger(Some(Reason::User), Reason::Event), Reason::User);
        assert_eq!(stronger(Some(Reason::Event), Reason::User), Reason::User);
        assert_eq!(stronger(Some(Reason::Wake), Reason::Event), Reason::Wake);
        assert_eq!(stronger(Some(Reason::Wake), Reason::User), Reason::User);
        // Of equals, the newer.
        assert_eq!(stronger(Some(Reason::Event), Reason::Timer), Reason::Timer);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn failures_are_tried_again_at_growing_intervals() {
        let away = Error::Network("refused".into());
        let secs = |e: &Error, n| backoff(e, n).as_secs();
        assert_eq!(
            [1, 2, 3, 4, 5, 6, 60].map(|n| secs(&away, n)),
            [30, 60, 120, 240, 480, 600, 600]
        );
        let folder = Error::Storage("read-only".into());
        assert_eq!(
            [1, 2, 3, 4, 5, u32::MAX].map(|n| secs(&folder, n)),
            [300, 600, 1200, 2400, 3600, 3600]
        );
        // The server busy or restarting is the network's kind of trouble.
        let busy = Error::Server {
            status: 503,
            code: "no_library".into(),
            message: String::new(),
        };
        assert_eq!(secs(&busy, 1), 30);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn a_sync_is_summed_up_in_a_line() {
        let mut report = Report {
            outcome: Some(Outcome::Complete),
            ..Default::default()
        };
        assert_eq!(summary(&report), "Up to date.");
        report.files = 1;
        report.deleted = 3;
        assert_eq!(
            summary(&report),
            "Up to date: 1 file downloaded, 3 removed."
        );
        report.outcome = Some(Outcome::Partial);
        assert_eq!(
            summary(&report),
            "Not everything is synced yet (1 file downloaded, 3 removed so far)."
        );
        report.message = Some("The device is full.".into());
        assert_eq!(
            summary(&report),
            "Not everything is synced yet: The device is full."
        );
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn a_marked_folder_is_remembered_for_its_server_only() {
        let dir = std::env::temp_dir().join(format!("coversync-cli-memory-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let settings = |server: &str, root: &str| Settings {
            server: server.into(),
            root: root.into(),
            tokens: Source::Keychain,
            state_file: Some(dir.join("state/sync-state.json")),
            watch: WatchOptions::default(),
            options: Options::default(),
            look_again: LOOK_AGAIN,
        };
        let mut memory = Memory::open(&settings("https://a/", "/music"));
        assert!(!memory.marked_before);
        memory.marked();
        assert!(memory.marked_before);
        assert!(Memory::open(&settings("https://a/", "/music")).marked_before);
        // Another folder, or another server's copy, starts anew.
        assert!(!Memory::open(&settings("https://a/", "/other")).marked_before);
        assert!(!Memory::open(&settings("https://b/", "/music")).marked_before);

        // Somewhere it can't be written for now: remembered for this run,
        // and written down once it can be.
        let blocked = dir.join("blocked");
        std::fs::write(&blocked, b"a file where the folder would go").unwrap();
        let elsewhere = || Settings {
            state_file: Some(blocked.join("sync-state.json")),
            ..settings("https://a/", "/music")
        };
        let mut memory = Memory::open(&elsewhere());
        memory.marked();
        assert!(memory.marked_before && !memory.saved);
        assert!(!Memory::open(&elsewhere()).marked_before);
        std::fs::remove_file(&blocked).unwrap();
        memory.marked();
        assert!(memory.saved);
        assert!(Memory::open(&elsewhere()).marked_before);
    }
}
