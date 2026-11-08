//! Pairing devices, and managing them.
//!
//! A logged-in browser's Add device makes a code (`POST /api/pairings`) and
//! watches it (`GET /api/pairings/{id}`) while the device trades it for its
//! own token at `POST /api/pair`, which is open without a login. Sync setup
//! lists the devices, renames, re-scopes and revokes them, and holds the
//! pairing setup: the CA certificate for apps, served at `/api/hello/ca`,
//! and whether plain http may carry a token.
//!
//! Only a password login may change any of it: an app's token, or a cookie
//! traded for one, can list the devices but not add, change or revoke them,
//! so a lost phone can't add devices or lock out the others.

use std::net::{IpAddr, SocketAddr};

use axum::extract::{ConnectInfo, FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};

use super::pairing::{self, Cancelled, Refused};
use super::store::{DeviceInfo, NewDevice, PairingSetup, Store};
use super::{Access, Scope, ca, error, limit, no_store, now};
use crate::Shared;

/// Longest device name.
const MAX_NAME: usize = 64;
/// Longest app version a device may report.
const MAX_APP_VERSION: usize = 64;

pub fn router() -> Router<Shared> {
    Router::new()
        .route("/api/devices", get(list))
        .route("/api/devices/{id}", put(update).delete(remove))
        .route("/api/devices/{id}/revoke", post(revoke))
        .route("/api/pairings", post(create))
        .route("/api/pairings/{id}", get(status).delete(close))
        .route("/api/pairing/setup", get(setup).put(save_setup))
        .route("/api/pairing/check-ca", get(check_ca))
        .route("/api/pair", post(pair))
        .route("/api/hello/ca", get(serve_ca))
}

/// A request turned away: the status, the API's `code` and what to tell
/// the user.
struct Refusal(StatusCode, &'static str, String);

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        error(self.0, self.1, &self.2)
    }
}

impl Refusal {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Refusal(status, code, message.into())
    }

    fn bad(code: &'static str, message: impl Into<String>) -> Self {
        Refusal::new(StatusCode::BAD_REQUEST, code, message)
    }

    /// Something went wrong on the server; the log says what.
    fn failed(e: impl std::fmt::Display) -> Self {
        tracing::error!("devices: {e}");
        Refusal::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "That didn't work on the server; see coversyncd's log.",
        )
    }

    fn no_device() -> Self {
        Refusal::new(
            StatusCode::NOT_FOUND,
            "no_device",
            "There's no such device; it may have been removed.",
        )
    }

    fn bad_ca(why: String) -> Self {
        Refusal::new(StatusCode::UNPROCESSABLE_ENTITY, "bad_ca", why)
    }
}

type Answer = Result<Response, Refusal>;

/// Run `f` on the login store, off the async runtime.
async fn db<T: Send + 'static>(
    state: &Shared,
    f: impl FnOnce(&Store) -> rusqlite::Result<T> + Send + 'static,
) -> Result<T, Refusal> {
    let s = state.clone();
    match tokio::task::spawn_blocking(move || f(&s.auth.store)).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(Refusal::failed(e)),
        Err(e) => Err(Refusal::failed(e)),
    }
}

/// Run `f` off the async runtime: file reads.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, Refusal> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(Refusal::failed)
}

/// A response that holds a secret (a code or a token): never kept anywhere.
fn secret(body: impl Serialize) -> Response {
    let mut res = Json(body).into_response();
    no_store(&mut res);
    res
}

/// Only a password login may change devices or how they pair.
fn password_only(access: &Access) -> Result<(), Refusal> {
    if access.password_login() {
        return Ok(());
    }
    Err(Refusal::new(
        StatusCode::FORBIDDEN,
        "password_required",
        "Managing devices needs a password login: an app's token, or a session traded for one, \
         can't add, change or revoke devices.",
    ))
}

/// A device's name, as the browser typed it.
fn device_name(name: &str) -> Result<String, Refusal> {
    let name = name.trim();
    let wrong = if name.is_empty() {
        "Give the device a name."
    } else if name.chars().count() > MAX_NAME {
        "A device's name can have at most 64 characters."
    } else if name.chars().any(char::is_control) {
        "A device's name can't hold control characters."
    } else {
        return Ok(name.to_owned());
    };
    Err(Refusal::bad("bad_name", wrong))
}

/// Something a device says about itself: trimmed, short and printable, or
/// left out when empty.
fn about(value: Option<&str>, max: usize, what: &str) -> Result<Option<String>, Refusal> {
    let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    if value.chars().count() > max || value.chars().any(char::is_control) {
        return Err(Refusal::bad(
            "bad_request",
            format!("The {what} must be at most {max} printable characters."),
        ));
    }
    Ok(Some(value.to_owned()))
}

