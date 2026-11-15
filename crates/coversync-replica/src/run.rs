//! One sync: a session, a reconcile if the server asks for one, then the
//! plan, page by page.

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use coversync_protocol::{
    EndReport, Fact, FactsBatch, FactsReply, Failure, Marker, Op, Outcome, PREFIX, PlanHeader,
    PlanLine, RESERVE_BYTES, Reason, SessionStart, SessionStarted, errors, valid_path,
};
use hyper::header::{self, HeaderValue};
use hyper::{Method, StatusCode};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::facts::Facts;
use crate::http::{Api, IDLE_TIMEOUT, next_chunk, read_all, refusal};
use crate::storage::{Entry, Partial, Storage};
use crate::{Again, Error, Observer, Options, Phase, Progress, Report, blocking};

/// How often facts and progress go to the server.
const FACTS_EVERY: Duration = Duration::from_secs(2);
/// Tries of one download before the network counts as lost.
const DOWNLOAD_TRIES: u32 = 4;
/// Failed facts batches in a row before the network counts as lost.
const SEND_TRIES: u32 = 4;
/// Fresh plans in one session, when the Library keeps changing under it.
const MAX_PLANS: usize = 20;

pub(crate) async fn sync(
    api: Arc<Api>,
    storage: Arc<dyn Storage>,
    options: Arc<Options>,
    reason: Reason,
    observer: Arc<dyn Observer>,
    cancel: CancellationToken,
) -> Result<Report, Error> {
    let began = Instant::now();
    let s = storage.clone();
    let _lock = blocking(move || s.lock())
        .await?
        .map_err(|e| Error::Storage(e.to_string()))?;
    let mut check = false;
    loop {
        let (mut report, refused) = session(
            &api, &storage, &options, reason, check, &observer, &cancel, began,
        )
        .await?;
        // A step the folder refused (a file that wasn't what the server
        // thought): the server's idea of the folder is wrong, so check it.
        if refused && !check && !cancel.is_cancelled() {
            tracing::info!("files changed on the device: checking the folder");
            check = true;
            continue;
        }
        report.seconds = began.elapsed().as_secs_f64();
        return Ok(report);
    }
}

/// Why a sync stopped starting new work.
#[derive(Debug, Clone)]
enum Stop {
    NoSpace,
    Network(String),
    Storage(String),
    Unauthorized,
    /// A step the folder refused.
    Refused,
}

impl Stop {
    /// Which of two reasons to keep.
    fn rank(&self) -> u8 {
        match self {
            Stop::Unauthorized => 4,
            Stop::Storage(_) => 3,
            Stop::Network(_) => 2,
            Stop::NoSpace => 1,
            Stop::Refused => 0,
        }
    }
}

#[derive(Default)]
struct Stats {
    puts: AtomicU64,
    bytes: AtomicU64,
    moved: AtomicU64,
    deleted: AtomicU64,
    failed: AtomicU64,
    gone: AtomicU64,
    puts_total: AtomicU64,
    bytes_total: AtomicU64,
    /// Bytes still to arrive for the downloads under way.
    owed: AtomicU64,
}

struct Ctx {
    api: Arc<Api>,
    storage: Arc<dyn Storage>,
    options: Arc<Options>,
    observer: Arc<dyn Observer>,
    facts: Facts,
    /// `/api/sync/v1/sessions/{s}`
    base: String,
    /// The user's: stop at once.
    cancel: CancellationToken,
    /// Stop starting new work.
    halt: CancellationToken,
    stop: Mutex<Option<Stop>>,
    /// The target moved on: finish what's running, then plan again.
    changed: AtomicBool,
    /// A plan has been fetched, so a facts reply's `changed` means something.
    planning: AtomicBool,
    stats: Stats,
    net: Arc<Semaphore>,
    disk: Arc<Semaphore>,
    sending: tokio::sync::Mutex<Sending>,
}

#[derive(Default)]
struct Sending {
    failures: u32,
    last_bytes: u64,
    last_at: Option<Instant>,
    rate: u64,
}

impl Ctx {
    fn stop(&self, why: Stop) {
        let mut stop = self.stop.lock().unwrap_or_else(|e| e.into_inner());
        if stop.as_ref().is_none_or(|s| why.rank() > s.rank()) {
            tracing::info!(?why, "sync stopping");
            *stop = Some(why);
        }
        self.halt.cancel();
    }

    /// Don't start anything new: stopped, or the target moved on.
    fn pausing(&self) -> bool {
        self.halt.is_cancelled() || self.changed.load(Ordering::Relaxed)
    }

    fn progress(&self) -> Progress {
        let s = &self.stats;
        Progress {
            files: s.puts.load(Ordering::Relaxed),
            files_total: s.puts_total.load(Ordering::Relaxed),
            bytes: s.bytes.load(Ordering::Relaxed),
            bytes_total: s.bytes_total.load(Ordering::Relaxed),
            rate: 0,
        }
    }

