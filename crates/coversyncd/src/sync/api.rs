//! The device routes, under `/api/sync/v1/`. Each acts for the device whose
//! token made the request; a `sync` token may use all of them and nothing
//! else. A browser's login isn't a device and is turned away.

use std::collections::HashMap;
use std::convert::Infallible;
use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use coversync_protocol::{
    Count, EndReport, FactsBatch, FactsReply, HEARTBEAT_RANGE, HEARTBEAT_SECONDS, Held, LastSync,
    Listed, Op, PAGE_BYTES, PAGE_OPS, PlanEnd, PlanHeader, PlanLine, Progress, Reason,
    ReconcileDone, SampleRequest, Sampled, SessionStart, SessionStarted, Status, TargetEvent,
};
use futures_util::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};

use super::plan::{self, Held as HeldFile};
use super::store::LastSession;
use super::target::{Source, Target};
use super::{NoTarget, now, ranges, target};
use crate::Shared;
use crate::auth::Access;
use crate::events::Notice;

/// Most files a reconcile's listing may name.
const MAX_LISTED: usize = 4_000_000;
/// A session no request has touched for this long is let go.
const SESSION_IDLE: Duration = Duration::from_secs(3600);
/// A session no request has touched for this long isn't shown as running:
/// its device has most likely gone without saying so.
const SESSION_QUIET: Duration = Duration::from_secs(300);
/// Browsers hear of a running sync's progress at most this often.
const PROGRESS_NOTICE: Duration = Duration::from_secs(1);
/// File reads for downloads.
const READ_CHUNK: usize = 512 * 1024;
/// How often a device's event stream checks its token is still good.
const STILL_DEVICE: Duration = Duration::from_secs(20);

pub fn router() -> Router<Shared> {
    Router::new()
        .route("/api/sync/v1/status", get(status))
        .route("/api/sync/v1/events", get(events))
        .route("/api/sync/v1/sessions", post(start))
        .route("/api/sync/v1/sessions/{s}/reconcile", post(reconcile))
        .route("/api/sync/v1/sessions/{s}/reconcile/samples", post(samples))
        .route("/api/sync/v1/sessions/{s}/plan", get(plan_page))
        .route("/api/sync/v1/sessions/{s}/facts", post(facts))
        .route("/api/sync/v1/sessions/{s}/end", post(end))
        .route("/api/sync/v1/content/{hash}", get(content))
        .route("/api/sync/v1/inventory", get(inventory))
        // A reconcile's listing of a large folder.
        .layer(DefaultBodyLimit::max(64 << 20))
}

/// A request turned away: the status, the API's `code` and what to tell
/// the user.
#[derive(Debug)]
pub(super) struct Problem(pub StatusCode, pub &'static str, pub String);

impl Problem {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Problem(status, code, message.into())
    }

    fn failed(e: impl std::fmt::Display) -> Self {
        tracing::error!("sync: {e}");
        Problem::new(StatusCode::INTERNAL_SERVER_ERROR, "failed", e.to_string())
    }

    fn bad(message: impl Into<String>) -> Self {
        Problem::new(StatusCode::BAD_REQUEST, "bad_request", message)
    }

    fn no_session() -> Self {
        Problem::new(
            StatusCode::NOT_FOUND,
            "no_session",
            "There's no such sync session; start another.",
        )
    }
}

impl IntoResponse for Problem {
    fn into_response(self) -> Response {
        #[derive(Serialize)]
        struct Body<'a> {
            error: &'a str,
            code: &'a str,
        }
        let mut res = (
            self.0,
            Json(Body {
                error: &self.2,
                code: self.1,
            }),
        )
            .into_response();
        res.headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        res
    }
}

impl From<NoTarget> for Problem {
    fn from(e: NoTarget) -> Self {
        match e {
            NoTarget::NoDevice => Problem::new(
                StatusCode::NOT_FOUND,
                "no_device",
                "This device has been removed.",
            ),
            NoTarget::NoLibrary => Problem::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "no_library",
                "The Library folder isn't available, so there's nothing to sync to.",
            ),
            NoTarget::Failed(e) => Problem::failed(e),
        }
    }
}

impl From<rusqlite::Error> for Problem {
    fn from(e: rusqlite::Error) -> Self {
        Problem::failed(e)
    }
}

type Answer<T = Response> = Result<T, Problem>;

/// The device a request is from.
fn device(access: &Access) -> Answer<i64> {
    access.device.ok_or_else(|| {
        Problem::new(
            StatusCode::FORBIDDEN,
            "not_a_device",
            "Only a paired device can sync; a browser's login can't.",
        )
    })
}

/// Run blocking work off the async runtime.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Answer<T> + Send + 'static) -> Answer<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(Problem::failed)?
}

pub(super) use coversync_protocol::valid_path;

