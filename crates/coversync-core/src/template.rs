//! Library folder structure templates, e.g. `{album_artist}/{album}/{track_number} - {title}`.
//!
//! Variables are rule catalog ids (see `rules::CATALOG`) plus `{year}`. Each `/`
//! starts a folder; the file extension is added automatically. `{track_number}`
//! is zero-padded to two digits. `{album:40}` keeps at most 40 characters of
//! the value, cut at a word break when one is close.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::{Error, Result, rules};

pub const DEFAULT: &str = "{album_artist}/{album}/{track_number} - {title}";

pub const SUGGESTIONS: &[&str] = &[
    DEFAULT,
    "{album_artist}/{year} - {album}/{track_number} - {title}",
    "{album_artist}/{album}/{disc_number}-{track_number} - {title}",
    "{genre}/{album_artist}/{album}/{track_number} - {title}",
    "{album_artist} - {album}/{track_number} {artist} - {title}",
];

/// `{year}` is the first four digits of the date tag.
pub const YEAR: &str = "year";
/// The longest a variable can be limited to; a whole component is capped
/// at [`MAX_NAME`] bytes anyway.
pub const MAX_LIMIT: usize = 180;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    Text(String),
    /// A variable, and the most characters of its value to keep.
    Var(&'static str, Option<usize>),
}

/// A parsed structure template: folders separated by `/`, the last being the file name.
#[derive(Debug, Clone)]
pub struct Template {
    segments: Vec<Vec<Part>>,
}

fn known(name: &str) -> Option<&'static str> {
    if name == YEAR {
        return Some(YEAR);
    }
    rules::kind(name).map(|k| k.id)
}

impl Template {
    pub fn parse(source: &str) -> Result<Self> {
        let source = source.trim();
        let invalid = |msg: String| Err(Error::Invalid(msg));
        if source.is_empty() {
            return invalid("The folder structure is empty.".into());
        }
        if source.starts_with('/') {
            return invalid(
                "The folder structure is relative to the library folder; drop the leading /."
                    .into(),
            );
        }
        let mut segments = Vec::new();
        for raw in source.split('/') {
            let raw = raw.trim();
            if raw.is_empty() {
                return invalid(
                    "The folder structure has an empty folder (two / in a row?).".into(),
                );
            }
            if raw == "." || raw == ".." {
                return invalid(format!("'{raw}' isn't allowed as a folder."));
            }
            let mut parts = Vec::new();
            let mut rest = raw;
            while !rest.is_empty() {
                match rest.find(['{', '}']) {
                    Some(i) if rest.as_bytes()[i] == b'}' => {
                        return invalid(format!("Unmatched }} in '{raw}'."));
                    }
                    Some(i) => {
                        if i > 0 {
                            parts.push(Part::Text(rest[..i].to_string()));
                        }
                        let Some(end) = rest[i..].find('}') else {
                            return invalid(format!("Unclosed {{ in '{raw}'."));
                        };
                        let inner = rest[i + 1..i + end].trim();
                        let (name, limit) = match inner.split_once(':') {
                            Some((name, limit)) => (name.trim(), Some(limit.trim())),
                            None => (inner, None),
                        };
                        let Some(id) = known(name) else {
                            return invalid(format!("Unknown variable {{{name}}}."));
                        };
                        let limit = match limit {
                            None => None,
                            Some(n) => match n.parse::<usize>() {
                                Ok(n @ 1..=MAX_LIMIT) => Some(n),
                                _ => {
                                    return invalid(format!(
                                        "{{{inner}}}: a length limit is a number of characters from 1 to {MAX_LIMIT}, like {{{name}:40}}."
                                    ));
                                }
                            },
                        };
                        parts.push(Part::Var(id, limit));
                        rest = &rest[i + end + 1..];
                    }
                    None => {
                        parts.push(Part::Text(rest.to_string()));
                        rest = "";
                    }
                }
            }
            segments.push(parts);
        }
        if !segments
            .last()
            .unwrap()
            .iter()
            .any(|p| matches!(p, Part::Var(..)))
        {
            return invalid("The file name (after the last /) needs at least one variable.".into());
        }
        Ok(Self { segments })
    }

