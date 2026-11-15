//! Sync: the sidebar's Sync setting, and what each paired device gets.
//!
//! Every device gets the whole Library. coversyncd keeps each device's
//! [`DeviceSettings`] with the device, and works out where each Library
//! file goes on it ([`device_paths`]), cached per settings version and
//! Library version ([`PathCache`]). The settings never say where the device
//! keeps its copy: that is the device's own business. The Library's own
//! database folder (`db::LIBRARY_DB`) isn't a track, so it never syncs.

use std::collections::{HashMap, HashSet};
use std::ops::RangeInclusive;
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::model::LibraryVersion;
use crate::template::{self, Template};
use crate::{Error, Library, Result, art};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncSetup {
    /// Sync is on: the sidebar shows its Sync section.
    pub enabled: bool,
}

impl Default for SyncSetup {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// The art widths a device can be limited to, in pixels.
pub const ART_WIDTHS: RangeInclusive<u32> = 16..=16384;

/// How a paired device lays out its copy of the Library, and its art.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DeviceSettings {
    /// The device's own folder structure (see [`template`]). None: each
    /// file keeps its path in the Library.
    pub structure: Option<String>,
    /// Names a FAT32 or exFAT card can hold (see [`template::fat_name`]).
    pub fat_safe: bool,
    pub art: DeviceArt,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DeviceArt {
    /// Write each album's cover as a file beside its tracks.
    pub cover_file: bool,
    /// That file's name without extension, as in Library setup ("cover" →
    /// cover.jpg).
    pub cover_name: String,
    /// The widest the cover file and the art embedded in tracks may be, in
    /// pixels. Wider art is scaled down to it, keeping its shape, where
    /// files are written for the device. None: art goes as it is.
    pub max_width: Option<u32>,
}

impl Default for DeviceArt {
    fn default() -> Self {
        Self {
            cover_file: true,
            cover_name: art::FILE_NAMES[0].to_string(),
            max_width: None,
        }
    }
}

impl DeviceSettings {
    /// Check settings as typed, and tidy them: a blank folder structure is
    /// the Library's.
    pub fn normalize(mut self) -> Result<Self> {
        self.structure = self
            .structure
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        if let Some(structure) = &self.structure {
            Template::parse(structure)?;
        }
        self.art.cover_name = art::cover_file_name(&self.art.cover_name)?;
        if let Some(width) = self.art.max_width
            && !ART_WIDTHS.contains(&width)
        {
            return Err(Error::Invalid(format!(
                "Art can be limited to {} to {} pixels wide; leave it empty for no limit.",
                ART_WIDTHS.start(),
                ART_WIDTHS.end()
            )));
        }
        Ok(self)
    }
}

/// A device's settings and their version, which goes up with each change
/// (0 until they're first saved).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DeviceSync {
    pub version: i64,
    pub settings: DeviceSettings,
}

/// Every Library track as working out device paths needs it, read in one
/// go (see [`Library::sync_catalog`]).
pub struct Catalog {
    pub library: LibraryVersion,
    /// In order of their paths.
    pub tracks: Vec<CatalogTrack>,
}

pub struct CatalogTrack {
    /// Its public id, as `Track::id`.
    pub id: i64,
    pub album_id: String,
    /// Relative to the Library folder.
    pub path: String,
    /// Its tag values as stored (a JSON object), when they were asked for.
    pub values: String,
}

/// Where a Library track goes on a device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeviceFile {
    pub track_id: i64,
    pub album_id: String,
    /// Relative to wherever the device keeps the Library, `/` between names.
    pub path: String,
}

/// Every Library track's path on a device, as of a Library version and the
/// device's settings version.
#[derive(Debug, Serialize)]
pub struct DevicePaths {
    pub library: LibraryVersion,
    pub settings_version: i64,
    /// Tracks with no value for a variable of the device's own folder
    /// structure, which was left empty.
    pub incomplete: usize,
    /// Tracks numbered to keep them apart from another at the same path.
    pub renamed: usize,
    pub files: Vec<DeviceFile>,
}

/// Where each Library track in `catalog` goes on a device with these
/// settings, in one pass, without touching the files or the database.
///
/// - Following the Library's folder structure, a track keeps its path in the
///   Library.
/// - With its own, the path comes from the track's tags. A variable the
///   track has no value for is left empty, and a name left empty is `_`, so
///   every track still goes to the device.
/// - FAT-safe names go through [`template::fat_name`].
/// - Tracks that would share a path, case aside (FAT, exFAT and Android's
///   shared storage ignore it), are kept apart: in order of their Library
///   paths, the first keeps it and the next get " (2)", " (3)" and so on.
pub fn device_paths(
    catalog: Catalog,
    settings_version: i64,
    settings: &DeviceSettings,
) -> Result<DevicePaths> {
    let template = settings
        .structure
        .as_deref()
        .map(Template::parse)
        .transpose()?;
    let kinds = template
        .as_ref()
        .map(Template::required_kinds)
        .unwrap_or_default();
    let mut incomplete = 0;
    let mut files = Vec::with_capacity(catalog.tracks.len());
    for track in catalog.tracks {
        let path = match &template {
            None => device_names(&track.path, settings.fat_safe),
            Some(template) => {
                let values = tag_values(&track.values, &kinds);
                let ext = Path::new(&track.path)
                    .extension()
                    .map(|e| e.to_string_lossy())
                    .unwrap_or_default();
                let (path, missing) = template.render_partial(&values, &ext);
                incomplete += usize::from(!missing.is_empty());
                device_names(&path.to_string_lossy(), settings.fat_safe)
            }
        };
        files.push(DeviceFile {
            track_id: track.id,
            album_id: track.album_id,
            path,
        });
    }
    let renamed = keep_apart(&mut files);
    Ok(DevicePaths {
        library: catalog.library,
        settings_version,
        incomplete,
        renamed,
        files,
    })
}

