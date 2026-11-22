//! `playr::audio_out`: the device callback (`AudioShared::render`) with its clock, the channel
//! remixer and the fallback resampler. No audio device is opened here.

use playr::audio_out::{AudioShared, LinearResampler, mix_channels};

/// 1000 Hz stereo, so one frame is exactly one millisecond of clock.
fn shared_1khz_stereo() -> AudioShared {
    let shared = AudioShared::default();
    shared.set_format(1000, 2);
    shared
}

fn assert_samples_close(actual: &[f32], expected: &[f32], what: &str) {
    assert_eq!(actual.len(), expected.len(), "{what}: length (actual {actual:?})");
    for (i, (a, e)) in actual.iter().zip(expected).enumerate() {
        let near = &actual[i.saturating_sub(2)..(i + 3).min(actual.len())];
        assert!((a - e).abs() <= 1e-6, "{what}: sample {i} expected {e}, got {a} (around it: {near:?})");
    }
}

mod render {
    use super::*;

    const FRAMES: [f32; 6] = [0.5, -0.25, 0.75, -1.0, 0.125, -0.625];

    #[test]
    fn plays_queued_samples_in_order_and_advances_the_clock_by_the_frames_played() {
        let shared = shared_1khz_stereo();
        shared.push(&FRAMES);
        let mut out = [9.0f32; 4];

        shared.render(&mut out);

        assert_eq!(out, [0.5, -0.25, 0.75, -1.0]);
        assert_eq!(shared.played_secs(), 0.002);
        assert_eq!(shared.buffered_frames(), 1);
        assert_eq!(shared.buffered_secs(), 0.001);

        let mut rest = [9.0f32; 2];
        shared.render(&mut rest);
        assert_eq!(rest, [0.125, -0.625]);
        assert_eq!(shared.played_secs(), 0.003);
        assert_eq!(shared.buffered_frames(), 0);
    }

