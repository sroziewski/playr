//! `playr::ts` on hand-built MPEG transport stream packets (ISO/IEC 13818-1): the program
//! tables, PES packets with their 33-bit timestamps, and ADTS framing of AAC.

mod common;

use common::finish;
use playr::ts::{AdtsConfig, AdtsFramer, Pes, StreamKind, TsDemux, adts_header, unsupported_name};
use common::ts_build::*;

const VIDEO_PID: u16 = 0x100;
const AUDIO_PID: u16 = 0x102;
const PMT_PID: u16 = 0x1000;

/// PAT (with a network entry before the program) and a PMT with H.264 and AAC.
fn tables() -> Vec<u8> {
    let mut out = pat(&[(0, 0x0010), (1, PMT_PID)]);
    out.extend(pmt(PMT_PID, &[], &[(0x1B, VIDEO_PID, vec![]), (0x0F, AUDIO_PID, vec![])]));
    out
}

mod program_tables {
    use super::*;

    #[test]
    fn the_first_video_and_the_first_audio_stream_are_picked_and_the_others_are_listed() {
        let lang = vec![0x0A, 4, b'p', b'o', b'l', 0];
        let streams = [
            (0x81, 0x110, vec![]),
            (0x1B, VIDEO_PID, vec![]),
            (0x24, 0x101, vec![]),
            (0x0F, AUDIO_PID, vec![]),
            (0x03, 0x103, vec![]),
            (0x06, 0x104, vec![0x6A, 1, 0]),
            (0x06, 0x105, [lang.clone(), vec![0x7A, 2, 0, 0]].concat()),
            (0x06, 0x106, lang),
            (0x15, 0x107, vec![]),
        ];
        let mut bytes = pat(&[(0, 0x0010), (1, PMT_PID)]);
        bytes.extend(pmt(PMT_PID, &[0x05, 4, b'H', b'D', b'M', b'V'], &streams));
        let mut demux = TsDemux::default();

        let out = demux.push(&bytes);

        assert_eq!(out, Vec::<Pes>::new());
        assert_eq!((demux.video, demux.audio), (Some((VIDEO_PID, StreamKind::H264)), Some((AUDIO_PID, StreamKind::Aac))));
        // DVB AC-3 (descriptor 0x6A) and E-AC-3 (0x7A) are named by their ATSC stream types.
        assert_eq!(demux.other_types, [0x81, 0x24, 0x03, 0x81, 0x87, 0x06, 0x15]);
        assert!(demux.has_program());
    }

    #[test]
    fn hevc_and_mpeg_audio_are_picked_when_they_come_first() {
        let mut bytes = pat(&[(3, PMT_PID)]);
        bytes.extend(pmt(PMT_PID, &[], &[(0x04, 0x201, vec![]), (0x24, 0x200, vec![]), (0x1B, 0x202, vec![])]));
        let mut demux = TsDemux::default();

        demux.push(&bytes);

        assert_eq!((demux.video, demux.audio, demux.other_types.clone()), (Some((0x200, StreamKind::Hevc)), Some((0x201, StreamKind::Mpa)), vec![0x1B]));
    }

    #[test]
    fn a_program_table_spread_over_several_packets_is_put_together() {
        // Long descriptors push the PMT section past one packet's 183 bytes of payload.
        let long = |tag: u8| [vec![tag, 150], vec![0x20; 150]].concat();
        let streams = [(0x06, 0x150, long(0x0A)), (0x06, 0x151, long(0x0A)), (0x06, 0x152, long(0x0A)), (0x1B, VIDEO_PID, vec![]), (0x0F, AUDIO_PID, vec![])];
        let section = pmt_section(&[], &streams);
        assert!(section.len() > 2 * 183, "test precondition: the section needs three packets");
        let mut bytes = pat(&[(1, PMT_PID)]);
        bytes.extend(psi_packets(PMT_PID, &section));
        let mut demux = TsDemux::default();

        demux.push(&bytes);

        assert_eq!((demux.video, demux.audio, demux.other_types.clone()), (Some((VIDEO_PID, StreamKind::H264)), Some((AUDIO_PID, StreamKind::Aac)), vec![0x06, 0x06, 0x06]));
    }

