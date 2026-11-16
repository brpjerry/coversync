//! Which of a server's tracks this computer holds a copy of, so the app
//! can play them from its own disk
//! ([`coversync_proxy::LocalFiles`]).
//!
//! The server knows what each device holds: its inventory
//! (`GET /api/sync/v1/inventory`) names, for every file the device has in
//! its current version, the track it is. The app asks for it as this
//! computer's device, keeps the list for a while, and looks a track up in
//! it when the UI wants its audio. A track is played from the copy only
//! when the file is where the inventory says, in the sync folder itself,
//! with the size the server has for it. Whatever isn't, is streamed from
//! the server as always.
//!
//! Only while sync is on, for the server it's set up with: with sync off,
//! the app is as it was without a copy.

use std::collections::HashMap;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use coversync_proxy::{Asker, LocalFile, LocalFiles};
use serde::Deserialize;

use crate::sync::{Config, Service, same_server};

/// How long a list of what's held is used before the server is asked
/// again. A track synced since then is streamed until it shows up.
const FRESH: Duration = Duration::from_secs(300);
/// How soon the server is asked again after it couldn't be.
const RETRY: Duration = Duration::from_secs(30);
/// How long a list goes on being used without looking whether sync is
/// still on for this server and folder: it can be turned off from a
/// terminal, where nothing tells the app.
const STILL_ON: Duration = Duration::from_secs(10);
/// More pages than any inventory has: a server that never ends its list is
/// given up on.
const MOST_PAGES: usize = 10_000;

/// A file as the inventory lists it. Lines this build doesn't know are
/// skipped.
#[derive(Deserialize)]
struct Line {
    path: Option<String>,
    size: Option<u64>,
    /// The track the file is, when the device holds its current version.
    track: Option<i64>,
    /// On the last line: where the next page starts, if there is one.
    next: Option<String>,
}

/// What's held, as last heard.
struct Held {
    /// The server it's of ([`crate::sync::server_url`]'s form).
    server: String,
    /// The sync folder, with links resolved.
    root: PathBuf,
    /// Each track's path in the folder, and its size.
    tracks: HashMap<i64, (String, u64)>,
    /// When sync was seen to be on for them: before the server was asked,
    /// which can take a while.
    seen_on: Instant,
}

/// What's known of the copy, and whether the server is being asked.
#[derive(Default)]
struct State {
    /// None: sync isn't on for the server, or nothing is known yet.
    held: Option<Arc<Held>>,
    /// The server the last answer (or lack of one) was for.
    server: Option<String>,
    /// The device it was asked as ([`Asker::device_mark`]).
    device: Option<u64>,
    /// When the server is to be asked again.
    again: Option<Instant>,
    asking: bool,
    /// Goes up whenever what's known is dropped: an answer asked for
    /// before that is about how things were, and isn't kept.
    age: u64,
    /// When sync was last seen to be on for the list's server and folder.
    seen_on: Option<Instant>,
}

impl State {
    /// Drops what's known, for `server` asked as `device` from here on
    /// (none: for whichever asks next).
    fn start_over(&mut self, server: Option<String>, device: Option<u64>) {
        *self = State {
            server,
            device,
            age: self.age + 1,
            ..State::default()
        };
    }

    /// Whether to ask `server`, as `device`, what's held now: its ticket,
    /// to hand back with the answer. None while an answer is fresh or on
    /// its way.
    fn begin(&mut self, server: &str, device: u64, now: Instant) -> Option<u64> {
        // Another server's list says nothing here, and neither does what
        // another device was told: paired again, the computer is one the
        // server may hold nothing for.
        if self.server.as_deref() != Some(server) || self.device != Some(device) {
            self.start_over(Some(server.to_owned()), Some(device));
        }
        let due = self.again.is_none_or(|at| at <= now);
        (due && !self.asking).then(|| {
            self.asking = true;
            self.age
        })
    }

    /// Takes the answer to the asking that `ticket` was given for, unless
    /// what was known has been dropped since.
    fn finish(&mut self, ticket: u64, answer: Result<Held, String>, now: Instant) {
        if ticket != self.age {
            return;
        }
        self.asking = false;
        match answer {
            Ok(held) => {
                tracing::debug!(
                    "this computer holds {} of {}'s tracks",
                    held.tracks.len(),
                    held.server
                );
                // The asking looked, when it began: not now, with the
                // server's answer in. Sync turned off since is seen at the
                // first use, if the answer took long.
                self.seen_on = Some(held.seen_on);
                self.held = Some(Arc::new(held));
                self.again = Some(now + FRESH);
            }
            Err(why) => {
                tracing::debug!("not playing from this computer's copy: {why}");
                self.held = None;
                self.again = Some(now + RETRY);
            }
        }
    }
}

