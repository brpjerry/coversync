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

fn file_cancellable(path: &Path, stopped: &AtomicBool) -> io::Result<FileHash> {
    let mut input = File::open(path)?;
    let before = input.metadata()?;
    if !before.is_file() {
        return Err(io::Error::other("not a regular file"));
    }
    let (hash, size) = stream_cancellable(&mut input, &mut io::sink(), stopped)?;
    if size != before.len()
        || !unchanged(&before, &input.metadata()?)
        || !unchanged(&before, &path.metadata()?)
    {
        return Err(io::Error::other("file changed while hashing"));
    }
    Ok(FileHash {
        hash,
        size,
        mtime_ns: mtime_ns(&before)?,
    })
}

/// Hash as bytes are copied, using one large buffer per worker.
pub fn stream(input: &mut impl Read, output: &mut impl Write) -> io::Result<(String, u64)> {
    stream_cancellable(input, output, &AtomicBool::new(false))
}

/// Keep filesystem clones cheap: a reflink copies no bytes, so hashing its
/// result is still the only read. Otherwise hash during the userspace copy.
pub fn copy(src: &Path, dest: &Path) -> io::Result<FileHash> {
    let mut input = File::open(src)?;
    let mut output = File::create(dest)?;
    #[cfg(target_os = "linux")]
    let cloned = {
        use std::os::fd::AsRawFd;
        // Both descriptors stay open throughout the ioctl.
        if unsafe { libc::ioctl(output.as_raw_fd(), libc::FICLONE, input.as_raw_fd()) } == 0 {
            true
        } else {
            let error = io::Error::last_os_error();
            if !matches!(
                error.raw_os_error(),
                Some(libc::EXDEV | libc::EOPNOTSUPP | libc::EINVAL | libc::ENOTTY)
            ) {
                return Err(error);
            }
            false
        }
    };
    #[cfg(not(target_os = "linux"))]
    let cloned = false;
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
    std::fs::set_permissions(dest, input.metadata()?.permissions())?;
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
            if stopped.load(Ordering::Relaxed) {
                break;
            }
            let hashes: Vec<_> = pool.install(|| {
                batch
                    .par_iter()
                    .filter_map(|job| {
                        if stopped.load(Ordering::Relaxed) {
                            return None;
                        }
                        let result = file_cancellable(&job.path, stopped);
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
                                p.errors += 1;
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
            lib.store_hashes(&hashes)?;
        }
        Ok(())
    })();
    progress.write().unwrap_or_else(|e| e.into_inner()).running = false;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

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