/// A relative path with FAT-safe names, if asked for.
fn device_names(path: &str, fat_safe: bool) -> String {
    if !fat_safe {
        return path.to_string();
    }
    let names: Vec<&str> = path.split('/').collect();
    let last = names.len() - 1;
    names
        .iter()
        .enumerate()
        .map(|(i, name)| template::fat_name(name, i == last))
        .collect::<Vec<_>>()
        .join("/")
}

/// The values of `kinds` among a track's stored tag values.
fn tag_values(json: &str, kinds: &[&'static str]) -> HashMap<&'static str, String> {
    let mut all: HashMap<String, String> = serde_json::from_str(json).unwrap_or_default();
    kinds
        .iter()
        .filter_map(|k| all.remove(*k).map(|v| (*k, v)))
        .collect()
}

/// Number the files whose path, case aside, an earlier one has. Returns
/// how many were numbered.
fn keep_apart(files: &mut [DeviceFile]) -> usize {
    let mut taken: HashSet<String> = HashSet::with_capacity(files.len());
    let clashes: Vec<usize> = (0..files.len())
        .filter(|&i| !taken.insert(files[i].path.to_lowercase()))
        .collect();
    for &i in &clashes {
        let path = &files[i].path;
        files[i].path = (2..)
            .map(|n| numbered(path, n))
            .find(|p| taken.insert(p.to_lowercase()))
            .expect("some number is free");
    }
    clashes.len()
}

/// `path` with " (n)" after its file name's stem.
fn numbered(path: &str, n: usize) -> String {
    let (dir, name) = match path.rsplit_once('/') {
        Some((dir, name)) => (format!("{dir}/"), name),
        None => (String::new(), path),
    };
    let (stem, ext) = template::split_extension(name);
    let suffix = format!(" ({n})");
    let stem = template::cut(stem, template::MAX_NAME - suffix.len() - ext.len()).trim_end();
    format!("{dir}{stem}{suffix}{ext}")
}

/// Each device's paths, as last worked out. They're worked out again once
/// the device's settings or the Library has changed.
#[derive(Default)]
pub struct PathCache {
    devices: Mutex<HashMap<i64, Slot>>,
}

/// One device's paths, locked while they're worked out.
type Slot = Arc<Mutex<Option<Arc<DevicePaths>>>>;

impl PathCache {
    /// Where every Library track goes on `device`, whose settings are
    /// `sync`. None without a Library folder.
    pub fn paths(
        &self,
        lib: &Library,
        device: i64,
        sync: &DeviceSync,
    ) -> Result<Option<Arc<DevicePaths>>> {
        let slot = self
            .devices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(device)
            .or_default()
            .clone();
        // Requests for one device wait for paths being worked out for it,
        // rather than each working them out too.
        let mut cached = slot.lock().unwrap_or_else(|e| e.into_inner());
        let Some(library) = lib.index_versions()?.library else {
            return Ok(None);
        };
        if let Some(paths) = cached.as_ref()
            && paths.settings_version == sync.version
            && paths.library == library
        {
            return Ok(Some(paths.clone()));
        }
        let own = sync.settings.structure.is_some();
        let Some(catalog) = lib.sync_catalog(own)? else {
            return Ok(None);
        };
        let paths = Arc::new(device_paths(catalog, sync.version, &sync.settings)?);
        *cached = Some(paths.clone());
        Ok(Some(paths))
    }

    /// Let a removed device's paths go.
    pub fn forget(&self, device: i64) {
        self.devices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&device);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::FileStat;
    use crate::tags::TrackTags;
    use crate::{Collection, LibrarySetup};

    /// The device paths of tracks given as (Library path, tag values), in
    /// path order as the catalog is read; and how many were incomplete and
    /// renamed.
    fn paths(tracks: &[(&str, &str)], settings: DeviceSettings) -> (Vec<String>, usize, usize) {
        let catalog = Catalog {
            library: LibraryVersion {
                id: "library".into(),
                generation: 1,
            },
            tracks: tracks
                .iter()
                .enumerate()
                .map(|(i, (path, values))| CatalogTrack {
                    id: -1 - i as i64,
                    album_id: "album".into(),
                    path: path.to_string(),
                    values: values.to_string(),
                })
                .collect(),
        };
        let settings = settings.normalize().unwrap();
        let p = device_paths(catalog, 1, &settings).unwrap();
        let files = p.files.into_iter().map(|f| f.path).collect();
        (files, p.incomplete, p.renamed)
    }

    #[test]
    fn library_paths_go_as_they_are_or_fat_safe() {
        let tracks = [
            ("A/B/01 - x.flac", ""),
            ("AC:DC/CON/x.flac", ""),
            ("AC:DC/CON/x?.flac", ""),
            ("a/b/01 - X.flac", ""),
        ];
        // Only a name that differs by case alone is numbered.
        let (got, _, renamed) = paths(&tracks, DeviceSettings::default());
        assert_eq!(
            got,
            [
                "A/B/01 - x.flac",
                "AC:DC/CON/x.flac",
                "AC:DC/CON/x?.flac",
                "a/b/01 - X (2).flac"
            ]
        );
        assert_eq!(renamed, 1);
        let fat = DeviceSettings {
            fat_safe: true,
            ..Default::default()
        };
        let (got, _, renamed) = paths(&tracks, fat);
        assert_eq!(
            got,
            [
                "A/B/01 - x.flac",
                "ACDC/_CON/x.flac",
                "ACDC/_CON/x (2).flac",
                "a/b/01 - X (2).flac"
            ]
        );
        assert_eq!(renamed, 2);
    }

    #[test]
    fn an_own_structure_places_every_track() {
        let full = r#"{"album_artist":"AC/DC","date":"1979-07-27","album":"Highway to Hell",
            "track_number":"1/10","title":"Highway to Hell"}"#;
        let gaps = r#"{"album":"Highway to Hell","track_number":"2","title":"Girls Got Rhythm"}"#;
        let tracks = [("x/1.flac", full), ("x/2.FLAC", gaps), ("y/1.flac", full)];
        let own = |fat_safe| DeviceSettings {
            structure: Some("{album_artist}/{year} - {album}/{track_number}: {title}".into()),
            fat_safe,
            ..Default::default()
        };
        let (got, incomplete, renamed) = paths(&tracks, own(false));
        assert_eq!(
            got,
            [
                "AC-DC/1979 - Highway to Hell/01: Highway to Hell.flac",
                // No album artist or year: those parts are left empty.
                "_/- Highway to Hell/02: Girls Got Rhythm.flac",
                "AC-DC/1979 - Highway to Hell/01: Highway to Hell (2).flac",
            ]
        );
        assert_eq!((incomplete, renamed), (1, 1));
        // FAT-safe names also leave out what the template itself writes.
        let (got, _, _) = paths(&tracks, own(true));
        assert_eq!(
            got[0],
            "AC-DC/1979 - Highway to Hell/01 Highway to Hell.flac"
        );
    }

    #[test]
    fn paths_are_worked_out_again_after_settings_or_the_library_change() {
        let dir = std::env::temp_dir().join(format!("coversync-sync-paths-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Library")).unwrap();
        let root = dir.join("Library").canonicalize().unwrap();
        let lib = Library::open(&dir.join("state/inbox.sqlite3")).unwrap();
        lib.set_library_setup(LibrarySetup {
            folder: Some(root.display().to_string()),
            ..Default::default()
        })
        .unwrap();
        let add = |title: &str| {
            let stat = FileStat {
                mtime_ns: -1,
                hash: None,
                path: root.join(format!("{title}.flac")),
                root: root.clone(),
                mtime: 1,
                size: 1,
                collection: Collection::Library,
                added: None,
            };
            let tags = TrackTags {
                values: [("title", title.to_string())].into(),
                format: "FLAC".into(),
                ..Default::default()
            };
            lib.upsert(&[(stat, "album".into(), root.clone(), tags)])
                .unwrap();
        };
        let files = |p: &DevicePaths| p.files.iter().map(|f| f.path.clone()).collect::<Vec<_>>();
        add("one");
        let cache = PathCache::default();
        let mut sync = DeviceSync {
            version: 0,
            settings: DeviceSettings::default(),
        };
        let first = cache.paths(&lib, 1, &sync).unwrap().unwrap();
        assert_eq!(files(&first), ["one.flac"]);
        let again = cache.paths(&lib, 1, &sync).unwrap().unwrap();
        assert!(Arc::ptr_eq(&first, &again));
        sync = DeviceSync {
            version: 1,
            settings: DeviceSettings {
                structure: Some("Own/{title}".into()),
                ..Default::default()
            },
        };
        let own = cache.paths(&lib, 1, &sync).unwrap().unwrap();
        assert_eq!(files(&own), ["Own/one.flac"]);
        add("two");
        let more = cache.paths(&lib, 1, &sync).unwrap().unwrap();
        assert_eq!(files(&more), ["Own/one.flac", "Own/two.flac"]);
        drop(lib);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn on_unless_turned_off() {
        assert!(SyncSetup::default().enabled);
        let empty: SyncSetup = serde_json::from_str("{}").unwrap();
        assert!(empty.enabled);
        let off: SyncSetup = serde_json::from_str(r#"{"enabled":false}"#).unwrap();
        assert!(!off.enabled);
    }
}
