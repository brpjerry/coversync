//! Smart suggestions as background jobs. Identifying an album takes a few
//! seconds (fingerprinting, rate-limited web lookups), so the import dialog
//! starts a job and polls it for progress and the result. The lookups'
//! answers are saved (unless Inbox setup's Save lookups is off), so
//! identifying the album again only fingerprints it.
//! MusicBrainz's cover art is downloaded when the dialog asks for it, and
//! cached.

use std::collections::HashMap;
use std::fs;
use std::path::{Path as FsPath, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{Path, Query, Request, State};
use axum::response::Response;
use coversync_core::art::{self, Art};
use coversync_core::identify::{self, Evidence, Identification, Stage, Step};
use coversync_core::musicbrainz::CoverFrom;
use coversync_core::{musicbrainz, romanize};
use serde::{Deserialize, Serialize};

use crate::api::{ApiResult, blocking, serve_file};
use crate::events::Notice;
use crate::{AppState, Shared};

/// Finished identifications are reused for this long.
const KEEP: Duration = Duration::from_secs(30 * 60);

/// Identification jobs by album id.
#[derive(Default)]
pub struct Jobs(Mutex<HashMap<String, Job>>);

impl Jobs {
    fn lock(&self) -> MutexGuard<'_, HashMap<String, Job>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Forget every result, e.g. when the AcoustID key changes. A job still
    /// running finishes for the dialog waiting on it, but isn't reused.
    pub fn clear(&self) {
        self.lock().retain(|_, j| {
            j.stale = true;
            matches!(j.state, JobState::Running(_))
        });
    }

    pub fn forget(&self, album_id: &str) {
        self.lock().remove(album_id);
    }
}

struct Job {
    /// The release asked for; None for the best match.
    release: Option<String>,
    /// Romanized tags asked for (or not); None for Inbox setup's choice.
    romanize: Option<bool>,
    /// Tells a superseded job's thread not to overwrite its successor.
    generation: u64,
    /// Started under settings since changed: neither its result nor its
    /// evidence is reused.
    stale: bool,
    finished: Option<Instant>,
    state: JobState,
    /// Kept from the first identification so other releases come quickly.
    evidence: Option<Arc<Evidence>>,
}

enum JobState {
    Running(Step),
    Done(Box<Identification>),
    Failed(String),
}

#[derive(Serialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum Snapshot {
    Running { stage: Stage, done: u32, total: u32 },
    Done { result: Box<Identification> },
    Failed { error: String },
}

impl From<&Job> for Snapshot {
    fn from(job: &Job) -> Self {
        match &job.state {
            JobState::Running(s) => Snapshot::Running {
                stage: s.stage,
                done: s.done,
                total: s.total,
            },
            JobState::Done(r) => Snapshot::Done { result: r.clone() },
            JobState::Failed(e) => Snapshot::Failed { error: e.clone() },
        }
    }
}

#[derive(Deserialize)]
pub struct IdentifyRequest {
    album_id: String,
    /// Suggest from this release instead of the best match.
    #[serde(default)]
    release_id: Option<String>,
    /// Romanize titles and names that don't sort A–Z; None: as Inbox setup says.
    #[serde(default)]
    romanize: Option<bool>,
    /// Start over even if a recent result exists.
    #[serde(default)]
    refresh: bool,
    /// Ask AcoustID and MusicBrainz again rather than use their saved
    /// answers, and save the new ones; implies `refresh`.
    #[serde(default)]
    fresh: bool,
}

