//! coversyncd: indexes a music library and serves the coversync web UI.

mod api;
mod suggest;
mod web;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use anyhow::Context;
use clap::Parser;
use coversync_core::cover::CoverCache;
use coversync_core::scan::{self, Progress};
use coversync_core::{Collection, Library, ScanProgress};
use tokio::sync::Mutex;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Inbox folder to use until one is chosen in Inbox setup.
    #[arg(long, alias = "library")]
    inbox: Option<PathBuf>,
    /// Address to listen on.
    #[arg(long, default_value = "127.0.0.1:8770")]
    listen: SocketAddr,
    /// Where the index database lives (default: $XDG_STATE_HOME/coversync).
    #[arg(long)]
    state_dir: Option<PathBuf>,
    /// Where cover thumbnails are cached (default: $XDG_CACHE_HOME/coversync).
    #[arg(long)]
    cache_dir: Option<PathBuf>,
    /// Allow the tag editor to modify files. Off by default.
    #[arg(long)]
    allow_writes: bool,
}

pub struct AppState {
    pub lib: Library,
    pub covers: CoverCache,
    /// MusicBrainz's cover images, downloaded for suggestions.
    pub mb_art: PathBuf,
    pub progress: Progress,
    pub allow_writes: bool,
    /// Serializes scans, tag writes and imports.
    pub jobs: Mutex<()>,
    /// A scan was requested while another ran; run again when it ends.
    pub rescan: AtomicBool,
    /// Smart suggestion lookups, by album.
    pub identify: suggest::Jobs,
}

impl AppState {
    /// The folders to index, from the Inbox and Library settings.
    pub fn roots(&self) -> Vec<(PathBuf, Collection)> {
        let inbox = self.lib.inbox_setup().ok().and_then(|s| s.folder);
        let library = self.lib.library_setup().ok().and_then(|s| s.folder);
        [(inbox, Collection::Inbox), (library, Collection::Library)]
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
            .collect()
    }
}

pub type Shared = Arc<AppState>;

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
        let s = state.clone();
        let result = tokio::task::spawn_blocking(move || {
            scan::scan(&s.lib, &s.covers, &s.roots(), &s.progress)
        })
        .await;
        match result {
            Ok(Ok(())) => tracing::info!("scan finished"),
            Ok(Err(e)) => tracing::error!("scan failed: {e}"),
            Err(e) => tracing::error!("scan panicked: {e}"),
        }
        state
            .progress
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .running = false;
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
    let cache_dir = args
        .cache_dir
        .unwrap_or_else(|| xdg("XDG_CACHE_HOME", ".cache"));
    let lib = Library::open(&state_dir.join("library.sqlite3"))?;
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

    let state = Arc::new(AppState {
        lib,
        covers,
        mb_art: cache_dir.join("musicbrainz"),
        progress: Arc::new(RwLock::new(ScanProgress::default())),
        allow_writes: args.allow_writes,
        jobs: Mutex::new(()),
        rescan: AtomicBool::new(false),
        identify: suggest::Jobs::default(),
    });
    tracing::info!(roots = ?state.roots(), writes = state.allow_writes, "starting");
    spawn_scan(state.clone());

    let app = api::router()
        .merge(web::router())
        .layer(TraceLayer::new_for_http())
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    tracing::info!("listening on http://{}", args.listen);
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("SIGTERM handler");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
        })
        .await?;
    Ok(())
}
