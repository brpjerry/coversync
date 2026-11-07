//! The desktop app's local proxy.
//!
//! The app's window loads the server's UI from this proxy, which listens on
//! 127.0.0.1 on a port of its own and forwards every request to the server:
//!
//! - Bodies stream both ways as they arrive, so covers, audio (and its `Range`
//!   seeks), uploads and server-sent events pass through untouched, and
//!   nothing is held in memory whole.
//! - Paths stay the same. A server at `https://n5pro.internal/coversync/` is
//!   served at `http://127.0.0.1:<port>/coversync/`, where a UI built with
//!   `BASE_PATH=/coversync` expects to be.
//! - The servers' cookies stay in the proxy, apart for each server, and go
//!   with requests as a browser would send them to the server itself. The
//!   app's `Authorization` header goes on every request once there is one
//!   ([`Upstream::with_authorization`]).
//! - Only the app's window gets in: any local process can reach a localhost
//!   port. The window opens [`Proxy::entry_url`], which carries a secret
//!   made for this launch. The proxy trades it for an HttpOnly cookie and
//!   refuses every request without that cookie.

mod headers;
mod jar;
mod pages;
mod unreachable;
mod upstream;

use std::fmt;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::header::{ACCEPT, AUTHORIZATION, COOKIE, HOST, LOCATION};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Version};
use axum::response::Response;
use axum::serve::ListenerExt;
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use tokio::net::TcpListener;
use url::Url;

use jar::Jar;
pub use unreachable::Unreachable;
pub use upstream::{Upstream, UpstreamError};

/// How long to wait for a TCP connection to the server.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a page load, or [`Proxy::check`], waits for the server to start
/// answering.
const PAGE_TIMEOUT: Duration = Duration::from_secs(15);

type UnreachableHandler = Arc<dyn Fn(Unreachable) + Send + Sync>;

/// Sets up a [`Proxy`].
pub struct Builder {
    upstream: Option<Upstream>,
    port: Option<u16>,
    cookie_file: Option<PathBuf>,
    page_timeout: Duration,
    on_unreachable: Option<UnreachableHandler>,
}

impl Default for Builder {
    fn default() -> Self {
        Self {
            upstream: None,
            port: None,
            cookie_file: None,
            page_timeout: PAGE_TIMEOUT,
            on_unreachable: None,
        }
    }
}

impl Builder {
    /// The server to forward to. Without one, the proxy answers 503 until
    /// [`Proxy::set_upstream`] gives it one.
    pub fn upstream(mut self, upstream: Option<Upstream>) -> Self {
        self.upstream = upstream;
        self
    }

    /// A port to try first, such as the one the last run used. The UI keeps
    /// its settings (sort, sidebar) in the webview's local storage, which
    /// belongs to the origin, port included, so a new port each run would
    /// lose them. A random port is used when this one is taken.
    pub fn preferred_port(mut self, port: Option<u16>) -> Self {
        self.port = port;
        self
    }

    /// Where the servers' persistent cookies are kept between runs (a login,
    /// say), readable only by the user. Without one, cookies last as long as
    /// the proxy.
    pub fn cookie_file(mut self, file: Option<PathBuf>) -> Self {
        self.cookie_file = file;
        self
    }

    /// How long a page load waits for the server to start answering before
    /// it counts as unreachable (15 seconds by default). Other requests, and
    /// bodies once they've started (uploads, playback, events), aren't timed.
    pub fn page_timeout(mut self, timeout: Duration) -> Self {
        self.page_timeout = timeout;
        self
    }

    /// Called when loading a page (a navigation, not an API call or a cover)
    /// can't reach the server, gets 502, 503 or 504 from it, or would be sent
    /// off to another origin. The proxy answers with a short error page of
    /// its own, or passes the server's on; the app can show its fallback
    /// page instead.
    pub fn on_unreachable(mut self, handler: impl Fn(Unreachable) + Send + Sync + 'static) -> Self {
        self.on_unreachable = Some(Arc::new(handler));
        self
    }

