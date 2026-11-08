//! The upload API through the whole app, as it is served.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path as FsPath, PathBuf};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, StatusCode, header};
use axum::response::Response;
use coversync_core::{InboxSetup, LibrarySetup, TagEdit, tags};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::ServiceExt;

use crate::Shared;
use crate::auth::{Scope, store};

const PASSWORD: &str = "correct horse battery";
const LAN: &str = "192.168.1.20:50000";

struct App {
    router: Router,
    state: Shared,
    dir: PathBuf,
    inbox: PathBuf,
    token: String,
}

fn now() -> i64 {
    super::now()
}

/// A daemon with an Inbox folder, a password, and an app token that may
/// manage the Library.
fn app(name: &str) -> App {
    let dir =
        std::env::temp_dir().join(format!("coversyncd-uploads-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let inbox = dir.join("Inbox");
    fs::create_dir_all(&inbox).unwrap();
    let dir = dir.canonicalize().unwrap();
    let inbox = inbox.canonicalize().unwrap();
    let state = crate::test_state(&dir);
    state.auth.stop_clock();
    let hash = store::hash_password(PASSWORD).unwrap();
    state.auth.store().set_password_hash(&hash, now()).unwrap();
    state
        .lib
        .set_inbox_setup(InboxSetup {
            folder: Some(inbox.display().to_string()),
            ..Default::default()
        })
        .unwrap();
    super::follow_folder(&state);
    let (_, token) = state
        .auth
        .store()
        .add_device("Desktop", Scope::Manage, now())
        .unwrap();
    App {
        router: crate::app(state.clone()),
        state,
        dir,
        inbox,
        token,
    }
}

impl App {
    async fn send(&self, mut req: Request<Body>) -> Response {
        req.extensions_mut()
            .insert(ConnectInfo(LAN.parse::<SocketAddr>().unwrap()));
        self.router.clone().oneshot(req).await.unwrap()
    }

    fn request(&self, method: Method, path: &str) -> axum::http::request::Builder {
        Request::builder()
            .method(method)
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {}", self.token))
    }

    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let req = self.request(method, path);
        let req = match body {
            Some(b) => req
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(b.to_string())),
            None => req.body(Body::empty()),
        };
        reply(self.send(req.unwrap()).await).await
    }

    async fn start(&self, folder: &str, files: &[(&str, &[u8])]) -> (StatusCode, Value) {
        let files: Vec<Value> = files
            .iter()
            .map(|(p, b)| json!({ "path": p, "size": b.len() }))
            .collect();
        let body = json!({ "folder": folder, "files": files });
        self.call(Method::POST, "/api/uploads", Some(body)).await
    }

    async fn put(
        &self,
        id: &str,
        index: usize,
        offset: usize,
        bytes: &[u8],
    ) -> (StatusCode, Value) {
        let req = self
            .request(
                Method::PUT,
                &format!("/api/uploads/{id}/files/{index}?offset={offset}"),
            )
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .body(Body::from(bytes.to_vec()))
            .unwrap();
        reply(self.send(req).await).await
    }

    async fn inbox_albums(&self) -> Vec<Value> {
        let (status, albums) = self.call(Method::GET, "/api/albums", None).await;
        assert_eq!(status, StatusCode::OK);
        albums.as_array().unwrap().clone()
    }

    /// Wait for the scan an upload's finish started to show `title`.
    async fn album_appears(&self, title: &str) -> Value {
        for _ in 0..200 {
            if let Some(a) = self
                .inbox_albums()
                .await
                .into_iter()
                .find(|a| a["title"] == title)
            {
                return a;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("{title} never showed up in the Inbox");
    }

    /// Scan now, as a rescan would, once any scan under way is done.
    async fn scan(&self) {
        let _guard = self.state.jobs.lock().await;
        let s = self.state.clone();
        tokio::task::spawn_blocking(move || {
            coversync_core::scan::scan(&s.lib, &s.covers, &s.roots(), &s.progress).unwrap()
        })
        .await
        .unwrap();
    }

    /// Serve the app on a port of its own, for requests a client cuts short.
    async fn serve(&self) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app =
            crate::app(self.state.clone()).into_make_service_with_connect_info::<SocketAddr>();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        addr
    }
}

async fn reply(res: Response) -> (StatusCode, Value) {
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// A tiny WAV tagged as track `n` of `album`, as bytes.
fn wav(dir: &FsPath, title: &str, album: &str, n: u32) -> Vec<u8> {
    let samples = [0u8; 1600];
    let mut b = Vec::new();
    b.extend(b"RIFF");
    b.extend(((4 + 24 + 8 + samples.len()) as u32).to_le_bytes());
    b.extend(b"WAVEfmt ");
    b.extend(16u32.to_le_bytes());
    for v in [1u16, 1] {
        b.extend(v.to_le_bytes());
    }
    b.extend(8000u32.to_le_bytes());
    b.extend(16000u32.to_le_bytes());
    for v in [2u16, 16] {
        b.extend(v.to_le_bytes());
    }
    b.extend(b"data");
    b.extend((samples.len() as u32).to_le_bytes());
    b.extend(samples);
    fs::create_dir_all(dir).unwrap();
    let path = dir.join(format!("{album}-{n}-{title}.wav"));
    fs::write(&path, b).unwrap();
    let edit = TagEdit {
        title: Some(title.into()),
        album: Some(album.into()),
        artist: Some("Someone".into()),
        track_no: Some(n.to_string()),
        ..Default::default()
    };
    tags::write(&path, &edit).unwrap();
    fs::read(path).unwrap()
}

fn staged(app: &App) -> Vec<PathBuf> {
    match fs::read_dir(
        app.inbox
            .join("Uploads")
            .join(coversync_core::upload::STAGING),
    ) {
        Ok(entries) => entries.flatten().map(|e| e.path()).collect(),
        Err(_) => Vec::new(),
    }
}

#[tokio::test]
async fn uploads_need_a_login_that_may_manage() {
    let app = app("login");
    let id = app.start("A", &[("a.flac", b"abc")]).await.1["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let routes = [
        (Method::GET, "/api/uploads".to_owned()),
        (Method::POST, "/api/uploads".to_owned()),
        (Method::GET, format!("/api/uploads/{id}")),
        (Method::PUT, format!("/api/uploads/{id}/files/0?offset=0")),
        (Method::POST, format!("/api/uploads/{id}/finish")),
        (Method::DELETE, format!("/api/uploads/{id}")),
    ];
    let (_, sync) = app
        .state
        .auth
        .store()
        .add_device("Phone", Scope::Sync, now())
        .unwrap();
    for (method, path) in &routes {
        let anonymous = Request::builder()
            .method(method.clone())
            .uri(path)
            .body(Body::from("abc"))
            .unwrap();
        let (status, body) = reply(app.send(anonymous).await).await;
        assert_eq!(
            (status, body["code"].as_str()),
            (StatusCode::UNAUTHORIZED, Some("login_required")),
            "{method} {path}"
        );
        let syncing = Request::builder()
            .method(method.clone())
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {sync}"))
            .body(Body::from("abc"))
            .unwrap();
        let (status, body) = reply(app.send(syncing).await).await;
        assert_eq!(
            (status, body["code"].as_str()),
            (StatusCode::FORBIDDEN, Some("sync_only")),
            "{method} {path}"
        );
    }
    // Nothing got through: the upload is as it was.
    let (status, up) = app
        .call(Method::GET, &format!("/api/uploads/{id}"), None)
        .await;
    assert_eq!((status, up["received"].as_u64()), (StatusCode::OK, Some(0)));

    // A browser's session works as well as a token.
    let login = Request::post("/api/login")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({ "password": PASSWORD, "path": "/" }).to_string(),
        ))
        .unwrap();
    let res = app.send(login).await;
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    let cookie = res.headers()[header::SET_COOKIE].to_str().unwrap();
    let cookie = cookie.split(';').next().unwrap().to_owned();
    let put = Request::put(format!("/api/uploads/{id}/files/0?offset=0"))
        .header(header::COOKIE, cookie)
        .body(Body::from("abc"))
        .unwrap();
    let (status, file) = reply(app.send(put).await).await;
    assert_eq!(
        (status, file["complete"].as_bool()),
        (StatusCode::OK, Some(true))
    );
}

