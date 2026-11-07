//! The change counters through each path that writes tracks: scans, imports
//! and tag edits, on real (tiny) audio files.

use std::fs;
use std::path::{Path, PathBuf};

use coversync_core::cover::CoverCache;
use coversync_core::import::{self, Edits};
use coversync_core::scan::{self, Progress};
use coversync_core::{
    Collection, InboxSetup, IngestMode, Library, LibrarySetup, LibraryVersion, TagEdit, tags,
};

struct Setup {
    inbox: PathBuf,
    root: PathBuf,
    lib: Library,
    covers: CoverCache,
    progress: Progress,
}

/// An Inbox and a Library folder in a fresh test folder.
fn setup(name: &str) -> Setup {
    let dir = std::env::temp_dir().join(format!("coversync-counter-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    for sub in ["Inbox", "Lib", "cache"] {
        fs::create_dir_all(dir.join(sub)).unwrap();
    }
    let dir = dir.canonicalize().unwrap();
    let (inbox, root) = (dir.join("Inbox"), dir.join("Lib"));
    let lib = Library::open_in(&dir.join("state")).unwrap();
    lib.set_inbox_setup(InboxSetup {
        folder: Some(inbox.display().to_string()),
        ..Default::default()
    })
    .unwrap();
    lib.set_library_setup(LibrarySetup {
        folder: Some(root.display().to_string()),
        structure: "{album}/{track_number} - {title}".into(),
        ..Default::default()
    })
    .unwrap();
    Setup {
        inbox,
        root,
        lib,
        covers: CoverCache::new(dir.join("cache")).unwrap(),
        progress: Progress::default(),
    }
}

/// A tiny silent WAV tagged with `title`, `album` and track number `n`.
fn wav(path: &Path, title: &str, album: &str, n: u32) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
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
    retag(
        path,
        TagEdit {
            title: Some(title.into()),
            album: Some(album.into()),
            track_no: Some(n.to_string()),
            ..Default::default()
        },
    );
}

/// Write `edit` to a file and move its modification time on, as a scan
/// only reads files whose time or size changed.
fn retag(path: &Path, edit: TagEdit) {
    let before = fs::metadata(path).ok().and_then(|m| m.modified().ok());
    tags::write(path, &edit).unwrap();
    if let Some(t) = before {
        let file = fs::File::options().write(true).open(path).unwrap();
        file.set_modified(t + std::time::Duration::from_secs(10))
            .unwrap();
    }
}

impl Setup {
    fn scan(&self) {
        let roots = [
            (self.inbox.clone(), Collection::Inbox),
            (self.root.clone(), Collection::Library),
        ];
        scan::scan(&self.lib, &self.covers, &roots, &self.progress).unwrap();
    }

    fn library(&self) -> LibraryVersion {
        self.lib.index_versions().unwrap().library.unwrap()
    }

    fn inbox_generation(&self) -> i64 {
        self.lib.index_versions().unwrap().inbox.generation
    }

    /// Library tracks changed since `since`: path, album id, generation.
    fn changed(&self, since: i64) -> Vec<(String, String, i64)> {
        let mut out: Vec<_> = self
            .lib
            .library_changes(since)
            .unwrap()
            .unwrap()
            .tracks
            .into_iter()
            .map(|t| (t.path, t.album_id, t.generation))
            .collect();
        out.sort();
        out
    }

    fn changed_paths(&self, since: i64) -> Vec<(String, i64)> {
        self.changed(since)
            .into_iter()
            .map(|(p, _, g)| (p, g))
            .collect()
    }

    fn removed(&self, since: i64) -> Vec<(String, i64)> {
        let mut out: Vec<_> = self
            .lib
            .library_changes(since)
            .unwrap()
            .unwrap()
            .removed
            .into_iter()
            .map(|t| (t.path, t.generation))
            .collect();
        out.sort();
        out
    }

    fn inbox_album(&self) -> String {
        self.lib
            .albums(Collection::Inbox, &[], &[], "", &[], None)
            .unwrap()[0]
            .id
            .clone()
    }

    fn import(&self, album_id: &str) -> import::ImportResult {
        import::execute(
            &self.lib,
            &self.covers,
            album_id,
            &Edits::new(),
            None,
            &self.progress,
        )
        .unwrap()
    }
}

