//! Splitting tags that name people into one name each.
//!
//! A tag can join several names with a phrase, "Ado feat. Eve" or
//! "Hoshizora Rin (CV. Iida Riho)". Library setup lists such phrases; the
//! import and MusicBrainz's suggestions put the separator in their place, and
//! filters offer each name on its own.

use std::collections::HashMap;
use std::ops::Range;

use crate::{Error, Result};

/// Tag kinds that name people, the only ones split at phrases. Sort names
/// are left alone: they sort the credit as a whole.
pub const PEOPLE: &[&str] = &[
    "artist",
    "album_artist",
    "artists",
    "album_artists",
    "composer",
    "lyricist",
    "writer",
    "arranger",
    "conductor",
    "performer",
    "producer",
    "engineer",
    "mix_engineer",
    "mix_dj",
    "remixer",
    "director",
    "original_artist",
    "original_lyricist",
];

/// Phrases Library setup starts with.
pub const DEFAULT_PHRASES: &[&str] = &["feat.", "ft.", "featuring", "(CV"];

/// At most this many phrases, of at most `PHRASE_CHARS` characters each.
const MAX_PHRASES: usize = 32;
const PHRASE_CHARS: usize = 32;

/// Opening brackets, as folded, and the brackets that close them.
const BRACKETS: &[(char, char)] = &[('(', ')'), ('[', ']'), ('{', '}'), ('【', '】')];

/// Lists of names are punctuated with these; a name never starts or ends
/// with one, e.g. after "Rin (CV. Iida Riho), Hanayo (CV. Kubo Yurika)".
const LIST_PUNCTUATION: &[char] = &[',', '、', ';', '&', '/', '×'];

/// For matching: full-width forms as ASCII ("（ＣＶ：" as "(CV:"), and
/// ASCII letters in lower case.
fn fold(c: char) -> char {
    let c = match c {
        '\u{FF01}'..='\u{FF5E}' => char::from_u32(c as u32 - 0xFEE0).unwrap_or(c),
        '\u{3000}' => ' ',
        _ => c,
    };
    c.to_ascii_lowercase()
}

fn closing(open: char) -> Option<char> {
    BRACKETS.iter().find(|(o, _)| *o == open).map(|(_, c)| *c)
}

/// Text a name can be made of: anything but spaces, list punctuation and brackets.
fn names(text: &str) -> bool {
    text.chars().map(fold).any(|c| {
        !c.is_whitespace()
            && !LIST_PUNCTUATION.contains(&c)
            && !BRACKETS.iter().any(|(o, cl)| c == *o || c == *cl)
    })
}

fn trim_name(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || LIST_PUNCTUATION.contains(&fold(c)))
}

/// Library setup's phrases as saved: trimmed, without blanks or repeats
/// (as matched), and within limits.
pub fn normalize_phrases(phrases: Vec<String>) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for p in phrases {
        let p = p.trim();
        if p.is_empty() {
            continue;
        }
        if p.chars().count() > PHRASE_CHARS || p.chars().any(char::is_control) {
            return Err(Error::Invalid(format!(
                "A phrase to split names at can have at most {PHRASE_CHARS} characters, and no control characters."
            )));
        }
        let folded: String = p.chars().map(fold).collect();
        if !out
            .iter()
            .any(|o| o.chars().map(fold).collect::<String>() == folded)
        {
            out.push(p.to_string());
        }
    }
    if out.len() > MAX_PHRASES {
        return Err(Error::Invalid(format!(
            "Names can be split at up to {MAX_PHRASES} phrases."
        )));
    }
    Ok(out)
}

#[derive(Debug, Clone)]
struct Phrase {
    /// Folded.
    chars: Vec<char>,
    /// Starts with an opening bracket, like "(CV".
    opens: bool,
    /// Starts or ends inside a word, so it must not run on into one:
    /// "feat" isn't in "feature".
    word_start: bool,
    word_end: bool,
}

/// How Library setup splits tags that hold several values: at the
/// separator, and in tags that name people, at its phrases too. The
/// default splits nothing.
#[derive(Debug, Clone, Default)]
pub struct Splitter {
    separator: String,
    phrases: Vec<Phrase>,
}

