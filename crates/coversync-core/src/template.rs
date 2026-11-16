//! Library folder structure templates, e.g. `{album_artist}/{album}/{track_number} - {title}`.
//!
//! Variables are rule catalog ids (see `rules::CATALOG`) plus `{year}`. Each `/`
//! starts a folder; the file extension is added automatically. `{track_number}`
//! is zero-padded to two digits. `{album:40}` keeps at most 40 characters of
//! the value, cut at a word break when one is close.
//!
//! A part in `< >` is optional: `<{disc_number}->` is left out when a
//! variable in it has no value, when it holds `{disc_number}` and the album
//! has one disc, or when it holds `{track_number}` and the album is a
//! single (one track on one disc). A folder that's nothing but such parts
//! is left out with them. Variables only in optional parts aren't required.
//! The album's disc and track totals come from the `disc_total` and
//! `track_total` values, or a `2/3` style number.
//!
//! Every name a template makes can be written on Windows, SMB shares and
//! FAT32 or exFAT cards: values have the characters those can't hold
//! replaced, a template's own text can't have them, names Windows keeps
//! for devices (CON, NUL, COM1, …) get a `_` in front, and no name ends in
//! a dot or space or is `..`. A folder name or a file's name before its
//! extension is at most [`MAX_NAME`] bytes. How long a whole path may be is
//! up to where it goes.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::{Error, Result, rules};

/// A disc number only when the album has more than one disc, and a track
/// number unless it's a single.
pub const DEFAULT: &str = "{album_artist}/{album}/<{disc_number}-><{track_number} - >{title}";

pub const SUGGESTIONS: &[&str] = &[
    DEFAULT,
    "{album_artist}/{year} - {album}/<{disc_number}-><{track_number} - >{title}",
    "{album_artist}/{album}/<Disc {disc_number}>/<{track_number} - >{title}",
    "{album_artist}/{album}/{track_number} - {title}",
    "{genre}/{album_artist}/{album}/<{disc_number}-><{track_number} - >{title}",
    "{album_artist} - {album}/<{disc_number}-><{track_number} >{artist} - {title}",
];

