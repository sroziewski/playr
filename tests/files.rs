//! Real files through `playr::demux` and the decoders, checked against FFmpeg.
//!
//! The fixtures and their reference data come from `tests/fixtures/generate.ps1`: every video
//! is 320x240 at 25 fps for 2 s (50 pictures), every audio track a 440 Hz sine. Expected
//! pictures and timestamps are FFmpeg's (`*.framecrc`, `*.f0.yuv`, `*.f37.yuv`); expected
//! audio lengths and levels are what `ffmpeg -i <fixture> -f f32le` / `-af astats` report.

mod common;

use std::time::Duration;

use common::*;
use playr::demux::open_path;
use playr::media::{AudioTrack, VideoCodec};
use playr::vdec::{is_keyframe, make_decoder};

struct VideoCase {
    file: &'static str,
    container: &'static str,
    codec: VideoCodec,
    /// Seconds, as reported by ffprobe for the container.
    duration: f64,
}

fn video_cases() -> Vec<VideoCase> {
    let case = |file, container, codec, duration| VideoCase { file, container, codec, duration };
    vec![
        case("h264_aac.mp4", "isomp4", VideoCodec::H264, 2.0),
        case("h264_bframes_aac.mp4", "isomp4", VideoCodec::H264, 2.0),
        case("h264_vorbis.mkv", "matroska", VideoCodec::H264, 2.0),
        case("hevc_vorbis.mkv", "matroska", VideoCodec::Hevc, 2.0),
        case("hevc_aac.mp4", "isomp4", VideoCodec::Hevc, 2.0),
        case("xvid_mp3.avi", "avi", VideoCodec::Mpeg4, 2.04),
        case("xvid_pcm16.avi", "avi", VideoCodec::Mpeg4, 2.0),
    ]
}

struct AudioCase {
    file: &'static str,
    container: &'static str,
    rate: u32,
    channels: usize,
    /// Sample frames FFmpeg decodes from the fixture.
    frames: usize,
    /// Samples per codec frame: the tolerance on `frames`. Zero for lossless formats.
    codec_frame: usize,
    /// Nominal length of the tone in seconds.
    secs: f64,
    /// RMS level of FFmpeg's decode in dBFS (`astats`). The source is a sine of amplitude 1/8;
    /// stereo fixtures are FFmpeg's upmix (-3 dB) and LAME attenuates a little more.
    rms_db: f64,
}

fn audio_cases() -> Vec<AudioCase> {
    let case = |file, container, rate, channels, frames, codec_frame, secs, rms_db| AudioCase {
        file,
        container,
        rate,
        channels,
        frames,
        codec_frame,
        secs,
        rms_db,
    };
    vec![
        case("sine.flac", "flac", 44100, 2, 132_300, 0, 3.0, -24.08),
        case("sine.wav", "wave", 44100, 1, 132_300, 0, 3.0, -21.07),
        case("sine.mp3", "mp3", 44100, 2, 132_300, 1152, 3.0, -24.53),
        case("sine.aac", "aac", 44100, 1, 134_144, 1024, 3.0, -21.14),
        case("sine.m4a", "isomp4", 44100, 2, 132_300, 1024, 3.0, -24.09),
        case("sine.ogg", "ogg", 44100, 2, 132_300, 1024, 3.0, -24.04),
        case("sine.opus", "ogg", 48000, 2, 144_000, 960, 3.0, -24.06),
        case("h264_aac.mp4", "isomp4", 44100, 1, 88_200, 1024, 2.0, -21.07),
        case("h264_bframes_aac.mp4", "isomp4", 44100, 1, 88_200, 1024, 2.0, -21.07),
        case("hevc_aac.mp4", "isomp4", 44100, 1, 88_200, 1024, 2.0, -21.07),
        case("h264_vorbis.mkv", "matroska", 44100, 2, 88_200, 1024, 2.0, -24.04),
        case("hevc_vorbis.mkv", "matroska", 44100, 2, 88_200, 1024, 2.0, -24.04),
        case("vp9_vorbis.mkv", "matroska", 44100, 2, 88_200, 1024, 2.0, -24.04),
        case("xvid_mp3.avi", "avi", 44100, 2, 89_856, 1152, 2.0, -24.61),
        case("xvid_pcm16.avi", "avi", 44100, 2, 88_200, 0, 2.0, -24.08),
        case("xvid_pcm8.avi", "avi", 44100, 1, 88_200, 0, 2.0, -21.11),
        case("mjpeg_pcm16.avi", "avi", 44100, 1, 88_200, 0, 2.0, -21.07),
    ]
}

fn is_aac_in_mp4(case: &AudioCase) -> bool {
    case.container == "isomp4"
}

#[test]
fn reference_pictures_and_checksums_agree_with_each_other() {
    // Guards the oracle itself: the Adler-32 used below must be the one FFmpeg wrote.
    let mut bad = Vec::new();
    for case in video_cases() {
        let crcs = read_framecrc(case.file);
        expect!(bad, crcs.len() == 50, "{}: {} reference pictures, expected 50", case.file, crcs.len());
        for n in [0usize, 37] {
            let raw = fixture_bytes(&format!("{}.f{n}.yuv", case.file));
            expect!(bad, raw.len() == 320 * 240 * 3 / 2, "{}: picture {n} has {} bytes", case.file, raw.len());
            expect!(
                bad,
                adler32_ffmpeg(&raw) == crcs[n].1,
                "{}: checksum of reference picture {n} is {:#010x}, framecrc says {:#010x}",
                case.file,
                adler32_ffmpeg(&raw),
                crcs[n].1
            );
        }
    }
    finish(bad);
}

#[test]
fn video_files_report_container_codec_size_duration_and_seekability() {
    let mut bad = Vec::new();
    for case in video_cases() {
        let demux = open_path(&fixture(case.file)).unwrap_or_else(|e| panic!("open {}: {e}", case.file));
        let info = demux.info();
        expect!(bad, info.container == case.container, "{}: container {:?}, expected {:?}", case.file, info.container, case.container);
        match &info.video {
            Some(v) => {
                expect!(bad, v.codec == case.codec, "{}: codec {:?}, expected {:?}", case.file, v.codec, case.codec);
                expect!(bad, (v.width, v.height) == (320, 240), "{}: size {}x{}, expected 320x240", case.file, v.width, v.height);
            }
            None => bad.push(format!("{}: no video track reported", case.file)),
        }
        expect!(bad, info.audio.is_some(), "{}: no audio track reported", case.file);
        match info.duration {
            Some(d) => expect!(bad, (d - case.duration).abs() <= 0.1, "{}: duration {d}, expected {} +/- 0.1", case.file, case.duration),
            None => bad.push(format!("{}: no duration reported, expected {}", case.file, case.duration)),
        }
        expect!(bad, info.seekable, "{}: a local file must be seekable", case.file);
        expect!(bad, info.notes.is_empty(), "{}: unexpected notes {:?}", case.file, info.notes);
    }
    finish(bad);
}

