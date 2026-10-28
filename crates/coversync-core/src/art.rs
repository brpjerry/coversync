//! Album art rules, and the cover image an import writes.
//!
//! Imported albums keep their art the way Library setup says: as a cover
//! file beside the tracks (named by the rule, whatever the source was
//! called), embedded in every audio file, or both.

use std::io::Cursor;

use image::codecs::jpeg::JpegEncoder;
use image::{ImageFormat, ImageReader};
use lofty::picture::{MimeType, Picture, PictureType};
use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// Stands for album art among required tag kinds in attention flags.
pub const ART: &str = "album_art";
/// Cover file names offered in Library setup.
pub const FILE_NAMES: &[&str] = &["cover", "folder", "front"];
/// The largest image accepted as album art.
pub const MAX_BYTES: usize = 32 << 20;
const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp"];

/// Where an imported album keeps its art.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Placement {
    /// An image file beside the tracks, e.g. cover.jpg.
    #[default]
    File,
    /// Inside each audio file.
    Embedded,
    Both,
}

impl Placement {
    pub fn file(self) -> bool {
        matches!(self, Self::File | Self::Both)
    }

    pub fn embedded(self) -> bool {
        matches!(self, Self::Embedded | Self::Both)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ArtRules {
    /// Every album needs cover art before it can be imported.
    pub required: bool,
    pub placement: Placement,
    /// The cover file's name without extension ("cover" → cover.jpg).
    pub file_name: String,
}

impl Default for ArtRules {
    fn default() -> Self {
        Self {
            required: false,
            placement: Placement::default(),
            file_name: FILE_NAMES[0].to_string(),
        }
    }
}

impl ArtRules {
    pub fn normalize(mut self) -> Result<Self> {
        let mut name = self.file_name.trim();
        // "cover.jpg" means "cover": the extension follows the image.
        if let Some((stem, ext)) = name.rsplit_once('.')
            && IMAGE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str())
        {
            name = stem.trim_end();
        }
        if name.is_empty() {
            name = FILE_NAMES[0];
        }
        if name.contains(['/', '\\', '\0']) || name.starts_with('.') || name.chars().count() > 64 {
            return Err(Error::Invalid(
                "The cover file name can't contain slashes or start with a dot.".into(),
            ));
        }
        self.file_name = name.to_string();
        Ok(self)
    }

    /// The cover file name for `art`, e.g. "cover.jpg".
    pub fn file_for(&self, art: &Art) -> String {
        format!("{}.{}", self.file_name, art.file_extension())
    }

    /// Whether `file_name` is this rule's cover file in any image format.
    pub fn is_cover_file(&self, file_name: &str) -> bool {
        file_name.rsplit_once('.').is_some_and(|(stem, ext)| {
            stem == self.file_name && IMAGE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str())
        })
    }
}

/// A cover image, as found beside or inside the files, or uploaded.
#[derive(Debug, Clone)]
pub struct Art {
    data: Vec<u8>,
    format: ImageFormat,
    pub width: u32,
    pub height: u32,
}

/// Formats kept as they are in a cover file; anything else becomes JPEG.
const FILE_FORMATS: &[ImageFormat] = &[ImageFormat::Jpeg, ImageFormat::Png, ImageFormat::WebP];
/// Formats kept as they are when embedded; every tag format and player knows these.
const EMBED_FORMATS: &[ImageFormat] = &[ImageFormat::Jpeg, ImageFormat::Png];

fn extension(format: ImageFormat) -> &'static str {
    match format {
        ImageFormat::Png => "png",
        ImageFormat::WebP => "webp",
        _ => "jpg",
    }
}

