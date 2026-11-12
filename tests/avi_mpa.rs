//! MPEG audio frame arithmetic and reassembly used by the AVI demuxer
//! (`playr::avi::mpa_frame_info`, `playr::avi::MpaFramer`).
//!
//! Expected sizes follow ISO/IEC 11172-3 / 13818-3:
//!   Layer I:              (12 * bitrate / rate + padding) * 4 bytes, 384 samples
//!   Layer II:             144 * bitrate / rate + padding bytes, 1152 samples
//!   Layer III, MPEG-1:    144 * bitrate / rate + padding bytes, 1152 samples
//!   Layer III, MPEG-2/2.5: 72 * bitrate / rate + padding bytes, 576 samples
//! (integer division).

use playr::avi::{MpaFramer, mpa_frame_info};

mod frame_info {
    use super::*;

    #[test]
    fn computes_length_samples_and_rate_from_the_header() {
        // (what, header, (bytes, samples, sample rate))
        let cases: [(&str, [u8; 4], (usize, u32, u32)); 17] = [
            ("MPEG-1 L3 128k 44.1k", [0xFF, 0xFB, 0x90, 0x00], (417, 1152, 44100)),
            ("MPEG-1 L3 128k 44.1k padded", [0xFF, 0xFB, 0x92, 0x00], (418, 1152, 44100)),
            ("MPEG-1 L3 128k 44.1k with CRC", [0xFF, 0xFA, 0x90, 0x00], (417, 1152, 44100)),
            ("MPEG-1 L3 128k 48k", [0xFF, 0xFB, 0x94, 0x00], (384, 1152, 48000)),
            ("MPEG-1 L3 128k 32k", [0xFF, 0xFB, 0x98, 0x00], (576, 1152, 32000)),
            ("MPEG-1 L3 320k 44.1k", [0xFF, 0xFB, 0xE0, 0x00], (1044, 1152, 44100)),
            ("MPEG-1 L3 32k 44.1k", [0xFF, 0xFB, 0x10, 0x00], (104, 1152, 44100)),
            ("MPEG-1 L3 joint stereo bits set", [0xFF, 0xFB, 0x90, 0x64], (417, 1152, 44100)),
            ("MPEG-1 L2 256k 48k", [0xFF, 0xFD, 0xC4, 0x00], (768, 1152, 48000)),
            ("MPEG-1 L2 128k 44.1k padded", [0xFF, 0xFD, 0x82, 0x00], (418, 1152, 44100)),
            ("MPEG-1 L1 384k 44.1k", [0xFF, 0xFF, 0xC0, 0x00], (416, 384, 44100)),
            ("MPEG-1 L1 384k 44.1k padded", [0xFF, 0xFF, 0xC2, 0x00], (420, 384, 44100)),
            ("MPEG-2 L3 64k 22.05k", [0xFF, 0xF3, 0x80, 0x00], (208, 576, 22050)),
            ("MPEG-2 L3 64k 24k padded", [0xFF, 0xF3, 0x86, 0x00], (193, 576, 24000)),
            ("MPEG-2 L2 64k 22.05k", [0xFF, 0xF5, 0x80, 0x00], (417, 1152, 22050)),
            ("MPEG-2 L1 128k 22.05k", [0xFF, 0xF7, 0x80, 0x00], (276, 384, 22050)),
            ("MPEG-2.5 L3 32k 8k", [0xFF, 0xE3, 0x48, 0x00], (288, 576, 8000)),
        ];
        for (what, header, expected) in cases {
            assert_eq!(mpa_frame_info(header), Some(expected), "{what} ({header:02X?})");
        }
    }

    #[test]
    fn rejects_headers_that_are_not_a_frame_start() {
        let cases: [(&str, [u8; 4]); 8] = [
            ("all zero", [0x00, 0x00, 0x00, 0x00]),
            ("first sync byte wrong", [0xFE, 0xFB, 0x90, 0x00]),
            ("sync bits missing in second byte", [0xFF, 0x1B, 0x90, 0x00]),
            ("only ten sync bits", [0xFF, 0xDB, 0x90, 0x00]),
            ("reserved version", [0xFF, 0xEB, 0x90, 0x00]),
            ("reserved layer", [0xFF, 0xF9, 0x90, 0x00]),
            ("forbidden bitrate index 15", [0xFF, 0xFB, 0xF0, 0x00]),
            ("reserved sample rate index 3", [0xFF, 0xFB, 0x9C, 0x00]),
        ];
        for (what, header) in cases {
            assert_eq!(mpa_frame_info(header), None, "{what} ({header:02X?})");
        }
    }

    #[test]
    fn free_format_frames_have_no_computable_length() {
        assert_eq!(mpa_frame_info([0xFF, 0xFB, 0x00, 0x00]), None);
    }
}

mod framer {
    use super::*;

    const FRAME_SECS: f64 = 1152.0 / 44100.0;

    /// A 128 kbit/s 44.1 kHz MPEG-1 Layer III frame (417 bytes, or 418 with the padding bit)
    /// whose payload is a filler byte that can never look like a sync word.
    fn frame(filler: u8, padded: bool) -> Vec<u8> {
        assert_ne!(filler, 0xFF);
        let mut f = vec![0xFF, 0xFB, if padded { 0x92 } else { 0x90 }, 0x00];
        f.resize(if padded { 418 } else { 417 }, filler);
        f
    }

