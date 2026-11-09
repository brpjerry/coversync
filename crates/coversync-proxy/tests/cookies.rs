//! The servers' cookies, which the proxy keeps apart for each server.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::header::{COOKIE, SET_COOKIE};
use axum::http::{Method, Response};
use common::{Log, Window, serve, upstream};
use coversync_proxy::{Cookies, Proxy};

/// A server that records each request, and sets the cookies its path asks
/// for: `/coversync/set/<name>=<value>[;attributes]`.
fn cookie_server(log: Log) -> Router {
    Router::new().fallback(move |req: Request| {
        let log = log.clone();
        async move {
            log.record(&req);
            let mut res = Response::builder();
            if let Some(cookie) = req.uri().path().strip_prefix("/coversync/set/") {
                let cookie = cookie.replace("%20", " ");
                res = res.header(SET_COOKIE, cookie);
            }
            res.body(Body::empty()).unwrap()
        }
    })
}

/// The `Cookie` header the server got last, if any.
fn last_cookies(log: &Log) -> Option<String> {
    log.last()
        .headers
        .get(COOKIE)
        .map(|c| c.to_str().unwrap().to_owned())
}

/// A file in a folder of its own, removed with it.
struct TempFile(PathBuf);

impl TempFile {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("coversync-proxy-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir.join("cookies.json"))
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.0.parent().unwrap());
    }
}

#[tokio::test]
async fn the_proxy_keeps_the_servers_cookies() {
    let log = Log::default();
    let addr = serve(cookie_server(log.clone())).await;
    let window = Window::open(addr, "/coversync/").await;

    let res = window
        .get("/coversync/set/session=abc;%20Path=/coversync/;%20HttpOnly;%20Max-Age=3600")
        .await;
    assert!(
        !res.headers().contains_key(SET_COOKIE),
        "the webview never sees the server's cookies"
    );

    // The webview sends its own cookie and one it shouldn't have; the server
    // gets the one it set.
    let req = window
        .request(Method::GET, "/coversync/api/status")
        .header(COOKIE, format!("{}; stray=1", window.cookie))
        .body(Body::empty())
        .unwrap();
    window.send(req).await;
    assert_eq!(last_cookies(&log).as_deref(), Some("session=abc"));

    // Its Path holds: nothing outside /coversync/ gets it.
    window.get("/coversync").await;
    assert_eq!(last_cookies(&log), None);

    // Logging out (Max-Age=0) removes it.
    window
        .get("/coversync/set/session=;%20Path=/coversync/;%20Max-Age=0")
        .await;
    window.get("/coversync/api/status").await;
    assert_eq!(last_cookies(&log), None);
}

#[tokio::test]
async fn servers_never_see_each_others_cookies() {
    // Two servers on one host, told apart by their ports, as browsers don't.
    let (log_a, log_b) = (Log::default(), Log::default());
    let a = serve(cookie_server(log_a.clone())).await;
    let b = serve(cookie_server(log_b.clone())).await;
    let window = Window::open(a, "/coversync/").await;

    window
        .get("/coversync/set/session=a;%20Path=/;%20Max-Age=3600")
        .await;
    window.proxy.set_upstream(Some(upstream(b, "/coversync/")));
    window.get("/coversync/api/status").await;
    assert_eq!(last_cookies(&log_b), None, "B never gets A's cookie");

    window
        .get("/coversync/set/session=b;%20Path=/;%20Max-Age=3600")
        .await;
    window.proxy.set_upstream(Some(upstream(a, "/coversync/")));
    window.get("/coversync/api/status").await;
    assert_eq!(last_cookies(&log_a).as_deref(), Some("session=a"));

    window.proxy.set_upstream(Some(upstream(b, "/coversync/")));
    window.get("/coversync/api/status").await;
    assert_eq!(last_cookies(&log_b).as_deref(), Some("session=b"));
}

#[tokio::test]
async fn persistent_cookies_last_across_restarts() {
    let file = TempFile::new("restart");
    let log = Log::default();
    let addr = serve(cookie_server(log.clone())).await;
    let open = || {
        Window::open_with(
            Proxy::builder()
                .upstream(Some(upstream(addr, "/coversync/")))
                .cookies(Cookies::open(Some(file.0.clone()))),
        )
    };

    let first = open().await;
    first
        .get("/coversync/set/session=kept;%20Path=/;%20Max-Age=3600")
        .await;
    first.get("/coversync/set/visit=1;%20Path=/").await;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&file.0).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "only the user can read it");
    }

    // A new proxy, as after the app restarts.
    let second = open().await;
    second.get("/coversync/api/status").await;
    assert_eq!(
        last_cookies(&log).as_deref(),
        Some("session=kept"),
        "a session cookie (no Max-Age or Expires) ends with the run, as in a browser"
    );
}

#[tokio::test]
async fn expired_cookies_are_not_sent() {
    let file = TempFile::new("expired");
    let log = Log::default();
    let addr = serve(cookie_server(log.clone())).await;
    let window = Window::open_with(
        Proxy::builder()
            .upstream(Some(upstream(addr, "/coversync/")))
            .cookies(Cookies::open(Some(file.0.clone()))),
    )
    .await;

    window
        .get("/coversync/set/brief=1;%20Path=/;%20Max-Age=1")
        .await;
    window.get("/coversync/api/status").await;
    assert_eq!(last_cookies(&log).as_deref(), Some("brief=1"));

    tokio::time::sleep(Duration::from_millis(1100)).await;
    window.get("/coversync/api/status").await;
    assert_eq!(last_cookies(&log), None, "expired in this run");

    // Nor after a restart, though it was saved while it was fresh.
    let again = Window::open_with(
        Proxy::builder()
            .upstream(Some(upstream(addr, "/coversync/")))
            .cookies(Cookies::open(Some(file.0.clone()))),
    )
    .await;
    again.get("/coversync/api/status").await;
    assert_eq!(last_cookies(&log), None, "expired in the file");
}

#[tokio::test]
async fn proxies_can_share_a_jar() {
    // The app runs a proxy for each server, on its own port, with one jar.
    let log = Log::default();
    let addr = serve(cookie_server(log.clone())).await;
    let jar = Cookies::default();
    let open = || {
        Window::open_with(
            Proxy::builder()
                .upstream(Some(upstream(addr, "/coversync/")))
                .cookies(jar.clone()),
        )
    };
    let (one, two) = (open().await, open().await);
    assert_ne!(one.proxy.addr(), two.proxy.addr());

    one.get("/coversync/set/session=shared;%20Path=/").await;
    two.get("/coversync/api/status").await;
    assert_eq!(last_cookies(&log).as_deref(), Some("session=shared"));
}