    /// Listens on 127.0.0.1, on the preferred port or a random one, and
    /// serves for as long as the tokio runtime runs.
    pub async fn start(self) -> io::Result<Proxy> {
        let preferred = match self.port {
            Some(port) if port != 0 => TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await.ok(),
            _ => None,
        };
        let listener = match preferred {
            Some(listener) => listener,
            None => TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?,
        };
        let addr = listener.local_addr()?;
        let shared = Arc::new(Shared {
            client: client()?,
            upstream: RwLock::new(self.upstream.map(Arc::new)),
            jar: Jar::open(self.cookie_file),
            secret: random_hex()?,
            cookie: random_hex()?,
            origin: format!("http://{addr}"),
            host: HeaderValue::from_str(&addr.to_string()).expect("an address is a valid header"),
            addr,
            page_timeout: self.page_timeout,
            on_unreachable: self.on_unreachable,
        });
        let app = Router::new()
            .fallback(handle)
            .with_state(Arc::clone(&shared));
        // Events and small API answers go out as soon as they're written.
        let listener = listener.tap_io(|tcp| {
            if let Err(err) = tcp.set_nodelay(true) {
                tracing::debug!("couldn't set TCP_NODELAY: {err}");
            }
        });
        tokio::spawn(async move {
            if let Err(err) = axum::serve(listener, app).await {
                tracing::error!("the proxy stopped: {err}");
            }
        });
        Ok(Proxy { shared })
    }
}

/// The running proxy. Clones share it.
#[derive(Clone)]
pub struct Proxy {
    shared: Arc<Shared>,
}

impl fmt::Debug for Proxy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Proxy")
            .field("origin", &self.shared.origin)
            .field("upstream", &self.upstream().map(|u| u.url()))
            .finish_non_exhaustive()
    }
}

impl Proxy {
    pub fn builder() -> Builder {
        Builder::default()
    }

    /// The address it listens on: 127.0.0.1 and its port.
    pub fn addr(&self) -> SocketAddr {
        self.shared.addr
    }

    /// `http://127.0.0.1:<port>`.
    pub fn origin(&self) -> &str {
        &self.shared.origin
    }

    /// The URL the window opens: `http://127.0.0.1:<port>/?k=<secret>`. The
    /// proxy answers it with its cookie and sends the window on to the UI,
    /// without the secret in the address. Opening it again (to retry, or
    /// after a change of server) is fine.
    pub fn entry_url(&self) -> String {
        format!("{}/?k={}", self.shared.origin, self.shared.secret)
    }

    pub fn upstream(&self) -> Option<Upstream> {
        self.shared.upstream().as_deref().cloned()
    }

    /// Switches to another server, or to none. Requests already under way
    /// finish with the old one. Each server's cookies stay its own.
    pub fn set_upstream(&self, upstream: Option<Upstream>) {
        *self.shared.upstream.write().expect("upstream lock") = upstream.map(Arc::new);
    }

    /// Whether `upstream` shows a UI: its page answers with a success, a
    /// login (401 or 403), or a redirect on its own origin. Doesn't change
    /// which server the proxy uses.
    pub async fn check(&self, upstream: &Upstream) -> Result<(), Unreachable> {
        let url = upstream.url();
        let page = Url::parse(&url).expect("a server's URL is valid");
        let uri = upstream
            .uri(page.path())
            .ok_or_else(|| Unreachable::new(upstream, "Its address isn't valid."))?;
        let mut req = Request::get(uri)
            .header(HOST, upstream.host())
            .header(ACCEPT, "text/html")
            .body(Body::empty())
            .expect("a valid request");
        if let Some(authorization) = upstream.authorization() {
            req.headers_mut()
                .insert(AUTHORIZATION, authorization.clone());
        }
        let timeout = self.shared.page_timeout;
        match tokio::time::timeout(timeout, self.shared.client.request(req)).await {
            Err(_) => Err(Unreachable::too_slow(upstream, timeout)),
            Ok(Err(err)) => Err(Unreachable::from_error(upstream, &err)),
            Ok(Ok(res)) => check_answer(upstream, &page, res.status(), res.headers()),
        }
    }
}

