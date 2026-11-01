//! Library rules: the conditions every file coversync manages must meet.
//!
//! Tag kinds use coversync's own stable ids, each mapped to one or more lofty
//! keys so the same requirement works across FLAC, MP3, MP4 and the rest.

use std::collections::HashSet;

use lofty::tag::{ItemKey, Tag};
use serde::{Deserialize, Serialize};

use crate::{Error, Result};

#[derive(Debug, Serialize)]
pub struct TagGroup {
    pub id: &'static str,
    pub label: &'static str,
}

#[derive(Debug, Serialize)]
pub struct TagKind {
    pub id: &'static str,
    pub label: &'static str,
    pub group: &'static str,
    /// Present when the file has any of these keys with a non-empty value.
    #[serde(skip)]
    pub keys: &'static [ItemKey],
}

pub const GROUPS: &[TagGroup] = &[
    TagGroup {
        id: "core",
        label: "Core",
    },
    TagGroup {
        id: "release",
        label: "Release",
    },
    TagGroup {
        id: "credits",
        label: "Credits",
    },
    TagGroup {
        id: "work",
        label: "Work & style",
    },
    TagGroup {
        id: "sorting",
        label: "Sorting",
    },
    TagGroup {
        id: "ids",
        label: "Identifiers",
    },
    TagGroup {
        id: "loudness",
        label: "Loudness",
    },
    TagGroup {
        id: "technical",
        label: "Technical",
    },
    TagGroup {
        id: "links",
        label: "Links",
    },
];

macro_rules! kinds {
    ($($group:literal => [$($id:literal, $label:literal, [$($key:ident),+];)+])+) => {
        &[$($(TagKind { id: $id, label: $label, group: $group, keys: &[$(ItemKey::$key),+] },)+)+]
    };
}

/// Every tag kind a rule can refer to, in display order. Podcast, internet
/// radio, TV show and Apple-internal keys are left out.
pub const CATALOG: &[TagKind] = kinds! {
    "core" => [
        "title", "Title", [TrackTitle];
        "artist", "Artist", [TrackArtist];
        "album", "Album", [AlbumTitle];
        "album_artist", "Album artist", [AlbumArtist];
        "track_number", "Track number", [TrackNumber];
        "track_total", "Track total", [TrackTotal];
        "disc_number", "Disc number", [DiscNumber];
        "disc_total", "Disc total", [DiscTotal];
        "date", "Year", [RecordingDate, Year];
        "genre", "Genre", [Genre];
    ]
    "release" => [
        "release_date", "Release date", [ReleaseDate];
        "original_date", "Original release date", [OriginalReleaseDate];
        "label", "Label", [Label];
        "publisher", "Publisher", [Publisher];
        "catalog_number", "Catalog number", [CatalogNumber];
        "barcode", "Barcode", [Barcode];
        "release_country", "Release country", [ReleaseCountry];
        "release_type", "Release type", [MusicBrainzReleaseType];
        "media", "Media", [OriginalMediaType];
        "compilation", "Compilation flag", [FlagCompilation];
        "disc_subtitle", "Disc subtitle", [SetSubtitle];
        "copyright", "Copyright", [CopyrightMessage];
        "license", "License", [License];
    ]
    "credits" => [
        "artists", "Artists (multi-value)", [TrackArtists];
        "album_artists", "Album artists (multi-value)", [AlbumArtists];
        "composer", "Composer", [Composer];
        "lyricist", "Lyricist", [Lyricist];
        "writer", "Writer", [Writer];
        "arranger", "Arranger", [Arranger];
        "conductor", "Conductor", [Conductor];
        "performer", "Performer", [Performer];
        "producer", "Producer", [Producer];
        "engineer", "Engineer", [Engineer];
        "mix_engineer", "Mix engineer", [MixEngineer];
        "mix_dj", "Mix DJ", [MixDj];
        "remixer", "Remixer", [Remixer];
        "director", "Director", [Director];
        "original_artist", "Original artist", [OriginalArtist];
        "original_lyricist", "Original lyricist", [OriginalLyricist];
        "original_album", "Original album", [OriginalAlbumTitle];
    ]
    "work" => [
        "subtitle", "Subtitle", [TrackSubtitle];
        "grouping", "Grouping", [ContentGroup];
        "work", "Work", [Work];
        "movement", "Movement", [Movement];
        "movement_number", "Movement number", [MovementNumber];
        "movement_total", "Movement total", [MovementTotal];
        "mood", "Mood", [Mood];
        "bpm", "BPM", [Bpm, IntegerBpm];
        "key", "Initial key", [InitialKey];
        "language", "Language", [Language];
        "script", "Script", [Script];
        "lyrics", "Lyrics", [Lyrics, UnsyncLyrics];
        "comment", "Comment", [Comment];
        "description", "Description", [Description];
        "rating", "Rating", [Popularimeter];
        "advisory", "Parental advisory", [ParentalAdvisory];
    ]
    "sorting" => [
        "title_sort", "Title sort", [TrackTitleSortOrder];
        "artist_sort", "Artist sort", [TrackArtistSortOrder];
        "album_sort", "Album sort", [AlbumTitleSortOrder];
        "album_artist_sort", "Album artist sort", [AlbumArtistSortOrder];
        "composer_sort", "Composer sort", [ComposerSortOrder];
    ]
    "ids" => [
        "isrc", "ISRC", [Isrc];
        "mb_recording_id", "MusicBrainz recording ID", [MusicBrainzRecordingId];
        "mb_track_id", "MusicBrainz track ID", [MusicBrainzTrackId];
        "mb_release_id", "MusicBrainz release ID", [MusicBrainzReleaseId];
        "mb_release_group_id", "MusicBrainz release group ID", [MusicBrainzReleaseGroupId];
        "mb_artist_id", "MusicBrainz artist ID", [MusicBrainzArtistId];
        "mb_release_artist_id", "MusicBrainz release artist ID", [MusicBrainzReleaseArtistId];
        "mb_work_id", "MusicBrainz work ID", [MusicBrainzWorkId];
        "acoustid", "AcoustID", [AcoustId];
        "acoustid_fingerprint", "AcoustID fingerprint", [AcoustIdFingerprint];
    ]
    "loudness" => [
        "rg_track_gain", "ReplayGain track gain", [ReplayGainTrackGain];
        "rg_track_peak", "ReplayGain track peak", [ReplayGainTrackPeak];
        "rg_album_gain", "ReplayGain album gain", [ReplayGainAlbumGain];
        "rg_album_peak", "ReplayGain album peak", [ReplayGainAlbumPeak];
        "r128_track_gain", "R128 track gain", [R128TrackGain];
        "r128_album_gain", "R128 album gain", [R128AlbumGain];
    ]
    "technical" => [
        "encoded_by", "Encoded by", [EncodedBy];
        "encoder", "Encoder", [EncoderSoftware];
        "encoder_settings", "Encoder settings", [EncoderSettings];
        "encoding_time", "Encoding time", [EncodingTime];
        "tagging_time", "Tagging time", [TaggingTime];
        "length", "Length", [Length];
        "original_filename", "Original filename", [OriginalFileName];
        "file_owner", "File owner", [FileOwner];
    ]
    "links" => [
        "artist_url", "Artist URL", [TrackArtistUrl];
        "audio_file_url", "Audio file URL", [AudioFileUrl];
        "audio_source_url", "Audio source URL", [AudioSourceUrl];
        "commercial_url", "Commercial info URL", [CommercialInformationUrl];
        "copyright_url", "Copyright URL", [CopyrightUrl];
        "payment_url", "Payment URL", [PaymentUrl];
        "publisher_url", "Publisher URL", [PublisherUrl];
    ]
};

