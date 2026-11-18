//! Keeping a Library album where the folder structure says: after its tags
//! are saved, each file goes to the path its tags now give, and the album's
//! other files (its cover file, say) go with it when it leaves a folder.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::template::Template;
use crate::{Error, Result, rules, tags};

/// A file to move, or moved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Move {
    pub from: PathBuf,
    pub to: PathBuf,
}

/// Tag values as a save would leave them: numbers as lofty writes them
/// back (`01` as `1`), so the structure renders them as it will.
pub fn as_written(values: &mut HashMap<&'static str, String>) {
    for kind in ["track_number", "track_total", "disc_number", "disc_total"] {
        if let Some(v) = values.get_mut(kind) {
            let n = v.split('/').next().unwrap_or(v).trim();
            if let Ok(n) = n.parse::<u32>() {
                *v = n.to_string();
            }
        }
    }
}

/// Where the folder structure puts each of `files` (an album's, under
/// `root`) with its tag values: the files that have to move. Refused, with
/// what's wrong, when the structure needs a value a file doesn't have, two
/// files would land on one path, or one would land on a file that stays.
pub fn plan(
    template: &Template,
    root: &Path,
    files: &[(PathBuf, HashMap<&'static str, String>)],
) -> Result<Vec<Move>> {
    let mut moves = Vec::new();
    let mut lacking: Vec<String> = Vec::new();
    for (path, values) in files {
        let extension = path
            .extension()
            .map(|e| e.to_string_lossy().into_owned())
            .unwrap_or_default();
        match template.render(values, &extension) {
            Ok(rel) => {
                let to = root.join(rel);
                if to != *path {
                    moves.push(Move {
                        from: path.clone(),
                        to,
                    });
                }
            }
            Err(missing) => {
                let names: Vec<&str> = missing
                    .iter()
                    .map(|v| rules::kind(v).map_or(*v, |k| k.label))
                    .collect();
                let file = path.file_name().unwrap_or_default().to_string_lossy();
                lacking.push(format!("{file} would have no {}", names.join(", ")));
            }
        }
    }
    if !lacking.is_empty() {
        return Err(Error::Invalid(format!(
            "The folder structure needs values these files wouldn't have, so nothing was saved: {}.",
            lacking.join("; ")
        )));
    }
    // Where every file ends up: none twice, and none on a file not moving away.
    let leaving: HashSet<&PathBuf> = moves.iter().map(|m| &m.from).collect();
    let mut taken: HashSet<PathBuf> = files
        .iter()
        .map(|(p, _)| p.clone())
        .filter(|p| !leaving.contains(p))
        .collect();
    for m in &moves {
        let name =
            m.to.strip_prefix(root)
                .unwrap_or(&m.to)
                .display()
                .to_string();
        if !taken.insert(m.to.clone()) {
            return Err(Error::Invalid(format!(
                "Two files would both be {name} under the folder structure, so nothing was saved."
            )));
        }
        if m.to.exists() && !leaving.contains(&m.to) {
            return Err(Error::Invalid(format!(
                "{name} already exists, so nothing was saved."
            )));
        }
    }
    Ok(moves)
}

/// The deepest folder under `root` holding every one of `paths`; None when
/// that's `root` itself.
fn common_dir(root: &Path, paths: impl Iterator<Item = PathBuf>) -> Option<PathBuf> {
    let mut common: Option<PathBuf> = None;
    for p in paths {
        let dir = p.parent()?.to_path_buf();
        common = Some(match common {
            None => dir,
            Some(c) => {
                let mut c = c;
                while !dir.starts_with(&c) {
                    c = c.parent()?.to_path_buf();
                }
                c
            }
        });
    }
    common.filter(|c| c != root && c.starts_with(root))
}

/// Whether there's an audio file under `dir`.
fn has_audio(dir: &Path) -> bool {
    walkdir(dir).iter().any(|p| tags::is_audio(p))
}

/// Every file under `dir`.
fn walkdir(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out
}

/// Remove `dir` and the folders above it, up to `root`, while they're empty.
fn prune(root: &Path, dir: &Path) {
    let mut d = dir.to_path_buf();
    while d != root && d.starts_with(root) && fs::remove_dir(&d).is_ok() {
        match d.parent() {
            Some(p) => d = p.to_path_buf(),
            None => break,
        }
    }
}

/// Make the `moves` [`plan`] gave, under `root`. Files trade places safely
/// (track 1 and 2 renumbered, say). When the album's files have all left the
/// folder that held them, its other files follow to the new one, where none
/// has the same name; folders left empty go. Gives every file moved, the
/// album's other files included.
pub fn apply(root: &Path, moves: &[Move]) -> Result<Vec<Move>> {
    if moves.is_empty() {
        return Ok(Vec::new());
    }
    // Aside first, under names no file has, so one can take another's place.
    let mut aside = Vec::new();
    for (i, m) in moves.iter().enumerate() {
        let name = m.from.file_name().unwrap_or_default().to_string_lossy();
        let tmp = m.from.with_file_name(format!(".coversync-move-{i}-{name}"));
        fs::rename(&m.from, &tmp)?;
        aside.push(tmp);
    }
    let mut done = Vec::new();
    for (m, tmp) in moves.iter().zip(&aside) {
        if let Some(dir) = m.to.parent() {
            fs::create_dir_all(dir)?;
        }
        if m.to.exists() {
            // Planned free; taken since. Put it back rather than overwrite.
            fs::rename(tmp, &m.from)?;
            return Err(Error::Invalid(format!(
                "{} appeared meanwhile, so {} stayed where it was.",
                m.to.display(),
                m.from.display()
            )));
        }
        fs::rename(tmp, &m.to)?;
        done.push(m.clone());
    }
    // The album's other files follow it out of its old folder.
    let old_dir = common_dir(root, moves.iter().map(|m| m.from.clone()));
    let new_dir = common_dir(root, moves.iter().map(|m| m.to.clone()));
    if let (Some(old_dir), Some(new_dir)) = (old_dir, new_dir)
        && old_dir != new_dir
        && !new_dir.starts_with(&old_dir)
        && old_dir.is_dir()
        && !has_audio(&old_dir)
    {
        for from in walkdir(&old_dir) {
            let Ok(rel) = from.strip_prefix(&old_dir) else {
                continue;
            };
            let to = new_dir.join(rel);
            if to.exists() {
                continue;
            }
            if let Some(dir) = to.parent() {
                fs::create_dir_all(dir)?;
            }
            fs::rename(&from, &to)?;
            done.push(Move { from, to });
        }
    }
    let mut dirs: Vec<&Path> = moves.iter().filter_map(|m| m.from.parent()).collect();
    dirs.extend(done.iter().filter_map(|m| m.from.parent()));
    dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
    dirs.dedup();
    for d in dirs {
        prune(root, d);
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(pairs: &[(&'static str, &str)]) -> HashMap<&'static str, String> {
        pairs.iter().map(|(k, v)| (*k, v.to_string())).collect()
    }

    struct Dir(PathBuf);
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn scratch(name: &str) -> Dir {
        let d =
            std::env::temp_dir().join(format!("coversync-relocate-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        Dir(d)
    }
    fn touch(p: &Path, text: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    fn structure() -> Template {
        Template::parse("{album_artist}/{album}/{track_number} - {title}").unwrap()
    }

    #[test]
    fn a_new_album_title_moves_the_album_and_its_other_files() {
        let d = scratch("album");
        let root = d.0.join("Lib");
        let old = |n: &str| root.join("Artist/Record").join(n);
        touch(&old("01 - One.flac"), "1");
        touch(&old("02 - Two.flac"), "2");
        touch(&old("cover.jpg"), "art");
        touch(&old("Scans/back.jpg"), "back");
        let files = vec![
            (
                old("01 - One.flac"),
                values(&[
                    ("album_artist", "Artist"),
                    ("album", "Record!"),
                    ("track_number", "1"),
                    ("title", "One"),
                ]),
            ),
            (
                old("02 - Two.flac"),
                values(&[
                    ("album_artist", "Artist"),
                    ("album", "Record!"),
                    ("track_number", "2"),
                    ("title", "Two"),
                ]),
            ),
        ];
        let moves = plan(&structure(), &root, &files).unwrap();
        assert_eq!(moves.len(), 2);
        let done = apply(&root, &moves).unwrap();
        let new = |n: &str| root.join("Artist/Record!").join(n);
        for n in [
            "01 - One.flac",
            "02 - Two.flac",
            "cover.jpg",
            "Scans/back.jpg",
        ] {
            assert!(new(n).is_file(), "{n}");
        }
        assert_eq!(done.len(), 4);
        // The old folder is gone; the artist's stays.
        assert!(!root.join("Artist/Record").exists());
        assert!(root.join("Artist").is_dir());
    }

    #[test]
    fn files_can_trade_places() {
        let d = scratch("swap");
        let root = d.0.join("Lib");
        let at = |n: &str| root.join("A/R").join(n);
        touch(&at("01 - X.flac"), "x");
        touch(&at("02 - X.flac"), "y");
        // Renumbered the other way round.
        let files = vec![
            (
                at("01 - X.flac"),
                values(&[
                    ("album_artist", "A"),
                    ("album", "R"),
                    ("track_number", "2"),
                    ("title", "X"),
                ]),
            ),
            (
                at("02 - X.flac"),
                values(&[
                    ("album_artist", "A"),
                    ("album", "R"),
                    ("track_number", "1"),
                    ("title", "X"),
                ]),
            ),
        ];
        let moves = plan(&structure(), &root, &files).unwrap();
        apply(&root, &moves).unwrap();
        assert_eq!(fs::read_to_string(at("02 - X.flac")).unwrap(), "x");
        assert_eq!(fs::read_to_string(at("01 - X.flac")).unwrap(), "y");
    }

    #[test]
    fn a_title_change_renames_one_file_and_leaves_the_rest() {
        let d = scratch("title");
        let root = d.0.join("Lib");
        let at = |n: &str| root.join("A/R").join(n);
        touch(&at("01 - One.flac"), "1");
        touch(&at("02 - Two.flac"), "2");
        touch(&at("cover.jpg"), "art");
        let files = vec![
            (
                at("01 - One.flac"),
                values(&[
                    ("album_artist", "A"),
                    ("album", "R"),
                    ("track_number", "1"),
                    ("title", "Uno"),
                ]),
            ),
            (
                at("02 - Two.flac"),
                values(&[
                    ("album_artist", "A"),
                    ("album", "R"),
                    ("track_number", "2"),
                    ("title", "Two"),
                ]),
            ),
        ];
        let moves = plan(&structure(), &root, &files).unwrap();
        assert_eq!(
            moves,
            [Move {
                from: at("01 - One.flac"),
                to: at("01 - Uno.flac")
            }]
        );
        apply(&root, &moves).unwrap();
        assert!(
            at("01 - Uno.flac").is_file()
                && at("02 - Two.flac").is_file()
                && at("cover.jpg").is_file()
        );
    }

    #[test]
    fn what_the_structure_cant_place_is_refused() {
        let d = scratch("refused");
        let root = d.0.join("Lib");
        let at = |n: &str| root.join("A/R").join(n);
        touch(&at("01 - One.flac"), "1");
        touch(&at("02 - Two.flac"), "2");
        touch(&root.join("A/Other/01 - One.flac"), "other");
        // A title cleared: no file name.
        let cleared = vec![(
            at("01 - One.flac"),
            values(&[("album_artist", "A"), ("album", "R"), ("track_number", "1")]),
        )];
        let e = plan(&structure(), &root, &cleared).unwrap_err().to_string();
        assert!(e.contains("Title"), "{e}");
        // Two tracks numbered alike.
        let twice = vec![
            (
                at("01 - One.flac"),
                values(&[
                    ("album_artist", "A"),
                    ("album", "R"),
                    ("track_number", "3"),
                    ("title", "Same"),
                ]),
            ),
            (
                at("02 - Two.flac"),
                values(&[
                    ("album_artist", "A"),
                    ("album", "R"),
                    ("track_number", "3"),
                    ("title", "Same"),
                ]),
            ),
        ];
        assert!(
            plan(&structure(), &root, &twice)
                .unwrap_err()
                .to_string()
                .contains("Two files")
        );
        // Onto another album's file.
        let onto = vec![(
            at("01 - One.flac"),
            values(&[
                ("album_artist", "A"),
                ("album", "Other"),
                ("track_number", "1"),
                ("title", "One"),
            ]),
        )];
        assert!(
            plan(&structure(), &root, &onto)
                .unwrap_err()
                .to_string()
                .contains("already exists")
        );
        // Nothing moved.
        assert!(at("01 - One.flac").is_file() && at("02 - Two.flac").is_file());
    }

    #[test]
    fn a_folder_still_holding_audio_keeps_its_other_files() {
        let d = scratch("split");
        let root = d.0.join("Lib");
        let at = |n: &str| root.join("A/R").join(n);
        touch(&at("01 - One.flac"), "1");
        touch(&at("02 - Two.flac"), "2");
        touch(&at("cover.jpg"), "art");
        // Only one track gets a new album: the cover stays with the other.
        let files = vec![
            (
                at("01 - One.flac"),
                values(&[
                    ("album_artist", "A"),
                    ("album", "S"),
                    ("track_number", "1"),
                    ("title", "One"),
                ]),
            ),
            (
                at("02 - Two.flac"),
                values(&[
                    ("album_artist", "A"),
                    ("album", "R"),
                    ("track_number", "2"),
                    ("title", "Two"),
                ]),
            ),
        ];
        let moves = plan(&structure(), &root, &files).unwrap();
        apply(&root, &moves).unwrap();
        assert!(root.join("A/S/01 - One.flac").is_file());
        assert!(at("cover.jpg").is_file() && at("02 - Two.flac").is_file());
    }

    #[test]
    fn numbers_render_as_written() {
        let mut v = values(&[
            ("track_number", "01/12"),
            ("disc_number", "01"),
            ("title", "x"),
        ]);
        as_written(&mut v);
        assert_eq!(
            (v["track_number"].as_str(), v["disc_number"].as_str()),
            ("1", "1")
        );
    }
}
