//! The sync agent (`coversync run`, in `coversync-cli`) against the whole
//! app, served over TCP as it is in use: what DSK-11 promises. The desktop
//! (a metered network, the battery) is told by hand.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use coversync_cli::agent::{self, Settings, Shows, Stop};
use coversync_cli::system::{Conditions, Feed, Nudge, System};
use coversync_cli::tokens::Source;
use coversync_core::sync::DeviceSettings;
use coversync_protocol::AutoSync;
use coversync_replica::{CancellationToken, Options, WatchOptions};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use super::tests::{Daemon, covers_made, tags, tree, write_track};
use crate::auth::store::Scope;

/// The app on a port of its own: its address.
async fn serve(d: &Daemon) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = d
        .router
        .clone()
        .into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

/// What the agent said it was doing.
#[derive(Default)]
struct Lines(Mutex<Vec<String>>, Mutex<Option<Hook>>);

/// Something to do the moment the agent next starts a sync.
type Hook = Box<dyn FnOnce() + Send>;

impl Shows for Lines {
    fn status(&self, text: &str) {
        self.0.lock().unwrap().push(text.to_owned());
        if text == "Syncing."
            && let Some(hook) = self.1.lock().unwrap().take()
        {
            hook();
        }
    }
}

impl Lines {
    /// Runs `hook` as the agent's next sync starts, before it has done
    /// anything.
    fn as_a_sync_starts(&self, hook: impl FnOnce() + Send + 'static) {
        *self.1.lock().unwrap() = Some(Box::new(hook));
    }

    /// How often it said something with `part` in it.
    fn said(&self, part: &str) -> usize {
        let lines = self.0.lock().unwrap();
        lines.iter().filter(|line| line.contains(part)).count()
    }
}

/// An agent at work.
struct Agent {
    root: PathBuf,
    token_file: PathBuf,
    feed: Feed,
    lines: Arc<Lines>,
    shutdown: CancellationToken,
    task: JoinHandle<Result<(), Stop>>,
}

/// Starts an agent for the folder `name`, which it doesn't make. Agents of
/// one name share what they remember, as one computer's runs do.
fn start(d: &Daemon, addr: SocketAddr, token: &str, name: &str) -> Agent {
    let root = d.dir.join(name);
    let token_file = d.dir.join(format!("{name}.token"));
    std::fs::write(&token_file, token).unwrap();
    let settings = Settings {
        server: format!("http://{addr}/"),
        root: root.clone(),
        tokens: Source::File(token_file.clone()),
        state_file: Some(d.dir.join(format!("{name}.state.json"))),
        watch: WatchOptions {
            heartbeat: 15,
            ..Default::default()
        },
        options: Options::default(),
        look_again: Duration::from_millis(100),
    };
    let (system, feed) = System::manual();
    let lines = Arc::new(Lines::default());
    let shutdown = CancellationToken::new();
    let task = tokio::spawn(agent::run(
        settings,
        system,
        lines.clone(),
        shutdown.clone(),
    ));
    Agent {
        root,
        token_file,
        feed,
        lines,
        shutdown,
        task,
    }
}

impl Agent {
    /// Whether the folder holds exactly the Library.
    fn in_step(&self, d: &Daemon) -> bool {
        tree(&self.root) == d.files()
    }

    /// How many syncs it has finished whole.
    fn syncs(&self) -> usize {
        self.lines.said("Up to date")
    }

    /// Waits for the folder to hold the Library and the sync that got it
    /// there to be over, `before` being [`Agent::syncs`] from before it. A
    /// sync still running takes a new change along, whatever the
    /// conditions: what comes next would be up to chance.
    async fn caught_up(&self, d: &Daemon, before: usize, what: &str) {
        until(&self.lines, what, || {
            self.in_step(d) && self.syncs() > before
        })
        .await;
    }

    async fn stop(self) -> Result<(), Stop> {
        self.shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(20), self.task)
            .await
            .expect("the agent stops when told")
            .unwrap()
    }
}