/// Start identifying an album, or report on the job already under way.
pub async fn start(
    State(state): State<Shared>,
    Json(req): Json<IdentifyRequest>,
) -> ApiResult<Json<Snapshot>> {
    if let Some(r) = req
        .release_id
        .as_deref()
        .filter(|r| !musicbrainz::is_mbid(r))
    {
        return Err(coversync_core::Error::Invalid(format!("'{r}' isn't a MusicBrainz id")).into());
    }
    let refresh = req.refresh || req.fresh;
    let mut jobs = state.identify.lock();
    let previous = jobs.get(&req.album_id);
    let reusable = previous.filter(|j| !refresh && !j.stale);
    if let Some(job) =
        reusable.filter(|j| j.release == req.release_id && j.romanize == req.romanize)
    {
        let fresh = match job.state {
            JobState::Running(_) => true,
            JobState::Done(_) => job.finished.is_some_and(|t| t.elapsed() < KEEP),
            JobState::Failed(_) => false,
        };
        if fresh {
            return Ok(Json(job.into()));
        }
    }
    let evidence = reusable.and_then(|j| j.evidence.clone());
    // Counted on from a stale job too, so it can't overwrite this one.
    let generation = previous.map_or(0, |j| j.generation + 1);
    let job = Job {
        release: req.release_id.clone(),
        romanize: req.romanize,
        generation,
        stale: false,
        finished: None,
        state: JobState::Running(Step {
            stage: Stage::Reading,
            done: 0,
            total: 0,
        }),
        evidence: evidence.clone(),
    };
    let snapshot = Snapshot::from(&job);
    jobs.insert(req.album_id.clone(), job);
    drop(jobs);

    let album_id = req.album_id;
    let release = req.release_id;
    let asked = req.romanize;
    let fresh = req.fresh;
    tokio::task::spawn_blocking(move || {
        let started = Instant::now();
        let progress = |step: Step| {
            update(&state, &album_id, generation, |job| {
                job.state = JobState::Running(step)
            });
        };
        let outcome = (|| -> coversync_core::Result<(Option<Evidence>, Identification)> {
            let setup = state.lib.inbox_setup()?.suggestions;
            let separator = state.lib.library_setup()?.rules.separator;
            let romanize = asked
                .unwrap_or(setup.prefer_english)
                .then_some(romanize::Options {
                    family_first: setup.family_name_first,
                });
            // Another release, or the same one romanized or not: the
            // evidence gathered already will do.
            let best = evidence
                .as_ref()
                .and_then(|ev| ev.candidates.first().map(|c| c.id.clone()));
            // Saved lookups are used and kept only while Inbox setup says so.
            let lookups = setup.save_lookups.then_some(&state.lookups);
            match (release.as_deref().or(best.as_deref()), evidence) {
                (Some(r), Some(ev)) => Ok((
                    None,
                    identify::suggest(
                        &ev,
                        lookups,
                        r,
                        release.is_some(),
                        romanize,
                        &separator,
                        &progress,
                    )?,
                )),
                _ => {
                    let key = state.lib.acoustid_key()?;
                    let client = musicbrainz::Client {
                        cache: lookups,
                        fresh,
                    };
                    let (ev, best) = identify::identify(
                        &state.lib,
                        client,
                        &album_id,
                        key.as_deref(),
                        romanize,
                        &separator,
                        &progress,
                    )?;
                    let result = match release.as_deref() {
                        Some(r) => identify::suggest(
                            &ev, lookups, r, true, romanize, &separator, &progress,
                        )?,
                        None => best,
                    };
                    Ok((Some(ev), result))
                }
            }
        })();
        match &outcome {
            Ok((_, r)) => tracing::info!(
                album = album_id,
                release = r.release.as_ref().map(|r| r.id.as_str()),
                method = ?r.method,
                saved = r.saved.is_some(),
                elapsed_ms = started.elapsed().as_millis() as u64,
                "identified"
            ),
            Err(e) => tracing::warn!(album = album_id, "identify: {e}"),
        }
        let finished = if outcome.is_ok() { "done" } else { "failed" };
        let current = update(&state, &album_id, generation, |job| {
            job.finished = Some(Instant::now());
            match outcome {
                Ok((ev, result)) => {
                    if let Some(ev) = ev {
                        job.evidence = Some(Arc::new(ev));
                    }
                    job.state = JobState::Done(Box::new(result));
                }
                Err(coversync_core::Error::NotFound) => {
                    job.state = JobState::Failed("This album isn't in the Inbox any more.".into())
                }
                Err(e) => job.state = JobState::Failed(e.to_string()),
            }
        });
        // A newer lookup for the album tells of its own end.
        if current {
            state.events.publish(Notice::LookupFinished {
                album_id,
                state: finished,
            });
        }
    });
    Ok(Json(snapshot))
}

/// Change the album's job, unless a newer one replaced it. Returns whether
/// the job was still the album's.
fn update(state: &Shared, album_id: &str, generation: u64, f: impl FnOnce(&mut Job)) -> bool {
    state
        .identify
        .lock()
        .get_mut(album_id)
        .filter(|j| j.generation == generation)
        .map(f)
        .is_some()
}

/// The album's identification job, as it stands.
pub async fn status(
    State(state): State<Shared>,
    Path(album_id): Path<String>,
) -> ApiResult<Json<Snapshot>> {
    let jobs = state.identify.lock();
    let job = jobs.get(&album_id).ok_or(coversync_core::Error::NotFound)?;
    Ok(Json(job.into()))
}

/// MusicBrainz's front cover for a release, as downloaded into the cache.
#[derive(Serialize, Deserialize)]
pub struct MbArt {
    width: u32,
    height: u32,
    /// The image file's extension: "jpg", "png" or "webp".
    extension: String,
    from: CoverFrom,
}

