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
    TagEdit, inbox, rules, tags,
};
use serde::{Deserialize, Serialize};
use tower::ServiceExt;
use tower_http::services::ServeFile;

use crate::{Shared, spawn_scan};

pub fn router() -> Router<Shared> {
    Router::new()
        .route("/api/status", get(status))
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
pub struct ApiError(StatusCode, String);

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
struct FolderStatus {
    path: String,
    exists: bool,
    dir: bool,
    writable: bool,
    /// Missing folders are created on first import if this is true.
    creatable: bool,
    /// Tracks in the coversync Library this folder holds, if it holds one.
    library: Option<u64>,
}

fn folder_status(path: &str) -> FolderStatus {
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
    let guard = state.jobs.lock().await;
    let result = blocking(&state, move |s| {
        let before = s.lib.library_setup()?;
        let setup = setup.normalize()?;
        must_be_folder(setup.folder.as_deref(), "library folder")?;
        inbox::check_folders(
            s.lib.inbox_setup()?.folder.as_deref(),
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
    .await;
    drop(guard);
    // A scan asked for while this held the lock runs now too.
    if result.as_ref().is_ok_and(|(_, moved)| *moved)
        || state
            .rescan
            .swap(false, std::sync::atomic::Ordering::SeqCst)
    {
        spawn_scan(state.clone());
    }
    let (response, _) = result?;
    Ok(Json(response))
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

/// A new Inbox folder must exist; changing it rescans.
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
    let (response, moved) = blocking(&state, move |s| {
        let before = s.lib.inbox_setup()?.folder;
        let setup = setup.normalize()?;
        if let Some(f) = &setup.folder
            && !std::path::Path::new(f).is_dir()
        {
            return Err(coversync_core::Error::Invalid(format!(
                "The Inbox folder {f} doesn't exist or isn't a folder."
            )));
        }
        inbox::check_folders(
            setup.folder.as_deref(),
            s.lib.library_setup()?.folder.as_deref(),
        )?;
        if let Some(key) = key {
            s.lib.set_acoustid_key(key.as_deref())?;
        }
        let saved = s.lib.set_inbox_setup(setup)?;
        // Suggestions found with the old key (or settings) shouldn't linger.
        s.identify.clear();
        let moved = before != saved.folder;
        Ok((inbox_response(s, saved)?, moved))
    })
    .await?;
    if moved {
        spawn_scan(state.clone());
    }
    Ok(Json(response))
}

#[derive(Serialize)]
struct UnhideResult {
    unhidden: u32,
    hidden: HiddenStats,
}

async fn unhide_all(State(state): State<Shared>) -> ApiResult<Json<UnhideResult>> {
    blocking(&state, |s| {
        let unhidden = s.lib.unhide_all()?;
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
    blocking(&state, move |s| s.lib.set_appearance(setup))
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
    blocking(&state, move |s| s.lib.set_sync_setup(setup))
        .await
        .map(|setup| Json(SyncSetupResponse { setup }))
}

#[derive(Deserialize)]
struct FolderQuery {
    path: String,
}

/// Live feedback while a folder path is typed in setup.
async fn folder_check(
    State(state): State<Shared>,
    Query(q): Query<FolderQuery>,
) -> ApiResult<Json<Option<FolderStatus>>> {
    blocking(&state, move |_| {
        Ok(rules::folder_setting(Some(q.path), "folder")?
            .as_deref()
            .map(folder_status))
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
    /// The ids of the album's tracks the edits were made on, from the same
    /// details. With a version, the album is found by its tracks, so an
    /// edit elsewhere that gave it a new id (a new title does) is reported
    /// as a conflict with the album under its new id. Without them, the
    /// album is the one its id names now.
    #[serde(default)]
    track_ids: Option<Vec<i64>>,
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
    /// The album as it is now, with its current version; null when none of
    /// its tracks is in the Inbox or Library any more.
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
    // Before resolving the album: saving Library setup can switch the Library.
    // Held until the files are written and indexed, so nothing can change
    // the album between the version check and the write.
    let _guard = state.jobs.lock().await;
    let allow_writes = state.allow_writes;
    let saved = blocking(&state, move |s| {
        // The album the edits are for, as it is now.
        let current = match (&body.version, &body.track_ids) {
            (Some(_), Some(tracks)) => match s.lib.album_of_tracks(tracks)? {
                Some(album) => s.lib.album(&album)?,
                None => None,
            },
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

    /// A save of `edit` to every track, made on `base` as the tag editor
    /// sends it: with its version and tracks.
    fn save_on(base: &AlbumDetail, edit: TagEdit) -> Json<TagWrite> {
        Json(TagWrite {
            album: edit,
            tracks: HashMap::new(),
            version: Some(base.version.clone()),
            track_ids: Some(base.tracks.iter().map(|t| t.id).collect()),
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
            track_ids: None,
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
                track_ids: None,
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