fn ndjson(body: String) -> Response {
    let mut res = ([(header::CONTENT_TYPE, "application/x-ndjson")], body).into_response();
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

/// The NDJSON lines of a request body.
fn lines<T: serde::de::DeserializeOwned>(body: &[u8]) -> Answer<Vec<T>> {
    body.split(|&b| b == b'\n')
        .filter(|l| !l.trim_ascii().is_empty())
        .map(|l| serde_json::from_slice(l).map_err(|e| Problem::bad(format!("bad line: {e}"))))
        .collect()
}

// Sessions

/// Sync sessions under way, by id.
#[derive(Default)]
pub struct Sessions(Mutex<HashMap<String, Session>>);

pub struct Session {
    device: i64,
    started_at: i64,
    reason: Reason,
    free_bytes: Option<u64>,
    /// Set while the device's folder needs a reconcile before a plan.
    reconcile: Option<Reconcile>,
    plan: Option<Arc<Snapshot>>,
    pub progress: Progress,
    touched: Instant,
    noticed: Option<Instant>,
}

/// A plan as worked out at the start, served page by page.
pub struct Snapshot {
    pub target: Arc<Target>,
    pub header: PlanHeader,
    ops: Vec<Op>,
}

#[derive(Default)]
struct Reconcile {
    listing: Vec<Listed>,
    listed: bool,
    /// Files to sample: the ranges asked for, and the contents of their
    /// size they could be, shared by every file of that size.
    wanted: HashMap<String, Arc<Wanted>>,
    target: Option<Arc<Target>>,
    /// Files found to hold a content, by path.
    matched: HashMap<String, String>,
    /// Candidates' own samples, read once for every part of the samples.
    samples: Arc<Mutex<HashMap<String, String>>>,
}

/// Contents of one size: the ranges whose sample tells them apart, and
/// which contents they are.
struct Wanted {
    ranges: Vec<[u64; 2]>,
    candidates: Vec<String>,
}

/// What Sync setup shows of a session under way.
#[derive(Debug, Clone, Serialize)]
pub struct Running {
    pub started_at: i64,
    pub reason: Reason,
    pub progress: Progress,
    pub totals: Option<coversync_protocol::Totals>,
    pub reconciling: bool,
}

impl Sessions {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Session>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Run `f` on a device's own session.
    fn with<T>(&self, id: &str, device: i64, f: impl FnOnce(&mut Session) -> T) -> Answer<T> {
        let mut sessions = self.lock();
        let session = sessions
            .get_mut(id)
            .filter(|s| s.device == device)
            .ok_or_else(Problem::no_session)?;
        session.touched = Instant::now();
        Ok(f(session))
    }

    /// The session under way on a device, as Sync setup shows it.
    pub fn running(&self, device: i64) -> Option<Running> {
        self.lock()
            .values()
            .find(|s| s.device == device && s.touched.elapsed() < SESSION_QUIET)
            .map(|s| Running {
                started_at: s.started_at,
                reason: s.reason,
                progress: s.progress.clone(),
                totals: s.plan.as_ref().map(|p| p.header.totals.clone()),
                reconciling: s.reconcile.is_some(),
            })
    }

    /// The free space a device last reported in a session under way.
    pub fn free_bytes(&self, device: i64) -> Option<u64> {
        self.lock()
            .values()
            .find(|s| s.device == device)
            .and_then(|s| s.free_bytes)
    }
}

fn random_id(prefix: &str) -> Answer<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(Problem::failed)?;
    Ok(format!(
        "{prefix}{}",
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ))
}

// Routes

/// `POST sessions`: start a sync.
async fn start(
    State(state): State<Shared>,
    Extension(access): Extension<Access>,
    Json(req): Json<SessionStart>,
) -> Answer<Json<SessionStarted>> {
    let device = device(&access)?;
    let s = state.clone();
    let (target, reconcile) = blocking(move || {
        s.sync
            .store
            .session_started(device, &req.client, &req.capabilities)?;
        let target = target(&s, device)?;
        // A sync asked for in Sync setup is under way now, with a target to
        // sync to: one refused for want of a Library stays asked for.
        s.sync
            .store
            .set_flag(device, super::store::Flag::Sync, false)?;
        let st = s.sync.store.device(device)?;
        let marked = req
            .marker
            .as_ref()
            .is_some_and(|m| m.device == device && Some(&m.epoch) == st.epoch.as_ref());
        let reconcile = !marked || req.check || st.check_requested;
        Ok((target, (reconcile, req.reason, req.free_bytes)))
    })
    .await?;
    let (reconcile, reason, free_bytes) = reconcile;
    let id = random_id("s_")?;
    {
        let mut sessions = state.sync.sessions.lock();
        // One session per device: a new one ends the last.
        sessions.retain(|_, s| s.device != device && s.touched.elapsed() < SESSION_IDLE);
        sessions.insert(
            id.clone(),
            Session {
                device,
                started_at: now(),
                reason,
                free_bytes,
                reconcile: reconcile.then(Reconcile::default),
                plan: None,
                progress: Progress::default(),
                touched: Instant::now(),
                noticed: None,
            },
        );
    }
    state.events.publish(Notice::DeviceSync { device });
    tracing::info!(
        device,
        session = id,
        ?reason,
        reconcile,
        "sync session started"
    );
    Ok(Json(SessionStarted {
        session: id,
        device,
        reconcile,
        target: target.version.clone(),
    }))
}

#[derive(Deserialize)]
struct Part {
    #[serde(default)]
    last: Option<u8>,
}

