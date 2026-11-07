//! The desktop app's local proxy.
//!
//! The app's window loads the server's UI from this proxy, which listens on
//! 127.0.0.1 on a random port and forwards every request to the server:
//!
//! - Bodies stream both ways as they arrive, so covers, audio (and its `Range`
//!   seeks), uploads and server-sent events pass through untouched, and
//!   nothing is held in memory whole.
//! - Paths stay the same. A server at `https://n5pro.internal/coversync/` is
//!   served at `http://127.0.0.1:<port>/coversync/`, where a UI built with
//!   `BASE_PATH=/coversync` expects to be.
//! - The server's cookies work over the local plain-HTTP origin, and the
//!   app's `Authorization` header goes on every request once there is one
//!   ([`Upstream::with_authorization`]).
//! - Only the app's window gets in: any local process can reach a localhost
//!   port. The window opens [`Proxy::entry_url`], which carries a secret
//!   made for this launch. The proxy trades it for an HttpOnly cookie and
//!   refuses every request without that cookie.

mod headers;
mod pages;
mod upstream;

use std::error::Error as StdError;
use std::fmt;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::header::{ACCEPT, AUTHORIZATION, HOST};
use axum::http::{HeaderValue, Method, StatusCode, Version};
use axum::response::Response;
use axum::serve::ListenerExt;
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use tokio::net::TcpListener;

pub use upstream::{Upstream, UpstreamError};

/// How long to wait for a TCP connection to the server.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long [`Proxy::check`] waits for an answer.
const CHECK_TIMEOUT: Duration = Duration::from_secs(15);

/// The server couldn't be reached, or answered the way a reverse proxy does
/// when the server behind it is down (502, 503 or 504).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unreachable {
    /// The server's URL.
    pub server: String,
    /// What went wrong, for people: `Connection refused (os error 111)`.
    pub reason: String,
}

impl fmt::Display for Unreachable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Can't reach {}: {}", self.server, self.reason)
    }
}

impl StdError for Unreachable {}

impl Unreachable {
    fn new(upstream: &Upstream, reason: String) -> Self {
        Self {
            server: upstream.url(),
            reason,
        }
    }