/// The app version a request names in `Coversync-App-Version`, if any.
/// Checked as `/api/pair` checks the field, but one that fails is ignored:
/// it's no reason to refuse the request.
pub(super) fn app_version(headers: &HeaderMap) -> Option<String> {
    let v = headers.get("coversync-app-version")?.to_str().ok()?;
    about(Some(v), MAX_APP_VERSION, "app version")
        .ok()
        .flatten()
}

#[derive(Serialize)]
struct Devices {
    devices: Vec<DeviceInfo>,
    /// The device this request comes from, for an app's token.
    this_device: Option<i64>,
    /// This request may change devices: it comes from a password login.
    can_manage: bool,
}

async fn list(State(state): State<Shared>, Extension(access): Extension<Access>) -> Answer {
    let devices = db(&state, |s| s.devices()).await?;
    Ok(Json(Devices {
        devices,
        this_device: access.device,
        can_manage: access.password_login(),
    })
    .into_response())
}

#[derive(Deserialize)]
struct DeviceChange {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    scope: Option<Scope>,
}

/// Rename or re-scope a device.
async fn update(
    State(state): State<Shared>,
    Extension(access): Extension<Access>,
    Path(id): Path<i64>,
    Json(change): Json<DeviceChange>,
) -> Answer {
    password_only(&access)?;
    let name = change.name.as_deref().map(device_name).transpose()?;
    let scope = change.scope;
    let device = db(&state, move |s| {
        match s.update_device(id, name.as_deref(), scope)? {
            true => s.device_info(id),
            false => Ok(None),
        }
    })
    .await?
    .ok_or_else(Refusal::no_device)?;
    tracing::info!(
        device = id,
        scope = device.scope.as_str(),
        "changed a device"
    );
    Ok(Json(device).into_response())
}

/// Revoke a device: its token gets 401 from its next request.
async fn revoke(
    State(state): State<Shared>,
    Extension(access): Extension<Access>,
    Path(id): Path<i64>,
) -> Answer {
    password_only(&access)?;
    let at = now();
    let device = db(&state, move |s| match s.revoke_device(id, at)? {
        true => s.device_info(id),
        false => Ok(None),
    })
    .await?
    .ok_or_else(Refusal::no_device)?;
    tracing::info!(device = id, "revoked a device");
    Ok(Json(device).into_response())
}

/// Forget a device, revoking it if it wasn't.
async fn remove(
    State(state): State<Shared>,
    Extension(access): Extension<Access>,
    Path(id): Path<i64>,
) -> Answer {
    password_only(&access)?;
    if !db(&state, move |s| s.remove_device(id)).await? {
        return Err(Refusal::no_device());
    }
    // A code that would have brought it back goes too.
    state.auth.pairings().forget_device(id);
    tracing::info!(device = id, "removed a device");
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
struct NewPairing {
    name: String,
    scope: Scope,
    /// The device to replace (Replace an existing device).
    #[serde(default)]
    replace: Option<i64>,
}

#[derive(Serialize)]
struct Created {
    id: String,
    /// 8 characters, shown as two groups of four.
    code: String,
    expires_in: u64,
    /// The CA certificate's SHA-256 in hex, for the QR code's `ca=`; null
    /// without one.
    ca: Option<String>,
    /// Why the CA setting can't be used, if it can't.
    ca_problem: Option<String>,
    /// Pairing over plain http is allowed.
    allow_http: bool,
}

/// What the CA setting gives, or what's wrong with it: read afresh each
/// time, as the file can change under it.
async fn ca_state(
    setup: &PairingSetup,
) -> Result<Option<Result<ca::Certificate, String>>, Refusal> {
    let Some(path) = setup.ca_path.clone() else {
        return Ok(None);
    };
    blocking(move || Some(ca::load(std::path::Path::new(&path)))).await
}

/// Add device: a code for a device to pair with.
async fn create(
    State(state): State<Shared>,
    Extension(access): Extension<Access>,
    Json(req): Json<NewPairing>,
) -> Answer {
    password_only(&access)?;
    let name = device_name(&req.name)?;
    let replace = req.replace;
    let (there, setup) = db(&state, move |s| {
        let there = match replace {
            Some(id) => s.device_info(id)?.is_some(),
            None => true,
        };
        Ok((there, s.pairing_setup()?))
    })
    .await?;
    if !there {
        return Err(Refusal::no_device());
    }
    // Before the code is made: one made for an answer that then fails
    // would wait with nobody to show it.
    let ca = ca_state(&setup).await?;
    let at = state.auth.instant();
    let made = state.auth.pairings().create(name, req.scope, replace, at);
    let Some((id, code)) = made.map_err(Refusal::failed)? else {
        return Err(Refusal::new(
            StatusCode::TOO_MANY_REQUESTS,
            "too_many_codes",
            "Too many pairing codes are waiting. Close other Add device dialogs first.",
        ));
    };
    tracing::info!(scope = req.scope.as_str(), replace, "made a pairing code");
    Ok(secret(Created {
        id,
        code,
        expires_in: pairing::LIFETIME.as_secs(),
        ca: ca
            .as_ref()
            .and_then(|c| c.as_ref().ok())
            .map(|c| c.fingerprint()),
        ca_problem: ca.and_then(Result::err),
        allow_http: setup.allow_http,
    }))
}

/// A pairing code's state, as the Add device dialog shows it.
#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum PairingStatus {
    Waiting {
        expires_in: u64,
    },
    /// The device that redeemed it; null if it has since been removed.
    Paired {
        device: Option<DeviceInfo>,
    },
    Expired,
    Cancelled {
        why: Cancelled,
    },
    /// The token couldn't be issued.
    Failed,
}