/// `POST sessions/{s}/reconcile`: the folder's listing, in one request or
/// several, the last with `?last=1`. Its reply names the files to sample,
/// or is `503 preparing` while the server is hashing or making files the
/// listed ones might be: the device starts a session again later.
async fn reconcile(
    State(state): State<Shared>,
    Extension(access): Extension<Access>,
    Path(id): Path<String>,
    Query(part): Query<Part>,
    body: Bytes,
) -> Answer {
    let device = device(&access)?;
    let mut listed: Vec<Listed> = lines(&body)?;
    listed.retain(|l| valid_path(&l.path));
    let last = part.last == Some(1);
    state.sync.sessions.with(&id, device, |s| {
        let r = s.reconcile.as_mut().ok_or_else(|| {
            Problem::new(
                StatusCode::CONFLICT,
                "no_reconcile",
                "This session has no reconcile to send.",
            )
        })?;
        if r.listed {
            return Err(Problem::bad("The listing was already finished."));
        }
        if r.listing.len() + listed.len() > MAX_LISTED {
            return Err(Problem::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "too_many_files",
                format!("A folder can hold at most {MAX_LISTED} files to sync."),
            ));
        }
        r.listing.extend(listed);
        r.listed = last;
        Ok(())
    })??;
    if !last {
        return Ok(ndjson(String::new()));
    }
    let listing = state.sync.sessions.with(&id, device, |s| {
        s.reconcile
            .as_ref()
            .map(|r| r.listing.clone())
            .unwrap_or_default()
    })?;
    let s = state.clone();
    let (target, wanted) = blocking(move || {
        let target = target(&s, device)?;
        // A file that might be content being hashed or made right now is
        // worth waiting for: the device tries again shortly, and the file
        // is adopted then rather than downloaded again. Content that isn't
        // on its way (a file that can't be read) doesn't hold a check up.
        if super::preparing_now(&s)
            && listing
                .iter()
                .any(|l| target.might_be_waiting(&l.path, l.size))
        {
            return Err(Problem::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "preparing",
                "The server is getting files ready; try again in a minute.",
            ));
        }
        Ok((target.clone(), to_sample(&target, &listing)))
    })
    .await?;
    let mut body = String::new();
    for (path, w) in &wanted {
        body.push_str(
            &serde_json::to_string(&SampleRequest {
                path: path.clone(),
                ranges: w.ranges.clone(),
            })
            .expect("requests serialize"),
        );
        body.push('\n');
    }
    state.sync.sessions.with(&id, device, |s| {
        if let Some(r) = s.reconcile.as_mut() {
            r.wanted = wanted.into_iter().collect();
            r.target = Some(target);
        }
    })?;
    Ok(ndjson(body))
}

/// The listed files worth sampling: those of a size some target content
/// has. The ranges for a size are the union of every such content's tag
/// areas and ends, so the sample tells the file apart from any of them.
fn to_sample(target: &Target, listing: &[Listed]) -> Vec<(String, Arc<Wanted>)> {
    use rayon::prelude::*;
    let mut by_size: HashMap<u64, Vec<&str>> = HashMap::new();
    for e in &target.entries {
        by_size.entry(e.size).or_default().push(&e.hash);
    }
    for hashes in by_size.values_mut() {
        hashes.sort_unstable();
        hashes.dedup();
    }
    let sizes: std::collections::HashSet<u64> = listing
        .iter()
        .map(|l| l.size)
        .filter(|s| by_size.contains_key(s))
        .collect();
    // For each size, the union of its contents' ranges, and the contents
    // those cover: one whose file couldn't be read isn't a candidate.
    let by_size: HashMap<u64, Arc<Wanted>> = sizes
        .into_par_iter()
        .filter_map(|size| {
            let (mut all, mut candidates) = (Vec::new(), Vec::new());
            for hash in &by_size[&size] {
                if let Some(found) =
                    source_file(target, hash).and_then(|(mut f, s)| ranges::ranges(&mut f, s).ok())
                {
                    all.extend(found);
                    candidates.push(hash.to_string());
                }
            }
            (!all.is_empty()).then(|| {
                let ranges = ranges::merge(all, size);
                (size, Arc::new(Wanted { ranges, candidates }))
            })
        })
        .collect();
    // Every file of a size shares its size's contents: a folder of many
    // files the same size mustn't cost their number squared.
    let mut out: Vec<(String, Arc<Wanted>)> = listing
        .iter()
        .filter_map(|l| Some((l.path.clone(), by_size.get(&l.size)?.clone())))
        .collect();
    out.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    out
}

/// A content's file, open, with its size, while it still holds the content.
fn source_file(target: &Target, hash: &str) -> Option<(File, u64)> {
    match target.source(hash)? {
        Source::Library {
            path,
            size,
            mtime_ns,
        } => {
            let f = File::open(path).ok()?;
            let meta = f.metadata().ok()?;
            (meta.len() == *size && coversync_core::hashes::mtime_ns(&meta).ok() == Some(*mtime_ns))
                .then_some((f, *size))
        }
        Source::Cover { path, size } => {
            let f = File::open(path).ok()?;
            (f.metadata().ok()?.len() == *size).then_some((f, *size))
        }
    }
}

