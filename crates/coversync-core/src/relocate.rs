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
}

const DIR: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// Names that are coversync's own: its database's folder, and files a save
/// keeps beside the album's while it works (`.coversync-tmp-*` copies and
/// `.coversync-move-*` files waiting to move).
fn own(name: &OsStr) -> bool {
    name.as_bytes().starts_with(b".coversync")
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

/// [`open_dir`], making the folders missing on the way.
fn make_dirs(root: BorrowedFd<'_>, rel: &Path) -> io::Result<OwnedFd> {
    let mut fd = openat(root, ".", DIR, Mode::empty())?;
    for name in names(rel)? {
        match mkdirat(&fd, name, Mode::from_raw_mode(0o777)) {
            Ok(()) | Err(Errno::EXIST) => {}
            Err(e) => return Err(e.into()),
        }
        fd = openat(&fd, name, DIR, Mode::empty())?;
    }
    Ok(fd)
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
/// name, which can't replace anything either, then drop the first; folders
/// there, or where hard links aren't, are checked first and then renamed.
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
    match linkat(from_dir, from, to_dir, to, AtFlags::empty()) {
        Ok(()) => {
            if let Err(e) = unlinkat(from_dir, from, AtFlags::empty()) {
                let _ = unlinkat(to_dir, to, AtFlags::empty());
                return Err(e.into());
            }
            return Ok(());
        }
        Err(Errno::EXIST) => return Err(Errno::EXIST.into()),
        Err(_) => {}
    }
    if entry(to_dir, to)?.is_some() {
        return Err(Errno::EXIST.into());
    }
    Ok(renameat(from_dir, from, to_dir, to)?)
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
        let Some(st) = entry(dir, &name)? else {
            continue;
        };
        out.push((name, kind(&st)));
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
/// another file system is in the way, or a file to move is a link that
/// would break.
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
                // A scan skips names starting with a dot (and `..` isn't a name).
                let hidden = to.components().any(|c| match c {
                    Component::Normal(n) => n.as_bytes().starts_with(b"."),
                    _ => true,
                });
                if hidden {
                    return Err(Error::Invalid(format!(
                        "The folder structure would put {file} at {}, where a scan wouldn't find it: no name in the path can start with a dot.",
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

    // Where every file ends up: none twice, and none where another needs a folder.
    let mut finals: HashMap<String, &Path> = HashMap::new();
    for (from, to) in &placed {
        let at: &Path = if from == to { from } else { to };
        if finals.insert(key(at, folded), at).is_some() {
            return Err(Error::Invalid(format!(
                "Two files would both be {}.",
                at.display()
            )));
        }
    }
    let mut folders: HashSet<String> = HashSet::new();
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
            folders.insert(k);
        }
    }

    // The files that move, as they are.
    let mut leaving: HashSet<(u64, u64)> = HashSet::new();
    let mut devices = Vec::new();
    for (from, to) in &moving {
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
        match kind(&st) {
            FileType::RegularFile => {}
            FileType::Symlink => {
                let target = io(readlinkat(&fd, name, Vec::new()))?;
                if !target.as_bytes().starts_with(b"/") && to.parent() != from.parent() {
                    return Err(Error::Invalid(format!(
                        "{} is a link to a path from its own folder, which would lead elsewhere from another folder.",
                        from.display()
                    )));
                }
            }
            _ => {
                return Err(Error::Invalid(format!("{} isn't a file.", from.display())));
            }
        }
        leaving.insert((st.st_dev, st.st_ino));
        devices.push(io(fstat(&fd))?.st_dev);
    }

    // Where they go: folders, or room for them, on the way, and room at the end.
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
                FileType::Directory => fd = io(openat(&fd, n, DIR, Mode::empty()))?,
                FileType::Symlink => {
                    return Err(Error::Invalid(format!(
                        "{} is a link, which a save doesn't put files in.",
                        at.display()
                    )));
                }
                // A file that moves away first leaves room for the folder.
                _ if leaving.contains(&(st.st_dev, st.st_ino)) => {
                    whole = false;
                    break;
                }
                _ => {
                    return Err(Error::Invalid(format!(
                        "{} is a file where the folder structure needs a folder.",
                        at.display()
                    )));
                }
            }
        }
        if io(fstat(&fd))?.st_dev != *device {
            return Err(Error::Invalid(format!(
                "{} would be on another file system than {}.",
                to.display(),
                from.display()
            )));
        }
        if whole
            && let Some(st) = entry(fd.as_fd(), name)?
            && !leaving.contains(&(st.st_dev, st.st_ino))
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
    plan.companions = companions(root_fd, &placed, &departing, &taken)
        .iter()
        .map(|(f, t)| absolute(f, t))
        .collect();
    plan.tracks = moving.iter().map(|(f, t)| absolute(f, t)).collect();
    Ok(plan)
}

/// The names the album's tracks will have, which its other files can't take.
struct Taken<'a> {
    finals: &'a HashMap<String, &'a Path>,
    /// The folders on the way to them.
    folders: &'a HashSet<String>,
    folded: bool,
}

