//! Reading and writing audio file tags through lofty.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use lofty::config::{ParseOptions, WriteOptions};
use lofty::file::{FileType, TaggedFile};
use lofty::ogg::tag::VorbisComments;
use lofty::picture::{Picture, PictureType};
use lofty::prelude::*;
use lofty::probe::Probe;
use lofty::tag::items::Timestamp;
use lofty::tag::{ItemValue, SplitTag, Tag, TagType};

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

/// The Vorbis comments of a FLAC or Ogg file that lofty's generic [`Tag`]
/// leaves out, read aside before an edit. Lofty's owned conversions put back
/// what a generic tag couldn't hold for other tag types, but its Vorbis
/// conversion rewrites values its generic items don't round-trip (see
/// [`save_vorbis`]), so Vorbis comments are written here.
struct VorbisRest {
    /// The comments lofty has no key for, as read (nothing renamed or
    /// rejected), with the file's vendor string.
    rest: VorbisComments,
    /// The vendor string, which lofty also reads as an ENCODER comment.
    vendor: String,
    /// Whether the file had ENCODER comments of its own.
    had_encoder: bool,
    /// The file's RATING comments, provider-qualified ones (`RATING:MusicBee`)
    /// included, as written and in their order (blanked in `rest`): coversync
    /// only ever clears a rating, and lofty can't write a provider's back the
    /// way it read it.
    ratings: Vec<(String, String)>,
}

/// The file's [`VorbisRest`]; None for a file whose primary tag isn't
/// Vorbis comments.
fn vorbis_rest(path: &Path, file_type: FileType) -> Result<Option<VorbisRest>> {
    use lofty::file::AudioFile;
    // Pictures go through `Tag`: not read here, so not kept twice.
    let options = ParseOptions::new()
        .read_properties(false)
        .read_cover_art(false);
    let mut f = std::io::BufReader::new(fs::File::open(path)?);
    let tag = match file_type {
        FileType::Flac => lofty::flac::FlacFile::read_from(&mut f, options)
            .map_err(tag_err)?
            .remove_vorbis_comments()
            .unwrap_or_default(),
        FileType::Vorbis => lofty::ogg::VorbisFile::read_from(&mut f, options)
            .map_err(tag_err)?
            .remove_vorbis_comments(),
        FileType::Opus => lofty::ogg::OpusFile::read_from(&mut f, options)
            .map_err(tag_err)?
            .remove_vorbis_comments(),
        FileType::Speex => lofty::ogg::SpeexFile::read_from(&mut f, options)
            .map_err(tag_err)?
            .remove_vorbis_comments(),
        _ => return Ok(None),
    };
    let vendor = tag.vendor().to_string();
    let had_encoder = tag.get("ENCODER").is_some();
    let is_rating = |k: &str| {
        let k = k.to_ascii_uppercase();
        k == "RATING" || k.starts_with("RATING:")
    };
    let ratings: Vec<(String, String)> = tag
        .items()
        .filter(|(k, _)| is_rating(k))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let mut rest = VorbisComments::from(tag.split_tag().0);
    // Those lofty couldn't read too, so all go back in their order. Blanked
    // rather than removed: lofty's `remove` reorders the other comments,
    // and its `push` refuses a nameless one (`=legacy`) a rebuilt list
    // would need; `insert` keeps the others' order, and an empty value isn't
    // written.
    let keys: std::collections::HashSet<String> = ratings
        .iter()
        .map(|(k, _)| k.to_ascii_uppercase())
        .collect();
    for key in keys {
        rest.insert(key, String::new());
    }
    Ok(Some(VorbisRest {
        rest,
        vendor,
        had_encoder,
        ratings,
    }))
}

/// Write `tag`, a FLAC or Ogg file's primary tag after an edit, as lofty's
/// own writer does (each item under its usual key, its text as it is) over
/// `rest`. The vendor string stays as it was; ENCODER comments are the
/// file's own, or one the edit set, never the vendor string lofty reads as
/// one; ratings are the file's own, unless the edit cleared them. `edited`
/// is the tag kinds (catalog ids) the edit set or cleared.
fn save_vorbis(path: &Path, mut tag: Tag, rest: VorbisRest, edited: &[&str]) -> Result<()> {
    use lofty::ogg::OggPictureStorage;
    use lofty::picture::PictureInformation;
    let encoders: Vec<String> = tag
        .get_strings(ItemKey::EncoderSoftware)
        .map(str::to_string)
        .collect();
    let encoder_edited = edited.contains(&"encoder");
    let vendor_only = !rest.had_encoder && !encoder_edited && encoders == [rest.vendor.as_str()];
    // A rating can't be typed in, only cleared (`validate_kind`).
    let ratings = if edited.contains(&"rating") {
        Vec::new()
    } else {
        rest.ratings
    };
    let mut out = rest.rest;
    out.set_vendor(rest.vendor);
    for item in tag.items() {
        if matches!(
            item.key(),
            ItemKey::EncoderSoftware | ItemKey::Popularimeter
        ) {
            continue;
        }
        let (ItemValue::Text(value) | ItemValue::Locator(value)) = item.value() else {
            continue;
        };
        if let Some(key) = item.key().map_key(TagType::VorbisComments) {
            out.push(key.to_string(), value.clone());
        }
    }
    if !vendor_only {
        for encoder in encoders {
            out.push("ENCODER".into(), encoder);
        }
    }
    // Ratings: the file's own as written; one from another tag (a FLAC that
    // had only ID3v2) through lofty's conversion, under its provider's key.
    if ratings.is_empty() {
        {
            // One at a time, each as lofty converts it. Lofty reads a rating
            // as "owner|stars|count" text, so it can't convert one whose
            // owner has a "|": that one goes through it under a stand-in
            // owner (an unknown provider either way, so the same scale), and
            // gets its own back in the field name. One whose owner can't
            // name a Vorbis field (it has "=") stays as lofty read it, under
            // RATING.
            for item in tag.items().filter(|i| i.key() == ItemKey::Popularimeter) {
                let ItemValue::Text(text) = item.value() else {
                    continue;
                };
                let convert = |text: String| {
                    let mut one = Tag::new(TagType::VorbisComments);
                    one.push(lofty::tag::TagItem::new(
                        ItemKey::Popularimeter,
                        ItemValue::Text(text),
                    ));
                    VorbisComments::from(one)
                };
                let converted = convert(text.clone());
                if converted.items().len() > 0 {
                    for (key, value) in converted.items() {
                        out.push(key.to_string(), value.to_string());
                    }
                    continue;
                }
                let mut parts = text.rsplitn(3, '|');
                let fields = (parts.next(), parts.next(), parts.next());
                let named = |owner: &str| {
                    !owner.is_empty()
                        && owner
                            .bytes()
                            .all(|b| (0x20..=0x7D).contains(&b) && b != b'=')
                };
                if let (Some(count), Some(stars), Some(owner)) = fields
                    && owner.contains('|')
                    && named(owner)
                    && let Some((_, value)) =
                        convert(format!("coversync|{stars}|{count}")).items().next()
                {
                    out.push(format!("RATING:{owner}"), value.to_string());
                } else {
                    out.push("RATING".into(), text.clone());
                }
            }
        }
    } else {
        for (key, value) in ratings {
            out.push(key, value);
        }
    }
    // From the end, so each move is cheap; then in their order.
    let mut pictures = Vec::with_capacity(tag.pictures().len());
    while !tag.pictures().is_empty() {
        pictures.push(tag.remove_picture(tag.pictures().len() - 1));
    }
    for picture in pictures.into_iter().rev() {
        let info = PictureInformation::from_picture(&picture).unwrap_or_default();
        // Lofty keeps one icon of each kind, as the format says; a file
        // with two isn't saved rather than lose one.
        if out
            .insert_picture(picture, Some(info))
            .map_err(tag_err)?
            .is_some()
        {
            return Err(Error::Tags(
                "the file has two icons of one kind, which FLAC and Ogg allow one of, so it wasn't changed".into(),
            ));
        }
    }
    out.save_to_path(path, WriteOptions::default())
        .map_err(tag_err)
}

