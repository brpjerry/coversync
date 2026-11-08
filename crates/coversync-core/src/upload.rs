//! Uploads into the Inbox: albums sent in chunks, staged where the scanner
//! doesn't look, then moved into the uploads folder whole.
//!
//! Each upload becomes one new folder in the uploads folder: `Uploads` in
//! the Inbox folder, unless Inbox setup names another one (which lets an
//! Inbox coversyncd can't write to take uploads; the scan covers that folder
//! as part of the Inbox). An upload is staged in
//! `<uploads folder>/.coversync-uploads/<id>/`, on the same filesystem:
//!
//! - `manifest.json`: the folder's name and each file's path and size,
//!   written once when the upload starts.
//! - `progress.json`: the byte ranges of each file written so far, saved
//!   every few seconds ([`Uploads::save`]), so that an upload carries on
//!   after coversyncd restarts.
//! - `files/`: the files, at their paths. Chunks are written in place at
//!   their offsets, in any order and several at once, and a file is flushed
//!   to disk once, when its last byte is written.
//!
//! Finishing renames `files/` into the uploads folder under the upload's
//! folder name, or `Name (2)`, `Name (3)`… when that is taken. Nothing is
//! ever overwritten, and the album appears whole or not at all.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::{Error, Result, tags};

/// The staging folder, in the uploads folder. The scanner skips folders
/// with this name.
pub const STAGING: &str = ".coversync-uploads";
/// The uploads folder's name in the Inbox folder, unless Inbox setup names
/// another folder.
pub const DEFAULT_FOLDER: &str = "Uploads";
/// An upload no chunk has reached for this long (seconds) is removed.
pub const IDLE_LIMIT: i64 = 24 * 3600;
/// The chunk size clients are told to use. Any size works.
pub const CHUNK_SIZE: u64 = 16 << 20;
/// The most files one upload may hold.
pub const MAX_FILES: usize = 10_000;
/// Longest relative path, in bytes.
const MAX_PATH: usize = 1024;
/// Longest folder name, in bytes, leaving room for " (2)" within the 255
/// bytes a name can have.
const MAX_FOLDER: usize = 240;
/// Kept free on the uploads folder's disk beyond what uploads need.
const SPARE: u64 = 64 << 20;
/// Files that travel with albums, besides the audio coversync reads.
const OTHER_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "webp", "gif", "bmp", "tif", "tiff", "cue", "log", "txt", "nfo", "m3u",
    "m3u8", "accurip", "md5", "ffp", "sfv", "pdf",
];
const MANIFEST: &str = "manifest.json";
const PROGRESS: &str = "progress.json";
const FILES: &str = "files";

/// The file extensions an upload may carry, lowercase: the audio formats
/// coversync reads, images, cue sheets, rip logs and the like.
pub fn extensions() -> Vec<&'static str> {
    tags::AUDIO_EXTENSIONS
        .iter()
        .chain(OTHER_EXTENSIONS)
        .copied()
        .collect()
}

fn accepted(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| {
            let e = e.to_ascii_lowercase();
            tags::AUDIO_EXTENSIONS.contains(&e.as_str()) || OTHER_EXTENSIONS.contains(&e.as_str())
        })
}

/// An upload as a client starts it.
#[derive(Debug, Clone, Deserialize)]
pub struct NewUpload {
    /// The folder the files go in, inside the uploads folder: one name, such
    /// as the album folder's. Empty means `Upload`.
    #[serde(default)]
    pub folder: String,
    pub files: Vec<NewFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewFile {
    /// Relative to the upload's folder, with `/` between folders, as in
    /// `CD1/01 Intro.flac`.
    pub path: String,
    pub size: u64,
}

/// The upload's folder name, checked: one plain name.
fn check_folder(name: &str) -> Result<String> {
    let name = name.trim();
    let name = if name.is_empty() { "Upload" } else { name };
    if name.contains('/') {
        return Err(Error::Invalid(format!(
            "The upload's folder name “{name}” must be one name, without /."
        )));
    }
    check_name(name, name)?;
    if name.len() > MAX_FOLDER {
        return Err(Error::Invalid(format!(
            "The upload's folder name is too long: at most {MAX_FOLDER} bytes."
        )));
    }
    Ok(name.to_string())
}

/// One name in a path: not empty, `.` or `..`, not hidden, no control
/// characters or backslashes, and no longer than a name can be.
fn check_name(name: &str, path: &str) -> Result<()> {
    let bad = |why: &str| Err(Error::Invalid(format!("“{path}” {why}")));
    if name.is_empty() {
        return bad("has an empty folder name in it (two / in a row, or one at the end).");
    }
    if name == "." || name == ".." {
        return bad("can't contain . or .. folders.");
    }
    if name.starts_with('.') {
        return bad(
            "is hidden (its name starts with a dot); hidden files and folders aren't uploaded.",
        );
    }
    if name.contains('\\') {
        return bad("has a backslash in it; use / between folders.");
    }
    if name.chars().any(char::is_control) {
        return bad("has a control character in it.");
    }
    if name.len() > 255 {
        return bad("has a name longer than 255 bytes.");
    }
    Ok(())
}

/// Check the files of a new upload: relative paths without `..`, of
/// accepted kinds, each once, none both a file and a folder.
fn check_files(files: &[NewFile]) -> Result<()> {
    if files.is_empty() {
        return Err(Error::Invalid("An upload needs at least one file.".into()));
    }
    if files.len() > MAX_FILES {
        return Err(Error::Invalid(format!(
            "An upload can hold at most {MAX_FILES} files; split it into several."
        )));
    }
    let mut paths = HashSet::new();
    let mut folders = HashSet::new();
    for f in files {
        let path = f.path.as_str();
        if path.is_empty() {
            return Err(Error::Invalid("A file in the upload has no path.".into()));
        }
        if path.len() > MAX_PATH {
            return Err(Error::Invalid(format!(
                "“{path}” is too long: at most {MAX_PATH} bytes."
            )));
        }
        if path.starts_with('/') {
            return Err(Error::Invalid(format!(
                "“{path}” must be relative to the upload's folder, not start with /."
            )));
        }
        let names: Vec<&str> = path.split('/').collect();
        for name in &names {
            check_name(name, path)?;
        }
        if !accepted(path) {
            return Err(Error::Invalid(format!(
                "“{path}” isn't a kind of file uploads take: audio, images, cue sheets, rip logs and the like."
            )));
        }
        if !paths.insert(path) {
            return Err(Error::Invalid(format!("“{path}” is in the upload twice.")));
        }
        for end in 1..names.len() {
            folders.insert(names[..end].join("/"));
        }
    }
    if let Some(both) = paths.iter().find(|p| folders.contains(**p)) {
        return Err(Error::Invalid(format!(
            "“{both}” is both a file and a folder in the upload."
        )));
    }
    Ok(())
}

/// Byte ranges of a file, as sorted, disjoint `[start, end)` pairs.
type Ranges = Vec<(u64, u64)>;

/// Add `[start, end)`, merging it with the ranges it touches.
fn insert(ranges: &mut Ranges, start: u64, end: u64) {
    if start >= end {
        return;
    }
    let (mut start, mut end) = (start, end);
    let mut merged = Vec::with_capacity(ranges.len() + 1);
    let mut placed = false;
    for &(a, b) in ranges.iter() {
        if b < start {
            merged.push((a, b));
        } else if a > end {
            if !placed {
                merged.push((start, end));
                placed = true;
            }
            merged.push((a, b));
        } else {
            start = start.min(a);
            end = end.max(b);
        }
    }
    if !placed {
        merged.push((start, end));
    }
    *ranges = merged;
}

fn covered(ranges: &Ranges) -> u64 {
    ranges.iter().map(|(a, b)| b - a).sum()
}

fn complete(ranges: &Ranges, size: u64) -> bool {
    size == 0 || ranges.as_slice() == [(0, size)]
}

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    folder: String,
    created_at: i64,
    files: Vec<NewFile>,
}

