//! The bitrate meter: `playr::bitrate` and `Shared::bitrate()` while a fixture plays.
//!
//! Expected rates in the player tests are worked out from the fixture's own packets (read with
//! the same demuxer, `common::demux_file`): bytes with a timestamp in [position − 5 s, position)
//! times 8, over the part of those five seconds the packets cover.

mod common;

use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use common::{demux_file, finish, fixture};
use playr::bitrate::{Display, EASE_SECS, KEEP, MAX_RECORDS, MIN_SPAN, Meter, Rate, SAMPLE_EVERY, WINDOW, ease, format};
use playr::player::{Player, State, Status};

#[test]
fn the_constants_are_the_agreed_ones() {
    assert_eq!((WINDOW, MIN_SPAN, KEEP, MAX_RECORDS, EASE_SECS), (5.0, 0.5, 90.0, 20_000, 0.6));
}

/// Ten seconds of a steady stream: a 1000-byte picture every 0.1 s from 0, a 200-byte sound
/// packet every 0.1 s from 0.05.
fn steady() -> Meter {
    let mut m = Meter::default();
    for i in 0..100 {
        m.record(i as f64 / 10.0, 1000, true);
        m.record(i as f64 / 10.0 + 0.05, 200, false);
    }
    m
}

fn near(a: Option<Rate>, b: Option<Rate>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => (a.total - b.total).abs() < 1e-6 && (a.video - b.video).abs() < 1e-6 && (a.audio - b.audio).abs() < 1e-6,
        (None, None) => true,
        _ => false,
    }
}

#[test]
fn the_rate_is_the_bytes_of_the_last_five_seconds_of_media_before_the_position() {
    // [5, 10): pictures at 5.0 .. 9.9 (50 x 1000 bytes), sound at 5.05 .. 9.95 (50 x 200 bytes),
    // over 5 s: 80 000 and 16 000 bits per second.
    let full = Some(Rate { total: 96_000.0, video: 80_000.0, audio: 16_000.0 });
    // [2.5, 7.5): pictures 2.5 .. 7.4, sound 2.55 .. 7.45 - the same counts.
    let middle = full;
    let cases = [
        ("a full window", steady(), 10.0, full),
        ("a window in the middle; later packets are not counted", steady(), 7.5, middle),
        ("a position that is not a number", steady(), f64::NAN, None),
        ("an infinite position", steady(), f64::INFINITY, None),
        ("nothing recorded", Meter::default(), 3.0, None),
    ];
    let mut bad = Vec::new();
    for (case, meter, position, expected) in cases {
        let actual = meter.rate(position);
        expect!(bad, near(actual, expected), "{case}: {actual:?}, expected {expected:?}");
    }
    finish(bad);
}

#[test]
fn just_after_the_first_packet_the_rate_covers_only_what_is_there_and_needs_half_a_second() {
    // Pictures of 1000 bytes every 0.1 s from 2.0 (as after a seek to 2 s).
    let mut m = Meter::default();
    for i in 0..10 {
        m.record(2.0 + i as f64 / 10.0, 1000, true);
    }
    // At 2.5: the span is 2.5 - max(-2.5, 2.0) = 0.5; packets 2.0 .. 2.4 hold 5000 bytes, so
    // 5000 * 8 / 0.5 = 80 000.
    let cases = [
        ("0.4 s covered", 2.4, None),
        ("exactly 0.5 s covered", 2.5, Some(Rate { total: 80_000.0, video: 80_000.0, audio: 0.0 })),
        ("before the first packet", 1.0, None),
    ];
    let mut bad = Vec::new();
    for (case, position, expected) in cases {
        let actual = m.rate(position);
        expect!(bad, near(actual, expected), "{case}: {actual:?}, expected {expected:?}");
    }
    finish(bad);
}

#[test]
fn a_packet_exactly_at_the_position_is_not_counted_and_one_exactly_five_seconds_back_is() {
    let mut m = Meter::default();
    m.record(0.0, 999, true);
    m.record(1.0, 1000, true);
    m.record(6.0, 4000, false);

    let rate = m.rate(6.0);

    // [1, 6): only the 1000 bytes at 1.0, over 5 s.
    assert!(near(rate, Some(Rate { total: 1600.0, video: 1600.0, audio: 0.0 })), "{rate:?}");
}

