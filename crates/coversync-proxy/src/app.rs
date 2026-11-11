//! The app's commands for a server's pages.
//!
//! The UI comes from the server, so it has to ask the app around it what
//! that app can do. It asks the proxy it was loaded from, at a path the proxy
//! answers itself and never forwards: `POST /__coversync-app/v1/<command>`
//! with a JSON body ([`RESERVED`]). The path is outside every server's
//! prefix; a server at the root of its domain never serves it either
//! (`coversyncd`'s router refuses it).
//!
//! The UI finds the app by the [`HEADER`] the proxy adds to `/api/hello`
//! answers, so in a browser, where the path would reach the server, it never
//! asks.
//!
//! Any script on the server's page can send these, one let in by a bug in
//! the UI included, so a command must be safe for all of them: none takes a
//! local path, hands out the app's token, or pairs. What they are is the
//! app's to say ([`AppCommands`]); the proxy only decides who may ask:
//!
//! - what every request needs: the proxy's own `Host`, its cookie, and no
//!   sign of another origin (see `handle` in the crate's root);
//! - `Sec-Fetch-Site: same-origin` and an `Origin` that is the proxy's own,
//!   both present: a page of another origin, a frame without one (`null`)
//!   and a program that isn't a browser all lack one or the other;
//! - a POST of `application/json`, which a form can't send.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request;
use axum::http::header::{ALLOW, CACHE_CONTROL, CONTENT_TYPE, ORIGIN};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use http_body_util::{BodyExt, Limited};
use serde_json::{Value, json};

use crate::upstream::Upstream;

/// On the proxy's `/api/hello` answers when the app takes commands: the
/// versions of the interface it speaks, as `1` or `1, 2`. Version `1` is
/// under `/__coversync-app/v1/`.
pub const HEADER: HeaderName = HeaderName::from_static("coversync-app");

/// The path the proxy keeps for the app, and everything under it.
pub const RESERVED: &str = "/__coversync-app";

/// The versions in [`HEADER`].
pub(crate) const VERSIONS: HeaderValue = HeaderValue::from_static("1");

/// Under [`RESERVED`]: version 1's commands.
const V1: &str = "/v1/";

/// More than a command's arguments ever come to.
const BODY_LIMIT: usize = 64 * 1024;

/// Why a command wasn't run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AppError {
    /// The app has no such command: an older app, asked by a newer UI.
    Unknown,
    /// Its arguments aren't what the command takes.
    Invalid(String),
    /// It was tried and didn't work.
    Failed(String),
}

/// A command's answer, once it has one: any JSON.
pub type AppAnswer = Pin<Box<dyn Future<Output = Result<Value, AppError>> + Send>>;

/// The app's commands for a server's pages.
pub trait AppCommands: Send + Sync {
    /// Runs `command` with `args` (the request's JSON body) for a page of
    /// `server`'s UI. Whatever it does must be safe for any script on that
    /// page to ask for.
    fn call(&self, server: &Upstream, command: &str, args: Value) -> AppAnswer;
}

