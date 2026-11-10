//! The arithmetic of momentum scrolling: whether a report looks like a
//! smoother's last one, how fast a coast starts, and how it goes on. The
//! window's side (`cpp/momentum.cpp`), which watches the wheel events and
//! sends the coast's, calls these; `cpp/momentum.h` says what it is all for.
//!
//! Speeds are in angle units per second (two angle units are one pixel),
//! times in seconds.

use crate::ffi::qt::{CoastStart, CoastState, CoastStep, Sample, Tuning};

/// Slower than this, nothing coasts: 120 px/s.
const SLOWEST: f64 = 240.0;
/// A coast ends below this.
const END: f64 = 40.0;
/// The fingers had stopped if their last movement is older than this when
/// they lift.
const STOPPED: f64 = 0.05;
/// A report this many times the ones before it is held back.
const FLUSH_FACTOR: f64 = 4.0;
/// Only right behind the reports before it.
const FLUSH_GAP: f64 = 0.03;
/// How far back the speed on screen looks at all.
const LOOK_BACK: f64 = 0.150;
/// A longer wait between two reports isn't one movement.
const LONGEST_STEP: f64 = 0.05;
/// A coast step never covers more time than this, however late it comes.
const LONGEST_TICK: f64 = 0.05;

/// Whether the report `(dx, dy)`, arriving at `now`, is several times the
/// size of the ones before it, right behind them: what a trackpad smoother
/// sends as the fingers lift.
pub fn looks_like_lift_flush(samples: &[Sample], dx: f64, dy: f64, now: f64) -> bool {
    let [.., a, b, c] = samples else {
        return false;
    };
    if now - c.arrived > FLUSH_GAP {
        return false;
    }
    let usual = [a, b, c].iter().map(|s| s.x.hypot(s.y)).sum::<f64>() / 3.0;
    dx.hypot(dy) > FLUSH_FACTOR * usual.max(1.0)
}

/// How a coast starts as the fingers lift at `now`, if the page was moving
/// or the fingers were. `held` is the report held back as a smoother's last
/// one, if there is one.
pub fn coast_start(
    samples: &[Sample],
    held: Option<(f64, f64)>,
    now: f64,
    tuning: &Tuning,
) -> CoastStart {
    let none = CoastStart {
        starts: false,
        shown_x: 0.0,
        shown_y: 0.0,
        fingers_x: 0.0,
        fingers_y: 0.0,
    };
    let Some(last) = samples.last() else {
        return none;
    };
    if now - last.arrived > STOPPED {
        return none;
    }
    // The speed that was on screen: movement over time, with the latest
    // movement counting most. A plain average over the gesture's end is too
    // fast after a slowing swipe and too slow after a quickening one.
    let (mut distance_x, mut distance_y, mut time) = (0.0, 0.0, 0.0);
    for pair in samples.windows(2) {
        let (before, sample) = (&pair[0], &pair[1]);
        let age = last.time - sample.time;
        let step = sample.time - before.time;
        if age > LOOK_BACK || step <= 0.0 || step > LONGEST_STEP {
            continue;
        }
        let weight = (-age / tuning.recent).exp();
        distance_x += weight * sample.x;
        distance_y += weight * sample.y;
        time += weight * step;
    }
    if time <= 0.0 {
        return none;
    }
    let shown = (distance_x / time, distance_y / time);
    // The fingers' own speed at the end, from what the smoother still held:
    // never against the page's direction, and at most twice its speed.
    let fingers = match held {
        Some(held) if tuning.lift_lag > 0.0 => (
            toward(held.0 / tuning.lift_lag, shown.0),
            toward(held.1 / tuning.lift_lag, shown.1),
        ),
        _ => shown,
    };
    if shown.0.hypot(shown.1) <= SLOWEST && fingers.0.hypot(fingers.1) <= SLOWEST {
        return none;
    }
    CoastStart {
        starts: true,
        shown_x: shown.0,
        shown_y: shown.1,
        fingers_x: fingers.0,
        fingers_y: fingers.1,
    }
}

/// `speed`, kept to `shown`'s direction and to twice its size.
fn toward(speed: f64, shown: f64) -> f64 {
    let limit = 2.0 * shown.abs();
    if shown >= 0.0 {
        speed.clamp(0.0, limit)
    } else {
        speed.clamp(-limit, 0.0)
    }
}

/// The coast as it is right after [`coast_start`].
pub fn coast_state(start: &CoastStart) -> CoastState {
    CoastState {
        velocity_x: start.fingers_x,
        velocity_y: start.fingers_y,
        excess_x: start.shown_x - start.fingers_x,
        excess_y: start.shown_y - start.fingers_y,
        carry_x: 0.0,
        carry_y: 0.0,
    }
}

