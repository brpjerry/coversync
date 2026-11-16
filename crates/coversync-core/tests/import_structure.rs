//! What an import plan makes of a folder structure with optional parts, on
//! real (tiny) audio files: which values have to agree across an album.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use coversync_core::cover::CoverCache;
use coversync_core::import::{self, Edits, ImportPlan};
use coversync_core::scan::{self, Progress};
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
        wav(&inbox.join(format!("Record/{n}.wav")), title, n);
    }
    let covers = CoverCache::new(dir.join("cache")).unwrap();
    let roots = [
        (inbox, Collection::Inbox),
        (root.clone(), Collection::Library),
    ];
    scan::scan(&lib, &covers, &roots, &Progress::default()).unwrap();
    let album = lib
        .albums(Collection::Inbox, &[], &[], "", &[], None)
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

/// A tiny silent WAV tagged with `title`, album "Record" and track `n`.
fn wav(path: &Path, title: &str, n: u32) {
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
fn optional_album_wide_values_have_to_agree() {
    let s = setup("wide", "{album}< ({genre})>/<{track_number} - >{title}");
    // Not required: an album without a genre goes in one folder.
    let plan = s.plan([&[], &[]]);
    assert!(plan.failing.is_empty() && plan.disagree.is_empty());
    assert_eq!(
        s.dests(&plan),
        ["Record/01 - One.wav", "Record/02 - Two.wav"]
    );
    let plan = s.plan([&[("genre", "Rock")], &[("genre", "Rock")]]);
    assert!(plan.disagree.is_empty());
    assert_eq!(
        s.dests(&plan),
        ["Record (Rock)/01 - One.wav", "Record (Rock)/02 - Two.wav"]
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
        ["Record/01 - One.wav", "Record/02 - Two.wav"]
    );
    // Two discs: both numbered.
    let plan = s.plan([&disc("1", "2"), &disc("2", "2")]);
    assert!(plan.disagree.is_empty(), "{:?}", plan.disagree);
    assert_eq!(
        s.dests(&plan),
        ["Record/1-01 - One.wav", "Record/2-02 - Two.wav"]
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
