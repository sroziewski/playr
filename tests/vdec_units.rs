//! Bitstream helpers in `playr::vdec` and `playr::player::pcm_to_f32`: NAL splitting,
//! keyframe detection, `hvcC` parsing and the timestamp queue. No decoder is run here.

use playr::media::{VideoCodec, VideoTrack};
use playr::player::pcm_to_f32;
use playr::vdec::{PtsQueue, annexb_nals, avcc_nals, hvcc_parameter_sets, is_keyframe, mpeg4_is_intra};

fn track(codec: VideoCodec, extra: Vec<u8>) -> VideoTrack {
    VideoTrack { codec, width: 320, height: 240, extra }
}

/// A minimal `avcC` record (no parameter sets) announcing `length_size`-byte NAL lengths.
fn avcc_extra(length_size: u8) -> Vec<u8> {
    vec![1, 0x64, 0x00, 0x1E, 0xFC | (length_size - 1), 0xE0, 0x00]
}

/// A minimal `hvcC` record (no arrays) announcing `length_size`-byte NAL lengths.
fn hvcc_extra(length_size: u8) -> Vec<u8> {
    let mut extra = vec![0u8; 23];
    extra[0] = 1;
    extra[21] = 0xFC | (length_size - 1);
    extra
}

fn length_prefixed(length_size: usize, nals: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for nal in nals {
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes()[4 - length_size..]);
        out.extend_from_slice(nal);
    }
    out
}

mod length_prefixed_nals {
    use super::*;

    #[test]
    fn splits_at_every_length_field_size() {
        let nals: [&[u8]; 3] = [&[0x67, 0xAA], &[0x68], &[0x65, 0x01, 0x02, 0x03]];
        for length_size in 1..=4 {
            let data = length_prefixed(length_size, &nals);
            let got: Vec<&[u8]> = avcc_nals(&data, length_size).collect();
            assert_eq!(got, nals, "length size {length_size}");
        }
    }

    #[test]
    fn stops_at_a_truncated_sample_instead_of_reading_past_the_end() {
        let complete: &[u8] = &[0x67, 0xAA];
        let whole = length_prefixed(4, &[complete, &[0x65, 0x01, 0x02, 0x03]]);
        // Cut inside the second NAL's payload, inside its length field, and right after it.
        for cut in [whole.len() - 1, 6 + 2, 6 + 4] {
            let got: Vec<&[u8]> = avcc_nals(&whole[..cut], 4).collect();
            assert_eq!(got, [complete], "sample cut to {cut} bytes");
        }
    }

    #[test]
    fn a_length_larger_than_the_address_space_ends_the_iteration() {
        let mut data = length_prefixed(4, &[&[0x67, 0xAA]]);
        data.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF, 0x65]);
        let got: Vec<&[u8]> = avcc_nals(&data, 4).collect();
        assert_eq!(got, [&[0x67, 0xAA][..]]);
    }

    #[test]
    fn yields_nothing_for_empty_data_or_an_impossible_length_size() {
        let data = length_prefixed(4, &[&[0x65, 0x01]]);
        assert_eq!(avcc_nals(&[], 4).count(), 0, "empty sample");
        assert_eq!(avcc_nals(&data, 0).count(), 0, "length size 0");
        assert_eq!(avcc_nals(&data, 5).count(), 0, "length size 5");
    }
}

mod annex_b_nals {
    use super::*;

