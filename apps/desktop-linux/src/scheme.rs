//! The app's own pages, at `coversync://app/`: the fallback page, built into
//! the binary from `apps/desktop/ui`, which the Windows app shows too.
//!
//! The scheme is registered as local, so a server's pages can't load or link
//! to it, and as secure and CORS-enabled, which module scripts need.

use webkit::{gio, glib, soup};

pub const SCHEME: &str = "coversync";
/// The fallback page.
pub const FALLBACK: &str = "coversync://app/index.html";
/// The origin of the app's pages.
const ORIGIN: &str = "coversync://app";

const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; \
                   font-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// A file under `coversync://app/`: its path, content type and bytes.
struct File(&'static str, &'static str, &'static [u8]);

macro_rules! ui {
    ($path:literal, $type:literal) => {
        File(
            concat!("/", $path),
            $type,
            include_bytes!(concat!("../../desktop/ui/", $path)),
        )
    };
}

const FILES: &[File] = &[
    ui!("index.html", "text/html; charset=utf-8"),
    ui!("fallback.css", "text/css; charset=utf-8"),
    ui!("fallback.js", "text/javascript; charset=utf-8"),
    ui!("bridge.js", "text/javascript; charset=utf-8"),
    ui!("logo.svg", "image/svg+xml"),
    ui!("fonts/inter-latin-wght-normal.woff2", "font/woff2"),
];

/// Serves the app's pages from `context`, the web context of the app's view.
pub fn register(context: &webkit::WebContext) {
    match context.security_manager() {
        Some(security) => {
            security.register_uri_scheme_as_local(SCHEME);
            security.register_uri_scheme_as_secure(SCHEME);
            security.register_uri_scheme_as_cors_enabled(SCHEME);
        }
        None => tracing::warn!("couldn't register {SCHEME}:// as local: no security manager"),
    }
    context.register_uri_scheme(SCHEME, serve);
}

/// Whether `uri` is one of the app's own pages.
pub fn is_app_page(uri: &str) -> bool {
    uri.starts_with("coversync://app/")
}

fn serve(request: &webkit::URISchemeRequest) {
    let uri = request.uri().map(String::from).unwrap_or_default();
    let path = request.path().map(String::from).unwrap_or_default();
    let path = if path == "/" { "/index.html" } else { &path };
    let file = is_app_page(&uri)
        .then(|| FILES.iter().find(|file| file.0 == path))
        .flatten();
    let Some(File(_, content_type, bytes)) = file else {
        let mut err = glib::Error::new(gio::IOErrorEnum::NotFound, &format!("No {uri} here"));
        request.finish_error(&mut err);
        return;
    };
    let stream = gio::MemoryInputStream::from_bytes(&glib::Bytes::from_static(bytes));
    let response = webkit::URISchemeResponse::new(&stream, bytes.len() as i64);
    response.set_content_type(content_type);
    let headers = soup::MessageHeaders::new(soup::MessageHeadersType::Response);
    headers.append("Content-Security-Policy", CSP);
    // WebKit fetches module scripts in CORS mode, and doesn't take a custom
    // scheme's page and script for the same origin without this.
    headers.append("Access-Control-Allow-Origin", ORIGIN);
    headers.append("Cache-Control", "no-store");
    response.set_http_headers(headers);
    request.finish_with_response(&response);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serves_only_its_own_pages() {
        assert!(is_app_page(FALLBACK));
        assert!(is_app_page("coversync://app/fallback.js"));
        assert!(!is_app_page("coversync://other/index.html"));
        assert!(!is_app_page("http://127.0.0.1:40000/coversync/"));
        assert!(!is_app_page(
            "https://coversync.example.com/coversync://app/"
        ));
    }

    #[test]
    fn has_every_file_the_page_loads() {
        let page = std::str::from_utf8(FILES[0].2).unwrap();
        for wanted in ["logo.svg", "fallback.css", "fallback.js"] {
            assert!(page.contains(wanted), "{wanted}");
            assert!(
                FILES.iter().any(|file| file.0 == format!("/{wanted}")),
                "{wanted}"
            );
        }
        let script = std::str::from_utf8(FILES[2].2).unwrap();
        assert!(script.contains("./bridge.js"));
    }
}
