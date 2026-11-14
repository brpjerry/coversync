//! Whole-file BLAKE3 digests. Reading bytes never holds the database lock.

use std::fs::{File, Metadata};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::UNIX_EPOCH;

use rayon::prelude::*;
use serde::Serialize;

use crate::{Library, Result};

const BUFFER: usize = 1024 * 1024;
const BATCH: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileHash {
    /// Algorithm is explicit so stored hashes cannot be mistaken for SHA-256.
    pub hash: String,
    pub size: u64,
    pub mtime_ns: i64,
}

#[derive(Debug, Clone)]
pub struct HashJob {
    pub library_id: String,
    pub root: PathBuf,
    pub id: i64,
    pub path: PathBuf,
    pub generation: i64,
    pub size: u64,
    pub mtime_ns: i64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct HashProgress {
    pub running: bool,
    pub files_total: u64,
    pub files_done: u64,
    pub bytes_hashed: u64,
    pub errors: u64,
    /// Files or index rows changed while this pass was reading them.
    pub skipped: u64,
}

pub type Progress = Arc<RwLock<HashProgress>>;

pub fn mtime_ns(meta: &Metadata) -> io::Result<i64> {
    let time = meta
        .modified()?
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?;
    i64::try_from(time.as_nanos()).map_err(io::Error::other)
}

fn unchanged(a: &Metadata, b: &Metadata) -> bool {
    if a.len() != b.len() || a.modified().ok() != b.modified().ok() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if (a.dev(), a.ino(), a.ctime(), a.ctime_nsec())
            != (b.dev(), b.ino(), b.ctime(), b.ctime_nsec())
        {
            return false;
        }
    }
    true
}

/// Hash a stable open file; reject replacement or modification during the read.
pub fn file(path: &Path) -> io::Result<FileHash> {
    file_cancellable(path, &AtomicBool::new(false))
}

#[derive(Debug)]
struct FileChanged;

impl std::fmt::Display for FileChanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("file changed while hashing")
    }
}

impl std::error::Error for FileChanged {}

fn file_cancellable(path: &Path, stopped: &AtomicBool) -> io::Result<FileHash> {
    let mut input = File::open(path)?;
    let before = input.metadata()?;
    hash_open_file(path, &mut input, &before, stopped)
}

fn hash_open_file(
    path: &Path,
    input: &mut File,
    before: &Metadata,
    stopped: &AtomicBool,
) -> io::Result<FileHash> {
    if !before.is_file() {
        return Err(io::Error::other("not a regular file"));
    }
    let (hash, size) = stream_cancellable(input, &mut io::sink(), stopped)?;
    if size != before.len()
        || !unchanged(before, &input.metadata()?)
        || !unchanged(before, &path.metadata()?)
    {
        return Err(io::Error::other(FileChanged));
    }
    Ok(FileHash {
        hash,
        size,
        mtime_ns: mtime_ns(before)?,
    })
}

/// Hash as bytes are copied, using one large buffer per worker.
pub fn stream(input: &mut impl Read, output: &mut impl Write) -> io::Result<(String, u64)> {
    stream_cancellable(input, output, &AtomicBool::new(false))
}

/// Keep filesystem clones cheap: a reflink copies no bytes, so hashing its
/// result is still the only read. Otherwise hash during the userspace copy.
pub fn copy(src: &Path, dest: &Path) -> io::Result<FileHash> {
    copy_with_clone(src, dest, try_clone)
}

/// A rejected clone is only an optimization failure for these errors.
fn clone_unavailable(error: &io::Error) -> bool {
    #[cfg(target_os = "linux")]
    return matches!(
        error.raw_os_error(),
        Some(
            libc::EXDEV
                | libc::EOPNOTSUPP
                | libc::EINVAL
                | libc::ENOTTY
                | libc::EPERM
                | libc::ENOSYS
        )
    );
    #[cfg(not(target_os = "linux"))]
    {
        error.kind() == io::ErrorKind::Unsupported
    }
}

fn try_clone(input: &File, output: &File) -> io::Result<bool> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        // Both descriptors stay open throughout the ioctl.
        if unsafe { libc::ioctl(output.as_raw_fd(), libc::FICLONE, input.as_raw_fd()) } == 0 {
            Ok(true)
        } else {
            Err(io::Error::last_os_error())
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (input, output);
        Ok(false)
    }
}