/// The album's other files that follow its tracks: from each folder whose
/// tracks all go to one other folder (a track's folder, or the album folder
/// a scan gives it, above disc folders), where nothing else would stay
/// behind, each file and audio-free folder in it, to the same name there
/// unless that's taken. A folder holding audio that stays, a link,
/// something stranger, or what can't be listed keeps everything in it.
fn companions(
    root: BorrowedFd<'_>,
    placed: &[(PathBuf, PathBuf)],
    departing: &HashSet<&Path>,
    taken: &Taken<'_>,
) -> Vec<(PathBuf, PathBuf)> {
    let parent = |p: &Path| p.parent().unwrap_or(Path::new("")).to_path_buf();
    let mut goes: BTreeMap<PathBuf, BTreeSet<PathBuf>> = BTreeMap::new();
    for (from, to) in placed {
        goes.entry(parent(from)).or_default().insert(parent(to));
        goes.entry(scan::album_dir(from))
            .or_default()
            .insert(scan::album_dir(to));
    }
    let mut out = Vec::new();
    let mut claimed: HashSet<String> = HashSet::new();
    for (from, to) in goes {
        let mut to = to.into_iter();
        let (Some(to), None) = (to.next(), to.next()) else {
            continue;
        };
        // Not the Library folder's own files, nor into it.
        if to == from || from.as_os_str().is_empty() || to.as_os_str().is_empty() {
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
        for name in names {
            let dest = to.join(&name);
            let k = key(&dest, taken.folded);
            if taken.finals.contains_key(&k) || taken.folders.contains(&k) || claimed.contains(&k) {
                continue;
            }
            if whole && !matches!(entry(at.as_fd(), &name), Ok(None)) {
                continue;
            }
            claimed.insert(k);
            out.push((from.join(&name), dest));
        }
    }
    out
}

/// The names in the folder `dir` (`rel`) that can follow the album to `to`:
/// its files that aren't audio and its folders with no audio at all. None
/// when it all stays: there's audio there that isn't the album's moving
/// away, a link or something stranger, or what can't be listed. coversync's
/// own names, and the way down to `to` (an album moving into a folder of
/// its own inside this one), stay.
fn movable(
    dir: BorrowedFd<'_>,
    rel: &Path,
    departing: &HashSet<&Path>,
    to: &Path,
) -> Option<Vec<OsString>> {
    let mut names = Vec::new();
    for (name, kind) in list(dir).ok()? {
        let path = rel.join(&name);
        if own(&name) || to.starts_with(&path) {
            continue;
        }
        match kind {
            FileType::RegularFile if tags::is_audio(&path) => {
                if !departing.contains(path.as_path()) {
                    return None;
                }
            }
            FileType::RegularFile => names.push(name),
            FileType::Directory => {
                let sub = openat(dir, &name, DIR, Mode::empty()).ok()?;
                // A folder of tracks that move (a disc's) has its own way.
                if !departing_audio(sub.as_fd(), &path, departing)? {
                    names.push(name);
                }
            }
            _ => return None,
        }
    }
    Some(names)
}

/// Whether the album's moving tracks are under `dir` (`rel`); None when
/// something there keeps it all in place (see [`movable`]).
fn departing_audio(dir: BorrowedFd<'_>, rel: &Path, departing: &HashSet<&Path>) -> Option<bool> {
    let mut found = false;
    for (name, kind) in list(dir).ok()? {
        let path = rel.join(&name);
        if own(&name) {
            continue;
        }
        match kind {
            FileType::RegularFile if tags::is_audio(&path) => {
                if !departing.contains(path.as_path()) {
                    return None;
                }
                found = true;
            }
            FileType::RegularFile => {}
            FileType::Directory => {
                let sub = openat(dir, &name, DIR, Mode::empty()).ok()?;
                found |= departing_audio(sub.as_fd(), &path, departing)?;
            }
            _ => return None,
        }
    }
    Some(found)
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

/// Make the moves [`plan`] gave, after recording them (see the module's
/// documentation). Tracks go aside first, under names no file has, so one
/// can take another's place; one that can't reach its own puts them all
/// back. Then the album's other files follow, each on its own: one that
/// can't stays where it was. Folders the album left empty go.
pub fn apply(lib: &Library, plan: &Plan) -> Result<Applied> {
    if plan.is_empty() {
        return Ok(Applied::default());
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
    let mut records = Vec::new();
    for (i, m) in plan.tracks.iter().chain(&plan.companions).enumerate() {
        let (from, to) = (rel(&m.from)?, rel(&m.to)?);
        let (dir, name) = split(&from)?;
        let st = entry(open_dir(root_fd, dir)?.as_fd(), name)?.ok_or_else(|| {
            Error::Invalid(format!(
                "{} isn't in the Library folder now.",
                from.display()
            ))
        })?;
        records.push(MoveRecord {
            seq: i as i64,
            from: from.to_string_lossy().into_owned(),
            staged: (i < plan.tracks.len()).then(|| format!(".coversync-move-{batch}-{i}")),
            to: to.to_string_lossy().into_owned(),
            dev: st.st_dev,
            ino: st.st_ino,
        });
    }
    lib.record_moves(&batch, &records)?;
    let (tracks, companions) = records.split_at(plan.tracks.len());
    let step = |r: &MoveRecord, aside: bool| -> std::result::Result<(), (io::Error, String)> {
        let what = |e: io::Error| (e, format!("{} to {}", r.from, r.to));
        let (dir, name) = split(Path::new(&r.from)).map_err(what)?;
        let fd = open_dir(root_fd, dir).map_err(what)?;
        match (&r.staged, aside) {
            (Some(staged), true) => rename_new(fd.as_fd(), name, fd.as_fd(), OsStr::new(staged)),
            (staged, _) => {
                let (to_dir, to_name) = split(Path::new(&r.to)).map_err(what)?;
                let to_fd = make_dirs(root_fd, to_dir).map_err(what)?;
                let from = staged.as_deref().map_or(name, OsStr::new);
                rename_new(fd.as_fd(), from, to_fd.as_fd(), to_name)
            }
        }
        .map_err(what)
    };
    let placed = tracks
        .iter()
        .try_for_each(|r| step(r, true))
        .and_then(|()| tracks.iter().try_for_each(|r| step(r, false)));
    if let Err((e, what)) = placed {
        let unsettled = put_back(root_fd, &records);
        lib.forget_moves(&batch, Some(&settled(&records, &unsettled)))?;
        let why = match e.kind() {
            io::ErrorKind::AlreadyExists => "something took its place meanwhile".to_string(),
            _ => e.to_string(),
        };
        return Err(Error::Invalid(format!("couldn't move {what}: {why}")));
    }
    let mut followed = Vec::new();
    for (r, m) in companions.iter().zip(&plan.companions) {
        match step(r, false) {
            Ok(()) => followed.push(m.clone()),
            Err((e, what)) => tracing::warn!("{what} stays: {e}"),
        }
    }
    prune(root_fd, &parents(&records, |r| &r.from));
    Ok(Applied {
        batch,
        tracks: plan.tracks.clone(),
        companions: followed,
    })
}

/// Put back the files of recorded moves, each where it was (so files that
/// traded places trade back), and remove the folders the moves made, if
/// empty. A file found neither at its new place nor waiting aside is left
/// alone. Gives the moves it couldn't undo.
fn put_back(root: BorrowedFd<'_>, records: &[MoveRecord]) -> Vec<i64> {
    let is_it = |rel: &Path, name: &OsStr, r: &MoveRecord| {
        open_dir(root, rel)
            .ok()
            .and_then(|fd| entry(fd.as_fd(), name).ok().flatten())
            .is_some_and(|st| st.st_dev == r.dev && st.st_ino == r.ino)
    };
    let mut unsettled = Vec::new();
    // From their new places aside (or, for files that didn't wait aside,
    // straight back), then from aside back.
    for r in records.iter().rev() {
        let (Ok((from_dir, from_name)), Ok((to_dir, to_name))) =
            (split(Path::new(&r.from)), split(Path::new(&r.to)))
        else {
            continue;
        };
        if !is_it(to_dir, to_name, r) {
            continue;
        }
        let back = r.staged.as_deref().map_or(from_name, OsStr::new);
        let moved = open_dir(root, to_dir).and_then(|to_fd| {
            let from_fd = make_dirs(root, from_dir)?;
            rename_new(to_fd.as_fd(), to_name, from_fd.as_fd(), back)
        });
        if let Err(e) = moved {
            tracing::error!(from = %r.to, to = %r.from, "couldn't put a file back: {e}");
            unsettled.push(r.seq);
        }
    }
    for r in records {
        let (Some(staged), Ok((dir, name))) = (&r.staged, split(Path::new(&r.from))) else {
            continue;
        };
        let staged = OsStr::new(staged);
        if unsettled.contains(&r.seq) || !is_it(dir, staged, r) {
            continue;
        }
        let moved =
            open_dir(root, dir).and_then(|fd| rename_new(fd.as_fd(), staged, fd.as_fd(), name));
        if let Err(e) = moved {
            tracing::error!(path = %r.from, "couldn't put a file back: {e}");
            unsettled.push(r.seq);
        }
    }
    prune(root, &parents(records, |r| &r.to));
    unsettled
}

/// Put back the files of moves recorded and never indexed (see the module's
/// documentation): a save that failed after moving files, or coversyncd
/// stopping while one did. Moves it can't undo stay recorded, to try again.
/// Gives how many it undid or found nothing to undo for.
pub fn recover(lib: &Library) -> Result<usize> {
    let batches = lib.recorded_moves()?;
    if batches.is_empty() {
        return Ok(0);
    }
    let Some(root) = lib.library_folder()? else {
        return Ok(0);
    };
    let root_fd = open_root(&root)?;
    let mut count = 0;
    for (batch, records) in batches {
        let unsettled = put_back(root_fd.as_fd(), &records);
        let done = settled(&records, &unsettled);
        count += done.len();
        lib.forget_moves(&batch, Some(&done))?;
        tracing::info!(
            batch,
            put_back = done.len(),
            stuck = unsettled.len(),
            "unindexed moves put back"
        );
    }
    Ok(count)
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
        assert_eq!(recover(&s.lib).unwrap(), 3);
        assert_eq!(read(&old.join("01 - One.flac")), "one");
        assert_eq!(read(&old.join("02 - Two.flac")), "two");
        assert_eq!(read(&old.join("cover.jpg")), "art");
        assert!(!new.exists());
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
    fn a_relative_link_moves_only_within_its_folder() {
        let s = scratch("rel-link");
        touch(&s.root.join("A/Old/real.flac"), "x");
        let from = s.root.join("A/Old/01 - X.flac");
        std::os::unix::fs::symlink("real.flac", &from).unwrap();
        let err = refused(
            &structure(),
            &s.root,
            &[(from.clone(), track("A", "New", "1", "X"))],
        );
        assert!(err.contains("link"), "{err}");
        // Renamed in its own folder, it still leads to the same file.
        let p = plan(
            &structure(),
            &s.root,
            &[(from, track("A", "Old", "2", "X"))],
        )
        .unwrap();
        assert_eq!(p.tracks.len(), 1);
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
        // Names a scan skips (the structure makes `..` a `_` itself).
        for album in [".hidden", ".coversync"] {
            let err = refused(&structure(), &s.root, &one(track("A", album, "1", "X")));
            assert!(err.contains("start with a dot"), "{album}: {err}");
        }
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
}
