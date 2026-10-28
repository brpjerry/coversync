//! Cover discovery, thumbnails and dominant colours.

use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};

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

fn load(source: &CoverSource) -> Result<DynamicImage> {
    let bytes = match source {
        CoverSource::File(p) => fs::read(p)?,
        CoverSource::Embedded(p) => tags::embedded_cover(p)?.ok_or(Error::NotFound)?,
    };
    Ok(image::load_from_memory(&bytes)?)
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
        Ok(Self { dir })
    }

    fn path(&self, album_id: &str, size: u32) -> PathBuf {
        self.dir.join(format!("{album_id}-{size}.jpg"))
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
    pub fn analyze(&self, job: &CoverJob) -> (Option<CoverSource>, Option<String>) {
        for size in SIZES {
            let _ = fs::remove_file(self.path(&job.album_id, *size));
        }
        let Some(source) = Self::find_source(job) else {
            return (None, None);
        };
        match load(&source) {
            Ok(img) => {
                let thumb = fit(&img, SIZES[1]);
                if let Ok(bytes) = encode_jpeg(&thumb) {
                    let _ = fs::write(self.path(&job.album_id, SIZES[1]), bytes);
                }
                (Some(source), Some(dominant_color(&thumb)))
            }
            Err(e) => {
                tracing::warn!(album = job.album_id, "cover decode: {e}");
                (None, None)
            }
        }
    }

    /// A cached JPEG of at least `size` pixels (or the largest bucket).
    pub fn thumbnail(&self, lib: &Library, album_id: &str, size: u32) -> Result<PathBuf> {
        let bucket = SIZES
            .iter()
            .copied()
            .find(|s| *s >= size)
            .unwrap_or(*SIZES.last().unwrap());
        let path = self.path(album_id, bucket);
        if path.exists() {
            return Ok(path);
        }
        let source = lib.cover_source(album_id)?.ok_or(Error::NotFound)?;
        let img = fit(&load(&source)?, bucket);
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, encode_jpeg(&img)?)?;
        fs::rename(&tmp, &path)?;
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage};

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
