//! The server's sync API over HTTP/1.1, one connection per download so
//! several fill the link together. TLS through rustls, checked against the
//! system's certificate store unless the client brings its own settings
//! (a pinned CA, say).

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{self, HeaderName, HeaderValue};
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::Error;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a reply's head may take.
const REPLY_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a reconcile's request may take: the server reads its own
/// copies' samples to match the device's, which on a large Library on a
/// slow disk takes a while.
pub(crate) const RECONCILE_TIMEOUT: Duration = Duration::from_secs(3600);
/// The longest wait for a body's next bytes, outside the event stream.
pub(crate) const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

type Https = hyper_rustls::HttpsConnector<HttpConnector>;

/// A device's token. A secret: never shown in a log or a debug print.
#[derive(Clone)]
pub struct Token(String);

impl Token {
    /// None for an empty token, or one that can't go in a header.
    pub fn new(token: impl Into<String>) -> Option<Self> {
        let token = token.into();
        let token = token.trim().to_string();
        (!token.is_empty() && HeaderValue::from_str(&format!("Bearer {token}")).is_ok())
            .then_some(Self(token))
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(…)")
    }
}

/// How the server's certificate is checked.
#[derive(Clone, Default)]
pub enum Tls {
    /// The system's certificate store (a LAN server's CA is trusted once
    /// it's installed there).
    #[default]
    System,
    /// The client's own rustls settings: a pinned CA, a self-signed
    /// certificate.
    Config(Arc<rustls::ClientConfig>),
}

impl fmt::Debug for Tls {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Tls::System => f.write_str("Tls::System"),
            Tls::Config(_) => f.write_str("Tls::Config(…)"),
        }
    }
}

/// Where the server is, and how to talk to it.
#[derive(Clone, Debug)]
pub struct Server {
    /// The server's address, as paired: `https://host/coversync`.
    pub url: String,
    pub token: Token,
    pub tls: Tls,
}

pub(crate) struct Api {
    client: Client<Https, Full<Bytes>>,
    base: String,
    auth: HeaderValue,
}

/// The server's refusal, as its JSON body gives it.
#[derive(serde::Deserialize)]
struct Problem {
    #[serde(default)]
    code: String,
    #[serde(default)]
    error: String,
}

impl Api {
    pub(crate) fn new(server: &Server, connections: usize) -> Result<Self, Error> {
        let mut http = HttpConnector::new();
        http.enforce_http(false);
        http.set_nodelay(true);
        http.set_connect_timeout(Some(CONNECT_TIMEOUT));
        let builder = hyper_rustls::HttpsConnectorBuilder::new();
        let https = match &server.tls {
            Tls::System => builder
                .with_provider_and_platform_verifier(rustls::crypto::ring::default_provider())
                .map_err(|e| Error::Network(format!("couldn't set up TLS: {e}")))?,
            Tls::Config(config) => builder.with_tls_config((**config).clone()),
        }
        .https_or_http()
        .enable_http1()
        .wrap_connector(http);
        let client = Client::builder(TokioExecutor::new())
            .pool_max_idle_per_host(connections.max(1) + 2)
            .pool_idle_timeout(Duration::from_secs(90))
            .build(https);
        let base = server.url.trim_end_matches('/').to_string();
        if !(base.starts_with("https://") || base.starts_with("http://")) {
            return Err(Error::Protocol(format!(
                "{base:?} isn't an http(s) address"
            )));
        }
        Ok(Self {
            client,
            base,
            auth: HeaderValue::from_str(&format!("Bearer {}", server.token.0))
                .expect("checked when the token was made"),
        })
    }

