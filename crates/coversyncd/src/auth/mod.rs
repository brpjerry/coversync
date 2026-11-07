//! Logging in: the password, browser sessions, app tokens, and the guard in
//! front of the API.
//!
//! The API is the boundary. Every `/api` route needs a session cookie or an
//! app token, except the few [`router`] opens: `/api/hello`, logging in and
//! out, and trading a token for a cookie. The UI's own files stay open (they
//! are the app's source and hold no data); it shows its login view when the
//! API answers 401. Covers and streams are loaded by `<img>` and `<audio>`,
//! which send the cookie, being same-origin.
//!
//! The password is set on the server with `coversyncd set-password`, never
//! from the web. Until it is, the API answers 401 with the code
//! `no_password`.

mod limit;
pub mod store;

use std::collections::HashSet;
use std::convert::Infallible;
use std::io::{BufRead, IsTerminal};
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use axum::extract::{ConnectInfo, FromRequestParts, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

use crate::Shared;
use limit::Limiter;
pub use store::{Access, Scope};
use store::{Store, Via};

/// The session cookie's name.
pub const COOKIE: &str = "coversync_session";
/// How long browsers keep the cookie (400 days, the most Chrome allows). The
/// session itself ends sooner once idle ([`store::SESSION_IDLE`]).
const COOKIE_MAX_AGE: u64 = 400 * 24 * 3600;
/// Longer passwords are refused before hashing.
const MAX_PASSWORD: usize = 1024;
const MIN_PASSWORD: usize = 8;
/// How many password checks may run at once; each takes 19 MiB for argon2.
const HASHING: usize = 2;

/// API versions this server speaks, for clients checking a URL.
const API_VERSIONS: &[&str] = &["1"];

/// Routes open without a login. Everything else under `/api` is guarded.
const OPEN: &[&str] = &[
    "/api/hello",
    "/api/login",
    "/api/login/token",
    "/api/logout",
];

pub struct Auth {
    store: Store,
    limiter: Mutex<Limiter>,
    /// Slots for password checks. A check holds its slot until argon2 is
    /// done, even if its client has given up.
    hashing: Arc<Semaphore>,
    /// Sessions and devices whose last use is being written.
    touching: Mutex<HashSet<Touch>>,
    #[cfg(test)]
    probe: Probe,
}

/// What tests steer and watch.
#[cfg(test)]
#[derive(Default)]
struct Probe {
    /// A stopped clock for the rate limits, so tests don't depend on how
    /// fast password checks run.
    stopped: Mutex<Option<Instant>>,
    /// Added to each password check, to keep it running a while.
    check_delay: Mutex<std::time::Duration>,
    /// Password checks running now, and the most that ran at once.
    running: std::sync::atomic::AtomicUsize,
    most_running: std::sync::atomic::AtomicUsize,
}

impl Auth {
    /// The login tables in the local database in `state_dir`, which
    /// [`coversync_core::Library::open_in`] has set up.
    pub fn open(state_dir: &Path) -> anyhow::Result<Self> {
        Ok(Auth {
            store: Store::open(&state_dir.join(coversync_core::db::LOCAL_DB))?,
            limiter: Mutex::new(Limiter::new(Instant::now())),
            hashing: Arc::new(Semaphore::new(HASHING)),
            touching: Mutex::new(HashSet::new()),
            #[cfg(test)]
            probe: Probe::default(),
        })
    }

    /// The time the rate limits count by.
    fn instant(&self) -> Instant {
        #[cfg(test)]
        if let Some(at) = *self.probe.stopped.lock().unwrap_or_else(|e| e.into_inner()) {
            return at;
        }
        Instant::now()
    }

    /// Check a password against the stored hash. Slow on purpose: run it
    /// on a blocking thread, holding a hashing slot.
    fn check_password(&self, password: &str, stored: &str) -> bool {
        #[cfg(test)]
        {
            use std::sync::atomic::Ordering::SeqCst;
            let now = self.probe.running.fetch_add(1, SeqCst) + 1;
            self.probe.most_running.fetch_max(now, SeqCst);
            std::thread::sleep(*self.probe.check_delay.lock().unwrap());
        }
        let right = store::verify_password(password, stored);
        #[cfg(test)]
        self.probe
            .running
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        right
    }

    fn limiter(&self) -> std::sync::MutexGuard<'_, Limiter> {
        self.limiter.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Take a login attempt for `client`, or say how long until it may try again.
    fn take_try(&self, client: limit::Client) -> Result<(), std::time::Duration> {
        let now = self.instant();
        self.limiter().take(client, now)
    }

    /// Give back the attempt of a login that succeeded.
    fn give_back(&self, client: limit::Client) {
        let now = self.instant();
        self.limiter().refund(client, now);
    }

    /// Count a login that failed against the overall limit.
    fn failed(&self, client: limit::Client) {
        let now = self.instant();
        self.limiter().failed(client, now);
    }

    #[cfg(test)]
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Stop the rate limits' clock: no bucket refills from here on.
    #[cfg(test)]
    pub fn stop_clock(&self) {
        *self.probe.stopped.lock().unwrap() = Some(Instant::now());
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Why a request was turned away; the API's `code` for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Denied {
    /// No password has been set on the server yet.
    NoPassword,
    /// No session cookie or token.
    LoginRequired,
    /// A session cookie that has expired or been ended.
    SessionExpired,
    /// A token that isn't valid, or whose device was revoked.
    InvalidToken,
    WrongPassword,
}

impl Denied {
    fn code(self) -> &'static str {
        match self {
            Denied::NoPassword => "no_password",
            Denied::LoginRequired => "login_required",
            Denied::SessionExpired => "session_expired",
            Denied::InvalidToken => "invalid_token",
            Denied::WrongPassword => "wrong_password",
        }
    }

    fn message(self) -> &'static str {
        match self {
            Denied::NoPassword => {
                "No password is set yet. Run `coversyncd set-password` on the server."
            }
            Denied::LoginRequired => "Sign in to continue.",
            Denied::SessionExpired => "Your session has ended. Sign in again.",
            Denied::InvalidToken => "This app's token isn't valid; pair it again.",
            Denied::WrongPassword => "That password isn't right.",
        }
    }
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: &'a str,
    code: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    retry_after: Option<u64>,
}