#[test]
fn timestamps_that_are_not_numbers_are_not_recorded() {
    let mut m = Meter::default();

    m.record(f64::NAN, 1000, true);
    m.record(f64::INFINITY, 1000, false);
    m.record(f64::NEG_INFINITY, 1000, true);
    m.record(1.5, 1000, true);

    assert_eq!((m.len(), m.is_empty()), (1, false));
}

#[test]
fn records_more_than_ninety_seconds_behind_the_newest_are_dropped() {
    let mut m = Meter::default();
    for pts in [0.0, 10.0, 10.5, 50.0, 100.5] {
        m.record(pts, 100, true);
    }

    // 100.5 - 90 = 10.5: the records at 0 and 10 go, the one at 10.5 stays.
    assert_eq!(m.len(), 3);
    let from_10_5 = m.rate(15.5);
    assert!(near(from_10_5, Some(Rate { total: 160.0, video: 160.0, audio: 0.0 })), "{from_10_5:?}");
}

#[test]
fn at_most_twenty_thousand_records_are_kept_and_the_oldest_go_first() {
    let mut m = Meter::default();
    for i in 0..(MAX_RECORDS + 5) {
        m.record(i as f64 / 1000.0, 10, true);
    }

    assert_eq!(m.len(), MAX_RECORDS);
    // The oldest left is record 5 (0.005 s): at 0.505 the span is 0.5 and records 5 .. 504 hold
    // 500 x 10 bytes, 40 000 bits over 0.5 s (the tolerance covers the decimal timestamps).
    let rate = m.rate(0.505);
    assert!(rate.is_some_and(|r| (r.total - 80_000.0).abs() < 1.0), "{rate:?}");
}

#[test]
fn clear_forgets_everything() {
    let mut m = steady();

    m.clear();

    assert_eq!((m.len(), m.is_empty(), m.rate(10.0)), (0, true, None));
}

#[test]
fn twenty_thousand_records_are_measured_quickly() {
    let mut m = Meter::default();
    for i in 0..MAX_RECORDS {
        m.record(i as f64 / 2000.0, 1500, i % 3 == 0);
    }
    let mut times: Vec<Duration> = (0..50)
        .map(|n| {
            let started = Instant::now();
            std::hint::black_box(m.rate(std::hint::black_box(5.0 + n as f64 * 0.01)));
            started.elapsed()
        })
        .collect();
    times.sort();

    // This crate is built unoptimised in tests, so the budget is far above the release target
    // of 1 ms; it catches a pass that went quadratic, not a few percent.
    assert!(times[25] < Duration::from_millis(5), "median {:?}", times[25]);
}

#[test]
fn rates_are_written_in_kilobits_or_megabits() {
    let cases = [
        (0.0, "0 kb/s"),
        (-5.0, "0 kb/s"),
        (f64::NAN, "0 kb/s"),
        (f64::INFINITY, "0 kb/s"),
        (1_499.0, "1 kb/s"),
        (128_000.0, "128 kb/s"),
        (999_400.0, "999 kb/s"),
        (999_499.0, "999 kb/s"),
        (999_500.0, "1.00 Mb/s"),
        (999_999.0, "1.00 Mb/s"),
        (1_000_000.0, "1.00 Mb/s"),
        (2_414_000.0, "2.41 Mb/s"),
        (12_345_678.0, "12.35 Mb/s"),
    ];
    let mut bad = Vec::new();
    for (bps, expected) in cases {
        expect!(bad, format(bps) == expected, "format({bps}) = {:?}, expected {expected:?}", format(bps));
    }
    finish(bad);
}