    /// Send the facts so far, with progress. `always`: even with nothing
    /// new (the end of a phase, to keep the session alive).
    async fn send(&self, always: bool) -> Result<(), Error> {
        let mut sending = self.sending.lock().await;
        let mut progress = self.progress();
        let now = Instant::now();
        if let Some(at) = sending.last_at {
            let secs = now.duration_since(at).as_secs_f64();
            if secs > 0.5 {
                let fresh = progress.bytes.saturating_sub(sending.last_bytes) as f64 / secs;
                // Smoothed over the last few batches.
                sending.rate = ((sending.rate as f64) * 0.5 + fresh * 0.5) as u64;
            }
        }
        progress.rate = sending.rate;
        let batch = self.facts.batch();
        if batch.done.is_empty()
            && batch.failed.is_empty()
            && !always
            && progress.bytes == sending.last_bytes
        {
            return Ok(());
        }
        sending.last_bytes = progress.bytes;
        sending.last_at = Some(now);
        self.observer.progress(&progress);
        let s = self.storage.clone();
        let free = blocking(move || s.free_bytes()).await?.ok().flatten();
        let n = batch.done.len();
        let body = FactsBatch {
            done: batch.done,
            failed: batch.failed.clone(),
            progress: Some(coversync_protocol::Progress {
                files: progress.files,
                bytes: progress.bytes,
                rate: Some(progress.rate),
            }),
            free_bytes: free,
        };
        match self
            .api
            .post_json::<_, FactsReply>(&format!("{}/facts", self.base), &body)
            .await
        {
            Ok(reply) => {
                sending.failures = 0;
                self.facts.confirmed(n);
                if reply.changed
                    && self.planning.load(Ordering::Relaxed)
                    && !self.changed.swap(true, Ordering::Relaxed)
                {
                    tracing::info!("the Library changed during the sync: planning again");
                }
                Ok(())
            }
            Err(e) => {
                self.facts.unsent(batch.failed);
                match &e {
                    Error::Unauthorized => self.stop(Stop::Unauthorized),
                    e if e.is_transient() => {
                        sending.failures += 1;
                        if sending.failures >= SEND_TRIES {
                            self.stop(Stop::Network(e.to_string()));
                        }
                    }
                    // The session gone (expired, or ended by another): the
                    // facts stay journaled for the next.
                    e => self.stop(Stop::Network(e.to_string())),
                }
                Err(e)
            }
        }
    }
}

