//! Library scanning: find audio files, read changed ones, then refresh covers.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use rayon::prelude::*;
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::cover::CoverCache;
use crate::db::FileStat;
use crate::model::{Collection, ScanProgress};
use crate::{Library, Result, tags};

const BATCH: usize = 256;

pub type Progress = Arc<RwLock<ScanProgress>>;

fn update(progress: &Progress, f: impl FnOnce(&mut ScanProgress)) {
    f(&mut progress.write().unwrap_or_else(|e| e.into_inner()));
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Multi-disc releases keep each disc in a `CD1`/`Disc 2` folder; treat the
/// parent as the album directory so the discs group together.
pub fn album_dir(file: &Path) -> PathBuf {
    let parent = file.parent().unwrap_or(Path::new("/"));
    let name = parent
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    match disc_folder_number(&name) {
        Some(_) => parent.parent().unwrap_or(parent).to_path_buf(),
        None => parent.to_path_buf(),
    }
}

/// The disc number of a `CD1` / `Disc 2` / `disk_3` style folder name.
pub fn disc_folder_number(name: &str) -> Option<u32> {
    let name = name.to_lowercase();
    let rest = ["cd", "disc", "disk"]
        .iter()
        .find_map(|p| name.strip_prefix(p))?
        .trim_start_matches([' ', '-', '_', '.']);
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

/// Stable album id from its directory and album title.
pub fn album_id(dir: &Path, album: Option<&str>) -> String {
    let mut h = Sha256::new();
    h.update(dir.as_os_str().as_encoded_bytes());
    h.update([0]);
    h.update(album.unwrap_or_default().to_lowercase().as_bytes());
    h.finalize()
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn stat(path: PathBuf, root: &Path, collection: Collection) -> Option<FileStat> {
    let meta = path.metadata().ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    Some(FileStat {
        path,
        root: root.to_path_buf(),
        mtime,
        size: meta.len(),
        collection,
    })
}

fn walk(roots: &[(PathBuf, Collection)], progress: &Progress) -> Vec<FileStat> {
    let mut files = Vec::new();
    for (root, collection) in roots {
        for entry in WalkDir::new(root)
            .follow_links(true)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let name = entry.file_name().to_string_lossy();
            if !entry.file_type().is_file()
                || name.starts_with('.')
                || !tags::is_audio(entry.path())
            {
                continue;
            }
            if let Some(s) = stat(entry.into_path(), root, *collection) {
                files.push(s);
                if files.len() % 500 == 0 {
                    let n = files.len() as u64;
                    update(progress, |p| p.files_seen = n);
                }
            }
        }
    }
    files
}

/// Read tags for the given files and store them, in batches.
pub fn index_files(lib: &Library, files: Vec<FileStat>, progress: &Progress) -> Result<()> {
    for chunk in files.chunks(BATCH) {
        let read: Vec<_> = chunk
            .par_iter()
            .filter_map(|stat| match tags::read(&stat.path) {
                Ok(t) => {
                    let dir = album_dir(&stat.path);
                    let id = album_id(&dir, t.album.as_deref());
                    Some((stat.clone(), id, dir, t))
                }
                Err(e) => {
                    tracing::warn!(path = %stat.path.display(), "unreadable: {e}");
                    update(progress, |p| p.errors += 1);
                    None
                }
            })
            .collect();
        lib.upsert(&read)?;
        let n = chunk.len() as u64;
        update(progress, |p| p.files_read += n);
    }
    Ok(())
}

/// Full incremental scan of the Inbox and Library folders, then the cover
/// pass. The index ends up holding exactly the files under `roots`.
pub fn scan(
    lib: &Library,
    covers: &CoverCache,
    roots: &[(PathBuf, Collection)],
    progress: &Progress,
) -> Result<()> {
    update(progress, |p| {
        *p = ScanProgress {
            running: true,
            phase: "walking".into(),
            ..Default::default()
        }
    });
    let files = walk(roots, progress);
    let known = lib.file_stats()?;
    let on_disk: HashSet<&Path> = files.iter().map(|f| f.path.as_path()).collect();
    let removed: Vec<PathBuf> = known
        .keys()
        .filter(|p| !on_disk.contains(p.as_path()))
        .cloned()
        .collect();
    let total = files.len() as u64;
    let changed: Vec<FileStat> = files
        .into_iter()
        .filter(|f| known.get(&f.path) != Some(&(f.mtime, f.size, f.collection)))
        .collect();
    tracing::info!(
        total,
        changed = changed.len(),
        removed = removed.len(),
        "scan"
    );
    update(progress, |p| {
        p.files_seen = total;
        p.files_read = total - changed.len() as u64;
        p.phase = "reading".into();
    });
    index_files(lib, changed, progress)?;
    lib.remove_paths(&removed)?;
    update(progress, |p| p.phase = "covers".into());
    refresh_covers(lib, covers, progress)?;
    let albums = lib.albums(Collection::Inbox, &[], &[], None)?.len() as u64;
    update(progress, |p| {
        p.running = false;
        p.phase = "idle".into();
        p.albums = albums;
        p.finished_at = Some(now());
    });
    Ok(())
}

pub fn refresh_covers(lib: &Library, covers: &CoverCache, progress: &Progress) -> Result<()> {
    let jobs = lib.cover_jobs()?;
    let total = jobs.len() as u64;
    update(progress, |p| {
        p.covers_total = total;
        p.covers_done = 0;
    });
    // Decoding large covers is memory-hungry; use fewer threads than the
    // tag reader so peak memory stays modest on big libraries.
    let threads = std::thread::available_parallelism().map_or(4, |n| (n.get() / 2).clamp(2, 8));
    let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build();
    let run = || {
        jobs.par_iter().for_each(|job| {
            let (source, color) = covers.analyze(job);
            if let Err(e) = lib.set_cover(
                &job.album_id,
                source.as_ref(),
                color.as_deref(),
                &job.signature,
            ) {
                tracing::warn!(album = job.album_id, "cover: {e}");
            }
            update(progress, |p| p.covers_done += 1);
        });
    };
    match pool {
        Ok(pool) => pool.install(run),
        Err(_) => run(),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disc_folders_group_under_album() {
        assert_eq!(
            album_dir(Path::new("/m/A/Album/CD1/01.flac")),
            Path::new("/m/A/Album")
        );
        assert_eq!(
            album_dir(Path::new("/m/A/Album/Disc 2/01.flac")),
            Path::new("/m/A/Album")
        );
        assert_eq!(
            album_dir(Path::new("/m/A/Album/disc_3/01.flac")),
            Path::new("/m/A/Album")
        );
        assert_eq!(
            album_dir(Path::new("/m/A/Discovery/01.flac")),
            Path::new("/m/A/Discovery")
        );
        assert_eq!(
            album_dir(Path::new("/m/A/CDs/01.flac")),
            Path::new("/m/A/CDs")
        );
    }

    #[test]
    fn album_ids_are_stable_and_case_insensitive() {
        let d = Path::new("/m/A/Album");
        assert_eq!(album_id(d, Some("Album")), album_id(d, Some("ALBUM")));
        assert_ne!(album_id(d, Some("Album")), album_id(d, Some("Other")));
        assert_eq!(album_id(d, None).len(), 16);
    }
}
