//! The server the proxy forwards to.

use std::fmt;

use axum::http::uri::{Authority, Scheme};
use axum::http::{HeaderValue, Uri};

/// A coversync server: its origin and the path prefix it serves the UI under.
///
/// `https://n5pro.internal/coversync/` has the origin `https://n5pro.internal`
/// and the prefix `/coversync`. The proxy serves the same paths locally, so a
/// UI built with `BASE_PATH=/coversync` finds itself at
/// `http://127.0.0.1:<port>/coversync/`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Upstream {
    scheme: Scheme,
    authority: Authority,
    host: HeaderValue,
    origin: String,
    prefix: String,
    authorization: Option<HeaderValue>,
}

/// Why a server URL can't be used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpstreamError(String);

impl fmt::Display for UpstreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UpstreamError {}

impl Upstream {
    /// Reads a server URL such as `https://n5pro.internal/coversync/`. Without
    /// a scheme, `https://` is assumed. A trailing `/` makes no difference.
    pub fn parse(url: &str) -> Result<Self, UpstreamError> {
        let url = url.trim();
        if url.is_empty() {
            return Err(UpstreamError("Enter the server's address.".into()));
        }
        let url = if url.contains("://") {
            url.to_owned()
        } else {
            format!("https://{url}")
        };
        let invalid = || UpstreamError(format!("{url} isn't a valid address."));
        let uri: Uri = url.parse().map_err(|_| invalid())?;
        let scheme = uri.scheme().cloned().ok_or_else(invalid)?;
        if scheme != Scheme::HTTP && scheme != Scheme::HTTPS {
            return Err(UpstreamError(format!(
                "{url} isn't an http:// or https:// address."
            )));
        }
        let authority = uri.authority().ok_or_else(invalid)?;
        if authority.as_str().contains('@') {
            return Err(UpstreamError(format!(
                "{url} has a user name in it; leave it out."
            )));
        }
        if uri.query().is_some() {
            return Err(UpstreamError(format!(
                "{url} has a query (?…) in it; leave it out."
            )));
        }
        let authority: Authority = authority
            .as_str()
            .to_ascii_lowercase()
            .parse()
            .map_err(|_| invalid())?;
        let host = HeaderValue::from_str(authority.as_str()).map_err(|_| invalid())?;
        let origin = format!("{scheme}://{authority}");
        let prefix = uri.path().trim_end_matches('/').to_owned();
        Ok(Self {
            scheme,
            authority,
            host,
            origin,
            prefix,
            authorization: None,
        })
    }

    /// The `Authorization` header the proxy adds to every request it forwards,
    /// replacing the webview's own. `None` (the default) passes the webview's
    /// through. The app's token goes here once it has one (DSK-02).
    pub fn with_authorization(mut self, authorization: Option<HeaderValue>) -> Self {
        self.authorization = authorization.map(|mut value| {
            value.set_sensitive(true);
            value
        });
        self
    }

    /// The server's URL, normalized: `https://n5pro.internal/coversync/`.
    pub fn url(&self) -> String {
        format!("{}{}/", self.origin, self.prefix)
    }

    /// `https://n5pro.internal`.
    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// The path the UI is served under, without a trailing `/`: `/coversync`,
    /// or empty for a server at the root.
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    pub fn authorization(&self) -> Option<&HeaderValue> {
        self.authorization.as_ref()
    }

    /// The `Host` header for requests to the server.
    pub(crate) fn host(&self) -> &HeaderValue {
        &self.host
    }

    /// Whether a local path is under the prefix, so the proxy forwards it.
    pub(crate) fn serves(&self, path: &str) -> bool {
        match path.strip_prefix(self.prefix.as_str()) {
            Some(rest) => self.prefix.is_empty() || rest.is_empty() || rest.starts_with('/'),
            None => false,
        }
    }

    /// The server's URL for a local path and query, which stay the same.
    pub(crate) fn uri(&self, path_and_query: &str) -> Option<Uri> {
        Uri::builder()
            .scheme(self.scheme.clone())
            .authority(self.authority.clone())
            .path_and_query(path_and_query)
            .build()
            .ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_origin_and_prefix() {
        let up = Upstream::parse("https://n5pro.internal/coversync/").unwrap();
        assert_eq!(up.origin(), "https://n5pro.internal");
        assert_eq!(up.prefix(), "/coversync");
        assert_eq!(up.url(), "https://n5pro.internal/coversync/");
        assert_eq!(up.host(), "n5pro.internal");

        let up = Upstream::parse("http://127.0.0.1:8770").unwrap();
        assert_eq!(up.origin(), "http://127.0.0.1:8770");
        assert_eq!(up.prefix(), "");
        assert_eq!(up.url(), "http://127.0.0.1:8770/");
        assert_eq!(up.host(), "127.0.0.1:8770");
    }

    #[test]
    fn assumes_https_and_lowercases_the_host() {
        let up = Upstream::parse("  N5Pro.Internal/coversync ").unwrap();
        assert_eq!(up.url(), "https://n5pro.internal/coversync/");
    }

    #[test]
    fn refuses_what_it_cant_use() {
        for url in [
            "",
            "ftp://n5pro.internal/",
            "https://user:pw@n5pro.internal/",
            "https://n5pro.internal/coversync/?x=1",
            "https:///nohost",
        ] {
            assert!(Upstream::parse(url).is_err(), "{url}");
        }
    }

    #[test]
    fn serves_only_paths_under_the_prefix() {
        let up = Upstream::parse("https://n5pro.internal/coversync/").unwrap();
        assert!(up.serves("/coversync"));
        assert!(up.serves("/coversync/"));
        assert!(up.serves("/coversync/api/status"));
        assert!(!up.serves("/coversyncx"));
        assert!(!up.serves("/"));
        assert!(!up.serves("/favicon.ico"));

        let root = Upstream::parse("https://n5pro.internal/").unwrap();
        assert!(root.serves("/"));
        assert!(root.serves("/api/status"));
    }

    #[test]
    fn keeps_the_token_out_of_debug_output() {
        let up = Upstream::parse("https://n5pro.internal/")
            .unwrap()
            .with_authorization(Some(HeaderValue::from_static("Bearer secret")));
        assert!(!format!("{up:?}").contains("secret"));
        assert_eq!(up.authorization().unwrap(), "Bearer secret");
    }
}