    fn from_error(upstream: &Upstream, err: &(dyn StdError + 'static)) -> Self {
        Self::new(upstream, describe(err))
    }

    fn from_status(upstream: &Upstream, status: StatusCode) -> Option<Self> {
        matches!(
            status,
            StatusCode::BAD_GATEWAY | StatusCode::SERVICE_UNAVAILABLE | StatusCode::GATEWAY_TIMEOUT
        )
        .then(|| Self::new(upstream, format!("it answered {status}.")))
    }
}

/// The innermost cause of a failed request, which says the most
/// (`Connection refused (os error 111)`), with a hint for a certificate the
/// system doesn't trust.
fn describe(err: &(dyn StdError + 'static)) -> String {
    let mut innermost = err;
    let mut untrusted = false;
    let mut cause = Some(err);
    while let Some(err) = cause {
        untrusted |= is_certificate_error(err);
        innermost = err;
        cause = err.source();
    }
    let mut reason = innermost.to_string();
    if untrusted {
        reason.push_str(
            ". The system doesn't trust the server's certificate: install the CA that \
             signed it in the system's certificate store.",
        );
    }
    reason
}

fn is_certificate_error(mut err: &(dyn StdError + 'static)) -> bool {
    // rustls's errors reach hyper inside an io::Error, inside another.
    loop {
        if let Some(tls) = err.downcast_ref::<rustls::Error>() {
            return matches!(tls, rustls::Error::InvalidCertificate(_));
        }
        match err.downcast_ref::<io::Error>().and_then(io::Error::get_ref) {
            Some(inner) => err = inner,
            None => return false,
        }
    }
}

type UnreachableHandler = Arc<dyn Fn(Unreachable) + Send + Sync>;

/// Sets up a [`Proxy`].
#[derive(Default)]
pub struct Builder {
    upstream: Option<Upstream>,
    port: Option<u16>,
    on_unreachable: Option<UnreachableHandler>,
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

    /// Called when loading a page (a navigation, not an API call or a cover)
    /// can't reach the server, or gets 502, 503 or 504 from it. The proxy
    /// answers with a short error page of its own either way; the app can
    /// show its fallback page instead.
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
            secret: random_hex()?,
            cookie: random_hex()?,
            origin: format!("http://{addr}"),
            host: HeaderValue::from_str(&addr.to_string()).expect("an address is a valid header"),
            addr,
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

    /// The address it listens on, 127.0.0.1 and a random port.
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
    /// finish with the old one.
    pub fn set_upstream(&self, upstream: Option<Upstream>) {
        *self.shared.upstream.write().expect("upstream lock") = upstream.map(Arc::new);
    }

    /// Whether `upstream` answers, by loading its UI's page. Doesn't change
    /// which server the proxy uses.
    pub async fn check(&self, upstream: &Upstream) -> Result<(), Unreachable> {
        let uri = upstream
            .uri(&format!("{}/", upstream.prefix()))
            .ok_or_else(|| Unreachable::new(upstream, "its address isn't valid.".into()))?;
        let mut req = Request::get(uri)
            .header(HOST, upstream.host())
            .header(ACCEPT, "text/html")
            .body(Body::empty())
            .expect("a valid request");
        if let Some(authorization) = upstream.authorization() {
            req.headers_mut()
                .insert(AUTHORIZATION, authorization.clone());
        }
        match tokio::time::timeout(CHECK_TIMEOUT, self.shared.client.request(req)).await {
            Err(_) => Err(Unreachable::new(
                upstream,
                format!(
                    "it didn't answer within {} seconds.",
                    CHECK_TIMEOUT.as_secs()
                ),
            )),
            Ok(Err(err)) => Err(Unreachable::from_error(upstream, &err)),
            Ok(Ok(res)) => match Unreachable::from_status(upstream, res.status()) {
                Some(unreachable) => Err(unreachable),
                None => Ok(()),
            },
        }
    }
}

type HttpsClient = Client<HttpsConnector<HttpConnector>, Body>;

struct Shared {
    client: HttpsClient,
    upstream: RwLock<Option<Arc<Upstream>>>,
    /// In the entry URL; traded for the cookie.
    secret: String,
    /// The value of the proxy's cookie.
    cookie: String,
    addr: SocketAddr,
    origin: String,
    host: HeaderValue,
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

    async fn forward(&self, upstream: &Upstream, req: Request) -> Response {
        let document = is_document(&req);
        let (mut parts, body) = req.into_parts();
        let path_and_query = parts.uri.path_and_query().map_or("/", |pq| pq.as_str());
        let Some(uri) = upstream.uri(path_and_query) else {
            return pages::text(StatusCode::BAD_REQUEST, "Bad request.");
        };
        parts.uri = uri;
        parts.version = Version::HTTP_11;
        headers::to_upstream(&mut parts.headers, &self.origin, upstream);
        let (method, uri) = (parts.method.clone(), parts.uri.clone());
        match self.client.request(Request::from_parts(parts, body)).await {
            Ok(res) => {
                tracing::debug!("{method} {uri} -> {}", res.status());
                if document
                    && let Some(unreachable) = Unreachable::from_status(upstream, res.status())
                {
                    self.report(unreachable);
                }
                let (mut parts, body) = res.into_parts();
                headers::from_upstream(&mut parts.headers, &self.origin, upstream);
                Response::from_parts(parts, Body::new(body))
            }
            Err(err) => {
                let unreachable = Unreachable::from_error(upstream, &err);
                tracing::warn!("{unreachable}");
                if document {
                    let page = pages::unreachable_page(&unreachable);
                    self.report(unreachable);
                    page
                } else {
                    pages::unreachable_json(&unreachable)
                }
            }
        }
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
    let path = req.uri().path();
    if path == "/"
        && let Some(key) = entry_key(req.uri().query())
    {
        if req.method() == Method::GET
            && headers::constant_time_eq(key.as_bytes(), proxy.secret.as_bytes())
        {
            let to = format!("{}/", upstream.as_deref().map_or("", Upstream::prefix));
            return pages::enter(&proxy.cookie, &to);
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
            return pages::see_other(&format!("{}/", upstream.prefix()));
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
    fn describes_the_innermost_cause() {
        let io = io::Error::new(io::ErrorKind::ConnectionRefused, "Connection refused");
        assert_eq!(describe(&io), "Connection refused");

        // As hyper-rustls hands it on.
        let tls = io::Error::other(io::Error::new(
            io::ErrorKind::InvalidData,
            rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer),
        ));
        let reason = describe(&tls);
        assert!(reason.starts_with("invalid peer certificate: UnknownIssuer"));
        assert!(reason.contains("certificate store"));
    }
}
