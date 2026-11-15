//! Syncing devices: the sync API (`/api/sync/v1/`, types in
//! coversync-protocol; the design is `artifacts/specs/sync-api-v1.md`).
//!
//! The server plans every sync. For each device it works out a target
//! ([`target`]): every file the device should hold, at its path, with its
//! content's hash. It keeps what the device holds, its inventory
//! ([`store`]), current from the device's reports. A plan ([`plan`]) is the
//! difference, in the order the device carries it out. Devices make
//! outgoing requests only; nothing here reaches out to them.

mod api;
mod plan;
mod ranges;
mod setup;
mod store;
mod target;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Router;
use coversync_core::art::Art;
use coversync_core::model::LibraryVersion;
use coversync_protocol::Capabilities;
use tokio::sync::{broadcast, watch};

use crate::Shared;
pub use api::Sessions;
use store::{CoverFile, Store};
use target::{CoverWanted, Target};

/// Where the device routes start; a `sync` token may use all of them.
pub const DEVICE_ROUTES: &str = "/api/sync/v1/";

/// How often the server looks for changes that move devices' targets.
const WATCH: Duration = Duration::from_secs(2);

pub fn router() -> Router<Shared> {
    api::router().merge(setup::router())
}

/// Unix seconds now.
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub struct SyncState {
    pub store: Store,
    /// Cover files made for devices, named by their hash.
    cover_dir: PathBuf,
    /// Each device's target, as last worked out.
    targets: Mutex<HashMap<i64, Cached>>,
    /// Goes up as cover files are made, so targets waiting on them are
    /// worked out again.
    covers_made: AtomicU64,
    /// Cover files being made, and those whose art couldn't be read.
    making: Mutex<HashSet<(String, u32)>>,
    failed_covers: Mutex<HashSet<(String, u32)>>,
    /// Sessions under way.
    pub sessions: Sessions,
    /// Goes up whenever devices' targets may have moved on: event streams
    /// watch it.
    pub changes: watch::Sender<u64>,
    /// Devices someone asked to sync from Sync setup.
    pub requests: broadcast::Sender<i64>,
}

/// A target, with what it was worked out from.
struct Cached {
    library: LibraryVersion,
    settings_version: i64,
    capabilities: Capabilities,
    covers_made: u64,
    target: Arc<Target>,
}

/// Why there's no target to sync to.
#[derive(Debug)]
pub enum NoTarget {
    /// The device was removed.
    NoDevice,
    /// There's no Library folder, or it's unavailable. Devices keep what
    /// they have rather than being emptied.
    NoLibrary,
    Failed(String),
}

impl<E: std::fmt::Display> From<E> for NoTarget {
    fn from(e: E) -> Self {
        NoTarget::Failed(e.to_string())
    }
}

impl SyncState {
    pub fn open(state_dir: &Path, cache_dir: &Path) -> anyhow::Result<Self> {
        let cover_dir = cache_dir.join("sync-covers");
        std::fs::create_dir_all(&cover_dir)?;
        Ok(SyncState {
            store: Store::open(&state_dir.join(coversync_core::db::LOCAL_DB))?,
            cover_dir,
            targets: Mutex::new(HashMap::new()),
            covers_made: AtomicU64::new(0),
            making: Mutex::new(HashSet::new()),
            failed_covers: Mutex::new(HashSet::new()),
            sessions: Sessions::default(),
            changes: watch::Sender::new(0),
            requests: broadcast::channel(64).0,
        })
    }

    /// Something devices' targets depend on changed.
    pub fn changed(&self) {
        self.changes.send_modify(|n| *n += 1);
    }

    /// Let a removed device's state go.
    pub fn forget(&self, device: i64) {
        self.targets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&device);
    }
}