/// `POST sessions/{s}/reconcile/samples`: the samples, in one request or
/// several, the last with `?last=1` (sent even when there was nothing to
/// sample). The last reply ends the reconcile with the new epoch.
async fn samples(
    State(state): State<Shared>,
    Extension(access): Extension<Access>,
    Path(id): Path<String>,
    Query(part): Query<Part>,
    body: Bytes,
) -> Answer {
    let device = device(&access)?;
    let sampled: Vec<Sampled> = lines(&body)?;
    // The work to do: each sample with its ranges and candidates.
    let (target, work, cache) = state.sync.sessions.with(&id, device, |s| {
        let r = s
            .reconcile
            .as_ref()
            .filter(|r| r.listed)
            .ok_or_else(|| Problem::bad("Send the whole listing first."))?;
        let target = r
            .target
            .clone()
            .ok_or_else(|| Problem::bad("No listing yet."))?;
        let work: Vec<(Sampled, Arc<Wanted>)> = sampled
            .into_iter()
            .filter_map(|x| {
                let w = r.wanted.get(&x.path)?.clone();
                Some((x, w))
            })
            .collect();
        Ok::<_, Problem>((target, work, r.samples.clone()))
    })??;
    let matched = blocking(move || {
        use rayon::prelude::*;
        // Each size's contents once, however many listed files share it,
        // and which sizes a file was sampled in (rather than hashed).
        let mut groups: HashMap<*const Wanted, &Wanted> = HashMap::new();
        let mut sampled_groups = std::collections::HashSet::new();
        for (x, w) in &work {
            groups.entry(Arc::as_ptr(w)).or_insert(w);
            if x.hash.is_none() {
                sampled_groups.insert(Arc::as_ptr(w));
            }
        }
        // Each candidate's own sample, read once however many listed files
        // it might be (a content's size, and so its ranges, are one), and
        // only for sizes some file was sampled in.
        let mut wanted: HashMap<&str, &Vec<[u64; 2]>> = HashMap::new();
        for (key, w) in &groups {
            if !sampled_groups.contains(key) {
                continue;
            }
            for c in &w.candidates {
                wanted.entry(c.as_str()).or_insert(&w.ranges);
            }
        }
        let known = cache.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let read: Vec<(String, String)> = wanted
            .into_par_iter()
            .filter(|(hash, _)| !known.contains_key(*hash))
            .filter_map(|(hash, ranges)| {
                let (mut f, size) = source_file(&target, hash)?;
                Some((
                    hash.to_string(),
                    coversync_protocol::sample(&mut f, size, ranges).ok()?,
                ))
            })
            .collect();
        let samples = {
            let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
            cache.extend(read);
            cache.clone()
        };
        // Each size's contents by sample, and by hash, looked up rather
        // than searched for every file. The first of two contents with one
        // sample wins, as the candidates are in order.
        let index: HashMap<*const Wanted, (HashMap<&str, &str>, std::collections::HashSet<&str>)> =
            groups
                .iter()
                .map(|(key, w)| {
                    let mut by_sample = HashMap::new();
                    for c in &w.candidates {
                        if let Some(sample) = samples.get(c.as_str()) {
                            by_sample.entry(sample.as_str()).or_insert(c.as_str());
                        }
                    }
                    let hashes = w.candidates.iter().map(String::as_str).collect();
                    (*key, (by_sample, hashes))
                })
                .collect();
        Ok(work
            .iter()
            .filter_map(|(x, w)| {
                let (by_sample, hashes) = &index[&Arc::as_ptr(w)];
                let hash = match (&x.hash, &x.sample) {
                    (Some(hash), _) => *hashes.get(hash.as_str())?,
                    (None, Some(sample)) => *by_sample.get(sample.as_str())?,
                    (None, None) => return None,
                };
                Some((x.path.clone(), hash.to_string()))
            })
            .collect::<Vec<_>>())
    })
    .await?;
    let finished = state.sync.sessions.with(&id, device, |s| {
        let r = s.reconcile.as_mut()?;
        r.matched.extend(matched);
        if part.last != Some(1) {
            return None;
        }
        let r = s.reconcile.take()?;
        Some(r)
    })?;
    let Some(r) = finished else {
        return Ok(ndjson(String::new()));
    };
    let target = r.target.clone().unwrap_or_default();
    let epoch = random_id("e_")?;
    let mut inventory = HashMap::new();
    let mut other = HashMap::new();
    let (mut adopted, mut moves, mut others) = (0, 0, Count::default());
    // What each content is of, for the files found to hold it.
    let mut owners: HashMap<&str, &plan::Entry> = HashMap::new();
    for e in &target.entries {
        owners.entry(e.hash.as_str()).or_insert(e);
    }
    for l in r.listing {
        match r.matched.get(&l.path) {
            Some(hash) => {
                let owner = target
                    .at(&l.path)
                    .filter(|e| &e.hash == hash)
                    .or_else(|| owners.get(hash.as_str()).copied());
                if target.at(&l.path).is_some_and(|e| &e.hash == hash) {
                    adopted += 1;
                } else {
                    moves += 1;
                }
                inventory.insert(
                    l.path,
                    HeldFile {
                        hash: hash.clone(),
                        size: l.size,
                        track: owner.and_then(|e| e.track),
                        album: owner.map(|e| e.album.clone()),
                    },
                );
            }
            None => {
                others.files += 1;
                others.bytes += l.size;
                other.insert(l.path, l.size);
            }
        }
    }
    let s = state.clone();
    let e = epoch.clone();
    blocking(move || Ok(s.sync.store.reconciled(device, &inventory, &other, &e)?)).await?;
    state.sync.changed();
    state.events.publish(Notice::DeviceSync { device });
    tracing::info!(
        device,
        adopted,
        moves,
        other = others.files,
        "reconciled a device's folder"
    );
    Ok(Json(ReconcileDone {
        epoch,
        adopted,
        moves,
        other: others,
    })
    .into_response())
}