/// Why lofty wouldn't write the ID3v2 `frame` back as it read it, if it
/// wouldn't: an ID3v2.2 frame with no ID3v2.4 name (dropped); a
/// compressed, encrypted or length-prefixed one (it consumes the prefix on
/// reading but keeps the flag); a chapter located by byte offsets, which a
/// resized tag moves away from its audio; or, inside a chapter or table of
/// contents, any of those or a frame flagged as unsynchronised (decoded on
/// reading, written as it is).
fn unwritable(frame: &lofty::id3::v2::Frame<'_>, nested: bool) -> Option<String> {
    use lofty::id3::v2::Frame;
    let id = frame.id_str();
    if frame.id().is_outdated() {
        return Some(format!("a {id} frame, which has no ID3v2.4 name"));
    }
    let flags = frame.flags();
    if flags.compression || flags.encryption.is_some() || flags.data_length_indicator.is_some() {
        return Some(format!(
            "a compressed, encrypted or length-prefixed {id} frame"
        ));
    }
    if nested && flags.unsynchronisation {
        return Some(format!(
            "an unsynchronised {id} frame inside a chapter or table of contents"
        ));
    }
    let children = match frame {
        Frame::Chapter(c) => {
            if c.offsets.start != u32::MAX || c.offsets.end != u32::MAX {
                return Some(format!(
                    "a chapter ({}) located by byte offsets, which a resized tag would leave pointing at the wrong audio",
                    c.id
                ));
            }
            &c.children
        }
        Frame::TableOfContents(t) => &t.children,
        _ => return None,
    };
    children.iter().find_map(|f| unwritable(f, true))
}

/// Write `tag`, a primary tag other than Vorbis comments after an edit,
/// through lofty's owned conversion: unlike its writer for a borrowed
/// generic tag, it puts back what the generic tag couldn't hold (custom
/// APE items and covers, unknown ID3v2 frames, freeform MP4 atoms, URL
/// frames) and joins repeated values as the format wants.
fn save_owned(path: &Path, tag: Tag) -> Result<()> {
    let options = WriteOptions::default();
    match tag.tag_type() {
        TagType::Id3v2 => {
            let mut id3 = lofty::id3::v2::Id3v2Tag::from(tag);
            // A frame lofty wouldn't write back as it read it: not saved
            // rather than lost or corrupted.
            if let Some(why) = (&id3).into_iter().find_map(|f| unwritable(f, false)) {
                return Err(Error::Tags(format!(
                    "the file's ID3v2 tag has {why}, which can't be written back as it is, so it wasn't changed"
                )));
            }
            // Frames lofty kept aside come flagged as unsynchronised but
            // already decoded: written as they are, unflagged, so they aren't
            // decoded twice.
            id3.retain_mut(|frame| {
                let mut flags = frame.flags();
                flags.unsynchronisation = false;
                frame.set_flags(flags);
                true
            });
            // Header flags as lofty writes them by default, as before: it
            // can't write CRC or restrictions back readably, and
            // unsynchronisation would decode its frames twice.
            id3.set_flags(lofty::id3::v2::Id3v2TagFlags::default());
            id3.save_to_path(path, options)
        }
        TagType::Mp4Ilst => lofty::mp4::Ilst::from(tag).save_to_path(path, options),
        TagType::Ape => {
            // Number pairs and the compilation flag as written: the
            // conversion would read them as numbers (Track=A1 lost, 01/09
            // made 1/9).
            let mut tag = tag;
            let mut literal = Vec::new();
            let mut typed = Vec::new();
            for (name, number, total) in [
                ("Track", ItemKey::TrackNumber, ItemKey::TrackTotal),
                ("Disc", ItemKey::DiscNumber, ItemKey::DiscTotal),
            ] {
                let value = |key| tag.get(key).map(|i| i.value().clone());
                match (value(number), value(total)) {
                    (None, None) => {}
                    (Some(ItemValue::Text(n)), None) => literal.push((name, n)),
                    (n @ (None | Some(ItemValue::Text(_))), Some(ItemValue::Text(t))) => {
                        let n = match n {
                            Some(ItemValue::Text(n)) => n,
                            _ => "0".into(),
                        };
                        literal.push((name, format!("{n}/{t}")));
                    }
                    // A binary or locator item, as it was (lofty files it
                    // under either key).
                    (Some(v), None) | (None, Some(v)) => typed.push((name, v)),
                    _ => {
                        return Err(Error::Tags(format!(
                            "the file's APE {name} item can't hold both its number and total as they are, so it wasn't changed"
                        )));
                    }
                }
                tag.remove_key(number);
                tag.remove_key(total);
            }
            // Compilation as it was, text, locator or binary, every value.
            let compilation: Vec<ItemValue> = tag
                .items()
                .filter(|i| i.key() == ItemKey::FlagCompilation)
                .map(|i| i.value().clone())
                .collect();
            tag.remove_key(ItemKey::FlagCompilation);
            let mut ape = lofty::ape::ApeTag::from(tag);
            for (name, value) in literal {
                let item = lofty::ape::ApeItem::new(name.into(), ItemValue::Text(value))
                    .map_err(tag_err)?;
                ape.insert(item);
            }
            for (name, value) in typed {
                ape.insert(lofty::ape::ApeItem::new(name.into(), value).map_err(tag_err)?);
            }
            for value in compilation {
                ape.push(lofty::ape::ApeItem::new("Compilation".into(), value).map_err(tag_err)?);
            }
            ape.save_to_path(path, options)
        }
        // RIFF INFO, AIFF text chunks: as lofty writes them.
        _ => tag.save_to_path(path, options),
    }
    .map_err(tag_err)
}