#[tokio::test]
async fn an_album_uploads_in_chunks_resumes_and_appears_in_the_inbox() {
    let app = app("album");
    let src = app.dir.join("src");
    let one = wav(&src, "One", "Two Discs", 1);
    let two = wav(&src, "Two", "Two Discs", 2);
    let three = wav(&src, "Three", "Two Discs", 1);
    let (status, up) = app
        .start(
            "Two Discs",
            &[
                ("CD1/01 One.wav", &one),
                ("CD1/02 Two.wav", &two),
                ("CD2/01 Three.wav", &three),
                ("rip.log", b""),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{up}");
    let id = up["id"].as_str().unwrap().to_owned();
    assert_eq!(up["files"][3]["complete"], true);
    assert_eq!(up["chunk_size"], coversync_core::upload::CHUNK_SIZE);

    // File 0 in two chunks, the second first; file 1 in one.
    let half = one.len() / 2;
    let (status, file) = app.put(&id, 0, half, &one[half..]).await;
    assert_eq!(status, StatusCode::OK, "{file}");
    assert_eq!(file["ranges"], json!([[half, one.len()]]));
    let (_, file) = app.put(&id, 0, 0, &one[..half]).await;
    assert_eq!(file["complete"], true);
    assert_eq!(app.put(&id, 1, 0, &two).await.0, StatusCode::OK);

    // File 2's connection is cut part way through its chunk.
    let addr = app.serve().await;
    let mut conn = tokio::net::TcpStream::connect(addr).await.unwrap();
    let head = format!(
        "PUT /api/uploads/{id}/files/2?offset=0 HTTP/1.1\r\nHost: test\r\nAuthorization: Bearer {}\r\nContent-Length: {}\r\n\r\n",
        app.token,
        three.len()
    );
    conn.write_all(head.as_bytes()).await.unwrap();
    let cut = 1000;
    conn.write_all(&three[..cut]).await.unwrap();
    conn.shutdown().await.unwrap();
    drop(conn);
    let mut got = Value::Null;
    for _ in 0..200 {
        got = app
            .call(Method::GET, &format!("/api/uploads/{id}"), None)
            .await
            .1;
        if got["files"][2]["received"] == cut {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(got["files"][2]["ranges"], json!([[0, cut]]), "{got}");
    assert_eq!(got["received"], one.len() + two.len() + cut);

    // Half-finished uploads stay out of the Inbox, and can't be finished.
    app.scan().await;
    assert!(app.inbox_albums().await.is_empty());
    let finish = format!("/api/uploads/{id}/finish");
    let (status, err) = app.call(Method::POST, &finish, None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        err["error"].as_str().unwrap().contains("CD2/01 Three.wav"),
        "{err}"
    );

    // The rest, from where it stopped.
    let (_, file) = app.put(&id, 2, cut, &three[cut..]).await;
    assert_eq!(file["complete"], true);
    let (status, done) = app.call(Method::POST, &finish, None).await;
    assert_eq!(status, StatusCode::OK, "{done}");
    let folder = app.inbox.join("Uploads/Two Discs");
    assert_eq!(done["path"], folder.display().to_string());
    assert_eq!(fs::read(folder.join("CD2/01 Three.wav")).unwrap(), three);
    assert!(folder.join("rip.log").is_file());
    assert!(staged(&app).is_empty());

    // Both discs make one album.
    let album = app.album_appears("Two Discs").await;
    assert_eq!(album["track_count"], 3);
    let (_, detail) = app
        .call(
            Method::GET,
            &format!("/api/albums/{}", album["id"].as_str().unwrap()),
            None,
        )
        .await;
    assert_eq!(detail["dir"], folder.display().to_string());
    let (status, _) = app
        .call(Method::GET, &format!("/api/uploads/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn bad_paths_and_chunks_are_refused() {
    let app = app("refused");
    for bad in [
        "../escape.flac",
        "/etc/escape.flac",
        "CD1/../../escape.flac",
        "a.exe",
    ] {
        let (status, err) = app.start("A", &[(bad, b"x")]).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
        assert!(err["error"].as_str().unwrap().contains(bad), "{err}");
    }
    let (status, _) = app.start("../A", &[("a.flac", b"x")]).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(staged(&app).is_empty());

    let (_, up) = app.start("A", &[("a.flac", b"0123456789")]).await;
    let id = up["id"].as_str().unwrap();
    // Past the end, by its offset or by the length it says it has.
    assert_eq!(
        app.put(id, 0, 11, b"").await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let (status, err) = app.put(id, 0, 5, b"too long").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(err["error"].as_str().unwrap().contains("10 bytes"), "{err}");
    assert_eq!(app.put(id, 1, 0, b"x").await.0, StatusCode::NOT_FOUND);
    assert_eq!(app.put("nope", 0, 0, b"x").await.0, StatusCode::NOT_FOUND);
    // A body sent without a length that turns out too long is refused too.
    let addr = app.serve().await;
    let mut conn = tokio::net::TcpStream::connect(addr).await.unwrap();
    let req = format!(
        "PUT /api/uploads/{id}/files/0?offset=0 HTTP/1.1\r\nHost: test\r\nAuthorization: Bearer {}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n4\r\n0123\r\n8\r\n456789xx\r\n0\r\n\r\n",
        app.token
    );
    conn.write_all(req.as_bytes()).await.unwrap();
    let mut answer = String::new();
    conn.read_to_string(&mut answer).await.unwrap();
    assert!(answer.starts_with("HTTP/1.1 422"), "{answer}");

    // Cancelled: gone, with what it staged.
    let path = format!("/api/uploads/{id}");
    assert_eq!(
        app.call(Method::DELETE, &path, None).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        app.call(Method::GET, &path, None).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        app.put(id, 0, 0, b"0123456789").await.0,
        StatusCode::NOT_FOUND
    );
    assert!(staged(&app).is_empty());
}

#[tokio::test]
async fn an_existing_folder_is_never_overwritten() {
    let app = app("clash");
    let mine = app.inbox.join("Uploads/A");
    fs::create_dir_all(&mine).unwrap();
    fs::write(mine.join("a.flac"), b"mine").unwrap();
    let (_, up) = app.start("A", &[("a.flac", b"theirs")]).await;
    let id = up["id"].as_str().unwrap();
    app.put(id, 0, 0, b"theirs").await;
    let (status, done) = app
        .call(Method::POST, &format!("/api/uploads/{id}/finish"), None)
        .await;
    assert_eq!(
        (status, done["folder"].as_str()),
        (StatusCode::OK, Some("A (2)"))
    );
    assert_eq!(fs::read(mine.join("a.flac")).unwrap(), b"mine");
}

#[tokio::test]
async fn a_read_only_inbox_takes_uploads_in_a_folder_of_their_own() {
    use std::os::unix::fs::PermissionsExt;
    // Root writes anywhere, so no folder is read-only to it.
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let app = app("readonly");
    let src = app.dir.join("src");
    let song = wav(&src, "Song", "Elsewhere", 1);
    fs::set_permissions(&app.inbox, fs::Permissions::from_mode(0o555)).unwrap();
    let (status, err) = app.start("Elsewhere", &[("01.wav", &song)]).await;
    let message = err["error"].as_str().unwrap_or_default().to_owned();
    let (_, listed) = app.call(Method::GET, "/api/uploads", None).await;
    fs::set_permissions(&app.inbox, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(status, StatusCode::FORBIDDEN, "{err}");
    assert!(
        message.contains("Uploads") && message.contains("Inbox setup"),
        "{message}"
    );
    assert_eq!(listed["folder"]["writable"], false);
    assert_eq!(listed["folder"]["creatable"], false);

    // Inbox setup points uploads elsewhere, outside the Inbox folder.
    let (_, setup) = app.call(Method::GET, "/api/inbox/setup", None).await;
    let mut body = setup["setup"].clone();
    let library = app.dir.join("Library");
    let save = |body: Value| app.call(Method::PUT, "/api/inbox/setup", Some(body));
    body["uploads"] = json!(app.dir.display().to_string());
    assert_eq!(
        save(body.clone()).await.0,
        StatusCode::UNPROCESSABLE_ENTITY,
        "holds the Inbox"
    );
    app.state
        .lib
        .set_library_setup(LibrarySetup {
            folder: Some(library.display().to_string()),
            ..Default::default()
        })
        .unwrap();
    body["uploads"] = json!(library.join("Up").display().to_string());
    assert_eq!(
        save(body.clone()).await.0,
        StatusCode::UNPROCESSABLE_ENTITY,
        "in the Library"
    );
    let uploads = app.dir.join("Uploads");
    body["uploads"] = json!(uploads.display().to_string());
    let (status, saved) = save(body).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["uploads"]["path"], uploads.display().to_string());
    assert_eq!(saved["uploads"]["creatable"], true);

    let (status, up) = app.start("Elsewhere", &[("01.wav", &song)]).await;
    assert_eq!(status, StatusCode::CREATED, "{up}");
    let id = up["id"].as_str().unwrap();
    app.put(id, 0, 0, &song).await;
    let (status, done) = app
        .call(Method::POST, &format!("/api/uploads/{id}/finish"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{done}");
    assert_eq!(
        done["path"],
        uploads.join("Elsewhere").display().to_string()
    );
    // Scanned as part of the Inbox.
    let album = app.album_appears("Elsewhere").await;
    assert_eq!(album["collection"], "inbox");
    let (_, detail) = app
        .call(
            Method::GET,
            &format!("/api/albums/{}", album["id"].as_str().unwrap()),
            None,
        )
        .await;
    assert_eq!(detail["tracks"][0]["rel_path"], "Elsewhere/01.wav");
}
