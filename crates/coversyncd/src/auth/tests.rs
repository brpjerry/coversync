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

#[derive(Clone)]
struct App {
    router: Router,
    state: Shared,
}

fn app(name: &str) -> App {
    let dir = std::env::temp_dir().join(format!("coversyncd-auth-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let state = crate::test_state(&dir);
    // However slow the password checks, no try comes back mid-test.
    state.auth.stop_clock();
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

/// Every path the routers in `api.rs` and here register, with `{params}`
/// filled in. axum can't list a router's routes, so they're read from the
/// source: a route added there is checked here without being listed.
fn registered_paths() -> Vec<String> {
    let mut paths = Vec::new();
    for source in [include_str!("../api.rs"), include_str!("mod.rs")] {
        for (at, _) in source.match_indices(".route(") {
            let Some(rest) = source[at + ".route(".len()..]
                .trim_start()
                .strip_prefix('"')
            else {
                continue;
            };
            let path = &rest[..rest.find('"').unwrap()];
            let filled: Vec<&str> = path
                .split('/')
                .map(|seg| if seg.starts_with('{') { "x1" } else { seg })
                .collect();
            paths.push(filled.join("/"));
        }
    }
    paths
}

/// Paths the API doesn't have, which must not be open either.
const UNKNOWN: &[&str] = &[
    "/api",
    "/api/",
    "/api/no-such-route",
    "/api/hello/",
    "/api/login/other",
];

#[tokio::test]
async fn every_api_route_needs_a_login() {
    let app = App::with_password("routes");
    let registered = registered_paths();
    assert!(registered.len() >= 25, "{registered:?}");
    assert!(registered.contains(&"/api/tracks/x1/stream".to_owned()));
    let guarded: Vec<&str> = registered
        .iter()
        .map(String::as_str)
        .filter(|p| !OPEN.contains(p))
        .chain(UNKNOWN.iter().copied())
        .collect();
    for path in guarded {
        for method in ["GET", "POST", "PUT", "DELETE"] {
            let req = Request::builder()
                .method(method)
                .uri(path)
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
    }
    // Only the open ones are, and they're all registered.
    for open in OPEN {
        assert!(registered.iter().any(|p| p == open), "{open}");
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
    // Other characters come percent-encoded, as in the URL.
    let res = app.send(login_request(PASSWORD, "/m%C3%BCsik/")).await;
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    assert!(set_cookie(&res).contains("Path=/m%C3%BCsik/;"));
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
    for path in registered_paths()
        .iter()
        .filter(|p| !OPEN.contains(&p.as_str()))
    {
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
async fn the_event_stream_needs_a_login_and_ends_with_it() {
    let app = App::with_password("events");
    let res = app.get("/api/events", None).await;
    assert_eq!(
        code(res).await,
        (StatusCode::UNAUTHORIZED, "login_required".into())
    );
    let cookie = app.cookie().await;
    let res = app.get("/api/events", Some(&cookie)).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()[header::CONTENT_TYPE], "text/event-stream");
    drop(res);
    // What an open stream checks, every ping, to go on.
    let headers = |cookie: &str| {
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, HeaderValue::from_str(cookie).unwrap());
        h
    };
    assert!(still_valid(&app.state, &headers(&cookie)).await);
    let other = app.cookie().await;
    let req = Request::post("/api/logout")
        .header(header::COOKIE, &cookie)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"path": "/coversync/"}"#))
        .unwrap();
    assert_eq!(app.send(req).await.status(), StatusCode::NO_CONTENT);
    assert!(!still_valid(&app.state, &headers(&cookie)).await);
    assert!(still_valid(&app.state, &headers(&other)).await);
    app.set_password("a brand new password");
    assert!(!still_valid(&app.state, &headers(&other)).await);
    assert!(!still_valid(&app.state, &HeaderMap::new()).await);
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
    assert_eq!(retry, 60);
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
    // The overall bucket is empty, but the right password from a client with
    // tries left still gets in.
    let res = app.login_from("10.0.4.1:1000", PASSWORD).await;
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    // A wrong one now costs its client every try it had left.
    let res = app.login_from("10.0.4.2:1000", "guess").await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let res = app.login_from("10.0.4.2:1000", PASSWORD).await;
    assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(res.headers()[header::RETRY_AFTER], "60");
}

#[tokio::test]
async fn what_a_login_is_served_stays_in_the_browser() {
    let app = App::with_password("private");
    let cookie = app.cookie().await;
    for path in [
        "/api/status",
        "/api/session",
        "/api/albums",
        "/api/appearance",
    ] {
        let res = app.get(path, Some(&cookie)).await;
        assert_eq!(res.status(), StatusCode::OK, "{path}");
        assert_eq!(
            res.headers()[header::CACHE_CONTROL],
            "private, no-cache",
            "{path}"
        );
    }
    // Responses that set their own policy keep it, made private.
    let res = |cache: Option<&'static str>| {
        let mut r = StatusCode::OK.into_response();
        if let Some(c) = cache {
            r.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static(c));
        }
        keep_private(&mut r);
        r.headers()[header::CACHE_CONTROL]
            .to_str()
            .unwrap()
            .to_owned()
    };
    assert_eq!(res(None), "private, no-cache");
    assert_eq!(res(Some("no-cache")), "private, no-cache");
    assert_eq!(
        res(Some("private, max-age=31536000, immutable")),
        "private, max-age=31536000, immutable"
    );
    assert_eq!(res(Some("public, max-age=60")), "private, max-age=60");
    assert_eq!(res(Some("no-store")), "no-store");
}

#[tokio::test]
async fn a_session_traded_for_a_token_counts_as_the_device_being_used() {
    let app = App::with_password("lastseen");
    let (id, token) = app.device(Scope::Manage);
    let res = app.send(trade(Some(&token), Some("/"))).await;
    let cookie = cookie_of(&res);
    let seen = || -> Option<i64> { app.state.auth.store().device_last_seen(id).unwrap() };
    assert!(seen().is_some(), "the trade itself is a use");
    app.state.auth.store().forget_last_seen(id).unwrap();
    assert_eq!(
        app.get("/api/status", Some(&cookie)).await.status(),
        StatusCode::OK
    );
    // Written in the background.
    for _ in 0..250 {
        if seen().is_some() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("a request through the traded session didn't note the device's use");
}

#[tokio::test]
async fn logins_given_up_on_keep_their_check_slot_until_it_ends() {
    use std::sync::atomic::Ordering::SeqCst;
    use std::time::Duration;
    let app = App::with_password("slots");
    let probe = &app.state.auth.probe;
    *probe.check_delay.lock().unwrap() = Duration::from_millis(400);
    // Rounds of logins whose clients give up while their passwords are being
    // checked; each round would take freed slots at once.
    for round in 0..3 {
        let logins: Vec<_> = (0..HASHING * 2)
            .map(|i| {
                let app = app.clone();
                tokio::spawn(async move {
                    app.login_from(&format!("10.2.{round}.{i}:1000"), "guess")
                        .await
                })
            })
            .collect();
        // The first round fills the slots; later ones would take any freed.
        if round == 0 {
            for _ in 0..250 {
                if probe.running.load(SeqCst) == HASHING {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        tokio::time::sleep(Duration::from_millis(60)).await;
        for login in logins {
            login.abort();
        }
    }
    for _ in 0..200 {
        if probe.running.load(SeqCst) == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(probe.running.load(SeqCst), 0);
    let most = probe.most_running.load(SeqCst);
    assert_eq!(most, HASHING, "{most} checks ran at once");
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
