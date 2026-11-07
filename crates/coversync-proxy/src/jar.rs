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
use std::sync::Mutex;

use axum::http::header::SET_COOKIE;
use axum::http::{HeaderMap, HeaderValue};
use cookie_store::{Cookie, CookieStore, RawCookie};
use url::Url;

pub(crate) struct Jar {
    /// One store for each server origin.
    stores: Mutex<HashMap<String, CookieStore>>,
    file: Option<PathBuf>,
    /// Held while saving, so saves land in the order they were taken.
    saving: Mutex<()>,
}

impl Jar {
    /// A jar with the cookies saved in `file`, if any. A missing or unreadable
    /// file starts it empty.
    pub(crate) fn open(file: Option<PathBuf>) -> Self {
        let stores = file.as_deref().map(load).unwrap_or_default();
        Self {
            stores: Mutex::new(stores),
            file,
            saving: Mutex::new(()),
        }
    }

    /// The `Cookie` header for a request to `url`, on the server at `origin`.
    pub(crate) fn header(&self, origin: &str, url: &Url) -> Option<HeaderValue> {
        let stores = self.stores.lock().expect("cookie lock");
        let pairs: Vec<String> = stores
            .get(origin)?
            .get_request_values(url)
            .map(|(name, value)| format!("{name}={value}"))
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
        self.stores
            .lock()
            .expect("cookie lock")
            .entry(origin.to_owned())
            .or_default()
            .store_response_cookies(cookies.into_iter(), url);
        self.save();
    }

    /// Writes the persistent, unexpired cookies to the file, whole.
    fn save(&self) {
        let Some(file) = &self.file else {
            return;
        };
        let _saving = self.saving.lock().expect("cookie save lock");
        let json = {
            let stores = self.stores.lock().expect("cookie lock");
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
