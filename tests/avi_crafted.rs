//! The AVI demuxer on hand-built files (`tests/common/avi.rs`): index variants, dropped
//! frames, codec tags, PCM widths and MPEG audio split across chunks.

mod common;

use common::avi::{Avi, Index, bitmap_info, chunk, list, strh, wave_format};
use common::*;
use playr::demux::open_path;
use playr::media::{AudioTrack, Packet, VideoCodec};
use symphonia::core::codecs::audio::well_known::CODEC_ID_MP3;

const ALL_INDEXES: [Index; 4] = [Index::Relative, Index::Absolute, Index::Missing, Index::MissingWithRecLists];

/// An MPEG-4 VOP: start code, coding type in the top two bits (00 intra, 01 predicted), filler.
fn vop(intra: bool, tag: u8, len: usize) -> Vec<u8> {
    let mut v = vec![0, 0, 1, 0xB6, if intra { 0x10 } else { 0x50 }];
    v.resize(len, tag);
    v
}

fn pcm_chunk(seed: u8) -> Vec<u8> {
    (0..800u32).map(|i| (i as u8).wrapping_mul(3).wrapping_add(seed)).collect()
}

/// 64x48 (stored top-down, i.e. with a negative height) at 10 fps with a dropped frame at
/// 0.2 s, interleaved with 8 kHz 8-bit mono PCM in chunks of 0.1 s.
fn av_file() -> Avi {
    let video = [Some((true, 0xA0, 7)), Some((false, 0xA1, 6)), None, Some((true, 0xA3, 9)), Some((false, 0xA4, 8))];
    let mut avi = Avi {
        streams: vec![
            (strh(b"vids", b"XVID", 1, 10, 0), bitmap_info(64, -48, b"XVID")),
            (strh(b"auds", &[0; 4], 1, 8000, 1), wave_format(1, 1, 8000, 8000, 1, 8)),
        ],
        chunks: Vec::new(),
    };
    for (n, frame) in video.into_iter().enumerate() {
        match frame {
            Some((intra, tag, len)) => avi.chunks.push((*b"00dc", vop(intra, tag, len), intra)),
            None => avi.chunks.push((*b"00dc", Vec::new(), false)),
        }
        avi.chunks.push((*b"01wb", pcm_chunk(n as u8), true));
    }
    avi
}

fn open_built(name: &str, bytes: &[u8]) -> Box<dyn playr::demux::Demuxer> {
    open_path(&tmp_file(name, bytes)).unwrap_or_else(|e| panic!("open {name}: {e}"))
}

fn assert_pts(got: &[f64], want: &[f64], what: &str) {
    assert_eq!(got.len(), want.len(), "{what}: timestamps {got:?}, expected {want:?}");
    for (g, w) in got.iter().zip(want) {
        assert!((g - w).abs() < 1e-9, "{what}: timestamps {got:?}, expected {want:?}");
    }
}

