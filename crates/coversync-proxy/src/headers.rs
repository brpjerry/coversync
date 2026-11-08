//! Header changes on the way to the server and back.

use axum::http::header::{
    ALT_SVC, AUTHORIZATION, CONNECTION, COOKIE, EXPECT, HOST, LOCATION, ORIGIN, REFERER,
    STRICT_TRANSPORT_SECURITY,
};
use axum::http::{HeaderMap, HeaderName, HeaderValue};

use crate::upstream::Upstream;

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
    // The server's own absolute URLs ("https://n5pro.internal/…" or
    // "//n5pro.internal/…") would take the window out of the proxy.
    if let Some(location) = headers.get(LOCATION).and_then(|l| l.to_str().ok())
        && let Some(rest) = upstream.own_location(location)
        && let Ok(value) = HeaderValue::from_str(&format!("{local_origin}{rest}"))
    {
        headers.insert(LOCATION, value);
    }
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

/// Whether the request carries the proxy's cookie, `name`, with this value:
/// proof that it comes from the app's window.
pub(crate) fn has_own_cookie(headers: &HeaderMap, name: &str, value: &str) -> bool {
    cookie_pairs(headers).any(|pair| {
        cookie_name(pair) == name.as_bytes()
            && pair
                .splitn(2, |&b| b == b'=')
                .nth(1)
                .is_some_and(|v| constant_time_eq(v.trim_ascii(), value.as_bytes()))
    })
}

/// Compares secrets without returning early on the first difference.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use axum::http::header::TRANSFER_ENCODING;

    use super::*;

    #[test]
    fn finds_the_own_cookie() {
        let mut headers = HeaderMap::new();
        headers.append(
            COOKIE,
            HeaderValue::from_static("session=abc; coversync-app-1=k1"),
        );
        headers.append(COOKIE, HeaderValue::from_static("coversync-app-2=k2"));
        assert!(has_own_cookie(&headers, "coversync-app-1", "k1"));
        assert!(has_own_cookie(&headers, "coversync-app-2", "k2"));
        assert!(!has_own_cookie(&headers, "coversync-app-1", "k2"));
        assert!(!has_own_cookie(&headers, "coversync-app-1", "k"));
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
