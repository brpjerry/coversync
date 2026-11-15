//! A plain sync client, for trying the sync API against a server and timing
//! it: it syncs a folder the way the spec says a device does, with several
//! downloads at once. Not the engine (SYN-05): no events, no free-space
//! checks, no cancelling.
//!
//! ```text
//! cargo run --release -p coversync-protocol --example sync_client -- \
//!     --server http://127.0.0.1:8770 --token-file TOKEN --root DIR [--jobs 4] [--full-hashes]
//! ```
//!
//! The token is read from a file, so it never shows in a process list.

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use coversync_protocol::{
    CLIENT_DIR, EndReport, Fact, FactsBatch, FactsReply, Failure, Listed, Marker, Op, Outcome,
    PlanLine, Reason, ReconcileDone, SampleRequest, Sampled, SessionStart, SessionStarted, errors,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

struct Args {
    server: String,
    token: String,
    root: PathBuf,
    jobs: usize,
    full_hashes: bool,
}

fn args() -> Result<Args> {
    let mut server = None;
    let mut token_file = None;
    let mut root = None;
    let mut jobs = 4;
    let mut full_hashes = false;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--server" => server = it.next(),
            "--token-file" => token_file = it.next(),
            "--root" => root = it.next(),
            "--jobs" => jobs = it.next().ok_or("--jobs needs a number")?.parse()?,
            "--full-hashes" => full_hashes = true,
            other => return Err(format!("unknown argument {other}").into()),
        }
    }
    let token = fs::read_to_string(token_file.ok_or("--token-file is required")?)?
        .trim()
        .to_string();
    Ok(Args {
        server: server
            .ok_or("--server is required")?
            .trim_end_matches('/')
            .to_string(),
        token,
        root: root.ok_or("--root is required")?.into(),
        jobs: jobs.max(1),
        full_hashes,
    })
}

struct Api {
    agent: ureq::Agent,
    base: String,
    auth: String,
}

impl Api {
    fn get(&self, path: &str) -> Result<ureq::http::Response<ureq::Body>> {
        Ok(self
            .agent
            .get(format!("{}{path}", self.base))
            .header("Authorization", &self.auth)
            .call()?)
    }

    fn post(&self, path: &str, body: &[u8]) -> Result<Vec<u8>> {
        let mut res = self
            .agent
            .post(format!("{}{path}", self.base))
            .header("Authorization", &self.auth)
            .header("Content-Type", "application/json")
            .send(body)?;
        let status = res.status();
        let body = res.body_mut().read_to_vec()?;
        if !status.is_success() {
            return Err(format!("POST {path}: {status} {}", String::from_utf8_lossy(&body)).into());
        }
        Ok(body)
    }

    fn post_json<T: serde::Serialize, R: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &T,
    ) -> Result<R> {
        Ok(serde_json::from_slice(
            &self.post(path, &serde_json::to_vec(body)?)?,
        )?)
    }
}

fn ndjson<T: serde::Serialize>(items: &[T]) -> Vec<u8> {
    let mut out = Vec::new();
    for i in items {
        serde_json::to_writer(&mut out, i).expect("lines serialize");
        out.push(b'\n');
    }
    out
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<Listed>) -> Result<()> {
    for e in fs::read_dir(dir)? {
        let e = e?;
        let path = e.path();
        let rel = path.strip_prefix(root)?.to_string_lossy().into_owned();
        if rel == CLIENT_DIR {
            continue;
        }
        let kind = e.file_type()?;
        if kind.is_dir() {
            walk(root, &path, out)?;
        } else if kind.is_file() {
            out.push(Listed {
                path: rel,
                size: e.metadata()?.len(),
            });
        }
    }
    Ok(())
}

fn reconcile(api: &Api, base: &str, root: &Path, full_hashes: bool) -> Result<ReconcileDone> {
    let mut listing = Vec::new();
    walk(root, root, &mut listing)?;
    let wanted = api.post(&format!("{base}/reconcile?last=1"), &ndjson(&listing))?;
    let wanted: Vec<SampleRequest> = wanted
        .split(|&b| b == b'\n')
        .filter(|l| !l.is_empty())
        .map(serde_json::from_slice)
        .collect::<std::result::Result<_, _>>()?;
    let mut sampled = Vec::with_capacity(wanted.len());
    for w in &wanted {
        let path = root.join(&w.path);
        let mut f = File::open(&path)?;
        let size = f.metadata()?.len();
        sampled.push(if full_hashes {
            let mut h = blake3::Hasher::new();
            h.update_reader(&mut f)?;
            Sampled {
                path: w.path.clone(),
                sample: None,
                hash: Some(h.finalize().to_hex().to_string()),
            }
        } else {
            Sampled {
                path: w.path.clone(),
                sample: Some(coversync_protocol::sample(&mut f, size, &w.ranges)?),
                hash: None,
            }
        });
    }
    let done = api.post(
        &format!("{base}/reconcile/samples?last=1"),
        &ndjson(&sampled),
    )?;
    eprintln!(
        "reconciled: {} listed, {} sampled",
        listing.len(),
        wanted.len()
    );
    Ok(serde_json::from_slice(&done)?)
}

