//! JSON API, cover thumbnails and audio streaming.

use std::collections::{HashMap, HashSet};
use std::path::{Path as FsPath, PathBuf};

use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use coversync_core::appearance::{self, AppearanceSetup};
use coversync_core::art::{self, Art};
use coversync_core::db::{CoverSource, FileStat, PreparedCopy};
use coversync_core::import::{self, ImportPlan, ImportResult};
use coversync_core::scan;
use coversync_core::scan::Progress;
use coversync_core::sync::SyncSetup;
use coversync_core::template::{self, Template};
use coversync_core::{
    AlbumDetail, Collection, HiddenStats, InboxSetup, IndexVersions, LibrarySetup, TagCoverage,
    TagEdit, inbox, relocate, rules, tags, upload,
};
use serde::{Deserialize, Serialize};
use tower::ServiceExt;
use tower_http::services::ServeFile;

use crate::events::{Notice, Setup};
use crate::{Shared, spawn_scan};

pub fn router() -> Router<Shared> {
    Router::new()
        .route("/api/status", get(status))
        .route("/api/events", get(crate::events::stream))
        .route("/api/rescan", post(rescan))
        .route("/api/albums", get(albums))
        .route("/api/albums/{id}", get(album))
        .route("/api/albums/{id}/cover", get(cover))
        .route("/api/albums/{id}/cover/full", get(full_cover))
        .route("/api/albums/{id}/tags", get(album_tags).put(write_tags))
        .route("/api/tracks/{id}/stream", get(stream))
        .route(
            "/api/library/setup",
            get(library_setup).put(save_library_setup),
        )
        .route("/api/inbox/coverage", get(inbox_coverage))
        .route("/api/inbox/setup", get(inbox_setup).put(save_inbox_setup))
        .route("/api/inbox/unhide", post(unhide_all))
        .route("/api/library/preview", get(structure_preview))
        .route("/api/library/restructure", post(restructure))
        .route("/api/fs/check", get(folder_check))
        .route("/api/import/plan", post(import_plan))
        .route(
            "/api/import",
            // Room for album art sent along (base64 grows it by a third).
            post(import_album).layer(DefaultBodyLimit::max(art::MAX_BYTES / 3 * 4 + (1 << 20))),
        )
        .route("/api/appearance", get(appearance).put(save_appearance))
        .route("/api/sync/setup", get(sync_setup).put(save_sync_setup))
        .route("/api/identify", post(crate::suggest::start))
        .route("/api/identify/{album_id}", get(crate::suggest::status))
        .route("/api/musicbrainz/art/{release}", get(crate::suggest::art))
        .route(
            "/api/musicbrainz/art/{release}/image",
            get(crate::suggest::art_image),
        )
}

#[derive(Debug)]
pub struct ApiError(pub StatusCode, pub String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}

impl From<coversync_core::Error> for ApiError {
    fn from(e: coversync_core::Error) -> Self {
        use coversync_core::Error::*;
        let code = match e {
            NotFound => StatusCode::NOT_FOUND,
            Tags(_) | Invalid(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Remote(_) => StatusCode::BAD_GATEWAY,
            ReadOnly(_) => StatusCode::FORBIDDEN,
            NoSpace(_) => StatusCode::INSUFFICIENT_STORAGE,
            Conflict(_) => StatusCode::CONFLICT,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        ApiError(code, e.to_string())
    }
}

impl From<tokio::task::JoinError> for ApiError {
    fn from(e: tokio::task::JoinError) -> Self {
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

/// Run blocking library work holding the jobs lock (see `AppState::jobs`),
/// in a task of its own: the work and the lock go on together until it's
/// done, even if the request is dropped (its client gone).
async fn exclusively<T: Send + 'static>(
    state: &Shared,
    f: impl FnOnce(&crate::AppState) -> coversync_core::Result<T> + Send + 'static,
) -> ApiResult<T> {
    let state = state.clone();
    tokio::spawn(async move {
        let _guard = state.jobs.lock().await;
        let result = blocking(&state, f).await;
        crate::spawn_hashes(state.clone());
        result
    })
    .await?
}

/// Run blocking library work off the async runtime.
pub(crate) async fn blocking<T: Send + 'static>(
    state: &Shared,
    f: impl FnOnce(&crate::AppState) -> coversync_core::Result<T> + Send + 'static,
) -> ApiResult<T> {
    let s = state.clone();
    Ok(tokio::task::spawn_blocking(move || f(&s)).await??)
}

#[derive(Serialize)]
struct Status {
    version: &'static str,
    scan: coversync_core::ScanProgress,
    hashes: coversync_core::hashes::HashProgress,
    /// `library`: the Library's id and change counter (null without a
    /// Library folder); `inbox`: the Inbox's counter.
    #[serde(flatten)]
    index: IndexVersions,
}

async fn status(State(state): State<Shared>) -> ApiResult<Json<Status>> {
    let scan = state
        .progress
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let index = blocking(&state, |s| s.lib.index_versions()).await?;
    Ok(Json(Status {
        version: env!("CARGO_PKG_VERSION"),
        scan,
        hashes: state
            .hashes
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone(),
        index,
    }))
}

async fn rescan(State(state): State<Shared>) -> StatusCode {
    spawn_scan(state);
    StatusCode::ACCEPTED
}

#[derive(Deserialize)]
struct AlbumsQuery {
    #[serde(default)]
    collection: Option<String>,
}

async fn albums(State(state): State<Shared>, Query(q): Query<AlbumsQuery>) -> ApiResult<Response> {
    let collection = Collection::parse(q.collection.as_deref().unwrap_or("inbox"));
    let albums = blocking(&state, move |s| {
        let appearance = s.lib.appearance()?;
        // Flag albums that couldn't be imported (or kept) as they are.
        let setup = s.lib.library_setup()?;
        let mut required = import::required_kinds(&setup);
        if setup.rules.art.required {
            required.push(art::ART);
        }
        // Autofill only applies to Inbox albums, the ones still to be imported.
        let autofill = (collection == Collection::Inbox)
            .then(|| s.lib.inbox_setup().map(|i| i.autofill))
            .transpose()?;
        s.lib.albums(
            collection,
            &appearance.extra_sort_keys(),
            &appearance.filter_keys(),
            &setup.rules.splitter(),
            &required,
            autofill.as_ref(),
        )
    })
    .await?;
    let mut res = Json(albums).into_response();
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    Ok(res)
}

async fn album(
    State(state): State<Shared>,
    Path(id): Path<String>,
) -> ApiResult<Json<AlbumDetail>> {
    blocking(&state, move |s| {
        s.lib.album(&id)?.ok_or(coversync_core::Error::NotFound)
    })
    .await
    .map(Json)
}

#[derive(Serialize)]
struct Variable {
    id: &'static str,
    label: &'static str,
    /// Tag group, as in the rule catalog; `{year}` counts as core.
    group: &'static str,
}

/// What a configured folder looks like on disk, as coversyncd sees it.
#[derive(Serialize)]
pub(crate) struct FolderStatus {
    path: String,
    exists: bool,
    dir: bool,
    writable: bool,
    /// Missing folders are created on first import (or upload) if this is true.
    creatable: bool,
    /// Tracks in the coversync Library this folder holds, if it holds one.
    library: Option<u64>,
}

pub(crate) fn folder_status(path: &str) -> FolderStatus {
    let p = std::path::Path::new(path);
    let meta = p.metadata().ok();
    FolderStatus {
        path: path.to_string(),
        exists: meta.is_some(),
        dir: meta.as_ref().is_some_and(|m| m.is_dir()),
        writable: import::writable(p),
        creatable: import::creatable(p),
        library: coversync_core::Library::library_in(p),
    }
}

/// [`folder_status`] for an uploads folder, which the first upload makes
/// only if the folder holding it is there (see `upload::check_parent`).
pub(crate) fn uploads_folder_status(path: &str) -> FolderStatus {
    FolderStatus {
        creatable: upload::can_write(std::path::Path::new(path)),
        ..folder_status(path)
    }
}

#[derive(Serialize)]
struct SetupResponse {
    setup: LibrarySetup,
    folder: Option<FolderStatus>,
    /// Saving opened a Library that has its own setup, which replaced the
    /// one sent.
    adopted: bool,
    /// Why the Library folder can't hold its database, if it can't.
    library_problem: Option<String>,
    groups: &'static [rules::TagGroup],
    kinds: &'static [rules::TagKind],
    essentials: &'static [&'static str],
    album_kinds: &'static [&'static str],
    not_editable: &'static [&'static str],
    suggestions: &'static [&'static str],
    variables: Vec<Variable>,
    art_file_names: &'static [&'static str],
}

fn setup_response(
    setup: LibrarySetup,
    adopted: bool,
    library_problem: Option<String>,
) -> SetupResponse {
    let mut variables = vec![Variable {
        id: template::YEAR,
        label: "Year (from date)",
        group: "core",
    }];
    variables.extend(rules::CATALOG.iter().map(|k| Variable {
        id: k.id,
        // The date tag is "Year" everywhere else; here {year} is the four digits and
        // {date} the tag as written, which may be a full date.
        label: if k.id == "date" {
            "Date, as tagged"
        } else {
            k.label
        },
        group: k.group,
    }));
    SetupResponse {
        folder: setup.folder.as_deref().map(folder_status),
        setup,
        adopted,
        library_problem,
        groups: rules::GROUPS,
        kinds: rules::CATALOG,
        essentials: rules::ESSENTIALS,
        album_kinds: rules::ALBUM_KINDS,
        not_editable: rules::NOT_EDITABLE,
        suggestions: template::SUGGESTIONS,
        variables,
        art_file_names: art::FILE_NAMES,
    }
}

async fn library_setup(State(state): State<Shared>) -> ApiResult<Json<SetupResponse>> {
    blocking(&state, |s| {
        Ok(setup_response(
            s.lib.library_setup()?,
            false,
            s.lib.library_problem()?,
        ))
    })
    .await
    .map(Json)
}

fn must_be_folder(path: Option<&str>, what: &str) -> coversync_core::Result<()> {
    if let Some(p) = path
        && std::path::Path::new(p).exists()
        && !std::path::Path::new(p).is_dir()
    {
        return Err(coversync_core::Error::Invalid(format!(
            "The {what} {p} is a file, not a folder."
        )));
    }
    Ok(())
}

/// Save Library setup. A new folder is indexed right away.
async fn save_library_setup(
    State(state): State<Shared>,
    Json(setup): Json<LibrarySetup>,
) -> ApiResult<Json<SetupResponse>> {
    // Switching the Library mustn't happen under a scan, import or tag write.
    save_exclusively(&state, Setup::Library, move |s| {
        let before = s.lib.library_setup()?;
        let setup = setup.normalize()?;
        must_be_folder(setup.folder.as_deref(), "library folder")?;
        let inbox_setup = s.lib.inbox_setup()?;
        inbox::check_folders(inbox_setup.folder.as_deref(), setup.folder.as_deref())?;
        inbox::check_uploads(
            inbox_setup.uploads.as_deref(),
            inbox_setup.folder.as_deref(),
            setup.folder.as_deref(),
        )?;
        let saved = s.lib.set_library_setup(setup.clone())?;
        // Suggestions joined, split or substituted the old way shouldn't linger.
        if before.rules.separator != saved.rules.separator
            || before.rules.split_at != saved.rules.split_at
            || before.rules.substitutions != saved.rules.substitutions
        {
            s.identify.clear();
        }
        let moved = before.folder != saved.folder;
        let adopted = saved != setup;
        Ok((
            setup_response(saved, adopted, s.lib.library_problem()?),
            moved,
        ))
    })
    .await
    .map(Json)
}

/// Run a setup save on a blocking thread, holding the jobs lock: not under
/// a scan, import or tag write, nor beside another save. It runs in a task
/// of its own, which holds the lock until the save is done, even if the
/// request is dropped (its client gone). Once it's saved, clients hear of
/// it (`setup_saved`); then a scan runs if the save moved a folder (the
/// `bool` it returns). A scan asked for meanwhile has been waiting for the
/// lock, and runs either way (see `spawn_scan`).
async fn save_exclusively<T: Send + 'static>(
    state: &Shared,
    setup: Setup,
    save: impl FnOnce(&crate::AppState) -> coversync_core::Result<(T, bool)> + Send + 'static,
) -> ApiResult<T> {
    let state = state.clone();
    tokio::spawn(async move {
        let guard = state.jobs.lock().await;
        let result = blocking(&state, save).await;
        drop(guard);
        if result.is_ok() {
            state.events.publish(Notice::SetupSaved { setup });
        }
        if result.as_ref().is_ok_and(|(_, moved)| *moved) {
            spawn_scan(state.clone());
        }
        result.map(|(saved, _)| saved)
    })
    .await?
}

async fn inbox_coverage(State(state): State<Shared>) -> ApiResult<Json<TagCoverage>> {
    blocking(&state, |s| {
        let autofill = s.lib.inbox_setup()?.autofill;
        s.lib.tag_coverage(Some(&autofill))
    })
    .await
    .map(Json)
}

#[derive(Serialize)]
struct InboxSetupResponse {
    setup: InboxSetup,
    folder: Option<FolderStatus>,
    /// Where uploads go: the uploads folder, or the Inbox folder's
    /// `Uploads`.
    uploads: Option<FolderStatus>,
    hidden: HiddenStats,
    suggest: SuggestStatus,
}

/// What smart suggestions have to work with. The key itself stays here.
#[derive(Serialize)]
struct SuggestStatus {
    /// The saved AcoustID key's last characters, to recognize it by.
    key_hint: Option<String>,
    /// The installed fpcalc's version.
    fpcalc: Option<String>,
}

fn inbox_response(
    s: &crate::AppState,
    setup: InboxSetup,
) -> coversync_core::Result<InboxSetupResponse> {
    let key = s.lib.acoustid_key()?;
    Ok(InboxSetupResponse {
        folder: setup.folder.as_deref().map(folder_status),
        uploads: setup
            .uploads_folder()
            .map(|f| uploads_folder_status(&f.display().to_string())),
        setup,
        hidden: s.lib.hidden_stats()?,
        suggest: SuggestStatus {
            key_hint: key.map(|k| {
                let tail: String = k.chars().rev().take(4).collect();
                tail.chars().rev().collect()
            }),
            fpcalc: coversync_core::identify::fpcalc_version(),
        },
    })
}

async fn inbox_setup(State(state): State<Shared>) -> ApiResult<Json<InboxSetupResponse>> {
    blocking(&state, |s| inbox_response(s, s.lib.inbox_setup()?))
        .await
        .map(Json)
}

#[derive(Deserialize)]
struct SaveInboxSetup {
    #[serde(flatten)]
    setup: InboxSetup,
    /// A new AcoustID API key; empty removes it, absent keeps it.
    #[serde(default)]
    acoustid_key: Option<String>,
}

/// A new Inbox folder must exist; changing it, or the uploads folder,
/// rescans. A new uploads folder cancels uploads under way into the old one.
/// Saves take turns with each other and with Library setup, scans, imports
/// and tag writes, so the folders checked are the folders saved, and the
/// uploads follow the setup last saved.
async fn save_inbox_setup(
    State(state): State<Shared>,
    Json(body): Json<SaveInboxSetup>,
) -> ApiResult<Json<InboxSetupResponse>> {
    let key = body
        .acoustid_key
        .as_deref()
        .map(inbox::acoustid_key)
        .transpose()?;
    let setup = body.setup;
    save_exclusively(&state, Setup::Inbox, move |s| {
        let before = s.lib.inbox_setup()?;
        let setup = setup.normalize()?;
        if let Some(f) = &setup.folder
            && !std::path::Path::new(f).is_dir()
        {
            return Err(coversync_core::Error::Invalid(format!(
                "The Inbox folder {f} doesn't exist or isn't a folder."
            )));
        }
        must_be_folder(setup.uploads.as_deref(), "uploads folder")?;
        if let Some(u) = &setup.uploads {
            upload::check_parent(std::path::Path::new(u))?;
        }
        let library = s.lib.library_setup()?.folder;
        inbox::check_folders(setup.folder.as_deref(), library.as_deref())?;
        inbox::check_uploads(
            setup.uploads.as_deref(),
            setup.folder.as_deref(),
            library.as_deref(),
        )?;
        if let Some(key) = key {
            s.lib.set_acoustid_key(key.as_deref())?;
        }
        let saved = s.lib.set_inbox_setup(setup)?;
        // Suggestions found with the old key (or settings) shouldn't linger.
        s.identify.clear();
        crate::uploads::follow_folder(s);
        let moved = before.folder != saved.folder || before.uploads != saved.uploads;
        Ok((inbox_response(s, saved)?, moved))
    })
    .await
    .map(Json)
}

#[derive(Serialize)]
struct UnhideResult {
    unhidden: u32,
    hidden: HiddenStats,
}

