//! HLS played end to end from a local HTTP server (`tests/common/hls_server.rs`) and from disk:
//! the FFmpeg-made streams of `tests/fixtures/hls` (MPEG-TS with H.264, HEVC, AAC and MP3,
//! fragmented MP4, AES-128), packed audio and transport streams built by the tests, master
//! playlists, a live playlist that grows, seeking, aborting, the encrypted-only setting, and
//! the paths in from `net`, `sites`, `youtube` and the player.
//!
//! The fixtures are testsrc2 160x120 at 25 fps without B-frames, a keyframe every second, cut
//! into 1 s segments, with a 440 Hz sine at 44.1 kHz (see `tests/fixtures/generate.ps1`).

mod common;
mod own_ip;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use common::hls_server::{HlsServer, PLAYLIST, Route, bytes, dynamic, fixture_dir, held, ignores_range, maybe, redirect, trickle};
use common::ts_build::{pat, pes_packets, pmt};
use common::{Demuxed, decode_audio, decode_video, drain, finish, fixture, fixture_bytes, tone_cycles};
use playr::demux::Demuxer;
use playr::hls::{self, Location, Opened};
use playr::media::{AudioTrack, Packet, VideoCodec};
use symphonia::core::codecs::audio::well_known::{CODEC_ID_AAC, CODEC_ID_MP3};

const CHILD: &str = "PLAYR_TEST_CHILD";
const FPS: f64 = 25.0;
const RATE: f64 = 44100.0;
const AAC_STEP: f64 = 1024.0 / RATE;

fn no_abort() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

fn open_url(url: &str) -> Result<Opened, String> {
    hls::open(&Location::Url(url.to_string()), None, &[], false, no_abort())
}

fn open_err(url: &str) -> String {
    match open_url(url) {
        Ok(_) => "opened".into(),
        Err(e) => e,
    }
}

/// The text of a fixture playlist.
fn playlist_text(dir: &str) -> String {
    String::from_utf8(fixture_bytes(&format!("hls/{dir}/index.m3u8"))).expect("playlist is text")
}

/// The sum of the `#EXTINF` durations written in a fixture playlist.
fn extinf_sum(dir: &str) -> f64 {
    playlist_text(dir)
        .lines()
        .filter_map(|l| l.trim().strip_prefix("#EXTINF:"))
        .map(|d| d.split(',').next().unwrap().parse::<f64>().unwrap())
        .sum()
}

fn video_pts(d: &Demuxed) -> Vec<f64> {
    d.video.iter().map(|(pts, _)| *pts).collect()
}

fn audio_pts(d: &Demuxed) -> Vec<f64> {
    d.audio.iter().map(|(pts, _)| *pts).collect()
}

/// Where neighbouring timestamps are not `step` apart (within 1 ms).
fn gaps(what: &str, pts: &[f64], step: f64) -> Vec<String> {
    pts.windows(2)
        .enumerate()
        .filter(|(_, w)| ((w[1] - w[0]) - step).abs() > 1e-3)
        .map(|(i, w)| format!("{what} {i}->{}: {:.4} -> {:.4}", i + 1, w[0], w[1]))
        .collect()
}

fn near(a: f64, b: f64, tolerance: f64) -> bool {
    (a - b).abs() <= tolerance
}

fn segment_paths(prefix: &str, ext: &str, numbers: impl IntoIterator<Item = u64>) -> Vec<String> {
    numbers.into_iter().map(|n| format!("/{prefix}/seg{n}.{ext}")).collect()
}

/// Checks a demuxed H.264 + AAC stream of `secs` seconds starting at 0: every picture and
/// audio frame present, timestamps evenly spaced across the segment boundaries, and both
/// decoding cleanly to 160x120 pictures and a 440 Hz tone.
fn check_av(bad: &mut Vec<String>, d: &Demuxed, secs: f64) {
    check_av_within(bad, d, secs, 5.0);
}

/// [`check_av`] for a run of segments cut out of a longer fixture: FFmpeg ends a segment's
/// sound at its own PES boundary, up to a few tens of milliseconds off the pictures, so the
/// tone may run up to 25 cycles (57 ms) longer or shorter.
fn check_av_part(bad: &mut Vec<String>, d: &Demuxed, secs: f64) {
    check_av_within(bad, d, secs, 25.0);
}

fn check_av_within(bad: &mut Vec<String>, d: &Demuxed, secs: f64, cycle_slack: f64) {
    let pictures = (secs * FPS).round() as usize;
    let v = video_pts(d);
    let a = audio_pts(d);
    expect!(bad, v.len() == pictures, "{} video packets, expected {pictures}", v.len());
    expect!(bad, v.first().is_some_and(|p| near(*p, 0.0, 0.05)), "first video pts {:?}", v.first());
    expect!(bad, a.first().is_some_and(|p| near(*p, 0.0, 0.05)), "first audio pts {:?}", a.first());
    bad.extend(gaps("video", &v, 1.0 / FPS));
    bad.extend(gaps("audio", &a, AAC_STEP));
    let decoded = decode_video(d);
    let sizes: Vec<(u32, u32)> = decoded.frames.iter().map(|f| (f.width, f.height)).collect();
    expect!(bad, decoded.frames.len() == pictures && decoded.errors == 0, "{} pictures decoded, {} errors ({:?})", decoded.frames.len(), decoded.errors, decoded.last_error);
    expect!(bad, sizes.iter().all(|s| *s == (160, 120)), "picture sizes {:?}", sizes.first());
    let sound = decode_audio(d);
    let cycles = tone_cycles(&sound.channel(0), 0.05) as f64;
    expect!(bad, sound.failed_packets == 0 && sound.rate == 44100, "audio: {} failed packets, rate {}", sound.failed_packets, sound.rate);
    expect!(bad, near(cycles, 440.0 * secs, cycle_slack), "{cycles} cycles of the tone, expected {}", 440.0 * secs);
}

fn aac_track(track: &Option<AudioTrack>) -> Option<(bool, Option<u32>)> {
    match track {
        Some(AudioTrack::Symphonia(p)) => Some((p.codec == CODEC_ID_AAC, p.sample_rate)),
        _ => None,
    }
}

mod recordings {
    use super::*;

    #[test]
    fn an_mpeg_ts_recording_plays_every_segment_once_with_timestamps_from_zero_across_segments() {
        let server = HlsServer::start(fixture_dir("ts", "vod"));

        let opened = open_url(&server.url("/vod/index.m3u8")).expect("open");
        let mut demux = opened.demuxer;
        let info = demux.info().clone();
        let (d, err) = drain(demux.as_mut());

        assert_eq!((opened.live, opened.secure, err), (false, true, None));
        assert_eq!((info.container.as_str(), info.duration, info.seekable), ("HLS", Some(extinf_sum("ts")), true));
        // A media playlist names no RESOLUTION: the size comes from the H.264 SPS (160x120,
        // coded as 160x128 with cropping).
        let video = info.video.map(|v| (v.codec, v.width, v.height));
        assert_eq!((video, aac_track(&info.audio), info.notes), (Some((VideoCodec::H264, 160, 120)), Some((true, Some(44100))), vec![]));
        let mut bad = Vec::new();
        check_av(&mut bad, &d, 6.0);
        finish(bad);
        let mut expected = vec!["/vod/index.m3u8".to_string()];
        expected.extend(segment_paths("vod", "ts", 0..6));
        assert_eq!(server.paths(), expected);
    }

    #[test]
    fn a_fragmented_mp4_recording_reads_its_map_once_and_plays() {
        let server = HlsServer::start(fixture_dir("fmp4", "f"));

        let opened = open_url(&server.url("/f/index.m3u8")).expect("open");
        let mut demux = opened.demuxer;
        let info = demux.info().clone();
        let (d, err) = drain(demux.as_mut());

        assert_eq!(err, None);
        assert_eq!((info.duration, info.seekable), (Some(extinf_sum("fmp4")), true));
        assert_eq!((info.video.as_ref().map(|v| v.codec.clone()), aac_track(&info.audio)), (Some(VideoCodec::H264), Some((true, Some(44100)))));
        let mut bad = Vec::new();
        check_av(&mut bad, &d, 4.0);
        finish(bad);
        let mut expected = vec!["/f/index.m3u8".to_string(), "/f/init.mp4".to_string()];
        expected.extend(segment_paths("f", "m4s", 0..4));
        assert_eq!(server.paths(), expected);
    }

    #[test]
    fn hevc_in_mpeg_ts_plays() {
        let server = HlsServer::start(fixture_dir("hevc", "h"));

        let mut demux = open_url(&server.url("/h/index.m3u8")).expect("open").demuxer;
        let video = demux.info().video.as_ref().map(|v| (v.codec.clone(), v.width, v.height));
        let (d, err) = drain(demux.as_mut());

        // The size comes from the HEVC SPS in the segment.
        assert_eq!((video, err), (Some((VideoCodec::Hevc, 160, 120)), None));
        let mut bad = Vec::new();
        check_av(&mut bad, &d, 2.0);
        finish(bad);
    }

    #[test]
    fn fmp4_timestamps_follow_each_fragments_decode_time_not_the_playlist_durations() {
        // The playlist claims 1.5 s per segment; the fragments hold 1 s each (their tfdt says
        // 0, 1, 2, 3 s), so the pictures must come at 0.00 .. 3.96 without a jump.
        let text = playlist_text("fmp4").replace("#EXTINF:1.000000,", "#EXTINF:1.500000,");
        assert_eq!(text.matches("#EXTINF:1.500000,").count(), 4, "test precondition: every duration replaced");
        let mut routes = vec![bytes("/f/index.m3u8", PLAYLIST, text.into_bytes())];
        routes.extend(fixture_dir("fmp4", "f").into_iter().filter(|r| !r.path.ends_with(".m3u8")));
        let server = HlsServer::start(routes);

        let mut demux = open_url(&server.url("/f/index.m3u8")).expect("open").demuxer;
        let duration = demux.info().duration;
        let (d, err) = drain(demux.as_mut());

        assert_eq!((duration, err), (Some(6.0), None), "the duration is still the sum of EXTINF");
        let mut bad = Vec::new();
        check_av(&mut bad, &d, 4.0);
        let v = video_pts(&d);
        expect!(bad, v.last().is_some_and(|p| near(*p, 3.96, 1e-3)), "last picture at {:?}", v.last());
        finish(bad);
    }

    #[test]
    fn mp3_sound_in_mpeg_ts_plays_without_a_picture() {
        let server = HlsServer::start(fixture_dir("mp3_ts", "m"));

        let mut demux = open_url(&server.url("/m/index.m3u8")).expect("open").demuxer;
        let info = demux.info().clone();
        let (d, err) = drain(demux.as_mut());

        let codec = match &info.audio {
            Some(AudioTrack::Symphonia(p)) => Some((p.codec == CODEC_ID_MP3, p.sample_rate)),
            _ => None,
        };
        assert_eq!((info.video.is_none(), codec, info.duration, err), (true, Some((true, Some(44100))), Some(extinf_sum("mp3_ts")), None));
        let mut bad = Vec::new();
        bad.extend(gaps("audio", &audio_pts(&d), 1152.0 / RATE));
        expect!(bad, d.audio.first().is_some_and(|a| near(a.0, 0.0, 0.05)), "first audio pts {:?}", d.audio.first().map(|a| a.0));
        let sound = decode_audio(&d);
        let cycles = tone_cycles(&sound.channel(0), 0.05) as f64;
        expect!(bad, sound.failed_packets == 0, "{} packets failed to decode", sound.failed_packets);
        expect!(bad, near(cycles, 880.0, 5.0), "{cycles} cycles of the tone in 2 s");
        finish(bad);
    }

    #[test]
    fn a_local_playlist_on_disk_plays_its_segments_from_the_same_folder() {
        let location = Location::File(fixture("hls/ts/index.m3u8"));

        let opened = hls::open(&location, None, &[], false, no_abort()).expect("open");
        let mut demux = opened.demuxer;
        let duration = demux.info().duration;
        let (d, err) = drain(demux.as_mut());

        assert_eq!((opened.live, opened.secure, duration, err), (false, true, Some(6.0), None));
        let mut bad = Vec::new();
        check_av(&mut bad, &d, 6.0);
        finish(bad);
    }

    /// Segments 0-2 of the TS fixture as one file, and a playlist naming each by byte range.
    fn byte_range_stream() -> (Vec<u8>, String) {
        let mut all = Vec::new();
        let mut text = String::from("#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-VERSION:4\n");
        for n in 0..3 {
            let seg = fixture_bytes(&format!("hls/ts/seg{n}.ts"));
            // Every other range leaves its offset out: it follows on from the previous one.
            if n == 1 {
                text.push_str(&format!("#EXTINF:1.0,\n#EXT-X-BYTERANGE:{}\nall.ts\n", seg.len()));
            } else {
                text.push_str(&format!("#EXTINF:1.0,\n#EXT-X-BYTERANGE:{}@{}\nall.ts\n", seg.len(), all.len()));
            }
            all.extend(seg);
        }
        text.push_str("#EXT-X-ENDLIST\n");
        (all, text)
    }