#[test]
fn audio_tracks_report_container_rate_duration_and_seekability() {
    let mut bad = Vec::new();
    for case in audio_cases() {
        let demux = open_path(&fixture(case.file)).unwrap_or_else(|e| panic!("open {}: {e}", case.file));
        let info = demux.info();
        expect!(bad, info.container == case.container, "{}: container {:?}, expected {:?}", case.file, info.container, case.container);
        match &info.audio {
            Some(AudioTrack::Symphonia(p)) => {
                expect!(bad, p.sample_rate == Some(case.rate), "{}: sample rate {:?}, expected {}", case.file, p.sample_rate, case.rate);
                expect!(bad, case.container != "avi" || case.file == "xvid_mp3.avi", "{}: PCM in AVI must be reported as a PCM track", case.file);
            }
            Some(AudioTrack::Pcm { rate, channels, bits }) => {
                let want_bits = if case.file == "xvid_pcm8.avi" { 8 } else { 16 };
                expect!(
                    bad,
                    (*rate, *channels as usize, *bits) == (case.rate, case.channels, want_bits),
                    "{}: PCM {rate} Hz, {channels} ch, {bits} bit; expected {} Hz, {} ch, {want_bits} bit",
                    case.file,
                    case.rate,
                    case.channels
                );
            }
            None => bad.push(format!("{}: no audio track reported", case.file)),
        }
        match info.duration {
            Some(d) => expect!(bad, d >= case.secs - 0.1 && d <= case.secs + 0.1, "{}: duration {d}, expected {} +/- 0.1", case.file, case.secs),
            None => bad.push(format!("{}: no duration reported, expected about {}", case.file, case.secs)),
        }
        expect!(bad, info.seekable, "{}: a local file must be seekable", case.file);
    }
    finish(bad);
}

#[test]
fn every_video_packet_decodes_and_pictures_come_out_in_presentation_order() {
    let mut bad = Vec::new();
    for case in video_cases() {
        let reference = read_framecrc(case.file);
        let demuxed = demux_file(case.file);
        expect!(bad, demuxed.video.len() == 50, "{}: {} video packets, expected 50", case.file, demuxed.video.len());

        let decoded = decode_video(&demuxed);
        expect!(bad, decoded.errors == 0, "{}: {} decoder errors, last: {:?}", case.file, decoded.errors, decoded.last_error);
        expect!(
            bad,
            decoded.frames.len() == reference.len(),
            "{}: {} pictures decoded, FFmpeg decodes {}",
            case.file,
            decoded.frames.len(),
            reference.len()
        );
        let pts: Vec<f64> = decoded.frames.iter().map(|f| f.pts).collect();
        expect!(bad, pts.windows(2).all(|w| w[0] < w[1]), "{}: picture timestamps are not strictly ascending: {pts:?}", case.file);
        for (n, frame) in decoded.frames.iter().enumerate() {
            let dims = (frame.width, frame.height, frame.chroma_width, frame.chroma_height);
            expect!(bad, dims == (320, 240, 160, 120), "{}: picture {n} dimensions {dims:?}", case.file);
        }
    }
    finish(bad);
}

/// One line per picture whose timestamp differs from FFmpeg's by more than a millisecond.
fn timestamp_mismatches(file: &str, pace: Duration, bad: &mut Vec<String>) {
    let reference = read_framecrc(file);
    let frames = decode_video_paced(&demux_file(file), pace).frames;
    expect!(bad, frames.len() == reference.len(), "{file}: {} pictures decoded, FFmpeg decodes {}", frames.len(), reference.len());
    let wrong: Vec<String> = frames
        .iter()
        .zip(&reference)
        .enumerate()
        .filter(|(_, (frame, (want, _)))| (frame.pts - want).abs() > 0.001)
        .map(|(n, (frame, (want, _)))| format!("#{n}: {:.4} (FFmpeg {want:.4})", frame.pts))
        .collect();
    if let Some(first) = wrong.first() {
        bad.push(format!(
            "{file}: {} of {} picture timestamps differ from FFmpeg; first {first}, last {}",
            wrong.len(),
            reference.len(),
            wrong.last().unwrap()
        ));
    }
}

const H264_FILES: [&str; 5] =
    ["h264_aac.mp4", "h264_bframes_aac.mp4", "h264_vorbis.mkv", "h264_longgop_aac.mp4", "h264_1080p_aac.mp4"];

#[test]
fn hevc_and_mpeg4_picture_timestamps_match_ffmpeg() {
    let mut bad = Vec::new();
    for file in ["hevc_vorbis.mkv", "hevc_aac.mp4", "xvid_mp3.avi", "xvid_pcm16.avi"] {
        timestamp_mismatches(file, Duration::ZERO, &mut bad);
    }
    finish(bad);
}

#[test]
fn h264_picture_timestamps_match_ffmpeg_when_the_decoder_keeps_up() {
    let mut bad = Vec::new();
    for file in H264_FILES {
        // Full HD gets one picture period (25 fps); the small pictures decode in far less.
        let pace = if file == "h264_1080p_aac.mp4" { 40 } else { 3 };
        timestamp_mismatches(file, Duration::from_millis(pace), &mut bad);
    }
    finish(bad);
}

#[test]
fn h264_picture_timestamps_match_ffmpeg_when_packets_arrive_as_fast_as_they_are_read() {
    // This is how the pipeline feeds the decoder after open and after a seek, while the frame
    // queue is still empty. The decoder works on other threads, so three rounds are run.
    let mut bad = Vec::new();
    for round in 1..=3 {
        let before = bad.len();
        for file in H264_FILES {
            timestamp_mismatches(file, Duration::ZERO, &mut bad);
        }
        for line in &mut bad[before..] {
            line.insert_str(0, &format!("round {round}: "));
        }
    }
    finish(bad);
}

/// Decodes `file` with the given pauses between packets and lists every picture whose
/// timestamp or content is not what FFmpeg has at that place in the sequence.
fn mistimed_pictures(file: &str, feeding: &str, pause_after: impl Fn(usize) -> Duration, bad: &mut Vec<String>) {
    let reference = read_framecrc(file);
    let frames = decode_video_with_pauses(&demux_file(file), pause_after).frames;
    expect!(bad, frames.len() == reference.len(), "{file}, {feeding}: {} pictures decoded, FFmpeg decodes {}", frames.len(), reference.len());
    let wrong: Vec<String> = frames
        .iter()
        .zip(&reference)
        .enumerate()
        .filter(|(_, (frame, (pts, crc)))| (frame.pts - pts).abs() > 0.001 || adler32_ffmpeg(&packed_yuv(frame)) != *crc)
        .map(|(n, (frame, (pts, _)))| format!("#{n} at {:.2} (FFmpeg {pts:.2})", frame.pts))
        .collect();
    if !wrong.is_empty() {
        let shown = wrong.iter().take(6).cloned().collect::<Vec<_>>().join(", ");
        bad.push(format!("{file}, {feeding}: {} of {} pictures mistimed or wrong: {shown}", wrong.len(), reference.len()));
    }
}