/// Whether a check's answer shows the UI is there.
fn check_answer(
    upstream: &Upstream,
    page: &Url,
    status: StatusCode,
    headers: &HeaderMap,
) -> Result<(), Unreachable> {
    if status.is_success() || status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN
    {
        return Ok(());
    }
    if status.is_redirection() {
        // An http:// address its server sends on to https://, say: the
        // window would follow it out of the proxy.
        let location = headers.get(LOCATION).and_then(|l| l.to_str().ok());
        return match location {
            Some(location) => upstream
                .keeps_origin(page, location)
                .map_err(|to| Unreachable::redirected(upstream, &to)),
            None => Err(Unreachable::answered(upstream, status)),
        };
    }
    Err(Unreachable::answered(upstream, status))
}

type HttpsClient = Client<HttpsConnector<HttpConnector>, Body>;

struct Shared {
    client: HttpsClient,
    upstream: RwLock<Option<Arc<Upstream>>>,
    jar: Jar,
    /// In the entry URL; traded for the cookie.
    secret: String,
    /// The value of the proxy's cookie.
    cookie: String,
    addr: SocketAddr,
    origin: String,
    host: HeaderValue,
    page_timeout: Duration,
    on_unreachable: Option<UnreachableHandler>,
}

impl Shared {
    fn upstream(&self) -> Option<Arc<Upstream>> {
        self.upstream.read().expect("upstream lock").clone()
    }

    fn report(&self, unreachable: Unreachable) {
        if let Some(handler) = &self.on_unreachable {
            handler(unreachable);
        }
    }

    /// The proxy's own error page for a page load, reported to the app; or
    /// the server's error shape for anything else.
    fn unreachable(&self, unreachable: Unreachable, document: bool) -> Response {
        tracing::warn!("{unreachable}");
        if document {
            let page = pages::unreachable_page(&unreachable);
            self.report(unreachable);
            page
        } else {
            pages::unreachable_json(&unreachable)
        }
    }

    async fn forward(&self, upstream: &Upstream, req: Request) -> Response {
        let document = is_document(&req);
        let (mut parts, body) = req.into_parts();
        let path_and_query = parts.uri.path_and_query().map_or("/", |pq| pq.as_str());
        let Some(uri) = upstream.uri(path_and_query) else {
            return pages::text(StatusCode::BAD_REQUEST, "Bad request.");
        };
        let Ok(url) = Url::parse(&uri.to_string()) else {
            return pages::text(StatusCode::BAD_REQUEST, "Bad request.");
        };
        parts.uri = uri;
        parts.version = Version::HTTP_11;
        headers::to_upstream(&mut parts.headers, &self.origin, upstream);
        // The webview's cookies are only ever the proxy's own; the server's
        // come from the jar.
        parts.headers.remove(COOKIE);
        if let Some(cookies) = self.jar.header(upstream.origin(), &url) {
            parts.headers.insert(COOKIE, cookies);
        }
        let method = parts.method.clone();
        let sent = self.client.request(Request::from_parts(parts, body));
        // Only a page load is timed, and only until the answer starts: an
        // upload, playback or an event stream can take as long as it takes.
        let answer = if document {
            match tokio::time::timeout(self.page_timeout, sent).await {
                Ok(answer) => answer,
                Err(_) => {
                    let too_slow = Unreachable::too_slow(upstream, self.page_timeout);
                    return self.unreachable(too_slow, true);
                }
            }
        } else {
            sent.await
        };
        let res = match answer {
            Ok(res) => res,
            Err(err) => {
                tracing::debug!("{method} {url}: {err:?}");
                return self.unreachable(Unreachable::from_error(upstream, &err), document);
            }
        };
        tracing::debug!("{method} {url} -> {}", res.status());
        let (mut parts, body) = res.into_parts();
        self.jar.take(upstream.origin(), &url, &mut parts.headers);
        if document {
            if let Some(down) = Unreachable::from_status(upstream, parts.status) {
                self.report(down);
            } else if parts.status.is_redirection()
                && let Some(location) = parts.headers.get(LOCATION).and_then(|l| l.to_str().ok())
                && let Err(to) = upstream.keeps_origin(&url, location)
            {
                return self.unreachable(Unreachable::redirected(upstream, &to), true);
            }
        }
        headers::from_upstream(&mut parts.headers, &self.origin, upstream);
        Response::from_parts(parts, Body::new(body))
    }
}

