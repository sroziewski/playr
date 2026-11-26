//! `playr::audio_out::AudioShared::heard_secs`: the clock of what has reached the speaker, as
//! opposed to `played_secs`, what the device callback has taken.
//!
//! Every test drives `render_at` with fixed instants and latencies. At 1000 Hz one frame is
//! one millisecond, so the expected values below are frame counts read off a time line:
//! a buffer of `n` frames rendered at `t` with latency `L` reaches the speaker at `t + L` and
//! has played out at `t + L + n ms`. The fixtures keep the latency steady, as a device does,
//! so before `t + L` the speaker is still playing the buffers before it, one media frame per
//! millisecond at speed 1.

use std::time::{Duration, Instant};

use playr::audio_out::AudioShared;

const RATE: u32 = 1000;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn shared_1khz_stereo() -> AudioShared {
    let shared = AudioShared::default();
    shared.set_format(RATE, 2);
    shared
}

/// `frames` stereo frames with distinct samples.
fn ramp(frames: usize) -> Vec<f32> {
    (0..frames).flat_map(|i| [i as f32 * 0.001, -(i as f32) * 0.001]).collect()
}

fn render(shared: &AudioShared, frames: usize, at: Instant, latency: Duration) {
    let mut out = vec![9.0f32; 2 * frames];
    shared.render_at(&mut out, at, latency);
}

/// One frame of sound is 1 ms; anything closer than a nanosecond is the same instant.
fn close(actual: f64, expected: f64) -> bool {
    (actual - expected).abs() <= 1e-9
}

/// Checks `heard_secs_at(t0 + offset)` for every `(label, offset ms, expected secs)` row and
/// reports all mismatches at once.
fn check(shared: &AudioShared, t0: Instant, rows: &[(&str, u64, f64)]) {
    let bad: Vec<String> = rows
        .iter()
        .filter_map(|&(label, offset, expected)| {
            let actual = shared.heard_secs_at(t0 + ms(offset));
            (!close(actual, expected)).then(|| format!("{label} (t0 + {offset} ms): heard {actual}, expected {expected}"))
        })
        .collect();
    assert!(bad.is_empty(), "{} row(s) wrong:\n{}", bad.len(), bad.join("\n"));
}

/// `count` callbacks of `frames` frames each, every `frames` ms from `t0 + from` ms, all with
/// the same `latency`.
fn steady(shared: &AudioShared, t0: Instant, from: u64, count: u64, frames: u64, latency: Duration) {
    for k in 0..count {
        render(shared, frames as usize, t0 + ms(from + k * frames), latency);
    }
}

/// Eleven 10-frame buffers every 10 ms from `t0`, 33 ms latency: media frame n is heard at
/// t0 + n + 33 ms, so heard(t) = t - t0 - 33 ms, up to 110 frames at t0 + 143 ms. The last
/// callback is at t0 + 100 ms; `played_secs` is 0.110 from then on.
fn played_110_with_33ms_latency() -> (AudioShared, Instant) {
    let shared = shared_1khz_stereo();
    shared.push(&ramp(200));
    let t0 = Instant::now();
    steady(&shared, t0, 0, 11, 10, ms(33));
    (shared, t0)
}

mod before_any_callback {
    use super::*;

    #[test]
    fn the_heard_clock_is_the_played_clock() {
        let shared = shared_1khz_stereo();
        shared.push(&ramp(50));
        let t0 = Instant::now();

        assert_eq!(shared.played_secs(), 0.0);
        check(&shared, t0, &[("now", 0, 0.0), ("a second later", 1000, 0.0)]);
    }
}

mod one_buffer {
    use super::*;

