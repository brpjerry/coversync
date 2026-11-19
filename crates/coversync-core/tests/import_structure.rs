//! What an import plan makes of a folder structure with optional parts, on
//! real (tiny) audio files: which values have to agree across an album.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use coversync_core::cover::CoverCache;
use coversync_core::import::{self, Edits, ImportPlan};
use coversync_core::scan::{self, Progress};
use coversync_core::substitute::Substitution;
use coversync_core::{Collection, InboxSetup, Library, LibrarySetup, TagEdit, tags};

struct Setup {
    root: PathBuf,
    lib: Library,
    album: String,
    /// The album's two tracks' ids, in order.
    tracks: [i64; 2],
}

/// An Inbox album "Record" of two tracks, One and Two, numbered 1 and 2,
/// and a Library folder with `structure`.
fn setup(name: &str, structure: &str) -> Setup {
    let dir =
        std::env::temp_dir().join(format!("coversync-structure-{}-{name}", std::process::id()));
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
        structure: structure.into(),
        ..Default::default()
    })
    .unwrap();
    for (n, title) in [(1, "One"), (2, "Two")] {
        track(&inbox.join(format!("Record/{n}.flac")), title, n);
    }
    let covers = CoverCache::new(dir.join("cache")).unwrap();
    let roots = [
        (inbox, Collection::Inbox),
        (root.clone(), Collection::Library),
    ];
    scan::scan(&lib, &covers, &roots, &Progress::default()).unwrap();
    let album = lib
        .albums(
            Collection::Inbox,
            &[],
            &[],
            &coversync_core::split::Splitter::default(),
            &[],
            None,
        )
        .unwrap()[0]
        .id
        .clone();
    let detail = lib.album(&album).unwrap().unwrap();
    let ids: Vec<i64> = detail.tracks.iter().map(|t| t.id).collect();
    Setup {
        root,
        lib,
        album,
        tracks: [ids[0], ids[1]],
    }
}

/// A tiny FLAC tagged with `title`, album "Record" and track `n`.
fn track(path: &Path, title: &str, n: u32) {
    flac(path, &[]);
    let edit = TagEdit {
        title: Some(title.into()),
        album: Some("Record".into()),
        track_no: Some(n.to_string()),
        ..Default::default()
    };
    tags::write(path, &edit).unwrap();
}

impl Setup {
    /// The plan with each track's `edits`.
    fn plan(&self, edits: [&[(&str, &str)]; 2]) -> ImportPlan {
        let edits: Edits = self
            .tracks
            .iter()
            .zip(edits)
            .map(|(id, pairs)| {
                let values: HashMap<String, String> = pairs
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect();
                (*id, values)
            })
            .collect();
        import::plan(&self.lib, &self.album, &edits, None).unwrap()
    }

    /// Where each track would go, under the Library folder.
    fn dests(&self, plan: &ImportPlan) -> Vec<String> {
        plan.tracks
            .iter()
            .map(|t| {
                let dest = PathBuf::from(t.dest.as_deref().unwrap());
                dest.strip_prefix(&self.root).unwrap().display().to_string()
            })
            .collect()
    }
}

#[test]
fn a_date_with_no_year_leaves_a_required_year_unplaced() {
    // As another tagger can leave it: a typed one would be refused.
    let undated = |s: &Setup| {
        use lofty::prelude::*;
        for n in [1, 2] {
            let path = s
                .root
                .parent()
                .unwrap()
                .join(format!("Inbox/Record/{n}.flac"));
            let mut file = lofty::read_from_path(&path).unwrap();
            let tag = file.primary_tag_mut().unwrap();
            tag.insert_text(lofty::tag::ItemKey::RecordingDate, "03.05.99".into());
            file.save_to_path(&path, lofty::config::WriteOptions::default())
                .unwrap();
        }
        rescan(s);
    };
    let s = setup("no-year", "{year}/{album}/{title}");
    undated(&s);
    let plan = s.plan([&[], &[]]);
    assert!(!plan.ready());
    assert_eq!(
        plan.problems,
        [
            "The date “03.05.99” has no year for the folder structure's {year}: four digits in a row."
        ]
    );
    // Optional, it's left out.
    let s = setup("no-year-optional", "{album}< ({year})>/{title}");
    undated(&s);
    let plan = s.plan([&[], &[]]);
    assert!(plan.ready(), "{:?}", plan.problems);
    assert_eq!(s.dests(&plan), ["Record/One.flac", "Record/Two.flac"]);
}