/// Whether the request loads a page into the window.
fn is_document(req: &Request) -> bool {
    match req.headers().get("sec-fetch-dest") {
        Some(dest) => dest == "document",
        None => {
            req.method() == Method::GET
                && req
                    .headers()
                    .get(ACCEPT)
                    .and_then(|accept| accept.to_str().ok())
                    .is_some_and(|accept| accept.contains("text/html"))
        }
    }
}

async fn handle(State(proxy): State<Arc<Shared>>, req: Request) -> Response {
    // A web page whose own name resolves to 127.0.0.1 (DNS rebinding) sends
    // its own name as the Host.
    if req.headers().get(HOST) != Some(&proxy.host) {
        return pages::forbidden();
    }
    let upstream = proxy.upstream();
    // Where the UI starts, as a full URL: a prefix can't then be read as a
    // host name.
    let start = |upstream: Option<&Upstream>| {
        format!("{}{}/", proxy.origin, upstream.map_or("", Upstream::prefix))
    };
    let path = req.uri().path();
    if path == "/"
        && let Some(key) = entry_key(req.uri().query())
    {
        if req.method() == Method::GET
            && headers::constant_time_eq(key.as_bytes(), proxy.secret.as_bytes())
        {
            return pages::enter(&proxy.cookie, &start(upstream.as_deref()));
        }
        return pages::forbidden();
    }
    if !headers::has_own_cookie(req.headers(), &proxy.cookie) {
        return pages::forbidden();
    }
    let Some(upstream) = upstream else {
        return pages::text(StatusCode::SERVICE_UNAVAILABLE, "No server is set.");
    };
    if !upstream.serves(path) {
        if path == "/" {
            return pages::see_other(&start(Some(&upstream)));
        }
        return pages::text(StatusCode::NOT_FOUND, "Not found.");
    }
    proxy.forward(&upstream, req).await
}

/// The entry URL's `k`.
fn entry_key(query: Option<&str>) -> Option<&str> {
    query?.split('&').find_map(|pair| pair.strip_prefix("k="))
}

fn client() -> io::Result<HttpsClient> {
    let mut http = HttpConnector::new();
    http.enforce_http(false);
    http.set_nodelay(true);
    http.set_connect_timeout(Some(CONNECT_TIMEOUT));
    // The system's certificate store: a LAN server's CA (mkcert's, say) is
    // trusted once it's installed there.
    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_provider_and_platform_verifier(rustls::crypto::ring::default_provider())?
        .https_or_http()
        .enable_http1()
        .wrap_connector(http);
    Ok(Client::builder(TokioExecutor::new()).build(https))
}

fn random_hex() -> io::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_entry_key() {
        assert_eq!(entry_key(Some("k=abc")), Some("abc"));
        assert_eq!(entry_key(Some("x=1&k=abc")), Some("abc"));
        assert_eq!(entry_key(Some("kk=abc")), None);
        assert_eq!(entry_key(None), None);
    }

    #[test]
    fn checks_answers() {
        let up = Upstream::parse("http://n5pro.internal/coversync/").unwrap();
        let page = Url::parse(&up.url()).unwrap();
        let answer = |status: u16, location: Option<&'static str>| {
            let mut headers = HeaderMap::new();
            if let Some(location) = location {
                headers.insert(LOCATION, HeaderValue::from_static(location));
            }
            check_answer(&up, &page, StatusCode::from_u16(status).unwrap(), &headers)
        };
        for ok in [200, 204, 401, 403] {
            assert_eq!(answer(ok, None), Ok(()), "{ok}");
        }
        assert_eq!(answer(302, Some("/coversync/login")), Ok(()));
        assert_eq!(answer(308, Some("//n5pro.internal/coversync/x")), Ok(()));
        assert!(answer(308, Some("https://n5pro.internal/coversync/")).is_err());
        assert!(answer(302, None).is_err());
        for refused in [400, 404, 410, 500, 502, 503] {
            assert!(answer(refused, None).is_err(), "{refused}");
        }
    }
}
