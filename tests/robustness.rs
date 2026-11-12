//! Broken input: empty files, noise, truncated and corrupted media. Nothing here may panic in
//! the demuxer or hang; a file that cannot be played has to say why.

mod common;

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::{Duration, Instant};

use common::*;
use playr::demux::open_path;

const EXTENSIONS: [&str; 11] = ["mp4", "m4a", "mkv", "avi", "mp3", "aac", "flac", "ogg", "opus", "wav", "bin"];

const FIXTURES: [&str; 19] = [
    "h264_aac.mp4",
    "h264_bframes_aac.mp4",
    "h264_vorbis.mkv",
    "hevc_vorbis.mkv",
    "hevc_aac.mp4",
    "hevc_big_hvcc_aac.mp4",
    "xvid_mp3.avi",
    "xvid_pcm16.avi",
    "xvid_pcm8.avi",
    "mjpeg_pcm16.avi",
    "mjpeg_ac3.avi",
    "vp9_vorbis.mkv",
    "sine.flac",
    "sine.mp3",
    "sine.aac",
    "sine.m4a",
    "sine.ogg",
    "sine.opus",
    "sine.wav",
];

/// Deterministic noise (a 64-bit LCG), so a failure can be reproduced.
fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (state >> 56) as u8
        })
        .collect()
}

#[derive(Debug, Default)]
struct Played {
    open_error: Option<String>,
    read_error: Option<String>,
    video_packets: usize,
    pictures: usize,
    audio_frames: usize,
    /// Panics inside a video or audio decoder. The pipeline contains these the same way
    /// (`catch_unwind` around every decode call), so they are counted rather than fatal.
    decoder_panics: usize,
}

/// Runs a file through the same stages as the player's pipeline: open, read every packet,
/// decode. A panic outside the decoders fails the calling test.
fn play_through(name: &str, bytes: &[u8]) -> Played {
    let started = Instant::now();
    let mut played = Played::default();
    let path = tmp_file(name, bytes);
    match open_path(&path) {
        Err(e) => played.open_error = Some(e),
        Ok(mut demux) => {
            let (d, err) = drain(demux.as_mut());
            played.read_error = err;
            played.video_packets = d.video.len();
            if let Some(Ok(mut dec)) = d.info.video.as_ref().map(playr::vdec::make_decoder) {
                for (pts, data) in &d.video {
                    match catch_unwind(AssertUnwindSafe(|| dec.decode(data, *pts))) {
                        Ok(frames) => played.pictures += frames.len(),
                        Err(_) => {
                            played.decoder_panics += 1;
                            break;
                        }
                    }
                }
                match catch_unwind(AssertUnwindSafe(|| dec.flush())) {
                    Ok(frames) => played.pictures += frames.len(),
                    Err(_) => played.decoder_panics += 1,
                }
            }
            if d.info.audio.is_some() {
                match catch_unwind(AssertUnwindSafe(|| decode_audio(&d))) {
                    Ok(audio) => played.audio_frames = audio.frames(),
                    Err(_) => played.decoder_panics += 1,
                }
            }
        }
    }
    let took = started.elapsed();
    assert!(took < Duration::from_secs(30), "{name}: took {took:?}");
    for error in [&played.open_error, &played.read_error].into_iter().flatten() {
        assert!(error.trim().len() >= 10, "{name}: unreadable error message {error:?}");
    }
    played
}

#[test]
fn an_empty_file_is_rejected_whatever_its_extension() {
    for ext in EXTENSIONS {
        let played = play_through(&format!("empty.{ext}"), &[]);
        assert!(played.open_error.is_some(), "empty.{ext} was opened: {played:?}");
    }
}

#[test]
fn noise_with_a_media_extension_is_rejected_or_ends_cleanly() {
    let mut panics = Vec::new();
    for (n, ext) in EXTENSIONS.into_iter().enumerate() {
        for len in [1usize, 11, 12, 13, 4096, 65536] {
            let name = format!("noise_{len}.{ext}");
            let played = play_through(&name, &noise(len, n as u64 * 100 + len as u64));
            // Noise holds no pictures, whatever the reader makes of it.
            assert_eq!(played.pictures, 0, "{name}: {played:?}");
            if played.decoder_panics > 0 {
                panics.push(name);
            }
        }
    }
    assert_eq!(panics, Vec::<String>::new(), "a decoder panicked on these files");
}

