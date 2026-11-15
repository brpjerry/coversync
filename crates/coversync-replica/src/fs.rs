//! [`FsStorage`]: a sync folder on a plain file system (desktops, a mounted
//! card).
//!
//! On Unix every path is opened one name at a time from the sync folder,
//! relative to the folder above (`openat`) and never following a link
//! (`O_NOFOLLOW`), for puts, moves, deletes, the marker and partial files
//! alike. A link inside the folder can't send a write anywhere else, even
//! one made while a sync runs. Elsewhere (Windows) each name is checked for
//! a link first, which a link made at just the wrong moment could slip past.

use std::io;
use std::path::Path;

use coversync_protocol::{BASIC_OPS, Capabilities, valid_path};

pub use imp::FsStorage;

/// The marker's name in the client's folder.
const MARKER: &str = "device.json";
/// The facts journal's.
const JOURNAL: &str = "facts";
/// Partial downloads' folder in the client's folder.
const PARTIAL: &str = "partial";

fn not_a_path(path: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("{path:?} isn't a path in the sync folder"),
    )
}

fn checked(path: &str) -> io::Result<&str> {
    if valid_path(path) {
        Ok(path)
    } else {
        Err(not_a_path(path))
    }
}

/// `path`'s folders and its last name.
fn split(path: &str) -> (Option<&str>, &str) {
    match path.rsplit_once('/') {
        Some((dirs, name)) => (Some(dirs), name),
        None => (None, path),
    }
}

fn base_capabilities(root: &Path, case_insensitive: bool, max_name: Option<u32>) -> Capabilities {
    // The longest whole path the system takes (Linux's PATH_MAX, Windows'
    // long paths), less the folder's own path and the slash after it.
    const SYSTEM_MAX_PATH: usize = 4095;
    let root_len = root.as_os_str().len() + 1;
    Capabilities {
        max_path_bytes: SYSTEM_MAX_PATH
            .checked_sub(root_len)
            .and_then(|n| u32::try_from(n).ok()),
        max_name_bytes: max_name,
        case_insensitive,
        ops: BASIC_OPS.iter().map(|s| s.to_string()).collect(),
    }
}

#[cfg(unix)]
mod imp {
    use std::ffi::CStr;
    use std::fs::File;
    use std::io::{self, Read, Write};
    use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
    use std::path::Path;
    use std::sync::Mutex;

    use coversync_protocol::{CLIENT_DIR, Capabilities, Listed};
    use rustix::fs::{
        AtFlags, Dir, FileType, Mode, OFlags, fstatvfs, mkdirat, openat, renameat, statat, unlinkat,
    };
    use rustix::io::Errno;

    use super::{JOURNAL, MARKER, PARTIAL, base_capabilities, checked, split};
    use crate::storage::{Entry, Partial, Source, Storage};

    const DIR: OFlags = OFlags::RDONLY
        .union(OFlags::DIRECTORY)
        .union(OFlags::NOFOLLOW)
        .union(OFlags::CLOEXEC);
    const READ: OFlags = OFlags::RDONLY
        .union(OFlags::NOFOLLOW)
        .union(OFlags::CLOEXEC);
    const WRITE: OFlags = OFlags::WRONLY
        .union(OFlags::CREATE)
        .union(OFlags::NOFOLLOW)
        .union(OFlags::CLOEXEC);

    fn file_mode() -> Mode {
        Mode::from_raw_mode(0o666)
    }

    fn dir_mode() -> Mode {
        Mode::from_raw_mode(0o777)
    }

    pub struct FsStorage {
        root: OwnedFd,
        /// `<root>/.coversync-replica`
        own: OwnedFd,
        /// `<root>/.coversync-replica/partial`
        partial: OwnedFd,
        capabilities: Capabilities,
        /// The journal, kept open for appending.
        journal: Mutex<Option<File>>,
    }

    /// The folder `name` in `at`, made if it's missing, opened without
    /// following a link.
    fn make_dir(at: BorrowedFd<'_>, name: &str) -> io::Result<OwnedFd> {
        match mkdirat(at, name, dir_mode()) {
            Ok(()) | Err(Errno::EXIST) => {}
            Err(e) => return Err(e.into()),
        }
        Ok(openat(at, name, DIR, Mode::empty())?)
    }

