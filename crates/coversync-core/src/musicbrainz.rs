//! Clients for the AcoustID and MusicBrainz web services, each kept to the
//! request rate the service asks for (AcoustID 3 a second, MusicBrainz 1),
//! and for MusicBrainz's cover images on the Cover Art Archive. Answers from
//! AcoustID and MusicBrainz are saved in a [`LookupCache`] and used instead of
//! asking again (see [`Client`]).

use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::lookups::{self, LookupCache, Service};
use crate::{Error, Result};

const USER_AGENT: &str = concat!("coversync/", env!("CARGO_PKG_VERSION"));
const ACOUSTID_URL: &str = "https://api.acoustid.org/v2/lookup";
const MUSICBRAINZ_URL: &str = "https://musicbrainz.org/ws/2";
/// Release artist credited on compilations of many artists.
pub const VARIOUS_ARTISTS: &str = "89ad4ac3-39f7-470e-963a-56509c546377";

static AGENT: LazyLock<ureq::Agent> = LazyLock::new(|| {
    ureq::Agent::config_builder()
        .user_agent(USER_AGENT)
        .timeout_global(Some(Duration::from_secs(30)))
        // Both services explain errors in a JSON body.
        .http_status_as_error(false)
        .build()
        .into()
});

/// Spaces requests to one service at least `gap` apart, across threads.
struct Throttle {
    gap: Duration,
    next: Mutex<Option<Instant>>,
}

impl Throttle {
    const fn new(gap: Duration) -> Self {
        Self {
            gap,
            next: Mutex::new(None),
        }
    }

    /// Wait for the next request slot.
    fn wait(&self) {
        let mut next = self.next.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(at) = *next {
            let now = Instant::now();
            if at > now {
                std::thread::sleep(at - now);
            }
        }
        *next = Some(Instant::now() + self.gap);
    }
}

static ACOUSTID: Throttle = Throttle::new(Duration::from_millis(340));
static MUSICBRAINZ: Throttle = Throttle::new(Duration::from_millis(1100));

fn unreachable(service: &'static str) -> impl Fn(ureq::Error) -> Error {
    move |e| Error::Remote(format!("Couldn't reach {service}: {e}"))
}

/// Looks things up on AcoustID and MusicBrainz, answering from `cache` what
/// was asked before.
#[derive(Clone, Copy)]
pub struct Client<'a> {
    pub cache: &'a LookupCache,
    /// Ask the services even when an answer is saved, and save the new one.
    pub fresh: bool,
}

/// What a lookup found.
#[derive(Debug, Clone)]
pub struct Answer<T> {
    pub value: T,
    /// When the oldest saved answer it used was saved; None when the
    /// services were asked just now.
    pub saved: Option<i64>,
}

impl<T> Answer<T> {
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Answer<U> {
        Answer {
            value: f(self.value),
            saved: self.saved,
        }
    }
}

// ---- AcoustID ----

#[derive(Debug, Deserialize)]
struct LookupResponse {
    status: String,
    #[serde(default)]
    fingerprints: Vec<BatchEntry>,
    error: Option<ServiceError>,
}