    #[test]
    fn splits_at_three_and_four_byte_start_codes() {
        let cases: [(&str, &[u8], &[&[u8]]); 5] = [
            ("three-byte codes", &[0, 0, 1, 0x67, 0xAA, 0, 0, 1, 0x68, 0xBB], &[&[0x67, 0xAA], &[0x68, 0xBB]]),
            ("four-byte codes", &[0, 0, 0, 1, 0x67, 0xAA, 0, 0, 0, 1, 0x65, 0x11], &[&[0x67, 0xAA], &[0x65, 0x11]]),
            ("mixed codes", &[0, 0, 0, 1, 0x67, 0xAA, 0, 0, 1, 0x65, 0x11, 0x22], &[&[0x67, 0xAA], &[0x65, 0x11, 0x22]]),
            ("bytes before the first code are not a NAL", &[0x99, 0x98, 0, 0, 1, 0x41, 0x55], &[&[0x41, 0x55]]),
            ("a start code with nothing after it", &[0, 0, 1, 0x67, 0xAA, 0, 0, 1], &[&[0x67, 0xAA]]),
        ];
        for (name, data, expected) in cases {
            assert_eq!(annexb_nals(data), expected, "case: {name}");
        }
    }

    #[test]
    fn yields_nothing_without_a_start_code() {
        let cases: [&[u8]; 4] = [&[], &[0, 0], &[0x65, 0x88, 0x84], &[0, 0, 2, 0x65]];
        for data in cases {
            assert_eq!(annexb_nals(data), Vec::<&[u8]>::new(), "data {data:02X?}");
        }
    }
}

mod keyframes {
    use super::*;

    #[test]
    fn h264_sample_is_a_keyframe_when_it_contains_an_idr_slice() {
        // NAL types: 5 IDR slice, 1 non-IDR slice, 6 SEI, 7 SPS, 9 access unit delimiter.
        let cases: [(&str, &[&[u8]], bool); 6] = [
            ("IDR only", &[&[0x65, 0x88]], true),
            ("IDR with another nal_ref_idc", &[&[0x25, 0x88]], true),
            ("AUD, SEI, then IDR", &[&[0x09, 0xF0], &[0x06, 0x05], &[0x65, 0x88]], true),
            ("non-IDR slice", &[&[0x41, 0x9A]], false),
            ("SPS without a slice", &[&[0x67, 0x64]], false),
            ("SEI then non-IDR slice", &[&[0x06, 0x05], &[0x61, 0x9A]], false),
        ];
        for length_size in [1usize, 2, 4] {
            let t = track(VideoCodec::H264, avcc_extra(length_size as u8));
            for (name, nals, expected) in cases {
                let data = length_prefixed(length_size, nals);
                assert_eq!(is_keyframe(&t, &data), expected, "avcC, length size {length_size}: {name}");
            }
        }
        // No avcC record: the stream is Annex B, as in AVI.
        let annexb = track(VideoCodec::H264, Vec::new());
        for (name, nals, expected) in cases {
            let data: Vec<u8> = nals.iter().flat_map(|n| [&[0u8, 0, 0, 1][..], n].concat()).collect();
            assert_eq!(is_keyframe(&annexb, &data), expected, "Annex B: {name}");
        }
    }

    #[test]
    fn h264_sample_with_a_recovery_point_sei_is_a_keyframe() {
        // Open-GOP encoders mark random-access pictures with a recovery-point SEI message
        // (payload type 6) instead of coding them as IDR. An SEI NAL (type 6) is a list of
        // messages: type, size, payload; then the trailing bits 0x80.
        let cases: [(&str, &[u8], bool); 10] = [
            ("recovery point as the first message", &[0x06, 0x06, 0x01, 0x84, 0x80], true),
            ("picture timing, then recovery point", &[0x06, 0x01, 0x02, 0xAA, 0xBB, 0x06, 0x01, 0x84, 0x80], true),
            ("user data only", &[0x06, 0x05, 0x03, 0x11, 0x22, 0x33, 0x80], false),
            ("user data whose payload reads like a recovery point", &[0x06, 0x05, 0x04, 0x06, 0x01, 0x84, 0x80, 0x80], false),
            ("payload type 261, coded as 0xFF 0x06", &[0x06, 0xFF, 0x06, 0x01, 0x84, 0x80], false),
            ("SEI header only", &[0x06], false),
            ("cut after the payload type", &[0x06, 0x05], false),
            ("payload size runs past the NAL", &[0x06, 0x05, 0x20, 0x11, 0x22], false),
            ("recovery point behind a message that is cut short", &[0x06, 0x05, 0x10, 0x06, 0x01, 0x84], false),
            ("recovery point cut before its payload", &[0x06, 0x06, 0x01], false),
        ];
        let slice: &[u8] = &[0x41, 0x9A, 0x12];
        let avcc = track(VideoCodec::H264, avcc_extra(4));
        let annexb = track(VideoCodec::H264, Vec::new());
        for (name, sei, expected) in cases {
            assert_eq!(is_keyframe(&avcc, &length_prefixed(4, &[sei, slice])), expected, "avcC: {name}");
            let data = [&[0u8, 0, 0, 1][..], sei, &[0, 0, 1], slice].concat();
            assert_eq!(is_keyframe(&annexb, &data), expected, "Annex B: {name}");
        }
    }

