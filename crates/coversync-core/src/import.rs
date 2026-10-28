//! Bringing an Inbox album into the Library.
//!
//! `plan` reports what an import would do and what stands in its way: tags
//! the rules or the folder structure need but the files lack, and problems
//! such as a missing library folder. `execute` re-plans with the user's edits
//! and, if nothing is missing, copies each file into place with the new tags,
//! then places the album art as Library setup's art rule says.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::art::{Art, Placement};
use crate::cover::CoverCache;
use crate::db::CoverSource;
use crate::inbox::IngestMode;
use crate::model::Collection;
use crate::scan::{self, Progress};
use crate::template::Template;
use crate::{Error, Library, Result, autofill, rules, tags};

/// Tag values typed in by the user: track id -> kind id -> value.
pub type Edits = HashMap<i64, HashMap<String, String>>;

#[derive(Debug, Clone, Serialize)]
pub struct ImportTrack {
    pub id: i64,
    pub title: String,
    pub file_name: String,
    /// Current values (with edits applied) for the kinds the import needs.
    pub values: HashMap<&'static str, String>,
    /// Needed kinds this track still has no value for.
    pub missing: Vec<&'static str>,
    /// Where the file would land, once every folder-structure variable has a value.
    pub dest: Option<String>,
    /// Empty tags Inbox setup's autofill would set, by kind (edits override these).
    pub autofilled: HashMap<&'static str, String>,
}

