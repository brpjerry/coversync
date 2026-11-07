//! The server the proxy forwards to.

use std::fmt;

use axum::http::uri::{Authority, Scheme};
use axum::http::{HeaderValue, Uri};
use url::Url;

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
    /// The origin as a URL, to resolve redirects against.
    base: Url,
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
    /// a scheme, `https://` is assumed.
    ///
    /// It's read the way a browser reads it, so the prefix is the path the
    /// webview asks for: the scheme's default port goes, dot segments are
    /// resolved, other characters are percent-encoded and an international
    /// name becomes its `xn--` form. Repeated and trailing slashes go too.
    pub fn parse(input: &str) -> Result<Self, UpstreamError> {
        let input = input.trim();
        if input.is_empty() {
            return Err(UpstreamError("Enter the server's address.".into()));
        }
        let input = if input.contains("://") {
            input.to_owned()
        } else {
            format!("https://{input}")
        };
        let invalid = || UpstreamError(format!("{input} isn't a valid address."));
        let url = Url::parse(&input).map_err(|_| invalid())?;
        let scheme = match url.scheme() {
            "https" => Scheme::HTTPS,
            "http" => Scheme::HTTP,
            _ => {
                return Err(UpstreamError(format!(
                    "{input} isn't an http:// or https:// address."
                )));
            }
        };
        if !url.username().is_empty() || url.password().is_some() {
            return Err(UpstreamError(format!(
                "{input} has a user name in it; leave it out."
            )));
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err(UpstreamError(format!(
                "{input} has a ? or # part; leave it out."
            )));
        }
        let host = url.host_str().ok_or_else(invalid)?;
        let authority = match url.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_owned(),
        };
        let authority: Authority = authority.parse().map_err(|_| invalid())?;
        let host = HeaderValue::from_str(authority.as_str()).map_err(|_| invalid())?;
        let origin = format!("{scheme}://{authority}");
        let base = Url::parse(&origin).map_err(|_| invalid())?;
        let mut prefix = String::with_capacity(url.path().len());
        for segment in url.path().split('/').filter(|s| !s.is_empty()) {
            prefix.push('/');
            prefix.push_str(segment);
        }
        Ok(Self {
            scheme,
            authority,
            host,
            origin,
            base,
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

    /// `https://n5pro.internal`, without the scheme's default port.
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

    /// Where a `Location` from the server leads, if it's on the server's own
    /// origin: its path, query and fragment. Absolute (`https://…`) and
    /// scheme-relative (`//…`) references are resolved the way a browser
    /// would, so a default port written out (`:443`) or left out matches.
    /// A relative reference (`/coversync/`) needs no change and gives `None`.
    pub(crate) fn own_location(&self, location: &str) -> Option<String> {
        let absolute = location.starts_with("//") || location.contains("://");
        if !absolute {
            return None;
        }
        let target = self.base.join(location).ok()?;
        (target.origin() == self.base.origin())
            .then(|| target[url::Position::BeforePath..].to_owned())
    }

    /// Whether a redirect from the server stays on its origin. `location` is
    /// resolved against `from`, the URL that answered with it.
    pub(crate) fn keeps_origin(&self, from: &Url, location: &str) -> Result<(), String> {
        let target = from.join(location).map_err(|_| location.to_owned())?;
        if target.origin() == self.base.origin() {
            Ok(())
        } else {
            Err(target.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn up(url: &str) -> Upstream {
        Upstream::parse(url).unwrap()
    }

    #[test]
    fn reads_the_origin_and_prefix() {
        let up_ = up("https://n5pro.internal/coversync/");
        assert_eq!(up_.origin(), "https://n5pro.internal");
        assert_eq!(up_.prefix(), "/coversync");
        assert_eq!(up_.url(), "https://n5pro.internal/coversync/");
        assert_eq!(up_.host(), "n5pro.internal");

        let local = up("http://127.0.0.1:8770");
        assert_eq!(local.origin(), "http://127.0.0.1:8770");
        assert_eq!(local.prefix(), "");
        assert_eq!(local.url(), "http://127.0.0.1:8770/");
        assert_eq!(local.host(), "127.0.0.1:8770");
    }

    #[test]
    fn reads_it_as_a_browser_does() {
        assert_eq!(
            up("  N5Pro.Internal/coversync ").url(),
            "https://n5pro.internal/coversync/"
        );
        // A default port goes; another stays.
        assert_eq!(
            up("https://n5pro.internal:443/coversync/").origin(),
            "https://n5pro.internal"
        );
        assert_eq!(up("http://host:80/").origin(), "http://host");
        assert_eq!(up("https://host:8443/").origin(), "https://host:8443");
        // The path the webview asks for: percent-encoded, dot segments resolved.
        assert_eq!(up("https://server/音乐/").prefix(), "/%E9%9F%B3%E4%B9%90");
        assert_eq!(up("https://server/a/../coversync/").prefix(), "/coversync");
        assert_eq!(up("https://server//coversync//").prefix(), "/coversync");
        assert_eq!(
            up("https://Bücher.example/").host(),
            "xn--bcher-kva.example"
        );
        assert_eq!(up("http://[::1]:8770/").host(), "[::1]:8770");
    }

    #[test]
    fn refuses_what_it_cant_use() {
        for url in [
            "",
            "ftp://n5pro.internal/",
            "https://user:pw@n5pro.internal/",
            "https://n5pro.internal/coversync/?x=1",
            "https://n5pro.internal/coversync/#x",
            "https://",
            "https://exa mple/",
        ] {
            assert!(Upstream::parse(url).is_err(), "{url}");
        }
    }

    #[test]
    fn serves_only_paths_under_the_prefix() {
        let up_ = up("https://n5pro.internal/coversync/");
        assert!(up_.serves("/coversync"));
        assert!(up_.serves("/coversync/"));
        assert!(up_.serves("/coversync/api/status"));
        assert!(!up_.serves("/coversyncx"));
        assert!(!up_.serves("/"));
        assert!(!up_.serves("/favicon.ico"));

        let root = up("https://n5pro.internal/");
        assert!(root.serves("/"));
        assert!(root.serves("/api/status"));
    }

    #[test]
    fn knows_its_own_locations() {
        let up_ = up("https://n5pro.internal/coversync/");
        let own = |l| up_.own_location(l);
        assert_eq!(
            own("https://n5pro.internal/coversync/?a=1").as_deref(),
            Some("/coversync/?a=1")
        );
        assert_eq!(
            own("https://n5pro.internal:443/coversync/").as_deref(),
            Some("/coversync/")
        );
        assert_eq!(
            own("//n5pro.internal/coversync/").as_deref(),
            Some("/coversync/")
        );
        assert_eq!(own("//elsewhere/coversync/"), None);
        assert_eq!(
            own("http://n5pro.internal/coversync/"),
            None,
            "another scheme"
        );
        assert_eq!(own("https://n5pro.internal:8443/"), None, "another port");
        assert_eq!(own("/coversync/"), None, "relative: left as it is");

        // Given with its default port, it still knows locations without one.
        let explicit = up("https://n5pro.internal:443/coversync/");
        assert_eq!(
            explicit
                .own_location("https://n5pro.internal/coversync/")
                .as_deref(),
            Some("/coversync/")
        );
    }

    #[test]
    fn tells_where_redirects_lead() {
        let up_ = up("http://n5pro.internal/coversync/");
        let from = Url::parse(&up_.url()).unwrap();
        assert_eq!(up_.keeps_origin(&from, "/coversync/login"), Ok(()));
        assert_eq!(up_.keeps_origin(&from, "//n5pro.internal/x"), Ok(()));
        assert_eq!(
            up_.keeps_origin(&from, "https://n5pro.internal/coversync/"),
            Err("https://n5pro.internal/coversync/".to_owned())
        );
    }

    #[test]
    fn keeps_the_token_out_of_debug_output() {
        let up_ = up("https://n5pro.internal/")
            .with_authorization(Some(HeaderValue::from_static("Bearer secret")));
        assert!(!format!("{up_:?}").contains("secret"));
        assert_eq!(up_.authorization().unwrap(), "Bearer secret");
    }
}
