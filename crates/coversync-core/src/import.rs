//! Bringing an Inbox album into the Library.
//!
//! `plan` reports what an import would do and what stands in its way: tags
//! the rules or the folder structure need but the files lack, and problems
//! such as a missing library folder. `execute` re-plans with the user's edits
//! and, if nothing is missing, copies each file into place with the new tags,
//! then places the album art as Library setup's art rule says.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use rayon::prelude::*;
use serde::Serialize;

use crate::art::{Art, Placement};
use crate::cover::CoverCache;
use crate::db::{CoverSource, FileStat};
use crate::inbox::IngestMode;
use crate::model::Collection;
use crate::scan::{self, Progress};
use crate::split::Splitter;
use crate::template::{self, Template};
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
    /// What the import sets by itself, by kind: empty tags Inbox setup's
    /// autofill fills, names split at Library setup's phrases, and its
    /// substitutions (edits override these).
    pub autofilled: HashMap<&'static str, String>,
}

/// What happens to the album art on import.
#[derive(Debug, Clone, Serialize)]
pub struct ArtPlan {
    pub required: bool,
    pub placement: Placement,
    /// Where the album's art comes from, e.g. "folder.jpg"; None if it has none.
    pub source: Option<String>,
    /// The art's size in pixels.
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// The cover file the import writes beside the tracks, e.g. "cover.jpg".
    pub file: Option<String>,
    /// The rule's cover file name, without extension.
    pub file_name: String,
    /// Tracks without embedded art that get the album's; every track for a
    /// chosen image, which replaces what they have.
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
    /// Album-wide kinds the folder structure uses that differ between
    /// tracks, so the album would be split across folders.
    pub disagree: Vec<&'static str>,
    pub tracks: Vec<ImportTrack>,
    /// Things blocking the import other than missing tags.
    pub problems: Vec<String>,
    /// Kinds autofill sets on at least one track, in catalog order.
    pub autofilled: Vec<&'static str>,
    /// Kinds naming people that the import splits into one name each, at
    /// Library setup's phrases, on at least one track; in catalog order.
    pub split: Vec<&'static str>,
    /// Kinds Library setup's substitutions change on at least one track, in
    /// catalog order.
    pub substituted: Vec<&'static str>,
    pub art: ArtPlan,
    /// Smart suggestions are on: the import dialog looks the album up.
    pub suggest: bool,
    /// With Inbox setup's Preserve modification date: the date added (Unix
    /// seconds) the album keeps in the Library, its date in the Inbox. None:
    /// the import time.
    pub added: Option<i64>,
}