fn error(status: StatusCode, code: &str, message: &str) -> Response {
    let body = ErrorBody {
        error: message,
        code,
        retry_after: None,
    };
    let mut res = (status, Json(body)).into_response();
    no_store(&mut res);
    res
}

fn denied(why: Denied) -> Response {
    let mut res = error(StatusCode::UNAUTHORIZED, why.code(), why.message());
    res.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Bearer realm=\"coversync\""),
    );
    res
}

fn internal(e: impl std::fmt::Display) -> Response {
    tracing::error!("login check failed: {e}");
    error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal",
        "The login check failed; see coversyncd's log.",
    )
}

fn too_many(wait: std::time::Duration) -> Response {
    let secs = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
    let body = ErrorBody {
        error: "Too many failed attempts. Wait a little and try again.",
        code: "too_many_attempts",
        retry_after: Some(secs),
    };
    let mut res = (StatusCode::TOO_MANY_REQUESTS, Json(body)).into_response();
    res.headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from(secs));
    no_store(&mut res);
    res
}

fn no_store(res: &mut Response) {
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
}

/// Whether `path` needs a login: all of `/api` but the [`OPEN`] routes.
fn guarded(path: &str) -> bool {
    (path == "/api" || path.starts_with("/api/")) && !OPEN.contains(&path)
}

/// What a request offers to prove who it is.
enum Credentials {
    Token(String),
    /// Session ids from every cookie of ours: one for another path may come too.
    Sessions(Vec<String>),
    None,
}

fn credentials(headers: &HeaderMap) -> Credentials {
    if let Some(auth) = headers.get(header::AUTHORIZATION)
        && let Ok(auth) = auth.to_str()
        && let Some((scheme, token)) = auth.split_once(' ')
        && scheme.eq_ignore_ascii_case("bearer")
    {
        return Credentials::Token(token.trim().to_owned());
    }
    let sessions: Vec<String> = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|c| c.trim().split_once('='))
        .filter(|(name, value)| *name == COOKIE && !value.is_empty())
        .map(|(_, value)| value.to_owned())
        .take(4)
        .collect();
    if sessions.is_empty() {
        Credentials::None
    } else {
        Credentials::Sessions(sessions)
    }
}