/// Download one content into its partial file, hashing as it arrives, then
/// flush it and rename it into place.
fn put(api: &Api, root: &Path, path: &str, hash: &str, size: u64, bytes: &AtomicU64) -> Result<()> {
    let partial = root.join(CLIENT_DIR).join("partial").join(hash);
    let have = fs::metadata(&partial)
        .map(|m| m.len())
        .unwrap_or(0)
        .min(size);
    let mut req = api
        .agent
        .get(format!("{}/api/sync/v1/content/{hash}", api.base))
        .header("Authorization", &api.auth);
    if have > 0 {
        req = req
            .header("Range", format!("bytes={have}-"))
            .header("If-Range", format!("\"{hash}\""));
    }
    let mut res = req.call()?;
    let status = res.status();
    if status == 410 {
        return Err(errors::GONE.into());
    }
    if !status.is_success() {
        return Err(format!("GET content: {status}").into());
    }
    let resumed = status == 206;
    let mut hasher = blake3::Hasher::new();
    let mut out = if resumed {
        let mut f = File::open(&partial)?;
        hasher.update_reader(&mut f)?;
        OpenOptions::new().append(true).open(&partial)?
    } else {
        File::create(&partial)?
    };
    let mut body = res.body_mut().with_config().limit(u64::MAX).reader();
    let mut buf = vec![0; 1 << 20];
    loop {
        let n = body.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        out.write_all(&buf[..n])?;
        bytes.fetch_add(n as u64, Ordering::Relaxed);
    }
    out.sync_all()?;
    if hasher.finalize().to_hex().as_str() != hash {
        let _ = fs::remove_file(&partial);
        return Err(format!("{path}: the bytes don't match their hash").into());
    }
    let dest = root.join(path);
    if fs::rename(&partial, &dest).is_err() {
        let _ = fs::remove_file(&dest);
        fs::rename(&partial, &dest)?;
    }
    Ok(())
}

