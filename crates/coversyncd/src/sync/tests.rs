//! The sync API end to end: a scripted client syncs a folder against the
//! whole app, as it's served, over a scratch Library.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, BodyDataStream};
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use coversync_core::LibrarySetup;
use coversync_core::sync::DeviceSettings;
use coversync_protocol::{
    CLIENT_DIR, EndReport, Fact, FactsBatch, Listed, Marker, Op, Outcome, PlanLine, ReconcileDone,
    SampleRequest, Sampled, SessionStart, SessionStarted,
};
use futures_util::StreamExt;
use tower::ServiceExt;

use crate::Shared;
use crate::auth::store::Scope;

/// An 8×6 PNG.
const PNG: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 8, 0, 0, 0, 6, 8, 2, 0,
    0, 0, 113, 103, 72, 172, 0, 0, 0, 17, 73, 68, 65, 84, 120, 156, 99, 56, 161, 161, 129, 21, 49,
    12, 164, 4, 0, 128, 247, 52, 129, 69, 202, 34, 253, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96,
    130,
];

/// A FLAC file with these Vorbis comments and `audio` bytes standing in
/// for its frames.
fn flac(path: &Path, comments: &[&str], audio: usize) {
    let mut b = b"fLaC".to_vec();
    b.extend([0, 0, 0, 34, 0x10, 0x00, 0x10, 0x00, 0, 0, 0, 0, 0, 0]);
    let packed: u64 = (44100 << 44) | (1 << 41) | (15 << 36);
    b.extend(packed.to_be_bytes());
    b.extend([0u8; 16]);
    let mut block = Vec::new();
    block.extend(9u32.to_le_bytes());
    block.extend(b"coversync");
    block.extend((comments.len() as u32).to_le_bytes());
    for c in comments {
        block.extend((c.len() as u32).to_le_bytes());
        block.extend(c.as_bytes());
    }
    b.push(0x80 | 4);
    b.extend(&(block.len() as u32).to_be_bytes()[1..]);
    b.extend(block);
    b.extend((0..audio).map(|i| (i % 251) as u8 | 1));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, b).unwrap();
}

pub(super) fn tags(title: &str, artist: &str, album: &str, n: u32) -> Vec<String> {
    vec![
        format!("TITLE={title}"),
        format!("ARTIST={artist}"),
        format!("ALBUMARTIST={artist}"),
        format!("ALBUM={album}"),
        format!("TRACKNUMBER={n}"),
    ]
}

pub(super) fn write_track(path: &Path, tags: &[String], audio: usize) {
    let tags: Vec<&str> = tags.iter().map(String::as_str).collect();
    flac(path, &tags, audio);
}

