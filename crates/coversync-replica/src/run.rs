//! One sync: a session, a reconcile if the server asks for one, then the
//! plan, page by page.

use std::collections::HashMap;
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
/// The longest the server goes without hearing from a session.
const KEEPALIVE: Duration = Duration::from_secs(60);

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
    // A sync that never got to its end (a crash, the power cut) may have
    // changed the folder without the server hearing of it, or without the
    // changes reaching the disk: the folder is checked.
    let s = storage.clone();
    let unfinished = blocking(move || s.begin_changes())
        .await?
        .map_err(|e| Error::Storage(format!("couldn't note the sync in the folder: {e}")))?;
    if unfinished {
        tracing::info!("the last sync never finished: checking the folder");
    }
    let mut check = unfinished;
    // A check this sync owes: the last one never finished, or the facts
    // journal couldn't be read. Until one happens, the note stays.
    let mut owed = unfinished;
    let mut checked = false;
    // Facts the journal couldn't keep and the server never confirmed: the
    // folder can't be trusted to match what the server knows.
    let mut lost = false;
    let result = loop {
        let mut flags = Flags::default();
        let session = session(
            &api, &storage, &options, reason, check, &observer, &cancel, began, &mut flags,
        )
        .await;
        // Whatever ended the session, the token refused included.
        lost |= flags.lost;
        owed |= flags.damaged;
        let (report, refused) = match session {
            Ok(r) => r,
            Err(e) => break Err(e),
        };
        checked |= report.reconciled.is_some();
        // A step the folder refused (a file that wasn't what the server
        // thought): the server's idea of the folder is wrong, so check it.
        if refused && !check && !cancel.is_cancelled() {
            tracing::info!("files changed on the device: checking the folder");
            check = true;
            continue;
        }
        break Ok(report);
    };
    // Everything this sync did is made durable before the note goes, unless
    // a check it owed didn't happen, or facts were lost: then the note stays
    // and the next sync checks the folder.
    if (!owed || checked) && !lost {
        let s = storage.clone();
        match blocking(move || s.end_changes()).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!("couldn't flush the folder: {e}"),
            Err(e) => tracing::warn!("couldn't flush the folder: {e}"),
        }
    } else if lost {
        tracing::warn!("facts couldn't be journaled: the next sync checks the folder");
    }
    // The flush is part of the sync's time.
    result.map(|mut report| {
        report.seconds = began.elapsed().as_secs_f64();
        report
    })
}

