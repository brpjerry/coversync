//! A reconcile: the folder's listing, then samples (or full hashes) of the
//! files the server picks, from which it rebuilds what it knows the device
//! holds. The only time a sync reads files already on the device.

use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use coversync_protocol::{Listed, ReconcileDone, SampleRequest, Sampled};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::http::Api;
use crate::storage::Storage;
use crate::{Error, Observer, Options, Phase, ReconcileBy, blocking};

/// About how much listing or samples goes in one request.
const REQUEST_BYTES: usize = 4 << 20;

pub(crate) async fn reconcile(
    api: &Api,
    session: &str,
    storage: &Arc<dyn Storage>,
    options: &Arc<Options>,
    observer: &dyn Observer,
    cancel: &CancellationToken,
) -> Result<ReconcileDone, Error> {
    let (by, readers) = (options.reconcile_by, options.readers);
    observer.phase(Phase::Listing);
    let s = storage.clone();
    let o = options.clone();
    let c = cancel.clone();
    let listing = blocking(move || {
        let mut lines = Vec::new();
        let mut files = 0u64;
        // Names the device keeps are never the server's business, nor
        // entered; once cancelled, nothing more is.
        let skip = |path: &str| c.is_cancelled() || crate::run::protected(&o, path);
        s.list(&skip, &mut |l: Listed| {
            serde_json::to_writer(&mut lines, &l).expect("listings serialize");
            lines.push(b'\n');
            files += 1;
        })?;
        Ok::<_, std::io::Error>((lines, files))
    })
    .await?
    .map_err(|e| Error::Storage(format!("couldn't list the folder: {e}")))?;
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let (listing, files) = listing;
    tracing::info!(files, "listed the folder");

    let base = format!("{}/sessions/{session}", coversync_protocol::PREFIX);
    let wanted = send_lines(api, &format!("{base}/reconcile"), &listing, cancel).await?;
    let wanted: Vec<SampleRequest> = wanted
        .split(|&b| b == b'\n')
        .filter(|l| !l.trim_ascii().is_empty())
        .map(crate::http::parse)
        .collect::<Result<_, _>>()?;

    observer.phase(Phase::Sampling {
        files: wanted.len() as u64,
    });
    let read = Arc::new(AtomicU64::new(0));
    let gate = Arc::new(Semaphore::new(readers.max(1)));
    let mut tasks = JoinSet::new();
    for w in wanted {
        let (s, gate, read, c) = (storage.clone(), gate.clone(), read.clone(), cancel.clone());
        tasks.spawn(async move {
            let _permit = gate.acquire_owned().await.expect("never closed");
            blocking(move || sample(&*s, w, by, &read, &c)).await
        });
    }
    // Every reader runs to its end (quickly, once cancelled: each checks)
    // before the sync lets the folder go.
    let mut sampled = Vec::new();
    while let Some(done) = tasks.join_next().await {
        let done = done.map_err(|e| Error::Storage(e.to_string()))???;
        if let Some(line) = done {
            serde_json::to_writer(&mut sampled, &line).expect("samples serialize");
            sampled.push(b'\n');
        }
    }
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    tracing::info!(
        bytes_read = read.load(Ordering::Relaxed),
        "sampled the folder"
    );
    let done = send_lines(api, &format!("{base}/reconcile/samples"), &sampled, cancel).await?;
    crate::http::parse(&done)
}

/// One file's sample or hash. A file that can't be read (gone meanwhile,
/// unreadable) is left out: the server counts it as one of the user's own.
fn sample(
    storage: &dyn Storage,
    w: SampleRequest,
    by: ReconcileBy,
    read: &AtomicU64,
    cancel: &CancellationToken,
) -> Result<Option<Sampled>, Error> {
    if cancel.is_cancelled() {
        return Ok(None);
    }
    let mut f = match storage.open(&w.path) {
        Ok(f) => f,
        Err(e) => {
            tracing::debug!(path = w.path, "couldn't open a file to sample: {e}");
            return Ok(None);
        }
    };
    let size = match f.seek(std::io::SeekFrom::End(0)) {
        Ok(n) => n,
        Err(e) => {
            tracing::debug!(path = w.path, "couldn't size a file to sample: {e}");
            return Ok(None);
        }
    };
    let result = match by {
        ReconcileBy::Samples => {
            read.fetch_add(
                w.ranges.iter().map(|[a, b]| b.saturating_sub(*a)).sum(),
                Ordering::Relaxed,
            );
            // A large range (embedded art) on slow storage stops at a cancel.
            let mut f = Cancellable { inner: f, cancel };
            coversync_protocol::sample(&mut f, size, &w.ranges).map(|s| Sampled {
                path: w.path.clone(),
                sample: Some(s),
                hash: None,
            })
        }
        ReconcileBy::Hashes => {
            read.fetch_add(size, Ordering::Relaxed);
            f.seek(std::io::SeekFrom::Start(0)).and_then(|_| {
                let mut h = blake3::Hasher::new();
                let mut buf = vec![0; 1 << 20];
                loop {
                    if cancel.is_cancelled() {
                        return Err(std::io::Error::other("cancelled"));
                    }
                    let n = match f.read(&mut buf) {
                        Ok(n) => n,
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(e) => return Err(e),
                    };
                    if n == 0 {
                        break;
                    }
                    h.update(&buf[..n]);
                }
                Ok(Sampled {
                    path: w.path.clone(),
                    sample: None,
                    hash: Some(h.finalize().to_hex().to_string()),
                })
            })
        }
    };
    match result {
        Ok(s) => Ok(Some(s)),
        Err(e) => {
            tracing::debug!(path = w.path, "couldn't sample a file: {e}");
            Ok(None)
        }
    }
}

/// A file read that fails once the sync is cancelled (not as Interrupted,
/// which `read_exact` would retry).
struct Cancellable<'a, R> {
    inner: R,
    cancel: &'a CancellationToken,
}

impl<R: Read> Read for Cancellable<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.cancel.is_cancelled() {
            return Err(std::io::Error::other("cancelled"));
        }
        self.inner.read(buf)
    }
}

impl<R: std::io::Seek> std::io::Seek for Cancellable<'_, R> {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(pos)
    }
}

/// NDJSON sent in parts, the last with `?last=1` (sent even when there's
/// nothing): the last reply.
async fn send_lines(
    api: &Api,
    path: &str,
    lines: &[u8],
    cancel: &CancellationToken,
) -> Result<Bytes, Error> {
    let mut rest = lines;
    loop {
        let cut = if rest.len() <= REQUEST_BYTES {
            rest.len()
        } else {
            // At a line's end.
            rest[..REQUEST_BYTES]
                .iter()
                .rposition(|&b| b == b'\n')
                .map_or(rest.len(), |i| i + 1)
        };
        let (part, after) = rest.split_at(cut);
        let last = after.is_empty();
        let url = if last {
            format!("{path}?last=1")
        } else {
            path.to_string()
        };
        let reply = tokio::select! {
            r = api.post_lines(&url, Bytes::copy_from_slice(part)) => r?,
            _ = cancel.cancelled() => return Err(Error::Cancelled),
        };
        if last {
            return Ok(reply);
        }
        rest = after;
    }
}

use std::io::Seek as _;
