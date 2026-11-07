//! A mock coversync server and a client that plays the app's window.

#![allow(dead_code)]

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::header::{COOKIE, SET_COOKIE};
use axum::http::{HeaderMap, Method, Response, StatusCode, request};
use bytes::Bytes;
use coversync_proxy::{Builder, Proxy, Upstream};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::time::timeout;

/// Long enough for anything that's on its way; a test that waits this long
/// has found the proxy holding something back.
pub const WAIT: Duration = Duration::from_secs(5);

/// Serves `router` on 127.0.0.1 and a random port.
pub async fn serve(router: Router) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    addr
}

/// What the mock server saw of a request.
#[derive(Clone, Debug)]
pub struct Seen {
    pub method: Method,
    pub uri: String,
    pub headers: HeaderMap,
}

#[derive(Clone, Default)]
pub struct Log(Arc<Mutex<Vec<Seen>>>);

impl Log {
    pub fn record(&self, req: &Request) {
        self.0.lock().unwrap().push(Seen {
            method: req.method().clone(),
            uri: req.uri().to_string(),
            headers: req.headers().clone(),
        });
    }

    pub fn len(&self) -> usize {
        self.0.lock().unwrap().len()
    }

    pub fn last(&self) -> Seen {
        self.0.lock().unwrap().last().cloned().expect("a request")
    }
}

/// A server that records each request and answers with its body, and its
/// method in `x-method`.
pub fn echo(log: Log) -> Router {
    Router::new().fallback(move |req: Request| {
        let log = log.clone();
        async move {
            log.record(&req);
            let method = req.method().clone();
            Response::builder()
                .header("x-method", method.as_str())
                .body(req.into_body())
                .unwrap()
        }
    })
}

pub fn upstream(addr: SocketAddr, prefix: &str) -> Upstream {
    Upstream::parse(&format!("http://{addr}{prefix}")).unwrap()
}

pub type HttpClient = Client<HttpConnector, Body>;

pub fn client() -> HttpClient {
    Client::builder(TokioExecutor::new()).build_http()
}

/// The app's window: a client holding the proxy's cookie.
#[derive(Clone)]
pub struct Window {
    pub proxy: Proxy,
    pub client: HttpClient,
    /// `coversync-app=<value>`, as the window sends it back.
    pub cookie: String,
}

impl Window {
    /// Starts a proxy for the server at `addr` and `prefix`, and opens it.
    pub async fn open(addr: SocketAddr, prefix: &str) -> Self {
        Self::open_with(Proxy::builder().upstream(Some(upstream(addr, prefix)))).await
    }

    pub async fn open_with(builder: Builder) -> Self {
        let proxy = builder.start().await.unwrap();
        let client = client();
        let res = client
            .get(proxy.entry_url().parse().unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let set_cookie = res.headers()[SET_COOKIE].to_str().unwrap();
        let cookie = set_cookie.split(';').next().unwrap().to_owned();
        Self {
            proxy,
            client,
            cookie,
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.proxy.origin())
    }

    /// A request with the cookie.
    pub fn request(&self, method: Method, path: &str) -> request::Builder {
        Request::builder()
            .method(method)
            .uri(self.url(path))
            .header(COOKIE, &self.cookie)
    }

    /// Sends a request and waits for the response's head, though not for
    /// its body.
    pub async fn send(&self, req: Request) -> Response<Incoming> {
        timeout(WAIT, self.client.request(req))
            .await
            .expect("the response starts without waiting for its whole body")
            .unwrap()
    }

    pub async fn get(&self, path: &str) -> Response<Incoming> {
        self.send(self.request(Method::GET, path).body(Body::empty()).unwrap())
            .await
    }
}

pub async fn bytes(res: Response<Incoming>) -> Bytes {
    res.into_body().collect().await.unwrap().to_bytes()
}

pub async fn text(res: Response<Incoming>) -> String {
    String::from_utf8(bytes(res).await.to_vec()).unwrap()
}

/// A body that sends what's put in the channel, as it's put there.
pub fn channel_body() -> (mpsc::Sender<Bytes>, Body) {
    let (tx, rx) = mpsc::channel::<Bytes>(1);
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv()
            .await
            .map(|chunk| (Ok::<_, Infallible>(chunk), rx))
    });
    (tx, Body::from_stream(stream))
}

/// `len` bytes that show it if any go missing or out of order.
pub fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}
