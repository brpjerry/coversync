//! Reading and writing audio file tags through lofty.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::Path;

use lofty::config::{ParseOptions, WriteOptions};
use lofty::file::{FileType, TaggedFile};
use lofty::ogg::tag::VorbisComments;
use lofty::picture::{Picture, PictureType};
use lofty::prelude::*;
use lofty::probe::Probe;
use lofty::tag::items::Timestamp;
use lofty::tag::{ItemValue, SplitTag, Tag, TagItem, TagType};
use serde::Serialize;

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

/// The tag reader's version, kept with each index: bumped when what reading
/// a file gives changes, so the next scan reads again the files
/// [`reader_changed`] names. 1: ID3v2 comments and lyrics with a
/// description (`iTunNORM`) aren't the comment, or the lyrics. 2: the same
/// for FLACs with an ID3v2 tag, which 1 left out.
pub const READER: u32 = 2;

/// Whether what reading `path` gives has changed since reader `since`: for
/// files that can hold ID3v2 tags, since 1 (a FLAC only if it starts with
/// one, or can't be opened to tell; since 2); Ogg files can't.
pub fn reader_changed(path: &Path, since: u32) -> bool {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    match ext.as_str() {
        "ogg" | "oga" | "opus" | "spx" => false,
        "flac" => {
            use std::io::Read;
            let mut head = [0u8; 3];
            since < 2
                && fs::File::open(path)
                    .and_then(|mut f| f.read_exact(&mut head))
                    .map_or(true, |()| &head == b"ID3")
        }
        _ => since < 1,
    }
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
    for (id, value) in edit_values(edit) {
        validate_kind(id, &value)?;
    }
    Ok(())
}

/// `edit`'s fields as tag kinds (catalog ids) with their values: written
/// the way any kind is ([`apply_kinds`]).
fn edit_values(edit: &TagEdit) -> Vec<(&'static str, String)> {
    [
        ("title", &edit.title),
        ("artist", &edit.artist),
        ("album", &edit.album),
        ("album_artist", &edit.album_artist),
        ("track_number", &edit.track_no),
        ("disc_number", &edit.disc_no),
        ("date", &edit.year),
        ("genre", &edit.genre),
    ]
    .into_iter()
    .filter_map(|(id, value)| value.clone().map(|v| (id, v)))
    .collect()
}

