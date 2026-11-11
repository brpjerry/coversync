//! Pairing: the app trades a code from the server's Add device for a token
//! of its own (`POST /api/pair`), which then goes with every request to that
//! server ([`Upstream::with_authorization`]).

use std::fmt;
use std::time::Duration;

use axum::body::Body;
use axum::extract::Request;
use axum::http::header::{
    ACCEPT, ACCEPT_ENCODING, AUTHORIZATION, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, HOST,
};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use http_body_util::{BodyExt, Limited};
use serde_json::{Value, json};

use crate::HttpsClient;
use crate::unreachable::Unreachable;
use crate::upstream::Upstream;

/// More than the server's answer to a pairing ever comes to.
const ANSWER_LIMIT: usize = 64 * 1024;

/// A device's token for a server. It is a secret: it never shows in a log
/// (its `Debug` hides it), a URL or a file, and goes nowhere but to the
/// keychain and, as a header, to its server.
#[derive(Clone, PartialEq, Eq)]
pub struct Token(String);

impl Token {
    /// `None` for what can't be sent as a header's value.
    pub fn new(token: impl Into<String>) -> Option<Self> {
        let token = token.into();
        let sendable = !token.is_empty() && HeaderValue::from_str(&token).is_ok();
        sendable.then_some(Self(token))
    }

    /// The token itself, for the keychain.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// The `Authorization` header that carries it.
    pub fn authorization(&self) -> HeaderValue {
        let mut value = HeaderValue::from_str(&format!("Bearer {}", self.0))
            .expect("checked when the token was made");
        value.set_sensitive(true);
        value
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(…)")
    }
}

/// What the app tells the server of itself as it pairs; the server shows
/// it in its list of devices.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AboutApp {
    /// What it runs on: `linux`.
    pub platform: String,
    pub app_version: String,
    /// This computer's name.
    pub hostname: String,
}

/// A pairing that worked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paired {
    /// The server's number for this device.
    pub device_id: i64,
    /// The name it was given in Add device.
    pub name: String,
    /// Whether the token opens the whole API, which the UI needs, rather
    /// than syncing only.
    pub manages: bool,
    pub token: Token,
}

/// Why a pairing didn't work, in words for people.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairError {
    /// The server's own words for a refusal, or what went wrong on the way.
    pub message: String,
    /// The server's code for a refusal (`wrong_code`, `code_expired`, …).
    pub code: Option<String>,
    /// The error underneath, when it adds something.
    pub detail: Option<String>,
}

impl fmt::Display for PairError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PairError {}

impl PairError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: None,
            detail: None,
        }
    }

    fn unreachable(unreachable: Unreachable) -> Self {
        Self {
            detail: unreachable.detail,
            ..Self::new(unreachable.reason)
        }
    }
}

/// The length of an answer that can be read here as one of the server's
/// refusals: JSON, not compressed, and short, which it says in
/// `Content-Length`. `None` for any other, which is passed on unread (a
/// reverse proxy may compress the server's answers, or send them in
/// chunks): see [`token_refused`].
pub(crate) fn small_json(headers: &HeaderMap) -> Option<usize> {
    let json = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|essence| essence.trim().eq_ignore_ascii_case("application/json"));
    let plain = headers
        .get(CONTENT_ENCODING)
        .is_none_or(|encoding| encoding == "identity");
    let length: usize = headers.get(CONTENT_LENGTH)?.to_str().ok()?.parse().ok()?;
    (json && plain && length <= ANSWER_LIMIT).then_some(length)
}

/// Asks `upstream` itself whether it still takes the token the proxy sends
/// it: for a 401 whose body couldn't be read on its way to the page. The
/// question is the session's (`/api/session`), asked uncompressed, and its
/// answer is the proxy's alone, so it's read whole however it's framed.
/// `false` when that can't be told: the token stays until the server
/// plainly refuses it.
pub(crate) async fn token_refused(
    client: &HttpsClient,
    upstream: &Upstream,
    timeout: Duration,
) -> bool {
    let (Some(authorization), Some(uri)) = (
        upstream.authorization(),
        upstream.uri(&format!("{}/api/session", upstream.prefix())),
    ) else {
        return false;
    };
    let Ok(req) = Request::get(uri)
        .header(HOST, upstream.host())
        .header(AUTHORIZATION, authorization)
        .header(ACCEPT, "application/json")
        .header(ACCEPT_ENCODING, "identity")
        .body(Body::empty())
    else {
        return false;
    };
    let asked = async {
        let res = client.request(req).await.ok()?;
        if res.status() != StatusCode::UNAUTHORIZED {
            return Some(false);
        }
        let body = Limited::new(res.into_body(), ANSWER_LIMIT).collect().await;
        Some(says_invalid_token(&body.ok()?.to_bytes()))
    };
    matches!(tokio::time::timeout(timeout, asked).await, Ok(Some(true)))
}

/// Whether a 401's body is the server refusing a token: its `code` is
/// `invalid_token`. A wrong password at its login is a 401 too.
pub(crate) fn says_invalid_token(body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(body).is_ok_and(|refusal| refusal["code"] == "invalid_token")
}