    #[test]
    fn underrun_pads_with_silence_and_counts_only_what_was_played() {
        let shared = shared_1khz_stereo();
        shared.push(&FRAMES[..4]);
        let mut out = [9.0f32; 10];

        shared.render(&mut out);

        assert_eq!(out, [0.5, -0.25, 0.75, -1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        assert_eq!(shared.played_secs(), 0.002);
    }

    #[test]
    fn empty_buffer_is_silence_and_a_stopped_clock() {
        let shared = shared_1khz_stereo();
        let mut out = [9.0f32; 6];

        shared.render(&mut out);

        assert_eq!(out, [0.0; 6]);
        assert_eq!(shared.played_secs(), 0.0);
    }

    #[test]
    fn free_run_advances_the_clock_by_the_whole_callback() {
        let shared = shared_1khz_stereo();
        shared.set_free_run(true);
        shared.push(&FRAMES[..4]);
        let mut out = [9.0f32; 10];

        shared.render(&mut out);
        assert_eq!(out, [0.5, -0.25, 0.75, -1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        assert_eq!(shared.played_secs(), 0.005);

        // And with nothing queued at all.
        let mut silent = [9.0f32; 6];
        shared.render(&mut silent);
        assert_eq!(silent, [0.0; 6]);
        assert_eq!(shared.played_secs(), 0.008);
    }

    #[test]
    fn paused_output_is_silence_and_neither_clock_nor_buffer_move() {
        let shared = shared_1khz_stereo();
        shared.push(&FRAMES);
        shared.set_paused(true);
        let mut out = [9.0f32; 4];

        shared.render(&mut out);

        assert!(shared.is_paused());
        assert_eq!(out, [0.0; 4]);
        assert_eq!(shared.played_secs(), 0.0);
        assert_eq!(shared.buffered_frames(), 3);

        shared.set_paused(false);
        shared.render(&mut out);
        assert_eq!(out, [0.5, -0.25, 0.75, -1.0], "playback resumes with the samples that were held");
        assert_eq!(shared.played_secs(), 0.002);
    }

    #[test]
    fn pause_wins_over_free_run() {
        let shared = shared_1khz_stereo();
        shared.set_free_run(true);
        shared.set_paused(true);
        let mut out = [9.0f32; 8];

        shared.render(&mut out);

        assert_eq!(out, [0.0; 8]);
        assert_eq!(shared.played_secs(), 0.0);
    }

    #[test]
    fn muted_output_is_silence_but_the_clock_and_buffer_advance() {
        let shared = shared_1khz_stereo();
        shared.push(&FRAMES);
        shared.set_muted(true);
        let mut out = [9.0f32; 4];

        shared.render(&mut out);

        assert!(shared.is_muted());
        assert_eq!(out, [0.0; 4]);
        assert_eq!(shared.played_secs(), 0.002);
        assert_eq!(shared.buffered_frames(), 1);

        shared.set_muted(false);
        let mut rest = [9.0f32; 2];
        shared.render(&mut rest);
        assert_eq!(rest, [0.125, -0.625], "unmuting restores the previous volume");
    }

    #[test]
    fn paused_and_muted_together_behave_as_paused() {
        let shared = shared_1khz_stereo();
        shared.push(&FRAMES);
        shared.set_muted(true);
        shared.set_paused(true);
        let mut out = [9.0f32; 4];

        shared.render(&mut out);

        assert_eq!(out, [0.0; 4]);
        assert_eq!(shared.played_secs(), 0.0);
        assert_eq!(shared.buffered_frames(), 3);
    }

    #[test]
    fn volume_scales_every_sample() {
        let shared = shared_1khz_stereo();
        shared.set_volume(0.25);
        shared.push(&FRAMES);
        let mut out = [9.0f32; 6];

        shared.render(&mut out);

        // Powers of two, so the products are exact in f32.
        assert_eq!(out, [0.125, -0.0625, 0.1875, -0.25, 0.03125, -0.15625]);
        assert_eq!(shared.played_secs(), 0.003);
    }

    #[test]
    fn volume_is_clamped_to_zero_through_one() {
        let shared = shared_1khz_stereo();
        for (requested, expected) in [(0.0, 0.0), (0.4, 0.4), (1.0, 1.0), (1.5, 1.0), (250.0, 1.0), (-0.3, 0.0)] {
            shared.set_volume(requested);
            assert_eq!(shared.volume(), expected, "set_volume({requested})");
        }
        for requested in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            shared.set_volume(requested);
            let v = shared.volume();
            assert!((0.0..=1.0).contains(&v), "set_volume({requested}) left the gain at {v}");
        }
    }

    #[test]
    fn zero_volume_is_silence_with_a_running_clock() {
        let shared = shared_1khz_stereo();
        shared.set_volume(0.0);
        shared.push(&FRAMES);
        let mut out = [9.0f32; 6];

        shared.render(&mut out);

        assert_eq!(out, [0.0; 6]);
        assert_eq!(shared.played_secs(), 0.003);
    }

    #[test]
    fn only_whole_frames_are_played_so_channels_never_swap() {
        let shared = shared_1khz_stereo();
        // One and a half frames queued.
        shared.push(&[0.5, -0.25, 0.75]);
        let mut out = [9.0f32; 4];

        shared.render(&mut out);
        assert_eq!(out, [0.5, -0.25, 0.0, 0.0]);
        assert_eq!(shared.played_secs(), 0.001);

        // The half frame is completed and comes out left-first.
        shared.push(&[-1.0]);
        shared.render(&mut out);
        assert_eq!(out, [0.75, -1.0, 0.0, 0.0]);
        assert_eq!(shared.played_secs(), 0.002);
    }

    #[test]
    fn an_odd_sized_device_buffer_takes_whole_frames_only() {
        let shared = shared_1khz_stereo();
        shared.push(&FRAMES);
        let mut out = [9.0f32; 3];

        shared.render(&mut out);

        assert_eq!(out, [0.5, -0.25, 0.0]);
        assert_eq!(shared.played_secs(), 0.001);
        assert_eq!(shared.buffered_frames(), 2);
    }

    #[test]
    fn clock_and_buffer_use_the_configured_format() {
        let shared = AudioShared::default();
        assert_eq!((shared.rate(), shared.channels()), (48000, 2), "default device format");
        shared.set_format(8000, 1);
        shared.push(&[0.1; 4000]);
        assert_eq!(shared.buffered_frames(), 4000);
        assert_eq!(shared.buffered_secs(), 0.5);

        let mut out = [9.0f32; 2000];
        shared.render(&mut out);

        assert_eq!(shared.played_secs(), 0.25);
        assert_eq!(shared.buffered_secs(), 0.25);
    }

    #[test]
    fn reset_drops_the_queue_and_restarts_the_clock() {
        let shared = shared_1khz_stereo();
        shared.push(&FRAMES);
        let mut out = [9.0f32; 2];
        shared.render(&mut out);
        assert_eq!(shared.played_secs(), 0.001);

        shared.reset();

        assert_eq!(shared.played_secs(), 0.0);
        assert_eq!(shared.buffered_frames(), 0);
        shared.render(&mut out);
        assert_eq!(out, [0.0, 0.0], "nothing of the old queue is left to play");
    }
}

mod speed {
    use super::*;

    const RATE: u32 = 8000;

    macro_rules! expect {
        ($bad:ident, $cond:expr, $($arg:tt)+) => {
            if !$cond {
                $bad.push(format!($($arg)+));
            }
        };
    }

    fn finish(bad: Vec<String>) {
        assert!(bad.is_empty(), "{} expectation(s) failed:\n{}", bad.len(), bad.join("\n"));
    }

    fn stereo_index_ramp(frames: usize) -> Vec<f32> {
        (0..frames).flat_map(|i| [i as f32, -(i as f32)]).collect()
    }

    fn tone(frames: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                let t = i as f32 / RATE as f32;
                [0.5 * (std::f32::consts::TAU * 440.0 * t).sin(), 0.25 * (std::f32::consts::TAU * 330.0 * t).sin()]
            })
            .collect()
    }

    fn stereo_8k() -> AudioShared {
        let shared = AudioShared::default();
        shared.set_format(RATE, 2);
        shared
    }

    fn played_frames(shared: &AudioShared) -> u64 {
        (shared.played_secs() * RATE as f64).round() as u64
    }

    #[test]
    fn set_speed_keeps_positive_finite_speeds_and_turns_anything_else_into_1() {
        let cases = [(0.5, 0.5), (2.0, 2.0), (3.0, 3.0), (1.0, 1.0), (0.0, 1.0), (-1.5, 1.0), (f64::NAN, 1.0), (f64::INFINITY, 1.0)];
        let shared = AudioShared::default();
        assert_eq!(shared.speed(), 1.0, "default");
        for (set, expected) in cases {
            shared.set_speed(set);
            assert_eq!(shared.speed(), expected, "set_speed({set})");
        }
    }

    #[test]
    fn the_clock_counts_media_time_and_the_buffer_only_what_is_still_to_play() {
        let mut bad = Vec::new();
        for speed in [0.5, 1.5, 2.0] {
            let shared = stereo_8k();
            shared.set_speed(speed);
            let pushed = 8000u64;
            shared.push(&tone(pushed as usize));
            let mut out = [0f32; 2 * 160];
            let mut out_frames = 0u64;
            for call in 0..20 {
                shared.render(&mut out);
                out_frames += 160;
                let (played, buffered) = (played_frames(&shared), shared.buffered_frames() as u64);
                expect!(bad, played + buffered == pushed, "speed {speed}, call {call}: played {played} + buffered {buffered} != pushed {pushed}");
            }
            let played = played_frames(&shared) as f64;
            let expected = out_frames as f64 * speed;
            let one_stretch_step = 96.0 * speed + 1.0;
            expect!(bad, (played - expected).abs() <= one_stretch_step, "speed {speed}: clock at {played} frames after {out_frames} out, expected {expected}");
        }
        finish(bad);
    }

    #[test]
    fn volume_scales_the_stretched_sound() {
        let full = stereo_8k();
        let half = stereo_8k();
        for shared in [&full, &half] {
            shared.set_speed(1.25);
            shared.push(&tone(4000));
        }
        half.set_volume(0.5);
        let (mut a, mut b) = ([0f32; 640], [0f32; 640]);

        full.render(&mut a);
        half.render(&mut b);

        assert!(a.iter().any(|s| s.abs() > 0.1), "the stretched output is not silent");
        let scaled: Vec<f32> = a.iter().map(|s| s * 0.5).collect();
        assert_eq!(b.to_vec(), scaled);
        assert_eq!(played_frames(&full), played_frames(&half));
    }

    #[test]
    fn muted_at_speed_is_silence_with_the_clock_running_and_paused_stops_both() {
        let loud = stereo_8k();
        let muted = stereo_8k();
        let paused = stereo_8k();
        for shared in [&loud, &muted, &paused] {
            shared.set_speed(2.0);
            shared.push(&tone(4000));
        }
        muted.set_muted(true);
        paused.set_paused(true);
        let (mut a, mut b, mut c) = ([0f32; 640], [9f32; 640], [9f32; 640]);

        loud.render(&mut a);
        muted.render(&mut b);
        paused.render(&mut c);

        assert_eq!(b, [0.0; 640]);
        assert_eq!(c, [0.0; 640]);
        assert!(played_frames(&loud) > 0);
        assert_eq!(played_frames(&muted), played_frames(&loud));
        assert_eq!((played_frames(&paused), paused.buffered_frames()), (0, 4000));
    }

    #[test]
    fn free_run_advances_the_clock_by_the_callback_times_the_speed() {
        let cases = [(2.0, 10, 0, 20), (0.75, 12, 0, 9), (1.5, 100, 0, 150), (2.0, 300, 4000, 600), (0.5, 300, 4000, 150)];
        let mut bad = Vec::new();
        for (speed, frames, queued, expected) in cases {
            let shared = stereo_8k();
            shared.set_speed(speed);
            shared.set_free_run(true);
            shared.push(&tone(queued));
            let mut out = vec![0f32; 2 * frames];
            shared.render(&mut out);
            let played = played_frames(&shared);
            expect!(bad, played == expected, "speed {speed}, {frames} frames, {queued} queued: clock {played}, expected {expected}");
        }
        finish(bad);
    }

    #[test]
    fn back_at_speed_1_the_queue_plays_on_exactly_from_the_first_frame_not_yet_played() {
        let shared = stereo_8k();
        let pushed = 20_000;
        shared.push(&stereo_index_ramp(pushed));
        shared.set_speed(1.5);
        let mut out = [0f32; 2 * 128];
        for _ in 0..10 {
            shared.render(&mut out);
        }

        shared.set_speed(1.0);
        let mut exact_from = None;
        for call in 0..30 {
            let before = played_frames(&shared);
            shared.render(&mut out);
            let expected: Vec<f32> = stereo_index_ramp(pushed)[2 * before as usize..2 * (before as usize + 128)].to_vec();
            if out.to_vec() == expected {
                exact_from.get_or_insert(call);
            } else {
                assert!(exact_from.is_none(), "call {call}: plain playback stopped again: {:?}", &out[..8]);
            }
            assert_eq!(played_frames(&shared) + shared.buffered_frames() as u64, pushed as u64, "call {call}");
        }

        assert!(exact_from.is_some_and(|c| c < 10), "plain playback resumed at call {exact_from:?}");
    }

    #[test]
    fn the_end_of_the_queue_is_played_to_an_empty_buffer_at_every_speed() {
        let mut bad = Vec::new();
        for speed in [0.5, 1.0, 1.5, 2.0] {
            let shared = stereo_8k();
            shared.set_speed(speed);
            let pushed = 1000u64;
            shared.push(&tone(pushed as usize));
            let mut out = [0f32; 2 * 160];
            for _ in 0..100 {
                shared.render(&mut out);
            }
            let (played, buffered) = (played_frames(&shared), shared.buffered_frames());
            expect!(bad, (played, buffered) == (pushed, 0), "speed {speed}: played {played}, still buffered {buffered}, expected {pushed} and 0");
        }
        finish(bad);
    }

    #[test]
    fn reset_at_speed_forgets_the_stretch() {
        let shared = stereo_8k();
        shared.set_speed(2.0);
        shared.push(&tone(4000));
        let mut out = [0f32; 640];
        shared.render(&mut out);

        shared.reset();
        shared.set_speed(1.0);
        shared.push(&stereo_index_ramp(10));
        let mut plain = [9f32; 8];
        shared.render(&mut plain);

        assert_eq!(plain, [0.0, -0.0, 1.0, -1.0, 2.0, -2.0, 3.0, -3.0]);
        assert_eq!((played_frames(&shared), shared.buffered_frames()), (4, 6));
    }
}

mod mix {
    use super::*;

