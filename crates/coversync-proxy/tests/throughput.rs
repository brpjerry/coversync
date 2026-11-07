//! How fast bodies go through the proxy, next to the same transfer straight
//! to the mock server. Ignored by default; run it in release mode:
//!
//! ```sh
//! cargo test --release -p coversync-proxy --test throughput -- --ignored --nocapture
//! ```
//!
//! `PROXY_BENCH_GIB` sets the size of each transfer (default 4).

mod common;

use std::convert::Infallible;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{Method, StatusCode};
use bytes::Bytes;
use common::{HttpClient, Window, client, serve};
use http_body_util::BodyExt;

/// About what a file server reads at a time.
const CHUNK: usize = 256 << 10;
const COVER: usize = 32 << 10;
const RUNS: usize = 3;

/// Streams `n` bytes for GET /big?n=…, a 32 KiB cover for GET /cover, and
/// counts what PUT sends.
fn bench_server() -> Router {
    let chunk = Bytes::from(vec![0x5a; CHUNK]);
    let cover = Bytes::from(vec![0xa5; COVER]);
    Router::new().fallback(move |req: Request| {
        let (chunk, cover) = (chunk.clone(), cover.clone());
        async move {
            if req.method() == Method::PUT {
                let mut body = req.into_body();
                let mut total = 0usize;
                while let Some(frame) = body.frame().await {
                    if let Ok(data) = frame.unwrap().into_data() {
                        total += data.len();
                    }
                }
                return Body::from(total.to_string());
            }
            if req.uri().path().ends_with("/cover") {
                return Body::from(cover);
            }
            let n: usize = req.uri().query().unwrap()[2..].parse().unwrap();
            Body::from_stream(futures_util::stream::iter(
                (0..n / CHUNK).map(move |_| Ok::<_, Infallible>(chunk.clone())),
            ))
        }
    })
}

fn upload_body(n: usize) -> Body {
    let chunk = Bytes::from(vec![0x5a; CHUNK]);
    Body::from_stream(futures_util::stream::iter(
        (0..n / CHUNK).map(move |_| Ok::<_, Infallible>(chunk.clone())),
    ))
}

/// Where requests go, and the cookie they need there.
struct Target<'a> {
    client: &'a HttpClient,
    origin: String,
    cookie: Option<&'a str>,
}

impl Target<'_> {
    fn request(&self, method: Method, path: &str, body: Body) -> Request {
        let mut req = Request::builder()
            .method(method)
            .uri(format!("{}{path}", self.origin));
        if let Some(cookie) = self.cookie {
            req = req.header("cookie", cookie);
        }
        req.body(body).unwrap()
    }

    async fn download(&self, n: usize) -> Duration {
        let start = Instant::now();
        let req = self.request(Method::GET, &format!("/coversync/big?n={n}"), Body::empty());
        let res = self.client.request(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let mut body = res.into_body();
        let mut total = 0;
        while let Some(frame) = body.frame().await {
            total += frame.unwrap().into_data().map_or(0, |d| d.len());
        }
        assert_eq!(total, n);
        start.elapsed()
    }

    async fn upload(&self, n: usize) -> Duration {
        let start = Instant::now();
        let req = self.request(Method::PUT, "/coversync/sink", upload_body(n));
        let res = self.client.request(req).await.unwrap();
        let got = res.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(got, n.to_string());
        start.elapsed()
    }

    /// `count` covers, `in_flight` at a time, as a grid of them loads.
    async fn covers(&self, count: usize, in_flight: usize) -> Duration {
        let start = Instant::now();
        let mut tasks = tokio::task::JoinSet::new();
        for worker in 0..in_flight {
            let reqs: Vec<Request> = (worker..count)
                .step_by(in_flight)
                .map(|_| self.request(Method::GET, "/coversync/cover", Body::empty()))
                .collect();
            let client = self.client.clone();
            tasks.spawn(async move {
                for req in reqs {
                    let res = client.request(req).await.unwrap();
                    let body = res.into_body().collect().await.unwrap().to_bytes();
                    assert_eq!(body.len(), COVER);
                }
            });
        }
        tasks.join_all().await;
        start.elapsed()
    }
}

fn gb_per_s(n: usize, took: Duration) -> f64 {
    n as f64 / took.as_secs_f64() / 1e9
}

async fn best<F: Future<Output = Duration>>(mut run: impl FnMut() -> F) -> Duration {
    let mut best = Duration::MAX;
    for _ in 0..RUNS {
        best = best.min(run().await);
    }
    best
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "a measurement; run it in release mode with --ignored --nocapture"]
async fn throughput() {
    let gib: usize = std::env::var("PROXY_BENCH_GIB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4);
    let n = gib << 30;
    let addr = serve(bench_server()).await;
    let window = Window::open(addr, "/coversync/").await;
    let plain = client();
    let direct = Target {
        client: &plain,
        origin: format!("http://{addr}"),
        cookie: None,
    };
    let proxied = Target {
        client: &window.client,
        origin: window.proxy.origin().to_owned(),
        cookie: Some(&window.cookie),
    };

    println!("{gib} GiB each way, best of {RUNS}, {CHUNK} byte chunks:");
    for (name, direct_took, proxied_took) in [
        (
            "download",
            best(|| direct.download(n)).await,
            best(|| proxied.download(n)).await,
        ),
        (
            "upload",
            best(|| direct.upload(n)).await,
            best(|| proxied.upload(n)).await,
        ),
    ] {
        let (d, p) = (gb_per_s(n, direct_took), gb_per_s(n, proxied_took));
        println!(
            "  {name:<8}  direct {d:5.2} GB/s   through the proxy {p:5.2} GB/s ({:.0}%)",
            100.0 * p / d
        );
    }

    let count = 5000;
    for in_flight in [1, 6] {
        let d = best(|| direct.covers(count, in_flight)).await;
        let p = best(|| proxied.covers(count, in_flight)).await;
        let per = |took: Duration| took.as_secs_f64() * 1e6 / count as f64;
        println!(
            "  {count} covers of {} KiB, {in_flight} at a time: direct {:.0} µs each, \
             through the proxy {:.0} µs each",
            COVER >> 10,
            per(d),
            per(p)
        );
    }
}