    #[test]
    fn lags_played_by_the_latency_plus_the_buffer_and_runs_in_real_time_up_to_its_end() {
        let (shared, t0) = played_110_with_33ms_latency();

        assert_eq!(shared.played_secs(), 0.110, "played_secs still counts what the callback took");
        check(
            &shared,
            t0,
            &[
                // 110 played - 33 latency - 10 still in the buffer.
                ("at the callback", 100, 0.067),
                ("halfway through the latency", 116, 0.083),
                ("the buffer reaches the speaker", 133, 0.100),
                ("2 ms into the buffer", 135, 0.102),
                ("7 ms into the buffer", 140, 0.107),
                ("the buffer has played out", 143, 0.110),
                ("long after, no further callback", 400, 0.110),
            ],
        );
    }

    #[test]
    fn advances_continuously_between_instants_not_in_whole_frames() {
        let (shared, t0) = played_110_with_33ms_latency();

        let a = shared.heard_secs_at(t0 + ms(135) + Duration::from_micros(250));
        let b = shared.heard_secs_at(t0 + ms(135) + Duration::from_micros(750));

        assert!(close(a, 0.10225), "heard {a} at 135.25 ms, expected 0.10225");
        assert!(close(b, 0.10275), "heard {b} at 135.75 ms, expected 0.10275");
    }

    #[test]
    fn nothing_is_heard_until_the_first_buffer_reaches_the_speaker() {
        let shared = shared_1khz_stereo();
        shared.push(&ramp(10));
        let t0 = Instant::now();
        render(&shared, 10, t0, ms(33));

        check(
            &shared,
            t0,
            &[
                ("at the callback", 0, 0.0),
                ("3 ms before the buffer reaches the speaker", 30, 0.0),
                ("5 ms into the buffer", 38, 0.005),
                ("played out", 43, 0.010),
            ],
        );
    }

    #[test]
    fn with_no_latency_the_buffer_is_heard_from_its_callback_on() {
        let shared = shared_1khz_stereo();
        shared.push(&ramp(200));
        let t0 = Instant::now();
        render(&shared, 100, t0, Duration::ZERO);
        render(&shared, 10, t0 + ms(100), Duration::ZERO);

        check(
            &shared,
            t0,
            &[
                ("at the callback", 100, 0.100),
                ("5 ms in", 105, 0.105),
                ("played out", 110, 0.110),
                ("long after", 150, 0.110),
            ],
        );
    }

    #[test]
    fn when_the_latency_grows_the_clock_holds_at_what_was_played_until_the_new_buffer_arrives() {
        // Ten 10-frame buffers every 10 ms with no latency: all 100 frames heard by t0 + 100.
        // The next buffer, at t0 + 100 ms, comes with 33 ms latency: silence from t0 + 100 to
        // t0 + 133 ms, then its 10 frames.
        let shared = shared_1khz_stereo();
        shared.push(&ramp(200));
        let t0 = Instant::now();
        steady(&shared, t0, 0, 10, 10, Duration::ZERO);
        render(&shared, 10, t0 + ms(100), ms(33));

        check(
            &shared,
            t0,
            &[
                ("at the callback", 100, 0.100),
                ("in the gap", 120, 0.100),
                ("the new buffer reaches the speaker", 133, 0.100),
                ("5 ms into it", 138, 0.105),
                ("played out", 143, 0.110),
            ],
        );
    }
}

mod latency_reports {
    use super::*;