#[test]
fn short_groups_of_pictures_keep_their_timestamps_however_fast_packets_are_fed() {
    // Every picture an IDR (H.264 and HEVC), and an IDR every fourth picture: many IDR
    // packets are inside the decoder at once, and how many depends on the feeding rhythm.
    let mut bad = Vec::new();
    for file in ["h264_intra_aac.mp4", "h264_gop4_aac.mp4", "hevc_intra_aac.mp4"] {
        mistimed_pictures(file, "as fast as read", |_| Duration::ZERO, &mut bad);
        mistimed_pictures(file, "paced", |_| Duration::from_millis(3), &mut bad);
        // Seventeen packets at once, then a pause long enough for the decoder to drain.
        mistimed_pictures(file, "in bursts", |n| Duration::from_millis(if n % 17 == 16 { 60 } else { 0 }), &mut bad);
        // A slow start that turns into a flood, and back.
        mistimed_pictures(file, "slow, then fast, then slow", |n| Duration::from_millis(if (30..70).contains(&n) { 0 } else { 5 }), &mut bad);
    }
    finish(bad);
}

#[test]
fn b_frame_packets_arrive_out_of_presentation_order_and_leave_the_decoder_in_order() {
    let demuxed = demux_file("h264_vorbis.mkv");
    let packet_pts: Vec<f64> = demuxed.video.iter().map(|(pts, _)| *pts).collect();
    assert!(
        packet_pts.windows(2).any(|w| w[0] > w[1]),
        "fixture precondition: the B-frame file must store packets out of presentation order, got {packet_pts:?}"
    );

    let frames = decode_video_paced(&demuxed, Duration::from_millis(3)).frames;

    let expected: Vec<f64> = (0..50).map(|n| n as f64 / 25.0).collect();
    let got: Vec<f64> = frames.iter().map(|f| f.pts).collect();
    assert_eq!(got.len(), expected.len(), "pictures: {got:?}");
    for (n, (g, e)) in got.iter().zip(&expected).enumerate() {
        assert!((g - e).abs() <= 0.001, "picture {n}: pts {g}, expected {e} (all: {got:?})");
    }
}

/// (picture count, indices whose checksum differs from FFmpeg's, the two pictures kept as raw
/// planes compared sample by sample: [(index, differing samples, largest difference)]).
fn compare_pictures_with_ffmpeg(file: &str) -> (usize, Vec<usize>, Vec<(usize, usize, u8)>) {
    let reference = read_framecrc(file);
    let decoded = decode_video(&demux_file(file));
    let packed: Vec<Vec<u8>> = decoded.frames.iter().map(packed_yuv).collect();
    let differing = packed
        .iter()
        .zip(&reference)
        .enumerate()
        .filter(|(_, (p, (_, crc)))| adler32_ffmpeg(p) != *crc)
        .map(|(n, _)| n)
        .collect();
    let raw = [0usize, 37]
        .into_iter()
        .filter_map(|n| {
            let (count, max) = diff_stats(packed.get(n)?, &fixture_bytes(&format!("{file}.f{n}.yuv")));
            Some((n, count, max))
        })
        .collect();
    (packed.len(), differing, raw)
}

#[test]
fn h264_and_hevc_pictures_are_bit_exact_with_ffmpeg() {
    // Both standards define decoding exactly, so any difference is a decoder defect.
    let mut bad = Vec::new();
    for case in video_cases().iter().filter(|c| c.codec != VideoCodec::Mpeg4) {
        let (count, differing, raw) = compare_pictures_with_ffmpeg(case.file);
        expect!(bad, count == 50, "{}: {count} pictures, expected 50", case.file);
        expect!(
            bad,
            differing.is_empty(),
            "{}: pictures {differing:?} differ from FFmpeg; (picture, differing samples, largest difference): {raw:?}",
            case.file
        );
    }
    finish(bad);
}

#[test]
fn mpeg4_pictures_match_ffmpeg_within_the_idct_tolerance_of_the_standard() {
    // MPEG-4 Part 2 does not fix the inverse DCT bit for bit (it only bounds its error, as
    // IEEE 1180 did), so two conforming decoders may differ by one step per transform. An
    // intra picture goes through one transform, the predicted picture right after an intra
    // picture (picture 37 follows the intra picture 36) through two.
    let mut bad = Vec::new();
    for case in video_cases().iter().filter(|c| c.codec == VideoCodec::Mpeg4) {
        let (count, _, raw) = compare_pictures_with_ffmpeg(case.file);
        expect!(bad, count == 50, "{}: {count} pictures, expected 50", case.file);
        expect!(bad, raw.len() == 2, "{}: reference pictures 0 and 37 were not both decoded", case.file);
        for (n, differing, largest) in raw {
            let allowed = if n == 0 { 1 } else { 2 };
            expect!(bad, largest <= allowed, "{}: picture {n} differs from FFmpeg by up to {largest}, allowed {allowed}", case.file);
            expect!(bad, differing * 100 < 320 * 240 * 3 / 2, "{}: picture {n} differs in {differing} samples, more than 1%", case.file);
        }
    }
    finish(bad);
}

#[test]
fn audio_decodes_to_the_expected_format_and_tone() {
    let mut bad = Vec::new();
    for case in audio_cases() {
        let demuxed = demux_file(case.file);
        let audio = decode_audio(&demuxed);
        expect!(bad, audio.failed_packets == 0, "{}: {} of {} audio packets failed to decode", case.file, audio.failed_packets, demuxed.audio.len());
        expect!(
            bad,
            (audio.rate, audio.channels) == (case.rate, case.channels),
            "{}: decoded {} Hz, {} ch; expected {} Hz, {} ch",
            case.file,
            audio.rate,
            audio.channels,
            case.rate,
            case.channels
        );
        for c in 0..audio.channels {
            let ch = audio.channel(c);
            let cycles = tone_cycles(&ch, 0.02) as f64;
            let want = 440.0 * case.secs;
            expect!(bad, (cycles - want).abs() <= want * 0.02, "{}: channel {c} has {cycles} tone cycles, expected {want} +/- 2%", case.file);
            let level = 20.0 * rms(&ch).log10();
            expect!(bad, (level - case.rms_db).abs() <= 0.3, "{}: channel {c} RMS {level:.2} dBFS, FFmpeg measures {} +/- 0.3", case.file, case.rms_db);
        }
    }
    finish(bad);
}