/// A sensible starting point for "require tags".
pub const ESSENTIALS: &[&str] = &[
    "title",
    "artist",
    "album",
    "album_artist",
    "track_number",
    "date",
    "genre",
];

pub fn kind(id: &str) -> Option<&'static TagKind> {
    CATALOG.iter().find(|k| k.id == id)
}

/// Catalog ids of the tag kinds `tag` has a non-empty value for.
pub fn present(tag: &Tag) -> Vec<&'static str> {
    let keys: HashSet<ItemKey> = tag
        .items()
        .filter(|i| i.value().text().is_none_or(|t| !t.trim().is_empty()))
        .map(|i| i.key())
        .collect();
    CATALOG
        .iter()
        .filter(|k| k.keys.iter().any(|key| keys.contains(key)))
        .map(|k| k.id)
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rules {
    /// Catalog ids every managed file must have a value for.
    #[serde(default)]
    pub required_tags: Vec<String>,
    #[serde(default)]
    pub art: crate::art::ArtRules,
    /// Joins the values of a tag that holds several (artists, genres…) in
    /// what MusicBrainz suggests.
    #[serde(default = "default_separator")]
    pub separator: String,
}

impl Default for Rules {
    fn default() -> Self {
        Self {
            required_tags: Vec::new(),
            art: Default::default(),
            separator: default_separator(),
        }
    }
}

/// The separator for multiple values unless Library setup says otherwise.
pub const DEFAULT_SEPARATOR: &str = ";";

fn default_separator() -> String {
    DEFAULT_SEPARATOR.to_string()
}

/// Kinds that normally hold one value for a whole album; the import editor
/// asks for these once rather than per track.
pub const ALBUM_KINDS: &[&str] = &[
    "album",
    "album_artist",
    "album_artists",
    "track_total",
    "disc_total",
    "date",
    "genre",
    "release_date",
    "original_date",
    "label",
    "publisher",
    "catalog_number",
    "barcode",
    "release_country",
    "release_type",
    "media",
    "compilation",
    "copyright",
    "license",
    "album_sort",
    "album_artist_sort",
    "mb_release_id",
    "mb_release_group_id",
    "mb_release_artist_id",
    "rg_album_gain",
    "rg_album_peak",
    "r128_album_gain",
    "original_album",
];