/// What `progress.json` holds.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Progress {
    /// The boot the ranges were written in (see [`boot_id`]).
    boot: String,
    updated_at: i64,
    files: Vec<FileState>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct FileState {
    ranges: Ranges,
    /// Flushed to disk since it was last written to.
    #[serde(default)]
    flushed: bool,
    /// Writes so far, to tell whether one came in during a flush.
    #[serde(skip)]
    writes: u64,
}

/// Where an upload is in its life. Writes happen only while it is open;
/// finishing and cancelling close it, after any write under way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Open,
    Finishing,
    Closed,
}

struct Staged {
    path: String,
    size: u64,
    state: Mutex<FileState>,
}

impl Staged {
    fn state(&self) -> MutexGuard<'_, FileState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// One upload under way.
pub struct Upload {
    id: String,
    /// `<uploads folder>/.coversync-uploads/<id>`.
    dir: PathBuf,
    folder: String,
    created_at: i64,
    files: Vec<Staged>,
    updated_at: AtomicI64,
    /// Progress not saved to `progress.json` yet.
    dirty: AtomicBool,
    /// Held shared by each write while it runs, so closing waits for writes
    /// under way, and only for those.
    phase: RwLock<Phase>,
}

/// An upload's progress, as the API reports it.
#[derive(Debug, Clone, Serialize)]
pub struct UploadStatus {
    pub id: String,
    pub folder: String,
    /// Bytes in all, and bytes written so far.
    pub size: u64,
    pub received: u64,
    pub file_count: usize,
    pub created_at: i64,
    /// When a chunk last arrived.
    pub updated_at: i64,
    /// When it is removed, unless another chunk arrives first.
    pub expires_at: i64,
    pub chunk_size: u64,
    /// Each file's progress, when asked for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<FileStatus>>,
}

/// A file's progress: the byte ranges written so far, as `[start, end)`
/// pairs, to send the rest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileStatus {
    pub index: usize,
    pub path: String,
    pub size: u64,
    pub received: u64,
    pub ranges: Ranges,
    pub complete: bool,
}

/// A finished upload: the folder it went into, in the uploads folder.
#[derive(Debug, Clone, Serialize)]
pub struct Finished {
    /// The folder's name, which has a number added if the name was taken.
    pub folder: String,
    pub path: String,
}

impl Upload {
    pub fn id(&self) -> &str {
        &self.id
    }

    fn size(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }

    /// Bytes still to come.
    fn remaining(&self) -> u64 {
        self.files
            .iter()
            .map(|f| f.size - covered(&f.state().ranges))
            .sum()
    }

    fn updated_at(&self) -> i64 {
        self.updated_at.load(Ordering::Relaxed)
    }

    /// Move from `from` to `to`, once writes under way are done.
    fn turn(&self, from: Phase, to: Phase) -> Result<()> {
        let mut phase = self.phase.write().unwrap_or_else(|e| e.into_inner());
        if *phase != from {
            return Err(closed(*phase));
        }
        *phase = to;
        Ok(())
    }

    fn staged(&self, index: usize) -> Result<&Staged> {
        self.files.get(index).ok_or(Error::NotFound)
    }

    fn staged_path(&self, index: usize) -> PathBuf {
        self.dir.join(FILES).join(&self.files[index].path)
    }

    /// The size of file `index`.
    pub fn file_size(&self, index: usize) -> Result<u64> {
        Ok(self.staged(index)?.size)
    }

    pub fn file_status(&self, index: usize) -> Result<FileStatus> {
        let f = self.staged(index)?;
        let state = f.state();
        Ok(FileStatus {
            index,
            path: f.path.clone(),
            size: f.size,
            received: covered(&state.ranges),
            ranges: state.ranges.clone(),
            complete: complete(&state.ranges, f.size),
        })
    }