#[test]
fn the_shown_figure_eases_toward_the_measured_one() {
    let one_step = 100.0 * (1.0 - (-1.0f64).exp());
    let cases = [
        ("one time constant from 0 to 100", 0.0, 100.0, 0.6, one_step),
        ("downwards", 100.0, 0.0, 0.6, 100.0 - one_step),
        ("already there", 50.0, 50.0, 0.25, 50.0),
        ("a long pause gets there", 0.0, 100.0, 60.0, 100.0),
        ("no time passed", 10.0, 100.0, 0.0, 100.0),
        ("time going backwards", 10.0, 100.0, -0.25, 100.0),
        ("nothing shown yet", f64::NAN, 100.0, 0.25, 100.0),
        ("an infinite figure shown", f64::INFINITY, 100.0, 0.25, 100.0),
    ];
    let mut bad = Vec::new();
    for (case, shown, target, dt, expected) in cases {
        let actual = ease(shown, target, dt);
        expect!(bad, (actual - expected).abs() < 1e-9, "{case}: ease({shown}, {target}, {dt}) = {actual}, expected {expected}");
    }
    finish(bad);
}

static AUDIO_DEVICE: Mutex<()> = Mutex::new(());

fn player() -> (Player, MutexGuard<'static, ()>) {
    let device = AUDIO_DEVICE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let player = Player::new(Box::new(|| {}));
    player.shared.audio.set_volume(0.0);
    (player, device)
}

