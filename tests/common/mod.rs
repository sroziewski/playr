//! Helpers shared by the integration test binaries.
#![allow(dead_code)]

pub mod avi;
pub mod http;
pub mod range_http;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use playr::demux::Demuxer;
use playr::media::{AudioPacket, AudioTrack, MediaInfo, Packet, VideoFrame};
use playr::player::pcm_to_f32;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::codecs::registry::CodecRegistry;

/// A checked-in fixture (see `tests/fixtures/generate.ps1`). A missing fixture fails the test
/// instead of skipping it.
pub fn fixture(name: &str) -> PathBuf {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures").join(name);
    assert!(path.is_file(), "fixture {} is missing; regenerate with tests/fixtures/generate.ps1", path.display());
    path
}

pub fn fixture_bytes(name: &str) -> Vec<u8> {
    std::fs::read(fixture(name)).expect("read fixture")
}

/// Writes a scratch file under Cargo's per-package test temp directory.
pub fn tmp_file(name: &str, bytes: &[u8]) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("playr-tests");
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("write temp file");
    path
}

/// Adler-32 the way FFmpeg's `framecrc` muxer computes it: seeded with 0, not with 1.
pub fn adler32_ffmpeg(data: &[u8]) -> u32 {
    let (mut a, mut b) = (0u32, 0u32);
    for chunk in data.chunks(4096) {
        for &byte in chunk {
            a += byte as u32;
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    (b << 16) | a
}

/// (pts in seconds, Adler-32) per picture from a `<fixture>.framecrc` reference file.
pub fn read_framecrc(fixture_name: &str) -> Vec<(f64, u32)> {
    let text = String::from_utf8(fixture_bytes(&format!("{fixture_name}.framecrc"))).expect("framecrc is text");
    let mut tb = None;
    let mut out = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("#tb 0: ") {
            let (num, den) = rest.trim().split_once('/').expect("time base");
            tb = Some(num.parse::<f64>().unwrap() / den.parse::<f64>().unwrap());
        }
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let cols: Vec<&str> = line.split(',').map(str::trim).collect();
        assert_eq!(cols.len(), 6, "unexpected framecrc line: {line}");
        let pts: f64 = cols[2].parse().expect("pts");
        let crc = u32::from_str_radix(cols[5].trim_start_matches("0x"), 16).expect("crc");
        out.push((pts * tb.expect("#tb line precedes the data"), crc));
    }
    out
}

/// The visible picture as tightly packed planar yuv420p (Y, then U, then V), which is the
/// layout of FFmpeg's `-f rawvideo -pix_fmt yuv420p`.
pub fn packed_yuv(f: &VideoFrame) -> Vec<u8> {
    let mut out = Vec::with_capacity((f.width * f.height + 2 * f.chroma_width * f.chroma_height) as usize);
    let mut plane = |data: &[u8], stride: u32, w: u32, h: u32| {
        for row in 0..h as usize {
            let start = row * stride as usize;
            out.extend_from_slice(&data[start..start + w as usize]);
        }
    };
    plane(&f.y, f.y_stride, f.width, f.height);
    plane(&f.u, f.c_stride, f.chroma_width, f.chroma_height);
    plane(&f.v, f.c_stride, f.chroma_width, f.chroma_height);
    out
}

pub struct Demuxed {
    pub info: MediaInfo,
    pub video: Vec<(f64, Box<[u8]>)>,
    pub audio: Vec<(f64, AudioPacket)>,
    /// (`skip`, `limit`) of each audio packet, parallel to `audio`.
    pub audio_trim: Vec<(u32, Option<u32>)>,
}

/// Reads a demuxer to its end. An `Err` from `next_packet` is returned as the second value.
pub fn drain(demux: &mut dyn Demuxer) -> (Demuxed, Option<String>) {
    let mut out = Demuxed { info: demux.info().clone(), video: Vec::new(), audio: Vec::new(), audio_trim: Vec::new() };
    // Far above any fixture; only bounds a demuxer that would loop forever on broken input.
    for _ in 0..1_000_000 {
        match demux.next_packet() {
            Ok(Some(Packet::Video { pts, data })) => out.video.push((pts, data)),
            Ok(Some(Packet::Audio { pts, packet, skip, limit })) => {
                out.audio.push((pts, packet));
                out.audio_trim.push((skip, limit));
            }
            Ok(None) => return (out, None),
            Err(e) => return (out, Some(e)),
        }
    }
    panic!("demuxer did not reach the end of the media within 1,000,000 packets");
}

pub fn demux_file(name: &str) -> Demuxed {
    let mut demux = playr::demux::open_path(&fixture(name)).unwrap_or_else(|e| panic!("open {name}: {e}"));
    let (out, err) = drain(demux.as_mut());
    assert_eq!(err, None, "{name}: demuxing an intact file must not fail");
    out
}

/// Records a failed expectation without stopping, so one table-driven test reports every bad
/// row instead of only the first.
#[macro_export]
macro_rules! expect {
    ($bad:ident, $cond:expr, $($arg:tt)+) => {
        if !$cond {
            $bad.push(format!($($arg)+));
        }
    };
}

pub fn finish(bad: Vec<String>) {
    assert!(bad.is_empty(), "{} expectation(s) failed:\n{}", bad.len(), bad.join("\n"));
}

/// (number of differing bytes, largest absolute difference) between two pictures.
pub fn diff_stats(a: &[u8], b: &[u8]) -> (usize, u8) {
    assert_eq!(a.len(), b.len(), "pictures differ in size");
    a.iter().zip(b).fold((0, 0), |(n, max), (x, y)| {
        let d = x.abs_diff(*y);
        (n + (d != 0) as usize, max.max(d))
    })
}

pub struct DecodedVideo {
    pub frames: Vec<VideoFrame>,
    pub errors: u64,
    pub last_error: Option<String>,
}

pub fn decode_video(d: &Demuxed) -> DecodedVideo {
    decode_video_paced(d, std::time::Duration::ZERO)
}

/// Decodes every video packet, waiting `pace` after each one (zero: as fast as possible, which
/// is how the pipeline feeds a decoder while its frame queue is empty).
pub fn decode_video_paced(d: &Demuxed, pace: std::time::Duration) -> DecodedVideo {
    decode_video_with_pauses(d, |_| pace)
}

/// Decodes every video packet, waiting `pause_after(n)` after packet n.
pub fn decode_video_with_pauses(d: &Demuxed, pause_after: impl Fn(usize) -> std::time::Duration) -> DecodedVideo {
    let track = d.info.video.as_ref().expect("video track");
    let mut dec = playr::vdec::make_decoder(track).expect("video decoder");
    let mut frames = Vec::new();
    for (n, (pts, data)) in d.video.iter().enumerate() {
        frames.extend(dec.decode(data, *pts));
        let pause = pause_after(n);
        if !pause.is_zero() {
            std::thread::sleep(pause);
        }
    }
    frames.extend(dec.flush());
    DecodedVideo { frames, errors: dec.errors(), last_error: dec.last_error().map(str::to_string) }
}

/// Symphonia's decoders plus the Opus adapter: the same set `playr::player` registers.
pub fn audio_codecs() -> &'static CodecRegistry {
    static REGISTRY: OnceLock<CodecRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let mut registry = CodecRegistry::new();
        symphonia::default::register_enabled_codecs(&mut registry);
        registry.register_audio_decoder::<symphonia_adapter_opus_rs::OpusAdapter>();
        registry
    })
}