    /// The upload's progress; with `files`, each file's too.
    pub fn status(&self, files: bool) -> UploadStatus {
        let updated_at = self.updated_at();
        let size = self.size();
        UploadStatus {
            id: self.id.clone(),
            folder: self.folder.clone(),
            size,
            received: size - self.remaining(),
            file_count: self.files.len(),
            created_at: self.created_at,
            updated_at,
            expires_at: updated_at + IDLE_LIMIT,
            chunk_size: CHUNK_SIZE,
            files: files.then(|| {
                (0..self.files.len())
                    .filter_map(|i| self.file_status(i).ok())
                    .collect()
            }),
        }
    }

    /// Start writing file `index` at `offset`, for one chunk.
    pub fn writer(self: &Arc<Self>, index: usize, offset: u64) -> Result<ChunkWriter> {
        let size = self.staged(index)?.size;
        if offset > size {
            return Err(beyond(&self.files[index]));
        }
        let phase = self.phase.read().unwrap_or_else(|e| e.into_inner());
        if *phase != Phase::Open {
            return Err(closed(*phase));
        }
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.staged_path(index))?;
        drop(phase);
        Ok(ChunkWriter {
            upload: self.clone(),
            index,
            file,
            pos: offset,
        })
    }

    fn record(&self, index: usize, start: u64, end: u64) {
        let mut state = self.files[index].state();
        insert(&mut state.ranges, start, end);
        state.flushed = false;
        state.writes += 1;
        drop(state);
        self.updated_at.fetch_max(now(), Ordering::Relaxed);
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// Save its progress, unless it's finishing or gone: that waits, as
    /// writes do, so the folder isn't removed under the save.
    fn save(&self) -> Result<()> {
        let phase = self.phase.read().unwrap_or_else(|e| e.into_inner());
        if *phase != Phase::Open {
            return Ok(());
        }
        let progress = Progress {
            boot: boot_id(),
            updated_at: self.updated_at(),
            files: self.files.iter().map(|f| f.state().clone()).collect(),
        };
        write_json(&self.dir.join(PROGRESS), &progress, false)
    }

    /// Flush every file, then move them into `uploads` under the upload's
    /// folder name (or a numbered one). The upload is finishing.
    fn move_into(&self, uploads: &Path) -> Result<PathBuf> {
        for (i, f) in self.files.iter().enumerate() {
            let path = self.staged_path(i);
            let file = match File::open(&path) {
                Err(e) if e.kind() == ErrorKind::NotFound && f.size == 0 => File::create(&path)?,
                file => file?,
            };
            if file.metadata()?.len() != f.size {
                // Changed behind coversyncd's back: it has to come again.
                *f.state() = FileState::default();
                self.dirty.store(true, Ordering::Relaxed);
                return Err(Error::Conflict(format!(
                    "“{}” changed on the server before the upload finished; send it again.",
                    f.path
                )));
            }
            if !f.state().flushed {
                file.sync_data()?;
                f.state().flushed = true;
            }
        }
        let dest = place(&self.dir.join(FILES), uploads, &self.folder)?;
        File::open(uploads)?.sync_all()?;
        Ok(dest)
    }

    /// An upload staged in `dir` before, or an error if it can't be used.
    /// After a reboot, only files flushed to disk keep their progress: what
    /// wasn't may have been lost.
    fn load(dir: &Path, id: &str, boot: &str) -> Result<Upload> {
        let manifest: Manifest = serde_json::from_slice(&fs::read(dir.join(MANIFEST))?)
            .map_err(|e| Error::Invalid(format!("unreadable manifest: {e}")))?;
        check_files(&manifest.files)?;
        let folder = check_folder(&manifest.folder)?;
        if !dir.join(FILES).is_dir() {
            return Err(Error::Invalid(
                "its files are gone (finished already?)".into(),
            ));
        }
        let progress: Progress = fs::read(dir.join(PROGRESS))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let same_boot = !boot.is_empty() && progress.boot == boot;
        let mut saved = progress.files.into_iter();
        let files = manifest
            .files
            .into_iter()
            .map(|f| {
                let mut state = saved.next().unwrap_or_default();
                let valid = state.ranges.iter().all(|(a, b)| a < b && *b <= f.size)
                    && state.ranges.windows(2).all(|w| w[0].1 < w[1].0);
                if !valid || !(same_boot || state.flushed) {
                    state = FileState::default();
                }
                Staged {
                    path: f.path,
                    size: f.size,
                    state: Mutex::new(state),
                }
            })
            .collect();
        Ok(Upload {
            id: id.to_string(),
            dir: dir.to_path_buf(),
            folder,
            created_at: manifest.created_at,
            files,
            updated_at: AtomicI64::new(progress.updated_at.max(manifest.created_at)),
            dirty: AtomicBool::new(false),
            phase: RwLock::new(Phase::Open),
        })
    }
}

/// Writes one chunk of a file, in order from its offset.
pub struct ChunkWriter {
    upload: Arc<Upload>,
    index: usize,
    file: File,
    pos: u64,
}

impl ChunkWriter {
    /// Write the chunk's next bytes. A chunk cut short keeps what was
    /// written: those bytes count as received.
    pub fn write(&mut self, buf: &[u8]) -> Result<()> {
        let staged = &self.upload.files[self.index];
        let end = self.pos + buf.len() as u64;
        if end > staged.size {
            return Err(beyond(staged));
        }
        {
            let phase = self.upload.phase.read().unwrap_or_else(|e| e.into_inner());
            if *phase != Phase::Open {
                return Err(closed(*phase));
            }
            self.file
                .write_all_at(buf, self.pos)
                .map_err(|e| out_of_space(e, &self.upload.dir))?;
        }
        self.upload.record(self.index, self.pos, end);
        self.pos = end;
        Ok(())
    }

