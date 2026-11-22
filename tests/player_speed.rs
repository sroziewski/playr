//! Playback speed on `playr::player::Shared`: the clamp, and the wall clock (the clock of a
//! picture without sound) running at the chosen speed. Nothing is opened, so no audio device
//! is used; the clock is started the way the pipeline starts it, by un-pausing.

use std::thread::sleep;
use std::time::{Duration, Instant};

use playr::player::{MAX_SPEED, MIN_SPEED, Player};

fn player() -> Player {
    Player::new(Box::new(|| {}))
}

/// The position, with instants taken just before and just after reading it.
fn sample(p: &Player) -> (Instant, f64, Instant) {
    let before = Instant::now();
    let position = p.shared.position();
    (before, position, Instant::now())
}

/// Asserts that the clock moved from `a` to `b` at `speed`: no faster than `speed` times the
/// longest interval the two readings allow, no slower than `speed` times the shortest less
/// 5 ms (the clock may start a moment after `a` was taken, when un-pausing follows it).
fn assert_rate(a: (Instant, f64, Instant), b: (Instant, f64, Instant), speed: f64, what: &str) {
    let longest = (b.2 - a.0).as_secs_f64();
    let shortest = (b.0 - a.2).as_secs_f64();
    let moved = b.1 - a.1;
    assert!(
        moved <= speed * longest + 1e-9 && moved >= speed * (shortest - 0.005),
        "{what}: moved {moved:.4} s in {shortest:.4}..{longest:.4} s of wall time, expected speed {speed}"
    );
}

#[test]
fn speed_is_clamped_to_a_quarter_through_double_and_nonsense_is_normal_speed() {
    assert_eq!((MIN_SPEED, MAX_SPEED), (0.25, 2.0));
    let cases = [
        (1.5, 1.5),
        (0.25, 0.25),
        (2.0, 2.0),
        (0.1, 0.25),
        (0.0, 0.25),
        (-1.0, 0.25),
        (3.0, 2.0),
        (f64::NAN, 1.0),
        (f64::INFINITY, 1.0),
        (f64::NEG_INFINITY, 1.0),
    ];
    let p = player();
    assert_eq!(p.shared.speed(), 1.0, "default");
    let mut bad = Vec::new();
    for (set, expected) in cases {
        p.set_speed(set);
        let (clock, audio) = (p.shared.speed(), p.shared.audio.speed());
        if (clock, audio) != (expected, expected) {
            bad.push(format!("set_speed({set}): clock {clock}, audio {audio}, expected {expected}"));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

#[test]
fn the_wall_clock_runs_at_the_speed() {
    for speed in [2.0, 0.5] {
        let p = player();
        p.set_speed(speed);
        p.shared.set_paused(true);
        let start = (Instant::now(), 0.0, Instant::now());

        p.shared.set_paused(false);
        sleep(Duration::from_millis(300));
        let later = sample(&p);

        assert_rate(start, later, speed, &format!("speed {speed}"));
    }
}

#[test]
fn changing_speed_while_playing_keeps_the_position_and_changes_the_rate() {
    let p = player();
    p.shared.set_paused(true);
    p.shared.set_paused(false);
    sleep(Duration::from_millis(200));

    let before = sample(&p);
    p.set_speed(0.5);
    let after = sample(&p);
    sleep(Duration::from_millis(300));
    let later = sample(&p);

    assert!(before.1 >= 0.15, "the clock ran at normal speed first: {}", before.1);
    assert_rate(before, after, 1.0, "across the change: no jump");
    assert_rate(after, later, 0.5, "after the change");
}

#[test]
fn pausing_holds_the_position_and_a_speed_set_while_paused_applies_on_resume() {
    let p = player();
    p.set_speed(2.0);
    p.shared.set_paused(true);
    p.shared.set_paused(false);
    sleep(Duration::from_millis(150));

    p.shared.set_paused(true);
    let held = p.shared.position();
    sleep(Duration::from_millis(150));
    p.set_speed(1.5);
    let still = sample(&p);
    p.shared.set_paused(false);
    sleep(Duration::from_millis(200));
    let resumed = sample(&p);

    assert!(held >= 0.25, "ran at double speed before the pause: {held}");
    assert_eq!(still.1, held, "paused, and a speed change while paused does not move the clock");
    assert_rate(still, resumed, 1.5, "after resuming at the new speed");
}
