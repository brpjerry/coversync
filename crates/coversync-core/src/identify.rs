//! Smart suggestions: identify an Inbox album on MusicBrainz, by its audio
//! (Chromaprint fingerprints looked up on AcoustID) where possible and by its
//! tags otherwise, then propose MusicBrainz's values for its tags: changes
//! where a tag differs, and values for tags that are empty.
//!
//! `identify` does the slow part once (reading, fingerprinting, lookups) and
//! keeps it as `Evidence`, so `suggest` can switch to another release with a
//! single MusicBrainz request. Lookups are answered from the
//! [`LookupCache`] where they were made before, so an album identified once
//! is identified again without asking either service.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use lofty::tag::TagType;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::lookups::{LookupCache, oldest};
use crate::musicbrainz::{self as mb, AcoustResult, Medium, Release};
use crate::romanize::{self, sorts_az};
use crate::{Error, Library, Result, import, tags};

/// Kinds suggested only for empty tags: genre is a matter of taste, and a
/// differing language or AcoustID id isn't a correction.
const FILL_ONLY: &[&str] = &["genre", "language", "script", "compilation", "acoustid"];
/// AcoustID matches scoring below this are ignored.
const MIN_SCORE: f64 = 0.5;
/// MusicBrainz search results scoring below this (out of 100) are ignored.
const MIN_SEARCH_SCORE: u32 = 70;
const MAX_CANDIDATES: usize = 8;
/// ISRCs sent in one search, enough to place any album.
const MAX_ISRCS: usize = 25;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Reading,
    Fingerprinting,
    Acoustid,
    Searching,
    Release,
    /// Fetching a romanized or translated tracklist.
    Romanizing,
}

/// Progress through an identification.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Step {
    pub stage: Stage,
    pub done: u32,
    pub total: u32,
}

/// How the release was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    /// Fingerprints matched recordings on it.
    Acoustid,
    /// It has the barcode (or UPC) the album's files are tagged with.
    Barcode,
    /// It has the recordings the album's ISRCs name.
    Isrc,
    /// A MusicBrainz search by the album's title and artist.
    Tags,
    /// Picked from the candidates by hand.
    Chosen,
}

impl Method {
    /// What the album was matched by, for notes: "matched by its …".
    fn by(self) -> &'static str {
        match self {
            Method::Barcode => "barcode",
            Method::Isrc => "ISRCs",
            _ => "title and artist",
        }
    }
}

/// How a track was paired with the release's tracks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Via {
    Acoustid,
    Position,
    Title,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReleaseSummary {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub date: Option<String>,
    pub country: Option<String>,
    /// e.g. "CD", "2×CD" or "CD + DVD".
    pub format: Option<String>,
    pub track_count: u32,
    pub disambiguation: Option<String>,
    /// The Cover Art Archive has a front cover for the release.
    pub front_art: bool,
    /// Whose front cover stands in when the release has none.
    pub release_group: Option<String>,
}