/// Kinds that can't be typed in as text (ratings are binary frames).
pub const NOT_EDITABLE: &[&str] = &["rating"];

fn default_structure() -> String {
    crate::template::DEFAULT.to_string()
}

/// How files in the Library are managed. Stored by the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibrarySetup {
    #[serde(default)]
    pub rules: Rules,
    /// Absolute path of the folder coversync manages.
    #[serde(default)]
    pub folder: Option<String>,
    /// Where files go inside `folder`; see `template`.
    #[serde(default = "default_structure")]
    pub structure: String,
}

impl Default for LibrarySetup {
    fn default() -> Self {
        Self {
            rules: Rules::default(),
            folder: None,
            structure: default_structure(),
        }
    }
}

/// An absolute folder path from a setting, or None when left empty.
pub fn folder_setting(value: Option<String>, what: &str) -> Result<Option<String>> {
    let Some(v) = value.map(|v| v.trim().trim_end_matches('/').to_string()) else {
        return Ok(None);
    };
    if v.is_empty() {
        return Ok(None);
    }
    if !v.starts_with('/') {
        return Err(Error::Invalid(format!(
            "The {what} must be an absolute path, like /data/Music."
        )));
    }
    if v.split('/').any(|c| c == ".." || c == ".") {
        return Err(Error::Invalid(format!(
            "The {what} can't contain . or .. components."
        )));
    }
    Ok(Some(v))
}

impl LibrarySetup {
    /// Validate, and store tag ids once, in catalog order.
    pub fn normalize(mut self) -> Result<Self> {
        self.folder = folder_setting(self.folder, "library folder")?;
        self.structure = self.structure.trim().to_string();
        crate::template::Template::parse(&self.structure)?;
        self.rules.art = self.rules.art.normalize()?;
        // Kept as typed: "; " and ";" are different separators.
        let separator = &self.rules.separator;
        if separator.trim().is_empty()
            || separator.chars().count() > 8
            || separator.chars().any(char::is_control)
        {
            return Err(Error::Invalid(
                "The separator for multiple values needs a character other than a space, and at most 8 in all."
                    .into(),
            ));
        }
        if let Some(bad) = self
            .rules
            .required_tags
            .iter()
            .find(|id| kind(id).is_none())
        {
            return Err(Error::Invalid(format!("unknown tag kind '{bad}'")));
        }
        let wanted: HashSet<String> = self.rules.required_tags.drain(..).collect();
        self.rules.required_tags = CATALOG
            .iter()
            .filter(|k| wanted.contains(k.id))
            .map(|k| k.id.to_string())
            .collect();
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lofty::tag::TagType;

    #[test]
    fn catalog_ids_are_unique_and_grouped() {
        let ids: HashSet<_> = CATALOG.iter().map(|k| k.id).collect();
        assert_eq!(ids.len(), CATALOG.len());
        assert!(
            CATALOG
                .iter()
                .all(|k| GROUPS.iter().any(|g| g.id == k.group))
        );
        assert!(ESSENTIALS.iter().all(|id| kind(id).is_some()));
    }

    #[test]
    fn present_ignores_blank_values_and_merges_aliases() {
        let mut tag = Tag::new(TagType::VorbisComments);
        tag.insert_text(ItemKey::TrackTitle, "Song".into());
        tag.insert_text(ItemKey::Year, "2003".into());
        tag.insert_text(ItemKey::Genre, "  ".into());
        let p = present(&tag);
        assert!(p.contains(&"title"));
        assert!(p.contains(&"date"));
        assert!(!p.contains(&"genre"));
    }

    #[test]
    fn normalize_orders_dedupes_and_rejects_unknown() {
        let setup = LibrarySetup {
            rules: Rules {
                required_tags: vec!["genre".into(), "title".into(), "genre".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            setup.normalize().unwrap().rules.required_tags,
            ["title", "genre"]
        );
        let bad = LibrarySetup {
            rules: Rules {
                required_tags: vec!["nope".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(bad.normalize().is_err());
    }

    #[test]
    fn separators_are_kept_as_typed_but_never_blank() {
        let with = |separator: &str| LibrarySetup {
            rules: Rules {
                separator: separator.into(),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(LibrarySetup::default().rules.separator, ";");
        // Saved before there was a separator: the default.
        let old: Rules = serde_json::from_str(r#"{"required_tags": []}"#).unwrap();
        assert_eq!(old.separator, ";");
        assert_eq!(with("; ").normalize().unwrap().rules.separator, "; ");
        assert_eq!(with(" / ").normalize().unwrap().rules.separator, " / ");
        for bad in ["", "  ", "\t", "123456789"] {
            assert!(with(bad).normalize().is_err(), "{bad:?}");
        }
    }
}
