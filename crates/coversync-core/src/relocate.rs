//! Keeping a Library album where the folder structure says: after its tags
//! are saved, each file goes to the path its tags now give, and the album's
//! other files (its cover file, say) follow it out of a folder it leaves.
//!
//! Every path is opened one name at a time from the Library folder, never
//! following a link (`O_NOFOLLOW`), and no move replaces anything
//! (`RENAME_NOREPLACE`, or a check where a file system lacks it): a link in
//! the Library can't send a file outside it, or take one from there. A save
//! records its moves in the Library's database before it makes any, and the
//! index takes the new paths and forgets the record in one transaction (see
//! [`Library::upsert_moving`]); moves left unindexed, by a failure or by
//! coversyncd stopping, are put back by [`recover`].

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use rustix::fs::{
    AtFlags, CWD, Dir, FileType, Mode, OFlags, RenameFlags, Stat, fstat, linkat, mkdirat, openat,
    readlinkat, renameat, renameat_with, statat, unlinkat,
};
use rustix::io::Errno;

use crate::db::{Library, MoveRecord};
use crate::template::Template;
use crate::{Error, Result, rules, scan, tags};

/// A file to move, or moved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Move {
    pub from: PathBuf,
    pub to: PathBuf,
}

/// What a save moves (see [`plan`]).
#[derive(Debug, Clone, Default)]
pub struct Plan {
    root: PathBuf,
    /// The album's tracks that move.
    pub tracks: Vec<Move>,
    /// Its other files and folders that follow them.
    pub companions: Vec<Move>,
    /// How many files each of `companions` is (a folder can hold several).
    companion_files: Vec<usize>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty() && self.companions.is_empty()
    }
}

/// What [`apply`] moved.
#[derive(Debug, Clone, Default)]
pub struct Applied {
    /// The record of these moves, which the index forgets as it takes them
    /// (see [`Library::upsert_moving`]); empty when nothing moved.
    pub batch: String,
    pub tracks: Vec<Move>,
    pub companions: Vec<Move>,
    /// The files moved, the album's other files counted one by one.
    pub files: usize,
}

const DIR: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// Names that are coversync's own, in any case: its database's folder,
/// uploads being staged, a device's own folder, and files a save keeps
/// beside the album's while it works (`.coversync-tmp-*` copies and
/// `.coversync-move-*` files waiting to move).
fn own(name: &OsStr) -> bool {
    name.as_bytes()
        .to_ascii_lowercase()
        .starts_with(b".coversync")
}

/// Why a scan wouldn't find a file at `to` (relative to the Library folder),
/// if it wouldn't: it skips file names starting with a dot, and coversync's
/// own folders. (Other folders starting with a dot, `.38 Special` say, it
/// looks in.)
fn unscanned(to: &Path) -> Option<&'static str> {
    let mut parts = to.components().peekable();
    while let Some(part) = parts.next() {
        let Component::Normal(name) = part else {
            return Some("a name that isn't one");
        };
        if parts.peek().is_none() {
            if name.as_bytes().starts_with(b".") {
                return Some("a file name starting with a dot");
            }
        } else if own(name) {
            return Some("a folder name coversync keeps for itself");
        }
    }
    None
}

/// The name a moving track waits under, beside where it was.
fn staged_name(batch: &str, seq: i64) -> String {
    format!(".coversync-move-{batch}-{seq}")
}

fn not_in_library(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("{} isn't a path in the Library folder", path.display()),
    )
}

/// `rel`'s names, refusing anything but plain names (`..`, say).
fn names(rel: &Path) -> io::Result<Vec<&OsStr>> {
    rel.components()
        .map(|c| match c {
            Component::Normal(n) => Ok(n),
            _ => Err(not_in_library(rel)),
        })
        .collect()
}

/// `rel`'s folder and its last name.
fn split(rel: &Path) -> io::Result<(&Path, &OsStr)> {
    let name = rel.file_name().ok_or_else(|| not_in_library(rel))?;
    Ok((rel.parent().unwrap_or(Path::new("")), name))
}

