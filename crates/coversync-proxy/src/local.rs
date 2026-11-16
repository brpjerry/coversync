//! Tracks this computer already holds: played from its own disk.
//!
//! A computer that syncs keeps a copy of the Library. When the UI asks for
//! a track's audio (`GET /api/tracks/{id}/stream`) and the app says the
//! copy has that very file ([`LocalFiles`]), the proxy answers from the
//! copy and the server isn't asked. Anything the app doesn't vouch for goes
//! to the server as always, so being wrong about what's held costs a
//! download, never a wrong or missing track.
//!
//! What the app needs to know for that, it asks the server through
//! [`Asker`]: with this computer's token, never the window's cookies.

use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::Request;
use axum::http::header::{
    ACCEPT_ENCODING, ACCEPT_RANGES, AUTHORIZATION, CACHE_CONTROL, CONTENT_LENGTH, CONTENT_RANGE,
    CONTENT_TYPE, HOST, IF_RANGE, RANGE,
};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use http_body_util::{BodyExt, Limited};
use tokio::io::AsyncReadExt;
use tokio_util::io::ReaderStream;

use crate::HttpsClient;
use crate::upstream::Upstream;

/// On an answer the proxy gave from this computer's copy.
pub const HEADER: HeaderName = HeaderName::from_static("coversync-local");

/// More than one of the server's answers to the app ever comes to.
const ANSWER_LIMIT: usize = 16 * 1024 * 1024;

/// A file to answer with, open.
pub struct LocalFile {
    pub file: File,
    /// Its length, as the answer will say.
    pub len: u64,
    pub content_type: &'static str,
}

/// What this computer holds of a server's Library.
pub trait LocalFiles: Send + Sync {
    /// The file the server would send for the track `id`
    /// (`/api/tracks/{id}/stream`), if this computer holds it. Called on a
    /// thread that may block briefly; whatever takes longer (asking the
    /// server what's held) is started here and answered next time.
    fn track(&self, id: i64, ask: &Asker) -> Option<LocalFile>;
}

/// Asks the server something for the app itself, as this computer's
/// device. Without a token it has nothing to ask with.
#[derive(Clone)]
pub struct Asker {
    pub(crate) client: HttpsClient,
    pub(crate) upstream: Arc<Upstream>,
    pub(crate) timeout: Duration,
}

impl Asker {
    /// The server asked.
    pub fn server(&self) -> &Upstream {
        &self.upstream
    }

    /// A number that stands for the token this asks with, and changes when
    /// the token does; none without a token. For telling what one device
    /// was told from what the next one is owed: it says nothing of the
    /// token itself.
    pub fn device_mark(&self) -> Option<u64> {
        use std::hash::{Hash, Hasher};
        let mut mark = std::collections::hash_map::DefaultHasher::new();
        self.upstream.authorization()?.as_bytes().hash(&mut mark);
        Some(mark.finish())
    }

    /// `GET`s `path` (below the server's prefix: `/api/sync/v1/inventory`)
    /// and returns the answer's body, if the server said yes.
    pub async fn get(&self, path: &str) -> Result<Vec<u8>, String> {
        let upstream = &self.upstream;
        let authorization = upstream
            .authorization()
            .ok_or("this computer isn't paired with the server")?;
        let uri = upstream
            .uri(&format!("{}{path}", upstream.prefix()))
            .ok_or("the address isn't valid")?;
        let req = Request::get(uri)
            .header(HOST, upstream.host())
            .header(AUTHORIZATION, authorization)
            .header(ACCEPT_ENCODING, "identity")
            .body(Body::empty())
            .map_err(|e| e.to_string())?;
        let asked = async {
            let res = self
                .client
                .request(req)
                .await
                .map_err(|e| format!("the server couldn't be reached: {e}"))?;
            if !res.status().is_success() {
                return Err(format!("the server answered {}", res.status()));
            }
            let body = Limited::new(res.into_body(), ANSWER_LIMIT)
                .collect()
                .await
                .map_err(|e| format!("the answer couldn't be read: {e}"))?;
            Ok(body.to_bytes().to_vec())
        };
        tokio::time::timeout(self.timeout, asked)
            .await
            .map_err(|_| "the server took too long".to_owned())?
    }
}

/// The track a request is for the audio of, if it is: `path` is below the
/// server's prefix.
pub(crate) fn track_asked(method: &Method, path: &str) -> Option<i64> {
    if !matches!(*method, Method::GET | Method::HEAD) {
        return None;
    }
    let id = path.strip_prefix("/api/tracks/")?.strip_suffix("/stream")?;
    // As the server writes ids: digits, with a sign for a Library's.
    let digits = id.strip_prefix('-').unwrap_or(id);
    (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| id.parse().ok())
        .flatten()
}

/// The bytes a `Range` header asks for, of a file `len` long: first and
/// last, inclusive.
#[derive(Debug, PartialEq, Eq)]
enum Asked {
    /// All of it: no range, or one this doesn't serve (several ranges).
    Whole,
    Part(u64, u64),
    /// Nothing of the file is in the range.
    Unsatisfiable,
}