    /// Done with the chunk. A file this completed is flushed to disk now,
    /// while other chunks keep coming, so finishing has little left to do.
    pub fn close(self) -> Result<FileStatus> {
        let staged = &self.upload.files[self.index];
        let due = {
            let state = staged.state();
            (!state.flushed && complete(&state.ranges, staged.size)).then_some(state.writes)
        };
        if let Some(writes) = due {
            self.file.sync_data()?;
            let mut state = staged.state();
            // Unless another chunk wrote to it meanwhile.
            if state.writes == writes {
                state.flushed = true;
                self.upload.dirty.store(true, Ordering::Relaxed);
            }
        }
        self.upload.file_status(self.index)
    }
}

fn beyond(f: &Staged) -> Error {
    Error::Invalid(format!(
        "That chunk goes past the end of “{}”, which is {} bytes.",
        f.path, f.size
    ))
}

fn closed(phase: Phase) -> Error {
    match phase {
        Phase::Finishing => Error::Conflict("This upload is being finished.".into()),
        _ => Error::NotFound,
    }
}

/// A full disk as [`Error::NoSpace`].
fn out_of_space(e: io::Error, dir: &Path) -> Error {
    if e.kind() == ErrorKind::StorageFull {
        Error::NoSpace(format!(
            "The disk holding {} is full.",
            dir.parent().and_then(Path::parent).unwrap_or(dir).display()
        ))
    } else {
        e.into()
    }
}

/// The uploads under way, all in one uploads folder.
#[derive(Default)]
pub struct Uploads {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    folder: Option<PathBuf>,
    uploads: HashMap<String, Arc<Upload>>,
}

impl Uploads {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The uploads folder, if there is one.
    pub fn folder(&self) -> Option<PathBuf> {
        self.lock().folder.clone()
    }

    /// Take uploads into `folder` from now on (`None`: no uploads). Uploads
    /// under way into another folder are cancelled, and the ones staged in
    /// this folder before are picked up again. Returns how many were
    /// cancelled.
    pub fn set_folder(&self, folder: Option<PathBuf>) -> usize {
        let mut inner = self.lock();
        if inner.folder == folder {
            return 0;
        }
        let found = folder
            .as_deref()
            .map(|f| load(&f.join(STAGING), now()))
            .unwrap_or_default();
        let old = std::mem::replace(&mut inner.uploads, found);
        inner.folder = folder;
        drop(inner);
        let n = old.len();
        for upload in old.into_values() {
            discard(&upload);
        }
        n
    }

    /// Start an upload: check its files and that they fit, and stage it.
    pub fn start(&self, new: NewUpload) -> Result<Arc<Upload>> {
        let folder = check_folder(&new.folder)?;
        check_files(&new.files)?;
        let uploads = self.folder().ok_or_else(|| {
            Error::Invalid("Choose an Inbox folder in Inbox setup before uploading.".into())
        })?;
        let staging = uploads.join(STAGING);
        fs::create_dir_all(&staging).map_err(|e| unwritable(e, &uploads))?;
        let id = new_id();
        let dir = staging.join(&id);
        let created_at = now();
        let staged = stage(&dir, &folder, &new.files, created_at);
        let upload = match staged {
            Ok(()) => Arc::new(Upload {
                id: id.clone(),
                dir: dir.clone(),
                folder,
                created_at,
                files: new
                    .files
                    .into_iter()
                    .map(|f| Staged {
                        path: f.path,
                        size: f.size,
                        state: Mutex::default(),
                    })
                    .collect(),
                updated_at: AtomicI64::new(created_at),
                dirty: AtomicBool::new(false),
                phase: RwLock::new(Phase::Open),
            }),
            Err(e) => {
                let _ = fs::remove_dir_all(&dir);
                return Err(e);
            }
        };
        // The space check and taking the upload on happen together, so two
        // uploads starting at once can't both count on the same free space.
        let mut inner = self.lock();
        let refused = if inner.folder.as_ref() != Some(&uploads) {
            Some(Error::Conflict(
                "The uploads folder changed while the upload started; start it again.".into(),
            ))
        } else {
            let pending: u64 = inner.uploads.values().map(|u| u.remaining()).sum();
            match free_space(&dir) {
                Ok(free) if free < upload.size() + pending + SPARE => {
                    Some(Error::NoSpace(format!(
                        "There isn't room for this upload in {}: it needs {}{}, and {} is free.",
                        uploads.display(),
                        bytes(upload.size()),
                        if pending > 0 {
                            format!(" besides the {} other uploads still need", bytes(pending))
                        } else {
                            String::new()
                        },
                        bytes(free.saturating_sub(SPARE)),
                    )))
                }
                Ok(_) => None,
                Err(e) => Some(e.into()),
            }
        };
        if let Some(e) = refused {
            drop(inner);
            let _ = fs::remove_dir_all(&dir);
            return Err(e);
        }
        inner.uploads.insert(id, upload.clone());
        Ok(upload)
    }

    pub fn get(&self, id: &str) -> Result<Arc<Upload>> {
        self.lock().uploads.get(id).cloned().ok_or(Error::NotFound)
    }

    /// The uploads under way, oldest first.
    pub fn list(&self) -> Vec<Arc<Upload>> {
        let mut all: Vec<_> = self.lock().uploads.values().cloned().collect();
        all.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
        all
    }