/// `{year}` is the first four digits of the date tag.
pub const YEAR: &str = "year";
/// The longest a variable can be limited to; a folder name or a file's
/// name before its extension is capped at [`MAX_NAME`] bytes anyway.
pub const MAX_LIMIT: usize = 180;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    Text(String),
    /// A variable, and the most characters of its value to keep.
    Var(&'static str, Option<usize>),
    /// Text and variables in `< >`, left out as a whole (see the module's
    /// notes). Never holds another.
    Optional(Vec<Part>),
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

fn invalid<T>(msg: String) -> Result<T> {
    Err(Error::Invalid(msg))
}

/// One folder's or the file name's parts: text, variables and optional
/// parts in `< >`.
fn segment(raw: &str) -> Result<Vec<Part>> {
    let mut parts = Vec::new();
    let mut rest = raw;
    loop {
        match rest.find(['<', '>']) {
            None => {
                parts.extend(tokens(rest, raw)?);
                return Ok(parts);
            }
            Some(i) if rest.as_bytes()[i] == b'>' => {
                return invalid(format!("Unmatched > in '{raw}'."));
            }
            Some(i) => {
                parts.extend(tokens(&rest[..i], raw)?);
                let inside = &rest[i + 1..];
                let Some(end) = inside.find(['<', '>']) else {
                    return invalid(format!("Unclosed < in '{raw}'."));
                };
                if inside.as_bytes()[end] == b'<' {
                    return invalid(format!("A part in < > can't hold another, in '{raw}'."));
                }
                let optional = tokens(&inside[..end], raw)?;
                if !optional.iter().any(|p| matches!(p, Part::Var(..))) {
                    return invalid(format!(
                        "A part in < > needs a variable to depend on, like <{{disc_number}}->, in '{raw}'."
                    ));
                }
                parts.push(Part::Optional(optional));
                rest = &inside[end + 1..];
            }
        }
    }
}

/// Text and `{variables}` in `chunk`, a piece of the folder `raw`.
fn tokens(chunk: &str, raw: &str) -> Result<Vec<Part>> {
    let mut parts = Vec::new();
    let mut rest = chunk;
    while !rest.is_empty() {
        match rest.find(['{', '}']) {
            Some(i) if rest.as_bytes()[i] == b'}' => {
                return invalid(format!("Unmatched }} in '{raw}'."));
            }
            Some(i) => {
                if i > 0 {
                    parts.push(text(&rest[..i])?);
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
                parts.push(text(rest)?);
                rest = "";
            }
        }
    }
    Ok(parts)
}

/// Every variable part, and whether it's in an optional part.
fn vars(parts: &[Part]) -> impl Iterator<Item = (&'static str, Option<usize>, bool)> + '_ {
    parts
        .iter()
        .flat_map(|part| -> Box<dyn Iterator<Item = _>> {
            match part {
                Part::Text(_) => Box::new(std::iter::empty()),
                Part::Var(v, limit) => Box::new(std::iter::once((*v, *limit, false))),
                Part::Optional(inner) => Box::new(inner.iter().filter_map(|p| match p {
                    Part::Var(v, limit) => Some((*v, *limit, true)),
                    _ => None,
                })),
            }
        })
}

impl Template {
    pub fn parse(source: &str) -> Result<Self> {
        let source = source.trim();
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
            segments.push(segment(raw)?);
        }
        if !segments
            .last()
            .unwrap()
            .iter()
            .any(|p| matches!(p, Part::Var(..)))
        {
            return invalid(
                "The file name (after the last /) needs at least one variable outside < >.".into(),
            );
        }
        Ok(Self { segments })
    }

    fn all_vars(&self) -> impl Iterator<Item = (&'static str, Option<usize>, bool)> + '_ {
        self.segments.iter().flat_map(|s| vars(s))
    }

    /// Variables used, in order of first appearance, optional parts' too.
    pub fn variables(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        for (v, _, _) in self.all_vars() {
            if !out.contains(&v) {
                out.push(v);
            }
        }
        out
    }

    /// Variables with a length limit, and the limit (the first, if a
    /// variable appears more than once).
    pub fn limits(&self) -> Vec<(&'static str, usize)> {
        let mut out: Vec<(&'static str, usize)> = Vec::new();
        for (v, limit, _) in self.all_vars() {
            if let Some(max) = limit
                && !out.iter().any(|(x, _)| *x == v)
            {
                out.push((v, max));
            }
        }
        out
    }

    /// Catalog kinds a file needs values for to be placed by this template:
    /// those of the variables outside optional parts.
    pub fn required_kinds(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        for (v, _, optional) in self.all_vars() {
            let id = kind_of(v);
            if !optional && !out.contains(&id) {
                out.push(id);
            }
        }
        out
    }

    /// Whether an optional part depends on the album having one disc (see
    /// [`one_disc`]): one holds the disc or track number.
    pub fn counts_discs(&self) -> bool {
        self.all_vars()
            .any(|(v, _, optional)| optional && (v == "disc_number" || v == "track_number"))
    }

    /// Catalog kinds whose values this template reads: every variable's,
    /// and the disc and track numbers and totals when an optional part
    /// depends on the album having one disc or one track.
    pub fn value_kinds(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        for (v, _, optional) in self.all_vars() {
            let mut kinds = vec![kind_of(v)];
            if optional && (v == "disc_number" || v == "track_number") {
                kinds.extend(["disc_number", "disc_total", "track_number", "track_total"]);
            }
            for id in kinds {
                if !out.contains(&id) {
                    out.push(id);
                }
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
    /// had no value, optional parts' aside.
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
            // Whether the folder is nothing but optional parts, all left out.
            let mut left_out = segment.iter().all(|p| matches!(p, Part::Optional(_)));
            for part in segment {
                match part {
                    Part::Text(t) => text.push_str(t),
                    Part::Var(v, limit) => match value(values, v) {
                        Some(val) => text.push_str(&shown(&val, *limit)),
                        None => {
                            if !missing.contains(v) {
                                missing.push(*v);
                            }
                        }
                    },
                    Part::Optional(parts) => {
                        if let Some(t) = optional(parts, values) {
                            text.push_str(&t);
                            left_out = false;
                        }
                    }
                }
            }
            if left_out && i != last {
                continue;
            }
            let mut name = finish(&text);
            if reserved(&name) {
                name = finish(&format!("_{name}"));
            }
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

/// The catalog kind a variable's value comes from.
fn kind_of(var: &'static str) -> &'static str {
    if var == YEAR { "date" } else { var }
}

/// A value as a name holds it, cut to `limit` characters.
fn shown(value: &str, limit: Option<usize>) -> String {
    let value = clean(value);
    match limit {
        Some(max) => shorten(&value, max),
        None => value,
    }
}

/// An optional part's text, or None when it's left out.
fn optional(parts: &[Part], values: &HashMap<&'static str, String>) -> Option<String> {
    let mut text = String::new();
    for part in parts {
        match part {
            Part::Text(t) => text.push_str(t),
            Part::Var(v, limit) => {
                let only = match *v {
                    "disc_number" => one_disc(values),
                    "track_number" => one_disc(values) && total(values, "track") == Some(1),
                    _ => false,
                };
                if only {
                    return None;
                }
                text.push_str(&shown(&value(values, v)?, *limit));
            }
            Part::Optional(_) => unreachable!("optional parts don't nest"),
        }
    }
    Some(text)
}

/// The album's disc or track total (`what` is "disc" or "track"): its
/// `_total` value, or what follows the `/` in a `2/3` style number. Only
/// the leading digits count, as tags written by hand can have more after.
fn total(values: &HashMap<&'static str, String>, what: &str) -> Option<u32> {
    let get = |k: &str| values.get(k).map(|v| v.trim()).filter(|v| !v.is_empty());
    let number = |v: &str| {
        let v = v.trim();
        v[..v.find(|c: char| !c.is_ascii_digit()).unwrap_or(v.len())]
            .parse::<u32>()
            .ok()
    };
    get(&format!("{what}_total"))
        .and_then(number)
        .or_else(|| number(get(&format!("{what}_number"))?.split_once('/')?.1))
}

/// Whether a track's album has one disc, as optional parts decide it: a
/// disc total of 1, or no disc number or total at all. A disc number
/// without a total, or a total that isn't 1 or can't be read, could be one
/// of several, so it doesn't count.
pub fn one_disc(values: &HashMap<&'static str, String>) -> bool {
    let empty = |k: &str| values.get(k).is_none_or(|v| v.trim().is_empty());
    match total(values, "disc") {
        Some(n) => n == 1,
        None => empty("disc_number") && empty("disc_total"),
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

/// The most bytes of UTF-8 a folder name, or a file's name before its
/// extension, gets: leaving room for the extension within what Linux (255
/// bytes), FAT32 and exFAT (255 UTF-16 units) allow.
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

/// Characters a name can't hold on Windows, SMB shares, or FAT32 and
/// exFAT cards, besides `/` and control characters. Values have them
/// replaced (see [`clean`]); a template's own text can't have them.
pub(crate) const NOT_IN_NAMES: &[char] = &['"', '*', ':', '<', '>', '?', '\\', '|'];

/// Text a template writes as it is, if a name can hold it.
fn text(t: &str) -> Result<Part> {
    match t
        .chars()
        .find(|c| c.is_control() || NOT_IN_NAMES.contains(c))
    {
        Some(c) => Err(Error::Invalid(format!(
            "{} can't be in a file or folder name on Windows or a memory card; use - or _ instead.",
            if c.is_control() {
                "A control character".to_string()
            } else {
                format!("'{c}'")
            }
        ))),
        None => Ok(Part::Text(t.to_string())),
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

/// Whether Windows keeps `stem` (up to its first dot) for a device, as
/// CON, NUL, COM1 and the like, which FAT cards and SMB shares can't hold
/// either. Such a name gets a `_` in front.
pub(crate) fn reserved(stem: &str) -> bool {
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
    fn optional_parts_leave_out_an_only_disc_and_a_single_track_number() {
        let t = Template::parse(DEFAULT).unwrap();
        let path = |pairs: &[(&'static str, &str)]| {
            let mut v = vals(&[("album_artist", "A"), ("album", "B"), ("title", "T")]);
            v.extend(vals(pairs));
            t.render(&v, "flac").unwrap()
        };
        for (pairs, want) in [
            // More than one disc: the disc number is there.
            (
                &[("disc_number", "2/2"), ("track_number", "5/12")][..],
                "2-05 - T",
            ),
            (
                &[
                    ("disc_number", "2"),
                    ("disc_total", "3"),
                    ("track_number", "5"),
                ],
                "2-05 - T",
            ),
            // One disc, by its total or for want of any disc number.
            (
                &[("disc_number", "1/1"), ("track_number", "5/12")],
                "05 - T",
            ),
            (
                &[
                    ("disc_number", "1"),
                    ("disc_total", "1"),
                    ("track_number", "5"),
                ],
                "05 - T",
            ),
            (&[("track_number", "5")], "05 - T"),
            // A single: no track number either.
            (&[("disc_number", "1/1"), ("track_number", "1/1")], "T"),
            // Totals as tagging tools sometimes leave them.
            (
                &[
                    ("disc_number", "1"),
                    ("disc_total", "1--tag=totaldiscs="),
                    ("track_number", "1"),
                    ("track_total", " 01 "),
                ],
                "T",
            ),
            (&[("track_number", "1"), ("track_total", "1")], "T"),
            // A disc of one track on a two-disc album keeps both.
            (
                &[("disc_number", "2/2"), ("track_number", "1/1")],
                "2-01 - T",
            ),
            // A disc number without a total could be one of several, as
            // could a total of 0 or one that can't be read.
            (&[("disc_number", "1"), ("track_number", "5")], "1-05 - T"),
            (
                &[
                    ("disc_number", "2"),
                    ("disc_total", "0"),
                    ("track_number", "1"),
                    ("track_total", "1"),
                ],
                "2-01 - T",
            ),
            (&[("disc_total", "?"), ("track_number", "1/1")], "01 - T"),
            (&[("disc_number", "1"), ("track_number", "1/1")], "1-01 - T"),
            // No track number: left out, not missing.
            (&[("disc_number", "1/1")], "T"),
        ] {
            assert_eq!(
                path(pairs),
                PathBuf::from(format!("A/B/{want}.flac")),
                "{pairs:?}"
            );
        }
        // Variables only in optional parts aren't required; the totals
        // they depend on are read.
        assert_eq!(t.required_kinds(), ["album_artist", "album", "title"]);
        assert_eq!(
            t.value_kinds(),
            [
                "album_artist",
                "album",
                "disc_number",
                "disc_total",
                "track_number",
                "track_total",
                "title"
            ]
        );
        assert_eq!(
            t.variables(),
            [
                "album_artist",
                "album",
                "disc_number",
                "track_number",
                "title"
            ]
        );
    }

    #[test]
    fn optional_parts_go_without_their_values_and_take_their_folder() {
        let t = Template::parse("{album}< ({year:4})>/<Disc {disc_number}>/{title}").unwrap();
        assert_eq!(t.required_kinds(), ["album", "title"]);
        assert_eq!(t.limits(), [("year", 4)]);
        let v = vals(&[
            ("album", "B"),
            ("date", "2003-01-02"),
            ("disc_number", "2/2"),
            ("title", "T"),
        ]);
        assert_eq!(
            t.render(&v, "flac").unwrap(),
            PathBuf::from("B (2003)/Disc 2/T.flac")
        );
        // No date, one disc: the year goes, and so does the disc's folder.
        let v = vals(&[("album", "B"), ("title", "T")]);
        assert_eq!(t.render(&v, "flac").unwrap(), PathBuf::from("B/T.flac"));
        // A folder with more than optional parts stays, as `_` if empty.
        let t = Template::parse("{album}/{genre}<{disc_number}>/{title}").unwrap();
        let (path, missing) = t.render_partial(&v, "flac");
        assert_eq!(
            (path, missing),
            (PathBuf::from("B/_/T.flac"), vec!["genre"])
        );
        // A folder of text alone, or an optional part that's there but
        // makes no name, is `_` as before.
        let v = vals(&[("album", "B"), ("genre", "..."), ("title", "T")]);
        for structure in ["{album}/.../{title}", "{album}/<{genre}>/{title}"] {
            let t = Template::parse(structure).unwrap();
            assert_eq!(
                t.render(&v, "flac").unwrap(),
                PathBuf::from("B/_/T.flac"),
                "{structure}"
            );
        }
        // Structures without < > render as they did.
        let t = Template::parse("{album}/{disc_number}-{track_number} - {title}").unwrap();
        let v = vals(&[
            ("album", "B"),
            ("disc_number", "1/1"),
            ("track_number", "1/1"),
            ("title", "T"),
        ]);
        assert_eq!(
            t.render(&v, "flac").unwrap(),
            PathBuf::from("B/1-01 - T.flac")
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
    fn names_windows_keeps_for_itself_get_a_mark() {
        let t = Template::parse("{album}/{title}").unwrap();
        for (album, title, path) in [
            ("CON", "nul", "_CON/_nul.flac"),
            ("Aux. Live", "LPT¹", "_Aux. Live/_LPT¹.flac"),
            ("COM10", "Console", "COM10/Console.flac"),
        ] {
            let v = vals(&[("album", album), ("title", title)]);
            assert_eq!(t.render(&v, "flac").unwrap(), PathBuf::from(path));
        }
        // Also when the length cut leaves one behind.
        let padded = format!("CON{}X", " ".repeat(178));
        let v = vals(&[("album", padded.as_str()), ("title", "T")]);
        assert_eq!(t.render(&v, "flac").unwrap(), PathBuf::from("_CON/T.flac"));
    }

    #[test]
    fn cuts_keep_characters_whole() {
        // Never inside a character, between a letter and its accent, or
        // inside an emoji.
        assert_eq!(cut(&"曲".repeat(100), MAX_NAME), "曲".repeat(60));
        let accent = format!("{}e\u{301}x", "a".repeat(179));
        assert_eq!(cut(&accent, MAX_NAME), "a".repeat(179));
        let family = format!("{}👨\u{200d}👩", "b".repeat(170));
        assert_eq!(cut(&family, MAX_NAME), "b".repeat(170));
    }

    #[test]
    fn reports_missing_variables() {
        let t = Template::parse("{album_artist}/{album}/{track_number} - {title}").unwrap();
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
            "{album_artist}: {album}/{title}",
            "{album}/{track_number} | {title}",
            "{album}/a\\b {title}",
            "<{album}/{title}",
            "{album}>/{title}",
            "{album}/<{track_number} - {title}",
            "{album}/<<{disc_number}>>{title}",
            "{album}/<{disc_number}<{track_number}>>{title}",
            "{album}/<Disc >{title}",
            "{album}/<>{title}",
            "{album}/<{track_number} - ><{title}>",
            "{album}/<{disc_number}-{track_number> {title}",
        ] {
            assert!(Template::parse(bad).is_err(), "{bad}");
        }
        for good in SUGGESTIONS {
            assert!(Template::parse(good).is_ok(), "{good}");
        }
    }
}
