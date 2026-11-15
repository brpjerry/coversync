//! Album art rules, and the cover image an import writes.
//!
//! Imported albums keep their art the way Library setup says: as a cover
//! file beside the tracks (named by the rule, whatever the source was
//! called), embedded in every audio file, or both.

#[cfg(feature = "server")]
use std::io::Cursor;

#[cfg(feature = "server")]
use image::codecs::jpeg::JpegEncoder;
#[cfg(feature = "server")]
use image::{ImageFormat, ImageReader};
#[cfg(feature = "server")]
use lofty::picture::{MimeType, Picture, PictureType};
use serde::{Deserialize, Serialize};

use crate::{Error, Result, template};

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

/// A cover file name as typed, without its extension: "cover.jpg" means
/// "cover", as the extension follows the image, and nothing means "cover".
/// Every image extension at the end goes, so a name taken once stays.
/// With `used` (a cover file is written), a name no file can have is
/// refused; one kept while it's off is only tidied.
pub fn cover_file_name(typed: &str, used: bool) -> Result<String> {
    let mut name = typed.trim();
    while let Some((stem, ext)) = name.rsplit_once('.')
        && IMAGE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str())
    {
        name = stem.trim_end();
    }
    if name.is_empty() {
        name = FILE_NAMES[0];
    }
    if used
        && (name.contains('/')
            || name.contains(template::NOT_IN_NAMES)
            || name.chars().any(char::is_control)
            || name.starts_with('.')
            || template::reserved(name)
            || name.chars().count() > 64)
    {
        return Err(Error::Invalid(
            "The cover file name can't contain / \\ : * ? \" < > |, start with a dot, or be \
             a name Windows keeps for itself, such as CON."
                .into(),
        ));
    }
    Ok(name.to_string())
}

impl ArtRules {
    pub fn normalize(mut self) -> Result<Self> {
        self.file_name = cover_file_name(&self.file_name, self.placement.file())?;
        Ok(self)
    }

    /// The cover file name for `art`, e.g. "cover.jpg".
    #[cfg(feature = "server")]
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
#[cfg(feature = "server")]
#[derive(Debug, Clone)]
pub struct Art {
    data: Vec<u8>,
    format: ImageFormat,
    pub width: u32,
    pub height: u32,
}

/// Formats kept as they are in a cover file; anything else becomes JPEG.
#[cfg(feature = "server")]
const FILE_FORMATS: &[ImageFormat] = &[ImageFormat::Jpeg, ImageFormat::Png, ImageFormat::WebP];
/// Formats kept as they are when embedded; every tag format and player knows these.
#[cfg(feature = "server")]
const EMBED_FORMATS: &[ImageFormat] = &[ImageFormat::Jpeg, ImageFormat::Png];

/// The media type of image data, for serving it.
#[cfg(feature = "server")]
pub fn media_type(data: &[u8]) -> &'static str {
    image::guess_format(data).map_or("application/octet-stream", |f| f.to_mime_type())
}

#[cfg(feature = "server")]
fn extension(format: ImageFormat) -> &'static str {
    match format {
        ImageFormat::Png => "png",
        ImageFormat::WebP => "webp",
        _ => "jpg",
    }
}

#[cfg(feature = "server")]
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

    /// The cover file a device gets, and its extension: the image as a
    /// cover file keeps it, or, when it's wider than `max_width`, scaled
    /// down to that width, keeping its shape, as a baseline JPEG.
    pub fn device_file(&self, max_width: Option<u32>) -> Result<(Vec<u8>, &'static str)> {
        let Some(width) = max_width.filter(|w| self.width > *w) else {
            return Ok((self.file_bytes()?, self.file_extension()));
        };
        let height = (u64::from(self.height) * u64::from(width) + u64::from(self.width) / 2)
            / u64::from(self.width);
        let img = image::load_from_memory(&self.data)?.resize_exact(
            width,
            (height as u32).max(1),
            image::imageops::FilterType::Lanczos3,
        );
        let mut out = Cursor::new(Vec::new());
        JpegEncoder::new_with_quality(&mut out, 90).encode_image(&img.to_rgb8())?;
        Ok((out.into_inner(), "jpg"))
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
    #[cfg(feature = "server")]
    use image::{DynamicImage, Rgb, RgbImage};

    #[cfg(feature = "server")]
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
        assert!(rules("art: front").is_err());
        assert!(rules("CON").is_err());
        // A name kept while no cover file is written is only tidied.
        let embedded = |name: &str| {
            ArtRules {
                placement: Placement::Embedded,
                file_name: name.into(),
                ..Default::default()
            }
            .normalize()
        };
        assert_eq!(embedded("art: front.jpg").unwrap().file_name, "art: front");
        assert_eq!(embedded(" ").unwrap().file_name, "cover");
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
    #[cfg(feature = "server")]
    fn images_keep_or_change_format() {
        let png = Art::new(image(ImageFormat::Png)).unwrap();
        assert_eq!((png.width, png.height), (8, 6));
        assert_eq!(png.file_extension(), "png");
        assert_eq!(png.picture().unwrap().mime_type(), Some(&MimeType::Png));

        // GIFs (some covers on the Cover Art Archive) become JPEG.
        let gif = Art::new(image(ImageFormat::Gif)).unwrap();
        assert_eq!((gif.width, gif.height), (8, 6));
        assert_eq!(gif.file_extension(), "jpg");
        let bytes = gif.file_bytes().unwrap();
        assert!(image::load_from_memory_with_format(&bytes, ImageFormat::Jpeg).is_ok());

        // WebP files stay WebP, but embedded art is JPEG.
        let webp = Art::new(image(ImageFormat::WebP)).unwrap();
        assert_eq!(webp.file_extension(), "webp");
        let pic = webp.picture().unwrap();
        assert_eq!(pic.mime_type(), Some(&MimeType::Jpeg));
        assert_eq!(pic.pic_type(), PictureType::CoverFront);
        assert!(image::load_from_memory_with_format(pic.data(), ImageFormat::Jpeg).is_ok());

        assert!(Art::new(b"not an image".to_vec()).is_err());
    }

    #[cfg(feature = "server")]
    #[test]
    fn device_files_are_scaled_only_when_too_wide() {
        let png = Art::new(image(ImageFormat::Png)).unwrap();
        let (bytes, ext) = png.device_file(Some(8)).unwrap();
        assert_eq!((ext, bytes), ("png", png.file_bytes().unwrap()));
        let (bytes, ext) = png.device_file(Some(4)).unwrap();
        let scaled = Art::new(bytes).unwrap();
        assert_eq!((ext, scaled.width, scaled.height), ("jpg", 4, 3));
    }
}
