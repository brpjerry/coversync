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

/// Lists of names are punctuated with these; where a phrase split two
/// names, neither starts or ends with one, e.g. after "Rin (CV. Iida Riho),
/// Hanayo (CV. Kubo Yurika)". An `&` goes only standing alone: "&TEAM"
/// keeps its own.
const LIST_PUNCTUATION: &[char] = &[',', '、', ';', '/', '×'];

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
            && c != '&'
            && !BRACKETS.iter().any(|(o, cl)| c == *o || c == *cl)
    })
}

/// Part of a word in a script written with spaces between words, so a
/// phrase can't start or end inside it: "x" isn't in "vōx". Chinese,
/// Japanese and Korean don't count, so "かめりあfeat.ななひら" splits.
fn word(c: char) -> bool {
    let u = c as u32;
    (c.is_alphanumeric() && u < 0x2E80)
        // Combining marks ("vo\u{304}x") and Latin ligatures.
        || matches!(
            u,
            0x0300..=0x036F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF
                | 0xFE20..=0xFE2F | 0xFB00..=0xFB06
        )
}

/// `text` without list punctuation (and spaces) at its start.
fn trim_list_start(mut text: &str) -> &str {
    loop {
        text = text.trim_start();
        let mut chars = text.chars();
        let Some(c) = chars.next().map(fold) else {
            return text;
        };
        let alone = chars.next().is_none_or(char::is_whitespace);
        if LIST_PUNCTUATION.contains(&c) || (c == '&' && alone) {
            text = &text[text.chars().next().map_or(0, char::len_utf8)..];
        } else {
            return text;
        }
    }
}