/// Trades `code` for a token at `upstream`'s `/api/pair`. Redirects aren't
/// followed: a code goes to the address the user named and nowhere else.
pub(crate) async fn pair(
    client: &HttpsClient,
    upstream: &Upstream,
    code: &str,
    about: &AboutApp,
    timeout: Duration,
) -> Result<Paired, PairError> {
    match tokio::time::timeout(timeout, redeem(client, upstream, code, about)).await {
        Ok(answer) => answer,
        Err(_) => Err(PairError::unreachable(Unreachable::too_slow(
            upstream, timeout,
        ))),
    }
}

async fn redeem(
    client: &HttpsClient,
    upstream: &Upstream,
    code: &str,
    about: &AboutApp,
) -> Result<Paired, PairError> {
    let invalid = || PairError::new("The server's address isn't valid.");
    let uri = upstream
        .uri(&format!("{}/api/pair", upstream.prefix()))
        .ok_or_else(invalid)?;
    let mut body = json!({ "code": code });
    for (name, value) in [
        ("platform", &about.platform),
        ("app_version", &about.app_version),
        ("hostname", &about.hostname),
    ] {
        if !value.is_empty() {
            body[name] = Value::from(value.as_str());
        }
    }
    let req = Request::post(uri)
        .header(HOST, upstream.host())
        .header(CONTENT_TYPE, "application/json")
        .header(ACCEPT, "application/json")
        .body(Body::from(body.to_string()))
        .map_err(|_| invalid())?;
    let res = client
        .request(req)
        .await
        .map_err(|err| PairError::unreachable(Unreachable::from_error(upstream, &err)))?;
    let status = res.status();
    let answer = Limited::new(res.into_body(), ANSWER_LIMIT)
        .collect()
        .await
        .map(|body| body.to_bytes())
        .unwrap_or_default();
    let answer: Value = serde_json::from_slice(&answer).unwrap_or(Value::Null);
    if status != StatusCode::OK {
        // The server's refusals say what to do next.
        return Err(match answer["error"].as_str() {
            Some(message) => PairError {
                code: answer["code"].as_str().map(str::to_owned),
                ..PairError::new(message)
            },
            None => PairError::new(format!("The server answered {status} to the pairing.")),
        });
    }
    let token = answer["token"].as_str().and_then(Token::new);
    match (token, answer["device_id"].as_i64()) {
        (Some(token), Some(device_id)) => Ok(Paired {
            device_id,
            name: answer["name"].as_str().unwrap_or_default().to_owned(),
            manages: answer["scope"] == "manage",
            token,
        }),
        _ => Err(PairError::new(
            "The server's answer to the pairing wasn't one this app understands.",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_never_shows_in_debug_output() {
        let token = Token::new("cst_0123456789abcdef").unwrap();
        assert_eq!(format!("{token:?}"), "Token(…)");
        let header = token.authorization();
        assert_eq!(header.to_str().unwrap(), "Bearer cst_0123456789abcdef");
        assert!(!format!("{header:?}").contains("cst_"));
        let upstream = Upstream::parse("https://music.example/")
            .unwrap()
            .with_authorization(Some(header));
        assert!(!format!("{upstream:?}").contains("cst_"), "{upstream:?}");
        let paired = Paired {
            device_id: 3,
            name: "Laptop".into(),
            manages: true,
            token,
        };
        assert!(!format!("{paired:?}").contains("cst_"));
    }

    #[test]
    fn tells_a_refused_token_from_other_refusals() {
        assert!(says_invalid_token(
            br#"{"error":"This app's token isn't valid; pair it again.","code":"invalid_token"}"#
        ));
        for other in [
            &br#"{"error":"That password isn't right.","code":"wrong_password"}"#[..],
            br#"{"error":"Sign in to continue.","code":"login_required"}"#,
            br#"{"code":["invalid_token"]}"#,
            b"invalid_token",
            b"",
        ] {
            assert!(
                !says_invalid_token(other),
                "{}",
                String::from_utf8_lossy(other)
            );
        }

        let headers = |content_type: &'static str, length: &'static str| {
            let mut headers = HeaderMap::new();
            headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
            headers.insert(CONTENT_LENGTH, HeaderValue::from_static(length));
            headers
        };
        assert_eq!(small_json(&headers("application/json", "83")), Some(83));
        assert_eq!(
            small_json(&headers("application/json; charset=utf-8", "0")),
            Some(0)
        );
        // A page, a stream without a length, a compressed answer, or more
        // than a refusal comes to.
        let mut gzipped = headers("application/json", "83");
        gzipped.insert(CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        assert_eq!(small_json(&gzipped), None);
        assert_eq!(small_json(&headers("text/html", "83")), None);
        assert_eq!(small_json(&headers("application/json", "70000")), None);
        let mut unsized_ = headers("application/json", "1");
        unsized_.remove(CONTENT_LENGTH);
        assert_eq!(small_json(&unsized_), None);
    }

    #[test]
    fn a_token_has_to_fit_a_header() {
        assert!(Token::new("").is_none());
        assert!(Token::new("cst_abc\r\nX-Injected: 1").is_none());
        assert!(Token::new("cst_abc").is_some());
    }
}