/// Moves the coast on by `dt`: the whole angle units to scroll by now, and
/// whether that was its last step. The fingers' speed falls off over
/// `tuning.tau`; what the speed on screen had over or under it, over
/// `tuning.blend`.
pub fn coast_step(state: &mut CoastState, dt: f64, tuning: &Tuning) -> CoastStep {
    let dt = dt.clamp(0.0, LONGEST_TICK);
    let keep = (-dt / tuning.tau).exp();
    let keep_excess = (-dt / tuning.blend).exp();
    let axis = |velocity: &mut f64, excess: &mut f64, carry: &mut f64| {
        // The distance covered in dt at a decaying speed, plus the fading
        // difference from the speed on screen.
        *carry +=
            *velocity * tuning.tau * (1.0 - keep) + *excess * tuning.blend * (1.0 - keep_excess);
        *velocity *= keep;
        *excess *= keep_excess;
        let whole = carry.trunc();
        *carry -= whole;
        whole as i32
    };
    let dx = axis(
        &mut state.velocity_x,
        &mut state.excess_x,
        &mut state.carry_x,
    );
    let dy = axis(
        &mut state.velocity_y,
        &mut state.excess_y,
        &mut state.carry_y,
    );
    let speed = (state.velocity_x + state.excess_x).hypot(state.velocity_y + state.excess_y);
    let done = speed < END && state.excess_x.hypot(state.excess_y) < END;
    CoastStep { dx, dy, done }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TUNING: Tuning = Tuning {
        tau: 0.325,
        recent: 0.030,
        lift_lag: 0.045,
        blend: 0.040,
    };

    /// Reports every 4 ms, the last at `end`, each `size(i)` angle units down
    /// the page, `i` counting back from the last (0).
    fn reports(count: usize, end: f64, size: impl Fn(usize) -> f64) -> Vec<Sample> {
        (0..count)
            .map(|n| {
                let back = count - 1 - n;
                let time = end - back as f64 * 0.004;
                Sample {
                    time,
                    arrived: time,
                    x: 0.0,
                    y: size(back),
                }
            })
            .collect()
    }

    #[test]
    fn a_report_many_times_the_usual_right_behind_them_is_a_lift_flush() {
        let steady = reports(20, 1.0, |_| 30.0);
        // What the Magic Trackpad plugin sends: about ten reports' worth.
        assert!(looks_like_lift_flush(&steady, 0.0, 300.0, 1.002));
        assert!(looks_like_lift_flush(&steady, 0.0, -300.0, 1.002));
        // A quickening swipe isn't one.
        assert!(!looks_like_lift_flush(&steady, 0.0, 60.0, 1.004));
        assert!(!looks_like_lift_flush(&steady, 0.0, 119.0, 1.004));
        // Nor a big report after a pause: a new push.
        assert!(!looks_like_lift_flush(&steady, 0.0, 300.0, 1.1));
        // Too little to compare with.
        assert!(!looks_like_lift_flush(&steady[..2], 0.0, 300.0, 1.002));
        // Tiny reports: 4 units isn't many times 1.
        let slow = reports(20, 1.0, |_| 1.0);
        assert!(!looks_like_lift_flush(&slow, 0.0, 4.0, 1.002));
        assert!(looks_like_lift_flush(&slow, 0.0, 5.0, 1.002));
    }

    #[test]
    fn a_coast_starts_at_the_speed_on_screen() {
        // 30 units every 4 ms: 7500 a second.
        let steady = reports(40, 1.0, |_| 30.0);
        let start = coast_start(&steady, None, 1.004, &TUNING);
        assert!(start.starts);
        assert!((start.shown_y - 7500.0).abs() < 1.0, "{}", start.shown_y);
        assert_eq!(start.fingers_y, start.shown_y);
        assert_eq!((start.shown_x, start.fingers_x), (0.0, 0.0));
    }

    #[test]
    fn the_speed_on_screen_is_the_latest_not_the_average() {
        // Slowing from 60 units a report to 20 over the last 40 reports.
        let slowing = reports(40, 1.0, |back| 20.0 + back as f64);
        let start = coast_start(&slowing, None, 1.004, &TUNING);
        let latest = 20.0 / 0.004;
        let average = 39.5 / 0.004;
        assert!(
            start.shown_y > latest && start.shown_y < latest * 1.4,
            "{}",
            start.shown_y
        );
        assert!(start.shown_y < average * 0.75);
    }

    #[test]
    fn nothing_coasts_once_the_fingers_have_stopped_or_from_a_crawl() {
        let steady = reports(40, 1.0, |_| 30.0);
        // Lifted 80 ms after the last movement.
        assert!(!coast_start(&steady, None, 1.08, &TUNING).starts);
        // 0.5 units every 4 ms: 125 a second, about 60 px/s.
        let crawl = reports(40, 1.0, |_| 0.5);
        assert!(!coast_start(&crawl, None, 1.004, &TUNING).starts);
        assert!(!coast_start(&[], None, 1.0, &TUNING).starts);
        assert!(!coast_start(&steady[..1], None, 1.0, &TUNING).starts);
    }

    #[test]
    fn a_held_report_tells_the_fingers_speed_within_limits() {
        let steady = reports(40, 1.0, |_| 30.0);
        let fingers = |held: f64| coast_start(&steady, Some((0.0, held)), 1.002, &TUNING).fingers_y;
        // 45 ms of movement at the speed on screen: the same speed.
        assert!((fingers(7500.0 * 0.045) - 7500.0).abs() < 1.0);
        // The fingers were slowing.
        assert!((fingers(150.0) - 150.0 / 0.045).abs() < 1.0);
        // At most twice the speed on screen, and never against it.
        assert!((fingers(5000.0) - 15000.0).abs() < 2.0);
        assert_eq!(fingers(-200.0), 0.0);
        // Its size ignored.
        let ignored = Tuning {
            lift_lag: 0.0,
            ..TUNING
        };
        let start = coast_start(&steady, Some((0.0, 5000.0)), 1.002, &ignored);
        assert_eq!(start.fingers_y, start.shown_y);
    }

    #[test]
    fn the_fingers_having_all_but_stopped_still_ends_the_page_smoothly() {
        let steady = reports(40, 1.0, |_| 30.0);
        // Hardly anything held: the fingers braked at the very end.
        let start = coast_start(&steady, Some((0.0, 2.0)), 1.002, &TUNING);
        assert!(start.starts, "the page was moving");
        let mut state = coast_state(&start);
        let mut travelled = 0;
        let mut steps = 0;
        loop {
            let step = coast_step(&mut state, 1.0 / 120.0, &TUNING);
            travelled += step.dy;
            steps += 1;
            if step.done {
                break;
            }
            assert!(steps < 200);
        }
        // About the speed on screen fading over the blend time: 7500 * 0.04,
        // not a coast's 7500 * 0.325.
        assert!((250..400).contains(&travelled), "{travelled}");
        assert!(steps < 40, "{steps}");
    }

    #[test]
    fn a_coast_covers_its_speed_times_the_fall_off_time_in_whole_steps() {
        let steady = reports(40, 1.0, |_| 30.0);
        let start = coast_start(&steady, None, 1.004, &TUNING);
        let mut state = coast_state(&start);
        let (mut travelled, mut steps, mut first, mut previous) = (0, 0, 0, i32::MAX);
        loop {
            let step = coast_step(&mut state, 1.0 / 120.0, &TUNING);
            assert_eq!(step.dx, 0);
            // Slowing all the way, give or take the fraction carried over.
            assert!(
                step.dy <= previous.saturating_add(1),
                "{} after {previous}",
                step.dy
            );
            if steps == 0 {
                first = step.dy;
            }
            previous = step.dy;
            travelled += step.dy;
            steps += 1;
            if step.done {
                break;
            }
            assert!(steps < 2000);
        }
        // The first frame carries on at the speed on screen: 7500 / 120.
        assert!((60..=63).contains(&first), "{first}");
        let whole = 7500.0 * 0.325;
        assert!(
            (travelled as f64 - whole).abs() < whole * 0.02,
            "{travelled}"
        );
        // To 40 units a second: ln(7500 / 40) fall-off times.
        let seconds = steps as f64 / 120.0;
        assert!(
            (seconds - 0.325 * (7500.0_f64 / 40.0).ln()).abs() < 0.05,
            "{seconds}"
        );
    }

    #[test]
    fn a_late_step_covers_no_more_than_a_twentieth_of_a_second() {
        let start = CoastStart {
            starts: true,
            shown_x: 0.0,
            shown_y: 7500.0,
            fingers_x: 0.0,
            fingers_y: 7500.0,
        };
        let mut late = coast_state(&start);
        let mut capped = coast_state(&start);
        assert_eq!(
            coast_step(&mut late, 30.0, &TUNING).dy,
            coast_step(&mut capped, 0.05, &TUNING).dy
        );
        assert!(!coast_step(&mut late, 0.0, &TUNING).done);
    }
}