    fn mixed(src: &[f32], src_ch: usize, dst_ch: usize) -> Vec<f32> {
        // Pre-filled, to show that the output is replaced rather than appended to.
        let mut out = vec![7.0, 7.0, 7.0];
        mix_channels(src, src_ch, dst_ch, &mut out);
        out
    }

    #[test]
    fn channels_are_copied_dropped_or_left_silent_without_changing_the_samples() {
        // (what, source, source channels, device channels, expected)
        let cases: [(&str, &[f32], usize, usize, &[f32]); 9] = [
            ("mono is copied to left and right", &[0.1, -0.2, 0.3], 1, 2, &[0.1, 0.1, -0.2, -0.2, 0.3, 0.3]),
            ("mono on a 5.1 device goes to the fronts only", &[0.5], 1, 6, &[0.5, 0.5, 0.0, 0.0, 0.0, 0.0]),
            ("quad keeps its front pair", &[0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8], 4, 2, &[0.1, 0.2, 0.5, 0.6]),
            ("three channels keep the first two", &[0.1, 0.2, 0.3, 0.4, 0.5, 0.6], 3, 2, &[0.1, 0.2, 0.4, 0.5]),
            ("stereo on a quad device leaves the rears silent", &[0.1, 0.2, 0.3, 0.4], 2, 4, &[0.1, 0.2, 0.0, 0.0, 0.3, 0.4, 0.0, 0.0]),
            ("stereo to stereo passes through", &[0.1, 0.2, 0.3, 0.4], 2, 2, &[0.1, 0.2, 0.3, 0.4]),
            ("mono to mono passes through", &[0.1, 0.2, 0.3], 1, 1, &[0.1, 0.2, 0.3]),
            ("5.1 to 5.1 passes through", &[0.1, 0.2, 0.3, 0.4, 0.5, 0.6], 6, 6, &[0.1, 0.2, 0.3, 0.4, 0.5, 0.6]),
            ("empty input", &[], 1, 2, &[]),
        ];
        for (what, src, src_ch, dst_ch, expected) in cases {
            assert_eq!(mixed(src, src_ch, dst_ch), expected, "{what}");
        }
    }

