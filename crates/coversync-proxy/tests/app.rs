//! The app's commands for a server's pages: who may ask, and that the path
//! never reaches the server.

mod common;

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::header::{ALLOW, CACHE_CONTROL, CONTENT_TYPE, ORIGIN};
use axum::http::{Method, StatusCode, request};
use common::{Log, Window, echo, serve, text, upstream};
use coversync_proxy::{
    APP_HEADER, APP_PATH, AppAnswer, AppCommands, AppError, Cookies, Proxy, Upstream,
};
use serde_json::{Value, json};

const CAPABILITIES: &str = "/__coversync-app/v1/capabilities";

/// An app with two commands, which records what it was asked.
#[derive(Default)]
struct App {
    asked: Mutex<Vec<(String, String, Value)>>,
}

impl App {
    fn asked(&self) -> Vec<(String, String, Value)> {
        self.asked.lock().unwrap().clone()
    }
}

impl AppCommands for App {
    fn call(&self, server: &Upstream, command: &str, args: Value) -> AppAnswer {
        self.asked
            .lock()
            .unwrap()
            .push((server.url(), command.to_owned(), args.clone()));
        let answer = match command {
            "capabilities" => Ok(json!({ "version": 1, "features": ["notify"] })),
            "check" => match args["n"].as_i64() {
                Some(1) => Ok(json!(null)),
                Some(_) => Err(AppError::Failed("It didn't work.".into())),
                None => Err(AppError::Invalid("n has to be a number.".into())),
            },
            _ => Err(AppError::Unknown),
        };
        Box::pin(async move { answer })
    }
}

/// A window on a proxy that has `app`'s commands, for an echo server at
/// `prefix`.
async fn window_with(app: &Arc<App>, prefix: &str) -> (Window, Log) {
    let log = Log::default();
    let addr = serve(echo(log.clone())).await;
    let window = Window::open_with(
        Proxy::builder()
            .upstream(Some(upstream(addr, prefix)))
            .app_commands(app.clone()),
    )
    .await;
    (window, log)
}

/// A command as the UI's `fetch` sends it from one of the proxy's pages.
fn command(window: &Window, path: &str) -> request::Builder {
    window
        .request(Method::POST, path)
        .header(ORIGIN, window.proxy.origin())
        .header("sec-fetch-site", "same-origin")
        .header("sec-fetch-mode", "cors")
        .header("sec-fetch-dest", "empty")
        .header(CONTENT_TYPE, "application/json")
}

async fn json_of(res: axum::http::Response<hyper::body::Incoming>) -> Value {
    assert_eq!(res.headers()[CONTENT_TYPE], "application/json");
    assert_eq!(res.headers()[CACHE_CONTROL], "no-store");
    serde_json::from_str(&text(res).await).unwrap()
}

#[tokio::test]
async fn the_ui_asks_the_app_what_it_can_do() {
    let app = Arc::new(App::default());
    for prefix in ["/coversync/", "/"] {
        let (window, log) = window_with(&app, prefix).await;
        let res = window
            .send(command(&window, CAPABILITIES).body("{}".into()).unwrap())
            .await;
        assert_eq!(res.status(), StatusCode::OK, "{prefix}");
        assert_eq!(
            json_of(res).await,
            json!({ "version": 1, "features": ["notify"] })
        );
        let (server, name, args) = app.asked().pop().unwrap();
        assert_eq!(server, window.proxy.upstream().unwrap().url());
        assert_eq!((name.as_str(), args), ("capabilities", json!({})));
        assert_eq!(log.len(), 0, "the server never sees it ({prefix})");
    }
}

#[tokio::test]
async fn hello_says_the_app_is_there() {
    let app = Arc::new(App::default());
    for (prefix, hello) in [("/coversync/", "/coversync/api/hello"), ("/", "/api/hello")] {
        let (window, _) = window_with(&app, prefix).await;
        let res = window.get(hello).await;
        assert_eq!(res.headers()[APP_HEADER], "1", "{prefix}");
        // Only there: the UI asks it once, and other answers stay as they are.
        for other in ["api/status", "api/hello/ca", ""] {
            let res = window.get(&format!("{prefix}{other}")).await;
            assert!(!res.headers().contains_key(APP_HEADER), "{prefix}{other}");
        }
    }
}

#[tokio::test]
async fn a_proxy_without_an_app_doesnt_say_it_has_one() {
    // A server that claims an app of its own accord.
    let log = Log::default();
    let server = Router::new().fallback({
        let log = log.clone();
        move |req: Request| {
            log.record(&req);
            async { ([(APP_HEADER, "1")], "{}") }
        }
    });
    let addr = serve(server).await;
    let window = Window::open(addr, "/").await;
    let res = window.get("/api/hello").await;
    assert!(
        !res.headers().contains_key(APP_HEADER),
        "only the proxy says so"
    );
    assert_eq!(log.len(), 1);

    // The path is still the app's: it answers, with nothing to offer.
    let res = window
        .send(command(&window, CAPABILITIES).body("{}".into()).unwrap())
        .await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    assert_eq!(json_of(res).await["code"], "unknown_command");
    assert_eq!(log.len(), 1, "not forwarded");
}