#[derive(Deserialize)]
pub struct ArtQuery {
    /// The release's group, whose cover stands in when the release has none.
    group: Option<String>,
    /// MusicBrainz says the release has a front cover of its own.
    #[serde(default = "yes")]
    front: bool,
}

fn yes() -> bool {
    true
}

/// One download at a time, so a cover asked for twice is fetched once.
static FETCHING: Mutex<()> = Mutex::new(());

fn check_mbid(id: &str) -> coversync_core::Result<()> {
    if musicbrainz::is_mbid(id) {
        Ok(())
    } else {
        Err(coversync_core::Error::Invalid(format!(
            "'{id}' isn't a MusicBrainz id"
        )))
    }
}

/// The cover cached for `release` and its image file. The description is
/// written after the image, so one always has the other.
fn cached(dir: &FsPath, release: &str) -> Option<(MbArt, PathBuf)> {
    let meta: MbArt =
        serde_json::from_slice(&fs::read(dir.join(format!("{release}.json"))).ok()?).ok()?;
    let image = dir.join(format!("{release}.{}", meta.extension));
    image.is_file().then_some((meta, image))
}

fn write_replacing(path: &FsPath, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)
        .and_then(|_| fs::rename(&tmp, path))
        .inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })
}

/// MusicBrainz's cover for `release`, downloaded unless it's cached; None
/// when the Cover Art Archive has none.
fn fetch_art(
    dir: &FsPath,
    release: &str,
    q: &ArtQuery,
) -> coversync_core::Result<Option<(MbArt, PathBuf)>> {
    let _one = FETCHING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(hit) = cached(dir, release) {
        return Ok(Some(hit));
    }
    let Some((bytes, from)) = musicbrainz::front_cover(
        q.front.then_some(release),
        q.group.as_deref(),
        art::MAX_BYTES,
    )?
    else {
        return Ok(None);
    };
    let art = Art::new(bytes).map_err(|_| {
        coversync_core::Error::Remote(
            "The Cover Art Archive's image isn't a JPEG, PNG or WebP.".into(),
        )
    })?;
    let meta = MbArt {
        width: art.width,
        height: art.height,
        extension: art.file_extension().to_string(),
        from,
    };
    fs::create_dir_all(dir)?;
    let image = dir.join(format!("{release}.{}", meta.extension));
    write_replacing(&image, &art.file_bytes()?)?;
    let json = serde_json::to_vec(&meta).expect("MbArt serializes");
    write_replacing(&dir.join(format!("{release}.json")), &json)?;
    Ok(Some((meta, image)))
}

/// Download MusicBrainz's front cover for a release (once) and describe it;
/// null when the Cover Art Archive has none.
pub async fn art(
    State(state): State<Shared>,
    Path(release): Path<String>,
    Query(q): Query<ArtQuery>,
) -> ApiResult<Json<Option<MbArt>>> {
    check_mbid(&release)?;
    if let Some(g) = &q.group {
        check_mbid(g)?;
    }
    blocking(&state, move |s| fetch_art(&s.mb_art, &release, &q))
        .await
        .map(|found| Json(found.map(|(meta, _)| meta)))
}

/// The cover image `art` downloaded.
pub async fn art_image(
    State(state): State<Shared>,
    Path(release): Path<String>,
    req: Request,
) -> ApiResult<Response> {
    check_mbid(&release)?;
    let (_, path) = cached(&state.mb_art, &release).ok_or(coversync_core::Error::NotFound)?;
    Ok(serve_file(path, req, "private, max-age=86400").await)
}

/// MusicBrainz's cover for `release`, as `art` downloaded it, for an import.
pub fn chosen_art(state: &AppState, release: &str) -> coversync_core::Result<Art> {
    check_mbid(release)?;
    let (_, path) = cached(&state.mb_art, release).ok_or_else(|| {
        coversync_core::Error::Invalid(
            "MusicBrainz's cover isn't downloaded any more; open the import again.".into(),
        )
    })?;
    Art::new(fs::read(path)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(state: JobState) -> Job {
        Job {
            release: None,
            romanize: None,
            generation: 3,
            stale: false,
            finished: None,
            state,
            evidence: None,
        }
    }

    #[test]
    fn clearing_keeps_running_jobs_but_marks_them_stale() {
        let jobs = Jobs::default();
        let running = JobState::Running(Step {
            stage: Stage::Reading,
            done: 0,
            total: 0,
        });
        jobs.lock().insert("running".into(), job(running));
        jobs.lock()
            .insert("failed".into(), job(JobState::Failed("x".into())));
        jobs.clear();
        let kept = jobs.lock();
        assert!(!kept.contains_key("failed"));
        // Still there for the dialog polling it, and for its generation.
        let running = &kept["running"];
        assert!(running.stale);
        assert_eq!(running.generation, 3);
    }
}
