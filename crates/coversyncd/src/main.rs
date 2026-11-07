//! coversyncd: indexes a music library and serves the coversync web UI.

mod api;
mod auth;
mod events;
mod suggest;
mod uploads;
mod web;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use anyhow::Context;
use axum::Router;
use clap::{Parser, Subcommand};
use coversync_core::cover::CoverCache;
use coversync_core::lookups::{self, LookupCache};
use coversync_core::scan::{self, Progress};
use coversync_core::upload::Uploads;
use coversync_core::{Collection, Library, ScanProgress};
use tokio::sync::Mutex;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,
    /// Inbox folder to use until one is chosen in Inbox setup.
    #[arg(long, alias = "library")]
    inbox: Option<PathBuf>,
    /// Address to listen on.
    #[arg(long, default_value = "127.0.0.1:8770")]
    listen: SocketAddr,
    /// Where the Inbox's database and this installation's settings live
    /// (default: $XDG_STATE_HOME/coversync). The Library keeps its own
    /// database in its folder.
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,
    /// Where cover thumbnails, MusicBrainz's covers and saved AcoustID and
    /// MusicBrainz lookups are kept (default: $XDG_CACHE_HOME/coversync).
    #[arg(long)]
    cache_dir: Option<PathBuf>,
    /// Allow the tag editor to modify files. Off by default.
    #[arg(long)]
    allow_writes: bool,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Set or reset the password for the web UI and the API.
    ///
    /// Prompts for it on a terminal, or reads one line from standard input.
    /// Run it with the service's --state-dir, if it has one; it works while
    /// the service runs. Browsers signed in with an old password are signed
    /// out; apps keep their tokens.
    SetPassword,
}

pub struct AppState {
    pub lib: Library,
    pub covers: CoverCache,
    /// MusicBrainz's cover images, downloaded for suggestions.
    pub mb_art: PathBuf,
    /// AcoustID's and MusicBrainz's answers, so lookups aren't made twice.
    pub lookups: LookupCache,
    pub progress: Progress,
    pub allow_writes: bool,
    /// Serializes scans, tag writes and imports.
    pub jobs: Mutex<()>,
    /// A scan was requested while another ran; run again when it ends.
    pub rescan: AtomicBool,
    /// Smart suggestion lookups, by album.
    pub identify: suggest::Jobs,
    /// The password, sessions and app tokens.
    pub auth: auth::Auth,
    /// Uploads under way into the Inbox.
    pub uploads: Uploads,
    /// Change notices for clients listening on `/api/events`.
    pub events: events::Events,
}

impl AppState {
    /// The folders to index, from the Inbox and Library settings.
    pub fn roots(&self) -> Vec<(PathBuf, Collection)> {
        let setup = self.lib.inbox_setup().ok();
        let inbox = setup.as_ref().and_then(|s| s.folder.clone());
        let uploads = setup.and_then(|s| s.uploads);
        let library = self.lib.library_setup().ok().and_then(|s| s.folder);
        index_roots(inbox, uploads, library)
    }
}

/// The folders to index: the Inbox and Library folders that are there, and
/// an uploads folder outside the Inbox folder, indexed as part of the Inbox
/// (one inside it is anyway).
fn index_roots(
    inbox: Option<String>,
    uploads: Option<String>,
    library: Option<String>,
) -> Vec<(PathBuf, Collection)> {
    let mut roots: Vec<_> = [(inbox, Collection::Inbox), (library, Collection::Library)]
        .into_iter()
        .filter_map(|(folder, c)| {
            let path = PathBuf::from(folder?);
            match path.canonicalize() {
                Ok(p) => Some((p, c)),
                Err(e) => {
                    // A library folder is created on first import.
                    if c == Collection::Inbox {
                        tracing::warn!(path = %path.display(), "Inbox folder unavailable: {e}");
                    }
                    None
                }
            }
        })
        .collect();
    // Created by the first upload, so it may not exist yet; and the Inbox
    // and Library folders may not be there either.
    if let Some(uploads) = uploads.and_then(|u| PathBuf::from(u).canonicalize().ok())
        && !roots.iter().any(|(r, _)| uploads.starts_with(r))
    {
        roots.push((uploads, Collection::Inbox));
    }
    roots
}

pub type Shared = Arc<AppState>;

/// The API, the login routes and the UI, behind the login guard.
fn app(state: Shared) -> Router {
    api::router()
        .merge(uploads::router())
        .merge(auth::router())
        .merge(web::router())
        // Every /api route but the few auth::router opens needs a login.
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::guard,
        ))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(fallback)))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("coversync")
}

/// Run a full scan on a blocking thread. If another job is running, the
/// scan runs once that one is done.
pub fn spawn_scan(state: Shared) {
    tokio::spawn(async move {
        let Ok(_guard) = state.jobs.try_lock() else {
            state.rescan.store(true, Ordering::SeqCst);
            return;
        };
        state.rescan.store(false, Ordering::SeqCst);
        let reporting = tokio::spawn(events::report_scan(state.clone()));
        let s = state.clone();
        let result = tokio::task::spawn_blocking(move || {
            // The Library folder may have appeared (or gone) since the last scan.
            s.lib.refresh_library()?;
            scan::scan(&s.lib, &s.covers, &s.roots(), &s.progress)
        })
        .await;
        match result {
            Ok(Ok(())) => tracing::info!("scan finished"),
            Ok(Err(e)) => tracing::error!("scan failed: {e}"),
            Err(e) => tracing::error!("scan panicked: {e}"),
        }
        reporting.abort();
        let progress = {
            let mut p = state.progress.write().unwrap_or_else(|e| e.into_inner());
            p.running = false;
            p.clone()
        };
        state.events.publish(events::Notice::ScanFinished(progress));
        release_memory();
        drop(_guard);
        if state.rescan.swap(false, Ordering::SeqCst) {
            spawn_scan(state);
        }
    });
}