    #[test]
    fn up_to_2_s_counts_and_anything_longer_is_taken_as_none() {
        // Five 100-frame buffers every 100 ms from t0, all with the same reported latency L;
        // the effective latency E is L, or 0 when L is ignored. Frame n is heard at
        // t0 + n + E ms, up to 500 frames. (label, L, heard at t0 + 405 ms, at t0 + 2405 ms):
        // E = 33 ms: 372, then all 500; E = 2 s: none yet, then 405; E = 0: 405, then 500.
        let rows: [(&str, Duration, f64, f64); 4] = [
            ("33 ms", ms(33), 0.372, 0.500),
            ("exactly 2 s", ms(2000), 0.0, 0.405),
            ("2 s and 1 ms", ms(2001), 0.405, 0.500),
            ("3 s", ms(3000), 0.405, 0.500),
        ];
        let mut bad = Vec::new();
        for (label, latency, at_405ms, at_2405ms) in rows {
            let shared = shared_1khz_stereo();
            shared.push(&ramp(500));
            let t0 = Instant::now();
            steady(&shared, t0, 0, 5, 100, latency);
            for (offset, expected) in [(405, at_405ms), (2405, at_2405ms)] {
                let actual = shared.heard_secs_at(t0 + ms(offset));
                if !close(actual, expected) {
                    bad.push(format!("latency {label}, t0 + {offset} ms: heard {actual}, expected {expected}"));
                }
            }
        }
        assert!(bad.is_empty(), "{} row(s) wrong:\n{}", bad.len(), bad.join("\n"));
    }

    #[test]
    fn a_2_s_latency_with_10_ms_buffers_keeps_every_buffer_on_its_way_in_mind() {
        // 250 buffers of 10 frames every 10 ms, 2 s latency: about 200 are in flight at once.
        // Frame n is heard at t0 + n + 2000 ms; at t0 + 2495 ms that is 495 frames.
        let shared = shared_1khz_stereo();
        shared.push(&ramp(2500));
        let t0 = Instant::now();
        steady(&shared, t0, 0, 250, 10, ms(2000));

        check(&shared, t0, &[("half a buffer after the last callback", 2495, 0.495), ("all played out", 4500, 2.500)]);
    }
}

mod regular_callbacks {
    use super::*;

    #[test]
    fn the_clock_is_continuous_across_callbacks_and_trails_real_time_by_the_latency() {
        // 10-frame buffers every 10 ms from t0, 33 ms latency: the speaker plays media frame
        // n at t0 + n + 33 ms, so heard(t) = t - t0 - 33 ms (not below 0), across every
        // callback.
        let shared = shared_1khz_stereo();
        shared.push(&ramp(400));
        let t0 = Instant::now();
        let expected = |offset: u64| (offset as f64 - 33.0).max(0.0) / RATE as f64;
        let mut bad = Vec::new();
        for k in 0..30u64 {
            let t = 10 * k;
            let before = shared.heard_secs_at(t0 + ms(t));
            render(&shared, 10, t0 + ms(t), ms(33));
            let after = shared.heard_secs_at(t0 + ms(t));
            let mid = shared.heard_secs_at(t0 + ms(t + 5));
            // Before the first callback the clock is played_secs, which is 0.
            if !close(before, expected(t)) {
                bad.push(format!("callback {k}: just before it heard {before}, expected {}", expected(t)));
            }
            if !close(after, before) {
                bad.push(format!("callback {k}: the clock jumped from {before} to {after}"));
            }
            if !close(mid, expected(t + 5)) {
                bad.push(format!("callback {k}: 5 ms after it heard {mid}, expected {}", expected(t + 5)));
            }
        }
        assert!(bad.is_empty(), "{} mismatch(es):\n{}", bad.len(), bad.join("\n"));
        assert_eq!(shared.played_secs(), 0.300);
    }
}

mod uneven_callbacks {
    use super::*;

