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

    /// The id of the album of `collection` titled `title`.
    fn album(&self, collection: Collection, title: &str) -> String {
        self.lib
            .albums(collection, &[], &[], "", &[], None)
            .unwrap()
            .into_iter()
            .find(|a| a.title == title)
            .unwrap_or_else(|| panic!("no album {title}"))
            .id
    }

    /// An album's tracks as its details give them: ids and paths.
    fn tracks_of(&self, album: &str) -> Vec<(i64, String)> {
        let detail = self.lib.album(album).unwrap().unwrap();
        detail
            .tracks
            .into_iter()
            .map(|t| (t.id, t.rel_path))
            .collect()
    }

    /// An album's version, as its details give it.
    fn version(&self, album: &str) -> String {
        self.lib.album(album).unwrap().unwrap().version
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
fn a_scan_and_an_import_count_each_batch_they_commit() {
    let s = setup("batches");
    // More files than one batch (256) holds.
    for i in 1..=300 {
        wav(
            &s.inbox.join(format!("Rec/{i:03}.wav")),
            &format!("T{i}"),
            "Record",
            i,
        );
    }
    s.scan();
    assert_eq!(s.inbox_generation(), 2, "two batches");
    s.import(&s.inbox_album());
    let mut per_generation = std::collections::BTreeMap::new();
    for (_, _, g) in s.changed(0) {
        *per_generation.entry(g).or_insert(0) += 1;
    }
    assert_eq!(
        per_generation.into_iter().collect::<Vec<_>>(),
        [(1, 256), (2, 44)]
    );
    assert_eq!(s.library().generation, 2);
    // Hiding the 300 originals is one more change to the Inbox.
    assert_eq!(s.inbox_generation(), 3);
    s.scan();
    assert_eq!((s.inbox_generation(), s.library().generation), (3, 2));
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
fn an_import_that_fails_to_index_its_copies_counts_nothing() {
    use std::os::unix::fs::PermissionsExt;
    let s = setup("importfail");
    s.lib.prepare_library(&s.root).unwrap();
    wav(&s.inbox.join("Rec/1.wav"), "One", "Record", 1);
    wav(&s.inbox.join("Rec/2.wav"), "Two", "Record", 2);
    s.scan();
    let before = s.lib.index_versions().unwrap();
    // The files can be copied, but the Library database can't take them:
    // its folder, which holds the journal, is read-only.
    let own = s.root.join(".coversync");
    let mode = |m| fs::set_permissions(&own, fs::Permissions::from_mode(m)).unwrap();
    mode(0o555);
    if fs::create_dir(own.join("probe")).is_ok() {
        return; // Running as root: permissions don't stop writes.
    }
    let result = import::execute(
        &s.lib,
        &s.covers,
        &s.inbox_album(),
        &Edits::new(),
        None,
        &s.progress,
    );
    mode(0o755);
    assert!(
        matches!(result, Err(coversync_core::Error::Db(_))),
        "{:?}",
        result.err()
    );
    // The copies were made (their folder is left), then taken back out; the
    // originals stay in the Inbox, shown. Nothing was committed, so nothing
    // is counted or removed.
    assert!(s.root.join("Record").is_dir());
    assert!(!s.root.join("Record/01 - One.wav").exists());
    assert!(!s.root.join("Record/02 - Two.wav").exists());
    assert_eq!(s.lib.index_versions().unwrap(), before);
    assert!(s.changed(-1).is_empty());
    assert!(s.removed(-1).is_empty());
    assert_eq!(
        s.lib.album(&s.inbox_album()).unwrap().unwrap().tracks.len(),
        2
    );
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

/// Retag a file with a genre, as an edit made outside coversync would.
fn regenre(path: &Path, genre: &str) {
    retag(
        path,
        TagEdit {
            genre: Some(genre.into()),
            ..Default::default()
        },
    );
}

#[test]
fn an_inbox_albums_version_changes_with_its_tracks_and_only_then() {
    let s = setup("version-inbox");
    for n in 1..=3 {
        wav(&s.inbox.join(format!("Rec/{n}.wav")), "T", "Record", n);
    }
    for n in 1..=2 {
        wav(&s.inbox.join(format!("Other/{n}.wav")), "T", "Other", n);
    }
    s.scan();
    let (record, other) = (
        s.album(Collection::Inbox, "Record"),
        s.album(Collection::Inbox, "Other"),
    );
    let v0 = s.version(&record);
    assert_eq!(v0.len(), 16);
    assert_eq!(s.version(&record), v0, "the same when asked again");
    assert_ne!(s.version(&other), v0);
    s.scan();
    assert_eq!(
        s.version(&record),
        v0,
        "a scan that finds nothing changes nothing"
    );

    // Another album's change leaves this one's version alone.
    let other_before = s.version(&other);
    regenre(&s.inbox.join("Other/1.wav"), "Jazz");
    s.scan();
    assert_eq!(s.version(&record), v0);
    assert_ne!(s.version(&other), other_before);

    // Any of its tracks written.
    regenre(&s.inbox.join("Rec/3.wav"), "Rock");
    s.scan();
    let v1 = s.version(&record);
    assert_ne!(v1, v0);

    // A track removed that wasn't the latest written: the highest
    // generation among those left stays as it was.
    fs::remove_file(s.inbox.join("Rec/1.wav")).unwrap();
    s.scan();
    let v2 = s.version(&record);
    assert_eq!(s.lib.album(&record).unwrap().unwrap().tracks.len(), 2);
    assert!(![&v0, &v1].contains(&&v2));

    // A track added.
    wav(&s.inbox.join("Rec/4.wav"), "T", "Record", 4);
    s.scan();
    let v3 = s.version(&record);
    assert!(![&v0, &v1, &v2].contains(&&v3));
    s.scan();
    assert_eq!(s.version(&record), v3);
}

#[test]
fn a_library_albums_version_changes_with_its_tracks_and_only_then() {
    let s = setup("version-library");
    for n in 1..=3 {
        wav(
            &s.inbox.join(format!("Rec/{n}.wav")),
            &format!("T{n}"),
            "Record",
            n,
        );
    }
    s.scan();
    let album = s.import(&s.inbox_album()).album_id;
    let v0 = s.version(&album);
    s.scan();
    assert_eq!(s.version(&album), v0);

    // The last track written, then the first removed: the tracks left
    // still top out at the generation of that write.
    regenre(&s.root.join("Record/03 - T3.wav"), "Rock");
    s.scan();
    let v1 = s.version(&album);
    assert_ne!(v1, v0);
    let written = s.library().generation;
    fs::remove_file(s.root.join("Record/01 - T1.wav")).unwrap();
    s.scan();
    assert_eq!(
        s.changed_paths(written - 1),
        [("Record/03 - T3.wav".into(), written)]
    );
    let v2 = s.version(&album);
    assert!(![&v0, &v1].contains(&&v2));

    wav(&s.root.join("Record/04 - T4.wav"), "T4", "Record", 4);
    s.scan();
    assert!(![&v0, &v1, &v2].contains(&&s.version(&album)));
}

#[test]
fn an_albums_tracks_find_it_under_a_new_id() {
    let s = setup("album-of-tracks");
    for n in 1..=3 {
        wav(&s.inbox.join(format!("Rec/{n}.wav")), "T", "Record", n);
    }
    s.scan();
    let record = s.album(Collection::Inbox, "Record");
    let tracks = s.tracks_of(&record);
    assert_eq!(
        s.lib.album_of_tracks(&tracks).unwrap().as_deref(),
        Some(record.as_str())
    );
    // A track counts only at the path it had: its id can name another file.
    let moved: Vec<(i64, String)> = tracks
        .iter()
        .map(|(id, path)| (*id, format!("{path}.old")))
        .collect();
    assert_eq!(s.lib.album_of_tracks(&moved).unwrap(), None);

    // A new title gives the album a new id.
    for n in 1..=3 {
        retag(
            &s.inbox.join(format!("Rec/{n}.wav")),
            TagEdit {
                album: Some("Renamed".into()),
                ..Default::default()
            },
        );
    }
    s.scan();
    let renamed = s.album(Collection::Inbox, "Renamed");
    assert_ne!(renamed, record);
    assert_eq!(
        s.lib.album_of_tracks(&tracks).unwrap().as_deref(),
        Some(renamed.as_str())
    );

    // Split across albums: the one most of them are in.
    retag(
        &s.inbox.join("Rec/1.wav"),
        TagEdit {
            album: Some("Single".into()),
            ..Default::default()
        },
    );
    s.scan();
    assert_eq!(
        s.lib.album_of_tracks(&tracks).unwrap().as_deref(),
        Some(renamed.as_str())
    );
    let single = s.album(Collection::Inbox, "Single");
    let first = s.tracks_of(&single);
    assert_eq!(first.len(), 1);
    assert_eq!(
        s.lib.album_of_tracks(&first).unwrap().as_deref(),
        Some(single.as_str())
    );

    // Imported in Copy mode: the Inbox files are hidden, and the Library's
    // copies are found by their own ids.
    let library = s.import(&renamed).album_id;
    assert_eq!(
        s.lib.album_of_tracks(&tracks).unwrap().as_deref(),
        Some(single.as_str())
    );
    let hidden: Vec<(i64, String)> = tracks
        .iter()
        .filter(|t| !first.contains(t))
        .cloned()
        .collect();
    assert_eq!(s.lib.album_of_tracks(&hidden).unwrap(), None);
    let copies = s.tracks_of(&library);
    assert!(copies.iter().all(|(id, _)| *id < 0));
    assert_eq!(
        s.lib.album_of_tracks(&copies).unwrap().as_deref(),
        Some(library.as_str())
    );
    assert_eq!(s.lib.album_of_tracks(&[]).unwrap(), None);
    // An id no track can have, as a client may send.
    assert_eq!(
        s.lib.album_of_tracks(&[(i64::MIN, "x".into())]).unwrap(),
        None
    );
    assert_eq!(s.lib.track_path(i64::MIN).unwrap(), None);
}

#[test]
fn imports_and_background_tag_rehashes_publish_hashes_of_the_final_bytes() {
    use coversync_core::hashes;
    let s = setup("hash-import");
    wav(&s.inbox.join("Rec/1.wav"), "One", "Record", 1);
    s.scan();
    let album = s.import(&s.inbox_album()).album_id;
    let path = s
        .lib
        .track_path(s.lib.album_track_ids(&album).unwrap()[0])
        .unwrap()
        .unwrap();
    let rel = path.strip_prefix(&s.root).unwrap();
    let before = s.lib.library_hashes().unwrap()[rel].clone();
    assert_eq!(before, hashes::file(&path).unwrap());
    assert!(before.hash.starts_with("blake3:"));
    assert!(s.lib.hash_jobs().unwrap().is_empty());
    let generation = s.library().generation;
    s.scan();
    assert_eq!(s.lib.library_hashes().unwrap()[rel], before);
    assert_eq!(s.library().generation, generation);

    regenre(&path, "Jazz");
    scan::index_files(
        &s.lib,
        vec![scan::stat(path.clone(), &s.root, Collection::Library).unwrap()],
        &s.progress,
    )
    .unwrap();
    assert!(s.lib.library_hashes().unwrap().is_empty());
    let saved_version = s.version(&album);
    hashes::run(&s.lib, &Default::default()).unwrap();
    assert_eq!(
        s.version(&album),
        saved_version,
        "rehashing must not cause a tag-save conflict"
    );
    let after = s.lib.library_hashes().unwrap()[rel].clone();
    assert_ne!(after.hash, before.hash);
    assert_eq!(after, hashes::file(&path).unwrap());
    assert_eq!(s.library().generation, generation + 2);
}

#[test]
fn scans_notice_same_size_changes_within_one_second_and_backfill_hashes() {
    use coversync_core::hashes;
    use std::time::{Duration, UNIX_EPOCH};
    let s = setup("hash-scan");
    let path = s.root.join("A/1.wav");
    wav(&path, "One", "A", 1);
    let timestamp = |ns| {
        fs::File::open(&path)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::new(1_700_000_000, ns))
            .unwrap()
    };
    timestamp(100);
    s.scan();
    assert!(s.lib.library_hashes().unwrap().is_empty());
    let progress = hashes::Progress::default();
    let album = s.album(Collection::Library, "A");
    let version = s.version(&album);
    hashes::run(&s.lib, &progress).unwrap();
    assert_eq!(s.version(&album), version);
    let before = s.lib.library_hashes().unwrap()[Path::new("A/1.wav")].clone();
    let generation = s.library().generation;
    assert_eq!(progress.read().unwrap().files_done, 1);
    assert!(!progress.read().unwrap().running);
    // Replace just an audio byte: tags and file length stay unchanged.
    let mut bytes = fs::read(&path).unwrap();
    let offset = bytes.windows(4).position(|w| w == b"data").unwrap() + 8;
    bytes[offset] ^= 1;
    fs::write(&path, &bytes).unwrap();
    timestamp(200);
    s.scan();
    assert!(
        s.lib.library_hashes().unwrap().is_empty(),
        "stale digest was invalidated"
    );
    hashes::run(&s.lib, &progress).unwrap();
    let after = s.lib.library_hashes().unwrap()[Path::new("A/1.wav")].clone();
    assert_eq!(after.size, before.size);
    assert_ne!(after.hash, before.hash);
    assert_eq!(after, hashes::file(&path).unwrap());
    assert!(s.library().generation > generation);
    let generation = s.library().generation;
    s.scan();
    hashes::run(&s.lib, &progress).unwrap();
    assert_eq!(progress.read().unwrap().files_done, 0);
    assert_eq!(s.library().generation, generation);
}

#[test]
fn a_background_hash_cannot_overwrite_a_newer_row_or_another_library() {
    use coversync_core::hashes;
    let s = setup("hash-lease");
    let path = s.root.join("A/1.wav");
    wav(&path, "One", "A", 1);
    s.scan();
    let job = s.lib.hash_jobs().unwrap().pop().unwrap();
    let hash = hashes::file(&path).unwrap();
    // A newer index transaction can preserve the exact metadata.
    scan::index_files(
        &s.lib,
        vec![scan::stat(path.clone(), &s.root, Collection::Library).unwrap()],
        &s.progress,
    )
    .unwrap();
    assert_eq!(s.lib.store_hashes(&[(&job, hash.clone())]).unwrap(), 0);
    assert!(s.lib.library_hashes().unwrap().is_empty());
    let job = s.lib.hash_jobs().unwrap().pop().unwrap();
    let other = s.root.with_file_name("Other");
    fs::create_dir_all(&other).unwrap();
    let mut setup = s.lib.library_setup().unwrap();
    setup.folder = Some(other.display().to_string());
    s.lib.set_library_setup(setup).unwrap();
    assert_eq!(s.lib.store_hashes(&[(&job, hash)]).unwrap(), 0);
    assert!(s.lib.library_hashes().unwrap().is_empty());
}

#[test]
fn hash_publication_rechecks_disk_and_survives_persisting_the_library() {
    use coversync_core::hashes;
    let s = setup("hash-persist");
    let path = s.root.join("A/1.wav");
    wav(&path, "One", "A", 1);
    s.scan();
    let job = s.lib.hash_jobs().unwrap().pop().unwrap();
    let hash = hashes::file(&path).unwrap();
    regenre(&path, "Rock");
    assert_eq!(s.lib.store_hashes(&[(&job, hash)]).unwrap(), 0);
    s.scan();
    hashes::run(&s.lib, &Default::default()).unwrap();
    let hashes = s.lib.library_hashes().unwrap();
    assert_eq!(hashes.len(), 1);
    s.lib.prepare_library(&s.root).unwrap();
    assert_eq!(s.lib.library_hashes().unwrap(), hashes);
    s.lib.refresh_library().unwrap();
    assert_eq!(s.lib.library_hashes().unwrap(), hashes);
}

#[test]
fn old_daemon_writes_invalidate_hashes_and_a_cancelled_pass_can_resume() {
    use coversync_core::hashes;
    use std::sync::atomic::AtomicBool;
    let s = setup("hash-compat");
    let path = s.root.join("A/1.wav");
    wav(&path, "One", "A", 1);
    s.scan();
    s.lib.prepare_library(&s.root).unwrap();
    let progress = Default::default();
    hashes::run_cancellable(&s.lib, &progress, &AtomicBool::new(true)).unwrap();
    assert!(s.lib.library_hashes().unwrap().is_empty());
    assert_eq!(progress.read().unwrap().files_done, 0);
    assert_eq!(progress.read().unwrap().errors, 0);
    assert!(!progress.read().unwrap().running);
    hashes::run(&s.lib, &progress).unwrap();
    assert_eq!(s.lib.library_hashes().unwrap().len(), 1);
    let own = rusqlite::Connection::open(s.root.join(coversync_core::db::LIBRARY_DB)).unwrap();
    // The old upsert writes these columns, even when its second timestamp
    // and size did not notice a tag edit. It knows nothing about hash columns.
    own.execute("UPDATE tracks SET mtime = mtime, size = size", [])
        .unwrap();
    assert!(s.lib.library_hashes().unwrap().is_empty());
    drop(own);
    s.lib.refresh_library().unwrap();
    hashes::run(&s.lib, &progress).unwrap();
    assert_eq!(s.lib.library_hashes().unwrap().len(), 1);
}

#[cfg(unix)]
#[test]
fn a_failed_parallel_import_joins_workers_before_removing_their_copies() {
    use std::os::unix::fs::PermissionsExt;
    let s = setup("hash-import-rollback");
    let mut setup = s.lib.library_setup().unwrap();
    setup.structure = "{album}/CD{disc_number}/{track_number} - {title}".into();
    s.lib.set_library_setup(setup).unwrap();
    for disc in 1..=2 {
        let path = s.inbox.join(format!("Record/CD{disc}/1.wav"));
        wav(&path, "One", "Record", 1);
        retag(
            &path,
            TagEdit {
                disc_no: Some(disc.to_string()),
                ..Default::default()
            },
        );
    }
    s.scan();
    let readonly = s.root.join("Record/CD2");
    fs::create_dir_all(&readonly).unwrap();
    fs::set_permissions(&readonly, fs::Permissions::from_mode(0o555)).unwrap();
    if fs::File::create(readonly.join("probe")).is_ok() {
        fs::set_permissions(&readonly, fs::Permissions::from_mode(0o755)).unwrap();
        return; // Root bypasses permissions.
    }
    assert!(
        import::plan(&s.lib, &s.inbox_album(), &Edits::new(), None)
            .unwrap()
            .ready()
    );
    let result = import::execute(
        &s.lib,
        &s.covers,
        &s.inbox_album(),
        &Edits::new(),
        None,
        &s.progress,
    );
    fs::set_permissions(&readonly, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(result.is_err());
    assert!(!s.root.join("Record/CD1/01 - One.wav").exists());
    assert!(!s.root.join("Record/CD2/01 - One.wav").exists());
    assert!(s.lib.library_hashes().unwrap().is_empty());
    assert_eq!(
        s.lib.album(&s.inbox_album()).unwrap().unwrap().tracks.len(),
        2
    );
}

#[test]
fn an_unreadable_hash_job_does_not_prevent_other_files_from_finishing() {
    use coversync_core::hashes;
    let s = setup("hash-error");
    for n in 1..=2 {
        wav(&s.root.join(format!("A/{n}.wav")), "One", "A", n);
    }
    s.scan();
    fs::rename(s.root.join("A/1.wav"), s.root.join("A/1.gone")).unwrap();
    let progress = Default::default();
    hashes::run(&s.lib, &progress).unwrap();
    let p = progress.read().unwrap();
    assert_eq!((p.files_total, p.files_done, p.errors), (2, 2, 1));
    assert!(!p.running);
    assert_eq!(s.lib.library_hashes().unwrap().len(), 1);
}