    #[test]
    fn a_later_different_program_table_does_not_replace_the_first() {
        let mut bytes = tables();
        bytes.extend(pmt(PMT_PID, &[], &[(0x24, 0x300, vec![]), (0x03, 0x301, vec![])]));
        let mut demux = TsDemux::default();

        demux.push(&bytes);

        assert_eq!((demux.video, demux.audio, demux.other_types.clone()), (Some((VIDEO_PID, StreamKind::H264)), Some((AUDIO_PID, StreamKind::Aac)), vec![]));
    }

    #[test]
    fn before_the_tables_arrive_no_stream_is_known_and_pes_packets_are_dropped() {
        let mut bytes = pes_packets(VIDEO_PID, Some(9000), &[0, 0, 1, 0x65, 1, 2, 3]);
        bytes.extend(tables());
        let mut demux = TsDemux::default();

        let out = [demux.push(&bytes), demux.flush()].concat();

        assert_eq!(out, Vec::<Pes>::new());
        assert!(demux.has_program());
    }

    #[test]
    fn stream_types_have_names_for_notes_and_kinds_for_the_supported_ones() {
        let names = [
            (0x01, Some("MPEG-2 video")),
            (0x02, Some("MPEG-2 video")),
            (0x10, Some("MPEG-4 video")),
            (0x11, Some("AAC (LATM) sound")),
            (0x81, Some("AC-3 sound")),
            (0x87, Some("E-AC-3 sound")),
            (0x42, Some("AVS video")),
            (0xEA, Some("VC-1 video")),
            (0x15, None),
            (0x1B, None),
        ];
        let kinds = [
            (0x1B, Some(StreamKind::H264)),
            (0x24, Some(StreamKind::Hevc)),
            (0x0F, Some(StreamKind::Aac)),
            (0x03, Some(StreamKind::Mpa)),
            (0x04, Some(StreamKind::Mpa)),
            (0x81, None),
            (0x06, None),
        ];
        let mut bad = Vec::new();
        for (t, expected) in names {
            expect!(bad, unsupported_name(t) == expected, "unsupported_name({t:#04x}) = {:?}", unsupported_name(t));
        }
        for (t, expected) in kinds {
            expect!(bad, StreamKind::from_type(t) == expected, "from_type({t:#04x}) = {:?}", StreamKind::from_type(t));
        }
        let video: Vec<bool> = [StreamKind::H264, StreamKind::Hevc, StreamKind::Aac, StreamKind::Mpa].iter().map(|k| k.is_video()).collect();
        expect!(bad, video == [true, true, false, false], "is_video: {video:?}");
        finish(bad);
    }
}

mod pes_packets {
    use super::*;

    #[test]
    fn a_pes_spread_over_packets_with_stuffing_is_handed_out_whole_when_the_next_one_starts() {
        let first: Vec<u8> = (0..400u32).map(|i| (i * 7 + 3) as u8).collect();
        let second = vec![0, 0, 1, 0x41, 0xEE];
        let mut bytes = tables();
        bytes.extend(pes_packets(VIDEO_PID, Some(126_000), &first));
        let mut demux = TsDemux::default();

        let done_early = demux.push(&bytes);
        let done_by_next = demux.push(&pes_packets(VIDEO_PID, Some(129_600), &second));
        let flushed = demux.flush();

        assert_eq!(done_early, Vec::<Pes>::new(), "a PES is complete only when the next one starts");
        assert_eq!(done_by_next, [Pes { kind: StreamKind::H264, pts: Some(126_000), data: first }]);
        assert_eq!(flushed, [Pes { kind: StreamKind::H264, pts: Some(129_600), data: second }]);
        assert_eq!(demux.flush(), Vec::<Pes>::new(), "a second flush has nothing left");
    }