    #[test]
    fn hevc_sample_is_a_keyframe_when_it_contains_an_irap_picture() {
        // The NAL type is bits 1..=6 of the first header byte; IRAP pictures are types 16..=23.
        let header = |nal_type: u8| [nal_type << 1, 0x01, 0xAF];
        let cases = [
            ("TRAIL_R (1)", 1u8, false),
            ("RASL_R (9)", 9, false),
            ("reserved non-IRAP (15)", 15, false),
            ("BLA_W_LP (16)", 16, true),
            ("IDR_W_RADL (19)", 19, true),
            ("IDR_N_LP (20)", 20, true),
            ("CRA (21)", 21, true),
            ("reserved IRAP (23)", 23, true),
            ("reserved non-IRAP (24)", 24, false),
            ("VPS (32)", 32, false),
            ("SPS (33)", 33, false),
        ];
        let aud = header(35);
        for (name, nal_type, expected) in cases {
            let picture = header(nal_type);
            for length_size in [2usize, 4] {
                let t = track(VideoCodec::Hevc, hvcc_extra(length_size as u8));
                let data = length_prefixed(length_size, &[&aud, &picture]);
                assert_eq!(is_keyframe(&t, &data), expected, "hvcC, length size {length_size}: {name}");
            }
            let annexb = track(VideoCodec::Hevc, Vec::new());
            let data = [&[0u8, 0, 0, 1][..], &aud, &[0, 0, 1], &picture].concat();
            assert_eq!(is_keyframe(&annexb, &data), expected, "Annex B: {name}");
        }
    }

    #[test]
    fn mpeg4_packet_is_a_keyframe_when_its_first_vop_is_intra_coded() {
        // After the VOP start code 00 00 01 B6 the top two bits are the coding type:
        // 00 I, 01 P, 10 B, 11 S (sprite).
        let cases: [(&str, &[u8], bool); 9] = [
            ("I-VOP", &[0, 0, 1, 0xB6, 0x10, 0x60], true),
            ("P-VOP", &[0, 0, 1, 0xB6, 0x50, 0x60], false),
            ("B-VOP", &[0, 0, 1, 0xB6, 0x90, 0x60], false),
            ("S-VOP", &[0, 0, 1, 0xB6, 0xD0, 0x60], false),
            ("VOS and VOL headers, then an I-VOP", &[0, 0, 1, 0xB0, 0xF5, 0, 0, 1, 0x20, 0x08, 0, 0, 1, 0xB6, 0x3F], true),
            ("P-VOP followed by an I-VOP", &[0, 0, 1, 0xB6, 0x50, 0, 0, 1, 0xB6, 0x10], false),
            ("headers without a VOP", &[0, 0, 1, 0xB0, 0xF5, 0, 0, 1, 0x20, 0x08], false),
            ("VOP start code cut off before the type", &[0, 0, 1, 0xB6], false),
            ("empty packet", &[], false),
        ];
        let t = track(VideoCodec::Mpeg4, Vec::new());
        for (name, data, expected) in cases {
            assert_eq!(mpeg4_is_intra(data), expected, "mpeg4_is_intra: {name}");
            assert_eq!(is_keyframe(&t, data), expected, "is_keyframe: {name}");
        }
    }
}