/// Waits for `ok`, 20 seconds at most. `said` is what to show if it never
/// comes: what the agent said it was doing.
async fn until(said: &Lines, what: &str, mut ok: impl FnMut() -> bool) {
    for _ in 0..400 {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "never happened: {what}; the agent said: {:#?}",
        said.0.lock().unwrap()
    );
}

/// A device that syncs a second after the last change.
fn quick(d: &Daemon, scope: Scope) -> (i64, String) {
    let (id, token) = d.device(scope);
    d.settings(
        id,
        DeviceSettings {
            auto: AutoSync {
                quiet_seconds: 1,
                ..Default::default()
            },
            ..Default::default()
        },
    );
    (id, token)
}

/// Changes a track in the Library, as an edit or an import does, and has
/// the server notice, as its watcher does within two seconds when it runs
/// (`spawn_watcher`, which these daemons don't start). Each `n` makes
/// another file.
fn edit(d: &Daemon, n: u32) {
    let mut edited = tags("Two", "A", "Album One", 2);
    edited.push(format!("GENRE=Edit {n}"));
    write_track(
        &d.library.join("Artist A/Album One/02 - Two.flac"),
        &edited,
        60_000 + 16 * n as usize,
    );
    d.rescan();
    d.state.sync.changed();
}

// Bugs caught: 0, False positives: 0
#[tokio::test]
async fn the_agent_keeps_the_folder_in_step_within_the_devices_conditions() {
    let d = Daemon::new("agent-follows");
    let (id, token) = quick(&d, Scope::Sync);
    covers_made(&d, &token).await;
    let addr = serve(&d).await;
    std::fs::create_dir_all(d.dir.join("laptop")).unwrap();
    let a = start(&d, addr, &token, "laptop");

    // Started on an empty folder: everything, without being asked.
    a.caught_up(&d, 0, "the first sync").await;

    // A change in the Library, after the quiet period.
    let before = a.syncs();
    edit(&d, 1);
    assert!(!a.in_step(&d));
    a.caught_up(&d, before, "the change arriving").await;

    // On a metered network, a device set to unmetered networks waits.
    a.feed
        .conditions
        .send(Conditions {
            metered: true,
            on_battery: false,
        })
        .unwrap();
    edit(&d, 2);
    until(&a.lines, "waiting for the network", || {
        a.lines.said("unmetered network") > 0
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!a.in_step(&d), "synced on a metered network");

    // Sync now, in Sync setup, is someone asking: at once, metered or not.
    let before = a.syncs();
    d.state.sync.requests.send(id).unwrap();
    a.caught_up(&d, before, "Sync now on a metered network")
        .await;

    // And what waits goes ahead when the network is unmetered again.
    edit(&d, 3);
    until(&a.lines, "waiting again", || {
        a.lines.said("unmetered network") > 1
    })
    .await;
    assert!(!a.in_step(&d));
    let before = a.syncs();
    a.feed.conditions.send(Conditions::default()).unwrap();
    a.caught_up(&d, before, "the network being unmetered again")
        .await;

    // The battery counts only for a device set to sync while charging.
    a.feed
        .conditions
        .send(Conditions {
            metered: false,
            on_battery: true,
        })
        .unwrap();
    let before = a.syncs();
    edit(&d, 4);
    a.caught_up(&d, before, "syncing on battery").await;
    d.settings(
        id,
        DeviceSettings {
            auto: AutoSync {
                quiet_seconds: 1,
                charging_only: true,
                ..Default::default()
            },
            ..Default::default()
        },
    );
    edit(&d, 5);
    until(&a.lines, "waiting for power", || {
        a.lines.said("plugged in") > 0
    })
    .await;
    assert!(!a.in_step(&d));
    let before = a.syncs();
    a.feed.conditions.send(Conditions::default()).unwrap();
    a.caught_up(&d, before, "being plugged in").await;

    // Waking from sleep: the server is asked at once, with nothing new
    // heard from it.
    let before = a.syncs();
    a.feed.nudges.send(Nudge::Woke).unwrap();
    a.caught_up(&d, before, "checking on waking").await;

    assert_eq!(a.stop().await, Ok(()));
}

// Bugs caught: 0, False positives: 0
#[tokio::test]
async fn a_sync_under_way_stops_when_its_conditions_stop_holding() {
    let d = Daemon::new("agent-interrupted");
    let (id, token) = quick(&d, Scope::Sync);
    covers_made(&d, &token).await;
    let addr = serve(&d).await;
    // No folder yet, so nothing starts before the hook is set.
    let a = start(&d, addr, &token, "laptop");
    until(&a.lines, "waiting for the folder", || {
        a.lines.said("Waiting for the folder") > 0
    })
    .await;
    // The network turns metered just as the first sync starts.
    let conditions = a.feed.conditions.clone();
    a.lines.as_a_sync_starts(move || {
        conditions
            .send(Conditions {
                metered: true,
                on_battery: false,
            })
            .unwrap();
    });
    std::fs::create_dir_all(&a.root).unwrap();
    until(&a.lines, "stopping for the metered network", || {
        a.lines.said("unmetered network") > 0
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!a.in_step(&d), "went on syncing on a metered network");
    assert_eq!(a.syncs(), 0);

    // Unmetered again: the sync that was stopped is the one that runs.
    a.feed.conditions.send(Conditions::default()).unwrap();
    a.caught_up(&d, 0, "going on when unmetered").await;

    // Someone asking isn't stopped by the same change.
    edit(&d, 1);
    a.caught_up(&d, 1, "the change arriving").await;
    let conditions = a.feed.conditions.clone();
    a.lines.as_a_sync_starts(move || {
        conditions
            .send(Conditions {
                metered: true,
                on_battery: false,
            })
            .unwrap();
    });
    let said = a.lines.said("unmetered network");
    edit(&d, 2);
    d.state.sync.requests.send(id).unwrap();
    a.caught_up(&d, 2, "Sync now through the change").await;
    assert_eq!(a.lines.said("unmetered network"), said);
    assert_eq!(a.stop().await, Ok(()));
}

// Bugs caught: 1, False positives: 0
// (A network change heard while a sync ran was forgotten, so the server
// wasn't asked again once the sync was over.)
#[tokio::test]
async fn the_agent_ends_when_its_token_is_refused_unless_theres_a_newer_one() {
    let d = Daemon::new("agent-refused");
    let (first, token) = quick(&d, Scope::Manage);
    covers_made(&d, &token).await;
    let addr = serve(&d).await;
    std::fs::create_dir_all(d.dir.join("laptop")).unwrap();
    let a = start(&d, addr, &token, "laptop");
    until(&a.lines, "the first sync", || a.in_step(&d)).await;

    // The computer is paired again (the app did it): the old token is
    // refused, the new one is found, and the agent carries on.
    let (second, newer) = quick(&d, Scope::Manage);
    std::fs::write(&a.token_file, &newer).unwrap();
    let store = d.state.auth.store();
    assert!(store.revoke_device(first, crate::sync::now()).unwrap());
    edit(&d, 1);
    until(&a.lines, "carrying on as the new device", || a.in_step(&d)).await;
    assert!(!a.task.is_finished());

    // Revoked with nothing newer to find: it ends, saying why. The server
    // tells a revoked device nothing more, and closes its stream within
    // half a minute; the agent asks sooner when the network changes.
    assert!(store.revoke_device(second, crate::sync::now()).unwrap());
    a.feed.nudges.send(Nudge::NetworkBack).unwrap();
    until(&a.lines, "the agent ending", || a.task.is_finished()).await;
    assert_eq!(a.task.await.unwrap(), Err(Stop::Refused));
}

// Bugs caught: 0, False positives: 0
#[tokio::test]
async fn a_folder_that_isnt_there_is_waited_for_and_never_made() {
    let d = Daemon::new("agent-missing");
    let (_, token) = quick(&d, Scope::Sync);
    covers_made(&d, &token).await;
    let addr = serve(&d).await;
    let a = start(&d, addr, &token, "disk");
    until(&a.lines, "waiting for the folder", || {
        a.lines.said("Waiting for the folder") > 0
    })
    .await;
    assert!(!a.root.exists(), "the agent made the folder");

    // The disk is mounted.
    std::fs::create_dir_all(&a.root).unwrap();
    until(&a.lines, "the first sync", || a.in_step(&d)).await;

    // The disk goes, leaving its empty mount point: the folder there now
    // is another one, without coversync's marker. Nothing is put in it.
    let away = d.dir.join("disk.away");
    std::fs::rename(&a.root, &away).unwrap();
    std::fs::create_dir_all(&a.root).unwrap();
    edit(&d, 1);
    until(&a.lines, "noticing the empty folder", || {
        a.lines.said("lost its coversync files") > 0
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(tree(&a.root).is_empty(), "filled the empty mount point");

    // The disk is back: the change that waited arrives on it.
    std::fs::remove_dir_all(&a.root).unwrap();
    std::fs::rename(&away, &a.root).unwrap();
    until(&a.lines, "the disk being back", || a.in_step(&d)).await;
    assert_eq!(a.stop().await, Ok(()));
}

// Bugs caught: 1, False positives: 0
// (An agent stopped before its first sync had ended whole didn't remember
// the folder, and filled it when it was found empty.)
#[tokio::test]
async fn an_emptied_folder_is_filled_again_only_by_sync_now() {
    let d = Daemon::new("agent-emptied");
    let (id, token) = quick(&d, Scope::Sync);
    covers_made(&d, &token).await;
    let addr = serve(&d).await;
    std::fs::create_dir_all(d.dir.join("laptop")).unwrap();
    let a = start(&d, addr, &token, "laptop");
    // Stopped as soon as the files are there, which may be before the
    // sync has ended: the folder is remembered either way.
    until(&a.lines, "the first sync", || a.in_step(&d)).await;
    let root = a.root.clone();
    assert_eq!(a.stop().await, Ok(()));

    // Emptied while the agent was off, then the agent starts as it does
    // with the session: waking doesn't fill it.
    std::fs::remove_dir_all(&root).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    let a = start(&d, addr, &token, "laptop");
    until(&a.lines, "noticing the empty folder", || {
        a.lines.said("lost its coversync files") > 0
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(tree(&root).is_empty());

    // Someone asking does.
    d.state.sync.requests.send(id).unwrap();
    until(&a.lines, "Sync now filling it", || a.in_step(&d)).await;
    assert_eq!(a.stop().await, Ok(()));
}

// Bugs caught: 0, False positives: 0
#[tokio::test]
async fn a_device_with_auto_sync_off_syncs_only_when_asked() {
    let d = Daemon::new("agent-off");
    let (id, token) = d.device(Scope::Sync);
    let auto = |enabled| DeviceSettings {
        auto: AutoSync {
            enabled,
            quiet_seconds: 1,
            ..Default::default()
        },
        ..Default::default()
    };
    d.settings(id, auto(false));
    covers_made(&d, &token).await;
    let addr = serve(&d).await;
    std::fs::create_dir_all(d.dir.join("laptop")).unwrap();
    let a = start(&d, addr, &token, "laptop");

    // Not on starting, and it says why.
    until(&a.lines, "saying auto sync is off", || {
        a.lines.said("Auto sync is off") > 0
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(tree(&a.root).is_empty(), "synced with auto sync off");

    // Sync now does.
    d.state.sync.requests.send(id).unwrap();
    a.caught_up(&d, 0, "Sync now with auto sync off").await;

    // A change in the Library doesn't.
    edit(&d, 1);
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert!(!a.in_step(&d), "synced a change with auto sync off");
    assert_eq!(a.syncs(), 1);

    // Turned on in Sync setup: what was left meanwhile arrives, without
    // waiting for the hourly check.
    d.settings(id, auto(true));
    a.caught_up(&d, 1, "auto sync turned on").await;
    assert_eq!(a.stop().await, Ok(()));
}