    #[test]
    fn with_jittery_callbacks_and_a_falling_latency_the_clock_never_goes_back_or_ahead_of_the_speaker() {
        // 10-frame buffers at uneven times (gaps of 12, 6, 13 and 9 ms over and over), the
        // reported latency falling by 1 ms per callback from 33 to 20 ms. From these the test
        // keeps its own list of when each buffer reaches the speaker. At any instant t:
        // - every buffer that has reached the speaker has had all media before it heard, so
        //   heard(t) >= the largest start among them;
        // - nothing of a buffer is heard before it arrives, nor more than its 10 frames, so
        //   heard(t) <= max over arrived buffers of start + min(t - arrival, 10).
        let gaps = [12u64, 6, 13, 9];
        let shared = shared_1khz_stereo();
        shared.push(&ramp(400));
        let t0 = Instant::now();
        let mut callbacks = Vec::new();
        let mut t = 0u64;
        for k in 0..36u64 {
            callbacks.push((t, 33u64.saturating_sub(k).max(20)));
            t += gaps[k as usize % gaps.len()];
        }
        let mut arrived: Vec<(u64, u64)> = Vec::new(); // (arrival ms, start frame)
        let mut next = 0;
        let mut previous = 0.0f64;
        let mut bad = Vec::new();
        for now in 0..=t + 60 {
            while next < callbacks.len() && callbacks[next].0 == now {
                let (at, latency) = callbacks[next];
                render(&shared, 10, t0 + ms(at), ms(latency));
                arrived.push((at + latency, 10 * next as u64));
                next += 1;
            }
            let heard = shared.heard_secs_at(t0 + ms(now)) * RATE as f64;
            let reached: Vec<&(u64, u64)> = arrived.iter().filter(|(arrival, _)| *arrival <= now).collect();
            let low = reached.iter().map(|(_, start)| *start as f64).fold(0.0, f64::max);
            let high = reached.iter().map(|(arrival, start)| (*start + (now - arrival).min(10)) as f64).fold(0.0, f64::max);
            if heard < previous - 1e-9 {
                bad.push(format!("t0 + {now} ms: went back from {previous} to {heard} frames"));
            }
            if heard < low - 1e-9 || heard > high + 1e-9 {
                bad.push(format!("t0 + {now} ms: heard {heard} frames, the speaker is between {low} and {high}"));
            }
            previous = heard;
        }
        assert_eq!(shared.played_secs(), 0.360, "precondition: all 36 buffers were full");
        assert!((previous - 360.0).abs() <= 1e-6, "the clock ends at the last frame, not at {previous}");
        assert!(bad.is_empty(), "{} mismatch(es):\n{}", bad.len(), bad.join("\n"));
    }
}

mod speed {
    use super::*;

    #[test]
    fn in_free_run_media_time_passes_at_the_speed() {
        // Free run counts out_frames * speed media frames per buffer, queued sound or not.
        // (label, speed, frames queued, heard at: callback, buffer start, 5 ms in, end, later)
        // Eleven 10-frame buffers every 10 ms, 33 ms latency, each holding 10 * speed media
        // frames: heard(t) = speed * (t - t0 - 33 ms), up to 110 * speed frames.
        let rows: [(&str, f64, usize, [f64; 5]); 3] = [
            ("speed 2", 2.0, 0, [0.134, 0.200, 0.210, 0.220, 0.220]),
            ("speed 0.5", 0.5, 0, [0.0335, 0.050, 0.0525, 0.055, 0.055]),
            ("speed 1 with sound queued", 1.0, 40, [0.067, 0.100, 0.105, 0.110, 0.110]),
        ];
        let mut bad = Vec::new();
        for (label, speed, queued, expected) in rows {
            let shared = shared_1khz_stereo();
            shared.set_speed(speed);
            shared.set_free_run(true);
            shared.push(&ramp(queued));
            let t0 = Instant::now();
            steady(&shared, t0, 0, 11, 10, ms(33));
            for (offset, want) in [100, 133, 138, 143, 300].into_iter().zip(expected) {
                let actual = shared.heard_secs_at(t0 + ms(offset));
                if !close(actual, want) {
                    bad.push(format!("{label}, t0 + {offset} ms: heard {actual}, expected {want}"));
                }
            }
        }
        assert!(bad.is_empty(), "{} row(s) wrong:\n{}", bad.len(), bad.join("\n"));
    }

