//! JSON API, cover thumbnails and audio streaming.

use std::collections::HashMap;
use std::path::PathBuf;

use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use base64::Engine;
use coversync_core::appearance::{self, AppearanceSetup};
use coversync_core::art::{self, Art};
use coversync_core::db::{CoverSource, FileStat};
use coversync_core::import::{self, ImportPlan, ImportResult};
use coversync_core::scan::Progress;
use coversync_core::scan::{self, album_dir_in, album_id_in};
use coversync_core::sync::SyncSetup;
use coversync_core::template::{self, Template};
use coversync_core::{
    AlbumDetail, Collection, HiddenStats, InboxSetup, IndexVersions, LibrarySetup, TagCoverage,
    TagEdit, inbox, rules, tags, upload,
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
        .route("/api/albums/{id}/tags", put(write_tags))
        .route("/api/tracks/{id}/stream", get(stream))
        .route(
            "/api/library/setup",
            get(library_setup).put(save_library_setup),
        )
        .route("/api/inbox/coverage", get(inbox_coverage))
        .route("/api/inbox/setup", get(inbox_setup).put(save_inbox_setup))
        .route("/api/inbox/unhide", post(unhide_all))
        .route("/api/library/preview", get(structure_preview))
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
        blocking(&state, f).await
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
    allow_writes: bool,
    scan: coversync_core::ScanProgress,
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
        allow_writes: state.allow_writes,
        scan,
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
            &setup.rules.separator,
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

/// Library setup is coversync's own configuration, not file contents, so
/// saving it doesn't need `--allow-writes`. A new folder is indexed right away.
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
        // Suggestions joined with the old separator shouldn't linger.
        if before.rules.separator != saved.rules.separator {
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
            Ok(t) => t,
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
            .albums(Collection::Inbox, &[], &[], "", &[], None)?
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
    let _guard = state.jobs.lock().await;
    let result = blocking(&state, move |s| {
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
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn cover(
    State(state): State<Shared>,
    Path(id): Path<String>,
    Query(q): Query<CoverQuery>,
    req: Request,
) -> ApiResult<Response> {
    let size = q.size.unwrap_or(512);
    let (path, current) = blocking(&state, move |s| {
        s.covers.thumbnail(&s.lib, &id, size, q.v.as_deref())
    })
    .await?;
    Ok(serve_file(path, req, cover_caching(current)).await)
}

/// The album's art at full size: the image file beside the tracks, or the
/// one embedded in them.
async fn full_cover(
    State(state): State<Shared>,
    Path(id): Path<String>,
    req: Request,
) -> ApiResult<Response> {
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
    let path = blocking(&state, move |s| {
        s.lib.track_path(id)?.ok_or(coversync_core::Error::NotFound)
    })
    .await?;
    Ok(serve_file(path, req, "private, max-age=3600").await)
}

#[derive(Deserialize)]
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
    /// a new file. When none of them is left (its files were all re-created
    /// or renamed, say), or without them, the album is the one its id names
    /// now.
    #[serde(default)]
    base_tracks: Option<Vec<BaseTrack>>,
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
    let allow_writes = state.allow_writes;
    let saved = exclusively(&state, move |s| {
        // The album the edits are for, as it is now. With none of its tracks
        // left (a row's path never changes, so the same ids at the same
        // generations would have been found), the one its id names, whose
        // version then can't match: other tracks make another digest.
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
        // Library files are coversync's own; editing Inbox files in place needs --allow-writes.
        if !allow_writes
            && current
                .as_ref()
                .is_some_and(|d| d.album.collection == Collection::Inbox)
        {
            return Ok(Err(ApiError(
                StatusCode::FORBIDDEN,
                "Editing Inbox files in place is disabled on this server (start coversyncd with --allow-writes). Import the album to edit its Library copy.".into(),
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
        let collection = current.album.collection;
        let ids: Vec<i64> = current.tracks.iter().map(|t| t.id).collect();
        if let Some(stray) = body.tracks.keys().find(|t| !ids.contains(t)) {
            return Err(coversync_core::Error::Tags(format!(
                "track {stray} is not in this album"
            )));
        }
        // Validate every edit before writing anything, so a bad field can't
        // leave the album half-written.
        let edits: Vec<(i64, TagEdit)> = ids
            .iter()
            .map(|t| (*t, merge(&body.album, body.tracks.get(t))))
            .filter(|(_, e)| !e.is_empty())
            .collect();
        for (_, edit) in &edits {
            tags::validate(edit)?;
        }
        let mut written = Vec::new();
        for (track, edit) in &edits {
            let (path, root) = s
                .lib
                .track_root(*track)?
                .ok_or(coversync_core::Error::NotFound)?;
            tags::write(&path, edit)?;
            written.push((path, root));
        }
        let stats: Vec<FileStat> = written
            .into_iter()
            .filter_map(|(path, root)| scan::stat(path, &root, collection))
            .collect();
        let count = stats.len();
        let first = stats.first().map(|f| (f.path.clone(), f.root.clone()));
        scan::index_files(&s.lib, stats, &s.progress)?;
        s.lib.remove_paths(&[])?;
        scan::refresh_covers(&s.lib, &s.covers, &s.progress)?;
        let new_id = match first {
            Some((p, root)) => album_id_in(
                collection,
                &root,
                &album_dir_in(collection, &root, &p),
                tags::read(&p)?.album.as_deref(),
            ),
            None => current.album.id.clone(),
        };
        let album = s.lib.album(&new_id)?;
        let previous = current.album.id;
        s.events.publish(Notice::AlbumChanged {
            id: new_id.clone(),
            previous_id: (new_id != previous).then_some(previous),
            collection,
            version: album.as_ref().map(|a| a.version.clone()),
        });
        Ok(Ok(TagWriteResult {
            album_id: new_id,
            files_written: count,
            album,
        }))
    })
    .await;
    crate::release_memory();
    saved?.map(Json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path as FsPath;
    use std::sync::Arc;

    /// A FLAC file with no audio, only these Vorbis comments.
    fn flac(path: &FsPath, comments: &[&str]) {
        let mut b = b"fLaC".to_vec();
        // STREAMINFO: 4096-sample blocks, 44.1 kHz, stereo, 16 bits, no samples.
        b.extend([0, 0, 0, 34, 0x10, 0x00, 0x10, 0x00, 0, 0, 0, 0, 0, 0]);
        let packed: u64 = (44100 << 44) | (1 << 41) | (15 << 36);
        b.extend(packed.to_be_bytes());
        b.extend([0u8; 16]);
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
            .albums(Collection::Inbox, &[], &[], "", &[], None)
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
        })
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

    /// A daemon's state that may edit Inbox files in place, over an Inbox
    /// holding albums "Record" (three tracks) and "Other" (two), scanned;
    /// and the Inbox folder.
    fn editing_daemon(name: &str) -> (Shared, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("coversyncd-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let inbox = dir.join("inbox");
        for n in 1..=3 {
            track(&inbox, "Record", n);
        }
        for n in 1..=2 {
            track(&inbox, "Other", n);
        }
        let mut state = Arc::into_inner(crate::test_state(&dir)).unwrap();
        state.allow_writes = true;
        state
            .lib
            .set_inbox_setup(InboxSetup {
                folder: Some(inbox.display().to_string()),
                ..Default::default()
            })
            .unwrap();
        let state = Arc::new(state);
        rescan_now(&state);
        (state, inbox)
    }

    fn rescan_now(state: &Shared) {
        scan::scan(&state.lib, &state.covers, &state.roots(), &state.progress).unwrap();
    }

    /// The details of the Inbox album titled `title`.
    fn detail(state: &Shared, title: &str) -> AlbumDetail {
        let albums = state
            .lib
            .albums(Collection::Inbox, &[], &[], "", &[], None)
            .unwrap();
        let album = albums.iter().find(|a| a.title == title).unwrap();
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
        let (state, inbox) = editing_daemon("conflict-tracks");
        let base = detail(&state, "Record");
        // Not the track written last: they were all written together.
        std::fs::remove_file(inbox.join("Record/01.flac")).unwrap();
        rescan_now(&state);
        let now =
            conflict(put(&state, &base.album.id, save_on(&base, genre("Rock"))).await).unwrap();
        assert_eq!(now.tracks.len(), 2);
        assert_eq!(genres(&state, &now), [None, None]);

        track(&inbox, "Record", 4);
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
        let (state, inbox) = editing_daemon("conflict-recreated");
        let base = detail(&state, "Record");
        // Renamed in place, as a tagger that names files after their tags
        // does: new tracks, under new ids, in the same album.
        for n in 1..=3 {
            let dir = inbox.join("Record");
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
        let (state, inbox) = editing_daemon("conflict-reused-id");
        // The track with the highest id: SQLite gives the next row its id
        // once it's gone.
        let albums = [detail(&state, "Record"), detail(&state, "Other")];
        let (base, last) = albums
            .iter()
            .flat_map(|a| a.tracks.iter().map(move |t| (a, t)))
            .max_by_key(|(_, t)| t.id)
            .unwrap();
        let folder = inbox.join(&base.album.title);
        std::fs::remove_file(inbox.join(&last.rel_path)).unwrap();
        rescan_now(&state);
        track(&inbox, &base.album.title, 9);
        rescan_now(&state);
        let now =
            conflict(put(&state, &base.album.id, save_on(base, genre("Rock"))).await).unwrap();
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
        let (state, inbox) = editing_daemon("conflict-reused-album");
        // The album holding the highest ids: SQLite gives them to the next
        // files once it's gone.
        let base = [detail(&state, "Record"), detail(&state, "Other")]
            .into_iter()
            .max_by_key(|a| a.tracks.iter().map(|t| t.id).max())
            .unwrap();
        std::fs::remove_dir_all(inbox.join(&base.album.title)).unwrap();
        rescan_now(&state);
        for n in 1..=base.tracks.len() {
            track(&inbox, "Fresh", n as u32);
        }
        rescan_now(&state);
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

    /// Take the jobs lock as `holder` does, and let it go again.
    async fn hold(state: Shared, holder: &'static str) {
        match holder {
            "tag write" => {
                let base = detail(&state, "Other");
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
        let (state, inbox) = editing_daemon("rescan-queued");
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
                .albums(Collection::Inbox, &[], &[], "", &[], None)
                .unwrap()
                .iter()
                .find(|a| a.title == "Late")
                .map_or(0, |a| a.track_count)
        };
        let holders = ["tag write", "import", "unhide", "Library setup", "scan"];
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

    #[tokio::test]
    async fn a_second_save_on_a_library_album_from_the_same_version_is_refused() {
        let (state, inbox) = editing_daemon("conflict-library");
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
        let (state, inbox) = editing_daemon("conflict-gone");
        let base = detail(&state, "Other");
        std::fs::remove_dir_all(inbox.join("Other")).unwrap();
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
                allow_writes: false,
                scan: Default::default(),
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
        assert_eq!(
            keys(&v),
            ["allow_writes", "inbox", "library", "scan", "version"]
        );
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