/// Keep glibc from holding on to a full scan's peak. Its adaptive mmap
/// threshold otherwise moves multi-megabyte cover decode buffers into
/// per-thread arenas, whose free space `malloc_trim` cannot release.
fn tune_allocator() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: mallopt only adjusts allocator parameters; called before any threads start.
    unsafe {
        libc::mallopt(libc::M_MMAP_THRESHOLD, 256 * 1024);
    }
}

/// Return freed heap pages to the OS after a scan or write.
pub fn release_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: malloc_trim only releases free heap pages; it has no preconditions.
    unsafe {
        libc::malloc_trim(0);
    }
}

fn main() -> anyhow::Result<()> {
    tune_allocator();
    tokio::runtime::Runtime::new()?.block_on(run())
}

async fn run() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,tower_http=warn,lofty=error".into()),
        )
        .init();
    let args = Args::parse();

    let state_dir = args
        .state_dir
        .unwrap_or_else(|| xdg("XDG_STATE_HOME", ".local/state"));
    if let Some(Command::SetPassword) = args.command {
        return auth::set_password(&state_dir);
    }
    let cache_dir = args
        .cache_dir
        .unwrap_or_else(|| xdg("XDG_CACHE_HOME", ".cache"));
    let lib = Library::open_in(&state_dir)?;
    // The command-line folder only seeds the setting; Inbox setup owns it after.
    if let Some(inbox) = &args.inbox {
        let mut setup = lib.inbox_setup()?;
        if setup.folder.is_none() {
            let path = inbox
                .canonicalize()
                .with_context(|| format!("inbox {}", inbox.display()))?;
            setup.folder = Some(path.display().to_string());
            lib.set_inbox_setup(setup)?;
        }
    }
    let covers = CoverCache::new(cache_dir.join("covers"))?;
    let auth = auth::Auth::open(&state_dir)?;

    let state = Arc::new(AppState {
        lib,
        covers,
        mb_art: cache_dir.join("musicbrainz"),
        lookups: LookupCache::open_or_memory(&cache_dir.join(lookups::FILE)),
        progress: Arc::new(RwLock::new(ScanProgress::default())),
        allow_writes: args.allow_writes,
        jobs: Mutex::new(()),
        rescan: AtomicBool::new(false),
        identify: suggest::Jobs::default(),
        auth,
        uploads: Uploads::default(),
        events: events::Events::default(),
    });
    tracing::info!(roots = ?state.roots(), writes = state.allow_writes, "starting");
    uploads::follow_folder(&state);
    spawn_scan(state.clone());
    tokio::spawn(uploads::housekeeping(state.clone()));

    let app = app(state.clone()).into_make_service_with_connect_info::<SocketAddr>();
    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    tracing::info!("listening on http://{}", args.listen);
    let closing = state.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("SIGTERM handler");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
            // Event streams never end on their own, and would hold up the shutdown.
            closing.events.close();
        })
        .await?;
    // Where uploads got to, so they carry on from there.
    tokio::task::spawn_blocking(move || state.uploads.save()).await?;
    Ok(())
}

/// A daemon's state in a scratch directory, for tests of the whole app.
#[cfg(test)]
pub(crate) fn test_state(dir: &std::path::Path) -> Shared {
    let state_dir = dir.join("state");
    Arc::new(AppState {
        lib: Library::open_in(&state_dir).unwrap(),
        covers: CoverCache::new(dir.join("covers")).unwrap(),
        mb_art: dir.join("musicbrainz"),
        lookups: LookupCache::open_or_memory(&dir.join(lookups::FILE)),
        progress: Arc::new(RwLock::new(ScanProgress::default())),
        allow_writes: false,
        jobs: Mutex::new(()),
        rescan: AtomicBool::new(false),
        identify: suggest::Jobs::default(),
        auth: auth::Auth::open(&state_dir).unwrap(),
        uploads: Uploads::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_uploads_folder_alone_is_indexed() {
        let dir = std::env::temp_dir().join(format!("coversyncd-roots-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Uploads")).unwrap();
        let dir = dir.canonicalize().unwrap();
        let s = |p: &str| Some(dir.join(p).display().to_string());
        // The Inbox disk isn't mounted, and nothing was imported yet.
        assert_eq!(
            index_roots(s("Inbox"), s("Uploads"), s("Library")),
            [(dir.join("Uploads"), Collection::Inbox)]
        );
        assert_eq!(
            index_roots(None, s("Uploads"), None),
            [(dir.join("Uploads"), Collection::Inbox)]
        );
        std::fs::create_dir_all(dir.join("Inbox/Uploads")).unwrap();
        std::fs::create_dir_all(dir.join("Library")).unwrap();
        assert_eq!(
            index_roots(s("Inbox"), s("Uploads"), s("Library")),
            [
                (dir.join("Inbox"), Collection::Inbox),
                (dir.join("Library"), Collection::Library),
                (dir.join("Uploads"), Collection::Inbox),
            ]
        );
        // One inside the Inbox folder is indexed with it.
        assert_eq!(index_roots(s("Inbox"), s("Inbox/Uploads"), None).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