#[derive(Debug, Deserialize)]
struct BatchEntry {
    index: usize,
    /// As sent, to be saved.
    #[serde(default)]
    results: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct ServiceError {
    #[serde(default)]
    code: i32,
    #[serde(default)]
    message: String,
}

/// One AcoustID track a fingerprint matched, with how well it matched.
#[derive(Debug, Clone, Deserialize)]
pub struct AcoustResult {
    pub id: String,
    pub score: f64,
    #[serde(default)]
    pub recordings: Vec<AcoustRecording>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AcoustRecording {
    pub id: String,
    #[serde(default)]
    pub releases: Vec<AcoustRelease>,
}

/// A release a recording appears on, as AcoustID summarizes it.
#[derive(Debug, Clone, Deserialize)]
pub struct AcoustRelease {
    pub id: String,
    pub title: Option<String>,
    pub country: Option<String>,
    pub date: Option<PartialDate>,
    pub track_count: Option<u32>,
    pub medium_count: Option<u32>,
    #[serde(default)]
    pub mediums: Vec<AcoustMedium>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PartialDate {
    pub year: Option<i32>,
    pub month: Option<u32>,
    pub day: Option<u32>,
}

impl PartialDate {
    /// "2015", "2015-03" or "2015-03-29".
    pub fn text(&self) -> Option<String> {
        let year = self.year?;
        Some(match (self.month, self.day) {
            (Some(m), Some(d)) => format!("{year:04}-{m:02}-{d:02}"),
            (Some(m), None) => format!("{year:04}-{m:02}"),
            _ => format!("{year:04}"),
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct AcoustMedium {
    pub position: Option<u32>,
    pub format: Option<String>,
    #[serde(default)]
    pub tracks: Vec<AcoustTrack>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AcoustTrack {
    pub position: Option<u32>,
}

/// Fingerprints sent to AcoustID in one request.
const BATCH: usize = 20;
/// What AcoustID includes with each match.
const META: &str = "recordings releases tracks compress";

/// What a fingerprint's matches are saved under. AcoustID's answer depends
/// on nothing else sent but the API key, which doesn't change it.
fn print_request(duration: u32, fingerprint: &str) -> String {
    let digest = Sha256::digest(format!("{META}\n{duration}\n{fingerprint}"));
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn unreadable(service: &str, e: impl std::fmt::Display) -> Error {
    Error::Remote(format!("{service} sent an unreadable answer: {e}"))
}

impl Client<'_> {
    /// Look up Chromaprint fingerprints, each with its audio's length in
    /// seconds; the matches come back in the same order. `key` is the
    /// AcoustID API key, needed only for fingerprints not looked up before.
    pub fn lookup(
        &self,
        key: &str,
        prints: &[(u32, &str)],
    ) -> Result<Answer<Vec<Vec<AcoustResult>>>> {
        let requests: Vec<String> = prints.iter().map(|(d, f)| print_request(*d, f)).collect();
        let mut found: Vec<Option<Vec<AcoustResult>>> = vec![None; prints.len()];
        let mut saved = None;
        if !self.fresh {
            for (slot, request) in found.iter_mut().zip(&requests) {
                let Some(s) = self.cache.get(Service::AcoustId, request) else {
                    continue;
                };
                match serde_json::from_str(&s.body) {
                    Ok(results) => {
                        *slot = Some(results);
                        saved = lookups::oldest(saved, Some(s.at));
                    }
                    Err(e) => {
                        tracing::warn!("a saved AcoustID answer is unreadable, asking again: {e}")
                    }
                }
            }
        }
        let missing: Vec<usize> = (0..prints.len()).filter(|&i| found[i].is_none()).collect();
        for chunk in missing.chunks(BATCH) {
            let batch: Vec<(u32, &str)> = chunk.iter().map(|&i| prints[i]).collect();
            for (&i, raw) in chunk.iter().zip(lookup_batch(key, &batch)?) {
                let results =
                    serde_json::from_value(raw.clone()).map_err(|e| unreadable("AcoustID", e))?;
                self.cache
                    .put(Service::AcoustId, &requests[i], &raw.to_string());
                found[i] = Some(results);
            }
        }
        Ok(Answer {
            value: found.into_iter().map(Option::unwrap_or_default).collect(),
            saved,
        })
    }

    /// The answer to a MusicBrainz request: saved, or asked for and saved.
    fn get<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        query: &[(&str, &str)],
    ) -> Result<Answer<T>> {
        let request = mb_request(url, query);
        if !self.fresh
            && let Some(s) = self.cache.get(Service::MusicBrainz, &request)
        {
            match serde_json::from_str(&s.body) {
                Ok(value) => {
                    return Ok(Answer {
                        value,
                        saved: Some(s.at),
                    });
                }
                Err(e) => tracing::warn!(
                    request,
                    "a saved MusicBrainz answer is unreadable, asking again: {e}"
                ),
            }
        }
        let body = get(url, query)?;
        let value = serde_json::from_str(&body).map_err(|e| unreadable("MusicBrainz", e))?;
        self.cache.put(Service::MusicBrainz, &request, &body);
        Ok(Answer { value, saved: None })
    }

    /// A release with its tracks, credits, labels and genres.
    pub fn release(&self, id: &str) -> Result<Answer<Release>> {
        if !is_mbid(id) {
            return Err(Error::Invalid(format!("'{id}' isn't a MusicBrainz id")));
        }
        self.get(&format!("{MUSICBRAINZ_URL}/release/{id}"), RELEASE_QUERY)
    }

    /// Releases titled `album` by `artist` (if known), best matches first.
    pub fn search_releases(
        &self,
        album: &str,
        artist: Option<&str>,
    ) -> Result<Answer<Vec<Release>>> {
        let mut query = format!("release:{}", phrase(album));
        if let Some(artist) = artist {
            query += &format!(" AND artist:{}", phrase(artist));
        }
        let res: Answer<SearchResponse> = self.get(
            &format!("{MUSICBRAINZ_URL}/release"),
            &[("query", &query), ("limit", "15"), ("fmt", "json")],
        )?;
        Ok(res.map(|r| r.releases))
    }
}

/// What a release lookup asks MusicBrainz to include.
const RELEASE_QUERY: &[(&str, &str)] = &[
    (
        "inc",
        "artist-credits recordings release-groups labels isrcs genres aliases release-rels",
    ),
    ("fmt", "json"),
];

/// What MusicBrainz's answer to a request is saved under: the URL and its
/// query, which says what the answer includes.
fn mb_request(url: &str, query: &[(&str, &str)]) -> String {
    format!(
        "{url} {}",
        serde_json::to_string(query).expect("a query serializes")
    )
}

/// Each fingerprint's matches, as AcoustID sent them.
fn lookup_batch(key: &str, prints: &[(u32, &str)]) -> Result<Vec<serde_json::Value>> {
    let mut form: Vec<(String, String)> = [
        ("client", key),
        ("meta", META),
        ("format", "json"),
        ("batch", "1"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    for (i, (duration, fingerprint)) in prints.iter().enumerate() {
        form.push((format!("duration.{i}"), duration.to_string()));
        form.push((format!("fingerprint.{i}"), fingerprint.to_string()));
    }
    for attempt in 0.. {
        ACOUSTID.wait();
        let mut res = AGENT
            .post(ACOUSTID_URL)
            .send_form(form.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .map_err(unreachable("AcoustID"))?;
        let status = res.status().as_u16();
        if (status == 429 || status == 503) && attempt < 2 {
            std::thread::sleep(Duration::from_secs(1 + attempt));
            continue;
        }
        let body: LookupResponse = res
            .body_mut()
            .with_config()
            .limit(64 << 20)
            .read_json()
            .map_err(|e| {
                Error::Remote(format!(
                    "AcoustID sent an unreadable answer (HTTP {status}): {e}"
                ))
            })?;
        if body.status == "ok" {
            let mut out = vec![serde_json::Value::Array(Vec::new()); prints.len()];
            for entry in body.fingerprints {
                if let (Some(slot), Some(results)) = (out.get_mut(entry.index), entry.results) {
                    *slot = results;
                }
            }
            return Ok(out);
        }
        let err = body.error.unwrap_or(ServiceError {
            code: 0,
            message: format!("HTTP {status}"),
        });
        return Err(match err.code {
            // 4: invalid API key; 5: invalid/disabled application.
            4 | 5 => Error::Invalid(
                "AcoustID didn't accept the API key. Check it in Inbox setup.".into(),
            ),
            _ => Error::Remote(format!("AcoustID: {}", err.message)),
        });
    }
    unreachable!()
}

// ---- MusicBrainz ----

#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    pub id: String,
    pub title: String,
    pub date: Option<String>,
    pub country: Option<String>,
    pub barcode: Option<String>,
    pub disambiguation: Option<String>,
    /// "Official", "Promotion", "Pseudo-Release"…
    pub status: Option<String>,
    #[serde(rename = "artist-credit", default)]
    pub artist_credit: Vec<Credit>,
    #[serde(rename = "release-group")]
    pub release_group: Option<ReleaseGroup>,
    #[serde(rename = "label-info", default)]
    pub label_info: Vec<LabelInfo>,
    #[serde(default)]
    pub media: Vec<Medium>,
    #[serde(default)]
    pub genres: Vec<Genre>,
    #[serde(rename = "text-representation")]
    pub text: Option<TextRepresentation>,
    #[serde(default)]
    pub aliases: Vec<Alias>,
    /// Links to other releases, e.g. a translated tracklist.
    #[serde(default)]
    pub relations: Vec<Relation>,
    /// What the Cover Art Archive holds for the release.
    #[serde(rename = "cover-art-archive")]
    pub cover_art: Option<CoverArtArchive>,
    /// Search results only: how well the release matched, out of 100.
    pub score: Option<u32>,
    /// Search results only: tracks across every medium.
    #[serde(rename = "track-count")]
    pub track_count: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Credit {
    pub name: String,
    #[serde(default)]
    pub joinphrase: String,
    pub artist: CreditArtist,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreditArtist {
    pub id: String,
    #[serde(rename = "sort-name")]
    pub sort_name: Option<String>,
    /// "Person", "Group", "Character"…
    #[serde(rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub aliases: Vec<Alias>,
}

/// Another name for an artist, label or release, e.g. a romanization.
#[derive(Debug, Clone, Deserialize)]
pub struct Alias {
    pub name: String,
    pub locale: Option<String>,
    /// The name to use for its locale.
    #[serde(default)]
    pub primary: Option<bool>,
    /// "Artist name", "Legal name", "Search hint"…
    #[serde(rename = "type")]
    pub kind: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Relation {
    /// e.g. "transl-tracklisting": the target lists this release's tracks
    /// translated or transliterated.
    #[serde(rename = "type")]
    pub kind: String,
    pub direction: String,
    pub release: Option<RelatedRelease>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RelatedRelease {
    pub id: String,
    pub title: String,
    #[serde(rename = "text-representation")]
    pub text: Option<TextRepresentation>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReleaseGroup {
    pub id: String,
    #[serde(rename = "primary-type")]
    pub primary_type: Option<String>,
    #[serde(rename = "secondary-types", default)]
    pub secondary_types: Vec<String>,
    #[serde(rename = "first-release-date")]
    pub first_release_date: Option<String>,
    #[serde(default)]
    pub genres: Vec<Genre>,
    #[serde(default)]
    pub aliases: Vec<Alias>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CoverArtArchive {
    /// There is a front cover image.
    #[serde(default)]
    pub front: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LabelInfo {
    #[serde(rename = "catalog-number")]
    pub catalog_number: Option<String>,
    pub label: Option<Label>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Label {
    pub name: String,
    #[serde(rename = "sort-name")]
    pub sort_name: Option<String>,
    #[serde(default)]
    pub aliases: Vec<Alias>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Medium {
    pub position: Option<u32>,
    pub format: Option<String>,
    pub title: Option<String>,
    #[serde(rename = "track-count")]
    pub track_count: Option<u32>,
    #[serde(default)]
    pub tracks: Vec<Track>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Track {
    pub id: String,
    pub position: Option<u32>,
    pub title: String,
    pub length: Option<u64>,
    #[serde(rename = "artist-credit", default)]
    pub artist_credit: Vec<Credit>,
    pub recording: Option<Recording>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Recording {
    pub id: String,
    #[serde(default)]
    pub isrcs: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Genre {
    pub name: String,
    #[serde(default)]
    pub count: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TextRepresentation {
    pub script: Option<String>,
    pub language: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    #[serde(default)]
    releases: Vec<Release>,
}

/// The credit as printed, e.g. "Artist feat. Other".
pub fn credit_name(credit: &[Credit]) -> String {
    credit
        .iter()
        .map(|c| format!("{}{}", c.name, c.joinphrase))
        .collect()
}

/// The credit with each artist's sort name, e.g. "Beatles, The".
pub fn credit_sort(credit: &[Credit]) -> String {
    credit
        .iter()
        .map(|c| {
            let name = c.artist.sort_name.as_deref().unwrap_or(&c.name);
            format!("{name}{}", c.joinphrase)
        })
        .collect()
}

/// MusicBrainz ids are UUIDs; anything else never reaches a URL.
pub fn is_mbid(id: &str) -> bool {
    id.len() == 36
        && id.chars().enumerate().all(|(i, c)| {
            matches!(i, 8 | 13 | 18 | 23) == (c == '-') && (c == '-' || c.is_ascii_hexdigit())
        })
}

/// The body of MusicBrainz's answer to a request.
fn get(url: &str, query: &[(&str, &str)]) -> Result<String> {
    for attempt in 0.. {
        MUSICBRAINZ.wait();
        let mut req = AGENT.get(url).header("Accept", "application/json");
        for (k, v) in query {
            req = req.query(*k, *v);
        }
        let mut res = req.call().map_err(unreachable("MusicBrainz"))?;
        match res.status().as_u16() {
            200 => {
                return res
                    .body_mut()
                    .read_to_string()
                    .map_err(|e| unreadable("MusicBrainz", e));
            }
            404 => return Err(Error::NotFound),
            // Over the rate limit, or briefly unavailable.
            429 | 503 if attempt < 2 => std::thread::sleep(Duration::from_secs(2 + attempt)),
            status => {
                return Err(Error::Remote(format!(
                    "MusicBrainz answered with HTTP {status}"
                )));
            }
        }
    }
    unreachable!()
}

/// Quote a value for a MusicBrainz (Lucene) search query.
fn phrase(value: &str) -> String {
    let escaped: String = value
        .chars()
        .flat_map(|c| {
            let special = "+-&|!(){}[]^\"~*?:\\/".contains(c);
            special.then_some('\\').into_iter().chain([c])
        })
        .collect();
    format!("\"{escaped}\"")
}

// ---- Cover Art Archive ----

const COVER_ART_URL: &str = "https://coverartarchive.org";

/// Images come from the Internet Archive, which can take a while over a big one.
static ART_AGENT: LazyLock<ureq::Agent> = LazyLock::new(|| {
    ureq::Agent::config_builder()
        .user_agent(USER_AGENT)
        .timeout_global(Some(Duration::from_secs(120)))
        .http_status_as_error(false)
        .build()
        .into()
});

/// Where a front cover came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverFrom {
    Release,
    /// The release has none; the Cover Art Archive picked one of the group's.
    ReleaseGroup,
}

enum Fetched {
    Image(Vec<u8>),
    Missing,
    TooLarge,
}

fn fetch_image(url: &str, limit: usize) -> Result<Fetched> {
    let mut res = 'call: {
        for attempt in 0.. {
            let res = ART_AGENT
                .get(url)
                .call()
                .map_err(unreachable("the Cover Art Archive"))?;
            match res.status().as_u16() {
                200 => break 'call res,
                404 => return Ok(Fetched::Missing),
                // The Internet Archive behind it has bad moments.
                500..=599 if attempt < 2 => std::thread::sleep(Duration::from_secs(1 + attempt)),
                status => {
                    return Err(Error::Remote(format!(
                        "The Cover Art Archive answered with HTTP {status}"
                    )));
                }
            }
        }
        unreachable!()
    };
    let length = res
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok()?.parse::<u64>().ok());
    if length.is_some_and(|n| n > limit as u64) {
        return Ok(Fetched::TooLarge);
    }
    match res
        .body_mut()
        .with_config()
        .limit(limit as u64)
        .read_to_vec()
    {
        Ok(bytes) => Ok(Fetched::Image(bytes)),
        Err(ureq::Error::BodyExceedsLimit(_)) => Ok(Fetched::TooLarge),
        Err(e) => Err(unreachable("the Cover Art Archive")(e)),
    }
}

/// The release's front cover from the Cover Art Archive, or failing that its
/// release group's: the original, or the 1200 px version when the original
/// is over `limit` bytes. None when neither has one. `release` is None when
/// MusicBrainz already says it has no front cover.
pub fn front_cover(
    release: Option<&str>,
    group: Option<&str>,
    limit: usize,
) -> Result<Option<(Vec<u8>, CoverFrom)>> {
    let places = [
        ("release", release, CoverFrom::Release),
        ("release-group", group, CoverFrom::ReleaseGroup),
    ];
    for (path, id, from) in places {
        let Some(id) = id else { continue };
        if !is_mbid(id) {
            return Err(Error::Invalid(format!("'{id}' isn't a MusicBrainz id")));
        }
        for size in ["front", "front-1200"] {
            match fetch_image(&format!("{COVER_ART_URL}/{path}/{id}/{size}"), limit)? {
                Fetched::Image(bytes) => return Ok(Some((bytes, from))),
                Fetched::Missing => break,
                Fetched::TooLarge => continue,
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mbids_and_queries() {
        assert!(is_mbid("8bd5c2c7-af25-43d0-933c-1faf685204a6"));
        assert!(!is_mbid("8bd5c2c7-af25-43d0-933c-1faf685204a"));
        assert!(!is_mbid("../../etc/passwd-aaaaaaaaaaaaaaaaaaaaaaaaa"));
        assert!(!is_mbid("8bd5c2c7xaf25-43d0-933c-1faf685204a6"));
        assert_eq!(phrase("AC/DC: Live!"), r#""AC\/DC\: Live\!""#);
    }

    #[test]
    fn saved_answers_are_used_without_asking() {
        let cache = LookupCache::in_memory();
        let client = Client {
            cache: &cache,
            fresh: false,
        };
        // No such release: asking MusicBrainz would fail.
        let id = "00000000-0000-4000-8000-00000000c0de";
        cache.put(
            Service::MusicBrainz,
            &mb_request(&format!("{MUSICBRAINZ_URL}/release/{id}"), RELEASE_QUERY),
            &format!(r#"{{"id": "{id}", "title": "Saved", "new-field": 1}}"#),
        );
        let found = client.release(id).unwrap();
        assert_eq!(found.value.title, "Saved");
        assert!(found.saved.is_some());

        // Nor would asking AcoustID, without a key.
        cache.put(
            Service::AcoustId,
            &print_request(215, "AQAAprint"),
            r#"[{"id": "a", "score": 0.9, "recordings": [{"id": "r"}]}]"#,
        );
        let found = client.lookup("", &[(215, "AQAAprint")]).unwrap();
        assert_eq!(found.value.len(), 1);
        assert_eq!(found.value[0][0].recordings[0].id, "r");
        assert!(found.saved.is_some());
        // Another fingerprint or length is another request.
        assert_ne!(
            print_request(215, "AQAAprint"),
            print_request(216, "AQAAprint")
        );
        assert_ne!(
            print_request(215, "AQAAprint"),
            print_request(215, "AQAAprinu")
        );
    }

    #[test]
    fn credits_join_names() {
        let credit: Vec<Credit> = serde_json::from_str(
            r#"[{"name": "Beatles", "joinphrase": " & ", "artist": {"id": "a", "sort-name": "Beatles, The"}},
                {"name": "Friend", "artist": {"id": "b"}}]"#,
        )
        .unwrap();
        assert_eq!(credit_name(&credit), "Beatles & Friend");
        assert_eq!(credit_sort(&credit), "Beatles, The & Friend");
    }
}