#[test]
fn noise_behind_a_valid_signature_is_rejected_or_ends_cleanly() {
    let signatures: [(&str, &[u8]); 9] = [
        ("avi", b"RIFF\x00\x10\x00\x00AVI LIST"),
        ("wav", b"RIFF\x00\x10\x00\x00WAVEfmt "),
        ("flac", b"fLaC\x00\x00\x00\x22"),
        ("ogg", b"OggS\x00\x02\x00\x00\x00\x00\x00\x00\x00\x00"),
        ("mp4", b"\x00\x00\x00\x18ftypisom\x00\x00\x02\x00isomiso2\x00\x00\x10\x00moov"),
        ("mkv", b"\x1A\x45\xDF\xA3\x9F\x42\x86\x81\x01\x42\x82\x88matroska\x18\x53\x80\x67"),
        ("mp3", b"ID3\x04\x00\x00\x00\x00\x00\x0A0123456789\xFF\xFB\x90\x00"),
        ("mp3", b"\xFF\xFB\x90\x00"),
        ("aac", b"\xFF\xF1\x50\x80\x2E\x7F\xFC"),
    ];
    let mut panics = Vec::new();
    for (n, (ext, signature)) in signatures.into_iter().enumerate() {
        for len in [0usize, 100, 20_000] {
            let name = format!("signed_noise_{n}_{len}.{ext}");
            let played = play_through(&name, &[signature, &noise(len, 7000 + n as u64 + len as u64)[..]].concat());
            assert_eq!(played.pictures, 0, "{name}: {played:?}");
            if played.decoder_panics > 0 {
                panics.push(name);
            }
        }
    }
    assert_eq!(panics, Vec::<String>::new(), "a decoder panicked on these files");
}

#[test]
fn truncated_files_are_rejected_or_play_what_is_there() {
    let mut panics = Vec::new();
    for file in FIXTURES {
        let whole = fixture_bytes(file);
        let intact = play_through(&format!("whole_{file}"), &whole);
        let cuts = [1, 12, 100, whole.len() / 10, whole.len() / 4, whole.len() / 2, whole.len() * 3 / 4, whole.len() * 9 / 10, whole.len() - 1];
        for cut in cuts {
            let name = format!("cut_{cut}_{file}");
            let played = play_through(&name, &whole[..cut]);
            // A shorter file cannot hold more than the whole one (plus one codec frame of
            // decoder flush).
            assert!(played.pictures <= intact.pictures, "{name}: {} pictures, the whole file has {}", played.pictures, intact.pictures);
            assert!(
                played.audio_frames <= intact.audio_frames + 2048,
                "{name}: {} audio frames, the whole file has {}",
                played.audio_frames,
                intact.audio_frames
            );
            if played.decoder_panics > 0 {
                panics.push(name);
            }
        }
    }
    assert_eq!(panics, Vec::<String>::new(), "a decoder panicked on these files");
}

#[test]
fn corrupted_stretches_inside_a_file_never_panic_the_demuxer() {
    let mut panics = Vec::new();
    for (n, file) in FIXTURES.into_iter().enumerate() {
        let whole = fixture_bytes(file);
        // Damage near the start (headers), in the middle and near the end (indexes).
        for (m, at) in [64, whole.len() / 3, whole.len() / 2, whole.len() - 600].into_iter().enumerate() {
            for fill in ["noise", "zeros", "ones"] {
                let mut bytes = whole.clone();
                let stretch = &mut bytes[at..at + 512];
                match fill {
                    "noise" => stretch.copy_from_slice(&noise(512, (n * 10 + m) as u64)),
                    "zeros" => stretch.fill(0),
                    _ => stretch.fill(0xFF),
                }
                let name = format!("damaged_{at}_{fill}_{file}");
                let played = play_through(&name, &bytes);
                if played.decoder_panics > 0 {
                    panics.push(name);
                }
            }
        }
    }
    assert_eq!(panics, Vec::<String>::new(), "a decoder panicked on these files");
}

#[test]
fn an_mp3_with_a_damaged_stretch_plays_on_after_it() {
    let whole = fixture_bytes("sine.mp3");
    let mut bytes = whole.clone();
    // About two frames (417 bytes each) in the middle of the file.
    let at = whole.len() / 2;
    bytes[at..at + 800].copy_from_slice(&noise(800, 42));

    let played = play_through("damaged_middle_sine.mp3", &bytes);

    assert_eq!(played.open_error, None);
    assert_eq!(played.decoder_panics, 0);
    // 3 s at 44.1 kHz; losing the damaged frames and their neighbours costs well under 10%.
    assert!(
        played.audio_frames >= 132_300 * 9 / 10 && played.audio_frames <= 132_300 + 2 * 1152,
        "{} sample frames played, expected at least 90% of 132300",
        played.audio_frames
    );
}

#[test]
fn a_missing_file_is_reported_with_its_path() {
    let path = tmp_file("exists.mp3", b"x").with_file_name("does-not-exist.mp3");

    let err = open_path(&path).err().expect("a missing file cannot be opened");

    // The text after the path is the operating system's.
    let prefix = format!("cannot open {}: ", path.display());
    assert!(err.starts_with(&prefix) && err.len() > prefix.len(), "{err:?}");
}