    /// Move a complete upload into the uploads folder. An incomplete one is
    /// refused, and stays as it is for the rest to be sent.
    pub fn finish(&self, id: &str) -> Result<Finished> {
        let upload = self.get(id)?;
        let unfinished: Vec<&Staged> = upload
            .files
            .iter()
            .filter(|f| !complete(&f.state().ranges, f.size))
            .collect();
        if let Some(first) = unfinished.first() {
            return Err(Error::Conflict(format!(
                "{} of the upload's {} files haven't arrived in full yet, such as “{}”.",
                unfinished.len(),
                upload.files.len(),
                first.path
            )));
        }
        let uploads = upload
            .dir
            .parent()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .ok_or(Error::NotFound)?;
        upload.turn(Phase::Open, Phase::Finishing)?;
        match upload.move_into(&uploads) {
            Ok(dest) => {
                self.lock().uploads.remove(id);
                let _ = upload.turn(Phase::Finishing, Phase::Closed);
                if let Err(e) = fs::remove_dir_all(&upload.dir) {
                    tracing::warn!(dir = %upload.dir.display(), "couldn't clean up a finished upload: {e}");
                }
                Ok(Finished {
                    folder: dest
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    path: dest.display().to_string(),
                })
            }
            Err(e) => {
                let _ = upload.turn(Phase::Finishing, Phase::Open);
                Err(e)
            }
        }
    }

    /// Cancel an upload and remove what it staged.
    pub fn cancel(&self, id: &str) -> Result<()> {
        let upload = self.get(id)?;
        upload.turn(Phase::Open, Phase::Closed)?;
        self.lock().uploads.remove(id);
        remove_staged(&upload.dir)
    }

    /// Save the progress of uploads that moved on since the last save. It
    /// isn't flushed: a reboot discards what wasn't flushed anyway (see
    /// [`Upload::load`]), and anything lost is just sent again.
    pub fn save(&self) {
        for upload in self.list() {
            if upload.dirty.swap(false, Ordering::Relaxed)
                && let Err(e) = upload.save()
            {
                upload.dirty.store(true, Ordering::Relaxed);
                tracing::warn!(upload = upload.id, "couldn't save upload progress: {e}");
            }
        }
    }

    /// Remove uploads no chunk has reached for [`IDLE_LIMIT`]. Returns how
    /// many.
    pub fn expire(&self, now: i64) -> usize {
        let idle: Vec<Arc<Upload>> = {
            let mut inner = self.lock();
            let ids: Vec<String> = inner
                .uploads
                .values()
                .filter(|u| u.updated_at() + IDLE_LIMIT <= now)
                .filter(|u| u.turn(Phase::Open, Phase::Closed).is_ok())
                .map(|u| u.id.clone())
                .collect();
            ids.iter()
                .filter_map(|id| inner.uploads.remove(id))
                .collect()
        };
        for upload in &idle {
            tracing::info!(
                upload = upload.id,
                folder = upload.folder,
                "removing an abandoned upload"
            );
            if let Err(e) = remove_staged(&upload.dir) {
                tracing::warn!(dir = %upload.dir.display(), "couldn't remove an abandoned upload: {e}");
            }
        }
        idle.len()
    }
}

/// Close an upload that is being dropped, and remove what it staged.
fn discard(upload: &Upload) {
    if upload.turn(Phase::Open, Phase::Closed).is_ok()
        && let Err(e) = remove_staged(&upload.dir)
    {
        tracing::warn!(dir = %upload.dir.display(), "couldn't remove a cancelled upload: {e}");
    }
}