/// Sample frames that are due at or after time zero: the pipeline discards what a stream
/// carries before that (encoder delay announced through a negative first timestamp).
fn check_audio_length(case: &AudioCase, bad: &mut Vec<String>) {
    let demuxed = demux_file(case.file);
    let audio = decode_audio(&demuxed);
    let first_pts = demuxed.audio.first().map_or(0.0, |(pts, _)| *pts);
    let before_zero = ((-first_pts).max(0.0) * case.rate as f64).round() as usize;
    let playable = audio.frames().saturating_sub(before_zero);
    expect!(
        bad,
        playable.abs_diff(case.frames) <= case.codec_frame,
        "{}: {playable} sample frames to play ({} decoded, {before_zero} before time zero), FFmpeg decodes {} (tolerance: one codec frame, {})",
        case.file,
        audio.frames(),
        case.frames,
        case.codec_frame
    );
}

#[test]
fn audio_length_is_within_one_codec_frame_of_ffmpeg() {
    let mut bad = Vec::new();
    for case in audio_cases().iter().filter(|c| !is_aac_in_mp4(c)) {
        check_audio_length(case, &mut bad);
    }
    finish(bad);
}

#[test]
fn aac_in_mp4_audio_length_is_within_one_codec_frame_of_ffmpeg() {
    let mut bad = Vec::new();
    for case in audio_cases().iter().filter(|c| is_aac_in_mp4(c)) {
        check_audio_length(case, &mut bad);
    }
    finish(bad);
}

#[test]
fn audio_timestamps_start_at_zero_and_ascend() {
    let mut bad = Vec::new();
    for case in audio_cases() {
        let demuxed = demux_file(case.file);
        let pts: Vec<f64> = demuxed.audio.iter().map(|(pts, _)| *pts).collect();
        let Some((first, last)) = pts.first().zip(pts.last()) else {
            bad.push(format!("{}: no audio packets", case.file));
            continue;
        };
        // An encoder may start up to one codec frame early or late (priming).
        expect!(bad, first.abs() <= 0.03, "{}: first audio packet at {first} s, expected 0 +/- 0.03", case.file);
        expect!(bad, pts.windows(2).all(|w| w[0] <= w[1]), "{}: audio timestamps go backwards", case.file);
        expect!(
            bad,
            *last <= case.secs + 0.05 && *last >= case.secs - 0.15,
            "{}: last audio packet at {last} s, expected just before {}",
            case.file,
            case.secs
        );
    }
    finish(bad);
}

#[test]
fn unsupported_video_codecs_are_named_and_their_audio_still_decodes() {
    let mut bad = Vec::new();
    for (file, codec_name, audio_frames) in [("mjpeg_pcm16.avi", "MJPG", 88_200usize), ("vp9_vorbis.mkv", "VP9", 88_200)] {
        let demuxed = demux_file(file);
        match &demuxed.info.video {
            Some(track) => {
                expect!(
                    bad,
                    track.codec == VideoCodec::Unsupported(codec_name.to_string()),
                    "{file}: codec {:?}, expected Unsupported({codec_name:?})",
                    track.codec
                );
                let err = make_decoder(track).err();
                let want = format!("no decoder for {codec_name} video");
                expect!(bad, err.as_deref() == Some(want.as_str()), "{file}: make_decoder gave {err:?}, expected Err({want:?})");
            }
            None => bad.push(format!("{file}: the unsupported video track is not reported at all")),
        }
        let audio = decode_audio(&demuxed);
        expect!(bad, audio.frames().abs_diff(audio_frames) <= 1024, "{file}: {} audio frames, expected about {audio_frames}", audio.frames());
    }
    finish(bad);
}

#[test]
fn avi_with_undecodable_picture_and_sound_reports_both_reasons() {
    let demuxed = demux_file("mjpeg_ac3.avi");

    assert_eq!(demuxed.info.video.as_ref().map(|v| v.codec.clone()), Some(VideoCodec::Unsupported("MJPG".into())));
    assert!(demuxed.info.audio.is_none(), "AC-3 has no decoder and must not be offered as a track");
    // 0x2000 is the WAVE format tag of AC-3.
    assert_eq!(demuxed.info.notes, ["audio format 0x2000 in AVI is not supported"]);
    assert_eq!(demuxed.audio.len(), 0);
}

#[test]
fn hevc_in_mp4_with_an_oversized_configuration_record_is_rejected_with_a_message() {
    // x265's default hvcC (with its version SEI) exceeds what the MP4 reader accepts. For now
    // the accepted behaviour is a reported error; the reason in brackets is the format
    // library's wording.
    let err = open_path(&fixture("hevc_big_hvcc_aac.mp4")).err().expect("the file is not expected to open");
    let prefix = "unrecognised or unsupported format (";
    assert!(err.starts_with(prefix) && err.ends_with(')') && err.len() > prefix.len() + 1, "{err:?}");
}

/// `sine.m4a` (131 AAC packets: 130 of 1024 frames, a last one of 204, 1024 frames of priming)
/// rewritten so that its audio track counts time in `timescale` ticks per second while the
/// samples stay at 44.1 kHz. `packet`, `last_packet` and `priming` are those three lengths in
/// ticks; they go into `mdhd`/`stts` and the media time of the edit list.
fn sine_m4a_with_timescale(timescale: u32, packet: u32, last_packet: u32, priming: u32) -> std::path::PathBuf {
    let mut bytes = fixture_bytes("sine.m4a");
    let find = |bytes: &[u8], kind: &[u8; 4]| bytes.windows(4).position(|w| w == kind).expect("box present") + 4;
    let get = |bytes: &[u8], at: usize| u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap());
    let put = |bytes: &mut [u8], at: usize, value: u32| bytes[at..at + 4].copy_from_slice(&value.to_be_bytes());

    let mdhd = find(&bytes, b"mdhd");
    assert_eq!((bytes[mdhd], get(&bytes, mdhd + 12)), (0, 44_100), "fixture precondition: version 0 mdhd at 44100");
    put(&mut bytes, mdhd + 12, timescale);
    put(&mut bytes, mdhd + 16, 130 * packet + last_packet);
    let stts = find(&bytes, b"stts");
    let entries = [get(&bytes, stts + 4), get(&bytes, stts + 8), get(&bytes, stts + 12), get(&bytes, stts + 16), get(&bytes, stts + 20)];
    assert_eq!(entries, [2, 130, 1024, 1, 204], "fixture precondition: 130 packets of 1024 frames and one of 204");
    put(&mut bytes, stts + 12, packet);
    put(&mut bytes, stts + 20, last_packet);
    let elst = find(&bytes, b"elst");
    assert_eq!((bytes[elst], get(&bytes, elst + 4), get(&bytes, elst + 12)), (0, 1, 1024), "fixture precondition: one edit at 1024");
    put(&mut bytes, elst + 12, priming);
    tmp_file(&format!("sine_timescale_{timescale}.m4a"), &bytes)
}

