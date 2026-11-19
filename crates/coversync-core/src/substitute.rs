//! Library setup's substitutions: vim's `s/pattern/replacement/flags` on
//! the tags each names, on import and in MusicBrainz's suggestions, after
//! names are split at phrases (see [`crate::split::Splitter`]).
//!
//! Patterns are the `regex` crate's (Perl-style, as vim's `\v`). Any ASCII
//! punctuation but `\` can stand for `/`. Flags: `g`, every match rather
//! than the first; `i`, ignoring case. In the replacement, `&` and `\0`
//! are the match, `\1`–`\9` its groups, `\u` and `\l` change the case of
//! the next character, `\U` and `\L` of what follows up to `\E` (or `\e`).

use std::borrow::Cow;

use regex::{Captures, Regex, RegexBuilder};
use serde::{Deserialize, Serialize};

use crate::{Error, Result, rules};

/// At most this many substitutions, of at most `EXPRESSION_CHARS` characters each.
const MAX_SUBSTITUTIONS: usize = 32;
const EXPRESSION_CHARS: usize = 500;
/// Bytes a compiled pattern may take.
const SIZE_LIMIT: usize = 1 << 20;

/// One of Library setup's substitutions, as saved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Substitution {
    /// `s/pattern/replacement/flags`, as typed.
    pub expression: String,
    /// Catalog ids of the tags it changes.
    pub tags: Vec<String>,
}

/// A substitution ready to run.
#[derive(Debug, Clone)]
pub struct Substitute {
    regex: Regex,
    replacement: Vec<Piece>,
    /// Every match, not only the first.
    global: bool,
    tags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
enum Piece {
    Text(String),
    /// The match (0) or one of its groups.
    Group(usize),
    /// `\u`, `\l`: the next character.
    Next(Case),
    /// `\U`, `\L`, and `\E` (None) that ends them.
    Span(Option<Case>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Case {
    Upper,
    Lower,
}

impl Substitute {
    pub fn new(s: &Substitution) -> Result<Self> {
        let bad = |why: String| Error::Invalid(format!("Substitution {}: {why}", s.expression));
        let (pattern, replacement, flags) = parse(&s.expression).map_err(bad)?;
        let mut global = false;
        let mut builder = RegexBuilder::new(&pattern);
        builder.size_limit(SIZE_LIMIT);
        for flag in flags.chars() {
            match flag {
                'g' => global = true,
                'i' => _ = builder.case_insensitive(true),
                _ => return Err(bad(format!("{flag} isn't a flag; g and i are."))),
            }
        }
        // The last line of a syntax error says what's wrong; the lines
        // above it point at where.
        let regex = builder.build().map_err(|e| {
            let e = e.to_string();
            bad(e
                .lines()
                .last()
                .unwrap_or(&e)
                .trim_start_matches("error: ")
                .to_string())
        })?;
        if let Some(n) = replacement.iter().find_map(|p| match p {
            Piece::Group(n) if *n >= regex.captures_len() => Some(n),
            _ => None,
        }) {
            return Err(bad(format!("the pattern has no group {n}.")));
        }
        Ok(Self {
            regex,
            replacement,
            global,
            tags: s.tags.clone(),
        })
    }

    pub fn changes(&self, kind: &str) -> bool {
        self.tags.iter().any(|t| t == kind)
    }

    /// `text` with the first match replaced, or with `g` every one.
    pub fn apply<'t>(&self, text: &'t str) -> Cow<'t, str> {
        let limit = if self.global { 0 } else { 1 };
        self.regex
            .replacen(text, limit, |caps: &Captures| self.expand(caps))
    }

