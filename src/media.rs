//! Types shared by the demuxers, decoders and the player pipeline.

use symphonia::core::codecs::audio::AudioCodecParameters;

pub use symphonia::core::packet::Packet as AudioPacket;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
    Hevc,
    Mpeg4,
    /// A codec that was recognised but has no decoder here; the string is shown to the user.
    Unsupported(String),
}

impl VideoCodec {
    pub fn name(&self) -> &str {
        match self {
            VideoCodec::H264 => "H.264",
            VideoCodec::Hevc => "HEVC",
            VideoCodec::Mpeg4 => "MPEG-4",
            VideoCodec::Unsupported(n) => n,
        }
    }
}

#[derive(Clone, Debug)]
pub struct VideoTrack {
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    /// Container-level decoder configuration: `avcC` for H.264, `hvcC` for HEVC, VOL headers
    /// for MPEG-4. Empty means the parameter sets travel in-band (Annex B, as in AVI).
    pub extra: Vec<u8>,
}

#[derive(Clone, Debug)]
pub enum AudioTrack {
    /// Decoded by a symphonia codec.
    Symphonia(AudioCodecParameters),
    /// Raw little-endian PCM (8-bit is unsigned, wider is signed), as stored in AVI.
    Pcm { rate: u32, channels: u16, bits: u16 },
}

#[derive(Clone, Debug, Default)]
pub struct MediaInfo {
    pub container: String,
    /// Seconds; `None` for live streams.
    pub duration: Option<f64>,
    pub seekable: bool,
    pub video: Option<VideoTrack>,
    pub audio: Option<AudioTrack>,
    /// Human-readable remarks about things that will not play (unsupported tracks and so on).
    pub notes: Vec<String>,
}

pub enum Packet {
    Video { pts: f64, data: Box<[u8]> },
    /// `pts` is the presentation time of the first sample that is kept. Of the frames the
    /// decoder returns for `packet`, the first `skip` are encoder priming and anything from
    /// index `limit` on is end padding; neither is to be played.
    Audio { pts: f64, packet: AudioPacket, skip: u32, limit: Option<u32> },
}

impl Packet {
    pub fn pts(&self) -> f64 {
        match self {
            Packet::Video { pts, .. } | Packet::Audio { pts, .. } => *pts,
        }
    }
}

/// Cuts decoded interleaved samples down to the frames `skip..limit` (see [`Packet::Audio`]).
pub fn trim_decoded(samples: &mut Vec<f32>, channels: usize, skip: u32, limit: Option<u32>) {
    let channels = channels.max(1);
    let frames = samples.len() / channels;
    let end = limit.map_or(frames, |l| (l as usize).min(frames));
    let start = (skip as usize).min(end);
    samples.truncate(end * channels);
    samples.drain(..start * channels);
}

/// One decoded picture as planar 8-bit YUV. Planes may be wider than the visible picture
/// (`*_stride` >= visible width), which is how MPEG-4 hands out macroblock-padded frames.
#[derive(Clone, Debug)]
pub struct VideoFrame {
    pub pts: f64,
    pub width: u32,
    pub height: u32,
    pub chroma_width: u32,
    pub chroma_height: u32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
    pub y_stride: u32,
    pub c_stride: u32,
}

/// `H:MM:SS` above an hour, `M:SS` below. Negative and non-finite inputs render as zero.
pub fn format_time(secs: f64) -> String {
    let total = if secs.is_finite() && secs > 0.0 { secs as u64 } else { 0 };
    let (h, m, s) = (total / 3600, total / 60 % 60, total % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}
