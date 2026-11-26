//! The whole pipeline through `playr::player::Player`, without a window.
//!
//! A `Player` opens the default audio device, so these tests run one at a time (they share
//! the device and depend on real time) and at volume zero. Pictures are checked against
//! FFmpeg's reference checksums (`*.framecrc`): the picture shown for a timestamp must be
//! the picture FFmpeg decodes for that timestamp.

mod common;

use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use common::http::{RadioServer, STATION, TITLES};
use common::*;
use playr::media::VideoFrame;
use playr::player::{Player, State, Status};

static AUDIO_DEVICE: Mutex<()> = Mutex::new(());

struct Session {
    player: Player,
    _device: MutexGuard<'static, ()>,
}

fn start() -> Session {
    let device = AUDIO_DEVICE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let player = Player::new(Box::new(|| {}));
    player.shared.audio.set_volume(0.0);
    Session { player, _device: device }
}

/// Polls until `probe` yields a value; fails with the player's status when it does not.
fn wait_for<T>(player: &Player, what: &str, timeout: Duration, mut probe: impl FnMut() -> Option<T>) -> T {
    let started = Instant::now();
    loop {
        if let Some(value) = probe() {
            return value;
        }
        if started.elapsed() > timeout {
            panic!(
                "timed out after {timeout:?} waiting for {what}; position {:.3}, status {:?}",
                player.shared.position(),
                player.shared.status()
            );
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

impl Session {
    /// Opens `target` and waits until loading is over, whichever way it went. The status
    /// names its source, which tells this open apart from whatever was open before.
    fn open(&self, target: &str) -> Status {
        self.player.open(target);
        wait_for(&self.player, &format!("{target} to finish loading"), Duration::from_secs(20), || {
            let status = self.player.shared.status();
            (status.source == target && !matches!(status.state, State::Idle | State::Loading)).then_some(status)
        })
    }
}

fn wait_for_state(player: &Player, state: State, timeout: Duration) -> Status {
    wait_for(player, &format!("state {state:?}"), timeout, || {
        let status = player.shared.status();
        (status.state == state).then_some(status)
    })
}

fn wait_for_position(player: &Player, expected: f64) {
    wait_for(player, &format!("position {expected}"), Duration::from_secs(15), || {
        ((player.shared.position() - expected).abs() < 1e-6).then_some(())
    });
}

fn next_frame(player: &Player) -> VideoFrame {
    wait_for(player, "a picture", Duration::from_secs(15), || player.shared.take_due_frame().0)
}

fn path_of(name: &str) -> String {
    fixture(name).to_str().expect("utf-8 path").to_string()
}

/// `None` when the picture is the one FFmpeg decodes for the frame's timestamp at 25 fps.
fn picture_fault(frame: &VideoFrame, reference: &[(f64, u32)]) -> Option<String> {
    let n = (frame.pts * 25.0).round() as usize;
    let Some((want_pts, want_crc)) = reference.get(n).copied() else {
        return Some(format!("pts {:.4} is past the last picture", frame.pts));
    };
    if (frame.pts - want_pts).abs() > 0.001 {
        return Some(format!("pts {:.4} is not a picture time of the file (nearest {want_pts:.4})", frame.pts));
    }
    let crc = adler32_ffmpeg(&packed_yuv(frame));
    (crc != want_crc).then(|| match reference.iter().find(|(_, c)| *c == crc) {
        Some((actual, _)) => format!("the picture shown at {:.4} is FFmpeg's picture for {actual:.4}", frame.pts),
        None => format!("the picture shown at {:.4} matches none of FFmpeg's pictures", frame.pts),
    })
}

#[test]
fn mp4_plays_from_start_to_end_with_pictures_on_the_clock() {
    let s = start();
    let reference = read_framecrc("h264_aac.mp4");
    let path = path_of("h264_aac.mp4");

    let status = s.open(&path);
    assert_eq!(status.state, State::Playing, "{status:?}");
    assert_eq!(status.error, None);
    assert_eq!(status.source, path);
    assert_eq!(status.title, "h264_aac.mp4");
    assert_eq!(status.info, "isomp4 · H.264 320×240");
    assert_eq!(status.video, Some((320, 240)));
    assert!(status.has_audio && status.seekable && !status.live, "{status:?}");
    assert_eq!(status.now_playing, None);
    assert_eq!(status.notes, Vec::<String>::new());
    let duration = status.duration.expect("duration");
    assert!((duration - 2.0).abs() <= 0.1, "duration {duration}");

    let mut frames: Vec<VideoFrame> = Vec::new();
    let mut early = Vec::new();
    let mut clock = Vec::new();
    let started = Instant::now();
    let ended = wait_for(&s.player, "the end of the file", Duration::from_secs(20), || {
        if let (Some(frame), _) = s.player.shared.take_due_frame() {
            // The clock only moves forward here, so reading it after the call bounds it.
            let position = s.player.shared.position();
            // The very first picture is shown at once, before the clock starts.
            if !frames.is_empty() && frame.pts > position + 0.005 {
                early.push(format!("pts {:.3} delivered at position {position:.3}", frame.pts));
            }
            frames.push(frame);
        }
        clock.push((started.elapsed().as_secs_f64(), s.player.shared.position()));
        let status = s.player.shared.status();
        (status.state != State::Playing).then_some(status)
    });

    assert_eq!(ended.state, State::Ended, "{ended:?}");
    assert_eq!(ended.error, None);
    assert_eq!(early, Vec::<String>::new(), "pictures delivered ahead of the clock");
    let pts: Vec<f64> = frames.iter().map(|f| f.pts).collect();
    assert!(pts.windows(2).all(|w| w[0] < w[1]), "pictures out of presentation order: {pts:?}");
    assert!(pts[0].abs() <= 0.001, "first picture at {}", pts[0]);
    // Late pictures may be skipped, but polling every 2 ms should see nearly all 50.
    assert!(pts.len() >= 40 && pts.len() <= 50, "{} pictures delivered: {pts:?}", pts.len());
    assert!(*pts.last().unwrap() >= 1.8, "last picture at {}", pts.last().unwrap());
    let faults: Vec<String> = frames.iter().filter_map(|f| picture_fault(f, &reference)).collect();
    assert_eq!(faults, Vec::<String>::new(), "pictures and timestamps do not belong together");

    // The clock runs in real time: compare it with the wall clock over the middle of the file.
    let from = clock.iter().find(|(_, pos)| *pos >= 0.3).expect("clock reached 0.3 s");
    let to = clock.iter().rev().find(|(_, pos)| *pos <= 1.6).expect("clock samples below 1.6 s");
    let rate = (to.1 - from.1) / (to.0 - from.0);
    assert!((0.9..=1.1).contains(&rate), "the clock ran at {rate:.3}x real time between {from:?} and {to:?}");
    let end = s.player.shared.position();
    assert!((1.9..=2.4).contains(&end), "position at the end is {end}, the file is 2 s long");
}

// h264_1080p_aac.mp4 is 1920x1080 with B-frames, 3 s (75 pictures): decoding takes real time
// here, unlike in the small files.

#[test]
fn full_hd_pictures_are_shown_at_their_own_timestamps_after_a_seek() {
    let s = start();
    let reference = read_framecrc("h264_1080p_aac.mp4");
    assert_eq!(s.open(&path_of("h264_1080p_aac.mp4")).state, State::Playing);

    // Paused, the first picture after a jump is the one for the resume point. The only
    // keyframe is at 0 s, so each jump decodes forward from there.
    s.player.toggle_pause();
    let mut bad = Vec::new();
    for (target, first_picture) in [(1.5, 1.52), (0.6, 0.6), (1.0, 1.0), (0.0, 0.0)] {
        s.player.seek(target);
        wait_for_position(&s.player, target);
        let frame = next_frame(&s.player);
        expect!(bad, (frame.pts - first_picture).abs() <= 0.001, "seek({target}): first picture at {:.4}, expected {first_picture}", frame.pts);
        if let Some(fault) = picture_fault(&frame, &reference) {
            bad.push(format!("seek({target}): {fault}"));
        }
    }
    finish(bad);
}

#[test]
fn full_hd_pictures_are_shown_at_their_own_timestamps_during_playback() {
    let s = start();
    let reference = read_framecrc("h264_1080p_aac.mp4");
    let status = s.open(&path_of("h264_1080p_aac.mp4"));
    assert_eq!(status.state, State::Playing, "{status:?}");
    assert_eq!(status.video, Some((1920, 1080)));

    let mut frames: Vec<VideoFrame> = Vec::new();
    let ended = wait_for(&s.player, "the end of the file", Duration::from_secs(40), || {
        frames.extend(s.player.shared.take_due_frame().0);
        let status = s.player.shared.status();
        (status.state != State::Playing).then_some(status)
    });

    assert_eq!(ended.state, State::Ended, "{ended:?}");
    assert_eq!(ended.notes, Vec::<String>::new());
    let pts: Vec<f64> = frames.iter().map(|f| f.pts).collect();
    assert!(pts.windows(2).all(|w| w[0] < w[1]), "pictures out of presentation order: {pts:?}");
    // Pictures that are overdue may be skipped, but most of the 75 have to make it.
    assert!(pts.len() >= 38 && pts.len() <= 75, "{} pictures delivered: {pts:?}", pts.len());
    let faults: Vec<String> = frames.iter().filter_map(|f| picture_fault(f, &reference)).collect();
    assert_eq!(faults, Vec::<String>::new(), "pictures and timestamps do not belong together");
}

#[test]
fn seek_resumes_at_the_target_or_at_the_keyframe_when_that_is_more_than_3_s_before_it() {
    // h264_longgop_aac.mp4: 10 s at 25 fps, keyframes at 0 s and 5 s only.
    let s = start();
    let reference = read_framecrc("h264_longgop_aac.mp4");
    assert_eq!(s.open(&path_of("h264_longgop_aac.mp4")).state, State::Playing);
    // Paused, the position after a seek is exactly where playback will resume.
    s.player.toggle_pause();
    assert!(s.player.shared.is_paused());

    // (target, where playback resumes, first picture shown)
    let cases = [
        (7.0, 7.0, 7.0),
        (6.93, 6.93, 6.96),
        (4.0, 0.0, 0.0),
        (8.0, 8.0, 8.0),
        (8.04, 5.0, 5.0),
        (3.0, 3.0, 3.0),
        (3.04, 0.0, 0.0),
        // The end is clamped to a quarter second before the duration, 4.75 s after a keyframe.
        (100.0, 5.0, 5.0),
        (-5.0, 0.0, 0.0),
        (5.0, 5.0, 5.0),
        // A small step back from 5.0 has to land earlier, not on the same keyframe again.
        (4.9, 0.0, 0.0),
        (2.0, 2.0, 2.0),
        (1.9, 1.9, 1.92),
    ];
    let mut bad = Vec::new();
    for (target, resume, first_picture) in cases {
        s.player.seek(target);
        wait_for_position(&s.player, resume);
        let frame = next_frame(&s.player);
        let what = format!("seek({target})");
        expect!(bad, (frame.pts - first_picture).abs() <= 0.001, "{what}: first picture at {:.4}, expected {first_picture}", frame.pts);
        if let Some(fault) = picture_fault(&frame, &reference) {
            bad.push(format!("{what}: {fault}"));
        }
        let position = s.player.shared.position();
        expect!(bad, (position - resume).abs() < 1e-6, "{what}: position moved to {position} while paused");
        expect!(bad, s.player.shared.is_paused(), "{what}: a seek while paused must stay paused");
        expect!(bad, s.player.shared.status().state == State::Playing, "{what}: state {:?}", s.player.shared.status().state);
    }
    finish(bad);
}

#[test]
fn matroska_seek_resumes_at_the_target() {
    // h264_vorbis.mkv has a single keyframe at 0 s, so every target is within 3 s of it.
    let s = start();
    let reference = read_framecrc("h264_vorbis.mkv");
    let status = s.open(&path_of("h264_vorbis.mkv"));
    assert_eq!(status.state, State::Playing, "{status:?}");
    assert_eq!(status.info, "matroska · H.264 320×240");
    s.player.toggle_pause();

    let mut bad = Vec::new();
    for (target, first_picture) in [(1.7, 1.72), (0.5, 0.52), (1.0, 1.0)] {
        s.player.seek(target);
        wait_for_position(&s.player, target);
        let frame = next_frame(&s.player);
        expect!(bad, (frame.pts - first_picture).abs() <= 0.001, "seek({target}): first picture at {:.4}, expected {first_picture}", frame.pts);
        if let Some(fault) = picture_fault(&frame, &reference) {
            bad.push(format!("seek({target}): {fault}"));
        }
    }
    finish(bad);
}

#[test]
fn playback_after_a_seek_runs_to_the_end() {
    let s = start();
    let reference = read_framecrc("h264_longgop_aac.mp4");
    assert_eq!(s.open(&path_of("h264_longgop_aac.mp4")).state, State::Playing);
    // 2.5 s after the keyframe at 5 s, so playback resumes at the target itself.
    s.player.toggle_pause();
    s.player.seek(7.5);
    wait_for_position(&s.player, 7.5);
    s.player.toggle_pause();

    let mut frames = Vec::new();
    let ended = wait_for(&s.player, "the end of the file", Duration::from_secs(20), || {
        frames.extend(s.player.shared.take_due_frame().0);
        let status = s.player.shared.status();
        (status.state != State::Playing).then_some(status)
    });

    assert_eq!(ended.state, State::Ended, "{ended:?}");
    let pts: Vec<f64> = frames.iter().map(|f| f.pts).collect();
    assert!(pts.windows(2).all(|w| w[0] < w[1]), "pictures out of presentation order: {pts:?}");
    // Pictures are 40 ms apart, so the first one at or after 7.5 s is the one at 7.52 s.
    assert!(pts.first().is_some_and(|p| (p - 7.52).abs() <= 0.001), "first picture after the seek: {pts:?}");
    assert!(pts.last().is_some_and(|p| *p >= 9.8), "last picture: {pts:?}");
    let faults: Vec<String> = frames.iter().filter_map(|f| picture_fault(f, &reference)).collect();
    assert_eq!(faults, Vec::<String>::new(), "pictures and timestamps do not belong together");
    let end = s.player.shared.position();
    assert!((9.9..=10.4).contains(&end), "position at the end is {end}, the file is 10 s long");
    assert!(s.player.shared.is_paused(), "the clock is held at the end");
}

/// Plays each file to its end, presses play again and expects playback from the beginning.
fn check_replay_after_the_end(files: &[&str]) {
    let mut bad = Vec::new();
    for file in files {
        let s = start();
        assert_eq!(s.open(&path_of(file)).state, State::Playing, "{file}");
        // Pictures have to be taken off the queue, as a window would: the end is reached
        // when the last one has been shown.
        wait_for(&s.player, "the end of the file", Duration::from_secs(20), || {
            s.player.shared.take_due_frame();
            (s.player.shared.status().state == State::Ended).then_some(())
        });

        s.player.toggle_pause();

        let started = Instant::now();
        let mut replaying = false;
        while !replaying && started.elapsed() < Duration::from_secs(6) {
            s.player.shared.take_due_frame();
            let position = s.player.shared.position();
            replaying = s.player.shared.status().state == State::Playing && position > 0.1 && position < 1.5;
            std::thread::sleep(Duration::from_millis(5));
        }
        let status = s.player.shared.status();
        expect!(
            bad,
            replaying,
            "{file}: play at the end did not restart: state {:?}, position {:.3}, error {:?}, notes {:?}",
            status.state,
            s.player.shared.position(),
            status.error,
            status.notes
        );
        expect!(bad, !replaying || !s.player.shared.is_paused(), "{file}: restarted but left paused");
    }
    finish(bad);
}

#[test]
fn play_at_the_end_restarts_avi_and_audio_files_from_the_beginning() {
    check_replay_after_the_end(&["xvid_pcm16.avi", "sine.wav", "sine.mp3"]);
}

#[test]
fn play_at_the_end_restarts_mp4_and_matroska_files_from_the_beginning() {
    check_replay_after_the_end(&["h264_aac.mp4", "sine.m4a", "h264_vorbis.mkv"]);
}

#[test]
fn pause_holds_the_position_and_resume_continues_from_it() {
    let s = start();
    assert_eq!(s.open(&path_of("h264_longgop_aac.mp4")).state, State::Playing);
    wait_for(&s.player, "playback to get going", Duration::from_secs(15), || (s.player.shared.position() > 0.3).then_some(()));

    s.player.toggle_pause();
    assert!(s.player.shared.is_paused());
    // A device callback that was already running may still finish.
    std::thread::sleep(Duration::from_millis(150));
    let held = s.player.shared.position();
    std::thread::sleep(Duration::from_millis(500));
    let still = s.player.shared.position();

    assert_eq!(still, held, "the position moved while paused");
    assert!((0.3..2.0).contains(&held), "paused at {held}");
    assert_eq!(s.player.shared.status().state, State::Playing, "pausing is not a state change");

    s.player.toggle_pause();

    assert!(!s.player.shared.is_paused());
    wait_for(&s.player, "the position to advance again", Duration::from_secs(15), || {
        (s.player.shared.position() > held + 0.3).then_some(())
    });
    let resumed = s.player.shared.position();
    assert!(resumed < held + 2.0, "resumed at {resumed} after pausing at {held}: the paused time was not left out");
}

fn play_a_fresh_copy_of_sine_at(s: &Session, speed: f64) -> (f64, f64, Status) {
    s.player.set_speed(speed);
    let copy = tmp_file(&format!("player_sine_at_{speed}.flac"), &fixture_bytes("sine.flac"));
    let status = s.open(copy.to_str().expect("utf-8 path"));
    assert_eq!(status.state, State::Playing, "speed {speed}: {status:?}");
    let (started, from) = wait_for(&s.player, "the clock to start", Duration::from_secs(10), || {
        let p = s.player.shared.position();
        (p > 0.05).then(|| (Instant::now(), p))
    });
    let ended = wait_for_state(&s.player, State::Ended, Duration::from_secs(15));
    let wall = started.elapsed().as_secs_f64();
    (wall, s.player.shared.position() - from, ended)
}

#[test]
fn opening_a_live_stream_returns_to_normal_speed_while_a_file_keeps_the_chosen_one() {
    let server = RadioServer::start(fixture_bytes("radio.mp3"));
    let s = start();
    s.player.set_speed(1.5);

    let file = s.open(&path_of("sine.flac"));
    let speed_on_file = (s.player.shared.speed(), s.player.shared.audio.speed());
    let live = s.open(&server.url("/stream"));

    assert_eq!((file.state, file.seekable), (State::Playing, true), "{file:?}");
    assert_eq!(speed_on_file, (1.5, 1.5));
    assert_eq!((live.state, live.seekable), (State::Playing, false), "{live:?}");
    assert_eq!((s.player.shared.speed(), s.player.shared.audio.speed()), (1.0, 1.0));
}

#[test]
fn at_double_speed_an_audio_file_runs_twice_as_fast_as_at_normal_speed_and_ends_at_3_s() {
    let s = start();

    let (wall_1x, moved_1x, ended_1x) = play_a_fresh_copy_of_sine_at(&s, 1.0);
    let (wall_2x, moved_2x, ended_2x) = play_a_fresh_copy_of_sine_at(&s, 2.0);
    let end_2x = s.player.shared.position();

    assert_eq!((ended_1x.error, ended_2x.error), (None, None));
    assert!((2.9..=3.3).contains(&end_2x), "position at the end is {end_2x}, the file is 3 s long");
    let (rate_1x, rate_2x) = (moved_1x / wall_1x, moved_2x / wall_2x);
    let ratio = rate_2x / rate_1x;
    assert!(
        (1.7..=2.3).contains(&ratio),
        "2x moved {moved_2x:.3} s in {wall_2x:.3} s, 1x moved {moved_1x:.3} s in {wall_1x:.3} s: ratio {ratio:.2}"
    );
}

#[test]
fn audio_file_reports_duration_seeks_and_ends() {
    let s = start();
    let status = s.open(&path_of("sine.flac"));
    assert_eq!(status.state, State::Playing, "{status:?}");
    assert_eq!((status.video, status.has_audio, status.seekable, status.live), (None, true, true, false));
    assert_eq!(status.info, "flac");
    assert_eq!(status.title, "sine.flac");
    let duration = status.duration.expect("duration");
    assert!((duration - 3.0).abs() <= 0.05, "duration {duration}");

    s.player.toggle_pause();
    s.player.seek(2.0);
    wait_for_position(&s.player, 2.0);
    s.player.toggle_pause();

    let ended = wait_for_state(&s.player, State::Ended, Duration::from_secs(15));
    assert_eq!(ended.error, None);
    let end = s.player.shared.position();
    assert!((2.9..=3.3).contains(&end), "position at the end is {end}, the file is 3 s long");
    assert_eq!(s.player.shared.take_due_frame().0.map(|f| f.pts), None, "an audio file has no pictures");
}

#[test]
fn unsupported_video_is_noted_and_the_audio_plays() {
    for (file, note, info) in [
        ("mjpeg_pcm16.avi", "no decoder for MJPG video", "avi"),
        ("vp9_vorbis.mkv", "no decoder for VP9 video", "matroska"),
    ] {
        let s = start();

        let status = s.open(&path_of(file));
        assert_eq!(status.state, State::Playing, "{file}: {status:?}");
        assert_eq!(status.notes, [note], "{file}");
        assert_eq!((status.video, status.has_audio), (None, true), "{file}");
        assert_eq!(status.info, info, "{file}");
        assert_eq!(status.error, None, "{file}");
        wait_for(&s.player, "the audio to play", Duration::from_secs(15), || (s.player.shared.position() > 0.3).then_some(()));
        let ended = wait_for_state(&s.player, State::Ended, Duration::from_secs(15));
        assert_eq!(ended.notes, [note], "{file}: notes at the end");
    }
}

#[test]
fn files_that_cannot_be_played_fail_with_a_readable_error() {
    let missing = fixture("sine.wav").with_file_name("no-such-file.mp4");
    let missing = missing.to_str().unwrap().to_string();
    let garbage = tmp_file("player_garbage.mp4", &(0..50_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8).collect::<Vec<u8>>());
    let empty = tmp_file("player_empty.mkv", &[]);
    let no_address = tmp_file("player_empty.m3u", b"#EXTM3U\n# nothing\n");
    let hls = tmp_file("player_hls.m3u8", b"#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6.0,\nhttp://127.0.0.1:9/seg0.ts\n#EXT-X-ENDLIST\n");

    // (target, how the error has to start, whether that is the whole message)
    let cases = [
        (missing.clone(), format!("cannot open {missing}: "), false),
        (garbage.to_str().unwrap().to_string(), "unrecognised or unsupported format (".to_string(), false),
        (empty.to_str().unwrap().to_string(), "unrecognised or unsupported format (".to_string(), false),
        (path_of("mjpeg_ac3.avi"), "audio format 0x2000 in AVI is not supported; no decoder for MJPG video".to_string(), true),
        (no_address.to_str().unwrap().to_string(), "playlist contains no stream address".to_string(), true),
        // Nothing listens on port 9: the playlist is read as HLS and its segment cannot be fetched.
        (hls.to_str().unwrap().to_string(), "cannot download http://127.0.0.1:9/seg0.ts: ".to_string(), false),
    ];
    let s = start();
    for (target, message, exact) in cases {
        let status = s.open(&target);
        assert_eq!(status.state, State::Failed, "{target}: {status:?}");
        let error = status.error.clone().unwrap_or_else(|| panic!("{target}: failed without a message"));
        if exact {
            assert_eq!(error, message, "{target}");
        } else {
            // The remainder is the operating system's or the format library's wording.
            assert!(error.starts_with(&message) && error.len() > message.len(), "{target}: {error:?}");
        }
        assert_eq!((status.video, status.has_audio, status.duration), (None, false, None), "{target}");
        assert_eq!(s.player.shared.take_due_frame().0.map(|f| f.pts), None, "{target}");
    }

    // The player is still usable after a failure.
    assert_eq!(s.open(&path_of("sine.wav")).state, State::Playing);
}

#[test]
fn stop_returns_to_idle() {
    let s = start();
    assert_eq!(s.open(&path_of("h264_aac.mp4")).state, State::Playing);
    wait_for(&s.player, "playback to get going", Duration::from_secs(15), || (s.player.shared.position() > 0.2).then_some(()));

    s.player.stop();

    let status = wait_for_state(&s.player, State::Idle, Duration::from_secs(15));
    assert_eq!((status.title.as_str(), status.video, status.has_audio, status.duration), ("", None, false, None));
    assert_eq!(status.error, None);
    assert_eq!(s.player.shared.take_due_frame().0.map(|f| f.pts), None, "no picture is left over after stop");
    let position = s.player.shared.position();
    std::thread::sleep(Duration::from_millis(300));
    assert!((0.0..0.01).contains(&position), "the clock restarts at stop, got {position}");
    assert_eq!(s.player.shared.position(), position, "the clock is held while nothing is open");
}

// ---------------------------------------------------------------------------------------------
// Scheduling: what the pipeline does when sound and picture do not come in step.

/// Takes pictures off the queue, as a window would, until playback is over one way or the
/// other. Returns the final status and the pictures in the order they were shown.
fn play_to_the_finish(player: &Player, timeout: Duration) -> (Status, Vec<VideoFrame>) {
    let mut frames = Vec::new();
    let status = wait_for(player, "playback to finish", timeout, || {
        frames.extend(player.shared.take_due_frame().0);
        let status = player.shared.status();
        matches!(status.state, State::Ended | State::Failed).then_some(status)
    });
    (status, frames)
}

fn assert_ascending(frames: &[VideoFrame], what: &str) {
    let pts: Vec<f64> = frames.iter().map(|f| f.pts).collect();
    assert!(pts.windows(2).all(|w| w[0] < w[1]), "{what}: pictures out of presentation order: {pts:.2?}");
}

/// A copy of an AVI fixture in which every chunk of stream 1 (the audio) is overwritten with
/// zeros, located through the file's own `idx1` index.
fn avi_with_silenced_audio_chunks(fixture_name: &str, tmp_name: &str) -> String {
    let mut bytes = fixture_bytes(fixture_name);
    let u32_at = |bytes: &[u8], at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
    let movi = bytes.windows(4).position(|w| w == b"movi").expect("movi list");
    let idx1 = bytes.windows(4).rposition(|w| w == b"idx1").expect("idx1 chunk");
    let entries = u32_at(&bytes, idx1 + 4) / 16;
    let mut wiped = 0;
    for n in 0..entries {
        let entry = idx1 + 8 + n * 16;
        if &bytes[entry..entry + 4] != b"01wb" {
            continue;
        }
        let (offset, size) = (u32_at(&bytes, entry + 8), u32_at(&bytes, entry + 12));
        // Offsets count from the 'movi' fourcc; the chunk's 8-byte header precedes its data.
        let chunk = movi + offset;
        assert_eq!(&bytes[chunk..chunk + 4], b"01wb", "fixture precondition: idx1 offsets are relative to movi");
        bytes[chunk + 8..chunk + 8 + size].fill(0);
        wiped += 1;
    }
    assert!(wiped > 10, "fixture precondition: {fixture_name} has audio chunks (found {wiped})");
    tmp_file(tmp_name, &bytes).to_str().unwrap().to_string()
}

#[test]
fn pictures_keep_coming_and_the_file_ends_when_its_audio_never_decodes() {
    // An audio track is announced, but its data holds no MPEG frame at all.
    let path = avi_with_silenced_audio_chunks("xvid_mp3.avi", "player_xvid_no_sound.avi");
    let reference: Vec<f64> = read_framecrc("xvid_mp3.avi").iter().map(|(pts, _)| *pts).collect();
    let s = start();
    let status = s.open(&path);
    assert_eq!(status.state, State::Playing, "{status:?}");
    assert_eq!(status.video, Some((320, 240)));

    let started = Instant::now();
    let (ended, frames) = play_to_the_finish(&s.player, Duration::from_secs(30));
    let took = started.elapsed().as_secs_f64();

    assert_eq!(ended.state, State::Ended, "{ended:?}");
    assert_ascending(&frames, "no sound");
    assert!(frames.len() >= 40, "{} of 50 pictures shown", frames.len());
    let off_grid: Vec<f64> = frames.iter().map(|f| f.pts).filter(|p| !reference.iter().any(|r| (r - p).abs() <= 0.001)).collect();
    assert_eq!(off_grid, Vec::<f64>::new(), "pictures at times the file does not have");
    // 2 s of picture, shown against a clock that runs in real time without the sound.
    assert!((1.5..=6.0).contains(&took), "the 2 s file took {took:.2} s to play");
    let end = s.player.shared.position();
    assert!((1.9..=2.6).contains(&end), "position at the end is {end}");
}

#[test]
fn picture_carries_on_at_its_own_timestamps_after_the_audio_runs_out() {
    // h264_16s_audio_2s.mp4: 16 s of picture (400 packets, a keyframe every second), 2 s of
    // sound. Past 2 s there is no audio to drive the clock, and looking for more of it makes
    // the reader run hundreds of packets ahead of the picture.
    let s = start();
    let reference = read_framecrc("h264_16s_audio_2s.mp4");
    let status = s.open(&path_of("h264_16s_audio_2s.mp4"));
    assert_eq!(status.state, State::Playing, "{status:?}");
    let duration = status.duration.expect("duration");
    assert!((duration - 16.0).abs() <= 0.1, "duration {duration}: the longer track counts");

    // Across the end of the audio and well beyond it.
    let mut frames: Vec<VideoFrame> = Vec::new();
    let mut early = Vec::new();
    let mut clock = Vec::new();
    let started = Instant::now();
    wait_for(&s.player, "the clock to reach 4.5 s", Duration::from_secs(30), || {
        if let (Some(frame), _) = s.player.shared.take_due_frame() {
            let position = s.player.shared.position();
            if !frames.is_empty() && frame.pts > position + 0.005 {
                early.push(format!("pts {:.3} at position {position:.3}", frame.pts));
            }
            frames.push(frame);
        }
        let position = s.player.shared.position();
        clock.push((started.elapsed().as_secs_f64(), position));
        (position >= 4.5).then_some(())
    });
    assert_eq!(s.player.shared.status().state, State::Playing);
    assert_eq!(early, Vec::<String>::new(), "pictures delivered ahead of the clock");
    assert_ascending(&frames, "across the end of the audio");
    assert!(frames.len() >= 90, "{} of about 112 pictures shown in the first 4.5 s", frames.len());
    let faults: Vec<String> = frames.iter().filter_map(|f| picture_fault(f, &reference)).collect();
    assert_eq!(faults, Vec::<String>::new(), "pictures and timestamps do not belong together");
    // Real time before the audio ends (0.5..1.5 s) and after it (2.5..4.5 s).
    for (from, to) in [(0.5, 1.5), (2.5, 4.4)] {
        let a = clock.iter().find(|(_, pos)| *pos >= from).expect("clock sample");
        let b = clock.iter().rev().find(|(_, pos)| *pos <= to).expect("clock sample");
        let rate = (b.1 - a.1) / (b.0 - a.0);
        assert!((0.9..=1.1).contains(&rate), "the clock ran at {rate:.3}x real time between {from} s and {to} s");
    }

    // Pause holds this clock too.
    s.player.toggle_pause();
    std::thread::sleep(Duration::from_millis(150));
    let held = s.player.shared.position();
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(s.player.shared.position(), held, "the position moved while paused");
    s.player.toggle_pause();

    // A jump while playing, into the part that has no sound, then on to the end.
    s.player.seek(14.0);
    wait_for(&s.player, "the seek to 14 s", Duration::from_secs(15), || {
        s.player.shared.take_due_frame();
        (s.player.shared.position() >= 14.0).then_some(())
    });
    let (ended, tail) = play_to_the_finish(&s.player, Duration::from_secs(30));
    assert_eq!(ended.state, State::Ended, "{ended:?}");
    assert_eq!(ended.notes, Vec::<String>::new());
    assert_ascending(&tail, "after the seek");
    assert!(tail.first().is_some_and(|f| f.pts >= 13.999), "first picture after the seek at {:?}", tail.first().map(|f| f.pts));
    assert!(tail.last().is_some_and(|f| f.pts >= 15.8), "last picture at {:?}", tail.last().map(|f| f.pts));
    let faults: Vec<String> = tail.iter().filter_map(|f| picture_fault(f, &reference)).collect();
    assert_eq!(faults, Vec::<String>::new(), "pictures and timestamps do not belong together after the seek");
    let end = s.player.shared.position();
    assert!((15.9..=16.5).contains(&end), "position at the end is {end}, the file is 16 s long");
}

#[test]
fn audio_after_a_seek_starts_at_the_resume_point_not_at_the_packet_boundary() {
    // Paused after a seek, nothing is played, so what is queued is exactly what was decoded:
    // whole packets, minus the part of the first one that lies before the resume point.
    let demuxed = demux_file("sine.flac");
    let (packet_start, next_start) = (demuxed.audio[20].0, demuxed.audio[21].0);
    let packet = next_start - packet_start;
    assert!(packet > 0.02, "fixture precondition: FLAC packets are long enough to tell ({packet} s)");
    let target = packet_start + packet * 0.5;

    let s = start();
    assert_eq!(s.open(&path_of("sine.flac")).state, State::Playing);
    s.player.toggle_pause();
    s.player.seek(target);
    wait_for_position(&s.player, target);
    // The pipeline tops the queue up to its low-water mark and then waits.
    let mut last = (-1.0, Instant::now());
    let queued = wait_for(&s.player, "the audio queue to settle", Duration::from_secs(15), || {
        let now = s.player.shared.audio.buffered_secs();
        if now != last.0 {
            last = (now, Instant::now());
        }
        (now > 0.0 && last.1.elapsed() > Duration::from_millis(300)).then_some(now)
    });

    // queued = n packets - half a packet, for some whole n.
    let packets = (queued + packet * 0.5) / packet;
    assert!(
        (packets - packets.round()).abs() <= 0.03 && packets.round() >= 1.0,
        "{queued:.4} s queued after seeking into the middle of a {packet:.4} s packet: {packets:.3} packets from the resume point, expected a whole number"
    );
    assert_eq!(s.player.shared.position(), target, "nothing is played while paused");
}

// ---------------------------------------------------------------------------------------------
// Broken files through the shipped pipeline.

/// Opens `target` and plays it out. Whatever the damage, playback has to come to rest in
/// `Ended` or `Failed`; a failure has to say why.
fn play_damaged(s: &Session, target: &str) -> (Status, Vec<VideoFrame>) {
    let opened = s.open(target);
    let (status, frames) = match opened.state {
        State::Failed => (opened, Vec::new()),
        _ => play_to_the_finish(&s.player, Duration::from_secs(40)),
    };
    if status.state == State::Failed {
        assert!(status.error.as_deref().is_some_and(|e| e.trim().len() >= 10), "{target}: failed without a readable error: {status:?}");
    }
    assert_ascending(&frames, target);
    (status, frames)
}

/// The error the demuxer itself reports when the file is read from start to end, if any.
fn demuxer_error(path: &str) -> Option<String> {
    let mut demux = playr::demux::open_path(std::path::Path::new(path)).ok()?;
    drain(demux.as_mut()).1
}

#[test]
fn a_truncated_avi_plays_what_is_there_and_says_why_it_stopped() {
    let whole = fixture_bytes("xvid_pcm16.avi");
    let path = tmp_file("player_cut.avi", &whole[..whole.len() * 6 / 10]);
    let path = path.to_str().unwrap();
    let s = start();

    let (status, frames) = play_damaged(&s, path);

    assert_eq!(status.state, State::Ended, "{status:?}");
    assert_eq!(status.notes, ["stopped early: AVI chunk runs past the end of the file"]);
    // 60% of 50 pictures; the cut falls inside one chunk.
    assert!((20..=35).contains(&frames.len()), "{} pictures shown from 60% of the file", frames.len());
    let end = s.player.shared.position();
    assert!((0.9..=1.6).contains(&end), "position at the end is {end}; about 1.2 s of the file is there");
}

#[test]
fn a_truncated_mp4_and_a_damaged_matroska_file_come_to_rest_with_the_demuxer_error_noted() {
    // Two files, one test: both go through the same assertions below, with the demuxer's own
    // verdict on each file as the expectation.
    let mp4 = fixture_bytes("h264_faststart_aac.mp4");
    let cut_mp4 = tmp_file("player_cut_faststart.mp4", &mp4[..mp4.len() * 55 / 100]);
    let mut mkv = fixture_bytes("h264_vorbis.mkv");
    let at = mkv.len() / 2;
    for (n, byte) in mkv[at..at + 3000].iter_mut().enumerate() {
        *byte = (n as u32).wrapping_mul(2_654_435_761).to_be_bytes()[0];
    }
    let damaged_mkv = tmp_file("player_damaged.mkv", &mkv);
    let s = start();

    // The MP4 reader refuses a media-data box that is longer than the file, so the cut MP4
    // yields an error before its first packet; the Matroska file plays its intact first part.
    for (path, min_pictures, played_something) in [(cut_mp4, 0, false), (damaged_mkv, 10, true)] {
        let path = path.to_str().unwrap();
        let (status, frames) = play_damaged(&s, path);

        assert!(frames.len() >= min_pictures, "{path}: only {} pictures shown; {status:?}", frames.len());
        assert!(frames.len() <= 50 && frames.iter().all(|f| f.pts < 2.1), "{path}: more pictures than the part that is there");
        // Either way the reader's message is the one reason given for stopping.
        assert_eq!(status.notes.len(), 1, "{path}: playback stopped short of the end and has to say why, once: {status:?}");
        let error = demuxer_error(path).unwrap_or_else(|| panic!("{path}: test precondition: the demuxer reports an error for this file"));
        assert_eq!(status.notes, [format!("stopped early: {error}")], "{path}: {status:?}");
        if played_something {
            // The intact first part was shown before the reader gave up: the file was played
            // as far as it goes, which is an early end with a note and not a failure.
            assert_eq!(status.state, State::Ended, "{path}: {status:?}");
            assert_eq!(status.error, None, "{path}: {status:?}");
        } else {
            // Nothing was shown or heard before the reader failed: that is a failure to play
            // the file, with the reader's message as the error and as the reason for stopping.
            assert_eq!(frames.len(), 0, "{path}");
            assert_eq!(status.state, State::Failed, "{path}: {status:?}");
            assert_eq!(status.error.as_deref(), Some(error.as_str()), "{path}");
        }
        let position = s.player.shared.position();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(s.player.shared.position(), position, "{path}: the clock runs on after {:?}", status.state);
    }
}

// ---------------------------------------------------------------------------------------------
// The audio device going away, and sound that is not there from the start.

/// Wall-clock time against playback position, sampled while a test drives the player.
struct ClockTrace {
    started: Instant,
    samples: Vec<(f64, f64)>,
}

impl ClockTrace {
    fn new() -> Self {
        Self { started: Instant::now(), samples: Vec::new() }
    }

    fn sample(&mut self, player: &Player) -> f64 {
        let position = player.shared.position();
        self.samples.push((self.started.elapsed().as_secs_f64(), position));
        position
    }

    /// The position never steps back by more than `back` seconds and never stands still for
    /// more than `stall` seconds, between the first sample at `from` and the last below `to`.
    fn assert_continuous(&self, from: f64, to: f64, back: f64, stall: f64, what: &str) {
        let span: Vec<&(f64, f64)> = self.samples.iter().skip_while(|(_, p)| *p < from).take_while(|(_, p)| *p < to).collect();
        assert!(span.len() > 10, "{what}: only {} clock samples between {from} s and {to} s", span.len());
        let mut high = *span[0];
        for &&(wall, position) in &span {
            assert!(position >= high.1 - back, "{what}: the position went back from {:.3} to {position:.3}", high.1);
            if position > high.1 {
                high = (wall, position);
            }
            assert!(wall - high.0 <= stall, "{what}: the position stood at {:.3} for {:.2} s", high.1, wall - high.0);
        }
    }

    /// Position seconds per wall-clock second between two positions.
    fn rate(&self, from: f64, to: f64) -> f64 {
        let a = self.samples.iter().find(|(_, p)| *p >= from).expect("clock sample at the start of the span");
        let b = self.samples.iter().rev().find(|(_, p)| *p <= to).expect("clock sample at the end of the span");
        (b.1 - a.1) / (b.0 - a.0)
    }
}

#[test]
fn playback_carries_on_when_the_audio_device_is_lost_once() {
    let s = start();
    let reference = read_framecrc("h264_faststart_aac.mp4");
    assert_eq!(s.open(&path_of("h264_faststart_aac.mp4")).state, State::Playing);

    let mut clock = ClockTrace::new();
    let mut frames = Vec::new();
    let mut lost = false;
    let ended = wait_for(&s.player, "the end of the file", Duration::from_secs(30), || {
        frames.extend(s.player.shared.take_due_frame().0);
        if clock.sample(&s.player) > 1.0 && !lost {
            lost = true;
            // What the device's error callback does when the output goes away.
            s.player.shared.audio.flag_device_error();
        }
        let status = s.player.shared.status();
        (status.state != State::Playing).then_some(status)
    });

    assert_eq!(ended.state, State::Ended, "{ended:?}");
    assert!(ended.has_audio, "one lost device is reopened");
    assert_eq!(ended.notes, Vec::<String>::new());
    // The audio that was queued when the device went (up to 0.6 s) is decoded again, so a
    // short hold is fine; a jump back or a hang is not.
    clock.assert_continuous(0.2, 3.9, 0.05, 1.0, "across the device loss");
    assert_ascending(&frames, "across the device loss");
    assert!(frames.len() >= 75, "{} of 100 pictures shown", frames.len());
    let faults: Vec<String> = frames.iter().filter_map(|f| picture_fault(f, &reference)).collect();
    assert_eq!(faults, Vec::<String>::new());
    let end = s.player.shared.position();
    assert!((3.9..=4.6).contains(&end), "position at the end is {end}; the file is 4 s long");
}

/// Loses the device again every 200 ms until the player's status satisfies `until`.
fn keep_losing_the_device(player: &Player, clock: &mut ClockTrace, until: impl Fn(&Status) -> bool) -> (Status, usize) {
    let mut losses = 0;
    let mut last_loss = Instant::now() - Duration::from_secs(1);
    let status = wait_for(player, "the player to give the device up", Duration::from_secs(20), || {
        player.shared.take_due_frame();
        clock.sample(player);
        let status = player.shared.status();
        if until(&status) {
            return Some(status);
        }
        if last_loss.elapsed() >= Duration::from_millis(200) {
            last_loss = Instant::now();
            losses += 1;
            player.shared.audio.flag_device_error();
        }
        None
    });
    (status, losses)
}

#[test]
fn a_device_that_keeps_failing_is_given_up_and_the_picture_plays_on() {
    let s = start();
    let reference = read_framecrc("h264_longgop_aac.mp4");
    assert_eq!(s.open(&path_of("h264_longgop_aac.mp4")).state, State::Playing);
    wait_for(&s.player, "playback to get going", Duration::from_secs(15), || {
        s.player.shared.take_due_frame();
        (s.player.shared.position() > 0.3).then_some(())
    });

    let mut clock = ClockTrace::new();
    let (status, losses) = keep_losing_the_device(&s.player, &mut clock, |st| !st.has_audio || st.state != State::Playing);

    // Five reopens are tried; the device is not opened a seventh time.
    assert!((6..=8).contains(&losses), "audio was given up after {losses} losses");
    assert_eq!(status.state, State::Playing, "{status:?}");
    assert_eq!(status.notes, ["audio device keeps failing"]);
    assert_eq!((status.video, status.has_audio, status.error.clone()), (Some((160, 120)), false, None));

    // From here the picture runs against the wall clock.
    let given_up_at = clock.sample(&s.player);
    let mut frames = Vec::new();
    wait_for(&s.player, "another 1.5 s of picture", Duration::from_secs(15), || {
        frames.extend(s.player.shared.take_due_frame().0);
        (clock.sample(&s.player) >= given_up_at + 1.5).then_some(())
    });
    let rate = clock.rate(given_up_at + 0.2, given_up_at + 1.4);
    assert!((0.9..=1.1).contains(&rate), "without sound the clock ran at {rate:.3}x real time");
    clock.assert_continuous(0.5, given_up_at + 1.4, 0.05, 1.0, "while the device kept failing");
    assert_ascending(&frames, "without sound");
    assert!(frames.len() >= 25, "{} pictures shown in 1.5 s", frames.len());
    let faults: Vec<String> = frames.iter().filter_map(|f| picture_fault(f, &reference)).collect();
    assert_eq!(faults, Vec::<String>::new());
}

#[test]
fn an_audio_file_fails_when_the_device_keeps_failing() {
    let s = start();
    assert_eq!(s.open(&path_of("radio.mp3")).state, State::Playing);
    wait_for(&s.player, "playback to get going", Duration::from_secs(15), || (s.player.shared.position() > 0.2).then_some(()));

    let mut clock = ClockTrace::new();
    let (status, losses) = keep_losing_the_device(&s.player, &mut clock, |st| st.state != State::Playing);

    assert!((6..=8).contains(&losses), "the file failed after {losses} losses");
    assert_eq!(status.state, State::Failed, "{status:?}");
    assert_eq!(status.error.as_deref(), Some("audio device keeps failing"));
    let position = s.player.shared.position();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(s.player.shared.position(), position, "the clock runs on after the failure");
}

#[test]
fn sound_that_starts_late_takes_over_the_clock_without_a_jump() {
    // h264_16s_audio_from_14s.mp4: 16 s of picture; the audio track starts at 13.98 s, 350
    // video packets into the file. Until then nothing is there to drive the clock.
    let s = start();
    let reference = read_framecrc("h264_16s_audio_from_14s.mp4");
    let status = s.open(&path_of("h264_16s_audio_from_14s.mp4"));
    assert_eq!(status.state, State::Playing, "{status:?}");
    assert_eq!(status.video, Some((160, 120)));

    let mut clock = ClockTrace::new();
    let mut frames: Vec<VideoFrame> = Vec::new();
    let mut early = Vec::new();
    let ended = wait_for(&s.player, "the end of the file", Duration::from_secs(60), || {
        if let (Some(frame), _) = s.player.shared.take_due_frame() {
            let position = s.player.shared.position();
            if !frames.is_empty() && frame.pts > position + 0.005 {
                early.push(format!("pts {:.3} at position {position:.3}", frame.pts));
            }
            frames.push(frame);
        }
        clock.sample(&s.player);
        let status = s.player.shared.status();
        (status.state != State::Playing).then_some(status)
    });

    assert_eq!(ended.state, State::Ended, "{ended:?}");
    assert!(ended.has_audio, "the late audio track is played: {ended:?}");
    assert_eq!(ended.notes, Vec::<String>::new());
    assert_eq!(early, Vec::<String>::new(), "pictures delivered ahead of the clock");
    assert_ascending(&frames, "whole file");
    assert!(frames.len() >= 340, "{} of 400 pictures shown", frames.len());
    let faults: Vec<String> = frames.iter().filter_map(|f| picture_fault(f, &reference)).collect();
    assert_eq!(faults, Vec::<String>::new(), "pictures and timestamps do not belong together");
    // Real time without sound, across the moment the sound starts, and with it.
    for (from, to) in [(1.0, 6.0), (8.0, 13.5), (13.5, 14.5), (14.5, 15.8)] {
        let rate = clock.rate(from, to);
        assert!((0.9..=1.1).contains(&rate), "the clock ran at {rate:.3}x real time between {from} s and {to} s");
    }
    clock.assert_continuous(0.5, 15.8, 0.03, 0.3, "from silence into sound");
    let end = s.player.shared.position();
    assert!((15.9..=16.6).contains(&end), "position at the end is {end}; the file is 16 s long");
}

#[test]
fn a_gap_in_the_audio_timestamps_is_played_as_silence_of_that_length() {
    // sine_gap.mka: 4 s by its timestamps, with the second from 1.5 s to 2.5 s missing, so it
    // holds only 3 s of samples. Played back to back they would end a second early.
    let s = start();
    let status = s.open(&path_of("sine_gap.mka"));
    assert_eq!(status.state, State::Playing, "{status:?}");
    let duration = status.duration.expect("duration");
    assert!((duration - 4.0).abs() <= 0.05, "duration {duration}");

    let mut clock = ClockTrace::new();
    let ended = wait_for(&s.player, "the end of the file", Duration::from_secs(30), || {
        clock.sample(&s.player);
        let status = s.player.shared.status();
        (status.state != State::Playing).then_some(status)
    });
    let took = clock.started.elapsed().as_secs_f64();

    assert_eq!(ended.state, State::Ended, "{ended:?}");
    assert_eq!(ended.notes, Vec::<String>::new());
    let end = s.player.shared.position();
    assert!((3.9..=4.4).contains(&end), "position at the end is {end}; the file is 4 s long");
    assert!((3.5..=5.0).contains(&took), "playing the 4 s file took {took:.2} s");
    clock.assert_continuous(0.3, 3.8, 0.03, 0.3, "across the gap");
    let rate = clock.rate(0.5, 3.7);
    assert!((0.9..=1.1).contains(&rate), "the clock ran at {rate:.3}x real time across the gap");
}

#[test]
fn of_several_files_opened_in_a_row_the_last_one_plays() {
    let s = start();
    let last = path_of("h264_aac.mp4");

    s.player.open(path_of("sine.wav"));
    s.player.open(path_of("sine.flac"));
    s.player.open(path_of("xvid_pcm16.avi"));
    let status = s.open(&last);

    assert_eq!(status.state, State::Playing, "{status:?}");
    assert_eq!((status.title.as_str(), status.info.as_str()), ("h264_aac.mp4", "isomp4 · H.264 320×240"));
    // When the first sound comes out depends on the device (over a second on some), so the
    // clock is given time to get going; meanwhile no earlier open may take over.
    let later = wait_for(&s.player, "the last file to play past 0.3 s", Duration::from_secs(10), || {
        let status = s.player.shared.status();
        (status.source != last || s.player.shared.position() > 0.3).then_some(status)
    });
    assert_eq!((later.source, later.state), (last, State::Playing), "an earlier open was carried out after the last one");
}

#[test]
fn a_seek_sent_right_after_the_last_open_applies_to_that_file() {
    let s = start();
    let reference = read_framecrc("h264_longgop_aac.mp4");
    let last = path_of("h264_longgop_aac.mp4");

    s.player.open(path_of("sine.wav"));
    // Meant for sine.wav, which is never played: must not carry over to the next file.
    s.player.seek(2.0);
    s.player.open(last.clone());
    s.player.seek(7.0);

    let mut frames = Vec::new();
    wait_for(&s.player, "the last file to play from 7 s", Duration::from_secs(20), || {
        let status = s.player.shared.status();
        let position = s.player.shared.position();
        if status.source == last && status.state == State::Playing && position >= 7.0 {
            frames.extend(s.player.shared.take_due_frame().0);
        }
        (frames.len() >= 5).then_some(())
    });

    let position = s.player.shared.position();
    assert!((7.0..9.0).contains(&position), "playing at {position}, expected shortly after 7 s");
    assert_ascending(&frames, "after open and seek");
    assert!((frames[0].pts - 7.0).abs() <= 0.2 && frames[0].pts >= 6.999, "first picture at {}", frames[0].pts);
    let faults: Vec<String> = frames.iter().filter_map(|f| picture_fault(f, &reference)).collect();
    assert_eq!(faults, Vec::<String>::new());
}

#[test]
fn a_seek_the_file_cannot_satisfy_ends_playback_with_a_note_and_a_later_seek_resumes_it() {
    // 60% of sine.flac: the header still promises 3 s, the data stops at about 1.8 s.
    let whole = fixture_bytes("sine.flac");
    let path = tmp_file("player_cut.flac", &whole[..whole.len() * 6 / 10]);
    let path = path.to_str().unwrap();
    {
        // What the demuxer itself says about this file.
        let mut demux = playr::demux::open_path(std::path::Path::new(path)).expect("open the cut file");
        assert_eq!(demux.info().duration, Some(3.0));
        assert_eq!(demux.seek(2.9), Err("seek position is past the end".to_string()));
    }
    let s = start();
    let status = s.open(path);
    assert_eq!(status.state, State::Playing, "{status:?}");
    assert!(status.seekable);
    wait_for(&s.player, "playback to get going", Duration::from_secs(15), || (s.player.shared.position() > 0.2).then_some(()));

    s.player.seek(2.9);

    let ended = wait_for_state(&s.player, State::Ended, Duration::from_secs(15));
    assert_eq!(ended.notes, ["seek position is past the end"]);
    assert_eq!(ended.error, None);
    let position = s.player.shared.position();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(s.player.shared.position(), position, "the clock runs on after the failed seek");

    s.player.seek(0.5);

    let mut first_seen = None;
    wait_for(&s.player, "playback to resume from 0.5 s", Duration::from_secs(15), || {
        let playing = s.player.shared.status().state == State::Playing;
        let now = s.player.shared.position();
        // The clock stood at `position`; the first different reading is where it restarted.
        if now != position {
            first_seen.get_or_insert(now);
        }
        (playing && now >= 0.8).then_some(())
    });
    let first_seen = first_seen.unwrap();
    assert!((0.5..0.8).contains(&first_seen), "playback resumed at {first_seen}, expected at 0.5 s");
    assert!(!s.player.shared.is_paused());
    assert!(s.player.shared.position() < 1.5, "position {} shortly after resuming at 0.5 s", s.player.shared.position());
}

#[test]
fn video_that_never_decodes_is_noted_while_the_audio_plays() {
    use common::avi::{Avi, Index, bitmap_info, strh, wave_format};
    // 100 MPEG-4 "pictures" that are a VOP start code followed by noise, next to 4 s of
    // 8 kHz 8-bit silence-free PCM.
    let mut avi = Avi {
        streams: vec![
            (strh(b"vids", b"XVID", 1, 25, 0), bitmap_info(64, 48, b"XVID")),
            (strh(b"auds", &[0; 4], 1, 8000, 1), wave_format(1, 1, 8000, 8000, 1, 8)),
        ],
        chunks: Vec::new(),
    };
    for n in 0..100u32 {
        let mut vop = vec![0, 0, 1, 0xB6, 0x10];
        vop.extend((0..200u32).map(|i| ((n * 200 + i).wrapping_mul(2_654_435_761) >> 24) as u8));
        avi.chunks.push((*b"00dc", vop, true));
        avi.chunks.push((*b"01wb", (0..320u32).map(|i| 128 + ((i % 16) as u8) * 4).collect(), true));
    }
    let path = tmp_file("player_undecodable_video.avi", &avi.build(Index::Relative));
    let s = start();

    let (status, frames) = play_damaged(&s, path.to_str().unwrap());

    assert_eq!(status.state, State::Ended, "{status:?}");
    assert_eq!(frames.len(), 0, "noise decoded into pictures");
    assert_eq!(status.notes.len(), 1, "{:?}", status.notes);
    // The reason in brackets is the decoder's wording.
    assert!(status.notes[0].starts_with("video cannot be decoded (") && status.notes[0].ends_with(')'), "{:?}", status.notes);
    let end = s.player.shared.position();
    assert!((3.8..=4.4).contains(&end), "position at the end is {end}; the audio is 4 s long");
}

// ---------------------------------------------------------------------------------------------
// More ways to seek.

#[test]
fn of_many_seeks_sent_at_once_the_last_one_decides() {
    let s = start();
    let reference = read_framecrc("h264_longgop_aac.mp4");
    assert_eq!(s.open(&path_of("h264_longgop_aac.mp4")).state, State::Playing);
    s.player.toggle_pause();

    // What dragging the seek bar produces.
    for target in [1.0, 2.0, 6.0, 3.0, 9.0, 0.5, 7.0] {
        s.player.seek(target);
    }

    wait_for_position(&s.player, 7.0);
    let frame = next_frame(&s.player);
    assert!((frame.pts - 7.0).abs() <= 0.001, "first picture at {}", frame.pts);
    assert_eq!(picture_fault(&frame, &reference), None);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(s.player.shared.position(), 7.0, "an earlier seek was carried out after the last one");
}

#[test]
fn seeking_while_playing_continues_from_the_target() {
    let s = start();
    let reference = read_framecrc("h264_longgop_aac.mp4");
    assert_eq!(s.open(&path_of("h264_longgop_aac.mp4")).state, State::Playing);
    wait_for(&s.player, "playback to get going", Duration::from_secs(15), || {
        s.player.shared.take_due_frame();
        (s.player.shared.position() > 0.3).then_some(())
    });

    s.player.seek(7.0);

    let sought = Instant::now();
    let mut frames = Vec::new();
    wait_for(&s.player, "playback past 7.5 s", Duration::from_secs(15), || {
        let position = s.player.shared.position();
        if position >= 7.0 {
            frames.extend(s.player.shared.take_due_frame().0);
        }
        (position >= 7.5).then_some(())
    });
    let took = sought.elapsed().as_secs_f64();
    assert!(!s.player.shared.is_paused());
    assert!(took >= 0.5 && took <= 3.0, "playing 0.5 s after the seek took {took:.2} s");
    assert_ascending(&frames, "after the seek");
    assert!(frames.first().is_some_and(|f| f.pts >= 6.999 && f.pts <= 7.2), "first picture at {:?}", frames.first().map(|f| f.pts));
    let faults: Vec<String> = frames.iter().filter_map(|f| picture_fault(f, &reference)).collect();
    assert_eq!(faults, Vec::<String>::new());
}

#[test]
fn avi_hevc_and_open_gop_files_seek_to_the_target() {
    // (file, check pictures against FFmpeg, [(target, where playback resumes, first picture)])
    let cases: [(&str, bool, &[(f64, f64, f64)]); 3] = [
        // MPEG-4 in AVI, keyframes every 0.48 s; its pictures are not bit-exact with FFmpeg's.
        ("xvid_pcm16.avi", false, &[(1.2, 1.2, 1.2), (0.5, 0.5, 0.52), (1.7, 1.7, 1.72), (0.0, 0.0, 0.0)]),
        // HEVC in MP4, one keyframe at 0 s.
        ("hevc_aac.mp4", true, &[(1.0, 1.0, 1.0), (0.42, 0.42, 0.44), (1.7, 1.7, 1.72)]),
        // H.264 with open GOPs: an IDR picture at 0 s only, recovery points at 1, 2 and 3 s.
        // Resuming at 3.6 s is possible only by starting at the recovery point at 3 s; from
        // the IDR picture it would be more than 3 s of decoding and playback would resume at 0.
        ("h264_opengop_aac.mp4", true, &[(3.6, 3.6, 3.6), (2.5, 2.5, 2.52), (1.0, 1.0, 1.0), (0.3, 0.3, 0.32)]),
    ];
    let mut bad = Vec::new();
    for (file, exact, seeks) in cases {
        let s = start();
        let reference = read_framecrc(file);
        assert_eq!(s.open(&path_of(file)).state, State::Playing, "{file}");
        s.player.toggle_pause();
        for (target, resume, first_picture) in seeks {
            let what = format!("{file} seek({target})");
            s.player.seek(*target);
            wait_for_position(&s.player, *resume);
            let frame = next_frame(&s.player);
            expect!(bad, (frame.pts - first_picture).abs() <= 0.001, "{what}: first picture at {:.4}, expected {first_picture}", frame.pts);
            if exact && let Some(fault) = picture_fault(&frame, &reference) {
                bad.push(format!("{what}: {fault}"));
            }
        }
    }
    finish(bad);
}

// ---------------------------------------------------------------------------------------------
// HTTP sources.

#[test]
fn a_file_downloaded_over_http_plays_to_its_end() {
    // Content-Length and no ICY metadata: a file, not a station.
    let server = RadioServer::start(fixture_bytes("radio.mp3"));
    let s = start();

    let status = s.open(&server.url("/plain.mp3"));
    assert_eq!(status.state, State::Playing, "{status:?}");
    assert!(!status.live, "a finite download is not a live stream");
    assert_eq!(status.now_playing, None);

    let (ended, _) = play_to_the_finish(&s.player, Duration::from_secs(30));
    assert_eq!(ended.state, State::Ended, "{ended:?}");
    assert_eq!(ended.error, None);
    let end = s.player.shared.position();
    assert!((3.8..=4.4).contains(&end), "position at the end is {end}; the file is 4.05 s long");
}

#[test]
fn opening_something_else_abandons_a_stalled_stream_at_once() {
    // The station sends 1000 bytes and goes silent; left alone the player would wait out the
    // 20 s stall timeout before it could do anything else.
    let server = RadioServer::start(fixture_bytes("radio.mp3"));
    let s = start();
    s.player.open(server.url("/stall"));
    wait_for(&s.player, "the stalled station to be contacted", Duration::from_secs(10), || {
        (!server.requests_for("/stall").is_empty()).then_some(())
    });
    std::thread::sleep(Duration::from_millis(300));

    let asked = Instant::now();
    let target = path_of("sine.wav");
    s.player.open(target.clone());
    let status = wait_for(&s.player, "the file opened instead", Duration::from_secs(8), || {
        let status = s.player.shared.status();
        (status.source == target && status.state == State::Playing).then_some(status)
    });

    assert!(asked.elapsed() < Duration::from_secs(5), "switching took {:?}", asked.elapsed());
    assert_eq!((status.live, status.has_audio), (false, true));
}

#[test]
fn radio_stream_plays_live_with_station_name_and_titles_until_the_broadcast_ends() {
    let server = RadioServer::start(fixture_bytes("radio.mp3"));
    let s = start();

    let status = s.open(&server.url("/stream"));
    assert_eq!(status.state, State::Playing, "{status:?}");
    assert_eq!(status.title, STATION);
    assert_eq!((status.live, status.seekable, status.duration), (true, false, None));
    assert_eq!((status.video, status.has_audio), (None, true));
    assert_eq!(status.info, "mp3");

    // The whole 4 s broadcast is buffered at once, so the last announced title is current.
    wait_for(&s.player, "the last title", Duration::from_secs(15), || {
        (s.player.shared.status().now_playing.as_deref() == Some(TITLES[2])).then_some(())
    });
    wait_for(&s.player, "the audio to play", Duration::from_secs(15), || (s.player.shared.position() > 0.5).then_some(()));
    // Seeking a live stream is not possible and must leave playback alone.
    s.player.seek(1.0);

    let ended = wait_for(&s.player, "the end of the broadcast", Duration::from_secs(20), || {
        let status = s.player.shared.status();
        (status.state != State::Playing).then_some(status)
    });
    assert_eq!(ended.state, State::Failed);
    assert_eq!(ended.error.as_deref(), Some("the stream ended"));
    let end = s.player.shared.position();
    assert!((3.8..=4.4).contains(&end), "position at the end is {end}, the broadcast is 4.05 s long");
}

#[test]
fn local_playlist_file_opens_the_stream_it_names() {
    let server = RadioServer::start(fixture_bytes("radio.mp3"));
    let pls = tmp_file(
        "player_station.pls",
        format!("[playlist]\nNumberOfEntries=1\nFile1={}\nTitle1=ignored\nVersion=2\n", server.url("/list.m3u")).as_bytes(),
    );
    let s = start();

    let status = s.open(pls.to_str().unwrap());
    assert_eq!(status.state, State::Playing, "{status:?}");
    assert_eq!((status.title.as_str(), status.live), (STATION, true));
    assert_eq!(server.requests_for("/stream").len(), 1);
}

#[test]
fn playback_carries_on_while_nothing_takes_the_pictures_and_then_shows_the_current_one() {
    // A covered, minimized or dragged window draws nothing, so nobody calls take_due_frame.
    // h264_16s_aac.mp4 has 400 video packets, more than the reader may stash (300) while
    // the picture queue stays full: the stash fills at about 13 s. The file's own sound ends at
    // 16 s, and the reader reaches it (the clock then runs free) about 0.6 s before that.
    let s = start();
    let reference = read_framecrc("h264_16s_aac.mp4");
    let status = s.open(&path_of("h264_16s_aac.mp4"));
    assert_eq!(status.state, State::Playing, "{status:?}");
    assert!(status.has_audio, "{status:?}");

    struct Sample {
        at: f64,
        position: f64,
        buffered: usize,
        free_run: bool,
        state: State,
    }
    let mut samples: Vec<Sample> = Vec::new();
    let started = Instant::now();
    wait_for(&s.player, "the clock to reach 14.5 s without the pictures being taken", Duration::from_secs(30), || {
        let audio = &s.player.shared.audio;
        let sample = Sample {
            at: started.elapsed().as_secs_f64(),
            position: s.player.shared.position(),
            buffered: audio.buffered_frames(),
            free_run: audio.is_free_run(),
            state: s.player.shared.status().state,
        };
        let done = sample.position >= 14.5;
        samples.push(sample);
        std::thread::sleep(Duration::from_millis(20));
        done.then_some(())
    });

    let not_playing: Vec<String> =
        samples.iter().filter(|x| x.state != State::Playing).map(|x| format!("{:?} at {:.2} s", x.state, x.position)).collect();
    assert_eq!(not_playing, Vec::<String>::new(), "playback left the Playing state");
    // Past the first half second the sound must be fed all along, never left to run dry and
    // never replaced by a silent clock.
    let starved: Vec<String> = samples
        .iter()
        .filter(|x| x.position >= 0.5 && (x.buffered == 0 || x.free_run))
        .map(|x| format!("at {:.2} s: {} frames buffered, free run {}", x.position, x.buffered, x.free_run))
        .collect();
    assert_eq!(starved, Vec::<String>::new(), "the sound ran dry while the pictures were not taken");
    for (from, to) in [(1.0, 7.0), (7.0, 14.4)] {
        let a = samples.iter().find(|x| x.position >= from).expect("clock sample");
        let b = samples.iter().rev().find(|x| x.position <= to).expect("clock sample");
        let rate = (b.position - a.position) / (b.at - a.at);
        assert!((0.9..=1.1).contains(&rate), "the clock ran at {rate:.3}x real time between {from} s and {to} s");
    }

    // The UI draws again: the first picture it gets is the one due now, not one left over
    // from when it stopped drawing. Due means pts <= position + 2 ms; the newest due picture
    // is at most one frame interval (40 ms) old, plus the pipeline's 4 ms idle wait.
    let (first, _) = s.player.shared.take_due_frame();
    let position = s.player.shared.position();
    let first = first.expect("a picture is due");
    let age = position - first.pts;
    assert!((-0.002..=0.050).contains(&age), "the picture shown at {position:.3} s is from {:.3} s", first.pts);
    assert_eq!(picture_fault(&first, &reference), None);

    // And the pictures go on from there, one by one.
    let mut frames = vec![first];
    wait_for(&s.player, "the clock to reach 15.3 s", Duration::from_secs(5), || {
        frames.extend(s.player.shared.take_due_frame().0);
        (s.player.shared.position() >= 15.3).then_some(())
    });
    assert_ascending(&frames, "after drawing resumed");
    assert!(frames.len() >= 16, "{} of about 20 pictures shown in the 0.8 s after drawing resumed", frames.len());
    let faults: Vec<String> = frames.iter().filter_map(|f| picture_fault(f, &reference)).collect();
    assert_eq!(faults, Vec::<String>::new(), "pictures and timestamps do not belong together");
    assert_eq!(s.player.shared.status().state, State::Playing);
}

#[test]
fn a_file_ends_while_nothing_takes_the_pictures_and_its_last_picture_is_still_shown() {
    // Both files: 2 s, 50 pictures at 25 fps, the last at 1.96 s. In h264_aac.mp4 the sound
    // (2.02 s) outlasts the pictures; in h264_2s_audio_1s.mp4 it stops at 1 s and the last
    // picture decides the end.
    let mut bad = Vec::new();
    for file in ["h264_aac.mp4", "h264_2s_audio_1s.mp4"] {
        let s = start();
        let reference = read_framecrc(file);
        let started = Instant::now();
        let status = s.open(&path_of(file));
        assert_eq!(status.state, State::Playing, "{file}: {status:?}");

        let (ended, position_at_end) = wait_for(&s.player, &format!("the end of {file} without the pictures being taken"), Duration::from_secs(15), || {
            let status = s.player.shared.status();
            let position = s.player.shared.position();
            (status.state != State::Playing).then_some((status, position))
        });
        let took = started.elapsed().as_secs_f64();

        if (ended.state, &ended.error) != (State::Ended, &None) {
            bad.push(format!("{file}: came to rest as {:?} with error {:?}", ended.state, ended.error));
        }
        // The position only moves forward (after the end the sound already sent still plays
        // out), so read after the Ended state it bounds where the end was decided: only once
        // the last picture was due (1.96 <= position + 2 ms).
        if position_at_end < 1.958 {
            bad.push(format!("{file}: Ended reported at {position_at_end:.3} s, before the last picture at 1.96 s was due"));
        }
        if !(1.9..=6.0).contains(&took) {
            bad.push(format!("{file}: the 2 s file took {took:.2} s to end"));
        }
        let (last, next) = s.player.shared.take_due_frame();
        match last {
            Some(last) if (last.pts - 1.96).abs() <= 0.001 => bad.extend(picture_fault(&last, &reference).map(|f| format!("{file}: {f}"))),
            Some(last) => bad.push(format!("{file}: the picture drawn after the end is from {:.3} s, not the last one at 1.96 s", last.pts)),
            None => bad.push(format!("{file}: no picture left to draw after the end")),
        }
        if next.is_some() {
            bad.push(format!("{file}: a picture is still queued after the last one ({next:?} s away)"));
        }
    }
    finish(bad);
}

/// A list of `files` (absolute paths, so no relative lookups) written to a fresh .m3u.
fn m3u_of(name: &str, files: &[&str]) -> String {
    let text: String = std::iter::once("#EXTM3U\n".to_string()).chain(files.iter().map(|f| format!("{}\n", path_of(f)))).collect();
    tmp_file(name, text.as_bytes()).to_str().expect("utf-8 path").to_string()
}

#[test]
fn in_a_list_a_drawing_window_shows_the_last_picture_of_an_entry_before_the_next_one_starts() {
    // Entry 1 ends on its last picture (its sound stops at 1 s), the moment the next entry
    // would clear the picture queue. The UI here draws at 60 Hz.
    let s = start();
    let list = m3u_of("player_two_entries.m3u", &["h264_2s_audio_1s.mp4", "h264_aac.mp4"]);
    let (first_ref, second_ref) = (read_framecrc("h264_2s_audio_1s.mp4"), read_framecrc("h264_aac.mp4"));
    assert_eq!(s.open(&list).state, State::Playing);

    let mut frames: Vec<VideoFrame> = Vec::new();
    let (mut last_taken, mut next_opened): (Option<Instant>, Option<Instant>) = (None, None);
    wait_for(&s.player, "entry 2 to show a picture past 0.5 s", Duration::from_secs(20), || {
        if let (Some(frame), _) = s.player.shared.take_due_frame() {
            if frame.width == 160 && (frame.pts - 1.96).abs() <= 0.001 {
                last_taken = Some(Instant::now());
            }
            frames.push(frame);
        }
        let status = s.player.shared.status();
        if status.entry == 1 && next_opened.is_none() {
            next_opened = Some(Instant::now());
        }
        std::thread::sleep(Duration::from_millis(16));
        (status.entry == 1 && frames.last().is_some_and(|f| f.pts >= 0.5 && f.width == 320)).then_some(())
    });

    // Once the last picture is taken the next entry opens at the pipeline's next look, at most
    // its 20 ms idle wait later; the entry number is set as soon as the list entry is picked,
    // and this loop sees it on the same pass or after one more 16 ms frame. About 40 ms in all;
    // 150 ms leaves room for a busy machine and is still far below the 250 ms the entry would
    // wait if the taken picture were not noticed.
    let (last_taken, next_opened) = (last_taken.expect("the 1.96 s picture of entry 1 was taken"), next_opened.expect("entry 2 opened"));
    let gap = next_opened.saturating_duration_since(last_taken).as_millis();
    assert!(next_opened >= last_taken, "entry 2 opened before the last picture of entry 1 was taken");
    assert!(gap <= 150, "entry 2 opened {gap} ms after the last picture of entry 1 was taken");

    // Entry 1 is 160x120, entry 2 320x240.
    let switch = frames.iter().position(|f| f.width == 320).expect("a picture of entry 2");
    let (last_of_first, first_of_second) = (&frames[switch - 1], &frames[switch]);
    assert!((last_of_first.pts - 1.96).abs() <= 0.001, "the last picture of entry 1 drawn was from {:.3} s, not 1.96 s", last_of_first.pts);
    assert_eq!(picture_fault(last_of_first, &first_ref), None);
    assert_eq!(picture_fault(first_of_second, &second_ref), None);
    assert!(frames[switch..].iter().all(|f| f.width == 320), "a picture of entry 1 came after entry 2 started");
}

#[test]
fn in_a_list_the_next_entry_opens_even_when_nothing_draws() {
    let s = start();
    let list = m3u_of("player_two_entries_undrawn.m3u", &["h264_2s_audio_1s.mp4", "h264_aac.mp4"]);
    assert_eq!(s.open(&list).state, State::Playing);

    // No take_due_frame at all. Entry 1's last picture is due at 1.958 s on its clock.
    let mut end_of_first: Option<Instant> = None;
    let status = wait_for(&s.player, "entry 2 to play", Duration::from_secs(20), || {
        let status = s.player.shared.status();
        if status.entry == 0 && end_of_first.is_none() && s.player.shared.position() >= 1.958 {
            end_of_first = Some(Instant::now());
        }
        (status.entry == 1 && status.state == State::Playing).then_some(status)
    });
    let waited = end_of_first.expect("entry 1 reached its last picture").elapsed().as_secs_f64();

    // Entry 2 (h264_aac.mp4) is the 320x240 one.
    assert_eq!((status.video, status.error), (Some((320, 240)), None));
    assert!(waited <= 1.0, "entry 2 opened {waited:.2} s after entry 1 reached its end");
}
