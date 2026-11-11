//! Pairing with a mock server, and a token the server no longer takes.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE, HOST, LOCATION};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{any, post};
use common::{Log, Window, echo, serve, text, upstream};
use coversync_proxy::{AboutApp, Proxy, Token, Upstream};
use serde_json::{Value, json};
use tokio::net::TcpListener;

fn about() -> AboutApp {
    AboutApp {
        platform: "linux".into(),
        app_version: "0.1.0".into(),
        hostname: "thinkpad".into(),
    }
}

async fn proxy() -> Proxy {
    Proxy::builder()
        .page_timeout(Duration::from_millis(300))
        .start()
        .await
        .unwrap()
}

fn json_answer(status: StatusCode, body: &Value) -> axum::response::Response {
    (
        status,
        [(CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

/// A server whose `/coversync/api/pair` answers with `status` and `body`,
/// recording what it was sent.
async fn pairing_server(
    status: StatusCode,
    body: Value,
) -> (Upstream, Arc<Mutex<Vec<(HeaderMap, Value)>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let router = Router::new().route(
        "/coversync/api/pair",
        post({
            let seen = seen.clone();
            move |headers: HeaderMap, sent: String| async move {
                let sent = serde_json::from_str(&sent).unwrap();
                seen.lock().unwrap().push((headers, sent));
                json_answer(status, &body)
            }
        }),
    );
    (upstream(serve(router).await, "/coversync/"), seen)
}

#[tokio::test]
async fn trades_a_code_for_a_token() {
    let (server, seen) = pairing_server(
        StatusCode::OK,
        json!({
            "device_id": 7, "token": "cst_secret", "name": "Laptop", "scope": "manage",
            "server": { "name": "n5pro", "version": "0.1.0" }, "library_id": null,
        }),
    )
    .await;
    let paired = proxy()
        .await
        .pair(&server, "K7QM4ZTD", &about())
        .await
        .unwrap();
    assert_eq!(paired.device_id, 7);
    assert_eq!(paired.name, "Laptop");
    assert!(paired.manages);
    assert_eq!(paired.token.expose(), "cst_secret");

    let (headers, sent) = seen.lock().unwrap().pop().unwrap();
    assert_eq!(
        sent,
        json!({ "code": "K7QM4ZTD", "platform": "linux", "app_version": "0.1.0", "hostname": "thinkpad" })
    );
    assert_eq!(headers[CONTENT_TYPE], "application/json");
    assert_eq!(headers[HOST], server.origin().trim_start_matches("http://"));
    assert!(!headers.contains_key(AUTHORIZATION));
}

#[tokio::test]
async fn says_what_the_app_doesnt_know_of_itself_by_leaving_it_out() {
    let (server, seen) = pairing_server(
        StatusCode::OK,
        json!({ "device_id": 1, "token": "cst_x", "name": "", "scope": "sync" }),
    )
    .await;
    let paired = proxy()
        .await
        .pair(&server, "K7QM4ZTD", &AboutApp::default())
        .await
        .unwrap();
    assert!(!paired.manages, "a sync-only token can't show the UI");
    let (_, sent) = seen.lock().unwrap().pop().unwrap();
    assert_eq!(sent, json!({ "code": "K7QM4ZTD" }));
}

#[tokio::test]
async fn a_refusal_comes_in_the_servers_words() {
    let (server, _) = pairing_server(
        StatusCode::BAD_REQUEST,
        json!({ "error": "That pairing code isn't right. Check it, or make a new one in Add device.", "code": "wrong_code" }),
    )
    .await;
    let refused = proxy()
        .await
        .pair(&server, "00000000", &about())
        .await
        .unwrap_err();
    assert_eq!(refused.code.as_deref(), Some("wrong_code"));
    assert!(
        refused
            .message
            .starts_with("That pairing code isn't right.")
    );
    assert_eq!(refused.to_string(), refused.message);
}

#[tokio::test]
async fn answers_it_cant_read_are_errors() {
    // Not coversync's JSON at all.
    let plain = Router::new().route(
        "/api/pair",
        any(|| async { (StatusCode::BAD_GATEWAY, "down") }),
    );
    let server = upstream(serve(plain).await, "/");
    let err = proxy()
        .await
        .pair(&server, "K7QM4ZTD", &about())
        .await
        .unwrap_err();
    assert_eq!(err.code, None);
    assert!(err.message.contains("502"), "{}", err.message);

    // A success without a token, and one whose token couldn't be a header.
    for body in [
        json!({ "device_id": 1 }),
        json!({ "token": "cst_x" }),
        json!({ "device_id": 1, "token": "cst_x\r\nX-Injected: 1" }),
        json!({ "device_id": 1, "token": "" }),
        json!("ok"),
    ] {
        let (server, _) = pairing_server(StatusCode::OK, body.clone()).await;
        let err = proxy()
            .await
            .pair(&server, "K7QM4ZTD", &about())
            .await
            .unwrap_err();
        assert!(
            err.message.contains("understand"),
            "{body}: {}",
            err.message
        );
    }
}

#[tokio::test]
async fn a_code_goes_only_to_the_address_named() {
    // The server sends the pairing on to another one.
    let log = Log::default();
    let elsewhere = serve(echo(log.clone())).await;
    let redirecting = Router::new().route(
        "/api/pair",
        any(move || async move {
            (
                StatusCode::TEMPORARY_REDIRECT,
                [(LOCATION, format!("http://{elsewhere}/api/pair"))],
            )
                .into_response()
        }),
    );
    let server = upstream(serve(redirecting).await, "/");
    let err = proxy()
        .await
        .pair(&server, "K7QM4ZTD", &about())
        .await
        .unwrap_err();
    assert!(err.message.contains("307"), "{}", err.message);
    assert_eq!(log.len(), 0, "the code wasn't sent on");
}

#[tokio::test]
async fn a_server_that_cant_be_reached_says_why() {
    // Nothing listens there.
    let closed = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap();
    let err = proxy()
        .await
        .pair(&upstream(closed, "/"), "K7QM4ZTD", &about())
        .await
        .unwrap_err();
    assert!(err.message.contains("refused"), "{}", err.message);

    // It takes the request and never answers.
    let silent = Router::new().route(
        "/api/pair",
        any(|| async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            StatusCode::OK
        }),
    );
    let err = proxy()
        .await
        .pair(&upstream(serve(silent).await, "/"), "K7QM4ZTD", &about())
        .await
        .unwrap_err();
    assert!(err.message.contains("didn't answer"), "{}", err.message);
}

/// A server that takes only the token `cst_good`.
fn guarded(log: Log) -> Router {
    Router::new().fallback(move |req: Request| {
        let log = log.clone();
        async move {
            log.record(&req);
            match req.headers().get(AUTHORIZATION) {
                Some(auth) if auth == "Bearer cst_good" => (StatusCode::OK, "in").into_response(),
                _ => json_answer(
                    StatusCode::UNAUTHORIZED,
                    &json!({ "error": "This app's token isn't valid; pair it again.", "code": "invalid_token" }),
                ),
            }
        }
    })
}

fn with_token(server: &Upstream, token: &str) -> Upstream {
    let token = Token::new(token).unwrap();
    server
        .clone()
        .with_authorization(Some(token.authorization()))
}

#[tokio::test]
async fn tells_the_app_when_its_token_is_refused() {
    let log = Log::default();
    let server = upstream(serve(guarded(log.clone())).await, "/");
    let rejected: Arc<Mutex<Vec<Upstream>>> = Arc::default();
    let window = Window::open_with(
        Proxy::builder()
            .upstream(Some(with_token(&server, "cst_good")))
            .on_rejected({
                let rejected = rejected.clone();
                move |upstream| rejected.lock().unwrap().push(upstream)
            }),
    )
    .await;

    // The token goes with every request, whatever the page sends itself.
    let own = window
        .request(Method::GET, "/api/status")
        .header(AUTHORIZATION, "Bearer the-pages-own")
        .body(Body::empty())
        .unwrap();
    assert_eq!(window.send(own).await.status(), StatusCode::OK);
    assert_eq!(log.last().headers[AUTHORIZATION], "Bearer cst_good");
    assert!(rejected.lock().unwrap().is_empty());

    // The device is removed on the server: the token it has stops working.
    let removed = with_token(&server, "cst_removed");
    window.proxy.set_upstream(Some(removed.clone()));
    let res = window.get("/api/status").await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let body: Value = serde_json::from_str(&text(res).await).unwrap();
    assert_eq!(
        body["code"], "invalid_token",
        "the page gets the answer as it is"
    );
    let told = rejected.lock().unwrap().clone();
    assert_eq!(told, [removed], "with the server and the token refused");
}

#[tokio::test]
async fn a_401_without_a_token_is_the_pages_own_business() {
    let log = Log::default();
    let server = upstream(serve(guarded(log.clone())).await, "/");
    let rejected: Arc<Mutex<Vec<Upstream>>> = Arc::default();
    let window = Window::open_with(Proxy::builder().upstream(Some(server)).on_rejected({
        let rejected = rejected.clone();
        move |upstream| rejected.lock().unwrap().push(upstream)
    }))
    .await;
    // Signed out, as in a browser: the UI shows its login.
    assert_eq!(
        window.get("/api/status").await.status(),
        StatusCode::UNAUTHORIZED
    );
    assert!(rejected.lock().unwrap().is_empty());
}