    fn expand(&self, caps: &Captures) -> String {
        let mut out = String::new();
        let mut next = None;
        let mut span = None;
        for piece in &self.replacement {
            let text = match piece {
                Piece::Text(t) => t.as_str(),
                Piece::Group(n) => caps.get(*n).map_or("", |m| m.as_str()),
                Piece::Next(case) => {
                    next = Some(*case);
                    continue;
                }
                Piece::Span(case) => {
                    span = *case;
                    // \E ends a \u still waiting for its character too.
                    if case.is_none() {
                        next = None;
                    }
                    continue;
                }
            };
            for c in text.chars() {
                match next.take().or(span) {
                    Some(Case::Upper) => out.extend(c.to_uppercase()),
                    Some(Case::Lower) => out.extend(c.to_lowercase()),
                    None => out.push(c),
                }
            }
        }
        out
    }
}

/// The pattern, replacement and flags of `expression`.
fn parse(expression: &str) -> std::result::Result<(String, Vec<Piece>, String), String> {
    let shape = "write it as s/pattern/replacement/flags.";
    let mut chars = expression
        .strip_prefix('s')
        .ok_or_else(|| shape.to_string())?
        .chars();
    let delimiter = chars
        .next()
        .filter(|c| c.is_ascii_punctuation() && *c != '\\')
        .ok_or_else(|| shape.to_string())?;
    let mut pattern = String::new();
    let mut replacement = Vec::new();
    let mut flags = String::new();
    // 0: the pattern, 1: the replacement, 2: the flags.
    let mut field = 0;
    while let Some(c) = chars.next() {
        if field == 2 {
            flags.push(c);
        } else if c == delimiter {
            field += 1;
        } else if c != '\\' {
            if field == 0 {
                pattern.push(c);
            } else if c == '&' {
                replacement.push(Piece::Group(0));
            } else {
                push_text(&mut replacement, c);
            }
        } else {
            let e = chars.next().ok_or("it ends in a lone \\.")?;
            if field == 0 {
                // The delimiter as itself; other escapes are the pattern's.
                if e == delimiter {
                    pattern.push_str(&regex::escape(&e.to_string()));
                } else {
                    pattern.push(c);
                    pattern.push(e);
                }
                continue;
            }
            replacement.push(match e {
                '0'..='9' => Piece::Group(e as usize - '0' as usize),
                'u' => Piece::Next(Case::Upper),
                'l' => Piece::Next(Case::Lower),
                'U' => Piece::Span(Some(Case::Upper)),
                'L' => Piece::Span(Some(Case::Lower)),
                'E' | 'e' => Piece::Span(None),
                '\\' | '&' => {
                    push_text(&mut replacement, e);
                    continue;
                }
                _ if e == delimiter => {
                    push_text(&mut replacement, e);
                    continue;
                }
                _ => return Err(format!("\\{e} means nothing in a replacement.")),
            });
        }
    }
    if field == 0 {
        return Err(shape.to_string());
    }
    if pattern.is_empty() {
        return Err("the pattern is empty.".to_string());
    }
    Ok((pattern, replacement, flags))
}

fn push_text(pieces: &mut Vec<Piece>, c: char) {
    if let Some(Piece::Text(t)) = pieces.last_mut() {
        t.push(c);
    } else {
        pieces.push(Piece::Text(c.to_string()));
    }
}

/// Library setup's substitutions as saved: trimmed, without blank ones,
/// each with the tags it changes in catalog order, and each one that runs.
pub fn normalize(substitutions: Vec<Substitution>) -> Result<Vec<Substitution>> {
    let typed = substitutions
        .iter()
        .filter(|s| !s.expression.trim().is_empty());
    if typed.count() > MAX_SUBSTITUTIONS {
        return Err(Error::Invalid(format!(
            "Library setup can have up to {MAX_SUBSTITUTIONS} substitutions."
        )));
    }
    let mut out = Vec::new();
    for s in substitutions {
        let expression = s.expression.trim().to_string();
        if expression.is_empty() {
            continue;
        }
        if expression.chars().count() > EXPRESSION_CHARS || expression.chars().any(char::is_control)
        {
            return Err(Error::Invalid(format!(
                "A substitution can have at most {EXPRESSION_CHARS} characters, and no control characters."
            )));
        }
        if let Some(bad) = s
            .tags
            .iter()
            .find(|t| rules::kind(t).is_none() || rules::NOT_EDITABLE.contains(&t.as_str()))
        {
            return Err(Error::Invalid(format!("unknown tag kind '{bad}'")));
        }
        let tags: Vec<String> = rules::CATALOG
            .iter()
            .filter(|k| s.tags.iter().any(|t| t == k.id))
            .map(|k| k.id.to_string())
            .collect();
        if tags.is_empty() {
            return Err(Error::Invalid(format!(
                "Substitution {expression}: choose the tags it changes."
            )));
        }
        let s = Substitution { expression, tags };
        Substitute::new(&s)?;
        out.push(s);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sub(expression: &str) -> Substitute {
        Substitute::new(&Substitution {
            expression: expression.into(),
            tags: vec!["title".into()],
        })
        .unwrap()
    }

    fn run(expression: &str, text: &str) -> String {
        sub(expression).apply(text).into_owned()
    }

    fn refused(expression: &str) -> String {
        Substitute::new(&Substitution {
            expression: expression.into(),
            tags: vec!["title".into()],
        })
        .unwrap_err()
        .to_string()
    }

    #[test]
    fn the_first_match_is_replaced_or_with_g_every_one() {
        assert_eq!(run("s/o/0/", "foo boo"), "f0o boo");
        assert_eq!(run("s/o/0/g", "foo boo"), "f00 b00");
        assert_eq!(run("s/O/0/gi", "fOo"), "f00");
        assert_eq!(run("s/x/y/", "foo"), "foo");
        // Without its last delimiter, as vim takes it; an empty replacement deletes.
        assert_eq!(run("s/o/0", "foo"), "f0o");
        assert_eq!(run(r"s/\s*\(Live\)$//", "Song (Live)"), "Song");
    }

    #[test]
    fn the_replacement_takes_vims_groups_and_case_changes() {
        assert_eq!(run("s/^(.+), The$/The \\1/", "Beatles, The"), "The Beatles");
        assert_eq!(run("s/b+/[&]/", "abbc"), "a[bb]c");
        assert_eq!(run(r"s/b+/[\0]/", "abbc"), "a[bb]c");
        assert_eq!(run(r"s/\w+/\u&/g", "the long way"), "The Long Way");
        assert_eq!(run(r"s/(\w+) (\w+)/\U\1\E \2/", "one two"), "ONE two");
        assert_eq!(run(r"s/.*/\L\u&/", "LOUD"), "Loud");
        assert_eq!(run(r"s/ß/\U&/", "ß"), "SS");
        // A group that took no part is empty; $ is only a dollar sign.
        assert_eq!(run(r"s/a(x)?/<\1$1>/", "a"), "<$1>");
        assert_eq!(run(r"s/a/\&\\/", "a"), r"&\");
        // \E ends a \u that had no character to change.
        assert_eq!(run(r"s/(a)?b/\u\1\Erest/", "b"), "rest");
    }

    #[test]
    fn any_punctuation_can_be_the_delimiter() {
        assert_eq!(run("s#a/b#c#", "a/b"), "c");
        assert_eq!(run(r"s/a\/b/c\/d/", "a/b"), "c/d");
        // The escaped delimiter is itself, even where the pattern gives it a meaning.
        assert_eq!(run(r"s|a\|b|c|", "a|b"), "c");
        assert_eq!(run(r"s.a\.b.x.", "a.b acb"), "x acb");
    }

    #[test]
    fn mistakes_are_refused_with_what_is_wrong() {
        assert!(refused("/a/b/").contains("s/pattern/replacement/flags"));
        assert!(refused("sab").contains("s/pattern/replacement/flags"));
        assert!(refused("s/a").contains("s/pattern/replacement/flags"));
        assert!(refused("s//b/").contains("the pattern is empty"));
        assert!(
            refused("s/(/b/").ends_with("unclosed group"),
            "{}",
            refused("s/(/b/")
        );
        assert!(refused("s/a/b/x").contains("x isn't a flag"));
        assert!(refused(r"s/a/\2/").contains("no group 2"));
        assert!(refused(r"s/a/\n/").contains(r"\n means nothing"));
        assert!(refused(r"s/a/b\").contains("lone"));
        assert!(refused(r"s/\w{1000}{1000}/x/").contains("size limit"));
        // Perl-style: lookaround and backreferences need backtracking.
        assert!(refused(r"s/a(?=b)/x/").contains("look-around"));
        assert!(refused(r"s/(a)\1/x/").contains("backreferences"));
    }

    #[test]
    fn saved_substitutions_are_trimmed_and_name_their_tags_in_catalog_order() {
        let saved = normalize(vec![
            Substitution {
                expression: "  s/a/b/ ".into(),
                tags: vec!["title".into(), "artist".into(), "title".into()],
            },
            Substitution {
                expression: " ".into(),
                tags: vec![],
            },
        ])
        .unwrap();
        assert_eq!(
            saved,
            vec![Substitution {
                expression: "s/a/b/".into(),
                tags: vec!["title".into(), "artist".into()],
            }]
        );
        let with = |expression: &str, tags: &[&str]| {
            normalize(vec![Substitution {
                expression: expression.into(),
                tags: tags.iter().map(|t| t.to_string()).collect(),
            }])
        };
        assert!(with("s/a/b/", &[]).is_err());
        assert!(with("s/a/b/", &["nope"]).is_err());
        assert!(with("s/a/b/", &["rating"]).is_err());
        assert!(with("s/(/b/", &["title"]).is_err());
        assert!(with(&format!("s/{}/b/", "a".repeat(500)), &["title"]).is_err());
        // Counted before any is compiled.
        let many = vec![
            Substitution {
                expression: "s/(/b/".into(),
                tags: vec!["title".into()],
            };
            33
        ];
        assert!(
            normalize(many)
                .unwrap_err()
                .to_string()
                .contains("up to 32")
        );
    }
}