/// `text` without list punctuation (and spaces) at its end.
fn trim_list_end(mut text: &str) -> &str {
    loop {
        text = text.trim_end();
        let mut chars = text.chars().rev();
        let Some(c) = chars.next().map(fold) else {
            return text;
        };
        let alone = chars.next().is_none_or(char::is_whitespace);
        if LIST_PUNCTUATION.contains(&c) || (c == '&' && alone) {
            text = &text[..text.len() - text.chars().next_back().map_or(0, char::len_utf8)];
        } else {
            return text;
        }
    }
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
                word_start: word(chars[0]),
                word_end: word(chars[chars.len() - 1]),
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
    /// phrases if `kind` names people (and there is a separator to put in
    /// their place). Trimmed, without blanks or repeats.
    pub fn parts(&self, kind: &str, value: &str) -> Vec<String> {
        let cuts = if PEOPLE.contains(&kind) {
            self.cuts(value)
        } else {
            Vec::new()
        };
        let mut out: Vec<String> = Vec::new();
        let mut from = 0;
        let ends = cuts.iter().map(|c| (c.start, c.end));
        let segments = ends.chain(std::iter::once((value.len(), value.len())));
        for (i, (start, end)) in segments.enumerate() {
            let segment = &value[from..start];
            from = end;
            let pieces: Vec<&str> = if self.separator.trim().is_empty() {
                vec![segment]
            } else {
                segment.split(self.separator.as_str()).collect()
            };
            let last = pieces.len() - 1;
            for (j, raw) in pieces.into_iter().enumerate() {
                // List punctuation goes only where a phrase split the names.
                let mut piece = raw.trim();
                if i > 0 && j == 0 {
                    let cut = &value[cuts[i - 1].clone()];
                    let after_bracket = cut
                        .chars()
                        .map(fold)
                        .any(|c| BRACKETS.iter().any(|(_, close)| c == *close));
                    // Right against a phrase's closing bracket, an & joins
                    // the next credit: "Rin (CV:Riho)&Hanayo".
                    if after_bracket && raw.chars().next().map(fold) == Some('&') {
                        piece = &raw[raw.chars().next().map_or(0, char::len_utf8)..];
                    }
                    piece = trim_list_start(piece);
                }
                if i < cuts.len() && j == last {
                    piece = trim_list_end(piece);
                }
                if !piece.is_empty() && !out.iter().any(|o| o == piece) {
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
        if self.phrases.is_empty() || self.separator.trim().is_empty() {
            return Vec::new();
        }
        let separator = self.separator.as_str();
        let at: Vec<usize> = value
            .char_indices()
            .map(|(i, _)| i)
            .chain(std::iter::once(value.len()))
            .collect();
        let folded: Vec<char> = value.chars().map(fold).collect();
        let n = folded.len();
        let mut cuts: Vec<Range<usize>> = Vec::new();
        // Where the name before the next phrase can start: after the last
        // phrase that split (its closing bracket doesn't count).
        let mut names_from = 0;
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
            // Only between two names of the same value: "X JAPAN" isn't
            // split at a phrase "x", nor is "Little Feat.;Eve" at "feat.".
            let before = &value[names_from.min(at[start])..at[start]];
            let before = before.rsplit(separator).next().unwrap_or(before);
            let entry_end = value[at[end]..]
                .find(separator)
                .map_or(value.len(), |e| at[end] + e);
            let after = &value[at[end]..close.map_or(entry_end, |c| at[c].min(entry_end))];
            let rest = close
                .filter(|c| at[*c] < entry_end)
                .map_or("", |c| &value[at[c + 1]..entry_end]);
            if names(before) && (names(after) || names(rest)) {
                names_from = at[end];
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
        if p.word_start && i > 0 && word(folded[i - 1]) {
            return None;
        }
        if p.word_end && end < n && word(folded[end]) {
            return None;
        }
        // "(CV:", "(CV.", "(CV :", "feat.:"; but not the separator, nor,
        // across spaces, punctuation of the next name's own (".que").
        let separator: Vec<char> = self.separator.trim().chars().map(fold).collect();
        let mut past = end;
        while past < n && folded[past].is_whitespace() {
            past += 1;
        }
        let alone = past == end || folded.get(past + 1).is_none_or(|c| c.is_whitespace());
        if past < n
            && matches!(folded[past], ':' | '.')
            && alone
            && !folded[past..].starts_with(&separator)
        {
            end = past + 1;
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
        // A phrase right after a closed bracket still has its name before.
        assert_eq!(
            rw(&s, "Rin (CV. Riho) feat. Eve").as_deref(),
            Some("Rin;Riho;Eve")
        );
        // Without spaces in Japanese, and with a : or . after any phrase.
        assert_eq!(
            rw(&s, "かめりあfeat.ななひら").as_deref(),
            Some("かめりあ;ななひら")
        );
        assert_eq!(rw(&s, "Ado feat.: Eve").as_deref(), Some("Ado;Eve"));
        // Across a space, a name's own punctuation stays.
        assert_eq!(rw(&s, "Ado feat. .que").as_deref(), Some("Ado;.que"));
        assert_eq!(
            rw(&s, "Ado feat. :wumpscut:").as_deref(),
            Some("Ado;:wumpscut:")
        );
        assert_eq!(
            rw(&s, "Natsuhi (CV : Yuri)").as_deref(),
            Some("Natsuhi;Yuri")
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
        // The names have to be in the same value, after any phrase before.
        assert_eq!(rw(&s, "Little Feat.;Eve"), None);
        assert_eq!(rw(&s, "Eve;Little Feat."), None);
        let both = Splitter::new(";", &["feat.".into(), "x".into()]);
        assert_eq!(
            rw(&both, "Ado feat. X JAPAN").as_deref(),
            Some("Ado;X JAPAN")
        );
        assert_eq!(rw(&both, "Ado;X JAPAN"), None);
        // Accented letters are part of words; "x" isn't in "vōx".
        assert_eq!(rw(&both, "vōx feat. Eve").as_deref(), Some("vōx;Eve"));
        assert_eq!(
            rw(&both, "vo\u{304}x feat. Eve").as_deref(),
            Some("vo\u{304}x;Eve")
        );
        // A : separator isn't taken along as a phrase's own.
        let colon = Splitter::new(":", &["feat.".into()]);
        assert_eq!(rw(&colon, "Little Feat.:Eve"), None);
        assert_eq!(rw(&colon, "Ado feat. Eve").as_deref(), Some("Ado:Eve"));
        // No phrase: as it was, separators and all.
        assert_eq!(rw(&s, "NayutalieN; Hatsune Miku"), None);
        assert_eq!(rw(&s, "Ookubo Rumi, Tsuda Minami & Taneda Risa"), None);
    }

    #[test]
    fn list_punctuation_goes_only_where_a_phrase_split_names() {
        let s = split();
        // An & of a name's own stays, at either end of the value.
        assert_eq!(rw(&s, "Ado feat. &TEAM").as_deref(), Some("Ado;&TEAM"));
        assert_eq!(
            rw(&s, "&ME feat. Keinemusik").as_deref(),
            Some("&ME;Keinemusik")
        );
        // After a bracket, but not against it: a name's own.
        assert_eq!(
            rw(&s, "Rin (CV. Riho), &TEAM").as_deref(),
            Some("Rin;Riho;&TEAM")
        );
        assert_eq!(
            rw(&s, "Ado (feat. Eve) &ME").as_deref(),
            Some("Ado;Eve;&ME")
        );
        // One standing alone between two credits goes, as does one right
        // against a phrase's closing bracket.
        assert_eq!(
            rw(&s, "Rin (CV:Riho)&Hanayo (CV:Yurika)").as_deref(),
            Some("Rin;Riho;Hanayo;Yurika")
        );
        assert_eq!(
            rw(&s, "Rin (CV: Riho) & Hanayo (CV: Yurika)").as_deref(),
            Some("Rin;Riho;Hanayo;Yurika")
        );
        // At the edges of the value, and at the separator, nothing is trimmed.
        assert_eq!(rw(&s, "Ado feat. Eve, ").as_deref(), Some("Ado;Eve,"));
        assert_eq!(s.parts("artist", ", A feat. B;/C"), [", A", "B", "/C"]);
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
        // No separator to put in their place: phrases don't split, as on import.
        let blank = Splitter::new(" ", &["feat.".to_string()]);
        assert_eq!(blank.parts("artist", "A feat. B"), ["A feat. B"]);
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