async fn unhide_all(State(state): State<Shared>) -> ApiResult<Json<UnhideResult>> {
    // Showing files again changes albums' tracks: not between a tag save's
    // version check and its write. Notices go out from the work itself,
    // which carries on if the request is dropped.
    exclusively(&state, |s| {
        let unhidden = s.lib.unhide_all()?;
        s.events.publish(Notice::SetupSaved {
            setup: Setup::Inbox,
        });
        Ok(UnhideResult {
            unhidden,
            hidden: s.lib.hidden_stats()?,
        })
    })
    .await
    .map(Json)
}

#[derive(Serialize)]
struct AppearanceResponse {
    setup: AppearanceSetup,
    defaults: &'static [&'static str],
    filter_defaults: &'static [&'static str],
    sortable: Vec<appearance::Sortable>,
    filterable: Vec<appearance::Sortable>,
    groups: Vec<appearance::SortGroup>,
    cover_sizes: appearance::CoverSizes,
}

fn appearance_response(setup: AppearanceSetup) -> Json<AppearanceResponse> {
    Json(AppearanceResponse {
        setup,
        defaults: appearance::DEFAULT_TABS,
        filter_defaults: appearance::DEFAULT_FILTERS,
        sortable: appearance::sortable(),
        filterable: appearance::filterable(),
        groups: appearance::groups(),
        cover_sizes: appearance::COVER_SIZES,
    })
}

async fn appearance(State(state): State<Shared>) -> ApiResult<Json<AppearanceResponse>> {
    blocking(&state, |s| s.lib.appearance())
        .await
        .map(appearance_response)
}

async fn save_appearance(
    State(state): State<Shared>,
    Json(setup): Json<AppearanceSetup>,
) -> ApiResult<Json<AppearanceResponse>> {
    blocking(&state, move |s| {
        let saved = s.lib.set_appearance(setup)?;
        s.events.publish(Notice::SetupSaved {
            setup: Setup::Appearance,
        });
        Ok(saved)
    })
    .await
    .map(appearance_response)
}

#[derive(Serialize)]
struct SyncSetupResponse {
    setup: SyncSetup,
}

async fn sync_setup(State(state): State<Shared>) -> ApiResult<Json<SyncSetupResponse>> {
    blocking(&state, |s| s.lib.sync_setup())
        .await
        .map(|setup| Json(SyncSetupResponse { setup }))
}

async fn save_sync_setup(
    State(state): State<Shared>,
    Json(setup): Json<SyncSetup>,
) -> ApiResult<Json<SyncSetupResponse>> {
    blocking(&state, move |s| {
        let setup = s.lib.set_sync_setup(setup)?;
        s.events.publish(Notice::SetupSaved { setup: Setup::Sync });
        Ok(SyncSetupResponse { setup })
    })
    .await
    .map(Json)
}

#[derive(Deserialize)]
struct FolderQuery {
    path: String,
    /// Checked as an uploads folder (see [`uploads_folder_status`]).
    #[serde(default)]
    uploads: bool,
}

/// Live feedback while a folder path is typed in setup.
async fn folder_check(
    State(state): State<Shared>,
    Query(q): Query<FolderQuery>,
) -> ApiResult<Json<Option<FolderStatus>>> {
    blocking(&state, move |_| {
        Ok(rules::folder_setting(Some(q.path), "folder")?
            .as_deref()
            .map(|p| {
                if q.uploads {
                    uploads_folder_status(p)
                } else {
                    folder_status(p)
                }
            }))
    })
    .await
    .map(Json)
}

#[derive(Deserialize)]
struct PreviewQuery {
    structure: String,
}

#[derive(Serialize)]
struct PreviewExample {
    from: String,
    to: Option<String>,
    missing: Vec<&'static str>,
}

#[derive(Serialize)]
struct Preview {
    error: Option<String>,
    /// Tag kinds the structure needs on every file.
    requires: Vec<&'static str>,
    examples: Vec<PreviewExample>,
}

/// Render a folder structure against real Inbox tracks, preferring an album
/// that has every variable.
async fn structure_preview(
    State(state): State<Shared>,
    Query(q): Query<PreviewQuery>,
) -> ApiResult<Json<Preview>> {
    blocking(&state, move |s| {
        let t = match Template::parse(&q.structure) {
            Ok(t) => t.with_separator(&s.lib.library_setup()?.rules.separator),
            Err(e) => {
                return Ok(Preview {
                    error: Some(e.to_string()),
                    requires: vec![],
                    examples: vec![],
                });
            }
        };
        // With length limits, albums with values a limit cuts come first, so
        // the preview shows what gets cut.
        let limits = t.limits();
        let overlong = if limits.is_empty() {
            Vec::new()
        } else {
            s.lib.overlong(&limits)?
        };
        let cut: std::collections::HashSet<i64> = overlong.iter().map(|(_, t)| *t).collect();
        let mut albums: Vec<String> = Vec::new();
        for (album, _) in &overlong {
            if albums.len() == 6 {
                break;
            }
            if !albums.contains(album) {
                albums.push(album.clone());
            }
        }
        for album in s
            .lib
            .albums(
                Collection::Inbox,
                &[],
                &[],
                &coversync_core::split::Splitter::default(),
                &[],
                None,
            )?
            .iter()
            .take(24)
        {
            if !albums.contains(&album.id) {
                albums.push(album.id.clone());
            }
        }
        let mut fallback = None;
        for album_id in &albums {
            let Some(mut detail) = s.lib.album(album_id)? else {
                continue;
            };
            // Shortened tracks first; otherwise in album order.
            detail.tracks.sort_by_key(|t| !cut.contains(&t.id));
            let mut examples = Vec::new();
            for track in detail.tracks.iter().take(3) {
                let Some(path) = s.lib.track_path(track.id)? else {
                    continue;
                };
                let values = tags::kind_values(&path)?;
                let ext = path
                    .extension()
                    .map(|e| e.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let (to, missing) = match t.render(&values, &ext) {
                    Ok(p) => (Some(p.display().to_string()), vec![]),
                    Err(m) => (None, m),
                };
                examples.push(PreviewExample {
                    from: track.rel_path.clone(),
                    to,
                    missing,
                });
            }
            if examples.iter().all(|e| e.to.is_some()) {
                return Ok(Preview {
                    error: None,
                    requires: t.required_kinds(),
                    examples,
                });
            }
            fallback.get_or_insert(examples);
        }
        Ok(Preview {
            error: None,
            requires: t.required_kinds(),
            examples: fallback.unwrap_or_default(),
        })
    })
    .await
    .map(Json)
}

#[derive(Deserialize)]
struct ImportRequest {
    album_id: String,
    #[serde(default)]
    edits: import::Edits,
    /// Album art chosen in the import editor, base64-encoded.
    #[serde(default)]
    art: Option<String>,
    /// Or MusicBrainz's cover for this release, as suggested.
    #[serde(default)]
    musicbrainz_art: Option<String>,
}

async fn import_plan(
    State(state): State<Shared>,
    Json(req): Json<ImportRequest>,
) -> ApiResult<Json<ImportPlan>> {
    blocking(&state, move |s| {
        import::plan(&s.lib, &req.album_id, &req.edits, None)
    })
    .await
    .map(Json)
}

/// Import one album. Runs exclusively with scans and tag writes.
async fn import_album(
    State(state): State<Shared>,
    Json(req): Json<ImportRequest>,
) -> ApiResult<Json<ImportResult>> {
    // Under the jobs lock, in a task of its own: a scan asked for meanwhile
    // waits for the whole import, even if the request is dropped.
    let result = exclusively(&state, move |s| {
        // Not onto a path a track an earlier save moved has to come back to
        // (nor while the tag editor reads a file there). The jobs lock keeps
        // a save from recording moves meanwhile.
        if !s.lib.recorded_moves()?.is_empty() {
            let _files = s.tag_files.write().unwrap_or_else(|e| e.into_inner());
            moves_put_back(s, "imported")?;
        }
        // Keep import bookkeeping out of the scan progress the UI shows.
        let progress = Progress::default();
        let art = req
            .art
            .map(|b64| {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(b64.trim())
                    .map_err(|_| {
                        coversync_core::Error::Invalid("The album art didn't arrive intact.".into())
                    })?;
                Art::new(bytes)
            })
            .transpose()?;
        let art = match (art, &req.musicbrainz_art) {
            (None, Some(release)) => Some(crate::suggest::chosen_art(s, release)?),
            (art, _) => art,
        };
        let imported =
            import::execute(&s.lib, &s.covers, &req.album_id, &req.edits, art, &progress)?;
        s.identify.forget(&req.album_id);
        s.events.publish(Notice::Imported {
            album_id: imported.album_id.clone(),
            inbox_id: req.album_id,
        });
        Ok(imported)
    })
    .await;
    crate::release_memory();
    result.map(Json)
}

#[derive(Deserialize)]
struct CoverQuery {
    size: Option<u32>,
    /// The album's `cover_version`, which changes whenever its art does.
    v: Option<String>,
}

/// How long a thumbnail may be kept. One served for the `cover_version` its
/// URL names is that rendering of the art for good; any other (no version, or
/// one the art has since moved on from) has to be checked every time. Private:
/// it was served to a login, so only the browser keeps it.
fn cover_caching(current: bool) -> &'static str {
    if current {
        "private, max-age=31536000, immutable"
    } else {
        "no-cache"
    }
}

pub(crate) async fn serve_file(path: PathBuf, req: Request, cache: &'static str) -> Response {
    match ServeFile::new(path).oneshot(req).await {
        Ok(mut res) => {
            // Never keep an error, least of all under a URL kept for good.
            let served = res.status().is_success() || res.status() == StatusCode::NOT_MODIFIED;
            let cache = if served { cache } else { "no-store" };
            res.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
            res.into_response()
        }
        // ServeFile reports file errors as HTTP responses; its service error
        // type is Infallible. Newer Rust rejects trying to format that value.
        Err(never) => match never {},
    }
}

async fn cover(
    State(state): State<Shared>,
    Path(id): Path<String>,
    Query(q): Query<CoverQuery>,
    req: Request,
) -> ApiResult<Response> {
    let size = q.size.unwrap_or(512);
    // A thumbnail not made yet reads the art where the album's files are
    // (see `AppState::moving`).
    let moving = state.moving.read().await;
    let (path, current) = blocking(&state, move |s| {
        s.covers.thumbnail(&s.lib, &id, size, q.v.as_deref())
    })
    .await?;
    drop(moving);
    Ok(serve_file(path, req, cover_caching(current)).await)
}

/// The album's art at full size: the image file beside the tracks, or the
/// one embedded in them.
async fn full_cover(
    State(state): State<Shared>,
    Path(id): Path<String>,
    req: Request,
) -> ApiResult<Response> {
    // Read where the album's files are (see `AppState::moving`).
    let _moving = state.moving.read().await;
    let source = blocking(&state, move |s| {
        s.lib
            .cover_source(&id)?
            .ok_or(coversync_core::Error::NotFound)
    })
    .await?;
    // Read from the art as it is now, which can change without the cover pass
    // noticing (an image beside the tracks replaced on its own): always check.
    let cache = "no-cache";
    match source {
        CoverSource::File(path) => Ok(serve_file(path, req, cache).await),
        CoverSource::Embedded(path) => {
            let data = blocking(&state, move |_| {
                tags::embedded_cover(&path)?.ok_or(coversync_core::Error::NotFound)
            })
            .await?;
            let headers = [
                (header::CONTENT_TYPE, art::media_type(&data)),
                (header::CACHE_CONTROL, cache),
            ];
            Ok((headers, data).into_response())
        }
    }
}

async fn stream(
    State(state): State<Shared>,
    Path(id): Path<i64>,
    req: Request,
) -> ApiResult<Response> {
    // Found and opened while no save moves files (see `moving`).
    let _moving = state.moving.read().await;
    let path = blocking(&state, move |s| {
        s.lib.track_path(id)?.ok_or(coversync_core::Error::NotFound)
    })
    .await?;
    Ok(serve_file(path, req, "private, max-age=3600").await)
}

/// Every track's tags, for the tag editor.
#[derive(Serialize)]
struct AlbumTags {
    /// The album's version as its tags were read (see [`AlbumDetail`]).
    version: String,
    tracks: Vec<TrackTags>,
}

#[derive(Serialize)]
struct TrackTags {
    id: i64,
    rel_path: String,
    #[serde(flatten)]
    tags: tags::FileTags,
}

async fn album_tags(
    State(state): State<Shared>,
    Path(id): Path<String>,
) -> ApiResult<Json<AlbumTags>> {
    blocking(&state, move |s| {
        // No save writes the files while they're read (a save indexes them
        // before it lets go), and a scan that indexes meanwhile shows in
        // the version: read again then.
        let _files = s.tag_files.read().unwrap_or_else(|e| e.into_inner());
        let separator = s.lib.library_setup()?.rules.separator;
        for _ in 0..3 {
            let detail = s.lib.album(&id)?.ok_or(coversync_core::Error::NotFound)?;
            let tracks = detail
                .tracks
                .iter()
                .map(|t| {
                    let (path, _) = s
                        .lib
                        .track_root(t.id)?
                        .ok_or(coversync_core::Error::NotFound)?;
                    Ok(TrackTags {
                        id: t.id,
                        rel_path: t.rel_path.clone(),
                        tags: tags::file_tags(&path, &separator)?,
                    })
                })
                .collect::<coversync_core::Result<_>>()?;
            let now = s.lib.album(&id)?.map(|d| d.version);
            if now.as_ref() == Some(&detail.version) {
                return Ok(AlbumTags {
                    version: detail.version,
                    tracks,
                });
            }
        }
        Err(coversync_core::Error::Invalid(
            "the album kept changing while its tags were read; try again".into(),
        ))
    })
    .await
    .map(Json)
}

/// One track's edit: by field, and by tag kind.
type TrackEdit<'a> = (i64, TagEdit, Vec<(&'a str, String)>);

#[derive(Deserialize, Default)]
struct TagWrite {
    /// Applied to every track in the album.
    #[serde(default)]
    album: TagEdit,
    /// Per-track edits keyed by track id; applied after the album edit.
    #[serde(default)]
    tracks: HashMap<i64, TagEdit>,
    /// The album's `version` (from its details) the edits were made on.
    /// If the album has changed since, nothing is written and the answer
    /// is 409 Conflict, with the album as it is now (see [`TagConflict`]).
    /// Without one, the edits are written whatever changed since, as for
    /// clients from before versions.
    #[serde(default)]
    version: Option<String>,
    /// The album's tracks the edits were made on, from the same details:
    /// their ids and paths. With a version, the album is found by these, so
    /// an edit elsewhere that gave it a new id (a new title does) is
    /// reported as a conflict with the album under its new id. A track
    /// counts only at the path it had, since a removed track's id can go to
    /// a new file in a database restored from an older copy. When none of
    /// them is left (its files were all re-created or renamed, say), or
    /// without them, the album is the one its id names now.
    #[serde(default)]
    base_tracks: Option<Vec<BaseTrack>>,
    /// Tag kinds (catalog ids) to set, by track id, after `album` and
    /// `tracks`; an empty value removes the kind.
    #[serde(default)]
    kinds: HashMap<i64, HashMap<String, String>>,
}

/// A track a tag save's edits were made on.
#[derive(Deserialize)]
struct BaseTrack {
    id: i64,
    /// As the album's details give it.
    rel_path: String,
}

#[derive(Serialize)]
struct TagWriteResult {
    /// The album's id after the edit; it changes when the album title does.
    album_id: String,
    files_written: usize,
    /// Files moved to where the folder structure puts them (Library albums),
    /// the album's other files among them.
    files_moved: usize,
    /// Saved, but the files couldn't all be moved: why.
    #[serde(skip_serializing_if = "Option::is_none")]
    warning: Option<String>,
    album: Option<AlbumDetail>,
}

/// The answer to a tag save made on a version of the album that is no
/// longer current: nothing was written.
#[derive(Debug, Serialize)]
struct TagConflict {
    error: &'static str,
    /// The album as it is now, with its current version; null when neither
    /// its tracks nor its id name an album in the Inbox or Library any more.
    album: Option<AlbumDetail>,
}

/// Why a tag save wrote nothing.
#[derive(Debug)]
enum TagWriteError {
    Api(ApiError),
    Conflict(Box<TagConflict>),
}

impl From<ApiError> for TagWriteError {
    fn from(e: ApiError) -> Self {
        TagWriteError::Api(e)
    }
}

impl IntoResponse for TagWriteError {
    fn into_response(self) -> Response {
        match self {
            TagWriteError::Api(e) => e.into_response(),
            TagWriteError::Conflict(c) => (StatusCode::CONFLICT, Json(c)).into_response(),
        }
    }
}

