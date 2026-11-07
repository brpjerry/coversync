//! Header changes on the way to the server and back.

use axum::http::header::{
    ALT_SVC, AUTHORIZATION, CONNECTION, COOKIE, EXPECT, HOST, LOCATION, ORIGIN, REFERER,
    SET_COOKIE, STRICT_TRANSPORT_SECURITY,
};
use axum::http::{HeaderMap, HeaderName, HeaderValue};

use crate::upstream::Upstream;

/// The proxy's own cookie, which proves a request comes from the app's window.
pub(crate) const COOKIE_NAME: &str = "coversync-app";

/// Headers that describe one connection, not the request or response.
const HOP_BY_HOP: [&str; 9] = [
    "connection",
    "keep-alive",
    "proxy-connection",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

fn strip_hop_by_hop(headers: &mut HeaderMap) {
    let listed: Vec<HeaderName> = headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| HeaderName::from_bytes(name.trim().as_bytes()).ok())
        .collect();
    for name in listed {
        headers.remove(name);
    }
    for name in HOP_BY_HOP {
        headers.remove(name);
    }
}

/// What follows `origin` in `url`, if `url` is on that origin: `""` or a
/// string starting with `/`, `?` or `#`.
fn after_origin<'a>(url: &'a str, origin: &str) -> Option<&'a str> {
    let rest = url.strip_prefix(origin)?;
    (rest.is_empty() || rest.starts_with(['/', '?', '#'])).then_some(rest)
}

/// Prepares the webview's request for the server.
pub(crate) fn to_upstream(headers: &mut HeaderMap, local_origin: &str, upstream: &Upstream) {
    strip_hop_by_hop(headers);
    // hyper sends the body without waiting for 100 Continue.
    headers.remove(EXPECT);
    headers.insert(HOST, upstream.host().clone());
    if headers.get(ORIGIN).is_some_and(|o| o == local_origin) {
        let origin = HeaderValue::from_str(upstream.origin()).expect("an origin is a valid header");
        headers.insert(ORIGIN, origin);
    }
    if let Some(referer) = headers.get(REFERER).and_then(|r| r.to_str().ok())
        && let Some(rest) = after_origin(referer, local_origin)
        && let Ok(value) = HeaderValue::from_str(&format!("{}{rest}", upstream.origin()))
    {
        headers.insert(REFERER, value);
    }
    strip_own_cookie(headers);
    if let Some(authorization) = upstream.authorization() {
        headers.insert(AUTHORIZATION, authorization.clone());
    }
}

/// Prepares the server's response for the webview, which reaches it over
/// plain HTTP at `local_origin`.
pub(crate) fn from_upstream(headers: &mut HeaderMap, local_origin: &str, upstream: &Upstream) {
    strip_hop_by_hop(headers);
    headers.remove(STRICT_TRANSPORT_SECURITY);
    headers.remove(ALT_SVC);
    if let Some(location) = headers.get(LOCATION).and_then(|l| l.to_str().ok())
        && let Some(rest) = after_origin(location, upstream.origin())
        && let Ok(value) = HeaderValue::from_str(&format!("{local_origin}{rest}"))
    {
        headers.insert(LOCATION, value);
    }
    if headers.contains_key(SET_COOKIE) {
        let cookies: Vec<HeaderValue> = headers
            .get_all(SET_COOKIE)
            .iter()
            .map(local_set_cookie)
            .collect();
        headers.remove(SET_COOKIE);
        for cookie in cookies {
            headers.append(SET_COOKIE, cookie);
        }
    }
}

/// A server's `Set-Cookie` made to work at `http://127.0.0.1:<port>`:
/// without `Secure` the webview stores it over plain HTTP, and without
/// `Domain` it belongs to 127.0.0.1. `SameSite=None` needs `Secure`, so it
/// becomes `Lax`, and `Partitioned` (which needs `Secure` too) goes.
fn local_set_cookie(value: &HeaderValue) -> HeaderValue {
    let Ok(cookie) = value.to_str() else {
        return value.clone();
    };
    let mut parts = cookie.split(';');
    let mut out = parts.next().unwrap_or_default().trim().to_owned();
    for attribute in parts {
        let attribute = attribute.trim();
        let (name, val) = attribute.split_once('=').unwrap_or((attribute, ""));
        let name = name.trim();
        if name.eq_ignore_ascii_case("secure")
            || name.eq_ignore_ascii_case("domain")
            || name.eq_ignore_ascii_case("partitioned")
            || attribute.is_empty()
        {
            continue;
        }
        out.push_str("; ");
        if name.eq_ignore_ascii_case("samesite") && val.trim().eq_ignore_ascii_case("none") {
            out.push_str("SameSite=Lax");
        } else {
            out.push_str(attribute);
        }
    }
    HeaderValue::from_str(&out).unwrap_or_else(|_| value.clone())
}

