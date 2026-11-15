//! The sync engine: one for every client. It runs the plans the server
//! makes (`artifacts/specs/sync-api-v1.md`): it keeps no database and
//! compares nothing. A sync starts a session, lists the folder when the
//! server asks for a reconcile, then carries out the plan: deletes, mkdirs,
//! moves and rmdirs in order, then puts, several downloads at once, each
//! streamed into a partial file, flushed and renamed into place. What it did
//! goes back to the server as facts, in batches.
//!
//! Clients differ in where they keep files (a [`Storage`]) and in what
//! starts a sync: [`Watch`] listens for the server's events and checks its
//! status, and tells the client when there's something to sync.
//!
//! Speed comes first (the README's speed rule): downloads overlap each
//! other, the network never waits for the disk, flushes happen per file on
//! their own threads, and nothing already on the device is read back
//! outside a reconcile.
//!
//! ```no_run
//! # async fn run() -> Result<(), coversync_replica::Error> {
//! use std::sync::Arc;
//! use coversync_replica::{Engine, FsStorage, Options, Reason, Server, Tls, Token};
//!
//! let storage = Arc::new(FsStorage::open("/home/me/Music".as_ref()).unwrap());
//! let server = Server {
//!     url: "https://n5pro.internal/coversync".into(),
//!     token: Token::new("cst_…").unwrap(),
//!     tls: Tls::System,
//! };
//! let engine = Engine::new(&server, storage, Options::default())?;
//! let report = engine.sync(Reason::User, Arc::new(()), Default::default()).await?;
//! println!("{report:?}");
//! # Ok(()) }
//! ```

mod facts;
mod fs;
mod http;
mod reconcile;
mod run;
mod storage;
mod watch;

use std::sync::Arc;
use std::time::Duration;

pub use coversync_protocol::{
    AutoSync, Capabilities, Client, Outcome, Reason, ReconcileDone, Status, Totals,
};
pub use fs::FsStorage;
pub use http::{Server, Tls, Token};
pub use storage::{Entry, Partial, Source, Storage};
pub use tokio_util::sync::CancellationToken;
pub use watch::{StatusCheck, Trigger, Watch, WatchOptions};

/// What went wrong, when a sync couldn't go on.
#[derive(Debug, Clone, thiserror::Error)]
pub enum Error {
    /// The token was revoked, or the device removed: pair again.
    #[error("the server no longer accepts this device's token")]
    Unauthorized,
    /// The server refused a request.
    #[error("the server refused: {message}")]
    Server {
        status: u16,
        /// Its code, such as `no_library` or `preparing`.
        code: String,
        message: String,
    },
    #[error("couldn't reach the server: {0}")]
    Network(String),
    #[error("the server's reply wasn't understood: {0}")]
    Protocol(String),
    /// The sync folder as a whole: unreadable, read-only.
    #[error("the sync folder: {0}")]
    Storage(String),
    #[error("cancelled")]
    Cancelled,
}

impl Error {
    /// The server's code for a refusal.
    pub fn code(&self) -> Option<&str> {
        match self {
            Error::Server { code, .. } => Some(code),
            _ => None,
        }
    }

    /// Worth trying again later without anyone doing anything: the network,
    /// or the server busy or briefly away.
    pub fn is_transient(&self) -> bool {
        match self {
            Error::Network(_) => true,
            Error::Server { status, .. } => *status >= 500,
            _ => false,
        }
    }
}

/// How a reconcile identifies the files already in the folder.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ReconcileBy {
    /// Samples of the byte ranges the server picks: the tag areas and the
    /// ends of each file.
    #[default]
    Samples,
    /// Whole-file hashes: every byte read, which also catches a file
    /// damaged in its audio. Which is faster per device is SYN-11's to
    /// measure.
    Hashes,
}

/// How the engine works on this device.
#[derive(Debug, Clone)]
pub struct Options {
    pub client: Client,
    /// Downloads at once. Several fill a link one stream can't, and keep
    /// small files (covers) from leaving it idle.
    pub downloads: usize,
    /// Check each download's bytes against its hash as they arrive, which
    /// costs a fast device nothing. TLS already protects the transfer.
    pub verify_hashes: bool,
    pub reconcile_by: ReconcileBy,
    /// Files read at once in a reconcile.
    pub readers: usize,
    /// How much of a download is handed to the disk at a time.
    pub buffer_bytes: usize,
    /// Top-level names the engine never touches, whatever the plan says
    /// (a player's `.rockbox`). Compared ignoring case.
    pub protected: Vec<String>,
    /// What the storage can hold, if not what it says itself.
    pub capabilities: Option<Capabilities>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            client: Client {
                platform: std::env::consts::OS.into(),
                version: env!("CARGO_PKG_VERSION").into(),
            },
            downloads: 4,
            verify_hashes: true,
            reconcile_by: ReconcileBy::Samples,
            readers: 4,
            buffer_bytes: 1 << 20,
            protected: Vec::new(),
            capabilities: None,
        }
    }
}