#[test]
fn mp4_audio_with_a_timescale_of_half_the_sample_rate_is_trimmed_exactly() {
    // Every length is a whole number of ticks here (512, 102 and 512), so nothing is lost to
    // rounding: FFmpeg decodes 132300 sample frames from this file, as from the original.
    let path = sine_m4a_with_timescale(22_050, 512, 102, 512);
    assert!((playr::demux::mp4_audio_priming_secs(&path) - 1024.0 / 44_100.0).abs() < 1e-9);

    let mut demux = open_path(&path).expect("open");
    let duration = demux.info().duration.expect("duration");
    let (demuxed, err) = drain(demux.as_mut());
    assert_eq!(err, None);

    // Skip and limit count decoded sample frames (44.1 kHz), not ticks. A timescale coarser
    // than the sample rate cannot say exactly how long an ordinary packet is, so only the
    // clearly short last packet is limited.
    let trims = &demuxed.audio_trim;
    assert_eq!(trims.len(), 131);
    assert_eq!(trims[0], (1024, None), "first packet: all priming");
    assert_eq!(trims[1..130].iter().filter(|t| **t != (0, None)).count(), 0, "ordinary packets are played whole");
    assert_eq!(trims[130], (0, Some(204)), "last packet: 204 frames before the padding");
    let audio = decode_audio(&demuxed);
    assert_eq!((audio.rate, audio.channels), (44_100, 2));
    assert_eq!(audio.frames(), 132_300, "FFmpeg decodes 132300 sample frames from this file");
    assert!((duration - 3.0).abs() <= 0.01, "duration {duration}");
    let times: Vec<f64> = demuxed.audio.iter().map(|(pts, _)| *pts).collect();
    assert!(times[0].abs() < 1e-6 && (times[2] - 1024.0 / 44_100.0).abs() < 1e-6, "packet times start {:?}", &times[..3]);
}

/// Offsets of the box headers along `path`, taking the first box of each kind.
fn box_path(bytes: &[u8], path: &[&[u8; 4]]) -> Vec<usize> {
    let mut found = Vec::new();
    let (mut pos, mut end) = (0, bytes.len());
    for kind in path {
        loop {
            assert!(pos + 8 <= end, "box {} not found", String::from_utf8_lossy(*kind));
            let size = u32::from_be_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
            if &bytes[pos + 4..pos + 8] == *kind {
                found.push(pos);
                (pos, end) = (pos + 8, pos + size);
                break;
            }
            pos += size;
        }
    }
    found
}

/// `sine.m4a` with its audio track on a 48000 timescale, the way a muxer writes it: every
/// packet's start is its true time rounded to a tick, so the 1024-frame packets last 1114 or
/// 1115 ticks. That takes one `stts` entry per packet, so the table and the boxes around it
/// grow (they are the end of the file; no chunk offset moves).
fn sine_m4a_on_a_48000_timescale() -> std::path::PathBuf {
    let ticks = |frames: u64| ((frames * 48_000) as f64 / 44_100.0).round() as u32;
    let mut durations: Vec<u32> = (0..130u64).map(|k| ticks((k + 1) * 1024) - ticks(k * 1024)).collect();
    durations.push(ticks(204));
    let mut stts = vec![0u8; 4];
    stts.extend_from_slice(&131u32.to_be_bytes());
    for duration in &durations {
        stts.extend_from_slice(&1u32.to_be_bytes());
        stts.extend_from_slice(&duration.to_be_bytes());
    }

    let mut bytes = fixture_bytes("sine.m4a");
    let path = box_path(&bytes, &[b"moov", b"trak", b"mdia", b"minf", b"stbl", b"stts"]);
    let get = |bytes: &[u8], at: usize| u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap());
    let put = |bytes: &mut [u8], at: usize, value: u32| bytes[at..at + 4].copy_from_slice(&value.to_be_bytes());
    let at = path[5];
    let old_size = get(&bytes, at) as usize;
    assert_eq!(path[0] + get(&bytes, path[0]) as usize, bytes.len(), "fixture precondition: moov is the last box");
    let new_box = [&((stts.len() + 8) as u32).to_be_bytes()[..], b"stts", &stts].concat();
    let grown = (new_box.len() - old_size) as u32;
    bytes.splice(at..at + old_size, new_box);
    for &ancestor in &path[..5] {
        let size = get(&bytes, ancestor);
        put(&mut bytes, ancestor, size + grown);
    }
    let mdhd = bytes.windows(4).position(|w| w == b"mdhd").unwrap() + 4;
    put(&mut bytes, mdhd + 12, 48_000);
    put(&mut bytes, mdhd + 16, durations.iter().sum());
    let elst = bytes.windows(4).position(|w| w == b"elst").unwrap() + 4;
    assert_eq!(get(&bytes, elst + 12), 1024, "fixture precondition: one edit at 1024");
    put(&mut bytes, elst + 12, ticks(1024));
    tmp_file("sine_timescale_48000.m4a", &bytes)
}

#[test]
fn mp4_audio_on_a_48000_timescale_keeps_every_frame_of_its_ordinary_packets() {
    // 44.1 kHz audio on a 48 kHz clock: a 1024-frame packet is 1114.56 ticks, written as 1114
    // or 1115. Read back, 1114 ticks are 1023.5 frames; cutting packets there would drop a
    // sample from about every second one.
    let path = sine_m4a_on_a_48000_timescale();
    assert!((playr::demux::mp4_audio_priming_secs(&path) - 1115.0 / 48_000.0).abs() < 1e-9);
    let (demuxed, err) = drain(open_path(&path).expect("open").as_mut());
    assert_eq!(err, None);

    let trims = &demuxed.audio_trim;
    assert_eq!(trims.len(), 131);
    // One tick is 0.92 sample frames, so each trimmed edge is known to one frame.
    assert!(trims[0].0.abs_diff(1024) <= 1 && trims[0].1.is_none(), "first packet {:?}: 1024 frames of priming", trims[0]);
    assert_eq!(trims[1..130].iter().filter(|t| **t != (0, None)).count(), 0, "ordinary packets are played whole");
    assert!(trims[130].0 == 0 && trims[130].1.is_some_and(|l| l.abs_diff(204) <= 1), "last packet {:?}: 204 frames", trims[130]);
    // The samples are those of sine.m4a, 132300 frames after trimming; only the way their
    // times are written has changed. Two trimmed edges, one frame each.
    let audio = decode_audio(&demuxed);
    assert!(audio.frames().abs_diff(132_300) <= 2, "{} sample frames kept, the stream holds 132300", audio.frames());
}

