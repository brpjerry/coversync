//! Plans: what a device has to do to hold its target, worked out from what
//! the server believes it holds. Pure, so every case can be tested without
//! a server.
//!
//! The order is the device's to follow: deletes first, then new folders
//! (parents first), moves (each into a free path, a cycle going through a
//! temporary name), removing folders left empty (deepest first), and
//! finally puts, which download.
//! Moves are matched by content hash, so a file that only changed place is
//! renamed, never downloaded again.
//!
//! On storage that ignores case in names, paths that differ only by case
//! are one file: every check of what's where compares them that way, while
//! the operations keep the target's spelling.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use coversync_protocol::{Op, RESERVE_BYTES, Totals};

/// A file the device should hold. A target lists them in the order its puts
/// should go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub hash: String,
    pub size: u64,
    pub track: Option<i64>,
    pub album: String,
}

/// A file the server believes the device holds, and what it's of: a track,
/// or an album's cover (no track), when the server knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Held {
    pub hash: String,
    pub size: u64,
    pub track: Option<i64>,
    pub album: Option<String>,
}

/// What the server knows about a device's folder.
pub struct Device<'a> {
    pub inventory: &'a HashMap<String, Held>,
    /// Files coversync didn't put there, by path, with their sizes.
    pub other: &'a HashMap<String, u64>,
    /// Puts that failed on the device, by (path, hash), held back until the
    /// content changes or the user retries them.
    pub held_back: &'a HashSet<(String, String)>,
    /// The user asked for the other files to be removed.
    pub remove_other: bool,
    /// Free space, when the device said. The plan then fits it.
    pub free_bytes: Option<u64>,
    /// Files whose copy on the device stays where it is: the new content
    /// of their track or cover is still being hashed or made, so it isn't
    /// in the target yet.
    pub keep: &'a dyn Fn(&Held) -> bool,
    /// The device's storage ignores case in names.
    pub fold_case: bool,
}

#[derive(Debug, Default)]
pub struct Plan {
    pub ops: Vec<Op>,
    pub totals: Totals,
}

/// Every folder `path` is in, nearest last ("a/b/c" gives "a", "a/b").
fn ancestors(path: &str) -> impl Iterator<Item = &str> {
    path.match_indices('/').map(move |(i, _)| &path[..i])
}

/// A move worked out from matching hashes.
struct Move {
    from: String,
    to: String,
    size: u64,
    hash: String,
}

/// How a device's storage tells paths apart.
#[derive(Clone, Copy)]
struct Names {
    fold_case: bool,
}

impl Names {
    /// A path as the storage compares it: in capitals when it ignores case,
    /// as device paths are kept apart (see `coversync_core::sync`).
    fn key(self, path: &str) -> String {
        if self.fold_case {
            path.to_uppercase()
        } else {
            path.to_string()
        }
    }

    /// The folders `paths` are in, by key, each with a spelling.
    fn folders<'a>(self, paths: impl Iterator<Item = &'a str>) -> HashMap<String, &'a str> {
        let mut out = HashMap::new();
        for dir in paths.flat_map(ancestors) {
            out.entry(self.key(dir)).or_insert(dir);
        }
        out
    }
}