    fn drain(framer: &mut MpaFramer) -> Vec<(f64, Vec<u8>, u32)> {
        std::iter::from_fn(|| framer.next_frame()).collect()
    }

    fn assert_frames(got: &[(f64, Vec<u8>, u32)], expected: &[Vec<u8>], first_pts: f64, what: &str) {
        assert_eq!(got.len(), expected.len(), "{what}: number of frames");
        for (i, (want, (pts, bytes, samples))) in expected.iter().zip(got).enumerate() {
            assert!(bytes == want, "{what}: frame {i} bytes differ (got {} bytes starting {:02X?})", bytes.len(), &bytes[..8.min(bytes.len())]);
            assert_eq!(*samples, 1152, "{what}: frame {i} samples");
            let want_pts = first_pts + i as f64 * FRAME_SECS;
            assert!((pts - want_pts).abs() < 1e-9, "{what}: frame {i} pts expected {want_pts}, got {pts}");
        }
    }

    #[test]
    fn splits_a_chunk_that_batches_several_frames() {
        let frames = [frame(0x11, false), frame(0x22, true), frame(0x33, false)];
        let mut framer = MpaFramer::default();

        framer.push(&frames.concat(), 2.5);
        let got = drain(&mut framer);

        assert_frames(&got, &frames, 2.5, "three frames in one chunk");
        assert_eq!(framer.next_frame(), None);
    }

    #[test]
    fn reassembles_frames_split_at_arbitrary_positions() {
        let frames = [frame(0x11, false), frame(0x22, true), frame(0x33, false), frame(0x44, false)];
        let stream = frames.concat();
        for chunk_len in [1, 2, 3, 5, 100, 416, 417, 418, 1000] {
            let mut framer = MpaFramer::default();
            let mut got = Vec::new();
            for (n, chunk) in stream.chunks(chunk_len).enumerate() {
                // Only the first chunk's timestamp anchors the stream; later ones are far off
                // on purpose and must not leak into the frame times.
                let pts = if n == 0 { 0.75 } else { 1000.0 + n as f64 };
                framer.push(chunk, pts);
                got.extend(drain(&mut framer));
            }
            assert_frames(&got, &frames, 0.75, &format!("chunks of {chunk_len} bytes"));
        }
    }

    #[test]
    fn skips_leading_garbage() {
        let frames = [frame(0x11, false), frame(0x22, false)];
        let mut data = vec![0x55, 0x00, 0x7F, 0xAA, 0x01, 0xFE, 0x10, 0x20, 0x30, 0x40, 0x50];
        data.extend(frames.concat());
        let mut framer = MpaFramer::default();

        framer.push(&data, 4.0);
        let got = drain(&mut framer);

        assert_frames(&got, &frames, 4.0, "garbage before the first frame");
    }

    #[test]
    fn skips_garbage_between_frames() {
        let frames = [frame(0x11, false), frame(0x22, false)];
        let data = [frames[0].clone(), vec![0x00, 0x13, 0x37], frames[1].clone()].concat();
        let mut framer = MpaFramer::default();

        framer.push(&data, 0.0);
        let got = drain(&mut framer);

        assert_frames(&got, &frames, 0.0, "garbage between two frames");
    }

    #[test]
    fn holds_an_incomplete_frame_until_the_rest_arrives() {
        let whole = frame(0x66, false);
        let mut framer = MpaFramer::default();

        framer.push(&whole[..416], 1.0);
        assert_eq!(framer.next_frame(), None, "one byte short of a frame");
        assert_eq!(framer.next_frame(), None, "asking again changes nothing");
        framer.push(&whole[416..], 9.0);

        assert_frames(&drain(&mut framer), &[whole], 1.0, "frame completed by a second chunk");
    }

    #[test]
    fn one_chunk_holding_thousands_of_frames_is_split_correctly_and_quickly() {
        // 5000 frames (about 2 MB, 130 s of audio) in a single push, every tenth one padded.
        let frames: Vec<Vec<u8>> = (0..5000).map(|n| frame((n % 200) as u8 + 1, n % 10 == 3)).collect();
        let mut framer = MpaFramer::default();
        framer.push(&frames.concat(), 7.0);

        let started = std::time::Instant::now();
        let got = drain(&mut framer);
        let took = started.elapsed();

        assert_frames(&got, &frames, 7.0, "5000 frames in one chunk");
        // Linear work takes milliseconds; re-copying the buffer for every frame takes far longer.
        assert!(took < std::time::Duration::from_secs(3), "splitting took {took:?}");
    }

    #[test]
    fn yields_nothing_without_data() {
        let mut framer = MpaFramer::default();
        assert_eq!(framer.next_frame(), None);
        framer.push(&[], 0.0);
        assert_eq!(framer.next_frame(), None);
        framer.push(&[0u8; 5000], 0.0);
        assert_eq!(framer.next_frame(), None, "no sync word in 5000 zero bytes");
    }

    #[test]
    fn reset_discards_the_partial_frame_and_takes_the_next_chunk_time() {
        let stale = frame(0x77, false);
        let fresh = frame(0x12, false);
        let mut framer = MpaFramer::default();
        framer.push(&stale[..200], 1.0);
        assert_eq!(framer.next_frame(), None);

        framer.reset();
        framer.push(&fresh, 5.0);

        assert_frames(&drain(&mut framer), &[fresh], 5.0, "after reset");
    }
}