/// This computer's copy of the Library, as the proxy sees it.
pub struct Local {
    /// The sync settings ([`crate::sync::file`]).
    file: PathBuf,
    service: Arc<dyn Service>,
    state: Arc<Mutex<State>>,
}

impl Local {
    pub fn new(file: PathBuf, service: Arc<dyn Service>) -> Self {
        Self {
            file,
            service,
            state: Arc::default(),
        }
    }

    /// The sync settings changed (sync turned on or off, another folder):
    /// what's known is dropped, and asked for afresh.
    pub fn forget(&self) {
        self.state
            .lock()
            .expect("local lock")
            .start_over(None, None);
    }

    /// Whether sync is on now for the server and the folder `held` is of.
    /// Blocks for as long as systemd takes to say.
    fn still_on(&self, held: &Held) -> bool {
        let Ok(config) = Config::load(&self.file) else {
            return false;
        };
        config
            .server
            .as_deref()
            .is_some_and(|kept| same_server(kept, &held.server))
            && config
                .root
                .and_then(|root| std::fs::canonicalize(root).ok())
                .is_some_and(|root| root == held.root)
            && self.service.state().enabled
    }

    /// The folder and the list for `server`, if sync is on for it. Blocks
    /// while it asks the server.
    async fn ask(file: PathBuf, service: Arc<dyn Service>, asker: Asker) -> Result<Held, String> {
        let server = asker.server().url();
        let seen_on = Instant::now();
        let settings = tokio::task::spawn_blocking(move || {
            // An agent that goes by other settings than this app's keeps
            // a copy this app knows nothing of.
            if service.settings_file().is_some_and(|theirs| theirs != file) {
                return Err("the sync agent reads other settings than this app's".to_owned());
            }
            let config = Config::load(&file)?;
            let on = service.state().enabled;
            Ok::<_, String>((config, on))
        })
        .await
        .map_err(|e| e.to_string())??;
        let (config, on) = settings;
        let root = match (on, config.server.as_deref(), config.root) {
            (true, Some(with), Some(root)) if same_server(with, &server) => root,
            _ => return Err("sync isn't on for this server".to_owned()),
        };
        let root = std::fs::canonicalize(&root)
            .map_err(|e| format!("the sync folder {} isn't there: {e}", root.display()))?;
        let mut tracks = HashMap::new();
        let mut after: Option<String> = None;
        for _ in 0..MOST_PAGES {
            let page = asker.get(&page_path(after.as_deref())).await?;
            let next = read_page(&page, &mut tracks);
            match next {
                // A cursor that doesn't move on would never end.
                Some(next) if after.as_ref() != Some(&next) => after = Some(next),
                _ => {
                    return Ok(Held {
                        server,
                        root,
                        tracks,
                        seen_on,
                    });
                }
            }
        }
        Err("the server's list of what this computer holds didn't end".to_owned())
    }
}

/// Where a page of the inventory is asked for: the first, or the one after
/// the cursor the last page gave (a path in the folder, so anything a file
/// name can hold).
fn page_path(after: Option<&str>) -> String {
    match after {
        None => "/api/sync/v1/inventory".to_owned(),
        Some(after) => {
            let after: String = url::form_urlencoded::byte_serialize(after.as_bytes()).collect();
            format!("/api/sync/v1/inventory?after={after}")
        }
    }
}

/// Takes a page's tracks into `tracks`, and returns the cursor for the
/// next page, if there is one.
fn read_page(page: &[u8], tracks: &mut HashMap<i64, (String, u64)>) -> Option<String> {
    let mut next = None;
    for line in page.split(|&b| b == b'\n').filter(|line| !line.is_empty()) {
        let Ok(line) = serde_json::from_slice::<Line>(line) else {
            continue;
        };
        if let (Some(path), Some(size), Some(track)) = (line.path, line.size, line.track) {
            tracks.insert(track, (path, size));
        }
        next = next.or(line.next);
    }
    next
}