pub fn plan(target: &[Entry], preparing: u64, device: &Device) -> Plan {
    let inventory = device.inventory;
    let names = Names {
        fold_case: device.fold_case,
    };
    let by_path: HashMap<&str, &Entry> = target.iter().map(|e| (e.path.as_str(), e)).collect();
    let in_place = |path: &str, held: &Held| by_path.get(path).is_some_and(|e| e.hash == held.hash);
    // A file with no place in the target that stays all the same.
    let kept = |path: &str| {
        !by_path.contains_key(path) && inventory.get(path).is_some_and(|h| (device.keep)(h))
    };

    let mut held: Vec<(&String, &Held)> = inventory.iter().collect();
    held.sort_unstable_by(|a, b| a.0.cmp(b.0));
    // Files that aren't where the target wants them, by content: each can
    // move to one path that needs it.
    let mut sources: HashMap<&str, VecDeque<&str>> = HashMap::new();
    for (path, h) in &held {
        if !in_place(path, h) && !kept(path) {
            sources
                .entry(h.hash.as_str())
                .or_default()
                .push_back(path.as_str());
        }
    }

    let mut moves = Vec::new();
    let mut puts: Vec<&Entry> = Vec::new();
    for entry in target {
        if inventory
            .get(&entry.path)
            .is_some_and(|h| h.hash == entry.hash)
        {
            continue;
        }
        if let Some(from) = sources.get_mut(entry.hash.as_str()).and_then(|list| {
            // Rather a file the target has no place for, which would be
            // deleted otherwise.
            let i = list
                .iter()
                .position(|p| !by_path.contains_key(p))
                .unwrap_or(0);
            list.remove(i)
        }) {
            moves.push(Move {
                from: from.to_string(),
                to: entry.path.clone(),
                size: entry.size,
                hash: entry.hash.clone(),
            });
        } else if !device
            .held_back
            .contains(&(entry.path.clone(), entry.hash.clone()))
        {
            // Only a download is held back after failing: a move of
            // content the device has still goes.
            puts.push(entry);
        }
    }

    // What's where before the plan, by key: the files the server knows of,
    // with their sizes and the content it believes they hold.
    let mut at: HashMap<String, (&str, u64, Option<&str>)> = HashMap::new();
    for (path, size) in device.other {
        at.insert(names.key(path), (path.as_str(), *size, None));
    }
    for (path, h) in inventory {
        at.insert(
            names.key(path),
            (path.as_str(), h.size, Some(h.hash.as_str())),
        );
    }
    let had = names.folders(at.values().map(|(p, _, _)| *p));
    // The folders the moves and puts need, before any is left out for space.
    let wanted: HashSet<String> = names
        .folders(
            moves
                .iter()
                .map(|m| m.to.as_str())
                .chain(puts.iter().map(|e| e.path.as_str())),
        )
        .into_keys()
        .collect();

    // A file where a folder must go, or a folder where a file must go (a
    // structure that puts files in a folder named like one of them):
    // renaming can't get past the other, so such a file is downloaded
    // again instead, and its old copy deleted.
    let (moves, redone): (Vec<Move>, Vec<Move>) = moves.into_iter().partition(|m| {
        !wanted.contains(&names.key(&m.from)) && !had.contains_key(&names.key(&m.to))
    });
    puts.extend(
        redone
            .iter()
            .filter_map(|m| by_path.get(m.to.as_str()).copied()),
    );
    let target_order: HashMap<&str, usize> = target
        .iter()
        .enumerate()
        .map(|(i, e)| (e.path.as_str(), i))
        .collect();
    puts.sort_by_key(|e| target_order[e.path.as_str()]);
    let moved: HashSet<&str> = moves.iter().map(|m| m.from.as_str()).collect();

    // Deletes: files with no place in the target, other files the user
    // asked to remove, and files where a folder must go. Files at a target
    // path stay until they're replaced, and so do files whose new content
    // isn't ready.
    let mut deletes: BTreeMap<String, (String, u64, Option<String>)> = BTreeMap::new();
    let in_the_way = |path: &str| wanted.contains(&names.key(path));
    for (path, h) in &held {
        let path = path.as_str();
        if !moved.contains(path) && !by_path.contains_key(path) && (!kept(path) || in_the_way(path))
        {
            deletes.insert(
                names.key(path),
                (path.to_string(), h.size, Some(h.hash.clone())),
            );
        }
    }
    for (path, size) in device.other {
        if !by_path.contains_key(path.as_str()) && (device.remove_other || in_the_way(path)) {
            deletes.insert(names.key(path), (path.clone(), *size, None));
        }
    }
    // A move goes into a free path: whatever else is there goes first,
    // unless it's moving away itself.
    for m in &moves {
        if let Some((path, size, hash)) = at.get(&names.key(&m.to))
            && !moved.contains(path)
        {
            deletes.insert(
                names.key(path),
                (path.to_string(), *size, hash.map(str::to_string)),
            );
        }
    }
    let mut after: HashMap<String, (String, u64)> = at
        .iter()
        .filter(|(k, _)| !deletes.contains_key(*k))
        .map(|(k, (p, s, _))| (k.clone(), (p.to_string(), *s)))
        .collect();

    // Every name in use, files and folders, which temporary names avoid.
    let mut taken: HashSet<String> = at
        .keys()
        .cloned()
        .chain(target.iter().map(|e| names.key(&e.path)))
        .chain(had.keys().cloned())
        .chain(
            names
                .folders(target.iter().map(|e| e.path.as_str()))
                .into_keys(),
        )
        .collect();
    let ordered = order_moves(moves, &mut taken, names);
    for m in &ordered {
        let size = after.remove(&names.key(&m.from)).map_or(m.size, |(_, s)| s);
        after.insert(names.key(&m.to), (m.to.clone(), size));
    }

    // Puts, as far as the space goes. A replacement downloads beside the
    // file it replaces, which goes only once it's done.
    let replaced = |path: &str| after.get(&names.key(path)).map(|(_, size)| *size);
    let freed: u64 = deletes.values().map(|(_, size, _)| size).sum();
    let mut budget = device
        .free_bytes
        .map(|free| free.saturating_add(freed).saturating_sub(RESERVE_BYTES));
    // The least room every put needs, in order, to go.
    let (mut required, mut spare) = (0u64, 0u64);
    for entry in &puts {
        let extra = entry.size.saturating_sub(spare);
        required = required.saturating_add(extra);
        spare = (spare + extra - entry.size).saturating_add(replaced(&entry.path).unwrap_or(0));
    }
    // Short of the reserve too: what's needed beyond the free space and the
    // deletes, reserve included.
    let short_bytes = match device.free_bytes {
        Some(free) if !puts.is_empty() => required
            .saturating_add(RESERVE_BYTES)
            .saturating_sub(free.saturating_add(freed)),
        _ => 0,
    };
    let mut put_ops = Vec::with_capacity(puts.len());
    for entry in puts {
        let replaces = replaced(&entry.path);
        if let Some(b) = budget.as_mut() {
            if entry.size > *b {
                break;
            }
            *b = *b - entry.size + replaces.unwrap_or(0);
        }
        put_ops.push(Op::Put {
            path: entry.path.clone(),
            hash: entry.hash.clone(),
            size: entry.size,
            track: entry.track,
            album: Some(entry.album.clone()),
            replace: replaces.is_some(),
        });
    }
    for op in &put_ops {
        after.insert(names.key(op.path()), (op.path().to_string(), 0));
    }

    // Folders: create those the moves and puts need, parents first; remove
    // those left with nothing in them, deepest first.
    let has = names.folders(after.values().map(|(p, _)| p.as_str()));
    let needed = names.folders(
        ordered
            .iter()
            .map(|m| m.to.as_str())
            .chain(put_ops.iter().map(Op::path)),
    );
    let mut mkdirs: Vec<&str> = needed
        .iter()
        .filter(|(k, _)| !had.contains_key(*k))
        .map(|(_, p)| *p)
        .collect();
    mkdirs.sort_unstable();
    let mut rmdirs: Vec<&str> = had
        .iter()
        .filter(|(k, _)| !has.contains_key(*k))
        .map(|(_, p)| *p)
        .collect();
    rmdirs.sort_unstable_by(|a, b| {
        b.matches('/')
            .count()
            .cmp(&a.matches('/').count())
            .then(a.cmp(b))
    });

    let totals = Totals {
        delete: deletes.len() as u64,
        moves: ordered.len() as u64,
        rmdir: rmdirs.len() as u64,
        mkdir: mkdirs.len() as u64,
        put: put_ops.len() as u64,
        put_bytes: put_ops
            .iter()
            .map(|op| match op {
                Op::Put { size, .. } => *size,
                _ => 0,
            })
            .sum(),
        preparing,
        short_bytes,
    };
    let mut ops = Vec::with_capacity(
        deletes.len() + ordered.len() + rmdirs.len() + mkdirs.len() + put_ops.len(),
    );
    ops.extend(
        deletes
            .into_values()
            .map(|(path, size, hash)| Op::Delete { path, size, hash }),
    );
    ops.extend(mkdirs.into_iter().map(|p| Op::Mkdir { path: p.into() }));
    ops.extend(ordered.into_iter().map(|m| Op::Move {
        from: m.from,
        path: m.to,
        size: m.size,
        hash: m.hash,
    }));
    ops.extend(rmdirs.into_iter().map(|p| Op::Rmdir { path: p.into() }));
    ops.extend(put_ops);
    Plan { ops, totals }
}

