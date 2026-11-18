//! Whether an album is missing tracks, by its own numbering: no MusicBrainz,
//! just its tracks' numbers and totals. Track numbers below the last one (or
//! up to the track total) that no track has are missing, and so are discs
//! numbered below the last one it has that no track is on. Discs past the
//! last one aren't: a disc total often counts a DVD or Blu-ray that holds no
//! audio. A total for the whole album on every disc says how many tracks
//! there should be but not where the missing ones go: those are counted,
//! unplaced.
//!
//! It says nothing when it can't tell: an album with a track that has no
//! number (it could be any of the missing ones), or with some tracks on a
//! numbered disc and some not.

use std::collections::{BTreeMap, BTreeSet};

use crate::model::{Gap, Incomplete, Track};

/// Track and disc numbers above this aren't an album's (and totals above it
/// aren't totals).
const MAX_NUMBER: u32 = 999;

/// The value most of them have, zero and none aside; the smaller on a tie,
/// so a disagreement asks for fewer tracks.
fn usual(values: impl Iterator<Item = Option<u32>>) -> Option<u32> {
    let mut counts: BTreeMap<u32, usize> = BTreeMap::new();
    for v in values.flatten().filter(|v| *v > 0) {
        *counts.entry(v).or_default() += 1;
    }
    // Ascending, so on a tie `max_by_key` would keep the larger: reverse.
    counts
        .into_iter()
        .rev()
        .max_by_key(|(_, n)| *n)
        .map(|(v, _)| v)
}

/// Runs of the numbers from 1 to `last` that aren't in `present`.
fn gaps(present: &BTreeSet<u32>, last: u32, disc: Option<u32>, out: &mut Vec<Gap>) {
    // The first number not accounted for yet.
    let mut next: u32 = 1;
    for &n in present.range(1..=last) {
        if n > next {
            out.push(Gap {
                disc,
                from: next,
                to: n - 1,
            });
        }
        next = n + 1;
    }
    if next <= last {
        out.push(Gap {
            disc,
            from: next,
            to: last,
        });
    }
}