    /// Variables used, in order of first appearance.
    pub fn variables(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        for part in self.segments.iter().flatten() {
            if let Part::Var(v, _) = part
                && !out.contains(v)
            {
                out.push(*v);
            }
        }
        out
    }

    /// Variables with a length limit, and the limit (the first, if a
    /// variable appears more than once).
    pub fn limits(&self) -> Vec<(&'static str, usize)> {
        let mut out: Vec<(&'static str, usize)> = Vec::new();
        for part in self.segments.iter().flatten() {
            if let Part::Var(v, Some(max)) = part
                && !out.iter().any(|(x, _)| x == v)
            {
                out.push((*v, *max));
            }
        }
        out
    }

    /// Catalog kinds a file needs values for to be placed by this template.
    pub fn required_kinds(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        for v in self.variables() {
            let id = if v == YEAR { "date" } else { v };
            if !out.contains(&id) {
                out.push(id);
            }
        }
        out
    }

    /// The relative path for a file with these tag values, keeping `extension`.
    /// Fails with the variables that have no value.
    pub fn render(
        &self,
        values: &HashMap<&'static str, String>,
        extension: &str,
    ) -> std::result::Result<PathBuf, Vec<&'static str>> {
        let (path, missing) = self.render_partial(values, extension);
        if missing.is_empty() {
            Ok(path)
        } else {
            Err(missing)
        }
    }

    /// The relative path even when variables have no value: those are left
    /// empty, and a name left empty is `_`. Also gives the variables that
    /// had no value.
    pub fn render_partial(
        &self,
        values: &HashMap<&'static str, String>,
        extension: &str,
    ) -> (PathBuf, Vec<&'static str>) {
        let mut missing = Vec::new();
        let mut path = PathBuf::new();
        let last = self.segments.len() - 1;
        for (i, segment) in self.segments.iter().enumerate() {
            let mut text = String::new();
            for part in segment {
                match part {
                    Part::Text(t) => text.push_str(t),
                    Part::Var(v, limit) => match value(values, v) {
                        Some(val) => {
                            let val = clean(&val);
                            text.push_str(&match limit {
                                Some(max) => shorten(&val, *max),
                                None => val,
                            })
                        }
                        None => {
                            if !missing.contains(v) {
                                missing.push(*v);
                            }
                        }
                    },
                }
            }
            let mut name = finish(&text);
            if name.is_empty() {
                name = "_".into();
            }
            if i == last && !extension.is_empty() {
                name = format!("{name}.{}", extension.to_lowercase());
            }
            path.push(name);
        }
        (path, missing)
    }
}

fn value(values: &HashMap<&'static str, String>, var: &str) -> Option<String> {
    let get = |k: &str| values.get(k).map(|v| v.trim()).filter(|v| !v.is_empty());
    match var {
        YEAR => {
            let digits: String = get("date")?
                .chars()
                .filter(char::is_ascii_digit)
                .take(4)
                .collect();
            (digits.len() == 4).then_some(digits)
        }
        "track_number" => {
            let v = get(var)?;
            let n = v.split('/').next().unwrap_or(v).trim();
            Some(match n.parse::<u32>() {
                Ok(n) => format!("{n:02}"),
                Err(_) => n.to_string(),
            })
        }
        "disc_number" => {
            let v = get(var)?;
            Some(v.split('/').next().unwrap_or(v).trim().to_string())
        }
        _ => get(var).map(str::to_string),
    }
}

/// Make a tag value safe inside one path component, also on SMB/Windows clients.
fn clean(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| match c {
            '/' | '\\' => '-',
            ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c => c,
        })
        .collect()
}

