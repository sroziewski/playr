//! `playr::media::trim_decoded`: cutting one packet's decoded frames down to `skip..limit`
//! (encoder priming at the front, padding from `limit` on), as `Packet::Audio` prescribes.

use playr::media::trim_decoded;

/// Five stereo frames; frame n is (n + 0.1, n + 0.2), so a lost, shifted or split frame shows.
fn stereo() -> Vec<f32> {
    (0..5).flat_map(|n| [n as f32 + 0.1, n as f32 + 0.2]).collect()
}

fn frames(range: std::ops::Range<usize>) -> Vec<f32> {
    stereo()[range.start * 2..range.end * 2].to_vec()
}

#[test]
fn keeps_the_frames_from_skip_up_to_limit() {
    // (what, skip, limit, frames kept)
    let cases = [
        ("nothing to trim", 0, None, 0..5),
        ("priming only", 2, None, 2..5),
        ("padding only", 0, Some(3), 0..3),
        ("priming and padding", 1, Some(4), 1..4),
        ("a single frame survives", 3, Some(4), 3..4),
        ("limit at the decoded length", 0, Some(5), 0..5),
        ("limit beyond the decoded length adds nothing", 1, Some(9), 1..5),
        ("the whole packet is priming", 5, None, 0..0),
        ("skip beyond the decoded length", 8, None, 0..0),
        ("limit zero: the whole packet is padding", 0, Some(0), 0..0),
        ("skip equal to limit", 2, Some(2), 0..0),
        ("skip past limit", 4, Some(2), 0..0),
    ];
    for (what, skip, limit, kept) in cases {
        let mut samples = stereo();
        trim_decoded(&mut samples, 2, skip, limit);
        assert_eq!(samples, frames(kept), "{what}: skip {skip}, limit {limit:?}");
    }
}

#[test]
fn counts_frames_not_samples() {
    // The same six samples as mono, stereo and three channels, trimmed to frames 1..2.
    let source = [0.1f32, 0.2, 0.3, 0.4, 0.5, 0.6];
    let cases: [(usize, &[f32]); 3] = [(1, &[0.2]), (2, &[0.3, 0.4]), (3, &[0.4, 0.5, 0.6])];
    for (channels, expected) in cases {
        let mut samples = source.to_vec();
        trim_decoded(&mut samples, channels, 1, Some(2));
        assert_eq!(samples, expected, "{channels} channel(s)");
    }
}

#[test]
fn an_empty_packet_stays_empty() {
    let mut samples: Vec<f32> = Vec::new();
    trim_decoded(&mut samples, 2, 3, Some(7));
    assert_eq!(samples, Vec::<f32>::new());
}