/// Moves in an order where each goes into a free path: one whose file is
/// still to move away waits for it. Moves that wait on each other in a
/// circle (two files trading places, or a file whose name changes only by
/// case on storage that ignores it) are untied by moving one to a temporary
/// name first. `taken` holds every name in use, which temporary names avoid.
fn order_moves(mut moves: Vec<Move>, taken: &mut HashSet<String>, names: Names) -> Vec<Move> {
    // The pending move whose file is at a path, and the one into a path.
    let mut leaving: HashMap<String, usize> = moves
        .iter()
        .enumerate()
        .map(|(i, m)| (names.key(&m.from), i))
        .collect();
    let into: HashMap<String, usize> = moves
        .iter()
        .enumerate()
        .map(|(i, m)| (names.key(&m.to), i))
        .collect();
    let mut done = vec![false; moves.len()];
    let mut ready: VecDeque<usize> = (0..moves.len())
        .filter(|&i| !leaving.contains_key(&names.key(&moves[i].to)))
        .collect();
    let mut ordered = Vec::with_capacity(moves.len());
    let mut next_stuck = 0;
    loop {
        while let Some(i) = ready.pop_front() {
            if done[i] {
                continue;
            }
            done[i] = true;
            let from = names.key(&moves[i].from);
            leaving.remove(&from);
            // Whatever waited for this path can go now.
            if let Some(&j) = into.get(&from)
                && !done[j]
            {
                ready.push_back(j);
            }
            ordered.push(Move {
                from: moves[i].from.clone(),
                to: moves[i].to.clone(),
                size: moves[i].size,
                hash: moves[i].hash.clone(),
            });
        }
        // Everything left waits on something else left: a circle.
        while next_stuck < moves.len() && done[next_stuck] {
            next_stuck += 1;
        }
        let Some(k) = (next_stuck < moves.len()).then_some(next_stuck) else {
            break;
        };
        let from = moves[k].from.clone();
        let temporary = free_name(&from, taken, names);
        leaving.remove(&names.key(&from));
        leaving.insert(names.key(&temporary), k);
        ordered.push(Move {
            from: from.clone(),
            to: temporary.clone(),
            size: moves[k].size,
            hash: moves[k].hash.clone(),
        });
        moves[k].from = temporary;
        if let Some(&j) = into.get(&names.key(&from)) {
            ready.push_back(j);
        }
    }
    ordered
}

