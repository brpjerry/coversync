//! The servers' cookies, kept by the proxy rather than the webview.
//!
//! Every server reaches the webview as 127.0.0.1, so cookies the webview kept
//! would go to whichever server came next. The proxy keeps them instead,
//! apart for each server's origin, and the webview only ever holds the
//! proxy's own cookie. Cookies are stored and sent the way a browser would
//! for the server's real URL (`Secure`, `Domain`, `Path`, expiry), and
//! persistent ones are saved to a file between runs.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::http::header::SET_COOKIE;
use axum::http::{HeaderMap, HeaderValue};
use cookie_store::{Cookie, CookieStore, RawCookie};
use url::Url;

/// The servers' cookies: a store for each server origin (scheme, host and
/// port). Clones share it, so proxies on several ports (one for each
/// server, say) can use one jar and one file. Only one process should use a
/// file at a time: each saves its whole jar.
#[derive(Clone, Default)]
pub struct Cookies(Arc<Jar>);

#[derive(Default)]
struct Jar {
    stores: Mutex<HashMap<String, CookieStore>>,
    file: Option<PathBuf>,
    /// Held while saving, so saves land in the order they were taken.
    saving: Mutex<()>,
}

impl Cookies {
    /// A jar with the persistent cookies saved in `file`, if any, which it
    /// saves them to as they change, readable only by the user. A missing or
    /// unreadable file starts it empty. Without a file, cookies last as long
    /// as the jar.
    pub fn open(file: Option<PathBuf>) -> Self {
        let stores = file.as_deref().map(load).unwrap_or_default();
        Self(Arc::new(Jar {
            stores: Mutex::new(stores),
            file,
            saving: Mutex::new(()),
        }))
    }

    /// The `Cookie` header for a request to `url`, on the server at `origin`:
    /// longer paths first, as browsers send them (RFC 6265, 5.4).
    pub(crate) fn header(&self, origin: &str, url: &Url) -> Option<HeaderValue> {
        let stores = self.0.stores.lock().expect("cookie lock");
        let mut cookies = stores.get(origin)?.matches(url);
        cookies.sort_by_key(|cookie| std::cmp::Reverse(cookie.path.len()));
        let pairs: Vec<String> = cookies
            .iter()
            .map(|cookie| format!("{}={}", cookie.name(), cookie.value()))
            .collect();
        if pairs.is_empty() {
            return None;
        }
        HeaderValue::from_str(&pairs.join("; ")).ok()
    }

    /// Keeps the cookies a response from `url` sets, and takes its
    /// `Set-Cookie` headers out so the webview never sees them.
    pub(crate) fn take(&self, origin: &str, url: &Url, headers: &mut HeaderMap) {
        if !headers.contains_key(SET_COOKIE) {
            return;
        }
        let cookies: Vec<RawCookie<'static>> = headers
            .get_all(SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .filter_map(|value| RawCookie::parse(value.to_owned()).ok())
            .collect();
        headers.remove(SET_COOKIE);
        self.0
            .stores
            .lock()
            .expect("cookie lock")
            .entry(origin.to_owned())
            .or_default()
            .store_response_cookies(cookies.into_iter(), url);
        self.save();
    }

    /// The server origins it holds cookies for.
    pub fn origins(&self) -> Vec<String> {
        let mut origins: Vec<String> = self
            .0
            .stores
            .lock()
            .expect("cookie lock")
            .iter()
            .filter(|(_, store)| store.iter_unexpired().next().is_some())
            .map(|(origin, _)| origin.clone())
            .collect();
        origins.sort();
        origins
    }

    /// Drops every cookie of the server at `origin` (`https://n5pro.internal`),
    /// and saves.
    pub fn forget(&self, origin: &str) {
        let removed = self
            .0
            .stores
            .lock()
            .expect("cookie lock")
            .remove(origin)
            .is_some();
        if removed {
            self.save();
        }
    }

    /// Writes the persistent, unexpired cookies to the file, whole.
    fn save(&self) {
        let Some(file) = &self.0.file else {
            return;
        };
        let _saving = self.0.saving.lock().expect("cookie save lock");
        let json = {
            let stores = self.0.stores.lock().expect("cookie lock");
            let saved: BTreeMap<&str, Vec<&Cookie<'static>>> = stores
                .iter()
                .map(|(origin, store)| {
                    let cookies = store.iter_unexpired().filter(|c| c.is_persistent());
                    (origin.as_str(), cookies.collect::<Vec<_>>())
                })
                .filter(|(_, cookies)| !cookies.is_empty())
                .collect();
            serde_json::to_vec_pretty(&saved)
        };
        let written = json
            .map_err(io::Error::other)
            .and_then(|json| write_private(file, &json));
        if let Err(err) = written {
            tracing::warn!("couldn't save cookies to {}: {err}", file.display());
        }
    }
}

fn load(file: &Path) -> HashMap<String, CookieStore> {
    let bytes = match fs::read(file) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return HashMap::new(),
        Err(err) => {
            tracing::warn!("couldn't read {}: {err}", file.display());
            return HashMap::new();
        }
    };
    match serde_json::from_slice::<BTreeMap<String, Vec<Cookie<'static>>>>(&bytes) {
        Ok(saved) => saved
            .into_iter()
            .filter_map(|(origin, cookies)| {
                // Expired ones are left out.
                let store =
                    CookieStore::from_cookies(cookies.into_iter().map(Ok::<_, ()>), false).ok()?;
                Some((origin, store))
            })
            .collect(),
        Err(err) => {
            tracing::warn!("ignoring {}: {err}", file.display());
            HashMap::new()
        }
    }
}