async fn status(
    State(state): State<Shared>,
    Extension(access): Extension<Access>,
    Path(id): Path<String>,
) -> Answer {
    password_only(&access)?;
    let at = state.auth.instant();
    let found = state.auth.pairings().status(&id, at);
    let status = match found {
        None => {
            return Err(Refusal::new(
                StatusCode::NOT_FOUND,
                "no_pairing",
                "That pairing code has ended. Make a new one.",
            ));
        }
        Some(pairing::Status::Waiting { expires_in }) => PairingStatus::Waiting { expires_in },
        Some(pairing::Status::Paired { device }) => PairingStatus::Paired {
            device: db(&state, move |s| s.device_info(device)).await?,
        },
        Some(pairing::Status::Expired) => PairingStatus::Expired,
        Some(pairing::Status::Cancelled { why }) => PairingStatus::Cancelled { why },
        Some(pairing::Status::Failed) => PairingStatus::Failed,
    };
    Ok(Json(status).into_response())
}

/// The dialog closed: its code stops working, if it hadn't been used.
async fn close(
    State(state): State<Shared>,
    Extension(access): Extension<Access>,
    Path(id): Path<String>,
) -> Answer {
    password_only(&access)?;
    state.auth.pairings().close(&id);
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Serialize)]
struct CaInfo {
    /// SHA-256, lowercase hex, as in the QR code.
    fingerprint: String,
    /// The same as `openssl x509 -noout -fingerprint -sha256` prints it.
    sha256: String,
}

impl From<&ca::Certificate> for CaInfo {
    fn from(cert: &ca::Certificate) -> Self {
        CaInfo {
            fingerprint: cert.fingerprint(),
            sha256: cert.fingerprint_colons(),
        }
    }
}

#[derive(Serialize)]
struct SetupResponse {
    setup: PairingSetup,
    /// The CA certificate the setting names, when it can be used.
    ca: Option<CaInfo>,
    /// Why it can't, when it can't.
    ca_problem: Option<String>,
}

async fn setup_response(setup: PairingSetup) -> Answer {
    let ca = ca_state(&setup).await?;
    Ok(Json(SetupResponse {
        setup,
        ca: ca.as_ref().and_then(|c| c.as_ref().ok()).map(CaInfo::from),
        ca_problem: ca.and_then(Result::err),
    })
    .into_response())
}

async fn setup(State(state): State<Shared>) -> Answer {
    let setup = db(&state, |s| s.pairing_setup()).await?;
    setup_response(setup).await
}

/// Save the pairing setup. A CA certificate is checked first: one that
/// can't be read, or a file that holds a private key, is refused.
async fn save_setup(
    State(state): State<Shared>,
    Extension(access): Extension<Access>,
    Json(setup): Json<PairingSetup>,
) -> Answer {
    password_only(&access)?;
    let path = ca::setting(setup.ca_path.as_deref())
        .map_err(Refusal::bad_ca)?
        .map(|p| p.to_string_lossy().into_owned());
    // Only a new path is checked here. The saved one is checked wherever
    // it's used, and the answer's `ca_problem` says if it has gone bad, so
    // a broken file doesn't stop the rest of the setup from being saved.
    let saved = db(&state, |s| s.pairing_setup()).await?.ca_path;
    if let Some(new) = path.clone().filter(|p| saved.as_ref() != Some(p)) {
        blocking(move || ca::load(std::path::Path::new(&new)))
            .await?
            .map_err(Refusal::bad_ca)?;
    }
    let setup = PairingSetup {
        ca_path: path,
        allow_http: setup.allow_http,
    };
    let saved = setup.clone();
    db(&state, move |s| s.set_pairing_setup(&saved)).await?;
    tracing::info!(ca = ?setup.ca_path, allow_http = setup.allow_http, "saved the pairing setup");
    setup_response(setup).await
}