    #[test]
    fn stereo_to_mono_averages_the_two_channels() {
        assert_samples_close(&mixed(&[0.2, 0.6, -0.4, 0.0, 1.0, 1.0], 2, 1), &[0.4, -0.2, 1.0], "2 -> 1");
    }

    #[test]
    fn five_point_one_folds_centre_and_surrounds_at_minus_3_db_and_drops_the_lfe() {
        // Channel order FL FR C LFE SL SR; two frames with different values in every slot.
        let src = [0.1, 0.2, 0.3, 0.9, 0.4, 0.5, -0.6, 0.7, 0.0, -0.8, 0.2, -0.1];
        // L = 0.5 * (FL + 0.707 C + 0.707 SL), R = 0.5 * (FR + 0.707 C + 0.707 SR)
        let expected = [
            0.5 * (0.1 + 0.707 * 0.3 + 0.707 * 0.4),
            0.5 * (0.2 + 0.707 * 0.3 + 0.707 * 0.5),
            0.5 * (-0.6 + 0.707 * 0.0 + 0.707 * 0.2),
            0.5 * (0.7 + 0.707 * 0.0 + 0.707 * -0.1),
        ];
        assert_samples_close(&mixed(&src, 6, 2), &expected, "5.1 -> 2");
    }

    #[test]
    fn five_point_one_to_mono_averages_the_folded_pair() {
        let src = [0.1, 0.2, 0.3, 0.9, 0.4, 0.5];
        let l = 0.5 * (0.1 + 0.707 * 0.3 + 0.707 * 0.4);
        let r = 0.5 * (0.2 + 0.707 * 0.3 + 0.707 * 0.5);
        assert_samples_close(&mixed(&src, 6, 1), &[0.5 * (l + r)], "5.1 -> 1");
    }