fn copy_with_clone(
    src: &Path,
    dest: &Path,
    clone: impl FnOnce(&File, &File) -> io::Result<bool>,
) -> io::Result<FileHash> {
    let mut input = File::open(src)?;
    let permissions = input.metadata()?.permissions();
    let mut options = File::options();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut output = options.open(dest)?;
    // Protect restrictive sources before any content reaches the temporary
    // file, and restore read access if the process umask removed it.
    output.set_permissions(permissions.clone())?;
    let cloned = match clone(&input, &output) {
        Err(error) if clone_unavailable(&error) => false,
        result => result?,
    };
    let hash = if cloned {
        file(dest)?
    } else {
        let (hash, size) = stream(&mut input, &mut output)?;
        FileHash {
            hash,
            size,
            mtime_ns: mtime_ns(&output.metadata()?)?,
        }
    };
    // Writes may clear special mode bits; preserve the completed-copy behavior.
    output.set_permissions(permissions)?;
    Ok(hash)
}

fn stream_cancellable(
    input: &mut impl Read,
    output: &mut impl Write,
    stopped: &AtomicBool,
) -> io::Result<(String, u64)> {
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0; BUFFER];
    let mut size = 0;
    loop {
        if stopped.load(Ordering::Relaxed) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "hashing stopped",
            ));
        }
        let n = match input.read(&mut buffer) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            other => other?,
        };
        if n == 0 {
            break;
        }
        output.write_all(&buffer[..n])?;
        hasher.update(&buffer[..n]);
        size += n as u64;
    }
    Ok((format!("blake3:{}", hasher.finalize().to_hex()), size))
}

/// Only dedicated hash workers change priority; never the shared Rayon pool.
fn low_priority() {
    #[cfg(target_os = "linux")]
    unsafe {
        // Linux applies these calls to the calling thread (who = 0).
        if libc::setpriority(libc::PRIO_PROCESS, 0, 10) != 0 {
            tracing::warn!("hash worker priority: {}", io::Error::last_os_error());
        }
        // IOPRIO_CLASS_BE, lowest priority within that class.
        if libc::syscall(libc::SYS_ioprio_set, 1, 0, (2 << 13) | 7) != 0 {
            tracing::warn!("hash worker I/O priority: {}", io::Error::last_os_error());
        }
    }
}

/// A pass over pending Library files. Commits use row generations and Library
/// identity as a lease, so scans, edits and Library switches can run alongside it.
pub fn run(lib: &Library, progress: &Progress) -> Result<()> {
    run_cancellable(lib, progress, &AtomicBool::new(false))
}

/// Stop between buffer reads during daemon shutdown; already committed batches stay valid.
pub fn run_cancellable(lib: &Library, progress: &Progress, stopped: &AtomicBool) -> Result<()> {
    let jobs = lib.hash_jobs()?;
    run_jobs(lib, progress, stopped, &jobs, |job| {
        file_cancellable(&job.path, stopped)
    })
}