impl ImportPlan {
    pub fn ready(&self) -> bool {
        self.failing.is_empty()
            && self.disagree.is_empty()
            && self.problems.is_empty()
            && !self.art.missing
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
#[cfg(unix)]
pub fn writable(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(path.as_os_str().as_bytes())
        // SAFETY: a valid NUL-terminated path; access only reads it.
        .is_ok_and(|c| unsafe { libc::access(c.as_ptr(), libc::W_OK) } == 0)
}

/// Without `access(2)`, the read-only attribute is all there is to go by.
#[cfg(not(unix))]
pub fn writable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| !m.permissions().readonly())
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
/// looks across the whole album. Names a file holds several times are read
/// together where `split` would split one (see [`tags::read_kinds_split`]).
pub fn read_album(
    lib: &Library,
    detail: &crate::AlbumDetail,
    autofill: &autofill::Autofill,
    pictures: bool,
    split: &Splitter,
) -> Result<Vec<ReadTrack>> {
    let mut paths = Vec::new();
    let mut infos = Vec::new();
    let mut extra = Vec::new();
    for track in &detail.tracks {
        let (path, _) = lib.track_root(track.id)?.ok_or(Error::NotFound)?;
        let read = tags::read_kinds_split(&path, pictures, split)?;
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

/// Album-wide kinds among the folder structure's `variables` on which
/// `tracks` differ, which would spread the album over several folders.
/// Dates count by year when the structure shows only the year. A track
/// without a value differs from one with it: an optional part would be left
/// out for one and not the other.
fn disagreeing(
    variables: &[&'static str],
    tracks: &[&HashMap<&'static str, String>],
) -> Vec<&'static str> {
    let mut out = Vec::new();
    for v in variables {
        let (kind, year) = if *v == template::YEAR {
            ("date", true)
        } else {
            (*v, false)
        };
        if !rules::ALBUM_KINDS.contains(&kind) || out.contains(&kind) {
            continue;
        }
        let mut seen: Vec<&str> = Vec::new();
        for track in tracks {
            let value = track.get(kind).map_or("", |v| v.trim());
            let value = if year {
                value.get(..4).unwrap_or(value)
            } else {
                value
            };
            if !seen.contains(&value) {
                seen.push(value);
            }
        }
        if seen.len() > 1 {
            out.push(kind);
        }
    }
    out
}

/// What to change so tracks stop sharing a file name, given the kinds the
/// folder structure `uses` and whether the clashing tracks are on different discs.
fn clash_fix(uses: &[&str], discs_differ: bool) -> &'static str {
    if !uses.contains(&"track_number") {
        "add something like {track_number} to the folder structure"
    } else if discs_differ && !uses.contains(&"disc_number") {
        "they're on different discs, so add {disc_number} to the folder structure, as in <{disc_number}->{track_number} - {title}"
    } else {
        "add a tag that tells them apart to the folder structure"
    }
}

/// The tags a track's copy is written with: autofilled ones first, then the
/// user's edits on top (an emptied value removes the tag from the copy, as
/// the plan assumed).
fn written_values(
    autofilled: &HashMap<&'static str, String>,
    edits: Option<&HashMap<String, String>>,
) -> Vec<(&'static str, String)> {
    let mut values: Vec<(&'static str, String)> =
        autofilled.iter().map(|(k, v)| (*k, v.clone())).collect();
    for (k, v) in edits.into_iter().flatten() {
        if let Some(kind) = rules::kind(k) {
            values.retain(|(x, _)| *x != kind.id);
            values.push((kind.id, v.trim().to_string()));
        }
    }
    values
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
    let reads = template.value_kinds();
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
        // A Library database that couldn't be opened is tried again by the
        // import itself (`prepare_library`), before anything is copied.
        _ => {}
    }

    let art_rules = &setup.rules.art;
    let splitter = setup.rules.splitter();
    let read = read_album(
        lib,
        &detail,
        &inbox.autofill,
        art_rules.placement.embedded(),
        &splitter,
    )?;
    let gets_art = if upload.is_some() {
        read.len()
    } else {
        read.iter().filter(|t| !t.has_art).count()
    } as u32;

    let mut tracks = Vec::new();
    // Each track's disc, to tell why file names clash.
    let mut discs = Vec::new();
    // Whether each track's album has one disc, as optional parts see it.
    let mut one_disc = Vec::new();
    // Variables in some track's path: what the album's tracks have to
    // agree on. One only in optional parts every track leaves out can't
    // split the album.
    let mut shown_vars: Vec<&'static str> = Vec::new();
    // Per track, the kinds autofill filled, split at phrases and
    // substitutions changed, while no edit replaces them.
    let mut filled_by = Vec::new();
    let mut split_by = Vec::new();
    let mut substituted_by = Vec::new();
    for (track, read) in detail.tracks.iter().zip(read) {
        let path = read.path;
        let mut values = read.values;
        // After autofill, so an album artist filled from the artist is split too.
        let (split, substituted) = splitter.rewrite_all(&mut values);
        let mut autofilled: HashMap<&'static str, String> = read
            .autofilled
            .iter()
            .chain(&split)
            .chain(&substituted)
            .map(|k| (*k, values.get(k).cloned().unwrap_or_default()))
            .collect();
        let mut typed_ok = true;
        for (kind, value) in edits.get(&track.id).into_iter().flatten() {
            let Some(k) = rules::kind(kind) else {
                return Err(Error::Invalid(format!("unknown tag kind '{kind}'")));
            };
            if let Err(e) = tags::validate_kind(k.id, value) {
                problems.push(format!("Track {}: {e}", track.track_no.unwrap_or(0)));
                typed_ok = false;
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
        // What the copy will be written with, checked against the file as it
        // is: a value its format can't hold makes the plan unready, rather
        // than fail the import part way through the copies.
        let to_write = written_values(&autofilled, edits.get(&track.id));
        if typed_ok
            && !to_write.is_empty()
            && let Err(e) = tags::check_kinds(&path, &crate::model::TagEdit::default(), &to_write)
        {
            problems.push(format!("Track {}: {e}", track.track_no.unwrap_or(0)));
        }
        filled_by.push(
            read.autofilled
                .iter()
                .copied()
                .filter(|k| autofilled.contains_key(k))
                .collect::<Vec<_>>(),
        );
        let kept = |kinds: Vec<&'static str>| -> Vec<&'static str> {
            kinds
                .into_iter()
                .filter(|k| autofilled.contains_key(k))
                .collect()
        };
        split_by.push(kept(split));
        substituted_by.push(kept(substituted));
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
        one_disc.push(template::one_disc(&values));
        for v in template.shown_variables(&values) {
            if !shown_vars.contains(&v) {
                shown_vars.push(v);
            }
        }
        discs.push(values.get("disc_number").map(|v| {
            let n = v.split('/').next().unwrap_or(v).trim();
            n.trim_start_matches('0').to_string()
        }));
        // What's required, and what the structure reads, so that what it
        // needs alike on every track can be checked.
        let shown: HashMap<&'static str, String> = values
            .into_iter()
            .filter(|(k, _)| kinds.contains(k) || reads.contains(k))
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
    // The first track at each file name, and its disc.
    let mut first: HashMap<&str, Option<&str>> = HashMap::new();
    let (mut clashes, mut discs_differ) = (0, false);
    for ((_, t), disc) in tracks.iter().zip(&discs) {
        let Some(dest) = t.dest.as_deref() else {
            continue;
        };
        match first.get(dest) {
            None => {
                first.insert(dest, disc.as_deref());
            }
            Some(other) => {
                clashes += 1;
                discs_differ |= *other != disc.as_deref();
            }
        }
    }
    if clashes > 0 {
        problems.push(format!(
            "{clashes} {} would land on the same file name; {}.",
            if clashes == 1 { "track" } else { "tracks" },
            clash_fix(&template.variables(), discs_differ)
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

    let failing: Vec<&'static str> = kinds
        .iter()
        .copied()
        .filter(|k| tracks.iter().any(|(_, t)| t.missing.contains(k)))
        .collect();
    let values: Vec<_> = tracks.iter().map(|(_, t)| &t.values).collect();
    let mut disagree = disagreeing(&shown_vars, &values);
    // One album's discs are named alike only if every track counts the
    // same number of discs.
    if template.counts_discs()
        && one_disc.iter().any(|o| *o != one_disc[0])
        && !disagree.contains(&"disc_total")
    {
        disagree.push("disc_total");
    }
    // Missing on some tracks already; that says what to fill in.
    disagree.retain(|k| !failing.contains(k));
    let in_order = |per_track: &[Vec<&'static str>]| -> Vec<&'static str> {
        rules::CATALOG
            .iter()
            .map(|k| k.id)
            .filter(|k| per_track.iter().any(|t| t.contains(k)))
            .collect()
    };
    let autofilled = in_order(&filled_by);
    let split = in_order(&split_by);
    let substituted = in_order(&substituted_by);

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
            gets_art
        } else {
            0
        },
        missing: art_rules.required && art.is_none(),
        width: art.as_ref().map(|(a, _)| a.width),
        height: art.as_ref().map(|(a, _)| a.height),
        source: art.map(|(_, from)| from),
    };
    Ok(ImportPlan {
        art,
        autofilled,
        split,
        substituted,
        added: (inbox.keep_added && detail.album.added_at > 0).then_some(detail.album.added_at),
        suggest: inbox.suggestions.enabled,
        album_id: album_id.to_string(),
        mode: inbox.ingest,
        library_folder: folder,
        kinds,
        failing,
        disagree,
        tracks: tracks.into_iter().map(|(_, t)| t).collect(),
        problems,
    })
}

/// Import `album_id` with `edits`, and `upload` as its art if given (an
/// uploaded image or MusicBrainz's, in place of the album's own). Fails
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
    // The Library folder may hold its own database (with its own rules) that
    // isn't open yet.
    lib.refresh_library()?;
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
        if !plan.disagree.is_empty() {
            let labels: Vec<&str> = plan
                .disagree
                .iter()
                .map(|k| rules::kind(k).map_or(*k, |k| k.label))
                .collect();
            reasons.push(format!(
                "The tracks differ in {}, which the folder structure needs the same for the whole album; give them all the same value.",
                labels.join(", ")
            ));
        }
        if plan.art.missing {
            reasons.push("The library rules require album art; choose an image.".into());
        }
        return Err(Error::Invalid(reasons.join(" ")));
    }
    let art_rules = lib.library_setup()?.rules.art;
    // An image chosen for the import replaces the art the tracks have.
    let replace = upload.is_some();
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
    // The first import creates the Library folder and its database; nothing
    // is copied, hidden or deleted unless the database is there to index it.
    lib.prepare_library(&folder)?;

    let mut sources = Vec::new();
    let mut written: Vec<PathBuf> = Vec::new();
    let mut hashes = HashMap::new();
    let mut art_files: Vec<PathBuf> = Vec::new();
    let mut embedded = 0;
    let copied = (|| -> Result<()> {
        let copies: Vec<_> = plan
            .tracks
            .par_iter()
            .map(|track| -> Result<_> {
                let (src, _) = lib.track_root(track.id)?.ok_or(Error::NotFound)?;
                let dest = PathBuf::from(
                    track
                        .dest
                        .as_deref()
                        .expect("ready plans have destinations"),
                );
                let values = written_values(&track.autofilled, edits.get(&track.id));
                let (added, hash) =
                    tags::copy_with_tags_hashed(&src, &dest, &values, picture.as_ref(), replace)?;
                Ok((src, dest, added, hash))
            })
            .collect();
        // Join every copy before rolling back: another worker must never
        // publish a file after failure cleanup has already visited it.
        let mut failed = None;
        for copy in copies {
            match copy {
                Ok((src, dest, added, hash)) => {
                    hashes.insert(dest.clone(), hash);
                    embedded += usize::from(added);
                    written.push(dest);
                    sources.push(src);
                }
                Err(e) => {
                    if failed.is_none() {
                        failed = Some(e);
                    }
                }
            }
        }
        if let Some(e) = failed {
            return Err(e);
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
    // Index the copies before the originals leave the Inbox, so a failure
    // here leaves them where they were, and takes the copies back out. They
    // count as added now, or at the album's Inbox date (Preserve
    // modification date).
    let stats = imported_stats(&written, &folder, plan.added.unwrap_or(now))
        .into_iter()
        .map(|mut stat| {
            stat.hash = hashes.remove(&stat.path);
            stat
        })
        .collect();
    if let Err(e) = scan::index_written_files(lib, stats, progress) {
        // Rows are kept by the resolved Library folder plus each file's path
        // under it as written (folders below it aren't resolved).
        let indexed: Vec<PathBuf> = match folder.canonicalize() {
            Ok(root) => written
                .iter()
                .filter_map(|p| p.strip_prefix(&folder).ok().map(|rel| root.join(rel)))
                .collect(),
            Err(_) => Vec::new(),
        };
        let _ = lib.remove_paths(&indexed);
        for f in written.iter().chain(&art_files) {
            let _ = fs::remove_file(f);
        }
        return Err(e);
    }
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
    scan::refresh_covers(lib, covers, progress)?;

    let first = &written[0];
    let album = tags::read(first)?.album;
    Ok(ImportResult {
        album_id: scan::album_id_in(
            Collection::Library,
            &folder,
            &scan::album_dir_in(Collection::Library, &folder, first),
            album.as_deref(),
        ),
        files: written.len(),
        dest_dir: dest_dir.display().to_string(),
        mode: plan.mode,
        art: art_summary(art_files.first(), embedded),
    })
}

/// The files an import wrote into `folder`, ready to index as added at
/// `added`: the import time, or the album's Inbox date.
fn imported_stats(written: &[PathBuf], folder: &Path, added: i64) -> Vec<FileStat> {
    written
        .iter()
        .filter_map(|p| scan::stat(p.clone(), folder, Collection::Library))
        .map(|s| FileStat {
            added: Some(added),
            ..s
        })
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imported_files_are_indexed_as_added_at_the_import() {
        let dir = std::env::temp_dir().join(format!("coversync-imported-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("A")).unwrap();
        let file = dir.join("A/1.flac");
        fs::write(&file, b"").unwrap();
        let stats = imported_stats(std::slice::from_ref(&file), &dir, 1234);
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].added, Some(1234));
        assert_eq!(stats[0].collection, Collection::Library);
        assert_eq!(stats[0].root, dir);
    }

    #[test]
    fn album_wide_values_that_differ_would_split_the_album() {
        let track = |pairs: &[(&'static str, &str)]| -> HashMap<&'static str, String> {
            pairs.iter().map(|(k, v)| (*k, v.to_string())).collect()
        };
        let a = track(&[
            ("album", "chiptune collection 1"),
            ("date", "2017-03-08"),
            ("title", "a"),
        ]);
        let b = track(&[
            ("album", "Chiptune collection 1"),
            ("date", "2017"),
            ("title", "b"),
        ]);
        let c = track(&[("album", "chiptune collection 1"), ("title", "c")]);
        let vars = ["album_artist", "album", "year", "track_number", "title"];
        // Case alone makes another folder; one year is one folder.
        assert_eq!(disagreeing(&vars, &[&a, &b]), ["album"]);
        // A track without a date differs from those with one.
        assert_eq!(disagreeing(&vars, &[&a, &b, &c]), ["album", "date"]);
        // None has an album artist: that's alike.
        assert!(!disagreeing(&vars, &[&a, &c]).contains(&"album_artist"));
        // The full date, when the structure shows it.
        assert_eq!(disagreeing(&["date"], &[&a, &b]), ["date"]);
        // Titles are per track.
        assert!(disagreeing(&["title"], &[&a, &b]).is_empty());
    }

    #[test]
    fn clash_advice_names_what_is_missing() {
        let fix = |uses: &[&str], discs| clash_fix(uses, discs);
        assert!(fix(&["album", "title"], false).contains("{track_number}"));
        // The structure has track numbers; the discs repeat them.
        let per_disc = fix(&["album", "track_number", "title"], true);
        assert!(per_disc.contains("add {disc_number}"), "{per_disc}");
        let both = fix(&["disc_number", "track_number", "title"], true);
        assert!(!both.contains("add {"), "{both}");
        assert!(!fix(&["track_number", "title"], false).contains("{disc_number}"));
    }
}