#[tokio::test]
async fn the_reserved_path_never_reaches_the_server() {
    let app = Arc::new(App::default());
    // A server at the root serves every other path.
    let (window, log) = window_with(&app, "/").await;
    for path in [
        APP_PATH.to_owned(),
        format!("{APP_PATH}/"),
        format!("{APP_PATH}/v1/capabilities"),
        format!("{APP_PATH}/v1/capabilities?x=1"),
        format!("{APP_PATH}/v2/later"),
        format!("{APP_PATH}/index.html"),
    ] {
        for method in [Method::GET, Method::POST, Method::PUT, Method::DELETE] {
            // As a page of the UI would send it, and as anything else would.
            let asked = command(&window, &path)
                .method(method.clone())
                .body("{}".into())
                .unwrap();
            window.send(asked).await;
            let plain = window
                .request(method.clone(), &path)
                .body(Body::empty())
                .unwrap();
            window.send(plain).await;
            let no_cookie = Request::builder()
                .method(method)
                .uri(window.url(&path))
                .body(Body::empty())
                .unwrap();
            window.send(no_cookie).await;
        }
    }
    assert_eq!(log.len(), 0);
    // Next to it, the server's own paths go through.
    window.get("/__coversync-apps").await;
    assert_eq!(log.last().uri, "/__coversync-apps");
}

#[tokio::test]
async fn only_the_proxys_own_pages_may_ask() {
    let app = Arc::new(App::default());
    let (window, log) = window_with(&app, "/coversync/").await;
    let own = window.proxy.origin().to_owned();
    let other_port = format!("http://127.0.0.1:{}", window.proxy.addr().port() ^ 1);

    // What a page's request says of where it comes from: its Origin, and
    // the browser's Sec-Fetch-Site.
    let refused: [(&str, Option<&str>, Option<&str>); 9] = [
        (
            "a page of another site",
            Some("https://elsewhere.example"),
            Some("cross-site"),
        ),
        (
            "another server's page in the window",
            Some(&other_port),
            Some("same-site"),
        ),
        (
            "a frame without an origin",
            Some("null"),
            Some("same-origin"),
        ),
        (
            "a frame without an origin, cross-site",
            Some("null"),
            Some("cross-site"),
        ),
        ("the address typed, or a link followed", None, Some("none")),
        ("no Origin", None, Some("same-origin")),
        ("no Sec-Fetch-Site", Some(&own), None),
        ("neither: not a browser", None, None),
        (
            "an Origin that only starts like ours",
            Some(&format!("{own}0")),
            Some("same-origin"),
        ),
    ];
    for (who, origin, site) in refused {
        let mut req = window
            .request(Method::POST, CAPABILITIES)
            .header(CONTENT_TYPE, "application/json");
        if let Some(origin) = origin {
            req = req.header(ORIGIN, origin);
        }
        if let Some(site) = site {
            req = req.header("sec-fetch-site", site);
        }
        let res = window.send(req.body("{}".into()).unwrap()).await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN, "{who}");
    }

    // Without the proxy's cookie, even one of its own pages' requests.
    let no_cookie = Request::post(window.url(CAPABILITIES))
        .header(ORIGIN, &own)
        .header("sec-fetch-site", "same-origin")
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();
    assert_eq!(window.send(no_cookie).await.status(), StatusCode::FORBIDDEN);

    assert!(app.asked().is_empty(), "none of them reached the app");
    assert_eq!(log.len(), 0);
}

#[tokio::test]
async fn another_servers_proxy_cant_call_this_one() {
    // Two servers in one app: two proxies, sharing the app's commands.
    let app = Arc::new(App::default());
    let jar = Cookies::default();
    let open = |addr| {
        Window::open_with(
            Proxy::builder()
                .upstream(Some(upstream(addr, "/")))
                .cookies(jar.clone())
                .app_commands(app.clone()),
        )
    };
    let (log_a, log_b) = (Log::default(), Log::default());
    let a = open(serve(echo(log_a.clone())).await).await;
    let b = open(serve(echo(log_b.clone())).await).await;

    // B's page asks A's proxy. The window holds both cookies, since both
    // are on 127.0.0.1, and sends them; the request says where it's from.
    let req = Request::post(a.url(CAPABILITIES))
        .header("cookie", format!("{}; {}", a.cookie, b.cookie))
        .header(ORIGIN, b.proxy.origin())
        .header("sec-fetch-site", "same-site")
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();
    assert_eq!(b.send(req).await.status(), StatusCode::FORBIDDEN);
    assert!(app.asked().is_empty());

    // Each page's own proxy answers for its own server.
    for window in [&a, &b] {
        let res = window
            .send(command(window, CAPABILITIES).body("{}".into()).unwrap())
            .await;
        assert_eq!(res.status(), StatusCode::OK);
        let (server, ..) = app.asked().pop().unwrap();
        assert_eq!(server, window.proxy.upstream().unwrap().url());
    }
    assert_eq!((log_a.len(), log_b.len()), (0, 0));
}