fn merge(album: &TagEdit, track: Option<&TagEdit>) -> TagEdit {
    let pick = |a: &Option<String>, t: Option<&Option<String>>| {
        t.and_then(|v| v.clone()).or_else(|| a.clone())
    };
    TagEdit {
        title: pick(&album.title, track.map(|t| &t.title)),
        artist: pick(&album.artist, track.map(|t| &t.artist)),
        album: pick(&album.album, track.map(|t| &t.album)),
        album_artist: pick(&album.album_artist, track.map(|t| &t.album_artist)),
        track_no: pick(&album.track_no, track.map(|t| &t.track_no)),
        disc_no: pick(&album.disc_no, track.map(|t| &t.disc_no)),
        year: pick(&album.year, track.map(|t| &t.year)),
        genre: pick(&album.genre, track.map(|t| &t.genre)),
    }
}

async fn write_tags(
    State(state): State<Shared>,
    Path(id): Path<String>,
    Json(body): Json<TagWrite>,
) -> Result<Json<TagWriteResult>, TagWriteError> {
    // The jobs lock, from before resolving the album (saving Library setup
    // can switch the Library) until the files are written and indexed, so
    // nothing can change the album between the version check and the
    // write: everything that writes tracks, or shows or hides them, takes
    // it. The write, once begun, goes on if the request is dropped.
    let saved = exclusively(&state, move |s| {
        // The album the edits are for, as it is now: found by its tracks, at
        // the paths the editor had them or ones a save moved them from. With
        // none of them left, the one its id names, whose version then can't
        // match: other tracks make another digest.
        let current = match (&body.version, &body.base_tracks) {
            (Some(_), Some(tracks)) => {
                let tracks: Vec<(i64, String)> =
                    tracks.iter().map(|t| (t.id, t.rel_path.clone())).collect();
                match s.lib.album_of_tracks(&tracks)? {
                    Some(album) => s.lib.album(&album)?,
                    None => s.lib.album(&id)?,
                }
            }
            _ => s.lib.album(&id)?,
        };
        // Inbox files are read-only: an album's tags are edited on its way
        // into the Library, and there.
        if current
            .as_ref()
            .is_some_and(|d| d.album.collection == Collection::Inbox)
        {
            return Ok(Err(ApiError(
                StatusCode::FORBIDDEN,
                "Inbox files are read-only. Import the album to edit its Library copy.".into(),
            )
            .into()));
        }
        if let Some(version) = &body.version
            && current.as_ref().map(|d| &d.version) != Some(version)
        {
            return Ok(Err(TagWriteError::Conflict(Box::new(TagConflict {
                error: "This album has changed since it was loaded, so nothing was saved.",
                album: current,
            }))));
        }
        let current = current.ok_or(coversync_core::Error::NotFound)?;
        let ids: Vec<i64> = current.tracks.iter().map(|t| t.id).collect();
        if let Some(stray) = body
            .tracks
            .keys()
            .chain(body.kinds.keys())
            .find(|t| !ids.contains(t))
        {
            return Err(coversync_core::Error::Tags(format!(
                "track {stray} is not in this album"
            )));
        }
        // Validate every edit before writing anything, so a bad field can't
        // leave the album half-written.
        let kinds_of = |t: &i64| -> Vec<(&str, String)> {
            let mut kinds: Vec<(&str, String)> = body
                .kinds
                .get(t)
                .map(|k| k.iter().map(|(id, v)| (id.as_str(), v.clone())).collect())
                .unwrap_or_default();
            kinds.sort();
            kinds
        };
        let edits: Vec<TrackEdit> = ids
            .iter()
            .map(|t| (*t, merge(&body.album, body.tracks.get(t)), kinds_of(t)))
            .filter(|(_, e, k)| !e.is_empty() || !k.is_empty())
            .collect();
        for (_, edit, kinds) in &edits {
            tags::validate(edit)?;
            for (id, value) in kinds {
                if coversync_core::rules::kind(id).is_none() {
                    return Err(coversync_core::Error::Invalid(format!("unknown tag kind '{id}'")));
                }
                tags::validate_kind(id, value)?;
            }
        }
        // The files are checked, written, moved and indexed before the tag
        // editor can read them again (see `tag_files`).
        let files = s.tag_files.write().unwrap_or_else(|e| e.into_inner());
        reclaim_copies(&s.lib)?;
        moves_put_back(s, "saved")?;
        // Every file can take its edits, as it is, before any is written: an
        // album of mixed formats or files isn't left half edited.
        for (track, edit, kinds) in &edits {
            let (path, _) = s
                .lib
                .track_root(*track)?
                .ok_or(coversync_core::Error::NotFound)?;
            tags::check_kinds(&path, edit, kinds).map_err(|e| {
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                coversync_core::Error::Invalid(format!("{name}: {e}"))
            })?;
        }
        // The album keeps to the folder structure (see `relocate`).
        let template = s.lib.library_setup()?.template()?;
        let layout = s.lib.library_folder()?.map(|root| (template, root));
        let named = |path: &FsPath, e: coversync_core::Error| {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            coversync_core::Error::Invalid(format!("{name}: {e}"))
        };
        // Each file's edit, written to a copy beside it: what can refuse an
        // edit does so before any file changes.
        let mut prepared = Vec::new();
        let mut recorded = Vec::new();
        for (track, edit, kinds) in &edits {
            let (path, root) = s
                .lib
                .track_root(*track)?
                .ok_or(coversync_core::Error::NotFound)?;
            // Recorded as soon as it exists, so one left by coversyncd
            // stopping before it's in place or gone is found again.
            let copy = tags::prepare_owned(&path, edit, kinds, |copy, file| {
                record_copy(&s.lib, copy, file)?;
                recorded.extend(copy.to_str().map(String::from));
                Ok(())
            })
                .map_err(|e| named(&path, e))?;
            prepared.push((*track, root, copy));
        }
        // Where the folder structure puts the album's files, with their tags
        // as saved (the edited ones read from their copies), refused too.
        let plan = match &layout {
            Some((template, root)) => {
                let copies: HashMap<i64, &FsPath> = prepared
                    .iter()
                    .map(|(id, _, copy)| (*id, copy.copy.as_path()))
                    .collect();
                let mut placed = Vec::new();
                for t in &ids {
                    let (path, _) = s
                        .lib
                        .track_root(*t)?
                        .ok_or(coversync_core::Error::NotFound)?;
                    let saved = copies.get(t).copied().unwrap_or(path.as_path());
                    let values = tags::read_kinds(saved, false)?.values;
                    placed.push((path, values));
                }
                let plan = relocate::plan(template, root, &placed).map_err(|e| match e {
                    coversync_core::Error::Invalid(m) => {
                        coversync_core::Error::Invalid(format!("{m} Nothing was saved."))
                    }
                    e => e,
                })?;
                // Moves are recorded in the Library folder's own database
                // (see `relocate::apply`), which its first import makes.
                if !plan.is_empty() && !s.lib.library_stored()? {
                    return Err(coversync_core::Error::Invalid(
                        "These files would move to where the folder structure puts them, which waits for the Library folder's database (its first import makes it). Nothing was saved.".into(),
                    ));
                }
                Some(plan)
            }
            None => None,
        };
        // Files put in place before one fails are indexed all the same, so
        // their tracks (and the album's version) say they changed.
        let mut written: Vec<(i64, PathBuf, PathBuf)> = Vec::new();
        let mut failed = None;
        for (track, root, copy) in prepared {
            let path = copy.path.clone();
            match copy.commit() {
                Ok(()) => written.push((track, path, root)),
                Err(e) => {
                    failed = Some(named(&path, e));
                    break;
                }
            }
        }
        // Every copy is in place or gone (those after a failure with it,
        // unless removing one failed: its record stays for the next save).
        // The records only matter to the next save, which checks them
        // again: one not forgotten now doesn't hold this save up.
        recorded.retain(|copy| {
            matches!(std::fs::symlink_metadata(copy), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
        });
        if let Err(e) = s.lib.forget_copies(&recorded) {
            tracing::warn!("couldn't forget the prepared copies' records: {e}");
        }
        let count = written.len();
        // Then the files go where their tags now put them, and the index
        // follows, before a stream can look for one (see `moving`).
        let moving = plan
            .as_ref()
            .filter(|p| failed.is_none() && !p.is_empty())
            .map(|_| s.moving.blocking_write());
        let mut moved = relocate::Applied::default();
        let mut warning = None;
        // Files a failed move couldn't put back: what's at their old paths
        // now isn't theirs, so those aren't indexed until they're back
        // (scans and saves wait for that too).
        let mut pending = HashSet::new();
        if failed.is_none()
            && let Some(plan) = &plan
        {
            match relocate::apply(&s.lib, plan) {
                Ok(done) => moved = done,
                Err(e) => {
                    tracing::warn!(error = %e, "tags saved, files not moved");
                    pending = pending_moves(&s.lib)?;
                    warning = Some(match pending.len() {
                        0 => format!("Saved, but the files weren't moved: {e}"),
                        n => format!(
                            "Saved, but the files weren't moved ({e}), and {n} couldn't be put back: see coversyncd's log."
                        ),
                    });
                }
            }
        }
        let stat_all = |paths: Vec<(PathBuf, PathBuf)>| -> Vec<FileStat> {
            paths
                .into_iter()
                .filter_map(|(path, root)| scan::stat(path, &root, Collection::Library))
                .collect()
        };
        let at_first: Vec<(PathBuf, PathBuf)> = written
            .iter()
            .map(|(_, path, root)| (path.clone(), root.clone()))
            .collect();
        // The files written, where they are now, and the tracks moved.
        let now: HashMap<&FsPath, &FsPath> = moved
            .tracks
            .iter()
            .map(|m| (m.from.as_path(), m.to.as_path()))
            .collect();
        let mut paths: Vec<(PathBuf, PathBuf)> = written
            .iter()
            .map(|(_, path, root)| {
                let path = now.get(path.as_path()).copied().unwrap_or(path);
                (path.to_path_buf(), root.clone())
            })
            .filter(|(path, _)| !pending.contains(path))
            .collect();
        if let Some((_, root)) = &layout {
            let mut indexed: HashSet<PathBuf> = paths.iter().map(|(p, _)| p.clone()).collect();
            for m in &moved.tracks {
                if indexed.insert(m.to.clone()) {
                    paths.push((m.to.clone(), root.clone()));
                }
            }
        }
        let pairs: Vec<(PathBuf, PathBuf)> = moved
            .tracks
            .iter()
            .map(|m| (m.from.clone(), m.to.clone()))
            .collect();
        let batch = (!moved.batch.is_empty()).then_some(moved.batch.as_str());
        // Publishing the edit invalidates old digests immediately. The hash
        // worker queued by exclusively() rehashes after the save can return.
        // Every moved track is found where it went, or none is indexed there.
        let stats = stat_all(paths);
        let found: HashSet<&FsPath> = stats.iter().map(|f| f.path.as_path()).collect();
        let lost = moved
            .tracks
            .iter()
            .find(|m| !found.contains(m.to.as_path()))
            .map(|m| m.to.clone());
        let indexed = match lost {
            Some(to) => Err(coversync_core::Error::Invalid(format!(
                "{} can't be read where it went",
                to.display()
            ))),
            None => scan::index_moved(&s.lib, stats, &pairs, batch, &s.progress),
        };
        if let Err(e) = indexed {
            if pairs.is_empty() {
                return Err(e);
            }
            // The index can't take the moves: the files go back to where it
            // has them.
            tracing::warn!(error = %e, "moved files not indexed, putting them back");
            let stuck = relocate::recover(&s.lib)?.stuck;
            warning = Some(match stuck {
                0 => format!("Saved, but the files weren't moved: {e}"),
                _ => format!(
                    "Saved, but the files weren't moved ({e}), and {stuck} couldn't be put back: see coversyncd's log."
                ),
            });
            moved = relocate::Applied::default();
            // Back where the index has them: indexed as saved, but for any
            // not back. A new title's album leaves its old cover behind.
            let pending = pending_moves(&s.lib)?;
            let back: Vec<(PathBuf, PathBuf)> = at_first
                .into_iter()
                .filter(|(path, _)| !pending.contains(path))
                .collect();
            scan::index_files(&s.lib, stat_all(back), &s.progress)?;
            s.lib.forget_orphan_covers()?;
        }
        drop(moving);
        drop(files);
        scan::refresh_covers(&s.lib, &s.covers, &s.progress)?;
        // The albums the tracks are in now (a moved track keeps its id):
        // the first written track's, which the editor follows, and any other.
        let previous = current.album.id;
        // With none written (the structure applied anew), the first track's.
        let followed = written.first().map(|(track, ..)| *track).or(ids.first().copied());
        let new_id = match followed {
            Some(track) => s.lib.track_album(track)?,
            None => None,
        }
        .unwrap_or_else(|| previous.clone());
        let mut albums = vec![new_id.clone()];
        for track in &ids {
            if let Some(a) = s.lib.track_album(*track)?
                && !albums.contains(&a)
            {
                albums.push(a);
            }
        }
        let album = s.lib.album(&new_id)?;
        if count > 0 || failed.is_none() {
            // A rename only when none of the tracks is left in the album the
            // edits were for: one that keeps some (a genre-led split, say) stays.
            let renamed = !albums.contains(&previous);
            for id in albums {
                let version = match id == new_id {
                    true => album.as_ref().map(|a| a.version.clone()),
                    false => s.lib.album(&id)?.map(|a| a.version),
                };
                s.events.publish(Notice::AlbumChanged {
                    previous_id: (renamed && id == new_id).then(|| previous.clone()),
                    id,
                    collection: Collection::Library,
                    version,
                });
            }
        }
        if let Some(e) = failed {
            return Err(e);
        }
        Ok(Ok(TagWriteResult {
            album_id: new_id,
            files_written: count,
            files_moved: moved.files,
            warning,
            album,
        }))
    })
    .await;
    crate::release_memory();
    saved?.map(Json)
}

#[derive(Serialize, Default)]
struct Restructured {
    /// Albums moved, and the files moved with them (tracks and covers).
    albums: usize,
    files: usize,
    /// Albums left where they were, each with why.
    failed: Vec<Unmoved>,
}

#[derive(Serialize)]
struct Unmoved {
    title: String,
    /// Its folder, under the Library folder: albums can share a title.
    dir: String,
    error: String,
}

/// Put every Library album where the folder structure says, as saving its
/// tags with no edits does (see `write_tags`), for a structure changed since
/// albums were imported. Albums already in place are left alone; one that
/// can't move stays, with why. The body is JSON (`{}`), which a form on
/// another site can't send. Once begun, the moves go on if the request is
/// dropped.
async fn restructure(
    State(state): State<Shared>,
    _: Json<serde_json::Value>,
) -> ApiResult<Json<Restructured>> {
    Ok(Json(tokio::spawn(restructure_all(state)).await??))
}

async fn restructure_all(state: Shared) -> ApiResult<Restructured> {
    let mut done = Restructured::default();
    // A second pass for albums another one was in the way of.
    for pass in 0..2 {
        let moved = done.files;
        // Under the jobs lock, as other work on the Library is; the saves
        // take it themselves (and queue the hashing they need).
        let checked = {
            let _jobs = state.jobs.lock().await;
            blocking(&state, misplaced).await
        };
        let (to_move, mut failed) = match checked {
            Ok(found) => found,
            Err(e) if pass == 0 => return Err(e),
            // The first pass's moves and report stand, saying why they
            // weren't looked at again.
            Err(ApiError(_, e)) => {
                for failure in &mut done.failed {
                    failure
                        .error
                        .push_str(&format!(" (Not checked again: {e})"));
                }
                break;
            }
        };
        for (detail, dir) in to_move {
            // Saved on the version found, so one changed meanwhile is reported, not moved.
            let body = TagWrite {
                version: Some(detail.version.clone()),
                base_tracks: Some(
                    detail
                        .tracks
                        .iter()
                        .map(|t| BaseTrack {
                            id: t.id,
                            rel_path: t.rel_path.clone(),
                        })
                        .collect(),
                ),
                ..Default::default()
            };
            let unmoved = |error: String| Unmoved {
                title: detail.album.title.clone(),
                dir: dir.clone(),
                error,
            };
            match write_tags(
                State(state.clone()),
                Path(detail.album.id.clone()),
                Json(body),
            )
            .await
            {
                Ok(Json(saved)) => {
                    if saved.files_moved > 0 {
                        done.albums += 1;
                        done.files += saved.files_moved;
                    }
                    if let Some(error) = saved.warning {
                        failed.push(unmoved(error));
                    }
                }
                Err(TagWriteError::Api(ApiError(_, error))) => failed.push(unmoved(error)),
                Err(TagWriteError::Conflict(c)) => failed.push(unmoved(c.error.into())),
            }
        }
        done.failed = failed;
        if done.files == moved || done.failed.is_empty() {
            break;
        }
    }
    Ok(done)
}

/// Albums with files to move, each with its folder under the Library
/// folder, and those that can't be placed, with why.
type Misplaced = (Vec<(AlbumDetail, String)>, Vec<Unmoved>);

/// The Library's albums the folder structure puts elsewhere (see [`Misplaced`]).
fn misplaced(s: &crate::AppState) -> coversync_core::Result<Misplaced> {
    let root = s
        .lib
        .library_folder()?
        .filter(|root| root.is_dir())
        .ok_or_else(|| {
            coversync_core::Error::Invalid("The Library folder isn't available.".into())
        })?;
    // Files a stopped save left moved away are back where the index has them.
    if !s.lib.recorded_moves()?.is_empty() {
        let _files = s.tag_files.write().unwrap_or_else(|e| e.into_inner());
        moves_put_back(s, "moved")?;
    }
    let template = s.lib.library_setup()?.template()?;
    let splitter = coversync_core::split::Splitter::default();
    let (mut to_move, mut failed) = (Vec::new(), Vec::new());
    for album in s
        .lib
        .albums(Collection::Library, &[], &[], &splitter, &[], None)?
    {
        let Some(detail) = s.lib.album(&album.id)? else {
            continue;
        };
        let placed = detail
            .tracks
            .iter()
            .map(|t| {
                let (path, _) = s
                    .lib
                    .track_root(t.id)?
                    .ok_or(coversync_core::Error::NotFound)?;
                let values = tags::kind_values(&path)?;
                Ok((path, values))
            })
            .collect::<coversync_core::Result<Vec<_>>>();
        let dir = FsPath::new(&detail.dir)
            .strip_prefix(&root)
            .map_or_else(|_| detail.dir.clone(), |d| d.display().to_string());
        match placed.and_then(|placed| relocate::plan(&template, &root, &placed)) {
            Ok(plan) if plan.is_empty() => {}
            Ok(_) => to_move.push((detail, dir)),
            Err(e) => failed.push(Unmoved {
                title: detail.album.title,
                dir,
                error: e.to_string(),
            }),
        }
    }
    Ok((to_move, failed))
}

/// Put back the moves saves made and never indexed (see `relocate`), before
/// a file is looked at where the index has it or written where one has to
/// come back to; refused, nothing `done`, while any can't be.
fn moves_put_back(s: &crate::AppState, done: &str) -> coversync_core::Result<()> {
    let _moving = s.moving.blocking_write();
    match relocate::recover(&s.lib)?.stuck {
        0 => Ok(()),
        _ => Err(coversync_core::Error::Invalid(format!(
            "Files an earlier save moved couldn't all be put back (see coversyncd's log), so nothing was {done}."
        ))),
    }
}

/// The Library paths moves recorded and not yet put back are to come back
/// to (see `relocate::recover`).
fn pending_moves(lib: &coversync_core::Library) -> coversync_core::Result<HashSet<PathBuf>> {
    let Some(root) = lib.library_folder()? else {
        return Ok(HashSet::new());
    };
    Ok(lib
        .recorded_moves()?
        .into_iter()
        .flat_map(|(_, records)| records)
        .map(|r| root.join(r.from))
        .collect())
}

/// Record a copy a save is making, as the file it is (see `PreparedCopy`):
/// told by the copy itself, open, not looked up again by its name.
fn record_copy(
    lib: &coversync_core::Library,
    copy: &FsPath,
    file: &std::fs::File,
) -> coversync_core::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let meta = file.metadata()?;
    match copy.to_str() {
        Some(path) => lib.record_copy(PreparedCopy {
            path: path.into(),
            dev: meta.dev(),
            ino: meta.ino(),
        }),
        None => Ok(()),
    }
}