#[test]
fn optional_album_wide_values_have_to_agree() {
    let s = setup("wide", "{album}< ({genre})>/<{track_number} - >{title}");
    // Not required: an album without a genre goes in one folder.
    let plan = s.plan([&[], &[]]);
    assert!(plan.failing.is_empty() && plan.disagree.is_empty());
    assert_eq!(
        s.dests(&plan),
        ["Record/01 - One.flac", "Record/02 - Two.flac"]
    );
    let plan = s.plan([&[("genre", "Rock")], &[("genre", "Rock")]]);
    assert!(plan.disagree.is_empty());
    assert_eq!(
        s.dests(&plan),
        ["Record (Rock)/01 - One.flac", "Record (Rock)/02 - Two.flac"]
    );
    // Two genres, or one on one track only, would split the album.
    for edits in [
        [&[("genre", "Rock")][..], &[("genre", "Pop")][..]],
        [&[("genre", "Rock")], &[]],
    ] {
        let plan = s.plan(edits);
        assert_eq!(plan.disagree, ["genre"], "{edits:?}");
        assert!(!plan.ready());
    }
}

#[test]
fn values_only_in_parts_every_track_leaves_out_needn_t_agree() {
    let s = setup(
        "left-out",
        "{album}< ({genre} {label})>/<{track_number} - >{title}",
    );
    // No track has a label: the part goes on both, whatever their genres.
    let plan = s.plan([&[("genre", "Rock")], &[("genre", "Pop")]]);
    assert!(plan.disagree.is_empty(), "{:?}", plan.disagree);
    assert_eq!(
        s.dests(&plan),
        ["Record/01 - One.flac", "Record/02 - Two.flac"]
    );
    // A label on one track keeps the part there: then they differ.
    let plan = s.plan([&[("genre", "Rock"), ("label", "L")], &[("genre", "Rock")]]);
    assert_eq!(plan.disagree, ["label"]);
}

#[test]
fn an_albums_discs_have_to_count_alike() {
    let s = setup(
        "discs",
        "{album}/<{disc_number}-><{track_number} - >{title}",
    );
    let disc = |n: &'static str, total: &'static str| -> [(&'static str, &'static str); 2] {
        [("disc_number", n), ("disc_total", total)]
    };
    // Totals spelt differently but the same: one disc, no disc numbers.
    let plan = s.plan([&disc("1", "1"), &disc("1", "01")]);
    assert!(plan.disagree.is_empty(), "{:?}", plan.disagree);
    assert_eq!(
        s.dests(&plan),
        ["Record/01 - One.flac", "Record/02 - Two.flac"]
    );
    // Two discs: both numbered.
    let plan = s.plan([&disc("1", "2"), &disc("2", "2")]);
    assert!(plan.disagree.is_empty(), "{:?}", plan.disagree);
    assert_eq!(
        s.dests(&plan),
        ["Record/1-01 - One.flac", "Record/2-02 - Two.flac"]
    );
    // "1 of 1" and "2 of 2", as separate totals or in the disc numbers
    // alone: the first disc would go without its number.
    for edits in [
        [disc("1", "1"), disc("2", "2")],
        [disc("1/1", ""), disc("2/2", "")],
    ] {
        let plan = s.plan([&edits[0], &edits[1]]);
        assert_eq!(plan.disagree, ["disc_total"], "{edits:?}");
    }
}

