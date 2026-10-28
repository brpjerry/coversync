//! Romanized tags for releases whose titles and names don't sort A–Z
//! (Japanese, Chinese, Korean, Cyrillic…), from what MusicBrainz offers: a
//! transliterated or translated tracklist (a pseudo-release linked to the
//! release, or sharing its recordings), artist and label aliases, and the
//! Latin sort names MusicBrainz gives artists in other scripts.

use std::collections::{HashMap, HashSet};

use crate::musicbrainz::{Alias, Credit, Release, Track};

/// A Latin letter, accented ones and fullwidth A–Z included.
fn latin(c: char) -> bool {
    matches!(c as u32,
        0x41..=0x5A | 0x61..=0x7A | 0xAA | 0xBA | 0xC0..=0x24F | 0x1E00..=0x1EFF
        | 0x2C60..=0x2C7F | 0xA720..=0xA7FF | 0xAB30..=0xAB6F | 0xFF21..=0xFF3A | 0xFF41..=0xFF5A)
}

/// Whether `text` sorts A–Z: every letter in it is Latin. Digits,
/// punctuation and symbols don't count either way.
pub fn sorts_az(text: &str) -> bool {
    text.chars().filter(|c| c.is_alphabetic()).all(latin)
}

/// Latin text with at least one letter, worth using as a name.
fn usable(text: &str) -> bool {
    sorts_az(text) && text.chars().any(char::is_alphabetic)
}

/// How romanized names are written.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Options {
    /// People's names romanized from Chinese, Japanese or Korean keep the
    /// family name first, as those languages write it: "Nishikino Maki".
    pub family_first: bool,
}

/// Han, kana and Hangul: scripts that write a person's family name first.
fn family_first_script(c: char) -> bool {
    matches!(c as u32,
        0x1100..=0x11FF | 0x3040..=0x30FF | 0x3130..=0x318F | 0x31F0..=0x31FF
        | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xAC00..=0xD7AF | 0xF900..=0xFAFF
        | 0xFF66..=0xFF9F | 0x20000..=0x2FA1F)
}