pub(super) struct Daemon {
    pub(super) dir: PathBuf,
    pub(super) library: PathBuf,
    pub(super) state: Shared,
    pub(super) router: Router,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Daemon {
    /// A daemon over a Library of two albums, the first with a cover image.
    pub(super) fn new(name: &str) -> Daemon {
        let dir =
            std::env::temp_dir().join(format!("coversyncd-sync-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let library = dir.join("library");
        let one = library.join("Artist A/Album One");
        write_track(
            &one.join("01 - One.flac"),
            &tags("One", "A", "Album One", 1),
            50_000,
        );
        write_track(
            &one.join("02 - Two.flac"),
            &tags("Two", "A", "Album One", 2),
            60_000,
        );
        std::fs::write(one.join("cover.png"), PNG).unwrap();
        let three = library.join("Artist B/Album Two/01 - Three.flac");
        write_track(&three, &tags("Three", "B", "Album Two", 1), 70_000);
        let state = crate::test_state(&dir);
        state
            .lib
            .set_library_setup(LibrarySetup {
                folder: Some(library.display().to_string()),
                ..Default::default()
            })
            .unwrap();
        let d = Daemon {
            router: crate::app(state.clone()),
            dir,
            library,
            state,
        };
        d.rescan();
        d
    }

    /// Index and hash the Library as it is now.
    pub(super) fn rescan(&self) {
        self.scan();
        coversync_core::hashes::run(&self.state.lib, &self.state.hashes).unwrap();
    }

    /// Index the Library as it is now, leaving changed files unhashed.
    pub(super) fn scan(&self) {
        let s = &self.state;
        s.lib.refresh_library().unwrap();
        coversync_core::scan::scan(&s.lib, &s.covers, &s.roots(), &s.progress).unwrap();
    }

    pub(super) fn device(&self, scope: Scope) -> (i64, String) {
        self.state
            .auth
            .store()
            .add_device("Test device", scope, crate::sync::now())
            .unwrap()
    }

    pub(super) fn settings(&self, device: i64, settings: DeviceSettings) {
        self.state
            .auth
            .store()
            .set_device_sync(device, &settings.normalize().unwrap())
            .unwrap();
        self.state.sync.changed();
    }

    /// The Library's files, by path, as a device following its structure
    /// holds them.
    pub(super) fn files(&self) -> BTreeMap<String, Vec<u8>> {
        tree(&self.library)
            .into_iter()
            .filter(|(p, _)| !p.starts_with(".coversync"))
            .collect()
    }

    pub(super) async fn call(
        &self,
        method: &str,
        path: &str,
        token: &str,
        body: Option<Vec<u8>>,
        headers: &[(header::HeaderName, &str)],
    ) -> Response {
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {token}"));
        if body.is_some() {
            req = req.header(header::CONTENT_TYPE, "application/json");
        }
        for (name, value) in headers {
            req = req.header(name, *value);
        }
        let req = req
            .body(body.map(Body::from).unwrap_or_else(Body::empty))
            .unwrap();
        self.router.clone().oneshot(req).await.unwrap()
    }
}

async fn bytes(res: Response) -> Vec<u8> {
    axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec()
}

pub(super) async fn json<T: serde::de::DeserializeOwned>(res: Response) -> T {
    let status = res.status();
    let body = bytes(res).await;
    assert!(
        status.is_success(),
        "{status}: {}",
        String::from_utf8_lossy(&body)
    );
    serde_json::from_slice(&body).unwrap()
}

/// Every file under `root`, by relative path, but the client's own.
pub(super) fn tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let path = e.path();
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if rel == CLIENT_DIR {
                continue;
            }
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                out.insert(rel, std::fs::read(&path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

pub(super) fn folders(root: &Path) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let path = e.path();
            if path.is_dir() && e.file_name() != CLIENT_DIR {
                out.push(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                );
                walk(root, &path, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

/// A device's side, as plainly as the protocol allows: a scripted client.
struct Client<'a> {
    d: &'a Daemon,
    token: String,
    root: PathBuf,
}

/// What a sync did.
#[derive(Debug, Default)]
struct Ran {
    reconciled: Option<ReconcileDone>,
    /// Each operation, short: "put a/b", "move a>b", …
    ops: Vec<String>,
}

impl Ran {
    fn count(&self, kind: &str) -> usize {
        self.ops.iter().filter(|o| o.starts_with(kind)).count()
    }
}

impl Client<'_> {
    fn new<'a>(d: &'a Daemon, token: String, name: &str) -> Client<'a> {
        let root = d.dir.join(name);
        std::fs::create_dir_all(&root).unwrap();
        Client { d, token, root }
    }

    async fn post<T: serde::Serialize>(&self, path: &str, body: &T) -> Response {
        let body = serde_json::to_vec(body).unwrap();
        self.d
            .call("POST", path, &self.token, Some(body), &[])
            .await
    }

    async fn post_lines(&self, path: &str, lines: Vec<String>) -> Response {
        let body = lines.join("\n").into_bytes();
        self.d
            .call("POST", path, &self.token, Some(body), &[])
            .await
    }

    fn marker_path(&self) -> PathBuf {
        self.root.join(CLIENT_DIR).join("device.json")
    }

    async fn sync(&self) -> Ran {
        let marker: Option<Marker> = std::fs::read(self.marker_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok());
        let start = SessionStart {
            marker,
            ..Default::default()
        };
        let started: SessionStarted = json(self.post("/api/sync/v1/sessions", &start).await).await;
        let base = format!("/api/sync/v1/sessions/{}", started.session);
        let mut ran = Ran::default();
        if started.reconcile {
            ran.reconciled = Some(self.reconcile(&base).await);
        }
        let mut ops = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let url = match &after {
                None => format!("{base}/plan"),
                Some(c) => format!("{base}/plan?after={c}"),
            };
            let res = self.d.call("GET", &url, &self.token, None, &[]).await;
            assert_eq!(res.status(), StatusCode::OK);
            let mut next = None;
            for line in bytes(res)
                .await
                .split(|&b| b == b'\n')
                .filter(|l| !l.is_empty())
            {
                match PlanLine::parse(line).unwrap() {
                    PlanLine::Op(op) => ops.push(op),
                    PlanLine::End(end) => next = end.next,
                    PlanLine::Header(_) => {}
                }
            }
            match next {
                Some(c) => after = Some(c),
                None => break,
            }
        }
        let mut done = Vec::new();
        for op in ops {
            if let Some(fact) = self.carry_out(&op).await {
                done.push(fact);
            }
            ran.ops.push(short(&op));
        }
        let batch = FactsBatch {
            done,
            ..Default::default()
        };
        let res = self.post(&format!("{base}/facts"), &batch).await;
        assert_eq!(res.status(), StatusCode::OK);
        let end = EndReport {
            result: Outcome::Complete,
            message: None,
            files: ran.ops.len() as u64,
            bytes: 0,
            seconds: 0,
            free_bytes: None,
        };
        let res = self.post(&format!("{base}/end"), &end).await;
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        ran
    }

    async fn reconcile(&self, base: &str) -> ReconcileDone {
        let listing: Vec<String> = tree(&self.root)
            .into_iter()
            .map(|(path, data)| {
                serde_json::to_string(&Listed {
                    path,
                    size: data.len() as u64,
                })
                .unwrap()
            })
            .collect();
        let res = self
            .post_lines(&format!("{base}/reconcile?last=1"), listing)
            .await;
        assert_eq!(res.status(), StatusCode::OK);
        let wanted: Vec<SampleRequest> = bytes(res)
            .await
            .split(|&b| b == b'\n')
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_slice(l).unwrap())
            .collect();
        let samples: Vec<String> = wanted
            .iter()
            .map(|w| {
                let path = self.root.join(&w.path);
                let size = std::fs::metadata(&path).unwrap().len();
                let mut f = std::fs::File::open(&path).unwrap();
                let sample = coversync_protocol::sample(&mut f, size, &w.ranges).unwrap();
                serde_json::to_string(&Sampled {
                    path: w.path.clone(),
                    sample: Some(sample),
                    hash: None,
                })
                .unwrap()
            })
            .collect();
        let done: ReconcileDone = json(
            self.post_lines(&format!("{base}/reconcile/samples?last=1"), samples)
                .await,
        )
        .await;
        std::fs::create_dir_all(self.root.join(CLIENT_DIR)).unwrap();
        let marker = Marker {
            device: 0,
            epoch: done.epoch.clone(),
        };
        // The device's id comes from its token: ask the server.
        let res = self
            .d
            .call("GET", "/api/session", &self.token, None, &[])
            .await;
        let session: serde_json::Value = json(res).await;
        let marker = Marker {
            device: session["device"].as_i64().unwrap(),
            ..marker
        };
        std::fs::write(self.marker_path(), serde_json::to_vec(&marker).unwrap()).unwrap();
        done
    }

    async fn carry_out(&self, op: &Op) -> Option<Fact> {
        let at = |p: &str| self.root.join(p);
        match op {
            Op::Delete { path, size, hash } => {
                let meta = std::fs::metadata(at(path)).ok()?;
                assert_eq!(meta.len(), *size, "{path}");
                std::fs::remove_file(at(path)).unwrap();
                Some(Fact::Delete {
                    path: path.clone(),
                    hash: hash.clone(),
                })
            }
            Op::Mkdir { path } => {
                std::fs::create_dir_all(at(path)).unwrap();
                None
            }
            Op::Move {
                from, path, hash, ..
            } => {
                std::fs::rename(at(from), at(path)).unwrap();
                Some(Fact::Move {
                    from: from.clone(),
                    path: path.clone(),
                    hash: hash.clone(),
                })
            }
            Op::Rmdir { path } => {
                let _ = std::fs::remove_dir(at(path));
                None
            }
            Op::Put {
                path, hash, size, ..
            } => {
                let url = format!("/api/sync/v1/content/{hash}");
                let res = self.d.call("GET", &url, &self.token, None, &[]).await;
                assert_eq!(res.status(), StatusCode::OK, "{path}");
                let data = bytes(res).await;
                assert_eq!(data.len() as u64, *size);
                assert_eq!(blake3::hash(&data).to_hex().as_str(), hash);
                let partial = self.root.join(CLIENT_DIR).join("partial");
                std::fs::create_dir_all(&partial).unwrap();
                std::fs::write(partial.join(hash), &data).unwrap();
                std::fs::rename(partial.join(hash), at(path)).unwrap();
                Some(Fact::Put {
                    path: path.clone(),
                    hash: hash.clone(),
                    size: *size,
                })
            }
        }
    }
}

fn short(op: &Op) -> String {
    match op {
        Op::Delete { path, .. } => format!("delete {path}"),
        Op::Mkdir { path } => format!("mkdir {path}"),
        Op::Move { from, path, .. } => format!("move {from}>{path}"),
        Op::Rmdir { path } => format!("rmdir {path}"),
        Op::Put { path, replace, .. } => {
            format!("put {path}{}", if *replace { " (replace)" } else { "" })
        }
    }
}

/// Wait for the cover files a device needs: they're made in the
/// background, and count as preparing until then.
pub(super) async fn covers_made(d: &Daemon, token: &str) {
    for _ in 0..100 {
        let res = d.call("GET", "/api/sync/v1/status", token, None, &[]).await;
        let status: coversync_protocol::Status = json(res).await;
        if status.preparing == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("cover files weren't made");
}

#[tokio::test]
async fn a_device_follows_the_library() {
    let d = Daemon::new("follows");
    let (id, token) = d.device(Scope::Sync);
    covers_made(&d, &token).await;
    let phone = Client::new(&d, token.clone(), "phone");

    // The first sync: an empty folder, then every file and the cover.
    let ran = phone.sync().await;
    let reconciled = ran.reconciled.as_ref().expect("a first sync reconciles");
    assert_eq!((reconciled.adopted, reconciled.moves), (0, 0));
    assert_eq!(ran.count("put"), 4, "{:?}", ran.ops);
    assert_eq!(tree(&phone.root), d.files());

    // Nothing changed: no reconcile, nothing to do.
    let ran = phone.sync().await;
    assert!(ran.reconciled.is_none());
    assert!(ran.ops.is_empty(), "{:?}", ran.ops);

    // A structure of its own: everything moves, nothing downloads.
    let own = DeviceSettings {
        structure: Some("{album}/{title}".into()),
        ..Default::default()
    };
    d.settings(id, own);
    let ran = phone.sync().await;
    assert_eq!(
        (ran.count("put"), ran.count("move")),
        (0, 4),
        "{:?}",
        ran.ops
    );
    let got: Vec<String> = tree(&phone.root).into_keys().collect();
    assert_eq!(
        got,
        [
            "Album One/One.flac",
            "Album One/Two.flac",
            "Album One/cover.png",
            "Album Two/Three.flac"
        ]
    );
    assert_eq!(folders(&phone.root), ["Album One", "Album Two"]);

    // A tag edit: one file again, in place. Until it's hashed, the device
    // keeps the copy it has.
    let mut edited = tags("Two", "A", "Album One", 2);
    edited.push("GENRE=Rock".into());
    write_track(
        &d.library.join("Artist A/Album One/02 - Two.flac"),
        &edited,
        60_000,
    );
    d.scan();
    let ran = phone.sync().await;
    assert!(ran.ops.is_empty(), "{:?}", ran.ops);
    d.rescan();
    let ran = phone.sync().await;
    assert_eq!(ran.ops, ["put Album One/Two.flac (replace)"]);
    assert_eq!(
        tree(&phone.root)["Album One/Two.flac"],
        std::fs::read(d.library.join("Artist A/Album One/02 - Two.flac")).unwrap()
    );

    // A new title moves the track under this structure. Until the file is
    // hashed again, the copy stays where it was; then it's replaced.
    let mut retitled = tags("Second", "A", "Album One", 2);
    retitled.push("GENRE=Rock".into());
    write_track(
        &d.library.join("Artist A/Album One/02 - Two.flac"),
        &retitled,
        60_000,
    );
    d.scan();
    let ran = phone.sync().await;
    assert!(ran.ops.is_empty(), "{:?}", ran.ops);
    d.rescan();
    let ran = phone.sync().await;
    assert_eq!(
        ran.ops,
        ["delete Album One/Two.flac", "put Album One/Second.flac"]
    );

    // An album removed from the Library goes from the device, folder and all.
    std::fs::remove_dir_all(d.library.join("Artist B")).unwrap();
    d.rescan();
    let ran = phone.sync().await;
    assert_eq!(ran.ops, ["delete Album Two/Three.flac", "rmdir Album Two"]);

    // A sync token reaches these routes, and no others.
    let res = d.call("GET", "/api/albums", &token, None, &[]).await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_copied_folder_is_adopted_by_samples() {
    let d = Daemon::new("adopt");
    let (id, token) = d.device(Scope::Manage);
    covers_made(&d, &token).await;
    let player = Client::new(&d, token, "player");
    let lib = |p: &str| std::fs::read(d.library.join(p)).unwrap();
    // A copy of one track somewhere else, a copy of another with its tags
    // changed but its size kept, and a file of the user's own.
    let put = |p: &str, data: &[u8]| {
        let path = player.root.join(p);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, data).unwrap();
    };
    put(
        "Elsewhere/one.flac",
        &lib("Artist A/Album One/01 - One.flac"),
    );
    let mut changed = lib("Artist B/Album Two/01 - Three.flac");
    let at = changed.windows(5).position(|w| w == b"Three").unwrap();
    changed[at..at + 5].copy_from_slice(b"THREE");
    put("Artist B/Album Two/01 - Three.flac", &changed);
    put("notes.txt", b"mine");

    let ran = player.sync().await;
    let r = ran.reconciled.unwrap();
    assert_eq!((r.adopted, r.moves, r.other.files), (0, 1, 2), "{r:?}");
    assert!(
        ran.ops
            .contains(&"move Elsewhere/one.flac>Artist A/Album One/01 - One.flac".into())
    );
    assert!(
        ran.ops
            .contains(&"put Artist B/Album Two/01 - Three.flac (replace)".into()),
        "{:?}",
        ran.ops
    );
    let mut want = d.files();
    want.insert("notes.txt".into(), b"mine".to_vec());
    assert_eq!(tree(&player.root), want);

    // Asked to, the next sync removes the file coversync didn't put there.
    d.state
        .sync
        .store
        .set_flag(id, super::store::Flag::RemoveOther, true)
        .unwrap();
    let ran = player.sync().await;
    assert_eq!(ran.ops, ["delete notes.txt"]);
    assert_eq!(tree(&player.root), d.files());
    // Asked once: files the user puts there later stay.
    assert!(!d.state.sync.store.device(id).unwrap().remove_other);
}

#[tokio::test]
async fn downloads_resume_and_changed_files_are_gone() {
    let d = Daemon::new("content");
    let (_, token) = d.device(Scope::Sync);
    let path = d.library.join("Artist A/Album One/01 - One.flac");
    let data = std::fs::read(&path).unwrap();
    let hash = blake3::hash(&data).to_hex().to_string();
    let url = format!("/api/sync/v1/content/{hash}");

    let res = d
        .call("GET", &url, &token, None, &[(header::RANGE, "bytes=100-")])
        .await;
    assert_eq!(res.status(), StatusCode::PARTIAL_CONTENT);
    let range = format!("bytes 100-{}/{}", data.len() - 1, data.len());
    assert_eq!(res.headers()[header::CONTENT_RANGE], range.as_str());
    assert_eq!(bytes(res).await, &data[100..]);
    // A resume of other bytes gets the whole file.
    let res = d
        .call(
            "GET",
            &url,
            &token,
            None,
            &[
                (header::RANGE, "bytes=100-"),
                (header::IF_RANGE, "\"other\""),
            ],
        )
        .await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(bytes(res).await, data);

    // Changed on disk since it was hashed, the file isn't served as it.
    std::fs::write(&path, [&data[..], b"more"].concat()).unwrap();
    let res = d.call("GET", &url, &token, None, &[]).await;
    assert_eq!(res.status(), StatusCode::GONE);
    let res = d
        .call("GET", "/api/sync/v1/content/0000", &token, None, &[])
        .await;
    assert_eq!(res.status(), StatusCode::GONE);
}

async fn next_event(stream: &mut BodyDataStream) -> String {
    loop {
        let chunk = tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .expect("an event")
            .unwrap()
            .unwrap();
        let text = String::from_utf8(chunk.to_vec()).unwrap();
        if text.starts_with("event:") {
            return text;
        }
    }
}

#[tokio::test]
async fn devices_hear_of_changes_and_check_cheaply() {
    let d = Daemon::new("events");
    let (id, token) = d.device(Scope::Sync);
    covers_made(&d, &token).await;

    let res = d
        .call("GET", "/api/sync/v1/status", &token, None, &[])
        .await;
    let etag = res.headers()[header::ETAG].to_str().unwrap().to_string();
    let status: coversync_protocol::Status = json(res).await;
    assert_eq!(status.pending.files, 4);
    let res = d
        .call(
            "GET",
            "/api/sync/v1/status",
            &token,
            None,
            &[(header::IF_NONE_MATCH, &etag)],
        )
        .await;
    assert_eq!(res.status(), StatusCode::NOT_MODIFIED);

    let res = d
        .call("GET", "/api/sync/v1/events?heartbeat=15", &token, None, &[])
        .await;
    assert_eq!(res.status(), StatusCode::OK);
    let mut stream = res.into_body().into_data_stream();
    let first = next_event(&mut stream).await;
    assert!(first.contains(&status.target), "{first}");
    // A new structure moves the target on.
    d.settings(
        id,
        DeviceSettings {
            structure: Some("{title}".into()),
            ..Default::default()
        },
    );
    let moved = next_event(&mut stream).await;
    assert!(
        moved.starts_with("event: target") && !moved.contains(&status.target),
        "{moved}"
    );
    // Sync now, from Sync setup.
    let _ = d.state.sync.requests.send(id);
    assert!(next_event(&mut stream).await.starts_with("event: sync"));
    // When it syncs by itself can change while its files don't.
    d.settings(
        id,
        DeviceSettings {
            structure: Some("{title}".into()),
            auto: coversync_protocol::AutoSync {
                quiet_seconds: 5,
                ..Default::default()
            },
            ..Default::default()
        },
    );
    let settings = next_event(&mut stream).await;
    assert!(
        settings.starts_with("event: settings") && settings.contains(r#""quiet_seconds":5"#),
        "{settings}"
    );
    // A revoked device is turned away at once; the others aren't.
    let (_, other) = d.device(Scope::Sync);
    d.state
        .auth
        .store()
        .revoke_device(id, crate::sync::now())
        .unwrap();
    let res = d
        .call("GET", "/api/sync/v1/status", &token, None, &[])
        .await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let res = d
        .call("GET", "/api/sync/v1/status", &other, None, &[])
        .await;
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn files_a_check_cant_tell_yet_are_left_alone() {
    let d = Daemon::new("waiting");
    let (_, token) = d.device(Scope::Sync);
    covers_made(&d, &token).await;
    let tablet = Client::new(&d, token, "tablet");
    tablet.sync().await;
    // A same-length title edit keeps the file's size; until it's hashed,
    // the device's copy can't be matched. A file of the user's own happens
    // to have the same size.
    let three = d.library.join("Artist B/Album Two/01 - Three.flac");
    let size = std::fs::metadata(&three).unwrap().len() as usize;
    write_track(&three, &tags("THREE", "B", "Album Two", 1), 70_000);
    d.scan();
    std::fs::write(tablet.root.join("mine.bin"), vec![9u8; size]).unwrap();
    std::fs::remove_file(tablet.marker_path()).unwrap();
    let ran = tablet.sync().await;
    let r = ran.reconciled.unwrap();
    // Both count as files of the user's own, which nothing deletes unasked.
    assert_eq!((r.adopted, r.other.files), (3, 2), "{r:?}");
    assert!(ran.ops.is_empty(), "{:?}", ran.ops);
    // Once it's hashed, the copy is replaced like any file in the way.
    d.rescan();
    let ran = tablet.sync().await;
    assert_eq!(
        ran.ops,
        ["put Artist B/Album Two/01 - Three.flac (replace)"]
    );
    let mut want = d.files();
    want.insert("mine.bin".into(), vec![9u8; size]);
    assert_eq!(tree(&tablet.root), want);
}

#[tokio::test]
async fn the_inventory_pages_in_path_order() {
    use coversync_protocol::PAGE_OPS;
    let d = Daemon::new("inventory");
    let (id, token) = d.device(Scope::Sync);
    // Exactly two pages, with paths whose byte order isn't their order by
    // case or by character.
    let mut held = std::collections::HashMap::new();
    for i in 0..2 * PAGE_OPS {
        let path = match i % 4 {
            0 => format!("a/{i:05}.flac"),
            1 => format!("B/{i:05}.flac"),
            2 => format!("é/{i:05}.flac"),
            _ => format!("z/{i:05}.flac"),
        };
        held.insert(
            path,
            super::plan::Held {
                hash: format!("{i:064x}"),
                size: i as u64,
                track: None,
                album: None,
            },
        );
    }
    d.state
        .sync
        .store
        .reconciled(id, &held, &Default::default(), "epoch")
        .unwrap();

    let mut seen = Vec::new();
    let mut pages = 0;
    let mut after: Option<String> = None;
    loop {
        let url = match &after {
            None => "/api/sync/v1/inventory".to_string(),
            Some(a) => {
                let a: String = a.bytes().map(|b| format!("%{b:02X}")).collect();
                format!("/api/sync/v1/inventory?after={a}")
            }
        };
        let res = d.call("GET", &url, &token, None, &[]).await;
        assert_eq!(res.status(), StatusCode::OK);
        let body = String::from_utf8(bytes(res).await).unwrap();
        let mut lines: Vec<serde_json::Value> = body
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let end = lines.pop().unwrap();
        pages += 1;
        for l in lines {
            let path = l["path"].as_str().unwrap().to_string();
            assert_eq!(l["size"].as_u64(), Some(held[&path].size));
            seen.push(path);
        }
        match end["next"].as_str() {
            Some(next) => after = Some(next.to_string()),
            None => break,
        }
    }
    // No empty page after the last full one.
    assert_eq!(pages, 2);
    let mut want: Vec<String> = held.keys().cloned().collect();
    want.sort();
    assert_eq!(seen, want);
}