#[test]
fn names_are_split_at_library_setups_phrases() {
    let s = setup("split", "{album_artist}/{album}/<{track_number} - >{title}");
    let inbox = s.root.parent().unwrap().join("Inbox/Record");
    for (n, artist) in [(1, "Ado feat. Eve"), (2, "Rin (CV. Riho)")] {
        let edit = TagEdit {
            artist: Some(artist.into()),
            album_artist: Some("Ado feat. Eve".into()),
            ..Default::default()
        };
        tags::write(&inbox.join(format!("{n}.flac")), &edit).unwrap();
    }
    let plan = s.plan([&[], &[]]);
    assert_eq!(plan.split, ["artist", "album_artist"]);
    // Split, not filled: autofill reports only what it filled.
    assert!(
        !plan.autofilled.contains(&"artist") && !plan.autofilled.contains(&"album_artist"),
        "{:?}",
        plan.autofilled
    );
    assert_eq!(plan.tracks[0].autofilled["artist"], "Ado;Eve");
    assert_eq!(plan.tracks[1].autofilled["artist"], "Rin;Riho");
    assert_eq!(
        s.dests(&plan),
        [
            "Ado;Eve/Record/01 - One.flac",
            "Ado;Eve/Record/02 - Two.flac"
        ]
    );
    // What's typed in the dialog stays as typed.
    let plan = s.plan([&[("artist", "Ado feat. Eve")], &[("artist", "Rin")]]);
    assert_eq!(plan.split, ["album_artist"]);

    // The copies get the names split.
    let covers = CoverCache::new(s.root.parent().unwrap().join("cache")).unwrap();
    let done = import::execute(
        &s.lib,
        &covers,
        &s.album,
        &Edits::new(),
        None,
        &Progress::default(),
    )
    .unwrap();
    assert_eq!(done.files, 2);
    let copy =
        |name: &str| tags::read_kinds(&s.root.join("Ado;Eve/Record").join(name), false).unwrap();
    assert_eq!(copy("01 - One.flac").values["artist"], "Ado;Eve");
    assert_eq!(copy("02 - Two.flac").values["artist"], "Rin;Riho");
    assert_eq!(copy("02 - Two.flac").values["album_artist"], "Ado;Eve");
}

#[test]
fn substitutions_change_tags_on_import_once_names_are_split() {
    let s = setup(
        "substitute",
        "{album_artist}/{album}/<{track_number} - >{title}",
    );
    let mut setup = s.lib.library_setup().unwrap();
    setup.rules.substitutions = vec![
        Substitution {
            expression: r"s/ \(Live\)$//".into(),
            tags: vec!["title".into()],
        },
        Substitution {
            expression: r"s/^Ado$/\U&/".into(),
            tags: vec!["artist".into(), "album_artist".into()],
        },
    ];
    s.lib.set_library_setup(setup).unwrap();
    let inbox = s.root.parent().unwrap().join("Inbox/Record");
    for (n, title) in [(1, "One (Live)"), (2, "Two")] {
        let edit = TagEdit {
            title: Some(title.into()),
            artist: Some("Ado feat. Eve".into()),
            album_artist: Some("Ado".into()),
            ..Default::default()
        };
        tags::write(&inbox.join(format!("{n}.flac")), &edit).unwrap();
    }
    let plan = s.plan([&[], &[]]);
    assert_eq!(plan.split, ["artist"]);
    assert_eq!(plan.substituted, ["title", "artist", "album_artist"]);
    assert_eq!(plan.tracks[0].autofilled["title"], "One");
    assert!(!plan.tracks[1].autofilled.contains_key("title"));
    // Each name on its own, once split.
    assert_eq!(plan.tracks[0].autofilled["artist"], "ADO;Eve");
    assert_eq!(
        s.dests(&plan),
        ["ADO/Record/01 - One.flac", "ADO/Record/02 - Two.flac"]
    );
    // What's typed in the dialog stays as typed.
    let plan = s.plan([&[("title", "One (Live)")], &[]]);
    assert_eq!(plan.substituted, ["artist", "album_artist"]);

    let covers = CoverCache::new(s.root.parent().unwrap().join("cache")).unwrap();
    import::execute(
        &s.lib,
        &covers,
        &s.album,
        &Edits::new(),
        None,
        &Progress::default(),
    )
    .unwrap();
    let copy = tags::read_kinds(&s.root.join("ADO/Record/01 - One.flac"), false).unwrap();
    assert_eq!(copy.values["title"], "One");
    assert_eq!(copy.values["artist"], "ADO;Eve");
    assert_eq!(copy.values["album_artist"], "ADO");
}