fn range(header: Option<&HeaderValue>, len: u64) -> Asked {
    let Some(spec) = header
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().strip_prefix("bytes="))
    else {
        return Asked::Whole;
    };
    let Some((first, last)) = spec.split_once('-') else {
        return Asked::Whole;
    };
    let (first, last) = (first.trim(), last.trim());
    if spec.contains(',') {
        return Asked::Whole;
    }
    match (first.parse::<u64>(), last.parse::<u64>()) {
        // The last so many bytes.
        (Err(_), Ok(suffix)) if first.is_empty() => {
            if suffix == 0 || len == 0 {
                Asked::Unsatisfiable
            } else {
                Asked::Part(len.saturating_sub(suffix), len - 1)
            }
        }
        (Ok(first), _) if first >= len => Asked::Unsatisfiable,
        (Ok(first), Err(_)) if last.is_empty() => Asked::Part(first, len - 1),
        (Ok(first), Ok(last)) if first <= last => Asked::Part(first, last.min(len - 1)),
        _ => Asked::Whole,
    }
}

/// Answers a request for a track's audio with `local`, as the server
/// answers one: the whole file, or the range asked for.
pub(crate) fn serve(mut local: LocalFile, method: &Method, headers: &HeaderMap) -> Response {
    let len = local.len;
    // A range that depends on the file being a certain version: there are
    // no versions to tell here, so the whole file answers it.
    let asked = if headers.contains_key(IF_RANGE) {
        Asked::Whole
    } else {
        range(headers.get(RANGE), len)
    };
    let (status, first, last) = match asked {
        Asked::Whole => (StatusCode::OK, 0, len.saturating_sub(1)),
        Asked::Part(first, last) => (StatusCode::PARTIAL_CONTENT, first, last),
        Asked::Unsatisfiable => {
            let mut res = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
            let value = format!("bytes */{len}");
            res.headers_mut().insert(
                CONTENT_RANGE,
                HeaderValue::from_str(&value).expect("digits and punctuation"),
            );
            res.headers_mut()
                .insert(HEADER, HeaderValue::from_static("1"));
            return res;
        }
    };
    let count = if len == 0 { 0 } else { last - first + 1 };
    let body = if *method == Method::HEAD || count == 0 {
        Body::empty()
    } else {
        if local.file.seek(SeekFrom::Start(first)).is_err() {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        let file = tokio::fs::File::from_std(local.file).take(count);
        Body::from_stream(ReaderStream::with_capacity(file, 256 * 1024))
    };
    let mut res = Response::new(body);
    *res.status_mut() = status;
    let headers = res.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(local.content_type));
    headers.insert(CONTENT_LENGTH, HeaderValue::from(count));
    headers.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    // As the server's own answer for a track.
    headers.insert(
        CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=3600"),
    );
    headers.insert(HEADER, HeaderValue::from_static("1"));
    if status == StatusCode::PARTIAL_CONTENT {
        let value = format!("bytes {first}-{last}/{len}");
        headers.insert(
            CONTENT_RANGE,
            HeaderValue::from_str(&value).expect("digits and punctuation"),
        );
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    // Bugs caught: 0, False positives: 0
    #[test]
    fn only_a_tracks_audio_is_answered_from_the_copy() {
        let get = |path| track_asked(&Method::GET, path);
        assert_eq!(get("/api/tracks/12/stream"), Some(12));
        // A Library's ids are negative.
        assert_eq!(get("/api/tracks/-4000000017/stream"), Some(-4_000_000_017));
        assert_eq!(
            track_asked(&Method::HEAD, "/api/tracks/12/stream"),
            Some(12)
        );
        for other in [
            "/api/tracks/12",
            "/api/tracks/12/stream/",
            "/api/tracks//stream",
            "/api/tracks/+12/stream",
            "/api/tracks/1.5/stream",
            "/api/tracks/../12/stream",
            "/api/tracks/99999999999999999999/stream",
            "/api/albums/12/cover",
            "/coversync/api/tracks/12/stream",
        ] {
            assert_eq!(get(other), None, "{other}");
        }
        assert_eq!(track_asked(&Method::POST, "/api/tracks/12/stream"), None);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn a_range_is_read_as_the_server_reads_it() {
        let asked = |spec: &'static str, len| range(Some(&HeaderValue::from_static(spec)), len);
        assert_eq!(range(None, 100), Asked::Whole);
        assert_eq!(asked("bytes=0-", 100), Asked::Part(0, 99));
        assert_eq!(asked("bytes=10-19", 100), Asked::Part(10, 19));
        // Past the end is cut to the end.
        assert_eq!(asked("bytes=90-500", 100), Asked::Part(90, 99));
        assert_eq!(asked("bytes=-10", 100), Asked::Part(90, 99));
        assert_eq!(asked("bytes=-500", 100), Asked::Part(0, 99));
        assert_eq!(asked("bytes=100-", 100), Asked::Unsatisfiable);
        assert_eq!(asked("bytes=-0", 100), Asked::Unsatisfiable);
        assert_eq!(asked("bytes=0-", 0), Asked::Unsatisfiable);
        // What isn't one range of bytes gets the whole file.
        assert_eq!(asked("bytes=0-1,5-6", 100), Asked::Whole);
        assert_eq!(asked("bytes=5-1", 100), Asked::Whole);
        assert_eq!(asked("bytes=a-b", 100), Asked::Whole);
        assert_eq!(asked("lines=1-2", 100), Asked::Whole);
    }
}