/// At most `max` characters of `value`. A cut mid-word moves back to the
/// last space if that keeps two thirds of the limit (titles in scripts
/// without spaces are cut where the limit falls), and trailing spaces and
/// separators go.
fn shorten(value: &str, max: usize) -> String {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= max {
        return value.to_string();
    }
    let mut cut = max;
    if chars[max].is_alphanumeric()
        && chars[max - 1].is_alphanumeric()
        && let Some(space) = chars[..max].iter().rposition(|c| c.is_whitespace())
        && space >= max * 2 / 3
    {
        cut = space;
    }
    let kept: String = chars[..cut].iter().collect();
    let tidy = kept.trim_end_matches(|c: char| c.is_whitespace() || "-_,.;&+~([{".contains(c));
    if tidy.is_empty() {
        kept
    } else {
        tidy.to_string()
    }
}

/// Trim, drop trailing dots (Windows), and cap the length of one component.
/// Dots and spaces go together, so no name is left as `.` or `..`.
fn finish(component: &str) -> String {
    let trailing = |c: char| c == '.' || c.is_whitespace();
    let s = component.trim().trim_end_matches(trailing);
    cut(s, MAX_NAME).trim_end_matches(trailing).to_string()
}

/// The most bytes of UTF-8 a file or folder name gets: below what Linux
/// (255 bytes), FAT32 and exFAT (255 UTF-16 units) allow.
pub const MAX_NAME: usize = 180;

/// At most `max` bytes of `s`, never splitting a character. Nor does it
/// part a character from the common marks that combine with it (accents,
/// dakuten, variation selectors, emoji joined by ZWJ; see
/// [`joins_previous`]), though it doesn't know every script's.
pub fn cut(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    loop {
        let next = s[end..].chars().next();
        let last = s[..end].chars().next_back();
        // A mark after the cut belongs to the character before it; a
        // joiner before the cut joins that character to the next.
        if next.is_some_and(joins_previous) || last == Some('\u{200d}') {
            end -= last.map_or(0, char::len_utf8);
        } else {
            return &s[..end];
        }
        if end == 0 {
            return "";
        }
    }
}

/// Characters drawn together with the one before them.
fn joins_previous(c: char) -> bool {
    matches!(c,
        '\u{0300}'..='\u{036f}' // combining diacritical marks
        | '\u{1ab0}'..='\u{1aff}'
        | '\u{1dc0}'..='\u{1dff}'
        | '\u{200c}'..='\u{200d}' // zero-width (non-)joiner
        | '\u{20d0}'..='\u{20ff}'
        | '\u{3099}'..='\u{309a}' // kana voicing marks
        | '\u{fe00}'..='\u{fe0f}' // variation selectors
        | '\u{fe20}'..='\u{fe2f}'
        | '\u{1f3fb}'..='\u{1f3ff}' // skin tones
        | '\u{e0020}'..='\u{e007f}' // emoji tag sequences
        | '\u{e0100}'..='\u{e01ef}')
}

/// Characters FAT32 and exFAT can't hold in a name, besides control ones.
const FAT_ILLEGAL: &[char] = &['"', '*', '/', ':', '<', '>', '?', '\\', '|'];

/// One file or folder name as a FAT32 or exFAT card can hold it, and
/// Windows can open it there. Characters those can't hold go, as do
/// leading and trailing spaces and trailing dots. A name Windows keeps for
/// a device (CON, NUL, COM1 and the like, with any extension) gets a `_` in
/// front. A name longer than [`MAX_NAME`] bytes is cut (see [`cut`]), and a
/// file keeps its extension. A name left empty is `_`.
pub fn fat_name(name: &str, file: bool) -> String {
    let clean: String = name
        .chars()
        .filter(|c| !c.is_control() && !FAT_ILLEGAL.contains(c))
        .collect();
    let (stem, ext) = if file {
        split_extension(&clean)
    } else {
        (clean.as_str(), "")
    };
    let tidy = |s: &str| s.trim().trim_end_matches(['.', ' ']).to_string();
    let max = MAX_NAME - ext.len();
    let mut stem = tidy(cut(&tidy(stem), max));
    // Checked once cut, which can leave a reserved name behind.
    if reserved(&stem) {
        stem = tidy(cut(&format!("_{stem}"), max));
    }
    if stem.is_empty() {
        format!("_{ext}")
    } else {
        format!("{stem}{ext}")
    }
}