/// `name=value` pairs of every `Cookie` header.
fn cookie_pairs(headers: &HeaderMap) -> impl Iterator<Item = &[u8]> {
    headers
        .get_all(COOKIE)
        .iter()
        .flat_map(|value| value.as_bytes().split(|&b| b == b';'))
        .map(<[u8]>::trim_ascii)
        .filter(|pair| !pair.is_empty())
}

fn cookie_name(pair: &[u8]) -> &[u8] {
    let name = pair.split(|&b| b == b'=').next().unwrap_or_default();
    name.trim_ascii()
}

/// Whether the request carries the proxy's cookie with this value.
pub(crate) fn has_own_cookie(headers: &HeaderMap, value: &str) -> bool {
    cookie_pairs(headers).any(|pair| {
        cookie_name(pair) == COOKIE_NAME.as_bytes()
            && pair
                .splitn(2, |&b| b == b'=')
                .nth(1)
                .is_some_and(|v| constant_time_eq(v.trim_ascii(), value.as_bytes()))
    })
}

/// Drops the proxy's cookie, which is no business of the server's, and keeps
/// the rest (the server's own session cookie, say).
fn strip_own_cookie(headers: &mut HeaderMap) {
    if !headers.contains_key(COOKIE) {
        return;
    }
    let kept: Vec<&[u8]> = cookie_pairs(headers)
        .filter(|pair| cookie_name(pair) != COOKIE_NAME.as_bytes())
        .collect();
    let joined = kept.join(&b"; "[..]);
    headers.remove(COOKIE);
    if !joined.is_empty()
        && let Ok(value) = HeaderValue::from_bytes(&joined)
    {
        headers.insert(COOKIE, value);
    }
}

/// Compares secrets without returning early on the first difference.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use axum::http::header::TRANSFER_ENCODING;

    use super::*;

    fn set_cookie(value: &'static str) -> String {
        local_set_cookie(&HeaderValue::from_static(value))
            .to_str()
            .unwrap()
            .to_owned()
    }

    #[test]
    fn server_cookies_lose_secure_and_domain() {
        assert_eq!(
            set_cookie(
                "session=abc; Path=/coversync; Domain=n5pro.internal; Secure; HttpOnly; SameSite=Lax; Max-Age=2592000"
            ),
            "session=abc; Path=/coversync; HttpOnly; SameSite=Lax; Max-Age=2592000"
        );
        assert_eq!(
            set_cookie("a=1;secure;samesite=none;Partitioned"),
            "a=1; SameSite=Lax"
        );
        assert_eq!(set_cookie("a=1"), "a=1");
    }

    #[test]
    fn the_own_cookie_stays_local() {
        let mut headers = HeaderMap::new();
        headers.append(
            COOKIE,
            HeaderValue::from_static("session=abc; coversync-app=k1"),
        );
        headers.append(COOKIE, HeaderValue::from_static("theme=dark"));
        assert!(has_own_cookie(&headers, "k1"));
        assert!(!has_own_cookie(&headers, "k2"));
        assert!(!has_own_cookie(&headers, "k"));
        strip_own_cookie(&mut headers);
        assert_eq!(headers.get_all(COOKIE).iter().count(), 1);
        assert_eq!(headers[COOKIE], "session=abc; theme=dark");

        let mut only_own = HeaderMap::new();
        only_own.insert(COOKIE, HeaderValue::from_static("coversync-app=k1"));
        strip_own_cookie(&mut only_own);
        assert!(!only_own.contains_key(COOKIE));
    }

    #[test]
    fn origins_match_whole() {
        let origin = "http://127.0.0.1:1234";
        assert_eq!(after_origin("http://127.0.0.1:1234", origin), Some(""));
        assert_eq!(
            after_origin("http://127.0.0.1:1234/coversync/", origin),
            Some("/coversync/")
        );
        assert_eq!(after_origin("http://127.0.0.1:12345/", origin), None);
        assert_eq!(after_origin("https://elsewhere/", origin), None);
    }

    #[test]
    fn drops_hop_by_hop_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(CONNECTION, HeaderValue::from_static("keep-alive, x-hop"));
        headers.insert("x-hop", HeaderValue::from_static("1"));
        headers.insert("keep-alive", HeaderValue::from_static("timeout=5"));
        headers.insert(TRANSFER_ENCODING, HeaderValue::from_static("chunked"));
        headers.insert("x-end", HeaderValue::from_static("1"));
        strip_hop_by_hop(&mut headers);
        assert_eq!(headers.len(), 1);
        assert!(headers.contains_key("x-end"));
    }
}