/// `fut`, unless the sync is cancelled first.
async fn unless_cancelled<T>(
    cancel: &CancellationToken,
    fut: impl std::future::Future<Output = Result<T, Error>>,
) -> Result<T, Error> {
    tokio::select! {
        r = fut => r,
        _ = cancel.cancelled() => Err(Error::Cancelled),
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
    /// The target kept changing until the session's plans ran out.
    Changing,
}

impl Stop {
    /// Which of two reasons to keep.
    fn rank(&self) -> u8 {
        match self {
            Stop::Unauthorized => 4,
            Stop::Storage(_) => 3,
            Stop::Network(_) => 2,
            Stop::NoSpace => 1,
            Stop::Refused | Stop::Changing => 0,
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
    /// Gone in the last plan, which no fresh plan has made up for.
    gone_last: AtomicU64,
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
    /// Partial files from earlier syncs that no download of this one has
    /// taken up yet, with their sizes: room the engine can take back.
    spare: Mutex<HashMap<String, u64>>,
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

    /// Free space as the server should count it: what the storage has,
    /// and what spare partial files hold, which the engine can take back.
    async fn free_bytes(&self) -> Result<Option<u64>, Error> {
        let s = self.storage.clone();
        let free = blocking(move || s.free_bytes()).await?.ok().flatten();
        let spare: u64 = self
            .spare
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .sum();
        Ok(free.map(|f| f.saturating_add(spare)))
    }

    /// Send facts, with progress. `always`, at the end of a phase or to
    /// keep the session alive: send even with nothing new, and every fact
    /// waiting, however many batches that takes. Otherwise one batch.
    async fn send(&self, always: bool) -> Result<(), Error> {
        loop {
            self.send_one(always).await?;
            if !always || self.facts.unconfirmed() == 0 {
                return Ok(());
            }
        }
    }

    async fn send_one(&self, always: bool) -> Result<(), Error> {
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
        let free = self.free_bytes().await?;
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
        let url = format!("{}/facts", self.base);
        match unless_cancelled(
            &self.cancel,
            self.api.post_json::<_, FactsReply>(&url, &body),
        )
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
                    Error::Cancelled => {}
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

/// What a session leaves the sync to know, however it ended.
#[derive(Default)]
struct Flags {
    /// Facts the journal couldn't keep never reached the server.
    lost: bool,
    /// The facts journal couldn't be read whole: a check is owed.
    damaged: bool,
}

/// One session. Returns its report and whether a step was refused.
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
    flags: &mut Flags,
) -> Result<(Report, bool), Error> {
    observer.phase(Phase::Starting);
    let s = storage.clone();
    let (marker, free, spare, facts) = blocking(move || {
        let marker = s
            .read_marker()
            .ok()
            .flatten()
            .and_then(|b| serde_json::from_slice::<Marker>(&b).ok());
        let free = s.free_bytes().ok().flatten();
        let spare: HashMap<String, u64> = s.partials().unwrap_or_default().into_iter().collect();
        (marker, free, spare, Facts::load(s))
    })
    .await?;
    // A journal that couldn't be read whole may have lost facts of what an
    // earlier sync did: the folder is checked.
    flags.damaged = facts.damaged();
    let check = check || flags.damaged;
    // Partial files are room the engine can take back: counted as free, so
    // that a download cut short still fits the plan when only its rest does.
    let free = free.map(|f| f.saturating_add(spare.values().sum()));
    let capabilities = options
        .capabilities
        .clone()
        .unwrap_or_else(|| storage.capabilities());
    let start = SessionStart {
        client: options.client.clone(),
        capabilities,
        marker,
        check,
        free_bytes: free,
        reason,
    };
    let url = format!("{PREFIX}/sessions");
    let started: SessionStarted = unless_cancelled(cancel, api.post_json(&url, &start)).await?;
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
        spare: Mutex::new(spare),
    });
    let mut report = Report {
        target: Some(started.target.clone()),
        ..Default::default()
    };

    let ran = run(&ctx, &started, &mut report).await;
    flags.lost = ctx.facts.lost();
    let stop = ctx.stop.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let s = &ctx.stats;
    report.files = s.puts.load(Ordering::Relaxed);
    report.bytes = s.bytes.load(Ordering::Relaxed);
    report.moved = s.moved.load(Ordering::Relaxed);
    report.deleted = s.deleted.load(Ordering::Relaxed);
    report.failed = s.failed.load(Ordering::Relaxed);
    report.gone = s.gone.load(Ordering::Relaxed);

    let cancelled = cancel.is_cancelled();
    let (outcome, message, again) = match (&ran, &stop) {
        (Err(Error::Unauthorized), _) | (_, Some(Stop::Unauthorized)) => {
            return Err(Error::Unauthorized);
        }
        _ if cancelled => (Outcome::Cancelled, None, Again::No),
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
        (Ok(()), Some(Stop::Changing)) => (
            Outcome::Partial,
            Some("the Library kept changing during the sync".into()),
            Again::After(Duration::from_secs(15)),
        ),
        // Files that changed on the server mid-sync come in a later plan,
        // once the server has hashed them again.
        (Ok(()), None) if s.gone_last.load(Ordering::Relaxed) > 0 => (
            Outcome::Partial,
            Some(format!(
                "{} files changed on the server during the sync",
                s.gone_last.load(Ordering::Relaxed)
            )),
            Again::After(Duration::from_secs(15)),
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
    // Cancelled, the user wants it over: what's left is tried briefly.
    let (files, bytes) = (report.files, report.bytes);
    let ending = async {
        let _ = ctx.send(true).await;
        let free = ctx.free_bytes().await.ok().flatten();
        let end = EndReport {
            result: outcome,
            message,
            files,
            bytes,
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
    };
    if reachable {
        // Cancelled before or during these, they get a few seconds more.
        let brief = Duration::from_secs(5);
        if cancelled {
            let _ = tokio::time::timeout(brief, ending).await;
        } else {
            tokio::pin!(ending);
            tokio::select! {
                _ = &mut ending => {}
                _ = cancel.cancelled() => {
                    let _ = tokio::time::timeout(brief, ending).await;
                }
            }
        }
    }
    // The plan ran to its end, so nothing is under way and no later page
    // wants a partial file: leftovers are of contents that left the target,
    // or of puts that failed.
    if ran.is_ok() && stop.is_none() && !cancelled {
        let s = storage.clone();
        if let Err(e) = blocking(move || s.clear_partials()).await? {
            tracing::debug!("couldn't clear partial files: {e}");
        }
    }
    tracing::info!(?report, "sync session ended");
    flags.lost = ctx.facts.lost();
    Ok((report, matches!(stop, Some(Stop::Refused))))
}

async fn run(ctx: &Arc<Ctx>, started: &SessionStarted, report: &mut Report) -> Result<(), Error> {
    if started.reconcile {
        // The server rebuilds what it knows from the folder: facts from
        // before are moot.
        ctx.facts.forget();
    }
    // Facts every couple of seconds while the plan runs, and something at
    // least every minute (a long reconcile included), so the server keeps
    // the session.
    let ticking = CancellationToken::new();
    let ticker = {
        let (ctx, ticking) = (ctx.clone(), ticking.clone());
        tokio::spawn(async move {
            let mut last = Instant::now();
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(FACTS_EVERY) => {}
                    _ = ticking.cancelled() => return,
                }
                let keepalive = last.elapsed() >= KEEPALIVE;
                if ctx.send(keepalive).await.is_ok() && keepalive {
                    last = Instant::now();
                }
            }
        })
    };
    let result = run_plans(ctx, started, report).await;
    ticking.cancel();
    let _ = ticker.await;
    result
}

async fn run_plans(
    ctx: &Arc<Ctx>,
    started: &SessionStarted,
    report: &mut Report,
) -> Result<(), Error> {
    if started.reconcile {
        let done = crate::reconcile::reconcile(
            &ctx.api,
            &started.session,
            &ctx.storage,
            &ctx.options,
            &*ctx.observer,
            &ctx.cancel,
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
    } else if ctx.facts.waiting() > 0 {
        // Facts an earlier sync couldn't send go first, so this plan
        // doesn't repeat their work.
        ctx.send(true).await?;
    }

    let mut result = Ok(());
    for _ in 0..MAX_PLANS {
        ctx.changed.store(false, Ordering::Relaxed);
        ctx.stats.gone_last.store(0, Ordering::Relaxed);
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
    // Plans ran out with the target still moving (a long import): what's
    // left waits for a later sync, which the report asks for.
    if result.is_ok() && !ctx.halt.is_cancelled() && ctx.changed.load(Ordering::Relaxed) {
        ctx.stop(Stop::Changing);
    }
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
    let mut putting = false;
    let result: Result<(), Error> = async {
        while let Some(mut fetching) = next.take() {
            let page = tokio::select! {
                page = &mut fetching => {
                    page.map_err(|e| Error::Protocol(format!("fetching the plan: {e}")))??
                }
                _ = ctx.cancel.cancelled() => {
                    fetching.abort();
                    return Err(Error::Cancelled);
                }
            };
            if first {
                first = false;
                let header = page
                    .header
                    .as_ref()
                    .ok_or_else(|| Error::Protocol("a plan without its header".into()))?;
                let t = &header.totals;
                tracing::info!(target = header.target, totals = ?t, "planned");
                ctx.planning.store(true, Ordering::Relaxed);
                // A fresh plan holds only what's left: the totals are what's
                // done so far and that.
                let s = &ctx.stats;
                s.puts_total
                    .store(s.puts.load(Ordering::Relaxed) + t.put, Ordering::Relaxed);
                s.bytes_total.store(
                    s.bytes.load(Ordering::Relaxed) + t.put_bytes,
                    Ordering::Relaxed,
                );
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
                        }
                        if ctx.pausing() {
                            return Ok(());
                        }
                        if !putting {
                            putting = true;
                            ctx.observer.phase(Phase::Putting);
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
    // As the server compares names on storage that ignores case.
    let first = path.split('/').next().unwrap_or(path).to_uppercase();
    options.protected.iter().any(|p| p.to_uppercase() == first)
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
    // Collect finished downloads, so the set stays small.
    while puts.try_join_next().is_some() {}
    // Its own partial file, if an earlier sync left one, is no spare: taken
    // first, so freeing room can't remove it, and what it holds needn't fit
    // again. Given back if this put doesn't start.
    let mut claim = Claim {
        ctx: ctx.clone(),
        name: None,
        size: 0,
    };
    if valid_path(&put.path) && is_hash(&put.hash) && !protected(&ctx.options, &put.path) {
        let name = partial_name(&put.hash, &put.path);
        let size = ctx
            .spare
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&name);
        if let Some(size) = size {
            claim.name = Some(name);
            claim.size = size;
        }
    }
    let need = put.size.saturating_sub(claim.size);
    // The plan counts on spare partial files going, and on each replacement
    // freeing its old file once it's done: take the spares back, then wait
    // for downloads under way, before calling the storage full.
    loop {
        if ctx.pausing() {
            return;
        }
        if room_for(ctx, need, put.replace).await {
            break;
        }
        if reclaim(ctx).await {
            continue;
        }
        let done = tokio::select! {
            done = puts.join_next() => done,
            _ = ctx.halt.cancelled() => return,
        };
        match done {
            None => {
                ctx.stop(Stop::NoSpace);
                return;
            }
            Some(Err(e)) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
            Some(_) => {}
        }
    }
    let net = tokio::select! {
        p = ctx.net.clone().acquire_owned() => p.expect("never closed"),
        _ = ctx.halt.cancelled() => return,
    };
    if ctx.pausing() {
        return;
    }
    // Started: the partial file is the download's now.
    claim.name = None;
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
                c.stats.gone_last.fetch_add(1, Ordering::Relaxed);
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

/// A put's own spare partial file, taken back into the spares unless the
/// put starts.
struct Claim {
    ctx: Arc<Ctx>,
    name: Option<String>,
    size: u64,
}

impl Drop for Claim {
    fn drop(&mut self) {
        if let Some(name) = self.name.take() {
            self.ctx
                .spare
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(name, self.size);
        }
    }
}

/// Remove the largest spare partial file no download has taken up; whether
/// there was one. One at a time, so the room check stops it as soon as it
/// has made enough: a spare a later put would resume is kept if it can be.
async fn reclaim(ctx: &Ctx) -> bool {
    let largest = {
        let mut spare = ctx.spare.lock().unwrap_or_else(|e| e.into_inner());
        let name = spare
            .iter()
            .max_by_key(|(_, size)| **size)
            .map(|(name, _)| name.clone());
        name.inspect(|n| {
            spare.remove(n);
        })
    };
    let Some(name) = largest else {
        return false;
    };
    tracing::info!(name, "taking back room from an old partial file");
    let s = ctx.storage.clone();
    let _ = blocking(move || s.remove_partial(&name)).await;
    true
}

/// Whether the storage has room for `need` more bytes (a put's, less what
/// its partial file holds already), beside the downloads under way. The
/// plan was fitted to the free space; this catches anything else filling
/// the storage meanwhile. A replacement frees its old file once it's done,
/// so it may use the reserve the server keeps.
async fn room_for(ctx: &Ctx, need: u64, replace: bool) -> bool {
    let s = ctx.storage.clone();
    let Ok(Ok(Some(free))) = blocking(move || s.free_bytes()).await else {
        return true;
    };
    let owed = ctx.stats.owed.load(Ordering::Relaxed);
    let margin = if replace { 0 } else { RESERVE_BYTES / 2 };
    free >= owed.saturating_add(need).saturating_add(margin)
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
            StatusCode::GONE => {
                // Of content that left the target: its room goes back.
                let s = ctx.storage.clone();
                let n = name.clone();
                let _ = blocking(move || s.remove_partial(&n)).await;
                return Err(Halt::Gone);
            }
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
                let body = tokio::select! {
                    body = read_all(res.into_body(), IDLE_TIMEOUT) => body.unwrap_or_default(),
                    _ = ctx.cancel.cancelled() => return Err(Halt::Cancelled),
                };
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
        let cancel = ctx.cancel.clone();
        let writer =
            tokio::task::spawn_blocking(move || write(&*s, &n, resume, verify, &cancel, rx));
        let streamed = stream(ctx, res.into_body(), tx, put.size - start, counted).await;
        let written = writer
            .await
            .map_err(|e| Halt::Stop(Stop::Storage(e.to_string())))?;
        match (streamed, written) {
            (_, Err(_)) if ctx.cancel.is_cancelled() => return Err(Halt::Cancelled),
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

/// Hash what `r` holds, a MiB at a time, stopping if the sync is cancelled
/// (a large partial file on a slow card takes a while).
fn hash_into(
    h: &mut blake3::Hasher,
    mut r: impl io::Read,
    cancel: &CancellationToken,
) -> io::Result<()> {
    let mut buf = vec![0; 1 << 20];
    loop {
        if cancel.is_cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        let n = match r.read(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if n == 0 {
            return Ok(());
        }
        h.update(&buf[..n]);
    }
}

/// The writer: the partial file, appended to or begun again, hashed as it
/// goes (from its start, when resumed).
fn write(
    storage: &dyn Storage,
    name: &str,
    resume: bool,
    verify: bool,
    cancel: &CancellationToken,
    mut rx: mpsc::Receiver<Bytes>,
) -> io::Result<Written> {
    let mut hasher = verify.then(blake3::Hasher::new);
    if resume && let Some(h) = hasher.as_mut() {
        hash_into(h, storage.read_partial(name)?, cancel)?;
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
                if let Err(e) = s
                    .read_partial(name)
                    .and_then(|r| hash_into(&mut h, r, &ctx.cancel))
                {
                    // Cancelled, the partial stays for next time.
                    return Err(if ctx.cancel.is_cancelled() {
                        Halt::Cancelled
                    } else {
                        storage_halt(&e, "a partial file")
                    });
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protected_names_ignore_case_in_any_script() {
        let options = Options {
            protected: vec![".rockbox".into(), "MÚSICA".into()],
            ..Default::default()
        };
        assert!(protected(&options, ".ROCKBOX/config.cfg"));
        assert!(protected(&options, "música/track.flac"));
        assert!(!protected(&options, "musica/track.flac"));
        assert!(!protected(&options, "Artist/.rockbox"));
    }
}