    #[test]
    fn a_stretched_buffer_is_heard_at_the_rate_its_media_frames_were_taken() {
        // The stretcher decides how many media frames a buffer takes; played_secs before and
        // after the callback says how many. Their sound is spread over the whole buffer.
        let shared = AudioShared::default();
        shared.set_format(8000, 2);
        shared.set_speed(2.0);
        shared.push(&ramp(8000));
        let t0 = Instant::now();
        for k in 0..10u64 {
            render(&shared, 160, t0 + ms(20 * k), ms(25));
        }
        let before = shared.played_secs();
        // 160 output frames at 8 kHz are 20 ms of sound.
        render(&shared, 160, t0 + ms(200), ms(25));
        let after = shared.played_secs();
        let media = (after - before) * 8000.0;
        assert!(media >= 200.0, "precondition: at speed 2 the buffer takes far more than its 160 output frames, took {media}");

        let rows = [
            ("the buffer reaches the speaker", ms(225), before),
            ("halfway through it", ms(235), before + (after - before) / 2.0),
            ("played out", ms(245), after),
            ("later", ms(400), after),
        ];
        let bad: Vec<String> = rows
            .iter()
            .filter_map(|&(label, offset, want)| {
                let actual = shared.heard_secs_at(t0 + offset);
                (!close(actual, want)).then(|| format!("{label}: heard {actual}, expected {want}"))
            })
            .collect();
        assert!(bad.is_empty(), "{} row(s) wrong:\n{}", bad.len(), bad.join("\n"));
    }
}

mod short_buffers {
    use super::*;

    /// Eleven full 10-frame buffers every 10 ms (33 ms latency), then at `t0 + 110 ms` a
    /// buffer of 10 output frames that only `queued` media frames of sound could fill. The
    /// sound sits at the front of the buffer, silence after it: it reaches the speaker at
    /// t0 + 143 ms and has played out at t0 + 143 + queued ms.
    fn short_buffer(queued: usize) -> (AudioShared, Instant, f64) {
        let shared = shared_1khz_stereo();
        shared.push(&ramp(110));
        let t0 = Instant::now();
        steady(&shared, t0, 0, 11, 10, ms(33));
        // 110 - 33 = 77 frames heard when the short buffer is rendered.
        let before = shared.heard_secs_at(t0 + ms(110));
        shared.push(&ramp(queued));
        render(&shared, 10, t0 + ms(110), ms(33));
        (shared, t0, before)
    }

    #[test]
    fn a_partly_filled_buffer_ends_the_clock_at_its_last_media_frame() {
        let (shared, t0, before) = short_buffer(4);

        assert_eq!(before, 0.077);
        assert_eq!(shared.played_secs(), 0.114, "only the 4 frames of sound count as played");
        check(
            &shared,
            t0,
            &[
                ("the short buffer reaches the speaker", 143, 0.110),
                ("its silence has played out", 153, 0.114),
                ("long after", 300, 0.114),
            ],
        );
    }

    #[test]
    fn an_empty_buffer_holds_the_clock_where_the_sound_ran_out() {
        // No sound at all in the buffer that reaches the speaker at t0 + 143 ms: the clock
        // stays at the end of the full buffers before it.
        let (shared, t0, _) = short_buffer(0);

        assert_eq!(shared.played_secs(), 0.110);
        check(
            &shared,
            t0,
            &[
                ("the empty buffer reaches the speaker", 143, 0.110),
                ("in the middle of its silence", 150, 0.110),
                ("long after", 300, 0.110),
            ],
        );
    }

    #[test]
    fn a_short_or_empty_buffer_does_not_move_the_clock_at_its_callback() {
        // At the callback the speaker is still playing the full buffers before it (latency
        // 33 ms of them), so the clock reads 0.077 whatever the new buffer holds.
        let mut bad = Vec::new();
        for queued in [4, 0] {
            let (shared, t0, before) = short_buffer(queued);
            for (label, offset, want) in [("at the callback", 110, 0.077), ("10 ms later", 120, 0.087)] {
                let actual = shared.heard_secs_at(t0 + ms(offset));
                if !close(actual, want) {
                    bad.push(format!("{queued} of 10 frames filled, {label}: heard {actual}, expected {want} (before the callback {before})"));
                }
            }
        }
        assert!(bad.is_empty(), "{} row(s) wrong:\n{}", bad.len(), bad.join("\n"));
    }

