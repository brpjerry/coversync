//! The guard and the login routes, through the whole app as it is served.

use std::net::SocketAddr;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use serde_json::Value;
use tower::ServiceExt;

use super::*;

const PASSWORD: &str = "correct horse battery";
const LAN: &str = "192.168.1.20:50000";

struct App {
    router: Router,
    state: Shared,
}

fn app(name: &str) -> App {
    let dir = std::env::temp_dir().join(format!("coversyncd-auth-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let state = crate::test_state(&dir);
    App {
        router: crate::app(state.clone()),
        state,
    }
}

impl App {
    fn with_password(name: &str) -> App {
        let app = app(name);
        app.set_password(PASSWORD);
        app
    }

    fn set_password(&self, password: &str) {
        let hash = store::hash_password(password).unwrap();
        self.state
            .auth
            .store()
            .set_password_hash(&hash, now())
            .unwrap();
    }

    fn device(&self, scope: Scope) -> (i64, String) {
        self.state
            .auth
            .store()
            .add_device("Test device", scope, now())
            .unwrap()
    }

    /// Send a request as if from `peer`.
    async fn send_from(&self, peer: &str, mut req: Request<Body>) -> Response {
        let peer: SocketAddr = peer.parse().unwrap();
        req.extensions_mut().insert(ConnectInfo(peer));
        self.router.clone().oneshot(req).await.unwrap()
    }

    async fn send(&self, req: Request<Body>) -> Response {
        self.send_from(LAN, req).await
    }

    async fn get(&self, path: &str, cookie: Option<&str>) -> Response {
        let mut req = Request::get(path);
        if let Some(c) = cookie {
            req = req.header(header::COOKIE, c);
        }
        self.send(req.body(Body::empty()).unwrap()).await
    }

    async fn login_from(&self, peer: &str, password: &str) -> Response {
        self.send_from(peer, login_request(password, "/coversync/"))
            .await
    }

    /// Log in, and return the cookie to send back.
    async fn cookie(&self) -> String {
        let res = self.login_from(LAN, PASSWORD).await;
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        cookie_of(&res)
    }
}

fn login_request(password: &str, path: &str) -> Request<Body> {
    let body = serde_json::json!({ "password": password, "path": path });
    Request::post("/api/login")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn set_cookie(res: &Response) -> &str {
    res.headers()[header::SET_COOKIE].to_str().unwrap()
}

/// The `name=value` part of the response's cookie.
fn cookie_of(res: &Response) -> String {
    set_cookie(res).split(';').next().unwrap().to_owned()
}

async fn body(res: Response) -> Value {
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

async fn code(res: Response) -> (StatusCode, String) {
    let status = res.status();
    let b = body(res).await;
    (status, b["code"].as_str().unwrap_or_default().to_owned())
}

#[tokio::test]
async fn nothing_is_reachable_before_a_password_is_set() {
    let app = app("first");
    let res = app.get("/api/status", None).await;
    assert!(res.headers().contains_key(header::WWW_AUTHENTICATE));
    let (status, code_) = code(res).await;
    assert_eq!(
        (status, code_.as_str()),
        (StatusCode::UNAUTHORIZED, "no_password")
    );
    let res = app.login_from(LAN, PASSWORD).await;
    assert_eq!(
        code(res).await,
        (StatusCode::UNAUTHORIZED, "no_password".into())
    );
    assert_eq!(app.get("/api/hello", None).await.status(), StatusCode::OK);

    // Once it's set, without a restart, the same login works.
    app.set_password(PASSWORD);
    let res = app.get("/api/status", None).await;
    assert_eq!(
        code(res).await,
        (StatusCode::UNAUTHORIZED, "login_required".into())
    );
    let cookie = app.cookie().await;
    assert_eq!(
        app.get("/api/status", Some(&cookie)).await.status(),
        StatusCode::OK
    );
}

/// Every route the API has, and some it doesn't.
const ROUTES: &[(&str, &str)] = &[
    ("GET", "/api/status"),
    ("POST", "/api/rescan"),
    ("GET", "/api/albums"),
    ("GET", "/api/albums/a1"),
    ("GET", "/api/albums/a1/cover"),
    ("GET", "/api/albums/a1/cover/full"),
    ("PUT", "/api/albums/a1/tags"),
    ("GET", "/api/tracks/1/stream"),
    ("GET", "/api/library/setup"),
    ("PUT", "/api/library/setup"),
    ("GET", "/api/inbox/coverage"),
    ("GET", "/api/inbox/setup"),
    ("PUT", "/api/inbox/setup"),
    ("POST", "/api/inbox/unhide"),
    ("GET", "/api/library/preview?structure=x"),
    ("GET", "/api/fs/check?path=/"),
    ("POST", "/api/import/plan"),
    ("POST", "/api/import"),
    ("GET", "/api/appearance"),
    ("PUT", "/api/appearance"),
    ("GET", "/api/sync/setup"),
    ("PUT", "/api/sync/setup"),
    ("POST", "/api/identify"),
    ("GET", "/api/identify/a1"),
    ("GET", "/api/musicbrainz/art/r1"),
    ("GET", "/api/musicbrainz/art/r1/image"),
    ("GET", "/api/session"),
    ("GET", "/api"),
    ("GET", "/api/"),
    ("GET", "/api/no-such-route"),
    ("GET", "/api/hello/"),
    ("DELETE", "/api/login/other"),
];

#[tokio::test]
async fn every_api_route_needs_a_login() {
    let app = App::with_password("routes");
    for (method, path) in ROUTES {
        let req = Request::builder()
            .method(*method)
            .uri(*path)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let (status, c) = code(app.send(req).await).await;
        assert_eq!(
            (status, c.as_str()),
            (StatusCode::UNAUTHORIZED, "login_required"),
            "{method} {path}"
        );
    }
    // The UI's own files are open: it shows its login view on a 401.
    for path in ["/", "/index.html", "/?view=library"] {
        let status = app.get(path, None).await.status();
        assert_ne!(status, StatusCode::UNAUTHORIZED, "{path}");
    }
}

#[tokio::test]
async fn a_login_sets_a_cookie_for_the_ui_path_only() {
    let app = App::with_password("login");
    let res = app.login_from(LAN, "wrong password").await;
    assert_eq!(
        code(res).await,
        (StatusCode::UNAUTHORIZED, "wrong_password".into())
    );

    let res = app.login_from(LAN, PASSWORD).await;
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    let set = set_cookie(&res);
    assert!(set.starts_with("coversync_session="), "{set}");
    for part in ["Path=/coversync/", "HttpOnly", "SameSite=Lax", "Max-Age="] {
        assert!(set.contains(part), "{set} lacks {part}");
    }
    // Plain HTTP (a direct connection, or the dev server): not Secure.
    assert!(!set.contains("Secure"), "{set}");
    let cookie = cookie_of(&res);
    assert_eq!(cookie.len(), "coversync_session=".len() + 43);

    let res = app.get("/api/session", Some(&cookie)).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        body(res).await,
        serde_json::json!({ "via": "password", "scope": "manage", "device": null })
    );
    // Covers and streams too: they're loaded with the cookie alone.
    for path in [
        "/api/albums?collection=inbox",
        "/api/albums/a1/cover",
        "/api/tracks/1/stream",
    ] {
        let status = app.get(path, Some(&cookie)).await.status();
        assert_ne!(status, StatusCode::UNAUTHORIZED, "{path}");
    }
    // Other cookies alongside, and a stale one of ours for another path.
    let mixed = format!("theme=dark; coversync_session=stale; {cookie}");
    assert_eq!(
        app.get("/api/status", Some(&mixed)).await.status(),
        StatusCode::OK
    );

    // Behind Caddy, over HTTPS.
    let req = Request::post("/api/login")
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-forwarded-proto", "https")
        .body(Body::from(
            serde_json::json!({ "password": PASSWORD, "path": "/" }).to_string(),
        ))
        .unwrap();
    let res = app.send_from("127.0.0.1:40000", req).await;
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    let set = set_cookie(&res);
    assert!(set.contains("; Secure") && set.contains("Path=/;"), "{set}");
}

#[tokio::test]
async fn cookie_paths_are_checked() {
    let app = App::with_password("paths");
    for path in [
        "coversync/",
        "",
        "/a;b",
        "/a,b",
        "/a b",
        "/a\nb",
        "/a\u{7f}",
        "/ä",
    ] {
        let res = app.send(login_request(PASSWORD, path)).await;
        assert_eq!(
            code(res).await,
            (StatusCode::BAD_REQUEST, "bad_path".into()),
            "{path:?}"
        );
    }
    let res = app.send(login_request(PASSWORD, "/music/coversync")).await;
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    assert!(set_cookie(&res).contains("Path=/music/coversync;"));
}

#[tokio::test]
async fn logging_out_ends_the_session() {
    let app = App::with_password("logout");
    let cookie = app.cookie().await;
    let other = app.cookie().await;
    let req = Request::post("/api/logout")
        .header(header::COOKIE, &cookie)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"path": "/coversync/"}"#))
        .unwrap();
    let res = app.send(req).await;
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    let set = set_cookie(&res);
    assert!(
        set.starts_with("coversync_session=;") && set.contains("Max-Age=0"),
        "{set}"
    );
    assert!(set.contains("Path=/coversync/"), "{set}");
    let res = app.get("/api/status", Some(&cookie)).await;
    assert_eq!(
        code(res).await,
        (StatusCode::UNAUTHORIZED, "session_expired".into())
    );
    // Another browser stays signed in.
    assert_eq!(
        app.get("/api/status", Some(&other)).await.status(),
        StatusCode::OK
    );
    // Logging out again, or without a session, is fine.
    let req = Request::post("/api/logout").body(Body::empty()).unwrap();
    assert_eq!(app.send(req).await.status(), StatusCode::NO_CONTENT);
}

fn bearer(path: &str, token: &str) -> Request<Body> {
    Request::get(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn app_tokens_open_the_api_by_scope() {
    let app = App::with_password("tokens");
    let (id, token) = app.device(Scope::Manage);
    for (_, path) in ROUTES.iter().filter(|(m, _)| *m == "GET").take(8) {
        let status = app.send(bearer(path, &token)).await.status();
        assert_ne!(status, StatusCode::UNAUTHORIZED, "{path}");
        assert_ne!(status, StatusCode::FORBIDDEN, "{path}");
    }
    let res = app.send(bearer("/api/session", &token)).await;
    assert_eq!(
        body(res).await,
        serde_json::json!({ "via": "token", "scope": "manage", "device": id })
    );
    // The scheme's case doesn't matter.
    let req = Request::get("/api/status")
        .header(header::AUTHORIZATION, format!("bearer {token}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(app.send(req).await.status(), StatusCode::OK);

    let (_, sync) = app.device(Scope::Sync);
    let res = app.send(bearer("/api/albums", &sync)).await;
    assert_eq!(code(res).await, (StatusCode::FORBIDDEN, "sync_only".into()));

    for bad in ["cst_unknown", "not-ours", ""] {
        let res = app.send(bearer("/api/status", bad)).await;
        assert_eq!(
            code(res).await,
            (StatusCode::UNAUTHORIZED, "invalid_token".into()),
            "{bad:?}"
        );
    }
    app.state.auth.store().revoke_device(id, now()).unwrap();
    let res = app.send(bearer("/api/status", &token)).await;
    assert_eq!(
        code(res).await,
        (StatusCode::UNAUTHORIZED, "invalid_token".into())
    );
}

fn trade(token: Option<&str>, path: Option<&str>) -> Request<Body> {
    let mut req =
        Request::post("/api/login/token").header(header::CONTENT_TYPE, "application/json");
    if let Some(t) = token {
        req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    let body = serde_json::json!({ "path": path });
    req.body(Body::from(body.to_string())).unwrap()
}

#[tokio::test]
async fn a_token_can_be_traded_for_a_cookie() {
    let app = App::with_password("trade");
    let (id, token) = app.device(Scope::Manage);
    let res = app.send(trade(Some(&token), Some("/coversync/"))).await;
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    assert!(set_cookie(&res).contains("Path=/coversync/"));
    let cookie = cookie_of(&res);
    let res = app.get("/api/session", Some(&cookie)).await;
    assert_eq!(
        body(res).await,
        serde_json::json!({ "via": "token_session", "scope": "manage", "device": id })
    );
    let session = cookie.split_once('=').unwrap().1;
    let access = app
        .state
        .auth
        .store()
        .session(session, now())
        .unwrap()
        .unwrap()
        .access;
    assert!(!access.password_login());

    let res = app.send(trade(Some("cst_unknown"), Some("/"))).await;
    assert_eq!(
        code(res).await,
        (StatusCode::UNAUTHORIZED, "invalid_token".into())
    );
    let res = app.send(trade(None, Some("/"))).await;
    assert_eq!(
        code(res).await,
        (StatusCode::UNAUTHORIZED, "invalid_token".into())
    );
    let res = app.send(trade(Some(&token), None)).await;
    assert_eq!(
        code(res).await,
        (StatusCode::BAD_REQUEST, "bad_path".into())
    );

    // A sync-only device's cookie is sync-only too.
    let (_, sync) = app.device(Scope::Sync);
    let res = app.send(trade(Some(&sync), Some("/"))).await;
    let sync_cookie = cookie_of(&res);
    let res = app.get("/api/albums", Some(&sync_cookie)).await;
    assert_eq!(code(res).await, (StatusCode::FORBIDDEN, "sync_only".into()));

    // Revoking the device ends the sessions traded for its token.
    app.state.auth.store().revoke_device(id, now()).unwrap();
    let res = app.get("/api/status", Some(&cookie)).await;
    assert_eq!(
        code(res).await,
        (StatusCode::UNAUTHORIZED, "session_expired".into())
    );
}

#[tokio::test]
async fn resetting_the_password_signs_browsers_out() {
    let app = App::with_password("reset");
    let cookie = app.cookie().await;
    let (_, token) = app.device(Scope::Manage);
    app.set_password("a brand new password");
    let res = app.get("/api/status", Some(&cookie)).await;
    assert_eq!(
        code(res).await,
        (StatusCode::UNAUTHORIZED, "session_expired".into())
    );
    let res = app.login_from(LAN, PASSWORD).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let res = app.login_from(LAN, "a brand new password").await;
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    // Apps keep their tokens.
    assert_eq!(
        app.send(bearer("/api/status", &token)).await.status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn failed_logins_are_limited_per_client() {
    let app = App::with_password("limit");
    let me = "192.168.1.30:1000";
    for _ in 0..5 {
        let res = app.login_from(me, "guess").await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }
    // Even the right password waits now.
    let res = app.login_from(me, PASSWORD).await;
    assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
    let retry: u64 = res.headers()[header::RETRY_AFTER]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((1..=60).contains(&retry), "{retry}");
    let b = body(res).await;
    assert_eq!(b["code"], "too_many_attempts");
    assert_eq!(b["retry_after"], retry);
    // Someone else on the LAN isn't held up.
    let res = app.login_from("192.168.1.31:1000", PASSWORD).await;
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    // Claiming another address doesn't help when not coming through a local proxy.
    let req = Request::post("/api/login")
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-forwarded-for", "192.168.1.99")
        .body(Body::from(
            serde_json::json!({ "password": PASSWORD, "path": "/" }).to_string(),
        ))
        .unwrap();
    assert_eq!(
        app.send_from(me, req).await.status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    // A token trade counts the same way.
    let res = app.send_from(me, trade(Some("cst_x"), Some("/"))).await;
    assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
}

fn proxied_login(client: &str, password: &str) -> Request<Body> {
    Request::post("/api/login")
        .header(header::CONTENT_TYPE, "application/json")
        // A client's own header, then the one Caddy adds.
        .header("x-forwarded-for", format!("10.9.9.9, {client}"))
        .body(Body::from(
            serde_json::json!({ "password": password, "path": "/" }).to_string(),
        ))
        .unwrap()
}

#[tokio::test]
async fn behind_the_proxy_each_client_is_limited_on_its_own() {
    let app = App::with_password("proxy");
    let caddy = "127.0.0.1:41000";
    for _ in 0..5 {
        let res = app
            .send_from(caddy, proxied_login("192.168.1.40", "guess"))
            .await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }
    let res = app
        .send_from(caddy, proxied_login("192.168.1.40", PASSWORD))
        .await;
    assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
    // Loopback is no pass: another client through the same proxy is its own.
    let res = app
        .send_from(caddy, proxied_login("192.168.1.41", PASSWORD))
        .await;
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn failed_logins_are_limited_overall() {
    let app = App::with_password("overall");
    for i in 0..20 {
        let res = app.login_from(&format!("10.0.3.{i}:1000"), "guess").await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "attempt {i}");
    }
    let res = app.login_from("10.0.4.1:1000", PASSWORD).await;
    assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn hello_needs_no_login() {
    let app = App::with_password("hello");
    let res = app.get("/api/hello", None).await;
    assert_eq!(res.status(), StatusCode::OK);
    let b = body(res).await;
    assert_eq!(b["app"], "coversync");
    assert_eq!(b["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(b["api"], serde_json::json!(["1"]));
    assert!(b["name"].is_string());
}

#[test]
fn only_the_open_routes_skip_the_guard() {
    for open in OPEN {
        assert!(!guarded(open), "{open}");
    }
    for path in [
        "/api",
        "/api/",
        "/api/status",
        "/api/hello/",
        "/api/login/",
        "/api/hello/ca",
    ] {
        assert!(guarded(path), "{path}");
    }
    for path in ["/", "/index.html", "/_app/x.js", "/apis", "/x/api/status"] {
        assert!(!guarded(path), "{path}");
    }
}

#[test]
fn the_client_address_comes_from_the_proxy_only_on_loopback() {
    let headers = |xff: &[&str]| {
        let mut h = HeaderMap::new();
        for v in xff {
            h.append("x-forwarded-for", v.parse().unwrap());
        }
        h
    };
    let ip = |s: &str| Some(s.parse::<IpAddr>().unwrap());
    let caddy = ip("127.0.0.1");
    assert_eq!(
        client_ip(caddy, &headers(&["192.168.1.5"])),
        ip("192.168.1.5")
    );
    assert_eq!(
        client_ip(caddy, &headers(&["1.2.3.4, 192.168.1.5"])),
        ip("192.168.1.5")
    );
    assert_eq!(
        client_ip(caddy, &headers(&["1.2.3.4", "192.168.1.6"])),
        ip("192.168.1.6")
    );
    assert_eq!(client_ip(ip("::1"), &headers(&["fd00::5"])), ip("fd00::5"));
    assert_eq!(
        client_ip(ip("::ffff:127.0.0.1"), &headers(&["192.168.1.7"])),
        ip("192.168.1.7")
    );
    assert_eq!(client_ip(caddy, &headers(&["garbage"])), caddy);
    assert_eq!(client_ip(caddy, &headers(&[])), caddy);
    let lan = ip("192.168.1.8");
    assert_eq!(client_ip(lan, &headers(&["192.168.1.5"])), lan);
    assert_eq!(client_ip(None, &headers(&["192.168.1.5"])), None);
}

#[test]
fn a_password_from_standard_input() {
    let read = |s: &str| password_line(std::io::Cursor::new(s.as_bytes().to_vec()));
    assert_eq!(read("hunter2 hunter2\n").unwrap(), "hunter2 hunter2");
    assert_eq!(read("hunter2 hunter2\r\n").unwrap(), "hunter2 hunter2");
    assert_eq!(
        read(" spaces kept \nsecond line\n").unwrap(),
        " spaces kept "
    );
    assert_eq!(read("no newline at all").unwrap(), "no newline at all");
    assert!(read("").is_err());
    assert!(read("\n").is_err());
    assert!(read("short\n").is_err());
    assert!(read(&"x".repeat(MAX_PASSWORD + 1)).is_err());
}

#[test]
fn set_password_works_on_a_new_state_directory() {
    let dir = std::env::temp_dir().join(format!("coversyncd-setpw-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    // What the command does after reading the password.
    let db = dir.join(coversync_core::db::LOCAL_DB);
    coversync_core::Library::open_in(&dir).unwrap();
    let store = Store::open(&db).unwrap();
    let hash = store::hash_password(PASSWORD).unwrap();
    assert!(!store.set_password_hash(&hash, now()).unwrap());
    // The service, started afterwards, finds it.
    let auth = Auth::open(&dir).unwrap();
    let stored = auth.store().password_hash().unwrap().unwrap();
    assert!(store::verify_password(PASSWORD, &stored));
}
