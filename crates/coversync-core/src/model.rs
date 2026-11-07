use serde::{Deserialize, Serialize};

/// Which folder a file was found in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Collection {
    /// Unmanaged files in the Inbox folder.
    #[default]
    Inbox,
    /// Files coversync manages in the Library folder.
    Library,
}

impl Collection {
    pub fn as_str(self) -> &'static str {
        match self {
            Collection::Inbox => "inbox",
            Collection::Library => "library",
        }
    }

    pub fn parse(s: &str) -> Self {
        if s == "library" {
            Collection::Library
        } else {
            Collection::Inbox
        }
    }
}

/// Album summary used by the grid.
#[derive(Debug, Clone, Serialize)]
pub struct Album {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub year: Option<i32>,
    pub genre: Option<String>,
    pub track_count: u32,
    pub duration_ms: u64,
    /// Dominant cover colour as `#rrggbb`, once the cover pass has run.
    pub color: Option<String>,
    pub has_cover: bool,
    /// Changes whenever the cover pass redoes the album's art. Cover URLs carry
    /// it, so a browser can keep a cover until the art changes.
    pub cover_version: Option<String>,
    /// When the album was added (Unix seconds): for the Inbox, its newest
    /// file's modification time; for the Library, when it was first imported.
    pub added_at: i64,
    pub format: String,
    pub collection: Collection,
    /// Album-wide values for the sort menu's extra sorts (see `appearance`).
    #[serde(skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub sort_values: std::collections::HashMap<String, String>,
    /// Every value of the sidebar's filter tags on any of the album's tracks
    /// (see `appearance`), with tags that hold several split apart.
    #[serde(skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub filter_values: std::collections::HashMap<String, Vec<String>>,
    /// Set when tracks lack tags the library rules or folder structure need.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attention: Option<Attention>,
}

/// What keeps an album from passing the library rules as it stands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Attention {
    /// Required tag kinds at least one track has no value for, in catalog order.
    pub missing: Vec<String>,
    /// How many tracks miss at least one of them.
    pub tracks: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct Track {
    pub id: i64,
    pub album_id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
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
    /// Path relative to the library root it was found under.
    pub rel_path: String,
    pub collection: Collection,
}

#[derive(Debug, Clone, Serialize)]
pub struct AlbumDetail {
    #[serde(flatten)]
    pub album: Album,
    pub dir: String,
    pub tracks: Vec<Track>,
}

/// Which Library an index is of, and how far its changes go.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LibraryVersion {
    /// A random UUID, made with the Library's database. Another id is
    /// another Library, whose counter says nothing about this one's. A
    /// Library restored from an older copy gets a new one from an
    /// installation that saw its counter get further.
    pub id: String,
    /// The change counter, which goes up with every change to the Library's
    /// tracks. Lower than a client saw before, under the same id, means the
    /// Library was restored from an older copy somewhere that didn't know
    /// the later state.
    pub generation: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InboxVersion {
    /// The Inbox's change counter: it goes up whenever its tracks change or
    /// files are hidden or shown again.
    pub generation: i64,
}

/// Where both indexes stand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IndexVersions {
    /// None while no Library folder is set up.
    pub library: Option<LibraryVersion>,
    pub inbox: InboxVersion,
}

/// A Library track written after the generation asked about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedTrack {
    pub id: i64,
    /// Relative to the Library folder.
    pub path: String,
    pub album_id: String,
    pub generation: i64,
}

/// A track that left the Library. A file may since have come back at the
/// same path: that is a new track, usually under a new id, listed among the
/// changed tracks (see [`LibraryChanges`] for the order to apply them in).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovedTrack {
    /// The id the track had.
    pub id: i64,
    /// Relative to the Library folder.
    pub path: String,
    pub album_id: String,
    pub generation: i64,
    /// Unix seconds.
    pub removed_at: i64,
}

/// What changed in the Library after a generation, each list earliest
/// change first. Apply them in generation order, and within a generation,
/// the removals before the tracks. A track listed is one still in the
/// Library, written after any removal of its id or path, and a removal and
/// a track can share a generation: an older coversyncd stamps all its
/// changes one above the counter, so its deleting a track and writing one
/// at the same path, or under the reused highest id, land together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryChanges {
    pub library: LibraryVersion,
    pub tracks: Vec<ChangedTrack>,
    pub removed: Vec<RemovedTrack>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ScanProgress {
    pub running: bool,
    pub phase: String,
    pub files_seen: u64,
    pub files_read: u64,
    pub albums: u64,
    pub covers_done: u64,
    pub covers_total: u64,
    pub errors: u64,
    pub finished_at: Option<i64>,
}

/// How common each rule tag kind is among indexed tracks.
#[derive(Debug, Clone, Serialize)]
pub struct TagCoverage {
    pub tracks: u32,
    pub albums: u32,
    /// One entry per `rules::CATALOG` kind, in catalog order.
    pub kinds: Vec<KindCoverage>,
    /// Per album: indices into `kinds` that every track of the album has.
    pub album_kinds: std::collections::HashMap<String, Vec<u16>>,
    /// Albums with no cover art, in a file beside the tracks or embedded.
    pub without_art: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct KindCoverage {
    pub id: &'static str,
    /// Tracks with a value for this kind.
    pub tracks: u32,
    /// Albums where every track has a value for this kind.
    pub albums: u32,
}

/// A partial tag update. `None` leaves a field untouched; `Some("")` clears it.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TagEdit {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub track_no: Option<String>,
    pub disc_no: Option<String>,
    pub year: Option<String>,
    pub genre: Option<String>,
}

impl TagEdit {
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.artist.is_none()
            && self.album.is_none()
            && self.album_artist.is_none()
            && self.track_no.is_none()
            && self.disc_no.is_none()
            && self.year.is_none()
            && self.genre.is_none()
    }
}
