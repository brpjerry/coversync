//! Reading and writing audio file tags through lofty.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use lofty::config::{ParseOptions, WriteOptions};
use lofty::file::{FileType, TaggedFile};
use lofty::picture::{Picture, PictureType};
use lofty::prelude::*;
use lofty::probe::Probe;
use lofty::tag::items::Timestamp;
use lofty::tag::{Tag, TagType};

use crate::model::TagEdit;
use crate::{Error, Result, rules};

pub const AUDIO_EXTENSIONS: &[&str] = &[
    "flac", "mp3", "m4a", "mp4", "aac", "ogg", "oga", "opus", "wv", "ape", "mpc", "aif", "aiff",
    "wav",
];

pub fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| AUDIO_EXTENSIONS.iter().any(|a| a.eq_ignore_ascii_case(e)))
}

/// Tag and stream properties for one file, as the scanner stores them.
#[derive(Debug, Clone, Default)]
pub struct TrackTags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub track_no: Option<u32>,
    pub track_total: Option<u32>,
    pub disc_no: Option<u32>,
    pub disc_total: Option<u32>,
    pub year: Option<i32>,
    pub genre: Option<String>,
    pub duration_ms: u64,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u8>,
    pub channels: Option<u8>,
    pub bitrate: Option<u32>,
    pub format: String,
    /// Rule catalog ids (see `rules::CATALOG`) this file has values for.
    pub tag_keys: Vec<&'static str>,
    /// Text value of each catalog kind the file has.
    pub values: HashMap<&'static str, String>,
}

fn tag_err(e: impl std::fmt::Display) -> Error {
    Error::Tags(e.to_string())
}

fn probe(path: &Path, cover_art: bool) -> Result<TaggedFile> {
    Probe::open(path)
        .map_err(tag_err)?
        .options(ParseOptions::new().read_cover_art(cover_art))
        .read()
        .map_err(tag_err)
}

fn text(s: Option<std::borrow::Cow<'_, str>>) -> Option<String> {
    s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// The first four-digit run in a date-like string, e.g. "2003-04-01" -> 2003.
fn year_from_str(s: &str) -> Option<i32> {
    let digits: String = s
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take(4)
        .collect();
    (digits.len() == 4).then(|| digits.parse().ok()).flatten()
}

fn year(tag: &Tag) -> Option<i32> {
    if let Some(ts) = tag.date() {
        return Some(ts.year as i32);
    }
    [
        ItemKey::RecordingDate,
        ItemKey::Year,
        ItemKey::ReleaseDate,
        ItemKey::OriginalReleaseDate,
    ]
    .into_iter()
    .find_map(|k| tag.get_string(k).and_then(year_from_str))
}

pub fn format_name(ty: FileType) -> String {
    match ty {
        FileType::Flac => "FLAC".into(),
        FileType::Mpeg => "MP3".into(),
        FileType::Mp4 => "M4A".into(),
        FileType::Aac => "AAC".into(),
        FileType::Opus => "Opus".into(),
        FileType::Vorbis => "Vorbis".into(),
        FileType::WavPack => "WavPack".into(),
        FileType::Ape => "APE".into(),
        FileType::Aiff => "AIFF".into(),
        FileType::Wav => "WAV".into(),
        other => format!("{other:?}"),
    }
}

pub fn read(path: &Path) -> Result<TrackTags> {
    let file = probe(path, false)?;
    let props = file.properties();
    let mut out = TrackTags {
        duration_ms: props.duration().as_millis() as u64,
        sample_rate: props.sample_rate(),
        bit_depth: props.bit_depth(),
        channels: props.channels(),
        bitrate: props.audio_bitrate().or(props.overall_bitrate()),
        format: format_name(file.file_type()),
        ..Default::default()
    };
    if let Some(tag) = file.primary_tag().or_else(|| file.first_tag()) {
        out.title = text(tag.title());
        out.artist = text(tag.artist());
        out.album = text(tag.album());
        out.album_artist = tag
            .get_string(ItemKey::AlbumArtist)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        out.genre = text(tag.genre());
        out.track_no = tag.track();
        out.track_total = tag.track_total();
        out.disc_no = tag.disk();
        out.disc_total = tag.disk_total();
        out.year = year(tag);
        out.tag_keys = crate::rules::present(tag);
        out.values = values_of(tag);
    }
    Ok(out)
}

/// The embedded front cover (or first picture) of a file, if any.
pub fn embedded_cover(path: &Path) -> Result<Option<Vec<u8>>> {
    let file = probe(path, true)?;
    let Some(tag) = file.primary_tag().or_else(|| file.first_tag()) else {
        return Ok(None);
    };
    let pics = tag.pictures();
    let pic = pics
        .iter()
        .find(|p| p.pic_type() == PictureType::CoverFront)
        .or_else(|| pics.first());
    Ok(pic.map(|p| p.data().to_vec()))
}

fn parse_number(field: &str, value: &str) -> Result<Option<u32>> {
    let v = value.trim();
    if v.is_empty() {
        return Ok(None);
    }
    // Accept "3" as well as "3/12".
    let head = v.split('/').next().unwrap_or(v).trim();
    head.parse()
        .map(Some)
        .map_err(|_| Error::Tags(format!("{field}: '{value}' is not a number")))
}

/// Check an edit's numeric and date fields without touching any file.
pub fn validate(edit: &TagEdit) -> Result<()> {
    if let Some(v) = &edit.track_no {
        parse_number("track", v)?;
    }
    if let Some(v) = &edit.disc_no {
        parse_number("disc", v)?;
    }
    if let Some(v) = edit
        .year
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        v.parse::<Timestamp>()
            .map_err(|_| Error::Tags(format!("year: '{v}' is not a date")))?;
    }
    Ok(())
}

