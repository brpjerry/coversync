//! Tracks this computer holds are played from its copy: the proxy answers
//! for them itself, as the server would, and asks the server for everything
//! else.

mod common;

use std::io::Write;
use std::sync::{Arc, Mutex};

use axum::http::header::{
    ACCEPT_RANGES, AUTHORIZATION, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, COOKIE, RANGE,
};
use axum::http::{HeaderValue, Method, StatusCode};
use common::{Log, Window, echo, serve, text, upstream};
use coversync_proxy::{Asker, LOCAL_HEADER, LocalFile, LocalFiles, Proxy};

const AUDIO: &[u8] = b"0123456789abcdefghij";

/// A copy holding one track, number 7, which notes who asked.
struct Copy {
    file: std::path::PathBuf,
    asked: Mutex<Vec<i64>>,
    asker: Mutex<Option<Asker>>,
}

impl Copy {
    fn new(name: &str) -> Arc<Self> {
        let dir = std::env::temp_dir().join(format!(
            "coversync-proxy-local-{name}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("07.flac");
        std::fs::File::create(&file)
            .unwrap()
            .write_all(AUDIO)
            .unwrap();
        Arc::new(Self {
            file,
            asked: Mutex::default(),
            asker: Mutex::default(),
        })
    }
}

impl LocalFiles for Copy {
    fn track(&self, id: i64, ask: &Asker) -> Option<LocalFile> {
        self.asked.lock().unwrap().push(id);
        *self.asker.lock().unwrap() = Some(ask.clone());
        (id == 7).then(|| LocalFile {
            file: std::fs::File::open(&self.file).unwrap(),
            len: AUDIO.len() as u64,
            content_type: "audio/flac",
        })
    }
}

async fn window_with(copy: &Arc<Copy>, prefix: &str) -> (Window, Log) {
    let log = Log::default();
    let addr = serve(echo(log.clone())).await;
    let server = upstream(addr, prefix)
        .with_authorization(Some(HeaderValue::from_static("Bearer cst_example")));
    let window = Window::open_with(
        Proxy::builder()
            .upstream(Some(server))
            .local_files(copy.clone()),
    )
    .await;
    (window, log)
}

// Bugs caught: 0, False positives: 0
#[tokio::test]
async fn a_track_this_computer_holds_is_played_from_its_copy() {
    let copy = Copy::new("held");
    for (prefix, path) in [
        ("/coversync/", "/coversync/api/tracks/7/stream"),
        ("/", "/api/tracks/7/stream"),
    ] {
        let (window, log) = window_with(&copy, prefix).await;
        // All of it.
        let res = window.get(path).await;
        assert_eq!(res.status(), StatusCode::OK, "{prefix}");
        assert_eq!(res.headers()[LOCAL_HEADER], "1");
        assert_eq!(res.headers()[CONTENT_TYPE], "audio/flac");
        assert_eq!(res.headers()[ACCEPT_RANGES], "bytes");
        assert_eq!(res.headers()[CONTENT_LENGTH], "20");
        assert_eq!(text(res).await.as_bytes(), AUDIO);

        // A range, as the player asks when it seeks.
        let ranged = |range: &'static str| {
            window.send(
                window
                    .request(Method::GET, path)
                    .header(RANGE, range)
                    .body("".into())
                    .unwrap(),
            )
        };
        let res = ranged("bytes=2-5").await;
        assert_eq!(res.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(res.headers()[CONTENT_RANGE], "bytes 2-5/20");
        assert_eq!(res.headers()[CONTENT_LENGTH], "4");
        assert_eq!(text(res).await, "2345");
        let res = ranged("bytes=15-").await;
        assert_eq!(res.headers()[CONTENT_RANGE], "bytes 15-19/20");
        assert_eq!(text(res).await, "fghij");
        let res = ranged("bytes=-3").await;
        assert_eq!(res.headers()[CONTENT_RANGE], "bytes 17-19/20");
        assert_eq!(text(res).await, "hij");
        let res = ranged("bytes=20-").await;
        assert_eq!(res.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(res.headers()[CONTENT_RANGE], "bytes */20");

        // Its headers alone.
        let res = window
            .send(window.request(Method::HEAD, path).body("".into()).unwrap())
            .await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()[CONTENT_LENGTH], "20");
        assert_eq!(text(res).await, "");

        assert_eq!(log.len(), 0, "the server was asked for a held track");
    }
}

// Bugs caught: 0, False positives: 0
#[tokio::test]
async fn everything_else_still_comes_from_the_server() {
    let copy = Copy::new("not-held");
    let (window, log) = window_with(&copy, "/coversync/").await;
    // A track the copy doesn't have.
    let res = window.get("/coversync/api/tracks/8/stream").await;
    assert!(res.headers().get(LOCAL_HEADER).is_none());
    assert_eq!(log.last().uri, "/coversync/api/tracks/8/stream");
    assert_eq!(*copy.asked.lock().unwrap(), [8]);
    // What isn't a track's audio is never looked for in the copy.
    for other in [
        "/coversync/api/tracks/7",
        "/coversync/api/albums/7/cover",
        "/coversync/api/tracks/7/stream/more",
        "/coversync/",
    ] {
        let before = log.len();
        window.get(other).await;
        assert_eq!(log.len(), before + 1, "{other}");
    }
    let before = log.len();
    window
        .send(
            window
                .request(Method::POST, "/coversync/api/tracks/7/stream")
                .body("x".into())
                .unwrap(),
        )
        .await;
    assert_eq!(log.len(), before + 1, "a POST");
    assert_eq!(*copy.asked.lock().unwrap(), [8]);
}

// Bugs caught: 0, False positives: 0
#[tokio::test]
async fn the_app_asks_the_server_as_this_computers_device() {
    let copy = Copy::new("asker");
    let (window, log) = window_with(&copy, "/coversync/").await;
    window.get("/coversync/api/tracks/7/stream").await;
    let asker = copy.asker.lock().unwrap().clone().unwrap();
    assert_eq!(asker.server().url(), window.proxy.upstream().unwrap().url());
    // The echo server answers with the request's body: none.
    let answer = asker.get("/api/sync/v1/inventory?after=a%20b").await;
    assert_eq!(answer, Ok(Vec::new()));
    let seen = log.last();
    assert_eq!(seen.uri, "/coversync/api/sync/v1/inventory?after=a%20b");
    assert_eq!(seen.headers[AUTHORIZATION], "Bearer cst_example");
    // The window's sign-in is no part of it.
    assert!(seen.headers.get(COOKIE).is_none());

    // The token it asks with has a mark, the same each time.
    let mark = asker.device_mark();
    assert!(mark.is_some());
    window.get("/coversync/api/tracks/7/stream").await;
    let again = copy.asker.lock().unwrap().clone().unwrap();
    assert_eq!(again.device_mark(), mark);

    // A computer that isn't paired has nothing to ask with.
    let unpaired = Window::open_with(
        Proxy::builder()
            .upstream(Some(upstream(serve(echo(Log::default())).await, "/")))
            .local_files(copy.clone()),
    )
    .await;
    unpaired.get("/api/tracks/7/stream").await;
    let asker = copy.asker.lock().unwrap().clone().unwrap();
    assert!(asker.get("/api/sync/v1/inventory").await.is_err());
    assert_eq!(asker.device_mark(), None);
}
