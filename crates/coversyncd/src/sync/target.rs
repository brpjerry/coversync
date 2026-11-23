//! A device's target: every file it should hold, at its path on the device,
//! with the content's hash and size, and where the server reads the bytes.
//!
//! Tracks come from the device's paths (SYN-03), with the Library file's own
//! hash. With a cover file on, each folder that holds one album's tracks
//! gets the album's art as a file, scaled to the device's width. Files the
//! server is still hashing or making count as preparing and wait for a
//! later plan; paths the device can't hold are left out and counted.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use coversync_core::db::CoverSource;
use coversync_core::sync::{DevicePaths, DeviceSettings};
use coversync_protocol::Capabilities;

use super::plan::{Entry, Held};
use super::store::CoverFile;

/// Where a content's bytes are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A Library file. Its bytes are the content only while it still has
    /// the size and time it was hashed at.
    Library {
        path: PathBuf,
        size: u64,
        mtime_ns: i64,
    },
    /// A cover file made for devices, in the cache.
    Cover { path: PathBuf, size: u64 },
}

#[derive(Debug, Default)]
pub struct Target {
    /// Changes whenever anything a plan depends on does.
    pub version: String,
    /// The files that are ready, in the order puts go: albums added to the
    /// Library most recently first, each album's cover before its tracks.
    pub entries: Vec<Entry>,
    /// Files still being hashed or made.
    pub preparing: u64,
    /// Files whose path the device can't hold: too long, or unusable (a
    /// name a hand-placed Library file has, such as one with a backslash).
    pub too_long: u64,
    sources: HashMap<String, Source>,
    paths: HashMap<String, usize>,
    /// Tracks still being hashed, by id, and their sizes.
    waiting_tracks: HashSet<i64>,
    waiting_sizes: HashSet<u64>,
    /// Albums whose cover file is still being made, and where it goes
    /// (without the extension, which follows the image).
    waiting_covers: HashSet<String>,
    waiting_cover_stems: HashSet<String>,
}

impl Target {
    /// Where the bytes of a content in this target are.
    pub fn source(&self, hash: &str) -> Option<&Source> {
        self.sources.get(hash)
    }

    /// The entry at a path on the device.
    pub fn at(&self, path: &str) -> Option<&Entry> {
        self.paths.get(path).map(|&i| &self.entries[i])
    }

    /// Whether a file the device holds, at `path`, is of a track or cover
    /// whose new content isn't ready: its copy stays meanwhile, wherever it
    /// is. A cover also stays where a cover is being made, as its album may
    /// have a new id since. A file the server doesn't know the owner of
    /// (one recorded before owners were) stays while anything is being
    /// prepared.
    pub fn waits(&self, path: &str, held: &Held) -> bool {
        match (held.track, &held.album) {
            (Some(track), _) => self.waiting_tracks.contains(&track),
            (None, Some(album)) => {
                self.waiting_covers.contains(album)
                    || path
                        .rsplit_once('.')
                        .is_some_and(|(stem, _)| self.waiting_cover_stems.contains(stem))
            }
            (None, None) => self.preparing > 0,
        }
    }

    /// Whether a file found on the device might be content still being
    /// hashed or made: one of a waiting track's size, or where a waiting
    /// cover goes. A check waits for such content while it's on its way.
    pub fn might_be_waiting(&self, path: &str, size: u64) -> bool {
        self.waiting_sizes.contains(&size)
            || path
                .rsplit_once('.')
                .is_some_and(|(stem, _)| self.waiting_cover_stems.contains(stem))
    }

    /// What the content at a path in this target is of: its track, if any,
    /// and its album.
    pub fn of(&self, path: &str, hash: &str) -> Option<(Option<i64>, String)> {
        self.at(path)
            .filter(|e| e.hash == hash)
            .map(|e| (e.track, e.album.clone()))
    }
}

/// A path as storage that ignores case compares it, as device paths are
/// kept apart (see `coversync_core::sync`).
fn case_key(path: &str) -> String {
    path.to_uppercase()
}

/// A cover file a target needs, not made yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverWanted {
    pub version: String,
    /// 0: the art as it is.
    pub width: u32,
    pub source: CoverSource,
}

pub struct Inputs<'a> {
    pub paths: &'a DevicePaths,
    pub settings: &'a DeviceSettings,
    pub capabilities: &'a Capabilities,
    /// Each album's art: where it is, and its version.
    pub art: &'a HashMap<String, (CoverSource, String)>,
    /// Cover files made, by (art version, width).
    pub covers: &'a HashMap<(String, u32), CoverFile>,
    /// Where those are.
    pub cover_dir: &'a Path,
    /// Cover files that couldn't be made, by (art version, width).
    pub failed: &'a HashSet<(String, u32)>,
}

