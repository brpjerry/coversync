//! Filling empty tags with values that can safely be assumed, at import time.
//!
//! Only empty tags are filled; nothing a file already has is changed. Each
//! rule can be turned off in Inbox setup.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::rules;
use crate::scan::disc_folder_number;

pub const VARIOUS_ARTISTS: &str = "Various Artists";

/// Which assumptions to make. Folder-name guesses are off by default because
/// folders are often named "Artist - Album" or similar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Autofill {
    /// Album artist from the artist, when every track has the same one.
    pub album_artist_from_artist: bool,
    /// ...or "Various Artists" when the tracks' artists differ.
    pub various_artists: bool,
    /// A track's artist from its album artist.
    pub artist_from_album_artist: bool,
    /// Album-wide tags some tracks have, copied to the rest when they agree.
    pub album_tags_from_tracks: bool,
    /// Track 1 of 1 when an album has a single, unnumbered track.
    pub single_track: bool,
    /// Disc 1 of 1 when nothing suggests more than one disc.
    pub single_disc: bool,
    /// Disc number from `CD1` / `Disc 2` folders.
    pub disc_from_folder: bool,
    /// Track total per disc when numbered 1..N; disc total from the highest disc.
    pub totals: bool,
    /// Track (and disc) number from a leading number in the file name.
    pub track_from_file_name: bool,
    /// Title from the file name, minus any leading number.
    pub title_from_file_name: bool,
    /// Album from the album folder's name.
    pub album_from_folder: bool,
    /// Year from a "(2003)" or "[2003]" in the album folder's name.
    pub year_from_folder: bool,
}

impl Default for Autofill {
    fn default() -> Self {
        Self {
            album_artist_from_artist: true,
            various_artists: true,
            artist_from_album_artist: true,
            album_tags_from_tracks: true,
            single_track: true,
            single_disc: true,
            disc_from_folder: true,
            totals: true,
            track_from_file_name: true,
            title_from_file_name: true,
            album_from_folder: false,
            year_from_folder: false,
        }
    }
}

/// One track as autofill sees it.
#[derive(Debug, Clone)]
pub struct TrackInfo {
    /// File name without its extension.
    pub stem: String,
    /// Name of the folder the file is in (a `CD1` folder for multi-disc albums).
    pub folder: String,
    /// Non-empty tag values by catalog kind.
    pub values: HashMap<&'static str, String>,
}

/// Leading `03`, `03.`, `03 -` or disc-track `1-03` / `2.05` in a file name,
/// and what's left as a title.
fn parse_stem(stem: &str) -> (Option<u32>, Option<u32>, &str) {
    // Some rippers prefix names with "#" ("# 01. Title", "# Title").
    let s = stem.trim().trim_start_matches('#').trim_start();
    let seps = [' ', '-', '.', '_'];
    fn rest_title(r: &str) -> &str {
        r.trim_start_matches([' ', '-', '.', '_']).trim()
    }
    let n1 = s.bytes().take_while(u8::is_ascii_digit).count();
    if n1 == 0 || n1 > 3 {
        return (None, None, s);
    }
    let first: u32 = s[..n1].parse().unwrap_or(0);
    let rest = &s[n1..];
    if n1 <= 2
        && let Some(r) = rest.strip_prefix(['-', '.'])
    {
        let n2 = r.bytes().take_while(u8::is_ascii_digit).count();
        let after = &r[n2..];
        if (1..=3).contains(&n2) && (after.is_empty() || after.starts_with(seps)) {
            return (Some(first), r[..n2].parse().ok(), rest_title(after));
        }
    }
    if rest.is_empty() || rest.starts_with(seps) {
        return (None, Some(first), rest_title(rest));
    }
    (None, None, s)
}

/// "3/12" -> 3.
fn number(v: &str) -> Option<u32> {
    v.split('/').next()?.trim().parse().ok()
}

/// A 19xx/20xx year in parentheses or brackets.
fn folder_year(name: &str) -> Option<String> {
    let b = name.as_bytes();
    (0..b.len().saturating_sub(5)).find_map(|i| {
        let open = b[i] == b'(' || b[i] == b'[';
        let close = b[i + 5] == b')' || b[i + 5] == b']';
        let year = &name.get(i + 1..i + 5)?;
        let ok = open
            && close
            && year.bytes().all(|c| c.is_ascii_digit())
            && (year.starts_with("19") || year.starts_with("20"));
        ok.then(|| year.to_string())
    })
}

