//! The sync engine (`coversync-replica`) against the whole app, served over
//! TCP as it is in use, over a scratch Library: what SYN-05 promises.

use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use coversync_core::sync::DeviceSettings;
use coversync_protocol::{AutoSync, Capabilities, Fact, Listed};
use coversync_replica::{
    CancellationToken, Engine, Entry, Error, FsStorage, Options, Outcome, Partial, Reason,
    ReconcileBy, Report, Server, Source, Storage, Tls, Token, Trigger, WatchOptions,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::tests::{Daemon, covers_made, folders, tags, tree, write_track};
use crate::Shared;
use crate::auth::store::Scope;

/// The app on a port of its own: its address.
async fn serve(d: &Daemon) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let listener = axum::serve::ListenerExt::tap_io(listener, |tcp| {
        tcp.set_nodelay(true).unwrap();
    });
    let app = d
        .router
        .clone()
        .into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

fn options() -> Options {
    Options {
        downloads: 3,
        ..Default::default()
    }
}

fn engine(addr: SocketAddr, token: &str, storage: Arc<dyn Storage>, options: Options) -> Engine {
    let server = Server {
        url: format!("http://{addr}"),
        token: Token::new(token).unwrap(),
        tls: Tls::System,
    };
    Engine::new(&server, storage, options).unwrap()
}

fn folder(d: &Daemon, name: &str) -> (PathBuf, Arc<FsStorage>) {
    let root = d.dir.join(name);
    std::fs::create_dir_all(&root).unwrap();
    let storage = Arc::new(FsStorage::open(&root).unwrap());
    (root, storage)
}

async fn sync(e: &Engine) -> Report {
    let report = e
        .sync(Reason::User, Arc::new(()), CancellationToken::new())
        .await
        .unwrap();
    eprintln!("{report:?}");
    report
}

/// Index and hash the Library as it is now, from anywhere.
fn rescan(state: &Shared) {
    state.lib.refresh_library().unwrap();
    coversync_core::scan::scan(&state.lib, &state.covers, &state.roots(), &state.progress).unwrap();
    coversync_core::hashes::run(&state.lib, &state.hashes).unwrap();
}

#[tokio::test]
async fn the_engine_follows_the_library() {
    let d = Daemon::new("engine-follows");
    let (id, token) = d.device(Scope::Sync);
    covers_made(&d, &token).await;
    let addr = serve(&d).await;
    let (root, storage) = folder(&d, "laptop");
    let e = engine(addr, &token, storage, options());

    // A full sync: a reconcile of the empty folder, then every file.
    let r = sync(&e).await;
    assert_eq!(r.outcome, Some(Outcome::Complete));
    assert!(r.reconciled.is_some());
    assert_eq!(r.files, 4);
    assert_eq!(tree(&root), d.files());
    let marker: coversync_protocol::Marker = serde_json::from_slice(
        &std::fs::read(root.join(".coversync-replica/device.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(marker.device, id);
    // Nothing left behind in the client's folder.
    assert_eq!(
        std::fs::read_dir(root.join(".coversync-replica/partial"))
            .unwrap()
            .count(),
        0
    );
    assert_eq!(
        std::fs::read(root.join(".coversync-replica/facts")).unwrap(),
        b""
    );

    // Nothing changed: no reconcile, nothing done.
    let r = sync(&e).await;
    assert!(r.reconciled.is_none());
    assert_eq!((r.files, r.moved, r.deleted), (0, 0, 0));

    // A tag edit: that file again, in place.
    let mut edited = tags("Two", "A", "Album One", 2);
    edited.push("GENRE=Rock".into());
    write_track(
        &d.library.join("Artist A/Album One/02 - Two.flac"),
        &edited,
        60_000,
    );
    d.rescan();
    let r = sync(&e).await;
    assert_eq!((r.files, r.moved, r.deleted), (1, 0, 0));
    assert_eq!(tree(&root), d.files());

    // A structure of its own: renames, no downloads, old folders gone.
    d.settings(
        id,
        DeviceSettings {
            structure: Some("{album}/{title}".into()),
            ..Default::default()
        },
    );
    let r = sync(&e).await;
    assert_eq!((r.files, r.moved), (0, 4), "{r:?}");
    assert_eq!(folders(&root), ["Album One", "Album Two"]);

    // An album removed from the Library goes, folder and all.
    std::fs::remove_dir_all(d.library.join("Artist B")).unwrap();
    d.rescan();
    let r = sync(&e).await;
    assert_eq!((r.files, r.deleted), (0, 1));
    assert_eq!(folders(&root), ["Album One"]);

    // A Library restored from a backup: the same bytes, new files and new
    // track ids. Nothing downloads again.
    let before = tree(&root);
    let backup = d.dir.join("backup");
    std::fs::rename(&d.library, &backup).unwrap();
    std::fs::create_dir_all(&d.library).unwrap();
    d.rescan();
    std::fs::remove_dir_all(&d.library).unwrap();
    copy_dir(&backup, &d.library);
    d.rescan();
    let r = sync(&e).await;
    assert_eq!((r.files, r.moved, r.deleted), (0, 0, 0), "{r:?}");
    assert_eq!(tree(&root), before);
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap().flatten() {
        let (src, dst) = (e.path(), to.join(e.file_name()));
        if src.is_dir() {
            copy_dir(&src, &dst);
        } else {
            std::fs::copy(&src, &dst).unwrap();
        }
    }
}

#[tokio::test]
async fn a_copied_folder_is_adopted_where_it_lies() {
    for by in [ReconcileBy::Samples, ReconcileBy::Hashes] {
        let d = Daemon::new(&format!("engine-adopt-{by:?}"));
        // Thirty tracks all the same size: each must be told apart from
        // the others by its sample, not taken for the first of its size.
        for n in 10..40 {
            write_track(
                &d.library.join(format!("Same/Size/{n} - Track {n}.flac")),
                &tags(&format!("Track {n}"), "Same", "Size", n),
                40_000,
            );
        }
        d.rescan();
        let (_, token) = d.device(Scope::Sync);
        covers_made(&d, &token).await;
        let addr = serve(&d).await;
        let (root, storage) = folder(&d, "player");
        // The Library copied in by hand, one album reorganised, and a file
        // of the user's own.
        copy_dir(&d.library, &root);
        std::fs::create_dir_all(root.join("Elsewhere")).unwrap();
        std::fs::rename(
            root.join("Artist B/Album Two/01 - Three.flac"),
            root.join("Elsewhere/three.flac"),
        )
        .unwrap();
        std::fs::write(root.join("notes.txt"), b"mine").unwrap();

        let e = engine(
            addr,
            &token,
            storage,
            Options {
                reconcile_by: by,
                ..options()
            },
        );
        let r = sync(&e).await;
        let done = r.reconciled.clone().unwrap();
        assert_eq!(
            (done.adopted, done.moves, done.other.files),
            (33, 1, 1),
            "{by:?}: {done:?}"
        );
        assert_eq!((r.files, r.moved), (0, 1), "{by:?}: {r:?}");
        let mut want = d.files();
        want.insert("notes.txt".into(), b"mine".to_vec());
        assert_eq!(tree(&root), want, "{by:?}");
        // The folder the move emptied goes too.
        assert!(!root.join("Elsewhere").exists());
    }
}

/// A relay to the server that cuts one connection once it has carried
/// `cut` bytes of replies, mid-download. Returns its address, the reply
/// bytes it has carried, and whether it has cut.
async fn cutting_relay(
    upstream: SocketAddr,
    cut: u64,
) -> (SocketAddr, Arc<AtomicU64>, Arc<AtomicBool>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let carried = Arc::new(AtomicU64::new(0));
    let cut_done = Arc::new(AtomicBool::new(false));
    let (total, did_cut) = (carried.clone(), cut_done.clone());
    tokio::spawn(async move {
        loop {
            let (client, _) = listener.accept().await.unwrap();
            let server = TcpStream::connect(upstream).await.unwrap();
            let (total, cut_done) = (total.clone(), cut_done.clone());
            tokio::spawn(async move {
                let (mut cr, mut cw) = client.into_split();
                let (mut sr, mut sw) = server.into_split();
                let up = tokio::spawn(async move {
                    let _ = tokio::io::copy(&mut cr, &mut sw).await;
                });
                let mut here = 0u64;
                let mut buf = vec![0; 16 << 10];
                loop {
                    let n = match sr.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    let mut n = n;
                    if here + n as u64 > cut && !cut_done.swap(true, Ordering::SeqCst) {
                        n = (cut - here) as usize;
                        let _ = cw.write_all(&buf[..n]).await;
                        total.fetch_add(n as u64, Ordering::SeqCst);
                        break;
                    }
                    if cw.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                    here += n as u64;
                    total.fetch_add(n as u64, Ordering::SeqCst);
                }
                up.abort();
            });
        }
    });
    (addr, carried, did_cut)
}

#[tokio::test]
async fn a_cut_connection_resumes_where_it_stopped() {
    let d = Daemon::new("engine-cut");
    let big = 3_000_000;
    write_track(
        &d.library.join("Artist C/Big/01 - Big.flac"),
        &tags("Big", "C", "Big", 1),
        big,
    );
    d.rescan();
    let (_, token) = d.device(Scope::Sync);
    covers_made(&d, &token).await;
    let addr = serve(&d).await;
    let (relay, carried, cut) = cutting_relay(addr, 1_000_000).await;
    let (root, storage) = folder(&d, "laptop");
    let e = engine(
        relay,
        &token,
        storage,
        Options {
            downloads: 1,
            ..options()
        },
    );
    let r = sync(&e).await;
    assert!(cut.load(Ordering::SeqCst), "the relay cut a download");
    assert_eq!(r.outcome, Some(Outcome::Complete), "{r:?}");
    assert_eq!(tree(&root), d.files());
    // Resumed, not begun again: what was carried before the cut wasn't
    // carried twice.
    let files: u64 = d.files().values().map(|v| v.len() as u64).sum();
    let carried = carried.load(Ordering::SeqCst);
    assert!(
        carried < files + 200_000,
        "{carried} bytes carried for {files} bytes of files"
    );
}

/// A folder on storage with `room` bytes for downloads, which may say it
/// has more than it does.
struct Tight {
    inner: FsStorage,
    room: Arc<AtomicU64>,
    used: Arc<AtomicU64>,
    /// What it says it has free, if not the truth.
    claims: Option<u64>,
}

struct TightFile {
    inner: Box<dyn Partial>,
    room: Arc<AtomicU64>,
    used: Arc<AtomicU64>,
}

impl Write for TightFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let used = self.used.fetch_add(buf.len() as u64, Ordering::SeqCst) + buf.len() as u64;
        if used > self.room.load(Ordering::SeqCst) {
            self.used.fetch_sub(buf.len() as u64, Ordering::SeqCst);
            return Err(io::ErrorKind::StorageFull.into());
        }
        self.inner.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl Partial for TightFile {
    fn sync(&mut self) -> io::Result<()> {
        self.inner.sync()
    }
}

impl Storage for Tight {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
    fn free_bytes(&self) -> io::Result<Option<u64>> {
        let free = self
            .room
            .load(Ordering::SeqCst)
            .saturating_sub(self.used.load(Ordering::SeqCst));
        Ok(Some(self.claims.unwrap_or(free)))
    }
    fn list(&self, skip: &dyn Fn(&str) -> bool, each: &mut dyn FnMut(Listed)) -> io::Result<()> {
        self.inner.list(skip, each)
    }
    fn stat(&self, path: &str) -> io::Result<Option<Entry>> {
        self.inner.stat(path)
    }
    fn remove_file(&self, path: &str) -> io::Result<()> {
        self.inner.remove_file(path)
    }
    fn create_dirs(&self, path: &str) -> io::Result<()> {
        self.inner.create_dirs(path)
    }
    fn remove_dir(&self, path: &str) -> io::Result<()> {
        self.inner.remove_dir(path)
    }
    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        self.inner.rename(from, to)
    }
    fn open(&self, path: &str) -> io::Result<Box<dyn Source>> {
        self.inner.open(path)
    }
    fn partial_len(&self, name: &str) -> io::Result<Option<u64>> {
        self.inner.partial_len(name)
    }
    fn write_partial(&self, name: &str, resume: bool) -> io::Result<Box<dyn Partial>> {
        if !resume && let Some(n) = self.inner.partial_len(name)? {
            self.used.fetch_sub(n, Ordering::SeqCst);
        }
        Ok(Box::new(TightFile {
            inner: self.inner.write_partial(name, resume)?,
            room: self.room.clone(),
            used: self.used.clone(),
        }))
    }
    fn read_partial(&self, name: &str) -> io::Result<Box<dyn Read + Send>> {
        self.inner.read_partial(name)
    }
    fn remove_partial(&self, name: &str) -> io::Result<()> {
        if let Some(n) = self.inner.partial_len(name)? {
            self.used.fetch_sub(n, Ordering::SeqCst);
        }
        self.inner.remove_partial(name)
    }
    fn publish(&self, name: &str, path: &str, replace: bool) -> io::Result<()> {
        self.inner.publish(name, path, replace)
    }
    fn partials(&self) -> io::Result<Vec<(String, u64)>> {
        self.inner.partials()
    }
    fn clear_partials(&self) -> io::Result<()> {
        self.inner.clear_partials()
    }
    fn begin_changes(&self) -> io::Result<bool> {
        self.inner.begin_changes()
    }
    fn end_changes(&self) -> io::Result<()> {
        self.inner.end_changes()
    }
    fn read_marker(&self) -> io::Result<Option<Vec<u8>>> {
        self.inner.read_marker()
    }
    fn write_marker(&self, bytes: &[u8]) -> io::Result<()> {
        self.inner.write_marker(bytes)
    }
    fn read_journal(&self) -> io::Result<Vec<u8>> {
        self.inner.read_journal()
    }
    fn append_journal(&self, bytes: &[u8]) -> io::Result<()> {
        self.inner.append_journal(bytes)
    }
    fn replace_journal(&self, bytes: &[u8]) -> io::Result<()> {
        self.inner.replace_journal(bytes)
    }
}

#[tokio::test]
async fn a_full_device_syncs_what_fits_and_says_how_much_more_it_needs() {
    let d = Daemon::new("engine-full");
    let (_, token) = d.device(Scope::Sync);
    covers_made(&d, &token).await;
    let addr = serve(&d).await;
    let files: u64 = d.files().values().map(|v| v.len() as u64).sum();
    let reserve = coversync_protocol::RESERVE_BYTES;

    // The storage says how little room it has: the server plans what fits.
    let root = d.dir.join("small");
    std::fs::create_dir_all(&root).unwrap();
    let room = Arc::new(AtomicU64::new(reserve + 120_000));
    let used = Arc::new(AtomicU64::new(0));
    let small = Arc::new(Tight {
        inner: FsStorage::open(&root).unwrap(),
        room: room.clone(),
        used: used.clone(),
        claims: None,
    });
    let e = engine(addr, &token, small, options());
    let r = sync(&e).await;
    assert_eq!(r.outcome, Some(Outcome::Partial), "{r:?}");
    assert!(r.short_bytes > 0, "{r:?}");
    assert!(r.files > 0 && r.files < 4, "{r:?}");
    assert_eq!(r.failed, 0);
    // With room made, the rest follows.
    room.store(reserve + files * 2, Ordering::SeqCst);
    let r = sync(&e).await;
    assert_eq!(r.outcome, Some(Outcome::Complete), "{r:?}");
    assert_eq!(tree(&root), d.files());

    // Storage that claims room it hasn't got: the sync stops when it runs
    // out, nothing is held back as failed, and it carries on once there's
    // room.
    let root = d.dir.join("liar");
    std::fs::create_dir_all(&root).unwrap();
    let room = Arc::new(AtomicU64::new(100_000));
    let liar = Arc::new(Tight {
        inner: FsStorage::open(&root).unwrap(),
        room: room.clone(),
        used: Arc::new(AtomicU64::new(0)),
        claims: Some(1 << 40),
    });
    let (_, token2) = d.device(Scope::Sync);
    let e = engine(addr, &token2, liar, options());
    let r = sync(&e).await;
    assert_eq!(r.outcome, Some(Outcome::Partial), "{r:?}");
    assert_eq!(r.failed, 0, "{r:?}");
    room.store(files * 2, Ordering::SeqCst);
    let r = sync(&e).await;
    assert_eq!(r.outcome, Some(Outcome::Complete), "{r:?}");
    assert_eq!(tree(&root), d.files());
}

#[tokio::test]
async fn a_crash_costs_at_most_one_download_and_never_a_wrong_file() {
    let d = Daemon::new("engine-crash");
    let (id, token) = d.device(Scope::Sync);
    covers_made(&d, &token).await;
    let addr = serve(&d).await;
    let (root, storage) = folder(&d, "laptop");
    let e = engine(addr, &token, storage, options());
    sync(&e).await;
    let path = "Artist A/Album One/01 - One.flac";
    let data = std::fs::read(root.join(path)).unwrap();
    let hash = blake3::hash(&data).to_hex().to_string();
    let forget = || {
        // The server never heard the put's fact.
        d.state
            .sync
            .store
            .apply_facts(
                id,
                &[Fact::Delete {
                    path: path.into(),
                    hash: Some(hash.clone()),
                }],
                &[],
                |_, _| None,
                0,
            )
            .unwrap();
    };

    // Renamed into place, its fact journaled but never sent: the journal
    // goes first, and nothing downloads.
    forget();
    let fact = Fact::Put {
        path: path.into(),
        hash: hash.clone(),
        size: data.len() as u64,
    };
    let mut line = serde_json::to_vec(&fact).unwrap();
    line.push(b'\n');
    std::fs::write(root.join(".coversync-replica/facts"), line).unwrap();
    let r = sync(&e).await;
    assert_eq!((r.files, r.bytes), (0, 0), "{r:?}");

    // Renamed into place, its fact lost: downloaded once more, in place.
    forget();
    let r = sync(&e).await;
    assert_eq!(r.files, 1, "{r:?}");
    assert_eq!(tree(&root), d.files());

    // Downloaded whole, never renamed: finished without downloading.
    forget();
    std::fs::remove_file(root.join(path)).unwrap();
    let partial = format!("{hash}-{}", &blake3::hash(path.as_bytes()).to_hex()[..16]);
    std::fs::write(root.join(".coversync-replica/partial").join(partial), &data).unwrap();
    let r = sync(&e).await;
    assert_eq!(r.files, 1, "{r:?}");
    assert_eq!(tree(&root), d.files());

    // A partial file whose bytes are wrong (damaged on the disk) is never
    // put in place: it's thrown away and downloaded afresh.
    forget();
    std::fs::remove_file(root.join(path)).unwrap();
    let partial = format!("{hash}-{}", &blake3::hash(path.as_bytes()).to_hex()[..16]);
    let mut wrong = data.clone();
    wrong[10] ^= 0xff;
    std::fs::write(
        root.join(".coversync-replica/partial").join(&partial),
        &wrong,
    )
    .unwrap();
    let r = sync(&e).await;
    assert_eq!((r.files, r.failed), (1, 0), "{r:?}");
    assert_eq!(tree(&root), d.files());
}

/// FsStorage, with something to do once, the first time a download is put
/// in place.
struct Hooked {
    inner: FsStorage,
    hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl Storage for Hooked {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
    fn free_bytes(&self) -> io::Result<Option<u64>> {
        self.inner.free_bytes()
    }
    fn list(&self, skip: &dyn Fn(&str) -> bool, each: &mut dyn FnMut(Listed)) -> io::Result<()> {
        self.inner.list(skip, each)
    }
    fn stat(&self, path: &str) -> io::Result<Option<Entry>> {
        self.inner.stat(path)
    }
    fn remove_file(&self, path: &str) -> io::Result<()> {
        self.inner.remove_file(path)
    }
    fn create_dirs(&self, path: &str) -> io::Result<()> {
        self.inner.create_dirs(path)
    }
    fn remove_dir(&self, path: &str) -> io::Result<()> {
        self.inner.remove_dir(path)
    }
    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        self.inner.rename(from, to)
    }
    fn open(&self, path: &str) -> io::Result<Box<dyn Source>> {
        self.inner.open(path)
    }
    fn partial_len(&self, name: &str) -> io::Result<Option<u64>> {
        self.inner.partial_len(name)
    }
    fn write_partial(&self, name: &str, resume: bool) -> io::Result<Box<dyn Partial>> {
        self.inner.write_partial(name, resume)
    }
    fn read_partial(&self, name: &str) -> io::Result<Box<dyn Read + Send>> {
        self.inner.read_partial(name)
    }
    fn remove_partial(&self, name: &str) -> io::Result<()> {
        self.inner.remove_partial(name)
    }
    fn publish(&self, name: &str, path: &str, replace: bool) -> io::Result<()> {
        let hook = self.hook.lock().unwrap().take();
        if let Some(hook) = hook {
            hook();
        }
        self.inner.publish(name, path, replace)
    }
    fn partials(&self) -> io::Result<Vec<(String, u64)>> {
        self.inner.partials()
    }
    fn clear_partials(&self) -> io::Result<()> {
        self.inner.clear_partials()
    }
    fn begin_changes(&self) -> io::Result<bool> {
        self.inner.begin_changes()
    }
    fn end_changes(&self) -> io::Result<()> {
        self.inner.end_changes()
    }
    fn read_marker(&self) -> io::Result<Option<Vec<u8>>> {
        self.inner.read_marker()
    }
    fn write_marker(&self, bytes: &[u8]) -> io::Result<()> {
        self.inner.write_marker(bytes)
    }
    fn read_journal(&self) -> io::Result<Vec<u8>> {
        self.inner.read_journal()
    }
    fn append_journal(&self, bytes: &[u8]) -> io::Result<()> {
        self.inner.append_journal(bytes)
    }
    fn replace_journal(&self, bytes: &[u8]) -> io::Result<()> {
        self.inner.replace_journal(bytes)
    }
}

#[tokio::test]
async fn the_library_changing_mid_sync_is_followed_to_the_end() {
    let d = Daemon::new("engine-moving");
    let (_, token) = d.device(Scope::Sync);
    covers_made(&d, &token).await;
    let addr = serve(&d).await;
    let root = d.dir.join("laptop");
    std::fs::create_dir_all(&root).unwrap();
    // While the first download is put in place, every track is retagged and
    // hashed again: the rest of the plan is out of date.
    let (state, library) = (d.state.clone(), d.library.clone());
    let edit = move || {
        for (path, title, artist, album, n, size) in [
            (
                "Artist A/Album One/01 - One.flac",
                "One",
                "A",
                "Album One",
                1,
                50_000,
            ),
            (
                "Artist A/Album One/02 - Two.flac",
                "Two",
                "A",
                "Album One",
                2,
                60_000,
            ),
            (
                "Artist B/Album Two/01 - Three.flac",
                "Three",
                "B",
                "Album Two",
                1,
                70_000,
            ),
        ] {
            let mut t = tags(title, artist, album, n);
            t.push("GENRE=Edited".into());
            write_track(&library.join(path), &t, size);
        }
        rescan(&state);
    };
    let storage = Arc::new(Hooked {
        inner: FsStorage::open(&root).unwrap(),
        hook: Mutex::new(Some(Box::new(edit))),
    });
    let e = engine(
        addr,
        &token,
        storage,
        Options {
            downloads: 1,
            ..options()
        },
    );
    let r = sync(&e).await;
    assert_eq!(r.outcome, Some(Outcome::Complete), "{r:?}");
    assert_eq!(tree(&root), d.files());
    // And it said so: the next sync has nothing to do.
    let r = sync(&e).await;
    assert_eq!((r.files, r.moved, r.deleted), (0, 0, 0), "{r:?}");
}

async fn next(w: &mut coversync_replica::Watch) -> Trigger {
    tokio::time::timeout(Duration::from_secs(20), w.next())
        .await
        .expect("the watch speaks up")
        .unwrap()
}

#[tokio::test]
async fn the_watch_says_when_to_sync() {
    let d = Daemon::new("engine-watch");
    let (id, token) = d.device(Scope::Sync);
    d.settings(
        id,
        DeviceSettings {
            auto: AutoSync {
                quiet_seconds: 1,
                ..Default::default()
            },
            ..Default::default()
        },
    );
    covers_made(&d, &token).await;
    let addr = serve(&d).await;
    let (root, storage) = folder(&d, "laptop");
    let e = engine(addr, &token, storage, options());
    let mut w = e.watch(WatchOptions {
        heartbeat: 15,
        ..Default::default()
    });
    // Waking with nothing synced yet: sync.
    assert_eq!(
        next(&mut w).await,
        Trigger {
            reason: Reason::Wake
        }
    );
    let r = sync(&e).await;
    w.synced(r.target.as_deref());
    assert_eq!(tree(&root), d.files());

    // A change in the Library: once it's been quiet a second.
    let mut edited = tags("Two", "A", "Album One", 2);
    edited.push("GENRE=Rock".into());
    write_track(
        &d.library.join("Artist A/Album One/02 - Two.flac"),
        &edited,
        60_000,
    );
    d.rescan();
    assert_eq!(
        next(&mut w).await,
        Trigger {
            reason: Reason::Event
        }
    );
    let r = sync(&e).await;
    assert_eq!(r.files, 1);
    w.synced(r.target.as_deref());

    // Sync now, from Sync setup: at once.
    d.state.sync.requests.send(id).unwrap();
    assert_eq!(
        next(&mut w).await,
        Trigger {
            reason: Reason::User
        }
    );
    assert_eq!(w.auto().map(|a| a.quiet_seconds), Some(1));

    // A change heard while a sync runs, which that sync reaches: no second
    // sync for it once the quiet period is over.
    let two = d.library.join("Artist A/Album One/02 - Two.flac");
    let mut jazz = tags("Two", "A", "Album One", 2);
    jazz.push("GENRE=Jazz".into());
    write_track(&two, &jazz, 60_000);
    d.rescan();
    let heard = tokio::time::timeout(Duration::from_millis(300), w.next()).await;
    assert!(heard.is_err(), "{heard:?}");
    let r = sync(&e).await;
    assert_eq!(r.files, 1);
    w.synced(r.target.as_deref());
    let again = tokio::time::timeout(Duration::from_secs(3), w.next()).await;
    assert!(again.is_err(), "{again:?}");

    // Auto sync off: a change waits. Turned back on, it's synced without
    // waiting for the hourly status check.
    let auto = |enabled| DeviceSettings {
        auto: AutoSync {
            enabled,
            quiet_seconds: 1,
            ..Default::default()
        },
        ..Default::default()
    };
    d.settings(id, auto(false));
    let mut blues = tags("Two", "A", "Album One", 2);
    blues.push("GENRE=Blues".into());
    write_track(&two, &blues, 60_000);
    d.rescan();
    let off = tokio::time::timeout(Duration::from_secs(3), w.next()).await;
    assert!(off.is_err(), "{off:?}");
    d.settings(id, auto(true));
    assert_eq!(
        next(&mut w).await,
        Trigger {
            reason: Reason::Timer
        }
    );
    let r = sync(&e).await;
    assert_eq!(r.files, 1);

    // The same device in a new, empty folder, while the server thinks it
    // has everything: waking still syncs, which finds the folder new.
    let (root2, storage2) = folder(&d, "laptop-2");
    let e2 = engine(addr, &token, storage2, options());
    let mut w2 = e2.watch(WatchOptions {
        heartbeat: 15,
        ..Default::default()
    });
    assert_eq!(
        next(&mut w2).await,
        Trigger {
            reason: Reason::Wake
        }
    );
    let r = sync(&e2).await;
    assert!(r.reconciled.is_some());
    assert_eq!(tree(&root2), d.files());
}

#[tokio::test]
async fn names_the_device_keeps_are_never_touched() {
    let d = Daemon::new("engine-protected");
    let (id, token) = d.device(Scope::Sync);
    covers_made(&d, &token).await;
    let addr = serve(&d).await;
    let (root, storage) = folder(&d, "player");
    std::fs::create_dir_all(root.join(".rockbox")).unwrap();
    std::fs::write(root.join(".rockbox/config.cfg"), b"volume: -20").unwrap();
    // A folder in it the sync couldn't even read: never entered.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(root.join(".rockbox/locked")).unwrap();
        std::fs::set_permissions(
            root.join(".rockbox/locked"),
            std::fs::Permissions::from_mode(0o000),
        )
        .unwrap();
    }
    std::fs::write(root.join("notes.txt"), b"mine").unwrap();
    let e = engine(
        addr,
        &token,
        storage,
        Options {
            protected: vec![".Rockbox".into()],
            ..options()
        },
    );
    let r = sync(&e).await;
    // Not even listed: only the user's note counts as theirs.
    assert_eq!(r.reconciled.unwrap().other.files, 1);
    d.state
        .sync
        .store
        .set_flag(id, super::store::Flag::RemoveOther, true)
        .unwrap();
    let r = sync(&e).await;
    assert_eq!(r.deleted, 1, "{r:?}");
    assert!(!root.join("notes.txt").exists());
    assert_eq!(
        std::fs::read(root.join(".rockbox/config.cfg")).unwrap(),
        b"volume: -20"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            root.join(".rockbox/locked"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
}

#[tokio::test]
async fn a_sync_that_never_finished_has_its_folder_checked() {
    let d = Daemon::new("engine-unfinished");
    let (_, token) = d.device(Scope::Sync);
    covers_made(&d, &token).await;
    let addr = serve(&d).await;
    let (root, storage) = folder(&d, "laptop");
    let e = engine(addr, &token, storage, options());
    assert!(sync(&e).await.reconciled.is_some());
    assert!(sync(&e).await.reconciled.is_none());
    let note = root.join(".coversync-replica/dirty");
    assert!(!note.exists());

    // A crash, or the power cut, mid-sync leaves its note: the next sync
    // checks the folder, whose changes the server may not have heard of.
    std::fs::write(&note, b"").unwrap();
    let r = sync(&e).await;
    assert!(r.reconciled.is_some(), "{r:?}");
    assert!(!note.exists());

    // Cancelled, a sync still ends: nothing to check after it.
    let cancel = CancellationToken::new();
    cancel.cancel();
    let cancelled = e.sync(Reason::User, Arc::new(()), cancel).await;
    assert!(matches!(cancelled, Err(Error::Cancelled)), "{cancelled:?}");
    assert!(!note.exists());
    assert!(sync(&e).await.reconciled.is_none());

    // A facts journal that can't be read whole may have lost what an
    // earlier sync did: checked too.
    std::fs::write(root.join(".coversync-replica/facts"), b"{not a fact\n").unwrap();
    // Cancelled before its check could happen, the sync still owes it: the
    // note stays, and the next sync checks.
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(e.sync(Reason::User, Arc::new(()), cancel).await.is_err());
    assert!(note.exists());
    assert!(sync(&e).await.reconciled.is_some());
    assert!(!note.exists());
    assert!(sync(&e).await.reconciled.is_none());
}

#[tokio::test]
async fn old_partial_files_give_their_room_back() {
    let d = Daemon::new("engine-spare");
    let (_, token) = d.device(Scope::Sync);
    covers_made(&d, &token).await;
    let addr = serve(&d).await;
    let files: u64 = d.files().values().map(|v| v.len() as u64).sum();
    let reserve = coversync_protocol::RESERVE_BYTES;
    let root = d.dir.join("small");
    std::fs::create_dir_all(&root).unwrap();
    let inner = FsStorage::open(&root).unwrap();
    // Left by earlier syncs: half of a file still to come, and a whole file
    // of content that has since left the Library, which fill the room the
    // rest needs.
    let path = "Artist A/Album One/01 - One.flac";
    let data = std::fs::read(d.library.join(path)).unwrap();
    let hash = blake3::hash(&data).to_hex().to_string();
    let partials = root.join(".coversync-replica/partial");
    let half = &data[..data.len() / 2];
    let resumable = format!("{hash}-{}", &blake3::hash(path.as_bytes()).to_hex()[..16]);
    std::fs::write(partials.join(resumable), half).unwrap();
    let old = 150_000u64;
    let obsolete = format!("{}-0000000000000000", "0".repeat(64));
    std::fs::write(partials.join(obsolete), vec![7; old as usize]).unwrap();
    let used = Arc::new(AtomicU64::new(half.len() as u64 + old));
    let small = Arc::new(Tight {
        inner,
        room: Arc::new(AtomicU64::new(reserve + files + 20_000)),
        used,
        claims: None,
    });
    let e = engine(addr, &token, small, options());
    let r = sync(&e).await;
    assert_eq!(r.outcome, Some(Outcome::Complete), "{r:?}");
    assert_eq!(tree(&root), d.files());
    assert_eq!(std::fs::read_dir(&partials).unwrap().count(), 0);
}