fn apply(tag: &mut Tag, edit: &TagEdit) -> Result<()> {
    fn set_text(
        value: &Option<String>,
        tag: &mut Tag,
        set: fn(&mut Tag, String),
        remove: fn(&mut Tag),
    ) {
        if let Some(v) = value {
            let v = v.trim();
            if v.is_empty() {
                remove(tag)
            } else {
                set(tag, v.to_string())
            }
        }
    }
    set_text(
        &edit.title,
        tag,
        |t, v| t.set_title(v),
        |t| t.remove_title(),
    );
    set_text(
        &edit.artist,
        tag,
        |t, v| t.set_artist(v),
        |t| t.remove_artist(),
    );
    set_text(
        &edit.album,
        tag,
        |t, v| t.set_album(v),
        |t| t.remove_album(),
    );
    set_text(
        &edit.genre,
        tag,
        |t, v| t.set_genre(v),
        |t| t.remove_genre(),
    );
    set_text(
        &edit.album_artist,
        tag,
        |t, v| {
            t.insert_text(ItemKey::AlbumArtist, v);
        },
        |t| t.remove_key(ItemKey::AlbumArtist),
    );
    if let Some(v) = &edit.track_no {
        match parse_number("track", v)? {
            Some(n) => tag.set_track(n),
            None => tag.remove_track(),
        }
    }
    if let Some(v) = &edit.disc_no {
        match parse_number("disc", v)? {
            Some(n) => tag.set_disk(n),
            None => tag.remove_disk(),
        }
    }
    if let Some(v) = &edit.year {
        let v = v.trim();
        if v.is_empty() {
            tag.remove_date();
        } else {
            let ts: Timestamp = v
                .parse()
                .map_err(|_| Error::Tags(format!("year: '{v}' is not a date")))?;
            tag.set_date(ts);
        }
    }
    Ok(())
}