    #[test]
    fn zero_channels_on_either_side_produce_no_output() {
        assert_eq!(mixed(&[0.1, 0.2], 0, 2), Vec::<f32>::new());
        assert_eq!(mixed(&[0.1, 0.2], 2, 0), Vec::<f32>::new());
        assert_eq!(mixed(&[], 0, 0), Vec::<f32>::new());
    }

    #[test]
    fn an_incomplete_trailing_frame_produces_no_output() {
        // Three samples of stereo are one frame and a half.
        assert_samples_close(&mixed(&[0.2, 0.6, 0.9], 2, 1), &[0.4], "2 -> 1 with a dangling sample");
    }
}

mod resampler {
    use super::*;

    fn run(r: &mut LinearResampler, input: &[f32], chunk_samples: &[usize]) -> Vec<f32> {
        let mut out = Vec::new();
        let mut at = 0;
        let mut sizes = chunk_samples.iter().cycle();
        while at < input.len() {
            let n = (*sizes.next().unwrap()).min(input.len() - at);
            r.process(&input[at..at + n], &mut out);
            at += n;
        }
        out
    }

    /// Starts away from zero, so it cannot be confused with the resampler's silent history.
    fn tone(frames: usize) -> Vec<f32> {
        (0..frames).map(|i| (i as f32 * 0.05 + 0.6).sin() * 0.8).collect()
    }