/// The device's target, and the cover files it still needs made.
pub fn build(i: Inputs) -> (Target, Vec<CoverWanted>) {
    let caps = i.capabilities;
    let fits = |path: &str| {
        super::api::valid_path(path)
            && caps
                .max_path_bytes
                .is_none_or(|max| path.len() <= max as usize)
            && caps
                .max_name_bytes
                .is_none_or(|max| path.split('/').all(|name| name.len() <= max as usize))
    };
    // Albums in the order their puts go: most recently added first, then by
    // where they are.
    let mut albums: HashMap<&str, (i64, &str)> = HashMap::new();
    for f in &i.paths.files {
        let a = albums
            .entry(f.album_id.as_str())
            .or_insert((f.content.added_at, f.path.as_str()));
        a.0 = a.0.max(f.content.added_at);
        a.1 = a.1.min(f.path.as_str());
    }

    let mut target = Target::default();
    // (album, 0 for its cover or 1 for a track, the entry)
    let mut ready: Vec<(&str, u8, Entry)> = Vec::new();
    for f in &i.paths.files {
        if !fits(&f.path) {
            target.too_long += 1;
            continue;
        }
        let Some(hash) = &f.content.hash else {
            target.preparing += 1;
            target.waiting_tracks.insert(f.track_id);
            target.waiting_sizes.insert(f.content.size);
            continue;
        };
        target
            .sources
            .entry(hash.clone())
            .or_insert_with(|| Source::Library {
                path: i.paths.root.join(&f.source),
                size: f.content.size,
                mtime_ns: f.content.mtime_ns,
            });
        ready.push((
            &f.album_id,
            1,
            Entry {
                path: f.path.clone(),
                hash: hash.clone(),
                size: f.content.size,
                track: Some(f.track_id),
                album: f.album_id.clone(),
            },
        ));
    }

    let mut wanted = Vec::new();
    if i.settings.art.cover_file {
        // A folder gets a cover only when it holds one album's tracks: a
        // shared cover would be wrong for the others. Folders are told apart
        // as storage that ignores case does, and a cover never takes a name
        // a track or folder has.
        let mut folders: HashMap<String, (&str, HashSet<&str>)> = HashMap::new();
        let mut taken: HashSet<String> = HashSet::new();
        for f in &i.paths.files {
            let dir = f.path.rsplit_once('/').map_or("", |(dir, _)| dir);
            folders
                .entry(case_key(dir))
                .or_insert((dir, HashSet::new()))
                .1
                .insert(&f.album_id);
            taken.insert(case_key(&f.path));
            for (n, _) in f.path.match_indices('/') {
                taken.insert(case_key(&f.path[..n]));
            }
        }
        let width = i.settings.art.max_width.unwrap_or(0);
        let mut asked: HashSet<&str> = HashSet::new();
        let mut folders: Vec<_> = folders.into_values().collect();
        folders.sort_unstable_by(|a, b| a.0.cmp(b.0));
        for (dir, in_it) in folders {
            let [album] = in_it.into_iter().collect::<Vec<_>>()[..] else {
                continue;
            };
            let stem = if dir.is_empty() {
                i.settings.art.cover_name.clone()
            } else {
                format!("{dir}/{}", i.settings.art.cover_name)
            };
            let Some((source, version)) = i.art.get(album) else {
                continue;
            };
            let key = (version.clone(), width);
            // Art that couldn't be read gets no cover, rather than one
            // being prepared for ever.
            if i.failed.contains(&key) {
                continue;
            }
            let Some(made) = i.covers.get(&key) else {
                target.preparing += 1;
                target.waiting_covers.insert(album.to_string());
                target.waiting_cover_stems.insert(stem);
                if asked.insert(version) {
                    wanted.push(CoverWanted {
                        version: version.clone(),
                        width,
                        source: source.clone(),
                    });
                }
                continue;
            };
            let path = format!("{stem}.{}", made.ext);
            if taken.contains(&case_key(&path)) {
                continue;
            }
            if !fits(&path) {
                target.too_long += 1;
                continue;
            }
            target
                .sources
                .entry(made.hash.clone())
                .or_insert_with(|| Source::Cover {
                    path: i.cover_dir.join(format!("{}.{}", made.hash, made.ext)),
                    size: made.size,
                });
            ready.push((
                album,
                0,
                Entry {
                    path,
                    hash: made.hash.clone(),
                    size: made.size,
                    track: None,
                    album: album.to_string(),
                },
            ));
        }
    }

    ready.sort_unstable_by(|a, b| {
        let (ra, rb) = (albums[a.0], albums[b.0]);
        rb.0.cmp(&ra.0)
            .then(ra.1.cmp(rb.1))
            .then(a.0.cmp(b.0))
            .then(a.1.cmp(&b.1))
            .then(a.2.path.cmp(&b.2.path))
    });
    target.entries = ready.into_iter().map(|(_, _, e)| e).collect();
    target.paths = target
        .entries
        .iter()
        .enumerate()
        .map(|(n, e)| (e.path.clone(), n))
        .collect();

    let mut hasher = blake3::Hasher::new();
    for e in &target.entries {
        hasher.update(e.path.as_bytes());
        hasher.update(b"\0");
        hasher.update(e.hash.as_bytes());
        hasher.update(b"\n");
    }
    hasher.update(&target.preparing.to_le_bytes());
    hasher.update(&target.too_long.to_le_bytes());
    target.version = format!("t_{}", &hasher.finalize().to_hex()[..32]);
    (target, wanted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use coversync_core::model::LibraryVersion;
    use coversync_core::sync::{Content, DeviceFile};

    fn file(path: &str, album: &str, hash: Option<&str>, added: i64) -> DeviceFile {
        DeviceFile {
            track_id: -1,
            album_id: album.into(),
            path: path.into(),
            source: format!("lib/{path}"),
            content: Content {
                size: 10,
                mtime_ns: 1,
                added_at: added,
                hash: hash.map(str::to_string),
            },
        }
    }

    fn paths(files: Vec<DeviceFile>) -> DevicePaths {
        DevicePaths {
            library: LibraryVersion {
                id: "l".into(),
                generation: 1,
            },
            root: PathBuf::from("/music"),
            settings_version: 1,
            separator: ";".into(),
            incomplete: 0,
            renamed: 0,
            files,
        }
    }

    #[test]
    fn covers_go_where_one_album_is_and_new_albums_first() {
        let paths = paths(vec![
            file("Old/1.flac", "old", Some("h1"), 100),
            file("New/1.flac", "new", Some("h2"), 200),
            file("New/2.flac", "new", None, 200),
            file("Mixed/a.flac", "a", Some("h3"), 50),
            file("Mixed/b.flac", "b", Some("h4"), 50),
        ]);
        let art: HashMap<String, (CoverSource, String)> = ["old", "new", "a"]
            .iter()
            .map(|a| {
                (
                    a.to_string(),
                    (
                        CoverSource::File(PathBuf::from(format!("/{a}.jpg"))),
                        format!("v-{a}"),
                    ),
                )
            })
            .collect();
        let covers = HashMap::from([(
            ("v-new".to_string(), 0),
            CoverFile {
                hash: "c-new".into(),
                size: 3,
                ext: "png".into(),
            },
        )]);
        let settings = DeviceSettings::default();
        let (target, wanted) = build(Inputs {
            paths: &paths,
            settings: &settings,
            capabilities: &Capabilities::default(),
            art: &art,
            covers: &covers,
            cover_dir: Path::new("/cache"),
            failed: &HashSet::new(),
        });
        let got: Vec<&str> = target.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            got,
            [
                "New/cover.png",
                "New/1.flac",
                "Old/1.flac",
                "Mixed/a.flac",
                "Mixed/b.flac"
            ]
        );
        // New/2.flac isn't hashed yet, and Old's cover isn't made; Mixed has
        // two albums, so no cover.
        assert_eq!(target.preparing, 2);
        assert_eq!(wanted.len(), 1);
        assert_eq!((wanted[0].version.as_str(), wanted[0].width), ("v-old", 0));
        assert_eq!(
            target.source("c-new"),
            Some(&Source::Cover {
                path: PathBuf::from("/cache/c-new.png"),
                size: 3
            })
        );
        assert_eq!(
            target.source("h1"),
            Some(&Source::Library {
                path: PathBuf::from("/music/lib/Old/1.flac"),
                size: 10,
                mtime_ns: 1
            })
        );
        // A path too long for the device is left out and counted.
        let short = Capabilities {
            max_path_bytes: Some(11),
            ..Default::default()
        };
        let (limited, _) = build(Inputs {
            paths: &paths,
            settings: &settings,
            capabilities: &short,
            art: &art,
            covers: &covers,
            cover_dir: Path::new("/cache"),
            failed: &HashSet::new(),
        });
        assert_eq!(limited.too_long, 3);
        assert_ne!(limited.version, target.version);
    }
}