#[test]
fn reads_tracks_timing_and_payloads_with_every_kind_of_index() {
    let avi = av_file();
    for index in ALL_INDEXES {
        let what = format!("{index:?}");
        let mut demux = open_built(&format!("crafted_av_{index:?}.avi"), &avi.build(index));
        let (d, err) = drain(demux.as_mut());
        assert_eq!(err, None, "{what}");

        assert_eq!(d.info.container, "avi", "{what}");
        assert!(d.info.seekable, "{what}");
        assert_eq!(d.info.notes, Vec::<String>::new(), "{what}");
        let video = d.info.video.as_ref().expect("video track");
        assert_eq!((&video.codec, video.width, video.height), (&VideoCodec::Mpeg4, 64, 48), "{what}");
        match d.info.audio.as_ref().expect("audio track") {
            AudioTrack::Pcm { rate, channels, bits } => assert_eq!((*rate, *channels, *bits), (8000, 1, 8), "{what}"),
            other => panic!("{what}: audio track {other:?}"),
        }
        // Five picture slots at 10 fps.
        let duration = d.info.duration.expect("duration");
        assert!((duration - 0.5).abs() < 1e-9, "{what}: duration {duration}");

        // The empty chunk at 0.2 s is a dropped frame: it takes its slot and yields no packet.
        let video_pts: Vec<f64> = d.video.iter().map(|(pts, _)| *pts).collect();
        assert_pts(&video_pts, &[0.0, 0.1, 0.3, 0.4], &format!("{what} video"));
        let payloads: Vec<&[u8]> = d.video.iter().map(|(_, data)| &data[..]).collect();
        assert_eq!(payloads, [vop(true, 0xA0, 7), vop(false, 0xA1, 6), vop(true, 0xA3, 9), vop(false, 0xA4, 8)], "{what}");

        let audio_pts: Vec<f64> = d.audio.iter().map(|(pts, _)| *pts).collect();
        assert_pts(&audio_pts, &[0.0, 0.1, 0.2, 0.3, 0.4], &format!("{what} audio"));
        for (n, (_, packet)) in d.audio.iter().enumerate() {
            assert!(packet.data[..] == pcm_chunk(n as u8)[..], "{what}: audio chunk {n} payload differs");
        }
    }
}

#[test]
fn seek_lands_on_the_last_keyframe_at_or_before_the_target_with_every_kind_of_index() {
    let avi = av_file();
    // Keyframes are at 0.0 and 0.3; 0.1 and 0.4 are predicted pictures, 0.2 is a dropped frame.
    let cases = [(0.0, 0.0, 0xA0), (0.1, 0.0, 0xA0), (0.25, 0.0, 0xA0), (0.3, 0.3, 0xA3), (0.35, 0.3, 0xA3), (9.0, 0.3, 0xA3), (-2.0, 0.0, 0xA0)];
    for index in ALL_INDEXES {
        let mut demux = open_built(&format!("crafted_seek_{index:?}.avi"), &avi.build(index));
        for (target, want, tag) in cases {
            let what = format!("{index:?} seek({target})");
            let landed = demux.seek(target).unwrap_or_else(|e| panic!("{what}: {e}"));
            assert!((landed - want).abs() < 1e-9, "{what}: landed at {landed}, expected {want}");

            let (mut video, mut audio) = (None, None);
            while video.is_none() || audio.is_none() {
                match demux.next_packet().unwrap().unwrap_or_else(|| panic!("{what}: media ended")) {
                    Packet::Video { pts, data } if video.is_none() => video = Some((pts, data)),
                    Packet::Audio { pts, .. } if audio.is_none() => audio = Some(pts),
                    _ => {}
                }
            }
            let (pts, data) = video.unwrap();
            assert!((pts - want).abs() < 1e-9, "{what}: first video packet at {pts}");
            assert_eq!(data[5], tag, "{what}: first video packet is not the keyframe's payload");
            assert!((audio.unwrap() - want).abs() < 1e-9, "{what}: first audio packet at {audio:?}, expected the chunk at {want}");
        }
    }
}

#[test]
fn video_codec_comes_from_the_stream_format_with_the_stream_handler_as_fallback() {
    // (strh handler, strf compression, expected codec)
    let cases: [(&[u8; 4], &[u8; 4], VideoCodec); 12] = [
        (b"XVID", b"XVID", VideoCodec::Mpeg4),
        (b"xvid", b"xvid", VideoCodec::Mpeg4),
        (b"divx", b"DX50", VideoCodec::Mpeg4),
        (b"FMP4", b"FMP4", VideoCodec::Mpeg4),
        (b"H264", b"H264", VideoCodec::H264),
        (b"avc1", b"avc1", VideoCodec::H264),
        (b"hevc", b"hvc1", VideoCodec::Hevc),
        (b"MJPG", b"MJPG", VideoCodec::Unsupported("MJPG".into())),
        (b"iv50", b"IV50", VideoCodec::Unsupported("IV50".into())),
        // The two tags disagree: the format wins when it is a known codec ...
        (b"MJPG", b"XVID", VideoCodec::Mpeg4),
        (b"XVID", b"H264", VideoCodec::H264),
        // ... and the handler is used when the format's tag is blank.
        (b"XVID", &[0, 0, 0, 0], VideoCodec::Mpeg4),
    ];
    for (handler, compression, expected) in cases {
        let avi = Avi {
            streams: vec![(strh(b"vids", handler, 1, 25, 0), bitmap_info(320, 240, compression))],
            chunks: vec![(*b"00dc", vop(true, 0x77, 16), true)],
        };
        let what = format!("handler {:?}, compression {:?}", String::from_utf8_lossy(handler), String::from_utf8_lossy(compression));
        let demux = open_built("crafted_fourcc.avi", &avi.build(Index::Relative));
        let track = demux.info().video.as_ref().unwrap_or_else(|| panic!("{what}: no video track"));
        assert_eq!(track.codec, expected, "{what}");
        assert_eq!((track.width, track.height), (320, 240), "{what}");
    }
}

