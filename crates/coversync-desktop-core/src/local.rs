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
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use coversync_proxy::{Asker, LocalFile, LocalFiles};
use serde::Deserialize;

use crate::sync::{Config, Service};

/// How long a list of what's held is used before the server is asked
/// again. A track synced since then is streamed until it shows up.
const FRESH: Duration = Duration::from_secs(300);
/// How soon the server is asked again after it couldn't be.
const RETRY: Duration = Duration::from_secs(30);
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
}

#[derive(Default)]
struct State {
    /// None: sync isn't on for the server, or nothing is known yet.
    held: Option<Arc<Held>>,
    /// The server the last answer (or lack of one) was for.
    server: Option<String>,
    /// When the server is to be asked again.
    again: Option<Instant>,
    asking: bool,
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
        *self.state.lock().expect("local lock") = State::default();
    }

    /// The folder and the list for `server`, if sync is on for it. Blocks
    /// while it asks the server.
    async fn ask(file: PathBuf, service: Arc<dyn Service>, asker: Asker) -> Result<Held, String> {
        let server = asker.server().url();
        let settings = tokio::task::spawn_blocking(move || {
            let config = Config::load(&file)?;
            let on = service.state().enabled;
            Ok::<_, String>((config, on))
        })
        .await
        .map_err(|e| e.to_string())??;
        let (config, on) = settings;
        let root = match (on, config.server.as_deref(), config.root) {
            (true, Some(with), Some(root)) if with == server => root,
            _ => return Err("sync isn't on for this server".to_owned()),
        };
        let root = std::fs::canonicalize(&root)
            .map_err(|e| format!("the sync folder {} isn't there: {e}", root.display()))?;
        let mut tracks = HashMap::new();
        let mut after: Option<String> = None;
        for _ in 0..MOST_PAGES {
            let path = match &after {
                None => "/api/sync/v1/inventory".to_owned(),
                Some(after) => {
                    let after: String =
                        url::form_urlencoded::byte_serialize(after.as_bytes()).collect();
                    format!("/api/sync/v1/inventory?after={after}")
                }
            };
            let page = asker.get(&path).await?;
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
            match next {
                // A cursor that doesn't move on would never end.
                Some(next) if after.as_ref() != Some(&next) => after = Some(next),
                _ => {
                    return Ok(Held {
                        server,
                        root,
                        tracks,
                    });
                }
            }
        }
        Err("the server's list of what this computer holds didn't end".to_owned())
    }
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
    let meta = file.metadata().ok()?;
    (meta.is_file() && meta.len() == size).then_some(file)
}

impl LocalFiles for Local {
    fn track(&self, id: i64, ask: &Asker) -> Option<LocalFile> {
        let server = ask.server().url();
        let held = {
            let mut state = self.state.lock().expect("local lock");
            // Another server's list says nothing here.
            if state.server.as_deref() != Some(server.as_str()) {
                *state = State {
                    server: Some(server.clone()),
                    ..State::default()
                };
            }
            let due = state.again.is_none_or(|at| at <= Instant::now());
            if due && !state.asking {
                // On the proxy's runtime: this thread is one of its own.
                if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                    state.asking = true;
                    let (file, service) = (self.file.clone(), self.service.clone());
                    let (asker, shared) = (ask.clone(), self.state.clone());
                    runtime.spawn(async move {
                        let asked_of = asker.server().url();
                        let answer = Local::ask(file, service, asker).await;
                        let mut state = shared.lock().expect("local lock");
                        // Forgotten, or on to another server, meanwhile.
                        if !state.asking || state.server.as_deref() != Some(asked_of.as_str()) {
                            return;
                        }
                        state.asking = false;
                        match answer {
                            Ok(held) => {
                                tracing::debug!(
                                    "this computer holds {} of {asked_of}'s tracks",
                                    held.tracks.len()
                                );
                                state.held = Some(Arc::new(held));
                                state.again = Some(Instant::now() + FRESH);
                            }
                            Err(why) => {
                                tracing::debug!("not playing from this computer's copy: {why}");
                                state.held = None;
                                state.again = Some(Instant::now() + RETRY);
                            }
                        }
                    });
                }
            }
            state.held.clone()?
        };
        if held.server != server {
            return None;
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