/// `edit`'s fields and `kinds` as one edit: a kind given both ways takes
/// its value from `kinds` (applied after the fields), so only what's
/// written has to fit the format.
fn merged<'a>(edit: &TagEdit, kinds: &[(&'a str, String)]) -> Result<Vec<(&'a str, String)>> {
    let mut all: Vec<(&'a str, String)> = edit_values(edit).into_iter().collect();
    for (id, value) in kinds {
        let id = rules::kind(id).map_or(*id, |k| k.id);
        // A field given another way still had to be one.
        for (_, displaced) in all.iter().filter(|(x, _)| *x == id) {
            validate_kind(id, displaced)?;
        }
        all.retain(|(x, _)| *x != id);
        all.push((id, value.clone()));
    }
    Ok(all)
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
    /// Whether the file has a comment block (a FLAC needn't).
    had_block: bool,
    /// Whether the file had ENCODER comments of its own.
    had_encoder: bool,
    /// The file's RATING comments, provider-qualified ones (`RATING:MusicBee`)
    /// included, as written and in their order (blanked in `rest`): coversync
    /// only ever clears a rating, and lofty can't write a provider's back the
    /// way it read it.
    ratings: Vec<(String, String)>,
}

/// A FLAC or Ogg file's Vorbis comments as written, without pictures, and
/// whether the file has a comment block (a FLAC needn't); None for a file
/// whose primary tag isn't Vorbis comments.
fn vorbis_comments(path: &Path, file_type: FileType) -> Result<Option<(VorbisComments, bool)>> {
    use lofty::file::AudioFile;
    if !matches!(
        file_type,
        FileType::Flac | FileType::Vorbis | FileType::Opus | FileType::Speex
    ) {
        return Ok(None);
    }
    // Without implicit conversions: splitting TRACKNUMBER=1/10 removes a
    // TOTALTRACKS alias in a way that reorders the other comments.
    let options = ParseOptions::new()
        .read_properties(false)
        .read_cover_art(false)
        .implicit_conversions(false);
    let mut f = std::io::BufReader::new(fs::File::open(path)?);
    let tag = match file_type {
        FileType::Flac => {
            let comments = lofty::flac::FlacFile::read_from(&mut f, options)
                .map_err(tag_err)?
                .remove_vorbis_comments();
            let had = comments.is_some();
            return Ok(Some((comments.unwrap_or_default(), had)));
        }
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
    Ok(Some((tag, true)))
}

/// The file's [`VorbisRest`]; None for a file whose primary tag isn't
/// Vorbis comments.
fn vorbis_rest(path: &Path, file_type: FileType) -> Result<Option<VorbisRest>> {
    // Pictures go through `Tag`: not read here, so not kept twice.
    let Some((tag, had_block)) = vorbis_comments(path, file_type)? else {
        return Ok(None);
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
    // Those lofty couldn't read too, so all go back in their order. The
    // ones left here are blanked rather than removed: lofty's `remove`
    // reorders the other comments, and its `push` refuses a nameless one
    // (`=legacy`) a rebuilt list would need; `insert` keeps the others'
    // order, and an empty value isn't written.
    let keys: std::collections::HashSet<String> = rest
        .items()
        .filter(|(k, _)| is_rating(k))
        .map(|(k, _)| k.to_ascii_uppercase())
        .collect();
    for key in keys {
        rest.insert(key, String::new());
    }
    Ok(Some(VorbisRest {
        rest,
        vendor,
        had_block,
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
    let had_block = rest.had_block;
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
    // TRACKNUM is an older name readers fall back to for the track number:
    // blanked (so not written) when the track number is written or edited,
    // as lofty's own setter does, so a cleared number doesn't come back.
    // A canonical number that isn't one (empty, or a vinyl side like A1)
    // doesn't stand in for it.
    if out.get("TRACKNUM").is_some()
        && (lofty::tag::Accessor::track(&tag).is_some() || edited.contains(&"track_number"))
    {
        out.insert("TRACKNUM".into(), String::new());
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
    // Lofty writes no comment block, so no vendor string, for a tag with
    // no comments (an edit that cleared them all): a blank one, which isn't
    // written itself, keeps the file's. A FLAC that had none gets none, so
    // its ID3v2 tag still shows.
    if had_block && out.items().len() == 0 {
        out.push("COMMENT".into(), String::new());
    }
    out.save_to_path(path, WriteOptions::default())
        .map_err(tag_err)
}

/// Why lofty wouldn't write the ID3v2 `frame` back as it read it, if it
/// wouldn't: an ID3v2.2 frame with no ID3v2.4 name (dropped); a
/// compressed, encrypted or length-prefixed one (it consumes the prefix on
/// reading but keeps the flag); a chapter or seek index (ASPI) located by
/// byte offsets, which a resized tag moves away from its audio; a second
/// root table of contents in one frame list (lofty writes the first);
/// or, inside a chapter or table of contents, any of those or a frame
/// flagged as unsynchronised (decoded on reading, written as it is).
/// `seen_root`: whether the frame list had a root table of contents yet.
fn unwritable(
    frame: &lofty::id3::v2::Frame<'_>,
    nested: bool,
    seen_root: &mut bool,
) -> Option<String> {
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
    if id == "ASPI" {
        return Some(
            "a seek index (ASPI) located by byte offsets, which a resized tag would leave pointing at the wrong audio".into(),
        );
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
        Frame::TableOfContents(t) => {
            if t.flags.top_level && std::mem::replace(seen_root, true) {
                return Some(format!(
                    "a second root table of contents ({}), of which only one is written",
                    t.id
                ));
            }
            &t.children
        }
        _ => return None,
    };
    let mut seen = false;
    children.iter().find_map(|f| unwritable(f, true, &mut seen))
}

/// A tag in its own format, ready to write.
enum Native {
    Id3v2(lofty::id3::v2::Id3v2Tag),
    Ilst(lofty::mp4::Ilst),
    Ape(lofty::ape::ApeTag),
    Other(Tag),
}

/// `tag`, a primary tag other than Vorbis comments after an edit, in its
/// format as it will be written, through lofty's owned conversion
/// (refused, as the save is, where it can't be): unlike its writer for a borrowed
/// generic tag, it puts back what the generic tag couldn't hold (custom
/// APE items and covers, unknown ID3v2 frames, freeform MP4 atoms, URL
/// frames) and joins repeated values as the format wants.
fn native(tag: Tag) -> Result<Native> {
    Ok(match tag.tag_type() {
        TagType::Id3v2 => {
            let mut id3 = lofty::id3::v2::Id3v2Tag::from(tag);
            // A frame lofty wouldn't write back as it read it: not saved
            // rather than lost or corrupted.
            let mut seen_root = false;
            if let Some(why) = (&id3)
                .into_iter()
                .find_map(|f| unwritable(f, false, &mut seen_root))
            {
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
            Native::Id3v2(id3)
        }
        TagType::Mp4Ilst => Native::Ilst(lofty::mp4::Ilst::from(tag)),
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
            Native::Ape(ape)
        }
        // RIFF INFO, AIFF text chunks: as lofty writes them.
        _ => Native::Other(tag),
    })
}

/// Write `tag`, a primary tag other than Vorbis comments after an edit, as
/// [`native`] makes it.
fn save_owned(path: &Path, tag: Tag) -> Result<()> {
    let options = WriteOptions::default();
    match native(tag)? {
        Native::Id3v2(t) => t.save_to_path(path, options),
        Native::Ilst(t) => t.save_to_path(path, options),
        Native::Ape(t) => t.save_to_path(path, options),
        Native::Other(t) => t.save_to_path(path, options),
    }
    .map_err(tag_err)
}

/// A tag block's items, as (key, value) text in the file's own naming.
type Block = (TagType, Vec<(String, String)>);

/// What each of the file's tag blocks holds that lofty's generic [`Tag`]
/// has no key for, as (key, value) text in the file's own naming; binary
/// ones by their size. For showing: an edit keeps them (see [`save`]).
/// `comments`: a FLAC or Ogg file's Vorbis comments, as written; `blocks`:
/// the tag blocks the file has (a FLAC is read again only for an ID3v2 one).
fn unmapped_items(
    path: &Path,
    file_type: FileType,
    comments: Option<VorbisComments>,
    blocks: &[TagType],
) -> Result<Vec<Block>> {
    use lofty::file::AudioFile;
    use lofty::id3::v2::{Frame, Id3v2Tag};
    use lofty::mp4::{AtomData, AtomIdent};
    // As the writer reads them (ID3v2.3's TYER and TDAT as the date, an MP4
    // gnre atom as the genre), so what it converts isn't listed again; FLAC
    // and Ogg comments are read as written.
    let options = ParseOptions::new()
        .read_properties(false)
        .read_cover_art(false);
    // Opened only for a native read.
    macro_rules! read {
        ($file:ty) => {
            <$file>::read_from(&mut std::io::BufReader::new(fs::File::open(path)?), options)
                .map_err(tag_err)?
        };
    }
    let bytes = |n: usize| format!("({n} bytes of data)");
    let id3v2 = |tag: &Id3v2Tag| -> Block {
        let rest = tag.clone().split_tag().0;
        let items = (&*rest)
            .into_iter()
            .map(|frame| {
                let id = frame.id_str().to_string();
                match frame {
                    Frame::Text(f) => (id, f.value.to_string()),
                    Frame::UserText(f) => {
                        (format!("{id}:{}", f.description), f.content.to_string())
                    }
                    Frame::Comment(f) => (format!("{id}:{}", f.description), f.content.to_string()),
                    Frame::UnsynchronizedText(f) => {
                        (format!("{id}:{}", f.description), f.content.to_string())
                    }
                    Frame::Url(f) => (id, f.url().to_string()),
                    Frame::UserUrl(f) => (format!("{id}:{}", f.description), f.content.to_string()),
                    Frame::UniqueFileIdentifier(f) => {
                        (format!("{id}:{}", f.owner), printable(&f.identifier))
                    }
                    Frame::Private(f) => (format!("{id}:{}", f.owner), bytes(f.private_data.len())),
                    // Involved people (TIPL, TMCL) lofty has no role for.
                    Frame::KeyValue(f) => (
                        id,
                        f.key_value_pairs
                            .iter()
                            .map(|(role, who)| format!("{role}: {who}"))
                            .collect::<Vec<_>>()
                            .join("; "),
                    ),
                    Frame::Binary(f) => (id, bytes(f.data.len())),
                    _ => (id, "(data)".to_string()),
                }
            })
            .collect();
        (TagType::Id3v2, items)
    };
    let ape = |tag: &lofty::ape::ApeTag| -> Block {
        let rest = tag.clone().split_tag().0;
        let items = (&*rest)
            .into_iter()
            .map(|item| (item.key().to_string(), value_text(item.value())))
            .collect();
        (TagType::Ape, items)
    };
    let vorbis = |tag: VorbisComments| -> Block {
        // As read: ratings lofty can't read included.
        let items = VorbisComments::from(tag.split_tag().0)
            .items()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        (TagType::VorbisComments, items)
    };
    let mut out = Vec::new();
    match file_type {
        FileType::Flac => {
            out.extend(comments.map(vorbis));
            // Only a FLAC with an ID3v2 tag is read again for it.
            if blocks.contains(&TagType::Id3v2) {
                out.extend(read!(lofty::flac::FlacFile).id3v2().map(id3v2));
            }
        }
        FileType::Vorbis | FileType::Opus | FileType::Speex => out.extend(comments.map(vorbis)),
        FileType::Mpeg => {
            let mpeg = read!(lofty::mpeg::MpegFile);
            out.extend(mpeg.id3v2().map(id3v2));
            out.extend(mpeg.ape().map(ape));
        }
        FileType::Aac => out.extend(read!(lofty::aac::AacFile).id3v2().map(id3v2)),
        FileType::Aiff => out.extend(read!(lofty::iff::aiff::AiffFile).id3v2().map(id3v2)),
        FileType::Wav => {
            let wav = read!(lofty::iff::wav::WavFile);
            out.extend(wav.id3v2().map(id3v2));
            if let Some(info) = wav.riff_info() {
                let items = info
                    .into_iter()
                    .filter(|(k, _)| ItemKey::from_key(TagType::RiffInfo, k).is_none())
                    .cloned()
                    .collect();
                out.push((TagType::RiffInfo, items));
            }
        }
        FileType::Ape => {
            let file = read!(lofty::ape::ApeFile);
            out.extend(file.ape().map(ape));
            out.extend(file.id3v2().map(id3v2));
        }
        FileType::WavPack => out.extend(read!(lofty::wavpack::WavPackFile).ape().map(ape)),
        FileType::Mpc => {
            let file = read!(lofty::musepack::MpcFile);
            out.extend(file.ape().map(ape));
            out.extend(file.id3v2().map(id3v2));
        }
        FileType::Mp4 => {
            if let Some(ilst) = read!(lofty::mp4::Mp4File).ilst() {
                let rest = ilst.clone().split_tag().0;
                let items = (&*rest)
                    .into_iter()
                    .map(|atom| {
                        let key = match atom.ident() {
                            // Byte codes: ©nam's first byte is 0xA9, not UTF-8.
                            AtomIdent::Fourcc(code) => code.iter().map(|b| *b as char).collect(),
                            AtomIdent::Freeform { mean, name } => format!("----:{mean}:{name}"),
                        };
                        let values: Vec<String> = atom
                            .data()
                            .map(|d| match d {
                                AtomData::UTF8(t) | AtomData::UTF16(t) => t.clone(),
                                AtomData::SignedInteger(n) => n.to_string(),
                                AtomData::UnsignedInteger(n) => n.to_string(),
                                AtomData::Bool(b) => b.to_string(),
                                AtomData::Picture(p) => bytes(p.data().len()),
                                AtomData::Unknown { data, .. } => bytes(data.len()),
                            })
                            .collect();
                        (key, values.join("; "))
                    })
                    .collect();
                out.push((TagType::Mp4Ilst, items));
            }
        }
        _ => {}
    }
    Ok(out)
}

/// Bytes as text if they are printable UTF-8, else as hex.
fn printable(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) if !text.chars().any(char::is_control) => text.to_string(),
        _ => bytes.iter().map(|b| format!("{b:02x}")).collect(),
    }
}

/// An item's value as text; binary ones by their size.
fn value_text(value: &ItemValue) -> String {
    match value {
        ItemValue::Text(t) | ItemValue::Locator(t) => t.clone(),
        ItemValue::Binary(b) => format!("({} bytes of data)", b.len()),
    }
}

/// A tag block's name, before its items in the read-only list.
fn block_name(t: TagType) -> &'static str {
    match t {
        TagType::Id3v2 => "ID3v2",
        TagType::Id3v1 => "ID3v1",
        TagType::Ape => "APE",
        TagType::VorbisComments => "Vorbis comments",
        TagType::Mp4Ilst => "MP4",
        TagType::RiffInfo => "RIFF INFO",
        TagType::AiffText => "AIFF text",
        _ => "Other tag",
    }
}

/// A file's tags as the tag editor shows them.
#[derive(Debug, Clone, Default, Serialize)]
pub struct FileTags {
    /// Every catalog kind the file has, by id; one held several times has
    /// its values joined by the Library's separator, as an import writes them.
    pub values: BTreeMap<&'static str, String>,
    /// Everything else, as (key, value) in the file's own naming, which an
    /// edit keeps but can't change: tags no kind names, ones held as data or
    /// with a description (`COMM:iTunNORM`), and the file's other tag blocks
    /// (ID3v2 in a FLAC, RIFF INFO beside ID3v2), each key after its
    /// block's name (`RIFF INFO · IXYZ`).
    pub other: Vec<(String, String)>,
}

/// The file's tags for the tag editor (see [`FileTags`]). Pictures and the
/// audio stream aren't read.
pub fn file_tags(path: &Path, separator: &str) -> Result<FileTags> {
    let file = Probe::open(path)
        .map_err(tag_err)?
        .options(
            ParseOptions::new()
                .read_properties(false)
                .read_cover_art(false),
        )
        .read()
        .map_err(tag_err)?;
    let mut out = FileTags::default();
    let Some(tag) = file.primary_tag().or_else(|| file.first_tag()) else {
        return Ok(out);
    };
    let shown = tag.tag_type();
    let editable = |i: &TagItem| !described(i) && !matches!(i.value(), ItemValue::Binary(_));
    for kind in rules::CATALOG {
        let mut values: Vec<&str> = Vec::new();
        for key in kind.keys {
            let earlier = values.len();
            for item in tag.get_items(*key).filter(|i| editable(i)) {
                let (ItemValue::Text(v) | ItemValue::Locator(v)) = item.value() else {
                    continue;
                };
                let v = v.trim();
                // Under another of the kind's names too: once.
                if !v.is_empty() && !values[..earlier].contains(&v) {
                    values.push(v);
                }
            }
        }
        if !values.is_empty() {
            out.values.insert(kind.id, values.join(separator));
        }
    }
    // FLAC and Ogg comments as written, read once.
    let comments = vorbis_comments(path, file.file_type())?.map(|(raw, _)| raw);
    // Lofty reads a Vorbis vendor string as the encoder when there's no
    // ENCODER comment: that isn't a tag the file has (an edit keeps the
    // vendor string apart).
    if shown == TagType::VorbisComments
        && !comments
            .as_ref()
            .is_some_and(|raw| raw.get("ENCODER").is_some())
    {
        out.values.remove("encoder");
    }
    let covered: Vec<ItemKey> = rules::CATALOG
        .iter()
        .flat_map(|k| k.keys.iter().copied())
        .collect();
    let native = |t: TagType, i: &TagItem| {
        let key = i
            .key()
            .map_key(t)
            .map_or_else(|| format!("{:?}", i.key()), str::to_string);
        if described(i) {
            format!("{key}:{}", i.description())
        } else {
            key
        }
    };
    // What lofty knows by a key no kind covers, or holds as data or with a
    // description…
    for item in tag
        .items()
        .filter(|i| !covered.contains(&i.key()) || !editable(i))
    {
        out.other
            .push((native(shown, item), value_text(item.value())));
    }
    // …and what it has no key for.
    let present: Vec<TagType> = file.tags().iter().map(|t| t.tag_type()).collect();
    let mut blocks = unmapped_items(path, file.file_type(), comments, &present)?;
    if let Some(at) = blocks.iter().position(|(t, _)| *t == shown) {
        out.other.extend(blocks.remove(at).1);
    }
    // The file's other tag blocks, whole.
    for block in file.tags().iter().filter(|t| t.tag_type() != shown) {
        let t = block.tag_type();
        let name = block_name(t);
        for item in block.items() {
            out.other.push((
                format!("{name} · {}", native(t, item)),
                value_text(item.value()),
            ));
        }
        if let Some(at) = blocks.iter().position(|(b, _)| *b == t) {
            for (key, value) in blocks.remove(at).1 {
                out.other.push((format!("{name} · {key}"), value));
            }
        }
    }
    Ok(out)
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

/// The tag kinds (catalog ids) `edit` sets or clears.
fn edit_kinds(edit: &TagEdit) -> Vec<&'static str> {
    edit_values(edit).into_iter().map(|(id, _)| id).collect()
}

/// Apply `edit` to the file's primary tag. The change is written to a copy
/// beside the file and renamed over it, so a failed write leaves the original.
pub fn write(path: &Path, edit: &TagEdit) -> Result<()> {
    write_with_kinds(path, edit, &[])
}

/// As [`write`], then set tag kinds (catalog ids) as an import does; an
/// empty value removes the kind.
pub fn write_with_kinds(path: &Path, edit: &TagEdit, kinds: &[(&str, String)]) -> Result<()> {
    // Keep the real extension last: lofty picks the format from it.
    let name = path.file_name().ok_or(Error::NotFound)?.to_string_lossy();
    let tmp = path.with_file_name(format!(".coversync-tmp-{name}"));
    fs::copy(path, &tmp)?;
    let result = (|| {
        let mut file = probe(&tmp, true)?;
        let rest = vorbis_rest(&tmp, file.file_type())?;
        let tag = primary_tag_mut(&mut file);
        apply_kinds(tag, &merged(edit, kinds)?)?;
        let mut edited = edit_kinds(edit);
        edited.extend(kinds.iter().map(|(id, _)| *id));
        save(&tmp, file, rest, &[], &edited)?;
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
    // As written: lofty's number conversions can reorder two UPCs.
    let options = ParseOptions::new()
        .read_properties(false)
        .read_cover_art(false)
        .implicit_conversions(false);
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

/// Whether an MP3's APE tag has a front or untyped cover of its own (lofty
/// keeps APE covers as items, not pictures, so they aren't in its generic
/// tag) other than `new`, the image replacing it. Such a cover isn't
/// replaced: lofty can't rewrite every APE layout safely (a tag with only a
/// footer loses 32 bytes of audio). An empty one, or one already `new`,
/// doesn't count.
fn secondary_ape_cover(path: &Path, new: &[u8]) -> Result<bool> {
    use lofty::file::AudioFile;
    let options = ParseOptions::new().read_properties(false);
    let mut f = std::io::BufReader::new(fs::File::open(path)?);
    let mpeg = lofty::mpeg::MpegFile::read_from(&mut f, options).map_err(tag_err)?;
    let Some(ape) = mpeg.ape() else {
        return Ok(false);
    };
    Ok(["Cover Art (Front)", "Cover Art (Other)"]
        .iter()
        .filter_map(|k| ape.get(k))
        .any(|item| match item.value() {
            // A file name, a zero byte, then the image.
            ItemValue::Binary(bytes) => {
                let image = bytes
                    .iter()
                    .position(|b| *b == 0)
                    .map_or(&bytes[..], |at| &bytes[at + 1..]);
                !image.is_empty() && image != new
            }
            ItemValue::Text(text) | ItemValue::Locator(text) => !text.is_empty(),
        }))
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
            .flat_map(|k| tag.get_items(*k))
            .filter(|i| !described(i))
            .filter_map(|i| match i.value() {
                ItemValue::Text(t) | ItemValue::Locator(t) => Some(t.trim()),
                ItemValue::Binary(_) => None,
            })
            .find(|v| !v.is_empty())
        {
            out.insert(kind.id, v.to_string());
        }
    }
    out
}

/// Whether `read` (lofty's reading of the date `typed`) has every part of
/// it: the numbers typed, the year first and then two digits a part where
/// they run together (20100825), are the ones read, in order.
fn read_whole(typed: &str, read: &Timestamp) -> bool {
    // Each part in its range: ID3v2 writes a month 13 but reads it back as
    // no date at all.
    let in_range = [
        read.month.is_none_or(|m| (1..=12).contains(&m)),
        read.day.is_none_or(|d| (1..=31).contains(&d)),
        read.hour.is_none_or(|h| h <= 23),
        read.minute.is_none_or(|m| m <= 59),
        read.second.is_none_or(|s| s <= 59),
    ];
    if in_range.contains(&false) {
        return false;
    }
    // Anything but numbers, separators, T and Z (a PM, say) isn't read.
    if !typed
        .trim()
        .chars()
        .all(|c| c.is_ascii_digit() || "-.: TZ".contains(c))
    {
        return false;
    }
    let mut typed_parts: Vec<u32> = Vec::new();
    let runs = typed
        .split(|c: char| !c.is_ascii_digit())
        .filter(|r| !r.is_empty());
    for (n, run) in runs.enumerate() {
        let mut rest = run;
        if n == 0 {
            let (year, after) = run.split_at(run.len().min(4));
            typed_parts.push(year.parse().unwrap_or(u32::MAX));
            rest = after;
        } else if run.len() <= 2 {
            typed_parts.push(run.parse().unwrap_or(u32::MAX));
            continue;
        }
        while !rest.is_empty() {
            let (part, after) = rest.split_at(rest.len().min(2));
            typed_parts.push(part.parse().unwrap_or(u32::MAX));
            rest = after;
        }
    }
    let read_parts: Vec<u32> = [
        Some(u32::from(read.year)),
        read.month.map(u32::from),
        read.day.map(u32::from),
        read.hour.map(u32::from),
        read.minute.map(u32::from),
        read.second.map(u32::from),
    ]
    .into_iter()
    .map_while(|part| part)
    .collect();
    typed_parts == read_parts
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
            let wrong = || {
                Error::Tags(format!(
                    "{label}: '{v}' should be a year like 2003, or a date like 2003-04-01"
                ))
            };
            let read = v.parse::<Timestamp>().map_err(|_| wrong())?;
            // Lofty stops at what it can't read (a time after a space): a
            // date it reads only part of would lose the rest when written.
            if !read_whole(v, &read) {
                return Err(wrong());
            }
        }
        _ if rules::NOT_EDITABLE.contains(&id) => {
            return Err(Error::Tags(format!("{label} can't be entered as text")));
        }
        _ => {}
    }
    Ok(())
}

/// Whether `item` is an ID3v2 comment or lyrics with a description
/// (`iTunNORM`, say): a tag of its own, not the field's.
fn described(item: &TagItem) -> bool {
    !item.description().is_empty()
}

/// Set tag kinds (catalog ids) on `tag`; an empty value removes the kind.
/// A value goes under the name the file has the kind under, or failing
/// that the first this format holds; one the format would drop on writing
/// (an MP4 compilation flag of "yes") is refused.
fn apply_kinds(tag: &mut Tag, values: &[(&str, String)]) -> Result<()> {
    // The kinds given values, to check they're all still there as written.
    let mut set: Vec<(&'static rules::TagKind, String)> = Vec::new();
    for (id, value) in values {
        let kind =
            rules::kind(id).ok_or_else(|| Error::Invalid(format!("unknown tag kind '{id}'")))?;
        validate_kind(id, value)?;
        let v = value.trim();
        // A value held as data (a binary APE item) isn't the field's text,
        // and isn't typed over.
        let field = |i: &TagItem| {
            kind.keys.contains(&i.key())
                && !described(i)
                && !matches!(i.value(), ItemValue::Binary(_))
        };
        if v.is_empty() {
            tag.retain(|i| !field(i));
            continue;
        }
        // Numbers and dates go through lofty's typed setters so each format
        // stores them natively (MP4 keeps track numbers as integers).
        let number = || parse_number(kind.label, v).map(|n| n.unwrap_or_default());
        // A value held as data isn't typed over (a binary APE Year).
        let data = |k: &ItemKey| {
            tag.items()
                .any(|i| i.key() == *k && matches!(i.value(), ItemValue::Binary(_)))
        };
        let held_as_data = || {
            Error::Tags(format!(
                "{}: the file holds it as data, which can't be typed over",
                kind.label
            ))
        };
        let typed = matches!(
            *id,
            "track_number" | "track_total" | "disc_number" | "disc_total" | "date"
        );
        if typed && kind.keys.iter().any(data) {
            return Err(held_as_data());
        }
        let unstorable = || {
            Error::Tags(format!(
                "{} can't be stored in this file's tag format",
                kind.label
            ))
        };
        match *id {
            "track_number" => tag.set_track(number()?),
            "track_total" => tag.set_track_total(number()?),
            "disc_number" => tag.set_disk(number()?),
            "disc_total" => tag.set_disk_total(number()?),
            "date" => tag.set_date(v.parse().map_err(|_| Error::Tags(format!("date: '{v}'")))?),
            _ => {
                // Plain ones in several languages (an English and a Japanese
                // comment) are more than one field can hold.
                let mut langs: Vec<[u8; 3]> = tag
                    .items()
                    .filter(|i| field(i))
                    .map(|i| *i.lang())
                    .collect();
                langs.dedup();
                if langs.len() > 1 {
                    return Err(Error::Tags(format!(
                        "{}: the file has it in several languages, which one field can't hold, so it wasn't changed",
                        kind.label
                    )));
                }
                let old = tag.items().find(|i| field(i)).cloned();
                // A name the format holds and no data item uses.
                let taken = |k: ItemKey| {
                    tag.items()
                        .any(|i| i.key() == k && matches!(i.value(), ItemValue::Binary(_)))
                };
                let key = old
                    .as_ref()
                    .map(TagItem::key)
                    .or_else(|| {
                        kind.keys
                            .iter()
                            .copied()
                            .find(|k| k.map_key(tag.tag_type()).is_some() && !taken(*k))
                    })
                    .ok_or_else(|| {
                        if kind.keys.iter().any(|k| taken(*k)) {
                            held_as_data()
                        } else {
                            unstorable()
                        }
                    })?;
                tag.retain(|i| !field(i));
                // A URL stays one (ID3v2's WOAR), a comment keeps its language.
                let value = match old.as_ref().map(TagItem::value) {
                    Some(ItemValue::Locator(_)) => ItemValue::Locator(v.to_string()),
                    _ => ItemValue::Text(v.to_string()),
                };
                let mut item = TagItem::new(key, value);
                if let Some(old) = &old {
                    item.set_lang(*old.lang());
                }
                if !tag.push(item) {
                    return Err(unstorable());
                }
            }
        }
        set.push((kind, v.to_string()));
    }
    // Lofty builds ID3v2's MVIN from the movement items in order: the total
    // goes after the number, however they were set. (Moved by copying and
    // the order-keeping `remove_key`: `take` reorders the other items.)
    if tag.tag_type() == TagType::Id3v2 {
        let totals: Vec<TagItem> = tag.get_items(ItemKey::MovementTotal).cloned().collect();
        if !totals.is_empty() {
            tag.remove_key(ItemKey::MovementTotal);
            for total in totals {
                tag.push(total);
            }
        }
    }
    // Together, as they'll be written: two kinds can share one field (Label
    // and Publisher are both ID3v2's TPUB).
    if let Some((kind, v)) = lost(tag, &set) {
        return Err(Error::Tags(format!(
            "{}: '{v}' can't be stored in this file's tag format{}",
            kind.label,
            if set.len() > 1 {
                " beside the other tags set"
            } else {
                ""
            }
        )));
    }
    Ok(())
}

/// The number kind a total kind goes with.
fn number_of(total: &str) -> Option<&'static str> {
    match total {
        "track_total" => Some("track_number"),
        "disc_total" => Some("disc_number"),
        "movement_total" => Some("movement_number"),
        _ => None,
    }
}

/// Which of `set` (kinds just given values on `tag`) isn't there once the
/// tag is converted to its format and encoded as it will be written
/// ([`native`]). Each kind's value is first converted alone: what comes back
/// is what the format makes of it (`2010.08.25` as `2010-08-25`, ID3v2's
/// genre `17` as `Rock`, Label as ID3v2's publisher frame), and nothing
/// back means the format drops it (a compilation flag that isn't 0, 1,
/// true or false; an MP4 advisory of 3; a track number past MP4's 65535; a
/// month 13 ID3v2 can't encode). Then all of them together must give each
/// of those back: two kinds can land on one field (Label and Publisher are
/// both TPUB). Vorbis comments hold any text as it is.
fn lost(
    tag: &Tag,
    set: &[(&'static rules::TagKind, String)],
) -> Option<(&'static rules::TagKind, String)> {
    if tag.tag_type() == TagType::VorbisComments || set.is_empty() {
        return None;
    }
    // A tag of these kinds' items (and `also`, more of the file's keys),
    // converted and encoded: None if refused.
    let back =
        |kinds: &[&'static rules::TagKind], also: &[ItemKey]| -> Option<Vec<(ItemKey, String)>> {
            let mut only = Tag::new(tag.tag_type());
            for item in tag.items().filter(|i| {
                kinds.iter().any(|k| k.keys.contains(&i.key())) || also.contains(&i.key())
            }) {
                only.push(item.clone());
            }
            let mut sink = std::io::sink();
            let options = WriteOptions::default();
            let converted: Tag = match native(only).ok()? {
                Native::Id3v2(t) => {
                    t.dump_to(&mut sink, options).ok()?;
                    t.into()
                }
                Native::Ilst(t) => {
                    t.dump_to(&mut sink, options).ok()?;
                    t.into()
                }
                Native::Ape(t) => {
                    t.dump_to(&mut sink, options).ok()?;
                    t.into()
                }
                Native::Other(t) => t,
            };
            Some(
                converted
                    .items()
                    .filter_map(|i| match i.value() {
                        ItemValue::Text(t) | ItemValue::Locator(t) => {
                            Some((i.key(), t.trim().to_string()))
                        }
                        ItemValue::Binary(_) => None,
                    })
                    .collect(),
            )
        };
    let number = |kind: &rules::TagKind, t: &str| parse_number(kind.label, t).ok().flatten();
    let id3v2 = tag.tag_type() == TagType::Id3v2;
    let mut alone = Vec::new();
    for (kind, value) in set {
        // ID3v2 reads its date and time frames as timestamps: one it reads
        // only part of (a time after a space; TBD) loses the rest. Its URL
        // frames hold Latin-1 only.
        if id3v2
            && kind.id != "date"
            && kind.keys.iter().any(|k| {
                matches!(
                    k.map_key(TagType::Id3v2),
                    Some("TDRC" | "TDRL" | "TDOR" | "TDTG" | "TDEN" | "TDLY")
                )
            })
            && !value
                .parse::<Timestamp>()
                .is_ok_and(|read| read_whole(value, &read))
        {
            return Some((kind, value.clone()));
        }
        if id3v2 && kind.id.ends_with("_url") && value.chars().any(|c| u32::from(c) > 0xFF) {
            return Some((kind, value.clone()));
        }
        // A total goes with its number, as they're written (ID3v2's MVIN,
        // MP4's trkn): alone, it would land in the number's place.
        let with: Vec<&'static rules::TagKind> = std::iter::once(*kind)
            .chain(number_of(kind.id).and_then(rules::kind))
            .collect();
        let items = match back(&with, &[]) {
            Some(items) if !items.is_empty() => items,
            _ => return Some((kind, value.clone())),
        };
        let numeric = matches!(
            kind.id,
            "track_number"
                | "track_total"
                | "disc_number"
                | "disc_total"
                | "movement_number"
                | "movement_total"
        );
        if numeric
            && !items
                .iter()
                .any(|(k, t)| kind.keys.contains(k) && number(kind, t) == number(kind, value))
        {
            return Some((kind, value.clone()));
        }
        // What it came back as under its own names; under another's only
        // when the format has it there (Label as Publisher). Not the other
        // half of a pair it was converted without (a disc total's number).
        let own: Vec<(ItemKey, String)> = items
            .iter()
            .filter(|(k, _)| kind.keys.contains(k))
            .cloned()
            .collect();
        if numeric && own.is_empty() {
            return Some((kind, value.clone()));
        }
        alone.push((*kind, value, if own.is_empty() { items } else { own }));
    }
    // The file's other tags that land in the same field (ID3v2's TSSE holds
    // Encoder and Encoder settings): together, one may push out another.
    let field = |key: ItemKey| key.map_key(tag.tag_type());
    let fields: Vec<&str> = set
        .iter()
        .flat_map(|(k, _)| k.keys.iter().filter_map(|key| field(*key)))
        .collect();
    let mut sharing: Vec<ItemKey> = Vec::new();
    for item in tag.items() {
        let key = item.key();
        if !set.iter().any(|(k, _)| k.keys.contains(&key))
            && field(key).is_some_and(|f| fields.contains(&f))
            && !sharing.contains(&key)
        {
            sharing.push(key);
        }
    }
    // One kind, alone in its field: converted alone is converted together.
    if set.len() == 1 && sharing.is_empty() {
        return None;
    }
    // Each total with its number (one the file has, if it isn't set too).
    let mut kinds: Vec<&'static rules::TagKind> = set.iter().map(|(k, _)| *k).collect();
    for number in set
        .iter()
        .filter_map(|(k, _)| number_of(k.id).and_then(rules::kind))
    {
        if !kinds.iter().any(|k| k.id == number.id) {
            kinds.push(number);
        }
    }
    let Some(together) = back(&kinds, &sharing) else {
        return set.first().cloned();
    };
    alone
        .into_iter()
        .find(|(_, _, items)| !items.iter().all(|item| together.contains(item)))
        .map(|(kind, value, _)| (kind, value.clone()))
}

/// Check, before anything is written, that `kinds` can be set on the file
/// at `path` as it is: everything a save would refuse (a kind its format
/// has no field for, a value it would drop, a field held as data, a frame
/// that can't be written back) is refused, so an album of mixed formats or
/// files isn't left half edited. Pictures and the audio stream aren't read.
pub fn check_kinds(path: &Path, edit: &TagEdit, kinds: &[(&str, String)]) -> Result<()> {
    let mut file = Probe::open(path)
        .map_err(tag_err)?
        .options(
            ParseOptions::new()
                .read_properties(false)
                .read_cover_art(false),
        )
        .read()
        .map_err(tag_err)?;
    apply_kinds(primary_tag_mut(&mut file), &merged(edit, kinds)?)?;
    let primary = file.primary_tag_type();
    if primary != TagType::VorbisComments
        && let Some(tag) = file.remove(primary)
    {
        // Encoded whole: a frame the file already has that can't be written
        // (an ID3v2 timestamp of month 13) fails here, not on saving.
        let mut sink = std::io::sink();
        let options = WriteOptions::default();
        match native(tag)? {
            Native::Id3v2(t) => t.dump_to(&mut sink, options),
            Native::Ilst(t) => t.dump_to(&mut sink, options),
            Native::Ape(t) => t.dump_to(&mut sink, options),
            Native::Other(t) => t.dump_to(&mut sink, options),
        }
        .map_err(tag_err)?;
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
                    if file_type == FileType::Mpeg
                        && file.tag(TagType::Ape).is_some()
                        && let Some(new) = art
                        && secondary_ape_cover(&tmp, new.data())?
                    {
                        return Err(Error::Tags(format!(
                            "{}: the MP3's APE tag has a cover of its own, which can't be replaced safely, so it wasn't changed",
                            src.display()
                        )));
                    }
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
        // As written: no number conversions.
        let options = ParseOptions::new().implicit_conversions(false);
        let flac = lofty::flac::FlacFile::read_from(&mut f, options).unwrap();
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
                "TRACKTOTAL=10",
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
    #[cfg(feature = "server")]
    fn an_mp3_whose_ape_tag_has_a_cover_keeps_its_covers() {
        use lofty::ape::{ApeItem, ApeTag};
        use lofty::id3::v2::Id3v2Tag;
        let dir = std::env::temp_dir().join(format!("coversync-mp3ape-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let (src, dest) = (dir.join("in.mp3"), dir.join("out.mp3"));
        fs::write(&src, include_bytes!("../tests/fixtures/silence.mp3")).unwrap();
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
        // ID3v2 with a front cover; APE with a front cover, a back cover and
        // an item of its own.
        let mut generic = Tag::new(TagType::Id3v2);
        generic.set_title("Song".into());
        generic.push_picture(cover(4));
        Id3v2Tag::from(generic)
            .save_to_path(&src, WriteOptions::default())
            .unwrap();
        let item = |k: &str, v: ItemValue| ApeItem::new(k.into(), v).unwrap();
        let mut ape = ApeTag::default();
        ape.insert(item(
            "Cover Art (Front)",
            ItemValue::Binary(b"front.jpg\0old".to_vec()),
        ));
        ape.insert(item(
            "Cover Art (Back)",
            ItemValue::Binary(b"back.jpg\0back".to_vec()),
        ));
        ape.insert(item("Ripper", ItemValue::Text("kept".into())));
        ape.save_to_path(&src, WriteOptions::default()).unwrap();
        let new = cover(8);
        let refused = copy_with_tags(&src, &dest, &[], Some(&new), true)
            .unwrap_err()
            .to_string();
        assert!(refused.contains("APE tag has a cover"), "{refused}");
        assert!(refused.contains("in.mp3"), "{refused}");
        assert!(!dest.exists());
        // One that's empty, or already the new image, has nothing to replace.
        let mut same = b"front.png\0".to_vec();
        same.extend_from_slice(new.data());
        for front in [b"front.jpg\0".to_vec(), same] {
            let mut ape = ApeTag::default();
            ape.insert(item("Cover Art (Front)", ItemValue::Binary(front)));
            ape.save_to_path(&src, WriteOptions::default()).unwrap();
            assert!(copy_with_tags(&src, &dest, &[], Some(&new), true).unwrap());
            assert_eq!(embedded_cover(&dest).unwrap().unwrap(), new.data());
            fs::remove_file(&dest).unwrap();
        }
        // An APE tag with no cover (ReplayGain, say) doesn't stand in the
        // way, and stays as it was.
        let mut ape = ApeTag::default();
        ape.insert(item(
            "REPLAYGAIN_TRACK_GAIN",
            ItemValue::Text("-6.1 dB".into()),
        ));
        ape.insert(item("Ripper", ItemValue::Text("kept".into())));
        ape.save_to_path(&src, WriteOptions::default()).unwrap();
        assert!(copy_with_tags(&src, &dest, &[], Some(&new), true).unwrap());
        assert_eq!(embedded_cover(&dest).unwrap().unwrap(), new.data());
        let mut f = fs::File::open(&dest).unwrap();
        let mpeg = lofty::mpeg::MpegFile::read_from(&mut f, ParseOptions::new()).unwrap();
        let ape = mpeg.ape().unwrap();
        assert!(matches!(
            ape.get("Ripper").map(|i| i.value()),
            Some(ItemValue::Text(t)) if t == "kept"
        ));
        assert!(ape.get("REPLAYGAIN_TRACK_GAIN").is_some());
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
        // Both the tag and its PRIV frame are unsynchronised: FF 00 00 and
        // FF 00 E0 on disk decode to FF 00 and FF E0.
        let tag = b"ID3\x04\x00\x80\x00\x00\x00\x1c\
                    PRIV\x00\x00\x00\x12\x00\x02coversync\x00\x01\xff\x00\x00\xff\x00\xe0\x02";
        wav_with_id3_chunk(&src, tag);
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
    fn number_pairs_dont_reorder_the_other_comments() {
        let dir = std::env::temp_dir().join(format!("coversync-npair-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = |name: &str| dir.join(name);
        flac_with_comments(
            &path("in.flac"),
            // In this order lofty's alias removal would swap the barcodes.
            &[
                "UPC=111",
                "UPC=222",
                "TOTALTRACKS=10",
                "TRACKNUMBER=1/10",
                "TITLE=Song",
            ],
        );
        let genre = [("genre", "Anime".to_string())];
        copy_with_tags(&path("in.flac"), &path("out.flac"), &genre, None, false).unwrap();
        let (_, items) = vorbis(&path("out.flac"));
        let upc: Vec<&str> = items
            .iter()
            .filter(|(k, _)| k == "UPC")
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(upc, ["111", "222"]);
        // Identify reads the same barcode from the file before and after.
        assert_eq!(super::upc(&path("in.flac")).as_deref(), Some("111"));
        assert_eq!(super::upc(&path("out.flac")).as_deref(), Some("111"));
        // The numbers as coversync writes them, the alias under its usual name.
        assert!(has(&items, "TRACKNUMBER", "1"));
        assert!(has(&items, "TRACKTOTAL", "10"));
        assert!(!items.iter().any(|(k, _)| k == "TOTALTRACKS"), "{items:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cleared_track_number_doesnt_come_back_from_its_old_name() {
        let dir = std::env::temp_dir().join(format!("coversync-tracknum-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = |name: &str| dir.join(name);
        flac_with_comments(
            &path("in.flac"),
            &["TITLE=Song", "TRACKNUM=2", "TRACKNUMBER=1", "UPC=111"],
        );
        // An unrelated edit: the number under its usual name only.
        let genre = [("genre", "Anime".to_string())];
        copy_with_tags(&path("in.flac"), &path("edit.flac"), &genre, None, false).unwrap();
        let (_, items) = vorbis(&path("edit.flac"));
        assert!(has(&items, "TRACKNUMBER", "1"));
        assert!(!items.iter().any(|(k, _)| k == "TRACKNUM"), "{items:?}");
        // Next to a number that isn't one, the old name stays.
        for (n, canonical) in ["TRACKNUMBER=A1", "TRACKNUMBER="].iter().enumerate() {
            flac_with_comments(
                &path(&format!("vinyl{n}.flac")),
                &["TITLE=Song", "TRACKNUM=2", *canonical],
            );
            copy_with_tags(
                &path(&format!("vinyl{n}.flac")),
                &path(&format!("vinyl{n}-out.flac")),
                &genre,
                None,
                false,
            )
            .unwrap();
            let (_, items) = vorbis(&path(&format!("vinyl{n}-out.flac")));
            assert!(has(&items, "TRACKNUM", "2"), "{canonical}: {items:?}");
        }
        // Cleared: gone under both names.
        let clear = [("track_number", String::new())];
        copy_with_tags(&path("in.flac"), &path("clear.flac"), &clear, None, false).unwrap();
        let (_, items) = vorbis(&path("clear.flac"));
        assert!(
            !items.iter().any(|(k, _)| k.starts_with("TRACKNUM")),
            "{items:?}"
        );
        assert!(has(&items, "UPC", "111"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_flac_with_only_id3v2_gets_no_empty_comment_block() {
        use lofty::id3::v2::Id3v2Tag;
        let dir = std::env::temp_dir().join(format!("coversync-noblock-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = |name: &str| dir.join(name);
        // Only a length (TLEN), which Vorbis comments have no field for.
        let mut generic = Tag::new(TagType::Id3v2);
        generic.insert_text(ItemKey::Length, "1000".into());
        let id3: Id3v2Tag = generic.into();
        flac_with_only_id3v2(&path("in.flac"), &id3);
        assert_eq!(
            read_kinds(&path("in.flac"), false).unwrap().values["length"],
            "1000"
        );
        // Clearing a genre it doesn't have.
        let clear = [("genre", String::new())];
        copy_with_tags(&path("in.flac"), &path("out.flac"), &clear, None, false).unwrap();
        let mut f = fs::File::open(path("out.flac")).unwrap();
        let flac = lofty::flac::FlacFile::read_from(&mut f, ParseOptions::new()).unwrap();
        assert!(flac.vorbis_comments().is_none());
        assert_eq!(
            read_kinds(&path("out.flac"), false).unwrap().values["length"],
            "1000"
        );
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
        // A file whose only comment was its rating keeps its comment block,
        // vendor string and all.
        flac_with_comments(&path("only.flac"), &["RATING=80"]);
        copy_with_tags(
            &path("only.flac"),
            &path("only-out.flac"),
            &clear,
            None,
            false,
        )
        .unwrap();
        let (vendor, items) = vorbis(&path("only-out.flac"));
        assert_eq!(vendor, "coversync");
        assert!(items.is_empty(), "{items:?}");
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
        // RIFF pads an odd-sized chunk; its size doesn't count the pad.
        if id3.len() % 2 == 1 {
            wav.push(0);
        }
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
        // ID3v2.4: a seek index (ASPI) with no index points.
        let src = dir.join("aspi.wav");
        wav_with_id3_chunk(
            &src,
            b"ID3\x04\x00\x00\x00\x00\x00\x15\
              ASPI\x00\x00\x00\x0b\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x08",
        );
        let refused = copy_with_tags(&src, &dir.join("aspi-out.wav"), &values, None, false);
        assert!(refused.unwrap_err().to_string().contains("ASPI"));
        assert!(!dir.join("aspi-out.wav").exists());
        // ID3v2.4: two root tables of contents, each empty.
        let src = dir.join("ctoc.wav");
        wav_with_id3_chunk(
            &src,
            b"ID3\x04\x00\x00\x00\x00\x00\x22\
              CTOC\x00\x00\x00\x07\x00\x00toc1\x00\x03\x00\
              CTOC\x00\x00\x00\x07\x00\x00toc2\x00\x03\x00",
        );
        let refused = copy_with_tags(&src, &dir.join("ctoc-out.wav"), &values, None, false);
        assert!(
            refused
                .unwrap_err()
                .to_string()
                .contains("second root table of contents (toc2)")
        );
        assert!(!dir.join("ctoc-out.wav").exists());
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
    fn the_editor_sees_every_kind_and_the_tags_no_kind_names() {
        let dir = std::env::temp_dir().join(format!("coversync-filetags-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.flac");
        flac_with_comments(
            &path,
            &[
                "TITLE=Song",
                "ARTISTS=Rin",
                "ARTISTS=Riho",
                "LABEL=Lantis",
                "TRACKTOTAL=13",
                "ALBUM_JP=アルバム",
                "CDTOC=1+96",
                "rating:MusicBee=60",
            ],
        );
        let tags = file_tags(&path, "; ").unwrap();
        assert_eq!(tags.values["title"], "Song");
        // Held twice: both, joined as an import writes them.
        assert_eq!(tags.values["artists"], "Rin; Riho");
        assert_eq!(tags.values["label"], "Lantis");
        assert_eq!(tags.values["track_total"], "13");
        assert!(!tags.values.contains_key("album"));
        let other: Vec<(&str, &str)> = tags
            .other
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert!(other.contains(&("ALBUM_JP", "アルバム")), "{other:?}");
        assert!(other.contains(&("CDTOC", "1+96")), "{other:?}");
        // A rating lofty can't read, as written; no blanks.
        assert!(other.contains(&("rating:MusicBee", "60")), "{other:?}");
        assert!(other.iter().all(|(_, v)| !v.is_empty()), "{other:?}");
        // Nothing a kind names is "other".
        assert!(
            !other.iter().any(|(k, _)| *k == "LABEL" || *k == "TITLE"),
            "{other:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_edit_sets_any_kind_and_keeps_the_rest() {
        let dir = std::env::temp_dir().join(format!("coversync-kindedit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.flac");
        flac_with_comments(
            &path,
            &["TITLE=Song", "LABEL=Lantis", "GENRE=Idol", "TITLE_JP=歌"],
        );
        let kinds = [
            ("label", "Lantis Company, Limited".to_string()),
            ("track_total", "13".to_string()),
            ("genre", String::new()),
        ];
        let edit = TagEdit {
            title: Some("Song!".into()),
            ..Default::default()
        };
        write_with_kinds(&path, &edit, &kinds).unwrap();
        let tags = file_tags(&path, "; ").unwrap();
        assert_eq!(tags.values["title"], "Song!");
        assert_eq!(tags.values["label"], "Lantis Company, Limited");
        assert_eq!(tags.values["track_total"], "13");
        // An empty value removes the kind.
        assert!(!tags.values.contains_key("genre"));
        assert!(has(&vorbis(&path).1, "TITLE_JP", "歌"));
        // A bad value writes nothing.
        let bad = [("track_total", "thirteen".to_string())];
        assert!(write_with_kinds(&path, &TagEdit::default(), &bad).is_err());
        assert_eq!(file_tags(&path, "; ").unwrap().values["track_total"], "13");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_editor_doesnt_take_the_vendor_for_an_encoder() {
        let dir = std::env::temp_dir().join(format!("coversync-noenc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = |name: &str| dir.join(name);
        // The fixture's vendor string is "coversync".
        flac_with_comments(&path("plain.flac"), &["TITLE=Song"]);
        let tags = file_tags(&path("plain.flac"), "; ").unwrap();
        assert!(!tags.values.contains_key("encoder"), "{:?}", tags.values);
        flac_with_comments(&path("enc.flac"), &["TITLE=Song", "ENCODER=flac 1.4"]);
        assert_eq!(
            file_tags(&path("enc.flac"), "; ").unwrap().values["encoder"],
            "flac 1.4"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// An MP3 with an ID3v2 tag holding a title, an artist URL (WOAR), a
    /// plain comment, an iTunes one with a description, and a TIPL role
    /// lofty has no key for; and an APE tag of its own.
    #[cfg(feature = "server")]
    fn mp3_with_comments(path: &Path) {
        use lofty::TextEncoding;
        use lofty::id3::v2::{CommentFrame, Frame, FrameId, Id3v2Tag, KeyValueFrame, UrlLinkFrame};
        fs::write(path, include_bytes!("../tests/fixtures/silence.mp3")).unwrap();
        let mut id3 = Id3v2Tag::new();
        id3.set_title("Song".into());
        id3.insert(Frame::Url(UrlLinkFrame::new(
            FrameId::new("WOAR").unwrap(),
            "https://example.org/rin",
        )));
        id3.insert(Frame::Comment(CommentFrame::new(
            TextEncoding::UTF8,
            *b"eng",
            String::new(),
            "Nice",
        )));
        id3.insert(Frame::Comment(CommentFrame::new(
            TextEncoding::UTF8,
            *b"eng",
            "iTunNORM",
            "0000ABCD",
        )));
        let roles: Vec<(std::borrow::Cow<str>, std::borrow::Cow<str>)> =
            vec![("ukulele".into(), "Rin".into())];
        id3.insert(Frame::KeyValue(KeyValueFrame::new(
            FrameId::new("TIPL").unwrap(),
            TextEncoding::UTF8,
            roles,
        )));
        id3.save_to_path(path, WriteOptions::default()).unwrap();
        let mut ape = lofty::ape::ApeTag::default();
        ape.insert(
            lofty::ape::ApeItem::new("Ripper".into(), ItemValue::Text("kept".into())).unwrap(),
        );
        ape.save_to_path(path, WriteOptions::default()).unwrap();
    }

    #[test]
    #[cfg(feature = "server")]
    fn the_editor_reads_every_block_and_what_lofty_holds_as_data() {
        use lofty::TextEncoding;
        use lofty::ape::{ApeItem, ApeTag};
        use lofty::id3::v2::{ExtendedTextFrame, Frame, Id3v2Tag};
        use lofty::mp4::{Atom, AtomData, AtomIdent};
        let dir = std::env::temp_dir().join(format!("coversync-blocks-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = |name: &str| dir.join(name);
        let other = |tags: &FileTags| -> Vec<(String, String)> { tags.other.clone() };
        let has_other = |tags: &FileTags, k: &str, v: &str| {
            tags.other.iter().any(|(key, value)| key == k && value == v)
        };

        // MP3: a URL is a value; a described comment, an unknown role and
        // the APE tag are read-only.
        mp3_with_comments(&path("a.mp3"));
        let tags = file_tags(&path("a.mp3"), "; ").unwrap();
        assert_eq!(tags.values["artist_url"], "https://example.org/rin");
        assert_eq!(tags.values["comment"], "Nice");
        assert!(
            has_other(&tags, "COMM:iTunNORM", "0000ABCD"),
            "{:?}",
            other(&tags)
        );
        assert!(
            has_other(&tags, "TIPL", "ukulele: Rin"),
            "{:?}",
            other(&tags)
        );
        assert!(
            has_other(&tags, "APE · Ripper", "kept"),
            "{:?}",
            other(&tags)
        );

        // WAV: RIFF INFO beside the ID3v2 tag the editor shows.
        wav_with_info(&path("a.wav"));
        let mut f = fs::File::open(path("a.wav")).unwrap();
        let mut wav = lofty::iff::wav::WavFile::read_from(&mut f, ParseOptions::new()).unwrap();
        drop(f);
        let mut info = wav.remove_riff_info().unwrap();
        info.insert("IXYZ".into(), "custom".into());
        info.save_to_path(path("a.wav"), WriteOptions::default())
            .unwrap();
        let mut id3 = Id3v2Tag::new();
        id3.set_title("Song".into());
        id3.save_to_path(path("a.wav"), WriteOptions::default())
            .unwrap();
        let tags = file_tags(&path("a.wav"), "; ").unwrap();
        assert!(
            has_other(&tags, "RIFF INFO · IXYZ", "custom"),
            "{:?}",
            other(&tags)
        );
        assert!(
            has_other(&tags, "RIFF INFO · INAM", "Song"),
            "{:?}",
            other(&tags)
        );

        // FLAC: an ID3v2 tag in front of the Vorbis comments, read-only.
        flac_with_comments(&path("plain.flac"), &["TITLE=Song"]);
        let mut id3 = Id3v2Tag::new();
        id3.insert(Frame::UserText(ExtendedTextFrame::new(
            TextEncoding::UTF8,
            "coversync test",
            "custom",
        )));
        let mut bytes = Vec::new();
        id3.dump_to(&mut bytes, WriteOptions::default()).unwrap();
        bytes.extend(fs::read(path("plain.flac")).unwrap());
        fs::write(path("id3.flac"), bytes).unwrap();
        let tags = file_tags(&path("id3.flac"), "; ").unwrap();
        assert_eq!(tags.values["title"], "Song");
        assert!(
            has_other(&tags, "ID3v2 · TXXX:coversync test", "custom"),
            "{:?}",
            other(&tags)
        );

        // M4A: a © code read byte for byte.
        fs::write(
            path("a.m4a"),
            include_bytes!("../tests/fixtures/silence.m4a"),
        )
        .unwrap();
        let mut ilst = lofty::mp4::Ilst::default();
        ilst.set_title("Song".into());
        ilst.insert(Atom::new(
            AtomIdent::Fourcc(*b"\xa9xyz"),
            AtomData::UTF8("x".into()),
        ));
        ilst.save_to_path(path("a.m4a"), WriteOptions::default())
            .unwrap();
        let tags = file_tags(&path("a.m4a"), "; ").unwrap();
        assert!(has_other(&tags, "\u{a9}xyz", "x"), "{:?}", other(&tags));

        // WavPack: a binary compilation flag isn't a value to type over.
        fs::write(path("a.wv"), include_bytes!("../tests/fixtures/silence.wv")).unwrap();
        let mut ape = ApeTag::default();
        ape.insert(ApeItem::new("Title".into(), ItemValue::Text("Song".into())).unwrap());
        ape.insert(ApeItem::new("Compilation".into(), ItemValue::Binary(vec![1])).unwrap());
        ape.save_to_path(path("a.wv"), WriteOptions::default())
            .unwrap();
        let tags = file_tags(&path("a.wv"), "; ").unwrap();
        assert!(!tags.values.contains_key("compilation"));
        assert!(
            tags.other
                .iter()
                .any(|(k, v)| k == "Compilation" && v == "(1 bytes of data)"),
            "{:?}",
            other(&tags)
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(feature = "server")]
    fn an_edit_writes_a_kind_where_the_file_has_it() {
        let dir = std::env::temp_dir().join(format!("coversync-kindkey-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = |name: &str| dir.join(name);
        let none = TagEdit::default();

        // FLAC: lyrics edited where they are (UNSYNCEDLYRICS), not added
        // beside them.
        flac_with_comments(&path("a.flac"), &["TITLE=Song", "UNSYNCEDLYRICS=old"]);
        write_with_kinds(&path("a.flac"), &none, &[("lyrics", "new".to_string())]).unwrap();
        let (_, items) = vorbis(&path("a.flac"));
        let lyrics: Vec<&(String, String)> =
            items.iter().filter(|(k, _)| k.contains("LYRICS")).collect();
        assert_eq!(lyrics, [&("UNSYNCEDLYRICS".to_string(), "new".to_string())]);

        // MP3: the plain comment changes, the described one stays; lyrics
        // go where ID3v2 has them; a URL stays a URL.
        mp3_with_comments(&path("a.mp3"));
        let kinds = [
            ("comment", "Better".to_string()),
            ("lyrics", "La la".to_string()),
            ("artist_url", "https://example.org/riho".to_string()),
        ];
        write_with_kinds(&path("a.mp3"), &none, &kinds).unwrap();
        let tags = file_tags(&path("a.mp3"), "; ").unwrap();
        assert_eq!(tags.values["comment"], "Better");
        assert_eq!(tags.values["lyrics"], "La la");
        assert_eq!(tags.values["artist_url"], "https://example.org/riho");
        assert!(
            tags.other
                .contains(&("COMM:iTunNORM".into(), "0000ABCD".into()))
        );
        // Clearing the comment leaves the described one too.
        write_with_kinds(&path("a.mp3"), &none, &[("comment", String::new())]).unwrap();
        let tags = file_tags(&path("a.mp3"), "; ").unwrap();
        assert!(!tags.values.contains_key("comment"));
        assert!(
            tags.other
                .contains(&("COMM:iTunNORM".into(), "0000ABCD".into()))
        );

        // M4A: a compilation flag MP4 can't hold is refused, not dropped.
        fs::write(
            path("a.m4a"),
            include_bytes!("../tests/fixtures/silence.m4a"),
        )
        .unwrap();
        let yes = [("compilation", "yes".to_string())];
        let refused = write_with_kinds(&path("a.m4a"), &none, &yes).unwrap_err();
        assert!(refused.to_string().contains("Compilation"), "{refused}");
        write_with_kinds(&path("a.m4a"), &none, &[("compilation", "1".to_string())]).unwrap();
        assert!(
            file_tags(&path("a.m4a"), "; ")
                .unwrap()
                .values
                .contains_key("compilation")
        );

        // Checked before writing: a kind the format has no field for.
        let publisher = [("publisher", "Lantis".to_string())];
        assert!(check_kinds(&path("a.m4a"), &TagEdit::default(), &publisher).is_err());
        assert!(check_kinds(&path("a.flac"), &TagEdit::default(), &publisher).is_ok());
        let _ = fs::remove_dir_all(&dir);
    }

    /// Dates the editor's form and the server agree on (see tag-form.test.ts).
    const DATES_READ_WHOLE: &[&str] = &[
        "2010",
        "2010-08",
        "2010-08-25",
        "2010.08.25",
        "2010.8.25",
        "20100825",
        "201008",
        "2010-08-25T12",
        "2010-08-25T12:30:45",
        "20100825T1230",
        "2010.08.25T12:30",
        "2010-08-25Z",
        "2010-08-25T",
        "2024-6-3T14:8:49",
        "2024- 6-03",
        "2024-6-03",
        "2024-06-03T1:02",
        "2024- 6",
        "2024-06- 3",
        "2024.6.03",
        "2024-06-03T 1:02",
        "2024-06-03T14:08:49Z",
        "2024-6-03Z",
        "202406",
        "2024-6 -03",
        "2024-06-03T1 :02",
        "2024-6 Z",
        "20240603T 1",
        "20240603T14 8",
        "2024 6 3",
        "2024 6",
    ];
    const DATES_READ_IN_PART: &[&str] = &[
        "2010-08-25 12:30",
        "2010-8-5",
        "2010/08/25",
        "2010-08-25T1230",
        "2010-08-25T12.30",
        "10-08-25",
        "summer",
        "2024-6",
        "2024-6-3",
        "2024-06-3",
        "2024-06-03T01:2",
        "2024-6-03T14:08:4",
        "2024-06 -03",
        "2010-08-25T01:30 PM",
        "2024 6 03",
    ];

    #[test]
    fn dates_are_refused_unless_lofty_reads_every_part() {
        for date in DATES_READ_WHOLE {
            assert!(validate_kind("date", date).is_ok(), "{date}");
        }
        for date in DATES_READ_IN_PART {
            assert!(validate_kind("date", date).is_err(), "{date}");
        }
    }

    #[test]
    #[cfg(feature = "server")]
    fn ape_items_held_as_data_are_left_and_literal_values_kept() {
        use lofty::ape::{ApeItem, ApeTag};
        let dir = std::env::temp_dir().join(format!("coversync-apedata-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = |name: &str| dir.join(name);
        let item = |k: &str, v: ItemValue| ApeItem::new(k.into(), v).unwrap();
        let none = TagEdit::default();
        fs::write(path("a.wv"), include_bytes!("../tests/fixtures/silence.wv")).unwrap();
        let mut ape = ApeTag::default();
        ape.insert(item("Title", ItemValue::Text("Song".into())));
        ape.insert(item("UnsyncedLyrics", ItemValue::Binary(vec![1, 2, 3])));
        ape.insert(item("Track", ItemValue::Binary(vec![4])));
        ape.save_to_path(path("a.wv"), WriteOptions::default())
            .unwrap();
        // Lyrics go under their other name; the data item stays.
        write_with_kinds(&path("a.wv"), &none, &[("lyrics", "La la".to_string())]).unwrap();
        // A literal compilation flag, as APE writes it.
        write_with_kinds(&path("a.wv"), &none, &[("compilation", "yes".to_string())]).unwrap();
        let mut f = fs::File::open(path("a.wv")).unwrap();
        let wv = lofty::wavpack::WavPackFile::read_from(&mut f, ParseOptions::new()).unwrap();
        let ape = wv.ape().unwrap();
        assert!(matches!(
            ape.get("UnsyncedLyrics").map(|i| i.value()),
            Some(ItemValue::Binary(b)) if b == &[1, 2, 3]
        ));
        assert!(matches!(
            ape.get("Lyrics").map(|i| i.value()),
            Some(ItemValue::Text(t)) if t == "La la"
        ));
        assert!(matches!(
            ape.get("Compilation").map(|i| i.value()),
            Some(ItemValue::Text(t)) if t == "yes"
        ));
        // A date where a data item holds it: refused, by kind or by field.
        let mut ape = lofty::wavpack::WavPackFile::read_from(
            &mut fs::File::open(path("a.wv")).unwrap(),
            ParseOptions::new(),
        )
        .unwrap()
        .remove_ape()
        .unwrap();
        ape.insert(item("Year", ItemValue::Binary(vec![9])));
        ape.save_to_path(path("a.wv"), WriteOptions::default())
            .unwrap();
        let date = [("date", "2010".to_string())];
        assert!(
            write_with_kinds(&path("a.wv"), &none, &date)
                .unwrap_err()
                .to_string()
                .contains("as data")
        );
        let year = TagEdit {
            year: Some("2010".into()),
            ..Default::default()
        };
        assert!(write(&path("a.wv"), &year).is_err());
        // A track number where a data item holds the number: refused before
        // anything is written.
        let three = [("track_number", "3".to_string())];
        assert!(check_kinds(&path("a.wv"), &TagEdit::default(), &three).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(feature = "server")]
    fn mp3_comments_dates_and_numbers_as_their_formats_hold_them() {
        use lofty::TextEncoding;
        use lofty::id3::v2::{CommentFrame, Frame, Id3v2Tag, UniqueFileIdentifierFrame};
        let dir = std::env::temp_dir().join(format!("coversync-mp3fmt-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = |name: &str| dir.join(name);
        let none = TagEdit::default();
        let comment = |lang: &[u8; 3], description: &str, text: &str| {
            Frame::Comment(CommentFrame::new(
                TextEncoding::UTF8,
                *lang,
                description.to_string(),
                text.to_string(),
            ))
        };

        // Only an iTunes comment: no comment, to the index or the rules.
        fs::write(
            path("itunes.mp3"),
            include_bytes!("../tests/fixtures/silence.mp3"),
        )
        .unwrap();
        let mut id3 = Id3v2Tag::new();
        id3.set_title("Song".into());
        id3.insert(comment(b"eng", "iTunNORM", "0000ABCD"));
        // A binary file identifier, shown as hex.
        id3.insert(Frame::UniqueFileIdentifier(UniqueFileIdentifierFrame::new(
            "x".to_string(),
            vec![0xFF, 0x00],
        )));
        id3.save_to_path(path("itunes.mp3"), WriteOptions::default())
            .unwrap();
        assert!(
            !read_kinds(&path("itunes.mp3"), false)
                .unwrap()
                .values
                .contains_key("comment")
        );
        let file = probe(&path("itunes.mp3"), false).unwrap();
        assert!(!rules::present(file.primary_tag().unwrap()).contains(&"comment"));
        let tags = file_tags(&path("itunes.mp3"), "; ").unwrap();
        assert!(
            tags.other.contains(&("UFID:x".into(), "ff00".into())),
            "{:?}",
            tags.other
        );

        // Plain comments in two languages: more than one field holds.
        fs::write(
            path("two.mp3"),
            include_bytes!("../tests/fixtures/silence.mp3"),
        )
        .unwrap();
        let mut id3 = Id3v2Tag::new();
        id3.insert(comment(b"eng", "", "Nice"));
        id3.insert(comment(b"jpn", "", "いい"));
        id3.save_to_path(path("two.mp3"), WriteOptions::default())
            .unwrap();
        let refused = write_with_kinds(&path("two.mp3"), &none, &[("comment", "One".to_string())]);
        assert!(
            refused
                .unwrap_err()
                .to_string()
                .contains("several languages")
        );

        // ID3v2.3: TYER and TDAT are the date, not other tags too. (Lofty
        // doesn't write them, so the tag is built byte by byte.)
        let mut v23 = b"ID3\x03\x00\x00\x00\x00\x00\x2d".to_vec();
        for (id, text) in [(b"TIT2", b"Song"), (b"TYER", b"2010"), (b"TDAT", b"2508")] {
            v23.extend(id);
            v23.extend(5u32.to_be_bytes());
            v23.extend([0, 0, 0]);
            v23.extend(text);
        }
        v23.extend(include_bytes!("../tests/fixtures/silence.mp3"));
        fs::write(path("v23.mp3"), v23).unwrap();
        let tags = file_tags(&path("v23.mp3"), "; ").unwrap();
        assert!(tags.values.contains_key("date"), "{:?}", tags.values);
        assert!(
            !tags
                .other
                .iter()
                .any(|(k, _)| k == "TYER" || k == "TDAT" || k == "TDRC"),
            "{:?}",
            tags.other
        );

        // Label and Publisher are one frame (TPUB): two values can't both be.
        let both = [
            ("label", "Lantis".to_string()),
            ("publisher", "Bandai".to_string()),
        ];
        let refused = write_with_kinds(&path("itunes.mp3"), &none, &both).unwrap_err();
        assert!(
            refused.to_string().contains("beside the other tags set"),
            "{refused}"
        );
        // The same value in both is one value.
        let same = [
            ("label", "Lantis".to_string()),
            ("publisher", "Lantis".to_string()),
        ];
        write_with_kinds(&path("itunes.mp3"), &none, &same).unwrap();
        // The album and track fields check dates as the kinds do.
        let year = TagEdit {
            year: Some("2010-08-25 12:30".into()),
            ..Default::default()
        };
        assert!(validate(&year).is_err());
        assert!(write(&path("itunes.mp3"), &year).is_err());

        // MP4 holds a track number up to 65535.
        fs::write(
            path("a.m4a"),
            include_bytes!("../tests/fixtures/silence.m4a"),
        )
        .unwrap();
        let big = [("track_number", "65536".to_string())];
        assert!(write_with_kinds(&path("a.m4a"), &none, &big).is_err());
        write_with_kinds(
            &path("a.m4a"),
            &none,
            &[("track_number", "65535".to_string())],
        )
        .unwrap();
        assert_eq!(
            file_tags(&path("a.m4a"), "; ").unwrap().values["track_number"],
            "65535"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn files_read_again_for_a_new_reader_are_those_it_reads_differently() {
        // Since reader 0: MP3s and M4As, not Ogg; a FLAC only with ID3v2
        // (or if it can't be opened to tell).
        assert!(reader_changed(Path::new("/m/a.mp3"), 0));
        assert!(reader_changed(Path::new("/m/a.M4A"), 0));
        assert!(!reader_changed(Path::new("/m/a.Opus"), 0));
        // A path under a file can't be opened, wherever this runs.
        let missing = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml/missing.flac");
        assert!(reader_changed(&missing, 0));
        // Since 1: only such FLACs.
        assert!(!reader_changed(Path::new("/m/a.mp3"), 1));
        assert!(reader_changed(&missing, 1));
        assert!(!reader_changed(&missing, READER));
    }

    #[test]
    #[cfg(feature = "server")]
    fn values_the_format_writes_its_own_way_are_kept_and_dropped_ones_refused() {
        let dir = std::env::temp_dir().join(format!("coversync-norm-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = |name: &str| dir.join(name);
        let none = TagEdit::default();
        let kinds = |pairs: &[(&'static str, &str)]| -> Vec<(&'static str, String)> {
            pairs.iter().map(|(k, v)| (*k, v.to_string())).collect()
        };

        // MP3: a dotted original date and a numeric genre, as ID3v2 has them.
        fs::write(
            path("a.mp3"),
            include_bytes!("../tests/fixtures/silence.mp3"),
        )
        .unwrap();
        write_with_kinds(
            &path("a.mp3"),
            &none,
            &kinds(&[("original_date", "2010.08.25"), ("genre", "17")]),
        )
        .unwrap();
        let tags = file_tags(&path("a.mp3"), "; ").unwrap();
        assert_eq!(tags.values["genre"], "Rock");
        assert!(
            tags.values["original_date"].starts_with("2010-08-25"),
            "{:?}",
            tags.values
        );
        // A month 13 ID3v2 can't encode: refused, nothing written.
        let refused = write_with_kinds(&path("a.mp3"), &none, &kinds(&[("date", "2024-13-01")]));
        assert!(refused.is_err());

        // M4A: an explicit advisory of 4 is MP4's 1; 3 is dropped, and a
        // track number of 3 beside it doesn't hide that.
        fs::write(
            path("a.m4a"),
            include_bytes!("../tests/fixtures/silence.m4a"),
        )
        .unwrap();
        write_with_kinds(&path("a.m4a"), &none, &kinds(&[("advisory", "4")])).unwrap();
        let refused = write_with_kinds(
            &path("a.m4a"),
            &none,
            &kinds(&[("advisory", "3"), ("track_number", "3")]),
        );
        assert!(refused.unwrap_err().to_string().contains("'3'"));

        // A field and a kind for the same tag: only the kind's value (applied
        // after) has to fit.
        let field = TagEdit {
            track_no: Some("65536".into()),
            ..Default::default()
        };
        write_with_kinds(&path("a.m4a"), &field, &kinds(&[("track_number", "1")])).unwrap();
        assert!(check_kinds(&path("a.m4a"), &field, &kinds(&[("track_number", "1")])).is_ok());
        assert_eq!(
            file_tags(&path("a.m4a"), "; ").unwrap().values["track_number"],
            "1"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(feature = "server")]
    fn id3v2_dates_urls_and_pairs_as_they_are_written() {
        let dir = std::env::temp_dir().join(format!("coversync-id3pairs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = |name: &str| dir.join(name);
        let none = TagEdit::default();
        let kinds = |pairs: &[(&'static str, &str)]| -> Vec<(&'static str, String)> {
            pairs.iter().map(|(k, v)| (*k, v.to_string())).collect()
        };
        fs::write(
            path("a.mp3"),
            include_bytes!("../tests/fixtures/silence.mp3"),
        )
        .unwrap();
        // An original or release date ID3v2 would read only part of.
        for kind in ["original_date", "release_date"] {
            let refused =
                write_with_kinds(&path("a.mp3"), &none, &kinds(&[(kind, "2010-08-25 12:30")]));
            assert!(refused.is_err(), "{kind}");
        }
        // A URL ID3v2 can't hold (its URL frames are Latin-1).
        let refused = write_with_kinds(
            &path("a.mp3"),
            &none,
            &kinds(&[("artist_url", "https://例え.jp")]),
        );
        assert!(refused.is_err());
        // A movement number and total, in either order: both kept (MVIN).
        for pair in [
            [("movement_number", "2"), ("movement_total", "4")],
            [("movement_total", "4"), ("movement_number", "2")],
        ] {
            write_with_kinds(&path("a.mp3"), &none, &kinds(&pair)).unwrap();
            let values = file_tags(&path("a.mp3"), "; ").unwrap().values;
            assert_eq!(values["movement_number"], "2");
            assert_eq!(values["movement_total"], "4");
        }
        // A total changed beside another field, the number left as it is.
        write_with_kinds(&path("a.mp3"), &none, &kinds(&[("movement_number", "2")])).unwrap();
        write_with_kinds(
            &path("a.mp3"),
            &none,
            &kinds(&[("movement_total", "5"), ("title", "Song")]),
        )
        .unwrap();
        let values = file_tags(&path("a.mp3"), "; ").unwrap().values;
        assert_eq!(
            (
                values["movement_number"].as_str(),
                values["movement_total"].as_str()
            ),
            ("2", "5")
        );
        // Encoder beside the file's Encoder settings: one field (TSSE) for two.
        write_with_kinds(
            &path("a.mp3"),
            &none,
            &kinds(&[("encoder_settings", "-V0")]),
        )
        .unwrap();
        let refused = write_with_kinds(&path("a.mp3"), &none, &kinds(&[("encoder", "LAME")]));
        assert!(refused.is_err());
        // A time with words after it isn't read whole.
        assert!(validate_kind("date", "2010-08-25T01:30 PM").is_err());
        // Parts out of range: not a date (the form leaves these to the server).
        for date in ["2024-13-01", "2024-06-32", "2024-06-03T24:00"] {
            assert!(validate_kind("date", date).is_err(), "{date}");
        }
        // A field given another way must still have been one.
        let bad = TagEdit {
            track_no: Some("invalid".into()),
            ..Default::default()
        };
        assert!(check_kinds(&path("a.mp3"), &bad, &kinds(&[("track_number", "1")])).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn numbers() {
        assert_eq!(parse_number("t", "3/12").unwrap(), Some(3));
        assert_eq!(parse_number("t", " ").unwrap(), None);
        assert!(parse_number("t", "x").is_err());
    }
}