#[test]
fn a_stream_format_naming_an_unsupported_codec_is_not_overruled_by_the_stream_handler() {
    // The format says Motion JPEG; an XVID handler tag next to it must not send JPEG data to
    // the MPEG-4 decoder and hide the "no decoder" note from the user.
    let avi = Avi {
        streams: vec![(strh(b"vids", b"XVID", 1, 25, 0), bitmap_info(320, 240, b"MJPG"))],
        chunks: vec![(*b"00dc", vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46], true)],
    };
    let demux = open_built("crafted_strf_mjpg_strh_xvid.avi", &avi.build(Index::Relative));

    let track = demux.info().video.as_ref().expect("video track");

    assert_eq!(track.codec, VideoCodec::Unsupported("MJPG".into()));
    assert_eq!(playr::vdec::make_decoder(track).err().as_deref(), Some("no decoder for MJPG video"));
}

#[test]
fn pcm_audio_of_every_supported_width_is_offered_and_decodes() {
    // One stereo frame per width: left at negative full scale, right at +0.5.
    let cases: [(u16, &[u8]); 4] = [
        (8, &[0x00, 0xC0]),
        (16, &[0x00, 0x80, 0x00, 0x40]),
        (24, &[0x00, 0x00, 0x80, 0x00, 0x00, 0x40]),
        (32, &[0x00, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00, 0x40]),
    ];
    for (bits, frame) in cases {
        let block = 2 * bits / 8;
        let data = [frame, frame, frame].concat();
        let avi = Avi {
            streams: vec![(
                strh(b"auds", &[0; 4], block as u32, 48000 * block as u32, block as u32),
                wave_format(1, 2, 48000, 48000 * block as u32, block, bits),
            )],
            chunks: vec![(*b"00wb", data.clone(), true), (*b"00wb", data.clone(), true)],
        };
        let mut demux = open_built(&format!("crafted_pcm{bits}.avi"), &avi.build(Index::Relative));
        let (d, err) = drain(demux.as_mut());
        assert_eq!(err, None, "{bits} bit");

        match d.info.audio.as_ref().unwrap_or_else(|| panic!("{bits} bit: no audio track, notes {:?}", d.info.notes)) {
            AudioTrack::Pcm { rate, channels, bits: got } => assert_eq!((*rate, *channels, *got), (48000, 2, bits)),
            other => panic!("{bits} bit: audio track {other:?}"),
        }
        assert!(d.info.video.is_none());
        let audio = decode_audio(&d);
        assert_eq!((audio.rate, audio.channels), (48000, 2), "{bits} bit");
        assert_eq!(audio.samples, [-1.0, 0.5].repeat(6), "{bits} bit");
        let pts: Vec<f64> = d.audio.iter().map(|(p, _)| *p).collect();
        // Three frames per chunk at 48 kHz.
        assert_pts(&pts, &[0.0, 3.0 / 48000.0], &format!("{bits} bit"));
    }
}

