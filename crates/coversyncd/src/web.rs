//! The built SvelteKit UI, embedded in the binary.

use std::sync::LazyLock;

use axum::Router;
use axum::extract::Request;
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use base64::Engine;
use rust_embed::RustEmbed;
use sha2::{Digest, Sha256};

use crate::Shared;

#[derive(RustEmbed)]
#[folder = "../../web/build"]
struct Assets;

/// Hashes of the inline `<script>` blocks SvelteKit puts in its shell page.
fn inline_script_hashes(html: &str) -> Vec<String> {
    let mut hashes = Vec::new();
    let mut rest = html;
    while let Some(start) = rest.find("<script") {
        rest = &rest[start..];
        let Some(open_end) = rest.find('>') else {
            break;
        };
        let open = &rest[..open_end];
        let body_start = open_end + 1;
        let Some(close) = rest[body_start..].find("</script>") else {
            break;
        };
        let body = &rest[body_start..body_start + close];
        if !open.contains(" src=") && !body.is_empty() {
            let digest = Sha256::digest(body.as_bytes());
            hashes.push(format!(
                "'sha256-{}'",
                base64::engine::general_purpose::STANDARD.encode(digest)
            ));
        }
        rest = &rest[body_start + close..];
    }
    hashes
}

/// The page's own policy, so a reverse proxy's site default (if any) doesn't apply.
static CSP: LazyLock<HeaderValue> = LazyLock::new(|| {
    let index = Assets::get("index.html")
        .map(|f| String::from_utf8_lossy(&f.data).into_owned())
        .unwrap_or_default();
    let scripts = inline_script_hashes(&index).join(" ");
    let policy = format!(
        "default-src 'none'; script-src 'self' {scripts}; style-src 'self' 'unsafe-inline'; \
         img-src 'self' data: blob:; media-src 'self'; font-src 'self'; connect-src 'self'; \
         manifest-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
    );
    HeaderValue::from_str(&policy).expect("valid CSP header")
});

async fn csp(req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    res.headers_mut()
        .insert(header::CONTENT_SECURITY_POLICY, CSP.clone());
    res
}

/// An embedded file, or 304 when the browser's copy is current. Behind a proxy
/// that adds `no-cache`, every load revalidates; the ETag keeps that cheap.
fn asset(path: &str, if_none_match: Option<&HeaderValue>) -> Option<Response> {
    let file = Assets::get(path)?;
    let hash = file.metadata.sha256_hash();
    let etag = format!(
        "\"{}\"",
        hash[..12]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );
    let etag = HeaderValue::from_str(&etag).ok()?;
    let cache = if path.starts_with("_app/immutable/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let cache = HeaderValue::from_static(cache);
    if if_none_match == Some(&etag) {
        return Some(
            (
                StatusCode::NOT_MODIFIED,
                [(header::ETAG, etag), (header::CACHE_CONTROL, cache)],
            )
                .into_response(),
        );
    }
    let mime = HeaderValue::from_str(file.metadata.mimetype()).ok()?;
    Some(
        (
            [
                (header::CONTENT_TYPE, mime),
                (header::CACHE_CONTROL, cache),
                (header::ETAG, etag),
            ],
            file.data,
        )
            .into_response(),
    )
}

async fn static_files(req: Request) -> Response {
    let path = req.uri().path().trim_start_matches('/');
    if path.starts_with("api/") {
        return StatusCode::NOT_FOUND.into_response();
    }
    let path = if path.is_empty() { "index.html" } else { path };
    let inm = req.headers().get(header::IF_NONE_MATCH);
    asset(path, inm)
        .or_else(|| {
            (!path.contains('.'))
                .then(|| asset("index.html", inm))
                .flatten()
        })
        .unwrap_or_else(|| StatusCode::NOT_FOUND.into_response())
}

/// The desktop app's own path. The app's window loads the UI through a
/// proxy in the app, and the UI asks the app for what only an app can do at
/// this path on the page's origin, which that proxy answers itself and
/// never forwards (`crates/coversync-proxy/src/app.rs`). The proxy keeps the
/// server's paths as they are, so for a server at the root of its domain
/// this path is one of the server's own: nothing may ever be served here.
/// The UI never asks for it in a browser.
const APP_PATH: &str = "/__coversync-app";

async fn app_path() -> StatusCode {
    StatusCode::NOT_FOUND
}

pub fn router() -> Router<Shared> {
    Router::new()
        .route("/", get(static_files))
        .route(APP_PATH, any(app_path))
        .route(&format!("{APP_PATH}/"), any(app_path))
        .route(&format!("{APP_PATH}/{{*rest}}"), any(app_path))
        .fallback(get(static_files))
        .layer(middleware::from_fn(csp))
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::extract::{ConnectInfo, Request};
    use axum::http::{Method, StatusCode};
    use tower::ServiceExt;

    use super::{APP_PATH, inline_script_hashes};

    /// The desktop app's proxy answers this path on the page's origin; the
    /// server, which that origin stands for, must never use it.
    #[tokio::test]
    async fn nothing_is_served_at_the_desktop_apps_path() {
        let dir = std::env::temp_dir().join(format!("coversyncd-app-path-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let app = crate::app(crate::test_state(&dir));
        let status = async |method: &Method, path: &str| {
            let mut req = Request::builder()
                .method(method.clone())
                .uri(path)
                .body(Body::empty())
                .unwrap();
            let peer: std::net::SocketAddr = "192.168.1.20:40000".parse().unwrap();
            req.extensions_mut().insert(ConnectInfo(peer));
            app.clone().oneshot(req).await.unwrap().status()
        };
        assert_eq!(APP_PATH, "/__coversync-app", "the app's proxy has it too");
        for path in [
            "/__coversync-app",
            "/__coversync-app/",
            "/__coversync-app/v1/capabilities",
            "/__coversync-app/v2/later",
        ] {
            for method in [Method::GET, Method::POST, Method::PUT, Method::DELETE] {
                assert_eq!(
                    status(&method, path).await,
                    StatusCode::NOT_FOUND,
                    "{method} {path}"
                );
            }
        }
        // Next to it, a path of the UI's still gets the UI's shell (or, in a
        // build without the UI, its plain 404): GET is allowed there.
        assert_ne!(
            status(&Method::POST, "/library").await,
            StatusCode::NOT_FOUND
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hashes_only_inline_scripts() {
        let html = r#"<script src="/a.js"></script><script>start()</script><script type="module">x</script>"#;
        let h = inline_script_hashes(html);
        assert_eq!(h.len(), 2);
        assert!(h[0].starts_with("'sha256-"));
    }
}