/// One session. Returns its report, and whether a step was refused.
#[allow(clippy::too_many_arguments)]
async fn session(
    api: &Arc<Api>,
    storage: &Arc<dyn Storage>,
    options: &Arc<Options>,
    reason: Reason,
    check: bool,
    observer: &Arc<dyn Observer>,
    cancel: &CancellationToken,
    began: Instant,
) -> Result<(Report, bool), Error> {
    observer.phase(Phase::Starting);
    let s = storage.clone();
    let (marker, free, facts) = blocking(move || {
        let marker = s
            .read_marker()
            .ok()
            .flatten()
            .and_then(|b| serde_json::from_slice::<Marker>(&b).ok());
        let free = s.free_bytes().ok().flatten();
        (marker, free, Facts::load(s))
    })
    .await?;
    let capabilities = options
        .capabilities
        .clone()
        .unwrap_or_else(|| storage.capabilities());
    let started: SessionStarted = api
        .post_json(
            &format!("{PREFIX}/sessions"),
            &SessionStart {
                client: options.client.clone(),
                capabilities,
                marker,
                check,
                free_bytes: free,
                reason,
            },
        )
        .await?;
    tracing::info!(
        session = started.session,
        reconcile = started.reconcile,
        "sync session started"
    );
    let ctx = Arc::new(Ctx {
        api: api.clone(),
        storage: storage.clone(),
        options: options.clone(),
        observer: observer.clone(),
        facts,
        base: format!("{PREFIX}/sessions/{}", started.session),
        halt: cancel.child_token(),
        cancel: cancel.clone(),
        stop: Mutex::new(None),
        changed: AtomicBool::new(false),
        planning: AtomicBool::new(false),
        stats: Stats::default(),
        net: Arc::new(Semaphore::new(options.downloads.max(1))),
        disk: Arc::new(Semaphore::new(options.downloads.max(1) * 2)),
        sending: Default::default(),
    });
    let mut report = Report {
        target: Some(started.target.clone()),
        ..Default::default()
    };

    let ran = run(&ctx, &started, &mut report).await;
    let stop = ctx.stop.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let s = &ctx.stats;
    report.files = s.puts.load(Ordering::Relaxed);
    report.bytes = s.bytes.load(Ordering::Relaxed);
    report.moved = s.moved.load(Ordering::Relaxed);
    report.deleted = s.deleted.load(Ordering::Relaxed);
    report.failed = s.failed.load(Ordering::Relaxed);
    report.gone = s.gone.load(Ordering::Relaxed);

    let (outcome, message, again) = match (&ran, &stop) {
        (Err(Error::Unauthorized), _) | (_, Some(Stop::Unauthorized)) => {
            return Err(Error::Unauthorized);
        }
        // The server is still hashing or making files a listed file might
        // be: the check waits for them.
        (Err(Error::Server { code, .. }), _) if code == "preparing" => (
            Outcome::Partial,
            Some("waiting for the server to prepare files".to_string()),
            Again::After(Duration::from_secs(15)),
        ),
        (Err(e), _) if e.is_transient() => (
            Outcome::Partial,
            Some(e.to_string()),
            Again::After(Duration::from_secs(30)),
        ),
        (Err(e), _) => (Outcome::Failed, Some(e.to_string()), Again::No),
        (Ok(()), _) if cancel.is_cancelled() => (Outcome::Cancelled, None, Again::No),
        (Ok(()), Some(Stop::Network(m))) => (
            Outcome::Partial,
            Some(format!("lost the connection: {m}")),
            Again::After(Duration::from_secs(30)),
        ),
        (Ok(()), Some(Stop::Storage(m))) => (Outcome::Failed, Some(m.clone()), Again::No),
        (Ok(()), Some(Stop::NoSpace)) => (
            Outcome::Partial,
            Some("the device is out of room".into()),
            Again::No,
        ),
        (Ok(()), Some(Stop::Refused)) => (
            Outcome::Partial,
            Some("files changed on the device".into()),
            Again::No,
        ),
        (Ok(()), None) if report.short_bytes > 0 => (
            Outcome::Partial,
            Some(format!(
                "the device needs {} MB more room",
                report.short_bytes.div_ceil(1_000_000)
            )),
            Again::No,
        ),
        (Ok(()), None) if report.failed > 0 => (
            Outcome::Partial,
            Some(format!("{} files failed", report.failed)),
            Again::No,
        ),
        (Ok(()), None) => (Outcome::Complete, None, Again::No),
    };
    report.outcome = Some(outcome);
    report.message = message.clone();
    report.again = match again {
        Again::No if report.preparing > 0 => Again::WhenChanged,
        a => a,
    };

    observer.phase(Phase::Ending);
    // What's left to tell the server, then the end report. Neither can
    // reach it when the network is what stopped the sync: the facts stay
    // journaled for next time.
    let reachable = !matches!(stop, Some(Stop::Network(_)))
        && !matches!(&ran, Err(e) if e.is_transient() && e.code() != Some("preparing"));
    if reachable {
        let _ = ctx.send(true).await;
        let s = storage.clone();
        let free = blocking(move || s.free_bytes()).await?.ok().flatten();
        let end = EndReport {
            result: outcome,
            message,
            files: report.files,
            bytes: report.bytes,
            seconds: began.elapsed().as_secs(),
            free_bytes: free,
        };
        if let Err(e) = api
            .call(
                Method::POST,
                &format!("{}/end", ctx.base),
                Some((
                    serde_json::to_vec(&end).expect("reports serialize").into(),
                    "application/json",
                )),
            )
            .await
        {
            tracing::debug!("couldn't send the end report: {e}");
        }
    }
    // Nothing is under way: leftovers are of contents that left the target.
    if outcome == Outcome::Complete {
        let s = storage.clone();
        if let Err(e) = blocking(move || s.clear_partials()).await? {
            tracing::debug!("couldn't clear partial files: {e}");
        }
    }
    tracing::info!(?report, "sync session ended");
    Ok((report, matches!(stop, Some(Stop::Refused))))
}

async fn run(ctx: &Arc<Ctx>, started: &SessionStarted, report: &mut Report) -> Result<(), Error> {
    if started.reconcile {
        // The server rebuilds what it knows from the folder: facts from
        // before are moot.
        ctx.facts.forget();
        let done = crate::reconcile::reconcile(
            &ctx.api,
            &started.session,
            &ctx.storage,
            &ctx.options,
            &*ctx.observer,
        )
        .await?;
        tracing::info!(?done, "reconciled");
        let marker = serde_json::to_vec(&Marker {
            device: started.device,
            epoch: done.epoch.clone(),
        })
        .expect("markers serialize");
        let s = ctx.storage.clone();
        blocking(move || s.write_marker(&marker))
            .await?
            .map_err(|e| Error::Storage(format!("couldn't write the marker: {e}")))?;
        report.reconciled = Some(done);
    } else {
        // Facts an earlier sync couldn't send go first, so this plan
        // doesn't repeat their work.
        while ctx.facts.waiting() > 0 {
            ctx.send(true).await?;
        }
    }

    // Facts every couple of seconds, while the plan runs.
    let ticking = CancellationToken::new();
    let ticker = {
        let (ctx, ticking) = (ctx.clone(), ticking.clone());
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(FACTS_EVERY) => {}
                    _ = ticking.cancelled() => return,
                }
                let _ = ctx.send(false).await;
            }
        })
    };
    let mut result = Ok(());
    for _ in 0..MAX_PLANS {
        ctx.changed.store(false, Ordering::Relaxed);
        result = plan(ctx, report).await;
        if result.is_err() || ctx.halt.is_cancelled() {
            break;
        }
        // What this plan did goes to the server before a fresh one is made
        // from it.
        if let Err(e) = ctx.send(true).await {
            result = Err(e);
            break;
        }
        if !ctx.changed.load(Ordering::Relaxed) {
            break;
        }
    }
    ticking.cancel();
    let _ = ticker.await;
    result
}