#[derive(Deserialize)]
struct Cursor {
    #[serde(default)]
    after: Option<String>,
}

/// `GET sessions/{s}/plan`: a page of the plan. Without a cursor the plan
/// is worked out afresh, from the inventory as the device's facts left it.
async fn plan_page(
    State(state): State<Shared>,
    Extension(access): Extension<Access>,
    Path(id): Path<String>,
    Query(cursor): Query<Cursor>,
) -> Answer {
    let device = device(&access)?;
    let (pending_reconcile, snapshot, free_bytes) = state.sync.sessions.with(&id, device, |s| {
        (s.reconcile.is_some(), s.plan.clone(), s.free_bytes)
    })?;
    if pending_reconcile {
        return Err(Problem::new(
            StatusCode::CONFLICT,
            "reconcile_first",
            "Send the folder's listing first.",
        ));
    }
    let start = match cursor.after.as_deref() {
        None => 0,
        Some(c) => c
            .strip_prefix('c')
            .and_then(|n| n.parse::<usize>().ok())
            .ok_or_else(|| Problem::bad("That isn't a plan cursor."))?,
    };
    let snapshot = match (cursor.after, snapshot) {
        (Some(_), Some(snapshot)) => snapshot,
        (Some(_), None) => return Err(Problem::no_session()),
        (None, _) => {
            let s = state.clone();
            let snapshot = blocking(move || work_out(&s, device, free_bytes)).await?;
            let snapshot = Arc::new(snapshot);
            state
                .sync
                .sessions
                .with(&id, device, |s| s.plan = Some(snapshot.clone()))?;
            state.events.publish(Notice::DeviceSync { device });
            snapshot
        }
    };
    let mut body = PlanLine::Header(snapshot.header.clone()).to_line();
    body.push('\n');
    let mut end = start.min(snapshot.ops.len());
    while end < snapshot.ops.len() && end - start < PAGE_OPS && body.len() < PAGE_BYTES {
        body.push_str(&PlanLine::Op(snapshot.ops[end].clone()).to_line());
        body.push('\n');
        end += 1;
    }
    let next = (end < snapshot.ops.len()).then(|| format!("c{end}"));
    body.push_str(&PlanLine::End(PlanEnd { next }).to_line());
    body.push('\n');
    Ok(ndjson(body))
}

/// A device's plan from its target and its files as the server knows them.
pub(super) fn work_out(state: &Shared, device: i64, free_bytes: Option<u64>) -> Answer<Snapshot> {
    let target = target(state, device)?;
    let mut files = state.sync.store.files(device)?;
    let holds_files = !files.inventory.is_empty();
    // Paths an earlier build took that the rules now refuse: left alone,
    // as the device couldn't report doing anything with them.
    files.inventory.retain(|path, _| valid_path(path));
    files.other.retain(|path, _| valid_path(path));
    let st = state.sync.store.device(device)?;
    // A file already where it should be takes the owner the target gives
    // it now: an album retagged keeps its cover's path and bytes, but not
    // its id, which waiting for a new cover goes by.
    let owners: Vec<(String, HeldFile, Option<i64>, String)> = files
        .inventory
        .iter()
        .filter_map(|(path, held)| {
            let e = target.at(path).filter(|e| e.hash == held.hash)?;
            (held.track != e.track || held.album.as_deref() != Some(e.album.as_str()))
                .then(|| (path.clone(), held.clone(), e.track, e.album.clone()))
        })
        .collect();
    if !owners.is_empty() {
        state.sync.store.set_owners(device, &owners)?;
        for (path, _, track, album) in owners {
            if let Some(held) = files.inventory.get_mut(&path) {
                held.track = track;
                held.album = Some(album);
            }
        }
    }
    // A Library with nothing in it, nothing on its way and a device holding
    // files is far likelier a mistake (an empty folder chosen, a disk not
    // mounted) than a wish to empty the device: plan nothing.
    let empty = target.entries.is_empty() && target.preparing == 0 && holds_files;
    if empty {
        tracing::warn!(
            device,
            "the Library is empty: planning nothing rather than emptying the device"
        );
    }
    let plan = if empty {
        plan::Plan::default()
    } else {
        plan::plan(
            &target.entries,
            target.preparing,
            &plan::Device {
                inventory: &files.inventory,
                other: &files.other,
                held_back: &files.held_back,
                remove_other: st.remove_other,
                free_bytes,
                keep: &|path, held| target.waits(path, held),
                fold_case: st.capabilities.case_insensitive,
            },
        )
    };
    Ok(Snapshot {
        header: PlanHeader {
            target: target.version.clone(),
            totals: plan.totals,
        },
        target,
        ops: plan.ops,
    })
}