struct Filler<'a> {
    tracks: &'a mut [TrackInfo],
    filled: Vec<Vec<&'static str>>,
}

impl Filler<'_> {
    fn has(&self, i: usize, kind: &str) -> bool {
        self.tracks[i].values.contains_key(kind)
    }

    fn get(&self, i: usize, kind: &str) -> Option<&str> {
        self.tracks[i].values.get(kind).map(String::as_str)
    }

    /// Set `kind` on track `i` if it's empty.
    fn set(&mut self, i: usize, kind: &'static str, value: impl Into<String>) {
        let value = value.into();
        if value.trim().is_empty() || self.has(i, kind) {
            return;
        }
        self.tracks[i].values.insert(kind, value.trim().to_string());
        self.filled[i].push(kind);
    }

    fn all(&self, kind: &str) -> bool {
        (0..self.tracks.len()).all(|i| self.has(i, kind))
    }

    fn none(&self, kind: &str) -> bool {
        (0..self.tracks.len()).all(|i| !self.has(i, kind))
    }

    /// The one value every track that has `kind` agrees on.
    fn agreed(&self, kind: &str) -> Option<String> {
        let values: HashSet<&str> = (0..self.tracks.len())
            .filter_map(|i| self.get(i, kind))
            .collect();
        (values.len() == 1).then(|| values.into_iter().next().unwrap().to_string())
    }
}

