//! The desktop app playing a synced track from this computer's copy
//! (`coversync_desktop_core::local`, behind the app's proxy), against the
//! whole server over TCP: DSK-12's local playback.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use coversync_desktop_core::local::Local;
use coversync_desktop_core::sync::{Config, Service, ServiceState};
use coversync_proxy::{Proxy, Upstream};
use coversync_replica::{
    CancellationToken, Engine, FsStorage, Options, Reason, Server, Tls, Token,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::tests::{Daemon, covers_made, tree};
use crate::auth::store::Scope;

/// The app on a port of its own: its address.
async fn serve(d: &Daemon) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = d
        .router
        .clone()
        .into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

/// The sync agent's service, on or off as told.
struct Switch(AtomicBool);

impl Service for Switch {
    fn state(&self) -> ServiceState {
        ServiceState {
            installed: true,
            enabled: self.0.load(Ordering::Relaxed),
            ..Default::default()
        }
    }

    fn turn(&self, on: bool) -> Result<(), String> {
        self.0.store(on, Ordering::Relaxed);
        Ok(())
    }

    fn restart(&self) -> Result<(), String> {
        Ok(())
    }
}

/// An answer's head and body, as the window gets them from the proxy.
struct Answer {
    head: String,
    body: Vec<u8>,
}

impl Answer {
    fn local(&self) -> bool {
        self.head
            .to_ascii_lowercase()
            .contains("coversync-local: 1")
    }
}

/// The app's window, as far as the proxy can tell: it enters by the
/// proxy's own address and sends its cookie from then on.
struct Window {
    addr: SocketAddr,
    cookie: String,
}

impl Window {
    async fn open(proxy: &Proxy) -> Self {
        let addr = proxy.addr();
        let entry = proxy.entry_url();
        let path = &entry[entry.find("/?").unwrap()..];
        let answer = request(addr, path, "").await;
        let cookie = answer
            .head
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("set-cookie")
                    .then(|| value.trim().split(';').next().unwrap().to_owned())
            })
            .expect("the proxy's cookie");
        Self { addr, cookie }
    }

    async fn get(&self, path: &str) -> Answer {
        request(self.addr, path, &format!("Cookie: {}\r\n", self.cookie)).await
    }
}

/// One GET, on a connection of its own, read to its end.
async fn request(addr: SocketAddr, path: &str, headers: &str) -> Answer {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let sent = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\n{headers}Connection: close\r\n\r\n");
    stream.write_all(sent.as_bytes()).await.unwrap();
    let mut all = Vec::new();
    stream.read_to_end(&mut all).await.unwrap();
    let split = all
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("an answer's head");
    Answer {
        head: String::from_utf8_lossy(&all[..split]).into_owned(),
        body: all[split + 4..].to_vec(),
    }
}

// Bugs caught: 0, False positives: 0
#[tokio::test]
async fn a_synced_track_plays_from_this_computers_copy_while_sync_is_on() {
    let d = Daemon::new("local-playback");
    let (_, token) = d.device(Scope::Manage);
    covers_made(&d, &token).await;
    let addr = serve(&d).await;
    let url = format!("http://{addr}/");

    // The computer's copy, synced as the agent syncs it.
    let root = d.dir.join("copy");
    std::fs::create_dir_all(&root).unwrap();
    let engine = Engine::new(
        &Server {
            url: url.clone(),
            token: Token::new(token.as_str()).unwrap(),
            tls: Tls::System,
        },
        Arc::new(FsStorage::open(&root).unwrap()),
        Options::default(),
    )
    .unwrap();
    engine
        .sync(Reason::User, Arc::new(()), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(tree(&root), d.files());

    // Which track is which file, as the server lists them for the device.
    let res = d
        .call("GET", "/api/sync/v1/inventory", &token, None, &[])
        .await;
    let listed = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let (track, path) = listed
        .split(|&b| b == b'\n')
        .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
        .find_map(|line| Some((line["track"].as_i64()?, line["path"].as_str()?.to_owned())))
        .expect("a track in the inventory");
    let audio = std::fs::read(d.library.join(&path)).unwrap();
    let stream = format!("/api/tracks/{track}/stream");

    // The app: its sync settings, and a proxy to the server as its device.
    let settings = d.dir.join("sync.json");
    let config = Config {
        server: Some(url.clone()),
        root: Some(root.clone()),
        ..Default::default()
    };
    config.save(&settings).unwrap();
    let switch = Arc::new(Switch(AtomicBool::new(true)));
    let local = Arc::new(Local::new(settings.clone(), switch.clone()));
    let server = Upstream::parse(&url)
        .unwrap()
        .with_authorization(Some(format!("Bearer {token}").parse().unwrap()));
    let proxy = Proxy::builder()
        .upstream(Some(server))
        .local_files(local.clone())
        .start()
        .await
        .unwrap();
    let window = Window::open(&proxy).await;

    // The first time, nothing is known of the copy: the server's own.
    let first = window.get(&stream).await;
    assert!(first.head.starts_with("HTTP/1.1 200"), "{}", first.head);
    assert!(!first.local());
    assert_eq!(first.body, audio);
    // Once the server has said what the device holds: the copy's, the same
    // bytes.
    let mut answer = window.get(&stream).await;
    for _ in 0..100 {
        if answer.local() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        answer = window.get(&stream).await;
    }
    assert!(
        answer.local(),
        "never played from the copy: {}",
        answer.head
    );
    assert!(answer.head.starts_with("HTTP/1.1 200"));
    assert_eq!(answer.body, audio);
    // A seek.
    let ranged = request(
        window.addr,
        &stream,
        &format!("Cookie: {}\r\nRange: bytes=100-199\r\n", window.cookie),
    )
    .await;
    assert!(ranged.head.starts_with("HTTP/1.1 206"), "{}", ranged.head);
    assert!(ranged.local());
    assert_eq!(ranged.body, audio[100..200]);

    // A file that isn't what the server has (changed on this computer, or
    // cut short) isn't played: the server's is.
    let copy = root.join(&path);
    std::fs::write(&copy, &audio[..audio.len() - 1]).unwrap();
    let answer = window.get(&stream).await;
    assert!(!answer.local());
    assert_eq!(answer.body, audio);
    std::fs::write(&copy, &audio).unwrap();
    assert!(window.get(&stream).await.local());

    // A track of the Inbox, or none at all, is the server's to answer.
    assert!(!window.get("/api/tracks/999999/stream").await.local());

    // Sync turned off: the app is as it was without a copy.
    switch.turn(false).unwrap();
    local.forget();
    for _ in 0..4 {
        let answer = window.get(&stream).await;
        assert!(!answer.local(), "played from the copy with sync off");
        assert_eq!(answer.body, audio);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // On again, but set up with another server: this one's tracks aren't
    // the copy's.
    switch.turn(true).unwrap();
    Config {
        server: Some("https://other.example/".into()),
        root: Some(root.clone()),
        ..Default::default()
    }
    .save(&settings)
    .unwrap();
    local.forget();
    for _ in 0..4 {
        assert!(!window.get(&stream).await.local());
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