/// A release the album might be.
#[derive(Debug, Clone, Serialize)]
pub struct Candidate {
    pub id: String,
    pub title: String,
    pub artist: Option<String>,
    pub date: Option<String>,
    pub country: Option<String>,
    pub format: Option<String>,
    pub track_count: Option<u32>,
    /// The album's tracks AcoustID found on this release.
    pub matched: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct Suggestion {
    pub kind: &'static str,
    /// What the import would write without the suggestion; None if empty.
    pub current: Option<String>,
    pub value: String,
    /// When `value` is romanized: MusicBrainz's value in the original script.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original: Option<String>,
    /// The current value is `original`: the suggestion only romanizes it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub romanizes: bool,
}

/// Romanized tags, for a release whose titles or names don't sort A–Z.
#[derive(Debug, Clone, Serialize)]
pub struct Romanization {
    /// The suggestions are romanized (otherwise in the original script).
    pub applied: bool,
    /// People's names from Chinese, Japanese or Korean put the family name first.
    pub family_first: bool,
    /// The release whose Latin track titles were used, if one was.
    pub tracklist: Option<Tracklist>,
    /// Album, disc and track titles romanized; 0 when not applied.
    pub titles: u32,
    /// Artist and label names romanized.
    pub names: u32,
    /// Titles and names MusicBrainz has nothing Latin for.
    pub left: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct Tracklist {
    pub id: String,
    pub title: String,
    /// An English translation rather than a transliteration.
    pub translated: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct TrackSuggestions {
    pub id: i64,
    /// None: no track on the release matched, so only album-wide values apply.
    pub via: Option<Via>,
    /// The matched track's position, e.g. "3" or "2-3" (disc 2).
    pub position: Option<String>,
    pub suggestions: Vec<Suggestion>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Identification {
    pub method: Method,
    /// None when nothing on MusicBrainz matched.
    pub release: Option<ReleaseSummary>,
    /// Releases to choose from, best first; the matched one included.
    pub candidates: Vec<Candidate>,
    /// Tracks fingerprinted, and of those, found on AcoustID.
    pub fingerprinted: u32,
    pub recognized: u32,
    pub tracks: Vec<TrackSuggestions>,
    /// Worth knowing, e.g. why it fell back to tags.
    pub notes: Vec<String>,
    /// Present when the release doesn't sort A–Z, romanized or not.
    pub romanization: Option<Romanization>,
    /// When the oldest saved lookup used was made (seconds since the Unix
    /// epoch); None when AcoustID and MusicBrainz were asked just now.
    pub saved: Option<i64>,
}

/// One of the album's tracks, as the matching sees it.
#[derive(Debug, Clone)]
struct Local {
    id: i64,
    values: HashMap<&'static str, String>,
    tag_type: TagType,
    disc: Option<u32>,
    track: Option<u32>,
    /// AcoustID matches: (recording id, AcoustID track id, score).
    recordings: Vec<(String, String, f64)>,
}

/// What `identify` learned, kept so another release can be suggested from.
#[derive(Debug)]
pub struct Evidence {
    tracks: Vec<Local>,
    pub candidates: Vec<Candidate>,
    method: Method,
    fingerprinted: u32,
    recognized: u32,
    notes: Vec<String>,
    /// The lookups asked the services again rather than use saved answers;
    /// releases fetched for this evidence later do too.
    fresh: bool,
    /// When the oldest saved answer the lookups used was saved.
    saved: Option<i64>,
    /// Releases fetched so far, with when each was saved, so switching back
    /// and forth is quick.
    releases: Mutex<HashMap<String, mb::Answer<Arc<Release>>>>,
}

impl Evidence {
    fn release(&self, cache: Option<&LookupCache>, id: &str) -> Result<mb::Answer<Arc<Release>>> {
        let cached = self
            .releases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned();
        if let Some(r) = cached {
            return Ok(r);
        }
        let client = mb::Client {
            cache,
            fresh: self.fresh,
        };
        let found = match client.release(id) {
            Err(Error::NotFound) => {
                return Err(Error::Remote(format!(
                    "MusicBrainz has no release {id} any more."
                )));
            }
            r => r?,
        };
        let release = found.map(Arc::new);
        self.releases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.to_string(), release.clone());
        Ok(release)
    }

    /// A Latin-script tracklist of `release`: a linked one, else one among
    /// the candidates sharing its recordings.
    fn tracklist(
        &self,
        cache: Option<&LookupCache>,
        release: &Release,
    ) -> Option<mb::Answer<Arc<Release>>> {
        let total: u32 = release.media.iter().map(|m| m.tracks.len() as u32).sum();
        let linked = romanize::linked_tracklists(release);
        let others = self
            .candidates
            .iter()
            .filter(|c| c.id != release.id && sorts_az(&c.title) && c.track_count == Some(total))
            .map(|c| c.id.clone())
            .take(2);
        let ids: Vec<String> = if linked.is_empty() {
            others.collect()
        } else {
            linked
        };
        ids.iter().take(3).find_map(|id| {
            let list = self
                .release(cache, id)
                .inspect_err(|e| tracing::warn!("tracklist {id}: {e}"))
                .ok()?;
            romanize::is_tracklist_for(&list.value, release).then_some(list)
        })
    }
}

#[derive(Debug, Deserialize)]
struct Fingerprint {
    duration: f64,
    fingerprint: String,
}

/// The installed fpcalc's version, if it's installed.
pub fn fpcalc_version() -> Option<String> {
    let out = Command::new("fpcalc").arg("-version").output().ok()?;
    // "fpcalc version 1.6.2 (FFmpeg …)"
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .nth(2)
        .map(str::to_string)
}

fn fingerprint(path: &Path) -> Result<Fingerprint> {
    let out = Command::new("fpcalc").arg("-json").arg(path).output()?;
    if !out.status.success() {
        return Err(Error::Tags(format!(
            "fpcalc couldn't read {}: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    serde_json::from_slice(&out.stdout)
        .map_err(|e| Error::Tags(format!("fpcalc output for {}: {e}", path.display())))
}

/// The leading number of "3", "03" or "3/12".
fn number(v: &str) -> Option<u32> {
    let digits: String = v.trim().chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

fn year(v: &str) -> Option<i32> {
    v.trim().get(..4)?.parse().ok()
}

/// For comparing: typographic punctuation as ASCII, spacing collapsed.
fn fold(v: &str) -> String {
    let mapped: String = v
        .replace('…', "...")
        .chars()
        .map(|c| match c {
            '‘' | '’' | '′' | '`' | '´' => '\'',
            '“' | '”' | '″' => '"',
            '‐' | '‑' | '‒' | '–' | '—' | '−' => '-',
            c => c,
        })
        .collect();
    mapped.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Titles compared loosely: case, punctuation and spacing ignored.
fn title_key(v: &str) -> String {
    v.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Whether `value` would change `current` in a way worth suggesting.
fn differs(kind: &str, current: &str, value: &str) -> bool {
    match kind {
        "track_number" | "track_total" | "disc_number" | "disc_total" => {
            number(current) != number(value)
        }
        // A year is as good as the full date it agrees with.
        "date" | "original_date" => year(current).is_none() || year(current) != year(value),
        // Spacing doesn't count either: "(CV. Pile)" is "(CV.Pile)".
        _ => {
            let squeeze = |v: &str| fold(v).replace(' ', "");
            squeeze(current) != squeeze(value)
        }
    }
}

/// "drum and bass" → "Drum and Bass".
fn genre_case(name: &str) -> String {
    const SMALL: &[&str] = &["and", "of", "the", "n", "in", "a"];
    name.split(' ')
        .enumerate()
        .map(|(i, word)| {
            if i > 0 && SMALL.contains(&word) {
                return word.to_string();
            }
            word.split('-')
                .map(|part| {
                    let mut c = part.chars();
                    c.next()
                        .map(|f| f.to_uppercase().chain(c).collect())
                        .unwrap_or_default()
                })
                .collect::<Vec<String>>()
                .join("-")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// "CD", "2×CD", "CD + DVD".
fn format_summary<'a>(formats: impl Iterator<Item = Option<&'a str>>) -> Option<String> {
    let mut runs: Vec<(String, u32)> = Vec::new();
    for f in formats {
        let f = f.unwrap_or("Unknown medium").to_string();
        match runs.last_mut() {
            Some((last, n)) if *last == f => *n += 1,
            _ => runs.push((f, 1)),
        }
    }
    (!runs.is_empty()).then(|| {
        runs.into_iter()
            .map(|(f, n)| if n > 1 { format!("{n}×{f}") } else { f })
            .collect::<Vec<_>>()
            .join(" + ")
    })
}

/// The value most tracks share for `kind`.
fn consensus<'a>(tracks: &'a [Local], kind: &str) -> Option<&'a str> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for t in tracks {
        if let Some(v) = t
            .values
            .get(kind)
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
        {
            *counts.entry(v).or_default() += 1;
        }
    }
    counts
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(a.0)))
        .map(|(v, _)| v)
}

/// A release AcoustID's recordings appear on, with the tracks found there.
struct Tally<'a> {
    release: &'a mb::AcoustRelease,
    /// Local track index -> best match score, in track order so equal
    /// releases' scores add up to exactly the same total.
    scores: BTreeMap<usize, f64>,
}

impl Tally<'_> {
    fn rank(&self, tracks: &[Local]) -> f64 {
        let n = tracks.len() as f64;
        let mut score: f64 = self.scores.values().sum();
        match self.release.track_count {
            Some(c) if c as f64 == n => score += 0.5 * n,
            Some(c) => score -= (c as f64 - n).abs().min(n) * 0.1,
            None => {}
        }
        let album = consensus(tracks, "album").map(title_key);
        if album.is_some() && album == self.release.title.as_deref().map(title_key) {
            score += 0.2 * n;
        }
        let year_now = consensus(tracks, "date").and_then(year);
        if year_now.is_some() && year_now == self.release.date.as_ref().and_then(|d| d.year) {
            score += 0.1 * n;
        }
        if consensus(tracks, "mb_release_id") == Some(self.release.id.as_str()) {
            score += n;
        }
        score
    }

    fn candidate(&self) -> Candidate {
        let r = self.release;
        Candidate {
            id: r.id.clone(),
            title: r.title.clone().unwrap_or_default(),
            artist: None,
            date: r.date.as_ref().and_then(|d| d.text()),
            country: r.country.clone(),
            format: format_summary(r.mediums.iter().map(|m| m.format.as_deref())),
            track_count: r.track_count,
            matched: self.scores.len() as u32,
        }
    }
}

/// Releases the fingerprints point to, best first, and whether the best
/// one covers enough of the recognized tracks to trust.
fn audio_candidates(tracks: &[Local], results: &[Vec<AcoustResult>]) -> (Vec<Candidate>, bool) {
    let mut tallies: HashMap<&str, Tally> = HashMap::new();
    for (i, found) in results.iter().enumerate() {
        for result in found.iter().filter(|r| r.score >= MIN_SCORE) {
            for rec in &result.recordings {
                for release in &rec.releases {
                    let tally = tallies.entry(release.id.as_str()).or_insert(Tally {
                        release,
                        scores: BTreeMap::new(),
                    });
                    let best = tally.scores.entry(i).or_default();
                    *best = best.max(result.score);
                }
            }
        }
    }
    let mut ranked: Vec<(f64, &Tally)> = tallies.values().map(|t| (t.rank(tracks), t)).collect();
    ranked.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            // Equal releases: the earliest dated one (undated ones are often
            // transliterated pseudo-releases), then a stable order.
            .then_with(|| {
                let date = |t: &Tally| {
                    let d = t.release.date.as_ref().and_then(|d| d.text());
                    (d.is_none(), d)
                };
                date(a.1).cmp(&date(b.1))
            })
            .then_with(|| a.1.release.id.cmp(&b.1.release.id))
    });
    let recognized = results
        .iter()
        .filter(|r| r.iter().any(|x| x.score >= MIN_SCORE))
        .count();
    let trusted = ranked
        .first()
        .is_some_and(|(_, t)| !t.scores.is_empty() && t.scores.len() * 2 >= recognized);
    let candidates = ranked
        .into_iter()
        .take(MAX_CANDIDATES)
        .map(|(_, t)| t.candidate())
        .collect();
    (candidates, trusted)
}

/// A barcode as MusicBrainz holds it: digits only, 8 (EAN-8) to 14 long.
fn as_barcode(v: &str) -> Option<String> {
    let digits: String = v.chars().filter(|c| !matches!(c, ' ' | '-')).collect();
    let valid = (8..=14).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit());
    valid.then_some(digits)
}

/// An ISRC as MusicBrainz holds it, e.g. "JP-U90-26-04206" -> "JPU902604206".
fn as_isrc(v: &str) -> Option<String> {
    let code: String = v
        .chars()
        .filter(|c| !matches!(c, ' ' | '-'))
        .collect::<String>()
        .to_ascii_uppercase();
    let b = code.as_bytes();
    let valid = b.len() == 12
        && b[..2].iter().all(u8::is_ascii_uppercase)
        && b[2..5].iter().all(u8::is_ascii_alphanumeric)
        && b[5..].iter().all(u8::is_ascii_digit);
    valid.then_some(code)
}

/// The album's ISRCs, each with the tracks tagged with it.
fn isrcs(tracks: &[Local]) -> BTreeMap<String, Vec<usize>> {
    let mut out: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, t) in tracks.iter().enumerate() {
        if let Some(code) = t.values.get("isrc").and_then(|v| as_isrc(v)) {
            out.entry(code).or_default().push(i);
        }
    }
    out
}

/// What searching MusicBrainz by the album's tags found, and how.
struct Searched {
    /// Barcode, Isrc or Tags.
    method: Method,
    /// Best first, each with a score out of 100 (100: an exact match).
    found: Vec<(u32, Candidate)>,
    /// What the album was searched by, for the note when nothing was found.
    by: Vec<&'static str>,
}

fn search_candidate(r: Release) -> Candidate {
    Candidate {
        artist: Some(mb::credit_name(&r.artist_credit)).filter(|a| !a.is_empty()),
        format: format_summary(r.media.iter().map(|m| m.format.as_deref())),
        date: r.date.filter(|d| !d.is_empty()),
        country: r.country,
        track_count: r.track_count,
        matched: 0,
        title: r.title,
        id: r.id,
    }
}

/// Releases found with a score each, best first for `tracks`: the score,
/// then the album's number of tracks, then its year. Ties keep their order.
fn rank_found(
    tracks: &[Local],
    found: impl IntoIterator<Item = (u32, Release)>,
) -> Vec<(u32, Candidate)> {
    let n = tracks.len() as u32;
    let year_now = consensus(tracks, "date").and_then(year);
    let mut ranked: Vec<(i64, (u32, Candidate))> = found
        .into_iter()
        .map(|(score, r)| {
            let mut rank = i64::from(score);
            if r.track_count == Some(n) {
                rank += 50;
            }
            if year_now.is_some() && r.date.as_deref().and_then(year) == year_now {
                rank += 10;
            }
            (rank, (score, search_candidate(r)))
        })
        .collect();
    ranked.sort_by_key(|a| std::cmp::Reverse(a.0));
    ranked.into_iter().map(|(_, c)| c).collect()
}

/// Search results MusicBrainz scored well enough, ranked.
fn well_matched(tracks: &[Local], found: Vec<Release>) -> Vec<(u32, Candidate)> {
    let scored = found
        .into_iter()
        .map(|r| (r.score.unwrap_or(0), r))
        .filter(|(score, _)| *score >= MIN_SEARCH_SCORE);
    rank_found(tracks, scored)
}

/// The releases `found` recordings are on, scored by the share of the
/// album's ISRCs each holds (100: all of them).
fn isrc_candidates(
    tracks: &[Local],
    isrcs: &BTreeMap<String, Vec<usize>>,
    found: Vec<mb::FoundRecording>,
) -> Vec<(u32, Candidate)> {
    let tagged: HashSet<usize> = isrcs.values().flatten().copied().collect();
    let mut on: BTreeMap<String, (Release, HashSet<usize>)> = BTreeMap::new();
    for recording in found {
        let held: Vec<usize> = recording
            .isrcs
            .iter()
            .filter_map(|code| isrcs.get(&code.to_ascii_uppercase()))
            .flatten()
            .copied()
            .collect();
        if held.is_empty() {
            continue;
        }
        for release in recording.releases {
            let (_, tracks) = on
                .entry(release.id.clone())
                .or_insert_with(|| (release, HashSet::new()));
            tracks.extend(&held);
        }
    }
    let mut scored: Vec<(u32, Release)> = on
        .into_values()
        .map(|(r, held)| ((held.len() * 100 / tagged.len().max(1)) as u32, r))
        .collect();
    // Equal ones: the earliest dated first, as originals come before reissues.
    scored.sort_by(|a, b| {
        let date = |r: &Release| {
            let d = r.date.clone().filter(|d| !d.is_empty());
            (d.is_none(), d)
        };
        date(&a.1).cmp(&date(&b.1))
    });
    rank_found(tracks, scored)
}

/// Releases found by searching MusicBrainz with the album's tags, best
/// first. A barcode names the release and ISRCs its recordings, so either
/// settles it; otherwise it's searched for by title and artist.
fn tag_candidates(
    client: mb::Client,
    tracks: &[Local],
    barcode: Option<&str>,
) -> Result<mb::Answer<Searched>> {
    let mut saved = None;
    let mut by = Vec::new();
    let done = |method, found, by, saved| {
        Ok(mb::Answer {
            value: Searched { method, found, by },
            saved,
        })
    };
    if let Some(code) = barcode {
        by.push("barcode");
        let res = client.search_barcode(code)?;
        saved = oldest(saved, res.saved);
        let found = well_matched(tracks, res.value);
        if !found.is_empty() {
            return done(Method::Barcode, found, by, saved);
        }
    }
    // The first few place an album; releases are scored by those sent.
    let isrcs: BTreeMap<String, Vec<usize>> = isrcs(tracks).into_iter().take(MAX_ISRCS).collect();
    let mut partly = Vec::new();
    if !isrcs.is_empty() {
        by.push("ISRC");
        let codes: Vec<&str> = isrcs.keys().map(String::as_str).collect();
        let res = client.search_isrcs(&codes)?;
        saved = oldest(saved, res.saved);
        let found = isrc_candidates(tracks, &isrcs, res.value);
        // At least half of them on one release: that's the album.
        if found.first().is_some_and(|(score, _)| *score >= 50) {
            return done(Method::Isrc, found, by, saved);
        }
        partly = found;
    }
    let Some(album) = consensus(tracks, "album") else {
        return done(Method::Isrc, partly, by, saved);
    };
    by.push("title and artist");
    let artist = consensus(tracks, "album_artist").or_else(|| consensus(tracks, "artist"));
    let search = client.search_releases(album, artist)?;
    saved = oldest(saved, search.saved);
    let mut found = well_matched(tracks, search.value);
    if let (true, Some(artist)) = (found.is_empty(), artist) {
        // A release search matches the artist as the release credits them,
        // so "Ushio Reira" misses 汐れいら, whose sort name it is. Look the
        // artist up by name, sort name and alias, and search by them.
        let artists = client.search_artists(artist)?;
        saved = oldest(saved, artists.saved);
        let ids: Vec<&str> = artists
            .value
            .iter()
            .filter(|a| a.score.unwrap_or(0) >= 90 && mb::is_mbid(&a.id))
            .filter(|a| !a.name.eq_ignore_ascii_case(artist))
            .take(3)
            .map(|a| a.id.as_str())
            .collect();
        if !ids.is_empty() {
            let search = client.search_releases_by(album, &ids)?;
            saved = oldest(saved, search.saved);
            found = well_matched(tracks, search.value);
        }
    }
    if found.is_empty() && !partly.is_empty() {
        return done(Method::Isrc, partly, by, saved);
    }
    found.extend(partly);
    done(Method::Tags, found, by, saved)
}

/// "a", "a and b", "a, b, and c".
fn listed(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [one] => one.to_string(),
        [a, b] => format!("{a} and {b}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
}

/// Read, fingerprint and look up `album_id`, then suggest tags from the
/// release that fits best. `key` is the AcoustID API key; without it (or
/// fpcalc) the album is matched by its tags alone. Lookups made before are
/// answered from `client`'s cache unless it asks for fresh ones.
pub fn identify(
    lib: &Library,
    client: mb::Client,
    album_id: &str,
    key: Option<&str>,
    romanize: Option<romanize::Options>,
    separator: &str,
    step: &(dyn Fn(Step) + Sync),
) -> Result<(Evidence, Identification)> {
    let detail = lib.album(album_id)?.ok_or(Error::NotFound)?;
    let n = detail.tracks.len() as u32;
    step(Step {
        stage: Stage::Reading,
        done: 0,
        total: n,
    });
    let inbox = lib.inbox_setup()?;
    let read = import::read_album(lib, &detail, &inbox.autofill, false)?;
    let mut tracks: Vec<Local> = detail
        .tracks
        .iter()
        .zip(&read)
        .map(|(t, r)| Local {
            id: t.id,
            disc: r.values.get("disc_number").and_then(|v| number(v)),
            track: r.values.get("track_number").and_then(|v| number(v)),
            values: r.values.clone(),
            tag_type: r.tag_type,
            recordings: Vec::new(),
        })
        .collect();

    let mut notes = Vec::new();
    let mut saved = None;
    let mut fingerprinted = 0;
    let mut results: Vec<Vec<AcoustResult>> = vec![Vec::new(); tracks.len()];
    match (key, fpcalc_version()) {
        (Some(key), Some(_)) => {
            let done = AtomicU32::new(0);
            step(Step {
                stage: Stage::Fingerprinting,
                done: 0,
                total: n,
            });
            let prints: Vec<Option<Fingerprint>> = read
                .par_iter()
                .map(|t| {
                    let print = fingerprint(&t.path)
                        .inspect_err(|e| tracing::warn!("fingerprint: {e}"))
                        .ok();
                    step(Step {
                        stage: Stage::Fingerprinting,
                        done: done.fetch_add(1, Ordering::Relaxed) + 1,
                        total: n,
                    });
                    print
                })
                .collect();
            let sent: Vec<(usize, u32, &str)> = prints
                .iter()
                .enumerate()
                .filter_map(|(i, p)| {
                    let p = p.as_ref()?;
                    Some((i, p.duration.round() as u32, p.fingerprint.as_str()))
                })
                .collect();
            fingerprinted = sent.len() as u32;
            if sent.len() < tracks.len() {
                notes.push(format!(
                    "fpcalc couldn't read {} of the {} tracks.",
                    tracks.len() - sent.len(),
                    tracks.len()
                ));
            }
            step(Step {
                stage: Stage::Acoustid,
                done: 0,
                total: fingerprinted,
            });
            let prints: Vec<(u32, &str)> = sent.iter().map(|(_, d, f)| (*d, *f)).collect();
            let found = client.lookup(key, &prints)?;
            saved = found.saved;
            for ((i, _, _), found) in sent.iter().zip(found.value) {
                results[*i] = found;
            }
        }
        (None, _) => notes.push(
            "No AcoustID API key is set, so the album was matched by its tags. Add one in Inbox setup to match by audio.".into(),
        ),
        (_, None) => notes.push(
            "fpcalc isn't installed (it comes with Chromaprint), so the album was matched by its tags.".into(),
        ),
    }
    for (track, found) in tracks.iter_mut().zip(&results) {
        for result in found.iter().filter(|r| r.score >= MIN_SCORE) {
            for rec in &result.recordings {
                track
                    .recordings
                    .push((rec.id.clone(), result.id.clone(), result.score));
            }
        }
    }
    let recognized = tracks.iter().filter(|t| !t.recordings.is_empty()).count() as u32;

    // The barcode, or the UPC some stores tag files with instead.
    let barcode = consensus(&tracks, "barcode")
        .and_then(as_barcode)
        .or_else(|| {
            read.first()
                .and_then(|t| tags::upc(&t.path))
                .as_deref()
                .and_then(as_barcode)
        });
    let mut searched = Vec::new();
    let (audio, trusted) = audio_candidates(&tracks, &results);
    let complete = |c: &Candidate| c.track_count == Some(n);
    let mut method = Method::Acoustid;
    let mut candidates = audio;
    // A search by title helps when the fingerprints don't settle it, or
    // point to a release with other tracks too (a box set, a compilation).
    if !trusted || !candidates.first().is_some_and(complete) {
        step(Step {
            stage: Stage::Searching,
            done: 0,
            total: 1,
        });
        let search = tag_candidates(client, &tracks, barcode.as_deref())?;
        saved = oldest(saved, search.saved);
        let Searched {
            method: found_by,
            found: tagged,
            by,
        } = search.value;
        searched = by;
        let exact = tagged
            .iter()
            .position(|(score, c)| *score == 100 && complete(c));
        if !trusted && !tagged.is_empty() {
            let how = found_by.by();
            if fingerprinted > 0 {
                notes.push(if recognized == 0 {
                    format!("AcoustID doesn't know these recordings, so the album was matched by its {how}.")
                } else {
                    format!("AcoustID recognized only {recognized} of {n} tracks, and they don't agree on a release, so the album was matched by its {how}.")
                });
            }
            method = found_by;
            let audio = std::mem::take(&mut candidates);
            candidates = tagged.into_iter().map(|(_, c)| c).chain(audio).collect();
        } else if let (true, Some(i)) = (trusted, exact) {
            let bigger = candidates
                .first()
                .map(|c| c.title.clone())
                .unwrap_or_default();
            let why = match found_by {
                Method::Barcode => "has the album's barcode and track count",
                Method::Isrc => "has the album's ISRCs and track count",
                _ => "matches the album's title, artist and track count",
            };
            notes.push(format!(
                "The recordings are also on “{bigger}”, a release with more tracks; this one {why}."
            ));
            method = found_by;
            let mut tagged: Vec<Candidate> = tagged.into_iter().map(|(_, c)| c).collect();
            let picked = tagged.remove(i);
            let audio = std::mem::take(&mut candidates);
            candidates = std::iter::once(picked).chain(audio).chain(tagged).collect();
        } else {
            candidates.extend(tagged.into_iter().map(|(_, c)| c));
        }
    }
    if candidates.is_empty() {
        let by = listed(&searched);
        notes.push(if recognized > 0 {
            "AcoustID knows these recordings, but not on any release.".into()
        } else if searched.is_empty() {
            "The album has no title, barcode or ISRCs to search MusicBrainz by.".into()
        } else if fingerprinted > 0 {
            format!("Neither AcoustID nor a MusicBrainz search by {by} found this album.")
        } else {
            format!("A MusicBrainz search by {by} found nothing.")
        });
    }
    let mut seen = HashSet::new();
    candidates.retain(|c| seen.insert(c.id.clone()));
    candidates.truncate(MAX_CANDIDATES);

    let evidence = Evidence {
        tracks,
        candidates,
        method,
        fingerprinted,
        recognized,
        notes,
        fresh: client.fresh,
        saved,
        releases: Mutex::default(),
    };
    let result = match evidence.candidates.first() {
        Some(best) => suggest(
            &evidence,
            client.cache,
            &best.id.clone(),
            false,
            romanize,
            separator,
            step,
        )?,
        None => Identification {
            method,
            release: None,
            candidates: Vec::new(),
            fingerprinted,
            recognized,
            tracks: Vec::new(),
            notes: evidence.notes.clone(),
            romanization: None,
            saved: evidence.saved,
        },
    };
    Ok((evidence, result))
}

/// Suggest tags from release `release_id`; `chosen` when picked by hand.
/// With `romanize`, titles and names that don't sort A–Z are suggested in
/// Latin script where MusicBrainz has them, written as its options say.
/// Tags with several values have them joined by `separator`. Releases are
/// fetched as `identify` fetched the evidence: from `cache` (if saving) where
/// they were fetched before, unless it asked the services again.
pub fn suggest(
    evidence: &Evidence,
    cache: Option<&LookupCache>,
    release_id: &str,
    chosen: bool,
    romanize: Option<romanize::Options>,
    separator: &str,
    step: &(dyn Fn(Step) + Sync),
) -> Result<Identification> {
    step(Step {
        stage: Stage::Release,
        done: 0,
        total: 1,
    });
    let found = evidence.release(cache, release_id)?;
    let mut saved = oldest(evidence.saved, found.saved);
    let release = found.value;
    let (shown, romanization) = if !romanize::needs_romanizing(&release) {
        (None, None)
    } else if let Some(options) = romanize {
        step(Step {
            stage: Stage::Romanizing,
            done: 0,
            total: 1,
        });
        let list = evidence.tracklist(cache, &release).map(|list| {
            saved = oldest(saved, list.saved);
            list.value
        });
        let (romanized, counts) = romanize::romanize(&release, list.as_deref(), options);
        let tracklist = list.map(|l| Tracklist {
            id: l.id.clone(),
            title: l.title.clone(),
            translated: romanize::is_translation(&l, &release),
        });
        let info = Romanization {
            applied: true,
            family_first: options.family_first,
            tracklist,
            titles: counts.titles,
            names: counts.names,
            left: counts.left,
        };
        (Some(romanized), Some(info))
    } else {
        let info = Romanization {
            applied: false,
            family_first: false,
            tracklist: None,
            titles: 0,
            names: 0,
            left: 0,
        };
        (None, Some(info))
    };
    let mut candidates = evidence.candidates.clone();
    if !candidates.iter().any(|c| c.id == release.id) {
        candidates.insert(0, summary_candidate(&release));
    }
    // Search results carry the artist; fill it in for the picked release.
    if let Some(c) = candidates.iter_mut().find(|c| c.id == release.id) {
        c.artist = Some(mb::credit_name(&release.artist_credit));
    }
    Ok(Identification {
        method: if chosen {
            Method::Chosen
        } else {
            evidence.method
        },
        release: Some(summary(&release)),
        candidates,
        fingerprinted: evidence.fingerprinted,
        recognized: evidence.recognized,
        tracks: match &shown {
            Some(romanized) => suggestions(&evidence.tracks, romanized, Some(&release), separator),
            None => suggestions(&evidence.tracks, &release, None, separator),
        },
        notes: evidence.notes.clone(),
        romanization,
        saved,
    })
}

fn summary(r: &Release) -> ReleaseSummary {
    ReleaseSummary {
        id: r.id.clone(),
        title: r.title.clone(),
        artist: mb::credit_name(&r.artist_credit),
        date: r.date.clone().filter(|d| !d.is_empty()),
        country: r.country.clone(),
        format: format_summary(r.media.iter().map(|m| m.format.as_deref())),
        track_count: r.media.iter().map(|m| m.tracks.len() as u32).sum(),
        disambiguation: r.disambiguation.clone().filter(|d| !d.is_empty()),
        front_art: r.cover_art.as_ref().is_some_and(|c| c.front),
        release_group: r.release_group.as_ref().map(|g| g.id.clone()),
    }
}

fn summary_candidate(r: &Release) -> Candidate {
    let s = summary(r);
    Candidate {
        id: s.id,
        title: s.title,
        artist: Some(s.artist),
        date: s.date,
        country: s.country,
        format: s.format,
        track_count: Some(s.track_count),
        matched: 0,
    }
}

/// Pair each local track with a release track: by fingerprint, then by
/// disc and track number (when the release has as many tracks as the
/// album), then by title. Returns (medium, track, how).
fn map_tracks(
    tracks: &[Local],
    release: &Release,
    original: Option<&Release>,
) -> Vec<Option<(usize, usize, Via)>> {
    let positions: Vec<(usize, usize, u32, u32)> = release
        .media
        .iter()
        .enumerate()
        .flat_map(|(m, medium)| {
            let disc = medium.position.unwrap_or(m as u32 + 1);
            medium
                .tracks
                .iter()
                .enumerate()
                .map(move |(t, track)| (m, t, disc, track.position.unwrap_or(t as u32 + 1)))
        })
        .collect();
    let track_at = |m: usize, t: usize| &release.media[m].tracks[t];
    let single = release.media.len() == 1;
    let mut taken: HashSet<(usize, usize)> = HashSet::new();
    let mut out = vec![None; tracks.len()];

    for (i, local) in tracks.iter().enumerate() {
        let recordings: HashSet<&str> = local.recordings.iter().map(|r| r.0.as_str()).collect();
        let same_place = |disc: u32, pos: u32| {
            local.track == Some(pos) && (local.disc.unwrap_or(1) == disc || single)
        };
        let hit = positions
            .iter()
            .filter(|(m, t, _, _)| {
                !taken.contains(&(*m, *t))
                    && track_at(*m, *t)
                        .recording
                        .as_ref()
                        .is_some_and(|r| recordings.contains(r.id.as_str()))
            })
            // A recording on the release twice (a reprise): prefer the same spot.
            .min_by_key(|(_, _, disc, pos)| !same_place(*disc, *pos));
        if let Some(&(m, t, _, _)) = hit {
            taken.insert((m, t));
            out[i] = Some((m, t, Via::Acoustid));
        }
    }
    // Numbers only mean the same thing when the release is laid out like the
    // album; on a bigger or smaller one, track 3 is some other song.
    let same_shape = positions.len() == tracks.len();
    for (i, local) in tracks.iter().enumerate() {
        let Some(pos) = local.track.filter(|_| same_shape && out[i].is_none()) else {
            continue;
        };
        if local.disc.is_none() && !single {
            continue;
        }
        let disc = local.disc.unwrap_or(1);
        if let Some(&(m, t, _, _)) = positions
            .iter()
            .find(|(m, t, d, p)| !taken.contains(&(*m, *t)) && *p == pos && (*d == disc || single))
        {
            taken.insert((m, t));
            out[i] = Some((m, t, Via::Position));
        }
    }
    for (i, local) in tracks.iter().enumerate() {
        let Some(title) = local.values.get("title").filter(|_| out[i].is_none()) else {
            continue;
        };
        let title = fold(title).to_lowercase();
        // Romanized or not, the album's title may be either.
        let same = |r: &Release, m: usize, t: usize| {
            fold(&r.media[m].tracks[t].title).to_lowercase() == title
        };
        if let Some(&(m, t, _, _)) = positions.iter().find(|(m, t, _, _)| {
            !taken.contains(&(*m, *t))
                && (same(release, *m, *t) || original.is_some_and(|o| same(o, *m, *t)))
        }) {
            taken.insert((m, t));
            out[i] = Some((m, t, Via::Title));
        }
    }
    out
}

/// The values in order, without repeats or blanks, joined by `separator`;
/// None when there are none.
fn joined(values: impl IntoIterator<Item = String>, separator: &str) -> Option<String> {
    let mut seen = Vec::new();
    for v in values {
        let v = v.trim().to_string();
        if !v.is_empty() && !seen.contains(&v) {
            seen.push(v);
        }
    }
    (!seen.is_empty()).then(|| seen.join(separator))
}

fn push(out: &mut Vec<(&'static str, String)>, kind: &'static str, value: Option<String>) {
    if let Some(v) = value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    {
        out.push((kind, v));
    }
}

/// Values every track of the release shares; several of one kind are
/// joined by `separator`.
fn release_values(r: &Release, separator: &str) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    let rg = r.release_group.as_ref();
    push(&mut out, "album", Some(r.title.clone()));
    push(
        &mut out,
        "album_artist",
        Some(mb::credit_name(&r.artist_credit)),
    );
    push(
        &mut out,
        "album_artists",
        joined(r.artist_credit.iter().map(|c| c.name.clone()), separator),
    );
    push(
        &mut out,
        "album_artist_sort",
        Some(mb::credit_sort(&r.artist_credit)),
    );
    push(&mut out, "date", r.date.clone());
    push(
        &mut out,
        "original_date",
        rg.and_then(|g| g.first_release_date.clone()),
    );
    let genres = if r.genres.is_empty() {
        rg.map(|g| g.genres.as_slice()).unwrap_or_default()
    } else {
        &r.genres
    };
    // The most voted first; ties keep MusicBrainz's order.
    let mut genres: Vec<_> = genres.iter().collect();
    genres.sort_by_key(|g| std::cmp::Reverse(g.count));
    push(
        &mut out,
        "genre",
        joined(genres.iter().map(|g| genre_case(&g.name)), separator),
    );
    let label = r.label_info.iter().find(|l| {
        l.label.as_ref().is_some_and(|l| l.name != "[no label]") || l.catalog_number.is_some()
    });
    push(
        &mut out,
        "label",
        label
            .and_then(|l| l.label.as_ref())
            .map(|l| l.name.clone())
            .filter(|n| n != "[no label]"),
    );
    push(
        &mut out,
        "catalog_number",
        label
            .and_then(|l| l.catalog_number.clone())
            .filter(|c| !c.eq_ignore_ascii_case("[none]")),
    );
    push(&mut out, "barcode", r.barcode.clone());
    push(&mut out, "release_country", r.country.clone());
    push(
        &mut out,
        "release_type",
        rg.and_then(|g| {
            let primary = g.primary_type.as_ref()?.to_lowercase();
            let rest = g.secondary_types.iter().map(|t| t.to_lowercase());
            joined(std::iter::once(primary).chain(rest), separator)
        }),
    );
    push(
        &mut out,
        "disc_total",
        (!r.media.is_empty()).then(|| r.media.len().to_string()),
    );
    push(&mut out, "mb_release_id", Some(r.id.clone()));
    push(&mut out, "mb_release_group_id", rg.map(|g| g.id.clone()));
    push(
        &mut out,
        "mb_release_artist_id",
        joined(
            r.artist_credit.iter().map(|c| c.artist.id.clone()),
            separator,
        ),
    );
    let first = r.artist_credit.first().map(|c| &c.artist.id);
    push(
        &mut out,
        "compilation",
        first
            .filter(|id| *id == mb::VARIOUS_ARTISTS)
            .map(|_| "1".to_string()),
    );
    push(
        &mut out,
        "script",
        r.text.as_ref().and_then(|t| t.script.clone()),
    );
    push(
        &mut out,
        "language",
        r.text.as_ref().and_then(|t| t.language.clone()),
    );
    out
}

/// Values for one track of the release; several of one kind are joined by
/// `separator`.
fn track_values(
    r: &Release,
    medium: &Medium,
    track: &mb::Track,
    acoustid: Option<&str>,
    separator: &str,
) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    let credit = if track.artist_credit.is_empty() {
        &r.artist_credit
    } else {
        &track.artist_credit
    };
    push(&mut out, "title", Some(track.title.clone()));
    push(&mut out, "artist", Some(mb::credit_name(credit)));
    push(
        &mut out,
        "artists",
        joined(credit.iter().map(|c| c.name.clone()), separator),
    );
    push(&mut out, "artist_sort", Some(mb::credit_sort(credit)));
    push(
        &mut out,
        "track_number",
        track.position.map(|p| p.to_string()),
    );
    push(
        &mut out,
        "track_total",
        Some(
            medium
                .track_count
                .unwrap_or(medium.tracks.len() as u32)
                .to_string(),
        ),
    );
    push(
        &mut out,
        "disc_number",
        medium.position.map(|p| p.to_string()),
    );
    push(&mut out, "media", medium.format.clone());
    push(&mut out, "disc_subtitle", medium.title.clone());
    push(&mut out, "mb_track_id", Some(track.id.clone()));
    let recording = track.recording.as_ref();
    push(&mut out, "mb_recording_id", recording.map(|r| r.id.clone()));
    push(
        &mut out,
        "mb_artist_id",
        joined(credit.iter().map(|c| c.artist.id.clone()), separator),
    );
    push(
        &mut out,
        "isrc",
        recording.and_then(|r| r.isrcs.first().cloned()),
    );
    push(&mut out, "acoustid", acoustid.map(str::to_string));
    out
}

/// Suggestions from `release`. When it's a romanization of `original`,
/// suggestions say what they romanize, and never trade a tag that sorts
/// A–Z for one that doesn't.
fn suggestions(
    tracks: &[Local],
    release: &Release,
    original: Option<&Release>,
    separator: &str,
) -> Vec<TrackSuggestions> {
    let shared = release_values(release, separator);
    let shared_original = original
        .map(|o| release_values(o, separator))
        .unwrap_or_default();
    let mapped = map_tracks(tracks, release, original);
    let multi_disc = release.media.len() > 1;
    tracks
        .iter()
        .zip(mapped)
        .map(|(local, mapped)| {
            let mut values = shared.clone();
            let mut originals = shared_original.clone();
            let mut position = None;
            if let Some((m, t, _)) = mapped {
                let medium = &release.media[m];
                let track = &medium.tracks[t];
                let recording = track.recording.as_ref().map(|r| r.id.as_str());
                // The AcoustID id of the fingerprint match for this recording.
                let acoustid = local
                    .recordings
                    .iter()
                    .filter(|(rec, _, _)| Some(rec.as_str()) == recording)
                    .max_by(|a, b| a.2.total_cmp(&b.2))
                    .map(|(_, id, _)| id.as_str());
                values.extend(track_values(release, medium, track, acoustid, separator));
                if let Some(o) = original {
                    let medium = &o.media[m];
                    let track = &medium.tracks[t];
                    originals.extend(track_values(o, medium, track, acoustid, separator));
                }
                let pos = track.position.unwrap_or(t as u32 + 1);
                position = Some(if multi_disc {
                    format!("{}-{pos}", medium.position.unwrap_or(m as u32 + 1))
                } else {
                    pos.to_string()
                });
            }
            // A sort name sorts its artist: only suggest it where the artist
            // agrees with MusicBrainz (or will be filled from it).
            let disagrees = |parent: &str| {
                let theirs = values.iter().find(|(k, _)| *k == parent).map(|(_, v)| v);
                match (local.values.get(parent), theirs) {
                    (Some(ours), Some(theirs)) => differs(parent, ours, theirs),
                    _ => false,
                }
            };
            let unsorted: Vec<&str> = [
                ("artist", "artist_sort"),
                ("album_artist", "album_artist_sort"),
            ]
            .into_iter()
            .filter(|(parent, _)| disagrees(parent))
            .map(|(_, sort)| sort)
            .collect();
            let suggestions = values
                .into_iter()
                .filter(|(kind, _)| !unsorted.contains(kind))
                .filter(|(kind, _)| tags::can_store(local.tag_type, kind))
                .filter_map(|(kind, value)| {
                    let current = local.values.get(kind).filter(|v| !v.trim().is_empty());
                    let before = originals
                        .iter()
                        .find(|(k, _)| *k == kind)
                        .map(|(_, v)| v)
                        .filter(|v| **v != value)
                        .cloned();
                    // The tag is MusicBrainz's original-script value, give or
                    // take case, spacing and punctuation.
                    let romanizes = match (current, &before) {
                        (Some(c), Some(o)) => {
                            !title_key(o).is_empty() && title_key(c) == title_key(o)
                        }
                        _ => false,
                    };
                    let suggestion = Suggestion {
                        kind,
                        current: current.cloned(),
                        value,
                        original: before,
                        romanizes,
                    };
                    match current {
                        None => Some(suggestion),
                        // Preferring Latin script: keep a tag that sorts A–Z
                        // over one MusicBrainz only has in another script.
                        Some(c)
                            if original.is_some()
                                && sorts_az(c)
                                && !sorts_az(&suggestion.value) =>
                        {
                            None
                        }
                        Some(c)
                            if !FILL_ONLY.contains(&kind)
                                && differs(kind, c, &suggestion.value) =>
                        {
                            Some(suggestion)
                        }
                        Some(_) => None,
                    }
                })
                .collect();
            TrackSuggestions {
                id: local.id,
                via: mapped.map(|(_, _, via)| via),
                position,
                suggestions,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(id: i64, values: &[(&'static str, &str)]) -> Local {
        let values: HashMap<&'static str, String> =
            values.iter().map(|(k, v)| (*k, v.to_string())).collect();
        Local {
            id,
            disc: values.get("disc_number").and_then(|v| number(v)),
            track: values.get("track_number").and_then(|v| number(v)),
            values,
            tag_type: TagType::VorbisComments,
            recordings: Vec::new(),
        }
    }

    /// Searches answered from saved lookups; one that isn't saved fails.
    fn searching(cache: &LookupCache) -> mb::Client<'_> {
        mb::Client {
            cache: Some(cache),
            fresh: false,
        }
    }

    fn found(id: &str, title: &str, score: u32, tracks: u32, date: &str) -> serde_json::Value {
        serde_json::json!({"id": id, "title": title, "score": score, "track-count": tracks,
            "date": date, "artist-credit": [{"name": "汐れいら",
                "artist": {"id": "c9c4b066-7ac7-455a-aff5-b4684f769876", "sort-name": "Ushio Reira"}}]})
    }

    #[test]
    fn barcodes_and_isrcs_are_read_as_musicbrainz_holds_them() {
        assert_eq!(
            as_barcode(" 4547-3668-60535 ").as_deref(),
            Some("4547366860535")
        );
        assert_eq!(as_barcode("1234567"), None);
        assert_eq!(as_barcode("45473668605x5"), None);
        assert_eq!(as_isrc("jp-u90-26-04206").as_deref(), Some("JPU902604206"));
        assert_eq!(as_isrc("JPU90260420"), None);
        assert_eq!(as_isrc("1PU902604206"), None);
        assert_eq!(listed(&["barcode"]), "barcode");
        assert_eq!(
            listed(&["barcode", "ISRC", "title and artist"]),
            "barcode, ISRC, and title and artist"
        );
    }

    #[test]
    fn a_barcode_settles_it_without_searching_by_title() {
        let cache = LookupCache::in_memory();
        let body = serde_json::json!({"releases": [
            found("a1111111-1111-4111-8111-111111111111", "Clover (CD)", 100, 3, "2026-09-02"),
            found("b2222222-2222-4222-8222-222222222222", "Clover", 100, 2, "2026-08-29"),
        ]});
        // A 12-digit UPC is asked for as the EAN too.
        mb::save_search(
            &cache,
            "release",
            "barcode:(454736682783 OR 0454736682783)",
            &body.to_string(),
        );
        let tracks = [
            local(1, &[("album", "Clover"), ("artist", "Ushio Reira")]),
            local(2, &[("album", "Clover"), ("artist", "Ushio Reira")]),
        ];
        // Searching by title would fail: nothing is saved for it.
        let s = tag_candidates(searching(&cache), &tracks, Some("454736682783")).unwrap();
        assert_eq!(s.value.method, Method::Barcode);
        assert_eq!(s.value.by, ["barcode"]);
        let titles: Vec<&str> = s
            .value
            .found
            .iter()
            .map(|(_, c)| c.title.as_str())
            .collect();
        // The one with the album's two tracks first.
        assert_eq!(titles, ["Clover", "Clover (CD)"]);
        assert!(s.saved.is_some());
    }

    #[test]
    fn isrcs_find_the_release_holding_most_of_the_tracks() {
        let cache = LookupCache::in_memory();
        let single = found(
            "c3333333-3333-4333-8333-333333333333",
            "メシア",
            100,
            2,
            "2026-01-07",
        );
        let best_of = found(
            "d4444444-4444-4444-8444-444444444444",
            "Best",
            100,
            20,
            "2027-01-01",
        );
        let body = serde_json::json!({"recordings": [
            {"id": "r1", "isrcs": ["JPB602601767"], "releases": [best_of, single]},
            {"id": "r2", "isrcs": ["JPB602601768", "USXX00000001"], "releases": [single]},
        ]});
        mb::save_search(
            &cache,
            "recording",
            "isrc:(JPB602601767 OR JPB602601768)",
            &body.to_string(),
        );
        let tracks = [
            local(1, &[("album", "Messiah"), ("isrc", "JP-B60-26-01767")]),
            local(2, &[("album", "Messiah"), ("isrc", "jpb602601768")]),
        ];
        let s = tag_candidates(searching(&cache), &tracks, None).unwrap();
        assert_eq!(s.value.method, Method::Isrc);
        let found: Vec<(u32, &str)> = s
            .value
            .found
            .iter()
            .map(|(score, c)| (*score, c.title.as_str()))
            .collect();
        assert_eq!(found, [(100, "メシア"), (50, "Best")]);
    }

    #[test]
    fn an_artist_credited_by_another_name_is_looked_up() {
        let cache = LookupCache::in_memory();
        let tracks = [local(
            1,
            &[("album", "Clover"), ("album_artist", "Ushio Reira")],
        )];
        // The release credits 汐れいら, so searching by "Ushio Reira" finds
        // nothing good.
        let poor = serde_json::json!({"releases": [found("e5555555-5555-4555-8555-555555555555", "Clover Field", 40, 1, "")]});
        mb::save_search(
            &cache,
            "release",
            r#"release:"Clover" AND artist:"Ushio Reira""#,
            &poor.to_string(),
        );
        let artists = serde_json::json!({"artists": [
            {"id": "c9c4b066-7ac7-455a-aff5-b4684f769876", "name": "汐れいら", "score": 100},
            {"id": "a2b6c01e-6b23-4a55-ba3f-52851b1d2664", "name": "汐れいら", "score": 96},
            {"id": "e807d2cf-0b93-4ba1-ba89-f62347719dfd", "name": "REIRA", "score": 49},
        ]});
        mb::save_search(
            &cache,
            "artist",
            r#"artist:"Ushio Reira" OR sortname:"Ushio Reira" OR alias:"Ushio Reira""#,
            &artists.to_string(),
        );
        let clover = serde_json::json!({"releases": [found("f6666666-6666-4666-8666-666666666666", "Clover", 100, 1, "2026-09-02")]});
        mb::save_search(
            &cache,
            "release",
            r#"release:"Clover" AND arid:("c9c4b066-7ac7-455a-aff5-b4684f769876" OR "a2b6c01e-6b23-4a55-ba3f-52851b1d2664")"#,
            &clover.to_string(),
        );
        let s = tag_candidates(searching(&cache), &tracks, None).unwrap();
        assert_eq!(s.value.method, Method::Tags);
        assert_eq!(s.value.by, ["title and artist"]);
        let ids: Vec<&str> = s.value.found.iter().map(|(_, c)| c.id.as_str()).collect();
        assert_eq!(ids, ["f6666666-6666-4666-8666-666666666666"]);

        // An artist MusicBrainz knows by the same name: nothing more to ask
        // (it would fail, as nothing is saved for it).
        let tracks = [local(
            1,
            &[("album", "Clover"), ("album_artist", "汐れいら")],
        )];
        mb::save_search(
            &cache,
            "release",
            r#"release:"Clover" AND artist:"汐れいら""#,
            &poor.to_string(),
        );
        mb::save_search(
            &cache,
            "artist",
            r#"artist:"汐れいら" OR sortname:"汐れいら" OR alias:"汐れいら""#,
            &serde_json::json!({"artists": [{"id": "c9c4b066-7ac7-455a-aff5-b4684f769876", "name": "汐れいら", "score": 100}]}).to_string(),
        );
        let s = tag_candidates(searching(&cache), &tracks, None).unwrap();
        assert!(s.value.found.is_empty());
    }

    fn release() -> Release {
        serde_json::from_str(
            r#"{
            "id": "8bd5c2c7-af25-43d0-933c-1faf685204a6", "title": "Thaw", "date": "2015-03-29",
            "country": "XW", "barcode": "", "status": "Official",
            "artist-credit": [{"name": "4ikai", "joinphrase": "", "artist": {"id": "445bdfac-1d9f-44a9-afd4-2242a0cae45a", "sort-name": "Aikai"}}],
            "release-group": {"id": "b65ecc8f-ee13-4270-abbe-b47654d8ce73", "primary-type": "Single", "secondary-types": [],
                "first-release-date": "2015-03-29", "genres": []},
            "label-info": [{"catalog-number": null, "label": {"name": "[no label]"}}],
            "genres": [{"name": "ambient", "count": 2}, {"name": "drum and bass", "count": 3}],
            "text-representation": {"script": "Latn", "language": "eng"},
            "media": [{"position": 1, "format": "Digital Media", "track-count": 2, "tracks": [
                {"id": "t1", "position": 1, "title": "Thaw", "length": 237000,
                 "artist-credit": [{"name": "4ikai", "joinphrase": "", "artist": {"id": "445bdfac-1d9f-44a9-afd4-2242a0cae45a", "sort-name": "Aikai"}}],
                 "recording": {"id": "rec-1", "isrcs": []}},
                {"id": "t2", "position": 2, "title": "Don’t Look Back", "length": 200000,
                 "recording": {"id": "rec-2", "isrcs": ["SEABC1500001"]}}
            ]}]
        }"#,
        )
        .unwrap()
    }

    fn find<'a>(t: &'a TrackSuggestions, kind: &str) -> Option<&'a Suggestion> {
        t.suggestions.iter().find(|s| s.kind == kind)
    }

    #[test]
    fn comparisons_ignore_formatting() {
        assert!(!differs("title", "Don't Look Back", "Don’t Look Back"));
        assert!(!differs("title", "Wait...", "Wait…"));
        assert!(differs("title", "thaw", "Thaw"));
        assert!(!differs(
            "artist",
            "Nishikino Maki(CV. Pile)",
            "Nishikino Maki(CV.Pile)"
        ));
        assert!(!differs("track_number", "01", "1"));
        assert!(!differs("track_number", "3/12", "3"));
        assert!(!differs("date", "2015", "2015-03-29"));
        assert!(differs("date", "2014", "2015-03-29"));
        assert!(differs("date", "soon", "2015"));
        assert_eq!(genre_case("drum and bass"), "Drum and Bass");
        assert_eq!(genre_case("j-pop"), "J-Pop");
        assert_eq!(
            format_summary(["CD", "CD", "DVD"].into_iter().map(Some)).as_deref(),
            Some("2×CD + DVD")
        );
    }

    #[test]
    fn several_values_are_joined_by_the_separator() {
        let mut r = release();
        r.artist_credit = serde_json::from_str(
            r#"[{"name": "4ikai", "joinphrase": " & ", "artist": {"id": "a-1", "sort-name": "Aikai"}},
                {"name": "Friend", "joinphrase": "", "artist": {"id": "a-2", "sort-name": "Friend"}}]"#,
        )
        .unwrap();
        let group = r.release_group.as_mut().unwrap();
        group.secondary_types = vec!["Soundtrack".into(), "Live".into()];
        let values = release_values(&r, " / ");
        let get = |k: &str| {
            values
                .iter()
                .find(|(kind, _)| *kind == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("album_artist"), Some("4ikai & Friend"));
        assert_eq!(get("album_artists"), Some("4ikai / Friend"));
        assert_eq!(get("mb_release_artist_id"), Some("a-1 / a-2"));
        assert_eq!(get("release_type"), Some("single / soundtrack / live"));
        // The most voted first.
        assert_eq!(get("genre"), Some("Drum and Bass / Ambient"));

        // The second track has no credit of its own: the release's applies.
        let medium = &r.media[0];
        let values = track_values(&r, medium, &medium.tracks[1], None, ";");
        let get = |k: &str| {
            values
                .iter()
                .find(|(kind, _)| *kind == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("artist"), Some("4ikai & Friend"));
        assert_eq!(get("artists"), Some("4ikai;Friend"));
        assert_eq!(get("mb_artist_id"), Some("a-1;a-2"));
        assert_eq!(
            joined(["a".into(), " ".into(), "a".into(), "b".into()], ";").as_deref(),
            Some("a;b")
        );
        assert_eq!(joined([], ";"), None);
    }

    #[test]
    fn fills_empty_tags_and_suggests_changes() {
        let mut first = local(
            10,
            &[
                ("title", "Thaw (Original Mix)"),
                ("artist", "4ikai"),
                ("album", "Thaw"),
                ("date", "2015"),
                ("genre", "Electronic"),
                ("track_number", "1"),
            ],
        );
        first
            .recordings
            .push(("rec-1".into(), "acoustid-1".into(), 0.98));
        let second = local(11, &[("title", "Don't look back"), ("track_number", "2")]);
        let out = suggestions(&[first, second], &release(), None, ";");

        let t = &out[0];
        assert_eq!(t.via, Some(Via::Acoustid));
        assert_eq!(t.position.as_deref(), Some("1"));
        let title = find(t, "title").unwrap();
        assert_eq!(title.current.as_deref(), Some("Thaw (Original Mix)"));
        assert_eq!(title.value, "Thaw");
        // Already right, or right enough: not suggested.
        assert!(find(t, "artist").is_none());
        assert!(find(t, "album").is_none());
        assert!(find(t, "date").is_none());
        // Genre is the user's call once set.
        assert!(find(t, "genre").is_none());
        // Empty tags get MusicBrainz's values.
        assert_eq!(find(t, "album_artist").unwrap().value, "4ikai");
        assert_eq!(find(t, "album_artist").unwrap().current, None);
        assert_eq!(find(t, "track_total").unwrap().value, "2");
        assert_eq!(find(t, "mb_recording_id").unwrap().value, "rec-1");
        assert_eq!(find(t, "acoustid").unwrap().value, "acoustid-1");
        assert_eq!(find(t, "release_type").unwrap().value, "single");
        assert_eq!(find(t, "artist_sort").unwrap().value, "Aikai");
        // "[no label]" and an empty barcode aren't values.
        assert!(find(t, "label").is_none());
        assert!(find(t, "barcode").is_none());

        // Paired by its track number; the title differs only in case.
        let t = &out[1];
        assert_eq!(t.via, Some(Via::Position));
        assert_eq!(find(t, "title").unwrap().value, "Don’t Look Back");
        assert_eq!(find(t, "genre").unwrap().value, "Drum and Bass;Ambient");
        // No track credit: the release's.
        assert_eq!(find(t, "artist").unwrap().value, "4ikai");
        assert_eq!(find(t, "isrc").unwrap().value, "SEABC1500001");
        assert!(find(t, "acoustid").is_none());
    }

    #[test]
    fn romanized_suggestions_say_what_they_replace() {
        let romanized = release();
        let mut original = release();
        original.title = "雪解け".into();
        original.media[0].tracks[0].title = "雪解け".into();
        original.media[0].tracks[1].title = "振り返らない".into();
        // The second title has no romanization: it stays in Japanese.
        let mut romanized_partly = romanized.clone();
        romanized_partly.media[0].tracks[0].title = "雪解け".into();

        let mut first = local(
            1,
            &[
                ("album", "雪解け "),
                ("title", "Thaw"),
                ("track_number", "1"),
            ],
        );
        first.recordings.push(("rec-1".into(), "a".into(), 1.0));
        let second = local(
            2,
            &[
                ("album", "Thaw"),
                ("title", "Don't Look Back"),
                ("track_number", "2"),
            ],
        );
        let out = suggestions(&[first, second], &romanized_partly, Some(&original), ";");

        // The tag is the original-script value: a pure romanization.
        let album = find(&out[0], "album").unwrap();
        assert_eq!(album.value, "Thaw");
        assert_eq!(album.original.as_deref(), Some("雪解け"));
        assert!(album.romanizes);
        // A tag that sorts A–Z is never traded for one that doesn't.
        assert!(find(&out[0], "title").is_none());
        // Already romanized, typographic apostrophe aside.
        assert!(find(&out[1], "album").is_none());
        assert!(find(&out[1], "title").is_none());
    }

    #[test]
    fn sort_names_follow_their_artist() {
        let mut ours = local(1, &[("artist", "Someone Else"), ("track_number", "1")]);
        ours.recordings.push(("rec-1".into(), "a".into(), 1.0));
        let theirs = local(2, &[("track_number", "2")]);
        let out = suggestions(&[ours, theirs], &release(), None, ";");
        // The artist differs: no sort name for an artist the track may keep.
        assert!(find(&out[0], "artist").is_some());
        assert!(find(&out[0], "artist_sort").is_none());
        // Empty artist: both come from MusicBrainz.
        assert_eq!(find(&out[1], "artist_sort").unwrap().value, "Aikai");
        assert_eq!(find(&out[1], "album_artist_sort").unwrap().value, "Aikai");
    }

    #[test]
    fn numbers_only_pair_on_a_release_shaped_like_the_album() {
        let tracks = [
            local(1, &[("title", "Intro"), ("track_number", "1")]),
            local(2, &[("title", "Outro"), ("track_number", "2")]),
            local(3, &[("title", "Bonus"), ("track_number", "3")]),
        ];
        let out = suggestions(&tracks, &release(), None, ";");
        assert!(out.iter().all(|t| t.via.is_none()));
        assert!(find(&out[0], "title").is_none());
    }

    #[test]
    fn pairs_by_title_and_leaves_strays_with_album_values() {
        let tracks = [
            local(1, &[("title", "don't look back")]),
            local(2, &[("title", "Bonus")]),
        ];
        let out = suggestions(&tracks, &release(), None, ";");
        assert_eq!(out[0].via, Some(Via::Title));
        assert_eq!(out[0].position.as_deref(), Some("2"));
        assert_eq!(out[1].via, None);
        assert!(find(&out[1], "title").is_none());
        assert_eq!(find(&out[1], "album").unwrap().value, "Thaw");
    }

    #[test]
    fn acoustid_picks_the_release_most_tracks_are_on() {
        let lookup: Vec<AcoustResult> = serde_json::from_str(
            r#"[{"id": "a1", "score": 0.97, "recordings": [{"id": "rec-1", "releases": [
                {"id": "single", "title": "Thaw", "track_count": 2, "mediums": [{"position": 1, "format": "Digital Media", "tracks": [{"position": 1}]}]},
                {"id": "best-of", "title": "Best Of", "track_count": 20, "mediums": [{"position": 1, "format": "CD", "tracks": [{"position": 7}]}]}
            ]}]}]"#,
        )
        .unwrap();
        let second: Vec<AcoustResult> = serde_json::from_str(
            r#"[{"id": "a2", "score": 0.9, "recordings": [{"id": "rec-2", "releases": [
                {"id": "single", "title": "Thaw", "track_count": 2, "mediums": []}
            ]}]}]"#,
        )
        .unwrap();
        let tracks = [local(1, &[("album", "Thaw")]), local(2, &[])];
        let (candidates, trusted) = audio_candidates(&tracks, &[lookup, second]);
        assert!(trusted);
        assert_eq!(candidates[0].id, "single");
        assert_eq!(candidates[0].matched, 2);
        assert_eq!(candidates[1].id, "best-of");
        assert_eq!(candidates[1].format.as_deref(), Some("CD"));

        let (none, trusted) = audio_candidates(&tracks, &[Vec::new(), Vec::new()]);
        assert!(none.is_empty() && !trusted);
    }

    #[test]
    fn equal_releases_rank_the_same_every_time() {
        // Two releases every track is on: their sums of scores must come out
        // exactly equal, so the stable order picks, not rounding.
        let n = 60;
        let results: Vec<Vec<AcoustResult>> = (0..n)
            .map(|i| {
                let score = 0.5 + f64::from(i * 37 % 50) / 101.0;
                serde_json::from_str(&format!(
                    r#"[{{"id": "a{i}", "score": {score}, "recordings": [{{"id": "rec-{i}", "releases": [
                        {{"id": "b", "title": "Album", "track_count": {n}}},
                        {{"id": "a", "title": "Album", "track_count": {n}}}
                    ]}}]}}]"#
                ))
                .unwrap()
            })
            .collect();
        let tracks: Vec<Local> = (0..n).map(|i| local(i.into(), &[])).collect();
        for _ in 0..20 {
            let (candidates, _) = audio_candidates(&tracks, &results);
            assert_eq!(candidates[0].id, "a");
        }
    }

    #[test]
    fn equal_releases_prefer_a_dated_one() {
        let lookup: Vec<AcoustResult> = serde_json::from_str(
            r#"[{"id": "a1", "score": 1.0, "recordings": [{"id": "rec-1", "releases": [
                {"id": "pseudo", "title": "Nichijou Character song", "track_count": 1},
                {"id": "cd", "title": "日常のキャラクターソング", "track_count": 1, "date": {"year": 2011, "month": 10, "day": 26}}
            ]}]}]"#,
        )
        .unwrap();
        let (candidates, _) = audio_candidates(&[local(1, &[])], &[lookup]);
        assert_eq!(candidates[0].id, "cd");
        assert_eq!(candidates[0].date.as_deref(), Some("2011-10-26"));
        assert_eq!(
            title_key("Made in Abyss: Original Soundtrack!"),
            title_key("MADE IN ABYSS ORIGINAL SOUNDTRACK")
        );
    }
}