#[test]
fn a_collection_picked_on_import_is_a_folder_and_none_is_none() {
    let s = setup(
        "collection",
        "{collection}/{album}/<{track_number} - >{title}",
    );
    let plan = s.plan([&[], &[]]);
    assert!(plan.ready(), "{:?}", plan.problems);
    assert_eq!(
        s.dests(&plan),
        ["Record/01 - One.flac", "Record/02 - Two.flac"]
    );
    let saga: &[(&str, &str)] = &[("collection", "Saga")];
    let plan = s.plan([saga, saga]);
    assert_eq!(
        s.dests(&plan),
        ["Saga/Record/01 - One.flac", "Saga/Record/02 - Two.flac"]
    );

    let covers = CoverCache::new(s.root.parent().unwrap().join("cache")).unwrap();
    let edits: Edits = s
        .tracks
        .iter()
        .map(|id| (*id, HashMap::from([("collection".into(), "Saga".into())])))
        .collect();
    // A read-only original still takes its tags, in a copy coversync can
    // edit again.
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    #[cfg(unix)]
    fs::set_permissions(
        s.root.parent().unwrap().join("Inbox/Record/2.flac"),
        fs::Permissions::from_mode(0o444),
    )
    .unwrap();
    import::execute(
        &s.lib,
        &covers,
        &s.album,
        &edits,
        None,
        &Progress::default(),
    )
    .unwrap();
    let path = s.root.join("Saga/Record/02 - Two.flac");
    let copy = tags::read_kinds(&path, false).unwrap();
    assert_eq!(copy.values["collection"], "Saga");
    #[cfg(unix)]
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o644
    );
}

/// A mono 8 kHz WAV of a second's ramp: 16-bit samples, or 32-bit floats
/// FLAC can't hold.
fn wav(path: &Path, float: bool) {
    let (format, bits) = if float { (3u16, 32u16) } else { (1, 16) };
    let data: Vec<u8> = (0..8000u32)
        .flat_map(|i| match float {
            true => (i as f32 / 8000.0 + 0.000_01).to_le_bytes().to_vec(),
            false => ((i % 2000) as i16 * 16).to_le_bytes().to_vec(),
        })
        .collect();
    let mut b = Vec::new();
    b.extend(b"RIFF");
    b.extend((4 + 24 + 8 + data.len() as u32).to_le_bytes());
    b.extend(b"WAVEfmt ");
    b.extend(16u32.to_le_bytes());
    b.extend(format.to_le_bytes());
    b.extend(1u16.to_le_bytes());
    b.extend(8000u32.to_le_bytes());
    b.extend((8000 * u32::from(bits) / 8).to_le_bytes());
    b.extend((bits / 8).to_le_bytes());
    b.extend(bits.to_le_bytes());
    b.extend(b"data");
    b.extend((data.len() as u32).to_le_bytes());
    b.extend(data);
    fs::write(path, b).unwrap();
}