    #[test]
    fn the_sound_of_a_partly_filled_buffer_is_heard_at_full_speed_then_the_clock_holds() {
        // 4 frames of sound at the front of the buffer: 1 media frame per ms from t0 + 143 ms.
        let (shared, t0, _) = short_buffer(4);

        check(
            &shared,
            t0,
            &[
                ("2 ms into the sound", 145, 0.112),
                ("3 ms into the sound", 146, 0.113),
                ("the sound has played out", 147, 0.114),
                ("in the silence", 150, 0.114),
            ],
        );
    }
}

mod pause_and_reset {
    use super::*;

    #[test]
    fn a_paused_callback_leaves_the_clock_on_the_last_buffer_which_then_holds() {
        let (shared, t0) = played_110_with_33ms_latency();
        shared.set_paused(true);
        render(&shared, 10, t0 + ms(117), ms(33));

        assert_eq!(shared.played_secs(), 0.110);
        check(
            &shared,
            t0,
            &[
                // The paused callback's silence changes nothing: the last buffer still plays.
                ("at the paused callback", 117, 0.084),
                ("the last buffer has played out", 143, 0.110),
                ("a second later, still paused", 1143, 0.110),
            ],
        );
    }

    #[test]
    fn after_resuming_the_clock_waits_for_the_new_buffer_instead_of_going_back() {
        // Paused at t0 + 117 ms; the last sound played out at t0 + 143 ms. Resumed at
        // t0 + 1000 ms: the first new buffer reaches the speaker at t0 + 1033 ms, and until
        // then the speaker is silent, so nothing more is heard than 0.110.
        let (shared, t0) = played_110_with_33ms_latency();
        shared.set_paused(true);
        render(&shared, 10, t0 + ms(117), ms(33));
        shared.set_paused(false);
        render(&shared, 10, t0 + ms(1000), ms(33));

        check(
            &shared,
            t0,
            &[
                ("at the resumed callback", 1000, 0.110),
                ("before the new buffer is heard", 1020, 0.110),
                ("the new buffer reaches the speaker", 1033, 0.110),
                ("5 ms into it", 1038, 0.115),
                ("played out", 1043, 0.120),
            ],
        );
    }

    #[test]
    fn reset_forgets_the_last_buffer_until_the_next_callback() {
        let (shared, t0) = played_110_with_33ms_latency();

        shared.reset();

        assert_eq!(shared.played_secs(), 0.0);
        check(&shared, t0, &[("where the old buffer was still playing", 135, 0.0), ("where it had played out", 143, 0.0)]);

        shared.push(&ramp(10));
        render(&shared, 10, t0 + ms(500), Duration::ZERO);
        check(&shared, t0, &[("5 ms into the first buffer after the reset", 505, 0.005), ("played out", 510, 0.010)]);
    }
}

mod device_callback {
    use super::*;

    #[test]
    fn render_counts_the_buffer_as_heard_from_now_with_no_latency() {
        let shared = shared_1khz_stereo();
        shared.push(&ramp(10));
        let mut out = [9.0f32; 20];

        let before = Instant::now();
        shared.render(&mut out);
        let just_after = shared.heard_secs();

        // Heard from the call on: at most the time since `before` into the 10 ms buffer.
        let bound = (before.elapsed().as_secs_f64()).min(0.010);
        assert!((0.0..=bound + 1e-9).contains(&just_after), "heard {just_after} right after render, at most {bound}");
        std::thread::sleep(ms(30));
        assert_eq!(shared.heard_secs(), 0.010, "30 ms later the 10 ms buffer has played out");
    }
}