impl Splitter {
    pub fn new(separator: &str, phrases: &[String]) -> Self {
        let mut phrases: Vec<Phrase> = phrases
            .iter()
            .map(|p| p.trim().chars().map(fold).collect::<Vec<char>>())
            .filter(|chars| !chars.is_empty())
            .map(|chars| Phrase {
                opens: closing(chars[0]).is_some(),
                word_start: chars[0].is_ascii_alphanumeric(),
                word_end: chars[chars.len() - 1].is_ascii_alphanumeric(),
                chars,
            })
            .collect();
        // The longest first, where one starts another.
        phrases.sort_by_key(|p| std::cmp::Reverse(p.chars.len()));
        Self {
            separator: separator.to_string(),
            phrases,
        }
    }

    pub fn separator(&self) -> &str {
        &self.separator
    }

    /// The names in `value` of `kind`: split at the separator, and at the
    /// phrases if `kind` names people. Trimmed, without blanks or repeats.
    pub fn parts<'a>(&self, kind: &str, value: &'a str) -> Vec<String> {
        let cuts = if PEOPLE.contains(&kind) {
            self.cuts(value)
        } else {
            Vec::new()
        };
        let mut out: Vec<String> = Vec::new();
        let mut from = 0;
        let ends = cuts.iter().map(|c| (c.start, c.end));
        for (start, end) in ends.chain(std::iter::once((value.len(), value.len()))) {
            let segment = &value[from..start];
            from = end;
            let pieces: Vec<&str> = if self.separator.trim().is_empty() {
                vec![segment]
            } else {
                segment.split(self.separator.as_str()).collect()
            };
            // List punctuation goes only where a phrase split the names.
            let trim = |p: &'a str| {
                if cuts.is_empty() {
                    p.trim()
                } else {
                    trim_name(p)
                }
            };
            for piece in pieces.into_iter().map(trim).filter(|p| !p.is_empty()) {
                if !out.iter().any(|o| o == piece) {
                    out.push(piece.to_string());
                }
            }
        }
        out
    }

    /// `value` of `kind` with the separator for each phrase that joins two
    /// names, or None when it has none (or `kind` doesn't name people).
    pub fn rewrite(&self, kind: &str, value: &str) -> Option<String> {
        if !PEOPLE.contains(&kind) || self.separator.trim().is_empty() {
            return None;
        }
        if self.cuts(value).is_empty() {
            return None;
        }
        let joined = self.parts(kind, value).join(&self.separator);
        (joined != value && !joined.is_empty()).then_some(joined)
    }

    /// Rewrite every value in `values` that a phrase splits; returns the
    /// kinds rewritten.
    pub fn rewrite_all(&self, values: &mut HashMap<&'static str, String>) -> Vec<&'static str> {
        let mut changed = Vec::new();
        for (kind, value) in values.iter_mut() {
            if let Some(v) = self.rewrite(kind, value) {
                *value = v;
                changed.push(*kind);
            }
        }
        changed
    }

    /// Byte ranges of `value` to cut out: each phrase joining two names
    /// (with a `:` or `.` right after it, as in "(CV:" and "(CV."), and,
    /// where the phrase opens a bracket or follows one, that bracket and
    /// the one closing it.
    fn cuts(&self, value: &str) -> Vec<Range<usize>> {
        if self.phrases.is_empty() {
            return Vec::new();
        }
        let at: Vec<usize> = value
            .char_indices()
            .map(|(i, _)| i)
            .chain(std::iter::once(value.len()))
            .collect();
        let folded: Vec<char> = value.chars().map(fold).collect();
        let n = folded.len();
        let mut cuts: Vec<Range<usize>> = Vec::new();
        let mut i = 0;
        while i < n {
            let Some((start, end, close)) = self
                .phrases
                .iter()
                .find_map(|p| self.match_at(p, &folded, i))
            else {
                i += 1;
                continue;
            };
            // Only between two names: "X JAPAN" isn't split at a phrase "x".
            let after = &value[at[end]..close.map_or(value.len(), |c| at[c])];
            let rest = close.map_or("", |c| &value[at[c + 1]..]);
            if names(&value[..at[start]]) && (names(after) || names(rest)) {
                cuts.push(at[start]..at[end]);
                if let Some(c) = close {
                    cuts.push(at[c]..at[c + 1]);
                }
            }
            i = end.max(i + 1);
        }
        cuts.sort_by_key(|c| c.start);
        // A bracket closed inside a later phrase: keep the cuts apart.
        let mut out: Vec<Range<usize>> = Vec::new();
        for c in cuts {
            match out.last_mut() {
                Some(last) if c.start < last.end => last.end = last.end.max(c.end),
                _ => out.push(c),
            }
        }
        out
    }

    /// Phrase `p` at char `i` of `folded`: the chars to cut (from an
    /// opening bracket before it, if any, to past a `:` or `.` after it),
    /// and the bracket closing the one it opens.
    fn match_at(
        &self,
        p: &Phrase,
        folded: &[char],
        i: usize,
    ) -> Option<(usize, usize, Option<usize>)> {
        let n = folded.len();
        let mut end = i + p.chars.len();
        if end > n || folded[i..end] != p.chars[..] {
            return None;
        }
        if p.word_start && i > 0 && folded[i - 1].is_ascii_alphanumeric() {
            return None;
        }
        if p.word_end && end < n && folded[end].is_ascii_alphanumeric() {
            return None;
        }
        if p.word_end && end < n && matches!(folded[end], ':' | '.') {
            end += 1;
        }
        // The bracket the phrase opens, or one just before it: "(feat. Eve)".
        let (start, open) = if p.opens {
            (i, Some(folded[i]))
        } else {
            let before = folded[..i].iter().rposition(|c| !c.is_whitespace());
            match before {
                Some(b) if closing(folded[b]).is_some() => (b, Some(folded[b])),
                _ => (i, None),
            }
        };
        let close = open.and_then(|o| {
            let shut = closing(o)?;
            let mut depth = 0;
            for (k, c) in folded.iter().enumerate().skip(end) {
                if *c == o {
                    depth += 1;
                } else if *c == shut {
                    if depth == 0 {
                        return Some(k);
                    }
                    depth -= 1;
                }
            }
            None
        });
        Some((start, end, close))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split() -> Splitter {
        let phrases: Vec<String> = DEFAULT_PHRASES.iter().map(|p| p.to_string()).collect();
        Splitter::new(";", &phrases)
    }

    fn rw(s: &Splitter, value: &str) -> Option<String> {
        s.rewrite("artist", value)
    }

    #[test]
    fn people_kinds_are_in_the_catalog() {
        for k in PEOPLE {
            assert!(crate::rules::kind(k).is_some(), "{k}");
        }
    }

    #[test]
    fn featured_artists_and_voice_actors_become_names_of_their_own() {
        let s = split();
        assert_eq!(
            rw(&s, "かめりあ feat. ななひら").as_deref(),
            Some("かめりあ;ななひら")
        );
        assert_eq!(
            rw(&s, "Hoshizora Rin(CV. Iida Riho)").as_deref(),
            Some("Hoshizora Rin;Iida Riho")
        );
        assert_eq!(
            rw(&s, "茅野ふたば (CV: 堀越せな)").as_deref(),
            Some("茅野ふたば;堀越せな")
        );
        // Full-width brackets and colon, and a phrase in another case.
        assert_eq!(
            rw(&s, "凛（ＣＶ：飯田里穂）").as_deref(),
            Some("凛;飯田里穂")
        );
        assert_eq!(rw(&s, "Ado FEAT. Eve").as_deref(), Some("Ado;Eve"));
        assert_eq!(rw(&s, "Ado featuring Eve").as_deref(), Some("Ado;Eve"));
        // A phrase in brackets of its own takes them along.
        assert_eq!(rw(&s, "Ado (feat. Eve)").as_deref(), Some("Ado;Eve"));
        assert_eq!(rw(&s, "Ado [ft. Eve]").as_deref(), Some("Ado;Eve"));
        // Brackets that aren't a phrase's stay.
        assert_eq!(
            rw(&s, "Hiroshi Okubo (nanosounds.jp) feat. SAK.").as_deref(),
            Some("Hiroshi Okubo (nanosounds.jp);SAK.")
        );
        // Several characters in a row, with list punctuation between them.
        assert_eq!(
            rw(
                &s,
                "鳳凰火凛 (CV: 健屋花那)、瀬戸海月 (CV: シスター・クレア)"
            )
            .as_deref(),
            Some("鳳凰火凛;健屋花那;瀬戸海月;シスター・クレア")
        );
        assert_eq!(
            rw(
                &s,
                "Minami Kotori(CV. Uchida Aya), Koizumi Hanayo(CV. Kubo Yurika)"
            )
            .as_deref(),
            Some("Minami Kotori;Uchida Aya;Koizumi Hanayo;Kubo Yurika")
        );
        assert_eq!(
            rw(&s, "アーリャ(CV.上坂すみれ)×花譜").as_deref(),
            Some("アーリャ;上坂すみれ;花譜")
        );
        // Unclosed, the bracket just goes.
        assert_eq!(rw(&s, "Rin (CV: Riho").as_deref(), Some("Rin;Riho"));
    }

    #[test]
    fn only_phrases_between_two_names_split() {
        let s = split();
        // Inside words.
        assert_eq!(rw(&s, "The Featherz"), None);
        assert_eq!(rw(&s, "Daft Punk"), None);
        assert_eq!(rw(&s, "defeat. Eve"), None);
        // Nothing before it, or nothing after it.
        assert_eq!(rw(&s, "feat. Eve"), None);
        assert_eq!(rw(&s, "Ado feat."), None);
        assert_eq!(rw(&s, "(CV: Riho)"), None);
        let x = Splitter::new(";", &["x".to_string(), "×".to_string()]);
        assert_eq!(rw(&x, "X JAPAN"), None);
        assert_eq!(rw(&x, "Max x Min").as_deref(), Some("Max;Min"));
        assert_eq!(
            rw(&x, "DAOKO × 中田ヤスタカ").as_deref(),
            Some("DAOKO;中田ヤスタカ")
        );
        // No phrase: as it was, separators and all.
        assert_eq!(rw(&s, "NayutalieN; Hatsune Miku"), None);
        assert_eq!(rw(&s, "Ookubo Rumi, Tsuda Minami & Taneda Risa"), None);
    }

    #[test]
    fn rewritten_values_join_every_name_once_with_the_separator() {
        let s = Splitter::new(" / ", &["feat.".to_string()]);
        assert_eq!(rw(&s, "A / B feat. C").as_deref(), Some("A / B / C"));
        assert_eq!(rw(&s, "A feat. A").as_deref(), Some("A"));
        // Tags that don't name people, and sort names, stay whole.
        assert_eq!(s.rewrite("title", "Song feat. C"), None);
        assert_eq!(s.rewrite("artist_sort", "A feat. C"), None);
        // No separator, or no phrases: nothing to split with.
        assert_eq!(
            Splitter::new("", &["feat.".to_string()]).rewrite("artist", "A feat. C"),
            None
        );
        assert_eq!(Splitter::new(";", &[]).rewrite("artist", "A feat. C"), None);
        let mut values: HashMap<&'static str, String> = HashMap::from([
            ("artist", "A feat. C".to_string()),
            ("title", "Song feat. C".to_string()),
            ("album_artist", "A".to_string()),
        ]);
        assert_eq!(s.rewrite_all(&mut values), vec!["artist"]);
        assert_eq!(values["artist"], "A / C");
        assert_eq!(values["title"], "Song feat. C");
    }

    #[test]
    fn parts_split_at_the_separator_and_in_names_at_phrases() {
        let s = split();
        assert_eq!(s.parts("artist", "A feat. B;C"), ["A", "B", "C"]);
        assert_eq!(
            s.parts("genre", "Rock;Pop feat. Jazz"),
            ["Rock", "Pop feat. Jazz"]
        );
        assert_eq!(s.parts("artist", " A ; ; A "), ["A"]);
        assert_eq!(
            Splitter::default().parts("artist", "A feat. B;C"),
            ["A feat. B;C"]
        );
    }

    #[test]
    fn phrases_are_saved_trimmed_once_each_and_within_limits() {
        let typed = vec![
            " feat. ".to_string(),
            "FEAT.".into(),
            "".into(),
            "(CV".into(),
        ];
        assert_eq!(normalize_phrases(typed).unwrap(), ["feat.", "(CV"]);
        assert!(normalize_phrases(vec!["a\u{7}".into()]).is_err());
        assert!(normalize_phrases(vec!["x".repeat(33)]).is_err());
        assert!(normalize_phrases((0..33).map(|i| format!("p{i}")).collect()).is_err());
    }
}
