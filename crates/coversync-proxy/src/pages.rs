//! The proxy's own answers.

use std::fmt::Write;

use axum::body::Body;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, LOCATION, REFERRER_POLICY, SET_COOKIE};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::Unreachable;

const HTML: &str = "text/html; charset=utf-8";

pub(crate) fn text(status: StatusCode, message: &'static str) -> Response {
    (
        status,
        [
            (CONTENT_TYPE, "text/plain; charset=utf-8"),
            (CACHE_CONTROL, "no-store"),
        ],
        message,
    )
        .into_response()
}

pub(crate) fn forbidden() -> Response {
    text(
        StatusCode::FORBIDDEN,
        "This is the coversync app's own connection.",
    )
}

pub(crate) fn see_other(to: &str) -> Response {
    match HeaderValue::from_str(to) {
        Ok(location) => (
            StatusCode::SEE_OTHER,
            [
                (LOCATION, location),
                (CACHE_CONTROL, HeaderValue::from_static("no-store")),
            ],
        )
            .into_response(),
        Err(_) => text(StatusCode::NOT_FOUND, "Not found."),
    }
}

/// The answer to the entry URL: the proxy's cookie, and a page that moves on
/// to `to`, leaving the secret out of the address and the history.
///
/// It isn't a 303. The window may have come from the app's bundled page, a
/// different site, and a browser may then hold back a SameSite=Strict cookie
/// on the redirect that follows. A navigation this page starts is same-site.
pub(crate) fn enter(cookie_name: &str, cookie: &str, to: &str) -> Response {
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>coversync</title>\
         <script>location.replace({})</script><a href=\"{}\">Continue</a>\n",
        js_string(to),
        html_escape(to)
    );
    let cookie = format!("{cookie_name}={cookie}; Path=/; HttpOnly; SameSite=Strict");
    Response::builder()
        .header(SET_COOKIE, cookie)
        .header(CONTENT_TYPE, HTML)
        .header(CACHE_CONTROL, "no-store")
        .header(REFERRER_POLICY, "no-referrer")
        .body(Body::from(body))
        .expect("a valid response")
}

/// What a page load gets when the server can't be reached.
pub(crate) fn unreachable_page(unreachable: &Unreachable) -> Response {
    let detail = unreachable
        .detail
        .as_deref()
        .map(|detail| format!("<p><small>{}</small></p>", html_escape(detail)))
        .unwrap_or_default();
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>Can't reach the server</title>\
         <h1>Can't reach the server</h1><p>{}</p>{detail}<p><a href=\"\">Try again</a></p>\n",
        html_escape(&unreachable.to_string())
    );
    (
        StatusCode::BAD_GATEWAY,
        [(CONTENT_TYPE, HTML), (CACHE_CONTROL, "no-store")],
        body,
    )
        .into_response()
}

/// What an API call gets when the server can't be reached, shaped like the
/// server's own errors (`{"error": …}`), which the UI shows.
pub(crate) fn unreachable_json(unreachable: &Unreachable) -> Response {
    let body = format!("{{\"error\":{}}}", js_string(&unreachable.to_string()));
    (
        StatusCode::BAD_GATEWAY,
        [
            (CONTENT_TYPE, "application/json"),
            (CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response()
}

/// A JSON string, also safe inside a `<script>`.
fn js_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '<' | '>' | '&' | '\u{2028}' | '\u{2029}' | '\0'..='\x1f' => {
                write!(out, "\\u{:04x}", c as u32).expect("writing to a String");
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_strings_for_scripts_and_markup() {
        assert_eq!(
            js_string("/a\"b</script>\n"),
            r#""/a\"b\u003c/script\u003e\u000a""#
        );
        assert_eq!(
            html_escape("<a href=\"x\">&'"),
            "&lt;a href=&quot;x&quot;&gt;&amp;&#39;"
        );
    }
}