/// A plan page: its operations, and the cursor for the next.
struct Page {
    header: Option<PlanHeader>,
    ops: Vec<Op>,
    next: Option<String>,
}

async fn page(api: Arc<Api>, base: String, after: Option<String>) -> Result<Page, Error> {
    let url = match &after {
        None => format!("{base}/plan"),
        Some(c) => format!("{base}/plan?after={c}"),
    };
    let body = api.get(&url).await?;
    let mut page = Page {
        header: None,
        ops: Vec::new(),
        next: None,
    };
    let mut ended = false;
    for line in body.split(|&b| b == b'\n') {
        if line.trim_ascii().is_empty() {
            continue;
        }
        match PlanLine::parse(line).map_err(|e| Error::Protocol(format!("a plan line: {e}")))? {
            PlanLine::Header(h) => page.header = Some(h),
            PlanLine::Op(op) => page.ops.push(op),
            PlanLine::End(e) => {
                page.next = e.next;
                ended = true;
            }
        }
    }
    if !ended {
        return Err(Error::Protocol("a plan page without its end".into()));
    }
    Ok(page)
}

/// Run one plan, from its first page.
async fn plan(ctx: &Arc<Ctx>, report: &mut Report) -> Result<(), Error> {
    let fetch =
        |after: Option<String>| tokio::spawn(page(ctx.api.clone(), ctx.base.clone(), after));
    let mut next = Some(fetch(None));
    let mut puts = JoinSet::new();
    let mut first = true;
    let result: Result<(), Error> = async {
        while let Some(fetching) = next.take() {
            let page = fetching
                .await
                .map_err(|e| Error::Protocol(format!("fetching the plan: {e}")))??;
            if first {
                first = false;
                let header = page
                    .header
                    .as_ref()
                    .ok_or_else(|| Error::Protocol("a plan without its header".into()))?;
                let t = &header.totals;
                tracing::info!(target = header.target, totals = ?t, "planned");
                ctx.planning.store(true, Ordering::Relaxed);
                ctx.stats.puts_total.fetch_add(t.put, Ordering::Relaxed);
                ctx.stats
                    .bytes_total
                    .fetch_add(t.put_bytes, Ordering::Relaxed);
                report.preparing = t.preparing;
                report.short_bytes = t.short_bytes;
                report.target = Some(header.target.clone());
                ctx.observer.phase(Phase::Planned { totals: t.clone() });
            }
            // The next page comes while this one runs.
            if let Some(after) = page.next.clone()
                && !ctx.pausing()
            {
                next = Some(fetch(Some(after)));
            }
            let mut ordered = Vec::new();
            for op in page.ops {
                match op {
                    Op::Put {
                        path,
                        hash,
                        size,
                        replace,
                        ..
                    } => {
                        if !ordered.is_empty() {
                            ordered_steps(ctx, std::mem::take(&mut ordered)).await?;
                            if ctx.pausing() {
                                return Ok(());
                            }
                            // The end of a phase: the server hears of it.
                            let _ = ctx.send(true).await;
                            ctx.observer.phase(Phase::Putting);
                        }
                        if ctx.pausing() {
                            return Ok(());
                        }
                        start_put(
                            ctx,
                            PutOp {
                                path,
                                hash,
                                size,
                                replace,
                            },
                            &mut puts,
                        )
                        .await;
                    }
                    other => {
                        // The server lists every put after the other steps;
                        // should one ever come later, it waits for them.
                        while puts.join_next().await.is_some() {}
                        ordered.push(other);
                    }
                }
            }
            if !ordered.is_empty() {
                ordered_steps(ctx, ordered).await?;
            }
            if ctx.pausing() {
                return Ok(());
            }
        }
        Ok(())
    }
    .await;
    if let Some(fetching) = next {
        fetching.abort();
    }
    // Downloads under way finish (or, cancelled, stop at once).
    while let Some(done) = puts.join_next().await {
        if let Err(e) = done
            && e.is_panic()
        {
            std::panic::resume_unwind(e.into_panic());
        }
    }
    result
}

/// Deletes, mkdirs, moves and rmdirs, in order, each checked with a stat.
/// One that the folder refuses stops the sync: later steps may count on it.
async fn ordered_steps(ctx: &Arc<Ctx>, ops: Vec<Op>) -> Result<(), Error> {
    let c = ctx.clone();
    let refused = blocking(move || {
        for op in ops {
            if c.cancel.is_cancelled() {
                return false;
            }
            if !step(&c, op) {
                return true;
            }
        }
        false
    })
    .await?;
    if refused {
        ctx.stop(Stop::Refused);
    }
    Ok(())
}