    #[test]
    fn byte_ranges_of_one_file_are_fetched_as_ranges_and_cut_out_of_a_server_that_ignores_them() {
        let (all, text) = byte_range_stream();
        let lens: Vec<usize> = (0..3).map(|n| fixture_bytes(&format!("hls/ts/seg{n}.ts")).len()).collect();
        let ranged = HlsServer::start(vec![bytes("/r/index.m3u8", PLAYLIST, text.clone().into_bytes()), bytes("/r/all.ts", "video/mp2t", all.clone())]);
        let whole = HlsServer::start(vec![bytes("/r/index.m3u8", PLAYLIST, text.into_bytes()), ignores_range("/r/all.ts", "video/mp2t", all)]);
        let mut bad = Vec::new();
        for (case, server) in [("ranges honoured", &ranged), ("ranges ignored", &whole)] {
            let mut demux = open_url(&server.url("/r/index.m3u8")).expect("open").demuxer;
            let (d, err) = drain(demux.as_mut());
            expect!(bad, err.is_none(), "{case}: {err:?}");
            let mut case_bad = Vec::new();
            check_av_part(&mut case_bad, &d, 3.0);
            bad.extend(case_bad.into_iter().map(|e| format!("{case}: {e}")));
            let ranges: Vec<Option<String>> = server.hits().into_iter().filter(|h| h.path == "/r/all.ts").map(|h| h.range).collect();
            let (a, b) = (lens[0], lens[0] + lens[1]);
            let expected = vec![
                Some(format!("bytes=0-{}", a - 1)),
                Some(format!("bytes={a}-{}", b - 1)),
                Some(format!("bytes={b}-{}", b + lens[2] - 1)),
            ];
            expect!(bad, ranges == expected, "{case}: range requests {ranges:?}");
        }
        finish(bad);
    }

    #[test]
    fn byte_ranges_of_a_local_file_are_read() {
        let (all, text) = byte_range_stream();
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("hls-ranges");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("all.ts"), all).unwrap();
        std::fs::write(dir.join("index.m3u8"), &text).unwrap();

        let opened = hls::open(&Location::File(dir.join("index.m3u8")), Some(text), &[], false, no_abort()).expect("open");
        let mut demux = opened.demuxer;
        let (d, err) = drain(demux.as_mut());

        assert_eq!(err, None);
        let mut bad = Vec::new();
        check_av_part(&mut bad, &d, 3.0);
        finish(bad);
    }
}

/// An ID3 tag with the `transportStreamTimestamp` PRIV frame packed audio segments start with.
fn id3_timestamp_tag(ticks: u64) -> Vec<u8> {
    let owner = b"com.apple.streaming.transportStreamTimestamp\0";
    let mut frame = b"PRIV".to_vec();
    frame.extend_from_slice(&((owner.len() + 8) as u32).to_be_bytes());
    frame.extend_from_slice(&[0, 0]);
    frame.extend_from_slice(owner);
    frame.extend_from_slice(&ticks.to_be_bytes());
    let size = frame.len();
    let mut tag = b"ID3\x04\x00\x00".to_vec();
    tag.extend_from_slice(&[(size >> 21) as u8 & 0x7f, (size >> 14) as u8 & 0x7f, (size >> 7) as u8 & 0x7f, size as u8 & 0x7f]);
    tag.extend_from_slice(&frame);
    tag
}

/// The ADTS frames of `sine.aac`, cut by the 13-bit frame length of each header.
fn adts_frames() -> Vec<Vec<u8>> {
    let data = fixture_bytes("sine.aac");
    let mut out = Vec::new();
    let mut at = 0;
    while at + 7 <= data.len() {
        assert_eq!((data[at], data[at + 1] & 0xF6), (0xFF, 0xF0), "ADTS sync at {at}");
        let len = ((data[at + 3] as usize & 3) << 11) | ((data[at + 4] as usize) << 3) | (data[at + 5] as usize >> 5);
        out.push(data[at..at + len].to_vec());
        at += len;
    }
    out
}

mod packed_audio {
    use super::*;

    #[test]
    fn aac_segments_with_id3_timestamps_play_on_one_clock_from_zero() {
        let frames = adts_frames();
        let cuts = [0, 43, 86, frames.len()];
        let mut routes = Vec::new();
        let mut text = String::from("#EXTM3U\n#EXT-X-TARGETDURATION:1\n");
        for n in 0..3 {
            // The stream's own clock starts at 10 s: the ID3 stamp is 90 kHz ticks.
            let ticks = 900_000 + (cuts[n] as u64 * 1024 * 90_000) / 44_100;
            let mut seg = id3_timestamp_tag(ticks);
            frames[cuts[n]..cuts[n + 1]].iter().for_each(|f| seg.extend_from_slice(f));
            text.push_str(&format!("#EXTINF:{:.6},\nseg{n}.aac\n", (cuts[n + 1] - cuts[n]) as f64 * AAC_STEP));
            routes.push(bytes(&format!("/p/seg{n}.aac"), "audio/aac", seg));
        }
        text.push_str("#EXT-X-ENDLIST\n");
        routes.push(bytes("/p/index.m3u8", PLAYLIST, text.into_bytes()));
        let server = HlsServer::start(routes);

        let mut demux = open_url(&server.url("/p/index.m3u8")).expect("open").demuxer;
        let info = demux.info().clone();
        let (d, err) = drain(demux.as_mut());

        assert_eq!((info.video.is_none(), aac_track(&info.audio), err), (true, Some((true, Some(44100))), None));
        let a = audio_pts(&d);
        let mut bad = Vec::new();
        expect!(bad, a.len() == frames.len(), "{} audio packets, {} frames in the segments", a.len(), frames.len());
        expect!(bad, a.first() == Some(&0.0), "first pts {:?}", a.first());
        bad.extend(gaps("audio", &a, AAC_STEP));
        let sound = decode_audio(&d);
        expect!(bad, sound.failed_packets == 0, "{} packets failed", sound.failed_packets);
        expect!(bad, near(tone_cycles(&sound.channel(0), 0.05) as f64, 1320.0, 5.0), "tone cycles in 3 s");
        finish(bad);
    }

    #[test]
    fn mp3_segments_without_a_timestamp_play_from_where_the_playlist_puts_them() {
        let mp3 = fixture_bytes("radio.mp3");
        let mut frames = Vec::new();
        let mut at = 0;
        while let Some((len, samples, rate)) = mp3.get(at..at + 4).and_then(|h| playr::avi::mpa_frame_info([h[0], h[1], h[2], h[3]])) {
            assert_eq!((samples, rate), (1152, 44100), "radio.mp3 is MPEG-1 layer III at 44.1 kHz");
            frames.push(mp3[at..(at + len).min(mp3.len())].to_vec());
            at += len;
        }
        let half = frames.len() / 2;
        let step = 1152.0 / RATE;
        let text = format!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:3\n#EXTINF:{:.9},\na.mp3\n#EXTINF:{:.9},\nb.mp3\n#EXT-X-ENDLIST\n",
            half as f64 * step,
            (frames.len() - half) as f64 * step
        );
        let server = HlsServer::start(vec![
            bytes("/p/index.m3u8", PLAYLIST, text.into_bytes()),
            bytes("/p/a.mp3", "audio/mpeg", frames[..half].concat()),
            bytes("/p/b.mp3", "audio/mpeg", frames[half..].concat()),
        ]);

        let mut demux = open_url(&server.url("/p/index.m3u8")).expect("open").demuxer;
        let is_mp3 = matches!(&demux.info().audio, Some(AudioTrack::Symphonia(p)) if p.codec == CODEC_ID_MP3);
        let (d, err) = drain(demux.as_mut());

        assert_eq!((is_mp3, err), (true, None));
        let a = audio_pts(&d);
        let mut bad = Vec::new();
        expect!(bad, a.len() == frames.len(), "{} packets for {} frames", a.len(), frames.len());
        expect!(bad, a.first() == Some(&0.0), "first pts {:?}", a.first());
        expect!(bad, a.get(half).is_some_and(|p| near(*p, half as f64 * step, 1e-6)), "second segment starts at {:?}", a.get(half));
        bad.extend(gaps("audio", &a, step));
        finish(bad);
    }
}

/// A playlist of `numbers` 1 s segments `seg<n>.ts`.
fn media_playlist(first: u64, numbers: std::ops::RangeInclusive<u64>, ended: bool) -> Vec<u8> {
    let mut text = format!("#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:{first}\n");
    for n in numbers {
        text.push_str(&format!("#EXTINF:1.000000,\nseg{n}.ts\n"));
    }
    if ended {
        text.push_str("#EXT-X-ENDLIST\n");
    }
    text.into_bytes()
}

mod seeking {
    use super::*;

    fn first_video_after_seek(demux: &mut dyn Demuxer) -> (Option<f64>, f64) {
        let mut lowest = f64::INFINITY;
        while let Ok(Some(p)) = demux.next_packet() {
            lowest = lowest.min(p.pts());
            if let Packet::Video { pts, .. } = p {
                return (Some(pts), lowest);
            }
        }
        (None, lowest)
    }

    #[test]
    fn a_seek_lands_on_the_start_of_the_segment_holding_the_target_and_plays_on_from_there() {
        let server = HlsServer::start(fixture_dir("ts", "vod"));
        let mut demux = open_url(&server.url("/vod/index.m3u8")).expect("open").demuxer;
        for _ in 0..10 {
            demux.next_packet().expect("packet");
        }
        let cases = [("inside the third", 2.5, 2.0), ("start", 0.0, 0.0), ("inside the last", 5.99, 5.0), ("past the end", 100.0, 5.0), ("before the start", -3.0, 0.0), ("exactly a boundary", 3.0, 3.0)];
        let mut bad = Vec::new();
        for (case, target, landed) in cases {
            let got = demux.seek(target);
            expect!(bad, got == Ok(landed), "{case}: seek({target}) = {got:?}, expected Ok({landed})");
            let (video, lowest) = first_video_after_seek(demux.as_mut());
            expect!(bad, video.is_some_and(|v| near(v, landed, 0.05)), "{case}: first picture after the seek at {video:?}");
            expect!(bad, lowest.is_finite() && lowest >= landed - 0.05, "{case}: a packet at {lowest} came before the landing point");
        }
        let (rest, err) = drain(demux.as_mut());
        let v = video_pts(&rest);
        expect!(bad, err.is_none(), "{err:?}");
        // Landed at 3.0 and took its first picture above: 3.04 .. 5.96.
        expect!(bad, v.len() == 74 && v.first().is_some_and(|p| near(*p, 3.04, 1e-3)), "{} pictures from {:?}", v.len(), v.first());
        bad.extend(gaps("video after the last seek", &v, 1.0 / FPS));
        finish(bad);
    }

    #[test]
    fn seeking_back_into_a_segment_just_played_does_not_download_it_again() {
        let server = HlsServer::start(fixture_dir("ts", "vod"));
        let mut demux = open_url(&server.url("/vod/index.m3u8")).expect("open").demuxer;
        while let Ok(Some(p)) = demux.next_packet() {
            if matches!(p, Packet::Video { pts, .. } if pts >= 1.0) {
                break;
            }
        }

        let landed = demux.seek(0.5);
        let next = demux.next_packet().map(|p| p.map(|p| p.pts()));

        assert_eq!(landed, Ok(0.0));
        assert!(matches!(next, Ok(Some(pts)) if pts < 0.05), "{next:?}");
        assert_eq!(server.count("/vod/seg0.ts"), 1, "{:?}", server.paths());
    }
}

mod live {
    use super::*;

    fn live_routes(playlists: impl Fn(usize) -> Vec<u8> + Send + Sync + 'static, extra: Vec<Route>) -> HlsServer {
        let mut routes = vec![dynamic("/live/index.m3u8", PLAYLIST, Box::new(playlists))];
        routes.extend(fixture_dir("ts", "live").into_iter().filter(|r| !r.path.ends_with(".m3u8")));
        routes.extend(extra);
        HlsServer::start(routes)
    }

    fn fetched_segments(server: &HlsServer) -> Vec<String> {
        server.paths().into_iter().filter(|p| p.ends_with(".ts")).collect()
    }

    #[test]
    fn a_live_stream_starts_three_segments_before_the_newest_reloads_and_ends_at_endlist() {
        // Reload 0: 0-4; reload 1: the window moved to 1-5; reload 2 on: 2-5 and the end.
        let server = live_routes(
            |n| match n {
                0 => media_playlist(0, 0..=4, false),
                1 => media_playlist(1, 1..=5, false),
                _ => media_playlist(2, 2..=5, true),
            },
            vec![],
        );

        let opened = open_url(&server.url("/live/index.m3u8")).expect("open");
        let mut demux = opened.demuxer;
        let info = demux.info().clone();
        let seek = demux.seek(1.0);
        let (d, err) = drain(demux.as_mut());

        assert!(opened.live);
        assert_eq!((info.duration, info.seekable, seek), (None, false, Err("a live stream cannot be sought in".to_string())));
        assert_eq!(err, None, "the end of the broadcast is a clean end");
        assert_eq!(fetched_segments(&server), segment_paths("live", "ts", 2..=5), "each segment once, from 3 before the newest");
        assert!(server.count("/live/index.m3u8") >= 3, "{:?}", server.paths());
        let mut bad = Vec::new();
        check_av_part(&mut bad, &d, 4.0);
        finish(bad);
    }

    #[test]
    fn a_live_stream_that_moved_past_the_next_segment_goes_on_from_the_oldest_one_listed() {
        // Reload 0 lists 0-2 (all three are played); by reload 1 the window starts at 4.
        let server = live_routes(
            |n| match n {
                0 => media_playlist(0, 0..=2, false),
                _ => media_playlist(4, 4..=5, true),
            },
            vec![],
        );

        let mut demux = open_url(&server.url("/live/index.m3u8")).expect("open").demuxer;
        let (d, err) = drain(demux.as_mut());

        assert_eq!(err, None);
        assert_eq!(fetched_segments(&server), segment_paths("live", "ts", [0, 1, 2, 4, 5]));
        let v = video_pts(&d);
        assert_eq!(v.len(), 125);
        assert!(v.windows(2).all(|w| w[1] > w[0]), "timestamps go back: {v:?}");
    }