impl Art {
    /// Check that `data` is an image coversync can read.
    pub fn new(data: Vec<u8>) -> Result<Self> {
        let unreadable = || Error::Invalid("That file isn't a JPEG, PNG or WebP image.".into());
        if data.len() > MAX_BYTES {
            return Err(Error::Invalid(format!(
                "Album art can be at most {} MB.",
                MAX_BYTES >> 20
            )));
        }
        let reader = ImageReader::new(Cursor::new(&data))
            .with_guessed_format()
            .map_err(|_| unreadable())?;
        let format = reader.format().ok_or_else(unreadable)?;
        let (width, height) = reader.into_dimensions().map_err(|_| unreadable())?;
        Ok(Self {
            data,
            format,
            width,
            height,
        })
    }

    fn encoded(&self, keep: &[ImageFormat]) -> Result<(Vec<u8>, ImageFormat)> {
        if keep.contains(&self.format) {
            return Ok((self.data.clone(), self.format));
        }
        let img = image::load_from_memory(&self.data)?;
        let mut out = Cursor::new(Vec::new());
        JpegEncoder::new_with_quality(&mut out, 92).encode_image(&img.to_rgb8())?;
        Ok((out.into_inner(), ImageFormat::Jpeg))
    }

    /// The extension a cover file of this image gets.
    pub fn file_extension(&self) -> &'static str {
        if FILE_FORMATS.contains(&self.format) {
            extension(self.format)
        } else {
            "jpg"
        }
    }

    /// The bytes of a cover file.
    pub fn file_bytes(&self) -> Result<Vec<u8>> {
        Ok(self.encoded(FILE_FORMATS)?.0)
    }

    /// The image as an embedded front cover.
    pub fn picture(&self) -> Result<Picture> {
        let (data, format) = self.encoded(EMBED_FORMATS)?;
        let mime = if format == ImageFormat::Png {
            MimeType::Png
        } else {
            MimeType::Jpeg
        };
        Ok(Picture::unchecked(data)
            .pic_type(PictureType::CoverFront)
            .mime_type(mime)
            .build())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, Rgb, RgbImage};

    fn image(format: ImageFormat) -> Vec<u8> {
        let img = DynamicImage::ImageRgb8(RgbImage::from_pixel(8, 6, Rgb([200, 40, 40])));
        let mut out = Cursor::new(Vec::new());
        img.write_to(&mut out, format).unwrap();
        out.into_inner()
    }

    #[test]
    fn file_names_are_normalized() {
        let rules = |name: &str| {
            ArtRules {
                file_name: name.into(),
                ..Default::default()
            }
            .normalize()
        };
        assert_eq!(rules(" folder ").unwrap().file_name, "folder");
        assert_eq!(rules("Cover.JPG").unwrap().file_name, "Cover");
        assert_eq!(rules("").unwrap().file_name, "cover");
        assert_eq!(rules("front.art").unwrap().file_name, "front.art");
        assert!(rules("art/cover").is_err());
        assert!(rules(".cover").is_err());
    }

    #[test]
    fn cover_files_match_any_image_extension() {
        let rules = ArtRules::default();
        assert!(rules.is_cover_file("cover.jpg"));
        assert!(rules.is_cover_file("cover.PNG"));
        assert!(!rules.is_cover_file("Cover.jpg"));
        assert!(!rules.is_cover_file("cover.txt"));
        assert!(!rules.is_cover_file("folder.jpg"));
    }

    #[test]
    fn images_keep_or_change_format() {
        let png = Art::new(image(ImageFormat::Png)).unwrap();
        assert_eq!((png.width, png.height), (8, 6));
        assert_eq!(png.file_extension(), "png");
        assert_eq!(png.picture().unwrap().mime_type(), Some(&MimeType::Png));

        // WebP files stay WebP, but embedded art is JPEG.
        let webp = Art::new(image(ImageFormat::WebP)).unwrap();
        assert_eq!(webp.file_extension(), "webp");
        let pic = webp.picture().unwrap();
        assert_eq!(pic.mime_type(), Some(&MimeType::Jpeg));
        assert_eq!(pic.pic_type(), PictureType::CoverFront);
        assert!(image::load_from_memory_with_format(pic.data(), ImageFormat::Jpeg).is_ok());

        assert!(Art::new(b"not an image".to_vec()).is_err());
    }
}