/// `POST sessions/{s}/facts`: what the device did. Accepted from a device's
/// earlier session too: facts are facts.
async fn facts(
    State(state): State<Shared>,
    Extension(access): Extension<Access>,
    Path(id): Path<String>,
    Json(batch): Json<FactsBatch>,
) -> Answer<Json<FactsReply>> {
    let device = device(&access)?;
    let paths_ok = batch.done.iter().all(|f| match f {
        coversync_protocol::Fact::Put { path, .. }
        | coversync_protocol::Fact::Delete { path, .. } => valid_path(path),
        coversync_protocol::Fact::Move { from, path, .. } => valid_path(from) && valid_path(path),
    });
    if !paths_ok {
        return Err(Problem::bad("A fact names a path outside the sync folder."));
    }
    let planned = state
        .sync
        .sessions
        .with(&id, device, |s| {
            if let Some(p) = &batch.progress {
                s.progress = p.clone();
            }
            if batch.free_bytes.is_some() {
                s.free_bytes = batch.free_bytes;
            }
            let notice = s.noticed.is_none_or(|t| t.elapsed() >= PROGRESS_NOTICE);
            if notice {
                s.noticed = Some(Instant::now());
            }
            (s.plan.clone(), notice)
        })
        .ok();
    let s = state.clone();
    let snapshot = planned.as_ref().and_then(|(p, _)| p.clone());
    let target = blocking(move || {
        // What each put's content is of: from the target now, or the one
        // the plan was made from.
        let now_target = target(&s, device).ok();
        let of = |path: &str, hash: &str| {
            now_target
                .as_ref()
                .and_then(|t| t.of(path, hash))
                .or_else(|| snapshot.as_ref().and_then(|p| p.target.of(path, hash)))
        };
        s.sync
            .store
            .apply_facts(device, &batch.done, &batch.failed, of, now())?;
        Ok(target(&s, device)?)
    })
    .await?;
    let planned_target = planned
        .as_ref()
        .and_then(|(p, _)| p.as_ref())
        .map(|p| p.target.version.clone());
    if planned.as_ref().is_some_and(|(_, notice)| *notice) {
        state.events.publish(Notice::DeviceSync { device });
    }
    Ok(Json(FactsReply {
        changed: planned_target.as_deref() != Some(target.version.as_str()),
        target: target.version.clone(),
    }))
}

/// `POST sessions/{s}/end`: the device's report; the session is over.
async fn end(
    State(state): State<Shared>,
    Extension(access): Extension<Access>,
    Path(id): Path<String>,
    Json(report): Json<EndReport>,
) -> Answer<StatusCode> {
    let device = device(&access)?;
    let session = {
        let mut sessions = state.sync.sessions.lock();
        match sessions.get(&id) {
            Some(s) if s.device == device => sessions.remove(&id),
            _ => None,
        }
    }
    .ok_or_else(Problem::no_session)?;
    let last = LastSession {
        started_at: session.started_at,
        ended_at: now(),
        reason: session.reason,
        report,
    };
    tracing::info!(device, session = id, result = ?last.report.result, files = last.report.files,
        bytes = last.report.bytes, "sync session ended");
    let s = state.clone();
    blocking(move || Ok(s.sync.store.set_last(device, &last)?)).await?;
    state.events.publish(Notice::DeviceSync { device });
    Ok(StatusCode::NO_CONTENT)
}

/// `GET status`: whether there's anything to sync, cheaply: 304 when
/// nothing changed since the device last asked.
async fn status(
    State(state): State<Shared>,
    Extension(access): Extension<Access>,
    headers: HeaderMap,
) -> Answer {
    let device = device(&access)?;
    let s = state.clone();
    let status = blocking(move || {
        let snapshot = work_out(&s, device, None)?;
        let st = s.sync.store.device(device)?;
        let auto = s
            .auth
            .device_sync(device)?
            .map(|d| d.settings.auto)
            .unwrap_or_default();
        let t = &snapshot.header.totals;
        Ok(Status {
            target: snapshot.target.version.clone(),
            changed_at: st.changed_at,
            pending: Count {
                files: t.put + t.moves + t.delete,
                bytes: t.put_bytes,
            },
            preparing: t.preparing,
            requested: st.sync_requested,
            auto,
            last: st.last.map(|l| LastSync {
                at: l.ended_at,
                result: l.report.result,
            }),
        })
    })
    .await?;
    let body = serde_json::to_vec(&status).expect("status serializes");
    let etag = format!("\"{}\"", &blake3::hash(&body).to_hex()[..32]);
    let unchanged = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag));
    let mut res = if unchanged {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        ([(header::CONTENT_TYPE, "application/json")], body).into_response()
    };
    res.headers_mut().insert(
        header::ETAG,
        HeaderValue::from_str(&etag).expect("etags are ascii"),
    );
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    Ok(res)
}