mod hvcc {
    use super::*;

    const VPS: &[u8] = &[0x40, 0x01, 0x0C, 0x01];
    const SPS: &[u8] = &[0x42, 0x01, 0x01, 0x21, 0x60];
    const PPS_A: &[u8] = &[0x44, 0x01, 0xC1];
    const PPS_B: &[u8] = &[0x44, 0x01, 0xE0, 0x73, 0xC0, 0x89];

    /// 22 fixed bytes, the array count, then per array: type, count (u16), length-prefixed NALs.
    fn record() -> Vec<u8> {
        let mut r = vec![0u8; 22];
        r[0] = 1;
        r[21] = 0xFF;
        r.push(3);
        for (nal_type, nals) in [(32u8, &[VPS][..]), (33, &[SPS]), (34, &[PPS_A, PPS_B])] {
            r.push(0x80 | nal_type);
            r.extend_from_slice(&(nals.len() as u16).to_be_bytes());
            for nal in nals {
                r.extend_from_slice(&(nal.len() as u16).to_be_bytes());
                r.extend_from_slice(nal);
            }
        }
        r
    }

    #[test]
    fn lists_every_parameter_set_in_record_order() {
        assert_eq!(hvcc_parameter_sets(&record()), [VPS, SPS, PPS_A, PPS_B]);
    }

    #[test]
    fn a_truncated_record_yields_only_the_sets_that_are_complete() {
        let full = record();
        let all: [&[u8]; 4] = [VPS, SPS, PPS_A, PPS_B];
        // End offset of each NAL inside the record: a set is complete once the cut is past it.
        let mut ends = Vec::new();
        let mut pos = 23;
        for count in [1, 1, 2] {
            pos += 3;
            for _ in 0..count {
                pos += 2 + all[ends.len()].len();
                ends.push(pos);
            }
        }
        assert_eq!(*ends.last().unwrap(), full.len(), "the offsets above describe the whole record");

        for cut in 0..full.len() {
            let complete = ends.iter().filter(|&&end| end <= cut).count();
            assert_eq!(hvcc_parameter_sets(&full[..cut]), all[..complete], "record cut to {cut} bytes");
        }
    }

    #[test]
    fn an_array_count_larger_than_the_record_is_harmless() {
        let mut r = record();
        r[22] = 200;
        assert_eq!(hvcc_parameter_sets(&r), [VPS, SPS, PPS_A, PPS_B]);
    }
}

mod pts_queue {
    use super::*;

    fn assert_pts(actual: f64, expected: f64, what: &str) {
        assert!((actual - expected).abs() < 1e-6, "{what}: expected {expected}, got {actual}");
    }

    #[test]
    fn timestamps_pushed_in_decode_order_come_out_in_display_order() {
        // I P B B B at 25 fps: decode order 0, 4, 2, 1, 3.
        let mut q = PtsQueue::new(16);
        for pts in [0.0, 0.16, 0.08, 0.04, 0.12] {
            q.push(pts);
        }
        assert_eq!(q.len(), 5);
        for (i, expected) in [0.0, 0.04, 0.08, 0.12, 0.16].into_iter().enumerate() {
            assert_pts(q.pop(), expected, &format!("pop {i}"));
        }
        assert_eq!(q.len(), 0);
        assert!(q.is_empty());
    }

    #[test]
    fn interleaved_pushes_and_pops_keep_display_order() {
        let mut q = PtsQueue::new(16);
        q.push(10.0);
        q.push(10.12);
        assert_pts(q.pop(), 10.0, "first picture");
        q.push(10.04);
        q.push(10.08);
        assert_pts(q.pop(), 10.04, "second picture");
        assert_pts(q.pop(), 10.08, "third picture");
        assert_pts(q.pop(), 10.12, "fourth picture");
    }

