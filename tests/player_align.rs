//! `playr::player::align_audio`: lining the first decoded audio after an open or a seek up
//! with the point playback resumes from.
//!
//! Contract: `lead = resume_at - pts`. A packet that starts before the resume point loses its
//! first `round(lead * rate)` frames, or is dropped when that is all of it. A packet that
//! starts more than 10 ms after the resume point is preceded by `floor(min(-lead, 60 s) *
//! rate)` frames of silence. Anything else is played as it is.

use playr::player::{AudioAlign, align_audio};

const PLAY_AS_IS: AudioAlign = AudioAlign::Play { skip: 0, pad: 0 };

#[test]
fn a_packet_that_starts_before_the_resume_point_loses_its_head() {
    // (what, resume_at, pts, frames in the packet, rate, expected)
    let cases = [
        ("starts exactly at the resume point", 2.0, 2.0, 1000, 1000, PLAY_AS_IS),
        ("0.4 ms early rounds to no frame", 2.0, 1.9996, 1000, 1000, PLAY_AS_IS),
        ("0.6 ms early rounds to one frame", 2.0, 1.9994, 1000, 1000, AudioAlign::Play { skip: 1, pad: 0 }),
        ("a quarter of the packet is early", 2.0, 1.75, 1000, 1000, AudioAlign::Play { skip: 250, pad: 0 }),
        ("all but the last frame is early", 2.0, 1.001, 1000, 1000, AudioAlign::Play { skip: 999, pad: 0 }),
        ("the packet ends exactly at the resume point", 2.0, 1.0, 1000, 1000, AudioAlign::Drop),
        ("the packet ends before the resume point", 2.0, 0.5, 1000, 1000, AudioAlign::Drop),
        ("the rate, not the frame count, converts time", 2.0, 1.75, 20_000, 44_100, AudioAlign::Play { skip: 11_025, pad: 0 }),
        ("an empty packet before the resume point", 2.0, 1.9, 0, 1000, AudioAlign::Drop),
    ];
    for (what, resume_at, pts, frames, rate, expected) in cases {
        assert_eq!(align_audio(resume_at, pts, frames, rate), expected, "{what}");
    }
}

#[test]
fn a_packet_that_starts_after_the_resume_point_is_preceded_by_silence() {
    // Resume point 0 keeps `lead` exactly `-pts`, so the 10 ms and 60 s limits are hit exactly.
    // (what, pts, rate, frames of silence)
    let cases = [
        ("5 ms late is within tolerance", 0.005, 1000, 0),
        ("exactly 10 ms late is within tolerance", 0.01, 1000, 0),
        ("just over 10 ms late", 0.0101, 1000, 10),
        ("silence is counted in whole frames, rounded down", 0.0127, 1000, 12),
        ("half a second late", 0.5, 1000, 500),
        ("half a second late at 44.1 kHz", 0.5, 44_100, 22_050),
        ("exactly 60 s late", 60.0, 1000, 60_000),
        ("more than 60 s late is capped", 60.5, 1000, 60_000),
        ("wildly late is capped", 86_400.0, 1000, 60_000),
    ];
    for (what, pts, rate, pad) in cases {
        assert_eq!(align_audio(0.0, pts, 1000, rate), AudioAlign::Play { skip: 0, pad }, "{what}");
    }
}

#[test]
fn padding_does_not_depend_on_the_packet_length() {
    for frames in [0, 1, 4096] {
        assert_eq!(align_audio(0.0, 0.25, frames, 1000), AudioAlign::Play { skip: 0, pad: 250 }, "{frames} frames");
    }
}