/// The Library folder (which may itself be a link).
fn open_root(root: &Path) -> io::Result<OwnedFd> {
    Ok(openat(
        CWD,
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?)
}

/// The folder `rel` under the Library folder, opened a name at a time
/// without following a link.
fn open_dir(root: BorrowedFd<'_>, rel: &Path) -> io::Result<OwnedFd> {
    let mut fd = openat(root, ".", DIR, Mode::empty())?;
    for name in names(rel)? {
        fd = openat(&fd, name, DIR, Mode::empty())?;
    }
    Ok(fd)
}

/// [`open_dir`], making the folders missing on the way (noted in `made`).
fn make_dirs(root: BorrowedFd<'_>, rel: &Path, made: &mut Vec<PathBuf>) -> io::Result<OwnedFd> {
    let mut fd = openat(root, ".", DIR, Mode::empty())?;
    let mut at = PathBuf::new();
    for name in names(rel)? {
        at.push(name);
        match mkdirat(&fd, name, Mode::from_raw_mode(0o777)) {
            Ok(()) => made.push(at.clone()),
            Err(Errno::EXIST) => {}
            Err(e) => return Err(e.into()),
        }
        fd = openat(&fd, name, DIR, Mode::empty())?;
    }
    Ok(fd)
}

/// Write each of `dirs` (relative to the Library folder) and the folders
/// above it to disk: the names moves gave and took are there, should the
/// power go, before the index says so. A folder gone is left out; one that
/// fails is only logged, as its files are where they should be all the same.
fn sync_dirs(root: BorrowedFd<'_>, dirs: impl IntoIterator<Item = PathBuf>) {
    let mut all: BTreeSet<PathBuf> = BTreeSet::new();
    for dir in dirs {
        all.extend(dir.ancestors().map(Path::to_path_buf));
    }
    for dir in all {
        let Ok(fd) = open_dir(root, &dir) else {
            continue;
        };
        if let Err(e) = rustix::fs::fsync(&fd) {
            tracing::warn!(dir = %dir.display(), "couldn't write the folder to disk: {e}");
        }
    }
}

/// What `name` in `dir` is, not following a link; None when there's nothing.
fn entry(dir: BorrowedFd<'_>, name: &OsStr) -> io::Result<Option<Stat>> {
    match statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(st) => Ok(Some(st)),
        Err(Errno::NOENT) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn io<T>(r: rustix::io::Result<T>) -> io::Result<T> {
    Ok(r?)
}

fn kind(st: &Stat) -> FileType {
    FileType::from_raw_mode(st.st_mode)
}

/// Rename, never replacing anything: `AlreadyExists` when `to` is taken.
/// File systems without `RENAME_NOREPLACE` (some network ones) make a second
/// name, which can't replace anything either, then drop the first. Folders
/// there are checked first and then renamed, as a rename can't put a folder
/// in place of a file or of a folder with anything in it. A file where hard
/// links aren't either could replace one that came meanwhile, so it doesn't
/// move.
fn rename_new(
    from_dir: BorrowedFd<'_>,
    from: &OsStr,
    to_dir: BorrowedFd<'_>,
    to: &OsStr,
) -> io::Result<()> {
    match renameat_with(from_dir, from, to_dir, to, RenameFlags::NOREPLACE) {
        Err(Errno::INVAL | Errno::NOSYS | Errno::OPNOTSUPP) => {}
        r => return Ok(r?),
    }
    let linked = match linkat(from_dir, from, to_dir, to, AtFlags::empty()) {
        Ok(()) => {
            if let Err(e) = unlinkat(from_dir, from, AtFlags::empty()) {
                let _ = unlinkat(to_dir, to, AtFlags::empty());
                return Err(e.into());
            }
            return Ok(());
        }
        Err(Errno::EXIST) => return Err(Errno::EXIST.into()),
        Err(e) => e,
    };
    if entry(to_dir, to)?.is_some() {
        return Err(Errno::EXIST.into());
    }
    if !entry(from_dir, from)?.is_some_and(|st| kind(&st) == FileType::Directory) {
        let e = io::Error::from(linked);
        return Err(io::Error::new(
            e.kind(),
            format!(
                "it can't be renamed without the risk of replacing another file, nor given a second name ({e})"
            ),
        ));
    }
    Ok(renameat(from_dir, from, to_dir, to)?)
}

/// Whether a file in `dir` can move as a save moves one, never replacing
/// another (see [`rename_new`]): tried on a file of its own, made and
/// removed here.
fn check_moves(dir: BorrowedFd<'_>) -> io::Result<()> {
    let from = format!(".coversync-probe-{}", random_id());
    let to = format!("{from}-moved");
    drop(openat(
        dir,
        from.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )?);
    let moved = rename_new(dir, OsStr::new(&from), dir, OsStr::new(&to));
    let made = if moved.is_ok() { &to } else { &from };
    unlinkat(dir, made.as_str(), AtFlags::empty())?;
    moved
}

/// A folder's entries and what each is, links not followed.
fn list(dir: BorrowedFd<'_>) -> io::Result<Vec<(OsString, FileType)>> {
    let mut out = Vec::new();
    let mut entries = Dir::read_from(dir)?;
    while let Some(e) = entries.read() {
        let e = e?;
        let name = e.file_name().to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        let name = OsStr::from_bytes(name).to_owned();
        // The folder says, unless its file system doesn't.
        let what = match e.file_type() {
            FileType::Unknown => match entry(dir, &name)? {
                Some(st) => kind(&st),
                None => continue,
            },
            what => what,
        };
        out.push((name, what));
    }
    Ok(out)
}

/// A folder's names, as they're spelt.
fn spellings(dir: BorrowedFd<'_>) -> io::Result<HashSet<OsString>> {
    let mut out = HashSet::new();
    let mut entries = Dir::read_from(dir)?;
    while let Some(e) = entries.read() {
        out.insert(OsStr::from_bytes(e?.file_name().to_bytes()).to_owned());
    }
    Ok(out)
}

/// Whether names that differ only in case are one name where the Library
/// is (a share from a Windows or macOS machine, say): a file made under one
/// spelling is found under another.
fn case_insensitive(root: BorrowedFd<'_>) -> bool {
    let Ok(at) = openat(root, ".coversync", DIR, Mode::empty())
        .or_else(|_| openat(root, ".", DIR, Mode::empty()))
    else {
        return false;
    };
    let name = format!(".coversync-case-{}", random_id());
    let made = openat(
        &at,
        name.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    );
    if made.is_err() {
        return false;
    }
    let found = statat(&at, name.to_uppercase().as_str(), AtFlags::SYMLINK_NOFOLLOW).is_ok();
    let _ = unlinkat(&at, name.as_str(), AtFlags::empty());
    found
}

/// 16 random hex digits.
fn random_id() -> String {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).expect("the OS random number generator works");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A path as names compare where the Library is (see [`case_insensitive`]).
fn key(rel: &Path, folded: bool) -> String {
    let s = rel.to_string_lossy();
    if folded {
        s.to_lowercase()
    } else {
        s.into_owned()
    }
}

/// Where the folder structure puts each of `files` (all of an album's, under
/// the Library folder `root`, with their tag values as they'll be on disk),
/// and the album's other files that follow them out of a folder they all
/// leave. Refused, with what's wrong, when the structure needs a value a
/// file doesn't have or a name a scan skips, two files would be one, a file
/// would be where a folder has to be or land on one that stays, a link or
/// another file system is in the way, or a file to move is a link to a path
/// from its own folder.
pub fn plan(
    template: &Template,
    root: &Path,
    files: &[(PathBuf, HashMap<&'static str, String>)],
) -> Result<Plan> {
    let mut placed: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut lacking: Vec<String> = Vec::new();
    for (path, values) in files {
        let from = path.strip_prefix(root).map_err(|_| not_in_library(path))?;
        let file = path.file_name().unwrap_or_default().to_string_lossy();
        let extension = path
            .extension()
            .map(|e| e.to_string_lossy().into_owned())
            .unwrap_or_default();
        match template.render(values, &extension) {
            Ok(to) => {
                // A file that stays is where a scan found it.
                if to.as_path() != from
                    && let Some(why) = unscanned(&to)
                {
                    return Err(Error::Invalid(format!(
                        "The folder structure would put {file} at {}, where a scan wouldn't find it ({why}).",
                        to.display()
                    )));
                }
                placed.push((from.to_path_buf(), to));
            }
            Err(missing) => {
                let names: Vec<&str> = missing
                    .iter()
                    .map(|v| rules::kind(v).map_or(*v, |k| k.label))
                    .collect();
                lacking.push(format!("{file} would have no {}", names.join(", ")));
            }
        }
    }
    if !lacking.is_empty() {
        return Err(Error::Invalid(format!(
            "The folder structure needs values these files wouldn't have: {}.",
            lacking.join("; ")
        )));
    }
    let mut plan = Plan {
        root: root.to_path_buf(),
        ..Plan::default()
    };
    let moving: Vec<&(PathBuf, PathBuf)> = placed.iter().filter(|(f, t)| f != t).collect();
    if moving.is_empty() {
        return Ok(plan);
    }
    let root_fd = open_root(root)?;
    let root_fd = root_fd.as_fd();
    let folded = case_insensitive(root_fd);

    let (finals, folders) = places(&placed, folded)?;

    // The files that move, as they are: each by its folder, its name and
    // itself, so another name for it (a hard link) isn't taken for it.
    let mut leaving: HashSet<Leaving> = HashSet::new();
    let mut devices = Vec::new();
    let mut checked = HashSet::new();
    for (from, _) in &moving {
        let (dir, name) = split(from)?;
        let fd = open_dir(root_fd, dir).map_err(|e| match Errno::from_io_error(&e) {
            Some(Errno::LOOP | Errno::NOTDIR) => Error::Invalid(format!(
                "{} is in a linked folder, which a save doesn't move files out of.",
                from.display()
            )),
            _ => e.into(),
        })?;
        let st = entry(fd.as_fd(), name)?.ok_or_else(|| {
            Error::Invalid(format!(
                "{} isn't in the Library folder now.",
                from.display()
            ))
        })?;
        if let Some(why) = unmovable(fd.as_fd(), name, &st)? {
            return Err(Error::Invalid(format!("{} {why}.", from.display())));
        }
        let parent = io(fstat(&fd))?;
        // Each file system once: one where a file can't move safely (see
        // `rename_new`) is found out before any tags are written.
        if checked.insert(parent.st_dev) {
            check_moves(fd.as_fd()).map_err(|e| {
                Error::Invalid(format!("Files in {} can't be moved: {e}.", dir.display()))
            })?;
        }
        leaving.insert((
            parent.st_dev,
            parent.st_ino,
            st.st_ino,
            key(Path::new(name), folded),
        ));
        devices.push(parent.st_dev);
    }

    // Where they go: folders, or room for them, on the way, and room at the
    // end (a file that leaves makes room for one, not for a folder).
    let mut spelled: HashMap<PathBuf, HashSet<OsString>> = HashMap::new();
    for ((from, to), device) in moving.iter().zip(&devices) {
        let (dir, name) = split(to)?;
        let mut fd = io(openat(root_fd, ".", DIR, Mode::empty()))?;
        let mut at = PathBuf::new();
        let mut whole = true;
        for n in names(dir)? {
            at.push(n);
            let Some(st) = entry(fd.as_fd(), n)? else {
                whole = false;
                break;
            };
            match kind(&st) {
                FileType::Directory => {
                    // Where names fold, the folder found can be spelt
                    // otherwise; the index would have it as the structure
                    // spells it, and a scan as it is.
                    if folded {
                        // Its folder's names, read once (names only: what
                        // each is would be a request each on a share).
                        let parent = at.parent().unwrap_or(Path::new(""));
                        let names = match spelled.get(parent) {
                            Some(names) => names,
                            None => spelled
                                .entry(parent.to_path_buf())
                                .or_insert(spellings(fd.as_fd())?),
                        };
                        if !names.contains(n) {
                            let Some(other) = names
                                .iter()
                                .find(|m| key(Path::new(m), true) == key(Path::new(n), true))
                            else {
                                return Err(Error::Invalid(format!(
                                    "The folder structure says {}, and a folder there answers to that name under another spelling: a save doesn't change a folder name's spelling.",
                                    at.display()
                                )));
                            };
                            return Err(Error::Invalid(format!(
                                "The folder structure says {}, but the folder there is {}: a save doesn't change a folder name's case.",
                                at.display(),
                                at.with_file_name(other).display()
                            )));
                        }
                    }
                    fd = io(openat(&fd, n, DIR, Mode::empty()))?;
                }
                FileType::Symlink => {
                    return Err(Error::Invalid(format!(
                        "{} is a link, which a save doesn't put files in.",
                        at.display()
                    )));
                }
                _ => {
                    return Err(Error::Invalid(format!(
                        "{} is a file where the folder structure needs a folder.",
                        at.display()
                    )));
                }
            }
        }
        let here = io(fstat(&fd))?;
        if here.st_dev != *device {
            return Err(Error::Invalid(format!(
                "{} would be on another file system than {}.",
                to.display(),
                from.display()
            )));
        }
        if whole
            && let Some(st) = entry(fd.as_fd(), name)?
            && !leaving.contains(&(
                here.st_dev,
                here.st_ino,
                st.st_ino,
                key(Path::new(name), folded),
            ))
        {
            return Err(Error::Invalid(format!("{} already exists.", to.display())));
        }
    }

    let departing: HashSet<&Path> = moving.iter().map(|(f, _)| f.as_path()).collect();
    let taken = Taken {
        finals: &finals,
        folders: &folders,
        folded,
    };
    let absolute = |from: &Path, to: &Path| Move {
        from: root.join(from),
        to: root.join(to),
    };
    for (from, to, files) in companions(root_fd, &placed, &departing, &taken) {
        plan.companions.push(absolute(&from, &to));
        plan.companion_files.push(files);
    }
    plan.tracks = moving.iter().map(|(f, t)| absolute(f, t)).collect();
    Ok(plan)
}

/// Where every file ends up (`placed`, the paths each has and will have),
/// and the folders on the way, as names compare where they are (see
/// [`key`]). Refused when two files would be one, one would be where
/// another needs a folder, or a folder would be spelt two ways (where names
/// fold, one folder, which the index would have under both).
fn places(placed: &[(PathBuf, PathBuf)], folded: bool) -> Result<(Keyed<'_>, Keyed<'_>)> {
    let mut finals: Keyed<'_> = HashMap::new();
    for (from, to) in placed {
        let at: &Path = if from == to { from } else { to };
        if finals.insert(key(at, folded), at).is_some() {
            return Err(Error::Invalid(format!(
                "Two files would both be {}.",
                at.display()
            )));
        }
    }
    let mut folders: Keyed<'_> = HashMap::new();
    for at in finals.values() {
        for dir in at.ancestors().skip(1) {
            if dir.as_os_str().is_empty() {
                break;
            }
            let k = key(dir, folded);
            if finals.contains_key(&k) {
                return Err(Error::Invalid(format!(
                    "{} would be both a file and a folder.",
                    dir.display()
                )));
            }
            if let Some(other) = folders.insert(k, dir)
                && other != dir
            {
                return Err(Error::Invalid(format!(
                    "{} and {} would be one folder: the folder structure gives these files names that differ only in case.",
                    other.display(),
                    dir.display()
                )));
            }
        }
    }
    Ok((finals, folders))
}

/// Paths by how names compare where they are (see [`key`]).
type Keyed<'a> = HashMap<String, &'a Path>;

/// A file that leaves its folder: the folder's device and inode, the
/// file's inode, and its name as names compare there (see [`key`]).
type Leaving = (u64, u64, u64, String);

/// Why the file `name` in `dir` (`st`) can't move, if it can't: it isn't a
/// file, or it's a link to a path from its own folder, which may lead
/// elsewhere once it, or what it leads to, has moved.
fn unmovable(dir: BorrowedFd<'_>, name: &OsStr, st: &Stat) -> io::Result<Option<&'static str>> {
    Ok(match kind(st) {
        FileType::RegularFile => None,
        FileType::Symlink => (!io(readlinkat(dir, name, Vec::new()))?
            .as_bytes()
            .starts_with(b"/"))
        .then_some("is a link to a path from its own folder, which a save doesn't move"),
        _ => Some("isn't a file"),
    })
}

/// The names the album's tracks will have, which its other files can't take.
struct Taken<'a> {
    finals: &'a Keyed<'a>,
    /// The folders on the way to them.
    folders: &'a Keyed<'a>,
    folded: bool,
}

/// The album's other files that follow its tracks, with how many files
/// each is. A folder's go where its album goes (the album folder a scan
/// gives it, above disc folders), or, for a folder that isn't one, where its
/// tracks do, when that's one other folder: each file and audio-free folder
/// in it, to the same name there unless that's taken. A folder holding
/// audio that stays, a link, something stranger, or what can't be listed
/// keeps everything in it; coversync's own folders are never looked in.
fn companions(
    root: BorrowedFd<'_>,
    placed: &[(PathBuf, PathBuf)],
    departing: &HashSet<&Path>,
    taken: &Taken<'_>,
) -> Vec<(PathBuf, PathBuf, usize)> {
    let parent = |p: &Path| p.parent().unwrap_or(Path::new("")).to_path_buf();
    let mut by_folder: BTreeMap<PathBuf, BTreeSet<PathBuf>> = BTreeMap::new();
    let mut by_album: BTreeMap<PathBuf, BTreeSet<PathBuf>> = BTreeMap::new();
    for (from, to) in placed {
        by_folder
            .entry(parent(from))
            .or_default()
            .insert(parent(to));
        by_album
            .entry(scan::album_dir(from))
            .or_default()
            .insert(scan::album_dir(to));
    }
    let only = |to: BTreeSet<PathBuf>| {
        let mut to = to.into_iter();
        match (to.next(), to.next()) {
            (Some(to), None) => Some(to),
            _ => None,
        }
    };
    let mut goes: BTreeMap<PathBuf, Option<PathBuf>> = BTreeMap::new();
    for (from, to) in by_folder {
        goes.insert(from, only(to));
    }
    // An album folder goes with its album, split or not, whatever its own
    // tracks do (a flat album into disc folders, say).
    for (from, to) in by_album {
        goes.insert(from, only(to));
    }
    let mut out = Vec::new();
    let mut claimed: HashSet<String> = HashSet::new();
    for (from, to) in goes {
        let Some(to) = to else { continue };
        // Not the Library folder's own files, nor into it, nor coversync's.
        if to == from
            || from.as_os_str().is_empty()
            || to.as_os_str().is_empty()
            || from.iter().any(own)
        {
            continue;
        }
        let Ok(fd) = open_dir(root, &from) else {
            continue;
        };
        let Some(names) = movable(fd.as_fd(), &from, departing, &to) else {
            continue;
        };
        if names.is_empty() {
            continue;
        }
        // The deepest of `to`'s folders there is, and whether that's all of it.
        let Ok(mut at) = openat(root, ".", DIR, Mode::empty()) else {
            continue;
        };
        let mut whole = true;
        for n in to.iter() {
            match openat(&at, n, DIR, Mode::empty()) {
                Ok(next) => at = next,
                Err(_) => {
                    whole = false;
                    break;
                }
            }
        }
        if fstat(&at).map(|st| st.st_dev).ok() != fstat(&fd).map(|st| st.st_dev).ok() {
            continue;
        }
        for (name, files) in names {
            let dest = to.join(&name);
            let k = key(&dest, taken.folded);
            if taken.finals.contains_key(&k)
                || taken.folders.contains_key(&k)
                || claimed.contains(&k)
            {
                continue;
            }
            if whole && !matches!(entry(at.as_fd(), &name), Ok(None)) {
                continue;
            }
            claimed.insert(k);
            out.push((from.join(&name), dest, files));
        }
    }
    out
}

/// The names in the folder `dir` (`rel`) that can follow the album to `to`,
/// with how many files each is: its files that aren't audio and its folders
/// with no audio at all. None when it all stays: there's audio there that
/// isn't the album's moving away, a link or something stranger, or what
/// can't be listed. coversync's own names, names the move record can't hold
/// (not UTF-8: a folder's is still looked in for audio), and the way down
/// to `to` (an album moving into a folder of its own inside this one), stay.
fn movable(
    dir: BorrowedFd<'_>,
    rel: &Path,
    departing: &HashSet<&Path>,
    to: &Path,
) -> Option<Vec<(OsString, usize)>> {
    let mut names = Vec::new();
    for (name, kind) in list(dir).ok()? {
        let path = rel.join(&name);
        if own(&name) {
            continue;
        }
        // The way down to `to` stays, but audio there that isn't the
        // album's keeps the rest in place too.
        if to.starts_with(&path) {
            if kind == FileType::Directory {
                let sub = openat(dir, &name, DIR, Mode::empty()).ok()?;
                departing_audio(sub.as_fd(), &path, departing)?;
            }
            continue;
        }
        match kind {
            FileType::RegularFile if tags::is_audio(&path) => {
                if !departing.contains(path.as_path()) {
                    return None;
                }
            }
            FileType::RegularFile => {
                if name.to_str().is_some() {
                    names.push((name, 1));
                }
            }
            FileType::Directory => {
                let sub = openat(dir, &name, DIR, Mode::empty()).ok()?;
                // A folder of tracks that move (a disc's) has its own way.
                let (audio, files) = departing_audio(sub.as_fd(), &path, departing)?;
                if !audio && name.to_str().is_some() {
                    names.push((name, files));
                }
            }
            _ => return None,
        }
    }
    Some(names)
}

/// Whether the album's moving tracks are under `dir` (`rel`), and how many
/// other files are; None when something there keeps it all in place (see
/// [`movable`]), coversync's own folders among it (uploads being staged,
/// say), which never move.
fn departing_audio(
    dir: BorrowedFd<'_>,
    rel: &Path,
    departing: &HashSet<&Path>,
) -> Option<(bool, usize)> {
    let (mut found, mut files) = (false, 0);
    for (name, kind) in list(dir).ok()? {
        let path = rel.join(&name);
        // Its own files (a copy a save is making) go with the folder.
        if own(&name) {
            match kind {
                FileType::Directory => return None,
                _ => files += 1,
            }
            continue;
        }
        match kind {
            FileType::RegularFile if tags::is_audio(&path) => {
                if !departing.contains(path.as_path()) {
                    return None;
                }
                found = true;
            }
            FileType::RegularFile => files += 1,
            FileType::Directory => {
                let sub = openat(dir, &name, DIR, Mode::empty()).ok()?;
                let (audio, more) = departing_audio(sub.as_fd(), &path, departing)?;
                found |= audio;
                files += more;
            }
            _ => return None,
        }
    }
    Some((found, files))
}

/// Remove each of `dirs` (relative to the Library folder) and the folders
/// above it, short of the Library folder, while they're empty.
fn prune(root: BorrowedFd<'_>, dirs: &[PathBuf]) {
    let mut dirs: Vec<&Path> = dirs.iter().map(PathBuf::as_path).collect();
    dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
    dirs.dedup();
    for dir in dirs {
        let mut d = dir;
        while let Ok((parent, name)) = split(d) {
            let removed = open_dir(root, parent)
                .is_ok_and(|fd| unlinkat(&fd, name, AtFlags::REMOVEDIR).is_ok());
            if !removed {
                break;
            }
            d = parent;
        }
    }
}

/// Remove those of `made` (folders a save made, relative to the Library
/// folder) that are empty, and no other.
fn unmake(root: BorrowedFd<'_>, made: &[PathBuf]) {
    let mut dirs: Vec<&Path> = made.iter().map(PathBuf::as_path).collect();
    dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
    dirs.dedup();
    for dir in dirs {
        if let Ok((parent, name)) = split(dir)
            && let Ok(fd) = open_dir(root, parent)
        {
            let _ = unlinkat(&fd, name, AtFlags::REMOVEDIR);
        }
    }
}

/// The folders of the records' paths on one `side`.
fn parents(records: &[MoveRecord], side: impl Fn(&MoveRecord) -> &str) -> Vec<PathBuf> {
    records
        .iter()
        .filter_map(|r| Path::new(side(r)).parent().map(Path::to_path_buf))
        .collect()
}

/// The seqs of `records` not among `unsettled`.
fn settled(records: &[MoveRecord], unsettled: &[i64]) -> Vec<i64> {
    records
        .iter()
        .map(|r| r.seq)
        .filter(|s| !unsettled.contains(s))
        .collect()
}

/// One step of a recorded move: aside (a track, beside where it was), or to
/// its place (from aside, or for one of the album's other files, straight).
/// Folders made on the way are noted in `made`.
fn step(
    root: BorrowedFd<'_>,
    r: &MoveRecord,
    aside: bool,
    made: &mut Vec<PathBuf>,
) -> std::result::Result<(), (io::Error, String)> {
    let what = |e: io::Error| (e, format!("{} to {}", r.from, r.to));
    let (dir, name) = split(Path::new(&r.from)).map_err(what)?;
    let fd = open_dir(root, dir).map_err(what)?;
    // The file recorded, not one put in its place since (a link, say).
    let source = match (&r.staged, aside) {
        (Some(staged), false) => OsStr::new(staged),
        _ => name,
    };
    match entry(fd.as_fd(), source).map_err(what)? {
        Some(st) if st.st_dev == r.dev && st.st_ino == r.ino => {}
        _ => return Err(what(io::Error::other("it was replaced meanwhile"))),
    }
    match (&r.staged, aside) {
        (Some(staged), true) => rename_new(fd.as_fd(), name, fd.as_fd(), OsStr::new(staged)),
        (staged, _) => {
            let (to_dir, to_name) = split(Path::new(&r.to)).map_err(what)?;
            let to_fd = make_dirs(root, to_dir, made).map_err(what)?;
            let from = staged.as_deref().map_or(name, OsStr::new);
            rename_new(fd.as_fd(), from, to_fd.as_fd(), to_name)
        }
    }
    .map_err(what)
}

/// Make the moves [`plan`] gave, after recording them (see the module's
/// documentation). Tracks go aside first, under names no file has, so one
/// can take another's place; one that can't reach its own puts them all
/// back. Then the album's other files follow, each on its own: one gone or
/// that can't stays where it was. Folders the album left empty go. Refused
/// while the Library folder has no database (before its first import), as
/// the record would be lost with coversyncd.
pub fn apply(lib: &Library, plan: &Plan) -> Result<Applied> {
    if plan.is_empty() {
        return Ok(Applied::default());
    }
    if !lib.library_stored()? {
        return Err(Error::Invalid(
            "the Library folder has no database yet (its first import makes one) to record moves in"
                .into(),
        ));
    }
    let root = &plan.root;
    let root_fd = open_root(root)?;
    let root_fd = root_fd.as_fd();
    let batch = random_id();
    let rel = |p: &Path| -> io::Result<PathBuf> {
        p.strip_prefix(root)
            .map(Path::to_path_buf)
            .map_err(|_| not_in_library(p))
    };
    let found = |from: &Path| -> io::Result<Option<Stat>> {
        let (dir, name) = split(from)?;
        entry(open_dir(root_fd, dir)?.as_fd(), name)
    };
    let record = |seq: usize, from: &Path, to: &Path, st: &Stat, staged: bool| MoveRecord {
        seq: seq as i64,
        from: from.to_string_lossy().into_owned(),
        staged: staged.then(|| staged_name(&batch, seq as i64)),
        to: to.to_string_lossy().into_owned(),
        dev: st.st_dev,
        ino: st.st_ino,
    };
    let mut records = Vec::new();
    for m in &plan.tracks {
        let (from, to) = (rel(&m.from)?, rel(&m.to)?);
        let (dir, name) = split(&from)?;
        let fd = open_dir(root_fd, dir)?;
        let st = entry(fd.as_fd(), name)?.ok_or_else(|| {
            Error::Invalid(format!(
                "{} isn't in the Library folder now.",
                from.display()
            ))
        })?;
        // As it was planned: something else put there meanwhile may not be.
        if let Some(why) = unmovable(fd.as_fd(), name, &st)? {
            return Err(Error::Invalid(format!("{} {why}", from.display())));
        }
        records.push(record(records.len(), &from, &to, &st, true));
    }
    // The album's other files that can still follow it: its folders still
    // with no other audio, and no more files than planned (one taken out
    // meanwhile leaves the rest to go).
    let departing: HashSet<PathBuf> = records.iter().map(|r| PathBuf::from(&r.from)).collect();
    let departing: HashSet<&Path> = departing.iter().map(PathBuf::as_path).collect();
    let mut now: HashMap<PathBuf, Option<HashMap<OsString, usize>>> = HashMap::new();
    let mut following = Vec::new();
    for (m, files) in plan.companions.iter().zip(&plan.companion_files) {
        let (Ok(from), Ok(to)) = (rel(&m.from), rel(&m.to)) else {
            continue;
        };
        let (Ok((dir, name)), Ok((to_dir, _))) = (split(&from), split(&to)) else {
            continue;
        };
        let can = now.entry(dir.to_path_buf()).or_insert_with(|| {
            let fd = open_dir(root_fd, dir).ok()?;
            let names = movable(fd.as_fd(), dir, &departing, to_dir)?;
            Some(names.into_iter().collect())
        });
        let Some(&count) = can.as_ref().and_then(|names| names.get(name)) else {
            continue;
        };
        if count > *files {
            continue;
        }
        let Ok(Some(st)) = found(&from) else { continue };
        records.push(record(records.len(), &from, &to, &st, false));
        following.push((m, count));
    }
    lib.record_moves(&batch, &records)?;
    let (tracks, companions) = records.split_at(plan.tracks.len());
    let mut made = Vec::new();
    let placed = tracks
        .iter()
        .try_for_each(|r| step(root_fd, r, true, &mut made))
        .and_then(|()| {
            tracks
                .iter()
                .try_for_each(|r| step(root_fd, r, false, &mut made))
        });
    if let Err((e, what)) = placed {
        let unsettled = put_back(root_fd, &records, &made);
        lib.forget_moves(&batch, Some(&settled(&records, &unsettled)))?;
        let why = match e.kind() {
            io::ErrorKind::AlreadyExists => "something took its place meanwhile".to_string(),
            _ => e.to_string(),
        };
        return Err(Error::Invalid(format!("couldn't move {what}: {why}")));
    }
    let mut followed = Vec::new();
    let mut files = plan.tracks.len();
    for (r, (m, n)) in companions.iter().zip(following) {
        match step(root_fd, r, false, &mut made) {
            Ok(()) => {
                followed.push(m.clone());
                files += n;
            }
            Err((e, what)) => tracing::warn!("{what} stays: {e}"),
        }
    }
    prune(root_fd, &parents(&records, |r| &r.from));
    let mut touched = parents(&records, |r| &r.from);
    touched.extend(parents(&records, |r| &r.to));
    sync_dirs(root_fd, touched);
    Ok(Applied {
        batch,
        tracks: plan.tracks.clone(),
        companions: followed,
        files,
    })
}

/// Put back the files of recorded moves, each where it was (so files that
/// traded places trade back), and remove the folders this save made
/// (`made`), if empty. Gives the moves not settled: a file that isn't back
/// where it was, but is still at its new place or aside, or can't be looked
/// for. One found nowhere (gone meanwhile) is settled.
fn put_back(root: BorrowedFd<'_>, records: &[MoveRecord], made: &[PathBuf]) -> Vec<i64> {
    // Whether the file is at `name` in `rel`; an error when that can't be told.
    let at = |rel: &Path, name: &OsStr, r: &MoveRecord| -> io::Result<bool> {
        let fd = match open_dir(root, rel) {
            Ok(fd) => fd,
            Err(e) if Errno::from_io_error(&e) == Some(Errno::NOENT) => return Ok(false),
            Err(e) => return Err(e),
        };
        Ok(entry(fd.as_fd(), name)?.is_some_and(|st| st.st_dev == r.dev && st.st_ino == r.ino))
    };
    // From their new places aside (or, for files that didn't wait aside,
    // straight back), then from aside back.
    for r in records.iter().rev() {
        let (Ok((from_dir, from_name)), Ok((to_dir, to_name))) =
            (split(Path::new(&r.from)), split(Path::new(&r.to)))
        else {
            continue;
        };
        // One that never left has no other place to come back from.
        if matches!(at(from_dir, from_name, r), Ok(true))
            || !matches!(at(to_dir, to_name, r), Ok(true))
        {
            continue;
        }
        let back = r.staged.as_deref().map_or(from_name, OsStr::new);
        let moved = open_dir(root, to_dir).and_then(|to_fd| {
            let from_fd = make_dirs(root, from_dir, &mut Vec::new())?;
            rename_new(to_fd.as_fd(), to_name, from_fd.as_fd(), back)
        });
        if let Err(e) = moved {
            tracing::error!(from = %r.to, to = %r.from, "couldn't put a file back: {e}");
        }
    }
    for r in records {
        let (Some(staged), Ok((dir, name))) = (&r.staged, split(Path::new(&r.from))) else {
            continue;
        };
        let staged = OsStr::new(staged);
        if !matches!(at(dir, staged, r), Ok(true)) {
            continue;
        }
        let moved =
            open_dir(root, dir).and_then(|fd| rename_new(fd.as_fd(), staged, fd.as_fd(), name));
        if let Err(e) = moved {
            tracing::error!(path = %r.from, "couldn't put a file back: {e}");
        }
    }
    // Back, or nowhere to be found?
    let mut unsettled = Vec::new();
    for r in records {
        let (Ok((from_dir, from_name)), Ok((to_dir, to_name))) =
            (split(Path::new(&r.from)), split(Path::new(&r.to)))
        else {
            unsettled.push(r.seq);
            continue;
        };
        let mut others = vec![(to_dir, to_name)];
        if let Some(staged) = &r.staged {
            others.push((from_dir, OsStr::new(staged)));
        }
        match at(from_dir, from_name, r) {
            // Back. Another name it has where it went (a move stopped
            // between giving it a second name and dropping the first) goes:
            // a scan would take it for another track.
            Ok(true) => {
                for (dir, name) in others {
                    if matches!(at(dir, name, r), Ok(true))
                        && let Err(e) = open_dir(root, dir)
                            .and_then(|fd| Ok(unlinkat(&fd, name, AtFlags::empty())?))
                    {
                        tracing::warn!(path = %r.from, "couldn't remove a second name for a file put back: {e}");
                    }
                }
            }
            // Not back: settled only when it's nowhere at all.
            Ok(false) => match others
                .iter()
                .any(|(dir, name)| !matches!(at(dir, name, r), Ok(false)))
            {
                true => unsettled.push(r.seq),
                false => tracing::warn!(path = %r.from, "a moved file is gone"),
            },
            // Its own place can't be looked in: it may well be there.
            Err(_) => unsettled.push(r.seq),
        }
    }
    unmake(root, made);
    let mut touched = parents(records, |r| &r.from);
    touched.extend(parents(records, |r| &r.to));
    sync_dirs(root, touched);
    unsettled
}

/// Whether `records` (a batch, `batch`) are ones [`apply`] made: plain paths
/// in the Library folder, and the names it waits files under. Anything else
/// (a database from elsewhere, say) isn't acted on.
fn valid(batch: &str, records: &[MoveRecord]) -> bool {
    let path =
        |p: &str| !p.is_empty() && split(Path::new(p)).is_ok() && names(Path::new(p)).is_ok();
    batch.len() == 16
        && batch.bytes().all(|b| b.is_ascii_hexdigit())
        && records.iter().all(|r| {
            path(&r.from)
                && path(&r.to)
                && r.staged
                    .as_ref()
                    .is_none_or(|s| *s == staged_name(batch, r.seq))
        })
}

/// What [`recover`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Recovered {
    /// Moves undone, or with nothing left to undo.
    pub put_back: usize,
    /// Moves still recorded: their files aren't all back where the index
    /// has them, so nothing should forget those tracks yet.
    pub stuck: usize,
}

/// Put back the files of moves recorded and never indexed (see the module's
/// documentation): a save that failed after moving files, or coversyncd
/// stopping while one did. Moves it can't undo, and records it doesn't
/// recognise, stay recorded.
pub fn recover(lib: &Library) -> Result<Recovered> {
    let batches = lib.recorded_moves()?;
    let mut done = Recovered::default();
    if batches.is_empty() {
        return Ok(done);
    }
    let Some(root) = lib.library_folder()? else {
        done.stuck = batches.iter().map(|(_, r)| r.len()).sum();
        return Ok(done);
    };
    let root_fd = open_root(&root)?;
    for (batch, records) in batches {
        if !valid(&batch, &records) {
            tracing::error!(
                batch,
                "a move record this coversyncd didn't make is left alone"
            );
            done.stuck += records.len();
            continue;
        }
        let unsettled = put_back(root_fd.as_fd(), &records, &[]);
        let settled = settled(&records, &unsettled);
        lib.forget_moves(&batch, Some(&settled))?;
        done.put_back += settled.len();
        done.stuck += unsettled.len();
        tracing::info!(
            batch,
            put_back = settled.len(),
            stuck = unsettled.len(),
            "unindexed moves put back"
        );
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::LibrarySetup;
    use std::fs;

    fn values(pairs: &[(&'static str, &str)]) -> HashMap<&'static str, String> {
        pairs.iter().map(|(k, v)| (*k, v.to_string())).collect()
    }

    /// A tag set for `{album_artist}/{album}/{track_number} - {title}`.
    fn track(artist: &str, album: &str, n: &str, title: &str) -> HashMap<&'static str, String> {
        values(&[
            ("album_artist", artist),
            ("album", album),
            ("track_number", n),
            ("title", title),
        ])
    }

    fn structure() -> Template {
        Template::parse("{album_artist}/{album}/{track_number} - {title}").unwrap()
    }

    /// A scratch folder holding a state database and a Library folder with
    /// its own database, as an import leaves it; gone when dropped.
    struct Scratch {
        dir: PathBuf,
        root: PathBuf,
        lib: Library,
    }

    impl Scratch {
        fn root_fd(&self) -> OwnedFd {
            open_root(&self.root).unwrap()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn scratch(name: &str) -> Scratch {
        let dir =
            std::env::temp_dir().join(format!("coversync-relocate-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let root = dir.join("Lib");
        let lib = Library::open(&dir.join("state.sqlite3")).unwrap();
        lib.set_library_setup(LibrarySetup {
            folder: Some(root.display().to_string()),
            ..Default::default()
        })
        .unwrap();
        lib.prepare_library(&root).unwrap();
        let root = root.canonicalize().unwrap();
        Scratch { dir, root, lib }
    }

    fn touch(p: &Path, text: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    fn read(p: &Path) -> String {
        fs::read_to_string(p).unwrap()
    }

    fn refused(
        template: &Template,
        root: &Path,
        files: &[(PathBuf, HashMap<&'static str, String>)],
    ) -> String {
        plan(template, root, files).unwrap_err().to_string()
    }

    #[test]
    fn a_new_album_title_moves_its_tracks_and_other_files_trading_places() {
        let s = scratch("album");
        let old = s.root.join("Artist/Record");
        touch(&old.join("01 - One.flac"), "one");
        touch(&old.join("02 - Two.flac"), "two");
        touch(&old.join("cover.jpg"), "art");
        touch(&old.join("Scans/back.jpg"), "back");
        // Renumbered too: each takes the other's name.
        let files = vec![
            (
                old.join("01 - One.flac"),
                track("Artist", "Record!", "2", "Two"),
            ),
            (
                old.join("02 - Two.flac"),
                track("Artist", "Record!", "1", "One"),
            ),
        ];
        let p = plan(&structure(), &s.root, &files).unwrap();
        assert_eq!(p.tracks.len(), 2);
        assert_eq!(p.companions.len(), 2, "{p:?}");
        let done = apply(&s.lib, &p).unwrap();
        let new = s.root.join("Artist/Record!");
        assert_eq!(read(&new.join("02 - Two.flac")), "one");
        assert_eq!(read(&new.join("01 - One.flac")), "two");
        assert_eq!(read(&new.join("cover.jpg")), "art");
        assert_eq!(read(&new.join("Scans/back.jpg")), "back");
        assert!(!old.exists(), "the folder left empty goes");
        assert!(s.root.join("Artist").exists());
        assert_eq!(done.companions.len(), 2);
        // Recorded until the index takes them.
        assert_eq!(s.lib.recorded_moves().unwrap().len(), 1);
    }

    #[test]
    fn a_title_change_renames_one_file_and_leaves_the_rest() {
        let s = scratch("title");
        let dir = s.root.join("Artist/Record");
        touch(&dir.join("01 - One.flac"), "one");
        touch(&dir.join("02 - Two.flac"), "two");
        touch(&dir.join("cover.jpg"), "art");
        let files = vec![
            (
                dir.join("01 - One.flac"),
                track("Artist", "Record", "1", "Uno"),
            ),
            (
                dir.join("02 - Two.flac"),
                track("Artist", "Record", "2", "Two"),
            ),
        ];
        let p = plan(&structure(), &s.root, &files).unwrap();
        assert_eq!(
            p.tracks,
            [Move {
                from: dir.join("01 - One.flac"),
                to: dir.join("01 - Uno.flac")
            }]
        );
        assert!(p.companions.is_empty());
        // What planning tried a move on is gone.
        let names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert!(
            !names.iter().any(|n| n.starts_with(".coversync")),
            "{names:?}"
        );
        apply(&s.lib, &p).unwrap();
        assert_eq!(read(&dir.join("01 - Uno.flac")), "one");
        assert_eq!(read(&dir.join("cover.jpg")), "art");
    }

    #[test]
    fn a_failed_move_puts_every_file_back_and_recovery_undoes_unindexed_ones() {
        let s = scratch("undo");
        let old = s.root.join("A/Old");
        touch(&old.join("01 - One.flac"), "one");
        touch(&old.join("02 - Two.flac"), "two");
        touch(&old.join("cover.jpg"), "art");
        let files = vec![
            (old.join("01 - One.flac"), track("A", "New", "1", "One")),
            (old.join("02 - Two.flac"), track("A", "New", "2", "Two")),
        ];
        let p = plan(&structure(), &s.root, &files).unwrap();
        // Taken after planning: the second can't land, so the first comes back.
        let new = s.root.join("A/New");
        touch(&new.join("02 - Two.flac"), "someone else's");
        let err = apply(&s.lib, &p).unwrap_err().to_string();
        assert!(err.contains("took its place"), "{err}");
        assert_eq!(read(&old.join("01 - One.flac")), "one");
        assert_eq!(read(&old.join("02 - Two.flac")), "two");
        assert_eq!(read(&old.join("cover.jpg")), "art");
        assert_eq!(read(&new.join("02 - Two.flac")), "someone else's");
        assert!(!new.join("01 - One.flac").exists());
        assert!(s.lib.recorded_moves().unwrap().is_empty());

        // Moved and never indexed (coversyncd stopped, say): put back, the
        // album's other files too, and the folder made for them goes.
        fs::remove_dir_all(&new).unwrap();
        let p = plan(&structure(), &s.root, &files).unwrap();
        apply(&s.lib, &p).unwrap();
        assert!(!old.exists());
        assert_eq!(
            recover(&s.lib).unwrap(),
            Recovered {
                put_back: 3,
                stuck: 0
            }
        );
        assert_eq!(read(&old.join("01 - One.flac")), "one");
        assert_eq!(read(&old.join("02 - Two.flac")), "two");
        assert_eq!(read(&old.join("cover.jpg")), "art");
        // Which folders the save made isn't recorded, so they stay (empty).
        assert_eq!(fs::read_dir(&new).unwrap().count(), 0);
        assert!(s.lib.recorded_moves().unwrap().is_empty());
    }

    #[test]
    fn links_on_the_way_are_refused_and_nothing_outside_is_touched() {
        let s = scratch("links");
        let outside = s.dir.join("outside");
        touch(&outside.join("x.flac"), "outside");
        // A linked album folder, which a scan follows.
        std::os::unix::fs::symlink(&outside, s.root.join("Linked")).unwrap();
        let files = vec![(s.root.join("Linked/x.flac"), track("A", "New", "1", "X"))];
        let err = refused(&structure(), &s.root, &files);
        assert!(err.contains("linked folder"), "{err}");
        // A linked destination folder.
        touch(&s.root.join("B/Old/01 - X.flac"), "x");
        fs::create_dir_all(s.root.join("A")).unwrap();
        std::os::unix::fs::symlink(&outside, s.root.join("A/New")).unwrap();
        let files = vec![(
            s.root.join("B/Old/01 - X.flac"),
            track("A", "New", "1", "X"),
        )];
        let err = refused(&structure(), &s.root, &files);
        assert!(err.contains("is a link"), "{err}");
        assert_eq!(read(&outside.join("x.flac")), "outside");
        // A link among the album's other files keeps them all where they are.
        fs::remove_file(s.root.join("A/New")).unwrap();
        touch(&s.root.join("B/Old/cover.jpg"), "art");
        std::os::unix::fs::symlink(&outside, s.root.join("B/Old/Extras")).unwrap();
        let p = plan(&structure(), &s.root, &files).unwrap();
        assert_eq!(p.tracks.len(), 1);
        assert!(p.companions.is_empty(), "{p:?}");
    }

    #[test]
    fn a_link_to_a_path_from_its_own_folder_doesnt_move() {
        let s = scratch("rel-link");
        touch(&s.root.join("A/Old/real.flac"), "x");
        let from = s.root.join("A/Old/01 - X.flac");
        std::os::unix::fs::symlink("real.flac", &from).unwrap();
        // Not elsewhere, nor renamed in its folder: what it leads to may move too.
        for album in ["New", "Old"] {
            let err = refused(
                &structure(),
                &s.root,
                &[(from.clone(), track("A", album, "2", "X"))],
            );
            assert!(err.contains("link"), "{err}");
        }
        // One to a whole path still leads there.
        std::fs::remove_file(&from).unwrap();
        std::os::unix::fs::symlink(s.root.join("A/Old/real.flac"), &from).unwrap();
        let p = plan(
            &structure(),
            &s.root,
            &[(from, track("A", "New", "1", "X"))],
        )
        .unwrap();
        assert_eq!(p.tracks.len(), 1);
    }

    #[test]
    fn a_hard_link_elsewhere_isnt_taken_for_the_file_leaving() {
        let s = scratch("hard-link");
        let from = s.root.join("A/Old/01 - X.flac");
        touch(&from, "x");
        // Another name for the same file, where the file would go.
        fs::create_dir_all(s.root.join("A/New")).unwrap();
        fs::hard_link(&from, s.root.join("A/New/01 - X.flac")).unwrap();
        let err = refused(
            &structure(),
            &s.root,
            &[(from.clone(), track("A", "New", "1", "X"))],
        );
        assert!(err.contains("already exists"), "{err}");
        // And where a folder would have to be.
        fs::hard_link(&from, s.root.join("A/Deep")).unwrap();
        let t = Template::parse("{album_artist}/Deep/{title}").unwrap();
        let err = refused(&t, &s.root, &[(from.clone(), track("A", "x", "1", "X"))]);
        assert!(err.contains("needs a folder"), "{err}");
        // Nor one beside it, in its own folder.
        fs::remove_file(s.root.join("A/New/01 - X.flac")).unwrap();
        fs::hard_link(&from, s.root.join("A/Old/02 - X.flac")).unwrap();
        let err = refused(
            &structure(),
            &s.root,
            &[(from.clone(), track("A", "Old", "2", "X"))],
        );
        assert!(err.contains("already exists"), "{err}");
        // Under its own name it's the file itself, which can take a new one.
        let p = plan(
            &structure(),
            &s.root,
            &[(from, track("A", "Old", "3", "X"))],
        )
        .unwrap();
        assert_eq!(p.tracks.len(), 1);
    }

    /// A record of `from` (relative) moving to `to`, as the file `from` is.
    fn record_of(
        s: &Scratch,
        seq: i64,
        from: &str,
        staged: Option<String>,
        to: &str,
    ) -> MoveRecord {
        use std::os::unix::fs::MetadataExt;
        let st = fs::symlink_metadata(s.root.join(from)).unwrap();
        MoveRecord {
            seq,
            from: from.into(),
            staged,
            to: to.into(),
            dev: st.dev(),
            ino: st.ino(),
        }
    }

    #[test]
    fn a_move_stopped_between_two_names_leaves_one_after_recovery() {
        // Where renames can't refuse to replace, a move gives a second name
        // and drops the first; coversyncd stopped in between.
        let s = scratch("two-names");
        let batch = "00000000000000dd";
        let staged = staged_name(batch, 0);
        touch(&s.root.join("A").join(&staged), "x");
        let r = MoveRecord {
            from: "A/01 - X.flac".into(),
            ..record_of(
                &s,
                0,
                &format!("A/{staged}"),
                Some(staged.clone()),
                "B/01 - X.flac",
            )
        };
        fs::create_dir_all(s.root.join("B")).unwrap();
        fs::hard_link(s.root.join("A").join(&staged), s.root.join("B/01 - X.flac")).unwrap();
        // And one of the album's other files, given its new name too.
        touch(&s.root.join("A/cover.jpg"), "art");
        let cover = record_of(&s, 1, "A/cover.jpg", None, "B/cover.jpg");
        fs::hard_link(s.root.join("A/cover.jpg"), s.root.join("B/cover.jpg")).unwrap();
        s.lib.record_moves(batch, &[r, cover]).unwrap();
        assert_eq!(
            recover(&s.lib).unwrap(),
            Recovered {
                put_back: 2,
                stuck: 0
            }
        );
        assert_eq!(read(&s.root.join("A/01 - X.flac")), "x");
        assert_eq!(read(&s.root.join("A/cover.jpg")), "art");
        assert!(!s.root.join("A").join(&staged).exists());
        assert!(!s.root.join("B/01 - X.flac").exists());
        assert!(!s.root.join("B/cover.jpg").exists());
    }

    #[test]
    fn a_place_that_cant_be_looked_in_keeps_its_record() {
        use std::os::unix::fs::PermissionsExt;
        let s = scratch("unreadable");
        touch(&s.root.join("A/Locked/cover.jpg"), "art");
        let r = record_of(&s, 0, "A/Locked/cover.jpg", None, "B/cover.jpg");
        let locked = s.root.join("A/Locked");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        // Someone who can look in anyway (root) can't test this.
        if fs::read_dir(&locked).is_ok() {
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }
        s.lib.record_moves("00000000000000ee", &[r]).unwrap();
        let done = recover(&s.lib);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            done.unwrap(),
            Recovered {
                put_back: 0,
                stuck: 1
            }
        );
        assert_eq!(s.lib.recorded_moves().unwrap().len(), 1);
    }

    #[test]
    fn a_file_replaced_after_it_was_recorded_isnt_moved() {
        let s = scratch("replaced");
        touch(&s.root.join("A/01 - X.flac"), "x");
        let r = MoveRecord {
            ino: 1,
            ..record_of(
                &s,
                0,
                "A/01 - X.flac",
                Some(staged_name("00000000000000ff", 0)),
                "B/01 - X.flac",
            )
        };
        let err = step(s.root_fd().as_fd(), &r, true, &mut Vec::new()).unwrap_err();
        assert!(err.0.to_string().contains("replaced meanwhile"), "{err:?}");
        assert_eq!(read(&s.root.join("A/01 - X.flac")), "x");
        assert_eq!(fs::read_dir(s.root.join("A")).unwrap().count(), 1);
    }

    #[test]
    fn other_audio_on_the_way_into_a_folder_inside_keeps_the_cover() {
        let s = scratch("way-down");
        let rec = s.root.join("A/Record");
        touch(&rec.join("01 - X.flac"), "x");
        touch(&rec.join("cover.jpg"), "art");
        touch(&rec.join("New/01 - Other.flac"), "another album's");
        let t = Template::parse("{album_artist}/Record/{album}/{track_number} - {title}").unwrap();
        let files = [(rec.join("01 - X.flac"), track("A", "New", "2", "X"))];
        let p = plan(&t, &s.root, &files).unwrap();
        assert_eq!(p.tracks.len(), 1);
        assert!(p.companions.is_empty(), "{p:?}");
    }

    #[test]
    fn a_folder_doesnt_take_the_place_of_a_file_that_leaves() {
        // Undoing that would find the folder in the file's way.
        let s = scratch("file-to-folder");
        let from = s.root.join("A/Old.flac");
        touch(&from, "x");
        let t = Template::parse("{album_artist}/{album}/{title}").unwrap();
        let err = refused(
            &t,
            &s.root,
            &[(from.clone(), track("A", "Old.flac", "1", "X"))],
        );
        assert!(err.contains("needs a folder"), "{err}");
        assert_eq!(read(&from), "x");
    }

    #[test]
    fn a_folder_spelt_two_ways_is_refused_where_names_fold() {
        let placed = |a: &str, b: &str| {
            vec![
                (PathBuf::from("x/1.flac"), PathBuf::from(a)),
                (PathBuf::from("x/2.flac"), PathBuf::from(b)),
            ]
        };
        let two = placed("ABBA/Gold/1.flac", "Abba/Gold/2.flac");
        let err = places(&two, true).unwrap_err().to_string();
        assert!(err.contains("differ only in case"), "{err}");
        // Two folders where names don't fold, and one spelt one way.
        assert!(places(&two, false).is_ok());
        assert!(places(&placed("ABBA/Gold/1.flac", "ABBA/Gold/2.flac"), true).is_ok());
        // Files, as before.
        let err = places(&placed("A/1.flac", "a/1.FLAC"), true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("both be"), "{err}");
    }

    #[test]
    fn a_record_this_coversyncd_didnt_make_is_left_alone() {
        let s = scratch("bad-record");
        let outside = s.dir.join("outside");
        touch(&outside.join("x.flac"), "outside");
        touch(&s.root.join("A/x.flac"), "inside");
        let st = fs::metadata(outside.join("x.flac")).unwrap();
        use std::os::unix::fs::MetadataExt;
        // A name to wait under that's a path (outside, even).
        let record = MoveRecord {
            seq: 0,
            from: "A/x.flac".into(),
            staged: Some("../../outside/x.flac".to_string()),
            to: "B/x.flac".into(),
            dev: st.dev(),
            ino: st.ino(),
        };
        s.lib
            .record_moves("0123456789abcdef", std::slice::from_ref(&record))
            .unwrap();
        // And a batch whose id isn't one.
        let fine = MoveRecord {
            staged: Some(staged_name("nothex", 0)),
            ..record
        };
        s.lib.record_moves("nothex", &[fine]).unwrap();
        assert_eq!(
            recover(&s.lib).unwrap(),
            Recovered {
                put_back: 0,
                stuck: 2
            }
        );
        assert_eq!(read(&outside.join("x.flac")), "outside");
        assert_eq!(read(&s.root.join("A/x.flac")), "inside");
        assert_eq!(
            s.lib.recorded_moves().unwrap().len(),
            2,
            "kept, not forgotten"
        );
    }

    #[test]
    fn a_rollback_leaves_empty_folders_it_didnt_make() {
        let s = scratch("own-folders");
        let from = s.root.join("A/Old/01 - X.flac");
        touch(&from, "x");
        touch(&s.root.join("A/Old/02 - Y.flac"), "y");
        // Empty folders that were there before.
        fs::create_dir_all(s.root.join("A/New/Empty")).unwrap();
        let files = vec![
            (from.clone(), track("A", "New", "1", "X")),
            (
                s.root.join("A/Old/02 - Y.flac"),
                track("A", "Newer", "2", "Y"),
            ),
        ];
        let p = plan(&structure(), &s.root, &files).unwrap();
        touch(&s.root.join("A/Newer/02 - Y.flac"), "taken");
        assert!(apply(&s.lib, &p).is_err());
        assert_eq!(read(&from), "x");
        assert!(s.root.join("A/New/Empty").is_dir(), "not made by the save");

        // One it made inside one that was there, empty: that one stays.
        fs::create_dir_all(s.root.join("A/Pre")).unwrap();
        let t = Template::parse("{album_artist}/Pre/{album}/{track_number} - {title}").unwrap();
        let files = vec![
            (from.clone(), track("A", "New", "1", "X")),
            (
                s.root.join("A/Old/02 - Y.flac"),
                track("B", "Newer", "2", "Y"),
            ),
        ];
        let p = plan(&t, &s.root, &files).unwrap();
        touch(&s.root.join("B/Pre/Newer/02 - Y.flac"), "taken");
        assert!(apply(&s.lib, &p).is_err());
        assert_eq!(read(&from), "x");
        assert!(!s.root.join("A/Pre/New").exists(), "made by the save");
        assert!(s.root.join("A/Pre").is_dir(), "there before");
    }

    #[test]
    fn a_file_turned_into_a_link_after_planning_doesnt_move() {
        let s = scratch("late-link");
        let from = s.root.join("A/Old/01 - X.flac");
        touch(&from, "x");
        touch(&s.root.join("A/Old/real.flac"), "real");
        let p = plan(
            &structure(),
            &s.root,
            &[(from.clone(), track("A", "New", "1", "X"))],
        )
        .unwrap();
        fs::remove_file(&from).unwrap();
        std::os::unix::fs::symlink("real.flac", &from).unwrap();
        let err = apply(&s.lib, &p).unwrap_err().to_string();
        assert!(err.contains("link to a path from its own folder"), "{err}");
        assert!(fs::symlink_metadata(&from).unwrap().is_symlink());
        assert!(!s.root.join("A/New").exists());
        assert!(s.lib.recorded_moves().unwrap().is_empty());
    }

    #[test]
    fn other_files_follow_as_they_are_when_the_moves_are_made() {
        let s = scratch("late-companions");
        let old = s.root.join("A/Old");
        touch(&old.join("01 - X.flac"), "x");
        touch(&old.join("cover.jpg"), "art");
        touch(&old.join("Scans/front.jpg"), "front");
        touch(&old.join("Scans/back.jpg"), "back");
        let files = [(old.join("01 - X.flac"), track("A", "New", "1", "X"))];
        let p = plan(&structure(), &s.root, &files).unwrap();
        assert_eq!(p.companions.len(), 2);
        // A scan taken out meanwhile: the rest still go, counted as they are.
        fs::remove_file(old.join("Scans/back.jpg")).unwrap();
        let done = apply(&s.lib, &p).unwrap();
        let new = s.root.join("A/New");
        assert_eq!(read(&new.join("Scans/front.jpg")), "front");
        assert_eq!(read(&new.join("cover.jpg")), "art");
        assert_eq!(done.files, 3);

        // Audio that came meanwhile, beside a loose cover: everything stays.
        let back = s.root.join("A/Back");
        let files = [(new.join("01 - X.flac"), track("A", "Back", "1", "X"))];
        let p = plan(&structure(), &s.root, &files).unwrap();
        assert_eq!(p.companions.len(), 2);
        touch(&new.join("02 - Other.flac"), "other");
        let done = apply(&s.lib, &p).unwrap();
        assert_eq!(read(&back.join("01 - X.flac")), "x");
        assert!(done.companions.is_empty(), "{done:?}");
        assert_eq!(read(&new.join("cover.jpg")), "art");
        assert_eq!(read(&new.join("Scans/front.jpg")), "front");
    }

    #[test]
    fn audio_in_a_folder_named_otherwise_than_utf_8_keeps_the_cover() {
        use std::os::unix::ffi::OsStrExt;
        let s = scratch("non-utf8");
        let old = s.root.join("A/Old");
        touch(&old.join("01 - X.flac"), "x");
        touch(&old.join("cover.jpg"), "art");
        let odd = old.join(OsStr::from_bytes(b"Bonus \xff"));
        touch(&odd.join("01 - Bonus.flac"), "bonus");
        let files = [(old.join("01 - X.flac"), track("A", "New", "1", "X"))];
        let p = plan(&structure(), &s.root, &files).unwrap();
        assert!(p.companions.is_empty(), "{p:?}");
        // With none there, the folder stays (its name can't be recorded)
        // and the cover goes.
        fs::remove_file(odd.join("01 - Bonus.flac")).unwrap();
        touch(&odd.join("notes.txt"), "notes");
        let p = plan(&structure(), &s.root, &files).unwrap();
        assert_eq!(p.companions.len(), 1, "{p:?}");
    }

    #[test]
    fn a_folder_holding_one_of_coversyncs_own_keeps_the_albums_files() {
        let s = scratch("own-inside");
        let old = s.root.join("A/Old");
        touch(&old.join("01 - X.flac"), "x");
        touch(&old.join("Extras/.coversync-uploads/1/part"), "uploading");
        touch(&old.join("Extras/notes.txt"), "notes");
        touch(&old.join("cover.jpg"), "art");
        let files = [(old.join("01 - X.flac"), track("A", "New", "1", "X"))];
        let p = plan(&structure(), &s.root, &files).unwrap();
        // As with a link there: the album's other files all stay.
        assert!(p.companions.is_empty(), "{p:?}");
    }

    #[test]
    fn a_track_that_stays_in_a_folder_named_like_coversyncs_isnt_refused() {
        let s = scratch("own-stays");
        let at = s.root.join("A/.coversync-music/01 - X.flac");
        touch(&at, "x");
        let t = Template::parse("{album_artist}/{album}/{track_number} - {title}").unwrap();
        let p = plan(
            &t,
            &s.root,
            &[(at, track("A", ".coversync-music", "1", "X"))],
        )
        .unwrap();
        assert!(p.is_empty());
    }

    #[test]
    fn moves_wait_for_the_librarys_own_database() {
        let dir =
            std::env::temp_dir().join(format!("coversync-relocate-memory-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let root = dir.join("Lib");
        touch(&root.join("A/Old/01 - X.flac"), "x");
        let lib = Library::open(&dir.join("state.sqlite3")).unwrap();
        lib.set_library_setup(LibrarySetup {
            folder: Some(root.display().to_string()),
            ..Default::default()
        })
        .unwrap();
        lib.refresh_library().unwrap();
        let root = root.canonicalize().unwrap();
        assert!(!lib.library_stored().unwrap());
        let p = plan(
            &structure(),
            &root,
            &[(root.join("A/Old/01 - X.flac"), track("A", "New", "1", "X"))],
        )
        .unwrap();
        let err = apply(&lib, &p).unwrap_err().to_string();
        assert!(err.contains("no database yet"), "{err}");
        assert_eq!(read(&root.join("A/Old/01 - X.flac")), "x");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn what_the_structure_cant_place_is_refused() {
        let s = scratch("refused");
        let from = s.root.join("A/Old/01 - X.flac");
        touch(&from, "x");
        let one = |values| [(from.clone(), values)];
        // A value missing.
        let err = refused(
            &structure(),
            &s.root,
            &one(values(&[("album_artist", "A"), ("title", "X")])),
        );
        assert!(err.contains("would have no Album"), "{err}");
        // A file where a folder has to be.
        touch(&s.root.join("A/New"), "a file");
        let err = refused(&structure(), &s.root, &one(track("A", "New", "1", "X")));
        assert!(err.contains("needs a folder"), "{err}");
        // Names a scan skips: a file name starting with a dot, and coversync's
        // own folders, in any case (the structure makes `..` a `_` itself).
        let bare = Template::parse("{album_artist}/{album}/{title}").unwrap();
        let err = refused(&bare, &s.root, &one(track("A", "New", "1", ".hidden")));
        assert!(err.contains("a file name starting with a dot"), "{err}");
        for album in [".coversync", ".CoverSync-Uploads", ".coversync-replica"] {
            let err = refused(&structure(), &s.root, &one(track("A", album, "1", "X")));
            assert!(err.contains("keeps for itself"), "{album}: {err}");
        }
        // Other folders starting with a dot a scan looks in.
        let p = plan(
            &structure(),
            &s.root,
            &one(track(".38 Special", "Rockin", "1", "X")),
        )
        .unwrap();
        assert_eq!(p.tracks.len(), 1);
        // Two files on one path.
        touch(&s.root.join("A/Old/02 - Y.flac"), "y");
        let err = refused(
            &structure(),
            &s.root,
            &[
                (from.clone(), track("A", "Same", "1", "X")),
                (
                    s.root.join("A/Old/02 - Y.flac"),
                    track("A", "Same", "1", "X"),
                ),
            ],
        );
        assert!(err.contains("both be"), "{err}");
        // One file where another needs a folder (an optional folder there).
        let t = Template::parse("{album_artist}/<{genre}>/{title}").unwrap();
        let err = refused(
            &t,
            &s.root,
            &[
                (
                    from.clone(),
                    values(&[("album_artist", "A"), ("title", "B")]),
                ),
                (
                    s.root.join("A/Old/02 - Y.flac"),
                    values(&[("album_artist", "A"), ("genre", "B.flac"), ("title", "Y")]),
                ),
            ],
        );
        assert!(err.contains("both a file and a folder"), "{err}");
        // A file that stays.
        touch(&s.root.join("A/Other/01 - X.flac"), "stays");
        let err = refused(&structure(), &s.root, &one(track("A", "Other", "1", "X")));
        assert!(err.contains("already exists"), "{err}");
        assert_eq!(read(&from), "x");
    }

    #[test]
    fn covers_go_where_an_album_goes_and_stay_when_it_splits() {
        let s = scratch("covers");
        // Into a folder of its own inside the old one.
        let rec = s.root.join("A/Record");
        touch(&rec.join("01 - X.flac"), "x");
        touch(&rec.join("cover.jpg"), "art");
        let t = Template::parse("{album_artist}/Record/{album}/{track_number} - {title}").unwrap();
        let p = plan(
            &t,
            &s.root,
            &[(rec.join("01 - X.flac"), track("A", "Deluxe", "1", "X"))],
        )
        .unwrap();
        apply(&s.lib, &p).unwrap();
        assert_eq!(read(&rec.join("Deluxe/01 - X.flac")), "x");
        assert_eq!(read(&rec.join("Deluxe/cover.jpg")), "art");

        // Split between two albums: the cover belongs to neither.
        let old = s.root.join("B/Old");
        touch(&old.join("01 - X.flac"), "x");
        touch(&old.join("02 - Y.flac"), "y");
        touch(&old.join("cover.jpg"), "art");
        let files = vec![
            (old.join("01 - X.flac"), track("B", "One", "1", "X")),
            (old.join("02 - Y.flac"), track("B", "Two", "2", "Y")),
        ];
        let p = plan(&structure(), &s.root, &files).unwrap();
        assert!(p.companions.is_empty(), "{p:?}");

        // Disc folders, each with its own picture, and the album's.
        let old = s.root.join("C/Old");
        touch(&old.join("Disc 1/01 - X.flac"), "x");
        touch(&old.join("Disc 1/disc.jpg"), "d1");
        touch(&old.join("Disc 2/01 - Y.flac"), "y");
        touch(&old.join("cover.jpg"), "art");
        let t =
            Template::parse("{album_artist}/{album}/Disc {disc_number}/{track_number} - {title}")
                .unwrap();
        let disc = |n: &str, title: &str| {
            values(&[
                ("album_artist", "C"),
                ("album", "New"),
                ("disc_number", n),
                ("track_number", "1"),
                ("title", title),
            ])
        };
        let files = vec![
            (old.join("Disc 1/01 - X.flac"), disc("1", "X")),
            (old.join("Disc 2/01 - Y.flac"), disc("2", "Y")),
        ];
        let p = plan(&t, &s.root, &files).unwrap();
        apply(&s.lib, &p).unwrap();
        let new = s.root.join("C/New");
        assert_eq!(read(&new.join("cover.jpg")), "art");
        assert_eq!(read(&new.join("Disc 1/disc.jpg")), "d1");
        assert_eq!(read(&new.join("Disc 2/01 - Y.flac")), "y");
        assert!(!old.exists());

        // Flattened: the disc folders' pictures come up, one per name.
        let flat = Template::parse("{album_artist}/{album}/{disc_number}-{track_number} - {title}")
            .unwrap();
        let files = vec![
            (new.join("Disc 1/01 - X.flac"), disc("1", "X")),
            (new.join("Disc 2/01 - Y.flac"), disc("2", "Y")),
        ];
        touch(&new.join("Disc 2/disc.jpg"), "d2");
        let p = plan(&flat, &s.root, &files).unwrap();
        apply(&s.lib, &p).unwrap();
        assert_eq!(read(&new.join("cover.jpg")), "art", "the album's own stays");
        assert_eq!(read(&new.join("disc.jpg")), "d1");
        assert_eq!(
            read(&new.join("Disc 2/disc.jpg")),
            "d2",
            "its name was taken"
        );
        assert!(!new.join("Disc 1").exists());

        // Audio that isn't the album's keeps the folder's other files there.
        let old = s.root.join("D/Old");
        touch(&old.join("01 - X.flac"), "x");
        touch(&old.join("stray.mp3"), "not the album's");
        touch(&old.join("cover.jpg"), "art");
        let p = plan(
            &structure(),
            &s.root,
            &[(old.join("01 - X.flac"), track("D", "New", "1", "X"))],
        )
        .unwrap();
        assert!(p.companions.is_empty(), "{p:?}");
    }

    #[test]
    fn a_flat_album_into_disc_folders_takes_its_cover_and_counts_its_files() {
        let s = scratch("into-discs");
        let old = s.root.join("A/Old");
        touch(&old.join("01 - X.flac"), "x");
        touch(&old.join("02 - Y.flac"), "y");
        touch(&old.join("cover.jpg"), "art");
        touch(&old.join("Scans/front.jpg"), "f");
        touch(&old.join("Scans/back.jpg"), "b");
        let t =
            Template::parse("{album_artist}/{album}/Disc {disc_number}/{track_number} - {title}")
                .unwrap();
        let disc = |n: &str, title: &str| {
            values(&[
                ("album_artist", "A"),
                ("album", "New"),
                ("disc_number", n),
                ("track_number", "1"),
                ("title", title),
            ])
        };
        let files = vec![
            (old.join("01 - X.flac"), disc("1", "X")),
            (old.join("02 - Y.flac"), disc("2", "Y")),
        ];
        let p = plan(&t, &s.root, &files).unwrap();
        let done = apply(&s.lib, &p).unwrap();
        let new = s.root.join("A/New");
        assert_eq!(read(&new.join("cover.jpg")), "art");
        assert_eq!(read(&new.join("Scans/back.jpg")), "b");
        assert_eq!(done.files, 2 + 1 + 2, "two tracks, the cover, two scans");
        assert!(!old.exists());
    }

    #[test]
    fn audio_kept_in_coversyncs_own_folder_leaves_its_files_there() {
        let s = scratch("own-source");
        let own = s.root.join(".coversync");
        touch(&own.join("01 - X.flac"), "x");
        let p = plan(
            &structure(),
            &s.root,
            &[(own.join("01 - X.flac"), track("A", "New", "1", "X"))],
        )
        .unwrap();
        assert!(p.companions.is_empty(), "{p:?}");
        apply(&s.lib, &p).unwrap();
        assert!(own.join("library.sqlite3").is_file());
    }
}
