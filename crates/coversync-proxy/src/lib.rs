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
//! - The servers' cookies stay in the proxy ([`Cookies`]), apart for each
//!   server, and go with requests as a browser would send them to the server
//!   itself. The app's `Authorization` header goes on every request once
//!   there is one ([`Upstream::with_authorization`]).
//! - Every answer names the server it came from in a `Coversync-Server`
//!   header ([`SERVER_HEADER`]), since the page's own address is the
//!   proxy's: the UI gives other devices that one (pairing).
//! - Only the app's window gets in: any local process can reach a localhost
//!   port. The window opens [`Proxy::entry_url`], which carries a secret
//!   made for this launch. The proxy trades it for an HttpOnly cookie and
//!   refuses every request without that cookie.
//! - The UI's requests to the app itself, under `/__coversync-app/`, are
//!   answered here and never forwarded ([`AppCommands`]).

mod app;
mod headers;
mod jar;
mod pages;
mod unreachable;
mod upstream;

use std::fmt;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::header::{ACCEPT, AUTHORIZATION, COOKIE, HOST, LOCATION, ORIGIN};
use axum::http::{HeaderValue, Method, StatusCode, Version};
use axum::response::Response;
use axum::serve::ListenerExt;
use http_body_util::{BodyExt, Limited};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use tokio::net::TcpListener;
use url::Url;

pub use app::{AppAnswer, AppCommands, AppError, HEADER as APP_HEADER, RESERVED as APP_PATH};
pub use headers::SERVER as SERVER_HEADER;
pub use jar::Cookies;
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
    avoid_ports: Vec<u16>,
    cookies: Option<Cookies>,
    page_timeout: Duration,
    on_unreachable: Option<UnreachableHandler>,
    commands: Option<Arc<dyn AppCommands>>,
}