/// Save an edit of `file`, read from `path` (`rest`, for Vorbis comments,
/// read before the edit): its primary tag, keeping what lofty's generic tag
/// couldn't hold, and of its other tags only those in `changed` (pictures
/// replaced) that the format can write. Untouched tags stay as they are on
/// disk. `edited`: the tag kinds the edit set or cleared.
fn save(
    path: &Path,
    mut file: TaggedFile,
    rest: Option<VorbisRest>,
    changed: &[TagType],
    edited: &[&str],
) -> Result<()> {
    let primary = file.primary_tag_type();
    for t in changed.iter().filter(|t| **t != primary) {
        if file.tag_support(*t).is_writable()
            && let Some(tag) = file.tag(*t)
        {
            tag.save_to_path(path, WriteOptions::default())
                .map_err(tag_err)?;
        }
    }
    let Some(tag) = file.remove(primary) else {
        return Ok(());
    };
    match rest {
        Some(rest) => save_vorbis(path, tag, rest, edited),
        None => save_owned(path, tag),
    }
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
        let rest = vorbis_rest(&tmp, file.file_type())?;
        let tag = primary_tag_mut(&mut file);
        apply(tag, edit)?;
        save(&tmp, file, rest, &[], &[])?;
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

/// As [`read_kinds`], but a tag naming people that the file holds several
/// times (Vorbis comments `ARTIST=A feat. B` and `ARTIST=C`, say) has them
/// all, joined by `split`'s separator, when one has a phrase to split at:
/// the import then rewrites the tag, and would otherwise keep only the first.
pub fn read_kinds_split(
    path: &Path,
    pictures: bool,
    split: &crate::split::Splitter,
) -> Result<KindValues> {
    let file = probe(path, pictures)?;
    let tag = file.primary_tag().or_else(|| file.first_tag());
    let mut values = tag.map(values_of).unwrap_or_default();
    if let Some(tag) = tag {
        for kind in crate::split::PEOPLE.iter().filter_map(|id| rules::kind(id)) {
            let all: Vec<&str> = kind
                .keys
                .iter()
                .flat_map(|key| tag.get_strings(*key))
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .collect();
            if all.len() > 1 && all.iter().any(|v| split.rewrite(kind.id, v).is_some()) {
                values.insert(kind.id, all.join(split.separator()));
            }
        }
    }
    Ok(KindValues {
        values,
        has_art: pictures && has_pictures(&file),
        tag_type: file.primary_tag_type(),
    })
}

/// The UPC some stores tag files with instead of a barcode, which lofty
/// leaves out of its generic tags: a Vorbis comment (FLAC, Ogg) or an ID3v2
/// TXXX frame (MP3, or an ID3v2 tag on a FLAC file).
pub fn upc(path: &Path) -> Option<String> {
    use lofty::flac::FlacFile;
    use lofty::mpeg::MpegFile;
    use lofty::ogg::{OpusFile, VorbisFile};
    let file_type = Probe::open(path)
        .ok()?
        .guess_file_type()
        .ok()?
        .file_type()?;
    let options = ParseOptions::new()
        .read_properties(false)
        .read_cover_art(false);
    let mut f = fs::File::open(path).ok()?;
    let found = match file_type {
        FileType::Flac => {
            let file = FlacFile::read_from(&mut f, options).ok()?;
            let vorbis = file
                .vorbis_comments()
                .and_then(|t| t.get("UPC"))
                .filter(|v| !v.trim().is_empty());
            vorbis
                .or_else(|| file.id3v2().and_then(|t| t.get_user_text("UPC")))
                .map(str::to_string)
        }
        FileType::Vorbis => VorbisFile::read_from(&mut f, options)
            .ok()?
            .vorbis_comments()
            .get("UPC")
            .map(str::to_string),
        FileType::Opus => OpusFile::read_from(&mut f, options)
            .ok()?
            .vorbis_comments()
            .get("UPC")
            .map(str::to_string),
        FileType::Mpeg => MpegFile::read_from(&mut f, options)
            .ok()?
            .id3v2()?
            .get_user_text("UPC")
            .map(str::to_string),
        _ => None,
    };
    found
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
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
            // Every value, not the last of each key (two ratings, say).
            for item in existing.items() {
                tag.push(item.clone());
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
                    "{label}: '{v}' should be a year like 2003, or a date like 2003-04-01"
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
    copy_with_tags_hashed(src, dest, values, art, replace).map(|(embedded, _)| embedded)
}

/// Import variant that returns the digest of the final bytes. Plain copies
/// hash while streaming or after a reflink; Lofty's seek-based tag rewrites
/// require a final read.
pub fn copy_with_tags_hashed(
    src: &Path,
    dest: &Path,
    values: &[(&str, String)],
    art: Option<&Picture>,
    replace: bool,
) -> Result<(bool, crate::hashes::FileHash)> {
    if dest.exists() {
        return Err(Error::Invalid(format!("{} already exists", dest.display())));
    }
    let dir = dest.parent().ok_or(Error::NotFound)?;
    fs::create_dir_all(dir)?;
    let name = dest.file_name().ok_or(Error::NotFound)?.to_string_lossy();
    let tmp = dest.with_file_name(format!(".coversync-tmp-{name}"));
    let result = (|| {
        let copied_hash = if values.is_empty() && art.is_none() {
            Some(crate::hashes::copy(src, &tmp)?)
        } else {
            fs::copy(src, &tmp)?;
            None
        };
        let mut embedded = false;
        if !values.is_empty() || art.is_some() {
            let mut file = probe(&tmp, true)?;
            let file_type = file.file_type();
            let art = art.filter(|_| replace || !has_pictures(&file));
            if !values.is_empty() || art.is_some() {
                let rest = vorbis_rest(&tmp, file.file_type())?;
                // Other tags are only written when their pictures change.
                let mut changed = Vec::new();
                if art.is_some() && replace {
                    // Untyped pictures go too: players show them as the cover.
                    // Back covers, booklets and the like stay.
                    let types: Vec<TagType> = file.tags().iter().map(|t| t.tag_type()).collect();
                    for t in types {
                        if let Some(tag) = file.tag_mut(t) {
                            let before = tag.pictures().len();
                            tag.remove_picture_type(PictureType::CoverFront);
                            tag.remove_picture_type(PictureType::Other);
                            if tag.pictures().len() != before {
                                changed.push(t);
                            }
                        }
                    }
                }
                let tag = primary_tag_mut(&mut file);
                apply_kinds(tag, values)?;
                if let Some(picture) = art {
                    tag.push_picture(picture.clone());
                }
                let edited: Vec<&str> = values.iter().map(|(id, _)| *id).collect();
                save(&tmp, file, rest, &changed, &edited)?;
            }
            if art.is_some() {
                // Some tag formats (RIFF INFO, say) silently drop pictures.
                if !has_pictures(&probe(&tmp, true)?) {
                    return Err(Error::Tags(format!(
                        "{} files can't hold embedded album art",
                        format_name(file_type)
                    )));
                }
                embedded = true;
            }
        }
        fs::File::open(&tmp)?.sync_all()?;
        let hash = match copied_hash {
            Some(hash) => hash,
            None => crate::hashes::file(&tmp)?,
        };
        if dest.exists() {
            return Err(Error::Invalid(format!("{} already exists", dest.display())));
        }
        fs::rename(&tmp, dest)?;
        Ok((embedded, hash))
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
    fn a_failed_plain_copy_removes_its_staging_file() {
        let dir =
            std::env::temp_dir().join(format!("coversync-copy-failure-{}", std::process::id()));
        let src = dir.join("not-a-file");
        fs::create_dir_all(&src).unwrap();
        let dest = dir.join("failed.wav");
        assert!(copy_with_tags_hashed(&src, &dest, &[], None, false).is_err());
        assert!(!dest.exists());
        assert!(!dir.join(".coversync-tmp-failed.wav").exists());
    }

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
    #[cfg(feature = "server")]
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
    #[cfg(feature = "server")]
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
    #[cfg(feature = "server")]
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

    /// A FLAC file with these Vorbis comments and a few bytes standing for
    /// its audio (lofty's writer expects some after the metadata).
    fn flac_with_comments(path: &Path, comments: &[&str]) {
        let mut b = b"fLaC".to_vec();
        // STREAMINFO: 4096-sample blocks, 44.1 kHz, stereo, 16 bits, no samples.
        b.extend([
            0x00, 0x00, 0x00, 34, 0x10, 0x00, 0x10, 0x00, 0, 0, 0, 0, 0, 0,
        ]);
        let packed: u64 = (44100 << 44) | (1 << 41) | (15 << 36);
        b.extend(packed.to_be_bytes());
        b.extend([0u8; 16]);
        let mut block = Vec::new();
        let vendor = b"coversync";
        block.extend((vendor.len() as u32).to_le_bytes());
        block.extend(vendor);
        block.extend((comments.len() as u32).to_le_bytes());
        for c in comments {
            block.extend((c.len() as u32).to_le_bytes());
            block.extend(c.as_bytes());
        }
        // The last block: VORBIS_COMMENT.
        b.push(0x80 | 4);
        b.extend(&(block.len() as u32).to_be_bytes()[1..]);
        b.extend(block);
        // A frame header's sync code, and some bytes.
        b.extend([0xFF, 0xF8, 0x69, 0x08, 0x00, 0x00, 0x00, 0x00]);
        fs::write(path, b).unwrap();
    }

    #[test]
    fn a_upc_comment_is_read_though_lofty_leaves_it_out() {
        let dir = std::env::temp_dir().join(format!("coversync-upc-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.flac");
        flac_with_comments(&path, &["TITLE=もし、", "upc= 4547366860535 "]);
        assert_eq!(upc(&path).as_deref(), Some("4547366860535"));
        // Not a barcode lofty knows, so not one read_kinds sees.
        assert!(
            !read_kinds(&path, false)
                .unwrap()
                .values
                .contains_key("barcode")
        );
        flac_with_comments(&path, &["TITLE=もし、", "UPC= "]);
        assert_eq!(upc(&path), None);
        flac_with_comments(&path, &["TITLE=もし、"]);
        assert_eq!(upc(&path), None);
        assert_eq!(upc(&dir.join("missing.flac")), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn names_held_several_times_are_read_together_when_one_splits() {
        let dir = std::env::temp_dir().join(format!("coversync-multi-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.flac");
        let split = crate::split::Splitter::new(";", &["feat.".into()]);
        flac_with_comments(&path, &["ARTIST=A feat. B", "ARTIST=C", "TITLE=T"]);
        let read = read_kinds_split(&path, false, &split).unwrap();
        assert_eq!(read.values["artist"], "A feat. B;C");
        assert_eq!(
            split.rewrite("artist", &read.values["artist"]).unwrap(),
            "A;B;C"
        );
        // As read_kinds sees it.
        assert_eq!(
            read_kinds(&path, false).unwrap().values["artist"],
            "A feat. B"
        );
        // Nothing to split: the first, as ever, and the file's own left alone.
        flac_with_comments(&path, &["ARTIST=A", "ARTIST=C"]);
        assert_eq!(
            read_kinds_split(&path, false, &split).unwrap().values["artist"],
            "A"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A FLAC file's own Vorbis comments: its vendor string and every
    /// comment, as lofty's generic `Tag` doesn't show them.
    fn vorbis(path: &Path) -> (String, Vec<(String, String)>) {
        let mut f = fs::File::open(path).unwrap();
        let flac = lofty::flac::FlacFile::read_from(&mut f, ParseOptions::new()).unwrap();
        let tag = flac.vorbis_comments().unwrap();
        let items = tag
            .items()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        (tag.vendor().to_string(), items)
    }

    fn has(items: &[(String, String)], key: &str, value: &str) -> bool {
        items.iter().any(|(k, v)| k == key && v == value)
    }

    fn keys<'a>(items: &'a [(String, String)], key: &'a str) -> impl Iterator<Item = &'a str> {
        items
            .iter()
            .filter(move |(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn an_import_keeps_the_tags_lofty_has_no_key_for() {
        let dir = std::env::temp_dir().join(format!("coversync-keep-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let (src, dest) = (dir.join("in.flac"), dir.join("out.flac"));
        let comments = [
            "TITLE=Song",
            "ALBUM_JP=僕らのLIVE 君とのLIFE",
            "CDTOC=3+96+2D2B+6256",
            "ORGANIZATION=Lantis",
            "TOTALTRACKS=13",
        ];
        flac_with_comments(&src, &comments);
        let values = [
            ("title", "Song!".to_string()),
            ("track_number", "1".to_string()),
        ];
        copy_with_tags(&src, &dest, &values, None, false).unwrap();
        let (vendor, items) = vorbis(&dest);
        assert!(
            has(&items, "ALBUM_JP", "僕らのLIVE 君とのLIFE"),
            "{items:?}"
        );
        assert!(has(&items, "CDTOC", "3+96+2D2B+6256"), "{items:?}");
        assert!(has(&items, "TITLE", "Song!"));
        assert!(has(&items, "TRACKNUMBER", "1"));
        assert_eq!(keys(&items, "TITLE").count(), 1);
        // Tags lofty knows by another name keep their values, under its name.
        assert!(has(&items, "LABEL", "Lantis"), "{items:?}");
        assert!(has(&items, "TRACKTOTAL", "13"), "{items:?}");
        // The vendor string stays the vendor string, not also an ENCODER comment.
        assert_eq!(vendor, "coversync");
        assert_eq!(keys(&items, "ENCODER").count(), 0, "{items:?}");
        // The source is only read.
        assert_eq!(vorbis(&src).1.len(), comments.len());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_vendor_string_and_encoder_stay_apart() {
        let dir = std::env::temp_dir().join(format!("coversync-vendor-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = |name: &str| dir.join(name);
        flac_with_comments(&path("in.flac"), &["TITLE=Song", "ENCODER=EAC"]);
        let number = [("track_number", "1".to_string())];
        copy_with_tags(&path("in.flac"), &path("a.flac"), &number, None, false).unwrap();
        let (vendor, items) = vorbis(&path("a.flac"));
        assert_eq!(vendor, "coversync");
        assert_eq!(keys(&items, "ENCODER").collect::<Vec<_>>(), ["EAC"]);
        // An edit to the encoder is the ENCODER comment, the vendor string kept.
        let encoder = [("encoder", "X".to_string())];
        copy_with_tags(&path("in.flac"), &path("b.flac"), &encoder, None, false).unwrap();
        let (vendor, items) = vorbis(&path("b.flac"));
        assert_eq!(vendor, "coversync");
        assert_eq!(keys(&items, "ENCODER").collect::<Vec<_>>(), ["X"]);
        // And with no ENCODER comment before, too.
        flac_with_comments(&path("plain.flac"), &["TITLE=Song"]);
        copy_with_tags(&path("plain.flac"), &path("c.flac"), &encoder, None, false).unwrap();
        let (vendor, items) = vorbis(&path("c.flac"));
        assert_eq!(vendor, "coversync");
        assert_eq!(keys(&items, "ENCODER").collect::<Vec<_>>(), ["X"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_tag_edit_keeps_the_tags_lofty_has_no_key_for() {
        let dir = std::env::temp_dir().join(format!("coversync-edit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.flac");
        flac_with_comments(&path, &["TITLE=Song", "TITLE_KO=노래", "ARTIST=A"]);
        let edit = TagEdit {
            title: Some("New".into()),
            ..Default::default()
        };
        write(&path, &edit).unwrap();
        let (vendor, items) = vorbis(&path);
        assert!(has(&items, "TITLE_KO", "노래"), "{items:?}");
        assert!(has(&items, "TITLE", "New"));
        assert!(has(&items, "ARTIST", "A"));
        assert_eq!(vendor, "coversync");
        assert_eq!(keys(&items, "ENCODER").count(), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(feature = "server")]
    fn flac_pictures_and_kept_tags_go_together() {
        let dir = std::env::temp_dir().join(format!("coversync-flacart-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = |name: &str| dir.join(name);
        flac_with_comments(&path("in.flac"), &["TITLE=Song", "ALBUM_JP=アルバム"]);
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(4, 4)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let art = crate::art::Art::new(png.into_inner())
            .unwrap()
            .picture()
            .unwrap();
        let number = [("track_number", "1".to_string())];
        assert!(
            copy_with_tags(
                &path("in.flac"),
                &path("art.flac"),
                &number,
                Some(&art),
                false
            )
            .unwrap()
        );
        assert_eq!(
            embedded_cover(&path("art.flac")).unwrap().unwrap(),
            art.data()
        );
        assert!(has(&vorbis(&path("art.flac")).1, "ALBUM_JP", "アルバム"));
        // A later edit keeps the picture, once.
        let title = [("title", "New".to_string())];
        copy_with_tags(&path("art.flac"), &path("again.flac"), &title, None, false).unwrap();
        let file = probe(&path("again.flac"), true).unwrap();
        let pictures: usize = file.tags().iter().map(|t| t.pictures().len()).sum();
        assert_eq!(pictures, 1);
        let (_, items) = vorbis(&path("again.flac"));
        assert!(has(&items, "ALBUM_JP", "アルバム"));
        assert!(has(&items, "TITLE", "New"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_edit_keeps_order_ratings_and_flags_as_written() {
        let dir = std::env::temp_dir().join(format!("coversync-order-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = |name: &str| dir.join(name);
        flac_with_comments(
            &path("in.flac"),
            &[
                "ARTIST=A",
                "ARTIST=B",
                "RATING=80",
                "COMPILATION=YES",
                "TITLE=Song",
            ],
        );
        let genre = [("genre", "Anime".to_string())];
        copy_with_tags(&path("in.flac"), &path("out.flac"), &genre, None, false).unwrap();
        let (_, items) = vorbis(&path("out.flac"));
        // The artists in their order: the first is still the track's artist.
        assert_eq!(keys(&items, "ARTIST").collect::<Vec<_>>(), ["A", "B"]);
        assert_eq!(
            read(&path("out.flac")).unwrap().artist.as_deref(),
            Some("A")
        );
        // Values lofty would reinterpret are written as they were.
        assert!(has(&items, "RATING", "80"), "{items:?}");
        assert!(has(&items, "COMPILATION", "YES"), "{items:?}");
        assert!(has(&items, "GENRE", "Anime"));
        // An encoder set to the vendor string, by an edit, is written.
        let encoder = [("encoder", "coversync".to_string())];
        copy_with_tags(&path("in.flac"), &path("enc.flac"), &encoder, None, false).unwrap();
        let (vendor, items) = vorbis(&path("enc.flac"));
        assert_eq!(vendor, "coversync");
        assert_eq!(keys(&items, "ENCODER").collect::<Vec<_>>(), ["coversync"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(feature = "server")]
    fn replacing_art_leaves_a_tag_the_format_cant_write() {
        use lofty::id3::v2::{Frame, Id3v2Tag, PrivateFrame};
        let dir = std::env::temp_dir().join(format!("coversync-roid3-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = |name: &str| dir.join(name);
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
        // A FLAC behind an ID3v2 tag, which lofty reads but won't write there:
        // a front cover, a title and a private frame.
        flac_with_comments(&path("plain.flac"), &["TITLE=Song"]);
        let mut id3 = Id3v2Tag::new();
        id3.set_title("Song".into());
        id3.insert(Frame::Private(PrivateFrame::new(
            "coversync",
            vec![1, 2, 3],
        )));
        let mut generic: Tag = id3.into();
        generic.push_picture(cover(4));
        let id3: Id3v2Tag = generic.into();
        let mut bytes = Vec::new();
        id3.dump_to(&mut bytes, WriteOptions::default()).unwrap();
        bytes.extend(fs::read(path("plain.flac")).unwrap());
        fs::write(path("in.flac"), bytes).unwrap();
        let new = cover(8);
        assert!(
            copy_with_tags(&path("in.flac"), &path("out.flac"), &[], Some(&new), true).unwrap()
        );
        assert_eq!(
            embedded_cover(&path("out.flac")).unwrap().unwrap(),
            new.data()
        );
        // The ID3v2 tag is as it was, private frame and all.
        let mut f = fs::File::open(path("out.flac")).unwrap();
        let flac = lofty::flac::FlacFile::read_from(&mut f, ParseOptions::new()).unwrap();
        let kept = flac.id3v2().unwrap();
        assert!(kept.into_iter().any(|frame| matches!(
            frame,
            Frame::Private(private)
                if private.owner == "coversync" && private.private_data.as_ref() == [1, 2, 3]
        )));
        assert_eq!(kept.title().as_deref(), Some("Song"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ratings_and_comments_lofty_cant_name_stay_as_written() {
        let dir = std::env::temp_dir().join(format!("coversync-ratings-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = |name: &str| dir.join(name);
        flac_with_comments(
            &path("in.flac"),
            &["TITLE=Song", "RATING:MusicBee=80", "RATING=60", "=legacy"],
        );
        let genre = [("genre", "Anime".to_string())];
        copy_with_tags(&path("in.flac"), &path("out.flac"), &genre, None, false).unwrap();
        let (_, items) = vorbis(&path("out.flac"));
        // A provider's rating under its own key and scale, and a bare one.
        assert!(has(&items, "RATING:MusicBee", "80"), "{items:?}");
        assert!(has(&items, "RATING", "60"), "{items:?}");
        assert_eq!(
            items
                .iter()
                .filter(|(k, _)| k.starts_with("RATING"))
                .count(),
            2
        );
        assert!(has(&items, "GENRE", "Anime"));
        // A comment with no name, as read.
        assert!(has(&items, "", "legacy"), "{items:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(feature = "server")]
    fn id3v2_urls_and_untouched_tags_are_kept() {
        use lofty::id3::v2::{Frame, FrameId, Id3v2Tag, UrlLinkFrame};
        let dir = std::env::temp_dir().join(format!("coversync-id3url-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let (src, dest) = (dir.join("in.wav"), dir.join("out.wav"));
        wav_with_info(&src);
        // A RIFF INFO item lofty has no key for, beside the ID3v2 tag.
        let mut f = fs::File::open(&src).unwrap();
        let mut wav = lofty::iff::wav::WavFile::read_from(&mut f, ParseOptions::new()).unwrap();
        drop(f);
        let mut info = wav.remove_riff_info().unwrap();
        info.insert("IXYZ".into(), "kept".into());
        info.save_to_path(&src, WriteOptions::default()).unwrap();
        let mut id3 = Id3v2Tag::new();
        id3.set_title("Song".into());
        id3.insert(Frame::Url(UrlLinkFrame::new(
            FrameId::new("WOAR").unwrap(),
            "https://example.com/artist",
        )));
        id3.save_to_path(&src, WriteOptions::default()).unwrap();
        let values = [("track_number", "1".to_string())];
        copy_with_tags(&src, &dest, &values, None, false).unwrap();
        let mut f = fs::File::open(&dest).unwrap();
        let wav = lofty::iff::wav::WavFile::read_from(&mut f, ParseOptions::new()).unwrap();
        let tag = wav.id3v2().unwrap();
        assert!(tag.into_iter().any(|frame| matches!(
            frame,
            Frame::Url(url) if url.url() == "https://example.com/artist"
        )));
        assert_eq!(tag.track(), Some(1));
        // The RIFF INFO tag wasn't touched.
        assert_eq!(wav.riff_info().unwrap().get("IXYZ"), Some("kept"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn mp4_atoms_lofty_has_no_key_for_are_kept() {
        use lofty::mp4::{Atom, AtomData, AtomIdent, Mp4File};
        let dir = std::env::temp_dir().join(format!("coversync-mp4-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let (src, dest) = (dir.join("in.m4a"), dir.join("out.m4a"));
        fs::write(&src, include_bytes!("../tests/fixtures/silence.m4a")).unwrap();
        let mut ilst = lofty::mp4::Ilst::default();
        ilst.set_title("Song".into());
        ilst.insert(Atom::new(
            AtomIdent::Freeform {
                mean: "com.apple.iTunes".into(),
                name: "CUSTOM".into(),
            },
            AtomData::UTF8("kept".into()),
        ));
        ilst.save_to_path(&src, WriteOptions::default()).unwrap();
        let values = [("track_number", "1".to_string())];
        copy_with_tags(&src, &dest, &values, None, false).unwrap();
        let mut f = fs::File::open(&dest).unwrap();
        let mp4 = Mp4File::read_from(&mut f, ParseOptions::new()).unwrap();
        let ilst = mp4.ilst().unwrap();
        let custom = AtomIdent::Freeform {
            mean: "com.apple.iTunes".into(),
            name: "CUSTOM".into(),
        };
        let kept: Vec<&AtomData> = ilst.get(&custom).unwrap().data().collect();
        assert!(
            matches!(kept[..], [AtomData::UTF8(v)] if v == "kept"),
            "{kept:?}"
        );
        assert_eq!(ilst.title().as_deref(), Some("Song"));
        assert_eq!(ilst.track(), Some(1));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ape_items_covers_and_numbers_are_kept_as_written() {
        use lofty::ape::{ApeItem, ApeTag};
        let dir = std::env::temp_dir().join(format!("coversync-ape-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let (src, dest) = (dir.join("in.wv"), dir.join("out.wv"));
        fs::write(&src, include_bytes!("../tests/fixtures/silence.wv")).unwrap();
        let item = |k: &str, v: ItemValue| ApeItem::new(k.into(), v).unwrap();
        let mut ape = ApeTag::default();
        ape.insert(item("Title", ItemValue::Text("Song".into())));
        ape.insert(item("Track", ItemValue::Text("A1".into())));
        ape.insert(item("Ripper", ItemValue::Text("kept".into())));
        ape.insert(item(
            "Cover Art (Front)",
            ItemValue::Binary(b"front.jpg\0data".to_vec()),
        ));
        ape.insert(item("Compilation", ItemValue::Binary(vec![1])));
        ape.insert(item("Disc", ItemValue::Binary(vec![2])));
        ape.save_to_path(&src, WriteOptions::default()).unwrap();
        let values = [("title", "New".to_string())];
        copy_with_tags(&src, &dest, &values, None, false).unwrap();
        let mut f = fs::File::open(&dest).unwrap();
        let wv = lofty::wavpack::WavPackFile::read_from(&mut f, ParseOptions::new()).unwrap();
        let ape = wv.ape().unwrap();
        let text = |k: &str| match ape.get(k).map(|i| i.value()) {
            Some(ItemValue::Text(v)) => Some(v.as_str()),
            _ => None,
        };
        assert_eq!(text("Title"), Some("New"));
        assert_eq!(text("Track"), Some("A1"));
        assert_eq!(text("Ripper"), Some("kept"));
        assert!(matches!(
            ape.get("Disc").map(|i| i.value()),
            Some(ItemValue::Binary(b)) if b == &[2]
        ));
        assert!(matches!(
            ape.get("Compilation").map(|i| i.value()),
            Some(ItemValue::Binary(b)) if b == &[1]
        ));
        assert!(matches!(
            ape.get("Cover Art (Front)").map(|i| i.value()),
            Some(ItemValue::Binary(b)) if b == b"front.jpg\0data"
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    /// A FLAC file with no Vorbis comments at all, behind `id3`.
    fn flac_with_only_id3v2(path: &Path, id3: &lofty::id3::v2::Id3v2Tag) {
        let mut b = Vec::new();
        id3.dump_to(&mut b, WriteOptions::default()).unwrap();
        b.extend(b"fLaC");
        // STREAMINFO, the last block: 4096-sample blocks, 44.1 kHz, stereo, 16 bits.
        b.extend([
            0x80, 0x00, 0x00, 34, 0x10, 0x00, 0x10, 0x00, 0, 0, 0, 0, 0, 0,
        ]);
        let packed: u64 = (44100 << 44) | (1 << 41) | (15 << 36);
        b.extend(packed.to_be_bytes());
        b.extend([0u8; 16]);
        b.extend([0xFF, 0xF8, 0x69, 0x08, 0x00, 0x00, 0x00, 0x00]);
        fs::write(path, b).unwrap();
    }

    #[test]
    fn a_rating_from_another_tag_gets_its_providers_key() {
        use lofty::id3::v2::{Frame, Id3v2Tag, PopularimeterFrame};
        let dir = std::env::temp_dir().join(format!("coversync-popm-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = |name: &str| dir.join(name);
        let mut id3 = Id3v2Tag::new();
        id3.set_title("Song".into());
        // Two providers, and one whose name can't be a Vorbis field's.
        id3.insert(Frame::Popularimeter(PopularimeterFrame::new(
            "MusicBee", 196, 0,
        )));
        id3.insert(Frame::Popularimeter(PopularimeterFrame::new(
            "Windows Media Player 9 Series",
            255,
            0,
        )));
        id3.insert(Frame::Popularimeter(PopularimeterFrame::new(
            "a=b@example.org",
            128,
            0,
        )));
        id3.insert(Frame::Popularimeter(PopularimeterFrame::new(
            "a|b@example.org",
            128,
            2,
        )));
        flac_with_only_id3v2(&path("in.flac"), &id3);
        let genre = [("genre", "Anime".to_string())];
        copy_with_tags(&path("in.flac"), &path("out.flac"), &genre, None, false).unwrap();
        let (_, items) = vorbis(&path("out.flac"));
        let ratings: Vec<(&str, &str)> = items
            .iter()
            .filter(|(k, _)| k.starts_with("RATING"))
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        // MusicBee's four stars on its Vorbis scale, under its key; Windows
        // Media Player's on its own.
        assert!(ratings.contains(&("RATING:MusicBee", "80")), "{ratings:?}");
        assert!(
            ratings.contains(&("RATING:Windows Media Player 9 Series", "255")),
            "{ratings:?}"
        );
        // The third, as lofty read it (owner, stars, play count), under RATING.
        assert!(
            ratings.contains(&("RATING", "a=b@example.org|3|0")),
            "{ratings:?}"
        );
        // One with "|" in its owner, under its own key on the default scale.
        assert!(
            ratings.contains(&("RATING:a|b@example.org", "60")),
            "{ratings:?}"
        );
        assert_eq!(ratings.len(), 4, "{ratings:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(feature = "server")]
    fn two_icons_of_one_kind_leave_the_file_unchanged() {
        let dir = std::env::temp_dir().join(format!("coversync-icons-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = |name: &str| dir.join(name);
        flac_with_comments(&path("in.flac"), &["TITLE=Song"]);
        let icon = |edge: u32| {
            let mut png = std::io::Cursor::new(Vec::new());
            image::DynamicImage::new_rgb8(edge, edge)
                .write_to(&mut png, image::ImageFormat::Png)
                .unwrap();
            Picture::unchecked(png.into_inner())
                .pic_type(PictureType::Icon)
                .mime_type(lofty::picture::MimeType::Png)
                .build()
        };
        // Lofty's own writer puts both in.
        let mut file = probe(&path("in.flac"), true).unwrap();
        let tag = primary_tag_mut(&mut file);
        tag.push_picture(icon(2));
        tag.push_picture(icon(3));
        file.save_to_path(path("in.flac"), WriteOptions::default())
            .unwrap();
        let genre = [("genre", "Anime".to_string())];
        let refused = copy_with_tags(&path("in.flac"), &path("out.flac"), &genre, None, false);
        assert!(refused.unwrap_err().to_string().contains("two icons"));
        assert!(!path("out.flac").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(feature = "server")]
    fn unsynchronised_id3v2_frames_are_written_back_plainly() {
        use lofty::id3::v2::Frame;
        let dir = std::env::temp_dir().join(format!("coversync-unsync-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let (src, dest) = (dir.join("in.wav"), dir.join("out.wav"));
        wav_with_info(&src);
        // Both the tag and its PRIV frame are unsynchronised: FF 00 00 and
        // FF 00 E0 on disk decode to FF 00 and FF E0.
        let tag = b"ID3\x04\x00\x80\x00\x00\x00\x1c\
                    PRIV\x00\x00\x00\x12\x00\x02coversync\x00\x01\xff\x00\x00\xff\x00\xe0\x02";
        let mut wav = fs::read(&src).unwrap();
        wav.extend_from_slice(b"ID3 ");
        wav.extend_from_slice(&(tag.len() as u32).to_le_bytes());
        wav.extend_from_slice(tag);
        let riff_size = (wav.len() - 8) as u32;
        wav[4..8].copy_from_slice(&riff_size.to_le_bytes());
        fs::write(&src, wav).unwrap();
        let private = |path: &Path| {
            let mut f = fs::File::open(path).unwrap();
            let wav = lofty::iff::wav::WavFile::read_from(&mut f, ParseOptions::new()).unwrap();
            let id3 = wav.id3v2().unwrap();
            let data = id3.into_iter().find_map(|frame| match frame {
                Frame::Private(p) => Some(p.private_data.to_vec()),
                _ => None,
            });
            (data, id3.flags().unsynchronisation)
        };
        let (read, was) = private(&src);
        assert!(was, "the fixture is unsynchronised");
        let expected: &[u8] = &[1, 0xFF, 0x00, 0xFF, 0xE0, 2];
        assert_eq!(read.as_deref(), Some(expected));
        let values = [("track_number", "1".to_string())];
        copy_with_tags(&src, &dest, &values, None, false).unwrap();
        // The data as lofty read it, in a tag no longer unsynchronised.
        let (written, unsync) = private(&dest);
        assert_eq!(written, read);
        assert!(written.is_some());
        assert!(!unsync);
        assert_eq!(
            read_kinds(&dest, false).unwrap().values["track_number"],
            "1"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ratings_keep_their_order_whatever_the_case_of_their_names() {
        let dir = std::env::temp_dir().join(format!("coversync-rorder-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = |name: &str| dir.join(name);
        flac_with_comments(
            &path("in.flac"),
            &["TITLE=Song", "RATING:MusicBee=80", "rating:MusicBee=60"],
        );
        let genre = [("genre", "Anime".to_string())];
        copy_with_tags(&path("in.flac"), &path("out.flac"), &genre, None, false).unwrap();
        let (_, items) = vorbis(&path("out.flac"));
        let ratings: Vec<(&str, &str)> = items
            .iter()
            .filter(|(k, _)| k.to_ascii_uppercase().starts_with("RATING"))
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(
            ratings,
            [("RATING:MusicBee", "80"), ("rating:MusicBee", "60")]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn taking_ratings_out_leaves_the_other_comments_in_order() {
        let dir = std::env::temp_dir().join(format!("coversync-corder-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = |name: &str| dir.join(name);
        flac_with_comments(
            &path("in.flac"),
            &[
                "TITLE=Song",
                "UPC=111",
                "UPC=222",
                "rating:MusicBee=60",
                "=legacy",
                "CDTOC=1+96",
            ],
        );
        let genre = [("genre", "Anime".to_string())];
        copy_with_tags(&path("in.flac"), &path("out.flac"), &genre, None, false).unwrap();
        let (_, items) = vorbis(&path("out.flac"));
        let kept: Vec<(&str, &str)> = items
            .iter()
            .filter(|(k, _)| ["UPC", "", "CDTOC", "rating:MusicBee"].contains(&k.as_str()))
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(
            kept,
            [
                ("UPC", "111"),
                ("UPC", "222"),
                ("", "legacy"),
                ("CDTOC", "1+96"),
                ("rating:MusicBee", "60"),
            ]
        );
        // The blank that stood in for the rating isn't written.
        assert!(items.iter().all(|(_, v)| !v.is_empty()), "{items:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn clearing_the_rating_clears_every_rating() {
        let dir = std::env::temp_dir().join(format!("coversync-rclear-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = |name: &str| dir.join(name);
        flac_with_comments(
            &path("in.flac"),
            &["TITLE=Song", "RATING=80", "rating:MusicBee=60", "UPC=111"],
        );
        let clear = [("rating", String::new())];
        copy_with_tags(&path("in.flac"), &path("out.flac"), &clear, None, false).unwrap();
        let (_, items) = vorbis(&path("out.flac"));
        assert!(
            items
                .iter()
                .all(|(k, _)| !k.to_ascii_uppercase().starts_with("RATING")),
            "{items:?}"
        );
        assert!(has(&items, "UPC", "111"));
        let _ = fs::remove_dir_all(&dir);
    }

    /// `wav_with_info`'s WAV with `id3` as its ID3 chunk.
    #[cfg(feature = "server")]
    fn wav_with_id3_chunk(path: &Path, id3: &[u8]) {
        wav_with_info(path);
        let mut wav = fs::read(path).unwrap();
        wav.extend_from_slice(b"ID3 ");
        wav.extend_from_slice(&(id3.len() as u32).to_le_bytes());
        wav.extend_from_slice(id3);
        let riff_size = (wav.len() - 8) as u32;
        wav[4..8].copy_from_slice(&riff_size.to_le_bytes());
        fs::write(path, wav).unwrap();
    }

    #[test]
    #[cfg(feature = "server")]
    fn id3v2_frames_lofty_cant_write_back_leave_the_file_unchanged() {
        let dir = std::env::temp_dir().join(format!("coversync-id3refuse-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let values = [("track_number", "1".to_string())];
        // ID3v2.4: a PRIV frame with a data length indicator (flags 00 01),
        // its payload led by the four-byte length.
        let src = dir.join("dli.wav");
        wav_with_id3_chunk(
            &src,
            b"ID3\x04\x00\x00\x00\x00\x00\x1e\
              PRIV\x00\x00\x00\x14\x00\x01\x00\x00\x00\x10coversync\x00\x01\x02\x03\x04\x05\x06",
        );
        let refused = copy_with_tags(&src, &dir.join("dli-out.wav"), &values, None, false);
        assert!(refused.unwrap_err().to_string().contains("length-prefixed"));
        assert!(!dir.join("dli-out.wav").exists());
        // ID3v2.2: a title, and an XYZ frame with no ID3v2.4 name.
        let src = dir.join("v22.wav");
        wav_with_id3_chunk(
            &src,
            b"ID3\x02\x00\x00\x00\x00\x00\x14TT2\x00\x00\x05\x00SongXYZ\x00\x00\x03abc",
        );
        let refused = copy_with_tags(&src, &dir.join("v22-out.wav"), &values, None, false);
        assert!(refused.unwrap_err().to_string().contains("XYZ"));
        assert!(!dir.join("v22-out.wav").exists());
        // ID3v2.3: a chapter by times, holding an XYZ frame with no ID3v2.4
        // name.
        let src = dir.join("nested.wav");
        wav_with_id3_chunk(
            &src,
            b"ID3\x03\x00\x00\x00\x00\x00\x2b\
              CHAP\x00\x00\x00\x21\x00\x00ch0\x00\
              \x00\x00\x00\x00\x00\x00\x03\xe8\xff\xff\xff\xff\xff\xff\xff\xff\
              XYZ\x00\x00\x00\x00\x03\x00\x00abc",
        );
        let refused = copy_with_tags(&src, &dir.join("nested-out.wav"), &values, None, false);
        assert!(refused.unwrap_err().to_string().contains("XYZ"));
        assert!(!dir.join("nested-out.wav").exists());
        // ID3v2.4: a chapter by byte offsets (0 to 100).
        let src = dir.join("offsets.wav");
        wav_with_id3_chunk(
            &src,
            b"ID3\x04\x00\x00\x00\x00\x00\x1e\
              CHAP\x00\x00\x00\x14\x00\x00ch0\x00\
              \x00\x00\x00\x00\x00\x00\x03\xe8\x00\x00\x00\x00\x00\x00\x00\x64",
        );
        let refused = copy_with_tags(&src, &dir.join("offsets-out.wav"), &values, None, false);
        assert!(refused.unwrap_err().to_string().contains("byte offsets"));
        assert!(!dir.join("offsets-out.wav").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(feature = "server")]
    fn a_chapter_by_times_is_kept() {
        use lofty::id3::v2::Frame;
        let dir = std::env::temp_dir().join(format!("coversync-chap-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let (src, dest) = (dir.join("in.wav"), dir.join("out.wav"));
        // ID3v2.4: chapter ch0, 0 to 1000 ms, no byte offsets, titled Intro.
        wav_with_id3_chunk(
            &src,
            b"ID3\x04\x00\x00\x00\x00\x00\x2e\
              CHAP\x00\x00\x00\x24\x00\x00ch0\x00\
              \x00\x00\x00\x00\x00\x00\x03\xe8\xff\xff\xff\xff\xff\xff\xff\xff\
              TIT2\x00\x00\x00\x06\x00\x00\x03Intro",
        );
        let values = [("track_number", "1".to_string())];
        copy_with_tags(&src, &dest, &values, None, false).unwrap();
        let mut f = fs::File::open(&dest).unwrap();
        let wav = lofty::iff::wav::WavFile::read_from(&mut f, ParseOptions::new()).unwrap();
        let chapter = wav
            .id3v2()
            .unwrap()
            .into_iter()
            .find_map(|frame| match frame {
                Frame::Chapter(c) => Some(c.clone()),
                _ => None,
            })
            .expect("the chapter");
        assert_eq!(chapter.id, "ch0");
        assert_eq!(chapter.times, 0..1000);
        assert_eq!(chapter.offsets, u32::MAX..u32::MAX);
        let title = chapter.children.iter().find_map(|f| match f {
            Frame::Text(t) => Some(t.value.to_string()),
            _ => None,
        });
        assert_eq!(title.as_deref(), Some("Intro"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(feature = "server")]
    fn id3v2_frames_lofty_has_no_key_for_are_kept() {
        use lofty::id3::v2::{Frame, Id3v2Tag, PrivateFrame};
        let dir = std::env::temp_dir().join(format!("coversync-id3-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let (src, dest) = (dir.join("in.wav"), dir.join("out.wav"));
        wav_with_info(&src);
        // WAV's primary tag: ID3v2, with a user text frame and a private one.
        let mut id3 = Id3v2Tag::new();
        id3.set_title("Song".into());
        id3.insert_user_text("CUSTOM".into(), "kept".into());
        id3.insert(Frame::Private(PrivateFrame::new(
            "coversync",
            vec![1, 2, 3],
        )));
        id3.save_to_path(&src, WriteOptions::default()).unwrap();
        let values = [("track_number", "1".to_string())];
        copy_with_tags(&src, &dest, &values, None, false).unwrap();
        let mut f = fs::File::open(&dest).unwrap();
        let wav = lofty::iff::wav::WavFile::read_from(&mut f, ParseOptions::new()).unwrap();
        let tag = wav.id3v2().unwrap();
        assert_eq!(tag.get_user_text("CUSTOM"), Some("kept"));
        assert!(tag.into_iter().any(|frame| matches!(
            frame,
            Frame::Private(private)
                if private.owner == "coversync" && private.private_data.as_ref() == [1, 2, 3]
        )));
        assert_eq!(tag.title().as_deref(), Some("Song"));
        assert_eq!(tag.track(), Some(1));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn numbers() {
        assert_eq!(parse_number("t", "3/12").unwrap(), Some(3));
        assert_eq!(parse_number("t", " ").unwrap(), None);
        assert!(parse_number("t", "x").is_err());
    }
}