#[test]
fn mp4_audio_with_a_millisecond_timescale_is_trimmed_to_within_a_tick_at_each_end() {
    // At 1000 ticks per second a 1024-frame packet (23.22 ms) is written as 23 ticks, the
    // 204-frame last packet (4.6 ms) as 5 and the priming as 23. FFmpeg decodes 132317 sample
    // frames from this file (`ffmpeg -i <file> -f f32le`, measured on the same patch).
    let path = sine_m4a_with_timescale(1000, 23, 5, 23);
    assert!((playr::demux::mp4_audio_priming_secs(&path) - 0.023).abs() < 1e-9);
    let (demuxed, err) = drain(open_path(&path).expect("open").as_mut());
    assert_eq!(err, None);

    // One tick is 44.1 sample frames: nothing the container says is more exact than that.
    let tick = 45;
    let trims = &demuxed.audio_trim;
    assert_eq!(trims.len(), 131);
    assert!(trims[0].0.abs_diff(1024) <= tick && trims[0].1.is_none(), "first packet {:?}: about 1024 frames of priming", trims[0]);
    // Ordinary packets look 23 ms (1014 frames) long in this file; cutting them there would
    // drop ten real frames out of every packet.
    assert_eq!(trims[1..130].iter().filter(|t| **t != (0, None)).count(), 0, "ordinary packets are played whole");
    let (last_skip, last_limit) = trims[130];
    assert!(last_skip == 0 && last_limit.is_some_and(|l| l.abs_diff(204) <= tick), "last packet {:?}: about 204 frames", trims[130]);
    // Two trimmed edges (priming, padding), each known to one tick.
    let audio = decode_audio(&demuxed);
    assert!(
        audio.frames().abs_diff(132_317) <= 2 * tick as usize,
        "{} sample frames kept, FFmpeg decodes 132317 (tolerance: one tick, {tick} frames, at each end)",
        audio.frames()
    );
}

#[test]
fn seeking_past_the_end_fails_or_lands_inside_and_leaves_the_file_usable() {
    let mut bad = Vec::new();
    for file in ["h264_aac.mp4", "h264_vorbis.mkv", "xvid_mp3.avi", "sine.m4a", "sine.flac", "sine.mp3", "sine.ogg", "sine.wav"] {
        let mut demux = open_path(&fixture(file)).unwrap();
        let duration = demux.info().duration.expect("duration");
        match demux.seek(duration + 100.0) {
            Ok(landed) => expect!(bad, (0.0..=duration + 0.05).contains(&landed), "{file}: landed at {landed}, the file is {duration} s long"),
            Err(e) => expect!(bad, e.trim().len() >= 10, "{file}: unreadable error {e:?}"),
        }
        // Whatever the answer was, the next seek has to work.
        match demux.seek(0.5) {
            Ok(landed) => expect!(bad, landed <= 0.501 && landed >= 0.0, "{file}: seek(0.5) afterwards landed at {landed}"),
            Err(e) => bad.push(format!("{file}: seek(0.5) after seeking past the end failed with {e:?}")),
        }
        let (rest, err) = drain(demux.as_mut());
        expect!(bad, err.is_none(), "{file}: reading after that failed with {err:?}");
        let last = rest.audio.last().map(|(pts, _)| *pts);
        expect!(bad, last.is_some_and(|p| p >= duration - 0.2), "{file}: reading stopped at {last:?} of {duration} s");
    }
    finish(bad);
}

#[test]
fn mp4_audio_priming_is_read_from_the_edit_list() {
    // FFmpeg's AAC encoder starts every stream with 1024 priming samples and records them in
    // the edit list (`ffmpeg -v trace`: "edit list 0 - media time: 1024"); at 44.1 kHz that
    // is 23.22 ms.
    let priming = 1024.0 / 44100.0;
    for file in ["sine.m4a", "h264_aac.mp4", "h264_bframes_aac.mp4", "hevc_aac.mp4", "h264_1080p_aac.mp4"] {
        let got = playr::demux::mp4_audio_priming_secs(&fixture(file));
        assert!((got - priming).abs() < 1e-6, "{file}: priming {got} s, expected {priming}");
    }
}

#[test]
fn files_without_an_mp4_edit_list_have_no_priming() {
    let missing = fixture("sine.wav").with_file_name("no-such-file.m4a");
    let empty = tmp_file("priming_empty.m4a", &[]);
    let cut = tmp_file("priming_cut.m4a", &fixture_bytes("sine.m4a")[..1000]);
    let cases = [
        ("another container", fixture("sine.flac")),
        ("AVI", fixture("xvid_mp3.avi")),
        ("Matroska", fixture("h264_vorbis.mkv")),
        ("missing file", missing),
        ("empty file", empty),
        ("MP4 cut off before its moov box", cut),
    ];
    for (what, path) in cases {
        assert_eq!(playr::demux::mp4_audio_priming_secs(&path), 0.0, "{what}");
    }
}

#[test]
fn only_mp4_audio_packets_carry_trim_instructions() {
    let mut bad = Vec::new();
    for case in audio_cases() {
        let demuxed = demux_file(case.file);
        let trims = &demuxed.audio_trim;
        if is_aac_in_mp4(&case) {
            // The 1024 priming samples are exactly the first packet; the samples kept from all
            // packets together are the stream FFmpeg decodes.
            let skipped: u32 = trims.iter().map(|(skip, _)| *skip).sum();
            expect!(bad, trims.first().map(|t| t.0) == Some(1024) && skipped == 1024, "{}: priming skips {:?} ...", case.file, &trims[..2.min(trims.len())]);
            let kept: usize = trims.iter().map(|(skip, limit)| (limit.unwrap_or(1024).min(1024) - (*skip).min(1024)) as usize).sum();
            expect!(bad, kept == case.frames, "{}: skip/limit keep {kept} sample frames, FFmpeg decodes {}", case.file, case.frames);
            let first_pts = demuxed.audio[0].0;
            expect!(bad, first_pts.abs() < 1e-6, "{}: the first kept sample is at {first_pts}, expected 0", case.file);
        } else {
            let other: Vec<_> = trims.iter().filter(|t| **t != (0, None)).take(3).collect();
            expect!(bad, other.is_empty(), "{}: unexpected trims {other:?}", case.file);
        }
    }
    finish(bad);
}

mod seek {
    use super::*;
    use playr::media::Packet;