/// What checking credentials found, and what to write down about it.
struct Checked {
    access: Result<Access, Denied>,
    /// Sessions (by their hash) and devices to note the use of.
    touch: Vec<Touch>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Touch {
    Session([u8; 32]),
    Device(i64),
}

fn check(store: &Store, credentials: Credentials, now: i64) -> rusqlite::Result<Checked> {
    let denied = |why| Checked {
        access: Err(why),
        touch: Vec::new(),
    };
    let unknown = match credentials {
        Credentials::Token(token) => {
            // Not one of ours, or absurdly long: no need to look it up.
            if !token.starts_with(store::TOKEN_PREFIX) || token.len() > 256 {
                return Ok(denied(Denied::InvalidToken));
            }
            return Ok(match store.device(&token, now)? {
                Some(d) => Checked {
                    access: Ok(Access {
                        via: Via::Token,
                        scope: d.scope,
                        device: Some(d.id),
                    }),
                    touch: d.stale.then_some(Touch::Device(d.id)).into_iter().collect(),
                },
                None => denied(Denied::InvalidToken),
            });
        }
        Credentials::Sessions(ids) => {
            for id in ids {
                if let Some(s) = store.session(&id, now)? {
                    let mut touch = Vec::new();
                    if s.stale {
                        touch.push(Touch::Session(store::digest(&id)));
                    }
                    if let Some(device) = s.access.device.filter(|_| s.device_stale) {
                        touch.push(Touch::Device(device));
                    }
                    return Ok(Checked {
                        access: Ok(s.access),
                        touch,
                    });
                }
            }
            Denied::SessionExpired
        }
        Credentials::None => Denied::LoginRequired,
    };
    Ok(denied(if store.password_hash()?.is_none() {
        Denied::NoPassword
    } else {
        unknown
    }))
}

/// Note a use in the background: a request never waits for the write, which
/// can queue behind a scan's.
fn touch(state: &Shared, touch: Touch, now: i64) {
    if !state
        .auth
        .touching
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(touch)
    {
        return;
    }
    let state = state.clone();
    tokio::task::spawn_blocking(move || {
        let store = &state.auth.store;
        let result = match touch {
            Touch::Session(hash) => store.touch_session(&hash, now),
            Touch::Device(id) => store.touch_device(id, now),
        };
        if let Err(e) = result {
            tracing::debug!("couldn't note a login's use: {e}");
        }
        state
            .auth
            .touching
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&touch);
    });
}

/// The guard in front of the whole app: a request for a guarded path goes
/// on only with a valid session cookie or a token with the `manage` scope,
/// and carries its [`Access`] as an extension.
pub async fn guard(State(state): State<Shared>, mut req: Request, next: Next) -> Response {
    if !guarded(req.uri().path()) {
        return next.run(req).await;
    }
    let found = credentials(req.headers());
    let now = now();
    let s = state.clone();
    let checked = match tokio::task::spawn_blocking(move || check(&s.auth.store, found, now)).await
    {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => return internal(e),
        Err(e) => return internal(e),
    };
    for t in checked.touch {
        touch(&state, t, now);
    }
    match checked.access {
        Ok(access) if access.scope == Scope::Sync => error(
            StatusCode::FORBIDDEN,
            "sync_only",
            "This device may only sync, and syncing isn't available yet.",
        ),
        Ok(access) => {
            req.extensions_mut().insert(access);
            let mut res = next.run(req).await;
            keep_private(&mut res);
            res
        }
        Err(why) => denied(why),
    }
}

/// Whether the credentials a request came with would still get it past the
/// guard. A long request, such as an event stream, ends once they don't: its
/// browser signed out, the password was reset, the device was revoked. When
/// that can't be told (the database is busy, say), they're taken as good.
pub async fn still_valid(state: &Shared, headers: &HeaderMap) -> bool {
    let found = credentials(headers);
    let s = state.clone();
    let now = now();
    match tokio::task::spawn_blocking(move || check(&s.auth.store, found, now)).await {
        Ok(Ok(checked)) => checked.access.is_ok_and(|a| a.scope != Scope::Sync),
        _ => true,
    }
}

