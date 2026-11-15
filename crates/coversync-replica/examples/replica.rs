//! Sync a folder with the engine, for trying it and timing it. The real
//! command-line client is `coversync` (DSK-11, SYN-06).
//!
//! ```text
//! cargo run --release -p coversync-replica --example replica -- \
//!     --server https://n5pro.internal/coversync --token-file TOKEN --root DIR \
//!     [--downloads 4] [--hashes] [--no-verify] [--watch]
//! ```
//!
//! The token is read from a file, so it never shows in a process list.
//! `--watch` keeps going, syncing whenever the server says there's
//! something to sync.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use coversync_replica::{
    Again, CancellationToken, Engine, Error, FsStorage, Observer, Options, Outcome, Phase,
    Progress, Reason, ReconcileBy, Server, Tls, Token, Trigger, WatchOptions,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

struct Show {
    began: Instant,
    last: AtomicU64,
}

impl Observer for Show {
    fn phase(&self, phase: Phase) {
        eprintln!("[{:7.2}s] {phase:?}", self.began.elapsed().as_secs_f64());
    }

    fn progress(&self, p: &Progress) {
        let secs = self.began.elapsed().as_secs();
        if self.last.swap(secs, Ordering::Relaxed) == secs {
            return;
        }
        eprintln!(
            "[{:7.2}s] {} of {} files, {:.2} of {:.2} GB, {:.0} MB/s",
            self.began.elapsed().as_secs_f64(),
            p.files,
            p.files_total,
            p.bytes as f64 / 1e9,
            p.bytes_total as f64 / 1e9,
            p.rate as f64 / 1e6,
        );
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "coversync_replica=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let mut server = None;
    let mut token_file = None;
    let mut root = None;
    let mut options = Options::default();
    let mut watch = false;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--server" => server = it.next(),
            "--token-file" => token_file = it.next(),
            "--root" => root = it.next(),
            "--downloads" => {
                options.downloads = it.next().ok_or("--downloads needs a number")?.parse()?
            }
            "--hashes" => options.reconcile_by = ReconcileBy::Hashes,
            "--no-verify" => options.verify_hashes = false,
            "--watch" => watch = true,
            other => return Err(format!("unknown argument {other}").into()),
        }
    }
    let token = std::fs::read_to_string(token_file.ok_or("--token-file is required")?)?;
    let server = Server {
        url: server.ok_or("--server is required")?,
        token: Token::new(token).ok_or("the token file holds no token")?,
        tls: Tls::System,
    };
    let root = root.ok_or("--root is required")?;
    std::fs::create_dir_all(&root)?;
    let storage = Arc::new(FsStorage::open(root.as_ref())?);
    let engine = Engine::new(&server, storage, options)?;
    let cancel = CancellationToken::new();
    {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            eprintln!("cancelling");
            cancel.cancel();
        });
    }
    let show = |began| -> Arc<dyn Observer> {
        Arc::new(Show {
            began,
            last: AtomicU64::new(u64::MAX),
        })
    };
    if !watch {
        let report = engine
            .sync(Reason::User, show(Instant::now()), cancel)
            .await?;
        eprintln!("{report:#?}");
        let mb = report.bytes as f64 / 1e6;
        eprintln!(
            "{} files, {mb:.0} MB in {:.1} s: {:.0} MB/s",
            report.files,
            report.seconds,
            mb / report.seconds.max(0.001)
        );
        return Ok(());
    }
    let mut w = engine.watch(WatchOptions::default());
    // A sync the report asks to try again (the network or server away, or
    // files still being prepared), and when.
    let mut retry_at: Option<tokio::time::Instant> = None;
    loop {
        let retry = async {
            match retry_at {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
        let trigger = tokio::select! {
            t = w.next() => t?,
            _ = retry => Trigger { reason: Reason::Timer },
            _ = cancel.cancelled() => return Ok(()),
        };
        retry_at = None;
        eprintln!("syncing: {trigger:?}");
        match engine
            .sync(trigger.reason, show(Instant::now()), cancel.clone())
            .await
        {
            Ok(report) => {
                eprintln!(
                    "{:?}: {} files, {} moved, {} deleted in {:.1} s ({:?})",
                    report.outcome,
                    report.files,
                    report.moved,
                    report.deleted,
                    report.seconds,
                    report.again
                );
                // Only a target fully reached is nothing new when the
                // server tells of it again.
                if report.outcome == Some(Outcome::Complete) {
                    w.synced(report.target.as_deref());
                }
                if let Again::After(wait) = report.again {
                    retry_at = Some(tokio::time::Instant::now() + wait);
                }
            }
            Err(Error::Unauthorized) => {
                return Err("the server refused the token: pair again".into());
            }
            Err(e) => {
                eprintln!("sync failed: {e}");
                if e.is_transient() {
                    retry_at = Some(tokio::time::Instant::now() + Duration::from_secs(30));
                }
            }
        }
    }
}