    #[test]
    fn a_full_queue_drops_its_oldest_timestamps() {
        let mut q = PtsQueue::new(3);
        for pts in [1.0, 2.0, 3.0, 4.0, 5.0] {
            q.push(pts);
        }
        assert_eq!(q.len(), 3);
        assert_pts(q.pop(), 3.0, "pop 0");
        assert_pts(q.pop(), 4.0, "pop 1");
        assert_pts(q.pop(), 5.0, "pop 2");
    }

    #[test]
    fn equal_and_negative_timestamps_are_kept() {
        let mut q = PtsQueue::new(8);
        for pts in [1.5, -0.08, 1.5] {
            q.push(pts);
        }
        assert_eq!(q.len(), 3);
        assert_pts(q.pop(), -0.08, "pop 0");
        assert_pts(q.pop(), 1.5, "pop 1");
        assert_pts(q.pop(), 1.5, "pop 2");
    }

    #[test]
    fn keeps_microsecond_precision() {
        let mut q = PtsQueue::new(8);
        q.push(3723.456789);
        assert_pts(q.pop(), 3723.456789, "pop");
    }

    // The decoders report a picture order count (POC) with every frame. It rises within a
    // sequence and restarts at each IDR picture, which is what the queue resynchronises on.

    #[test]
    fn pop_poc_returns_display_order_like_pop_while_nothing_is_lost() {
        // IDR, then P B B in decode order; POCs 0 2 4 6 in display order.
        let mut q = PtsQueue::new(64);
        q.push_idr(5.0);
        for pts in [5.12, 5.04, 5.08] {
            q.push(pts);
        }
        assert_eq!(q.len(), 4);
        for (poc, expected) in [(0, 5.0), (2, 5.04), (4, 5.08), (6, 5.12)] {
            assert_pts(q.pop_poc(poc), expected, &format!("frame with POC {poc}"));
        }
        assert_eq!(q.len(), 0);
    }

    #[test]
    fn a_dropped_picture_stops_shifting_timestamps_at_the_next_idr() {
        let mut q = PtsQueue::new(64);
        q.push_idr(0.0);
        q.push(0.04);
        q.push(0.08);
        q.push_idr(0.12);
        q.push(0.16);

        assert_pts(q.pop_poc(0), 0.0, "first IDR frame");
        assert_pts(q.pop_poc(2), 0.04, "second frame");
        // The decoder dropped the picture for 0.08; the next frame out is the second IDR,
        // recognisable by its POC starting over.
        assert_pts(q.pop_poc(0), 0.12, "second IDR frame");
        assert_pts(q.pop_poc(2), 0.16, "frame after the second IDR");
        assert_eq!(q.len(), 0, "the dropped picture's timestamp is gone");
    }

    #[test]
    fn several_dropped_pictures_before_an_idr_are_all_discarded() {
        let mut q = PtsQueue::new(64);
        q.push_idr(2.0);
        for pts in [2.04, 2.08, 2.12, 2.16] {
            q.push(pts);
        }
        q.push_idr(2.2);
        q.push(2.24);

        assert_pts(q.pop_poc(0), 2.0, "first IDR frame");
        assert_pts(q.pop_poc(2), 2.04, "second frame");
        // 2.08, 2.12 and 2.16 never come out of the decoder.
        assert_pts(q.pop_poc(0), 2.2, "second IDR frame");
        assert_pts(q.pop_poc(2), 2.24, "frame after the second IDR");
    }

    #[test]
    fn frames_of_a_stream_joined_mid_sequence_keep_their_timestamps_until_the_first_idr() {
        // Decoding starts on a non-IDR picture (a recovery point), the first IDR comes later.
        let mut q = PtsQueue::new(64);
        q.push(1.0);
        q.push(1.04);
        q.push_idr(1.08);
        q.push(1.12);

        assert_pts(q.pop_poc(10), 1.0, "first frame, before any IDR");
        assert_pts(q.pop_poc(12), 1.04, "second frame, before any IDR");
        assert_pts(q.pop_poc(0), 1.08, "the IDR frame");
        assert_pts(q.pop_poc(2), 1.12, "frame after the IDR");
    }