#[derive(Deserialize)]
struct Heartbeat {
    #[serde(default)]
    heartbeat: Option<u32>,
}

/// `GET events`: `target` when the device's target changes (and once at
/// the start), `sync` when someone asks for a sync in Sync setup, and
/// `settings` (an `AutoSync`) when its automatic-sync settings change; it
/// gets them at first from `GET status`. It ends once the token no longer
/// names the device, as checked every [`STILL_DEVICE`].
async fn events(
    State(state): State<Shared>,
    Extension(access): Extension<Access>,
    Query(q): Query<Heartbeat>,
    headers: HeaderMap,
) -> Answer {
    let device = device(&access)?;
    let heartbeat = q
        .heartbeat
        .unwrap_or(HEARTBEAT_SECONDS)
        .clamp(*HEARTBEAT_RANGE.start(), *HEARTBEAT_RANGE.end());
    struct Listening {
        state: Shared,
        changes: tokio::sync::watch::Receiver<u64>,
        requests: tokio::sync::broadcast::Receiver<i64>,
        closed: tokio::sync::watch::Receiver<bool>,
        sent: Option<String>,
        auto: Option<coversync_protocol::AutoSync>,
        queued: Option<Event>,
        first: bool,
    }
    let ended = {
        let state = state.clone();
        async move {
            loop {
                tokio::time::sleep(STILL_DEVICE).await;
                if !crate::auth::still_device(&state, &headers, device).await {
                    return;
                }
            }
        }
    };
    let listening = Listening {
        changes: state.sync.changes.subscribe(),
        requests: state.sync.requests.subscribe(),
        closed: state.events.closed(),
        state: state.clone(),
        sent: None,
        auto: None,
        queued: None,
        first: true,
    };
    let events = stream::unfold(listening, move |mut l| async move {
        loop {
            if let Some(event) = l.queued.take() {
                return Some((Ok::<_, Infallible>(event), l));
            }
            enum Woke {
                Changed,
                Asked,
                Other,
                Stop,
            }
            if !l.first {
                let woke = tokio::select! {
                    changed = l.changes.changed() => match changed {
                        Ok(()) => Woke::Changed,
                        Err(_) => Woke::Stop,
                    },
                    request = l.requests.recv() => match request {
                        Ok(d) if d == device => Woke::Asked,
                        Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => Woke::Other,
                        Err(_) => Woke::Stop,
                    },
                    _ = l.closed.wait_for(|c| *c) => Woke::Stop,
                };
                match woke {
                    Woke::Changed => {}
                    Woke::Other => continue,
                    Woke::Stop => return None,
                    Woke::Asked => {
                        let event = Event::default().event("sync").data("{}");
                        return Some((Ok::<_, Infallible>(event), l));
                    }
                }
            }
            l.first = false;
            let s = l.state.clone();
            let found = tokio::task::spawn_blocking(move || {
                let target = target(&s, device).ok()?;
                let changed_at = s.sync.store.device(device).ok()?.changed_at;
                let auto = s.auth.device_sync(device).ok()??.settings.auto;
                Some((
                    TargetEvent {
                        target: target.version.clone(),
                        changed_at,
                    },
                    auto,
                ))
            })
            .await
            .ok()
            .flatten();
            let Some((found, auto)) = found else { continue };
            // When the device syncs by itself can change while its files
            // don't: it hears of that too (not at the start, as `status`
            // has it).
            if l.auto.is_some_and(|a| a != auto) {
                l.queued = Some(
                    Event::default()
                        .event("settings")
                        .data(serde_json::to_string(&auto).expect("events serialize")),
                );
            }
            l.auto = Some(auto);
            if l.sent.as_ref() == Some(&found.target) {
                continue;
            }
            l.sent = Some(found.target.clone());
            let event = Event::default()
                .event("target")
                .data(serde_json::to_string(&found).expect("events serialize"));
            return Some((Ok(event), l));
        }
    });
    Ok((
        [(header::HeaderName::from_static("x-accel-buffering"), "no")],
        Sse::new(events.take_until(ended)).keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(heartbeat.into()))
                .text("heartbeat"),
        ),
    )
        .into_response())
}