    /// A live stream whose first playlist lists segments 0-2 and every later one `later`;
    /// `seg<k>.ts` exists for each k in `present` (the TS fixture's segment k), others are 404.
    fn live_with_gaps(later: Vec<u8>, present: &[u64]) -> HlsServer {
        let mut routes = vec![dynamic("/live/index.m3u8", PLAYLIST, Box::new(move |n| if n == 0 { media_playlist(0, 0..=2, false) } else { later.clone() }))];
        for &k in present {
            routes.push(bytes(&format!("/live/seg{k}.ts"), "video/mp2t", fixture_bytes(&format!("hls/ts/seg{k}.ts"))));
        }
        HlsServer::start(routes)
    }

    #[test]
    fn three_live_segments_in_a_row_that_cannot_be_downloaded_end_the_stream_with_the_last_error() {
        let server = live_with_gaps(media_playlist(0, 0..=5, false), &[0, 1, 2]);
        let started = Instant::now();

        let mut demux = open_url(&server.url("/live/index.m3u8")).expect("open").demuxer;
        let (d, err) = drain(demux.as_mut());
        let took = started.elapsed();

        let prefix = format!("cannot download {}: ", server.url("/live/seg5.ts"));
        assert!(err.as_ref().is_some_and(|e| e.starts_with(&prefix) && e.len() > prefix.len()), "{err:?}");
        assert!(took < Duration::from_secs(15), "the stream took {took:?} to give up (it used to take over 30 s)");
        assert_eq!(d.video.len(), 75, "segments 0-2 play before the error");
        // Each missing segment is tried 3 times (the fetcher's attempts) before it counts as lost.
        let missing: Vec<usize> = (3..=5).map(|k| server.count(&format!("/live/seg{k}.ts"))).collect();
        assert_eq!(missing, [3, 3, 3]);
    }

    #[test]
    fn live_segments_that_cannot_be_downloaded_are_skipped_while_fewer_than_three_fail_in_a_row() {
        let cases = [
            ("one missing", media_playlist(0, 0..=4, true), vec![0, 1, 2, 4]),
            ("missing, missing, present, missing, missing", media_playlist(0, 0..=7, true), vec![0, 1, 2, 5]),
        ];
        let mut bad = Vec::new();
        for (case, later, present) in cases {
            let server = live_with_gaps(later, &present);
            let mut demux = open_url(&server.url("/live/index.m3u8")).expect("open").demuxer;
            let (d, err) = drain(demux.as_mut());
            expect!(bad, err.is_none(), "{case}: {err:?}");
            expect!(bad, d.video.len() == 25 * present.len(), "{case}: {} pictures", d.video.len());
            let fetched: Vec<usize> = present.iter().map(|k| server.count(&format!("/live/seg{k}.ts"))).collect();
            expect!(bad, fetched.iter().all(|c| *c == 1), "{case}: present segments fetched {fetched:?} times");
        }
        finish(bad);
    }

    #[test]
    fn a_live_audio_stream_plays_its_segments_in_order() {
        let playlist = dynamic(
            "/a/index.m3u8",
            PLAYLIST,
            Box::new(|n| {
                let text = String::from_utf8(fixture_bytes("hls/audio_only/index.m3u8")).unwrap();
                // The recording as a broadcast that is complete from the second reload.
                if n == 0 { text.replace("#EXT-X-ENDLIST\n", "").replace("#EXT-X-PLAYLIST-TYPE:VOD\n", "").into_bytes() } else { text.into_bytes() }
            }),
        );
        let server = HlsServer::start(
            std::iter::once(playlist).chain(fixture_dir("audio_only", "a").into_iter().filter(|r| !r.path.ends_with(".m3u8"))).collect(),
        );

        let opened = open_url(&server.url("/a/index.m3u8")).expect("open");
        let mut demux = opened.demuxer;
        let (d, err) = drain(demux.as_mut());

        assert_eq!((opened.live, err), (true, None));
        let fetched: Vec<String> = server.paths().into_iter().filter(|p| p.ends_with(".ts")).collect();
        assert_eq!(fetched, segment_paths("a", "ts", 2..5));
        assert_eq!(gaps("audio", &audio_pts(&d), AAC_STEP), Vec::<String>::new());
    }
}

/// CBC with PKCS#7 padding, from the AES block cipher.
fn aes128_cbc_encrypt(plain: &[u8], key: &[u8; 16], iv: &[u8; 16]) -> Vec<u8> {
    use aes::cipher::{Array, BlockCipherEncrypt, KeyInit};
    let cipher = aes::Aes128::new(&Array::from(*key));
    let pad = 16 - plain.len() % 16;
    let mut data = plain.to_vec();
    data.extend(std::iter::repeat_n(pad as u8, pad));
    let mut previous = *iv;
    let (blocks, _) = data.as_chunks_mut::<16>();
    for block in blocks {
        for (b, p) in block.iter_mut().zip(previous) {
            *b ^= p;
        }
        let mut arr = Array::from(*block);
        cipher.encrypt_block(&mut arr);
        block.copy_from_slice(&arr);
        previous = *block;
    }
    data
}

const KEY: [u8; 16] = [0x2b, 0x7e, 0x15, 0x16, 0x28, 0xae, 0xd2, 0xa6, 0xab, 0xf7, 0x15, 0x88, 0x09, 0xcf, 0x4f, 0x3c];

mod encryption {
    use super::*;

    #[test]
    fn an_aes_128_stream_made_by_ffmpeg_with_an_explicit_iv_plays_and_its_key_is_fetched_once() {
        let server = HlsServer::start(fixture_dir("aes", "k"));

        let mut demux = open_url(&server.url("/k/index.m3u8")).expect("open").demuxer;
        let (d, err) = drain(demux.as_mut());

        assert_eq!(err, None);
        let mut bad = Vec::new();
        check_av(&mut bad, &d, 2.0);
        finish(bad);
        let mut paths = server.paths();
        paths.sort();
        assert_eq!(paths, ["/k/index.m3u8", "/k/key.bin", "/k/seg0.ts", "/k/seg1.ts"]);
    }

    #[test]
    fn without_an_iv_each_segment_is_decrypted_with_its_media_sequence_number() {
        // The SDT packet FFmpeg puts first is dropped so that the first AES block holds the
        // program association table: decrypted with any other IV its program number (bytes
        // 13-14) would come out as 0 for sequence 300 (IV byte 14 = 0x01) and nothing would play.
        let plain: Vec<Vec<u8>> = (1..=2).map(|n| fixture_bytes(&format!("hls/ts/seg{n}.ts"))[188..].to_vec()).collect();
        assert_eq!((plain[0][0], plain[0][1] & 0x1F, plain[0][2], &plain[0][13..15]), (0x47, 0, 0, &[0u8, 1][..]), "test precondition: a PAT for program 1 comes first");
        let mut routes = vec![
            bytes(
                "/s/index.m3u8",
                PLAYLIST,
                b"#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:300\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n\
                  #EXTINF:1.0,\nseg300.ts\n#EXTINF:1.0,\nseg301.ts\n#EXT-X-ENDLIST\n"
                    .to_vec(),
            ),
            bytes("/s/key.bin", "application/octet-stream", KEY.to_vec()),
        ];
        for (i, seq) in [300u64, 301].into_iter().enumerate() {
            let mut iv = [0u8; 16];
            iv[8..].copy_from_slice(&seq.to_be_bytes());
            routes.push(bytes(&format!("/s/seg{seq}.ts"), "video/mp2t", aes128_cbc_encrypt(&plain[i], &KEY, &iv)));
        }
        let server = HlsServer::start(routes);

        let mut demux = open_url(&server.url("/s/index.m3u8")).expect("open").demuxer;
        let (d, err) = drain(demux.as_mut());

        assert_eq!(err, None);
        let mut bad = Vec::new();
        check_av(&mut bad, &d, 2.0);
        finish(bad);
    }

    #[test]
    fn drm_and_broken_encryption_are_refused_with_a_message_saying_why() {
        let seg = fixture_bytes("hls/aes/seg0.ts");
        let mut wrong_key = KEY;
        wrong_key[15] ^= 0x5a;
        let drm = |method: &str| format!("this stream is protected with {method} (DRM), which playr cannot play");
        let cases = [
            ("SAMPLE-AES", "METHOD=SAMPLE-AES,URI=\"key.bin\",KEYFORMAT=\"identity\"", KEY.to_vec(), seg.clone(), drm("SAMPLE-AES")),
            ("SAMPLE-AES-CTR", "METHOD=SAMPLE-AES-CTR,URI=\"key.bin\"", KEY.to_vec(), seg.clone(), drm("SAMPLE-AES-CTR")),
            ("another method", "METHOD=ISO-23001-7,URI=\"key.bin\"", KEY.to_vec(), seg.clone(), drm("ISO-23001-7")),
            ("wrong key", "METHOD=AES-128,URI=\"key.bin\",IV=0x000102030405060708090a0b0c0d0e0f", wrong_key.to_vec(), seg.clone(), "an encrypted segment did not decrypt (wrong key?)".to_string()),
            ("key of 15 bytes", "METHOD=AES-128,URI=\"key.bin\"", KEY[..15].to_vec(), seg.clone(), "the stream's decryption key is not 16 bytes".to_string()),
            ("no key address", "METHOD=AES-128", KEY.to_vec(), seg.clone(), "an encrypted segment names no key".to_string()),
            ("segment not whole blocks", "METHOD=AES-128,URI=\"key.bin\"", KEY.to_vec(), seg[..1000].to_vec(), "an encrypted segment is not a whole number of AES blocks".to_string()),
        ];
        let mut bad = Vec::new();
        for (case, key_line, key, segment, expected) in cases {
            let text = format!("#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-KEY:{key_line}\n#EXTINF:1.0,\nseg0.ts\n#EXT-X-ENDLIST\n");
            let server = HlsServer::start(vec![
                bytes("/e/index.m3u8", PLAYLIST, text.into_bytes()),
                bytes("/e/key.bin", "application/octet-stream", key),
                bytes("/e/seg0.ts", "video/mp2t", segment),
            ]);
            let actual = open_err(&server.url("/e/index.m3u8"));
            expect!(bad, actual == expected, "{case}: {actual:?}");
        }
        finish(bad);
    }
}

mod master_playlists {
    use super::*;

    const MASTER: &str = "#EXTM3U\n\
        #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"Commentary\",LANGUAGE=\"pl\",AUTOSELECT=YES,URI=\"audio_other/index.m3u8\"\n\
        #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"Main\",LANGUAGE=\"en\",DEFAULT=YES,URI=\"audio_only/index.m3u8\"\n\
        #EXT-X-STREAM-INF:BANDWIDTH=9000000,RESOLUTION=3840x2160,CODECS=\"avc1.640033,mp4a.40.2\",AUDIO=\"aud\"\n\
        uhd/index.m3u8\n\
        #EXT-X-STREAM-INF:BANDWIDTH=7000000,RESOLUTION=1920x1080,CODECS=\"vp09.00.40.08,mp4a.40.2\",AUDIO=\"aud\"\n\
        vp9/index.m3u8\n\
        #EXT-X-STREAM-INF:BANDWIDTH=600000,RESOLUTION=160x120,CODECS=\"avc1.64000c,mp4a.40.2\",AUDIO=\"aud\"\n\
        video_only/index.m3u8\n\
        #EXT-X-STREAM-INF:BANDWIDTH=200000,RESOLUTION=80x60,CODECS=\"avc1.64000c,mp4a.40.2\",AUDIO=\"aud\"\n\
        low/index.m3u8\n";

    fn server() -> HlsServer {
        let mut routes = vec![bytes("/m/master.m3u8", PLAYLIST, MASTER.as_bytes().to_vec())];
        routes.extend(fixture_dir("video_only", "m/video_only"));
        routes.extend(fixture_dir("audio_only", "m/audio_only"));
        routes.extend(fixture_dir("video_only", "m/low"));
        HlsServer::start(routes)
    }

    fn requested_under(server: &HlsServer, folder: &str) -> Vec<String> {
        server.paths().into_iter().filter(|p| p.starts_with(&format!("/m/{folder}/"))).collect()
    }

    #[test]
    fn the_best_playable_variant_up_to_1080p_plays_with_the_default_audio_rendition() {
        let server = server();

        let opened = open_url(&server.url("/m/master.m3u8")).expect("open");
        let mut demux = opened.demuxer;
        let info = demux.info().clone();
        let (d, err) = drain(demux.as_mut());

        let video = info.video.as_ref().map(|v| (v.codec.clone(), v.width, v.height));
        assert_eq!((opened.live, opened.secure, err), (false, true, None));
        assert_eq!((video, aac_track(&info.audio)), (Some((VideoCodec::H264, 160, 120)), Some((true, Some(44100)))));
        assert_eq!((info.duration, info.seekable), (Some(extinf_sum("video_only").max(extinf_sum("audio_only"))), true));
        let mut bad = Vec::new();
        check_av(&mut bad, &d, 4.0);
        finish(bad);
        assert_eq!(server.count("/m/master.m3u8"), 1);
        for (folder, wanted) in [("uhd", false), ("vp9", false), ("low", false), ("audio_other", false), ("video_only", true), ("audio_only", true)] {
            let got = requested_under(&server, folder);
            assert_eq!(!got.is_empty(), wanted, "{folder}: {got:?}");
        }
        assert_eq!(server.count("/m/video_only/index.m3u8") + server.count("/m/audio_only/index.m3u8"), 2);
    }