#[derive(Deserialize)]
struct CheckQuery {
    path: String,
}

/// Sync setup checks the CA certificate path as it's typed.
async fn check_ca(Extension(access): Extension<Access>, Query(q): Query<CheckQuery>) -> Answer {
    password_only(&access)?;
    let Some(path) = ca::setting(Some(&q.path)).map_err(Refusal::bad_ca)? else {
        return Ok(Json(None::<CaInfo>).into_response());
    };
    let cert = blocking(move || ca::load(&path))
        .await?
        .map_err(Refusal::bad_ca)?;
    Ok(Json(Some(CaInfo::from(&cert))).into_response())
}

/// Open to all: the CA certificate for apps, alone, in PEM. The app checks
/// it against the fingerprint in the QR code before trusting it.
async fn serve_ca(State(state): State<Shared>) -> Answer {
    let setup = db(&state, |s| s.pairing_setup()).await?;
    match ca_state(&setup).await? {
        None => Err(Refusal::new(
            StatusCode::NOT_FOUND,
            "no_ca",
            "This server has no CA certificate for apps: its certificate is publicly trusted.",
        )),
        Some(Ok(cert)) => Ok((
            [
                (
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/x-pem-file"),
                ),
                (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
            ],
            cert.pem(),
        )
            .into_response()),
        Some(Err(why)) => {
            tracing::error!(path = ?setup.ca_path, "the CA certificate for apps can't be served: {why}");
            Err(Refusal::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "ca_unusable",
                "The CA certificate for apps can't be used; Sync setup says why.",
            ))
        }
    }
}

/// The address a request came from (not the one a proxy forwarded).
struct Peer(Option<IpAddr>);

impl<S: Send + Sync> FromRequestParts<S> for Peer {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        let peer = parts.extensions.get::<ConnectInfo<SocketAddr>>();
        Ok(Peer(peer.map(|c| c.0.ip())))
    }
}

/// Whether a token handed over in answer to this request stays off the
/// network unencrypted. Only two kinds of request qualify, both from a peer
/// on this machine (coversyncd itself speaks plain http):
///
/// - one the proxy here received over HTTPS (`X-Forwarded-Proto: https`);
/// - one that came straight from this machine, with no sign of a proxy.
///
/// A request a proxy forwarded over plain http is refused, whatever client
/// it names: it came over a network.
fn private_transport(peer: Option<IpAddr>, headers: &HeaderMap) -> bool {
    if !peer.is_some_and(|ip| ip.to_canonical().is_loopback()) {
        return false;
    }
    if super::over_https(headers) {
        return true;
    }
    ![
        "x-forwarded-for",
        "x-forwarded-proto",
        "x-forwarded-host",
        "forwarded",
        "via",
    ]
    .iter()
    .any(|h| headers.contains_key(*h))
}

/// What a device says about itself at `/api/pair`, besides its code. It's
/// read only after the transport check, so that a code sent in the clear is
/// cancelled however wrong the rest of the request is.
#[derive(Deserialize)]
struct AboutDevice {
    /// What the device runs on, such as "android" or "linux".
    #[serde(default)]
    platform: Option<String>,
    #[serde(default)]
    app_version: Option<String>,
    #[serde(default)]
    hostname: Option<String>,
}

#[derive(Serialize)]
struct Server {
    /// The server's host name.
    name: String,
    version: &'static str,
}

#[derive(Serialize)]
struct Paired {
    device_id: i64,
    token: String,
    name: String,
    scope: Scope,
    server: Server,
    /// The Library's id; null while there's no Library folder.
    library_id: Option<String>,
}