    fn entry(st: &rustix::fs::Stat) -> Entry {
        match FileType::from_raw_mode(st.st_mode as _) {
            FileType::RegularFile => Entry::File {
                size: st.st_size as u64,
            },
            FileType::Directory => Entry::Dir,
            _ => Entry::Other,
        }
    }

    struct PartialFile(File);

    impl Write for PartialFile {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.write(buf)
        }
        fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
            self.0.write_all(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Partial for PartialFile {
        fn sync(&mut self) -> io::Result<()> {
            self.0.sync_data()
        }
    }

    impl FsStorage {
        /// The sync folder at `root`, which must exist. The folder itself
        /// may be reached through a link; nothing in it is.
        pub fn open(root: &Path) -> io::Result<Self> {
            let root_path = std::fs::canonicalize(root)?;
            let root = rustix::fs::open(
                &root_path,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
                Mode::empty(),
            )?;
            let own = make_dir(root.as_fd(), CLIENT_DIR)?;
            let partial = make_dir(own.as_fd(), PARTIAL)?;
            let case_insensitive = probe_case(own.as_fd())?;
            let max_name = fstatvfs(&root)
                .ok()
                .and_then(|v| u32::try_from(v.f_namemax).ok())
                .filter(|&n| n > 0);
            Ok(Self {
                capabilities: base_capabilities(&root_path, case_insensitive, max_name),
                root,
                own,
                partial,
                journal: Mutex::new(None),
            })
        }

        /// The folder holding `path`'s last name, opened name by name, and
        /// that name. None stands for the sync folder itself.
        fn parent<'p>(&self, path: &'p str) -> io::Result<(Option<OwnedFd>, &'p str)> {
            let (dirs, name) = split(checked(path)?);
            Ok((dirs.map(|d| self.walk(d)).transpose()?, name))
        }

        fn walk(&self, dirs: &str) -> io::Result<OwnedFd> {
            let mut at: Option<OwnedFd> = None;
            for name in dirs.split('/') {
                let base = at.as_ref().map_or(self.root.as_fd(), |f| f.as_fd());
                at = Some(openat(base, name, DIR, Mode::empty()).map_err(link_error)?);
            }
            at.ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))
        }

        fn fd<'a>(&'a self, at: &'a Option<OwnedFd>) -> BorrowedFd<'a> {
            at.as_ref().map_or(self.root.as_fd(), |f| f.as_fd())
        }

        fn with_journal<T>(&self, f: impl FnOnce(&mut File) -> io::Result<T>) -> io::Result<T> {
            let mut journal = self.journal.lock().unwrap_or_else(|e| e.into_inner());
            if journal.is_none() {
                let fd = openat(&self.own, JOURNAL, WRITE | OFlags::APPEND, file_mode())?;
                *journal = Some(File::from(fd));
            }
            f(journal.as_mut().expect("opened above"))
        }
    }

    /// A folder that is a link reads as ELOOP (or ENOTDIR): say so.
    fn link_error(e: Errno) -> io::Error {
        if e == Errno::LOOP {
            io::Error::new(io::ErrorKind::InvalidInput, "goes through a symbolic link")
        } else {
            e.into()
        }
    }

    /// Whether names that differ only by case are one file here.
    fn probe_case(own: BorrowedFd<'_>) -> io::Result<bool> {
        let lower = "case-probe";
        let upper = "CASE-PROBE";
        let _ = unlinkat(own, upper, AtFlags::empty());
        drop(openat(own, lower, WRITE, file_mode())?);
        let insensitive = statat(own, upper, AtFlags::SYMLINK_NOFOLLOW).is_ok();
        let _ = unlinkat(own, lower, AtFlags::empty());
        Ok(insensitive)
    }

    fn not_found(e: &Errno) -> bool {
        *e == Errno::NOENT || *e == Errno::NOTDIR
    }

    impl Storage for FsStorage {
        fn lock(&self) -> io::Result<Option<Box<dyn Send>>> {
            let fd = openat(&self.own, "lock", WRITE, file_mode()).map_err(link_error)?;
            match rustix::fs::flock(&fd, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => Ok(Some(Box::new(fd))),
                Err(Errno::WOULDBLOCK) => Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "another sync is running in this folder",
                )),
                Err(e) => Err(e.into()),
            }
        }

        fn capabilities(&self) -> Capabilities {
            self.capabilities.clone()
        }

        fn free_bytes(&self) -> io::Result<Option<u64>> {
            let v = fstatvfs(&self.root)?;
            Ok(Some(v.f_bavail.saturating_mul(v.f_frsize)))
        }

        fn list(&self, each: &mut dyn FnMut(Listed)) -> io::Result<()> {
            fn walk(dir: OwnedFd, prefix: &str, each: &mut dyn FnMut(Listed)) -> io::Result<()> {
                let mut entries = Dir::read_from(&dir)?;
                while let Some(e) = entries.read() {
                    let e = e?;
                    let raw: &CStr = e.file_name();
                    let bytes = raw.to_bytes();
                    if bytes == b"." || bytes == b".." {
                        continue;
                    }
                    // Paths are UTF-8 in the API: a name that isn't can't be
                    // the server's, and is left alone.
                    let Ok(name) = raw.to_str() else {
                        tracing::debug!(?raw, "skipping a name that isn't UTF-8");
                        continue;
                    };
                    if prefix.is_empty() && name.eq_ignore_ascii_case(CLIENT_DIR) {
                        continue;
                    }
                    let path = if prefix.is_empty() {
                        name.to_string()
                    } else {
                        format!("{prefix}/{name}")
                    };
                    let kind = match e.file_type() {
                        FileType::Unknown => match statat(&dir, raw, AtFlags::SYMLINK_NOFOLLOW) {
                            Ok(st) => entry(&st),
                            Err(e) if not_found(&e) => continue,
                            Err(e) => return Err(e.into()),
                        },
                        FileType::Directory => Entry::Dir,
                        FileType::RegularFile => match statat(&dir, raw, AtFlags::SYMLINK_NOFOLLOW)
                        {
                            Ok(st) => entry(&st),
                            Err(e) if not_found(&e) => continue,
                            Err(e) => return Err(e.into()),
                        },
                        _ => Entry::Other,
                    };
                    match kind {
                        Entry::File { size } => each(Listed { path, size }),
                        Entry::Dir => match openat(&dir, raw, DIR, Mode::empty()) {
                            Ok(sub) => walk(sub, &path, each)?,
                            // Gone meanwhile, or swapped for a link.
                            Err(e) if not_found(&e) || e == Errno::LOOP => {}
                            Err(e) => return Err(e.into()),
                        },
                        Entry::Other => {}
                    }
                }
                Ok(())
            }
            let root = openat(&self.root, ".", DIR, Mode::empty())?;
            walk(root, "", each)
        }

        fn stat(&self, path: &str) -> io::Result<Option<Entry>> {
            let (at, name) = match self.parent(path) {
                Ok(p) => p,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(e) if e.raw_os_error() == Some(Errno::NOTDIR.raw_os_error()) => {
                    return Ok(None);
                }
                Err(e) => return Err(e),
            };
            match statat(self.fd(&at), name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(st) => Ok(Some(entry(&st))),
                Err(e) if not_found(&e) => Ok(None),
                Err(e) => Err(e.into()),
            }
        }

        fn remove_file(&self, path: &str) -> io::Result<()> {
            let (at, name) = self.parent(path)?;
            Ok(unlinkat(self.fd(&at), name, AtFlags::empty())?)
        }

        fn create_dirs(&self, path: &str) -> io::Result<()> {
            checked(path)?;
            let mut at: Option<OwnedFd> = None;
            for name in path.split('/') {
                let base = at.as_ref().map_or(self.root.as_fd(), |f| f.as_fd());
                at = Some(make_dir(base, name).map_err(|e| match e.raw_os_error() {
                    Some(n) if n == Errno::LOOP.raw_os_error() => link_error(Errno::LOOP),
                    _ => e,
                })?);
            }
            Ok(())
        }

        fn remove_dir(&self, path: &str) -> io::Result<()> {
            let (at, name) = match self.parent(path) {
                Ok(p) => p,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(e),
            };
            match unlinkat(self.fd(&at), name, AtFlags::REMOVEDIR) {
                Ok(()) => Ok(()),
                Err(e) if e == Errno::NOENT || e == Errno::NOTEMPTY || e == Errno::EXIST => Ok(()),
                Err(e) => Err(e.into()),
            }
        }

        fn rename(&self, from: &str, to: &str) -> io::Result<()> {
            let (a, a_name) = self.parent(from)?;
            let (b, b_name) = self.parent(to)?;
            Ok(renameat(self.fd(&a), a_name, self.fd(&b), b_name)?)
        }

        fn open(&self, path: &str) -> io::Result<Box<dyn Source>> {
            let (at, name) = self.parent(path)?;
            let fd = openat(self.fd(&at), name, READ, Mode::empty()).map_err(link_error)?;
            Ok(Box::new(File::from(fd)))
        }

        fn partial_len(&self, name: &str) -> io::Result<Option<u64>> {
            match statat(&self.partial, name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(st) => Ok(match entry(&st) {
                    Entry::File { size } => Some(size),
                    _ => None,
                }),
                Err(Errno::NOENT) => Ok(None),
                Err(e) => Err(e.into()),
            }
        }

        fn write_partial(&self, name: &str, resume: bool) -> io::Result<Box<dyn Partial>> {
            let how = if resume {
                WRITE | OFlags::APPEND
            } else {
                WRITE | OFlags::TRUNC
            };
            let fd = openat(&self.partial, name, how, file_mode()).map_err(link_error)?;
            Ok(Box::new(PartialFile(File::from(fd))))
        }

        fn read_partial(&self, name: &str) -> io::Result<Box<dyn Read + Send>> {
            let fd = openat(&self.partial, name, READ, Mode::empty()).map_err(link_error)?;
            Ok(Box::new(File::from(fd)))
        }

        fn remove_partial(&self, name: &str) -> io::Result<()> {
            match unlinkat(&self.partial, name, AtFlags::empty()) {
                Ok(()) | Err(Errno::NOENT) => Ok(()),
                Err(e) => Err(e.into()),
            }
        }

        fn publish(&self, name: &str, path: &str, _replace: bool) -> io::Result<()> {
            let (at, to) = match self.parent(path) {
                Ok(p) => p,
                // Its folder went (removed by hand mid-sync): made again.
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    let (dirs, _) = split(path);
                    if let Some(dirs) = dirs {
                        self.create_dirs(dirs)?;
                    }
                    self.parent(path)?
                }
                Err(e) => return Err(e),
            };
            Ok(renameat(&self.partial, name, self.fd(&at), to)?)
        }

        fn clear_partials(&self) -> io::Result<()> {
            let mut entries = Dir::read_from(&self.partial)?;
            while let Some(e) = entries.read() {
                let e = e?;
                let name = e.file_name();
                if matches!(name.to_bytes(), b"." | b"..") {
                    continue;
                }
                match unlinkat(&self.partial, name, AtFlags::empty()) {
                    Ok(()) | Err(Errno::NOENT) => {}
                    Err(e) => return Err(e.into()),
                }
            }
            Ok(())
        }

        fn read_marker(&self) -> io::Result<Option<Vec<u8>>> {
            match openat(&self.own, MARKER, READ, Mode::empty()) {
                Ok(fd) => {
                    let mut out = Vec::new();
                    File::from(fd).read_to_end(&mut out)?;
                    Ok(Some(out))
                }
                Err(Errno::NOENT) => Ok(None),
                Err(e) => Err(link_error(e)),
            }
        }

        fn write_marker(&self, bytes: &[u8]) -> io::Result<()> {
            let tmp = "device.json.new";
            let mut f = File::from(openat(&self.own, tmp, WRITE | OFlags::TRUNC, file_mode())?);
            f.write_all(bytes)?;
            f.sync_data()?;
            Ok(renameat(&self.own, tmp, &self.own, MARKER)?)
        }

        fn read_journal(&self) -> io::Result<Vec<u8>> {
            match openat(&self.own, JOURNAL, READ, Mode::empty()) {
                Ok(fd) => {
                    let mut out = Vec::new();
                    File::from(fd).read_to_end(&mut out)?;
                    Ok(out)
                }
                Err(Errno::NOENT) => Ok(Vec::new()),
                Err(e) => Err(link_error(e)),
            }
        }

        fn append_journal(&self, bytes: &[u8]) -> io::Result<()> {
            self.with_journal(|f| f.write_all(bytes))
        }

        fn replace_journal(&self, bytes: &[u8]) -> io::Result<()> {
            self.with_journal(|f| {
                // Appending, so the write lands at the new end: the start.
                f.set_len(0)?;
                f.write_all(bytes)
            })
        }
    }
}