#[tokio::test]
async fn commands_are_json_posts() {
    let app = Arc::new(App::default());
    let (window, log) = window_with(&app, "/coversync/").await;

    for method in [Method::GET, Method::PUT, Method::DELETE] {
        let req = command(&window, CAPABILITIES).method(method.clone());
        let res = window.send(req.body("{}".into()).unwrap()).await;
        assert_eq!(res.status(), StatusCode::METHOD_NOT_ALLOWED, "{method}");
        assert_eq!(res.headers()[ALLOW], "POST");
    }

    // What a form can send.
    for form in [
        "text/plain",
        "application/x-www-form-urlencoded",
        "multipart/form-data; boundary=x",
    ] {
        let req = window
            .request(Method::POST, CAPABILITIES)
            .header(ORIGIN, window.proxy.origin())
            .header("sec-fetch-site", "same-origin")
            .header(CONTENT_TYPE, form);
        let res = window.send(req.body("{}".into()).unwrap()).await;
        assert_eq!(res.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE, "{form}");
    }
    let untyped = window
        .request(Method::POST, CAPABILITIES)
        .header(ORIGIN, window.proxy.origin())
        .header("sec-fetch-site", "same-origin");
    let res = window.send(untyped.body("{}".into()).unwrap()).await;
    assert_eq!(res.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);

    for bad in ["", "{", "capabilities"] {
        let res = window
            .send(command(&window, CAPABILITIES).body(bad.into()).unwrap())
            .await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{bad:?}");
        assert_eq!(json_of(res).await["code"], "invalid");
    }

    let long = format!("{{\"x\":\"{}\"}}", "a".repeat(70_000));
    let res = window
        .send(command(&window, CAPABILITIES).body(long.into()).unwrap())
        .await;
    assert_eq!(res.status(), StatusCode::PAYLOAD_TOO_LARGE);

    assert!(app.asked().is_empty());
    assert_eq!(log.len(), 0);
}

#[tokio::test]
async fn an_older_app_says_what_it_doesnt_know() {
    let app = Arc::new(App::default());
    let (window, log) = window_with(&app, "/coversync/").await;

    // A command a newer UI knows and this app doesn't.
    let res = window
        .send(
            command(&window, "/__coversync-app/v1/media-keys")
                .body("{}".into())
                .unwrap(),
        )
        .await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let body = json_of(res).await;
    assert_eq!(body["code"], "unknown_command");
    assert!(body["error"].is_string(), "the server's error shape");

    // A version it doesn't speak, and paths that are no command's.
    for path in [
        "/__coversync-app/v2/capabilities",
        "/__coversync-app/v1/",
        "/__coversync-app/v1/a/b",
        "/__coversync-app/v1/..",
        "/__coversync-app/capabilities",
        "/__coversync-app",
    ] {
        let res = window
            .send(command(&window, path).body("{}".into()).unwrap())
            .await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND, "{path}");
    }
    assert_eq!(
        app.asked().len(),
        1,
        "only the well-formed one was the app's to refuse"
    );

    // What a command makes of its arguments.
    let check = |body: &'static str| {
        let req = command(&window, "/__coversync-app/v1/check");
        window.send(req.body(body.into()).unwrap())
    };
    assert_eq!(check(r#"{"n":1}"#).await.status(), StatusCode::OK);
    let invalid = check(r#"{"n":"x"}"#).await;
    assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        json_of(invalid).await,
        json!({ "error": "n has to be a number.", "code": "invalid" })
    );
    let failed = check(r#"{"n":2}"#).await;
    assert_eq!(failed.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(json_of(failed).await["code"], "failed");
    assert_eq!(log.len(), 0);
}

#[tokio::test]
async fn a_proxy_without_a_server_has_no_commands() {
    let app = Arc::new(App::default());
    let window = Window::open_with(Proxy::builder().app_commands(app.clone())).await;
    let res = window
        .send(command(&window, CAPABILITIES).body("{}".into()).unwrap())
        .await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    assert!(app.asked().is_empty());
}