    #[test]
    fn a_picture_dropped_before_the_first_idr_of_a_joined_stream_is_recovered_at_that_idr() {
        let mut q = PtsQueue::new(64);
        q.push(1.0);
        q.push(1.04);
        q.push_idr(1.08);

        assert_pts(q.pop_poc(10), 1.0, "first frame, before any IDR");
        // The picture for 1.04 is dropped.
        assert_pts(q.pop_poc(0), 1.08, "the IDR frame");
    }

    #[test]
    fn consecutive_idr_pictures_with_equal_poc_each_get_their_own_timestamp() {
        // An all-intra stream: every picture is an IDR with POC 0.
        let mut q = PtsQueue::new(64);
        for pts in [3.0, 3.04, 3.08] {
            q.push_idr(pts);
        }
        for (n, expected) in [3.0, 3.04, 3.08].into_iter().enumerate() {
            assert_pts(q.pop_poc(0), expected, &format!("IDR frame {n}"));
        }
        assert_eq!(q.len(), 0);
    }

    #[test]
    fn a_picture_dropped_between_two_idr_pictures_with_equal_poc_is_discarded() {
        // IDR, one predicted picture that the decoder drops, IDR again: both frames have POC 0.
        let mut q = PtsQueue::new(64);
        q.push_idr(4.0);
        q.push(4.04);
        q.push_idr(4.08);
        q.push(4.12);

        assert_pts(q.pop_poc(0), 4.0, "first IDR frame");
        assert_pts(q.pop_poc(0), 4.08, "second IDR frame");
        assert_pts(q.pop_poc(2), 4.12, "frame after the second IDR");
    }

    #[test]
    fn idr_frames_take_the_oldest_outstanding_idr_timestamp_while_more_are_fed() {
        // An all-intra stream: the first frame comes out after one picture, the second only
        // after four more have gone in. It is still the picture for 0.04.
        let mut q = PtsQueue::new(64);
        q.push_idr(0.0);
        assert_pts(q.pop_poc(0), 0.0, "first frame");
        for pts in [0.04, 0.08, 0.12, 0.16] {
            q.push_idr(pts);
        }
        for (n, expected) in [0.04, 0.08, 0.12, 0.16].into_iter().enumerate() {
            assert_pts(q.pop_poc(0), expected, &format!("frame {}", n + 2));
        }
        assert_eq!(q.len(), 0);
    }

    /// Runs `pictures` pictures in groups of `gop_len` (an IDR, then predicted pictures,
    /// 25 fps, no reordering) through a model of a decoder that loses nothing: frame j comes
    /// out once `in_flight(j)` pictures from j on have been fed (or everything has). Returns
    /// the (true, assigned) timestamps of the frames whose timestamp is wrong.
    fn mistimed_by_an_undamaged_decoder(gop_len: usize, pictures: usize, in_flight: impl Fn(usize) -> usize) -> Vec<(f64, f64)> {
        let mut q = PtsQueue::new(1024);
        let mut fed = 0;
        let mut wrong = Vec::new();
        for j in 0..pictures {
            while fed < (j + in_flight(j)).clamp(j + 1, pictures) {
                let pts = fed as f64 * 0.04;
                if fed % gop_len == 0 { q.push_idr(pts) } else { q.push(pts) }
                fed += 1;
            }
            let got = q.pop_poc(2 * (j % gop_len) as i32);
            if (got - j as f64 * 0.04).abs() > 1e-6 {
                wrong.push((j as f64 * 0.04, got));
            }
        }
        wrong
    }