fn remove_staged(dir: &Path) -> Result<()> {
    match fs::remove_dir_all(dir) {
        Err(e) if e.kind() != ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

/// Create an upload's staging folder: its folders, its empty files, and
/// last its manifest, so a folder without one is a start that never ended.
fn stage(dir: &Path, folder: &str, files: &[NewFile], created_at: i64) -> Result<()> {
    let root = dir.join(FILES);
    fs::create_dir(dir)?;
    fs::create_dir(&root)?;
    let folders: HashSet<&Path> = files
        .iter()
        .filter_map(|f| Path::new(&f.path).parent())
        .filter(|p| !p.as_os_str().is_empty())
        .collect();
    for f in folders {
        fs::create_dir_all(root.join(f))?;
    }
    for f in files.iter().filter(|f| f.size == 0) {
        File::create(root.join(&f.path))?;
    }
    let manifest = Manifest {
        folder: folder.to_string(),
        created_at,
        files: files.to_vec(),
    };
    write_json(&dir.join(MANIFEST), &manifest, true)
}

/// The uploads staged in `staging`, leaving out (and removing) ones that
/// can't be used or have been idle too long.
fn load(staging: &Path, now: i64) -> HashMap<String, Arc<Upload>> {
    let mut found = HashMap::new();
    let Ok(entries) = fs::read_dir(staging) else {
        return found;
    };
    let boot = boot_id();
    for entry in entries.flatten() {
        let dir = entry.path();
        let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !valid_id(&id) || !dir.is_dir() {
            continue;
        }
        match Upload::load(&dir, &id, &boot) {
            Ok(u) if u.updated_at() + IDLE_LIMIT > now => {
                found.insert(id, Arc::new(u));
                continue;
            }
            Ok(u) => tracing::info!(
                upload = id,
                folder = u.folder,
                "removing an abandoned upload"
            ),
            Err(e) => {
                tracing::warn!(dir = %dir.display(), "removing an upload that can't go on: {e}")
            }
        }
        if let Err(e) = remove_staged(&dir) {
            tracing::warn!(dir = %dir.display(), "couldn't remove it: {e}");
        }
    }
    found
}

/// Move `src` (a folder) into `parent` as `name`, or as `name (2)`,
/// `name (3)`… if that exists: never over anything.
fn place(src: &Path, parent: &Path, name: &str) -> Result<PathBuf> {
    for n in 1..=1000 {
        let dest = parent.join(if n == 1 {
            name.to_string()
        } else {
            format!("{name} ({n})")
        });
        // Taking the name with mkdir is atomic; the rename then replaces
        // that empty folder, ours, and fails if anything was put in it.
        match fs::create_dir(&dest) {
            Ok(()) => {
                if let Err(e) = fs::rename(src, &dest) {
                    let _ = fs::remove_dir(&dest);
                    return Err(e.into());
                }
                return Ok(dest);
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(Error::Conflict(format!(
        "The uploads folder has too many folders named “{name}”."
    )))
}

/// Write JSON to `path` through a temporary file, so a reader sees the old
/// contents or the new.
fn write_json(path: &Path, value: &impl Serialize, flush: bool) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    let mut file = File::create(&tmp)?;
    file.write_all(&serde_json::to_vec(value).expect("upload state serializes"))?;
    if flush {
        file.sync_data()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

/// A folder that can't be written to as [`Error::ReadOnly`].
fn unwritable(e: io::Error, folder: &Path) -> Error {
    match e.kind() {
        ErrorKind::ReadOnlyFilesystem | ErrorKind::PermissionDenied => Error::ReadOnly(format!(
            "coversyncd can't write to the uploads folder {} ({e}). Choose a folder it can write to as the uploads folder in Inbox setup.",
            folder.display()
        )),
        _ => e.into(),
    }
}

/// Bytes free to this user on the filesystem holding `dir`.
fn free_space(dir: &Path) -> io::Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes())?;
    // SAFETY: statvfs only writes to the struct it's given; a zeroed one is a valid start.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: a valid NUL-terminated path and a pointer to that struct.
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(stat.f_bavail as u64 * stat.f_frsize as u64)
}

fn bytes(n: u64) -> String {
    if n >= 1_000_000_000 {
        format!("{:.1} GB", n as f64 / 1e9)
    } else {
        format!("{:.0} MB", (n as f64 / 1e6).ceil())
    }
}

/// This boot's id. Data written but not flushed survives coversyncd
/// restarting, but not the machine; empty where unknown.
fn boot_id() -> String {
    fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn new_id() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("the OS random number generator works");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn valid_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh uploads folder, and uploads taken into it.
    fn uploads(name: &str) -> (PathBuf, Uploads) {
        let dir =
            std::env::temp_dir().join(format!("coversync-upload-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let folder = dir.join("Uploads");
        let u = Uploads::default();
        assert_eq!(u.set_folder(Some(folder.clone())), 0);
        (folder, u)
    }

    fn new(folder: &str, files: &[(&str, u64)]) -> NewUpload {
        NewUpload {
            folder: folder.into(),
            files: files
                .iter()
                .map(|(p, s)| NewFile {
                    path: p.to_string(),
                    size: *s,
                })
                .collect(),
        }
    }

    /// Write `data` to file `index` at `offset` as one chunk.
    fn chunk(u: &Arc<Upload>, index: usize, offset: u64, data: &[u8]) -> Result<FileStatus> {
        let mut w = u.writer(index, offset)?;
        for part in data.chunks(7) {
            w.write(part)?;
        }
        w.close()
    }

    fn bytes_of(n: usize, seed: u8) -> Vec<u8> {
        (0..n)
            .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
            .collect()
    }

    fn invalid(files: &[(&str, u64)]) -> String {
        match check_files(&new("A", files).files) {
            Err(Error::Invalid(m)) => m,
            other => panic!("{files:?} was let through: {other:?}"),
        }
    }

    #[test]
    fn paths_must_be_plain_relative_paths_of_album_files() {
        let fine = new(
            "A",
            &[("CD1/01 Intro.flac", 1), ("cover.JPG", 2), ("rip.log", 0)],
        );
        assert!(check_files(&fine.files).is_ok());
        assert!(invalid(&[("../x.flac", 1)]).contains(".."));
        assert!(invalid(&[("CD1/../../x.flac", 1)]).contains(".."));
        assert!(invalid(&[("./x.flac", 1)]).contains(".."));
        assert!(invalid(&[("/etc/x.flac", 1)]).contains("relative"));
        assert!(invalid(&[("a//x.flac", 1)]).contains("empty"));
        assert!(invalid(&[("a/", 1)]).contains("empty"));
        assert!(invalid(&[("", 1)]).contains("no path"));
        assert!(invalid(&[(".hidden/x.flac", 1)]).contains("hidden"));
        assert!(invalid(&[("._x.flac", 1)]).contains("hidden"));
        assert!(invalid(&[("CD1\\x.flac", 1)]).contains("backslash"));
        assert!(invalid(&[("x\n.flac", 1)]).contains("control"));
        assert!(invalid(&[("setup.exe", 1)]).contains("kind of file"));
        assert!(invalid(&[("README", 1)]).contains("kind of file"));
        assert!(invalid(&[("a.flac", 1), ("a.flac", 2)]).contains("twice"));
        assert!(invalid(&[("a.flac", 1), ("a.flac/b.flac", 2)]).contains("both"));
        assert!(invalid(&[]).contains("at least one"));
        let long = format!("{}.flac", "a".repeat(300));
        assert!(invalid(&[(&long, 1)]).contains("255"));
        let many: Vec<(String, u64)> = (0..=MAX_FILES).map(|i| (format!("{i}.flac"), 1)).collect();
        let many: Vec<(&str, u64)> = many.iter().map(|(p, s)| (p.as_str(), *s)).collect();
        assert!(invalid(&many).contains("at most"));

        assert_eq!(check_folder(" Album (2019) ").unwrap(), "Album (2019)");
        assert_eq!(check_folder("").unwrap(), "Upload");
        for bad in ["a/b", "..", ".", ".hidden", "a\\b"] {
            assert!(check_folder(bad).is_err(), "{bad}");
        }
        assert!(check_folder(&"a".repeat(MAX_FOLDER + 1)).is_err());
    }

    #[test]
    fn ranges_merge_as_they_fill_in() {
        let mut r = Ranges::new();
        insert(&mut r, 10, 20);
        insert(&mut r, 30, 40);
        assert_eq!(r, [(10, 20), (30, 40)]);
        insert(&mut r, 20, 30);
        assert_eq!(r, [(10, 40)]);
        insert(&mut r, 0, 5);
        insert(&mut r, 50, 60);
        insert(&mut r, 15, 25);
        insert(&mut r, 5, 5);
        assert_eq!(r, [(0, 5), (10, 40), (50, 60)]);
        insert(&mut r, 3, 55);
        assert_eq!(r, [(0, 60)]);
        assert_eq!(covered(&r), 60);
        assert!(complete(&r, 60));
        assert!(!complete(&r, 61));
        assert!(complete(&Ranges::new(), 0));
    }

    #[test]
    fn chunks_land_at_their_offsets_in_any_order_and_finish_into_a_folder() {
        let (folder, u) = uploads("chunks");
        let one = bytes_of(100, 1);
        let two = bytes_of(30, 2);
        let files = [("CD1/01.flac", 100), ("CD2/01.flac", 30), ("empty.log", 0)];
        let up = u.start(new("Album", &files)).unwrap();
        // Staged where the scanner doesn't look.
        let staged = folder.join(STAGING).join(up.id()).join(FILES);
        assert!(staged.join("CD1").is_dir() && staged.join("empty.log").is_file());
        // Out of order, and partly twice over: chunks can be sent again.
        assert_eq!(chunk(&up, 0, 60, &one[60..]).unwrap().ranges, [(60, 100)]);
        let s = chunk(&up, 0, 0, &one[..40]).unwrap();
        assert_eq!(s.ranges, [(0, 40), (60, 100)]);
        let s = chunk(&up, 0, 30, &one[30..70]).unwrap();
        assert!(s.complete && s.received == 100);
        assert!(up.files[0].state().flushed, "a complete file is flushed");
        chunk(&up, 1, 0, &two).unwrap();
        let status = up.status(true);
        assert_eq!(
            (status.size, status.received, status.file_count),
            (130, 130, 3)
        );
        assert!(status.files.unwrap().iter().all(|f| f.complete));

        let done = u.finish(up.id()).unwrap();
        assert_eq!(done.folder, "Album");
        let album = folder.join("Album");
        assert_eq!(done.path, album.display().to_string());
        assert_eq!(fs::read(album.join("CD1/01.flac")).unwrap(), one);
        assert_eq!(fs::read(album.join("CD2/01.flac")).unwrap(), two);
        assert_eq!(fs::read(album.join("empty.log")).unwrap(), b"");
        assert!(!folder.join(STAGING).join(up.id()).exists());
        assert!(matches!(u.get(up.id()), Err(Error::NotFound)));
        assert!(matches!(chunk(&up, 0, 0, &one), Err(Error::NotFound)));
    }

    #[test]
    fn a_cut_chunk_keeps_what_arrived_and_resumes() {
        let (_, u) = uploads("cut");
        let data = bytes_of(64, 3);
        let up = u.start(new("A", &[("a.flac", 64)])).unwrap();
        // The connection drops after 20 bytes: the writer goes without closing.
        let mut w = up.writer(0, 0).unwrap();
        w.write(&data[..20]).unwrap();
        drop(w);
        let s = up.file_status(0).unwrap();
        assert_eq!((s.received, s.complete), (20, false));
        assert_eq!(s.ranges, [(0, 20)]);
        assert!(matches!(u.finish(up.id()), Err(Error::Conflict(m)) if m.contains("“a.flac”")));
        // The client asks how far it got, and sends the rest.
        assert!(chunk(&up, 0, 20, &data[20..]).unwrap().complete);
        let done = u.finish(up.id()).unwrap();
        assert_eq!(
            fs::read(Path::new(&done.path).join("a.flac")).unwrap(),
            data
        );
    }

    #[test]
    fn chunks_past_the_end_are_refused() {
        let (_, u) = uploads("past");
        let up = u.start(new("A", &[("a.flac", 10)])).unwrap();
        assert!(matches!(up.writer(0, 11), Err(Error::Invalid(_))));
        assert!(
            matches!(chunk(&up, 0, 5, &[0; 6]), Err(Error::Invalid(m)) if m.contains("past the end"))
        );
        assert!(matches!(up.writer(1, 0), Err(Error::NotFound)));
        assert!(matches!(u.get("nope"), Err(Error::NotFound)));
    }

    #[test]
    fn a_taken_name_gets_a_number_and_nothing_is_overwritten() {
        let (folder, u) = uploads("clash");
        fs::create_dir_all(folder.join("Album")).unwrap();
        fs::write(folder.join("Album/01.flac"), b"mine").unwrap();
        fs::create_dir_all(folder.join("Album (2)")).unwrap();
        let up = u.start(new("Album", &[("01.flac", 3)])).unwrap();
        chunk(&up, 0, 0, b"new").unwrap();
        let done = u.finish(up.id()).unwrap();
        assert_eq!(done.folder, "Album (3)");
        assert_eq!(fs::read(folder.join("Album/01.flac")).unwrap(), b"mine");
        assert_eq!(fs::read_dir(folder.join("Album (2)")).unwrap().count(), 0);
        assert_eq!(fs::read(folder.join("Album (3)/01.flac")).unwrap(), b"new");
    }

    #[test]
    fn cancelling_removes_what_was_staged() {
        let (folder, u) = uploads("cancel");
        let up = u.start(new("A", &[("a.flac", 10)])).unwrap();
        chunk(&up, 0, 0, &[1; 5]).unwrap();
        u.cancel(up.id()).unwrap();
        assert!(!folder.join(STAGING).join(up.id()).exists());
        assert!(matches!(u.get(up.id()), Err(Error::NotFound)));
        assert!(matches!(chunk(&up, 0, 5, &[1; 5]), Err(Error::NotFound)));
        assert!(matches!(u.cancel(up.id()), Err(Error::NotFound)));
        assert!(!folder.join("A").exists());
    }

    #[test]
    fn a_folder_coversyncd_cant_write_to_is_refused_clearly() {
        use std::os::unix::fs::PermissionsExt;
        // Root writes anywhere, so no folder is read-only to it.
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let (folder, u) = uploads("readonly");
        fs::create_dir_all(&folder).unwrap();
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o555)).unwrap();
        let refused = u.start(new("A", &[("a.flac", 1)]));
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o755)).unwrap();
        match refused {
            Err(Error::ReadOnly(m)) => assert!(
                m.contains(&folder.display().to_string()) && m.contains("Inbox setup"),
                "{m}"
            ),
            other => panic!("{:?}", other.map(|u| u.id().to_owned())),
        }
        let none = Uploads::default();
        assert!(
            matches!(none.start(new("A", &[("a.flac", 1)])), Err(Error::Invalid(m)) if m.contains("Inbox folder"))
        );
    }

    #[test]
    fn uploads_that_dont_fit_are_refused_up_front() {
        let (folder, u) = uploads("space");
        let free = free_space(folder.parent().unwrap()).unwrap();
        let first = u.start(new("A", &[("a.flac", free / 2)])).unwrap();
        // What the first one has still to come counts against the second.
        match u.start(new("B", &[("b.flac", free / 2)])) {
            Err(Error::NoSpace(m)) => assert!(m.contains("other uploads still need"), "{m}"),
            other => panic!("{:?}", other.map(|u| u.id().to_owned())),
        }
        assert_eq!(fs::read_dir(folder.join(STAGING)).unwrap().count(), 1);
        u.cancel(first.id()).unwrap();
        assert!(matches!(
            u.start(new("C", &[("c.flac", u64::MAX / 2)])),
            Err(Error::NoSpace(_))
        ));
    }

    #[test]
    fn progress_carries_on_after_a_restart() {
        let (folder, u) = uploads("restart");
        let up = u.start(new("A", &[("a.flac", 10), ("b.flac", 4)])).unwrap();
        chunk(&up, 0, 0, &[7; 6]).unwrap();
        chunk(&up, 1, 0, &[8; 4]).unwrap();
        u.save();
        let progress = folder.join(STAGING).join(up.id()).join(PROGRESS);
        assert!(progress.is_file());

        let again = Uploads::default();
        again.set_folder(Some(folder.clone()));
        let back = again.get(up.id()).unwrap();
        assert_eq!(back.file_status(0).unwrap().ranges, [(0, 6)]);
        assert!(back.file_status(1).unwrap().complete);
        assert_eq!(back.status(false).folder, "A");

        // After a reboot, only what was flushed is trusted.
        let mut saved: Progress = serde_json::from_slice(&fs::read(&progress).unwrap()).unwrap();
        saved.boot = "another boot".into();
        fs::write(&progress, serde_json::to_vec(&saved).unwrap()).unwrap();
        let rebooted = Uploads::default();
        rebooted.set_folder(Some(folder.clone()));
        let back = rebooted.get(up.id()).unwrap();
        assert_eq!(back.file_status(0).unwrap().received, 0);
        assert!(back.file_status(1).unwrap().complete);
        chunk(&back, 0, 0, &[7; 10]).unwrap();
        let done = rebooted.finish(up.id()).unwrap();
        assert_eq!(
            fs::read(Path::new(&done.path).join("a.flac")).unwrap(),
            [7; 10]
        );
    }

    #[test]
    fn staging_left_unusable_is_cleaned_up() {
        let (folder, u) = uploads("leftovers");
        let up = u.start(new("A", &[("a.flac", 1)])).unwrap();
        let staging = folder.join(STAGING);
        // A start that never wrote its manifest, and something not ours.
        let broken = staging.join("0123456789abcdef0123456789abcdef");
        fs::create_dir_all(broken.join(FILES)).unwrap();
        fs::create_dir_all(staging.join("not-an-upload")).unwrap();
        let again = Uploads::default();
        again.set_folder(Some(folder.clone()));
        assert_eq!(again.list().len(), 1);
        assert!(again.get(up.id()).is_ok());
        assert!(!broken.exists());
        assert!(staging.join("not-an-upload").exists());
    }

    #[test]
    fn idle_uploads_are_removed() {
        let (folder, u) = uploads("idle");
        let idle = u.start(new("A", &[("a.flac", 1)])).unwrap();
        let busy = u.start(new("B", &[("b.flac", 2)])).unwrap();
        assert_eq!(u.expire(now() + 60), 0);
        busy.updated_at.store(now() + IDLE_LIMIT, Ordering::Relaxed);
        assert_eq!(u.expire(now() + IDLE_LIMIT + 1), 1);
        assert!(!folder.join(STAGING).join(idle.id()).exists());
        assert!(u.get(busy.id()).is_ok());
        // One left idle while coversyncd was stopped goes when it's next read.
        let long_ago = now() - IDLE_LIMIT - 1;
        busy.updated_at.store(long_ago, Ordering::Relaxed);
        busy.dirty.store(true, Ordering::Relaxed);
        u.save();
        let manifest = folder.join(STAGING).join(busy.id()).join(MANIFEST);
        let mut m: Manifest = serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
        m.created_at = long_ago;
        fs::write(&manifest, serde_json::to_vec(&m).unwrap()).unwrap();
        let again = Uploads::default();
        again.set_folder(Some(folder.clone()));
        assert!(again.list().is_empty());
        assert!(!folder.join(STAGING).join(busy.id()).exists());
    }

    #[test]
    fn a_new_uploads_folder_cancels_uploads_into_the_old_one() {
        let (folder, u) = uploads("moved");
        let up = u.start(new("A", &[("a.flac", 1)])).unwrap();
        let other = folder.with_file_name("Elsewhere");
        assert_eq!(u.set_folder(Some(other.clone())), 1);
        assert!(!folder.join(STAGING).join(up.id()).exists());
        assert!(u.list().is_empty());
        assert_eq!(u.folder(), Some(other));
        assert_eq!(u.set_folder(None), 0);
    }
}