/// Apply `edit` to the file's primary tag. The change is written to a copy
/// beside the file and renamed over it, so a failed write leaves the original.
pub fn write(path: &Path, edit: &TagEdit) -> Result<()> {
    // Keep the real extension last: lofty picks the format from it.
    let name = path.file_name().ok_or(Error::NotFound)?.to_string_lossy();
    let tmp = path.with_file_name(format!(".coversync-tmp-{name}"));
    fs::copy(path, &tmp)?;
    let result = (|| {
        let mut file = probe(&tmp, true)?;
        let tag = primary_tag_mut(&mut file);
        apply(tag, edit)?;
        file.save_to_path(&tmp, WriteOptions::default())
            .map_err(tag_err)?;
        fs::File::open(&tmp)?.sync_all()?;
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// The text value of every rule tag kind the file has, keyed by catalog id.
pub fn kind_values(path: &Path) -> Result<HashMap<&'static str, String>> {
    Ok(read_kinds(path, false)?.values)
}

/// A file's tag kind values, whether it has embedded pictures (only looked
/// for when asked, since reading them costs more), and the tag type new
/// values would be written to.
pub struct KindValues {
    pub values: HashMap<&'static str, String>,
    pub has_art: bool,
    pub tag_type: TagType,
}

pub fn read_kinds(path: &Path, pictures: bool) -> Result<KindValues> {
    let file = probe(path, pictures)?;
    let values = file
        .primary_tag()
        .or_else(|| file.first_tag())
        .map(values_of)
        .unwrap_or_default();
    Ok(KindValues {
        values,
        has_art: pictures && has_pictures(&file),
        tag_type: file.primary_tag_type(),
    })
}

/// Whether a `tag_type` tag can hold `kind` as text (numbers and dates
/// always fit; some formats have no place for, say, MusicBrainz ids).
pub fn can_store(tag_type: TagType, kind: &str) -> bool {
    let Some(k) = rules::kind(kind) else {
        return false;
    };
    matches!(
        kind,
        "track_number" | "track_total" | "disc_number" | "disc_total" | "date"
    ) || Tag::new(tag_type).insert_text(k.keys[0], "x".into())
}

/// The file's primary tag, created if missing. A new one starts as a copy of
/// the tag the file has (RIFF INFO, ID3v1…), since readers prefer the primary
/// tag and would otherwise lose every value only the old one held.
fn primary_tag_mut(file: &mut TaggedFile) -> &mut Tag {
    if file.primary_tag().is_none() {
        let mut tag = Tag::new(file.primary_tag_type());
        if let Some(existing) = file.first_tag() {
            for item in existing.items() {
                tag.insert(item.clone());
            }
            for picture in existing.pictures() {
                tag.push_picture(picture.clone());
            }
        }
        file.insert_tag(tag);
    }
    file.primary_tag_mut().expect("primary tag inserted above")
}

fn has_pictures(file: &TaggedFile) -> bool {
    file.tags().iter().any(|t| !t.pictures().is_empty())
}

fn values_of(tag: &Tag) -> HashMap<&'static str, String> {
    let mut out = HashMap::new();
    for kind in rules::CATALOG {
        if let Some(v) = kind
            .keys
            .iter()
            .filter_map(|k| tag.get_string(*k))
            .map(str::trim)
            .find(|v| !v.is_empty())
        {
            out.insert(kind.id, v.to_string());
        }
    }
    out
}

/// Check a value typed for a tag kind before anything is written.
pub fn validate_kind(id: &str, value: &str) -> Result<()> {
    let v = value.trim();
    if v.is_empty() {
        return Ok(());
    }
    let label = rules::kind(id).map_or(id, |k| k.label);
    match id {
        "track_number" | "track_total" | "disc_number" | "disc_total" => {
            parse_number(label, v)?;
        }
        "date" => {
            v.parse::<Timestamp>().map_err(|_| {
                Error::Tags(format!(
                    "{label}: '{v}' is not a date like 2003 or 2003-04-01"
                ))
            })?;
        }
        _ if rules::NOT_EDITABLE.contains(&id) => {
            return Err(Error::Tags(format!("{label} can't be entered as text")));
        }
        _ => {}
    }
    Ok(())
}

/// Set tag kinds (catalog ids) on `tag`; an empty value removes the kind.
fn apply_kinds(tag: &mut Tag, values: &[(&str, String)]) -> Result<()> {
    for (id, value) in values {
        let kind =
            rules::kind(id).ok_or_else(|| Error::Invalid(format!("unknown tag kind '{id}'")))?;
        validate_kind(id, value)?;
        let v = value.trim();
        if v.is_empty() {
            for key in kind.keys {
                tag.remove_key(*key);
            }
            continue;
        }
        // Numbers and dates go through lofty's typed setters so each format
        // stores them natively (MP4 keeps track numbers as integers).
        let number = || parse_number(kind.label, v).map(|n| n.unwrap_or_default());
        match *id {
            "track_number" => tag.set_track(number()?),
            "track_total" => tag.set_track_total(number()?),
            "disc_number" => tag.set_disk(number()?),
            "disc_total" => tag.set_disk_total(number()?),
            "date" => tag.set_date(v.parse().map_err(|_| Error::Tags(format!("date: '{v}'")))?),
            _ => {
                if !tag.insert_text(kind.keys[0], v.to_string()) {
                    return Err(Error::Tags(format!(
                        "{} can't be stored in this file's tag format",
                        kind.label
                    )));
                }
            }
        }
    }
    Ok(())
}

/// Copy `src` to `dest` (which must not exist), setting `values` on the copy
/// and embedding `art` if the file has no pictures yet, or with `replace` in
/// place of its front cover. Returns whether art was embedded. `src` is only
/// read. The copy is written beside `dest` and renamed into place, so a
/// failure never leaves a half-written file at `dest`.
pub fn copy_with_tags(
    src: &Path,
    dest: &Path,
    values: &[(&str, String)],
    art: Option<&Picture>,
    replace: bool,
) -> Result<bool> {
    if dest.exists() {
        return Err(Error::Invalid(format!("{} already exists", dest.display())));
    }
    let dir = dest.parent().ok_or(Error::NotFound)?;
    fs::create_dir_all(dir)?;
    let name = dest.file_name().ok_or(Error::NotFound)?.to_string_lossy();
    let tmp = dest.with_file_name(format!(".coversync-tmp-{name}"));
    let result = (|| {
        fs::copy(src, &tmp)?;
        let mut embedded = false;
        if !values.is_empty() || art.is_some() {
            let mut file = probe(&tmp, true)?;
            let art = art.filter(|_| replace || !has_pictures(&file));
            if !values.is_empty() || art.is_some() {
                if art.is_some() && replace {
                    // Untyped pictures go too: players show them as the cover.
                    // Back covers, booklets and the like stay.
                    let types: Vec<TagType> = file.tags().iter().map(|t| t.tag_type()).collect();
                    for t in types {
                        if let Some(tag) = file.tag_mut(t) {
                            tag.remove_picture_type(PictureType::CoverFront);
                            tag.remove_picture_type(PictureType::Other);
                        }
                    }
                }
                let tag = primary_tag_mut(&mut file);
                apply_kinds(tag, values)?;
                if let Some(picture) = art {
                    tag.push_picture(picture.clone());
                }
                file.save_to_path(&tmp, WriteOptions::default())
                    .map_err(tag_err)?;
            }
            if art.is_some() {
                // Some tag formats (RIFF INFO, say) silently drop pictures.
                if !has_pictures(&probe(&tmp, true)?) {
                    return Err(Error::Tags(format!(
                        "{} files can't hold embedded album art",
                        format_name(file.file_type())
                    )));
                }
                embedded = true;
            }
        }
        fs::File::open(&tmp)?.sync_all()?;
        if dest.exists() {
            return Err(Error::Invalid(format!("{} already exists", dest.display())));
        }
        fs::rename(&tmp, dest)?;
        Ok(embedded)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn years() {
        assert_eq!(year_from_str("2003-04-01"), Some(2003));
        assert_eq!(year_from_str("  1999"), Some(1999));
        assert_eq!(year_from_str("199"), None);
        assert_eq!(year_from_str(""), None);
    }

    #[test]
    fn validate_rejects_bad_fields() {
        let ok = TagEdit {
            track_no: Some("3/12".into()),
            year: Some("2003-04-01".into()),
            ..Default::default()
        };
        assert!(validate(&ok).is_ok());
        let bad = TagEdit {
            year: Some("twenty".into()),
            ..Default::default()
        };
        assert!(validate(&bad).is_err());
    }

    /// A tiny silent WAV whose only tag is RIFF INFO (as ffmpeg writes them).
    fn wav_with_info(path: &Path) {
        let samples = [0u8; 16];
        let mut b = Vec::new();
        b.extend(b"RIFF");
        b.extend(((4 + 24 + 8 + samples.len()) as u32).to_le_bytes());
        b.extend(b"WAVEfmt ");
        b.extend(16u32.to_le_bytes());
        for v in [1u16, 1] {
            b.extend(v.to_le_bytes());
        }
        b.extend(8000u32.to_le_bytes());
        b.extend(16000u32.to_le_bytes());
        for v in [2u16, 16] {
            b.extend(v.to_le_bytes());
        }
        b.extend(b"data");
        b.extend((samples.len() as u32).to_le_bytes());
        b.extend(samples);
        fs::write(path, b).unwrap();
        let mut file = probe(path, false).unwrap();
        let mut tag = Tag::new(lofty::tag::TagType::RiffInfo);
        tag.set_title("Song".into());
        tag.set_album("Record".into());
        file.insert_tag(tag);
        file.save_to_path(path, WriteOptions::default()).unwrap();
    }

    #[test]
    fn a_new_primary_tag_keeps_values_from_the_old_one() {
        let dir = std::env::temp_dir().join(format!("coversync-tags-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let (src, dest) = (dir.join("in.wav"), dir.join("out.wav"));
        let _ = fs::remove_file(&dest);
        wav_with_info(&src);
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(4, 4)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let art = crate::art::Art::new(png.into_inner())
            .unwrap()
            .picture()
            .unwrap();

        let values = [("track_number", "1".to_string())];
        assert!(copy_with_tags(&src, &dest, &values, Some(&art), false).unwrap());
        let t = read(&dest).unwrap();
        assert_eq!(t.title.as_deref(), Some("Song"));
        assert_eq!(t.album.as_deref(), Some("Record"));
        assert_eq!(t.track_no, Some(1));
        assert!(embedded_cover(&dest).unwrap().is_some());
        // The source is only read.
        assert!(embedded_cover(&src).unwrap().is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn replaced_art_takes_the_front_covers_place() {
        let dir = std::env::temp_dir().join(format!("coversync-art-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = |name: &str| dir.join(name);
        for name in ["old.wav", "kept.wav", "new.wav"] {
            let _ = fs::remove_file(path(name));
        }
        wav_with_info(&path("in.wav"));
        let cover = |edge: u32| {
            let mut png = std::io::Cursor::new(Vec::new());
            image::DynamicImage::new_rgb8(edge, edge)
                .write_to(&mut png, image::ImageFormat::Png)
                .unwrap();
            crate::art::Art::new(png.into_inner())
                .unwrap()
                .picture()
                .unwrap()
        };
        let (old, new) = (cover(4), cover(8));
        assert!(copy_with_tags(&path("in.wav"), &path("old.wav"), &[], Some(&old), false).unwrap());
        // A back cover beside the front one.
        let mut file = probe(&path("old.wav"), true).unwrap();
        let back = Picture::unchecked(old.data().to_vec())
            .pic_type(PictureType::CoverBack)
            .mime_type(lofty::picture::MimeType::Png)
            .build();
        primary_tag_mut(&mut file).push_picture(back);
        file.save_to_path(path("old.wav"), WriteOptions::default())
            .unwrap();

        // Without `replace`, a file with art keeps it.
        let kept = copy_with_tags(&path("old.wav"), &path("kept.wav"), &[], Some(&new), false);
        assert!(!kept.unwrap());
        assert_eq!(
            embedded_cover(&path("kept.wav")).unwrap().unwrap(),
            old.data()
        );

        assert!(copy_with_tags(&path("old.wav"), &path("new.wav"), &[], Some(&new), true).unwrap());
        assert_eq!(
            embedded_cover(&path("new.wav")).unwrap().unwrap(),
            new.data()
        );
        let file = probe(&path("new.wav"), true).unwrap();
        let types: Vec<PictureType> = file
            .tags()
            .iter()
            .flat_map(|t| t.pictures())
            .map(|p| p.pic_type())
            .collect();
        let fronts = types.iter().filter(|t| **t == PictureType::CoverFront);
        assert_eq!(fronts.count(), 1);
        assert!(types.contains(&PictureType::CoverBack));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn numbers() {
        assert_eq!(parse_number("t", "3/12").unwrap(), Some(3));
        assert_eq!(parse_number("t", " ").unwrap(), None);
        assert!(parse_number("t", "x").is_err());
    }
}