    /// First video and first audio timestamp after a seek, reading at most 200 packets.
    fn first_after_seek(demux: &mut dyn playr::demux::Demuxer) -> (Option<(f64, Box<[u8]>)>, Option<f64>) {
        let (mut video, mut audio) = (None, None);
        for _ in 0..200 {
            match demux.next_packet().expect("next_packet after seek") {
                Some(Packet::Video { pts, data }) if video.is_none() => video = Some((pts, data)),
                Some(Packet::Audio { pts, .. }) if audio.is_none() => audio = Some(pts),
                Some(_) => {}
                None => break,
            }
            if video.is_some() && audio.is_some() {
                break;
            }
        }
        (video, audio)
    }

    #[test]
    fn audio_files_resume_at_the_target_and_play_the_remainder() {
        let mut bad = Vec::new();
        for case in audio_cases().iter().filter(|c| c.file.starts_with("sine.")) {
            for target in [0.0, 1.0, 2.5] {
                let mut demux = open_path(&fixture(case.file)).unwrap();
                let what = format!("{} seek({target})", case.file);
                let landed = match demux.seek(target) {
                    Ok(l) => l,
                    Err(e) => {
                        bad.push(format!("{what}: failed with {e:?}"));
                        continue;
                    }
                };
                // "At or before" the target, and no further back than a couple of codec frames.
                expect!(bad, landed <= target + 0.001 && landed >= target - 0.1, "{what}: landed at {landed}");
                let (rest, err) = drain(demux.as_mut());
                expect!(bad, err.is_none(), "{what}: reading on failed with {err:?}");
                let first = rest.audio.first().map(|(pts, _)| *pts);
                expect!(
                    bad,
                    first.is_some_and(|p| p <= target + 0.001 && p >= target - 0.1),
                    "{what}: first packet after the seek is at {first:?}"
                );
                let audio = decode_audio(&rest);
                let remaining = audio.frames() as f64 / case.rate as f64;
                let want = case.secs - target;
                expect!(bad, (remaining - want).abs() <= 0.15, "{what}: {remaining:.3} s of audio follow, expected {want} +/- 0.15");
            }
        }
        finish(bad);
    }

    #[test]
    fn video_files_land_at_or_before_the_target() {
        let mut bad = Vec::new();
        for case in video_cases() {
            for target in [0.0, 0.5, 1.0, 1.7] {
                let mut demux = open_path(&fixture(case.file)).unwrap();
                let what = format!("{} seek({target})", case.file);
                let landed = match demux.seek(target) {
                    Ok(l) => l,
                    Err(e) => {
                        bad.push(format!("{what}: failed with {e:?}"));
                        continue;
                    }
                };
                expect!(bad, (0.0..=target + 0.001).contains(&landed), "{what}: landed at {landed}");
                let (video, _) = first_after_seek(demux.as_mut());
                // With B-frames the first packet in decode order can be a later picture, so
                // the bound is one reorder window (two pictures) past the target.
                let slack = if case.file == "h264_aac.mp4" || case.container == "avi" { 0.001 } else { 0.081 };
                expect!(
                    bad,
                    video.as_ref().is_some_and(|(pts, _)| *pts <= target + slack),
                    "{what}: first video packet after the seek is at {:?}",
                    video.as_ref().map(|(pts, _)| *pts)
                );
            }
        }
        finish(bad);
    }

    #[test]
    fn seeking_back_and_forth_on_one_open_file_keeps_working() {
        // What scrubbing does: jump, read a little, jump again, in both directions.
        let files: Vec<&str> =
            video_cases().iter().map(|c| c.file).chain(audio_cases().iter().map(|c| c.file).filter(|f| f.starts_with("sine."))).collect();
        let mut bad = Vec::new();
        for file in files {
            let mut demux = open_path(&fixture(file)).unwrap();
            for (step, target) in [1.5, 0.5, 0.0, 1.0, 0.2, 1.8].into_iter().enumerate() {
                let what = format!("{file} seek #{step} to {target}");
                if let Err(e) = demux.seek(target) {
                    bad.push(format!("{what}: failed with {e:?}"));
                    continue;
                }
                let mut first = None;
                for n in 0..8 {
                    match demux.next_packet() {
                        Ok(Some(Packet::Video { pts, .. })) | Ok(Some(Packet::Audio { pts, .. })) => {
                            first.get_or_insert(pts);
                        }
                        Ok(None) => break,
                        Err(e) => {
                            bad.push(format!("{what}: packet {n} after the seek failed with {e:?}"));
                            break;
                        }
                    }
                }
                // Loose on purpose (exact landing is covered above): the jump has to have
                // happened at all, in either direction.
                expect!(
                    bad,
                    first.is_some_and(|p: f64| (p - target).abs() <= 0.6),
                    "{what}: reading resumed at {first:?}"
                );
            }
        }
        finish(bad);
    }

    #[test]
    fn a_file_read_to_its_end_can_be_rewound_and_read_again() {
        // Replaying after the end is a seek to zero on a demuxer that has already hit EOF.
        let mut files: Vec<&str> = video_cases().iter().map(|c| c.file).chain(audio_cases().iter().map(|c| c.file)).collect();
        files.sort_unstable();
        files.dedup();
        let mut bad = Vec::new();
        for file in files {
            let mut demux = open_path(&fixture(file)).unwrap();
            let (first_pass, err) = drain(demux.as_mut());
            assert_eq!(err, None, "{file}: first pass");
            let landed = match demux.seek(0.0) {
                Ok(l) => l,
                Err(e) => {
                    bad.push(format!("{file}: seek(0) after the end failed with {e:?}"));
                    continue;
                }
            };
            expect!(bad, landed.abs() <= 0.03, "{file}: seek(0) after the end landed at {landed}");
            let (second_pass, err) = drain(demux.as_mut());
            expect!(bad, err.is_none(), "{file}: reading after the rewind failed with {err:?}");
            // A rewind is a fresh start: the same packets at the same times as the first pass.
            let times = |d: &Demuxed| -> (Vec<f64>, Vec<f64>) {
                (d.video.iter().map(|(pts, _)| *pts).collect(), d.audio.iter().map(|(pts, _)| *pts).collect())
            };
            let (first, second) = (times(&first_pass), times(&second_pass));
            expect!(
                bad,
                second == first,
                "{file}: second pass read {} video and {} audio packets (audio from {:?}), the first {} and {} (audio from {:?})",
                second.0.len(),
                second.1.len(),
                second.1.first(),
                first.0.len(),
                first.1.len(),
                first.1.first()
            );
            expect!(bad, second_pass.audio_trim == first_pass.audio_trim, "{file}: trim instructions differ after the rewind");
        }
        finish(bad);
    }

