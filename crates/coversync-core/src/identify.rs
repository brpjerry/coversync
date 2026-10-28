//! Smart suggestions: identify an Inbox album on MusicBrainz, by its audio
//! (Chromaprint fingerprints looked up on AcoustID) where possible and by its
//! tags otherwise, then propose MusicBrainz's values for its tags: changes
//! where a tag differs, and values for tags that are empty.
//!
//! `identify` does the slow part once (reading, fingerprinting, lookups) and
//! keeps it as `Evidence`, so `suggest` can switch to another release with a
//! single MusicBrainz request.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use lofty::tag::TagType;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

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
    /// A MusicBrainz search by the album's title and artist.
    Tags,
    /// Picked from the candidates by hand.
    Chosen,
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
    /// Releases fetched so far, so switching back and forth is quick.
    releases: Mutex<HashMap<String, Arc<Release>>>,
}

impl Evidence {
    fn release(&self, id: &str) -> Result<Arc<Release>> {
        let cached = self
            .releases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned();
        if let Some(r) = cached {
            return Ok(r);
        }
        let release = match mb::release(id) {
            Err(Error::NotFound) => {
                return Err(Error::Remote(format!(
                    "MusicBrainz has no release {id} any more."
                )));
            }
            r => Arc::new(r?),
        };
        self.releases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.to_string(), release.clone());
        Ok(release)
    }

    /// A Latin-script tracklist of `release`: a linked one, else one among
    /// the candidates sharing its recordings.
    fn tracklist(&self, release: &Release) -> Option<Arc<Release>> {
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
                .release(id)
                .inspect_err(|e| tracing::warn!("tracklist {id}: {e}"))
                .ok()?;
            romanize::is_tracklist_for(&list, release).then_some(list)
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
    /// Local track index -> best match score.
    scores: HashMap<usize, f64>,
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
                        scores: HashMap::new(),
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

/// Releases found by searching for the album's title and artist, best
/// first, with MusicBrainz's score for each (100: title and artist exact).
fn tag_candidates(tracks: &[Local]) -> Result<Vec<(u32, Candidate)>> {
    let Some(album) = consensus(tracks, "album") else {
        return Ok(Vec::new());
    };
    let artist = consensus(tracks, "album_artist").or_else(|| consensus(tracks, "artist"));
    let n = tracks.len() as u32;
    let year_now = consensus(tracks, "date").and_then(year);
    let mut found: Vec<(i64, (u32, Candidate))> = mb::search_releases(album, artist)?
        .into_iter()
        .filter(|r| r.score.unwrap_or(0) >= MIN_SEARCH_SCORE)
        .map(|r| {
            let score = r.score.unwrap_or(0);
            let mut rank = i64::from(score);
            if r.track_count == Some(n) {
                rank += 50;
            }
            if year_now.is_some() && r.date.as_deref().and_then(year) == year_now {
                rank += 10;
            }
            let candidate = Candidate {
                artist: Some(mb::credit_name(&r.artist_credit)).filter(|a| !a.is_empty()),
                format: format_summary(r.media.iter().map(|m| m.format.as_deref())),
                date: r.date.filter(|d| !d.is_empty()),
                country: r.country,
                track_count: r.track_count,
                matched: 0,
                title: r.title,
                id: r.id,
            };
            (rank, (score, candidate))
        })
        .collect();
    found.sort_by_key(|a| std::cmp::Reverse(a.0));
    Ok(found
        .into_iter()
        .map(|(_, c)| c)
        .take(MAX_CANDIDATES)
        .collect())
}

/// Read, fingerprint and look up `album_id`, then suggest tags from the
/// release that fits best. `key` is the AcoustID API key; without it (or
/// fpcalc) the album is matched by its tags alone.
pub fn identify(
    lib: &Library,
    album_id: &str,
    key: Option<&str>,
    romanize: Option<romanize::Options>,
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
            for ((i, _, _), found) in sent.iter().zip(mb::lookup(key, &prints)?) {
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
        let tagged = tag_candidates(&tracks)?;
        let exact = tagged
            .iter()
            .position(|(score, c)| *score == 100 && complete(c));
        if !trusted && !tagged.is_empty() {
            if fingerprinted > 0 {
                notes.push(if recognized == 0 {
                    "AcoustID doesn't know these recordings, so the album was matched by its title and artist.".into()
                } else {
                    format!("AcoustID recognized only {recognized} of {n} tracks, and they don't agree on a release, so the album was matched by its title and artist.")
                });
            }
            method = Method::Tags;
            let audio = std::mem::take(&mut candidates);
            candidates = tagged.into_iter().map(|(_, c)| c).chain(audio).collect();
        } else if let (true, Some(i)) = (trusted, exact) {
            let bigger = candidates
                .first()
                .map(|c| c.title.clone())
                .unwrap_or_default();
            notes.push(format!("The recordings are also on “{bigger}”, a release with more tracks; this one matches the album's title, artist and track count."));
            method = Method::Tags;
            let mut tagged: Vec<Candidate> = tagged.into_iter().map(|(_, c)| c).collect();
            let picked = tagged.remove(i);
            let audio = std::mem::take(&mut candidates);
            candidates = std::iter::once(picked).chain(audio).chain(tagged).collect();
        } else {
            candidates.extend(tagged.into_iter().map(|(_, c)| c));
        }
    }
    if candidates.is_empty() {
        notes.push(if recognized > 0 {
            "AcoustID knows these recordings, but not on any release.".into()
        } else if fingerprinted > 0 {
            "Neither AcoustID nor a MusicBrainz search by title found this album.".into()
        } else {
            "A MusicBrainz search by title found nothing.".into()
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
        releases: Mutex::default(),
    };
    let result = match evidence.candidates.first() {
        Some(best) => suggest(&evidence, &best.id.clone(), false, romanize, step)?,
        None => Identification {
            method,
            release: None,
            candidates: Vec::new(),
            fingerprinted,
            recognized,
            tracks: Vec::new(),
            notes: evidence.notes.clone(),
            romanization: None,
        },
    };
    Ok((evidence, result))
}

/// Suggest tags from release `release_id`; `chosen` when picked by hand.
/// With `romanize`, titles and names that don't sort A–Z are suggested in
/// Latin script where MusicBrainz has them, written as its options say.
pub fn suggest(
    evidence: &Evidence,
    release_id: &str,
    chosen: bool,
    romanize: Option<romanize::Options>,
    step: &(dyn Fn(Step) + Sync),
) -> Result<Identification> {
    step(Step {
        stage: Stage::Release,
        done: 0,
        total: 1,
    });
    let release = evidence.release(release_id)?;
    let (shown, romanization) = if !romanize::needs_romanizing(&release) {
        (None, None)
    } else if let Some(options) = romanize {
        step(Step {
            stage: Stage::Romanizing,
            done: 0,
            total: 1,
        });
        let list = evidence.tracklist(&release);
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
            Some(romanized) => suggestions(&evidence.tracks, romanized, Some(&release)),
            None => suggestions(&evidence.tracks, &release, None),
        },
        notes: evidence.notes.clone(),
        romanization,
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

fn push(out: &mut Vec<(&'static str, String)>, kind: &'static str, value: Option<String>) {
    if let Some(v) = value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    {
        out.push((kind, v));
    }
}

/// Values every track of the release shares.
fn release_values(r: &Release) -> Vec<(&'static str, String)> {
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
    // The most voted; ties go to the first listed.
    let top = genres.iter().rev().max_by_key(|g| g.count);
    push(&mut out, "genre", top.map(|g| genre_case(&g.name)));
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
            Some(
                std::iter::once(primary)
                    .chain(rest)
                    .collect::<Vec<_>>()
                    .join("; "),
            )
        }),
    );
    push(
        &mut out,
        "disc_total",
        (!r.media.is_empty()).then(|| r.media.len().to_string()),
    );
    push(&mut out, "mb_release_id", Some(r.id.clone()));
    push(&mut out, "mb_release_group_id", rg.map(|g| g.id.clone()));
    let first = r.artist_credit.first().map(|c| &c.artist.id);
    push(&mut out, "mb_release_artist_id", first.cloned());
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

/// Values for one track of the release.
fn track_values(
    r: &Release,
    medium: &Medium,
    track: &mb::Track,
    acoustid: Option<&str>,
) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    let credit = if track.artist_credit.is_empty() {
        &r.artist_credit
    } else {
        &track.artist_credit
    };
    push(&mut out, "title", Some(track.title.clone()));
    push(&mut out, "artist", Some(mb::credit_name(credit)));
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
        credit.first().map(|c| c.artist.id.clone()),
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
) -> Vec<TrackSuggestions> {
    let shared = release_values(release);
    let shared_original = original.map(release_values).unwrap_or_default();
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
                values.extend(track_values(release, medium, track, acoustid));
                if let Some(o) = original {
                    let medium = &o.media[m];
                    originals.extend(track_values(o, medium, &medium.tracks[t], acoustid));
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
        let out = suggestions(&[first, second], &release(), None);

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
        assert_eq!(find(t, "genre").unwrap().value, "Drum and Bass");
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
        let out = suggestions(&[first, second], &romanized_partly, Some(&original));

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
        let out = suggestions(&[ours, theirs], &release(), None);
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
        let out = suggestions(&tracks, &release(), None);
        assert!(out.iter().all(|t| t.via.is_none()));
        assert!(find(&out[0], "title").is_none());
    }

    #[test]
    fn pairs_by_title_and_leaves_strays_with_album_values() {
        let tracks = [
            local(1, &[("title", "don't look back")]),
            local(2, &[("title", "Bonus")]),
        ];
        let out = suggestions(&tracks, &release(), None);
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