/// Where a sync has got to, for a client to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    Starting,
    /// Listing the folder for a reconcile.
    Listing,
    /// Reading the files the server picked.
    Sampling {
        files: u64,
    },
    /// The plan is in: what it will do.
    Planned {
        totals: Totals,
    },
    /// Downloading.
    Putting,
    Ending,
}

/// How far a sync has got.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Progress {
    pub files: u64,
    pub files_total: u64,
    pub bytes: u64,
    pub bytes_total: u64,
    /// Bytes a second, lately.
    pub rate: u64,
}

/// What a client hears during a sync. Called from the engine's tasks: keep
/// it quick.
pub trait Observer: Send + Sync {
    fn phase(&self, _phase: Phase) {}
    fn progress(&self, _progress: &Progress) {}
}

/// Hear nothing.
impl Observer for () {}

/// When to sync again by itself.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Again {
    /// Not until something changes.
    #[default]
    No,
    /// The server is still preparing files: at its next `target` event, or
    /// a status check that finds something pending.
    WhenChanged,
    /// After this long (the network or the server was away).
    After(Duration),
}

/// What a sync did.
#[derive(Debug, Clone, Default)]
pub struct Report {
    pub outcome: Option<Outcome>,
    pub message: Option<String>,
    /// Files downloaded and put in place.
    pub files: u64,
    pub bytes: u64,
    pub moved: u64,
    pub deleted: u64,
    /// Files the device couldn't put, held back by the server until they
    /// change or someone chooses Retry.
    pub failed: u64,
    /// Downloads dropped because their content changed meanwhile.
    pub gone: u64,
    /// Files the server is still making or hashing.
    pub preparing: u64,
    /// Room the device would still need.
    pub short_bytes: u64,
    pub reconciled: Option<ReconcileDone>,
    /// The target the sync worked to.
    pub target: Option<String>,
    pub seconds: f64,
    pub again: Again,
}

/// The sync engine for one device's folder.
pub struct Engine {
    api: Arc<http::Api>,
    storage: Arc<dyn Storage>,
    options: Arc<Options>,
}

impl Engine {
    pub fn new(
        server: &Server,
        storage: Arc<dyn Storage>,
        options: Options,
    ) -> Result<Self, Error> {
        Ok(Self {
            api: Arc::new(http::Api::new(server, options.downloads)?),
            storage,
            options: Arc::new(options),
        })
    }

    /// Sync the folder now. Cancelling stops at once, keeping partial
    /// downloads for next time.
    pub async fn sync(
        &self,
        reason: Reason,
        observer: Arc<dyn Observer>,
        cancel: CancellationToken,
    ) -> Result<Report, Error> {
        // On a task of its own, which keeps the folder's lock until its
        // work has stopped: dropping this future cancels the sync rather
        // than abandoning it half-way.
        let cancel = cancel.child_token();
        let _cancel_on_drop = cancel.clone().drop_guard();
        let worker = tokio::spawn(run::sync(
            self.api.clone(),
            self.storage.clone(),
            self.options.clone(),
            reason,
            observer,
            cancel,
        ));
        match worker.await {
            Ok(result) => result,
            Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
            Err(e) => Err(Error::Storage(format!("the sync stopped: {e}"))),
        }
    }

    /// Whether there's anything to sync: [`StatusCheck::Unchanged`] when
    /// the status is still the one `etag` names, which costs the server one
    /// small reply.
    pub async fn status(&self, etag: Option<&str>) -> Result<StatusCheck, Error> {
        watch::status(&self.api, etag).await
    }

    /// Listen for the server's word that there's something to sync.
    pub fn watch(&self, options: WatchOptions) -> Watch {
        Watch::new(self.api.clone(), options)
    }
}

/// Run blocking work (the storage's) off the async threads.
pub(crate) async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, Error> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Error::Storage(format!("a storage task failed: {e}")))
}