#[test]
fn lossless_audio_that_isnt_flac_is_converted_with_its_tags() {
    if !coversync_core::transcode::available() {
        eprintln!("ffmpeg isn't installed: skipped");
        return;
    }
    let s = setup("convert", "{collection}/{album}/<{track_number} - >{title}");
    let inbox = s.root.parent().unwrap().join("Inbox/Record");
    fs::remove_file(inbox.join("2.flac")).unwrap();
    wav(&inbox.join("2.wav"), false);
    let wv = inbox.join("3.wv");
    fs::write(&wv, include_bytes!("fixtures/silence.wv")).unwrap();
    for (n, path) in [(2, &inbox.join("2.wav")), (3, &wv)] {
        let edit = TagEdit {
            title: Some(format!("Track {n}")),
            album: Some("Record".into()),
            track_no: Some(n.to_string()),
            ..Default::default()
        };
        if n == 2 {
            // The album's collection (the WavPack gets it from autofill),
            // and a BPM ID3v2 holds as an integer.
            let kinds = [
                ("collection", "Saga".to_string()),
                ("bpm", "120".to_string()),
            ];
            tags::write_with_kinds(path, &edit, &kinds).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(path, fs::Permissions::from_mode(0o640)).unwrap();
            }
        } else {
            // Not written by coversync: an APE tag, with a cover as an item.
            let mut ape = lofty::ape::ApeTag::default();
            let mut cover = b"front.png\0".to_vec();
            let mut png = std::io::Cursor::new(Vec::new());
            image::DynamicImage::new_rgb8(4, 4)
                .write_to(&mut png, image::ImageFormat::Png)
                .unwrap();
            cover.extend(png.into_inner());
            let text = |v: &str| lofty::tag::ItemValue::Text(v.into());
            for (key, value) in [
                ("Album", text("Record")),
                ("Title", text("Track 3")),
                ("Track", text("3")),
                ("Cover Art (Front)", lofty::tag::ItemValue::Binary(cover)),
            ] {
                ape.insert(lofty::ape::ApeItem::new(key.into(), value).unwrap());
            }
            lofty::tag::TagExt::save_to_path(&ape, path, lofty::config::WriteOptions::default())
                .unwrap();
        }
    }
    rescan(&s);
    let album = s.lib.album(&s.album).unwrap().unwrap();
    assert_eq!(album.tracks.len(), 3);
    let plan = import::plan(&s.lib, &s.album, &Edits::new(), None).unwrap();
    assert!(plan.ready(), "{:?}", plan.problems);
    let converted: Vec<bool> = plan.tracks.iter().map(|t| t.to_flac).collect();
    assert_eq!(converted, [false, true, true]);

    let covers = CoverCache::new(s.root.parent().unwrap().join("cache")).unwrap();
    import::execute(
        &s.lib,
        &covers,
        &s.album,
        &Edits::new(),
        None,
        &Progress::default(),
    )
    .unwrap();
    for (name, title) in [
        ("02 - Track 2.flac", "Track 2"),
        ("03 - Track 3.flac", "Track 3"),
    ] {
        let path = s.root.join("Saga/Record").join(name);
        let file = lofty::probe::Probe::open(&path)
            .unwrap()
            .guess_file_type()
            .unwrap();
        assert_eq!(
            file.file_type(),
            Some(lofty::file::FileType::Flac),
            "{name}"
        );
        let read = tags::read_kinds(&path, true).unwrap();
        assert_eq!(read.values["title"], title);
        assert_eq!(read.values["collection"], "Saga");
        if name.starts_with("02") {
            assert_eq!(read.values["bpm"], "120");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = fs::metadata(&path).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o640);
            }
        } else {
            assert!(read.has_art, "the APE tag's cover");
        }
    }
    let names = |dir: &Path| -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    };
    // Nothing of the conversion is left beside the tracks.
    assert_eq!(
        names(&s.root.join("Saga/Record")),
        ["01 - One.flac", "02 - Track 2.flac", "03 - Track 3.flac"]
    );
}