#[cfg(not(unix))]
mod imp {
    use std::fs::{self, File, OpenOptions};
    use std::io::{self, Read, Write};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use coversync_protocol::{CLIENT_DIR, Capabilities, Listed};

    use super::{JOURNAL, MARKER, PARTIAL, base_capabilities, checked, split};
    use crate::storage::{Entry, Partial, Source, Storage};

    pub struct FsStorage {
        root: PathBuf,
        own: PathBuf,
        partial: PathBuf,
        capabilities: Capabilities,
        journal: Mutex<Option<File>>,
    }

    struct PartialFile(File);

    impl Write for PartialFile {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Partial for PartialFile {
        fn sync(&mut self) -> io::Result<()> {
            self.0.sync_data()
        }
    }

    fn entry(m: &fs::Metadata) -> Entry {
        let t = m.file_type();
        if t.is_symlink() {
            Entry::Other
        } else if t.is_file() {
            Entry::File { size: m.len() }
        } else if t.is_dir() {
            Entry::Dir
        } else {
            Entry::Other
        }
    }

    impl FsStorage {
        pub fn open(root: &Path) -> io::Result<Self> {
            let root = fs::canonicalize(root)?;
            let own = root.join(CLIENT_DIR);
            let partial = own.join(PARTIAL);
            fs::create_dir_all(&partial)?;
            for p in [&own, &partial] {
                if fs::symlink_metadata(p)?.file_type().is_symlink() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "the client's folder is a symbolic link",
                    ));
                }
            }
            let probe = own.join("case-probe");
            File::create(&probe)?;
            let case_insensitive = own.join("CASE-PROBE").exists();
            let _ = fs::remove_file(&probe);
            Ok(Self {
                capabilities: base_capabilities(&root, case_insensitive, Some(255)),
                root,
                own,
                partial,
                journal: Mutex::new(None),
            })
        }

        /// `path` under the folder, refusing a path through a link.
        fn resolve(&self, path: &str) -> io::Result<PathBuf> {
            let mut at = self.root.clone();
            for name in checked(path)?.split('/') {
                at.push(name);
                if fs::symlink_metadata(&at).is_ok_and(|m| m.file_type().is_symlink()) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "goes through a symbolic link",
                    ));
                }
            }
            Ok(at)
        }

        fn with_journal<T>(&self, f: impl FnOnce(&mut File) -> io::Result<T>) -> io::Result<T> {
            let mut journal = self.journal.lock().unwrap_or_else(|e| e.into_inner());
            if journal.is_none() {
                *journal = Some(
                    OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(self.own.join(JOURNAL))?,
                );
            }
            f(journal.as_mut().expect("opened above"))
        }
    }

    impl Storage for FsStorage {
        fn capabilities(&self) -> Capabilities {
            self.capabilities.clone()
        }

        fn free_bytes(&self) -> io::Result<Option<u64>> {
            Ok(None)
        }

        fn list(&self, each: &mut dyn FnMut(Listed)) -> io::Result<()> {
            fn walk(dir: &Path, prefix: &str, each: &mut dyn FnMut(Listed)) -> io::Result<()> {
                for e in fs::read_dir(dir)? {
                    let e = e?;
                    let Ok(name) = e.file_name().into_string() else {
                        continue;
                    };
                    if prefix.is_empty() && name.eq_ignore_ascii_case(CLIENT_DIR) {
                        continue;
                    }
                    let path = if prefix.is_empty() {
                        name
                    } else {
                        format!("{prefix}/{name}")
                    };
                    match entry(&fs::symlink_metadata(e.path())?) {
                        Entry::File { size } => each(Listed { path, size }),
                        Entry::Dir => walk(&e.path(), &path, each)?,
                        Entry::Other => {}
                    }
                }
                Ok(())
            }
            walk(&self.root, "", each)
        }

        fn stat(&self, path: &str) -> io::Result<Option<Entry>> {
            match fs::symlink_metadata(self.resolve(path)?) {
                Ok(m) => Ok(Some(entry(&m))),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e),
            }
        }

        fn remove_file(&self, path: &str) -> io::Result<()> {
            fs::remove_file(self.resolve(path)?)
        }

        fn create_dirs(&self, path: &str) -> io::Result<()> {
            fs::create_dir_all(self.resolve(path)?)
        }

        fn remove_dir(&self, path: &str) -> io::Result<()> {
            match fs::remove_dir(self.resolve(path)?) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                // Not empty: no error.
                _ => Ok(()),
            }
        }

        fn rename(&self, from: &str, to: &str) -> io::Result<()> {
            fs::rename(self.resolve(from)?, self.resolve(to)?)
        }

        fn open(&self, path: &str) -> io::Result<Box<dyn Source>> {
            Ok(Box::new(File::open(self.resolve(path)?)?))
        }

        fn partial_len(&self, name: &str) -> io::Result<Option<u64>> {
            match fs::symlink_metadata(self.partial.join(name)) {
                Ok(m) if m.is_file() => Ok(Some(m.len())),
                Ok(_) => Ok(None),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e),
            }
        }

        fn write_partial(&self, name: &str, resume: bool) -> io::Result<Box<dyn Partial>> {
            let mut o = OpenOptions::new();
            o.create(true);
            if resume {
                o.append(true);
            } else {
                o.write(true).truncate(true);
            }
            Ok(Box::new(PartialFile(o.open(self.partial.join(name))?)))
        }

        fn read_partial(&self, name: &str) -> io::Result<Box<dyn Read + Send>> {
            Ok(Box::new(File::open(self.partial.join(name))?))
        }

        fn remove_partial(&self, name: &str) -> io::Result<()> {
            match fs::remove_file(self.partial.join(name)) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            }
        }

        fn publish(&self, name: &str, path: &str, _replace: bool) -> io::Result<()> {
            let to = self.resolve(path)?;
            if let (Some(dirs), _) = split(path) {
                fs::create_dir_all(self.resolve(dirs)?)?;
            }
            fs::rename(self.partial.join(name), to)
        }

        fn clear_partials(&self) -> io::Result<()> {
            for e in fs::read_dir(&self.partial)? {
                let _ = fs::remove_file(e?.path());
            }
            Ok(())
        }

        fn read_marker(&self) -> io::Result<Option<Vec<u8>>> {
            match fs::read(self.own.join(MARKER)) {
                Ok(b) => Ok(Some(b)),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e),
            }
        }

        fn write_marker(&self, bytes: &[u8]) -> io::Result<()> {
            let tmp = self.own.join("device.json.new");
            let mut f = File::create(&tmp)?;
            f.write_all(bytes)?;
            f.sync_data()?;
            fs::rename(tmp, self.own.join(MARKER))
        }

        fn read_journal(&self) -> io::Result<Vec<u8>> {
            let mut out = Vec::new();
            match File::open(self.own.join(JOURNAL)) {
                Ok(mut f) => {
                    f.read_to_end(&mut out)?;
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            Ok(out)
        }

        fn append_journal(&self, bytes: &[u8]) -> io::Result<()> {
            self.with_journal(|f| f.write_all(bytes))
        }

        fn replace_journal(&self, bytes: &[u8]) -> io::Result<()> {
            self.with_journal(|f| {
                f.set_len(0)?;
                f.write_all(bytes)
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{Entry, Storage};
    use std::io::{Read, Write};

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "coversync-replica-fs-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn files_go_where_the_plan_says_and_nowhere_else() {
        let dir = scratch("basic");
        let s = FsStorage::open(&dir).unwrap();
        s.create_dirs("A/B").unwrap();
        let mut p = s.write_partial("x", false).unwrap();
        p.write_all(b"hello").unwrap();
        p.sync().unwrap();
        drop(p);
        assert_eq!(s.partial_len("x").unwrap(), Some(5));
        s.publish("x", "A/B/one.flac", false).unwrap();
        assert_eq!(
            s.stat("A/B/one.flac").unwrap(),
            Some(Entry::File { size: 5 })
        );
        assert_eq!(s.stat("A/B").unwrap(), Some(Entry::Dir));
        assert_eq!(s.stat("A/C/none").unwrap(), None);
        // Its folder gone by hand mid-sync: made again.
        let mut p = s.write_partial("y", false).unwrap();
        p.write_all(b"hi").unwrap();
        drop(p);
        s.publish("y", "New/Album/two.flac", true).unwrap();
        assert_eq!(
            std::fs::read(dir.join("New/Album/two.flac")).unwrap(),
            b"hi"
        );
        s.rename("A/B/one.flac", "A/one.flac").unwrap();
        let mut text = String::new();
        s.open("A/one.flac")
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "hello");
        s.remove_dir("A").unwrap(); // not empty: no error, still there
        assert_eq!(s.stat("A").unwrap(), Some(Entry::Dir));
        s.remove_dir("A/B").unwrap();
        assert_eq!(s.stat("A/B").unwrap(), None);

        let mut listed = Vec::new();
        s.list(&mut |l| listed.push((l.path, l.size))).unwrap();
        listed.sort();
        assert_eq!(
            listed,
            vec![
                ("A/one.flac".to_string(), 5),
                ("New/Album/two.flac".to_string(), 2)
            ]
        );
        for bad in [
            "../x",
            "/etc/passwd",
            ".coversync-replica/device.json",
            "a//b",
        ] {
            assert!(s.stat(bad).is_err(), "{bad}");
            assert!(s.remove_file(bad).is_err(), "{bad}");
        }

        s.write_marker(br#"{"device":1,"epoch":"e"}"#).unwrap();
        assert_eq!(
            s.read_marker().unwrap().unwrap(),
            br#"{"device":1,"epoch":"e"}"#
        );
        s.append_journal(b"one\n").unwrap();
        s.append_journal(b"two\n").unwrap();
        assert_eq!(s.read_journal().unwrap(), b"one\ntwo\n");
        s.replace_journal(b"two\n").unwrap();
        s.append_journal(b"three\n").unwrap();
        assert_eq!(s.read_journal().unwrap(), b"two\nthree\n");
        s.clear_partials().unwrap();

        // One sync in a folder at a time.
        let held = s.lock().unwrap();
        let other = FsStorage::open(&dir).unwrap();
        if held.is_some() {
            assert!(other.lock().is_err());
            drop(held);
            assert!(other.lock().unwrap().is_some());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn links_in_the_folder_are_never_followed() {
        let dir = scratch("links");
        let outside = scratch("links-outside");
        std::fs::write(outside.join("victim"), b"keep me").unwrap();
        let s = FsStorage::open(&dir).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("Linked")).unwrap();
        std::os::unix::fs::symlink(outside.join("victim"), dir.join("file-link")).unwrap();

        // Through a linked folder: refused, whatever the operation.
        assert!(s.remove_file("Linked/victim").is_err());
        // Seen through a link, nothing is there to act on.
        assert!(!matches!(s.stat("Linked/victim"), Ok(Some(_))));
        assert!(s.create_dirs("Linked/sub").is_err());
        let mut p = s.write_partial("z", false).unwrap();
        p.write_all(b"new").unwrap();
        drop(p);
        assert!(s.publish("z", "Linked/victim", true).is_err());
        assert!(s.open("file-link").is_err());
        // A link itself is something else, not a file.
        assert_eq!(s.stat("file-link").unwrap(), Some(Entry::Other));
        assert_eq!(std::fs::read(outside.join("victim")).unwrap(), b"keep me");

        // Not listed either.
        let mut listed = Vec::new();
        s.list(&mut |l| listed.push(l.path)).unwrap();
        assert!(listed.is_empty(), "{listed:?}");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }
}