#[test]
fn audio_only_avi_duration_covers_the_last_chunk() {
    // Five chunks of 0.1 s of 8 kHz 8-bit mono PCM: half a second of sound.
    let avi = Avi {
        streams: vec![(strh(b"auds", &[0; 4], 1, 8000, 1), wave_format(1, 1, 8000, 8000, 1, 8))],
        chunks: (0..5).map(|n| (*b"00wb", pcm_chunk(n), true)).collect(),
    };
    let demux = open_built("crafted_audio_only.avi", &avi.build(Index::Relative));

    let duration = demux.info().duration.expect("an audio-only file has a duration");

    assert!((duration - 0.5).abs() < 1e-6, "duration {duration}, expected 0.5");
}

#[test]
fn mpeg_audio_frames_split_across_chunks_come_out_one_frame_per_packet() {
    // radio.mp3 is 155 bare MPEG-1 Layer III frames (64 kbit/s, 44.1 kHz, mono). Cutting it
    // every 500 bytes puts every chunk boundary inside a frame.
    let mp3 = fixture_bytes("radio.mp3");
    let avi = Avi {
        streams: vec![(strh(b"auds", &[0; 4], 1, 8000, 1), wave_format(0x55, 1, 44100, 8000, 1, 0))],
        chunks: mp3.chunks(500).map(|c| (*b"00wb", c.to_vec(), true)).collect(),
    };
    let mut demux = open_built("crafted_mp3.avi", &avi.build(Index::Relative));
    match demux.info().audio.as_ref().expect("audio track") {
        AudioTrack::Symphonia(p) => assert_eq!((p.codec, p.sample_rate), (CODEC_ID_MP3, Some(44100))),
        other => panic!("audio track {other:?}"),
    }
    let (d, err) = drain(demux.as_mut());
    assert_eq!(err, None);

    assert_eq!(d.audio.len(), 155, "one packet per MPEG frame");
    let rejoined: Vec<u8> = d.audio.iter().flat_map(|(_, p)| p.data.to_vec()).collect();
    assert!(rejoined == mp3, "the frames, put back together, are not the original stream");
    for (n, (pts, packet)) in d.audio.iter().enumerate() {
        assert_eq!(packet.data[0], 0xFF, "packet {n} does not start at a frame header");
        let want = n as f64 * 1152.0 / 44100.0;
        assert!((pts - want).abs() < 1e-6, "packet {n} at {pts}, expected {want}");
    }
    let audio = decode_audio(&d);
    assert_eq!((audio.rate, audio.channels, audio.failed_packets), (44100, 1, 0));
    assert_eq!(audio.frames(), 155 * 1152);
    let cycles = tone_cycles(&audio.samples, 0.02) as f64;
    assert!((cycles - 1760.0).abs() <= 35.0, "{cycles} cycles of the 440 Hz tone in 4 s, expected 1760 +/- 2%");
}

#[test]
fn files_without_usable_content_are_rejected_with_a_reason() {
    let no_streams = {
        let hdrl = list(b"hdrl", &chunk(b"avih", &[0u8; 56]));
        let movi = list(b"movi", &chunk(b"00dc", &vop(true, 1, 16)));
        chunk(b"RIFF", &[&b"AVI "[..], &hdrl, &movi].concat())
    };
    let no_movi = {
        let avi = Avi { streams: vec![(strh(b"vids", b"XVID", 1, 25, 0), bitmap_info(64, 48, b"XVID"))], chunks: Vec::new() };
        chunk(b"RIFF", &[&b"AVI "[..], &avi.header_list()].concat())
    };
    let header_only = chunk(b"RIFF", b"AVI ");
    let cases = [
        ("no_streams", no_streams, "AVI has no playable streams"),
        ("no_movi", no_movi, "AVI has no movi list"),
        ("header_only", header_only, "AVI has no movi list"),
    ];
    for (name, bytes, message) in cases {
        let result = open_path(&tmp_file(&format!("crafted_{name}.avi"), &bytes));
        assert_eq!(result.err().as_deref(), Some(message), "{name}");
    }
}