pub(crate) fn protected(options: &Options, path: &str) -> bool {
    let first = path.split('/').next().unwrap_or(path);
    options
        .protected
        .iter()
        .any(|p| p.eq_ignore_ascii_case(first))
}

/// One step; false if the folder refused it.
fn step(ctx: &Ctx, op: Op) -> bool {
    let s = &*ctx.storage;
    let refuse = |op: &str, path: &str, hash: Option<String>, error: &str, detail: String| {
        tracing::info!(op, path, error, detail, "a step was refused");
        ctx.facts.failed(Failure {
            op: op.into(),
            path: path.into(),
            hash,
            error: error.into(),
            detail: Some(detail),
        });
        false
    };
    let guard = |path: &str| -> Option<&'static str> {
        if !valid_path(path) {
            Some(errors::IO)
        } else if protected(&ctx.options, path) {
            Some(errors::PROTECTED)
        } else {
            None
        }
    };
    match op {
        Op::Delete { path, size, hash } => {
            if let Some(e) = guard(&path) {
                return refuse(
                    "delete",
                    &path,
                    hash,
                    e,
                    "not a path the device changes".into(),
                );
            }
            match s.stat(&path) {
                Ok(None) => {}
                Ok(Some(Entry::File { size: found })) if found == size => {
                    if let Err(e) = s.remove_file(&path)
                        && e.kind() != io::ErrorKind::NotFound
                    {
                        return refuse("delete", &path, hash, errors::IO, e.to_string());
                    }
                }
                Ok(found) => {
                    return refuse(
                        "delete",
                        &path,
                        hash,
                        errors::CHANGED,
                        format!("not the file the server meant: {found:?}"),
                    );
                }
                Err(e) => return refuse("delete", &path, hash, errors::IO, e.to_string()),
            }
            ctx.facts.done(Fact::Delete { path, hash });
            ctx.stats.deleted.fetch_add(1, Ordering::Relaxed);
        }
        Op::Mkdir { path } => {
            if guard(&path).is_none() {
                // A folder that can't be made fails its puts, one by one.
                if let Err(e) = s.create_dirs(&path) {
                    tracing::info!(path, "couldn't make a folder: {e}");
                }
            }
        }
        Op::Move {
            from,
            path,
            size,
            hash,
        } => {
            if let Some(e) = guard(&from).or(guard(&path)) {
                return refuse(
                    "move",
                    &from,
                    Some(hash),
                    e,
                    "not a path the device changes".into(),
                );
            }
            let src = match s.stat(&from) {
                Ok(src) => src,
                Err(e) => return refuse("move", &from, Some(hash), errors::IO, e.to_string()),
            };
            if src == Some(Entry::File { size }) {
                if let Err(e) = s.rename(&from, &path) {
                    return refuse("move", &from, Some(hash), errors::IO, e.to_string());
                }
            } else if src.is_some() || s.stat(&path).ok().flatten() != Some(Entry::File { size }) {
                // Done before only if the source has gone and the
                // destination is what it should be.
                return refuse(
                    "move",
                    &from,
                    Some(hash),
                    errors::CHANGED,
                    format!("not the file the server meant: {src:?}"),
                );
            }
            ctx.facts.done(Fact::Move { from, path, hash });
            ctx.stats.moved.fetch_add(1, Ordering::Relaxed);
        }
        Op::Rmdir { path } => {
            if guard(&path).is_none()
                && let Err(e) = s.remove_dir(&path)
            {
                tracing::debug!(path, "couldn't remove a folder: {e}");
            }
        }
        Op::Put { .. } => unreachable!("puts aren't ordered steps"),
    }
    true
}

struct PutOp {
    path: String,
    hash: String,
    size: u64,
    replace: bool,
}

/// Start a download once there's a free slot and room for it.
async fn start_put(ctx: &Arc<Ctx>, put: PutOp, puts: &mut JoinSet<()>) {
    let disk = tokio::select! {
        p = ctx.disk.clone().acquire_owned() => p.expect("never closed"),
        _ = ctx.halt.cancelled() => return,
    };
    let net = tokio::select! {
        p = ctx.net.clone().acquire_owned() => p.expect("never closed"),
        _ = ctx.halt.cancelled() => return,
    };
    // Collect finished downloads, so the set stays small.
    while puts.try_join_next().is_some() {}
    if ctx.pausing() {
        return;
    }
    if !room_for(ctx, &put).await {
        ctx.stop(Stop::NoSpace);
        return;
    }
    ctx.stats.owed.fetch_add(put.size, Ordering::Relaxed);
    let c = ctx.clone();
    puts.spawn(async move {
        let size = put.size;
        // This file's bytes on the storage, or on their way to it.
        let mut counted = 0;
        let mut result = download(&c, &put, net, &mut counted).await;
        if matches!(result, Err(Halt::Mismatch)) {
            // Bytes that don't match their hash: the file changed on the
            // server mid-download (the next try finds it gone), or they
            // were damaged on the way. Once more, from the start.
            result = match c.net.clone().acquire_owned().await {
                Ok(net) => match download(&c, &put, net, &mut counted).await {
                    Err(Halt::Mismatch) => Err(Halt::File(
                        errors::IO,
                        "the bytes downloaded don't match their hash".into(),
                    )),
                    other => other,
                },
                Err(_) => Err(Halt::Cancelled),
            };
        }
        // What it still owed, it won't now.
        c.stats
            .owed
            .fetch_sub(size - counted.min(size), Ordering::Relaxed);
        match result {
            Ok(()) => {}
            Err(Halt::Gone) => {
                c.stats.gone.fetch_add(1, Ordering::Relaxed);
            }
            Err(Halt::File(error, detail)) => {
                tracing::info!(path = put.path, error, detail, "a download failed");
                c.stats.failed.fetch_add(1, Ordering::Relaxed);
                c.facts.failed(Failure {
                    op: "put".into(),
                    path: put.path,
                    hash: Some(put.hash),
                    error: error.into(),
                    detail: Some(detail),
                });
            }
            Err(Halt::Stop(stop)) => c.stop(stop),
            Err(Halt::Cancelled | Halt::Mismatch) => {}
        }
        drop(disk);
    });
}