/// `GET content/{hash}`: a content's bytes, with `Range`. Served only while
/// the hash is in the device's target and the file still holds it.
async fn content(
    State(state): State<Shared>,
    Extension(access): Extension<Access>,
    Path(hash): Path<String>,
    headers: HeaderMap,
) -> Answer {
    let device = device(&access)?;
    let s = state.clone();
    let h = hash.clone();
    let opened = blocking(move || {
        let target = target(&s, device)?;
        if target.source(&h).is_none() {
            return Ok(None);
        }
        let opened = source_file(&target, &h);
        match target.source(&h) {
            // The file changed since it was indexed: a scan reads it again,
            // and the hash pass after it hashes it.
            Some(Source::Library { .. }) if opened.is_none() => crate::spawn_scan(s.clone()),
            // The cover file has gone from the cache: it's made again once
            // the target is worked out afresh.
            Some(Source::Cover { .. }) if opened.is_none() => s.sync.forget(device),
            _ => {}
        }
        Ok(opened)
    })
    .await?;
    let Some((mut file, size)) = opened else {
        return Err(Problem::new(
            StatusCode::GONE,
            "gone",
            "That content isn't this device's any more; ask for a new plan.",
        ));
    };
    let etag = format!("\"{hash}\"");
    let range = match headers.get(header::RANGE).and_then(|v| v.to_str().ok()) {
        // A resumed download of other bytes gets the whole file.
        Some(_)
            if headers
                .get(header::IF_RANGE)
                .is_some_and(|v| v.as_bytes() != etag.as_bytes()) =>
        {
            None
        }
        Some(r) => match byte_range(r, size) {
            Some(range) => Some(range),
            None => {
                let mut res = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
                res.headers_mut().insert(
                    header::CONTENT_RANGE,
                    HeaderValue::from_str(&format!("bytes */{size}")).expect("ascii"),
                );
                return Ok(res);
            }
        },
        None => None,
    };
    let (start, end) = range.unwrap_or((0, size));
    file.seek(SeekFrom::Start(start)).map_err(Problem::failed)?;
    let file = tokio::fs::File::from_std(file);
    let body = Body::from_stream(stream::unfold(
        (file, end - start),
        |(mut file, left)| async move {
            use tokio::io::AsyncReadExt;
            if left == 0 {
                return None;
            }
            // Bounded before the cast, which would wrap on a 32-bit build.
            let mut buf = vec![0; left.min(READ_CHUNK as u64) as usize];
            match file.read(&mut buf).await {
                Ok(0) => Some((
                    Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "the file got shorter",
                    )),
                    (file, 0),
                )),
                Ok(n) => {
                    buf.truncate(n);
                    Some((Ok(Bytes::from(buf)), (file, left - n as u64)))
                }
                Err(e) => Some((Err(e), (file, 0))),
            }
        },
    ));
    let mut res = Response::new(body);
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(end - start));
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    h.insert(
        header::ETAG,
        HeaderValue::from_str(&etag).map_err(Problem::failed)?,
    );
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=31536000, immutable"),
    );
    if range.is_some() {
        *res.status_mut() = StatusCode::PARTIAL_CONTENT;
        res.headers_mut().insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes {start}-{}/{size}", end - 1)).expect("ascii"),
        );
    }
    Ok(res)
}

/// One byte range (`bytes=a-b`, `bytes=a-` or `bytes=-n`) of a file of
/// `size` bytes, as `[start, end)`; None if it can't be served.
fn byte_range(header: &str, size: u64) -> Option<(u64, u64)> {
    let spec = header.trim().strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None;
    }
    let (a, b) = spec.split_once('-')?;
    let (start, end) = match (a.trim(), b.trim()) {
        ("", n) => {
            let n: u64 = n.parse().ok()?;
            (size.saturating_sub(n), size)
        }
        (a, "") => (a.parse().ok()?, size),
        (a, b) => (
            a.parse().ok()?,
            b.parse::<u64>().ok()?.saturating_add(1).min(size),
        ),
    };
    (start < end && start < size).then_some((start, end))
}

/// `GET inventory`: what the server believes the device holds, in pages of
/// NDJSON, the last line a [`PlanEnd`] with the next cursor.
async fn inventory(
    State(state): State<Shared>,
    Extension(access): Extension<Access>,
    Query(cursor): Query<Cursor>,
) -> Answer {
    let device = device(&access)?;
    let s = state.clone();
    blocking(move || {
        let target = target(&s, device).ok();
        let mut held: Vec<(String, HeldFile)> =
            s.sync.store.files(device)?.inventory.into_iter().collect();
        held.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        let start = match cursor.after.as_deref() {
            None => 0,
            Some(after) => held.partition_point(|(p, _)| p.as_str() <= after),
        };
        let page = &held[start.min(held.len())..(start + PAGE_OPS).min(held.len())];
        let mut body = String::new();
        for (path, h) in page {
            let entry = target
                .as_ref()
                .and_then(|t| t.at(path))
                .filter(|e| e.hash == h.hash);
            let line = Held {
                path: path.clone(),
                hash: h.hash.clone(),
                size: h.size,
                track: entry.and_then(|e| e.track),
                album: entry.map(|e| e.album.clone()),
            };
            body.push_str(&serde_json::to_string(&line).expect("inventory serializes"));
            body.push('\n');
        }
        let next = (start + page.len() < held.len())
            .then(|| page.last().map(|(p, _)| p.clone()))
            .flatten();
        body.push_str(&PlanLine::End(PlanEnd { next }).to_line());
        body.push('\n');
        Ok(ndjson(body))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_parse_as_http_has_them() {
        assert_eq!(byte_range("bytes=0-9", 100), Some((0, 10)));
        assert_eq!(byte_range("bytes=90-", 100), Some((90, 100)));
        assert_eq!(byte_range("bytes=-10", 100), Some((90, 100)));
        assert_eq!(byte_range("bytes=50-500", 100), Some((50, 100)));
        assert_eq!(byte_range("bytes=100-", 100), None);
        assert_eq!(byte_range("bytes=0-1,5-6", 100), None);
    }
}