    #[test]
    fn nothing_lost_means_every_frame_gets_its_own_timestamp_whatever_the_decoder_latency_does() {
        // How many pictures are inside the decoder depends on how fast packets arrive, so it
        // rises, falls and jumps; with short groups several IDR packets are outstanding.
        let schedules: [(&str, fn(usize) -> usize); 6] = [
            ("one in flight", |_| 1),
            ("steady at six", |_| 6),
            ("rising", |j| 1 + j / 2),
            ("falling", |j| 40usize.saturating_sub(j).max(1)),
            ("sawtooth", |j| 1 + (j * 5) % 13),
            ("everything fed before the first frame", |_| usize::MAX / 2),
        ];
        for gop_len in [1, 2, 3, 4, 12] {
            for (name, in_flight) in schedules {
                let wrong = mistimed_by_an_undamaged_decoder(gop_len, 60, in_flight);
                assert_eq!(wrong, Vec::<(f64, f64)>::new(), "groups of {gop_len}, {name}: (true, assigned) timestamps");
            }
        }
    }

    #[test]
    fn discard_takes_back_the_timestamp_of_a_rejected_picture() {
        let mut q = PtsQueue::new(64);
        q.push_idr(0.0);
        for pts in [0.04, 0.08, 0.12] {
            q.push(pts);
        }

        q.discard(0.08);

        assert_eq!(q.len(), 3);
        for (poc, expected) in [(0, 0.0), (2, 0.04), (6, 0.12)] {
            assert_pts(q.pop_poc(poc), expected, &format!("frame with POC {poc}"));
        }
    }

    #[test]
    fn discarding_a_rejected_idr_picture_removes_its_marker_too() {
        let mut q = PtsQueue::new(64);
        q.push_idr(0.0);
        q.push(0.04);
        q.push_idr(0.08);
        q.push(0.12);
        q.push_idr(0.16);
        q.push(0.2);

        // The IDR packet at 0.08 is rejected by the decoder. The picture that depended on it
        // (0.12) produces no frame either, without the queue being told.
        q.discard(0.08);

        assert_eq!(q.len(), 5);
        assert_pts(q.pop_poc(0), 0.0, "first IDR frame");
        assert_pts(q.pop_poc(2), 0.04, "second frame");
        assert_pts(q.pop_poc(0), 0.16, "the next IDR frame that does come out");
        assert_pts(q.pop_poc(2), 0.2, "frame after it");
        assert_eq!(q.len(), 0, "the orphaned timestamp 0.12 is gone as well");
    }

    #[test]
    fn discarding_a_timestamp_that_is_not_queued_changes_nothing() {
        let mut q = PtsQueue::new(64);
        q.push_idr(1.0);
        q.push(1.04);

        q.discard(1.02);
        q.discard(7.0);

        assert_eq!(q.len(), 2);
        assert_pts(q.pop_poc(0), 1.0, "IDR frame");
        assert_pts(q.pop_poc(2), 1.04, "second frame");
    }

    #[test]
    fn discarding_one_of_two_equal_timestamps_keeps_the_other() {
        let mut q = PtsQueue::new(64);
        for pts in [1.0, 1.0, 1.04] {
            q.push(pts);
        }

        q.discard(1.0);

        assert_eq!(q.len(), 2);
        assert_pts(q.pop(), 1.0, "the remaining one");
        assert_pts(q.pop(), 1.04, "the next");
    }

    #[test]
    fn a_frame_the_queue_had_no_timestamp_for_stops_shifting_the_rest_at_the_next_idr() {
        // The packet at 0.04 turns out to hold two pictures, so one frame more comes out of
        // this group than timestamps went in. From the next IDR on the pairing is right again.
        let mut q = PtsQueue::new(64);
        q.push_idr(0.0);
        q.push(0.04);
        q.push(0.08);
        q.push_idr(0.12);
        q.push(0.16);
        q.push(0.2);

        assert_pts(q.pop_poc(0), 0.0, "first IDR frame");
        assert_pts(q.pop_poc(2), 0.04, "second frame");
        // Two more frames of the first group for the one timestamp left in it; what they are
        // given is not asserted here.
        for poc in [4, 6] {
            q.pop_poc(poc);
        }
        assert_pts(q.pop_poc(0), 0.12, "second IDR frame");
        assert_pts(q.pop_poc(2), 0.16, "frame after it");
        assert_pts(q.pop_poc(4), 0.2, "last frame");
    }