/// What happens to the album art on import.
#[derive(Debug, Clone, Serialize)]
pub struct ArtPlan {
    pub required: bool,
    pub placement: Placement,
    /// Where the album's art comes from, e.g. "folder.jpg"; None if it has none.
    pub source: Option<String>,
    /// The cover file the import writes beside the tracks, e.g. "cover.jpg".
    pub file: Option<String>,
    /// The rule's cover file name, without extension.
    pub file_name: String,
    /// Tracks without embedded art that get the album's.
    pub embed: u32,
    /// The rules require art and the album has none: the import needs an image.
    pub missing: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImportPlan {
    pub album_id: String,
    pub mode: IngestMode,
    pub library_folder: Option<String>,
    /// Kinds every file needs: required by the rules or used by the folder structure.
    pub kinds: Vec<&'static str>,
    /// Kinds at least one track is missing.
    pub failing: Vec<&'static str>,
    pub tracks: Vec<ImportTrack>,
    /// Things blocking the import other than missing tags.
    pub problems: Vec<String>,
    /// Kinds autofill sets on at least one track, in catalog order.
    pub autofilled: Vec<&'static str>,
    pub art: ArtPlan,
    /// Smart suggestions are on: the import dialog looks the album up.
    pub suggest: bool,
}

impl ImportPlan {
    pub fn ready(&self) -> bool {
        self.failing.is_empty() && self.problems.is_empty() && !self.art.missing
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ImportResult {
    /// The album's id in the Library.
    pub album_id: String,
    pub files: usize,
    pub dest_dir: String,
    pub mode: IngestMode,
    /// What happened to the album art, e.g. "cover.jpg" or "embedded in 12 files".
    pub art: Option<String>,
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The album's art as the cover pass found it, and where it came from.
fn found_art(lib: &Library, album_id: &str) -> Option<(Art, String)> {
    let source = lib.cover_source(album_id).ok()??;
    let (data, from) = match &source {
        CoverSource::File(p) => (fs::read(p).ok()?, file_name(p)),
        CoverSource::Embedded(p) => (
            tags::embedded_cover(p).ok()??,
            format!("the art embedded in {}", file_name(p)),
        ),
    };
    match Art::new(data) {
        Ok(art) => Some((art, from)),
        Err(e) => {
            tracing::warn!(album = album_id, "album art unusable: {e}");
            None
        }
    }
}

/// Whether this process may create files at `path`, judged by the nearest
/// existing ancestor (missing folders are created on import).
pub fn creatable(path: &Path) -> bool {
    let mut p = Some(path);
    while let Some(dir) = p {
        if dir.exists() {
            return writable(dir);
        }
        p = dir.parent();
    }
    false
}

/// `access(2)` rather than mode bits, so read-only mounts count as not writable.
pub fn writable(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(path.as_os_str().as_bytes())
        // SAFETY: a valid NUL-terminated path; access only reads it.
        .is_ok_and(|c| unsafe { libc::access(c.as_ptr(), libc::W_OK) } == 0)
}

/// Tag kinds every file must have before it can be imported: the rules'
/// required tags plus whatever the folder structure uses.
pub fn required_kinds(setup: &crate::LibrarySetup) -> Vec<&'static str> {
    match Template::parse(&setup.structure) {
        Ok(t) => needed_kinds(&setup.rules.required_tags, &t),
        Err(_) => needed_kinds(
            &setup.rules.required_tags,
            &Template::parse(crate::template::DEFAULT).expect("default parses"),
        ),
    }
}

fn needed_kinds(required: &[String], template: &Template) -> Vec<&'static str> {
    let template_kinds = template.required_kinds();
    rules::CATALOG
        .iter()
        .map(|k| k.id)
        .filter(|id| required.iter().any(|r| r == id) || template_kinds.contains(id))
        .collect()
}

/// A track's tags as an import would start from them.
pub struct ReadTrack {
    pub path: PathBuf,
    /// The file's values with autofill's assumptions added.
    pub values: HashMap<&'static str, String>,
    /// Kinds autofill set.
    pub autofilled: Vec<&'static str>,
    /// Embedded art (only looked for when asked).
    pub has_art: bool,
    pub tag_type: lofty::tag::TagType,
}

/// Read every track of `detail`, in order, and apply `autofill`, which
/// looks across the whole album.
pub fn read_album(
    lib: &Library,
    detail: &crate::AlbumDetail,
    autofill: &autofill::Autofill,
    pictures: bool,
) -> Result<Vec<ReadTrack>> {
    let mut paths = Vec::new();
    let mut infos = Vec::new();
    let mut extra = Vec::new();
    for track in &detail.tracks {
        let (path, _) = lib.track_root(track.id)?.ok_or(Error::NotFound)?;
        let read = tags::read_kinds(&path, pictures)?;
        infos.push(autofill::TrackInfo {
            stem: path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            folder: path
                .parent()
                .and_then(Path::file_name)
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            values: read.values,
        });
        extra.push((read.has_art, read.tag_type));
        paths.push(path);
    }
    let album_folder = Path::new(&detail.dir)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let filled = autofill::fill(autofill, &album_folder, &mut infos);
    Ok(paths
        .into_iter()
        .zip(infos)
        .zip(filled)
        .zip(extra)
        .map(
            |(((path, info), autofilled), (has_art, tag_type))| ReadTrack {
                path,
                values: info.values,
                autofilled,
                has_art,
                tag_type,
            },
        )
        .collect())
}

/// What importing `album_id` would do, with `edits` applied and `upload` as
/// the album art (instead of what the album has).
pub fn plan(
    lib: &Library,
    album_id: &str,
    edits: &Edits,
    upload: Option<&Art>,
) -> Result<ImportPlan> {
    let setup = lib.library_setup()?;
    let inbox = lib.inbox_setup()?;
    let detail = lib.album(album_id)?.ok_or(Error::NotFound)?;
    let template = Template::parse(&setup.structure)?;
    let kinds = needed_kinds(&setup.rules.required_tags, &template);
    let folder = setup.folder.clone();
    let mut problems = Vec::new();

    if detail.album.collection == Collection::Library {
        problems.push("This album is already in the Library.".into());
    }
    match &folder {
        None => problems.push("Choose a library folder in Library setup first.".into()),
        Some(f) if !creatable(Path::new(f)) => {
            problems.push(format!("coversyncd can't write to the library folder {f}."))
        }
        _ => {}
    }

    let art_rules = &setup.rules.art;
    let read = read_album(
        lib,
        &detail,
        &inbox.autofill,
        art_rules.placement.embedded(),
    )?;
    let without_art = read.iter().filter(|t| !t.has_art).count() as u32;

    let mut tracks = Vec::new();
    for (track, read) in detail.tracks.iter().zip(read) {
        let path = read.path;
        let mut values = read.values;
        let mut autofilled: HashMap<&'static str, String> = read
            .autofilled
            .iter()
            .map(|k| (*k, values.get(k).cloned().unwrap_or_default()))
            .collect();
        for (kind, value) in edits.get(&track.id).into_iter().flatten() {
            let Some(k) = rules::kind(kind) else {
                return Err(Error::Invalid(format!("unknown tag kind '{kind}'")));
            };
            if let Err(e) = tags::validate_kind(k.id, value) {
                problems.push(format!("Track {}: {e}", track.track_no.unwrap_or(0)));
            }
            let value = value.trim();
            // What the user typed (or cleared) wins over any assumption.
            autofilled.remove(k.id);
            if value.is_empty() {
                values.remove(k.id);
            } else {
                values.insert(k.id, value.to_string());
            }
        }
        let missing: Vec<&'static str> = kinds
            .iter()
            .copied()
            .filter(|k| !values.contains_key(k))
            .collect();
        let extension = path
            .extension()
            .map(|e| e.to_string_lossy().into_owned())
            .unwrap_or_default();
        let dest = folder.as_ref().and_then(|f| {
            template
                .render(&values, &extension)
                .ok()
                .map(|rel| Path::new(f).join(rel))
        });
        let shown: HashMap<&'static str, String> = values
            .into_iter()
            .filter(|(k, _)| kinds.contains(k))
            .collect();
        tracks.push((
            path,
            ImportTrack {
                id: track.id,
                title: track.title.clone(),
                file_name: track
                    .rel_path
                    .rsplit('/')
                    .next()
                    .unwrap_or(&track.rel_path)
                    .to_string(),
                values: shown,
                missing,
                dest: dest.map(|d| d.display().to_string()),
                autofilled,
            },
        ));
    }

    if inbox.ingest == IngestMode::Move
        && let Some((path, _)) = tracks.first()
        && !path.parent().is_some_and(writable)
    {
        problems.push("Move needs to delete the originals, but the Inbox folder is read-only to coversyncd. Switch to Copy in Inbox setup.".into());
    }
    let mut seen = HashSet::new();
    let clashes = tracks
        .iter()
        .filter_map(|(_, t)| t.dest.as_ref())
        .filter(|d| !seen.insert(d.as_str()))
        .count();
    if clashes > 0 {
        problems.push(format!(
            "{clashes} {} would land on the same file name; add something like {{track_number}} to the folder structure.",
            if clashes == 1 { "track" } else { "tracks" }
        ));
    }
    let existing = tracks
        .iter()
        .filter_map(|(_, t)| t.dest.as_ref())
        .filter(|d| Path::new(d).exists())
        .count();
    if existing > 0 {
        problems.push(format!(
            "{existing} {} already exist in the Library.",
            if existing == 1 { "file" } else { "files" }
        ));
    }

    let failing = kinds
        .iter()
        .copied()
        .filter(|k| tracks.iter().any(|(_, t)| t.missing.contains(k)))
        .collect();
    let autofilled = rules::CATALOG
        .iter()
        .map(|k| k.id)
        .filter(|k| tracks.iter().any(|(_, t)| t.autofilled.contains_key(k)))
        .collect();

    let found = upload.is_none().then(|| found_art(lib, album_id)).flatten();
    let art = upload
        .map(|a| (a, "the image you chose".to_string()))
        .or_else(|| found.as_ref().map(|(a, from)| (a, from.clone())));
    let art = ArtPlan {
        required: art_rules.required,
        placement: art_rules.placement,
        file_name: art_rules.file_name.clone(),
        file: art
            .as_ref()
            .filter(|_| art_rules.placement.file())
            .map(|(a, _)| art_rules.file_for(a)),
        embed: if art.is_some() && art_rules.placement.embedded() {
            without_art
        } else {
            0
        },
        missing: art_rules.required && art.is_none(),
        source: art.map(|(_, from)| from),
    };
    Ok(ImportPlan {
        art,
        autofilled,
        suggest: inbox.suggestions.enabled,
        album_id: album_id.to_string(),
        mode: inbox.ingest,
        library_folder: folder,
        kinds,
        failing,
        tracks: tracks.into_iter().map(|(_, t)| t).collect(),
        problems,
    })
}

/// Import `album_id` with `edits`, and `upload` as its art if given. Fails
/// without changing anything if the plan isn't ready; a failure part-way
/// removes the files already written.
pub fn execute(
    lib: &Library,
    covers: &CoverCache,
    album_id: &str,
    edits: &Edits,
    upload: Option<Art>,
    progress: &Progress,
) -> Result<ImportResult> {
    let plan = plan(lib, album_id, edits, upload.as_ref())?;
    if !plan.ready() {
        let mut reasons = plan.problems.clone();
        if !plan.failing.is_empty() {
            let labels: Vec<&str> = plan
                .failing
                .iter()
                .map(|k| rules::kind(k).map_or(*k, |k| k.label))
                .collect();
            reasons.push(format!("Missing tags: {}.", labels.join(", ")));
        }
        if plan.art.missing {
            reasons.push("The library rules require album art; choose an image.".into());
        }
        return Err(Error::Invalid(reasons.join(" ")));
    }
    let art_rules = lib.library_setup()?.rules.art;
    let art = upload.or_else(|| found_art(lib, album_id).map(|(a, _)| a));
    let picture = match &art {
        Some(a) if plan.art.placement.embedded() => Some(a.picture()?),
        _ => None,
    };
    let folder = PathBuf::from(
        plan.library_folder
            .as_deref()
            .expect("ready plans have a folder"),
    );

    let mut sources = Vec::new();
    let mut written: Vec<PathBuf> = Vec::new();
    let mut art_files: Vec<PathBuf> = Vec::new();
    let mut embedded = 0;
    let copied = (|| -> Result<()> {
        for track in &plan.tracks {
            let (src, _) = lib.track_root(track.id)?.ok_or(Error::NotFound)?;
            let dest = PathBuf::from(
                track
                    .dest
                    .as_deref()
                    .expect("ready plans have destinations"),
            );
            // Autofilled tags first, then the user's edits on top (an emptied
            // value removes the tag from the copy, as the plan assumed).
            let mut values: Vec<(&str, String)> = track
                .autofilled
                .iter()
                .map(|(k, v)| (*k, v.clone()))
                .collect();
            for (k, v) in edits.get(&track.id).into_iter().flatten() {
                if let Some(kind) = rules::kind(k) {
                    values.retain(|(x, _)| *x != kind.id);
                    values.push((kind.id, v.trim().to_string()));
                }
            }
            let added = tags::copy_with_tags(&src, &dest, &values, picture.as_ref())?;
            embedded += usize::from(added);
            written.push(dest);
            sources.push(src);
        }
        // The cover file goes in every folder the tracks landed in (one per
        // disc, say), under the rule's name, unless one is already there.
        if let Some(art) = art.as_ref().filter(|_| plan.art.placement.file()) {
            let bytes = art.file_bytes()?;
            let mut dirs: Vec<&Path> = written.iter().filter_map(|p| p.parent()).collect();
            dirs.sort();
            dirs.dedup();
            for dir in dirs {
                let has_cover = fs::read_dir(dir)?
                    .filter_map(|e| e.ok())
                    .any(|e| art_rules.is_cover_file(&e.file_name().to_string_lossy()));
                if has_cover {
                    continue;
                }
                let target = dir.join(art_rules.file_for(art));
                let tmp = dir.join(format!(".coversync-tmp-{}", file_name(&target)));
                fs::write(&tmp, &bytes)
                    .and_then(|_| fs::rename(&tmp, &target))
                    .inspect_err(|_| {
                        let _ = fs::remove_file(&tmp);
                    })?;
                art_files.push(target);
            }
        }
        Ok(())
    })();
    if let Err(e) = copied {
        for f in written.iter().chain(&art_files) {
            let _ = fs::remove_file(f);
        }
        return Err(e);
    }
    let dest_dir = written[0].parent().unwrap_or(&folder).to_path_buf();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    match plan.mode {
        IngestMode::Copy => lib.hide_paths(&sources, now)?,
        IngestMode::Move => {
            for src in &sources {
                if let Err(e) = fs::remove_file(src) {
                    tracing::warn!(path = %src.display(), "original not removed: {e}");
                }
            }
            lib.remove_paths(&sources)?;
        }
    }
    let stats = written
        .iter()
        .filter_map(|p| scan::stat(p.clone(), &folder, Collection::Library))
        .collect();
    scan::index_files(lib, stats, progress)?;
    scan::refresh_covers(lib, covers, progress)?;

    let first = &written[0];
    let album = tags::read(first)?.album;
    Ok(ImportResult {
        album_id: scan::album_id(&scan::album_dir(first), album.as_deref()),
        files: written.len(),
        dest_dir: dest_dir.display().to_string(),
        mode: plan.mode,
        art: art_summary(art_files.first(), embedded),
    })
}

fn art_summary(file: Option<&PathBuf>, embedded: usize) -> Option<String> {
    let file = file.map(|f| file_name(f));
    let embedded = (embedded > 0).then(|| {
        format!(
            "art embedded in {embedded} {}",
            if embedded == 1 { "file" } else { "files" }
        )
    });
    match (file, embedded) {
        (Some(f), Some(e)) => Some(format!("{f}, {e}")),
        (f, e) => f.or(e),
    }
}