/// A device's target as things are now, worked out again only when the
/// Library, the device's settings or capabilities, or the cover files made
/// have changed. Blocking.
pub fn target(state: &Shared, device: i64) -> Result<Arc<Target>, NoTarget> {
    let sync = state.auth.device_sync(device)?.ok_or(NoTarget::NoDevice)?;
    let capabilities = state.sync.store.device(device)?.capabilities;
    let covers_made = state.sync.covers_made.load(Ordering::SeqCst);
    let paths = state
        .device_paths
        .paths(&state.lib, device, &sync)?
        .ok_or(NoTarget::NoLibrary)?;
    {
        let cached = state.sync.targets.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = cached.get(&device)
            && c.library == paths.library
            && c.settings_version == sync.version
            && c.capabilities == capabilities
            && c.covers_made == covers_made
        {
            return Ok(c.target.clone());
        }
    }
    let art = state.lib.cover_versions()?;
    let covers = state.sync.store.covers()?;
    let (target, wanted) = target::build(target::Inputs {
        paths: &paths,
        settings: &sync.settings,
        capabilities: &capabilities,
        art: &art,
        covers: &covers,
        cover_dir: &state.sync.cover_dir,
    });
    let target = Arc::new(target);
    state
        .sync
        .store
        .saw_target(device, &target.version, now())?;
    state
        .sync
        .targets
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            device,
            Cached {
                library: paths.library.clone(),
                settings_version: sync.version,
                capabilities,
                covers_made,
                target: target.clone(),
            },
        );
    if !wanted.is_empty() {
        make_covers(state.clone(), wanted);
    }
    Ok(target)
}

/// Make cover files for devices, on every core, off the async runtime.
/// Each is made once per art version and width, however many devices or
/// requests want it.
fn make_covers(state: Shared, wanted: Vec<CoverWanted>) {
    let wanted: Vec<CoverWanted> = {
        let mut making = state.sync.making.lock().unwrap_or_else(|e| e.into_inner());
        let failed = state
            .sync
            .failed_covers
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        wanted
            .into_iter()
            .filter(|w| {
                let key = (w.version.clone(), w.width);
                !failed.contains(&key) && making.insert(key)
            })
            .collect()
    };
    if wanted.is_empty() {
        return;
    }
    tokio::task::spawn_blocking(move || {
        use rayon::prelude::*;
        let made: Vec<_> = wanted
            .par_iter()
            .map(|w| (w, make_cover(&state.sync.cover_dir, w)))
            .collect();
        for (w, result) in made {
            let key = (w.version.clone(), w.width);
            match result
                .and_then(|file| Ok(state.sync.store.add_cover(&w.version, w.width, &file)?))
            {
                Ok(()) => {}
                Err(e) => {
                    tracing::warn!(version = w.version, "cover file for devices: {e}");
                    state
                        .sync
                        .failed_covers
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(key.clone());
                }
            }
            state
                .sync
                .making
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&key);
        }
        state.sync.covers_made.fetch_add(1, Ordering::SeqCst);
        state.sync.changed();
    });
}

/// One cover file: the album's art at the width asked for, written into
/// the cache under its hash.
fn make_cover(dir: &Path, w: &CoverWanted) -> anyhow::Result<CoverFile> {
    let art = Art::new(coversync_core::cover::read_art(&w.source)?)?;
    let (bytes, ext) = art.device_file((w.width > 0).then_some(w.width))?;
    let hash = blake3::hash(&bytes).to_hex().to_string();
    let path = dir.join(format!("{hash}.{ext}"));
    if !path.exists() {
        let tmp = dir.join(format!("{hash}.{ext}.tmp"));
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, &path)?;
    }
    Ok(CoverFile {
        hash,
        size: bytes.len() as u64,
        ext: ext.to_string(),
    })
}

/// Watch for changes that move devices' targets: the Library's version
/// (which also goes up as files are hashed) and devices' settings. Each
/// device's target is worked out again then, so its `changed_at` is when
/// the change happened rather than when the device next asked, and event
/// streams hear of it.
pub fn spawn_watcher(state: Shared) {
    tokio::spawn(async move {
        let mut seen = None;
        let mut closed = state.events.closed();
        loop {
            tokio::select! {
                _ = tokio::time::sleep(WATCH) => {}
                _ = closed.wait_for(|c| *c) => return,
            }
            let s = state.clone();
            let now = tokio::task::spawn_blocking(move || {
                let library = s.lib.index_versions().ok()?.library;
                let settings: Vec<(i64, i64)> = s
                    .auth
                    .device_syncs()
                    .ok()?
                    .into_iter()
                    .map(|(id, sync)| (id, sync.version))
                    .collect();
                Some((library, settings))
            })
            .await
            .ok()
            .flatten();
            let Some(mut now) = now else { continue };
            now.1.sort_unstable();
            if seen.as_ref() == Some(&now) {
                continue;
            }
            let first = seen.is_none();
            seen = Some(now.clone());
            if first {
                continue;
            }
            let s = state.clone();
            let _ = tokio::task::spawn_blocking(move || {
                for (device, _) in &now.1 {
                    let _ = target(&s, *device);
                }
            })
            .await;
            state.sync.changed();
        }
    });
}