/// Fill empty tags across one album's tracks (`album_folder` is the album
/// directory's name). Returns, per track, the kinds that were filled.
pub fn fill(
    opts: &Autofill,
    album_folder: &str,
    tracks: &mut [TrackInfo],
) -> Vec<Vec<&'static str>> {
    let n = tracks.len();
    let mut f = Filler {
        tracks,
        filled: vec![Vec::new(); n],
    };

    // From file names.
    for i in 0..n {
        let stem = f.tracks[i].stem.clone();
        let (disc, track, title) = parse_stem(&stem);
        if opts.track_from_file_name {
            if let Some(t) = track {
                f.set(i, "track_number", t.to_string());
            }
            if let Some(d) = disc {
                f.set(i, "disc_number", d.to_string());
            }
        }
        if opts.title_from_file_name {
            f.set(i, "title", title.to_string());
        }
    }

    // A single, unnumbered track is track 1 of 1.
    if opts.single_track && n == 1 && !f.has(0, "track_number") {
        f.set(0, "track_number", "1");
        f.set(0, "track_total", "1");
    }

    // From CD1 / Disc 2 folders.
    let folder_discs: Vec<Option<u32>> = f
        .tracks
        .iter()
        .map(|t| disc_folder_number(&t.folder))
        .collect();
    if opts.disc_from_folder {
        for (i, d) in folder_discs.iter().enumerate() {
            if let Some(d) = d {
                f.set(i, "disc_number", d.to_string());
            }
        }
        if folder_discs.iter().all(Option::is_some)
            && let Some(max) = folder_discs.iter().flatten().max()
        {
            for i in 0..n {
                f.set(i, "disc_total", max.to_string());
            }
        }
    }

    // Album-wide tags some tracks already agree on.
    if opts.album_tags_from_tracks {
        for kind in rules::ALBUM_KINDS.iter().filter(|k| **k != "track_total") {
            let Some(kind) = rules::kind(kind).map(|k| k.id) else {
                continue;
            };
            if let Some(v) = f.agreed(kind) {
                for i in 0..n {
                    f.set(i, kind, v.clone());
                }
            }
        }
    }

    // From the album folder's name.
    if opts.album_from_folder && f.none("album") {
        for i in 0..n {
            f.set(i, "album", album_folder);
        }
    }
    if opts.year_from_folder
        && f.none("date")
        && let Some(year) = folder_year(album_folder)
    {
        for i in 0..n {
            f.set(i, "date", year.clone());
        }
    }

    // Album artist and artist from each other.
    if opts.album_artist_from_artist && f.none("album_artist") && f.all("artist") {
        let artist = f
            .agreed("artist")
            .or_else(|| opts.various_artists.then(|| VARIOUS_ARTISTS.to_string()));
        if let Some(a) = artist {
            for i in 0..n {
                f.set(i, "album_artist", a.clone());
            }
        }
    }
    if opts.artist_from_album_artist {
        for i in 0..n {
            if let Some(a) = f.get(i, "album_artist").map(str::to_string) {
                f.set(i, "artist", a);
            }
        }
    }

    // One disc: no disc numbers or disc folders, and track numbers that don't repeat.
    if opts.single_disc && f.none("disc_number") && folder_discs.iter().all(Option::is_none) {
        let numbers: Vec<Option<u32>> = (0..n)
            .map(|i| f.get(i, "track_number").and_then(number))
            .collect();
        let unique: HashSet<u32> = numbers.iter().flatten().copied().collect();
        if numbers.iter().all(Option::is_some) && unique.len() == n {
            for i in 0..n {
                f.set(i, "disc_number", "1");
                f.set(i, "disc_total", "1");
            }
        }
    }

    // Totals: a disc numbered exactly 1..N has N tracks; the highest disc is the disc total.
    if opts.totals {
        let disc_of = |f: &Filler, i: usize| f.get(i, "disc_number").and_then(number).unwrap_or(1);
        let discs: HashSet<u32> = (0..n).map(|i| disc_of(&f, i)).collect();
        for d in discs {
            let members: Vec<usize> = (0..n).filter(|i| disc_of(&f, *i) == d).collect();
            let mut nums: Vec<u32> = members
                .iter()
                .filter_map(|i| f.get(*i, "track_number").and_then(number))
                .collect();
            nums.sort_unstable();
            let complete = nums.len() == members.len()
                && nums.iter().enumerate().all(|(k, v)| *v == k as u32 + 1);
            if complete {
                for i in members {
                    f.set(i, "track_total", nums.len().to_string());
                }
            }
        }
        if f.all("disc_number")
            && let Some(max) = (0..n)
                .filter_map(|i| f.get(i, "disc_number").and_then(number))
                .max()
        {
            for i in 0..n {
                f.set(i, "disc_total", max.to_string());
            }
        }
    }

    f.filled
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(stem: &str, folder: &str, values: &[(&'static str, &str)]) -> TrackInfo {
        TrackInfo {
            stem: stem.into(),
            folder: folder.into(),
            values: values.iter().map(|(k, v)| (*k, v.to_string())).collect(),
        }
    }

    fn val<'a>(t: &'a TrackInfo, k: &str) -> Option<&'a str> {
        t.values.get(k).map(String::as_str)
    }

    #[test]
    fn file_names() {
        assert_eq!(parse_stem("03 Title"), (None, Some(3), "Title"));
        assert_eq!(parse_stem("03. Title"), (None, Some(3), "Title"));
        assert_eq!(parse_stem("2-05 - Title"), (Some(2), Some(5), "Title"));
        assert_eq!(parse_stem("1.07 Title"), (Some(1), Some(7), "Title"));
        assert_eq!(parse_stem("1999 Remix"), (None, None, "1999 Remix"));
        assert_eq!(parse_stem("Title"), (None, None, "Title"));
        assert_eq!(parse_stem("7S"), (None, None, "7S"));
        assert_eq!(parse_stem("# 01. ABSOLUTE"), (None, Some(1), "ABSOLUTE"));
        assert_eq!(parse_stem("# Colorful"), (None, None, "Colorful"));
    }

    #[test]
    fn album_artist_and_artist_fill_each_other() {
        let opts = Autofill::default();
        let mut t = vec![
            track("01 A", "x", &[("artist", "Kano")]),
            track("02 B", "x", &[("artist", "Kano")]),
        ];
        fill(&opts, "x", &mut t);
        assert_eq!(val(&t[0], "album_artist"), Some("Kano"));
        let mut t = vec![
            track("01 A", "x", &[("artist", "A")]),
            track("02 B", "x", &[("artist", "B")]),
        ];
        fill(&opts, "x", &mut t);
        assert_eq!(val(&t[1], "album_artist"), Some(VARIOUS_ARTISTS));
        let mut t = vec![
            track("01 A", "x", &[("album_artist", "Ado")]),
            track(
                "02 B",
                "x",
                &[("artist", "Ado feat. X"), ("album_artist", "Ado")],
            ),
        ];
        let filled = fill(&opts, "x", &mut t);
        assert_eq!(val(&t[0], "artist"), Some("Ado"));
        assert_eq!(
            val(&t[1], "artist"),
            Some("Ado feat. X"),
            "existing values are never changed"
        );
        assert!(filled[0].contains(&"artist") && !filled[1].contains(&"artist"));
    }

    #[test]
    fn single_disc_and_totals() {
        let opts = Autofill::default();
        let mut t = vec![
            track("01 A", "Album", &[]),
            track("02 B", "Album", &[]),
            track("03 C", "Album", &[]),
        ];
        fill(&opts, "Album", &mut t);
        assert_eq!(val(&t[0], "disc_number"), Some("1"));
        assert_eq!(val(&t[0], "disc_total"), Some("1"));
        assert_eq!(val(&t[2], "track_total"), Some("3"));
        // A repeated track number suggests two discs: don't guess.
        let mut t = vec![track("01 A", "Album", &[]), track("01 B", "Album", &[])];
        fill(&opts, "Album", &mut t);
        assert_eq!(val(&t[0], "disc_number"), None);
        // A gap in the numbering means the total isn't known.
        let mut t = vec![track("01 A", "Album", &[]), track("03 C", "Album", &[])];
        fill(&opts, "Album", &mut t);
        assert_eq!(val(&t[0], "track_total"), None);
    }

    #[test]
    fn one_track_albums_are_track_and_disc_one_of_one() {
        let mut t = vec![track("# Colorful", "Colorful", &[("title", "Colorful")])];
        fill(&Autofill::default(), "Colorful", &mut t);
        assert_eq!(val(&t[0], "track_number"), Some("1"));
        assert_eq!(val(&t[0], "track_total"), Some("1"));
        assert_eq!(val(&t[0], "disc_number"), Some("1"));
    }

    #[test]
    fn disc_folders() {
        let opts = Autofill::default();
        let mut t = vec![
            track("01 A", "CD1", &[]),
            track("01 B", "CD2", &[]),
            track("02 C", "CD2", &[]),
        ];
        fill(&opts, "Album", &mut t);
        assert_eq!(val(&t[1], "disc_number"), Some("2"));
        assert_eq!(val(&t[0], "disc_total"), Some("2"));
        assert_eq!(val(&t[0], "track_total"), Some("1"));
        assert_eq!(val(&t[2], "track_total"), Some("2"));
    }

    #[test]
    fn album_tags_spread_only_when_tracks_agree() {
        let opts = Autofill::default();
        let mut t = vec![
            track("01 A", "x", &[("genre", "Pop")]),
            track("02 B", "x", &[]),
        ];
        fill(&opts, "x", &mut t);
        assert_eq!(val(&t[1], "genre"), Some("Pop"));
        let mut t = vec![
            track("01 A", "x", &[("genre", "Pop")]),
            track("02 B", "x", &[("genre", "Rock")]),
            track("03 C", "x", &[]),
        ];
        fill(&opts, "x", &mut t);
        assert_eq!(val(&t[2], "genre"), None);
    }

    #[test]
    fn folder_guesses_are_opt_in() {
        let mut t = vec![track("01 A", "x", &[])];
        fill(&Autofill::default(), "Thaw (2015)", &mut t);
        assert_eq!(val(&t[0], "album"), None);
        assert_eq!(val(&t[0], "date"), None);
        let opts = Autofill {
            album_from_folder: true,
            year_from_folder: true,
            ..Autofill::default()
        };
        fill(&opts, "Thaw (2015)", &mut t);
        assert_eq!(val(&t[0], "album"), Some("Thaw (2015)"));
        assert_eq!(val(&t[0], "date"), Some("2015"));
    }

    #[test]
    fn everything_off_fills_nothing() {
        let off: Autofill = serde_json::from_str(
            r#"{"album_artist_from_artist":false,"various_artists":false,"artist_from_album_artist":false,
                "album_tags_from_tracks":false,"single_track":false,"single_disc":false,"disc_from_folder":false,"totals":false,
                "track_from_file_name":false,"title_from_file_name":false}"#,
        )
        .unwrap();
        let mut t = vec![track("01 A", "CD1", &[("artist", "X")])];
        assert!(fill(&off, "x", &mut t).iter().all(Vec::is_empty));
    }
}
