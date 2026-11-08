//! The proxy against a mock server.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::header::{
    ACCEPT, ACCEPT_RANGES, ALT_SVC, AUTHORIZATION, CACHE_CONTROL, CONNECTION, CONTENT_LENGTH,
    CONTENT_RANGE, CONTENT_TYPE, COOKIE, ETAG, HOST, IF_NONE_MATCH, IF_RANGE, LOCATION, ORIGIN,
    PROXY_AUTHORIZATION, RANGE, REFERER, SET_COOKIE, STRICT_TRANSPORT_SECURITY,
};
use axum::http::{HeaderValue, Method, Response, StatusCode};
use axum::response::{IntoResponse, Redirect};
use bytes::Bytes;
use common::{
    Log, WAIT, Window, bytes, channel_body, client, echo, pattern, serve, text, upstream,
};
use coversync_proxy::{Cookies, Proxy, Unreachable};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::time::timeout;

const MIB: usize = 1 << 20;

/// Reads frames until `len` bytes have come, or fails if they don't come.
async fn read_exactly(body: &mut Incoming, len: usize) -> Vec<u8> {
    let mut got = Vec::new();
    while got.len() < len {
        let frame = timeout(WAIT, body.frame())
            .await
            .expect("the bytes sent so far arrive")
            .expect("the body goes on")
            .unwrap();
        if let Ok(data) = frame.into_data() {
            got.extend_from_slice(&data);
        }
    }
    assert_eq!(got.len(), len, "nothing more than was sent");
    got
}

#[tokio::test]
async fn the_entry_url_sets_the_cookie_and_moves_on() {
    let log = Log::default();
    let addr = serve(echo(log.clone())).await;
    let proxy = Proxy::builder()
        .upstream(Some(upstream(addr, "/coversync/")))
        .start()
        .await
        .unwrap();
    let entry = proxy.entry_url();
    let secret = entry.split_once("/?k=").unwrap().1.to_owned();
    assert_eq!(entry, format!("{}/?k={secret}", proxy.origin()));
    assert!(proxy.origin().starts_with("http://127.0.0.1:"));

    let res = client().get(entry.parse().unwrap()).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()[CACHE_CONTROL], "no-store");
    let cookie = res.headers()[SET_COOKIE].to_str().unwrap().to_owned();
    let name = format!("coversync-app-{}=", proxy.addr().port());
    assert!(cookie.starts_with(&name), "{cookie}");
    for attribute in ["; Path=/", "; HttpOnly", "; SameSite=Strict"] {
        assert!(cookie.contains(attribute), "{cookie}");
    }
    assert!(!cookie.contains(&secret), "the cookie isn't the secret");
    let page = text(res).await;
    let start = format!(r#"location.replace("{}/coversync/")"#, proxy.origin());
    assert!(page.contains(&start), "{page}");
    assert!(!page.contains(&secret));
    assert_eq!(log.len(), 0, "the server never sees the entry URL");
}