impl From<Refused> for Refusal {
    fn from(why: Refused) -> Self {
        match why {
            Refused::Wrong => Refusal::bad(
                "wrong_code",
                "That pairing code isn't right. Check it, or make a new one in Add device.",
            ),
            Refused::Expired => Refusal::new(
                StatusCode::GONE,
                "code_expired",
                "That pairing code has expired or was cancelled. Make a new one in Add device.",
            ),
            Refused::Used => Refusal::new(
                StatusCode::GONE,
                "code_used",
                "That pairing code has been used already. Make a new one in Add device.",
            ),
        }
    }
}

/// Open to all: a device trades a pairing code for its own token. It creates
/// nothing without a waiting code, and is rate-limited like logins; after
/// [`pairing::FAILURES`] failures every waiting code is cancelled.
async fn pair(
    State(state): State<Shared>,
    Peer(peer): Peer,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Answer {
    // `{"code", "platform"?, "app_version"?, "hostname"?}`, read as plain
    // JSON: only the code is looked at before the transport check.
    let code = body
        .get("code")
        .and_then(serde_json::Value::as_str)
        .and_then(pairing::normalize)
        .ok_or_else(|| {
            Refusal::bad(
                "bad_code",
                "A pairing code has 8 letters and digits, like K7QM-4ZTD.",
            )
        })?;
    let client = super::client_ip(peer, &headers);
    let allow_http = db(&state, |s| s.pairing_setup()).await?.allow_http;
    let at = state.auth.instant();
    if !allow_http && !private_transport(peer, &headers) {
        // The code crossed the network unencrypted: whoever saw it mustn't
        // get to use it over HTTPS instead.
        state
            .auth
            .pairings()
            .cancel_code(&code, Cancelled::Insecure, at);
        tracing::warn!(client = ?client, "pairing refused: plain http");
        return Err(Refusal::new(
            StatusCode::FORBIDDEN,
            "https_required",
            "Pairing over plain http:// is refused, as the token would cross the network \
             unencrypted, and this code is cancelled. Use the server's https:// address, or allow \
             plain http in Sync setup for a network that is encrypted already.",
        ));
    }
    // After the transport check, so a code sent in the clear is cancelled
    // whatever else is wrong with the request.
    let req: AboutDevice = serde_json::from_value(body).map_err(|e| {
        Refusal::bad(
            "bad_request",
            format!("The platform, app version and host name must be text: {e}."),
        )
    })?;
    let platform = about(req.platform.as_deref(), 32, "platform")?;
    let hostname = about(req.hostname.as_deref(), 253, "host name")?;
    let app_version = about(req.app_version.as_deref(), MAX_APP_VERSION, "app version")?;
    let limited = limit::Client::new(client);
    if let Err(wait) = state.auth.pair_try(limited) {
        tracing::info!(client = ?client, "pairing refused: too many failed attempts");
        return Ok(super::too_many(wait));
    }
    let redeemed = state.auth.pairings().redeem(&code, at);
    let claim = redeemed.inspect_err(|why| {
        state.auth.pair_failed(limited);
        tracing::warn!(client = ?client, "failed pairing: {why:?}");
    })?;
    let ip = client.map(|ip| ip.to_canonical().to_string());
    let name = claim.name.clone();
    let (scope, replace) = (claim.scope, claim.replace);
    let issued = db(&state, move |s| {
        let new = NewDevice {
            name: &name,
            scope,
            replace,
            platform: platform.as_deref(),
            hostname: hostname.as_deref(),
            app_version: app_version.as_deref(),
            ip: ip.as_deref(),
        };
        s.pair_device(&new, now())
    })
    .await;
    let issued = issued.inspect_err(|_| state.auth.pairings().failed(&claim.id))?;
    let Some((device, token)) = issued else {
        // The device to replace was removed while this one paired: it
        // doesn't come back.
        state.auth.pairings().replaced_gone(&claim.id);
        state.auth.pair_give_back(limited);
        tracing::warn!(client = ?client, replace, "pairing refused: the device to replace was removed");
        return Err(Refusal::new(
            StatusCode::NOT_FOUND,
            "no_device",
            "The device this code would replace was removed. Make a new code in Add device.",
        ));
    };
    state.auth.pairings().paired(&claim.id, device);
    state.auth.pair_give_back(limited);
    let s = state.clone();
    let library_id = blocking(move || s.lib.index_versions())
        .await
        .ok()
        .and_then(Result::ok)
        .and_then(|v| v.library)
        .map(|l| l.id);
    tracing::info!(client = ?client, device, replaced = replace.is_some(), "paired a device");
    Ok(secret(Paired {
        device_id: device,
        token,
        name: claim.name,
        scope,
        server: Server {
            name: super::host_name(),
            version: env!("CARGO_PKG_VERSION"),
        },
        library_id,
    }))
}