/// What `tracks`' numbering says the album lacks; None when nothing (or
/// when it can't tell).
pub fn incomplete(tracks: &[&Track]) -> Option<Incomplete> {
    let number = |t: &Track| t.track_no.filter(|n| *n > 0);
    // A number past any real album's is some other numbering (a catalog's,
    // say), whose gaps mean nothing here.
    if tracks.is_empty()
        || tracks
            .iter()
            .any(|t| number(t).is_none_or(|n| n > MAX_NUMBER))
    {
        return None;
    }
    let disc_of = |t: &Track| t.disc_no.filter(|d| *d > 0);
    if tracks
        .iter()
        .any(|t| disc_of(t).is_some_and(|d| d > MAX_NUMBER))
    {
        return None;
    }
    let on_discs = tracks.iter().filter(|t| disc_of(t).is_some()).count();
    if on_discs != 0 && on_discs != tracks.len() {
        return None;
    }
    let mut discs: BTreeMap<u32, Vec<&Track>> = BTreeMap::new();
    for t in tracks {
        discs.entry(disc_of(t).unwrap_or(1)).or_default().push(t);
    }
    let span = |ts: &[&Track]| {
        let numbers = ts.iter().filter_map(|t| number(t));
        (
            numbers.clone().min().unwrap_or(0),
            numbers.max().unwrap_or(0),
        )
    };
    let missing_discs: Vec<u32> = match discs.keys().last() {
        Some(last) if on_discs > 0 => (1..*last).filter(|d| !discs.contains_key(d)).collect(),
        _ => Vec::new(),
    };
    // Disc 2 starting where disc 1 left off: numbered straight through, so
    // one run of numbers.
    let through = discs.len() > 1
        && discs
            .values()
            .zip(discs.values().skip(1))
            .all(|(a, b)| span(b).0 > span(a).1);
    let runs: Vec<(Option<u32>, Vec<&Track>)> = if through || on_discs == 0 {
        vec![(None, tracks.to_vec())]
    } else {
        // Which disc a gap is on only needs saying when there's more than one.
        let several = discs.len() > 1 || !missing_discs.is_empty() || !discs.contains_key(&1);
        discs
            .into_iter()
            .map(|(d, ts)| (several.then_some(d), ts))
            .collect()
    };
    // A total below the numbers there are (or past any album's) is wrong, not
    // a limit: left out before the vote, so it can't outvote a usable one.
    let totals: Vec<Option<u32>> = runs
        .iter()
        .map(|(_, ts)| {
            let possible = span(ts).1..=MAX_NUMBER;
            usual(
                ts.iter()
                    .map(|t| t.track_total.filter(|n| possible.contains(n))),
            )
        })
        .collect();
    // Every disc tagged with the album's total rather than its own: the same
    // total on each, enough for all of them together.
    let tops: u32 = runs.iter().map(|(_, ts)| span(ts).1).sum();
    let album_wide = runs.len() > 1
        && totals.iter().all(|t| *t == totals[0])
        && totals[0].is_some_and(|t| t >= tops);
    // Beyond the gaps, what the album's total asks for: unless a disc it may
    // count isn't there (one in between, or one past the last, a DVD maybe).
    let declared = usual(
        tracks
            .iter()
            .map(|t| t.disc_total.filter(|n| *n <= MAX_NUMBER)),
    );
    let unplaced = match totals[0] {
        Some(total)
            if album_wide
                && missing_discs.is_empty()
                && declared.is_none_or(|n| n as usize == runs.len()) =>
        {
            total - tops
        }
        _ => 0,
    };
    let mut missing = Vec::new();
    for ((disc, ts), total) in runs.iter().zip(totals) {
        let present: BTreeSet<u32> = ts.iter().filter_map(|t| number(t)).collect();
        let total = total.filter(|_| !album_wide);
        gaps(&present, total.unwrap_or(span(ts).1), *disc, &mut missing);
    }
    (!missing.is_empty() || !missing_discs.is_empty() || unplaced > 0).then_some(Incomplete {
        tracks: missing,
        discs: missing_discs,
        unplaced,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Collection;

    /// A track numbered `disc`-`no` of `total`, on a disc of `discs`.
    fn t(disc: Option<u32>, no: Option<u32>, total: Option<u32>, discs: Option<u32>) -> Track {
        Track {
            id: 0,
            album_id: "a".into(),
            title: String::new(),
            artist: String::new(),
            album: String::new(),
            album_artist: None,
            track_no: no,
            track_total: total,
            disc_no: disc,
            disc_total: discs,
            year: None,
            genre: None,
            duration_ms: 0,
            sample_rate: None,
            bit_depth: None,
            channels: None,
            bitrate: None,
            format: "FLAC".into(),
            rel_path: String::new(),
            collection: Collection::Inbox,
        }
    }

    fn check(tracks: &[Track]) -> Option<Incomplete> {
        incomplete(&tracks.iter().collect::<Vec<_>>())
    }

    /// Tracks `numbers` of `total` on one disc.
    fn one_disc(numbers: &[u32], total: Option<u32>) -> Vec<Track> {
        numbers
            .iter()
            .map(|n| t(None, Some(*n), total, None))
            .collect()
    }

    fn gap(disc: Option<u32>, from: u32, to: u32) -> Gap {
        Gap { disc, from, to }
    }

    #[test]
    fn a_whole_album_is_complete() {
        assert_eq!(check(&one_disc(&[1, 2, 3], Some(3))), None);
        assert_eq!(check(&one_disc(&[1, 2, 3], None)), None);
        assert_eq!(check(&one_disc(&[3, 1, 2], None)), None);
        // The same number twice (a file in two formats) is no gap.
        assert_eq!(check(&one_disc(&[1, 2, 2, 3], Some(3))), None);
    }

    #[test]
    fn numbers_not_there_are_missing() {
        let found = check(&one_disc(&[1, 2, 4, 8], None)).unwrap();
        assert_eq!(found.tracks, [gap(None, 3, 3), gap(None, 5, 7)]);
        assert!(found.discs.is_empty());
        // Track 1 too.
        let found = check(&one_disc(&[2, 3], None)).unwrap();
        assert_eq!(found.tracks, [gap(None, 1, 1)]);
    }

    #[test]
    fn the_track_total_says_how_many_there_should_be() {
        let found = check(&one_disc(&[1, 2, 3], Some(5))).unwrap();
        assert_eq!(found.tracks, [gap(None, 4, 5)]);
        // A single with its instrumental missing.
        let found = check(&one_disc(&[1], Some(2))).unwrap();
        assert_eq!(found.tracks, [gap(None, 2, 2)]);
    }

    #[test]
    fn a_total_below_the_numbers_is_ignored() {
        assert_eq!(check(&one_disc(&[1, 2, 3, 4], Some(2))), None);
    }

    #[test]
    fn impossible_totals_dont_outvote_a_possible_one() {
        // 2 is below track 3, 1000 past any album's: 5 is the total.
        let mut tracks = one_disc(&[1, 2, 3], Some(2));
        tracks[2].track_total = Some(5);
        assert_eq!(check(&tracks).unwrap().tracks, [gap(None, 4, 5)]);
        let mut tracks = one_disc(&[1, 2, 3], Some(1000));
        tracks[2].track_total = Some(5);
        assert_eq!(check(&tracks).unwrap().tracks, [gap(None, 4, 5)]);
    }

    #[test]
    fn the_usual_total_wins_and_a_tie_asks_for_fewer() {
        let mut tracks = one_disc(&[1, 2, 3], Some(4));
        tracks[0].track_total = Some(9);
        assert_eq!(check(&tracks).unwrap().tracks, [gap(None, 4, 4)]);
        let mut tracks = one_disc(&[1, 2], Some(5));
        tracks[0].track_total = Some(3);
        assert_eq!(check(&tracks).unwrap().tracks, [gap(None, 3, 3)]);
        // Totals of zero aren't totals.
        assert_eq!(check(&one_disc(&[1, 2], Some(0))), None);
    }

    #[test]
    fn numbers_past_an_albums_mean_nothing() {
        // A garbage total is no total.
        assert_eq!(check(&one_disc(&[1, 2], Some(u32::MAX))), None);
        assert_eq!(check(&one_disc(&[1, 2], Some(1000))), None);
        assert_eq!(
            check(&one_disc(&[1, 2], Some(999))).unwrap().tracks,
            [gap(None, 3, 999)]
        );
        // Nor are numbers like these an album's.
        assert_eq!(check(&one_disc(&[1, 2, 4_000_000_000], None)), None);
        assert_eq!(check(&[t(Some(1000), Some(1), None, None)]), None);
    }

    #[test]
    fn a_track_without_a_number_could_be_any_gap() {
        let mut tracks = one_disc(&[1, 2, 4], Some(4));
        tracks.push(t(None, None, Some(4), None));
        assert_eq!(check(&tracks), None);
        // Nor does 0 number one.
        let mut tracks = one_disc(&[1, 2], Some(3));
        tracks.push(t(None, Some(0), Some(3), None));
        assert_eq!(check(&tracks), None);
    }

    #[test]
    fn discs_number_their_own_tracks() {
        let tracks = [
            t(Some(1), Some(1), Some(3), Some(2)),
            t(Some(1), Some(3), Some(3), Some(2)),
            t(Some(2), Some(1), Some(2), Some(2)),
        ];
        let found = check(&tracks).unwrap();
        assert_eq!(found.tracks, [gap(Some(1), 2, 2), gap(Some(2), 2, 2)]);
        assert!(found.discs.is_empty());
    }

    #[test]
    fn a_disc_between_others_is_missing_but_not_one_past_the_last() {
        let tracks = [
            t(Some(1), Some(1), Some(1), Some(4)),
            t(Some(3), Some(1), Some(1), Some(4)),
        ];
        let found = check(&tracks).unwrap();
        assert!(found.tracks.is_empty());
        assert_eq!(found.discs, [2]);
        // Disc 4 of 4, a DVD maybe, isn't asked for.
        let tracks = [
            t(Some(1), Some(1), Some(1), Some(2)),
            t(Some(2), Some(1), Some(1), Some(2)),
        ];
        assert_eq!(check(&tracks), None);
        let dvd_too = [t(Some(1), Some(1), Some(1), Some(2))];
        assert_eq!(check(&dvd_too), None);
    }

    #[test]
    fn only_disc_2_misses_disc_1_and_says_which_disc_its_gaps_are_on() {
        let tracks = [
            t(Some(2), Some(1), Some(3), Some(2)),
            t(Some(2), Some(2), Some(3), Some(2)),
        ];
        let found = check(&tracks).unwrap();
        assert_eq!(found.discs, [1]);
        assert_eq!(found.tracks, [gap(Some(2), 3, 3)]);
        // Disc 1 of 1: no disc to name.
        let tracks = [
            t(Some(1), Some(1), Some(2), Some(1)),
            t(Some(1), Some(2), Some(2), Some(1)),
        ];
        assert_eq!(check(&tracks), None);
        let tracks = [t(Some(1), Some(2), Some(2), Some(1))];
        assert_eq!(check(&tracks).unwrap().tracks, [gap(None, 1, 1)]);
    }

    #[test]
    fn numbers_running_on_through_the_discs_are_one_run() {
        let tracks = [
            t(Some(1), Some(1), Some(10), None),
            t(Some(1), Some(2), Some(10), None),
            t(Some(2), Some(3), Some(10), None),
            t(Some(2), Some(5), Some(10), None),
        ];
        let found = check(&tracks).unwrap();
        assert_eq!(found.tracks, [gap(None, 4, 4), gap(None, 6, 10)]);
        // Each disc's own total, below its numbers, is no limit.
        let tracks = [
            t(Some(1), Some(1), Some(2), None),
            t(Some(1), Some(2), Some(2), None),
            t(Some(2), Some(3), Some(2), None),
            t(Some(2), Some(4), Some(2), None),
        ];
        assert_eq!(check(&tracks), None);
    }

    #[test]
    fn an_album_wide_total_on_every_disc_isnt_each_discs() {
        // 3 + 2 tracks, each tagged "of 5".
        let tracks = [
            t(Some(1), Some(1), Some(5), Some(2)),
            t(Some(1), Some(2), Some(5), Some(2)),
            t(Some(1), Some(3), Some(5), Some(2)),
            t(Some(2), Some(1), Some(5), Some(2)),
            t(Some(2), Some(2), Some(5), Some(2)),
        ];
        assert_eq!(check(&tracks), None);
        // Gaps still show.
        let tracks = [
            t(Some(1), Some(1), Some(5), Some(2)),
            t(Some(1), Some(3), Some(5), Some(2)),
            t(Some(2), Some(1), Some(5), Some(2)),
        ];
        assert_eq!(check(&tracks).unwrap().tracks, [gap(Some(1), 2, 2)]);
        // The same total on each disc, but each disc's own: 4 of 4 and 2 of 4.
        let tracks = [
            t(Some(1), Some(1), Some(4), Some(2)),
            t(Some(1), Some(2), Some(4), Some(2)),
            t(Some(1), Some(3), Some(4), Some(2)),
            t(Some(1), Some(4), Some(4), Some(2)),
            t(Some(2), Some(1), Some(4), Some(2)),
            t(Some(2), Some(2), Some(4), Some(2)),
        ];
        assert_eq!(check(&tracks).unwrap().tracks, [gap(Some(2), 3, 4)]);
    }

    #[test]
    fn an_album_wide_total_counts_what_it_cant_place() {
        // Tracks 1-2 on each of 2 discs, each "of 5": one more, on either.
        let tracks = [
            t(Some(1), Some(1), Some(5), Some(2)),
            t(Some(1), Some(2), Some(5), Some(2)),
            t(Some(2), Some(1), Some(5), Some(2)),
            t(Some(2), Some(2), Some(5), Some(2)),
        ];
        let found = check(&tracks).unwrap();
        assert!(found.tracks.is_empty() && found.discs.is_empty());
        assert_eq!(found.unplaced, 1);
        // A gap too: 1, 3 and 1 of 6 lack disc 1's track 2, and 2 more on either.
        let tracks = [
            t(Some(1), Some(1), Some(6), None),
            t(Some(1), Some(3), Some(6), None),
            t(Some(2), Some(1), Some(6), None),
        ];
        let found = check(&tracks).unwrap();
        assert_eq!(found.tracks, [gap(Some(1), 2, 2)]);
        assert_eq!(found.unplaced, 2);
        // A third disc the tags count (a DVD, maybe) could hold the rest.
        let tracks = [
            t(Some(1), Some(1), Some(5), Some(3)),
            t(Some(1), Some(2), Some(5), Some(3)),
            t(Some(2), Some(1), Some(5), Some(3)),
            t(Some(2), Some(2), Some(5), Some(3)),
        ];
        assert_eq!(check(&tracks), None);
    }

    #[test]
    fn some_tracks_on_a_disc_and_some_not_cant_be_told() {
        let tracks = [
            t(Some(1), Some(1), Some(12), Some(2)),
            t(Some(1), Some(2), Some(12), Some(2)),
            t(None, Some(1), Some(2), None),
            t(None, Some(2), Some(2), None),
        ];
        assert_eq!(check(&tracks), None);
    }
}