#[test]
fn an_import_counts_one_change_to_each_collection() {
    let s = setup("import");
    wav(&s.inbox.join("Rec/1.wav"), "One", "Record", 1);
    wav(&s.inbox.join("Rec/2.wav"), "Two", "Record", 2);
    s.scan();
    assert_eq!(s.inbox_generation(), 1);
    // The Library folder is indexed in memory until the first import.
    let before = s.library();
    assert_eq!(before.generation, 0);

    let result = s.import(&s.inbox_album());
    let after = s.library();
    assert_eq!(after.id, before.id, "the database carries the index on");
    assert_eq!(after.generation, 1);
    assert_eq!(
        s.changed(0),
        [
            ("Record/01 - One.wav".into(), result.album_id.clone(), 1),
            ("Record/02 - Two.wav".into(), result.album_id.clone(), 1),
        ]
    );
    // Copy mode hides the originals, which changes the Inbox.
    assert_eq!(s.inbox_generation(), 2);

    // A scan that finds nothing new counts nothing.
    s.scan();
    assert_eq!(s.library().generation, 1);
    assert_eq!(s.inbox_generation(), 2);
}

#[test]
fn a_move_import_counts_the_inbox_files_it_takes() {
    let s = setup("importmove");
    let mut setup = s.lib.inbox_setup().unwrap();
    setup.ingest = IngestMode::Move;
    s.lib.set_inbox_setup(setup).unwrap();
    wav(&s.inbox.join("Rec/1.wav"), "One", "Record", 1);
    s.scan();
    assert_eq!(s.inbox_generation(), 1);
    s.import(&s.inbox_album());
    assert!(!s.inbox.join("Rec/1.wav").exists());
    assert_eq!(s.inbox_generation(), 2);
    assert_eq!(s.library().generation, 1);
    s.scan();
    assert_eq!((s.inbox_generation(), s.library().generation), (2, 1));
}

#[test]
fn a_tag_edit_counts_a_change_to_its_tracks_even_when_the_album_id_changes() {
    let s = setup("tags");
    wav(&s.inbox.join("Rec/1.wav"), "One", "Record", 1);
    wav(&s.inbox.join("Rec/2.wav"), "Two", "Record", 2);
    s.scan();
    let old_album = s.import(&s.inbox_album()).album_id;
    assert_eq!(s.library().generation, 1);

    // What the tag editor's save does: write the files, index them again,
    // and drop covers of albums left without tracks.
    let ids = s.lib.album_track_ids(&old_album).unwrap();
    let mut stats = Vec::new();
    for id in ids {
        let (path, root) = s.lib.track_root(id).unwrap().unwrap();
        retag(
            &path,
            TagEdit {
                album: Some("Renamed".into()),
                ..Default::default()
            },
        );
        stats.push(scan::stat(path, &root, Collection::Library).unwrap());
    }
    scan::index_files(&s.lib, stats, &s.progress).unwrap();
    s.lib.remove_paths(&[]).unwrap();

    assert_eq!(s.library().generation, 2);
    // The album's id changed with its title; its tracks, at the same
    // paths, carry the change.
    let changed = s.changed(1);
    assert_eq!(changed.len(), 2);
    let new_album = &changed[0].1;
    assert_ne!(*new_album, old_album);
    assert!(
        changed
            .iter()
            .all(|(_, album, g)| album == new_album && *g == 2)
    );
    assert_eq!(
        s.changed_paths(1),
        [
            ("Record/01 - One.wav".into(), 2),
            ("Record/02 - Two.wav".into(), 2)
        ]
    );
    assert!(s.removed(0).is_empty());
    s.scan();
    assert_eq!(s.library().generation, 2);
}

#[test]
fn a_scan_counts_new_changed_removed_and_moved_files() {
    let s = setup("scan");
    s.lib.prepare_library(&s.root).unwrap();
    wav(&s.root.join("A/1.wav"), "One", "A", 1);
    wav(&s.root.join("A/2.wav"), "Two", "A", 2);
    s.scan();
    assert_eq!(s.library().generation, 1);
    assert_eq!(
        s.changed_paths(0),
        [("A/1.wav".into(), 1), ("A/2.wav".into(), 1)]
    );
    s.scan();
    assert_eq!(s.library().generation, 1, "nothing changed");

    // Changed outside coversync.
    retag(
        &s.root.join("A/1.wav"),
        TagEdit {
            title: Some("First".into()),
            ..Default::default()
        },
    );
    s.scan();
    assert_eq!(s.library().generation, 2);
    assert_eq!(s.changed_paths(1), [("A/1.wav".into(), 2)]);

    // Deleted.
    fs::remove_file(s.root.join("A/2.wav")).unwrap();
    s.scan();
    assert_eq!(s.library().generation, 3);
    assert!(s.changed_paths(2).is_empty());
    assert_eq!(s.removed(2), [("A/2.wav".into(), 3)]);

    // Moved, as a change of folder structure would: the new paths are new
    // tracks, the old ones removed.
    fs::rename(s.root.join("A"), s.root.join("B")).unwrap();
    s.scan();
    let now = s.library().generation;
    assert!(now > 3);
    assert_eq!(s.changed_paths(3), [("B/1.wav".into(), 4)]);
    assert_eq!(s.removed(3), [("A/1.wav".into(), now)]);
    s.scan();
    assert_eq!(s.library().generation, now);
}