    /// Send a request; the reply's head, whatever its status.
    pub(crate) async fn send(
        &self,
        method: Method,
        path: &str,
        headers: &[(HeaderName, HeaderValue)],
        body: Option<(Bytes, &'static str)>,
    ) -> Result<Response<Incoming>, Error> {
        self.send_within(method, path, headers, body, REPLY_TIMEOUT)
            .await
    }

    /// [`Self::send`], waiting up to `within` for the reply's head.
    pub(crate) async fn send_within(
        &self,
        method: Method,
        path: &str,
        headers: &[(HeaderName, HeaderValue)],
        body: Option<(Bytes, &'static str)>,
        within: Duration,
    ) -> Result<Response<Incoming>, Error> {
        let mut req = Request::builder()
            .method(method)
            .uri(format!("{}{path}", self.base))
            .header(header::AUTHORIZATION, self.auth.clone());
        for (name, value) in headers {
            req = req.header(name, value);
        }
        let body = match body {
            Some((bytes, kind)) => {
                req = req.header(header::CONTENT_TYPE, kind);
                bytes
            }
            None => Bytes::new(),
        };
        let req = req
            .body(Full::new(body))
            .map_err(|e| Error::Protocol(e.to_string()))?;
        match tokio::time::timeout(within, self.client.request(req)).await {
            Ok(Ok(res)) => Ok(res),
            Ok(Err(e)) => Err(Error::Network(error_chain(&e))),
            Err(_) => Err(Error::Network("the server didn't answer in time".into())),
        }
    }

    /// The whole body of a successful reply, or the server's refusal.
    pub(crate) async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<(Bytes, &'static str)>,
    ) -> Result<Bytes, Error> {
        let res = self.send(method, path, &[], body).await?;
        let status = res.status();
        let body = read_all(res.into_body(), IDLE_TIMEOUT).await?;
        if status.is_success() {
            Ok(body)
        } else {
            Err(refusal(status, &body))
        }
    }

    pub(crate) async fn get(&self, path: &str) -> Result<Bytes, Error> {
        self.call(Method::GET, path, None).await
    }

    pub(crate) async fn post_json<T: Serialize, R: DeserializeOwned>(
        &self,
        path: &str,
        body: &T,
    ) -> Result<R, Error> {
        let body = serde_json::to_vec(body).map_err(|e| Error::Protocol(e.to_string()))?;
        let reply = self
            .call(Method::POST, path, Some((body.into(), "application/json")))
            .await?;
        parse(&reply)
    }

    /// NDJSON for a reconcile, which may take the server a while.
    pub(crate) async fn post_lines(&self, path: &str, body: Bytes) -> Result<Bytes, Error> {
        let res = self
            .send_within(
                Method::POST,
                path,
                &[],
                Some((body, "application/x-ndjson")),
                RECONCILE_TIMEOUT,
            )
            .await?;
        let status = res.status();
        let body = read_all(res.into_body(), IDLE_TIMEOUT).await?;
        if status.is_success() {
            Ok(body)
        } else {
            Err(refusal(status, &body))
        }
    }
}

pub(crate) fn parse<R: DeserializeOwned>(body: &[u8]) -> Result<R, Error> {
    serde_json::from_slice(body).map_err(|e| Error::Protocol(e.to_string()))
}

/// The error a refused request stands for.
pub(crate) fn refusal(status: StatusCode, body: &[u8]) -> Error {
    let problem: Option<Problem> = serde_json::from_slice(body).ok();
    let (code, message) = match problem {
        Some(p) => (p.code, p.error),
        None => (String::new(), String::from_utf8_lossy(body).into_owned()),
    };
    if status == StatusCode::UNAUTHORIZED {
        return Error::Unauthorized;
    }
    Error::Server {
        status: status.as_u16(),
        code,
        message: if message.is_empty() {
            status.to_string()
        } else {
            message
        },
    }
}

/// A body read to its end, each wait for more bytes cut off after `idle`.
pub(crate) async fn read_all(mut body: Incoming, idle: Duration) -> Result<Bytes, Error> {
    let mut out = BytesMut::new();
    while let Some(chunk) = next_chunk(&mut body, idle).await? {
        out.extend_from_slice(&chunk);
    }
    Ok(out.freeze())
}

/// A body's next bytes, None at its end.
pub(crate) async fn next_chunk(
    body: &mut Incoming,
    idle: Duration,
) -> Result<Option<Bytes>, Error> {
    loop {
        let frame = match tokio::time::timeout(idle, body.frame()).await {
            Err(_) => return Err(Error::Network("the server went quiet".into())),
            Ok(None) => return Ok(None),
            Ok(Some(Err(e))) => return Err(Error::Network(error_chain(&e))),
            Ok(Some(Ok(frame))) => frame,
        };
        if let Ok(data) = frame.into_data()
            && !data.is_empty()
        {
            return Ok(Some(data));
        }
    }
}

/// An error with its causes, which hold the useful part ("connection
/// refused", the certificate problem).
fn error_chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        let text = s.to_string();
        if !out.contains(&text) {
            out.push_str(": ");
            out.push_str(&text);
        }
        source = s.source();
    }
    out
}