/// The type the server sends a file of this name as: only audio the copy is
/// known to be played from. Anything else is left to the server.
fn content_type(path: &str) -> Option<&'static str> {
    let extension = Path::new(path).extension()?.to_str()?.to_ascii_lowercase();
    Some(match extension.as_str() {
        "flac" => "audio/flac",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "ogg" | "oga" | "opus" => "audio/ogg",
        "wav" => "audio/wav",
        _ => return None,
    })
}

/// Opens the copy's file at `path` (the server's, relative, with `/`), if
/// it is in `root` itself and `size` long.
fn open(root: &Path, path: &str, size: u64) -> Option<File> {
    let relative = Path::new(path);
    // Names only: nothing that leads up or away.
    if !relative
        .components()
        .all(|part| matches!(part, Component::Normal(_)))
    {
        return None;
    }
    // Through no link out of the folder: the file's real place is in it.
    let real = std::fs::canonicalize(root.join(relative)).ok()?;
    if !real.starts_with(root) {
        return None;
    }
    let file = File::open(&real).ok()?;
    // The path can have changed between the look and the open (a link put
    // where a folder was): what counts is the file that was opened, and
    // the kernel says where that one is.
    let opened = std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd())).ok()?;
    if opened != real {
        return None;
    }
    let meta = file.metadata().ok()?;
    (meta.is_file() && meta.len() == size).then_some(file)
}

