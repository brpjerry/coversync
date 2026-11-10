//! The app's own pages, at `coversync://app/`: the fallback page, built into
//! the binary from `ui/`.
//!
//! The window serves them (`cpp/pages.cpp`), under a scheme registered as
//! local, so a server's pages can't load or link to it, and as secure and
//! CORS-enabled, which module scripts need. The fallback page's commands
//! travel the same way: a POST to [`BRIDGE`] (see [`crate::bridge`]).

pub const SCHEME: &str = "coversync";
/// The fallback page.
pub const FALLBACK: &str = "coversync://app/index.html";
/// The origin of the app's pages.
pub const ORIGIN: &str = "coversync://app";
/// Where the fallback page's commands are sent.
pub const BRIDGE: &str = "coversync://app/__bridge";

/// Nothing but the pages' own files; `connect-src` is for their commands,
/// which are fetches to the pages' own origin.
pub const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; \
                       font-src 'self'; connect-src 'self'; base-uri 'none'; form-action 'none'; \
                       frame-ancestors 'none'";

/// A file under `coversync://app/`.
pub struct Page {
    pub path: &'static str,
    pub content_type: &'static str,
    pub body: &'static [u8],
}

macro_rules! ui {
    ($path:literal, $type:literal) => {
        Page {
            path: concat!("/", $path),
            content_type: $type,
            body: include_bytes!(concat!("../ui/", $path)),
        }
    };
}

const PAGES: &[Page] = &[
    ui!("index.html", "text/html; charset=utf-8"),
    ui!("fallback.css", "text/css; charset=utf-8"),
    ui!("fallback.js", "text/javascript; charset=utf-8"),
    ui!("bridge.js", "text/javascript; charset=utf-8"),
    ui!("logo.svg", "image/svg+xml"),
    ui!("fonts/inter-latin-wght-normal.woff2", "font/woff2"),
];

/// The page at `path` (a URL's path: `/index.html`).
pub fn find(path: &str) -> Option<&'static Page> {
    let path = if path == "/" { "/index.html" } else { path };
    PAGES.iter().find(|page| page.path == path)
}

/// Whether `uri` is one of the app's own pages.
pub fn is_app_page(uri: &str) -> bool {
    uri.starts_with("coversync://app/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serves_only_its_own_pages() {
        assert!(is_app_page(FALLBACK));
        assert!(is_app_page("coversync://app/fallback.js"));
        assert!(is_app_page(BRIDGE));
        assert!(!is_app_page("coversync://other/index.html"));
        assert!(!is_app_page("http://127.0.0.1:40000/coversync/"));
        assert!(!is_app_page(
            "https://coversync.example.com/coversync://app/"
        ));
        assert!(FALLBACK.starts_with(ORIGIN) && FALLBACK.starts_with(SCHEME));
    }

    #[test]
    fn finds_pages_by_their_paths() {
        assert_eq!(find("/").map(|page| page.path), Some("/index.html"));
        assert_eq!(
            find("/logo.svg").map(|page| page.content_type),
            Some("image/svg+xml")
        );
        assert!(
            find("/__bridge").is_none(),
            "the commands' address is no file"
        );
        assert!(find("/../Cargo.toml").is_none());
        assert!(find("index.html").is_none());
    }

    #[test]
    fn has_every_file_the_page_loads() {
        let page = std::str::from_utf8(find("/index.html").unwrap().body).unwrap();
        for wanted in ["logo.svg", "fallback.css", "fallback.js"] {
            assert!(page.contains(wanted), "{wanted}");
            assert!(find(&format!("/{wanted}")).is_some(), "{wanted}");
        }
        let script = std::str::from_utf8(find("/fallback.js").unwrap().body).unwrap();
        assert!(script.contains("./bridge.js"));
        assert!(find("/bridge.js").is_some());
    }

    #[test]
    fn the_pages_may_only_reach_their_own_origin() {
        assert!(CSP.contains("default-src 'none'"));
        assert!(CSP.contains("connect-src 'self'"));
        assert!(!CSP.contains("unsafe"));
    }
}