fn run_jobs(
    lib: &Library,
    progress: &Progress,
    stopped: &AtomicBool,
    jobs: &[HashJob],
    hash_file: impl Fn(&HashJob) -> io::Result<FileHash> + Sync,
) -> Result<()> {
    *progress.write().unwrap_or_else(|e| e.into_inner()) = HashProgress {
        running: true,
        files_total: jobs.len() as u64,
        ..Default::default()
    };
    let result = (|| {
        if jobs.is_empty() || stopped.load(Ordering::Relaxed) {
            return Ok(());
        }
        let pool = rayon::ThreadPoolBuilder::new()
            .start_handler(|_| low_priority())
            .build()
            .map_err(io::Error::other)?;
        for batch in jobs.chunks(BATCH) {
            if stopped.load(Ordering::Relaxed) || !lib.hash_library_matches(&batch[0])? {
                break;
            }
            let hashes: Vec<_> = pool.install(|| {
                batch
                    .par_iter()
                    .filter_map(|job| {
                        if stopped.load(Ordering::Relaxed) {
                            return None;
                        }
                        let result = hash_file(job);
                        if stopped.load(Ordering::Relaxed) {
                            return None;
                        }
                        let mut p = progress.write().unwrap_or_else(|e| e.into_inner());
                        p.files_done += 1;
                        match result {
                            Ok(hash) if hash.size == job.size && hash.mtime_ns == job.mtime_ns => {
                                p.bytes_hashed += hash.size;
                                Some((job, hash))
                            }
                            Ok(_) => {
                                p.skipped += 1;
                                tracing::debug!(path = %job.path.display(), "hash: file changed since indexing");
                                None
                            }
                            Err(e) if e.get_ref().is_some_and(|e| e.is::<FileChanged>()) => {
                                p.skipped += 1;
                                tracing::debug!(path = %job.path.display(), "hash: {e}");
                                None
                            }
                            Err(e) => {
                                p.errors += 1;
                                tracing::warn!(path = %job.path.display(), "hash: {e}");
                                None
                            }
                        }
                    })
                    .collect()
            });
            let stored = lib.store_hashes(&hashes)?;
            progress.write().unwrap_or_else(|e| e.into_inner()).skipped +=
                (hashes.len() - stored) as u64;
        }
        Ok(())
    })();
    progress.write().unwrap_or_else(|e| e.into_inner()).running = false;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let base = std::env::var_os("COVERSYNC_TEST_TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let path = base.join(format!(
            "coversync-hashes-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[cfg(unix)]
    fn check_copy_permissions(force_stream: bool) {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("copy");
        let src = dir.join("source");
        let dest = dir.join("destination");
        let bytes = vec![37; BUFFER + 17];
        std::fs::write(&src, &bytes).unwrap();
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o600)).unwrap();
        let got = copy_with_clone(&src, &dest, |input, output| {
            // Check the mode before either copying path writes its first byte.
            assert_eq!(output.metadata()?.permissions().mode() & 0o777, 0o600);
            assert_eq!(output.metadata()?.len(), 0);
            let result = if force_stream {
                Ok(false)
            } else {
                try_clone(input, output)
            };
            eprintln!("filesystem clone result: {result:?}");
            result
        })
        .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), bytes);
        assert_eq!(got, file(&dest).unwrap());
        assert_eq!(
            got.hash,
            format!("blake3:{}", blake3::hash(&bytes).to_hex())
        );
        assert_eq!(
            std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let public = dir.join("public-entry-point");
        assert_eq!(copy(&src, &public).unwrap(), file(&public).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn copies_preserve_permissions_before_writing_and_hash_final_bytes() {
        check_copy_permissions(true);
        check_copy_permissions(false);
    }

    #[cfg(unix)]
    #[test]
    fn copying_with_a_restrictive_umask_keeps_the_clone_readable() {
        // umask is process-wide; isolate it from the parallel test runner.
        const CHILD: &str = "COVERSYNC_TEST_RESTRICTIVE_UMASK";
        if std::env::var_os(CHILD).is_none() {
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "hashes::tests::copying_with_a_restrictive_umask_keeps_the_clone_readable",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("umask");
        let src = dir.join("source");
        std::fs::write(&src, b"private").unwrap();
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o600)).unwrap();
        // SAFETY: only this test runs in the isolated subprocess.
        unsafe {
            libc::umask(0o777);
        }
        let dest = dir.join("copy");
        assert_eq!(copy(&src, &dest).unwrap(), file(&dest).unwrap());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unavailable_or_blocked_clones_fall_back_but_io_errors_propagate() {
        let dir = scratch("clone-errors");
        let src = dir.join("source");
        std::fs::write(&src, b"a streaming copy still works").unwrap();
        for errno in [
            libc::EXDEV,
            libc::EOPNOTSUPP,
            libc::EINVAL,
            libc::ENOTTY,
            libc::EPERM,
            libc::ENOSYS,
        ] {
            let dest = dir.join(errno.to_string());
            let hash =
                copy_with_clone(&src, &dest, |_, _| Err(io::Error::from_raw_os_error(errno)))
                    .unwrap();
            assert_eq!(std::fs::read(&dest).unwrap(), std::fs::read(&src).unwrap());
            assert_eq!(hash, file(&dest).unwrap());
        }
        for errno in [libc::EIO, libc::ENOSPC, libc::EBADF] {
            let dest = dir.join(errno.to_string());
            assert_eq!(
                copy_with_clone(&src, &dest, |_, _| Err(io::Error::from_raw_os_error(errno)))
                    .unwrap_err()
                    .raw_os_error(),
                Some(errno)
            );
            assert_eq!(dest.metadata().unwrap().len(), 0);
        }
    }

    #[test]
    fn open_files_reject_replacement_and_modification() {
        for replace in [false, true] {
            let dir = scratch("changed");
            let path = dir.join("source");
            std::fs::write(&path, b"before").unwrap();
            let mut input = File::open(&path).unwrap();
            let before = input.metadata().unwrap();
            if replace {
                std::fs::rename(&path, dir.join("old")).unwrap();
            }
            std::fs::write(&path, b"changed contents").unwrap();
            let error =
                hash_open_file(&path, &mut input, &before, &AtomicBool::new(false)).unwrap_err();
            assert!(error.get_ref().unwrap().is::<FileChanged>());
        }
    }

    fn pending_files(count: usize) -> (Library, PathBuf) {
        use crate::{Collection, LibrarySetup, db::FileStat, tags::TrackTags};
        let dir = scratch("jobs");
        let root = dir.join("Library");
        std::fs::create_dir_all(&root).unwrap();
        let lib = Library::open_in(&dir.join("state")).unwrap();
        lib.set_library_setup(LibrarySetup {
            folder: Some(root.display().to_string()),
            ..Default::default()
        })
        .unwrap();
        let rows: Vec<_> = (0..count)
            .map(|n| {
                let path = root.join(format!("{n:04}.wav"));
                std::fs::write(&path, b"bytes").unwrap();
                let meta = path.metadata().unwrap();
                (
                    FileStat {
                        path,
                        root: root.clone(),
                        mtime: 0,
                        mtime_ns: mtime_ns(&meta).unwrap(),
                        size: meta.len(),
                        collection: Collection::Library,
                        added: None,
                        hash: None,
                    },
                    "album".into(),
                    root.clone(),
                    TrackTags::default(),
                )
            })
            .collect();
        lib.upsert(&rows).unwrap();
        (lib, root)
    }

    #[test]
    fn switching_libraries_stops_the_old_pass_before_the_next_batch() {
        let (lib, root) = pending_files(BATCH + 1);
        let jobs = lib.hash_jobs().unwrap();
        let progress = Progress::default();
        let switched = AtomicBool::new(false);
        let other = root.with_file_name("Other");
        std::fs::create_dir_all(&other).unwrap();
        run_jobs(&lib, &progress, &AtomicBool::new(false), &jobs, |job| {
            if !switched.swap(true, Ordering::SeqCst) {
                let mut setup = lib.library_setup().unwrap();
                setup.folder = Some(other.display().to_string());
                lib.set_library_setup(setup).unwrap();
            }
            file(&job.path)
        })
        .unwrap();
        let progress = progress.read().unwrap();
        assert_eq!(progress.files_done, BATCH as u64);
        assert_eq!(progress.skipped, BATCH as u64);
        assert_eq!(progress.errors, 0);
        assert!(!progress.running);
        assert!(lib.library_hashes().unwrap().is_empty());
    }

    #[test]
    fn changing_files_are_skipped_and_real_read_errors_are_counted() {
        let (lib, _) = pending_files(4);
        let jobs = lib.hash_jobs().unwrap();
        let progress = Progress::default();
        run_jobs(
            &lib,
            &progress,
            &AtomicBool::new(false),
            &jobs,
            |job| match job.path.file_stem().unwrap().to_str().unwrap() {
                "0000" => {
                    let mut hash = file(&job.path)?;
                    hash.size += 1;
                    Ok(hash)
                }
                "0001" => Err(io::Error::other(FileChanged)),
                "0002" => Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "test unreadable",
                )),
                _ => file(&job.path),
            },
        )
        .unwrap();
        let p = progress.read().unwrap();
        assert_eq!(
            (p.files_total, p.files_done, p.skipped, p.errors),
            (4, 4, 2, 1)
        );
        assert_eq!(lib.library_hashes().unwrap().len(), 1);
    }

    #[test]
    fn copying_hashes_every_byte_across_buffer_boundaries() {
        let bytes: Vec<_> = (0..BUFFER * 2 + 17).map(|n| (n % 251) as u8).collect();
        let mut copied = Vec::new();
        let (hash, size) = stream(&mut bytes.as_slice(), &mut copied).unwrap();
        assert_eq!(copied, bytes);
        assert_eq!(size, bytes.len() as u64);
        assert_eq!(hash, format!("blake3:{}", blake3::hash(&bytes).to_hex()));
        let (hash, size) = stream(&mut b"abc".as_slice(), &mut io::sink()).unwrap();
        assert_eq!(size, 3);
        assert_eq!(
            hash,
            "blake3:6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85"
        );
    }

    #[test]
    fn shutdown_interrupts_a_file_between_buffer_reads() {
        struct StopAfterRead<'a>(&'a AtomicBool);
        impl Read for StopAfterRead<'_> {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                buffer[..32].fill(7);
                self.0.store(true, Ordering::Relaxed);
                Ok(32)
            }
        }
        let stopped = AtomicBool::new(false);
        let mut output = Vec::new();
        let result = stream_cancellable(&mut StopAfterRead(&stopped), &mut output, &stopped);
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Interrupted);
        assert_eq!(output, vec![7; 32]);
    }
}