/// Replaces `file` whole with `bytes`, readable only by its owner.
fn write_private(file: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = file.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut partial = file.as_os_str().to_owned();
    partial.push(format!(".{}.partial", std::process::id()));
    let partial = PathBuf::from(partial);
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut out = options.open(&partial)?;
    out.write_all(bytes)?;
    out.sync_all()?;
    fs::rename(&partial, file)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORIGIN: &str = "https://n5pro.internal";

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    /// Stores what a response from `from` sets.
    fn set(jar: &Cookies, from: &str, cookies: &[&'static str]) {
        let mut headers = HeaderMap::new();
        for cookie in cookies {
            headers.append(SET_COOKIE, HeaderValue::from_static(cookie));
        }
        jar.take(ORIGIN, &url(from), &mut headers);
        assert!(headers.is_empty(), "Set-Cookie is taken out");
    }

    fn sent(jar: &Cookies, to: &str) -> Option<String> {
        jar.header(ORIGIN, &url(to))
            .map(|h| h.to_str().unwrap().to_owned())
    }

    #[test]
    fn secure_cookies_go_only_over_https() {
        let jar = Cookies::default();
        set(
            &jar,
            "https://n5pro.internal/coversync/login",
            &[
                "session=s; Path=/; Secure; HttpOnly",
                "__Secure-a=1; Path=/; Secure",
                "__Host-b=2; Path=/; Secure",
            ],
        );
        let over_https = sent(&jar, "https://n5pro.internal/coversync/").unwrap();
        for cookie in ["session=s", "__Secure-a=1", "__Host-b=2"] {
            assert!(over_https.contains(cookie), "{over_https}");
        }
        assert_eq!(sent(&jar, "http://n5pro.internal/coversync/"), None);
    }

    #[test]
    fn a_domain_must_be_the_servers() {
        let jar = Cookies::default();
        set(
            &jar,
            "https://n5pro.internal/",
            &[
                "elsewhere=1; Domain=example.com",
                "sibling=1; Domain=other.internal",
                "own=1; Domain=n5pro.internal",
            ],
        );
        assert_eq!(
            sent(&jar, "https://n5pro.internal/").as_deref(),
            Some("own=1")
        );
        assert_eq!(
            jar.header("https://other.internal", &url("https://other.internal/")),
            None
        );
    }

    #[test]
    fn forgets_a_server() {
        let dir = std::env::temp_dir().join(format!("coversync-jar-forget-{}", std::process::id()));
        let file = dir.join("cookies.json");
        let jar = Cookies::open(Some(file.clone()));
        set(
            &jar,
            "https://n5pro.internal/",
            &["session=s; Max-Age=3600"],
        );
        assert_eq!(jar.origins(), [ORIGIN]);
        jar.forget(ORIGIN);
        assert!(jar.origins().is_empty());
        assert!(Cookies::open(Some(file)).origins().is_empty(), "saved");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn longer_paths_go_first() {
        let jar = Cookies::default();
        set(
            &jar,
            "https://n5pro.internal/coversync/api/x",
            &[
                "session=root; Path=/",
                "session=api; Path=/coversync/api",
                "session=ui; Path=/coversync/",
            ],
        );
        assert_eq!(
            sent(&jar, "https://n5pro.internal/coversync/api/status").as_deref(),
            Some("session=api; session=ui; session=root")
        );
    }
}