/// Whether `path` is the app's, never the server's.
pub(crate) fn reserved(path: &str) -> bool {
    path.strip_prefix(RESERVED)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Answers a request to a [`reserved`] path, which has passed the checks
/// every request gets.
pub(crate) async fn answer(
    commands: Option<&Arc<dyn AppCommands>>,
    own_origin: &str,
    upstream: Option<&Upstream>,
    req: Request,
) -> Response {
    if !same_origin(req.headers(), own_origin) {
        return error(
            StatusCode::FORBIDDEN,
            "forbidden",
            "Only the server's own pages can ask the app.",
        );
    }
    if req.method() != Method::POST {
        let mut refused = error(
            StatusCode::METHOD_NOT_ALLOWED,
            "method",
            "The app's commands are POSTs.",
        );
        refused
            .headers_mut()
            .insert(ALLOW, HeaderValue::from_static("POST"));
        return refused;
    }
    if !is_json(req.headers()) {
        return error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "not_json",
            "The app's commands take JSON.",
        );
    }
    let unknown = || {
        error(
            StatusCode::NOT_FOUND,
            "unknown_command",
            "The app has no such command.",
        )
    };
    let path = req.uri().path();
    let Some(command) = path
        .strip_prefix(RESERVED)
        .and_then(|rest| rest.strip_prefix(V1))
        .filter(|command| is_command_name(command))
        .map(str::to_owned)
    else {
        return unknown();
    };
    // Only a window's app has commands, and they are about its server.
    let (Some(commands), Some(upstream)) = (commands, upstream) else {
        return unknown();
    };
    let body = match Limited::new(req.into_body(), BODY_LIMIT).collect().await {
        Ok(body) => body.to_bytes(),
        Err(_) => {
            return error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "too_large",
                "That's more than a command takes.",
            );
        }
    };
    let Ok(args) = serde_json::from_slice::<Value>(&body) else {
        return error(StatusCode::BAD_REQUEST, "invalid", "That isn't JSON.");
    };
    match commands.call(upstream, &command, args).await {
        Ok(value) => json_response(StatusCode::OK, &value),
        Err(AppError::Unknown) => unknown(),
        Err(AppError::Invalid(why)) => error(StatusCode::BAD_REQUEST, "invalid", &why),
        Err(AppError::Failed(why)) => error(StatusCode::INTERNAL_SERVER_ERROR, "failed", &why),
    }
}

/// Whether the browser says the request comes from a page of the proxy's
/// own origin: both headers there, and both saying so.
fn same_origin(headers: &HeaderMap, own: &str) -> bool {
    headers
        .get("sec-fetch-site")
        .is_some_and(|site| site == "same-origin")
        && headers.get(ORIGIN).is_some_and(|origin| origin == own)
}

fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|essence| essence.trim().eq_ignore_ascii_case("application/json"))
}

/// A command's name: letters, digits, `-` and `_`, so nothing in it reads
/// as another path.
fn is_command_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// An error in the server's own shape (`{"error": …}`), with a `code` the
/// UI can tell apart.
fn error(status: StatusCode, code: &str, message: &str) -> Response {
    json_response(status, &json!({ "error": message, "code": code }))
}

fn json_response(status: StatusCode, value: &Value) -> Response {
    (
        status,
        [
            (CONTENT_TYPE, "application/json"),
            (CACHE_CONTROL, "no-store"),
        ],
        Body::from(value.to_string()),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_only_its_own_path() {
        for own in [
            "/__coversync-app",
            "/__coversync-app/",
            "/__coversync-app/v1/capabilities",
            "/__coversync-app/v2/anything/else",
        ] {
            assert!(reserved(own), "{own}");
        }
        for servers in [
            "/",
            "/__coversync-apps",
            "/__coversync-app.js",
            "/coversync/__coversync-app/v1/capabilities",
            "/api/hello",
        ] {
            assert!(!reserved(servers), "{servers}");
        }
    }

    #[test]
    fn a_command_name_is_one_path_segment() {
        for name in ["capabilities", "open-settings", "media_keys", "v2"] {
            assert!(is_command_name(name), "{name}");
        }
        for name in ["", "a/b", "..", "a%2fb", "a b", "é", &"a".repeat(65)] {
            assert!(!is_command_name(name), "{name}");
        }
    }

    #[test]
    fn json_is_told_by_its_type() {
        let typed = |value: &'static str| {
            let mut headers = HeaderMap::new();
            headers.insert(CONTENT_TYPE, HeaderValue::from_static(value));
            is_json(&headers)
        };
        assert!(typed("application/json"));
        assert!(typed("Application/JSON; charset=utf-8"));
        assert!(!typed("text/plain"));
        assert!(!typed("application/x-www-form-urlencoded"));
        assert!(!typed("application/jsonp"));
        assert!(!is_json(&HeaderMap::new()));
    }
}