/// Words compared loosely: letters and digits only, any case.
fn key(words: &[&str]) -> String {
    words
        .iter()
        .flat_map(|w| w.chars())
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// A romanized name with the family name moved to the front, best effort.
/// The sort name ("Nishikino, Maki") says which words are the family name,
/// so names of three words or more work when it does; a sort name without
/// a comma means a single name (a stage name), which stays as it is. With
/// no sort name to go by, two words swap and anything else stays.
fn family_first(name: &str, sort: Option<&str>) -> String {
    let words: Vec<&str> = name.split_whitespace().collect();
    let swap = |at: usize| format!("{} {}", words[at..].join(" "), words[..at].join(" "));
    match sort.filter(|s| usable(s)).map(|s| s.split_once(", ")) {
        Some(Some((family, given))) if !given.contains(", ") => {
            let family: Vec<&str> = family.split_whitespace().collect();
            let n = family.len();
            if words.len() <= n {
                name.to_string()
            } else if key(&words[words.len() - n..]) == key(&family) {
                swap(words.len() - n)
            } else if key(&words[..n]) == key(&family) {
                // Already family name first.
                name.to_string()
            } else if words.len() == 2 {
                // Spelled unlike the sort name ("Yūki" for "Yuuki", say).
                swap(1)
            } else {
                name.to_string()
            }
        }
        Some(_) => name.to_string(),
        None if words.len() == 2 => swap(1),
        None => name.to_string(),
    }
}

/// People and characters have family names; groups, orchestras and choirs don't.
fn has_family_name(kind: Option<&str>) -> bool {
    !matches!(kind, Some("Group" | "Orchestra" | "Choir"))
}

/// "Sadohara, Kaori" → "Kaori Sadohara", as MusicBrainz sort names put the
/// family name first.
fn unsort(sort: &str) -> String {
    match sort.split_once(", ") {
        Some((last, first)) if !first.contains(", ") => format!("{first} {last}"),
        _ => sort.to_string(),
    }
}

/// The best Latin alias: English and primary for its locale first. Search
/// hints (misspellings and the like) never count.
fn alias(aliases: &[Alias]) -> Option<&str> {
    aliases
        .iter()
        .filter(|a| a.kind.as_deref() != Some("Search hint") && usable(&a.name))
        .filter_map(|a| {
            let locale = a.locale.as_deref().unwrap_or("");
            let english = locale == "en" || locale.starts_with("en_") || locale.starts_with("en-");
            let primary = a.primary == Some(true);
            match (english, primary) {
                (true, true) => Some((0, a)),
                (true, false) => Some((1, a)),
                (false, true) => Some((2, a)),
                (false, false) => None,
            }
        })
        .min_by_key(|(rank, _)| *rank)
        .map(|(_, a)| a.name.as_str())
}

/// A name in Latin script for `name`, from aliases or the sort name.
fn latin_name(name: &str, aliases: &[Alias], sort_name: Option<&str>) -> Option<String> {
    if sorts_az(name) {
        return None;
    }
    alias(aliases)
        .map(str::to_string)
        .or_else(|| sort_name.filter(|s| usable(s)).map(unsort))
}

/// Join phrases in fullwidth or Japanese punctuation, in ASCII.
fn join(phrase: &str) -> String {
    match phrase.trim() {
        "と" | "・" | "＆" | "&" => return " & ".into(),
        "、" | "，" => return ", ".into(),
        _ => {}
    }
    phrase
        .replace(['、', '，'], ", ")
        .replace('＆', " & ")
        .replace('（', " (")
        .replace('）', ")")
        .replace('：', ": ")
        .replace('／', " / ")
        .replace('　', " ")
}

fn romanize_credit(credit: &mut [Credit], options: Options) -> u32 {
    let mut changed = 0;
    for c in credit {
        let sort = c.artist.sort_name.as_deref();
        if let Some(name) = latin_name(&c.name, &c.artist.aliases, sort) {
            let reorder = options.family_first
                && c.name.chars().any(family_first_script)
                && has_family_name(c.artist.kind.as_deref());
            c.name = if reorder {
                family_first(&name, sort)
            } else {
                name
            };
            changed += 1;
        }
        c.joinphrase = join(&c.joinphrase);
    }
    changed
}

fn recordings(release: &Release) -> HashSet<&str> {
    release
        .media
        .iter()
        .flat_map(|m| &m.tracks)
        .filter_map(|t| t.recording.as_ref().map(|r| r.id.as_str()))
        .collect()
}

/// Releases linked as `release`'s tracklist in Latin script, best first:
/// transliterations (same language) before translations.
pub fn linked_tracklists(release: &Release) -> Vec<String> {
    let language = release.text.as_ref().and_then(|t| t.language.as_deref());
    let mut links: Vec<(bool, &str)> = release
        .relations
        .iter()
        .filter(|r| r.kind == "transl-tracklisting" && r.direction == "forward")
        .filter_map(|r| r.release.as_ref())
        .filter(|r| {
            r.text.as_ref().and_then(|t| t.script.as_deref()) == Some("Latn") || usable(&r.title)
        })
        .map(|r| {
            let translated = r.text.as_ref().and_then(|t| t.language.as_deref()) != language;
            (translated, r.id.as_str())
        })
        .collect();
    links.sort();
    links.into_iter().map(|(_, id)| id.to_string()).collect()
}

/// Whether `tracklist` lists `release`'s tracks in Latin script: mostly the
/// same recordings, titled in Latin script.
pub fn is_tracklist_for(tracklist: &Release, release: &Release) -> bool {
    let latin = tracklist.text.as_ref().and_then(|t| t.script.as_deref()) == Some("Latn")
        || usable(&tracklist.title);
    let ours = recordings(release);
    let shared = recordings(tracklist).intersection(&ours).count();
    latin && tracklist.id != release.id && !ours.is_empty() && shared * 2 >= ours.len()
}

/// Whether the tracklist translates rather than transliterates.
pub fn is_translation(tracklist: &Release, release: &Release) -> bool {
    let language = |r: &Release| r.text.as_ref().and_then(|t| t.language.clone());
    language(tracklist) != language(release)
}

/// Whether any title or name on the release doesn't sort A–Z.
pub fn needs_romanizing(release: &Release) -> bool {
    let credit = |c: &[Credit]| c.iter().any(|c| !sorts_az(&c.name));
    !sorts_az(&release.title)
        || credit(&release.artist_credit)
        || release
            .media
            .iter()
            .flat_map(|m| &m.tracks)
            .any(|t| !sorts_az(&t.title) || credit(&t.artist_credit))
}

/// What romanizing a release changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// Album, disc and track titles romanized.
    pub titles: u32,
    /// Artist and label names romanized.
    pub names: u32,
    /// Titles and names still not sorting A–Z: MusicBrainz has nothing Latin.
    pub left: u32,
}

