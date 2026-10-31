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
use coversync_core::scan::{self, album_dir, album_id};
use coversync_core::sync::SyncSetup;
use coversync_core::template::{self, Template};
use coversync_core::{
    AlbumDetail, Collection, HiddenStats, InboxSetup, LibrarySetup, TagCoverage, TagEdit, inbox,
    rules, tags,
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
}

async fn status(State(state): State<Shared>) -> Json<Status> {
    let scan = state
        .progress
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    Json(Status {
        version: env!("CARGO_PKG_VERSION"),
        allow_writes: state.allow_writes,
        scan,
    })
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
    }
}

#[derive(Serialize)]
struct SetupResponse {
    setup: LibrarySetup,
    folder: Option<FolderStatus>,
    groups: &'static [rules::TagGroup],
    kinds: &'static [rules::TagKind],
    essentials: &'static [&'static str],
    album_kinds: &'static [&'static str],
    not_editable: &'static [&'static str],
    suggestions: &'static [&'static str],
    variables: Vec<Variable>,
    art_file_names: &'static [&'static str],
}

fn setup_response(setup: LibrarySetup) -> SetupResponse {
    let mut variables = vec![Variable {
        id: template::YEAR,
        label: "Year (from date)",
        group: "core",
    }];
    variables.extend(rules::CATALOG.iter().map(|k| Variable {
        id: k.id,
        label: k.label,
        group: k.group,
    }));
    SetupResponse {
        folder: setup.folder.as_deref().map(folder_status),
        setup,
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
    blocking(&state, |s| Ok(setup_response(s.lib.library_setup()?)))
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
    let (response, moved) = blocking(&state, move |s| {
        let before = s.lib.library_setup()?;
        let setup = setup.normalize()?;
        must_be_folder(setup.folder.as_deref(), "library folder")?;
        inbox::check_folders(
            s.lib.inbox_setup()?.folder.as_deref(),
            setup.folder.as_deref(),
        )?;
        let saved = s.lib.set_library_setup(setup)?;
        // Suggestions joined with the old separator shouldn't linger.
        if before.rules.separator != saved.rules.separator {
            s.identify.clear();
        }
        let moved = before.folder != saved.folder;
        Ok((setup_response(saved), moved))
    })
    .await?;
    if moved {
        spawn_scan(state.clone());
    }
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
}

pub(crate) async fn serve_file(path: PathBuf, req: Request, cache: &'static str) -> Response {
    match ServeFile::new(path).oneshot(req).await {
        Ok(mut res) => {
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
    let path = blocking(&state, move |s| s.covers.thumbnail(&s.lib, &id, size)).await?;
    Ok(serve_file(path, req, "public, max-age=86400").await)
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
    let cache = "private, max-age=300";
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
}

#[derive(Serialize)]
struct TagWriteResult {
    /// The album's id after the edit; it changes when the album title does.
    album_id: String,
    files_written: usize,
    album: Option<AlbumDetail>,
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
) -> ApiResult<Json<TagWriteResult>> {
    // Library files are coversync's own; editing Inbox files in place needs --allow-writes.
    let lookup = id.clone();
    let in_library = blocking(&state, move |s| {
        Ok(s.lib
            .album(&lookup)?
            .is_some_and(|d| d.album.collection == Collection::Library))
    })
    .await?;
    if !state.allow_writes && !in_library {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "Editing Inbox files in place is disabled on this server (start coversyncd with --allow-writes). Import the album to edit its Library copy.".into(),
        ));
    }
    let _guard = state.jobs.lock().await;
    let result = blocking(&state, move |s| {
        let ids = s.lib.album_track_ids(&id)?;
        if ids.is_empty() {
            return Err(coversync_core::Error::NotFound);
        }
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
        let collection = if in_library {
            Collection::Library
        } else {
            Collection::Inbox
        };
        let stats: Vec<FileStat> = written
            .into_iter()
            .filter_map(|(path, root)| scan::stat(path, &root, collection))
            .collect();
        let count = stats.len();
        let first = stats.first().map(|f| f.path.clone());
        scan::index_files(&s.lib, stats, &s.progress)?;
        s.lib.remove_paths(&[])?;
        scan::refresh_covers(&s.lib, &s.covers, &s.progress)?;
        let new_id = match first {
            Some(p) => album_id(&album_dir(&p), tags::read(&p)?.album.as_deref()),
            None => id.clone(),
        };
        let album = s.lib.album(&new_id)?;
        Ok(TagWriteResult {
            album_id: new_id,
            files_written: count,
            album,
        })
    })
    .await?;
    crate::release_memory();
    Ok(Json(result))
}

#[cfg(test)]
mod tests {
    use super::*;

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