    #[test]
    fn timestamps_use_all_33_bits() {
        let cases = [
            ("zero", 0u64),
            ("all 33 bits set", (1u64 << 33) - 1),
            ("top bit only", 1u64 << 32),
            ("every field different", 0x1_2345_6789),
            ("about 26 hours", 26 * 3600 * 90_000),
        ];
        let mut bad = Vec::new();
        for (case, pts) in cases {
            let mut bytes = tables();
            bytes.extend(pes_packets(AUDIO_PID, Some(pts), &[0xA1, 0xA2]));
            let mut demux = TsDemux::default();
            let out = [demux.push(&bytes), demux.flush()].concat();
            let expected = vec![Pes { kind: StreamKind::Aac, pts: Some(pts), data: vec![0xA1, 0xA2] }];
            expect!(bad, out == expected, "{case}: {out:?}");
        }
        finish(bad);
    }

    #[test]
    fn a_pes_without_pts_or_with_pts_and_dts_is_read() {
        let mut bytes = tables();
        bytes.extend(pes_packets(VIDEO_PID, None, &[0, 0, 1, 0x09, 0x10]));
        bytes.extend(pes_packets_with_dts(VIDEO_PID, 183_600, 180_000, &[0, 0, 1, 0x65, 0x88]));
        let mut demux = TsDemux::default();

        let out = [demux.push(&bytes), demux.flush()].concat();

        assert_eq!(
            out,
            [
                Pes { kind: StreamKind::H264, pts: None, data: vec![0, 0, 1, 0x09, 0x10] },
                Pes { kind: StreamKind::H264, pts: Some(183_600), data: vec![0, 0, 1, 0x65, 0x88] },
            ]
        );
    }

    #[test]
    fn video_and_audio_are_collected_separately_while_interleaved() {
        let video: Vec<u8> = (0..300u32).map(|i| i as u8).collect();
        let audio: Vec<u8> = (0..250u32).map(|i| (255 - i) as u8).collect();
        let v = pes_packets(VIDEO_PID, Some(90_000), &video);
        let a = pes_packets(AUDIO_PID, Some(87_000), &audio);
        let mut bytes = tables();
        // v0 a0 v1 a1: each stream's continuation follows the other's start.
        bytes.extend_from_slice(&v[..188]);
        bytes.extend_from_slice(&a[..188]);
        bytes.extend_from_slice(&v[188..]);
        bytes.extend_from_slice(&a[188..]);
        let mut demux = TsDemux::default();

        let out = [demux.push(&bytes), demux.flush()].concat();

        assert_eq!(
            out,
            [Pes { kind: StreamKind::H264, pts: Some(90_000), data: video }, Pes { kind: StreamKind::Aac, pts: Some(87_000), data: audio }]
        );
    }

    #[test]
    fn packets_with_errors_without_payload_or_of_other_streams_add_nothing() {
        let mut bytes = tables();
        bytes.extend(pes_packets(VIDEO_PID, Some(3600), &[0, 0, 1, 0x65, 0x11]));
        let mut errored = continuation(VIDEO_PID, &[0xBA, 0xD0]);
        errored[1] |= 0x80; // transport_error_indicator
        bytes.extend(errored);
        bytes.extend(adaptation_only(VIDEO_PID));
        bytes.extend(pes_packets(0x1FF, Some(7200), &[0xCC; 10]));
        bytes.extend(continuation(0x1FF, &[0xCC; 10]));
        bytes.extend(continuation(VIDEO_PID, &[0x22, 0x33]));
        let mut demux = TsDemux::default();

        let out = [demux.push(&bytes), demux.flush()].concat();

        assert_eq!(out, [Pes { kind: StreamKind::H264, pts: Some(3600), data: vec![0, 0, 1, 0x65, 0x11, 0x22, 0x33] }]);
    }