/// What a login was served may be kept by the browser only, never by a
/// shared cache, which would hand it to requests without one. A response
/// with no policy of its own is checked every time.
fn keep_private(res: &mut Response) {
    let own = res
        .headers()
        .get(header::CACHE_CONTROL)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let directives: Vec<&str> = own
        .split(',')
        .map(str::trim)
        .filter(|d| !d.is_empty() && !d.eq_ignore_ascii_case("public"))
        .collect();
    let private =
        |d: &&str| d.eq_ignore_ascii_case("private") || d.eq_ignore_ascii_case("no-store");
    let policy = if directives.is_empty() {
        "private, no-cache".to_owned()
    } else if directives.iter().any(private) {
        directives.join(", ")
    } else {
        format!("private, {}", directives.join(", "))
    };
    let policy = HeaderValue::from_str(&policy).unwrap_or(HeaderValue::from_static("no-store"));
    res.headers_mut().insert(header::CACHE_CONTROL, policy);
}

/// The client's address: the connecting peer's, or, when that is a proxy on
/// this machine (Caddy), the one it forwarded. Loopback itself is never
/// trusted: every request through the proxy comes from it.
struct ClientAddr(Option<IpAddr>);

impl<S: Send + Sync> FromRequestParts<S> for ClientAddr {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Infallible> {
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|c| c.0.ip());
        Ok(ClientAddr(client_ip(peer, &parts.headers)))
    }
}

fn client_ip(peer: Option<IpAddr>, headers: &HeaderMap) -> Option<IpAddr> {
    let peer = peer.map(|ip| ip.to_canonical());
    if !peer.is_some_and(|ip| ip.is_loopback()) {
        return peer;
    }
    // The address the nearest proxy saw is the last one it added.
    let forwarded = headers
        .get_all("x-forwarded-for")
        .iter()
        .next_back()
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.rsplit(',').next())
        .and_then(|ip| ip.trim().parse::<IpAddr>().ok());
    forwarded.or(peer)
}

/// The request reached the proxy over HTTPS, so the cookie can be Secure.
fn over_https(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("https"))
}

/// The UI's base path, which the cookie is limited to so that other sites
/// behind the same host never get it. coversyncd doesn't know it behind a
/// proxy that strips its prefix, so the UI sends it. Like the URL it is part
/// of, it is ASCII, with anything else percent-encoded (`/m%C3%BCsik/`).
fn cookie_path(path: &str) -> Option<&str> {
    let ok = path.starts_with('/')
        && path.len() <= 512
        && path
            .bytes()
            .all(|b| b.is_ascii_graphic() && b != b';' && b != b',');
    ok.then_some(path)
}

fn set_cookie(value: &str, path: &str, max_age: u64, secure: bool) -> HeaderValue {
    let secure = if secure { "; Secure" } else { "" };
    let cookie =
        format!("{COOKIE}={value}; Path={path}; Max-Age={max_age}; HttpOnly; SameSite=Lax{secure}");
    // The value is base64url and the path visible ASCII without separators.
    HeaderValue::from_str(&cookie).expect("cookie is a valid header value")
}

fn bad_path() -> Response {
    error(
        StatusCode::BAD_REQUEST,
        "bad_path",
        "The cookie path must start with / and can't hold ; , spaces or control characters.",
    )
}

/// A new session, in a 204 response that sets its cookie.
fn signed_in(id: &str, path: &str, headers: &HeaderMap) -> Response {
    let mut res = StatusCode::NO_CONTENT.into_response();
    res.headers_mut().insert(
        header::SET_COOKIE,
        set_cookie(id, path, COOKIE_MAX_AGE, over_https(headers)),
    );
    no_store(&mut res);
    res
}

pub fn router() -> Router<Shared> {
    Router::new()
        .route("/api/hello", get(hello))
        .route("/api/login", post(login))
        .route("/api/login/token", post(login_with_token))
        .route("/api/logout", post(logout))
        .route("/api/session", get(session))
}

#[derive(Serialize)]
struct Hello {
    app: &'static str,
    /// The server's host name.
    name: String,
    version: &'static str,
    api: &'static [&'static str],
}