    #[test]
    fn popping_an_empty_queue_extrapolates_forward_from_the_last_timestamp() {
        let mut q = PtsQueue::new(8);
        q.push(2.0);
        assert_pts(q.pop(), 2.0, "the queued timestamp");

        let first = q.pop();
        let second = q.pop();

        // The contract is "a little later each time": at most a tenth of a second per
        // picture, which covers every frame rate from 10 fps up.
        assert!(first > 2.0 && first <= 2.1, "first extrapolated timestamp {first}");
        assert!(second > first && second <= first + 0.1, "second extrapolated timestamp {second} after {first}");
    }
}

mod pcm {
    use super::*;

    fn convert(data: &[u8], bits: u16) -> Vec<f32> {
        let mut out = Vec::new();
        pcm_to_f32(data, bits, &mut out);
        out
    }

    #[test]
    fn converts_every_sample_width_to_the_range_minus_one_to_one() {
        // 8-bit is unsigned with silence at 128; wider samples are signed little-endian.
        // (bits, bytes, expected samples)
        let cases: [(u16, &[u8], &[f32]); 4] = [
            (8, &[0, 64, 128, 192, 255], &[-1.0, -0.5, 0.0, 0.5, 127.0 / 128.0]),
            (
                16,
                &[0x00, 0x80, 0x00, 0xC0, 0x00, 0x00, 0x00, 0x40, 0xFF, 0x7F, 0x01, 0x00],
                &[-1.0, -0.5, 0.0, 0.5, 32767.0 / 32768.0, 1.0 / 32768.0],
            ),
            (
                24,
                &[
                    0x00, 0x00, 0x80, 0x00, 0x00, 0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x40, 0xFF, 0xFF, 0x7F, 0x01, 0x00,
                    0x00, 0xFF, 0xFF, 0xFF,
                ],
                &[-1.0, -0.5, 0.0, 0.5, 8_388_607.0 / 8_388_608.0, 1.0 / 8_388_608.0, -1.0 / 8_388_608.0],
            ),
            (
                32,
                &[
                    0x00, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00, 0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x40, 0x00,
                    0x01, 0x00, 0x00,
                ],
                &[-1.0, -0.5, 0.0, 0.5, 256.0 / 2_147_483_648.0],
            ),
        ];
        for (bits, data, expected) in cases {
            assert_eq!(convert(data, bits), expected, "{bits} bit");
        }
    }

    #[test]
    fn full_scale_positive_32_bit_stays_within_range() {
        // 2^31 - 1 is not representable in f32, so the result may round to exactly 1.0.
        let top = convert(&[0xFF, 0xFF, 0xFF, 0x7F], 32);
        assert_eq!(top.len(), 1);
        assert!(top[0] > 0.999_999 && top[0] <= 1.0, "full-scale positive sample became {}", top[0]);
    }

    #[test]
    fn a_dangling_partial_sample_is_ignored() {
        assert_eq!(convert(&[0x00, 0x40, 0x12], 16), [0.5]);
        assert_eq!(convert(&[0x00, 0x00, 0x40, 0x12, 0x34], 24), [0.5]);
        assert_eq!(convert(&[0x00, 0x00, 0x00, 0x40, 0x12, 0x34, 0x56], 32), [0.5]);
        assert_eq!(convert(&[], 16), Vec::<f32>::new());
    }

    #[test]
    fn an_unsupported_sample_width_yields_no_samples() {
        for bits in [0, 12, 20, 64] {
            assert_eq!(convert(&[0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88], bits), Vec::<f32>::new(), "{bits} bits");
        }
    }
}