    #[test]
    fn for_sound_only_the_cheapest_variant_is_read_but_only_the_audio_rendition_is_downloaded() {
        let server = server();

        let opened = hls::open(&Location::Url(server.url("/m/master.m3u8")), None, &[], true, no_abort()).expect("open");
        let mut demux = opened.demuxer;
        let info = demux.info().clone();
        let (d, err) = drain(demux.as_mut());

        assert_eq!((info.video.is_none(), aac_track(&info.audio), err), (true, Some((true, Some(44100))), None));
        assert_eq!(requested_under(&server, "low"), ["/m/low/index.m3u8"], "the variant's playlist only, no picture");
        assert_eq!(requested_under(&server, "video_only"), Vec::<String>::new());
        let mut expected = vec!["/m/audio_only/index.m3u8".to_string()];
        expected.extend(segment_paths("m/audio_only", "ts", 0..5));
        assert_eq!(requested_under(&server, "audio_only"), expected);
        assert!(d.audio.len() > 150, "{} audio packets in 4 s", d.audio.len());
    }

    #[test]
    fn an_audio_rendition_without_an_address_leaves_the_sound_in_the_variant() {
        let master = "#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"main\",DEFAULT=YES\n\
                      #EXT-X-STREAM-INF:BANDWIDTH=900000,RESOLUTION=160x120,CODECS=\"avc1.64000c,mp4a.40.2\",AUDIO=\"a\"\nts/index.m3u8\n";
        let mut routes = vec![bytes("/m/master.m3u8", PLAYLIST, master.as_bytes().to_vec())];
        routes.extend(fixture_dir("ts", "m/ts"));
        let server = HlsServer::start(routes);

        let mut demux = open_url(&server.url("/m/master.m3u8")).expect("open").demuxer;
        let (d, err) = drain(demux.as_mut());

        assert_eq!(err, None);
        let mut bad = Vec::new();
        check_av(&mut bad, &d, 6.0);
        finish(bad);
        let mut expected = vec!["/m/master.m3u8".to_string(), "/m/ts/index.m3u8".to_string()];
        expected.extend(segment_paths("m/ts", "ts", 0..6));
        assert_eq!(server.paths(), expected);
    }

    #[test]
    fn playlists_that_lead_to_nothing_playable_say_why() {
        let cases = [
            (
                "only unplayable codecs",
                "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1,CODECS=\"vp09.00.40.08,mp4a.40.2\"\nvp9.m3u8\n\
                 #EXT-X-STREAM-INF:BANDWIDTH=2,CODECS=\"av01.0.08M.08\"\nav1.m3u8\n",
                "the HLS stream uses codecs playr cannot play (vp09.00.40.08,mp4a.40.2; av01.0.08M.08)",
            ),
            ("no variant left", "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nfile:///etc/passwd\n", "the HLS playlist lists no streams"),
            ("a variant that is a master again", "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nmaster.m3u8\n", "an HLS variant playlist lists further variants"),
            ("a recording without segments", "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-ENDLIST\n", "the HLS playlist lists no segments"),
            ("not HLS", "<!doctype html><html></html>", "not an HLS playlist"),
        ];
        let mut bad = Vec::new();
        for (case, text, expected) in cases {
            let server = HlsServer::start(vec![bytes("/x/master.m3u8", PLAYLIST, text.as_bytes().to_vec())]);
            let actual = open_err(&server.url("/x/master.m3u8"));
            expect!(bad, actual == expected, "{case}: {actual:?}");
        }
        finish(bad);
    }
}

/// A transport stream segment of H.264 PES packets (pts in 90 kHz ticks, payload).
fn crafted_segment(pes: &[(u64, &[u8])]) -> Vec<u8> {
    let mut out = pat(&[(1, 0x1000)]);
    out.extend(pmt(0x1000, &[], &[(0x1B, 0x100, vec![])]));
    for (pts, data) in pes {
        out.extend(pes_packets(0x100, Some(*pts), data));
    }
    out
}

mod timestamps {
    use super::*;

    const WRAP: u64 = 1 << 33;

    #[test]
    fn timestamps_follow_the_playlist_through_drift_discontinuities_jumps_and_the_33_bit_wrap() {
        let r = 900_000; // the stream's clock starts at 10 s
        let segments: Vec<(bool, Vec<u8>)> = vec![
            // Anchored at 0. The first PES opens with the tail of a picture from before the
            // stream (dropped: there is nothing to append it to); the second PES starts with
            // the end of the first picture.
            (false, crafted_segment(&[(r, &[0xEE, 0, 0, 0, 1, 0x65, 0xA0]), (r + 3600, &[0xA1, 0xA2, 0, 0, 1, 0x41, 0xA3])])),
            // 0.1 s late but within 2 s: continues the clock. The middle PES has no start code.
            (false, crafted_segment(&[(r + 99_000, &[0, 0, 1, 0x65, 0xB0]), (r + 102_600, &[0xB1, 0xB2]), (r + 106_200, &[0, 0, 1, 0x41, 0xB3])])),
            // Would be 3.5 s on the clock (1.5 s off), but a discontinuity re-anchors it to 2 s.
            (true, crafted_segment(&[(r + 315_000, &[0, 0, 1, 0x65, 0xC0]), (r + 318_600, &[0, 0, 1, 0x41, 0xC1])])),
            // 97 s ahead without a tag: re-anchored to 3 s.
            (false, crafted_segment(&[(r + 9_000_000, &[0, 0, 1, 0x65, 0xD0]), (r + 9_003_600, &[0, 0, 1, 0x41, 0xD1])])),
            // The 33-bit clock wraps between the two pictures of this segment.
            (false, crafted_segment(&[(WRAP - 1800, &[0, 0, 1, 0x65, 0xE0]), (1800, &[0, 0, 1, 0x41, 0xE1])])),
            // After the wrap the clock reads 1.02 s: far from the playlist's 5 s, re-anchored.
            (false, crafted_segment(&[(91_800, &[0, 0, 1, 0x65, 0xF0]), (95_400, &[0, 0, 1, 0x41, 0xF1])])),
        ];
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("hls-crafted");
        std::fs::create_dir_all(&dir).unwrap();
        let mut text = String::from("#EXTM3U\n#EXT-X-TARGETDURATION:1\n");
        for (n, (discontinuity, data)) in segments.iter().enumerate() {
            std::fs::write(dir.join(format!("s{n}.ts")), data).unwrap();
            if *discontinuity {
                text.push_str("#EXT-X-DISCONTINUITY\n");
            }
            text.push_str(&format!("#EXTINF:1.0,\ns{n}.ts\n"));
        }
        text.push_str("#EXT-X-ENDLIST\n");
        std::fs::write(dir.join("index.m3u8"), &text).unwrap();

        let opened = hls::open(&Location::File(dir.join("index.m3u8")), None, &[], false, no_abort()).expect("open");
        let mut demux = opened.demuxer;
        let info = demux.info().clone();
        let (d, err) = drain(demux.as_mut());

        assert_eq!((info.video.map(|v| v.codec), info.audio.is_none(), err), (Some(VideoCodec::H264), true, None));
        let expected: Vec<(f64, Vec<u8>)> = vec![
            (0.0, vec![0, 0, 0, 1, 0x65, 0xA0, 0xA1, 0xA2]),
            (0.04, vec![0, 0, 1, 0x41, 0xA3]),
            (1.1, vec![0, 0, 1, 0x65, 0xB0, 0xB1, 0xB2]),
            (1.18, vec![0, 0, 1, 0x41, 0xB3]),
            (2.0, vec![0, 0, 1, 0x65, 0xC0]),
            (2.04, vec![0, 0, 1, 0x41, 0xC1]),
            (3.0, vec![0, 0, 1, 0x65, 0xD0]),
            (3.04, vec![0, 0, 1, 0x41, 0xD1]),
            (4.0, vec![0, 0, 1, 0x65, 0xE0]),
            (4.04, vec![0, 0, 1, 0x41, 0xE1]),
            (5.0, vec![0, 0, 1, 0x65, 0xF0]),
            (5.04, vec![0, 0, 1, 0x41, 0xF1]),
        ];
        let actual: Vec<(f64, Vec<u8>)> = d.video.iter().map(|(pts, data)| (*pts, data.to_vec())).collect();
        assert_eq!(actual.len(), expected.len(), "{actual:?}");
        let mut bad = Vec::new();
        for (i, ((pts, data), (want_pts, want_data))) in actual.iter().zip(&expected).enumerate() {
            expect!(bad, near(*pts, *want_pts, 1e-6) && data == want_data, "picture {i}: ({pts}, {data:02x?}), expected ({want_pts}, {want_data:02x?})");
        }
        finish(bad);
    }
}

mod unsupported {
    use super::*;

    /// A one-segment recording on disk whose transport stream holds `streams` (type, pid), each
    /// with one PES packet.
    fn recording(name: &str, streams: &[(u8, u16)]) -> PathBuf {
        let mut seg = pat(&[(1, 0x1000)]);
        seg.extend(pmt(0x1000, &[], &streams.iter().map(|&(t, pid)| (t, pid, vec![])).collect::<Vec<_>>()));
        for &(_, pid) in streams {
            seg.extend(pes_packets(pid, Some(900_000), &[0, 0, 1, 0x65, 0x77]));
        }
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("hls-unsupported").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("s0.ts"), seg).unwrap();
        std::fs::write(dir.join("index.m3u8"), "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1.0,\ns0.ts\n#EXT-X-ENDLIST\n").unwrap();
        dir.join("index.m3u8")
    }

    #[test]
    fn a_stream_with_a_picture_and_ac3_sound_plays_the_picture_and_notes_the_sound() {
        let index = recording("h264_ac3", &[(0x1B, 0x100), (0x81, 0x101)]);

        let opened = hls::open(&Location::File(index), None, &[], false, no_abort()).expect("open");
        let info = opened.demuxer.info();

        let got = (info.video.as_ref().map(|v| v.codec.clone()), info.audio.is_none(), info.notes.clone());
        assert_eq!(got, (Some(VideoCodec::H264), true, vec!["AC-3 sound in this stream cannot be played".to_string()]));
    }

    #[test]
    fn a_stream_with_nothing_playable_names_what_it_holds() {
        let cases = [
            ("E-AC-3 sound only", vec![(0x87u8, 0x101u16)], "E-AC-3 sound in this stream cannot be played"),
            ("MPEG-2 video and AC-3 sound", vec![(0x02, 0x100), (0x81, 0x101)], "MPEG-2 video in this stream cannot be played; AC-3 sound in this stream cannot be played"),
        ];
        let mut bad = Vec::new();
        for (case, streams, expected) in cases {
            let index = recording(&case.replace(' ', "_"), &streams);
            let actual = match hls::open(&Location::File(index), None, &[], false, no_abort()) {
                Ok(_) => "opened".to_string(),
                Err(e) => e,
            };
            expect!(bad, actual == expected, "{case}: {actual:?}");
        }
        finish(bad);
    }

    #[test]
    fn segments_that_hold_no_media_at_all_end_in_a_readable_error() {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("hls-unsupported").join("garbage");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("s0.bin"), b"this is not a media segment".repeat(40)).unwrap();
        std::fs::write(dir.join("index.m3u8"), "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1.0,\ns0.bin\n#EXT-X-ENDLIST\n").unwrap();

        let actual = match hls::open(&Location::File(dir.join("index.m3u8")), None, &[], false, no_abort()) {
            Ok(_) => "opened".to_string(),
            Err(e) => e,
        };

        assert_eq!(actual, "the HLS stream ended before any picture or sound arrived");
    }
}

mod aborting {
    use super::*;

    #[test]
    fn the_abort_flag_ends_a_wait_for_a_stalled_segment_at_once_and_stops_the_fetcher() {
        let mut routes = vec![bytes("/s/index.m3u8", PLAYLIST, media_playlist(0, 0..=3, true))];
        for n in 0..4 {
            let seg = fixture_bytes(&format!("hls/ts/seg{n}.ts"));
            routes.push(if n == 1 { held("/s/seg1.ts", "video/mp2t", seg) } else { bytes(&format!("/s/seg{n}.ts"), "video/mp2t", seg) });
        }
        let server = HlsServer::start(routes);
        let abort = no_abort();
        let mut demux = hls::open(&Location::Url(server.url("/s/index.m3u8")), None, &[], false, abort.clone()).expect("open").demuxer;
        let mut played = 0;
        let setter = {
            let abort = abort.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(300));
                abort.store(true, Ordering::Relaxed);
            })
        };

        let started = Instant::now();
        let result = loop {
            match demux.next_packet() {
                Ok(Some(_)) => played += 1,
                other => break other.map(|p| p.map(|p| p.pts())),
            }
        };
        let waited = started.elapsed();
        setter.join().unwrap();
        server.release();
        std::thread::sleep(Duration::from_millis(1500));

        assert_eq!(result, Err("stream closed".to_string()));
        assert!(waited < Duration::from_secs(2), "next_packet took {waited:?} to notice the abort");
        assert!(played >= 25, "the first segment ({played} packets) plays before the stall");
        assert_eq!((server.count("/s/seg2.ts"), server.count("/s/seg3.ts")), (0, 0), "{:?}", server.paths());
    }
}

mod entry_points {
    use super::*;