impl Default for Builder {
    fn default() -> Self {
        Self {
            upstream: None,
            port: None,
            avoid_ports: Vec::new(),
            cookies: None,
            page_timeout: PAGE_TIMEOUT,
            on_unreachable: None,
            commands: None,
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

    /// A port to try first: the one kept for this server, say. The
    /// webview's cache and the UI's settings in local storage (sort,
    /// sidebar) belong to the origin, port included, so a server keeps them
    /// on its own port. A random port (see [`Builder::avoid_ports`]) is used
    /// when this one is taken.
    pub fn preferred_port(mut self, port: Option<u16>) -> Self {
        self.port = port;
        self
    }

    /// Ports a random port mustn't be: those kept for other servers, whose
    /// origins (and with them the webview's cache and local storage) are
    /// theirs alone.
    pub fn avoid_ports(mut self, ports: impl IntoIterator<Item = u16>) -> Self {
        self.avoid_ports = ports.into_iter().collect();
        self
    }

    /// The servers' cookies, which proxies can share. Without one, the proxy
    /// has its own, kept in memory.
    pub fn cookies(mut self, cookies: Cookies) -> Self {
        self.cookies = Some(cookies);
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
    /// can't reach the server, gets 502, 503 or 504 from it, or would be
    /// sent outside its origin and prefix. The proxy answers with a short
    /// error page of its own, or passes the server's on; the app can show
    /// its fallback page instead.
    pub fn on_unreachable(mut self, handler: impl Fn(Unreachable) + Send + Sync + 'static) -> Self {
        self.on_unreachable = Some(Arc::new(handler));
        self
    }

    /// The app's commands for the server's pages, which the proxy answers
    /// under [`APP_PATH`] and says it has in [`APP_HEADER`]. Without them it
    /// has none, and doesn't say so; the path is never forwarded either way.
    pub fn app_commands(mut self, commands: Arc<dyn AppCommands>) -> Self {
        self.commands = Some(commands);
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
            None => random_port(&self.avoid_ports).await?,
        };
        let addr = listener.local_addr()?;
        let shared = Arc::new(Shared {
            client: client()?,
            upstream: RwLock::new(self.upstream.map(Arc::new)),
            jar: self.cookies.unwrap_or_default(),
            secret: random_hex()?,
            // Cookies belong to a host, not a port: each proxy's has a name
            // of its own, so proxies on other ports don't overwrite it.
            cookie_name: format!("coversync-app-{}", addr.port()),
            cookie: random_hex()?,
            origin: format!("http://{addr}"),
            host: HeaderValue::from_str(&addr.to_string()).expect("an address is a valid header"),
            addr,
            page_timeout: self.page_timeout,
            on_unreachable: self.on_unreachable,
            commands: self.commands,
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

    /// Whether `upstream` is a coversync server the app can use: its
    /// `/api/hello` (open without a login) says `"app": "coversync"`.
    /// Redirects within the server's origin and prefix are followed, up to
    /// five; any other redirect, an error, or anything else is refused,
    /// saying why. The server's cookies go with it, and any it sets are
    /// kept, as for a page load. Doesn't change which server the proxy uses.
    pub async fn check(&self, upstream: &Upstream) -> Result<(), Unreachable> {
        let timeout = self.shared.page_timeout;
        match tokio::time::timeout(timeout, self.hello(upstream)).await {
            Err(_) => Err(Unreachable::too_slow(upstream, timeout)),
            Ok(answer) => answer,
        }
    }

    async fn hello(&self, upstream: &Upstream) -> Result<(), Unreachable> {
        let shared = &self.shared;
        let mut url = Url::parse(&format!("{}api/hello", upstream.url()))
            .map_err(|_| Unreachable::new(upstream, "Its address isn't valid."))?;
        // The first request, then one for each redirect followed.
        for _ in 0..=MAX_REDIRECTS {
            let uri = upstream
                .uri(&url[url::Position::BeforePath..])
                .ok_or_else(|| Unreachable::new(upstream, "Its address isn't valid."))?;
            let mut req = Request::get(uri)
                .header(HOST, upstream.host())
                .header(ACCEPT, "application/json")
                .body(Body::empty())
                .expect("a valid request");
            if let Some(authorization) = upstream.authorization() {
                req.headers_mut()
                    .insert(AUTHORIZATION, authorization.clone());
            }
            if let Some(cookies) = shared.jar.header(upstream.origin(), &url) {
                req.headers_mut().insert(COOKIE, cookies);
            }
            let res = shared
                .client
                .request(req)
                .await
                .map_err(|err| Unreachable::from_error(upstream, &err))?;
            let (mut parts, body) = res.into_parts();
            shared.jar.take(upstream.origin(), &url, &mut parts.headers);
            let status = parts.status;
            if status.is_redirection() {
                // An http:// address its server sends on to https://, say:
                // the window would follow it out of the proxy.
                let location = parts
                    .headers
                    .get(LOCATION)
                    .and_then(|l| l.to_str().ok())
                    .ok_or_else(|| Unreachable::answered(upstream, status))?;
                upstream
                    .keeps_server(&url, location)
                    .map_err(|to| Unreachable::redirected(upstream, &to))?;
                url = url
                    .join(location)
                    .map_err(|_| Unreachable::answered(upstream, status))?;
                continue;
            }
            if !status.is_success() {
                return Err(Unreachable::hello_answered(upstream, status));
            }
            let body = Limited::new(body, HELLO_LIMIT)
                .collect()
                .await
                .map(|body| body.to_bytes())
                .unwrap_or_default();
            let is_coversync = serde_json::from_slice::<serde_json::Value>(&body)
                .is_ok_and(|hello| hello["app"] == "coversync");
            return if is_coversync {
                Ok(())
            } else {
                Err(Unreachable::not_coversync(upstream))
            };
        }
        Err(Unreachable::new(upstream, "It redirects too many times."))
    }
}

/// Redirects [`Proxy::check`] follows within the server's origin and prefix.
const MAX_REDIRECTS: usize = 5;
/// More than `/api/hello` ever says.
const HELLO_LIMIT: usize = 64 * 1024;

/// A random free port on 127.0.0.1 that isn't one of `avoid`.
async fn random_port(avoid: &[u16]) -> io::Result<TcpListener> {
    for _ in 0..32 {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        if !avoid.contains(&listener.local_addr()?.port()) {
            return Ok(listener);
        }
    }
    Err(io::Error::other(
        "no free port that isn't kept for another server",
    ))
}

type HttpsClient = Client<HttpsConnector<HttpConnector>, Body>;

struct Shared {
    client: HttpsClient,
    upstream: RwLock<Option<Arc<Upstream>>>,
    jar: Cookies,
    /// In the entry URL; traded for the cookie.
    secret: String,
    /// The proxy's cookie: `coversync-app-<port>`.
    cookie_name: String,
    cookie: String,
    addr: SocketAddr,
    origin: String,
    host: HeaderValue,
    page_timeout: Duration,
    on_unreachable: Option<UnreachableHandler>,
    /// The app's commands for the server's pages.
    commands: Option<Arc<dyn AppCommands>>,
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
        // Where the UI looks for the app.
        let hello = self.commands.is_some()
            && parts.uri.path().strip_prefix(upstream.prefix()) == Some("/api/hello");
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
                tracing::warn!("{down}");
                self.report(down);
            } else if parts.status.is_redirection()
                && let Some(location) = parts.headers.get(LOCATION).and_then(|l| l.to_str().ok())
                && let Err(to) = upstream.keeps_server(&url, location)
            {
                return self.unreachable(Unreachable::redirected(upstream, &to), true);
            }
        }
        headers::from_upstream(&mut parts.headers, &self.origin, upstream);
        if hello {
            parts.headers.insert(app::HEADER, app::VERSIONS);
        }
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
            return pages::enter(
                &proxy.cookie_name,
                &proxy.cookie,
                &start(upstream.as_deref()),
            );
        }
        return pages::forbidden();
    }
    if !headers::has_own_cookie(req.headers(), &proxy.cookie_name, &proxy.cookie) {
        return pages::forbidden();
    }
    // Every proxy is on 127.0.0.1, so another proxy's page (another server's
    // UI) is the same site, and the window sends this proxy's cookie with
    // its requests here too. Refuse what another origin sends.
    if from_elsewhere(req.headers(), &proxy.origin) {
        return pages::forbidden();
    }
    // The app's own path, whatever the server's prefix: never forwarded.
    if app::reserved(path) {
        return app::answer(
            proxy.commands.as_ref(),
            &proxy.origin,
            upstream.as_deref(),
            req,
        )
        .await;
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

/// Whether a request comes from another origin: an `Origin` that isn't
/// ours (`null`, from a sandboxed frame, gets no SameSite=Strict cookie
/// anyway), or Fetch Metadata saying it's from another origin of this site
/// or another site. Navigations the app starts say `none`.
fn from_elsewhere(headers: &axum::http::HeaderMap, own: &str) -> bool {
    let foreign_origin = headers
        .get(ORIGIN)
        .is_some_and(|origin| origin != "null" && origin != own);
    let foreign_site = headers
        .get("sec-fetch-site")
        .is_some_and(|site| site == "same-site" || site == "cross-site");
    foreign_origin || foreign_site
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
}