/// Open to all: tells a client that this URL is a coversync server, and
/// which API versions it speaks.
async fn hello() -> Json<Hello> {
    Json(Hello {
        app: "coversync",
        name: host_name(),
        version: env!("CARGO_PKG_VERSION"),
        api: API_VERSIONS,
    })
}

fn host_name() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes at most buf.len() bytes into the buffer.
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0;
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    match ok {
        true => String::from_utf8_lossy(&buf[..end]).into_owned(),
        false => String::new(),
    }
}

/// How this request is signed in. Guarded, so a 401 says it isn't.
async fn session(Extension(access): Extension<Access>) -> Json<Access> {
    Json(access)
}

#[derive(Deserialize)]
struct LoginRequest {
    password: String,
    /// The UI's base path, for the cookie (see [`cookie_path`]).
    path: String,
}

async fn login(
    State(state): State<Shared>,
    ClientAddr(ip): ClientAddr,
    headers: HeaderMap,
    Json(body): Json<LoginRequest>,
) -> Response {
    let Some(path) = cookie_path(&body.path).map(str::to_owned) else {
        return bad_path();
    };
    let s = state.clone();
    let stored = match tokio::task::spawn_blocking(move || s.auth.store.password_hash()).await {
        Ok(Ok(Some(hash))) => hash,
        Ok(Ok(None)) => return denied(Denied::NoPassword),
        Ok(Err(e)) => return internal(e),
        Err(e) => return internal(e),
    };
    let client = limit::Client::new(ip);
    if let Err(wait) = state.auth.take_try(client) {
        tracing::info!(client = ?ip, "login refused: too many failed attempts");
        return too_many(wait);
    }
    let checked = stored.clone();
    let right = body.password.len() <= MAX_PASSWORD && {
        let slot = match state.auth.hashing.clone().acquire_owned().await {
            Ok(slot) => slot,
            Err(e) => return internal(e),
        };
        let password = body.password;
        let s = state.clone();
        tokio::task::spawn_blocking(move || {
            // The slot goes with the check: a client that gives up can't
            // free it while argon2 is still running.
            let _slot = slot;
            s.auth.check_password(&password, &stored)
        })
        .await
        .unwrap_or(false)
    };
    let s = state.clone();
    let now = now();
    // Only while the hash checked is still the stored one: a reset that
    // committed meanwhile makes this password wrong.
    let created = match right {
        true => {
            tokio::task::spawn_blocking(move || s.auth.store.create_password_session(&checked, now))
                .await
        }
        false => Ok(Ok(None)),
    };
    match created {
        Ok(Ok(Some(id))) => {
            state.auth.give_back(client);
            tracing::info!(client = ?ip, "signed in");
            signed_in(&id, &path, &headers)
        }
        Ok(Ok(None)) => {
            state.auth.failed(client);
            tracing::warn!(client = ?ip, "failed login: wrong password");
            denied(Denied::WrongPassword)
        }
        Ok(Err(e)) => internal(e),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
struct PathRequest {
    /// The UI's base path, for the cookie (see [`cookie_path`]).
    path: Option<String>,
}

/// Trade an app's token (`Authorization: Bearer`) for a session cookie, for
/// a WebView that can't send the header itself. The session keeps the
/// device's scope, and isn't a password login.
async fn login_with_token(
    State(state): State<Shared>,
    ClientAddr(ip): ClientAddr,
    headers: HeaderMap,
    Json(body): Json<PathRequest>,
) -> Response {
    let Some(path) = body
        .path
        .as_deref()
        .and_then(cookie_path)
        .map(str::to_owned)
    else {
        return bad_path();
    };
    let Credentials::Token(token) = credentials(&headers) else {
        return denied(Denied::InvalidToken);
    };
    let client = limit::Client::new(ip);
    if let Err(wait) = state.auth.take_try(client) {
        return too_many(wait);
    }
    let s = state.clone();
    let now = now();
    let traded = tokio::task::spawn_blocking(move || -> rusqlite::Result<Result<_, Denied>> {
        let found = check(&s.auth.store, Credentials::Token(token), now)?;
        let stale = !found.touch.is_empty();
        match found.access {
            Ok(Access {
                device: Some(device),
                scope,
                ..
            }) => {
                let id = s.auth.store.create_device_session(device, scope, now)?;
                if stale {
                    s.auth.store.touch_device(device, now)?;
                }
                Ok(Ok((id, device)))
            }
            // A token always names its device.
            Ok(_) => Ok(Err(Denied::InvalidToken)),
            Err(why) => Ok(Err(why)),
        }
    })
    .await;
    match traded {
        Ok(Ok(Ok((id, device)))) => {
            state.auth.give_back(client);
            tracing::info!(client = ?ip, device, "traded a token for a session");
            signed_in(&id, &path, &headers)
        }
        Ok(Ok(Err(why))) => {
            state.auth.failed(client);
            tracing::warn!(client = ?ip, "failed token trade: {}", why.code());
            denied(why)
        }
        Ok(Err(e)) => internal(e),
        Err(e) => internal(e),
    }
}

/// End this browser's session and clear its cookie. Open without a login,
/// so a session that has already ended still clears its cookie.
async fn logout(
    State(state): State<Shared>,
    headers: HeaderMap,
    body: Option<Json<PathRequest>>,
) -> Response {
    let path = body.and_then(|Json(b)| b.path);
    let path = match path.as_deref().map(cookie_path) {
        Some(None) => return bad_path(),
        Some(Some(p)) => Some(p.to_owned()),
        None => None,
    };
    if let Credentials::Sessions(ids) = credentials(&headers) {
        let s = state.clone();
        let ended = tokio::task::spawn_blocking(move || {
            ids.iter().try_for_each(|id| s.auth.store.end_session(id))
        })
        .await;
        match ended {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return internal(e),
            Err(e) => return internal(e),
        }
    }
    let mut res = StatusCode::NO_CONTENT.into_response();
    if let Some(path) = path {
        res.headers_mut().insert(
            header::SET_COOKIE,
            set_cookie("", &path, 0, over_https(&headers)),
        );
    }
    no_store(&mut res);
    res
}

/// `coversyncd set-password`: set or reset the password, in the local
/// database in `state_dir`. A running service uses it from the next login.
pub fn set_password(state_dir: &Path) -> anyhow::Result<()> {
    // First, so a refused password leaves no new state directory behind.
    let password = read_password()?;
    let hash = store::hash_password(&password)?;
    let db = state_dir.join(coversync_core::db::LOCAL_DB);
    // A new state directory gets its database the way the service makes it
    // (or takes over one from an older release).
    if !db.exists() {
        coversync_core::Library::open_in(state_dir)?;
    }
    let store = Store::open(&db)?;
    let replaced = store.set_password_hash(&hash, now())?;
    if replaced {
        println!(
            "Password changed in {}. Browsers signed in with the old one are signed out.",
            db.display()
        );
    } else {
        println!("Password set in {}.", db.display());
    }
    Ok(())
}

/// On a terminal, prompt twice without echo; otherwise read one line.
fn read_password() -> anyhow::Result<String> {
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        return password_line(stdin.lock());
    }
    let first = rpassword::prompt_password("New password: ")?;
    acceptable(&first)?;
    let again = rpassword::prompt_password("Type it again: ")?;
    anyhow::ensure!(
        first == again,
        "The two don't match; the password wasn't changed."
    );
    Ok(first)
}

/// The password on the first line of `input`, without its line ending.
fn password_line(mut input: impl BufRead) -> anyhow::Result<String> {
    let mut line = String::new();
    input.read_line(&mut line)?;
    anyhow::ensure!(!line.is_empty(), "No password came on standard input.");
    let line = line.strip_suffix('\n').unwrap_or(&line);
    let line = line.strip_suffix('\r').unwrap_or(line);
    acceptable(line)?;
    Ok(line.to_owned())
}

fn acceptable(password: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        password.chars().count() >= MIN_PASSWORD,
        "The password needs at least {MIN_PASSWORD} characters."
    );
    anyhow::ensure!(
        password.len() <= MAX_PASSWORD,
        "The password can't be longer than {MAX_PASSWORD} bytes."
    );
    Ok(())
}

#[cfg(test)]
mod tests;