fn wait_for(player: &Player, what: &str, mut done: impl FnMut(&Status) -> bool) -> Status {
    let started = Instant::now();
    loop {
        let status = player.shared.status();
        if done(&status) {
            return status;
        }
        assert!(started.elapsed() < Duration::from_secs(30), "timed out waiting for {what}: {status:?}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// The rate the fixture's own packets give at `position`, by the meter's rule.
fn expected_rate(name: &str, position: f64) -> Rate {
    let d = demux_file(name);
    let video: Vec<(f64, usize)> = d.video.iter().map(|(pts, data)| (*pts, data.len())).collect();
    let audio: Vec<(f64, usize)> = d.audio.iter().map(|(pts, p)| (*pts, p.data.len())).collect();
    let earliest = video.iter().chain(&audio).map(|p| p.0).fold(f64::INFINITY, f64::min);
    let span = position - (position - 5.0).max(earliest);
    let sum = |list: &[(f64, usize)]| list.iter().filter(|(pts, _)| *pts >= position - 5.0 && *pts < position).map(|p| p.1).sum::<usize>() as f64;
    let (v, a) = (sum(&video) * 8.0 / span, sum(&audio) * 8.0 / span);
    Rate { total: v + a, video: v, audio: a }
}

/// Pauses and returns the position once it has stopped moving, with the rate there.
fn pause_and_measure(player: &Player) -> (f64, Option<Rate>) {
    player.shared.set_paused(true);
    let started = Instant::now();
    let mut last = player.shared.position();
    loop {
        std::thread::sleep(Duration::from_millis(30));
        let now = player.shared.position();
        if now == last {
            return (now, player.shared.bitrate());
        }
        assert!(started.elapsed() < Duration::from_secs(10), "the position kept moving after pausing: {now}");
        last = now;
    }
}

#[test]
fn while_a_file_plays_the_meter_reports_the_rate_of_its_packets_over_the_last_five_seconds() {
    let name = "h264_longgop_aac.mp4";
    let (player, _device) = player();
    player.set_speed(2.0);
    let path = fixture(name).to_str().unwrap().to_string();

    player.open(path.clone());
    wait_for(&player, "the file to play past 6 s", |s| s.state == State::Playing && player.shared.position() > 6.2);
    let (position, rate) = pause_and_measure(&player);
    player.stop();

    let expected = expected_rate(name, position);
    let close = |a: f64, b: f64| (a - b).abs() <= b * 0.001 + 1.0;
    assert!(
        rate.is_some_and(|r| close(r.total, expected.total) && close(r.video, expected.video) && close(r.audio, expected.audio)),
        "at {position:.3}: {rate:?}, expected {expected:?}"
    );
    let file_bits = std::fs::metadata(fixture(name)).unwrap().len() as f64 * 8.0;
    assert!(expected.total > 0.5 * file_bits / 10.0 && expected.total < 1.5 * file_bits / 10.0, "{expected:?} against {file_bits} bits in 10 s");
}

#[test]
fn just_after_opening_and_after_each_seek_there_is_no_figure_until_half_a_second_is_covered() {
    // Every picture is a keyframe, so a seek starts reading at the target itself.
    let name = "h264_intra_aac.mp4";
    let (player, _device) = player();
    let path = fixture(name).to_str().unwrap().to_string();

    player.open(path.clone());
    wait_for(&player, "the file to play", |s| s.source == path && s.state == State::Playing);
    let (opened_at, at_open) = pause_and_measure(&player);
    player.shared.set_paused(false);
    wait_for(&player, "1.5 s of playing", |_| player.shared.position() > 1.5);
    let (played_to, while_playing) = pause_and_measure(&player);
    player.seek(3.0);
    wait_for(&player, "the seek to 3 s", |_| (player.shared.position() - 3.0).abs() < 1e-6);
    let after_forward_seek = player.shared.bitrate();
    player.seek(0.8);
    wait_for(&player, "the seek back to 0.8 s", |_| (player.shared.position() - 0.8).abs() < 1e-6);
    let after_backward_seek = player.shared.bitrate();
    player.stop();

    assert!(opened_at < MIN_SPAN, "paused only at {opened_at}: the check below would prove nothing");
    assert_eq!(at_open, None, "at {opened_at:.3} right after opening");
    let expected = expected_rate(name, played_to);
    assert!(while_playing.is_some_and(|r| (r.total - expected.total).abs() <= expected.total * 0.001 + 1.0), "at {played_to:.3}: {while_playing:?}, expected {expected:?}");
    assert_eq!(after_forward_seek, None, "after the seek forward to 3 s");
    assert_eq!(after_backward_seek, None, "after the seek back to 0.8 s the packets read before it still counted");
}

#[test]
fn opening_another_file_starts_the_meter_afresh() {
    let (first, second) = ("h264_longgop_aac.mp4", "h264_intra_aac.mp4");
    let (player, _device) = player();
    player.set_speed(2.0);
    let (first_path, second_path) = (fixture(first).to_str().unwrap().to_string(), fixture(second).to_str().unwrap().to_string());

    player.open(first_path.clone());
    wait_for(&player, "the first file to play past 6 s", |s| s.source == first_path && s.state == State::Playing && player.shared.position() > 6.0);
    let before_switch = player.shared.bitrate();
    player.open(second_path.clone());
    wait_for(&player, "the second file to play", |s| s.source == second_path && s.state == State::Playing);
    let (opened_at, at_open) = pause_and_measure(&player);
    player.shared.set_paused(false);
    wait_for(&player, "1.5 s of the second file", |_| player.shared.position() > 1.5);
    let (played_to, later) = pause_and_measure(&player);
    player.stop();

    assert!(before_switch.is_some(), "the first file had no rate to carry over");
    assert!(opened_at < MIN_SPAN, "paused only at {opened_at}: the check below would prove nothing");
    assert_eq!(at_open, None, "at {opened_at:.3} right after opening the second file");
    let expected = expected_rate(second, played_to);
    assert!(
        later.is_some_and(|r| (r.total - expected.total).abs() <= expected.total * 0.001 + 1.0 && (r.video - expected.video).abs() <= expected.video * 0.001 + 1.0),
        "at {played_to:.3}: {later:?}, expected the second file's own {expected:?}"
    );
}

fn rate(total: f64) -> Rate {
    Rate { total, video: total * 0.75, audio: total * 0.25 }
}

/// A sample source that counts how often it was asked.
fn sampler(calls: &std::cell::Cell<u32>, answer: Option<Rate>) -> impl FnOnce() -> Option<Rate> + '_ {
    move || {
        calls.set(calls.get() + 1);
        answer
    }
}

#[test]
fn the_label_samples_every_250_ms_eases_toward_the_sample_and_holds_while_paused() {
    assert_eq!(SAMPLE_EVERY, Duration::from_millis(250));
    let ms = |n: u64| Duration::from_millis(n);
    let t0 = Instant::now();
    let asked = std::cell::Cell::new(0);
    let eased = |from: f64, to: f64, dt_ms: f64| from + (to - from) * (1.0 - (-dt_ms / 1000.0 / 0.6).exp());

    let first = Display::step(None, t0, false, sampler(&asked, Some(rate(1000.0)))).expect("a first sample");
    let asked_first = asked.replace(0);
    let early = Display::step(Some(first), t0 + ms(100), false, sampler(&asked, Some(rate(9000.0)))).expect("kept");
    let still_early = Display::step(Some(early), t0 + ms(249), false, sampler(&asked, Some(rate(9000.0)))).expect("kept");
    let asked_early = asked.replace(0);
    let due = Display::step(Some(still_early), t0 + ms(250), false, sampler(&asked, Some(rate(2000.0)))).expect("sampled");
    let moving = Display::step(Some(due), t0 + ms(400), false, sampler(&asked, None)).expect("not due yet");
    let asked_due = asked.replace(0);
    let paused = Display::step(Some(moving), t0 + ms(900), true, sampler(&asked, Some(rate(7000.0)))).expect("held");
    let asked_paused = asked.replace(0);
    let resumed = Display::step(Some(paused), t0 + ms(950), false, sampler(&asked, Some(rate(2000.0)))).expect("sampled again");
    let settled = Display::step(Some(resumed), t0 + ms(950) + Duration::from_secs(60), false, sampler(&asked, Some(rate(2000.0)))).expect("sampled");
    let asked_resumed = asked.replace(0);
    let cleared = Display::step(Some(settled), t0 + ms(950) + Duration::from_secs(61), false, sampler(&asked, None));
    let nothing_while_paused = Display::step(None, t0, true, sampler(&asked, Some(rate(1.0))));
    let asked_at_the_end = asked.get();

    let shown = [first.shown, early.shown, still_early.shown, due.shown, moving.shown, paused.shown, resumed.shown];
    let expected_shown = [
        1000.0,
        1000.0,
        1000.0,
        eased(1000.0, 2000.0, 1.0),
        eased(eased(1000.0, 2000.0, 1.0), 2000.0, 150.0),
        eased(eased(1000.0, 2000.0, 1.0), 2000.0, 150.0),
        eased(eased(eased(1000.0, 2000.0, 1.0), 2000.0, 150.0), 2000.0, 50.0),
    ];
    let mut bad = Vec::new();
    for (n, (actual, expected)) in shown.iter().zip(expected_shown).enumerate() {
        expect!(bad, (actual - expected).abs() < 1e-9, "step {n}: shown {actual}, expected {expected}");
    }
    expect!(bad, (first.rate, due.rate, paused.rate) == (rate(1000.0), rate(2000.0), rate(2000.0)), "rates {:?}", (first.rate, due.rate, paused.rate));
    expect!(bad, (asked_first, asked_early, asked_due, asked_paused, asked_resumed, asked_at_the_end) == (1, 0, 1, 0, 2, 1), "samples asked for: {:?}", (asked_first, asked_early, asked_due, asked_paused, asked_resumed, asked_at_the_end));
    expect!(bad, (settled.shown - 2000.0).abs() < 1e-6, "after a minute: {}", settled.shown);
    expect!(bad, cleared.is_none(), "a sample with no figure must clear the label: {cleared:?}");
    expect!(bad, nothing_while_paused.is_none(), "{nothing_while_paused:?}");
    expect!(bad, (first.settling(), due.settling(), settled.settling()) == (false, true, false), "settling {:?}", (first.settling(), due.settling(), settled.settling()));
    finish(bad);
}

#[test]
fn the_label_counts_as_settled_within_half_a_percent() {
    let t0 = Instant::now();
    let at = |shown_from: f64, to: f64| {
        let s = Display::step(None, t0, false, || Some(rate(shown_from))).unwrap();
        Display::step(Some(s), t0 + SAMPLE_EVERY, false, || Some(rate(to))).unwrap()
    };
    // From 1000 toward 2000 over 0.25 s the figure covers 1 - e^(-0.25/0.6) of the way (34 %):
    // far from settled. From 1990 toward 2000 it starts 0.5 % away and moves closer: settled.
    let far = at(1000.0, 2000.0);
    let near = at(1990.0, 2000.0);
    let zero = at(0.0, 0.0);

    assert_eq!((far.settling(), near.settling(), zero.settling()), (true, false, false), "{far:?} {near:?} {zero:?}");
}