    #[test]
    fn a_payload_start_that_is_not_a_pes_header_is_ignored_with_what_follows_it() {
        let mut bytes = tables();
        let mut not_pes = pes_packets(VIDEO_PID, Some(3600), &[0, 0, 1, 0x65]);
        let payload_at = 188 - (9 + 5 + 4); // the PES (header, PTS, data) ends the packet
        not_pes[payload_at..payload_at + 3].copy_from_slice(&[0xFF, 0xFF, 0xFF]);
        bytes.extend(not_pes);
        bytes.extend(continuation(VIDEO_PID, &[0x44; 8]));
        bytes.extend(pes_packets(VIDEO_PID, Some(7200), &[0, 0, 1, 0x41]));
        let mut demux = TsDemux::default();

        let out = [demux.push(&bytes), demux.flush()].concat();

        assert_eq!(out, [Pes { kind: StreamKind::H264, pts: Some(7200), data: vec![0, 0, 1, 0x41] }]);
    }

    #[test]
    fn the_demuxer_finds_the_packets_again_after_stray_bytes() {
        let mut bytes = vec![0x00, 0x47, 0x13, 0x47, 0x99];
        bytes.extend(tables());
        bytes.extend(pes_packets(AUDIO_PID, Some(1234), &[1, 2, 3]));
        bytes.extend_from_slice(&[0x00, 0x13, 0x99]);
        bytes.extend(pes_packets(AUDIO_PID, Some(5678), &[4, 5, 6]));
        let mut demux = TsDemux::default();

        let out = [demux.push(&bytes), demux.flush()].concat();

        assert_eq!(
            out,
            [Pes { kind: StreamKind::Aac, pts: Some(1234), data: vec![1, 2, 3] }, Pes { kind: StreamKind::Aac, pts: Some(5678), data: vec![4, 5, 6] }]
        );
    }
}

/// An ADTS header (protection absent unless `crc`) for a frame of `len` bytes in all.
fn adts(object_type: u8, rate_index: u8, channels: u8, len: usize, crc: bool) -> Vec<u8> {
    vec![
        0xFF,
        if crc { 0xF0 } else { 0xF1 },
        ((object_type - 1) << 6) | (rate_index << 2) | (channels >> 2),
        ((channels & 3) << 6) | ((len >> 11) as u8 & 3),
        (len >> 3) as u8,
        ((len & 7) as u8) << 5 | 0x1F,
        0xFC,
    ]
}

fn frame(rate_index: u8, channels: u8, payload: &[u8]) -> Vec<u8> {
    let mut f = adts(2, rate_index, channels, 7 + payload.len(), false);
    f.extend_from_slice(payload);
    f
}

mod adts {
    use super::*;

    #[test]
    fn the_header_gives_the_settings_the_header_length_and_the_frame_length() {
        let lc_44k_stereo = AdtsConfig { object_type: 2, rate_index: 4, rate: 44100, channels: 2 };
        let he_48k_mono = AdtsConfig { object_type: 1, rate_index: 3, rate: 48000, channels: 1 };
        let mut with_crc = adts(2, 4, 2, 300, true);
        with_crc.extend_from_slice(&[0xAB, 0xCD]);
        let cases = [
            ("AAC LC 44.1 kHz stereo", adts(2, 4, 2, 371, false), Some((lc_44k_stereo, 7, 371))),
            ("with CRC: 9-byte header", with_crc, Some((lc_44k_stereo, 9, 300))),
            ("object type 1, 48 kHz mono", adts(1, 3, 1, 8191, false), Some((he_48k_mono, 7, 8191))),
            ("7350 Hz is the last rate", adts(2, 12, 6, 100, false), Some((AdtsConfig { object_type: 2, rate_index: 12, rate: 7350, channels: 6 }, 7, 100))),
            ("rate index 13 is reserved", adts(2, 13, 2, 100, false), None),
            ("frame shorter than its header", adts(2, 4, 2, 6, false), None),
            ("frame shorter than its CRC header", adts(2, 4, 2, 8, true), None),
            ("layer bits set: not ADTS", { let mut h = adts(2, 4, 2, 100, false); h[1] = 0xF3; h }, None),
            ("no sync word", { let mut h = adts(2, 4, 2, 100, false); h[0] = 0xFE; h }, None),
            ("six bytes", adts(2, 4, 2, 100, false)[..6].to_vec(), None),
        ];
        let mut bad = Vec::new();
        for (case, bytes, expected) in cases {
            let actual = adts_header(&bytes);
            expect!(bad, actual == expected, "{case}: {actual:?}, expected {expected:?}");
        }
        finish(bad);
    }