    #[test]
    fn output_length_follows_the_rate_ratio_across_chunk_boundaries() {
        // (source rate, device rate, input frames, expected output frames)
        let cases = [
            (44100, 48000, 4410, 4800),
            (48000, 44100, 4800, 4410),
            (48000, 16000, 3000, 1000),
            (22050, 44100, 1000, 2000),
            (8000, 48000, 800, 4800),
        ];
        for (src_rate, dst_rate, frames, expected) in cases {
            for chunks in [&[usize::MAX][..], &[1], &[7, 100, 333, 1]] {
                let mut r = LinearResampler::new(src_rate, dst_rate, 1);
                let out = run(&mut r, &tone(frames), chunks);
                let diff = out.len() as i64 - expected as i64;
                assert!(
                    diff.abs() <= 1,
                    "{src_rate} -> {dst_rate} Hz in chunks {chunks:?}: expected {expected} +/- 1 frames, got {}",
                    out.len()
                );
            }
        }
    }

    #[test]
    fn chunking_does_not_change_the_output() {
        let input = tone(2000);
        let mut whole = LinearResampler::new(44100, 48000, 1);
        let reference = run(&mut whole, &input, &[usize::MAX]);

        for chunks in [&[1usize][..], &[2, 3], &[7, 100, 333, 1], &[999]] {
            let mut r = LinearResampler::new(44100, 48000, 1);
            let out = run(&mut r, &input, chunks);
            assert_samples_close(&out, &reference, &format!("chunks {chunks:?}"));
        }
    }

    #[test]
    fn identity_ratio_reproduces_the_input() {
        let input = tone(500);
        let mut r = LinearResampler::new(44100, 44100, 1);

        let out = run(&mut r, &input, &[64, 1, 200]);

        assert_eq!(out.len(), input.len());
        // A streaming interpolator may hold one frame of history; apart from that latency the
        // samples must be the input's.
        let delay = if (out[0] - input[0]).abs() <= 1e-6 { 0 } else { 1 };
        assert_samples_close(&out[delay..], &input[..input.len() - delay], "identity (latency removed)");
    }

    #[test]
    fn interpolates_linearly_between_neighbouring_samples() {
        // A ramp through zero at the frame before the first one, which is the resampler's
        // initial history: doubling the rate must give the same ramp at half the slope.
        let input: Vec<f32> = (1..=200).map(|i| i as f32 * 0.004).collect();
        let mut r = LinearResampler::new(24000, 48000, 1);

        let out = run(&mut r, &input, &[3, 5, 2, 50]);

        assert_eq!(out.len(), 400);
        let expected: Vec<f32> = (0..400).map(|k| k as f32 * 0.002).collect();
        assert_samples_close(&out, &expected, "2x upsampled ramp");
    }

    #[test]
    fn channels_are_resampled_independently() {
        // Left carries a tone, right a constant: any cross-talk would disturb the constant.
        let frames = 600;
        let left = tone(frames);
        let input: Vec<f32> = left.iter().flat_map(|&l| [l, -0.5]).collect();
        let mut stereo = LinearResampler::new(44100, 48000, 2);
        let mut mono = LinearResampler::new(44100, 48000, 1);

        let out = run(&mut stereo, &input, &[14, 200, 2]);
        let left_only = run(&mut mono, &left, &[usize::MAX]);

        assert_eq!(out.len() % 2, 0);
        let out_left: Vec<f32> = out.iter().step_by(2).copied().collect();
        let out_right: Vec<f32> = out.iter().skip(1).step_by(2).copied().collect();
        assert_samples_close(&out_left, &left_only, "left channel");
        // The first two output frames ramp up from the silent history; after that it is flat.
        assert_samples_close(&out_right[2..], &vec![-0.5; out_right.len() - 2], "right channel");
    }

    #[test]
    fn reset_returns_to_the_initial_state() {
        let input = tone(300);
        let mut fresh = LinearResampler::new(44100, 48000, 1);
        let reference = run(&mut fresh, &input, &[usize::MAX]);
        let mut reused = LinearResampler::new(44100, 48000, 1);
        run(&mut reused, &tone(77), &[10]);

        reused.reset();
        let out = run(&mut reused, &input, &[usize::MAX]);

        assert_samples_close(&out, &reference, "after reset");
    }

    #[test]
    fn empty_input_produces_no_output() {
        let mut r = LinearResampler::new(44100, 48000, 2);
        let mut out = Vec::new();
        r.process(&[], &mut out);
        assert_eq!(out, Vec::<f32>::new());
    }
}
