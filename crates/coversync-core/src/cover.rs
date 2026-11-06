//! Cover discovery, thumbnails and dominant colours.

use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView};

use crate::db::{CoverJob, CoverSource};
use crate::{Error, Library, Result, tags};

/// Thumbnail edge lengths the daemon serves; requests round up to one of these.
pub const SIZES: &[u32] = &[256, 512, 1024];
const PREFERRED: &[&str] = &["cover", "folder", "front", "album", "albumart"];
const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp"];

pub struct CoverCache {
    dir: PathBuf,
}

fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| IMAGE_EXTENSIONS.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

/// A cover image file in `dir`: a conventionally named one if present,
/// otherwise the largest image directly inside it.
fn folder_image(dir: &Path) -> Option<PathBuf> {
    let mut images: Vec<(PathBuf, u64)> = fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()) && is_image(&e.path()))
        .map(|e| (e.path(), e.metadata().map(|m| m.len()).unwrap_or(0)))
        .collect();
    for name in PREFERRED {
        if let Some((p, _)) = images.iter().find(|(p, _)| {
            p.file_stem()
                .is_some_and(|s| s.to_string_lossy().eq_ignore_ascii_case(name))
        }) {
            return Some(p.clone());
        }
    }
    images.sort_by_key(|(_, len)| std::cmp::Reverse(*len));
    images.into_iter().next().map(|(p, _)| p)
}

/// The art a source points at, as stored.
fn read(source: &CoverSource) -> Result<Vec<u8>> {
    match source {
        CoverSource::File(p) => Ok(fs::read(p)?),
        CoverSource::Embedded(p) => tags::embedded_cover(p)?.ok_or(Error::NotFound),
    }
}