/// `release` with every title and name that doesn't sort A–Z replaced by a
/// Latin one where MusicBrainz has it.
pub fn romanize(
    release: &Release,
    tracklist: Option<&Release>,
    options: Options,
) -> (Release, Counts) {
    let mut out = release.clone();
    let mut titles = 0;
    let mut set = |target: &mut String, value: Option<&str>| {
        if let Some(v) = value.filter(|v| usable(v)).filter(|_| !sorts_az(target)) {
            *target = v.to_string();
            titles += 1;
        }
    };

    let group = release.release_group.as_ref();
    let title = tracklist
        .map(|t| t.title.as_str())
        .filter(|t| usable(t))
        .or_else(|| alias(&release.aliases))
        .or_else(|| group.and_then(|g| alias(&g.aliases)));
    set(&mut out.title, title);

    if let Some(list) = tracklist {
        let tracks: Vec<(u32, &Track)> = list
            .media
            .iter()
            .enumerate()
            .flat_map(|(i, m)| {
                let disc = m.position.unwrap_or(i as u32 + 1);
                m.tracks.iter().map(move |t| (disc, t))
            })
            .collect();
        let by_recording: HashMap<&str, &Track> = tracks
            .iter()
            .filter_map(|(_, t)| Some((t.recording.as_ref()?.id.as_str(), *t)))
            .collect();
        let by_position: HashMap<(u32, u32), &Track> = tracks
            .iter()
            .filter_map(|(disc, t)| Some(((*disc, t.position?), *t)))
            .collect();
        for (i, medium) in out.media.iter_mut().enumerate() {
            let disc = medium.position.unwrap_or(i as u32 + 1);
            if let Some(title) = medium.title.as_mut() {
                let theirs = list
                    .media
                    .iter()
                    .find(|m| m.position == Some(disc))
                    .and_then(|m| m.title.as_deref());
                set(title, theirs);
            }
            for track in &mut medium.tracks {
                let theirs = track
                    .recording
                    .as_ref()
                    .and_then(|r| by_recording.get(r.id.as_str()))
                    .or_else(|| track.position.and_then(|p| by_position.get(&(disc, p))))
                    .map(|t| t.title.as_str());
                set(&mut track.title, theirs);
            }
        }
    }

    let mut names = romanize_credit(&mut out.artist_credit, options);
    for track in out.media.iter_mut().flat_map(|m| &mut m.tracks) {
        names += romanize_credit(&mut track.artist_credit, options);
    }
    for label in out.label_info.iter_mut().filter_map(|l| l.label.as_mut()) {
        if let Some(name) = latin_name(&label.name, &label.aliases, label.sort_name.as_deref()) {
            label.name = name;
            names += 1;
        }
    }

    let tracks = out.media.iter().flat_map(|m| &m.tracks);
    let left = std::iter::once(out.title.as_str())
        .chain(out.media.iter().filter_map(|m| m.title.as_deref()))
        .chain(tracks.clone().map(|t| t.title.as_str()))
        .chain(out.artist_credit.iter().map(|c| c.name.as_str()))
        .chain(
            tracks
                .flat_map(|t| &t.artist_credit)
                .map(|c| c.name.as_str()),
        )
        .chain(
            out.label_info
                .iter()
                .filter_map(|l| Some(l.label.as_ref()?.name.as_str())),
        )
        .filter(|v| !sorts_az(v))
        .count() as u32;
    (
        out,
        Counts {
            titles,
            names,
            left,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_sorts_a_to_z() {
        assert!(sorts_az("Hyperborea"));
        assert!(sorts_az("Björk – Homogénic (2×CD)"));
        assert!(sorts_az("24h"));
        assert!(!sorts_az("寓話"));
        assert!(!sorts_az("DAOKO × ドラガリアロスト"));
        assert!(!sorts_az("Кино"));
        assert!(!sorts_az("μ's"));
        assert_eq!(unsort("Sadohara, Kaori"), "Kaori Sadohara");
        assert_eq!(unsort("KAF"), "KAF");
        assert_eq!(join("、"), ", ");
        assert_eq!(join("（CV："), " (CV: ");
    }

    fn release(json: &str) -> Release {
        serde_json::from_str(json).unwrap()
    }

    const ORIGINAL: &str = r#"{
        "id": "orig", "title": "寓話", "status": "Official",
        "text-representation": {"language": "jpn", "script": "Jpan"},
        "artist-credit": [{"name": "花譜", "artist": {"id": "kaf", "sort-name": "KAF",
            "aliases": [{"name": "KAF", "locale": "en", "primary": true, "type": "Artist name"},
                        {"name": "Kafu", "type": "Search hint"}]}}],
        "release-group": {"id": "rg", "aliases": [{"name": "Fable (album)", "locale": "en", "primary": true}]},
        "label-info": [{"label": {"name": "ランティス", "sort-name": "Lantis"}}],
        "relations": [
            {"type": "transl-tracklisting", "direction": "forward",
             "release": {"id": "translation", "title": "Fable", "text-representation": {"language": "eng", "script": "Latn"}}},
            {"type": "transl-tracklisting", "direction": "forward",
             "release": {"id": "transliteration", "title": "Guuwa", "text-representation": {"language": "jpn", "script": "Latn"}}},
            {"type": "transl-tracklisting", "direction": "backward",
             "release": {"id": "other", "title": "Other", "text-representation": {"script": "Latn"}}}
        ],
        "media": [{"position": 1, "tracks": [
            {"id": "t1", "position": 1, "title": "何者", "recording": {"id": "r1"},
             "artist-credit": [
                {"name": "安中さん", "joinphrase": "(CV:", "artist": {"id": "a", "sort-name": "Annaka-san"}},
                {"name": "佐土原かおり", "joinphrase": ")", "artist": {"id": "b", "sort-name": "Sadohara, Kaori"}}]},
            {"id": "t2", "position": 2, "title": "Interlude", "recording": {"id": "r2"}}
        ]}]
    }"#;

    #[test]
    fn transliterations_come_before_translations() {
        assert_eq!(
            linked_tracklists(&release(ORIGINAL)),
            ["transliteration", "translation"]
        );
    }

    #[test]
    fn romanizes_titles_names_and_labels() {
        let original = release(ORIGINAL);
        let tracklist = release(
            r#"{"id": "transliteration", "title": "Guuwa", "status": "Pseudo-Release",
                "text-representation": {"language": "jpn", "script": "Latn"},
                "media": [{"position": 1, "tracks": [
                    {"id": "p1", "position": 1, "title": "Nanimono", "recording": {"id": "r1"}},
                    {"id": "p2", "position": 2, "title": "Interlude (Latin already)", "recording": {"id": "r2"}}]}]}"#,
        );
        assert!(is_tracklist_for(&tracklist, &original));
        assert!(!is_translation(&tracklist, &original));
        let (r, counts) = romanize(&original, Some(&tracklist), Options::default());
        assert_eq!(r.title, "Guuwa");
        let tracks = &r.media[0].tracks;
        assert_eq!(tracks[0].title, "Nanimono");
        // Titles that already sort A–Z stay as the release has them.
        assert_eq!(tracks[1].title, "Interlude");
        assert_eq!(crate::musicbrainz::credit_name(&r.artist_credit), "KAF");
        assert_eq!(
            crate::musicbrainz::credit_name(&tracks[0].artist_credit),
            "Annaka-san(CV:Kaori Sadohara)"
        );
        assert_eq!(r.label_info[0].label.as_ref().unwrap().name, "Lantis");
        assert_eq!(
            counts,
            Counts {
                titles: 2,
                names: 4,
                left: 0
            }
        );

        // No tracklist: the album title comes from an alias.
        let (r, counts) = romanize(&original, None, Options::default());
        assert_eq!(r.title, "Fable (album)");
        assert_eq!(r.media[0].tracks[0].title, "何者");
        assert_eq!((counts.titles, counts.left), (1, 1));
    }

    #[test]
    fn family_names_first_best_effort() {
        let ff = family_first;
        assert_eq!(
            ff("Maki Nishikino", Some("Nishikino, Maki")),
            "Nishikino Maki"
        );
        assert_eq!(
            ff("Kaori Sadohara", Some("Sadohara, Kaori")),
            "Sadohara Kaori"
        );
        // Already in that order.
        assert_eq!(ff("Hatsune Miku", Some("Hatsune, Miku")), "Hatsune Miku");
        // The sort name says which words make the family name.
        assert_eq!(ff("Dick Van Dyke", Some("Van Dyke, Dick")), "Van Dyke Dick");
        assert_eq!(ff("Yūki Kaji", Some("Kaji, Yuuki")), "Kaji Yūki");
        assert_eq!(ff("Ann Mary Lee", Some("Lee, Ann Mary")), "Lee Ann Mary");
        // Single names and names the sort name doesn't explain stay.
        assert_eq!(ff("KAF", Some("KAF")), "KAF");
        assert_eq!(ff("Tokyo Jihen", Some("Tokyo Jihen")), "Tokyo Jihen");
        assert_eq!(ff("Annaka-san", Some("Annaka-san")), "Annaka-san");
        assert_eq!(ff("A B C", Some("X, Y")), "A B C");
        // No sort name: two words swap, others stay.
        assert_eq!(ff("Aya Uchida", None), "Uchida Aya");
        assert_eq!(ff("Aya Mari Uchida", None), "Aya Mari Uchida");
    }

    #[test]
    fn only_east_asian_people_get_family_names_first() {
        let original = release(
            r#"{"id": "r", "title": "x",
            "artist-credit": [
                {"name": "西木野真姫", "joinphrase": "(CV.", "artist": {"id": "a", "type": "Character", "sort-name": "Nishikino, Maki",
                    "aliases": [{"name": "Maki Nishikino", "locale": "en", "primary": true}]}},
                {"name": "Pile", "joinphrase": ") & ", "artist": {"id": "b", "type": "Person", "sort-name": "Pile"}},
                {"name": "東京事変", "joinphrase": " & ", "artist": {"id": "c", "type": "Group", "sort-name": "Jihen, Tokyo",
                    "aliases": [{"name": "Tokyo Jihen", "locale": "en", "primary": true}]}},
                {"name": "Виктор Цой", "artist": {"id": "d", "type": "Person", "sort-name": "Tsoi, Viktor",
                    "aliases": [{"name": "Viktor Tsoi", "locale": "en", "primary": true}]}}
            ]}"#,
        );
        let credit = |options| {
            let (r, _) = romanize(&original, None, options);
            crate::musicbrainz::credit_name(&r.artist_credit)
        };
        assert_eq!(
            credit(Options::default()),
            "Maki Nishikino(CV.Pile) & Tokyo Jihen & Viktor Tsoi"
        );
        assert_eq!(
            credit(Options { family_first: true }),
            "Nishikino Maki(CV.Pile) & Tokyo Jihen & Viktor Tsoi"
        );
    }
}