#[test]
fn float_audio_isnt_imported() {
    if !coversync_core::transcode::available() {
        eprintln!("ffmpeg isn't installed: skipped");
        return;
    }
    let s = setup("float", "{album}/<{track_number} - >{title}");
    let inbox = s.root.parent().unwrap().join("Inbox/Record");
    fs::remove_file(inbox.join("2.flac")).unwrap();
    wav(&inbox.join("2.wav"), true);
    let edit = TagEdit {
        title: Some("Two".into()),
        album: Some("Record".into()),
        track_no: Some("2".into()),
        ..Default::default()
    };
    tags::write(&inbox.join("2.wav"), &edit).unwrap();
    rescan(&s);
    let covers = CoverCache::new(s.root.parent().unwrap().join("cache")).unwrap();
    let refused = import::execute(
        &s.lib,
        &covers,
        &s.album,
        &Edits::new(),
        None,
        &Progress::default(),
    )
    .unwrap_err()
    .to_string();
    assert!(refused.contains("without loss"), "{refused}");
    // Nothing is left: no copy, no conversion.
    let left = fs::read_dir(s.root.join("Record")).map_or(0, |d| d.count());
    assert_eq!(left, 0);
}

#[test]
fn no_phrases_split_nothing() {
    let s = setup("no-split", "{album}/<{track_number} - >{title}");
    let mut setup = s.lib.library_setup().unwrap();
    setup.rules.split_at.clear();
    s.lib.set_library_setup(setup).unwrap();
    let inbox = s.root.parent().unwrap().join("Inbox/Record");
    let edit = TagEdit {
        artist: Some("Ado feat. Eve".into()),
        ..Default::default()
    };
    tags::write(&inbox.join("1.flac"), &edit).unwrap();
    let plan = s.plan([&[], &[]]);
    assert!(plan.split.is_empty());
    assert!(!plan.tracks[0].autofilled.contains_key("artist"));
}

#[test]
fn an_album_artist_autofill_fills_from_the_artist_is_split_too() {
    let s = setup(
        "split-filled",
        "{album_artist}/{album}/<{track_number} - >{title}",
    );
    let inbox = s.root.parent().unwrap().join("Inbox/Record");
    for n in [1, 2] {
        let edit = TagEdit {
            artist: Some("Ado feat. Eve".into()),
            ..Default::default()
        };
        tags::write(&inbox.join(format!("{n}.flac")), &edit).unwrap();
    }
    let plan = s.plan([&[], &[]]);
    assert!(
        plan.autofilled.contains(&"album_artist"),
        "{:?}",
        plan.autofilled
    );
    assert_eq!(plan.split, ["artist", "album_artist"]);
    assert_eq!(plan.tracks[0].autofilled["album_artist"], "Ado;Eve");
    assert_eq!(
        s.dests(&plan),
        [
            "Ado;Eve/Record/01 - One.flac",
            "Ado;Eve/Record/02 - Two.flac"
        ]
    );
}

/// Set the Inbox files' modification times, newest last, and rescan.
fn touch_and_rescan(s: &Setup, times: [u64; 2]) {
    let dir = s.root.parent().unwrap();
    for (n, secs) in [1, 2].into_iter().zip(times) {
        let file = fs::File::options()
            .write(true)
            .open(dir.join(format!("Inbox/Record/{n}.flac")))
            .unwrap();
        file.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs))
            .unwrap();
    }
    rescan(s);
}

fn rescan(s: &Setup) {
    let dir = s.root.parent().unwrap();
    let covers = CoverCache::new(dir.join("cache")).unwrap();
    let roots = [
        (dir.join("Inbox"), Collection::Inbox),
        (s.root.clone(), Collection::Library),
    ];
    scan::scan(&s.lib, &covers, &roots, &Progress::default()).unwrap();
}