/// Remove the copies saves recorded and never put in place or took away
/// (coversyncd stopped meanwhile), each only while it's the file recorded:
/// nothing else is removed by its name. Saves take turns, so before one
/// starts, every copy recorded is one of these. A record is forgotten once
/// its copy is gone or its name is another file's; one that can't be told
/// or removed (a share away, say) is kept for the next save.
fn reclaim_copies(lib: &coversync_core::Library) -> coversync_core::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let mut done = Vec::new();
    for copy in lib.recorded_copies()? {
        let path = FsPath::new(&copy.path);
        let settled = match std::fs::symlink_metadata(path) {
            Ok(m) if m.is_file() && m.dev() == copy.dev && m.ino() == copy.ino => {
                match std::fs::remove_file(path) {
                    Ok(()) => {
                        tracing::info!(path = copy.path, "removed a copy a stopped save left");
                        true
                    }
                    Err(e) => {
                        tracing::warn!(
                            path = copy.path,
                            "couldn't remove a copy a stopped save left: {e}"
                        );
                        false
                    }
                }
            }
            Ok(_) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
            Err(e) => {
                tracing::warn!(
                    path = copy.path,
                    "couldn't look for a copy a stopped save left: {e}"
                );
                false
            }
        };
        if settled {
            done.push(copy.path);
        }
    }
    lib.forget_copies(&done)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A FLAC file with these Vorbis comments and a few bytes standing for its audio.
    fn flac(path: &FsPath, comments: &[&str]) {
        flac_with_icons(path, comments, 0);
    }

    /// As `flac`, with `icons` 32x32 file icons (PICTURE blocks of type 1),
    /// which FLAC allows one of: a save refuses a file with two, as only its
    /// write finds out (the check before reads no pictures).
    fn flac_with_icons(path: &FsPath, comments: &[&str], icons: usize) {
        let mut b = b"fLaC".to_vec();
        // STREAMINFO: 4096-sample blocks, 44.1 kHz, stereo, 16 bits, no samples.
        b.extend([0, 0, 0, 34, 0x10, 0x00, 0x10, 0x00, 0, 0, 0, 0, 0, 0]);
        let packed: u64 = (44100 << 44) | (1 << 41) | (15 << 36);
        b.extend(packed.to_be_bytes());
        b.extend([0u8; 16]);
        for _ in 0..icons {
            let mut picture = Vec::new();
            picture.extend(1u32.to_be_bytes());
            picture.extend(9u32.to_be_bytes());
            picture.extend(b"image/png");
            picture.extend(0u32.to_be_bytes());
            for v in [32u32, 32, 24, 0] {
                picture.extend(v.to_be_bytes());
            }
            picture.extend(4u32.to_be_bytes());
            picture.extend([1, 2, 3, 4]);
            b.push(6);
            b.extend(&(picture.len() as u32).to_be_bytes()[1..]);
            b.extend(picture);
        }
        let mut block = Vec::new();
        block.extend(9u32.to_le_bytes());
        block.extend(b"coversync");
        block.extend((comments.len() as u32).to_le_bytes());
        for c in comments {
            block.extend((c.len() as u32).to_le_bytes());
            block.extend(c.as_bytes());
        }
        // The last block: VORBIS_COMMENT.
        b.push(0x80 | 4);
        b.extend(&(block.len() as u32).to_be_bytes()[1..]);
        b.extend(block);
        // A frame header's sync code and some bytes: lofty's writer expects audio after the metadata.
        b.extend([0xFF, 0xF8, 0x69, 0x08, 0x00, 0x00, 0x00, 0x00]);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b).unwrap();
    }

    /// Removes a test's scratch folder when the test ends.
    struct Scratch(std::path::PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A daemon's state over an Inbox holding one album and an empty Library
    /// folder, scanned; the Inbox album's id; and the folder, removed with it.
    fn daemon(name: &str) -> (Shared, String, Scratch) {
        let dir = std::env::temp_dir().join(format!("coversyncd-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let inbox = dir.join("inbox");
        flac(
            &inbox.join("Artist/Record/01.flac"),
            &[
                "TITLE=Song",
                "ARTIST=Artist",
                "ALBUMARTIST=Artist",
                "ALBUM=Record",
                "TRACKNUMBER=1",
                "CDTOC=1+96",
            ],
        );
        std::fs::create_dir_all(dir.join("library")).unwrap();
        let state = crate::test_state(&dir);
        state
            .lib
            .set_inbox_setup(InboxSetup {
                folder: Some(inbox.display().to_string()),
                ..Default::default()
            })
            .unwrap();
        state
            .lib
            .set_library_setup(LibrarySetup {
                folder: Some(dir.join("library").display().to_string()),
                ..Default::default()
            })
            .unwrap();
        scan::scan(&state.lib, &state.covers, &state.roots(), &state.progress).unwrap();
        let albums = state
            .lib
            .albums(
                Collection::Inbox,
                &[],
                &[],
                &coversync_core::split::Splitter::default(),
                &[],
                None,
            )
            .unwrap();
        assert_eq!(albums.len(), 1);
        let id = albums[0].id.clone();
        (state, id, Scratch(dir))
    }

    fn edit(album: TagEdit) -> Json<TagWrite> {
        Json(TagWrite {
            album,
            tracks: HashMap::new(),
            version: None,
            base_tracks: None,
            kinds: HashMap::new(),
        })
    }

    #[tokio::test]
    async fn the_editor_reads_and_writes_every_kind() {
        let (state, inbox_id, _dir) = daemon("kinds");
        let request = serde_json::from_value(serde_json::json!({ "album_id": inbox_id })).unwrap();
        let Json(imported) = import_album(State(state.clone()), Json(request))
            .await
            .unwrap();
        let id = imported.album_id;
        let read = |id: &str| album_tags(State(state.clone()), Path(id.to_string()));
        let Json(tags) = read(&id).await.unwrap();
        assert_eq!(tags.tracks.len(), 1);
        let track = &tags.tracks[0];
        assert_eq!(track.tags.values["title"], "Song");
        assert_eq!(track.tags.values["album"], "Record");
        assert!(
            track.tags.other.contains(&("CDTOC".into(), "1+96".into())),
            "{:?}",
            track.tags.other
        );
        let detail = state.lib.album(&id).unwrap().unwrap();
        assert_eq!(tags.version, detail.version);

        // Any kind, on its track; an empty value removes one.
        let kinds = |pairs: &[(&str, &str)]| {
            HashMap::from([(
                track.id,
                pairs
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect::<HashMap<_, _>>(),
            )])
        };
        let body = TagWrite {
            kinds: kinds(&[("label", "Lantis"), ("track_total", "1"), ("artist", "")]),
            ..Default::default()
        };
        let Json(saved) = write_tags(State(state.clone()), Path(id.clone()), Json(body))
            .await
            .unwrap();
        assert_eq!(saved.files_written, 1);
        let Json(after) = read(&saved.album_id).await.unwrap();
        let values = &after.tracks[0].tags.values;
        assert_eq!(values["label"], "Lantis");
        assert_eq!(values["track_total"], "1");
        assert!(!values.contains_key("artist"));
        // Tags no kind names survive the edit.
        assert!(
            after.tracks[0]
                .tags
                .other
                .contains(&("CDTOC".into(), "1+96".into()))
        );

        // A kind that doesn't exist, one that can't be typed, or a bad
        // value: refused, and nothing written.
        for bad in [("nope", "x"), ("rating", "5"), ("track_total", "many")] {
            let body = TagWrite {
                kinds: kinds(&[bad, ("label", "Other")]),
                ..Default::default()
            };
            let result = write_tags(
                State(state.clone()),
                Path(saved.album_id.clone()),
                Json(body),
            )
            .await;
            assert!(result.is_err(), "{bad:?}");
        }
        let Json(unchanged) = read(&saved.album_id).await.unwrap();
        assert_eq!(unchanged.tracks[0].tags.values["label"], "Lantis");
    }

    #[tokio::test]
    async fn a_kind_one_format_cant_hold_writes_no_track() {
        let (state, _, dir) = daemon("mixed");
        // An M4A beside the FLAC, in the same album: MP4 has no publisher field.
        let m4a = dir.0.join("inbox/Artist/Record/02.m4a");
        std::fs::write(
            &m4a,
            include_bytes!("../../coversync-core/tests/fixtures/silence.m4a"),
        )
        .unwrap();
        let tags_of = TagEdit {
            title: Some("Other".into()),
            artist: Some("Artist".into()),
            album: Some("Record".into()),
            album_artist: Some("Artist".into()),
            track_no: Some("2".into()),
            ..Default::default()
        };
        tags::write(&m4a, &tags_of).unwrap();
        scan::scan(&state.lib, &state.covers, &state.roots(), &state.progress).unwrap();
        // The index was filled by this tag reader.
        assert_eq!(
            state.lib.tag_reader(Collection::Inbox).unwrap(),
            tags::READER
        );
        let albums = state
            .lib
            .albums(
                Collection::Inbox,
                &[],
                &[],
                &coversync_core::split::Splitter::default(),
                &[],
                None,
            )
            .unwrap();
        assert_eq!(albums.len(), 1);
        // The import's plan: a track number the M4A can't hold isn't ready,
        // and says which track.
        let inbox = state.lib.album(&albums[0].id).unwrap().unwrap();
        let m4a_track = inbox
            .tracks
            .iter()
            .find(|t| t.rel_path.ends_with(".m4a"))
            .unwrap()
            .id;
        let request = serde_json::from_value(serde_json::json!({
            "album_id": albums[0].id,
            "edits": { m4a_track.to_string(): { "track_number": "65536" } }
        }))
        .unwrap();
        let Json(plan) = import_plan(State(state.clone()), Json(request))
            .await
            .unwrap();
        assert!(!plan.ready());
        assert!(
            plan.problems.iter().any(|p| p.starts_with("Track 2:")),
            "{:?}",
            plan.problems
        );
        // In the Library, where the editor writes.
        let request =
            serde_json::from_value(serde_json::json!({ "album_id": albums[0].id })).unwrap();
        let Json(imported) = import_album(State(state.clone()), Json(request))
            .await
            .unwrap();
        let id = imported.album_id;
        let detail = state.lib.album(&id).unwrap().unwrap();
        assert_eq!(detail.tracks.len(), 2);
        assert!(detail.tracks[0].rel_path.ends_with(".flac"));
        let (flac, _) = state.lib.track_root(detail.tracks[0].id).unwrap().unwrap();
        let before = std::fs::read(&flac).unwrap();
        // Publisher on every track: the FLAC (first) could take it, the M4A can't.
        let kinds = detail
            .tracks
            .iter()
            .map(|t| {
                (
                    t.id,
                    HashMap::from([("publisher".to_string(), "Lantis".to_string())]),
                )
            })
            .collect();
        let body = TagWrite {
            kinds,
            ..Default::default()
        };
        let refused = write_tags(State(state.clone()), Path(id.clone()), Json(body)).await;
        // The FLAC is as it was.
        assert_eq!(std::fs::read(&flac).unwrap(), before);
        let Err(TagWriteError::Api(e)) = refused else {
            panic!("not refused");
        };
        // Named by the file that can't take it.
        let said = format!("{e:?}");
        assert!(
            said.contains(".m4a: ") && said.contains("Publisher"),
            "{said}"
        );
    }

    #[tokio::test]
    async fn a_library_save_moves_the_files_where_the_structure_says() {
        let (state, inbox_id, _dir) = daemon("relocate");
        let request = serde_json::from_value(serde_json::json!({ "album_id": inbox_id })).unwrap();
        let Json(imported) = import_album(State(state.clone()), Json(request))
            .await
            .unwrap();
        let detail = state.lib.album(&imported.album_id).unwrap().unwrap();
        let track = detail.tracks[0].id;
        let (path, root) = state.lib.track_root(track).unwrap().unwrap();
        assert_eq!(path, root.join("Artist/Record/Song.flac"));
        // Another file of the album's, which goes with it.
        std::fs::write(root.join("Artist/Record/cover.jpg"), b"art").unwrap();

        // A new album title: a new folder.
        let body = TagWrite {
            kinds: HashMap::from([(
                track,
                HashMap::from([("album".to_string(), "Another record".to_string())]),
            )]),
            ..Default::default()
        };
        let Json(saved) = write_tags(
            State(state.clone()),
            Path(imported.album_id.clone()),
            Json(body),
        )
        .await
        .unwrap();
        assert_eq!(saved.files_moved, 2, "the track and its cover");
        assert!(saved.warning.is_none());
        let moved = root.join("Artist/Another record");
        assert!(moved.join("Song.flac").is_file());
        assert_eq!(std::fs::read(moved.join("cover.jpg")).unwrap(), b"art");
        assert!(!root.join("Artist/Record").exists());
        // The index has the file where it is now, and the album under its new id.
        let album = saved.album.unwrap();
        assert_eq!(album.tracks.len(), 1);
        assert_eq!(album.tracks[0].rel_path, "Artist/Another record/Song.flac");
        assert!(state.lib.album(&imported.album_id).unwrap().is_none());

        // A title cleared would leave the file without a name: refused, nothing written.
        let track = album.tracks[0].id;
        let body = TagWrite {
            kinds: HashMap::from([(track, HashMap::from([("title".to_string(), String::new())]))]),
            ..Default::default()
        };
        let refused = write_tags(
            State(state.clone()),
            Path(saved.album_id.clone()),
            Json(body),
        )
        .await;
        let Err(TagWriteError::Api(ApiError(_, said))) = refused else {
            panic!("not refused");
        };
        assert!(said.contains("would have no Title"), "{said}");
        assert!(said.ends_with("Nothing was saved."), "{said}");
        let after = tags::read(&moved.join("Song.flac")).unwrap();
        assert_eq!(after.title.as_deref(), Some("Song"));
    }

    #[tokio::test]
    async fn a_scan_moves_left_stuck_keep_from_starting_is_announced_as_failed() {
        let (state, _root, _) =
            library_record("relocate-scan", coversync_core::template::DEFAULT).await;
        // A record recovery won't act on (not one it made): stuck.
        let stuck = coversync_core::db::MoveRecord {
            seq: 0,
            from: "A/x.flac".into(),
            staged: Some("not-ours".into()),
            to: "B/x.flac".into(),
            dev: 0,
            ino: 0,
        };
        state
            .lib
            .record_moves("00000000000000cc", &[stuck])
            .unwrap();
        let scanned = |state: &Shared| {
            let mut heard = state.events.subscribe();
            crate::spawn_scan(state.clone());
            async move {
                tokio::time::timeout(std::time::Duration::from_secs(10), async {
                    loop {
                        if let Ok(Notice::ScanFinished(p)) = heard.recv().await {
                            return p;
                        }
                    }
                })
                .await
                .unwrap()
            }
        };
        let p = scanned(&state).await;
        assert_eq!(p.phase, "failed");
        assert!(!p.running);
        // Once the record's gone, scans finish again.
        state.lib.forget_moves("00000000000000cc", None).unwrap();
        let p = scanned(&state).await;
        assert_eq!(p.phase, "idle");
    }

    #[tokio::test]
    async fn a_copy_a_stopped_save_left_goes_and_nothing_else_by_its_name() {
        let (state, root) = editing_daemon("copies-left");
        let album = detail(&state, "Record");
        // A save that stopped once its copy was made: no cleanup ran.
        let file = root.join("Record/01.flac");
        let left = tags::prepare_owned(&file, &genre("Rock"), &[], |c, f| {
            record_copy(&state.lib, c, f)
        })
        .unwrap();
        let copy = left.copy.clone();
        std::mem::forget(left);
        assert!(copy.is_file());
        // A recorded name that's another file now, and one named like a copy.
        let other = root.join("Record/.coversync-tmp-dead-1.flac");
        std::fs::write(&other, "someone's").unwrap();
        state
            .lib
            .record_copy(PreparedCopy {
                path: other.to_str().unwrap().into(),
                dev: 0,
                ino: 0,
            })
            .unwrap();
        let backup = root.join("Record/.coversync-tmp-dead-1.flac.backup");
        std::fs::write(&backup, "a backup").unwrap();
        // And one where it can't be looked for now (a share away, say).
        use std::os::unix::fs::PermissionsExt;
        let away = root.join("Record/Away");
        std::fs::create_dir_all(&away).unwrap();
        let far_path = away.join(".coversync-tmp-1-0.flac");
        let far = std::fs::File::create(&far_path).unwrap();
        record_copy(&state.lib, &far_path, &far).unwrap();
        std::fs::set_permissions(&away, std::fs::Permissions::from_mode(0o000)).unwrap();
        let checkable = std::fs::read_dir(&away).is_ok();
        put(&state, &album.album.id, edit(genre("Jazz")))
            .await
            .unwrap();
        std::fs::set_permissions(&away, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!copy.exists(), "the copy left goes");
        assert!(other.exists() && backup.exists(), "nothing else does");
        let kept: Vec<String> = state
            .lib
            .recorded_copies()
            .unwrap()
            .into_iter()
            .map(|c| c.path)
            .collect();
        // Kept for the next save, which can look (root could all along).
        if !checkable {
            assert_eq!(kept, [far_path.to_str().unwrap()]);
            put(&state, &album.album.id, edit(genre("Pop")))
                .await
                .unwrap();
        }
        assert!(!far_path.exists());
        assert!(state.lib.recorded_copies().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_import_waits_for_moves_that_couldnt_be_put_back() {
        let (state, _root, _) =
            library_record("relocate-import", coversync_core::template::DEFAULT).await;
        // A record recovery won't act on (not one it made): stuck.
        let stuck = coversync_core::db::MoveRecord {
            seq: 0,
            from: "A/x.flac".into(),
            staged: Some("not-ours".into()),
            to: "B/x.flac".into(),
            dev: 0,
            ino: 0,
        };
        state
            .lib
            .record_moves("00000000000000cd", &[stuck])
            .unwrap();
        let request = serde_json::from_value(
            serde_json::json!({ "album_id": detail(&state, "Other").album.id }),
        )
        .unwrap();
        let refused = import_album(State(state.clone()), Json(request)).await;
        let Err(ApiError(_, said)) = refused else {
            panic!("not refused");
        };
        assert!(said.contains("nothing was imported"), "{said}");
        // Once it's settled, the import goes through.
        state.lib.forget_moves("00000000000000cd", None).unwrap();
        import_titled(&state, "Other").await;
    }

    #[tokio::test]
    async fn a_save_that_would_move_files_waits_for_the_librarys_database() {
        // A Library folder scanned, never imported into: indexed for now only.
        let (state, inbox) = inbox_daemon("relocate-unstored");
        with_library(&state, &inbox);
        let root = state.lib.library_folder().unwrap().unwrap();
        std::fs::create_dir_all(root.join("Artist/Record")).unwrap();
        std::fs::copy(
            inbox.join("Record/01.flac"),
            root.join("Artist/Record/01 - Song 1.flac"),
        )
        .unwrap();
        state.lib.refresh_library().unwrap();
        scan::scan(&state.lib, &state.covers, &state.roots(), &state.progress).unwrap();
        assert!(!state.lib.library_stored().unwrap());
        let albums = state
            .lib
            .albums(
                Collection::Library,
                &[],
                &[],
                &coversync_core::split::Splitter::default(),
                &[],
                None,
            )
            .unwrap();
        let album = state.lib.album(&albums[0].id).unwrap().unwrap();
        let body = TagWrite {
            kinds: HashMap::from([(
                album.tracks[0].id,
                HashMap::from([("album".to_string(), "Renamed".to_string())]),
            )]),
            ..Default::default()
        };
        let refused = write_tags(
            State(state.clone()),
            Path(album.album.id.clone()),
            Json(body),
        )
        .await;
        let Err(TagWriteError::Api(ApiError(_, said))) = refused else {
            panic!("not refused");
        };
        assert!(said.contains("database"), "{said}");
        let file = root.join("Artist/Record/01 - Song 1.flac");
        assert_eq!(tags::read(&file).unwrap().album.as_deref(), Some("Record"));
    }

    /// The "Record" album (three tracks) imported from `editing_daemon`'s
    /// Inbox into a Library with `structure`: the daemon, the Library
    /// folder, and the album as imported.
    async fn library_record(name: &str, structure: &str) -> (Shared, PathBuf, AlbumDetail) {
        let (state, inbox) = inbox_daemon(name);
        with_library(&state, &inbox);
        set_structure(&state, structure);
        let id = import_titled(&state, "Record").await;
        let root = state.lib.library_folder().unwrap().unwrap();
        (state.clone(), root, state.lib.album(&id).unwrap().unwrap())
    }

    fn set_structure(state: &Shared, structure: &str) {
        let setup = state.lib.library_setup().unwrap();
        state
            .lib
            .set_library_setup(LibrarySetup {
                structure: structure.into(),
                ..setup
            })
            .unwrap();
    }

    /// Each Library track row's import time less its row id (the test sets
    /// them so), by path.
    fn added_less_id(root: &FsPath) -> Vec<(String, i64)> {
        let db = rusqlite::Connection::open(root.join(".coversync/library.sqlite3")).unwrap();
        let mut stmt = db
            .prepare("SELECT path, added_at - id FROM tracks ORDER BY path")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    #[tokio::test]
    async fn moved_tracks_keep_their_ids_dates_and_rows_and_a_stale_editor_finds_them() {
        let (state, root, base) =
            library_record("relocate-ids", coversync_core::template::DEFAULT).await;
        // Imported long ago, each track at its own time.
        rusqlite::Connection::open(root.join(".coversync/library.sqlite3"))
            .unwrap()
            .execute("UPDATE tracks SET added_at = 1000 + id", [])
            .unwrap();
        let base = state.lib.album(&base.album.id).unwrap().unwrap();
        let since = state
            .lib
            .library_changes(0)
            .unwrap()
            .unwrap()
            .library
            .generation;

        // Tracks 1 and 2 trade numbers and titles: each file takes the other's path.
        let (one, two) = (&base.tracks[0], &base.tracks[1]);
        assert_eq!(one.rel_path, "Artist/Record/01 - Song 1.flac");
        let swap = |n: &str, title: &str| TagEdit {
            track_no: Some(n.into()),
            title: Some(title.into()),
            ..Default::default()
        };
        let body = Json(TagWrite {
            tracks: HashMap::from([(one.id, swap("2", "Song 2")), (two.id, swap("1", "Song 1"))]),
            version: Some(base.version.clone()),
            base_tracks: Some(base_tracks(&base)),
            ..Default::default()
        });
        let saved = put(&state, &base.album.id, body).await.unwrap();
        assert!(saved.warning.is_none());
        assert_eq!(saved.files_moved, 2);
        let album = saved.album.expect("the album is still there");
        assert_eq!(album.album.id, base.album.id);
        assert_eq!(album.tracks.len(), 3);
        let by_id = |id: i64| album.tracks.iter().find(|t| t.id == id).unwrap();
        assert_eq!(by_id(one.id).rel_path, two.rel_path);
        assert_eq!(by_id(two.id).rel_path, one.rel_path);
        assert_eq!(
            by_id(one.id).moved_from,
            std::slice::from_ref(&one.rel_path)
        );
        let file = tags::read(&root.join(&two.rel_path)).unwrap();
        assert_eq!(file.title.as_deref(), Some("Song 2"));
        // No track left: nothing removed for clients, and the dates stayed.
        let changes = state.lib.library_changes(since).unwrap().unwrap();
        assert!(changes.removed.is_empty(), "{:?}", changes.removed);
        assert!(added_less_id(&root).iter().all(|(_, d)| *d == 1000));
        assert!(state.lib.recorded_moves().unwrap().is_empty());

        // A new title: every file moves, and the album gets a new id.
        std::fs::write(root.join("Artist/Record/cover.jpg"), b"art").unwrap();
        let renamed = TagEdit {
            album: Some("Record 2".into()),
            ..Default::default()
        };
        let saved = put(&state, &album.album.id, save_on(&album, renamed))
            .await
            .unwrap();
        assert_eq!(saved.files_moved, 4, "three tracks and the cover");
        let now = saved.album.unwrap();
        assert_ne!(now.album.id, album.album.id);
        let mut ids: Vec<i64> = now.tracks.iter().map(|t| t.id).collect();
        let mut before: Vec<i64> = album.tracks.iter().map(|t| t.id).collect();
        ids.sort();
        before.sort();
        assert_eq!(ids, before);
        assert!(
            now.tracks
                .iter()
                .all(|t| t.rel_path.starts_with("Artist/Record 2/"))
        );
        assert_eq!(
            std::fs::read(root.join("Artist/Record 2/cover.jpg")).unwrap(),
            b"art"
        );
        assert!(!root.join("Artist/Record").exists());
        assert!(
            state
                .lib
                .library_changes(since)
                .unwrap()
                .unwrap()
                .removed
                .is_empty()
        );
        assert!(added_less_id(&root).iter().all(|(_, d)| *d == 1000));

        // Another editor, still on the album as it was before: its tracks are
        // found at the paths they had, and the save refused with the album
        // as it is now.
        let stale = Json(TagWrite {
            tracks: HashMap::from([(album.tracks[2].id, genre("Jazz"))]),
            version: Some(album.version.clone()),
            base_tracks: Some(base_tracks(&album)),
            ..Default::default()
        });
        let found = conflict(put(&state, &album.album.id, stale).await).expect("found");
        assert_eq!(found.album.id, now.album.id);
        let third = found
            .tracks
            .iter()
            .find(|t| t.id == album.tracks[2].id)
            .unwrap();
        assert_eq!(
            third.moved_from,
            std::slice::from_ref(&album.tracks[2].rel_path)
        );

        // A scan finds them all where the index has them.
        rescan_now(&state);
        let scanned = state.lib.album(&now.album.id).unwrap().unwrap();
        assert_eq!(scanned.version, now.version);
        assert!(
            state
                .lib
                .library_changes(since)
                .unwrap()
                .unwrap()
                .removed
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_new_structure_moves_the_tracks_a_save_left_alone() {
        let (state, root, base) =
            library_record("relocate-structure", coversync_core::template::DEFAULT).await;
        std::fs::write(root.join("Artist/Record/cover.jpg"), b"art").unwrap();
        set_structure(
            &state,
            "{album_artist} - {album}/<{track_number} - >{title}",
        );
        // One track edited; the other two move all the same.
        let body = Json(TagWrite {
            tracks: HashMap::from([(base.tracks[2].id, genre("Rock"))]),
            version: Some(base.version.clone()),
            base_tracks: Some(base_tracks(&base)),
            ..Default::default()
        });
        let saved = put(&state, &base.album.id, body).await.unwrap();
        assert_eq!(saved.files_written, 1);
        assert_eq!(saved.files_moved, 4);
        let now = saved.album.unwrap();
        let paths: Vec<&str> = now.tracks.iter().map(|t| t.rel_path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "Artist - Record/01 - Song 1.flac",
                "Artist - Record/02 - Song 2.flac",
                "Artist - Record/03 - Song 3.flac"
            ]
        );
        assert_eq!(
            std::fs::read(root.join("Artist - Record/cover.jpg")).unwrap(),
            b"art"
        );
        assert!(!root.join("Artist").exists());
        rescan_now(&state);
        assert_eq!(
            state.lib.album(&now.album.id).unwrap().unwrap().version,
            now.version
        );
    }

    #[tokio::test]
    async fn restructuring_moves_the_albums_a_new_structure_puts_elsewhere() {
        let (state, root, _) =
            library_record("restructure", coversync_core::template::DEFAULT).await;
        import_titled(&state, "Other").await;
        std::fs::write(root.join("Artist/Record/cover.jpg"), b"art").unwrap();
        // A folder holding more than an album's tracks and cover stays, and says why.
        std::fs::write(root.join("Artist/Other/notes.txt"), b"mine").unwrap();
        set_structure(
            &state,
            "{album_artist} - {album}/<{track_number} - >{title}",
        );
        let all = || restructure(State(state.clone()), Json(serde_json::json!({})));
        let Json(done) = all().await.unwrap();
        assert_eq!((done.albums, done.files), (1, 4));
        assert!(root.join("Artist - Record/03 - Song 3.flac").is_file());
        assert!(root.join("Artist - Record/cover.jpg").is_file());
        assert_eq!(done.failed.len(), 1);
        let other = &done.failed[0];
        assert_eq!(
            (other.title.as_str(), other.dir.as_str()),
            ("Other", "Artist/Other")
        );
        assert!(other.error.contains("notes.txt"), "{}", other.error);
        assert!(root.join("Artist/Other").is_dir());
        // Once it's moved aside, the rest moves; what's in place is left
        // alone, unannounced.
        std::fs::remove_file(root.join("Artist/Other/notes.txt")).unwrap();
        let mut heard = state.events.subscribe();
        let Json(rest) = all().await.unwrap();
        assert_eq!((rest.albums, rest.failed.len()), (1, 0));
        assert!(!root.join("Artist").exists());
        let mut told = Vec::new();
        while let Ok(notice) = heard.try_recv() {
            if let Notice::AlbumChanged { previous_id, .. } = notice {
                told.push(previous_id);
            }
        }
        assert_eq!(told.len(), 1, "only the album that moved: {told:?}");
        let Json(again) = all().await.unwrap();
        assert_eq!((again.albums, again.files, again.failed.len()), (0, 0, 0));
    }

    #[tokio::test]
    async fn restructuring_moves_an_album_into_a_place_another_one_left() {
        let (state, root, record) =
            library_record("restructure-chain", "{album}/<{track_number} - >{title}").await;
        let other = state
            .lib
            .album(&import_titled(&state, "Other").await)
            .unwrap()
            .unwrap();
        // Record's files go where Other's are; Other's go somewhere free.
        put(&state, &record.album.id, save_on(&record, genre("Other")))
            .await
            .unwrap();
        put(&state, &other.album.id, save_on(&other, genre("Third")))
            .await
            .unwrap();
        set_structure(&state, "{genre}/<{track_number} - >{title}");
        let Json(done) = restructure(State(state.clone()), Json(serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!((done.albums, done.files, done.failed.len()), (2, 5, 0));
        assert!(root.join("Other/03 - Song 3.flac").is_file());
        assert!(root.join("Third/02 - Song 2.flac").is_file());
        assert!(!root.join("Record").exists());
    }

    #[tokio::test]
    async fn a_track_moved_to_another_album_leaves_the_rest_and_both_are_told() {
        let structure = "<{genre}>/{album_artist}/{album}/<{track_number} - >{title}";
        let (state, root, base) = library_record("relocate-split", structure).await;
        let mut heard = state.events.subscribe();
        let first = base.tracks[0].id;
        let body = Json(TagWrite {
            tracks: HashMap::from([(first, genre("Rock"))]),
            version: Some(base.version.clone()),
            base_tracks: Some(base_tracks(&base)),
            ..Default::default()
        });
        let saved = put(&state, &base.album.id, body).await.unwrap();
        assert!(root.join("Rock/Artist/Record/01 - Song 1.flac").is_file());
        let moved = saved.album.unwrap();
        assert_ne!(moved.album.id, base.album.id);
        assert_eq!(moved.tracks.len(), 1);
        let stayed = state.lib.album(&base.album.id).unwrap().unwrap();
        assert_eq!(stayed.tracks.len(), 2);
        let mut told = Vec::new();
        while let Ok(n) = heard.try_recv() {
            if let Notice::AlbumChanged {
                id,
                previous_id,
                version,
                ..
            } = n
            {
                told.push((id, previous_id, version));
            }
        }
        // Not a rename: the album the edit was for is still there.
        assert_eq!(
            told,
            [
                (moved.album.id.clone(), None, Some(moved.version.clone())),
                (base.album.id.clone(), None, Some(stayed.version.clone())),
            ]
        );
    }

    #[tokio::test]
    async fn placement_uses_the_values_the_files_will_hold() {
        let (state, inbox) = inbox_daemon("relocate-values");
        with_library(&state, &inbox);
        // An MP3, whose ID3v2 genre 17 reads back as Rock.
        let mp3 = inbox.join("Tape/song.mp3");
        std::fs::create_dir_all(mp3.parent().unwrap()).unwrap();
        std::fs::write(
            &mp3,
            include_bytes!("../../coversync-core/tests/fixtures/silence.mp3"),
        )
        .unwrap();
        let tagged = TagEdit {
            title: Some("Song".into()),
            artist: Some("Artist".into()),
            album_artist: Some("Artist".into()),
            album: Some("Tape".into()),
            ..Default::default()
        };
        tags::write_with_kinds(&mp3, &tagged, &[]).unwrap();
        // And a FLAC whose disc total is written 02.
        flac(
            &inbox.join("Box/01.flac"),
            &[
                "TITLE=Song",
                "ARTIST=Artist",
                "ALBUMARTIST=Artist",
                "ALBUM=Box",
                "DISCNUMBER=1",
                "DISCTOTAL=02",
            ],
        );
        rescan_now(&state);
        set_structure(&state, "<{genre}>/{album_artist}/{album}/{title}");
        let tape = import_titled(&state, "Tape").await;
        let root = state.lib.library_folder().unwrap().unwrap();
        assert!(root.join("Artist/Tape/Song.mp3").is_file());

        // A file where the request's text would go doesn't stand in the way:
        // the tag will read Rock.
        std::fs::create_dir_all(root.join("17/Artist/Tape")).unwrap();
        std::fs::write(root.join("17/Artist/Tape/Song.mp3"), b"other").unwrap();
        let saved = put(&state, &tape, edit(genre("17"))).await.unwrap();
        assert!(saved.warning.is_none());
        assert!(root.join("Rock/Artist/Tape/Song.mp3").is_file());
        assert_eq!(
            std::fs::read(root.join("17/Artist/Tape/Song.mp3")).unwrap(),
            b"other"
        );
        // A file where the tag's value would go does: refused, nothing written.
        std::fs::create_dir_all(root.join("Blues/Artist/Tape")).unwrap();
        std::fs::write(root.join("Blues/Artist/Tape/Song.mp3"), b"other").unwrap();
        let refused = put(&state, &saved.album_id, edit(genre("0"))).await;
        let Err(TagWriteError::Api(ApiError(_, said))) = refused else {
            panic!("not refused");
        };
        assert!(
            said.contains("Blues/Artist/Tape/Song.mp3 already exists"),
            "{said}"
        );
        assert!(said.ends_with("Nothing was saved."), "{said}");
        let kept = tags::read(&root.join("Rock/Artist/Tape/Song.mp3")).unwrap();
        assert_eq!(kept.genre.as_deref(), Some("Rock"));

        // The disc total as written stays in the folder name across a save.
        set_structure(&state, "{album_artist}/{album}<-{disc_total}>/{title}");
        let boxed = import_titled(&state, "Box").await;
        assert!(
            root.join("Artist/Box-02/Song.flac").is_file(),
            "imported as written"
        );
        let retitled = TagEdit {
            title: Some("Song!".into()),
            ..Default::default()
        };
        let saved = put(&state, &boxed, edit(retitled)).await.unwrap();
        assert!(saved.warning.is_none());
        assert!(root.join("Artist/Box-02/Song!.flac").is_file());

        // The FLAC's vendor string isn't an encoder: a structure that needs
        // one says the file has none, as the tag editor shows it.
        set_structure(&state, "{encoder}/{title}");
        let refused = put(&state, &saved.album_id, edit(genre("Jazz"))).await;
        let Err(TagWriteError::Api(ApiError(_, said))) = refused else {
            panic!("not refused");
        };
        assert!(said.contains("would have no Encoder"), "{said}");
    }

    #[tokio::test]
    async fn a_save_with_no_edits_applies_a_new_structure() {
        let (state, root, base) =
            library_record("relocate-empty", coversync_core::template::DEFAULT).await;
        set_structure(
            &state,
            "{album_artist} - {album}/<{track_number} - >{title}",
        );
        let mut heard = state.events.subscribe();
        let saved = put(&state, &base.album.id, Json(TagWrite::default()))
            .await
            .unwrap();
        assert_eq!(saved.files_written, 0);
        assert_eq!(saved.files_moved, 3);
        let now = saved.album.expect("the album where it went");
        assert_eq!(saved.album_id, now.album.id);
        assert_ne!(now.album.id, base.album.id);
        assert!(root.join("Artist - Record/01 - Song 1.flac").is_file());
        match heard.try_recv().unwrap() {
            Notice::AlbumChanged {
                id, previous_id, ..
            } => {
                assert_eq!(id, now.album.id);
                assert_eq!(previous_id.as_deref(), Some(base.album.id.as_str()));
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_save_puts_back_unindexed_moves_before_it_reads_a_file() {
        let (state, root, base) =
            library_record("relocate-first", coversync_core::template::DEFAULT).await;
        // Left aside by a save coversyncd didn't finish: where the index has
        // the track, nothing.
        let rel = &base.tracks[0].rel_path;
        let dir = root.join(rel).parent().unwrap().to_path_buf();
        let staged = coversync_core::db::MoveRecord {
            seq: 0,
            from: rel.clone(),
            staged: Some(".coversync-move-00000000000000aa-0".into()),
            to: "Elsewhere/01 - Song 1.flac".into(),
            dev: 0,
            ino: 0,
        };
        let meta = {
            use std::os::unix::fs::MetadataExt;
            let m = std::fs::metadata(root.join(rel)).unwrap();
            (m.dev(), m.ino())
        };
        std::fs::rename(
            root.join(rel),
            dir.join(".coversync-move-00000000000000aa-0"),
        )
        .unwrap();
        state
            .lib
            .record_moves(
                "00000000000000aa",
                &[coversync_core::db::MoveRecord {
                    dev: meta.0,
                    ino: meta.1,
                    ..staged
                }],
            )
            .unwrap();
        let body = Json(TagWrite {
            tracks: HashMap::from([(base.tracks[0].id, genre("Rock"))]),
            ..Default::default()
        });
        let saved = put(&state, &base.album.id, body).await.unwrap();
        assert_eq!(saved.files_written, 1);
        let back = tags::read(&root.join(rel)).unwrap();
        assert_eq!(back.genre.as_deref(), Some("Rock"));
        assert!(state.lib.recorded_moves().unwrap().is_empty());

        // One that can't be put back (its place taken) stops saves on the
        // Library, rather than forget the track.
        std::fs::rename(
            root.join(rel),
            dir.join(".coversync-move-00000000000000bb-0"),
        )
        .unwrap();
        std::fs::write(root.join(rel), b"someone else's").unwrap();
        let meta = {
            use std::os::unix::fs::MetadataExt;
            let m = std::fs::metadata(dir.join(".coversync-move-00000000000000bb-0")).unwrap();
            (m.dev(), m.ino())
        };
        state
            .lib
            .record_moves(
                "00000000000000bb",
                &[coversync_core::db::MoveRecord {
                    seq: 0,
                    from: rel.clone(),
                    staged: Some(".coversync-move-00000000000000bb-0".into()),
                    to: "Elsewhere/01 - Song 1.flac".into(),
                    dev: meta.0,
                    ino: meta.1,
                }],
            )
            .unwrap();
        let refused = put(&state, &base.album.id, edit(genre("Jazz"))).await;
        let Err(TagWriteError::Api(ApiError(_, said))) = refused else {
            panic!("not refused");
        };
        assert!(said.contains("couldn't all be put back"), "{said}");
        assert_eq!(state.lib.recorded_moves().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_new_title_drops_the_old_albums_cover_row() {
        let (state, _) = editing_daemon("retitle-cover");
        let base = detail(&state, "Record");
        scan::refresh_covers(&state.lib, &state.covers, &state.progress).unwrap();
        assert!(state.lib.cover(&base.album.id).unwrap().is_some());
        let retitled = TagEdit {
            album: Some("Record 2".into()),
            ..Default::default()
        };
        let saved = put(&state, &base.album.id, save_on(&base, retitled))
            .await
            .unwrap();
        assert_ne!(saved.album_id, base.album.id);
        assert!(state.lib.cover(&base.album.id).unwrap().is_none());
    }

    #[tokio::test]
    async fn a_stream_finds_its_file_only_once_moves_are_indexed() {
        let (state, inbox_id, _dir) = daemon("stream-moving");
        let track = state.lib.album(&inbox_id).unwrap().unwrap().tracks[0].id;
        let moving = state.moving.write().await;
        let request = Request::builder()
            .uri("/")
            .body(axum::body::Body::empty())
            .unwrap();
        let served = tokio::spawn(stream(State(state.clone()), Path(track), request));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(!served.is_finished(), "waits while files move");
        drop(moving);
        let response = served.await.unwrap().unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn imports_and_edits_are_published() {
        let (state, inbox_id, _dir) = daemon("notices");
        let mut heard = state.events.subscribe();

        let request = serde_json::from_value(serde_json::json!({ "album_id": inbox_id })).unwrap();
        let Json(imported) = import_album(State(state.clone()), Json(request))
            .await
            .unwrap();
        match heard.try_recv().unwrap() {
            Notice::Imported {
                album_id,
                inbox_id: from,
            } => {
                assert_eq!(album_id, imported.album_id);
                assert_eq!(from, inbox_id);
                assert_ne!(album_id, inbox_id);
            }
            other => panic!("{other:?}"),
        }

        // An edit that keeps the album's id comes with no previous id.
        let genre = TagEdit {
            genre: Some("Rock".into()),
            ..Default::default()
        };
        let path = Path(imported.album_id.clone());
        let Json(kept) = write_tags(State(state.clone()), path, edit(genre))
            .await
            .unwrap();
        assert_eq!(kept.album_id, imported.album_id);
        match heard.try_recv().unwrap() {
            Notice::AlbumChanged {
                id,
                previous_id,
                collection,
                version,
            } => {
                assert_eq!(id, imported.album_id);
                assert_eq!(previous_id, None);
                assert_eq!(collection, Collection::Library);
                // The version the album has after the edit, as its details give it.
                assert_eq!(
                    version.as_ref(),
                    Some(&kept.album.as_ref().unwrap().version)
                );
            }
            other => panic!("{other:?}"),
        }

        // A new album title gives the album a new id, and the old one comes along.
        let title = TagEdit {
            album: Some("Another record".into()),
            ..Default::default()
        };
        let path = Path(imported.album_id.clone());
        let Json(renamed) = write_tags(State(state.clone()), path, edit(title))
            .await
            .unwrap();
        assert_ne!(renamed.album_id, imported.album_id);
        match heard.try_recv().unwrap() {
            Notice::AlbumChanged {
                id, previous_id, ..
            } => {
                assert_eq!(id, renamed.album_id);
                assert_eq!(previous_id.as_deref(), Some(imported.album_id.as_str()));
            }
            other => panic!("{other:?}"),
        }
        assert!(heard.try_recv().is_err());
    }

    #[tokio::test]
    async fn suggestions_are_forgotten_when_names_would_split_or_join_otherwise() {
        let (state, _, _dir) = daemon("phrases");
        let library = state.lib.library_setup().unwrap();
        state.identify.finished("a");
        // Saved as it was: kept.
        let _ = save_library_setup(State(state.clone()), Json(library.clone()))
            .await
            .unwrap();
        assert_eq!(state.identify.count(), 1);
        let mut phrases = library.clone();
        phrases.rules.split_at = vec!["vs.".into()];
        let _ = save_library_setup(State(state.clone()), Json(phrases))
            .await
            .unwrap();
        assert_eq!(state.identify.count(), 0);
        // The separator alone, from the setup as saved now.
        state.identify.finished("a");
        let mut separator = state.lib.library_setup().unwrap();
        separator.rules.separator = " / ".into();
        let _ = save_library_setup(State(state.clone()), Json(separator))
            .await
            .unwrap();
        assert_eq!(state.identify.count(), 0);
        // And the substitutions.
        state.identify.finished("a");
        let mut substitutions = state.lib.library_setup().unwrap();
        substitutions.rules.substitutions = vec![coversync_core::substitute::Substitution {
            expression: "s/a/b/".into(),
            tags: vec!["title".into()],
        }];
        let _ = save_library_setup(State(state.clone()), Json(substitutions))
            .await
            .unwrap();
        assert_eq!(state.identify.count(), 0);
    }

    #[tokio::test]
    async fn saved_setups_are_published_and_failed_saves_are_not() {
        let (state, _, dir) = daemon("setups");
        let mut heard = state.events.subscribe();
        let saved = |heard: &mut tokio::sync::broadcast::Receiver<Notice>| match heard.try_recv() {
            Ok(Notice::SetupSaved { setup }) => Some(serde_json::to_value(setup).unwrap()),
            Ok(other) => panic!("{other:?}"),
            Err(_) => None,
        };

        let _ = unhide_all(State(state.clone())).await.unwrap();
        assert_eq!(saved(&mut heard), Some("inbox".into()));
        let appearance = state.lib.appearance().unwrap();
        let _ = save_appearance(State(state.clone()), Json(appearance))
            .await
            .unwrap();
        assert_eq!(saved(&mut heard), Some("appearance".into()));
        let sync = state.lib.sync_setup().unwrap();
        let _ = save_sync_setup(State(state.clone()), Json(sync))
            .await
            .unwrap();
        assert_eq!(saved(&mut heard), Some("sync".into()));
        let library = state.lib.library_setup().unwrap();
        let _ = save_library_setup(State(state.clone()), Json(library.clone()))
            .await
            .unwrap();
        assert_eq!(saved(&mut heard), Some("library".into()));
        let inbox = serde_json::to_value(state.lib.inbox_setup().unwrap()).unwrap();
        let request = || serde_json::from_value(inbox.clone()).unwrap();
        let _ = save_inbox_setup(State(state.clone()), Json(request()))
            .await
            .unwrap();
        assert_eq!(saved(&mut heard), Some("inbox".into()));

        // Saves that fail tell nobody.
        let file = dir.0.join("a file");
        std::fs::write(&file, b"").unwrap();
        let bad_library = LibrarySetup {
            folder: Some(file.display().to_string()),
            ..library
        };
        assert!(
            save_library_setup(State(state.clone()), Json(bad_library))
                .await
                .is_err()
        );
        let mut bad_inbox = inbox.clone();
        bad_inbox["folder"] = dir.0.join("missing").display().to_string().into();
        let bad_inbox = serde_json::from_value(bad_inbox).unwrap();
        assert!(
            save_inbox_setup(State(state.clone()), Json(bad_inbox))
                .await
                .is_err()
        );
        assert_eq!(saved(&mut heard), None);
    }

    /// Track `n` of the album `title`, in the Inbox's folder of that name.
    fn track(inbox: &FsPath, title: &str, n: u32) {
        flac(
            &inbox.join(format!("{title}/{n:02}.flac")),
            &[
                &format!("TITLE=Song {n}"),
                "ARTIST=Artist",
                "ALBUMARTIST=Artist",
                &format!("ALBUM={title}"),
                &format!("TRACKNUMBER={n}"),
            ],
        );
    }

    /// Albums "Record" (three tracks) and "Other" (two) in `folder`.
    fn two_albums(folder: &FsPath) {
        for n in 1..=3 {
            track(folder, "Record", n);
        }
        for n in 1..=2 {
            track(folder, "Other", n);
        }
    }

    fn daemon_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("coversyncd-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// A daemon's state over an Inbox holding `two_albums`, scanned; and the
    /// Inbox folder.
    fn inbox_daemon(name: &str) -> (Shared, PathBuf) {
        let dir = daemon_dir(name);
        let inbox = dir.join("inbox");
        two_albums(&inbox);
        let state = crate::test_state(&dir);
        state
            .lib
            .set_inbox_setup(InboxSetup {
                folder: Some(inbox.display().to_string()),
                ..Default::default()
            })
            .unwrap();
        rescan_now(&state);
        (state, inbox)
    }

    /// A daemon's state over a Library holding `two_albums`, scanned, whose
    /// structure (`{album}/{track_number}`) keeps them where they are, so
    /// tag saves move nothing; and the Library folder.
    fn editing_daemon(name: &str) -> (Shared, PathBuf) {
        let dir = daemon_dir(name);
        let root = dir.join("library");
        two_albums(&root);
        let state = crate::test_state(&dir);
        state
            .lib
            .set_library_setup(LibrarySetup {
                folder: Some(root.display().to_string()),
                structure: "{album}/{track_number}".into(),
                ..Default::default()
            })
            .unwrap();
        state.lib.prepare_library(&root).unwrap();
        rescan_now(&state);
        (state, root)
    }

    fn rescan_now(state: &Shared) {
        scan::scan(&state.lib, &state.covers, &state.roots(), &state.progress).unwrap();
    }

    /// The details of the album titled `title`: in the Library, or else the Inbox.
    fn detail(state: &Shared, title: &str) -> AlbumDetail {
        let album = [Collection::Library, Collection::Inbox]
            .into_iter()
            .find_map(|collection| {
                let splitter = coversync_core::split::Splitter::default();
                let albums = state
                    .lib
                    .albums(collection, &[], &[], &splitter, &[], None)
                    .unwrap();
                albums.into_iter().find(|a| a.title == title)
            })
            .unwrap();
        state.lib.album(&album.id).unwrap().unwrap()
    }

    fn genre(genre: &str) -> TagEdit {
        TagEdit {
            genre: Some(genre.into()),
            ..Default::default()
        }
    }

    /// `album`'s tracks as a save made on it sends them.
    fn base_tracks(album: &AlbumDetail) -> Vec<BaseTrack> {
        album
            .tracks
            .iter()
            .map(|t| BaseTrack {
                id: t.id,
                rel_path: t.rel_path.clone(),
            })
            .collect()
    }

    /// A save of `edit` to every track, made on `base` as the tag editor
    /// sends it: with its version and tracks.
    fn save_on(base: &AlbumDetail, edit: TagEdit) -> Json<TagWrite> {
        Json(TagWrite {
            album: edit,
            tracks: HashMap::new(),
            version: Some(base.version.clone()),
            base_tracks: Some(base_tracks(base)),
            kinds: HashMap::new(),
        })
    }

    async fn put(
        state: &Shared,
        id: &str,
        body: Json<TagWrite>,
    ) -> Result<TagWriteResult, TagWriteError> {
        write_tags(State(state.clone()), Path(id.to_string()), body)
            .await
            .map(|Json(r)| r)
    }

    /// The album a save was refused with.
    fn conflict(result: Result<TagWriteResult, TagWriteError>) -> Option<AlbumDetail> {
        match result {
            Err(TagWriteError::Conflict(c)) => c.album,
            Err(TagWriteError::Api(e)) => panic!("not a conflict: {e:?}"),
            Ok(r) => panic!("saved {} files", r.files_written),
        }
    }

    /// Give the Library file at `path` the track id `id`, as a Library
    /// database restored from an older copy can: the ids it had given out
    /// since the copy was made go to other files.
    fn give_id(root: &FsPath, path: &FsPath, id: i64) {
        let db = root.join(coversync_core::db::LIBRARY_DB);
        // The row's id, without the database's prefix (see `db`).
        let row = -id & 0xFFFF_FFFF;
        let rel = path.strip_prefix(root).unwrap();
        let moved = rusqlite::Connection::open(db)
            .unwrap()
            .execute(
                "UPDATE tracks SET id = ?1 WHERE path = ?2",
                rusqlite::params![row, rel.to_string_lossy()],
            )
            .unwrap();
        assert_eq!(moved, 1, "{}", path.display());
    }

    /// The genre each of an album's files is tagged with now.
    fn genres(state: &Shared, album: &AlbumDetail) -> Vec<Option<String>> {
        album
            .tracks
            .iter()
            .map(|t| {
                let path = state.lib.track_path(t.id).unwrap().unwrap();
                tags::read(&path).unwrap().genre
            })
            .collect()
    }

    #[tokio::test]
    async fn a_second_save_from_the_same_version_is_refused() {
        let (state, _) = editing_daemon("conflict-twice");
        let base = detail(&state, "Record");
        let first = put(&state, &base.album.id, save_on(&base, genre("Rock")))
            .await
            .unwrap();
        assert_eq!(first.files_written, 3);
        let saved = first.album.unwrap();
        assert_ne!(saved.version, base.version);
        wait_idle(&state).await;
        let counted = state.lib.index_versions().unwrap();

        // The second, from the same base, writes nothing and answers with
        // the album as the first left it.
        let second = put(&state, &base.album.id, save_on(&base, genre("Jazz"))).await;
        let Err(TagWriteError::Conflict(refused)) = second else {
            panic!("not refused");
        };
        let res = TagWriteError::Conflict(refused).into_response();
        assert_eq!(res.status(), StatusCode::CONFLICT);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .contains("nothing was saved")
        );
        assert_eq!(body["album"]["id"], saved.album.id);
        assert_eq!(body["album"]["version"], saved.version);
        assert_eq!(body["album"]["genre"], "Rock");
        assert_eq!(body["album"]["tracks"].as_array().unwrap().len(), 3);
        assert_eq!(genres(&state, &saved), vec![Some("Rock".to_string()); 3]);
        assert_eq!(state.lib.index_versions().unwrap(), counted);

        // Made again on the album as it is now, it goes through.
        let current = detail(&state, "Record");
        assert_eq!(current.version, saved.version);
        let again = put(&state, &current.album.id, save_on(&current, genre("Jazz")))
            .await
            .unwrap();
        assert_eq!(
            genres(&state, &again.album.unwrap()),
            vec![Some("Jazz".to_string()); 3]
        );
    }

    #[tokio::test]
    async fn a_save_after_another_album_changed_goes_through() {
        let (state, _) = editing_daemon("conflict-other");
        let base = detail(&state, "Record");
        let other = detail(&state, "Other");
        put(&state, &other.album.id, save_on(&other, genre("Jazz")))
            .await
            .unwrap();
        rescan_now(&state);
        assert_eq!(detail(&state, "Record").version, base.version);
        let saved = put(&state, &base.album.id, save_on(&base, genre("Rock")))
            .await
            .unwrap();
        assert_eq!(saved.files_written, 3);
        assert_eq!(
            genres(&state, &saved.album.unwrap()),
            vec![Some("Rock".to_string()); 3]
        );
    }

    #[tokio::test]
    async fn a_save_after_a_track_left_or_joined_the_album_is_refused() {
        let (state, root) = editing_daemon("conflict-tracks");
        let base = detail(&state, "Record");
        // Not the track written last: they were all written together.
        std::fs::remove_file(root.join("Record/01.flac")).unwrap();
        rescan_now(&state);
        let now =
            conflict(put(&state, &base.album.id, save_on(&base, genre("Rock"))).await).unwrap();
        assert_eq!(now.tracks.len(), 2);
        assert_eq!(genres(&state, &now), [None, None]);

        track(&root, "Record", 4);
        rescan_now(&state);
        let joined =
            conflict(put(&state, &now.album.id, save_on(&now, genre("Rock"))).await).unwrap();
        assert_eq!(joined.tracks.len(), 3);
        assert_eq!(genres(&state, &joined), [None, None, None]);
    }

    #[tokio::test]
    async fn a_rename_saves_and_saves_after_it_follow_the_new_id() {
        let (state, _) = editing_daemon("conflict-rename");
        let base = detail(&state, "Record");
        let title = TagEdit {
            album: Some("Renamed".into()),
            ..Default::default()
        };
        let renamed = put(&state, &base.album.id, save_on(&base, title))
            .await
            .unwrap();
        assert_ne!(renamed.album_id, base.album.id);
        let now = renamed.album.unwrap();
        assert_eq!(now.album.title, "Renamed");

        // The next save, on the version the rename answered with.
        let saved = put(&state, &now.album.id, save_on(&now, genre("Rock")))
            .await
            .unwrap();
        assert_eq!(saved.album_id, now.album.id);

        // A save made on the album before the rename, as another client
        // would send it, to the id it had then: refused, with the album
        // found by its tracks under its new id.
        let stale =
            conflict(put(&state, &base.album.id, save_on(&base, genre("Jazz"))).await).unwrap();
        assert_eq!(stale.album.id, now.album.id);
        assert_eq!(stale.album.genre.as_deref(), Some("Rock"));
        assert_eq!(genres(&state, &stale), vec![Some("Rock".to_string()); 3]);
    }

    #[tokio::test]
    async fn a_save_after_every_file_was_re_created_is_refused_with_the_album() {
        let (state, root) = editing_daemon("conflict-recreated");
        let base = detail(&state, "Record");
        // Renamed in place, as a tagger that names files after their tags
        // does: new tracks, under new ids, in the same album.
        for n in 1..=3 {
            let dir = root.join("Record");
            std::fs::rename(
                dir.join(format!("{n:02}.flac")),
                dir.join(format!("{n:02} - Song {n}.flac")),
            )
            .unwrap();
        }
        rescan_now(&state);
        let ids: Vec<i64> = base.tracks.iter().map(|t| t.id).collect();
        let tracks: Vec<(i64, String)> = base
            .tracks
            .iter()
            .map(|t| (t.id, t.rel_path.clone()))
            .collect();
        assert_eq!(state.lib.album_of_tracks(&tracks).unwrap(), None);
        let now = conflict(put(&state, &base.album.id, save_on(&base, genre("Rock"))).await)
            .expect("found by its id");
        assert_eq!(now.album.id, base.album.id);
        assert_eq!(now.tracks.len(), 3);
        assert!(now.tracks.iter().all(|t| !ids.contains(&t.id)));
        assert_eq!(genres(&state, &now), [None, None, None]);
        // Made again on the album as it is now, it goes through.
        let saved = put(&state, &now.album.id, save_on(&now, genre("Rock")))
            .await
            .unwrap();
        assert_eq!(
            genres(&state, &saved.album.unwrap()),
            vec![Some("Rock".to_string()); 3]
        );
    }

    #[tokio::test]
    async fn a_track_id_used_again_comes_back_at_another_path() {
        let (state, root) = editing_daemon("conflict-reused-id");
        let base = detail(&state, "Record");
        let last = base.tracks.last().unwrap();
        let folder = root.join(&base.album.title);
        std::fs::remove_file(root.join(&last.rel_path)).unwrap();
        rescan_now(&state);
        track(&root, &base.album.title, 9);
        rescan_now(&state);
        // The new file gets the removed track's id.
        give_id(&root, &folder.join("09.flac"), last.id);
        let now =
            conflict(put(&state, &base.album.id, save_on(&base, genre("Rock"))).await).unwrap();
        let again = now
            .tracks
            .iter()
            .find(|t| t.id == last.id)
            .expect("id used again");
        assert_ne!(again.rel_path, last.rel_path);
        assert!(folder.join("09.flac").ends_with(&again.rel_path));
        assert!(genres(&state, &now).iter().all(Option::is_none));
    }

    #[tokio::test]
    async fn an_album_whose_ids_all_went_to_new_files_is_not_followed() {
        let (state, root) = editing_daemon("conflict-reused-album");
        let base = detail(&state, "Record");
        std::fs::remove_dir_all(root.join(&base.album.title)).unwrap();
        rescan_now(&state);
        for n in 1..=base.tracks.len() {
            track(&root, "Fresh", n as u32);
        }
        rescan_now(&state);
        // The new files get the removed tracks' ids.
        for (n, t) in (1..).zip(&base.tracks) {
            give_id(&root, &root.join(format!("Fresh/{n:02}.flac")), t.id);
        }
        let fresh = detail(&state, "Fresh");
        let ids = |a: &AlbumDetail| {
            let mut ids: Vec<i64> = a.tracks.iter().map(|t| t.id).collect();
            ids.sort();
            ids
        };
        assert_eq!(ids(&fresh), ids(&base), "every id went to the new album");
        let refused = conflict(put(&state, &base.album.id, save_on(&base, genre("Rock"))).await);
        assert!(
            refused.is_none(),
            "followed to {:?}",
            refused.map(|a| a.album.title)
        );
        assert!(genres(&state, &fresh).iter().all(Option::is_none));
    }

    /// Once the files a save or scan queued are hashed (which raises the
    /// Library's change counter).
    async fn wait_idle(state: &Shared) {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let guard = state.hash_jobs.lock().await;
                if !state.hash_pending.load(std::sync::atomic::Ordering::SeqCst) {
                    break;
                }
                drop(guard);
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("hash queue drains");
    }

    /// Take the jobs lock as `holder` does, and let it go again.
    async fn hold(state: Shared, holder: &'static str) {
        match holder {
            // On the album "import" brought into the Library.
            "tag write" => {
                let base = detail(&state, "Record");
                put(&state, &base.album.id, save_on(&base, genre("Rock")))
                    .await
                    .unwrap();
            }
            "import" => {
                let request = serde_json::from_value(
                    serde_json::json!({ "album_id": detail(&state, "Record").album.id }),
                )
                .unwrap();
                let Json(_) = import_album(State(state), Json(request)).await.unwrap();
            }
            "unhide" => {
                let Json(_) = unhide_all(State(state)).await.unwrap();
            }
            "Library setup" => {
                let setup = state.lib.library_setup().unwrap();
                let Json(_) = save_library_setup(State(state), Json(setup)).await.unwrap();
            }
            "scan" => crate::spawn_scan(state),
            _ => unreachable!(),
        }
    }

    #[tokio::test]
    async fn a_rescan_asked_for_while_the_jobs_lock_is_taken_still_runs() {
        use std::time::{Duration, Instant};
        let (state, inbox) = inbox_daemon("rescan-queued");
        let root = inbox.parent().unwrap().join("library");
        std::fs::create_dir_all(&root).unwrap();
        state
            .lib
            .set_library_setup(LibrarySetup {
                folder: Some(root.display().to_string()),
                ..Default::default()
            })
            .unwrap();
        let late = |state: &Shared| {
            state
                .lib
                .albums(
                    Collection::Inbox,
                    &[],
                    &[],
                    &coversync_core::split::Splitter::default(),
                    &[],
                    None,
                )
                .unwrap()
                .iter()
                .find(|a| a.title == "Late")
                .map_or(0, |a| a.track_count)
        };
        let holders = ["import", "tag write", "unhide", "Library setup", "scan"];
        for (n, holder) in (1..).zip(holders) {
            // The holder waits behind this test for the lock, and takes it
            // when the test lets go; the scan is asked for meanwhile.
            let guard = state.jobs.lock().await;
            let job = tokio::spawn(hold(state.clone(), holder));
            tokio::time::sleep(Duration::from_millis(50)).await;
            track(&inbox, "Late", n);
            crate::spawn_scan(state.clone());
            drop(guard);
            job.await.unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            while late(&state) < n {
                assert!(
                    Instant::now() < deadline,
                    "the scan asked for under the {holder} never ran"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        // Nothing left waiting.
        let _idle = state.jobs.lock().await;
        assert!(!state.rescan.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// Point Library setup at a folder beside the Inbox.
    fn with_library(state: &Shared, inbox: &FsPath) {
        let root = inbox.parent().unwrap().join("library");
        std::fs::create_dir_all(&root).unwrap();
        state
            .lib
            .set_library_setup(LibrarySetup {
                folder: Some(root.display().to_string()),
                ..Default::default()
            })
            .unwrap();
    }

    /// Import the Inbox album titled `title`; its id in the Library.
    async fn import_titled(state: &Shared, title: &str) -> String {
        let request = serde_json::from_value(
            serde_json::json!({ "album_id": detail(state, title).album.id }),
        )
        .unwrap();
        let Json(imported) = import_album(State(state.clone()), Json(request))
            .await
            .unwrap();
        imported.album_id
    }

    #[tokio::test]
    async fn scans_and_tag_saves_hash_automatically_and_queue_behind_an_active_pass() {
        use coversync_core::hashes;
        use std::sync::atomic::Ordering;
        use std::time::Duration;

        let (state, inbox) = inbox_daemon("hash-queue");
        with_library(&state, &inbox);
        // Laid out as `track` lays files, so a save doesn't move them.
        let mut setup = state.lib.library_setup().unwrap();
        setup.structure = "{album}/{track_number}".into();
        state.lib.set_library_setup(setup).unwrap();
        let root = std::path::PathBuf::from(state.lib.library_setup().unwrap().folder.unwrap());
        for n in 1..=3 {
            track(&root, "Library album", n);
        }
        let mut heard = state.events.subscribe();
        crate::spawn_scan(state.clone());
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if matches!(heard.recv().await.unwrap(), Notice::ScanFinished(_)) {
                    break;
                }
            }
        })
        .await
        .unwrap();
        // Scan completion schedules the pass before yielding its task.
        wait_idle(&state).await;
        let before = state.lib.library_hashes().unwrap();
        assert_eq!(before.len(), 3, "scan automatically queues hashing");
        let report = status(State(state.clone())).await.unwrap().0;
        assert_eq!(
            (
                report.hashes.files_total,
                report.hashes.files_done,
                report.hashes.errors,
                report.hashes.skipped
            ),
            (3, 3, 0, 0)
        );
        assert_eq!(
            report.hashes.bytes_hashed,
            before.values().map(|h| h.size).sum::<u64>()
        );
        assert!(!report.hashes.running);
        let id = state
            .lib
            .albums(
                Collection::Library,
                &[],
                &[],
                &coversync_core::split::Splitter::default(),
                &[],
                None,
            )
            .unwrap()[0]
            .id
            .clone();
        let base = state.lib.album(&id).unwrap().unwrap();

        // Hold a pass in the queue after its snapshot has finished. An API
        // save during it must arrange a subsequent pass, even when several
        // requests coalesce before the current pass releases its slot.
        let (started, wait_started) = tokio::sync::oneshot::channel();
        let (release, wait_release) = tokio::sync::oneshot::channel();
        crate::queue_hashes(state.clone(), move |_| async move {
            started.send(()).unwrap();
            wait_release.await.unwrap();
        });
        wait_started.await.unwrap();
        let saved = put(&state, &id, save_on(&base, genre("Rock")))
            .await
            .unwrap()
            .album
            .unwrap();
        assert!(state.lib.library_hashes().unwrap().is_empty());
        assert!(state.hash_pending.load(Ordering::SeqCst));
        for _ in 0..5 {
            crate::spawn_hashes(state.clone());
        }
        release.send(()).unwrap();
        wait_idle(&state).await;
        let after = state.lib.library_hashes().unwrap();
        assert_eq!(after.len(), 3, "tag save automatically queues its rehash");
        for (rel, hash) in &after {
            assert_eq!(*hash, hashes::file(&root.join(rel)).unwrap());
            assert_ne!(hash.hash, before[rel].hash);
        }
        assert_eq!(
            state.lib.album(&id).unwrap().unwrap().version,
            saved.version
        );
        let report = status(State(state.clone())).await.unwrap().0;
        assert_eq!(
            (
                report.hashes.files_done,
                report.hashes.errors,
                report.hashes.skipped
            ),
            (3, 0, 0)
        );
    }

    #[tokio::test]
    async fn a_save_one_file_cant_take_changes_no_file() {
        let (state, root) = editing_daemon("conflict-partial");
        let base = detail(&state, "Record");
        // The second file has two icons, which only writing it finds: every
        // file's edit is written to a copy before any file changes, so the
        // first stays as it was too.
        flac_with_icons(
            &root.join(&base.tracks[1].rel_path),
            &[
                "TITLE=Song 2",
                "ARTIST=Artist",
                "ALBUMARTIST=Artist",
                "ALBUM=Record",
                "TRACKNUMBER=2",
            ],
            2,
        );
        let failed = put(&state, &base.album.id, save_on(&base, genre("Rock"))).await;
        match failed {
            Err(TagWriteError::Api(ApiError(_, message))) => {
                assert!(message.contains("02"), "names the file: {message}")
            }
            _ => panic!("saved"),
        }
        let first = state.lib.track_path(base.tracks[0].id).unwrap().unwrap();
        assert_eq!(tags::read(&first).unwrap().genre, None);
        assert_eq!(detail(&state, "Record").version, base.version);
        let copies = std::fs::read_dir(first.parent().unwrap())
            .unwrap()
            .filter(|e| {
                let name = e.as_ref().unwrap().file_name();
                name.to_string_lossy().starts_with(".coversync")
            })
            .count();
        assert_eq!(copies, 0, "no copy is left behind");

        // A file gone (no scan has noticed) is found before anything is
        // written: the album stays as it was.
        std::fs::remove_file(root.join(&base.tracks[2].rel_path)).unwrap();
        let failed = put(&state, &base.album.id, save_on(&base, genre("Jazz"))).await;
        assert!(matches!(failed, Err(TagWriteError::Api(_))));
        assert_eq!(tags::read(&first).unwrap().genre, None);
        assert_eq!(detail(&state, "Record").version, base.version);
    }

    #[tokio::test]
    async fn inbox_albums_are_never_saved_and_library_saves_find_theirs_by_its_tracks() {
        let (state, inbox) = inbox_daemon("conflict-writes-off");
        with_library(&state, &inbox);
        // Inbox files are read-only.
        let other = detail(&state, "Other");
        match put(&state, &other.album.id, save_on(&other, genre("Rock"))).await {
            Err(TagWriteError::Api(ApiError(status, _))) => {
                assert_eq!(status, StatusCode::FORBIDDEN)
            }
            Err(TagWriteError::Conflict(_)) => panic!("a conflict"),
            Ok(_) => panic!("written"),
        }
        assert_eq!(genres(&state, &other), [None, None]);

        // A Library album, renamed: a save made on it as it is now, sent to the
        // id it had, is found by its tracks and written.
        let old_id = import_titled(&state, "Record").await;
        let base = state.lib.album(&old_id).unwrap().unwrap();
        let title = TagEdit {
            album: Some("Renamed".into()),
            ..Default::default()
        };
        let renamed = put(&state, &old_id, save_on(&base, title))
            .await
            .unwrap()
            .album
            .unwrap();
        assert_ne!(renamed.album.id, old_id);
        let saved = put(&state, &old_id, save_on(&renamed, genre("Rock")))
            .await
            .unwrap();
        assert_eq!(saved.album_id, renamed.album.id);
        assert_eq!(
            genres(&state, &saved.album.unwrap()),
            vec![Some("Rock".to_string()); 3]
        );
    }

    #[tokio::test]
    async fn a_scan_waits_for_work_whose_request_was_dropped() {
        use std::time::{Duration, Instant, SystemTime};
        use tokio::sync::broadcast::error::TryRecvError;
        let (state, inbox) = inbox_daemon("cancelled");
        with_library(&state, &inbox);
        let library = inbox.parent().unwrap().join("library");
        // Enough files that the work outlasts its request by a while.
        for n in 1..=200 {
            track(&inbox, "Big", n);
        }
        rescan_now(&state);
        let library_tracks = |state: &Shared| -> u32 {
            state
                .lib
                .albums(
                    Collection::Library,
                    &[],
                    &[],
                    &coversync_core::split::Splitter::default(),
                    &[],
                    None,
                )
                .unwrap()
                .iter()
                .map(|a| a.track_count)
                .sum()
        };
        /// Whether `dir` holds a FLAC file, at any depth.
        fn any_flac(dir: &FsPath) -> bool {
            std::fs::read_dir(dir)
                .into_iter()
                .flatten()
                .flatten()
                .any(|e| {
                    let p = e.path();
                    if p.is_dir() {
                        any_flac(&p)
                    } else {
                        p.extension().is_some_and(|x| x == "flac")
                    }
                })
        }
        let modified = |p: &FsPath| std::fs::metadata(p).and_then(|m| m.modified()).ok();
        for holder in ["import", "tag write"] {
            let mut heard = state.events.subscribe();
            // What the work does first, once it holds the lock: copy a file
            // into the Library, or write one's tags.
            let (request, started): (_, Box<dyn Fn() -> bool>) = match holder {
                "import" => {
                    let request = serde_json::from_value(
                        serde_json::json!({ "album_id": detail(&state, "Big").album.id }),
                    )
                    .unwrap();
                    let s = state.clone();
                    let request = tokio::spawn(async move {
                        let _ = import_album(State(s), Json(request)).await;
                    });
                    let library = library.clone();
                    (request, Box::new(move || any_flac(&library)))
                }
                _ => {
                    let album = &state
                        .lib
                        .albums(
                            Collection::Library,
                            &[],
                            &[],
                            &coversync_core::split::Splitter::default(),
                            &[],
                            None,
                        )
                        .unwrap()[0];
                    let first = state.lib.album(&album.id).unwrap().unwrap().tracks[0].id;
                    let first = state.lib.track_path(first).unwrap().unwrap();
                    let before: Option<SystemTime> = modified(&first);
                    let (s, id) = (state.clone(), album.id.clone());
                    let request = tokio::spawn(async move {
                        let _ = put(&s, &id, edit(genre("Rock"))).await;
                    });
                    (request, Box::new(move || modified(&first) != before))
                }
            };
            let deadline = Instant::now() + Duration::from_secs(10);
            while !started() {
                assert!(Instant::now() < deadline, "the {holder} never started");
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            // The work holds the lock: a scan asked for now waits; then the
            // request goes (its client hung up), while the work goes on.
            if let Ok(Notice::Imported { .. } | Notice::AlbumChanged { .. }) = heard.try_recv() {
                panic!("the {holder} was done before its request could be dropped");
            }
            crate::spawn_scan(state.clone());
            request.abort();
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut done = false;
            loop {
                assert!(
                    Instant::now() < deadline,
                    "the {holder} and the scan after it didn't both end"
                );
                match heard.try_recv() {
                    Ok(Notice::Imported { .. } | Notice::AlbumChanged { .. }) => done = true,
                    Ok(Notice::ScanFinished(_)) => {
                        assert!(done, "the scan finished before the {holder}");
                        break;
                    }
                    Ok(_) | Err(TryRecvError::Empty) => {}
                    Err(e) => panic!("{e:?}"),
                }
                // The scan only starts once the work is done.
                if !done {
                    let scanning = state.progress.read().unwrap().running;
                    assert!(!scanning, "a scan ran while the {holder} did");
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            assert_eq!(library_tracks(&state), 200, "no ghost tracks");
        }
    }

    #[tokio::test]
    async fn a_second_save_on_a_library_album_from_the_same_version_is_refused() {
        let (state, inbox) = inbox_daemon("conflict-library");
        let root = inbox.parent().unwrap().join("library");
        std::fs::create_dir_all(&root).unwrap();
        state
            .lib
            .set_library_setup(LibrarySetup {
                folder: Some(root.display().to_string()),
                ..Default::default()
            })
            .unwrap();
        let request = serde_json::from_value(
            serde_json::json!({ "album_id": detail(&state, "Record").album.id }),
        )
        .unwrap();
        let Json(imported) = import_album(State(state.clone()), Json(request))
            .await
            .unwrap();
        let base = state.lib.album(&imported.album_id).unwrap().unwrap();
        assert_eq!(base.album.collection, Collection::Library);
        assert!(base.tracks.iter().all(|t| t.id < 0));

        let first = put(&state, &base.album.id, save_on(&base, genre("Rock")))
            .await
            .unwrap();
        let saved = first.album.unwrap();
        let now =
            conflict(put(&state, &base.album.id, save_on(&base, genre("Jazz"))).await).unwrap();
        assert_eq!(now.version, saved.version);
        assert_eq!(genres(&state, &now), vec![Some("Rock".to_string()); 3]);
        let again = put(&state, &now.album.id, save_on(&now, genre("Jazz")))
            .await
            .unwrap();
        assert_eq!(
            genres(&state, &again.album.unwrap()),
            vec![Some("Jazz".to_string()); 3]
        );
    }

    #[tokio::test]
    async fn a_save_on_an_album_no_longer_there_is_refused_with_none() {
        let (state, root) = editing_daemon("conflict-gone");
        let base = detail(&state, "Other");
        std::fs::remove_dir_all(root.join("Other")).unwrap();
        rescan_now(&state);
        assert!(
            conflict(put(&state, &base.album.id, save_on(&base, genre("Rock"))).await).is_none()
        );
    }

    #[tokio::test]
    async fn a_save_without_a_version_is_written_as_before() {
        let (state, _) = editing_daemon("conflict-unversioned");
        let base = detail(&state, "Record");
        put(&state, &base.album.id, save_on(&base, genre("Rock")))
            .await
            .unwrap();
        // An older client's save, made on the album before that one.
        let old = Json(TagWrite {
            album: genre("Jazz"),
            tracks: HashMap::new(),
            version: None,
            base_tracks: None,
            kinds: HashMap::new(),
        });
        let saved = put(&state, &base.album.id, old).await.unwrap();
        assert_eq!(
            genres(&state, &saved.album.unwrap()),
            vec![Some("Jazz".to_string()); 3]
        );
    }

    #[tokio::test]
    async fn a_version_without_tracks_is_checked_against_the_albums_id() {
        let (state, _) = editing_daemon("conflict-by-id");
        let base = detail(&state, "Record");
        let by_id = |edit| {
            Json(TagWrite {
                album: edit,
                tracks: HashMap::new(),
                version: Some(base.version.clone()),
                base_tracks: None,
                kinds: HashMap::new(),
            })
        };
        put(&state, &base.album.id, by_id(genre("Rock")))
            .await
            .unwrap();
        let now = conflict(put(&state, &base.album.id, by_id(genre("Jazz"))).await).unwrap();
        assert_eq!(now.album.genre.as_deref(), Some("Rock"));
    }

    #[test]
    fn album_details_carry_their_version() {
        let (state, _) = editing_daemon("version-json");
        let base = detail(&state, "Record");
        let json = serde_json::to_value(&base).unwrap();
        assert_eq!(json["version"], base.version);
        assert_eq!(base.version.len(), 16);
    }

    #[test]
    fn only_current_cover_renderings_are_kept() {
        assert_eq!(cover_caching(false), "no-cache");
        assert!(cover_caching(true).contains("immutable"));
    }

    #[tokio::test]
    async fn errors_are_never_kept() {
        let dir = std::env::temp_dir().join(format!("coversyncd-serve-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("c.jpg"), b"jpeg").unwrap();
        let get = || {
            axum::http::Request::builder()
                .uri("/")
                .body(axum::body::Body::empty())
                .unwrap()
        };
        let served = serve_file(dir.join("c.jpg"), get(), cover_caching(true)).await;
        assert_eq!(served.status(), StatusCode::OK);
        assert_eq!(served.headers()[header::CACHE_CONTROL], cover_caching(true));
        let missing = serve_file(dir.join("gone.jpg"), get(), cover_caching(true)).await;
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        assert_eq!(missing.headers()[header::CACHE_CONTROL], "no-store");
    }

    #[test]
    fn status_reports_the_library_and_inbox_counters() {
        let dir = std::env::temp_dir().join(format!("coversyncd-status-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let lib = coversync_core::Library::open_in(&dir.join("state")).unwrap();
        let status = |lib: &coversync_core::Library| {
            serde_json::to_value(Status {
                version: "0",
                scan: Default::default(),
                hashes: Default::default(),
                index: lib.index_versions().unwrap(),
            })
            .unwrap()
        };
        let keys = |v: &serde_json::Value| {
            let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
            k.sort();
            k
        };
        let v = status(&lib);
        assert_eq!(keys(&v), ["hashes", "inbox", "library", "scan", "version"]);
        assert_eq!(v["library"], serde_json::Value::Null);
        assert_eq!(v["inbox"], serde_json::json!({ "generation": 0 }));

        let root = dir.join("Lib");
        lib.set_library_setup(LibrarySetup {
            folder: Some(root.display().to_string()),
            ..Default::default()
        })
        .unwrap();
        lib.prepare_library(&root).unwrap();
        let v = status(&lib);
        assert_eq!(keys(&v["library"]), ["generation", "id"]);
        assert_eq!(v["library"]["generation"], 0);
        assert_eq!(v["library"]["id"].as_str().unwrap().len(), 36);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn track_edits_override_album_edits() {
        let album = TagEdit {
            genre: Some("Rock".into()),
            artist: Some("A".into()),
            ..Default::default()
        };
        let track = TagEdit {
            artist: Some("B".into()),
            ..Default::default()
        };
        let m = merge(&album, Some(&track));
        assert_eq!(m.artist.as_deref(), Some("B"));
        assert_eq!(m.genre.as_deref(), Some("Rock"));
        assert!(m.title.is_none());
    }
}