    #[test]
    fn the_audio_specific_config_is_the_two_byte_form_of_the_header_settings() {
        // ISO/IEC 14496-3: 5 bits object type, 4 bits rate index, 4 bits channels, 3 zero bits.
        // 0x1210 and 0x1188 are the well-known configs of LC 44.1 kHz stereo and LC 48 kHz mono.
        let cases = [
            (AdtsConfig { object_type: 2, rate_index: 4, rate: 44100, channels: 2 }, [0x12, 0x10]),
            (AdtsConfig { object_type: 2, rate_index: 3, rate: 48000, channels: 1 }, [0x11, 0x88]),
            (AdtsConfig { object_type: 5, rate_index: 6, rate: 24000, channels: 2 }, [0x2B, 0x10]),
            (AdtsConfig { object_type: 2, rate_index: 11, rate: 8000, channels: 6 }, [0x15, 0xB0]),
        ];
        let mut bad = Vec::new();
        for (config, expected) in cases {
            let actual = config.audio_specific_config();
            expect!(bad, actual == expected, "{config:?}: {actual:02x?}, expected {expected:02x?}");
        }
        finish(bad);
    }

    #[test]
    fn frames_are_cut_out_without_their_headers_and_timed_1024_samples_apart() {
        let config = AdtsConfig { object_type: 2, rate_index: 3, rate: 48000, channels: 2 };
        let mut data = vec![0x00, 0x13]; // junk before the first frame
        data.extend(frame(3, 2, &[1, 1, 1]));
        data.extend(frame(3, 2, &[2, 2]));
        data.extend(frame(3, 2, &[3; 10]));
        let mut framer = AdtsFramer::default();

        framer.push(&data, Some(10.0));
        let frames: Vec<_> = std::iter::from_fn(|| framer.next_frame()).collect();

        let step = 1024.0 / 48000.0;
        let expected = [(10.0, vec![1, 1, 1]), (10.0 + step, vec![2, 2]), (10.0 + 2.0 * step, vec![3; 10])];
        assert_eq!(frames.len(), expected.len(), "{frames:?}");
        for ((pts, raw, cfg), (want_pts, want_raw)) in frames.iter().zip(&expected) {
            assert!((pts - want_pts).abs() < 1e-9, "pts {pts}, expected {want_pts}");
            assert_eq!((raw, *cfg), (want_raw, config));
        }
    }

    #[test]
    fn a_frame_split_over_two_pushes_keeps_the_time_of_its_run() {
        let whole = frame(4, 2, &[9; 20]);
        let next = frame(4, 2, &[8; 5]);
        let mut framer = AdtsFramer::default();

        framer.push(&whole[..12], Some(5.0));
        let before = framer.next_frame();
        framer.push(&[&whole[12..], &next[..]].concat(), Some(9.0));
        let frames: Vec<_> = std::iter::from_fn(|| framer.next_frame()).collect();

        let config = AdtsConfig { object_type: 2, rate_index: 4, rate: 44100, channels: 2 };
        assert_eq!(before, None);
        assert_eq!(frames, [(5.0, vec![9; 20], config), (5.0 + 1024.0 / 44100.0, vec![8; 5], config)]);
    }

    #[test]
    fn a_push_after_the_buffer_ran_dry_takes_its_own_time_and_reset_forgets_everything() {
        let mut framer = AdtsFramer::default();
        framer.push(&frame(4, 2, &[1]), Some(1.0));
        let first = framer.next_frame().map(|f| f.0);
        framer.push(&frame(4, 2, &[2]), Some(7.5));
        let second = framer.next_frame().map(|f| f.0);
        framer.push(&frame(4, 2, &[3])[..5], Some(20.0));
        framer.reset();
        framer.push(&frame(4, 2, &[4]), None);
        let after_reset = framer.next_frame().map(|f| (f.0, f.1));

        // Without a time the run starts at 0.
        assert_eq!((first, second, after_reset), (Some(1.0), Some(7.5), Some((0.0, vec![4]))));
    }
}