/// A temporary name beside `path` that nothing uses, which it then takes.
/// It's short (".cs1", ".cs2", …), so it fits wherever `path` does: no
/// shorter audio file name is worth the case.
fn free_name(path: &str, taken: &mut HashSet<String>, names: Names) -> String {
    let dir = path.rsplit_once('/').map(|(dir, _)| dir);
    (1..)
        .map(|n| match dir {
            Some(dir) => format!("{dir}/.cs{n}"),
            None => format!(".cs{n}"),
        })
        .find(|name| taken.insert(names.key(name)))
        .expect("a free name")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, hash: &str, size: u64) -> Entry {
        Entry {
            path: path.into(),
            hash: hash.into(),
            size,
            track: None,
            album: "al".into(),
        }
    }

    fn held(files: &[(&str, &str, u64)]) -> HashMap<String, Held> {
        files
            .iter()
            .map(|(p, h, s)| {
                (
                    p.to_string(),
                    Held {
                        hash: h.to_string(),
                        size: *s,
                        track: None,
                        album: None,
                    },
                )
            })
            .collect()
    }

    /// The plan's operations, written short: "put a/1 (replace)", "move x>y".
    fn ops(
        target: &[Entry],
        inventory: &[(&str, &str, u64)],
        other: &[(&str, u64)],
    ) -> Vec<String> {
        let inventory = held(inventory);
        let other = other.iter().map(|(p, s)| (p.to_string(), *s)).collect();
        let plan = plan(
            target,
            0,
            &Device {
                inventory: &inventory,
                other: &other,
                held_back: &HashSet::new(),
                remove_other: false,
                free_bytes: None,
                keep: &|_| false,
                fold_case: false,
            },
        );
        plan.ops.iter().map(short).collect()
    }

    fn short(op: &Op) -> String {
        match op {
            Op::Delete { path, .. } => format!("delete {path}"),
            Op::Move { from, path, .. } => format!("move {from}>{path}"),
            Op::Rmdir { path } => format!("rmdir {path}"),
            Op::Mkdir { path } => format!("mkdir {path}"),
            Op::Put { path, replace, .. } => {
                format!("put {path}{}", if *replace { " (replace)" } else { "" })
            }
        }
    }

    #[test]
    fn a_new_device_gets_folders_then_files() {
        let target = [
            entry("A/x/1", "h1", 5),
            entry("A/x/2", "h2", 5),
            entry("B/1", "h3", 5),
        ];
        assert_eq!(
            ops(&target, &[], &[]),
            [
                "mkdir A",
                "mkdir A/x",
                "mkdir B",
                "put A/x/1",
                "put A/x/2",
                "put B/1"
            ]
        );
        // Once it holds them, there's nothing to do.
        let all = [("A/x/1", "h1", 5), ("A/x/2", "h2", 5), ("B/1", "h3", 5)];
        assert!(ops(&target, &all, &[]).is_empty());
    }

    #[test]
    fn a_new_structure_is_renames() {
        let inventory = [("Old/1", "h1", 5), ("Old/2", "h2", 5)];
        let target = [entry("New/a/1", "h1", 5), entry("New/a/2", "h2", 5)];
        assert_eq!(
            ops(&target, &inventory, &[]),
            [
                "mkdir New",
                "mkdir New/a",
                "move Old/1>New/a/1",
                "move Old/2>New/a/2",
                "rmdir Old"
            ]
        );
    }

    #[test]
    fn edits_replace_and_removals_delete() {
        let inventory = [("A/1", "old", 5), ("A/2", "h2", 5), ("Gone/1", "h3", 5)];
        let target = [entry("A/1", "new", 6), entry("A/2", "h2", 5)];
        assert_eq!(
            ops(&target, &inventory, &[]),
            ["delete Gone/1", "rmdir Gone", "put A/1 (replace)"]
        );
    }

    #[test]
    fn files_that_trade_places_go_through_a_temporary_name() {
        let inventory = [("a", "h1", 1), ("b", "h2", 2), ("c", "h3", 3)];
        // a and b trade; c moves into d, whose content moves on from there.
        let target = [
            entry("a", "h2", 2),
            entry("b", "h1", 1),
            entry("d", "h3", 3),
        ];
        assert_eq!(
            ops(&target, &inventory, &[]),
            ["move c>d", "move b>.cs1", "move a>b", "move .cs1>a"]
        );
        // A chain needs no temporary name: x waits until y has moved on.
        let inventory = [("x", "h1", 1), ("y", "h2", 2)];
        let target = [entry("y", "h1", 1), entry("z", "h2", 2)];
        assert_eq!(ops(&target, &inventory, &[]), ["move y>z", "move x>y"]);
    }

    #[test]
    fn other_files_stay_unless_in_the_way() {
        let inventory = [("old", "h1", 1)];
        let other = [("mine.mp3", 7), ("new", 9), ("put", 4)];
        let target = [entry("new", "h1", 1), entry("put", "h2", 2)];
        // One at a move's path goes first; one at a put's path is replaced.
        assert_eq!(
            ops(&target, &inventory, &other),
            ["delete new", "move old>new", "put put (replace)"]
        );
        // Asked to, the rest go too.
        let inventory = held(&[]);
        let other: HashMap<String, u64> = [("mine.mp3".to_string(), 7)].into();
        let plan = plan(
            &[],
            0,
            &Device {
                inventory: &inventory,
                other: &other,
                held_back: &HashSet::new(),
                remove_other: true,
                free_bytes: None,
                keep: &|_| false,
                fold_case: false,
            },
        );
        assert_eq!(
            plan.ops.iter().map(short).collect::<Vec<_>>(),
            ["delete mine.mp3"]
        );
    }

    #[test]
    fn puts_stop_where_the_space_ends() {
        let mb = 1 << 20;
        let inventory = held(&[("gone", "h0", 10 * mb)]);
        let target = [
            entry("a", "h1", 30 * mb),
            entry("b", "h2", 50 * mb),
            entry("c", "h3", mb),
        ];
        let plan = plan(
            &target,
            2,
            &Device {
                inventory: &inventory,
                other: &HashMap::new(),
                held_back: &HashSet::new(),
                remove_other: false,
                // With the delete, 30 MB free after the 64 MB reserve.
                free_bytes: Some(84 * mb),
                keep: &|_| false,
                fold_case: false,
            },
        );
        // Order is kept: c would fit, but not before b.
        assert_eq!(
            plan.ops.iter().map(short).collect::<Vec<_>>(),
            ["delete gone", "put a"]
        );
        assert_eq!(plan.totals.short_bytes, 51 * mb);
        assert_eq!((plan.totals.put, plan.totals.preparing), (1, 2));
    }

    #[test]
    fn failed_puts_are_held_back() {
        // c failed before, but the device has its content elsewhere now:
        // moving it needs no download.
        let inventory = held(&[("old/c", "h3", 1)]);
        let target = [
            entry("a", "h1", 1),
            entry("b", "h2", 1),
            entry("c", "h3", 1),
        ];
        let held_back = HashSet::from([
            ("a".to_string(), "h1".to_string()),
            ("c".to_string(), "h3".to_string()),
        ]);
        let plan = plan(
            &target,
            0,
            &Device {
                inventory: &inventory,
                other: &HashMap::new(),
                held_back: &held_back,
                remove_other: false,
                free_bytes: None,
                keep: &|_| false,
                fold_case: false,
            },
        );
        assert_eq!(
            plan.ops.iter().map(short).collect::<Vec<_>>(),
            ["move old/c>c", "rmdir old", "put b"]
        );
    }

    #[test]
    fn a_file_where_a_folder_must_go_is_downloaded_again() {
        // A structure that files tracks in a folder named like the track:
        // renaming can't put x.flac/x.flac where x.flac is, nor back.
        let target = [entry("x.flac/x.flac", "h1", 1)];
        assert_eq!(
            ops(&target, &[("x.flac", "h1", 1)], &[]),
            ["delete x.flac", "mkdir x.flac", "put x.flac/x.flac"]
        );
        let target = [entry("x.flac", "h1", 1)];
        assert_eq!(
            ops(&target, &[("x.flac/x.flac", "h1", 1)], &[]),
            ["delete x.flac/x.flac", "rmdir x.flac", "put x.flac"]
        );
    }

    /// The plan's operations for a device whose storage ignores case.
    fn folded(target: &[Entry], inventory: &[(&str, &str, u64)]) -> Vec<String> {
        let inventory = held(inventory);
        let plan = plan(
            target,
            0,
            &Device {
                inventory: &inventory,
                other: &HashMap::new(),
                held_back: &HashSet::new(),
                remove_other: false,
                free_bytes: None,
                keep: &|_| false,
                fold_case: true,
            },
        );
        plan.ops.iter().map(short).collect()
    }

    #[test]
    fn names_that_differ_by_case_alone_are_one_file_where_case_is_ignored() {
        // A new spelling goes through a temporary name, where it would be
        // the same file.
        let inventory = [("x.flac", "h1", 1)];
        let target = [entry("X.flac", "h1", 1)];
        assert_eq!(
            folded(&target, &inventory),
            ["move x.flac>.cs1", "move .cs1>X.flac"]
        );
        assert_eq!(ops(&target, &inventory, &[]), ["move x.flac>X.flac"]);
        // Two files trading names that differ only by case are a circle.
        let inventory = [("a.flac", "h1", 1), ("b.flac", "h2", 2)];
        let target = [entry("B.flac", "h1", 1), entry("A.flac", "h2", 2)];
        assert_eq!(
            folded(&target, &inventory),
            ["move a.flac>.cs1", "move b.flac>A.flac", "move .cs1>B.flac"]
        );
    }

    #[test]
    fn copies_wait_for_their_new_content() {
        // Track 7's title was edited, which moves it under the device's
        // structure: its new hash isn't known yet, so the device keeps the
        // copy it has, at its old path, rather than losing it meanwhile.
        let mut inventory = held(&[("A/a.flac", "old", 5), ("Gone/g.flac", "h", 5)]);
        inventory.get_mut("A/a.flac").unwrap().track = Some(7);
        let plan = plan(
            &[],
            1,
            &Device {
                inventory: &inventory,
                other: &HashMap::new(),
                held_back: &HashSet::new(),
                remove_other: false,
                free_bytes: None,
                keep: &|h| h.track == Some(7),
                fold_case: false,
            },
        );
        assert_eq!(
            plan.ops.iter().map(short).collect::<Vec<_>>(),
            ["delete Gone/g.flac", "rmdir Gone"]
        );
    }

    #[test]
    fn a_replacement_needs_room_beside_the_file_it_replaces() {
        let mb = 1 << 20;
        let inventory = held(&[("a", "old", 100 * mb)]);
        let plan = plan(
            &[entry("a", "new", 100 * mb)],
            0,
            &Device {
                inventory: &inventory,
                other: &HashMap::new(),
                held_back: &HashSet::new(),
                remove_other: false,
                // Only the reserve is free.
                free_bytes: Some(64 * mb),
                keep: &|_| false,
                fold_case: false,
            },
        );
        assert!(plan.ops.is_empty());
        assert_eq!(plan.totals.short_bytes, 100 * mb);
    }
}