#[test]
fn preserve_modification_date_keeps_the_albums_inbox_date() {
    let s = setup("keep-added", "{album}/<{track_number} - >{title}");
    touch_and_rescan(&s, [1_500_000_000, 1_600_000_000]);
    // The Inbox's date: its newest file's.
    let inbox_date = s.lib.album(&s.album).unwrap().unwrap().album.added_at;
    assert_eq!(inbox_date, 1_600_000_000);
    // Off: the import time.
    assert_eq!(s.plan([&[], &[]]).added, None);

    let mut inbox = s.lib.inbox_setup().unwrap();
    inbox.keep_added = true;
    s.lib.set_inbox_setup(inbox).unwrap();
    assert_eq!(s.plan([&[], &[]]).added, Some(inbox_date));
    let covers = CoverCache::new(s.root.parent().unwrap().join("cache")).unwrap();
    import::execute(
        &s.lib,
        &covers,
        &s.album,
        &Edits::new(),
        None,
        &Progress::default(),
    )
    .unwrap();
    let library = s
        .lib
        .albums(
            Collection::Library,
            &[],
            &[],
            &coversync_core::split::Splitter::default(),
            &[],
            None,
        )
        .unwrap();
    assert_eq!(library.len(), 1);
    assert_eq!(library[0].added_at, inbox_date);
    // A rescan keeps it.
    touch_and_rescan(&s, [1_500_000_000, 1_600_000_000]);
    let library = s
        .lib
        .albums(
            Collection::Library,
            &[],
            &[],
            &coversync_core::split::Splitter::default(),
            &[],
            None,
        )
        .unwrap();
    assert_eq!(library[0].added_at, inbox_date);
}

#[test]
fn without_it_an_import_is_added_when_it_happens() {
    let s = setup("import-time", "{album}/<{track_number} - >{title}");
    touch_and_rescan(&s, [1_500_000_000, 1_600_000_000]);
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let covers = CoverCache::new(s.root.parent().unwrap().join("cache")).unwrap();
    import::execute(
        &s.lib,
        &covers,
        &s.album,
        &Edits::new(),
        None,
        &Progress::default(),
    )
    .unwrap();
    let library = s
        .lib
        .albums(
            Collection::Library,
            &[],
            &[],
            &coversync_core::split::Splitter::default(),
            &[],
            None,
        )
        .unwrap();
    assert!(library[0].added_at >= before, "{}", library[0].added_at);
}

#[test]
fn with_no_inbox_date_it_falls_back_to_the_import_time() {
    let s = setup("keep-unknown", "{album}/<{track_number} - >{title}");
    touch_and_rescan(&s, [0, 0]);
    assert_eq!(s.lib.album(&s.album).unwrap().unwrap().album.added_at, 0);
    let mut inbox = s.lib.inbox_setup().unwrap();
    inbox.keep_added = true;
    s.lib.set_inbox_setup(inbox).unwrap();
    assert_eq!(s.plan([&[], &[]]).added, None);
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let covers = CoverCache::new(s.root.parent().unwrap().join("cache")).unwrap();
    import::execute(
        &s.lib,
        &covers,
        &s.album,
        &Edits::new(),
        None,
        &Progress::default(),
    )
    .unwrap();
    let library = s
        .lib
        .albums(
            Collection::Library,
            &[],
            &[],
            &coversync_core::split::Splitter::default(),
            &[],
            None,
        )
        .unwrap();
    assert!(library[0].added_at >= before, "{}", library[0].added_at);
}