    #[test]
    fn reading_continues_forward_from_the_landing_point() {
        // After a seek the packets that follow belong to the place it landed on; with
        // B-frames a packet may precede the landing picture by the reorder depth (0.2 s is
        // five pictures here), but nothing may come from further back.
        let mut bad = Vec::new();
        for case in video_cases() {
            for target in [0.0, 0.5, 1.0, 1.7] {
                let mut demux = open_path(&fixture(case.file)).unwrap();
                let what = format!("{} seek({target})", case.file);
                let Ok(landed) = demux.seek(target) else {
                    bad.push(format!("{what}: failed"));
                    continue;
                };
                let (mut video, mut audio) = (Vec::new(), Vec::new());
                for _ in 0..16 {
                    match demux.next_packet() {
                        Ok(Some(Packet::Video { pts, .. })) => video.push(pts),
                        Ok(Some(Packet::Audio { pts, .. })) => audio.push(pts),
                        _ => break,
                    }
                }
                expect!(
                    bad,
                    video.iter().all(|pts| *pts >= landed - 0.2),
                    "{what}: landed at {landed:.2}, then video packets at {video:.2?}"
                );
                expect!(bad, audio.windows(2).all(|w| w[0] <= w[1]), "{what}: audio packets go backwards: {audio:.2?}");
                expect!(
                    bad,
                    audio.iter().all(|pts| *pts >= landed - 0.5),
                    "{what}: landed at {landed:.2}, then audio packets at {audio:.2?}"
                );
            }
        }
        finish(bad);
    }

    /// (file, keyframe times per ffprobe)
    const AVI_KEYFRAMES: [(&str, [f64; 5]); 2] =
        [("xvid_pcm16.avi", [0.0, 0.48, 0.96, 1.44, 1.92]), ("xvid_mp3.avi", [0.0, 0.52, 1.0, 1.48, 1.96])];

    fn check_avi_seeks(file: &str, path: &std::path::Path, keys: &[f64; 5], bad: &mut Vec<String>) {
        // (target, index of the keyframe it must land on)
        let targets = [
            (-1.0, 0),
            (0.0, 0),
            (keys[1] - 0.01, 0),
            (keys[1], 1),
            (1.2, 2),
            (keys[3] + 0.3, 3),
            (keys[4] + 0.03, 4),
            (100.0, 4),
        ];
        for (target, key_index) in targets {
            let want = keys[key_index];
            let what = format!("{file} seek({target})");
            let mut demux = open_path(path).unwrap_or_else(|e| panic!("open {file}: {e}"));
            let track = demux.info().video.clone().expect("video track");
            let landed = match demux.seek(target) {
                Ok(l) => l,
                Err(e) => {
                    bad.push(format!("{what}: failed with {e:?}"));
                    continue;
                }
            };
            expect!(bad, (landed - want).abs() <= 0.001, "{what}: landed at {landed}, the keyframe at or before the target is {want}");
            let (video, audio) = first_after_seek(demux.as_mut());
            match video {
                Some((pts, data)) => {
                    expect!(bad, (pts - want).abs() <= 0.001, "{what}: first video packet at {pts}, expected {want}");
                    expect!(bad, is_keyframe(&track, &data), "{what}: first video packet is not a keyframe");
                }
                None => bad.push(format!("{what}: no video packet follows")),
            }
            // Sound has to be available from the landing point: the chunk containing it.
            expect!(
                bad,
                audio.is_some_and(|a| a <= want + 0.001 && a >= want - 0.1),
                "{what}: first audio packet at {audio:?}, expected within 0.1 s before {want}"
            );
        }
    }

    #[test]
    fn avi_lands_exactly_on_the_keyframe_at_or_before_the_target() {
        let mut bad = Vec::new();
        for (file, keys) in &AVI_KEYFRAMES {
            check_avi_seeks(file, &fixture(file), keys, &mut bad);
        }
        finish(bad);
    }

    #[test]
    fn avi_without_an_index_still_demuxes_decodes_and_seeks() {
        let mut bad = Vec::new();
        for (file, keys) in &AVI_KEYFRAMES {
            let bytes = fixture_bytes(file);
            let idx1 = bytes.windows(4).rposition(|w| w == b"idx1").expect("fixture has an idx1 chunk");
            assert!(idx1 > bytes.len() / 2, "{file}: idx1 is expected after the movi list");
            let name = format!("no_index_{file}");
            let path = tmp_file(&name, &bytes[..idx1]);

            let mut demux = open_path(&path).unwrap_or_else(|e| panic!("open {name}: {e}"));
            let (demuxed, err) = drain(demux.as_mut());
            expect!(bad, err.is_none(), "{name}: demuxing failed with {err:?}");
            expect!(bad, demuxed.info.seekable, "{name}: must stay seekable");
            expect!(bad, demuxed.video.len() == 50, "{name}: {} video packets, expected 50", demuxed.video.len());

            let reference = read_framecrc(file);
            let frames = decode_video(&demuxed).frames;
            expect!(bad, frames.len() == reference.len(), "{name}: {} pictures, expected {}", frames.len(), reference.len());
            for (n, (frame, (want_pts, _))) in frames.iter().zip(&reference).enumerate() {
                expect!(bad, (frame.pts - want_pts).abs() <= 0.001, "{name}: picture {n} has pts {}, FFmpeg says {want_pts}", frame.pts);
            }
            let audio = decode_audio(&demuxed);
            let want_audio: usize = if *file == "xvid_mp3.avi" { 89_856 } else { 88_200 };
            expect!(bad, audio.frames().abs_diff(want_audio) <= 1152, "{name}: {} audio frames, expected about {want_audio}", audio.frames());

            check_avi_seeks(&name, &path, keys, &mut bad);
        }
        finish(bad);
    }

    #[test]
    fn mp3_in_avi_keeps_decoding_after_a_seek_into_the_middle_of_a_chunk() {
        let mut demux = open_path(&fixture("xvid_mp3.avi")).unwrap();
        let landed = demux.seek(1.2).expect("seek");
        let (rest, err) = drain(demux.as_mut());
        assert_eq!(err, None);

        let audio = decode_audio(&rest);

        // The first frames after a jump may depend on bits from before it (bit reservoir).
        assert!(audio.failed_packets <= 2, "{} of {} packets failed after the seek", audio.failed_packets, rest.audio.len());
        let remaining = audio.frames() as f64 / 44100.0;
        let want = 89_856.0 / 44100.0 - landed;
        assert!((remaining - want).abs() <= 0.15, "{remaining:.3} s of audio follow the seek to {landed}, expected {want:.3} +/- 0.15");
        let pts: Vec<f64> = rest.audio.iter().map(|(p, _)| *p).collect();
        assert!(pts.windows(2).all(|w| w[0] < w[1]), "audio timestamps after the seek are not ascending");
    }
}