pub struct DecodedAudio {
    pub rate: u32,
    pub channels: usize,
    /// Interleaved.
    pub samples: Vec<f32>,
    pub failed_packets: usize,
}

impl DecodedAudio {
    pub fn frames(&self) -> usize {
        self.samples.len() / self.channels.max(1)
    }

    pub fn channel(&self, c: usize) -> Vec<f32> {
        self.samples.iter().skip(c).step_by(self.channels).copied().collect()
    }
}

/// Decodes every audio packet and keeps what the pipeline plays: each packet's decoded frames
/// cut down to `skip..limit` with `playr::media::trim_decoded`, as `Packet::Audio` prescribes.
pub fn decode_audio(d: &Demuxed) -> DecodedAudio {
    let mut out = DecodedAudio { rate: 0, channels: 0, samples: Vec::new(), failed_packets: 0 };
    let mut scratch = Vec::new();
    let packets = d.audio.iter().zip(&d.audio_trim);
    match d.info.audio.as_ref().expect("audio track") {
        AudioTrack::Pcm { rate, channels, bits } => {
            out.rate = *rate;
            out.channels = *channels as usize;
            for ((_, p), (skip, limit)) in packets {
                scratch.clear();
                pcm_to_f32(&p.data, *bits, &mut scratch);
                playr::media::trim_decoded(&mut scratch, out.channels, *skip, *limit);
                out.samples.extend_from_slice(&scratch);
            }
        }
        AudioTrack::Symphonia(params) => {
            let mut dec = audio_codecs()
                .make_audio_decoder(params, &AudioDecoderOptions::default())
                .unwrap_or_else(|e| panic!("no audio decoder: {e}"));
            for ((_, p), (skip, limit)) in packets {
                match dec.decode(p) {
                    Ok(buf) => {
                        out.rate = buf.spec().rate();
                        out.channels = buf.spec().channels().count();
                        scratch.clear();
                        buf.copy_to_vec_interleaved::<f32>(&mut scratch);
                        playr::media::trim_decoded(&mut scratch, out.channels, *skip, *limit);
                        out.samples.extend_from_slice(&scratch);
                    }
                    Err(_) => out.failed_packets += 1,
                }
            }
        }
    }
    out
}

/// Cycles of a tone: upward swings from below `-threshold` to above `+threshold`. The
/// hysteresis keeps codec noise in near-silent stretches (encoder priming) from counting.
pub fn tone_cycles(samples: &[f32], threshold: f32) -> usize {
    let mut below = false;
    let mut cycles = 0;
    for &s in samples {
        if s < -threshold {
            below = true;
        } else if s > threshold && below {
            below = false;
            cycles += 1;
        }
    }
    cycles
}

pub fn rms(samples: &[f32]) -> f64 {
    (samples.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / samples.len().max(1) as f64).sqrt()
}

