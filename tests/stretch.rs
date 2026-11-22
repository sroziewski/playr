//! `playr::stretch`: playback speed without a change of pitch (WSOLA).

mod common;

use std::collections::VecDeque;
use std::f32::consts::TAU;

use common::{finish, rms, tone_cycles};
use playr::controls::SPEEDS;
use playr::stretch::{Rendered, Stretcher};

const RATE: u32 = 44100;
/// 12 ms of output per step at 44.1 kHz.
const HOP: usize = 529;

fn sine(frames: usize, freq: f32, amplitude: f32, rate: u32) -> Vec<f32> {
    (0..frames).map(|i| amplitude * (TAU * freq * i as f32 / rate as f32).sin()).collect()
}

fn interleave(left: &[f32], right: &[f32]) -> Vec<f32> {
    left.iter().zip(right).flat_map(|(l, r)| [*l, *r]).collect()
}

struct Run {
    out: Vec<f32>,
    media_frames: u64,
}

/// Renders in device-sized calls until the stretcher stops producing, checking after every call
/// that the media position equals the input taken minus the input still held.
fn run(s: &mut Stretcher, input: &mut VecDeque<f32>, speed: f64, channels: usize, call_frames: usize) -> Run {
    let total_frames = (input.len() / channels) as u64;
    let mut run = Run { out: Vec::new(), media_frames: 0 };
    let mut buf = vec![0f32; call_frames * channels];
    for call in 0..100_000 {
        let done = s.render(input, &mut buf, speed, RATE, channels);
        run.out.extend_from_slice(&buf[..done.samples]);
        run.media_frames += done.media_frames;
        let unplayed = (input.len() / channels - s.played_frames_held()) as u64;
        assert_eq!(run.media_frames, total_frames - unplayed, "speed {speed}, call {call}: media frames vs input consumed");
        assert_eq!(done.samples % channels, 0, "whole frames only");
        if done.samples == 0 {
            return run;
        }
    }
    panic!("the stretcher never ran dry");
}

#[test]
fn at_speed_1_a_fresh_stretcher_produces_nothing_and_leaves_the_input_alone() {
    let mut s = Stretcher::default();
    let mut input: VecDeque<f32> = sine(5000, 440.0, 0.5, RATE).into();
    let before = input.clone();
    let mut out = [9f32; 256];

    let done = s.render(&mut input, &mut out, 1.0, RATE, 1);

    assert_eq!(done, Rendered { samples: 0, media_frames: 0 });
    assert_eq!(input, before);
    assert!(!s.is_active());
    assert_eq!(s.played_frames_held(), 0);
}

#[test]
fn output_length_is_the_input_played_divided_by_the_speed() {
    let frames = RATE as usize;
    let mut bad = Vec::new();
    for speed in SPEEDS.into_iter().filter(|&s| s != 1.0) {
        let mut s = Stretcher::default();
        let mut input: VecDeque<f32> = sine(frames, 440.0, 0.5, RATE).into();

        let r = run(&mut s, &mut input, speed, 1, 1024);

        let out_frames = r.out.len() as f64;
        let expected = r.media_frames as f64 / speed;
        expect!(bad, (out_frames - expected).abs() <= HOP as f64, "speed {speed}: {out_frames} frames out for {} in, expected {expected:.0}", r.media_frames);
        expect!(bad, r.media_frames as usize + 4 * HOP >= frames, "speed {speed}: only {} of {frames} input frames played", r.media_frames);
    }
    finish(bad);
}

#[test]
fn the_pitch_of_a_tone_stays_the_same_at_every_speed() {
    let frames = RATE as usize;
    let mut bad = Vec::new();
    for speed in SPEEDS.into_iter().filter(|&s| s != 1.0) {
        let mut s = Stretcher::default();
        let mut input: VecDeque<f32> = sine(frames, 440.0, 0.5, RATE).into();

        let r = run(&mut s, &mut input, speed, 1, 1024);

        let secs = r.out.len() as f64 / RATE as f64;
        let freq = tone_cycles(&r.out, 0.1) as f64 / secs;
        expect!(bad, (freq - 440.0).abs() <= 440.0 * 0.02, "speed {speed}: {freq:.1} Hz over {secs:.3} s, expected 440 Hz");
        let level = rms(&r.out);
        let expected_level = 0.5 / 2f64.sqrt();
        expect!(bad, (level - expected_level).abs() <= expected_level * 0.1, "speed {speed}: rms {level:.4}, expected {expected_level:.4}");
    }
    finish(bad);
}