/// A cover's version: a digest of the art itself, so the same art always has
/// the same version and different art a different one, however and whenever
/// it was found. Thumbnails are filed under it.
fn version_of(art: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(art)[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// What the cover pass found for an album.
pub struct Cover {
    pub source: CoverSource,
    pub color: String,
    pub version: String,
}

fn encode_jpeg(img: &DynamicImage) -> Result<Vec<u8>> {
    let mut out = Cursor::new(Vec::new());
    JpegEncoder::new_with_quality(&mut out, 84).encode_image(&img.to_rgb8())?;
    Ok(out.into_inner())
}

fn fit(img: &DynamicImage, size: u32) -> DynamicImage {
    let (w, h) = img.dimensions();
    if w <= size && h <= size {
        img.clone()
    } else {
        img.resize(size, size, FilterType::CatmullRom)
    }
}

/// The most common saturated hue bucket, averaged, as `#rrggbb`.
/// Falls back to the overall average for greyscale covers.
pub fn dominant_color(img: &DynamicImage) -> String {
    let small = img.resize_exact(32, 32, FilterType::Triangle).to_rgb8();
    let mut buckets = vec![(0u32, [0u64; 3]); 4096];
    let mut all = [0u64; 3];
    for px in small.pixels() {
        let [r, g, b] = px.0;
        for (i, c) in [r, g, b].into_iter().enumerate() {
            all[i] += c as u64;
        }
        let max = r.max(g).max(b) as i32;
        let min = r.min(g).min(b) as i32;
        // Skip washed-out and near-black pixels so a white border or black
        // background doesn't win.
        if max - min < 40 || max < 40 {
            continue;
        }
        let key = ((r as usize >> 4) << 8) | ((g as usize >> 4) << 4) | (b as usize >> 4);
        let bucket = &mut buckets[key];
        bucket.0 += 1;
        for (i, c) in [r, g, b].into_iter().enumerate() {
            bucket.1[i] += c as u64;
        }
    }
    let (count, sum) = buckets
        .into_iter()
        .max_by_key(|(n, _)| *n)
        .filter(|(n, _)| *n >= 6)
        .unwrap_or((small.pixels().len() as u32, all));
    let [r, g, b] = sum.map(|s| (s / count.max(1) as u64) as u8);
    format!("#{r:02x}{g:02x}{b:02x}")
}

impl CoverCache {
    pub fn new(dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(&dir)?;
        // Thumbnails used to lie here directly, as `<album>-<size>.jpg`.
        for entry in fs::read_dir(&dir)?.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_file()) {
                let _ = fs::remove_file(entry.path());
            }
        }
        Ok(Self { dir })
    }

    fn path(&self, album_id: &str, version: &str, size: u32) -> PathBuf {
        self.dir
            .join(album_id)
            .join(format!("{version}-{size}.jpg"))
    }

    fn write(&self, path: &Path, img: &DynamicImage) -> Result<()> {
        static TEMP: AtomicU64 = AtomicU64::new(0);
        fs::create_dir_all(path.parent().unwrap_or(&self.dir))?;
        let tmp = path.with_extension(format!("{}.tmp", TEMP.fetch_add(1, Ordering::Relaxed)));
        let bytes = encode_jpeg(img)?;
        let written = fs::write(&tmp, bytes).and_then(|()| fs::rename(&tmp, path));
        if written.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        Ok(written?)
    }

    /// Remove an album's thumbnails, except those of the `keep` version.
    fn clear(&self, album_id: &str, keep: Option<&str>) {
        let kept = keep.map(|v| format!("{v}-"));
        let Ok(files) = fs::read_dir(self.dir.join(album_id)) else {
            return;
        };
        for file in files.flatten() {
            let name = file.file_name();
            if !kept
                .as_deref()
                .is_some_and(|k| name.to_string_lossy().starts_with(k))
            {
                let _ = fs::remove_file(file.path());
            }
        }
    }

    fn find_source(job: &CoverJob) -> Option<CoverSource> {
        let track_dir = job.first_track.parent();
        folder_image(&job.album_dir)
            .or_else(|| track_dir.and_then(folder_image))
            .map(CoverSource::File)
            .or_else(|| {
                tags::embedded_cover(&job.first_track)
                    .ok()
                    .flatten()
                    .map(|_| CoverSource::Embedded(job.first_track.clone()))
            })
    }

    /// Locate the cover, pre-render the grid thumbnail and pick its colour.
    /// Thumbnails of any other art the album had are removed.
    pub fn analyze(&self, job: &CoverJob) -> Option<Cover> {
        let found = Self::find_source(job).and_then(|source| {
            let rendered = read(&source).and_then(|art| {
                let thumb = fit(&image::load_from_memory(&art)?, SIZES[1]);
                let version = version_of(&art);
                let path = self.path(&job.album_id, &version, SIZES[1]);
                // The art is found all the same: a thumbnail request renders it again.
                if !path.exists()
                    && let Err(e) = self.write(&path, &thumb)
                {
                    tracing::warn!(album = job.album_id, "cover thumbnail: {e}");
                }
                Ok((dominant_color(&thumb), version))
            });
            match rendered {
                Ok((color, version)) => Some(Cover {
                    source,
                    color,
                    version,
                }),
                Err(e) => {
                    tracing::warn!(album = job.album_id, "cover decode: {e}");
                    None
                }
            }
        });
        self.clear(&job.album_id, found.as_ref().map(|c| c.version.as_str()));
        found
    }

    /// A JPEG of the album's art at least `size` pixels across (or the largest
    /// bucket), and whether it is the rendering of the version `wanted`, which
    /// a browser can then keep for good.
    ///
    /// The recorded version's thumbnails are served while they last. Without
    /// them the art is read as it is now; if it has changed since the cover
    /// pass looked, its new version is recorded, so album lists name it.
    pub fn thumbnail(
        &self,
        lib: &Library,
        album_id: &str,
        size: u32,
        wanted: Option<&str>,
    ) -> Result<(PathBuf, bool)> {
        let bucket = SIZES
            .iter()
            .copied()
            .find(|s| *s >= size)
            .unwrap_or(*SIZES.last().unwrap());
        let (source, recorded) = lib.cover(album_id)?.ok_or(Error::NotFound)?;
        if let Some(version) = recorded.as_deref() {
            let path = self.path(album_id, version, bucket);
            if path.exists() {
                return Ok((path, wanted == Some(version)));
            }
        }
        let art = read(&source.ok_or(Error::NotFound)?)?;
        let version = version_of(&art);
        let img = fit(&image::load_from_memory(&art)?, bucket);
        let path = self.path(album_id, &version, bucket);
        if !path.exists() {
            self.write(&path, &img)?;
        }
        if recorded.as_deref() != Some(version.as_str()) {
            lib.set_cover_version(album_id, &version, &dominant_color(&img))?;
            self.clear(album_id, Some(&version));
        }
        let current = wanted == Some(version.as_str());
        Ok((path, current))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage};

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("coversync-cover-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("A")).unwrap();
        dir
    }

    fn paint(dir: &Path, rgb: [u8; 3]) {
        let img = DynamicImage::ImageRgb8(RgbImage::from_pixel(64, 64, Rgb(rgb)));
        img.save(dir.join("A/cover.png")).unwrap();
    }

    fn job(dir: &Path) -> CoverJob {
        CoverJob {
            album_id: "a".into(),
            album_dir: dir.join("A"),
            first_track: dir.join("A/1.flac"),
            signature: "5:1".into(),
        }
    }

    #[test]
    fn the_version_follows_the_art_whatever_the_signature() {
        let dir = temp_dir("version");
        fs::create_dir_all(dir.join("cache")).unwrap();
        fs::write(dir.join("cache/a-512.jpg"), b"old layout").unwrap();
        let cache = CoverCache::new(dir.join("cache")).unwrap();
        assert!(!dir.join("cache/a-512.jpg").exists());
        paint(&dir, [200, 40, 40]);
        let red = cache.analyze(&job(&dir)).unwrap();
        // Looked at again (say it left the index and came back): same art, same version.
        assert_eq!(cache.analyze(&job(&dir)).unwrap().version, red.version);
        assert!(cache.path("a", &red.version, 512).exists());
        paint(&dir, [20, 200, 40]);
        let green = cache.analyze(&job(&dir)).unwrap();
        assert_ne!(green.version, red.version);
        assert!(!cache.path("a", &red.version, 512).exists());
        assert!(cache.path("a", &green.version, 512).exists());
    }

    #[test]
    fn a_failed_write_leaves_no_temporary_file() {
        let dir = temp_dir("tmpfiles");
        let cache = CoverCache::new(dir.join("cache")).unwrap();
        // Renaming onto a directory fails after the temporary file is written.
        let target = cache.path("a", "v", 256);
        fs::create_dir_all(&target).unwrap();
        let img = DynamicImage::ImageRgb8(RgbImage::from_pixel(8, 8, Rgb([1, 2, 3])));
        assert!(cache.write(&target, &img).is_err());
        let left: Vec<_> = fs::read_dir(dir.join("cache/a"))
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(left.len(), 1, "only the directory in the way: {left:?}");
    }

    #[test]
    fn a_cache_that_cant_be_written_keeps_the_cover() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("readonly");
        let cache = CoverCache::new(dir.join("cache")).unwrap();
        paint(&dir, [200, 40, 40]);
        let lock = |mode| fs::set_permissions(dir.join("cache"), fs::Permissions::from_mode(mode));
        lock(0o555).unwrap();
        // Root writes regardless, and there's nothing to test.
        let writable = fs::write(dir.join("cache/probe"), b"").is_ok();
        let cover = cache.analyze(&job(&dir));
        lock(0o755).unwrap();
        if writable {
            return;
        }
        let cover = cover.expect("the art was found, though its thumbnail couldn't be written");
        assert!(!cache.path("a", &cover.version, 512).exists());
    }

    #[test]
    fn only_the_rendering_a_url_names_is_kept_for_good() {
        let dir = temp_dir("thumbs");
        let lib = Library::open(&dir.join("db.sqlite3")).unwrap();
        let cache = CoverCache::new(dir.join("cache")).unwrap();
        paint(&dir, [200, 40, 40]);
        let red = cache.analyze(&job(&dir)).unwrap();
        lib.set_cover("a", Some(&red), "5:1").unwrap();
        let (path, current) = cache.thumbnail(&lib, "a", 256, Some(&red.version)).unwrap();
        assert!(current && path.exists());
        assert!(!cache.thumbnail(&lib, "a", 256, None).unwrap().1);
        // The art is replaced on its own and the thumbnails cleared: the old URL
        // gets the new art, but not to keep, and the new version is recorded.
        paint(&dir, [20, 200, 40]);
        fs::remove_dir_all(dir.join("cache/a")).unwrap();
        let (path, current) = cache.thumbnail(&lib, "a", 256, Some(&red.version)).unwrap();
        assert!(!current);
        let green = lib.cover("a").unwrap().unwrap().1.unwrap();
        assert_ne!(green, red.version);
        assert_eq!(path, cache.path("a", &green, 256));
        assert!(cache.thumbnail(&lib, "a", 1024, Some(&green)).unwrap().1);
    }

    #[test]
    fn dominant_colour_ignores_white_border() {
        let mut img = RgbImage::from_pixel(64, 64, Rgb([255, 255, 255]));
        for x in 8..56 {
            for y in 8..56 {
                img.put_pixel(x, y, Rgb([200, 40, 40]));
            }
        }
        let c = dominant_color(&DynamicImage::ImageRgb8(img));
        assert!(c.starts_with("#c"), "{c}");
    }

    #[test]
    fn greyscale_falls_back_to_average() {
        let img = RgbImage::from_pixel(16, 16, Rgb([128, 128, 128]));
        assert_eq!(dominant_color(&DynamicImage::ImageRgb8(img)), "#808080");
    }
}
