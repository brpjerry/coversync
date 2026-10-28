//! Smart suggestions as background jobs. Identifying an album takes a few
//! seconds (fingerprinting, rate-limited web lookups), so the import dialog
//! starts a job and polls it for progress and the result.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{Path, State};
use coversync_core::identify::{self, Evidence, Identification, Stage, Step};
use coversync_core::{musicbrainz, romanize};
use serde::{Deserialize, Serialize};

use crate::Shared;
use crate::api::ApiResult;

/// Finished identifications are reused for this long.
const KEEP: Duration = Duration::from_secs(30 * 60);

/// Identification jobs by album id.
#[derive(Default)]
pub struct Jobs(Mutex<HashMap<String, Job>>);

impl Jobs {
    fn lock(&self) -> MutexGuard<'_, HashMap<String, Job>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Forget every result, e.g. when the AcoustID key changes.
    pub fn clear(&self) {
        self.lock()
            .retain(|_, j| matches!(j.state, JobState::Running(_)));
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
    let mut jobs = state.identify.lock();
    let previous = jobs.get(&req.album_id);
    if let Some(job) = previous
        .filter(|j| !req.refresh && j.release == req.release_id && j.romanize == req.romanize)
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
    let evidence = previous
        .filter(|_| !req.refresh)
        .and_then(|j| j.evidence.clone());
    let generation = previous.map_or(0, |j| j.generation + 1);
    let job = Job {
        release: req.release_id.clone(),
        romanize: req.romanize,
        generation,
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
    tokio::task::spawn_blocking(move || {
        let started = Instant::now();
        let progress = |step: Step| {
            update(&state, &album_id, generation, |job| {
                job.state = JobState::Running(step)
            })
        };
        let outcome = (|| -> coversync_core::Result<(Option<Evidence>, Identification)> {
            let setup = state.lib.inbox_setup()?.suggestions;
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
            match (release.as_deref().or(best.as_deref()), evidence) {
                (Some(r), Some(ev)) => Ok((
                    None,
                    identify::suggest(&ev, r, release.is_some(), romanize, &progress)?,
                )),
                _ => {
                    let key = state.lib.acoustid_key()?;
                    let (ev, best) = identify::identify(
                        &state.lib,
                        &album_id,
                        key.as_deref(),
                        romanize,
                        &progress,
                    )?;
                    let result = match release.as_deref() {
                        Some(r) => identify::suggest(&ev, r, true, romanize, &progress)?,
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
                elapsed_ms = started.elapsed().as_millis() as u64,
                "identified"
            ),
            Err(e) => tracing::warn!(album = album_id, "identify: {e}"),
        }
        update(&state, &album_id, generation, |job| {
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
    });
    Ok(Json(snapshot))
}

/// Change the album's job, unless a newer one replaced it.
fn update(state: &Shared, album_id: &str, generation: u64, f: impl FnOnce(&mut Job)) {
    if let Some(job) = state
        .identify
        .lock()
        .get_mut(album_id)
        .filter(|j| j.generation == generation)
    {
        f(job);
    }
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