    #[test]
    fn open_url_hands_back_hls_playlists_found_by_media_type_or_by_name() {
        let master = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1,RESOLUTION=160x120\nts/index.m3u8\n";
        let media = String::from_utf8(media_playlist(0, 0..=1, true)).unwrap();
        let server = HlsServer::start(vec![bytes("/typed", PLAYLIST, master.as_bytes().to_vec()), bytes("/named.m3u8", "text/plain", media.clone().into_bytes())]);
        let mut bad = Vec::new();
        for (case, path, expected) in [("master by media type", "/typed", master.to_string()), ("media playlist by name", "/named.m3u8", media)] {
            let url = server.url(path);
            match playr::net::open_url(&url, no_abort(), Box::new(|_| {})) {
                Ok(s) => expect!(bad, s.hls.as_deref() == Some(expected.as_str()) && s.url == url, "{case}: {:?} at {}", s.hls, s.url),
                Err(e) => bad.push(format!("{case}: {e}")),
            }
        }
        finish(bad);
        assert_eq!(server.paths(), ["/typed", "/named.m3u8"], "no segment or variant fetched");
    }

    fn page(formats: Vec<playr::sites::Format>) -> playr::sites::Page {
        playr::sites::Page { site: "TestSite".into(), title: "T".into(), duration: None, headers: vec![("Referer".into(), "https://site.example/v/1".into())], formats, only_unplayable: false }
    }

    fn hls_format(id: &str, url: String, audio: Option<String>) -> playr::sites::Format {
        playr::sites::Format { id: id.into(), url, ext: Some("mp4".into()), width: None, height: Some(120), tbr: None, hls: true, audio }
    }

    #[test]
    fn a_sites_muxed_hls_format_and_a_picture_only_one_with_its_sound_open() {
        let mut routes = fixture_dir("ts", "s/ts");
        routes.extend(fixture_dir("video_only", "s/v"));
        routes.extend(fixture_dir("audio_only", "s/a"));
        let server = HlsServer::start(routes);
        let muxed = hls_format("hls-120", server.url("/s/ts/index.m3u8"), None);
        let pair = hls_format("hls-v", server.url("/s/v/index.m3u8"), Some(server.url("/s/a/index.m3u8")));
        let mut bad = Vec::new();
        for (case, format, secs) in [("muxed", muxed, 6.0), ("picture with separate sound", pair, 4.0)] {
            let p = page(vec![format.clone()]);
            match playr::sites::open(&p, &format, no_abort()) {
                Ok((mut demux, secure)) => {
                    let info = demux.info().clone();
                    let (d, err) = drain(demux.as_mut());
                    expect!(bad, secure && err.is_none(), "{case}: secure {secure}, {err:?}");
                    expect!(bad, info.video.is_some() && info.audio.is_some(), "{case}: tracks {:?} / {}", info.video, info.audio.is_some());
                    let mut case_bad = Vec::new();
                    check_av(&mut case_bad, &d, secs);
                    bad.extend(case_bad.into_iter().map(|e| format!("{case}: {e}")));
                }
                Err(e) => bad.push(format!("{case}: {e}")),
            }
        }
        finish(bad);
    }

    fn live_video(server: &HlsServer) -> playr::youtube::Video {
        let vf = |id: &str, height: u32, path: &str| playr::youtube::VideoFormat { id: id.into(), url: server.url(path), width: height * 4 / 3, height, fps: 25.0, bitrate: 0.0 };
        playr::youtube::Video {
            id: "aB3_-x9Zq0Y".into(),
            title: "Live".into(),
            channel: None,
            duration: None,
            live: true,
            video: vec![vf("96", 1080, "/y/big/index.m3u8"), vf("93", 360, "/y/small/index.m3u8")],
            audio: vec![],
            captions: vec![],
            headers: vec![],
        }
    }

    #[test]
    fn a_youtube_live_video_plays_its_chosen_hls_quality_and_sound_only_takes_the_smallest() {
        let mut routes = fixture_dir("ts", "y/big");
        routes.extend(fixture_dir("hevc", "y/small"));
        let server = HlsServer::start(routes);
        let video = live_video(&server);
        let cases = [
            ("default: the best", playr::youtube::Choice::default(), false, Some("96"), Some(VideoCodec::H264), "/y/big/index.m3u8"),
            ("chosen", playr::youtube::Choice { video: Some("93".into()), audio: None }, false, Some("93"), Some(VideoCodec::Hevc), "/y/small/index.m3u8"),
            ("sound only: the smallest", playr::youtube::Choice::default(), true, None, None, "/y/small/index.m3u8"),
        ];
        let mut bad = Vec::new();
        for (case, choice, audio_only, chosen, codec, playlist) in cases {
            let before = server.count(playlist);
            match playr::youtube::open(&video, &choice, audio_only, no_abort()) {
                Ok(opened) => {
                    let info = opened.demuxer.info();
                    let got = (opened.choice.video.as_deref(), info.video.as_ref().map(|v| v.codec.clone()), info.audio.is_some());
                    expect!(bad, got == (chosen, codec, true), "{case}: {got:?}");
                    expect!(bad, server.count(playlist) == before + 1, "{case}: {playlist} was not read");
                }
                Err(e) => bad.push(format!("{case}: {e}")),
            }
        }
        finish(bad);
    }
}

fn refusal_prefix(host: &str) -> String {
    format!(
        "{host} could not be opened over an encrypted connection (https), and playr is set to encrypted connections only, \
         so it was not opened unencrypted. Untick \u{1f512} Encrypted connections only to play it anyway. ("
    )
}

fn is_child() -> bool {
    std::env::var_os(CHILD).is_some()
}