fn main() -> Result<()> {
    let a = args()?;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_idle_connections_per_host(a.jobs + 2)
        .build()
        .into();
    let api = Arc::new(Api {
        agent,
        base: a.server.clone(),
        auth: format!("Bearer {}", a.token),
    });
    let root = a.root.clone();
    fs::create_dir_all(root.join(CLIENT_DIR).join("partial"))?;
    let marker_path = root.join(CLIENT_DIR).join("device.json");
    let marker: Option<Marker> = fs::read(&marker_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());
    let began = Instant::now();
    let started: SessionStarted = api.post_json(
        "/api/sync/v1/sessions",
        &SessionStart {
            marker,
            reason: Reason::User,
            ..Default::default()
        },
    )?;
    let base = format!("/api/sync/v1/sessions/{}", started.session);
    if started.reconcile {
        let t = Instant::now();
        let done = reconcile(&api, &base, &root, a.full_hashes)?;
        eprintln!("{done:?} in {:.2?}", t.elapsed());
        let mut session = api.get("/api/session")?;
        let session: serde_json::Value =
            serde_json::from_slice(&session.body_mut().read_to_vec()?)?;
        let device = session["device"].as_i64().ok_or("not a device token")?;
        fs::write(
            &marker_path,
            serde_json::to_vec(&Marker {
                device,
                epoch: done.epoch,
            })?,
        )?;
    }

    // The whole plan, page by page.
    let t = Instant::now();
    let mut ops = Vec::new();
    let mut after: Option<String> = None;
    let mut header = None;
    loop {
        let url = match &after {
            None => format!("{base}/plan"),
            Some(c) => format!("{base}/plan?after={c}"),
        };
        let mut res = api.get(&url)?;
        let body = res.body_mut().read_to_vec()?;
        if !res.status().is_success() {
            return Err(format!("plan: {}", String::from_utf8_lossy(&body)).into());
        }
        let mut next = None;
        for line in body.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
            match PlanLine::parse(line)? {
                PlanLine::Header(h) => header = Some(h),
                PlanLine::Op(op) => ops.push(op),
                PlanLine::End(e) => next = e.next,
            }
        }
        match next {
            Some(c) => after = Some(c),
            None => break,
        }
    }
    eprintln!(
        "plan: {:?}, read in {:.2?}",
        header.map(|h| h.totals),
        t.elapsed()
    );

    let facts = Arc::new(Mutex::new(Vec::<Fact>::new()));
    let failed = Arc::new(Mutex::new(Vec::<Failure>::new()));
    let send = |final_batch: bool| -> Result<bool> {
        let done = std::mem::take(&mut *facts.lock().unwrap());
        let failed = std::mem::take(&mut *failed.lock().unwrap());
        if done.is_empty() && failed.is_empty() && !final_batch {
            return Ok(false);
        }
        let reply: FactsReply = api.post_json(
            &format!("{base}/facts"),
            &FactsBatch {
                done,
                failed,
                progress: None,
                free_bytes: None,
            },
        )?;
        Ok(reply.changed)
    };

    // Everything but puts, in order.
    let at = |p: &str| root.join(p);
    let mut puts = VecDeque::new();
    for op in ops {
        match op {
            Op::Delete { path, size } => {
                if fs::metadata(at(&path)).is_ok_and(|m| m.len() == size) {
                    fs::remove_file(at(&path))?;
                }
                facts.lock().unwrap().push(Fact::Delete { path });
            }
            Op::Mkdir { path } => fs::create_dir_all(at(&path))?,
            Op::Move {
                from, path, size, ..
            } => {
                if fs::metadata(at(&from)).is_ok() {
                    fs::rename(at(&from), at(&path))?;
                } else if !fs::metadata(at(&path)).is_ok_and(|m| m.len() == size) {
                    continue;
                }
                facts.lock().unwrap().push(Fact::Move { from, path });
            }
            Op::Rmdir { path } => {
                let _ = fs::remove_dir(at(&path));
            }
            Op::Put {
                path, hash, size, ..
            } => puts.push_back((path, hash, size)),
        }
    }
    send(false)?;

    // Puts, several at once, with facts sent every 2 seconds.
    let total: u64 = puts.iter().map(|p| p.2).sum();
    let count = puts.len();
    let queue = Arc::new(Mutex::new(puts));
    let bytes = Arc::new(AtomicU64::new(0));
    let finished = AtomicBool::new(false);
    let t = Instant::now();
    std::thread::scope(|scope| -> Result<()> {
        let mut workers = Vec::new();
        for _ in 0..a.jobs {
            let (queue, api, facts, failed, bytes, root) =
                (&queue, &api, &facts, &failed, &bytes, &root);
            workers.push(scope.spawn(move || {
                loop {
                    let Some((path, hash, size)) = queue.lock().unwrap().pop_front() else {
                        return;
                    };
                    match put(api, root, &path, &hash, size, bytes) {
                        Ok(()) => facts.lock().unwrap().push(Fact::Put { path, hash }),
                        Err(e) => {
                            eprintln!("put {path}: {e}");
                            failed.lock().unwrap().push(Failure {
                                op: "put".into(),
                                path,
                                hash: Some(hash),
                                error: if e.to_string() == errors::GONE {
                                    errors::GONE.into()
                                } else {
                                    errors::IO.into()
                                },
                                detail: Some(e.to_string()),
                            });
                        }
                    }
                }
            }));
        }
        let finished = &finished;
        let reporter = scope.spawn(|| -> Result<()> {
            let mut last = Instant::now();
            while !finished.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(100));
                if last.elapsed() < Duration::from_secs(2) {
                    continue;
                }
                last = Instant::now();
                send(false)?;
                let done = bytes.load(Ordering::Relaxed);
                let secs = t.elapsed().as_secs_f64();
                eprintln!(
                    "{:.1} of {:.1} GB, {:.0} MB/s",
                    done as f64 / 1e9,
                    total as f64 / 1e9,
                    done as f64 / 1e6 / secs.max(0.001)
                );
            }
            Ok(())
        });
        for w in workers {
            w.join().expect("download threads don't panic");
        }
        finished.store(true, Ordering::Relaxed);
        reporter.join().expect("the reporter doesn't panic")
    })?;
    let changed = send(true)?;
    let secs = t.elapsed().as_secs_f64();
    let moved = bytes.load(Ordering::Relaxed);
    eprintln!(
        "{count} files, {:.2} GB in {secs:.1} s: {:.0} MB/s{}",
        moved as f64 / 1e9,
        moved as f64 / 1e6 / secs.max(0.001),
        if changed {
            " (the Library changed meanwhile: sync again)"
        } else {
            ""
        }
    );
    let report = EndReport {
        result: Outcome::Complete,
        message: None,
        files: count as u64,
        bytes: moved,
        seconds: began.elapsed().as_secs(),
        free_bytes: None,
    };
    api.post(&format!("{base}/end"), &serde_json::to_vec(&report)?)?;
    Ok(())
}