/// A FLAC file with these Vorbis comments and a few bytes standing for its
/// audio.
fn flac(path: &Path, comments: &[&str]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut b = b"fLaC".to_vec();
    // STREAMINFO: 4096-sample blocks, 44.1 kHz, stereo, 16 bits, no samples.
    b.extend([
        0x00, 0x00, 0x00, 34, 0x10, 0x00, 0x10, 0x00, 0, 0, 0, 0, 0, 0,
    ]);
    let packed: u64 = (44100 << 44) | (1 << 41) | (15 << 36);
    b.extend(packed.to_be_bytes());
    b.extend([0u8; 16]);
    let mut block = Vec::new();
    let vendor = b"reference libFLAC 1.2.1 20070917";
    block.extend((vendor.len() as u32).to_le_bytes());
    block.extend(vendor);
    block.extend((comments.len() as u32).to_le_bytes());
    for c in comments {
        block.extend((c.len() as u32).to_le_bytes());
        block.extend(c.as_bytes());
    }
    b.push(0x80 | 4);
    b.extend(&(block.len() as u32).to_be_bytes()[1..]);
    b.extend(block);
    b.extend([0xFF, 0xF8, 0x69, 0x08, 0x00, 0x00, 0x00, 0x00]);
    fs::write(path, b).unwrap();
}

/// A FLAC file's vendor string and its Vorbis comments, as written.
fn comments(path: &Path) -> (String, Vec<(String, String)>) {
    use lofty::file::AudioFile;
    let mut f = fs::File::open(path).unwrap();
    let flac =
        lofty::flac::FlacFile::read_from(&mut f, lofty::config::ParseOptions::new()).unwrap();
    let tag = flac.vorbis_comments().unwrap();
    let items = tag
        .items()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    (tag.vendor().to_string(), items)
}

#[test]
fn an_import_keeps_the_tags_the_catalog_doesnt_know() {
    let dir = std::env::temp_dir().join(format!("coversync-unmapped-{}", std::process::id()));
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
    // Multilingual titles and a rip's table of contents, as a ripper leaves them.
    flac(
        &inbox.join("Record/01.flac"),
        &[
            "TITLE=Bokura no LIVE",
            "TITLE_JP=僕らのLIVE",
            "ALBUM=Record",
            "ALBUM_JP=レコード",
            "TRACKNUMBER=01",
            "CDTOC=1+96+2D2B",
        ],
    );
    let source = inbox.join("Record/01.flac");
    let original = fs::read(&source).unwrap();
    let covers = CoverCache::new(dir.join("cache")).unwrap();
    let roots = [
        (inbox.clone(), Collection::Inbox),
        (root.clone(), Collection::Library),
    ];
    scan::scan(&lib, &covers, &roots, &Progress::default()).unwrap();
    let album = lib
        .albums(
            Collection::Inbox,
            &[],
            &[],
            &coversync_core::split::Splitter::default(),
            &[],
            None,
        )
        .unwrap()[0]
        .id
        .clone();
    let track = lib.album(&album).unwrap().unwrap().tracks[0].id;
    // An edit, so the copy's tags are written, not copied as they are.
    let edits: Edits = [(
        track,
        HashMap::from([("genre".to_string(), "Anime".to_string())]),
    )]
    .into();
    import::execute(&lib, &covers, &album, &edits, None, &Progress::default()).unwrap();
    let copies: Vec<PathBuf> = fs::read_dir(root.join("Record"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "flac"))
        .collect();
    assert_eq!(copies.len(), 1, "{copies:?}");
    let copy = &copies[0];
    let (vendor, items) = comments(copy);
    let has = |k: &str, v: &str| items.iter().any(|(a, b)| a == k && b == v);
    for (k, v) in [
        ("TITLE_JP", "僕らのLIVE"),
        ("ALBUM_JP", "レコード"),
        ("CDTOC", "1+96+2D2B"),
        ("GENRE", "Anime"),
        ("TITLE", "Bokura no LIVE"),
    ] {
        assert!(has(k, v), "{k}={v} missing from {items:?}");
    }
    assert_eq!(vendor, "reference libFLAC 1.2.1 20070917");
    assert!(!items.iter().any(|(k, _)| k == "ENCODER"), "{items:?}");
    // The original is untouched.
    assert_eq!(fs::read(&source).unwrap(), original);
    let _ = fs::remove_dir_all(&dir);
}