/// A file name's stem and its extension, dot included ("" for none).
pub fn split_extension(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(dot)
            if name.len() - dot <= 17 && name.len() - dot > 1 && !name[dot..].contains(' ') =>
        {
            name.split_at(dot)
        }
        _ => (name, ""),
    }
}

/// Whether Windows keeps `stem` (up to its first dot) for a device.
fn reserved(stem: &str) -> bool {
    let base = stem.split('.').next().unwrap_or_default().trim_end();
    let upper = base.to_ascii_uppercase();
    if ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"].contains(&upper.as_str()) {
        return true;
    }
    ["COM", "LPT"].iter().any(|port| {
        upper.strip_prefix(port).is_some_and(|n| {
            let mut n = n.chars();
            n.next()
                .is_some_and(|d| d.is_ascii_digit() || "¹²³".contains(d))
                && n.next().is_none()
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vals(pairs: &[(&'static str, &str)]) -> HashMap<&'static str, String> {
        pairs.iter().map(|(k, v)| (*k, v.to_string())).collect()
    }

    #[test]
    fn renders_default_with_padding_and_extension() {
        let t = Template::parse(DEFAULT).unwrap();
        let v = vals(&[
            ("album_artist", "4ikai"),
            ("album", "Thaw"),
            ("track_number", "1/2"),
            ("title", "Thaw"),
        ]);
        assert_eq!(
            t.render(&v, "FLAC").unwrap(),
            PathBuf::from("4ikai/Thaw/01 - Thaw.flac")
        );
    }

    #[test]
    fn year_comes_from_date_and_counts_as_date() {
        let t = Template::parse("{album_artist}/{year} - {album}/{title}").unwrap();
        assert_eq!(
            t.required_kinds(),
            ["album_artist", "date", "album", "title"]
        );
        let v = vals(&[
            ("album_artist", "A"),
            ("date", "2003-04-01"),
            ("album", "B"),
            ("title", "C"),
        ]);
        assert_eq!(
            t.render(&v, "mp3").unwrap(),
            PathBuf::from("A/2003 - B/C.mp3")
        );
    }

    #[test]
    fn values_cannot_add_folders_or_reserved_characters() {
        let t = Template::parse("{artist}/{title}").unwrap();
        let v = vals(&[("artist", "AC/DC"), ("title", "What? Why: \"Now\"..")]);
        assert_eq!(
            t.render(&v, "flac").unwrap(),
            PathBuf::from("AC-DC/What_ Why_ _Now_.flac")
        );
    }

    #[test]
    fn a_name_is_never_dots_alone() {
        // With the value missing, or there, the folder isn't `..`.
        let t = Template::parse(".. .{composer}/{title}").unwrap();
        let (path, missing) = t.render_partial(&vals(&[("title", "T")]), "flac");
        assert_eq!(
            (path, missing),
            (PathBuf::from("_/T.flac"), vec!["composer"])
        );
        let t = Template::parse("{album}/{title}").unwrap();
        let v = vals(&[("album", ".. ."), ("title", "T")]);
        assert_eq!(t.render(&v, "flac").unwrap(), PathBuf::from("_/T.flac"));
    }

    #[test]
    fn limits_cut_values_at_word_breaks() {
        assert_eq!(
            shorten("MADE IN ABYSS ORIGINAL SOUNDTRACK", 20),
            "MADE IN ABYSS"
        );
        assert_eq!(shorten("Thaw", 20), "Thaw");
        // No space close enough: cut where the limit falls.
        assert_eq!(
            shorten("Supercalifragilistic Expialidocious", 10),
            "Supercalif"
        );
        assert_eq!(shorten("ラブライブ! Solo Live! III", 5), "ラブライブ");
        // Separators and spaces left dangling at the end go.
        assert_eq!(shorten("Kevin Penkin - Made in Abyss", 15), "Kevin Penkin");
        assert_eq!(shorten("Who (Live)", 5), "Who");

        let t =
            Template::parse("{album_artist:12}/{year} - {album:20}/{track_number} - {title : 8}")
                .unwrap();
        assert_eq!(
            t.required_kinds(),
            ["album_artist", "date", "album", "track_number", "title"]
        );
        assert_eq!(
            t.limits(),
            [("album_artist", 12), ("album", 20), ("title", 8)]
        );
        let v = vals(&[
            ("album_artist", "Kevin Penkin feat. Raj Ramayya"),
            ("date", "2017"),
            ("album", "MADE IN ABYSS ORIGINAL SOUNDTRACK"),
            ("track_number", "3"),
            ("title", "Hanezeve Caradhina"),
        ]);
        assert_eq!(
            t.render(&v, "flac").unwrap(),
            PathBuf::from("Kevin Penkin/2017 - MADE IN ABYSS/03 - Hanezeve.flac")
        );
    }

    #[test]
    fn fat_names() {
        for (name, file, safe) in [
            ("CON", false, "_CON"),
            ("con.flac", true, "_con.flac"),
            ("NUL .flac", true, "_NUL.flac"),
            ("LPT¹", false, "_LPT¹"),
            ("COM10", false, "COM10"),
            ("Console", false, "Console"),
            ("AC:DC? *Live* <1979>|\"", false, "ACDC Live 1979"),
            ("a\\b/c\u{7}", false, "abc"),
            ("Vol. 1... ", false, "Vol. 1"),
            (" Track . .flac", true, "Track.flac"),
            ("???", false, "_"),
            ("?.flac", true, "_.flac"),
            ("Mr. Brightside", false, "Mr. Brightside"),
        ] {
            assert_eq!(fat_name(name, file), safe, "{name:?}");
        }
        // Cut to MAX_NAME bytes, keeping the extension, and never inside a
        // character, between a letter and its accent, or inside an emoji.
        let long = format!("{}.flac", "曲".repeat(100));
        assert_eq!(fat_name(&long, true), format!("{}.flac", "曲".repeat(58)));
        let accent = format!("{}e\u{301}x.flac", "a".repeat(174));
        assert_eq!(fat_name(&accent, true), format!("{}.flac", "a".repeat(174)));
        let family = format!("{}👨\u{200d}👩", "b".repeat(170));
        assert_eq!(fat_name(&family, false), "b".repeat(170));
        // A name the cut leaves reserved is caught too.
        let padded = format!("CON{}X.flac", " ".repeat(174));
        assert_eq!(fat_name(&padded, true), "_CON.flac");
    }

    #[test]
    fn reports_missing_variables() {
        let t = Template::parse(DEFAULT).unwrap();
        let v = vals(&[("album_artist", "A"), ("album", " "), ("title", "T")]);
        assert_eq!(t.render(&v, "flac").unwrap_err(), ["album", "track_number"]);
    }

    #[test]
    fn rejects_bad_templates() {
        for bad in [
            "",
            "/{album}/{title}",
            "{album}//{title}",
            "{album}/..",
            "{nope}/{title}",
            "{album/{title}",
            "{album}}/{title}",
            "{album}/fixed",
            "{album:0}/{title}",
            "{album:x}/{title}",
            "{album:-3}/{title}",
            "{album:999}/{title}",
            "{album:}/{title}",
        ] {
            assert!(Template::parse(bad).is_err(), "{bad}");
        }
        for good in SUGGESTIONS {
            assert!(Template::parse(good).is_ok(), "{good}");
        }
    }
}