#[tokio::test]
async fn requests_without_the_cookie_are_refused() {
    let log = Log::default();
    let addr = serve(echo(log.clone())).await;
    let window = Window::open(addr, "/coversync/").await;
    let path = "/coversync/api/status";

    let res = window
        .client
        .get(window.url(path).parse().unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN, "no cookie");

    let wrong = Request::get(window.url(path))
        .header(
            COOKIE,
            format!("coversync-app-{}=0000", window.proxy.addr().port()),
        )
        .body(Body::empty())
        .unwrap();
    assert_eq!(window.send(wrong).await.status(), StatusCode::FORBIDDEN);

    let res = window
        .client
        .get(window.url("/?k=0000").parse().unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN, "a wrong secret");
    assert!(!res.headers().contains_key(SET_COOKIE));

    // A page elsewhere that resolves its name to 127.0.0.1 can't use the
    // cookie either, even if it had it.
    let port = window.proxy.addr().port();
    let rebound = window
        .request(Method::GET, path)
        .header(HOST, format!("attacker.example:{port}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(window.send(rebound).await.status(), StatusCode::FORBIDDEN);

    assert_eq!(log.len(), 0, "nothing refused reaches the server");
    assert_eq!(window.get(path).await.status(), StatusCode::OK);
    assert_eq!(log.len(), 1);
}

#[tokio::test]
async fn keeps_the_servers_path_prefix() {
    let log = Log::default();
    let addr = serve(echo(log.clone())).await;
    let window = Window::open(addr, "/coversync/").await;

    window.get("/coversync/api/albums?collection=library").await;
    assert_eq!(log.last().uri, "/coversync/api/albums?collection=library");
    window.get("/coversync").await;
    assert_eq!(log.last().uri, "/coversync");

    let res = window.get("/").await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(res.headers()[LOCATION], window.url("/coversync/"));
    for outside in ["/coversyncx", "/favicon.ico", "/api/status"] {
        assert_eq!(
            window.get(outside).await.status(),
            StatusCode::NOT_FOUND,
            "{outside}"
        );
    }
    assert_eq!(log.len(), 2, "only paths under the prefix are forwarded");

    // A server at the root gets every path.
    let root = Window::open(addr, "/").await;
    root.get("/").await;
    assert_eq!(log.last().uri, "/");
    root.get("/api/status").await;
    assert_eq!(log.last().uri, "/api/status");
}

#[tokio::test]
async fn forwards_every_method() {
    let log = Log::default();
    let addr = serve(echo(log.clone())).await;
    let window = Window::open(addr, "/coversync/").await;

    for method in [
        Method::GET,
        Method::POST,
        Method::PUT,
        Method::PATCH,
        Method::DELETE,
        Method::OPTIONS,
        Method::from_bytes(b"PROPFIND").unwrap(),
    ] {
        let req = window
            .request(method.clone(), "/coversync/echo")
            .body(Body::from("a body"))
            .unwrap();
        let res = window.send(req).await;
        assert_eq!(res.status(), StatusCode::OK, "{method}");
        assert_eq!(res.headers()["x-method"], method.as_str());
        assert_eq!(text(res).await, "a body", "{method}");
        assert_eq!(log.last().method, method);
    }

    // HEAD keeps the length of the body it doesn't get.
    let head = Router::new().fallback(|| async { (StatusCode::OK, "eleven byte") });
    let window = Window::open(serve(head).await, "/").await;
    let res = window
        .send(
            window
                .request(Method::HEAD, "/")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()[CONTENT_LENGTH], "11");
    assert!(bytes(res).await.is_empty());
}

#[tokio::test]
async fn rewrites_headers_for_the_server() {
    let log = Log::default();
    let addr = serve(echo(log.clone())).await;
    let window = Window::open(addr, "/coversync/").await;
    let local = window.proxy.origin().to_owned();

    // The proxy's cookie, and one the webview shouldn't have.
    let req = Request::post(window.url("/coversync/api/library/setup"))
        .header(
            COOKIE,
            format!("{}; session=abc; theme=dark", window.cookie),
        )
        .header(ORIGIN, &local)
        .header(REFERER, format!("{local}/coversync/library?q=1"))
        .header(AUTHORIZATION, "Basic d2Vidmlldzpvd24=")
        .header(CONNECTION, "x-hop")
        .header("x-hop", "1")
        .header(PROXY_AUTHORIZATION, "Basic eA==")
        .header("x-kept", "1")
        .body(Body::empty())
        .unwrap();
    window.send(req).await;

    let seen = log.last().headers;
    assert_eq!(seen[HOST], addr.to_string());
    assert_eq!(seen[ORIGIN], format!("http://{addr}"));
    assert_eq!(
        seen[REFERER],
        format!("http://{addr}/coversync/library?q=1")
    );
    assert!(
        !seen.contains_key(COOKIE),
        "the webview's cookies stay here; the server's come from the proxy's jar"
    );
    assert_eq!(
        seen[AUTHORIZATION], "Basic d2Vidmlldzpvd24=",
        "without a token, the window's own passes"
    );
    assert!(!seen.contains_key("x-hop"));
    assert!(!seen.contains_key(PROXY_AUTHORIZATION));
    assert_eq!(seen["x-kept"], "1");

    // An Origin from elsewhere isn't touched.
    let req = window
        .request(Method::GET, "/coversync/")
        .header(ORIGIN, "null")
        .body(Body::empty())
        .unwrap();
    window.send(req).await;
    assert_eq!(log.last().headers[ORIGIN], "null");
}

#[tokio::test]
async fn adds_the_apps_authorization() {
    let log = Log::default();
    let addr = serve(echo(log.clone())).await;
    let token = HeaderValue::from_static("Bearer app-token");
    let up = upstream(addr, "/coversync/").with_authorization(Some(token));
    let window = Window::open_with(Proxy::builder().upstream(Some(up))).await;

    window.get("/coversync/api/status").await;
    assert_eq!(log.last().headers[AUTHORIZATION], "Bearer app-token");
    let req = window
        .request(Method::GET, "/coversync/api/status")
        .header(AUTHORIZATION, "Basic eA==")
        .body(Body::empty())
        .unwrap();
    window.send(req).await;
    assert_eq!(log.last().headers[AUTHORIZATION], "Bearer app-token");
}

#[tokio::test]
async fn redirects_stay_in_the_proxy() {
    let router = Router::new().fallback(|req: Request| async move {
        let host = req.headers()[HOST].to_str().unwrap().to_owned();
        let to = match req.uri().path() {
            "/coversync/out" => "https://example.com/elsewhere".to_owned(),
            "/coversync/relative" => "//example.com/elsewhere".to_owned(),
            "/coversync/scheme-relative" => format!("//{host}/coversync/?from=login"),
            _ => format!("http://{host}/coversync/?from=login"),
        };
        Response::builder()
            .status(StatusCode::SEE_OTHER)
            .header(LOCATION, to)
            .header(STRICT_TRANSPORT_SECURITY, "max-age=31536000")
            .header(ALT_SVC, "h3=\":443\"")
            .body(Body::empty())
            .unwrap()
    });
    let window = Window::open(serve(router).await, "/coversync/").await;
    let local = |path: &str| window.url(path);

    let res = window.get("/coversync/api/login").await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(res.headers()[LOCATION], local("/coversync/?from=login"));
    assert!(!res.headers().contains_key(STRICT_TRANSPORT_SECURITY));
    assert!(!res.headers().contains_key(ALT_SVC));

    let res = window.get("/coversync/scheme-relative").await;
    assert_eq!(res.headers()[LOCATION], local("/coversync/?from=login"));

    // Other sites are other sites.
    let res = window.get("/coversync/out").await;
    assert_eq!(res.headers()[LOCATION], "https://example.com/elsewhere");
    let res = window.get("/coversync/relative").await;
    assert_eq!(res.headers()[LOCATION], "//example.com/elsewhere");
}

/// A route whose body is whatever the test sends down `rx`, with these headers.
fn streaming_route(
    rx: mpsc::Receiver<Bytes>,
    headers: &'static [(&'static str, &'static str)],
) -> Router {
    let rx = Arc::new(Mutex::new(Some(rx)));
    Router::new().fallback(move || {
        let rx = rx.lock().unwrap().take().expect("one request");
        async move {
            let stream = futures_util::stream::unfold(rx, |mut rx| async move {
                rx.recv()
                    .await
                    .map(|chunk| (Ok::<_, std::convert::Infallible>(chunk), rx))
            });
            let mut res = Response::builder();
            for (name, value) in headers {
                res = res.header(*name, *value);
            }
            res.body(Body::from_stream(stream)).unwrap()
        }
    })
}

#[tokio::test]
async fn streams_response_bodies() {
    let (tx, rx) = mpsc::channel(1);
    // The length is known up front, as for a cover or a FLAC file.
    const LENGTH: &str = "2097152";
    let router = streaming_route(rx, &[("content-length", LENGTH)]);
    let window = Window::open(serve(router).await, "/coversync/").await;

    let res = window.get("/coversync/api/tracks/1/stream").await;
    assert_eq!(res.headers()[CONTENT_LENGTH], LENGTH);
    let mut body = res.into_body();
    let first = pattern(MIB);
    tx.send(Bytes::from(first.clone())).await.unwrap();
    // The first half arrives while the server holds back the second.
    assert_eq!(read_exactly(&mut body, MIB).await, first);
    tx.send(Bytes::from(vec![7; MIB])).await.unwrap();
    assert_eq!(read_exactly(&mut body, MIB).await, vec![7; MIB]);
    drop(tx);
    assert!(body.frame().await.is_none());
}

#[tokio::test]
async fn streams_request_bodies() {
    let (seen_tx, mut seen_rx) = mpsc::unbounded_channel::<usize>();
    let router = Router::new().fallback(move |req: Request| {
        let seen_tx = seen_tx.clone();
        async move {
            let mut body = req.into_body();
            let mut total = 0;
            while let Some(frame) = body.frame().await {
                if let Ok(data) = frame.unwrap().into_data() {
                    total += data.len();
                    seen_tx.send(total).unwrap();
                }
            }
            total.to_string()
        }
    });
    let window = Window::open(serve(router).await, "/coversync/").await;

    let (tx, body) = channel_body();
    let req = window
        .request(Method::POST, "/coversync/api/upload")
        .header(CONTENT_LENGTH, 2 * MIB)
        .body(body)
        .unwrap();
    let upload = tokio::spawn({
        let window = window.clone();
        async move { text(window.send(req).await).await }
    });
    tx.send(Bytes::from(vec![1; MIB])).await.unwrap();
    // The server has the first half before the window sends the second.
    let mut received = 0;
    while received < MIB {
        received = timeout(WAIT, seen_rx.recv())
            .await
            .expect("the bytes sent so far reach the server")
            .unwrap();
    }
    assert_eq!(received, MIB);
    tx.send(Bytes::from(vec![2; MIB])).await.unwrap();
    drop(tx);
    assert_eq!(upload.await.unwrap(), (2 * MIB).to_string());
}

#[tokio::test]
async fn passes_server_sent_events_as_they_come() {
    let (tx, rx) = mpsc::channel(1);
    let router = streaming_route(
        rx,
        &[
            ("content-type", "text/event-stream"),
            ("cache-control", "no-cache"),
        ],
    );
    let window = Window::open(serve(router).await, "/coversync/").await;

    let req = window
        .request(Method::GET, "/coversync/api/events")
        .header(ACCEPT, "text/event-stream")
        .body(Body::empty())
        .unwrap();
    let res = window.send(req).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()[CONTENT_TYPE], "text/event-stream");
    let mut body = res.into_body();
    for event in [
        "event: scan\ndata: {\"albums\":1}\n\n",
        "data: two\n\n",
        ": ping\n\n",
    ] {
        tx.send(Bytes::from_static(event.as_bytes())).await.unwrap();
        assert_eq!(read_exactly(&mut body, event.len()).await, event.as_bytes());
    }
}

#[tokio::test]
async fn passes_ranges() {
    let log = Log::default();
    let track = Bytes::from(pattern(1000));
    let router = Router::new().fallback({
        let log = log.clone();
        let track = track.clone();
        move |req: Request| {
            log.record(&req);
            let track = track.clone();
            async move {
                let range = req
                    .headers()
                    .get(RANGE)
                    .map(|r| r.to_str().unwrap().to_owned());
                let Some(range) = range else {
                    return Response::builder()
                        .header(ACCEPT_RANGES, "bytes")
                        .body(Body::from(track))
                        .unwrap();
                };
                let (start, end) = range
                    .strip_prefix("bytes=")
                    .unwrap()
                    .split_once('-')
                    .unwrap();
                let start: usize = start.parse().unwrap();
                let end: usize = if end.is_empty() {
                    track.len() - 1
                } else {
                    end.parse().unwrap()
                };
                Response::builder()
                    .status(StatusCode::PARTIAL_CONTENT)
                    .header(ACCEPT_RANGES, "bytes")
                    .header(
                        CONTENT_RANGE,
                        format!("bytes {start}-{end}/{}", track.len()),
                    )
                    .body(Body::from(track.slice(start..=end)))
                    .unwrap()
            }
        }
    });
    let window = Window::open(serve(router).await, "/coversync/").await;

    let req = window
        .request(Method::GET, "/coversync/api/tracks/1/stream")
        .header(RANGE, "bytes=100-199")
        .header(IF_RANGE, "\"v1\"")
        .body(Body::empty())
        .unwrap();
    let res = window.send(req).await;
    assert_eq!(res.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(res.headers()[CONTENT_RANGE], "bytes 100-199/1000");
    assert_eq!(res.headers()[CONTENT_LENGTH], "100");
    assert_eq!(res.headers()[ACCEPT_RANGES], "bytes");
    assert_eq!(bytes(res).await, track.slice(100..200));
    let seen = log.last().headers;
    assert_eq!(seen[RANGE], "bytes=100-199");
    assert_eq!(seen[IF_RANGE], "\"v1\"");

    let req = window
        .request(Method::GET, "/coversync/api/tracks/1/stream")
        .header(RANGE, "bytes=900-")
        .body(Body::empty())
        .unwrap();
    let res = window.send(req).await;
    assert_eq!(res.headers()[CONTENT_RANGE], "bytes 900-999/1000");
    assert_eq!(bytes(res).await, track.slice(900..));
}

#[tokio::test]
async fn large_bodies_arrive_whole() {
    let blob = Bytes::from(pattern(16 * MIB + 3));
    let router = Router::new().fallback({
        let blob = blob.clone();
        move |req: Request| {
            let blob = blob.clone();
            async move {
                if req.method() == Method::GET {
                    return Body::from(blob);
                }
                let got = req.into_body().collect().await.unwrap().to_bytes();
                Body::from(if got == blob { "same" } else { "different" })
            }
        }
    });
    let window = Window::open(serve(router).await, "/coversync/").await;

    assert_eq!(bytes(window.get("/coversync/blob").await).await, blob);
    let req = window
        .request(Method::PUT, "/coversync/blob")
        .body(Body::from(blob.clone()))
        .unwrap();
    assert_eq!(text(window.send(req).await).await, "same");
}

/// A page load, as the webview sends it.
fn page_load(window: &Window, path: &str) -> Request {
    window
        .request(Method::GET, path)
        .header(ACCEPT, "text/html,application/xhtml+xml,*/*;q=0.8")
        .header("sec-fetch-dest", "document")
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn reports_a_server_that_cant_be_reached() {
    let closed = {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap()
    };
    let (tx, mut reports) = mpsc::unbounded_channel::<Unreachable>();
    let builder = Proxy::builder()
        .upstream(Some(upstream(closed, "/coversync/")))
        .on_unreachable(move |unreachable| tx.send(unreachable).unwrap());
    let window = Window::open_with(builder).await;
    let server = format!("http://{closed}/coversync/");

    // An API call gets an error shaped like the server's, which the UI shows.
    let res = window.get("/coversync/api/status").await;
    assert_eq!(res.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(res.headers()[CONTENT_TYPE], "application/json");
    let body = text(res).await;
    assert!(
        body.starts_with(&format!(
            "{{\"error\":\"Can't reach {server}. The connection was refused"
        )),
        "{body}"
    );
    assert!(reports.try_recv().is_err(), "an API call isn't a page load");

    // A page load is reported, so the app can show its fallback page.
    let res = window.send(page_load(&window, "/coversync/")).await;
    assert_eq!(res.status(), StatusCode::BAD_GATEWAY);
    assert!(text(res).await.contains("Can't reach the server"));
    let report = reports.try_recv().unwrap();
    assert_eq!(report.server, server);
    assert!(report.reason.contains("refused"), "{report:?}");
    assert_eq!(report.detail, None, "the OS's words add nothing to it");

    // Without Sec-Fetch-Dest, a GET that accepts HTML is a page load too.
    let req = window
        .request(Method::GET, "/coversync/library")
        .header(ACCEPT, "text/html")
        .body(Body::empty())
        .unwrap();
    window.send(req).await;
    assert!(reports.try_recv().is_ok());

    let err = window.proxy.check(&upstream(closed, "/coversync/")).await;
    assert!(err.unwrap_err().reason.to_lowercase().contains("refused"));
}

#[tokio::test]
async fn reports_a_server_down_behind_its_reverse_proxy() {
    // Caddy with coversyncd stopped answers 502.
    let down = Arc::new(AtomicBool::new(true));
    let router = Router::new().fallback({
        let down = down.clone();
        move || {
            let down = down.load(Ordering::SeqCst);
            async move {
                if down {
                    StatusCode::BAD_GATEWAY.into_response()
                } else {
                    hello().into_response()
                }
            }
        }
    });
    let addr = serve(router).await;
    let (tx, mut reports) = mpsc::unbounded_channel::<Unreachable>();
    let up = upstream(addr, "/coversync/");
    let builder = Proxy::builder()
        .upstream(Some(up.clone()))
        .on_unreachable(move |unreachable| tx.send(unreachable).unwrap());
    let window = Window::open_with(builder).await;

    assert_eq!(
        window.get("/coversync/api/status").await.status(),
        StatusCode::BAD_GATEWAY
    );
    assert!(reports.try_recv().is_err());
    let res = window.send(page_load(&window, "/coversync/")).await;
    assert_eq!(res.status(), StatusCode::BAD_GATEWAY, "passed through");
    assert_eq!(
        reports.try_recv().unwrap().reason,
        "It answered 502 Bad Gateway, so coversync behind it may be stopped."
    );
    assert!(window.proxy.check(&up).await.is_err());

    // Once it's back, a retry loads the page.
    down.store(false, Ordering::SeqCst);
    assert_eq!(window.proxy.check(&up).await, Ok(()));
    let res = window.send(page_load(&window, "/coversync/")).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(reports.try_recv().is_err());
}

#[tokio::test]
async fn switches_servers() {
    let (log_a, log_b) = (Log::default(), Log::default());
    let a = serve(echo(log_a.clone())).await;
    let b = serve(echo(log_b.clone())).await;
    let window = Window::open(a, "/one/").await;

    window.get("/one/api/status").await;
    assert_eq!(log_a.len(), 1);

    window.proxy.set_upstream(Some(upstream(b, "/two")));
    assert_eq!(
        window.proxy.upstream().unwrap().url(),
        format!("http://{b}/two/")
    );
    assert_eq!(
        window.get("/one/api/status").await.status(),
        StatusCode::NOT_FOUND
    );
    window.get("/two/api/status").await;
    assert_eq!((log_a.len(), log_b.len()), (1, 1));
    let res = window
        .client
        .get(window.proxy.entry_url().parse().unwrap())
        .await
        .unwrap();
    let start = format!(r#"location.replace("{}/two/")"#, window.proxy.origin());
    assert!(text(res).await.contains(&start));

    window.proxy.set_upstream(None);
    assert_eq!(
        window.get("/two/api/status").await.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[tokio::test]
async fn keeps_its_port_when_it_can() {
    let free = {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    };
    let first = Proxy::builder()
        .preferred_port(Some(free))
        .start()
        .await
        .unwrap();
    assert_eq!(first.addr().port(), free);
    // Taken now, so another proxy gets a port of its own.
    let second = Proxy::builder()
        .preferred_port(Some(free))
        .start()
        .await
        .unwrap();
    assert_ne!(second.addr().port(), free);
}

/// coversync's `/api/hello`.
fn hello() -> impl IntoResponse {
    (
        [(CONTENT_TYPE, "application/json")],
        r#"{"app":"coversync","name":"test","version":"0.1.0","api":["1"]}"#,
    )
}

#[tokio::test]
async fn check_wants_coversync() {
    let router = Router::new().fallback(|req: Request| async move {
        let host = req.headers()[HOST].to_str().unwrap().to_owned();
        match req.uri().path() {
            "/coversync/api/hello" => hello().into_response(),
            // Another app's catch-all page.
            "/spa/api/hello" => (StatusCode::OK, "<!doctype html>").into_response(),
            "/old/api/hello" => StatusCode::NOT_FOUND.into_response(),
            "/guarded/api/hello" => StatusCode::UNAUTHORIZED.into_response(),
            // On its own origin, but outside the prefix the proxy serves.
            "/moved/api/hello" => Redirect::permanent("/coversync/api/hello").into_response(),
            "/redirected/api/hello" => Redirect::permanent("/redirected/hello").into_response(),
            "/redirected/hello" => hello().into_response(),
            // Another app's page, too big to be a hello.
            "/huge/api/hello" => (StatusCode::OK, vec![b' '; 100 * 1024]).into_response(),
            "/https/api/hello" => {
                Redirect::permanent(&format!("https://{host}/https/api/hello")).into_response()
            }
            _ => StatusCode::NOT_FOUND.into_response(),
        }
    });
    let addr = serve(router).await;
    let proxy = Proxy::builder().start().await.unwrap();
    let check = |prefix: &'static str| {
        let proxy = proxy.clone();
        async move { proxy.check(&upstream(addr, prefix)).await }
    };
    let reason = |prefix| async move { check(prefix).await.unwrap_err().reason };

    assert_eq!(check("/coversync/").await, Ok(()));
    assert_eq!(
        check("/redirected/").await,
        Ok(()),
        "a redirect under the prefix is followed"
    );
    let moved = reason("/moved/").await;
    assert!(
        moved.contains(&format!("on to http://{addr}/coversync/api/hello")),
        "a redirect outside the prefix isn't: {moved}"
    );
    assert!(reason("/huge/").await.contains("isn't coversync"));
    assert!(reason("/spa/").await.contains("isn't coversync"));
    assert!(reason("/old/").await.contains("too old for this app"));
    assert!(reason("/coversycn/").await.contains("no /api/hello"));
    let guarded = reason("/guarded/").await;
    assert!(
        guarded.contains("login in front of the server") && guarded.contains("--server"),
        "{guarded}"
    );
    let upgrade = reason("/https/").await;
    assert!(
        upgrade.contains(&format!("on to https://{addr}/https/api/hello")),
        "{upgrade}"
    );
}

#[tokio::test]
async fn check_uses_the_servers_cookies() {
    let log = Log::default();
    let router = Router::new().fallback({
        let log = log.clone();
        move |req: Request| {
            log.record(&req);
            async move {
                // A server that rotates its session on every answer.
                let session = if req.uri().path().ends_with("/api/hello") {
                    "session=from-check; Path=/"
                } else {
                    "session=from-page; Path=/"
                };
                ([(SET_COOKIE, session)], hello()).into_response()
            }
        }
    });
    let addr = serve(router).await;
    let up = upstream(addr, "/coversync/");
    let window = Window::open_with(Proxy::builder().upstream(Some(up.clone()))).await;

    window.get("/coversync/").await;
    assert!(window.proxy.check(&up).await.is_ok());
    assert_eq!(
        log.last().headers[COOKIE],
        "session=from-page",
        "the check sends the jar's cookies"
    );
    window.get("/coversync/api/status").await;
    assert_eq!(
        log.last().headers[COOKIE],
        "session=from-check",
        "and keeps the one its answer set"
    );
}

#[tokio::test]
async fn check_follows_five_redirects_and_no_more() {
    // /coversync/hop/N redirects to N-1; hop 0 is the hello.
    let router = Router::new().fallback(|req: Request| async move {
        let path = req.uri().path().to_owned();
        if path == "/five/api/hello" {
            return Redirect::temporary("/five/hop/4").into_response();
        }
        if path == "/six/api/hello" {
            return Redirect::temporary("/six/hop/5").into_response();
        }
        let (prefix, hop) = path.rsplit_once("/hop/").unwrap();
        match hop.parse::<u32>().unwrap() {
            0 => hello().into_response(),
            n => Redirect::temporary(&format!("{prefix}/hop/{}", n - 1)).into_response(),
        }
    });
    let addr = serve(router).await;
    let proxy = Proxy::builder().start().await.unwrap();
    assert_eq!(proxy.check(&upstream(addr, "/five/")).await, Ok(()));
    let six = proxy.check(&upstream(addr, "/six/")).await.unwrap_err();
    assert_eq!(six.reason, "It redirects too many times.");
}

#[tokio::test]
async fn another_proxys_page_cant_use_this_one() {
    // Two servers, a proxy each, in one window: B's page is the same site as
    // A's proxy, so the window would send A's cookie with B's requests.
    let log_a = Log::default();
    let a = serve(echo(log_a.clone())).await;
    let b = serve(echo(Log::default())).await;
    let jar = Cookies::default();
    let open = |addr| {
        Window::open_with(
            Proxy::builder()
                .upstream(Some(upstream(addr, "/coversync/")))
                .cookies(jar.clone()),
        )
    };
    let (window_a, window_b) = (open(a).await, open(b).await);
    let post = |origin: Option<&str>, site: Option<&str>| {
        let mut req = window_a.request(Method::POST, "/coversync/api/rescan");
        if let Some(origin) = origin {
            req = req.header(ORIGIN, origin);
        }
        if let Some(site) = site {
            req = req.header("sec-fetch-site", site);
        }
        req.body(Body::empty()).unwrap()
    };

    // B's page posting to A's proxy, with A's cookie.
    let b_origin = window_b.proxy.origin().to_owned();
    for (origin, site) in [
        (Some(b_origin.as_str()), None),
        (Some(b_origin.as_str()), Some("same-site")),
        (None, Some("same-site")),
        (Some("https://elsewhere.example"), Some("cross-site")),
    ] {
        let res = window_a.send(post(origin, site)).await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN, "{origin:?} {site:?}");
    }
    assert_eq!(log_a.len(), 0, "none of them reaches A");

    // A's own page, a navigation the app starts, and a sandboxed frame's
    // null all pass.
    let a_origin = window_a.proxy.origin().to_owned();
    for (origin, site) in [
        (Some(a_origin.as_str()), Some("same-origin")),
        (None, Some("none")),
        (Some("null"), None),
    ] {
        let res = window_a.send(post(origin, site)).await;
        assert_eq!(res.status(), StatusCode::OK, "{origin:?} {site:?}");
    }

    // The entry URL works from anywhere: it carries its own secret.
    let req = Request::get(window_a.proxy.entry_url())
        .header("sec-fetch-site", "cross-site")
        .body(Body::empty())
        .unwrap();
    assert_eq!(window_a.send(req).await.status(), StatusCode::OK);
}

#[tokio::test]
async fn an_answer_after_a_switch_keeps_its_cookies_with_its_server() {
    // Server A answers only when the test lets it, after the switch.
    let (release, held) = tokio::sync::oneshot::channel::<()>();
    let held = Arc::new(Mutex::new(Some(held)));
    let log_a = Log::default();
    let a = serve(Router::new().fallback({
        let (held, log_a) = (held.clone(), log_a.clone());
        move |req: Request| {
            log_a.record(&req);
            let held = held.lock().unwrap().take();
            async move {
                if let Some(held) = held {
                    held.await.unwrap();
                }
                ([(SET_COOKIE, "session=a; Path=/")], "late").into_response()
            }
        }
    }))
    .await;
    let log_b = Log::default();
    let b = serve(echo(log_b.clone())).await;
    let window = Window::open(a, "/coversync/").await;

    let late = tokio::spawn({
        let window = window.clone();
        async move { window.get("/coversync/api/slow").await }
    });
    // Wait for A to have the request, then switch to B while it's held.
    while log_a.len() == 0 {
        tokio::task::yield_now().await;
    }
    window.proxy.set_upstream(Some(upstream(b, "/coversync/")));
    release.send(()).unwrap();
    let res = late.await.unwrap();
    assert!(!res.headers().contains_key(SET_COOKIE));

    window.get("/coversync/api/status").await;
    assert!(
        !log_b.last().headers.contains_key(COOKIE),
        "A's late cookie isn't B's"
    );
    window.proxy.set_upstream(Some(upstream(a, "/coversync/")));
    window.get("/coversync/api/status").await;
    assert_eq!(log_a.last().headers[COOKIE], "session=a");
}

#[tokio::test]
async fn a_random_port_avoids_other_servers_ports() {
    let first = Proxy::builder().start().await.unwrap();
    let taken: Vec<u16> = (0..200)
        .map(|i| first.addr().port().wrapping_add(i))
        .collect();
    let proxy = Proxy::builder()
        .avoid_ports(taken.clone())
        .start()
        .await
        .unwrap();
    assert!(!taken.contains(&proxy.addr().port()));
}

#[tokio::test]
async fn a_page_load_that_gets_no_answer_is_reported() {
    // A server that takes connections and never answers.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            held.push(socket);
        }
    });
    let (tx, mut reports) = mpsc::unbounded_channel::<Unreachable>();
    let builder = Proxy::builder()
        .upstream(Some(upstream(addr, "/coversync/")))
        .page_timeout(Duration::from_millis(300))
        .on_unreachable(move |unreachable| tx.send(unreachable).unwrap());
    let window = Window::open_with(builder).await;

    // An API call, a download or an event stream isn't timed.
    let api = tokio::spawn({
        let window = window.clone();
        async move {
            window
                .client
                .request(
                    window
                        .request(Method::GET, "/coversync/api/events")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
        }
    });

    let res = window.send(page_load(&window, "/coversync/")).await;
    assert_eq!(res.status(), StatusCode::BAD_GATEWAY);
    let report = reports.try_recv().unwrap();
    assert_eq!(report.reason, "It didn't answer within 0.3 seconds.");

    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!api.is_finished(), "the API call still waits");
    api.abort();
}

#[tokio::test]
async fn a_page_load_sent_off_to_another_origin_is_reported() {
    let router = Router::new().fallback(|req: Request| async move {
        let host = req.headers()[HOST].to_str().unwrap().to_owned();
        Redirect::permanent(&format!("https://{host}/coversync/"))
    });
    let addr = serve(router).await;
    let (tx, mut reports) = mpsc::unbounded_channel::<Unreachable>();
    let builder = Proxy::builder()
        .upstream(Some(upstream(addr, "/coversync/")))
        .on_unreachable(move |unreachable| tx.send(unreachable).unwrap());
    let window = Window::open_with(builder).await;

    let res = window.send(page_load(&window, "/coversync/")).await;
    assert_eq!(res.status(), StatusCode::BAD_GATEWAY);
    assert!(!res.headers().contains_key(LOCATION), "the window stays");
    let report = reports.try_recv().unwrap();
    assert!(
        report
            .reason
            .contains(&format!("on to https://{addr}/coversync/")),
        "{report:?}"
    );
}

/// What a webview's HTTP cache does, in short: keeps an answer's `ETag`
/// under its full URL (the origin, port included), and sends it back as
/// `If-None-Match` the next time that URL is loaded.
#[derive(Default)]
struct BrowserCache(std::collections::HashMap<String, HeaderValue>);

impl BrowserCache {
    async fn get(&mut self, window: &Window, path: &str) {
        let url = window.url(path);
        let mut req = window.request(Method::GET, path);
        if let Some(etag) = self.0.get(&url) {
            req = req.header(IF_NONE_MATCH, etag.clone());
        }
        let res = window.send(req.body(Body::empty()).unwrap()).await;
        if let Some(etag) = res.headers().get(ETAG) {
            self.0.insert(url, etag.clone());
        }
    }
}

#[tokio::test]
async fn a_browser_cache_stays_with_its_server() {
    // Two servers with the same path, each with its own version of it.
    let server = |etag: &'static str, log: Log| {
        Router::new().fallback(move |req: Request| {
            log.record(&req);
            async move { ([(ETAG, etag)], "page") }
        })
    };
    let (log_a, log_b) = (Log::default(), Log::default());
    let a = serve(server("\"from-a\"", log_a.clone())).await;
    let b = serve(server("\"from-b\"", log_b.clone())).await;
    let jar = Cookies::default();
    let open = |addr| {
        Window::open_with(
            Proxy::builder()
                .upstream(Some(upstream(addr, "/coversync/")))
                .cookies(jar.clone()),
        )
    };
    let mut cache = BrowserCache::default();

    // The app's way: a proxy, so an origin, for each server.
    let (window_a, window_b) = (open(a).await, open(b).await);
    cache.get(&window_a, "/coversync/").await;
    cache.get(&window_a, "/coversync/").await;
    assert_eq!(log_a.last().headers[IF_NONE_MATCH], "\"from-a\"");
    cache.get(&window_b, "/coversync/").await;
    assert!(
        !log_b.last().headers.contains_key(IF_NONE_MATCH),
        "B never sees what A's answers left in the cache"
    );

    // One origin for both, switched from A to B: the cache hands B A's ETag,
    // and a 304 from B would show A's page.
    window_a
        .proxy
        .set_upstream(Some(upstream(b, "/coversync/")));
    cache.get(&window_a, "/coversync/").await;
    assert_eq!(log_b.last().headers[IF_NONE_MATCH], "\"from-a\"");
}