/// In the test process: runs `test` alone in a copy of this binary and returns false. In that
/// copy: returns true, and the caller runs the test body.
fn in_child(test: &str) -> bool {
    if is_child() {
        return true;
    }
    let out = Command::new(std::env::current_exe().expect("test binary"))
        .args([test, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .output()
        .expect("start the child test");
    let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "{test} failed in the child process:\n{stdout}\n{stderr}");
    assert!(stdout.contains("test result: ok. 1 passed"), "{test} did not run in the child process:\n{stdout}");
    false
}

mod privacy {
    use super::*;
    use own_ip::{OwnServer, Seen, route};

    fn remote_segment() -> Option<OwnServer> {
        let server = OwnServer::start(vec![
            route("/seg0.ts", "video/mp2t", fixture_bytes("hls/ts/seg0.ts"), false, true),
            route("/r/index.m3u8", PLAYLIST, media_playlist(0, 0..=0, true), false, true),
            route("/r/seg0.ts", "video/mp2t", fixture_bytes("hls/ts/seg0.ts"), false, true),
        ]);
        if server.is_none() {
            eprintln!("skipped: this computer has no network address other than loopback");
        }
        server
    }

    fn local_playlist_of(remote: &OwnServer) -> HlsServer {
        let text = format!("#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1.0,\n{}\n#EXT-X-ENDLIST\n", remote.url("/seg0.ts"));
        HlsServer::start(vec![bytes("/l/index.m3u8", PLAYLIST, text.into_bytes())])
    }

    #[test]
    fn with_the_setting_on_a_segment_server_that_speaks_only_plain_http_is_refused() {
        assert!(playr::privacy::https_only(), "test precondition: the setting is on by default");
        let Some(remote) = remote_segment() else { return };
        let local = local_playlist_of(&remote);

        let error = open_err(&local.url("/l/index.m3u8"));

        let prefix = format!("{}cannot download ", refusal_prefix(&remote.host()));
        assert!(error.starts_with(&prefix) && error.ends_with(')') && error.len() > prefix.len() + 1, "{error:?}");
        let seen = remote.seen();
        assert!(!seen.is_empty() && seen.iter().all(|s| *s == Seen::Tls), "only TLS attempts may reach the server: {seen:?}");
    }

    #[test]
    fn with_the_setting_off_anything_fetched_over_plain_http_makes_the_stream_insecure() {
        if !in_child("privacy::with_the_setting_off_anything_fetched_over_plain_http_makes_the_stream_insecure") {
            return;
        }
        playr::privacy::set_https_only(false);
        let Some(remote) = remote_segment() else { return };
        let local = local_playlist_of(&remote);
        let all_local = HlsServer::start(fixture_dir("ts", "l2"));

        let segment_remote = open_url(&local.url("/l/index.m3u8")).expect("open with a remote segment");
        let playlist_remote = open_url(&remote.url("/r/index.m3u8")).expect("open a remote playlist");
        let loopback_only = open_url(&all_local.url("/l2/index.m3u8")).expect("open on loopback");

        assert_eq!((segment_remote.secure, playlist_remote.secure, loopback_only.secure), (false, false, true));
        let seen = remote.seen();
        let first_get = seen.iter().position(|s| *s == Seen::Get("/seg0.ts".into()));
        assert!(first_get.is_some_and(|i| i > 0 && seen[i - 1] == Seen::Tls), "https was not tried before plain http: {seen:?}");
    }
}

mod player {
    use super::*;
    use playr::player::{Player, State, Status};

    static AUDIO_DEVICE: Mutex<()> = Mutex::new(());

    fn start() -> (Player, MutexGuard<'static, ()>) {
        let device = AUDIO_DEVICE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let player = Player::new(Box::new(|| {}));
        player.shared.audio.set_volume(0.0);
        (player, device)
    }

    fn open_and_wait(player: &Player, target: &str) -> Status {
        player.open(target);
        let started = Instant::now();
        loop {
            let status = player.shared.status();
            if status.source == target && !matches!(status.state, State::Idle | State::Loading) {
                return status;
            }
            assert!(started.elapsed() < Duration::from_secs(30), "timed out opening {target}: {status:?}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// (state, duration, seekable, live, picture size, sound, info, error, notes)
    type Seen = (State, Option<f64>, bool, bool, Option<(u32, u32)>, bool, String, Option<String>, Vec<String>);

    fn seen(s: &Status) -> Seen {
        (s.state, s.duration, s.seekable, s.live, s.video, s.has_audio, s.info.clone(), s.error.clone(), s.notes.clone())
    }

    #[test]
    fn the_player_plays_an_hls_address_and_a_local_m3u8_file_as_seekable_recordings() {
        let server = HlsServer::start(fixture_dir("ts", "p"));
        let file: PathBuf = fixture("hls/ts/index.m3u8");
        let (player, _device) = start();

        let from_url = open_and_wait(&player, &server.url("/p/index.m3u8"));
        player.stop();
        let from_file = open_and_wait(&player, file.to_str().unwrap());
        player.stop();

        let expected: Seen = (State::Playing, Some(6.0), true, false, Some((160, 120)), true, "HLS · H.264 160×120".into(), None, vec![]);
        assert_eq!(seen(&from_url), expected, "from the address");
        assert_eq!(seen(&from_file), expected, "from the file");
    }
}

/// Server paths that were asked for and lie outside `under`.
fn requested_outside(server: &HlsServer, under: &[&str]) -> Vec<String> {
    server.paths().into_iter().filter(|p| !under.iter().any(|u| p.starts_with(u))).collect()
}

mod redirects {
    use super::*;

    /// `/r/index.m3u8` answers 302 to `/deep/dir/index.m3u8`, whose segment addresses are
    /// relative: they only exist under `/deep/dir/`.
    fn redirected_recording() -> HlsServer {
        let mut routes = vec![redirect("/r/index.m3u8", "/deep/dir/index.m3u8")];
        routes.extend(fixture_dir("ts", "deep/dir"));
        HlsServer::start(routes)
    }

    #[test]
    fn open_url_reports_where_a_redirected_playlist_came_from_and_its_segments_resolve_there() {
        let server = redirected_recording();

        let stream = playr::net::open_url(&server.url("/r/index.m3u8"), no_abort(), Box::new(|_| {})).expect("open_url");
        let landed = stream.url.clone();
        let opened = hls::open(&Location::Url(stream.url), stream.hls, &[], false, no_abort()).expect("hls::open");
        let mut demux = opened.demuxer;
        let (d, err) = drain(demux.as_mut());

        assert_eq!((landed, err), (server.url("/deep/dir/index.m3u8"), None));
        assert_eq!(d.video.len(), 150);
        assert_eq!(requested_outside(&server, &["/r/index.m3u8", "/deep/dir/"]), Vec::<String>::new());
    }

    #[test]
    fn hls_open_resolves_segments_against_the_address_a_playlist_was_redirected_to() {
        let server = redirected_recording();

        let mut demux = open_url(&server.url("/r/index.m3u8")).expect("open").demuxer;
        let (d, err) = drain(demux.as_mut());

        assert_eq!((d.video.len(), err), (150, None));
        let mut expected = vec!["/r/index.m3u8".to_string(), "/deep/dir/index.m3u8".to_string()];
        expected.extend(segment_paths("deep/dir", "ts", 0..6));
        assert_eq!(server.paths(), expected);
    }

    #[test]
    fn variant_and_rendition_playlists_that_redirect_resolve_their_segments_where_they_landed() {
        let master = "#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"main\",DEFAULT=YES,URI=\"a.m3u8\"\n\
                      #EXT-X-STREAM-INF:BANDWIDTH=600000,RESOLUTION=160x120,CODECS=\"avc1.64000c,mp4a.40.2\",AUDIO=\"a\"\nv.m3u8\n";
        let mut routes = vec![
            bytes("/m/master.m3u8", PLAYLIST, master.as_bytes().to_vec()),
            redirect("/m/v.m3u8", "/deep/v/index.m3u8"),
            redirect("/m/a.m3u8", "/deep/a/index.m3u8"),
        ];
        routes.extend(fixture_dir("video_only", "deep/v"));
        routes.extend(fixture_dir("audio_only", "deep/a"));
        let server = HlsServer::start(routes);

        let mut demux = open_url(&server.url("/m/master.m3u8")).expect("open").demuxer;
        let (d, err) = drain(demux.as_mut());

        assert_eq!(err, None);
        let mut bad = Vec::new();
        check_av(&mut bad, &d, 4.0);
        finish(bad);
        assert_eq!(requested_outside(&server, &["/m/", "/deep/v/", "/deep/a/"]), Vec::<String>::new());
        assert_eq!((server.count("/deep/v/seg3.ts"), server.count("/deep/a/seg4.ts")), (1, 1));
    }

    #[test]
    fn a_redirected_live_playlist_is_reloaded_and_resolved_where_it_landed() {
        let mut routes = vec![
            redirect("/r/live.m3u8", "/deep/live/index.m3u8"),
            dynamic("/deep/live/index.m3u8", PLAYLIST, Box::new(|n| media_playlist(0, if n == 0 { 0..=2 } else { 0..=4 }, n > 0))),
        ];
        routes.extend(fixture_dir("ts", "deep/live").into_iter().filter(|r| !r.path.ends_with(".m3u8")));
        let server = HlsServer::start(routes);

        let opened = open_url(&server.url("/r/live.m3u8")).expect("open");
        let mut demux = opened.demuxer;
        let (d, err) = drain(demux.as_mut());

        assert_eq!((opened.live, d.video.len(), err), (true, 125, None));
        assert_eq!(server.count("/r/live.m3u8"), 1, "reloads go to where the playlist landed");
        assert!(server.count("/deep/live/index.m3u8") >= 2, "{:?}", server.paths());
        assert_eq!(requested_outside(&server, &["/r/live.m3u8", "/deep/live/"]), Vec::<String>::new());
    }
}

/// A transport stream segment of H.264 PES packets, some without a PTS.
fn crafted_segment_opt(pes: &[(Option<u64>, &[u8])]) -> Vec<u8> {
    let mut out = pat(&[(1, 0x1000)]);
    out.extend(pmt(0x1000, &[], &[(0x1B, 0x100, vec![])]));
    for (pts, data) in pes {
        out.extend(pes_packets(0x100, *pts, data));
    }
    out
}

/// Writes `segments` (discontinuity flag, bytes) and a recording playlist naming each with
/// `#EXTINF:<extinf>` to a fresh folder; returns the playlist's path.
fn write_recording(name: &str, extinf: &str, segments: &[(bool, Vec<u8>)]) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("hls-written").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut text = String::from("#EXTM3U\n#EXT-X-TARGETDURATION:10\n");
    for (n, (discontinuity, data)) in segments.iter().enumerate() {
        std::fs::write(dir.join(format!("s{n}.ts")), data).unwrap();
        if *discontinuity {
            text.push_str("#EXT-X-DISCONTINUITY\n");
        }
        text.push_str(&format!("#EXTINF:{extinf},\ns{n}.ts\n"));
    }
    text.push_str("#EXT-X-ENDLIST\n");
    std::fs::write(dir.join("index.m3u8"), text).unwrap();
    dir.join("index.m3u8")
}

fn play_file(index: PathBuf) -> (Demuxed, Option<String>) {
    let opened = hls::open(&Location::File(index), None, &[], false, no_abort()).expect("open");
    let mut demux = opened.demuxer;
    drain(demux.as_mut())
}

/// `count` pictures 0.04 s apart from `first` (90 kHz ticks), each a distinct payload.
fn pictures(first: u64, count: u64, tag: u8) -> Vec<(Option<u64>, Vec<u8>)> {
    (0..count).map(|i| (Some(first + i * 3600), vec![0, 0, 1, if i == 0 { 0x65 } else { 0x41 }, tag, i as u8])).collect()
}

fn segment_of(pics: &[(Option<u64>, Vec<u8>)]) -> Vec<u8> {
    let refs: Vec<(Option<u64>, &[u8])> = pics.iter().map(|(p, d)| (*p, d.as_slice())).collect();
    crafted_segment_opt(&refs)
}

mod continuity {
    use super::*;

    const R: u64 = 900_000;

    #[test]
    fn a_pes_without_a_pts_continues_the_picture_before_it_and_is_dropped_with_none_before_it() {
        let seg = crafted_segment_opt(&[
            (None, &[0, 0, 1, 0x09, 0xF0]),
            (Some(R), &[0, 0, 1, 0x65, 0xA0]),
            (None, &[0, 0, 1, 0x01, 0xA1]),
            (Some(R + 3600), &[0, 0, 1, 0x41, 0xA2]),
        ]);
        let index = write_recording("no_pts", "1.0", &[(false, seg)]);

        let (d, err) = play_file(index);

        // Timestamps rounded to the microsecond: 0.04 s is 3600 ticks, not exact in binary.
        let actual: Vec<(f64, Vec<u8>)> = d.video.iter().map(|(p, b)| ((p * 1e6).round() / 1e6, b.to_vec())).collect();
        assert_eq!(err, None);
        assert_eq!(actual, [(0.0, vec![0, 0, 1, 0x65, 0xA0, 0, 0, 1, 0x01, 0xA1]), (0.04, vec![0, 0, 1, 0x41, 0xA2])]);
    }

    #[test]
    fn segments_shorter_than_their_extinf_continue_from_where_the_last_one_ended() {
        // 8 segments of 15 pictures (0.6 s) each written as EXTINF:1. By the seventh the
        // playlist's clock is 2.4 s ahead of the pictures; they must still run on evenly.
        let segments: Vec<(bool, Vec<u8>)> = (0..8).map(|n| (false, segment_of(&pictures(R + n * 54_000, 15, n as u8)))).collect();
        let index = write_recording("short_segments", "1", &segments);

        let (d, err) = play_file(index);

        let v = video_pts(&d);
        assert_eq!((v.len(), err), (120, None));
        assert_eq!(v.first(), Some(&0.0));
        assert_eq!(gaps("video", &v, 0.04), Vec::<String>::new());
    }

    #[test]
    fn a_jump_of_up_to_2_s_either_way_continues_and_a_larger_one_re_anchors_to_the_playlist() {
        // 25 pictures (1 s) per segment, EXTINF 1. Each segment starts off where the previous
        // one ended by: -1.5 s, -2.5 s, +1.5 s, +2.5 s.
        let firsts = [R, R - 45_000, R - 180_000, R + 45_000, R + 360_000];
        let segments: Vec<(bool, Vec<u8>)> = firsts.iter().enumerate().map(|(n, f)| (false, segment_of(&pictures(*f, 25, n as u8)))).collect();
        let index = write_recording("jumps", "1.0", &segments);

        let (d, err) = play_file(index);

        assert_eq!((d.video.len(), err), (125, None));
        let starts: Vec<f64> = d.video.iter().step_by(25).map(|(p, _)| (p * 1e6).round() / 1e6).collect();
        // 0; 1.0 - 1.5 = -0.5 continues; 0.5 - 2.5 re-anchors to the playlist's 2.0;
        // 3.0 + 1.5 = 4.5 continues; 5.5 + 2.5 re-anchors to 4.0.
        assert_eq!(starts, [0.0, -0.5, 2.0, 4.5, 4.0]);
        let inside: Vec<String> = d.video.chunks(25).flat_map(|c| gaps("in a segment", &c.iter().map(|(p, _)| *p).collect::<Vec<_>>(), 0.04)).collect();
        assert_eq!(inside, Vec::<String>::new());
    }

    #[test]
    fn a_segment_is_anchored_at_its_first_picture_even_when_its_sound_starts_earlier() {
        let frames = adts_frames();
        let mut seg = pat(&[(1, 0x1000)]);
        seg.extend(pmt(0x1000, &[], &[(0x1B, 0x100, vec![]), (0x0F, 0x101, vec![])]));
        // The sound starts 0.1 s (9000 ticks) before the first picture.
        seg.extend(pes_packets(0x101, Some(R - 9000), &frames[..4].concat()));
        seg.extend(pes_packets(0x100, Some(R), &[0, 0, 1, 0x65, 0x51]));
        seg.extend(pes_packets(0x100, Some(R + 3600), &[0, 0, 1, 0x41, 0x52]));
        let index = write_recording("sound_first", "1.0", &[(false, seg)]);

        let (d, err) = play_file(index);

        assert_eq!(err, None);
        let v: Vec<f64> = video_pts(&d).iter().map(|p| (p * 1e6).round() / 1e6).collect();
        assert_eq!(v, [0.0, 0.04]);
        let a = audio_pts(&d);
        assert_eq!(a.len(), 4);
        assert!(near(a[0], -0.1, 1e-6), "the sound keeps its 0.1 s lead: {a:?}");
        assert_eq!(gaps("audio", &a, AAC_STEP), Vec::<String>::new());
    }
}

mod stopping {
    use super::*;

    /// The TS recording with segment 1 trickling out slowly after its first half, padded with
    /// `padding` null packets.
    fn slow_second_segment(padding: usize) -> HlsServer {
        let mut routes = vec![bytes("/s/index.m3u8", PLAYLIST, media_playlist(0, 0..=5, true))];
        let mut null = vec![0x47, 0x1F, 0xFF, 0x10];
        null.resize(188, 0xFF);
        for n in 0..6 {
            let mut seg = fixture_bytes(&format!("hls/ts/seg{n}.ts"));
            let path = format!("/s/seg{n}.ts");
            routes.push(if n == 1 {
                (0..padding).for_each(|_| seg.extend_from_slice(&null));
                trickle(&path, "video/mp2t", seg)
            } else {
                bytes(&path, "video/mp2t", seg)
            });
        }
        HlsServer::start(routes)
    }

    fn wait_until(limit: Duration, mut done: impl FnMut() -> bool) -> Option<Duration> {
        let started = Instant::now();
        while started.elapsed() < limit {
            if done() {
                return Some(started.elapsed());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        None
    }

    #[test]
    fn a_download_in_progress_is_abandoned_promptly_after_a_seek_an_abort_or_a_drop() {
        // 12 000 null packets make a 2.3 MB segment; without them it is the fixture's 34 KB.
        // Either way its second half arrives at 320 bytes a second, far slower than the test.
        // Either way playr moves on at once and never asks for the segment again; only the
        // large one's connection is closed at once (the small one's times out by itself).
        let cases = [("seek away", 12_000), ("abort", 12_000), ("drop", 12_000), ("seek away", 0), ("abort", 0), ("drop", 0)];
        let mut bad = Vec::new();
        for (trigger, padding) in cases {
            let case = format!("{trigger}, {} KB segment", (34_216 + padding * 188) / 1024);
            let server = slow_second_segment(padding);
            let abort = no_abort();
            let mut demux = Some(hls::open(&Location::Url(server.url("/s/index.m3u8")), None, &[], false, abort.clone()).expect("open").demuxer);
            let started = wait_until(Duration::from_secs(5), || server.count("/s/seg1.ts") == 1);
            expect!(bad, started.is_some(), "{case}: the slow segment was never asked for");
            // By now the first half is read and the download waits for the trickle.
            std::thread::sleep(Duration::from_millis(300));
            match trigger {
                "seek away" => {
                    let landed = demux.as_mut().unwrap().seek(3.0);
                    expect!(bad, landed == Ok(3.0), "{case}: {landed:?}");
                }
                "abort" => abort.store(true, Ordering::Relaxed),
                _ => {
                    let dropping = Instant::now();
                    drop(demux.take());
                    expect!(bad, dropping.elapsed() < Duration::from_millis(500), "{case}: the drop took {:?}", dropping.elapsed());
                }
            }
            let moved_on = Instant::now();
            match trigger {
                "seek away" => {
                    let next = demux.as_mut().unwrap().next_packet().map(|p| p.map(|p| p.pts()));
                    expect!(bad, matches!(next, Ok(Some(pts)) if pts >= 2.95), "{case}: after the seek {next:?}");
                    expect!(bad, moved_on.elapsed() < Duration::from_secs(2), "{case}: the first packet after the seek took {:?}", moved_on.elapsed());
                }
                "abort" => {
                    // What was already read may still come out; then the abort.
                    let end = loop {
                        match demux.as_mut().unwrap().next_packet() {
                            Ok(Some(_)) => continue,
                            other => break other.map(|p| p.map(|p| p.pts())),
                        }
                    };
                    expect!(bad, end == Err("stream closed".to_string()), "{case}: {end:?}");
                    expect!(bad, moved_on.elapsed() < Duration::from_secs(1), "{case}: next_packet took {:?} to fail", moved_on.elapsed());
                }
                _ => {}
            }
            if padding > 0 {
                // A large segment is read in chunks: the connection itself is closed at once.
                let hung_up = wait_until(Duration::from_secs(5), || server.hung_up());
                expect!(bad, hung_up.is_some_and(|t| t < Duration::from_secs(2)), "{case}: the download was left running ({hung_up:?})");
            } else {
                // A small one is left to time out on its own (BODY_STALL); playr does not wait for it.
                std::thread::sleep(Duration::from_millis(1500));
            }
            expect!(bad, server.count("/s/seg1.ts") == 1, "{case}: the slow segment was asked for again");
            if trigger != "seek away" {
                expect!(bad, server.count("/s/seg2.ts") == 0, "{case}: the fetcher went on to segment 2");
            }
        }
        finish(bad);
    }
}

mod later_fetches {
    use super::*;
    use own_ip::{OwnServer, route};
    use playr::player::{Player, State};

    struct Fake {
        info: playr::media::MediaInfo,
        unencrypted: bool,
    }

    impl Demuxer for Fake {
        fn info(&self) -> &playr::media::MediaInfo {
            &self.info
        }
        fn next_packet(&mut self) -> Result<Option<Packet>, String> {
            Ok(None)
        }
        fn seek(&mut self, secs: f64) -> Result<f64, String> {
            Ok(secs)
        }
        fn unencrypted(&self) -> bool {
            self.unencrypted
        }
    }

    #[test]
    fn picture_and_sound_together_are_unencrypted_when_either_side_is() {
        let fake = |unencrypted| Box::new(Fake { info: Default::default(), unencrypted }) as Box<dyn Demuxer + Send>;
        let cases = [(false, false, false), (true, false, true), (false, true, true), (true, true, true)];
        let mut bad = Vec::new();
        for (picture, sound, expected) in cases {
            let merged = playr::dash::MergedDemuxer::new(fake(picture), fake(sound));
            expect!(bad, merged.unencrypted() == expected, "picture {picture}, sound {sound}: {}", merged.unencrypted());
        }
        finish(bad);
    }

    /// A recording on loopback whose segments 4 and 5 come over plain HTTP from this
    /// computer's network address, so they are fetched only well after opening.
    fn half_remote() -> Option<(HlsServer, OwnServer)> {
        let remote = OwnServer::start(vec![
            route("/seg4.ts", "video/mp2t", fixture_bytes("hls/ts/seg4.ts"), false, true),
            route("/seg5.ts", "video/mp2t", fixture_bytes("hls/ts/seg5.ts"), false, true),
        ]);
        let Some(remote) = remote else {
            eprintln!("skipped: this computer has no network address other than loopback");
            return None;
        };
        let mut text = String::from("#EXTM3U\n#EXT-X-TARGETDURATION:1\n");
        for n in 0..6 {
            let uri = if n < 4 { format!("seg{n}.ts") } else { remote.url(&format!("/seg{n}.ts")) };
            text.push_str(&format!("#EXTINF:1.0,\n{uri}\n"));
        }
        text.push_str("#EXT-X-ENDLIST\n");
        let mut routes = vec![bytes("/h/index.m3u8", PLAYLIST, text.into_bytes())];
        routes.extend(fixture_dir("ts", "h").into_iter().filter(|r| !r.path.ends_with(".m3u8")));
        Some((HlsServer::start(routes), remote))
    }

    #[test]
    fn segments_fetched_over_plain_http_after_opening_turn_the_stream_unencrypted() {
        if !in_child("later_fetches::segments_fetched_over_plain_http_after_opening_turn_the_stream_unencrypted") {
            return;
        }
        playr::privacy::set_https_only(false);
        let Some((local, _remote)) = half_remote() else { return };

        let opened = open_url(&local.url("/h/index.m3u8")).expect("open");
        let mut demux = opened.demuxer;
        let before = demux.unencrypted();
        let (d, err) = drain(demux.as_mut());
        let after = demux.unencrypted();

        assert_eq!((opened.secure, before, after, err, d.video.len()), (true, false, true, None, 150));
    }

    #[test]
    fn the_player_adds_the_unencrypted_note_when_a_later_segment_comes_over_plain_http() {
        if !in_child("later_fetches::the_player_adds_the_unencrypted_note_when_a_later_segment_comes_over_plain_http") {
            return;
        }
        playr::privacy::set_https_only(false);
        let Some((local, _remote)) = half_remote() else { return };
        let player = Player::new(Box::new(|| {}));
        player.shared.audio.set_volume(0.0);
        let url = local.url("/h/index.m3u8");

        player.open(&url);
        let started = Instant::now();
        let status = loop {
            let s = player.shared.status();
            if s.source == url && !s.notes.is_empty() || started.elapsed() > Duration::from_secs(20) {
                break s;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        player.stop();

        let note = "\u{1f513} unencrypted connection (http): the network can see what is playing".to_string();
        assert_eq!((status.state, status.notes), (State::Playing, vec![note]));
    }
}

/// The first PES timestamp of the first video or audio stream in a fixture segment.
fn first_raw_pts(fixture_segment: &str, video: bool) -> u64 {
    let mut demux = playr::ts::TsDemux::default();
    let pes = [demux.push(&fixture_bytes(fixture_segment)), demux.flush()].concat();
    pes.into_iter().filter(|p| p.kind.is_video() == video).find_map(|p| p.pts).expect("a timestamp")
}

mod separate_playlists {
    use super::*;

    #[test]
    fn open_pair_takes_the_sound_only_from_the_sound_playlist() {
        // The picture playlist's segments carry AAC too; that sound must not be played as well.
        let mut routes = fixture_dir("ts", "p/v");
        routes.extend(fixture_dir("audio_only", "p/a"));
        let server = HlsServer::start(routes);
        let mut alone = open_url(&server.url("/p/a/index.m3u8")).expect("sound alone").demuxer;
        let (sound_alone, _) = drain(alone.as_mut());

        let opened = hls::open_pair(&Location::Url(server.url("/p/v/index.m3u8")), &Location::Url(server.url("/p/a/index.m3u8")), &[], no_abort()).expect("open_pair");
        let mut demux = opened.demuxer;
        let (d, err) = drain(demux.as_mut());

        assert_eq!(err, None);
        assert_eq!((d.video.len(), d.audio.len()), (150, sound_alone.audio.len()));
        assert_eq!(audio_pts(&d), audio_pts(&sound_alone), "every sound packet is the sound playlist's");
    }

    #[test]
    fn picture_and_sound_renditions_of_a_live_stream_share_one_clock() {
        // Live: the picture playlist (4 segments) is joined at segment 1, the sound playlist
        // (5 segments) at segment 2. Their first packets must keep the distance their own
        // timestamps have, not both start at 0.
        let live_then_ended = |dir: &'static str| -> Box<dyn Fn(usize) -> Vec<u8> + Send + Sync> {
            Box::new(move |n| {
                let text = String::from_utf8(fixture_bytes(&format!("hls/{dir}/index.m3u8"))).unwrap();
                if n == 0 { text.replace("#EXT-X-ENDLIST\n", "").replace("#EXT-X-PLAYLIST-TYPE:VOD\n", "").into_bytes() } else { text.into_bytes() }
            })
        };
        let master = "#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"main\",DEFAULT=YES,URI=\"a/index.m3u8\"\n\
                      #EXT-X-STREAM-INF:BANDWIDTH=600000,RESOLUTION=160x120,CODECS=\"avc1.64000c,mp4a.40.2\",AUDIO=\"a\"\nv/index.m3u8\n";
        let mut routes = vec![
            bytes("/l/master.m3u8", PLAYLIST, master.as_bytes().to_vec()),
            dynamic("/l/v/index.m3u8", PLAYLIST, live_then_ended("video_only")),
            dynamic("/l/a/index.m3u8", PLAYLIST, live_then_ended("audio_only")),
        ];
        routes.extend(fixture_dir("video_only", "l/v").into_iter().filter(|r| !r.path.ends_with(".m3u8")));
        routes.extend(fixture_dir("audio_only", "l/a").into_iter().filter(|r| !r.path.ends_with(".m3u8")));
        let server = HlsServer::start(routes);
        let picture_raw = first_raw_pts("hls/video_only/seg1.ts", true);
        let sound_raw = first_raw_pts("hls/audio_only/seg2.ts", false);

        let opened = open_url(&server.url("/l/master.m3u8")).expect("open");
        let mut demux = opened.demuxer;
        let (d, err) = drain(demux.as_mut());

        assert_eq!((opened.live, err), (true, None));
        assert_eq!((server.count("/l/v/seg0.ts"), server.count("/l/a/seg1.ts")), (0, 0), "test precondition: joined at segments 1 and 2");
        let expected = (picture_raw as f64 - sound_raw as f64) / 90_000.0;
        let actual = video_pts(&d)[0] - audio_pts(&d)[0];
        assert!(near(actual, expected, 1e-6), "first picture - first sound = {actual}, expected {expected}");
    }

    #[test]
    fn when_the_picture_side_holds_no_picture_the_sound_alone_is_played() {
        let master = "#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"main\",DEFAULT=YES,URI=\"alt/index.m3u8\"\n\
                      #EXT-X-STREAM-INF:BANDWIDTH=128000,CODECS=\"mp4a.40.2\",AUDIO=\"a\"\nsound/index.m3u8\n";
        let mut routes = vec![bytes("/f/master.m3u8", PLAYLIST, master.as_bytes().to_vec())];
        routes.extend(fixture_dir("audio_only", "f/sound"));
        routes.extend(fixture_dir("audio_only", "f/alt"));
        let server = HlsServer::start(routes);
        let pair = || hls::open_pair(&Location::Url(server.url("/f/sound/index.m3u8")), &Location::Url(server.url("/f/alt/index.m3u8")), &[], no_abort());
        let cases = [("master with a sound-only variant", true), ("open_pair with a sound-only picture", false)];
        let mut bad = Vec::new();
        for (case, through_master) in cases {
            let opened = if through_master { open_url(&server.url("/f/master.m3u8")) } else { pair() };
            match opened {
                Ok(opened) => {
                    let mut demux = opened.demuxer;
                    let info = demux.info().clone();
                    let (d, err) = drain(demux.as_mut());
                    expect!(bad, info.video.is_none() && aac_track(&info.audio) == Some((true, Some(44100))), "{case}: tracks {:?}", info.video);
                    expect!(bad, err.is_none() && d.video.is_empty() && !d.audio.is_empty(), "{case}: {err:?}, {} audio", d.audio.len());
                }
                Err(e) => bad.push(format!("{case}: {e}")),
            }
        }
        finish(bad);
    }
}

mod limits {
    use super::*;

    /// A TS segment of `megabytes` MB: tables, one picture with `tag`, null packets to the
    /// size, and (`late_tag`) a second picture after that.
    fn big_segment(megabytes: usize, tag: u8, late_tag: Option<u8>) -> Vec<u8> {
        let mut seg = crafted_segment(&[(900_000 + u64::from(tag) * 90_000, &[0, 0, 1, 0x65, tag])]);
        let mut null = vec![0x47, 0x1F, 0xFF, 0x10];
        null.resize(188, 0xFF);
        while seg.len() + 188 <= megabytes << 20 {
            seg.extend_from_slice(&null);
        }
        if let Some(late) = late_tag {
            seg.extend(pes_packets(0x100, Some(900_000 + u64::from(tag) * 90_000 + 3600), &[0, 0, 1, 0x41, late]));
        }
        seg
    }

    #[test]
    fn the_segment_cache_holds_at_most_48_mb_so_seeking_back_downloads_an_evicted_segment_again() {
        // Three 20 MB segments: after the third, the first no longer fits in 48 MB.
        let mut routes = vec![bytes("/c/index.m3u8", PLAYLIST, media_playlist(0, 0..=2, true))];
        for n in 0..3u8 {
            routes.push(bytes(&format!("/c/seg{n}.ts"), "video/mp2t", big_segment(20, n, None)));
        }
        let server = HlsServer::start(routes);
        let mut demux = open_url(&server.url("/c/index.m3u8")).expect("open").demuxer;
        let (d, err) = drain(demux.as_mut());
        assert_eq!((d.video.len(), err), (3, None));

        let landed = demux.seek(0.0);
        let next = demux.next_packet().map(|p| p.map(|p| p.pts()));

        assert_eq!((landed, next), (Ok(0.0), Ok(Some(0.0))));
        assert_eq!(server.count("/c/seg0.ts"), 2, "{:?}", server.paths());
    }

    #[test]
    fn a_segment_is_read_up_to_64_mb() {
        let seg = big_segment(64, 0, Some(0x77));
        assert!(seg.len() > 64 << 20, "test precondition: the second picture lies past 64 MB");
        let server = HlsServer::start(vec![bytes("/b/index.m3u8", PLAYLIST, media_playlist(0, 0..=0, true)), bytes("/b/seg0.ts", "video/mp2t", seg)]);

        let mut demux = open_url(&server.url("/b/index.m3u8")).expect("open").demuxer;
        let (d, err) = drain(demux.as_mut());

        let data: Vec<Vec<u8>> = d.video.iter().map(|(_, b)| b.to_vec()).collect();
        assert_eq!((data, err), (vec![vec![0, 0, 1, 0x65, 0]], None));
    }
}

mod reviewer_findings {
    use super::*;

    fn site_headers() -> Vec<(String, String)> {
        vec![("Referer".into(), "https://site.example/watch/7".into()), ("User-Agent".into(), "SiteAgent/3.1".into())]
    }

    /// (path, Referer, User-Agent) of every request.
    fn sent_headers(server: &HlsServer) -> Vec<(String, Option<String>, Option<String>)> {
        server.hits().into_iter().map(|h| (h.path.clone(), h.header("referer").map(str::to_string), h.header("user-agent").map(str::to_string))).collect()
    }

    #[test]
    fn a_sites_headers_go_with_every_playlist_key_and_segment_request() {
        let server = HlsServer::start(fixture_dir("aes", "k"));
        let format = playr::sites::Format { id: "hls".into(), url: server.url("/k/index.m3u8"), ext: None, width: None, height: Some(120), tbr: None, hls: true, audio: None };
        let page = playr::sites::Page { site: "TestSite".into(), title: "T".into(), duration: None, headers: site_headers(), formats: vec![format.clone()], only_unplayable: false };

        let (mut demux, _) = playr::sites::open(&page, &format, no_abort()).expect("open");
        let (_, err) = drain(demux.as_mut());

        assert_eq!(err, None);
        let referer = Some("https://site.example/watch/7".to_string());
        let agent = Some("SiteAgent/3.1".to_string());
        let mut sent = sent_headers(&server);
        sent.sort();
        let expected: Vec<_> = ["/k/index.m3u8", "/k/key.bin", "/k/seg0.ts", "/k/seg1.ts"].iter().map(|p| (p.to_string(), referer.clone(), agent.clone())).collect();
        assert_eq!(sent, expected);
    }

    #[test]
    fn a_youtube_live_videos_headers_go_with_its_playlist_and_segment_requests() {
        let server = HlsServer::start(fixture_dir("ts", "y"));
        let video = playr::youtube::Video {
            id: "aB3_-x9Zq0Y".into(),
            title: "Live".into(),
            channel: None,
            duration: None,
            live: true,
            video: vec![playr::youtube::VideoFormat { id: "95".into(), url: server.url("/y/index.m3u8"), width: 160, height: 120, fps: 25.0, bitrate: 0.0 }],
            audio: vec![],
            captions: vec![],
            headers: site_headers(),
        };

        let mut demux = playr::youtube::open(&video, &playr::youtube::Choice::default(), false, no_abort()).expect("open").demuxer;
        let (_, err) = drain(demux.as_mut());

        assert_eq!(err, None);
        let sent = sent_headers(&server);
        assert_eq!(sent.len(), 7, "{sent:?}");
        let wrong: Vec<_> = sent.iter().filter(|(_, r, a)| r.as_deref() != Some("https://site.example/watch/7") || a.as_deref() != Some("SiteAgent/3.1")).collect();
        assert_eq!(wrong, Vec::<&(String, Option<String>, Option<String>)>::new());
    }

    #[test]
    fn a_podcast_link_answering_hls_opens_as_a_stream_finite_for_a_recording_and_endless_live() {
        // The live playlist lists 0-2 until segment 2 has been downloaded, then 0-3 and the end:
        // however often open_link asks for it before playing, it is live when played.
        let seg2_served = Arc::new(AtomicBool::new(false));
        let mut routes = fixture_dir("ts", "pod");
        routes.extend(fixture_dir("ts", "pod/live").into_iter().filter(|r| r.path != "/pod/live/seg2.ts" && !r.path.ends_with(".m3u8")));
        let (flag, seg2) = (seg2_served.clone(), fixture_bytes("hls/ts/seg2.ts"));
        routes.push(dynamic("/pod/live/seg2.ts", "video/mp2t", Box::new(move |_| {
            flag.store(true, Ordering::Relaxed);
            seg2.clone()
        })));
        let flag = seg2_served.clone();
        routes.push(dynamic("/pod/live/index.m3u8", PLAYLIST, Box::new(move |_| {
            let grown = flag.load(Ordering::Relaxed);
            media_playlist(0, if grown { 0..=3 } else { 0..=2 }, grown)
        })));
        let server = HlsServer::start(routes);
        let cases = [("recording", "/pod/index.m3u8", true, 150), ("live", "/pod/live/index.m3u8", false, 100)];
        let mut bad = Vec::new();
        for (case, path, finite, pictures) in cases {
            match playr::podcast::open_link(&server.url(path), no_abort(), Box::new(|_| {})) {
                Ok(mut opened) => {
                    let (d, err) = drain(opened.demuxer.as_mut());
                    let got = (opened.ranged, opened.finite, opened.secure, err, d.video.len());
                    expect!(bad, got == (false, finite, true, None, pictures), "{case}: {got:?}");
                }
                Err(e) => bad.push(format!("{case}: {e}")),
            }
        }
        finish(bad);
    }

    #[test]
    fn segments_under_different_keys_each_decrypt_with_their_own_key_fetched_once() {
        let key2: [u8; 16] = [0x10, 0x32, 0x54, 0x76, 0x98, 0xba, 0xdc, 0xfe, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef];
        let iv = |n: u8| {
            let mut iv = [0u8; 16];
            iv[15] = n;
            iv
        };
        let text = "#EXTM3U\n#EXT-X-TARGETDURATION:1\n\
                    #EXT-X-KEY:METHOD=AES-128,URI=\"k1.bin\",IV=0x00000000000000000000000000000001\n#EXTINF:1.0,\nseg0.ts\n\
                    #EXT-X-KEY:METHOD=AES-128,URI=\"k2.bin\",IV=0x00000000000000000000000000000002\n#EXTINF:1.0,\nseg1.ts\n#EXT-X-ENDLIST\n";
        let server = HlsServer::start(vec![
            bytes("/r/index.m3u8", PLAYLIST, text.as_bytes().to_vec()),
            bytes("/r/k1.bin", "application/octet-stream", KEY.to_vec()),
            bytes("/r/k2.bin", "application/octet-stream", key2.to_vec()),
            bytes("/r/seg0.ts", "video/mp2t", aes128_cbc_encrypt(&fixture_bytes("hls/ts/seg0.ts"), &KEY, &iv(1))),
            bytes("/r/seg1.ts", "video/mp2t", aes128_cbc_encrypt(&fixture_bytes("hls/ts/seg1.ts"), &key2, &iv(2))),
        ]);

        let mut demux = open_url(&server.url("/r/index.m3u8")).expect("open").demuxer;
        let (d, err) = drain(demux.as_mut());

        assert_eq!(err, None);
        let mut bad = Vec::new();
        check_av_part(&mut bad, &d, 2.0);
        finish(bad);
        assert_eq!((server.count("/r/k1.bin"), server.count("/r/k2.bin")), (1, 1));
    }

    #[test]
    fn a_live_reload_that_fails_or_is_not_a_media_playlist_keeps_the_old_one() {
        let master = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nother.m3u8\n";
        let mut routes = vec![maybe(
            "/w/index.m3u8",
            PLAYLIST,
            Box::new(move |n| match n {
                0 => Some(media_playlist(0, 0..=2, false)),
                1 => None,
                2 => Some(master.as_bytes().to_vec()),
                3 => Some(b"<!doctype html><title>busy</title>".to_vec()),
                _ => Some(media_playlist(0, 0..=4, true)),
            }),
        )];
        routes.extend(fixture_dir("ts", "w").into_iter().filter(|r| !r.path.ends_with(".m3u8")));
        let server = HlsServer::start(routes);

        let mut demux = open_url(&server.url("/w/index.m3u8")).expect("open").demuxer;
        let (d, err) = drain(demux.as_mut());

        assert_eq!((d.video.len(), err), (125, None));
        let fetched: Vec<String> = server.paths().into_iter().filter(|p| p.ends_with(".ts")).collect();
        assert_eq!(fetched, segment_paths("w", "ts", 0..5));
        assert!(server.count("/w/index.m3u8") >= 5, "{:?}", server.paths());
    }
}

/// Slow by design: these wait out playr's own give-up times (30 s and 45 s).
mod give_up_times {
    use super::*;

    /// Plays everything that comes and returns (pictures, the ending, seconds from `since`).
    fn play_out(demux: &mut dyn Demuxer, since: Instant) -> (usize, Result<(), String>, f64) {
        let mut pictures = 0;
        loop {
            match demux.next_packet() {
                Ok(Some(Packet::Video { .. })) => pictures += 1,
                Ok(Some(_)) => {}
                Ok(None) => return (pictures, Ok(()), since.elapsed().as_secs_f64()),
                Err(e) => return (pictures, Err(e), since.elapsed().as_secs_f64()),
            }
        }
    }

    #[test]
    fn a_live_playlist_that_never_grows_ends_with_stopped_updating_after_about_30_s() {
        // Target duration 1 s: the fetcher's floor of 30 s applies (6 x 1 s is less).
        let mut routes = vec![dynamic("/n/index.m3u8", PLAYLIST, Box::new(|_| media_playlist(0, 0..=2, false)))];
        routes.extend(fixture_dir("ts", "n").into_iter().filter(|r| !r.path.ends_with(".m3u8")));
        let server = HlsServer::start(routes);
        let started = Instant::now();

        let mut demux = open_url(&server.url("/n/index.m3u8")).expect("open").demuxer;
        let (pictures, end, secs) = play_out(demux.as_mut(), started);

        assert_eq!((pictures, end), (75, Err("the live stream stopped updating".to_string())));
        assert!((29.0..40.0).contains(&secs), "gave up after {secs:.1} s");
        assert!(server.count("/n/index.m3u8") >= 20, "kept reloading meanwhile: {:?}", server.count("/n/index.m3u8"));
    }

    #[test]
    fn a_recording_whose_next_segment_never_answers_ends_with_stalled_after_about_45_s() {
        // Segment 1 is asked for but no response (not even headers) ever comes; the server
        // lets the held connections go when it is dropped at the end of the test.
        let mut routes = vec![bytes("/q/index.m3u8", PLAYLIST, media_playlist(0, 0..=2, true))];
        routes.push(bytes("/q/seg0.ts", "video/mp2t", fixture_bytes("hls/ts/seg0.ts")));
        routes.push(held("/q/seg1.ts", "video/mp2t", fixture_bytes("hls/ts/seg1.ts")));
        routes.push(bytes("/q/seg2.ts", "video/mp2t", fixture_bytes("hls/ts/seg2.ts")));
        let server = HlsServer::start(routes);

        let mut demux = open_url(&server.url("/q/index.m3u8")).expect("open").demuxer;
        let started = Instant::now();
        let (pictures, end, secs) = play_out(demux.as_mut(), started);

        assert_eq!((pictures, end), (25, Err("the HLS stream stalled".to_string())));
        assert!((44.0..60.0).contains(&secs), "gave up after {secs:.1} s");
        assert_eq!(server.count("/q/seg2.ts"), 0);
    }
}

mod seeking_across_a_discontinuity {
    use super::*;

    const R: u64 = 900_000;

    /// Four 1 s segments of 25 pictures; from segment 2 on (after EXT-X-DISCONTINUITY) the
    /// stream's own clock is 10 s ahead.
    fn recording() -> PathBuf {
        let segments: Vec<(bool, Vec<u8>)> = (0..4u64)
            .map(|n| {
                let first = R + n * 90_000 + if n >= 2 { 900_000 } else { 0 };
                (n == 2, segment_of(&pictures(first, 25, n as u8)))
            })
            .collect();
        write_recording("discontinuity_seek", "1.0", &segments)
    }

    fn first_picture(demux: &mut dyn Demuxer) -> Option<f64> {
        while let Ok(Some(p)) = demux.next_packet() {
            if let Packet::Video { pts, .. } = p {
                return Some(pts);
            }
        }
        None
    }

    #[test]
    fn played_straight_through_each_segment_starts_where_the_playlist_puts_it() {
        let (d, err) = play_file(recording());

        let starts: Vec<f64> = d.video.iter().step_by(25).map(|(p, _)| (p * 1e6).round() / 1e6).collect();
        assert_eq!((starts, err), (vec![0.0, 1.0, 2.0, 3.0], None));
    }

    #[test]
    fn a_seek_into_or_past_the_discontinuity_lands_the_first_picture_where_seek_said() {
        let opened = hls::open(&Location::File(recording()), None, &[], false, no_abort()).expect("open");
        let mut demux = opened.demuxer;
        first_picture(demux.as_mut());
        // Each seek starts from wherever the previous one left the stream.
        let cases = [("into the segment after it", 3.4, 3.0), ("into the first segment after it", 2.5, 2.0), ("back before it", 1.5, 1.0), ("forward again", 2.0, 2.0)];
        let mut bad = Vec::new();
        for (case, target, expected) in cases {
            let landed = demux.seek(target);
            let picture = first_picture(demux.as_mut());
            expect!(bad, landed == Ok(expected), "{case}: seek({target}) = {landed:?}");
            expect!(bad, picture.is_some_and(|p| near(p, expected, 0.05)), "{case}: first picture at {picture:?}, seek said {landed:?}");
        }
        finish(bad);
    }

    #[test]
    fn after_a_seek_picture_and_sound_from_separate_playlists_start_together_at_the_landing_point() {
        let master = "#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"main\",DEFAULT=YES,URI=\"a/index.m3u8\"\n\
                      #EXT-X-STREAM-INF:BANDWIDTH=600000,RESOLUTION=160x120,CODECS=\"avc1.64000c,mp4a.40.2\",AUDIO=\"a\"\nv/index.m3u8\n";
        let mut routes = vec![bytes("/k/master.m3u8", PLAYLIST, master.as_bytes().to_vec())];
        routes.extend(fixture_dir("video_only", "k/v"));
        routes.extend(fixture_dir("audio_only", "k/a"));
        let server = HlsServer::start(routes);
        let mut demux = open_url(&server.url("/k/master.m3u8")).expect("open").demuxer;
        first_picture(demux.as_mut());

        let landed = demux.seek(2.5);
        let (mut video, mut audio) = (None, None);
        while video.is_none() || audio.is_none() {
            match demux.next_packet() {
                Ok(Some(Packet::Video { pts, .. })) => video = video.or(Some(pts)),
                Ok(Some(p)) => audio = audio.or(Some(p.pts())),
                _ => break,
            }
        }

        assert_eq!(landed, Ok(2.0));
        assert!(video.is_some_and(|v| near(v, 2.0, 0.05)), "first picture at {video:?}");
        assert!(audio.is_some_and(|a| near(a, 2.0, 0.1)), "first sound at {audio:?}");
    }
}