impl LocalFiles for Local {
    fn track(&self, id: i64, ask: &Asker) -> Option<LocalFile> {
        let server = ask.server().url();
        // Only as a device: without a token (signed in by password, or
        // the token just refused and removed) the server has no copy of
        // this computer's to speak of, and none is played from.
        let Some(device) = ask.device_mark() else {
            self.forget();
            return None;
        };
        let held = {
            let mut state = self.state.lock().expect("local lock");
            // On the proxy's runtime: this thread is one of its own.
            if let Ok(runtime) = tokio::runtime::Handle::try_current()
                && let Some(ticket) = state.begin(&server, device, Instant::now())
            {
                let (file, service) = (self.file.clone(), self.service.clone());
                let (asker, shared) = (ask.clone(), self.state.clone());
                runtime.spawn(async move {
                    let answer = Local::ask(file, service, asker).await;
                    let mut state = shared.lock().expect("local lock");
                    state.finish(ticket, answer, Instant::now());
                });
            }
            let held = state.held.clone()?;
            let looked = state
                .seen_on
                .is_some_and(|at| at + STILL_ON > Instant::now());
            (held, looked, state.age)
        };
        let (held, looked, age) = held;
        if held.server != server {
            return None;
        }
        // Sync can be turned off, or moved, without the app hearing of it
        // (systemctl, `coversync setup`): looked at again every little
        // while, on this thread, which may wait for it.
        if !looked {
            let on = self.still_on(&held);
            let mut state = self.state.lock().expect("local lock");
            if state.age == age {
                if on {
                    state.seen_on = Some(Instant::now());
                } else {
                    tracing::debug!("sync is no longer on here: not playing from the copy");
                    state.start_over(Some(server.clone()), Some(device));
                    // Asked again in a while, not at every track.
                    state.again = Some(Instant::now() + RETRY);
                }
            }
            if !on {
                return None;
            }
        }
        let (path, size) = held.tracks.get(&id)?;
        let content_type = content_type(path)?;
        let file = open(&held.root, path, *size)?;
        Some(LocalFile {
            file,
            len: *size,
            content_type,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("coversync-local-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(dir).unwrap()
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn only_a_file_in_the_folder_itself_with_the_right_size_is_opened() {
        let dir = scratch("open");
        let root = dir.join("music");
        std::fs::create_dir_all(root.join("Artist/Album")).unwrap();
        std::fs::write(root.join("Artist/Album/01.flac"), b"twelve bytes").unwrap();
        std::fs::write(dir.join("secret.flac"), b"twelve bytes").unwrap();

        assert!(open(&root, "Artist/Album/01.flac", 12).is_some());
        // Another size: the file isn't the one the server means.
        assert!(open(&root, "Artist/Album/01.flac", 11).is_none());
        assert!(open(&root, "Artist/Album/02.flac", 12).is_none());
        assert!(open(&root, "Artist/Album", 0).is_none(), "a folder");
        // Nothing that leads out of the folder, by name or by link.
        assert!(open(&root, "../secret.flac", 12).is_none());
        assert!(open(&root, "Artist/../../secret.flac", 12).is_none());
        assert!(open(&root, dir.join("secret.flac").to_str().unwrap(), 12).is_none());
        std::os::unix::fs::symlink(dir.join("secret.flac"), root.join("link.flac")).unwrap();
        assert!(open(&root, "link.flac", 12).is_none());
        std::os::unix::fs::symlink(&dir, root.join("up")).unwrap();
        assert!(open(&root, "up/secret.flac", 12).is_none());
        // A link that stays inside is the folder's own business.
        std::os::unix::fs::symlink(root.join("Artist"), root.join("Same")).unwrap();
        assert!(open(&root, "Same/Album/01.flac", 12).is_some());
    }

    fn held(server: &str, tracks: usize) -> Held {
        Held {
            server: server.into(),
            root: "/music".into(),
            tracks: (0..tracks as i64)
                .map(|id| (id, ("a.flac".into(), 1)))
                .collect(),
            seen_on: Instant::now(),
        }
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn the_server_is_asked_once_at_a_time_and_again_when_its_answer_is_old() {
        let (a, start) = ("https://a/", Instant::now());
        let mut state = State::default();
        let ticket = state.begin(a, 1, start).expect("nothing is known: ask");
        // While that's on its way, nobody else asks.
        assert_eq!(state.begin(a, 1, start), None);
        assert!(state.held.is_none());
        state.finish(ticket, Ok(held(a, 3)), start);
        assert_eq!(state.held.as_ref().map(|h| h.tracks.len()), Some(3));
        // Fresh for a while, then asked again, with the old list still used.
        assert_eq!(
            state.begin(a, 1, start + FRESH - Duration::from_secs(1)),
            None
        );
        let ticket = state.begin(a, 1, start + FRESH).expect("old: ask again");
        assert!(state.held.is_some());
        // The server away: nothing is held, and it's asked again sooner.
        state.finish(ticket, Err("away".into()), start + FRESH);
        assert!(state.held.is_none());
        assert_eq!(
            state.begin(a, 1, start + FRESH + Duration::from_secs(1)),
            None
        );
        assert!(state.begin(a, 1, start + FRESH + RETRY).is_some());
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn an_answer_from_before_the_settings_changed_isnt_kept() {
        let (a, now) = ("https://a/", Instant::now());
        let mut state = State::default();
        let before = state.begin(a, 1, now).unwrap();
        // Sync is turned off (the settings page says so), and a track is
        // asked for at once, while the first asking is still on its way.
        state.start_over(None, None);
        let after = state.begin(a, 1, now).expect("asked afresh");
        assert_ne!(before, after);
        // The old answer (sync was on) arrives first, then the new one.
        state.finish(before, Ok(held(a, 3)), now);
        assert!(state.held.is_none(), "kept what was true before");
        assert_eq!(state.begin(a, 1, now), None, "the new asking is still on");
        state.finish(after, Err("sync isn't on for this server".into()), now);
        assert!(state.held.is_none());

        // Another server shown: the first one's list is dropped, and an
        // answer about it that arrives late is too.
        let mut state = State::default();
        let first = state.begin(a, 1, now).unwrap();
        state.finish(first, Ok(held(a, 3)), now);
        let again = state.begin(a, 1, now + FRESH).unwrap();
        let other = state
            .begin("https://b/", 1, now + FRESH)
            .expect("another server: ask it");
        assert!(state.held.is_none());
        state.finish(again, Ok(held(a, 3)), now + FRESH);
        assert!(state.held.is_none());
        state.finish(other, Ok(held("https://b/", 1)), now + FRESH);
        assert_eq!(
            state.held.as_ref().map(|h| h.server.as_str()),
            Some("https://b/")
        );
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn what_one_device_was_told_isnt_used_for_the_next() {
        let (a, now) = ("https://a/", Instant::now());
        let mut state = State::default();
        let first = state.begin(a, 1, now).unwrap();
        state.finish(first, Ok(held(a, 3)), now);
        assert!(state.held.is_some());
        assert_eq!(state.begin(a, 1, now), None, "fresh, for the same device");
        // Paired again (another token): the list goes, and the server is
        // asked as the new device at once.
        let second = state.begin(a, 2, now).expect("asked as the new device");
        assert!(state.held.is_none());
        // The old device's answer, if one was still on its way, is dropped.
        state.finish(first, Ok(held(a, 3)), now);
        assert!(state.held.is_none());
        state.finish(second, Ok(held(a, 0)), now);
        assert_eq!(state.held.as_ref().map(|h| h.tracks.len()), Some(0));
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn the_inventory_is_read_a_page_at_a_time_whatever_its_paths_hold() {
        assert_eq!(page_path(None), "/api/sync/v1/inventory");
        // The cursor is a path: spaces, other scripts and what a URL
        // means something by all go as the path's own bytes.
        assert_eq!(
            page_path(Some("Björk & Co/100% Hits? #1/01 A+B=C.flac")),
            "/api/sync/v1/inventory?after=Bj%C3%B6rk+%26+Co%2F100%25+Hits%3F+%231%2F01+A%2BB%3DC.flac"
        );
        let mut tracks = HashMap::new();
        let page = br#"{"path":"A/1.flac","hash":"h1","size":10,"track":-5,"album":"x"}
{"path":"A/cover.jpg","hash":"h2","size":3}
{"path":"A/2.flac","hash":"h3","size":20}
not json at all
{"next":"A/2.flac"}
"#;
        // Only files that are tracks, in their current version, count; a
        // line this build can't read is passed over.
        assert_eq!(read_page(page, &mut tracks).as_deref(), Some("A/2.flac"));
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[&-5], ("A/1.flac".to_owned(), 10));
        // The last page says so by naming no next.
        let last = br#"{"path":"B/1.flac","hash":"h4","size":7,"track":-6}
{"next":null}
"#;
        assert_eq!(read_page(last, &mut tracks), None);
        assert_eq!(tracks.len(), 2);
        assert_eq!(read_page(b"{}\n", &mut tracks), None);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn a_slow_answer_doesnt_count_as_a_fresh_look_at_whether_sync_is_on() {
        let (a, start) = ("https://a/", Instant::now());
        let mut state = State::default();
        let ticket = state.begin(a, 1, start).unwrap();
        // Sync was seen on when the asking began; the server took a
        // minute to answer.
        let answer = Held {
            seen_on: start,
            ..held(a, 3)
        };
        state.finish(ticket, Ok(answer), start + Duration::from_secs(60));
        // What counts is when it was looked at, so the first use looks
        // again.
        assert_eq!(state.seen_on, Some(start));
        assert!(state.seen_on.unwrap() + STILL_ON <= start + Duration::from_secs(60));
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn sync_turned_off_or_moved_behind_the_apps_back_is_seen() {
        use crate::sync::ServiceState;

        struct Switch(Mutex<bool>);
        impl Service for Switch {
            fn state(&self) -> ServiceState {
                ServiceState {
                    installed: true,
                    enabled: *self.0.lock().unwrap(),
                    ..Default::default()
                }
            }
            fn turn(&self, on: bool) -> Result<(), String> {
                *self.0.lock().unwrap() = on;
                Ok(())
            }
            fn restart(&self) -> Result<(), String> {
                Ok(())
            }
        }

        let dir = scratch("still-on");
        let (root, other) = (dir.join("music"), dir.join("other"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let file = dir.join("sync.json");
        let set = |server: &str, root: &Path| {
            Config {
                server: Some(server.into()),
                root: Some(root.to_owned()),
                ..Default::default()
            }
            .save(&file)
            .unwrap()
        };
        let switch = Arc::new(Switch(Mutex::new(true)));
        let local = Local::new(file.clone(), switch.clone());
        let list = Held {
            server: "https://a/".into(),
            root: root.clone(),
            tracks: HashMap::new(),
            seen_on: Instant::now(),
        };
        set("https://a/", &root);
        assert!(local.still_on(&list));
        // Turned off from a terminal.
        switch.turn(false).unwrap();
        assert!(!local.still_on(&list));
        switch.turn(true).unwrap();
        // Moved to another folder, or another server, with `coversync setup`.
        set("https://a/", &other);
        assert!(!local.still_on(&list));
        set("https://b/", &root);
        assert!(!local.still_on(&list));
        set("https://a/", &root);
        assert!(local.still_on(&list));
        // The settings gone altogether.
        std::fs::remove_file(&file).unwrap();
        assert!(!local.still_on(&list));
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn only_audio_is_played_from_the_copy() {
        assert_eq!(content_type("A/B/01 - One.flac"), Some("audio/flac"));
        assert_eq!(content_type("A/B/01.MP3"), Some("audio/mpeg"));
        assert_eq!(content_type("A/B/01.opus"), Some("audio/ogg"));
        assert_eq!(content_type("A/B/cover.jpg"), None);
        assert_eq!(content_type("A/B/flac"), None);
    }
}