/// Whether the storage has room for `put`, beside the downloads under way.
/// The plan was fitted to the free space; this catches anything else
/// filling the storage meanwhile. A replacement frees its old file once
/// it's done, so it may use the reserve the server keeps.
async fn room_for(ctx: &Ctx, put: &PutOp) -> bool {
    let s = ctx.storage.clone();
    let Ok(Ok(Some(free))) = blocking(move || s.free_bytes()).await else {
        return true;
    };
    let owed = ctx.stats.owed.load(Ordering::Relaxed);
    let margin = if put.replace { 0 } else { RESERVE_BYTES / 2 };
    free >= owed.saturating_add(put.size).saturating_add(margin)
}

/// Why a download ended without its file in place.
enum Halt {
    /// Its content left the target (`410 Gone`): a later plan has it.
    Gone,
    /// This file failed, with an error code and a detail.
    File(&'static str, String),
    /// The sync can't go on.
    Stop(Stop),
    Cancelled,
    /// The bytes didn't match their hash (and were thrown away).
    Mismatch,
}

/// Where a storage error leaves a download.
fn storage_halt(e: &io::Error, what: &str) -> Halt {
    use io::ErrorKind as K;
    match e.kind() {
        K::StorageFull | K::QuotaExceeded => Halt::Stop(Stop::NoSpace),
        K::InvalidFilename => Halt::File(errors::PATH_TOO_LONG, format!("{what}: {e}")),
        K::ReadOnlyFilesystem => Halt::Stop(Stop::Storage(format!("{what}: {e}"))),
        // This path's own trouble: a bad name for the storage, a folder in
        // the way, a link, a file too big for it (FAT32's 4 GiB).
        K::InvalidInput
        | K::FileTooLarge
        | K::IsADirectory
        | K::NotADirectory
        | K::AlreadyExists
        | K::DirectoryNotEmpty
        | K::PermissionDenied
        | K::NotFound => Halt::File(errors::IO, format!("{what}: {e}")),
        _ => Halt::Stop(Stop::Storage(format!("{what}: {e}"))),
    }
}

fn is_hash(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Each destination's own partial file: the same content can go to two
/// paths at once (a cover in each disc's folder).
fn partial_name(hash: &str, path: &str) -> String {
    format!("{hash}-{}", &blake3::hash(path.as_bytes()).to_hex()[..16])
}

/// Download `put` into its partial file, resuming what's there, then
/// flush it and put it in place.
async fn download(
    ctx: &Arc<Ctx>,
    put: &PutOp,
    net: OwnedSemaphorePermit,
    counted: &mut u64,
) -> Result<(), Halt> {
    if !valid_path(&put.path) || !is_hash(&put.hash) {
        return Err(Halt::File(errors::IO, "not a put the device takes".into()));
    }
    if protected(&ctx.options, &put.path) {
        return Err(Halt::File(
            errors::PROTECTED,
            "a name the device keeps".into(),
        ));
    }
    let name = partial_name(&put.hash, &put.path);
    let verify = ctx.options.verify_hashes;
    let mut net = Some(net);
    let mut tries = 0;
    let finished = loop {
        let s = ctx.storage.clone();
        let n = name.clone();
        let mut have = blocking(move || s.partial_len(&n))
            .await
            .map_err(|e| Halt::Stop(Stop::Storage(e.to_string())))?
            .map_err(|e| storage_halt(&e, "a partial file"))?
            .unwrap_or(0);
        if have > put.size {
            let s = ctx.storage.clone();
            let n = name.clone();
            let _ = blocking(move || s.remove_partial(&n)).await;
            have = 0;
        }
        recount(&ctx.stats, counted, have);
        if have == put.size {
            // All there already (the last sync stopped before putting it in
            // place): only the check and the rename are left.
            break None;
        }
        let mut headers = Vec::new();
        if have > 0 {
            headers.push((
                header::RANGE,
                HeaderValue::from_str(&format!("bytes={have}-")).expect("a valid header"),
            ));
            headers.push((
                header::IF_RANGE,
                HeaderValue::from_str(&format!("\"{}\"", put.hash)).expect("a valid header"),
            ));
        }
        let url = format!("{PREFIX}/content/{}", put.hash);
        let sent = tokio::select! {
            r = ctx.api.send(Method::GET, &url, &headers, None) => r,
            _ = ctx.cancel.cancelled() => return Err(Halt::Cancelled),
        };
        let res = match sent {
            Ok(res) => res,
            Err(Error::Network(m)) => {
                tries += 1;
                if tries >= DOWNLOAD_TRIES {
                    return Err(Halt::Stop(Stop::Network(m)));
                }
                backoff(ctx, tries).await?;
                continue;
            }
            Err(e) => return Err(Halt::Stop(Stop::Network(e.to_string()))),
        };
        let status = res.status();
        let resume = match status {
            StatusCode::OK => false,
            StatusCode::PARTIAL_CONTENT => {
                let from = res
                    .headers()
                    .get(header::CONTENT_RANGE)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.strip_prefix("bytes "))
                    .and_then(|v| v.split('-').next())
                    .and_then(|v| v.parse::<u64>().ok());
                if from != Some(have) {
                    return Err(Halt::File(
                        errors::IO,
                        format!("the server resumed at {from:?}, not {have}"),
                    ));
                }
                true
            }
            StatusCode::GONE => return Err(Halt::Gone),
            StatusCode::UNAUTHORIZED => return Err(Halt::Stop(Stop::Unauthorized)),
            StatusCode::RANGE_NOT_SATISFIABLE => {
                let s = ctx.storage.clone();
                let n = name.clone();
                let _ = blocking(move || s.remove_partial(&n)).await;
                tries += 1;
                if tries >= DOWNLOAD_TRIES {
                    return Err(Halt::File(
                        errors::IO,
                        "the server refused every range".into(),
                    ));
                }
                continue;
            }
            status => {
                let body = read_all(res.into_body(), IDLE_TIMEOUT)
                    .await
                    .unwrap_or_default();
                let e = refusal(status, &body);
                if e.is_transient() {
                    tries += 1;
                    if tries >= DOWNLOAD_TRIES {
                        return Err(Halt::Stop(Stop::Network(e.to_string())));
                    }
                    backoff(ctx, tries).await?;
                    continue;
                }
                return Err(Halt::File(errors::IO, e.to_string()));
            }
        };
        let start = if resume { have } else { 0 };
        recount(&ctx.stats, counted, start);

        // The disk gets the bytes on a thread of its own, hashing them on
        // the way, so the network never waits for a write.
        let (tx, rx) = mpsc::channel::<Bytes>(4);
        let s = ctx.storage.clone();
        let n = name.clone();
        let writer = tokio::task::spawn_blocking(move || write(&*s, &n, resume, verify, rx));
        let streamed = stream(ctx, res.into_body(), tx, put.size - start, counted).await;
        let written = writer
            .await
            .map_err(|e| Halt::Stop(Stop::Storage(e.to_string())))?;
        match (streamed, written) {
            (_, Err(e)) => {
                if matches!(
                    e.kind(),
                    io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded
                ) {
                    // Its room goes back; it starts over when there's more.
                    let s = ctx.storage.clone();
                    let n = name.clone();
                    let _ = blocking(move || s.remove_partial(&n)).await;
                }
                return Err(storage_halt(&e, "writing"));
            }
            (Err(Streamed::Cancelled), _) => return Err(Halt::Cancelled),
            (Err(Streamed::TooLong), _) => {
                return Err(Halt::File(
                    errors::IO,
                    "the server sent more than the file".into(),
                ));
            }
            (Err(Streamed::Network(m)), Ok(_)) => {
                tries += 1;
                if tries >= DOWNLOAD_TRIES {
                    return Err(Halt::Stop(Stop::Network(m)));
                }
                backoff(ctx, tries).await?;
                continue;
            }
            (Ok(()), Ok((file, hasher, n))) => {
                if start + n != put.size {
                    // Cut short: what came is kept, and the rest asked for.
                    tries += 1;
                    if tries >= DOWNLOAD_TRIES {
                        return Err(Halt::Stop(Stop::Network(
                            "downloads keep ending early".into(),
                        )));
                    }
                    continue;
                }
                break Some((file, hasher));
            }
        }
    };
    // The network's part is done: the next download can start while this
    // one is flushed.
    drop(net.take());
    let c = ctx.clone();
    let path = put.path.clone();
    let hash = put.hash.clone();
    let (size, replace) = (put.size, put.replace);
    blocking(move || finish(&c, &name, &path, &hash, size, replace, verify, finished))
        .await
        .map_err(|e| Halt::Stop(Stop::Storage(e.to_string())))?
}

/// Set this file's share of the progress to `now` bytes, and what it still
/// owes the storage to match.
fn recount(stats: &Stats, counted: &mut u64, now: u64) {
    if now > *counted {
        stats.bytes.fetch_add(now - *counted, Ordering::Relaxed);
        stats.owed.fetch_sub(now - *counted, Ordering::Relaxed);
    } else {
        stats.bytes.fetch_sub(*counted - now, Ordering::Relaxed);
        stats.owed.fetch_add(*counted - now, Ordering::Relaxed);
    }
    *counted = now;
}

async fn backoff(ctx: &Ctx, tries: u32) -> Result<(), Halt> {
    let wait = Duration::from_millis(500 << tries.min(5));
    tokio::select! {
        _ = tokio::time::sleep(wait) => Ok(()),
        _ = ctx.cancel.cancelled() => Err(Halt::Cancelled),
    }
}

enum Streamed {
    Network(String),
    TooLong,
    Cancelled,
}

/// Pass a download's body to its writer in large pieces.
async fn stream(
    ctx: &Ctx,
    mut body: hyper::body::Incoming,
    tx: mpsc::Sender<Bytes>,
    expected: u64,
    counted: &mut u64,
) -> Result<(), Streamed> {
    let size = ctx.options.buffer_bytes.max(64 << 10);
    let mut buf = BytesMut::with_capacity(size);
    let mut got = 0u64;
    loop {
        let chunk = tokio::select! {
            c = next_chunk(&mut body, IDLE_TIMEOUT) => c,
            _ = ctx.cancel.cancelled() => {
                // What came is kept, for the next sync to resume.
                if !buf.is_empty() {
                    let _ = tx.send(buf.split().freeze()).await;
                }
                return Err(Streamed::Cancelled);
            }
        };
        match chunk {
            Ok(Some(data)) => {
                got += data.len() as u64;
                if got > expected {
                    return Err(Streamed::TooLong);
                }
                let n = data.len() as u64;
                recount(&ctx.stats, counted, *counted + n);
                buf.extend_from_slice(&data);
                if buf.len() >= size && tx.send(buf.split().freeze()).await.is_err() {
                    // The writer failed: its error says why.
                    return Ok(());
                }
                if buf.capacity() < size {
                    buf.reserve(size);
                }
            }
            Ok(None) => {
                if !buf.is_empty() {
                    let _ = tx.send(buf.freeze()).await;
                }
                return Ok(());
            }
            Err(e) => {
                if !buf.is_empty() {
                    let _ = tx.send(buf.freeze()).await;
                }
                return Err(Streamed::Network(e.to_string()));
            }
        }
    }
}

type Written = (Box<dyn Partial>, Option<blake3::Hasher>, u64);

/// The writer: the partial file, appended to or begun again, hashed as it
/// goes (from its start, when resumed).
fn write(
    storage: &dyn Storage,
    name: &str,
    resume: bool,
    verify: bool,
    mut rx: mpsc::Receiver<Bytes>,
) -> io::Result<Written> {
    let mut hasher = verify.then(blake3::Hasher::new);
    if resume && let Some(h) = hasher.as_mut() {
        h.update_reader(storage.read_partial(name)?)?;
    }
    let mut file = storage.write_partial(name, resume)?;
    let mut n = 0u64;
    while let Some(buf) = rx.blocking_recv() {
        file.write_all(&buf)?;
        if let Some(h) = hasher.as_mut() {
            h.update(&buf);
        }
        n += buf.len() as u64;
    }
    Ok((file, hasher, n))
}

/// Check the download's hash, flush it and put it in place, and record the
/// fact.
#[allow(clippy::too_many_arguments)]
fn finish(
    ctx: &Ctx,
    name: &str,
    path: &str,
    hash: &str,
    size: u64,
    replace: bool,
    verify: bool,
    written: Option<(Box<dyn Partial>, Option<blake3::Hasher>)>,
) -> Result<(), Halt> {
    let s = &*ctx.storage;
    let (mut file, hasher) = match written {
        Some(w) => w,
        // Found whole: opened to be flushed, and read to be checked.
        None => {
            let file = s
                .write_partial(name, true)
                .map_err(|e| storage_halt(&e, "a partial file"))?;
            let hasher = if verify {
                let mut h = blake3::Hasher::new();
                s.read_partial(name)
                    .and_then(|r| h.update_reader(r).map(|_| ()))
                    .map_err(|e| storage_halt(&e, "a partial file"))?;
                Some(h)
            } else {
                None
            };
            (file, hasher)
        }
    };
    if let Some(h) = hasher
        && h.finalize().to_hex().as_str() != hash
    {
        let _ = s.remove_partial(name);
        return Err(Halt::Mismatch);
    }
    file.sync().map_err(|e| storage_halt(&e, "flushing"))?;
    drop(file);
    s.publish(name, path, replace)
        .map_err(|e| storage_halt(&e, "putting it in place"))?;
    ctx.facts.done(Fact::Put {
        path: path.into(),
        hash: hash.into(),
        size,
    });
    ctx.stats.puts.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

use std::io::Write as _;