#[test]
fn stereo_channels_keep_their_own_tone_and_level() {
    let frames = 2 * RATE as usize;
    let left = sine(frames, 440.0, 0.5, RATE);
    let right = sine(frames, 660.0, 0.25, RATE);
    let mut s = Stretcher::default();
    let mut input: VecDeque<f32> = interleave(&left, &right).into();

    let r = run(&mut s, &mut input, 1.5, 2, 512);

    let secs = (r.out.len() / 2) as f64 / RATE as f64;
    let out_left: Vec<f32> = r.out.iter().step_by(2).copied().collect();
    let out_right: Vec<f32> = r.out.iter().skip(1).step_by(2).copied().collect();
    let freq = |x: &[f32], threshold: f32| tone_cycles(x, threshold) as f64 / secs;
    let (fl, fr) = (freq(&out_left, 0.1), freq(&out_right, 0.05));
    assert!((fl - 440.0).abs() <= 8.8, "left {fl:.1} Hz, expected 440");
    assert!((fr - 660.0).abs() <= 13.2, "right {fr:.1} Hz, expected 660");
    let (ll, lr) = (rms(&out_left), rms(&out_right));
    assert!((ll - 0.3536).abs() <= 0.035, "left rms {ll:.4}");
    assert!((lr - 0.1768).abs() <= 0.018, "right rms {lr:.4}");
}

#[test]
fn too_little_input_produces_nothing_and_takes_nothing() {
    let mut s = Stretcher::default();
    let mut input: VecDeque<f32> = sine(HOP - 1, 440.0, 0.5, RATE).into();
    let mut out = [0f32; 2048];

    let done = s.render(&mut input, &mut out, 2.0, RATE, 1);

    assert_eq!(done, Rendered::default());
    assert_eq!(input.len(), HOP - 1);
    assert!(!s.is_active());
}

#[test]
fn an_odd_sized_output_buffer_gets_whole_frames_only() {
    let mut s = Stretcher::default();
    let mut input: VecDeque<f32> = interleave(&sine(20_000, 440.0, 0.5, RATE), &sine(20_000, 330.0, 0.5, RATE)).into();
    let mut out = [7f32; 1001];

    let done = s.render(&mut input, &mut out, 0.75, RATE, 2);

    assert_eq!(done.samples, 1000);
    assert_eq!(out[1000], 7.0, "the odd sample is left alone");
}

#[test]
fn reset_forgets_held_input_and_pending_output() {
    let mut s = Stretcher::default();
    let mut input: VecDeque<f32> = sine(20_000, 440.0, 0.5, RATE).into();
    let mut out = [0f32; 300];
    s.render(&mut input, &mut out, 1.5, RATE, 1);
    let active_before = s.is_active();

    s.reset();

    assert!(active_before);
    assert!(!s.is_active());
    assert_eq!(s.played_frames_held(), 0);
    assert_eq!(s.render(&mut input, &mut out, 1.0, RATE, 1), Rendered::default(), "nothing pending after a reset");
}

#[test]
fn returning_to_speed_1_hands_out_what_is_pending_then_continues_exactly_after_the_played_input() {
    // A ramp: each frame's value is its index, so a dropped or repeated frame shows.
    let frames = 30_000;
    let mut input: VecDeque<f32> = (0..frames).map(|i| i as f32).collect();
    let mut s = Stretcher::default();
    let mut out = [0f32; 256];
    let mut played = 0u64;
    for _ in 0..20 {
        played += s.render(&mut input, &mut out, 1.5, RATE, 1).media_frames;
    }

    let mut flushed = 0;
    let mut calls = 0;
    while s.is_active() {
        let done = s.render(&mut input, &mut out, 1.0, RATE, 1);
        assert_eq!(done.media_frames, 0, "pending output was already counted");
        flushed += done.samples;
        calls += 1;
        assert!(calls < 50, "still holding input after {calls} calls at speed 1");
    }

    assert!(played > 0 && flushed > 0, "played {played}, flushed {flushed}");
    assert_eq!(s.played_frames_held(), 0);
    assert_eq!(input.front().copied(), Some(played as f32), "the next frame at speed 1 is the first one not yet played");
    assert_eq!(input.len() as u64, frames as u64 - played);
}
