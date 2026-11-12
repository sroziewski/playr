//! Container demuxing: symphonia for everything it understands, the local AVI reader otherwise.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::video::VideoCodecId;
use symphonia::core::codecs::video::well_known::extra_data::{
    VIDEO_EXTRA_DATA_ID_AVC_DECODER_CONFIG, VIDEO_EXTRA_DATA_ID_HEVC_DECODER_CONFIG,
};
use symphonia::core::codecs::video::well_known::{
    CODEC_ID_AV1, CODEC_ID_H264, CODEC_ID_HEVC, CODEC_ID_MJPEG, CODEC_ID_MPEG2, CODEC_ID_MPEG4, CODEC_ID_THEORA,
    CODEC_ID_VP8, CODEC_ID_VP9,
};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::{Time, TimeBase, Timestamp};

use crate::avi::AviDemuxer;
use crate::media::{AudioTrack, MediaInfo, Packet, VideoCodec, VideoTrack};

pub trait Demuxer {
    fn info(&self) -> &MediaInfo;
    /// `Ok(None)` at the end of the media.
    fn next_packet(&mut self) -> Result<Option<Packet>, String>;
    /// Repositions at or before `secs` and returns where it landed. The next video packet is
    /// not guaranteed to be a keyframe; the caller searches for one.
    fn seek(&mut self, secs: f64) -> Result<f64, String>;
}

/// Opens a local file, picking the demuxer from its content rather than its name.
pub fn open_path(path: &Path) -> Result<Box<dyn Demuxer>, String> {
    let mut file = File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    let mut magic = [0u8; 12];
    let got = file.read(&mut magic).unwrap_or(0);
    file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    if got == 12 && &magic[0..4] == b"RIFF" && &magic[8..12] == b"AVI " {
        return Ok(Box::new(AviDemuxer::open(file)?));
    }
    Ok(Box::new(SymDemuxer::open_file(path)?))
}

/// Opens a non-seekable byte stream (internet radio, HTTP downloads).
pub fn open_stream(
    source: Box<dyn MediaSource>,
    ext: Option<&str>,
    mime: Option<&str>,
) -> Result<Box<dyn Demuxer>, String> {
    Ok(Box::new(SymDemuxer::open(source, ext, mime)?))
}

fn video_codec(id: VideoCodecId) -> VideoCodec {
    let name = match id {
        CODEC_ID_H264 => return VideoCodec::H264,
        CODEC_ID_HEVC => return VideoCodec::Hevc,
        CODEC_ID_MPEG4 => return VideoCodec::Mpeg4,
        CODEC_ID_VP8 => "VP8".to_string(),
        CODEC_ID_VP9 => "VP9".to_string(),
        CODEC_ID_AV1 => "AV1".to_string(),
        CODEC_ID_MJPEG => "Motion JPEG".to_string(),
        CODEC_ID_MPEG2 => "MPEG-2".to_string(),
        CODEC_ID_THEORA => "Theora".to_string(),
        other => format!("codec {other}"),
    };
    VideoCodec::Unsupported(name)
}

fn ts_secs(tb: Option<TimeBase>, ts: Timestamp) -> f64 {
    tb.and_then(|tb| tb.calc_time(ts)).map_or(0.0, |t| t.as_secs_f64())
}

/// Seconds of encoder priming at the start of the first audio track of an MP4 file, taken
/// from its edit list (`moov/trak/edts/elst`), or 0 when there is none.
///
/// AAC encoders emit about one frame of warm-up before the real signal and record its length
/// here; symphonia does not surface it, and playing it would put sound that much behind the
/// picture.
pub fn mp4_audio_priming_secs(path: &Path) -> f64 {
    read_moov(path).and_then(|moov| audio_priming_from_moov(&moov)).unwrap_or(0.0)
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    b.get(at..at + 4).map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

fn be64(b: &[u8], at: usize) -> Option<u64> {
    b.get(at..at + 8).map(|s| u64::from_be_bytes(s.try_into().expect("8 bytes")))
}

fn read_moov(path: &Path) -> Option<Vec<u8>> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let mut pos = 0u64;
    while pos + 8 <= len {
        let mut head = [0u8; 16];
        file.seek(SeekFrom::Start(pos)).ok()?;
        file.read_exact(&mut head[..8]).ok()?;
        let mut size = be32(&head, 0)? as u64;
        let mut header = 8u64;
        if size == 1 {
            file.read_exact(&mut head[8..]).ok()?;
            size = be64(&head, 8)?;
            header = 16;
        } else if size == 0 {
            size = len - pos;
        }
        if size < header {
            return None;
        }
        if &head[4..8] == b"moov" {
            let body = size - header;
            if body > 64 << 20 || pos + size > len {
                return None;
            }
            let mut buf = vec![0u8; body as usize];
            file.read_exact(&mut buf).ok()?;
            return Some(buf);
        }
        pos = pos.checked_add(size)?;
    }
    None
}

/// Child boxes of an MP4 container box body, as (type, body).
fn boxes(body: &[u8]) -> impl Iterator<Item = ([u8; 4], &[u8])> {
    let mut pos = 0usize;
    std::iter::from_fn(move || {
        let size = be32(body, pos)? as usize;
        let ty: [u8; 4] = body.get(pos + 4..pos + 8)?.try_into().ok()?;
        if size < 8 || pos + size > body.len() {
            return None;
        }
        let inner = &body[pos + 8..pos + size];
        pos += size;
        Some((ty, inner))
    })
}

fn child<'a>(body: &'a [u8], ty: &[u8; 4]) -> Option<&'a [u8]> {
    boxes(body).find(|(t, _)| t == ty).map(|(_, b)| b)
}

fn audio_priming_from_moov(moov: &[u8]) -> Option<f64> {
    for (_, trak) in boxes(moov).filter(|(t, _)| t == b"trak") {
        let mdia = child(trak, b"mdia")?;
        if child(mdia, b"hdlr")?.get(8..12)? != b"soun" {
            continue;
        }
        let mdhd = child(mdia, b"mdhd")?;
        let timescale = if *mdhd.first()? == 1 { be32(mdhd, 20)? } else { be32(mdhd, 12)? };
        let elst = child(child(trak, b"edts")?, b"elst")?;
        let version = *elst.first()?;
        let entries = be32(elst, 4)? as usize;
        for i in 0..entries.min(16) {
            // An entry is (segment duration, media time, rate); media time -1 is an empty edit.
            let media_time = if version == 1 {
                be64(elst, 8 + i * 20 + 8)? as i64
            } else {
                be32(elst, 8 + i * 12 + 4)? as i32 as i64
            };
            if media_time >= 0 {
                return (timescale > 0).then(|| media_time as f64 / timescale as f64);
            }
        }
        return None;
    }
    None
}

pub struct SymDemuxer {
    fmt: Box<dyn FormatReader>,
    info: MediaInfo,
    video: Option<(u32, Option<TimeBase>)>,
    audio: Option<(u32, Option<TimeBase>)>,
    /// Set for local files, so the reader can be rebuilt when symphonia cannot reposition it.
    path: Option<PathBuf>,
    /// The reader has returned end-of-media or an error since it was last (re)opened.
    ended: bool,
    /// MP4 only: packet durations are sample-accurate, so encoder priming (this many seconds
    /// at the start) and the padding in the final packet can be cut away.
    mp4_trim: bool,
    priming_secs: f64,
    audio_rate: Option<u32>,
    /// Longest audio packet seen so far, in sample frames: the size of an ordinary packet.
    nominal_frames: f64,
    /// Packets already read while checking where a seek really landed.
    peeked: VecDeque<Packet>,
}

/// How far before the landing point packets may legitimately sit right after a seek
/// (B-frame reordering, audio interleaving).
const SEEK_SLACK: f64 = 0.5;

impl SymDemuxer {
    pub fn open_file(path: &Path) -> Result<Self, String> {
        let file = File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
        let ext = path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase());
        let mut demux = Self::open(Box::new(file), ext.as_deref(), None)?;
        demux.path = Some(path.to_path_buf());
        if demux.info.container == "isomp4" {
            demux.mp4_trim = true;
            demux.priming_secs = mp4_audio_priming_secs(path);
            // The track length includes the priming that will not be played.
            if demux.info.video.is_none() {
                demux.info.duration = demux.info.duration.map(|d| (d - demux.priming_secs).max(0.0));
            }
        }
        Ok(demux)
    }

    pub fn open(source: Box<dyn MediaSource>, ext: Option<&str>, mime: Option<&str>) -> Result<Self, String> {
        let seekable = source.is_seekable();
        let mss = MediaSourceStream::new(source, Default::default());
        let mut hint = Hint::new();
        if let Some(ext) = ext {
            hint.with_extension(ext);
        }
        if let Some(mime) = mime {
            hint.mime_type(mime);
        }
        let fmt = symphonia::default::get_probe()
            .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
            .map_err(|e| format!("unrecognised or unsupported format ({e})"))?;

        let mut info = MediaInfo {
            container: fmt.format_info().short_name.to_string(),
            seekable,
            ..Default::default()
        };
        let mut video = None;
        let mut audio = None;
        let mut audio_rate = None;
        let mut durations = Vec::new();

        if let Some(t) = fmt.default_track(TrackType::Video)
            && let Some(CodecParameters::Video(p)) = &t.codec_params
        {
            let wanted = [VIDEO_EXTRA_DATA_ID_AVC_DECODER_CONFIG, VIDEO_EXTRA_DATA_ID_HEVC_DECODER_CONFIG];
            let extra = p
                .extra_data
                .iter()
                .find(|e| wanted.contains(&e.id))
                .or(p.extra_data.first())
                .map(|e| e.data.to_vec())
                .unwrap_or_default();
            info.video = Some(VideoTrack {
                codec: video_codec(p.codec),
                width: p.width.unwrap_or(0) as u32,
                height: p.height.unwrap_or(0) as u32,
                extra,
            });
            video = Some((t.id, t.time_base));
            durations.extend(t.time_base.zip(t.duration).and_then(|(tb, d)| tb.calc_duration(d)));
        }
        if let Some(t) = fmt.default_track(TrackType::Audio)
            && let Some(CodecParameters::Audio(p)) = &t.codec_params
        {
            info.audio = Some(AudioTrack::Symphonia(p.clone()));
            audio_rate = p.sample_rate;
            audio = Some((t.id, t.time_base));
            durations.extend(t.time_base.zip(t.duration).and_then(|(tb, d)| tb.calc_duration(d)));
        }
        if video.is_none() && audio.is_none() {
            return Err("no audio or video track found".into());
        }
        if durations.is_empty() {
            // Matroska records one duration for the whole segment rather than per track.
            let media = fmt.media_info();
            durations.extend(media.time_base.zip(media.duration).and_then(|(tb, d)| tb.calc_duration(d)));
        }
        info.duration = durations.iter().map(Time::as_secs_f64).fold(None, |acc: Option<f64>, d| {
            Some(acc.map_or(d, |a| a.max(d)))
        });
        // A stream that cannot seek has no meaningful total length even if a header claims one.
        if !seekable {
            info.duration = None;
        }
        Ok(Self {
            fmt,
            info,
            video,
            audio,
            path: None,
            ended: false,
            mp4_trim: false,
            priming_secs: 0.0,
            audio_rate,
            nominal_frames: 0.0,
            peeked: VecDeque::new(),
        })
    }

    /// Symphonia's MP4 and Matroska readers cannot be repositioned once they have run off the
    /// end of the file, so a seek from there starts over with a fresh reader.
    fn reopen(&mut self) -> Result<(), String> {
        let path = self.path.clone().ok_or("stream cannot be reopened")?;
        *self = Self::open_file(&path)?;
        Ok(())
    }

    fn try_seek(&mut self, secs: f64) -> Result<f64, String> {
        let track = self.video.or(self.audio);
        let time = Time::try_from_secs_f64(secs).ok_or("seek position out of range")?;
        let to = self
            .fmt
            .seek(SeekMode::Accurate, SeekTo::Time { time, track_id: track.map(|t| t.0) })
            .map_err(|e| format!("seek failed: {e}"))?;
        let tb = [self.video, self.audio].into_iter().flatten().find(|t| t.0 == to.track_id).and_then(|t| t.1);
        Ok(ts_secs(tb, to.actual_ts))
    }
}

impl Demuxer for SymDemuxer {
    fn info(&self) -> &MediaInfo {
        &self.info
    }

    fn next_packet(&mut self) -> Result<Option<Packet>, String> {
        match self.peeked.pop_front() {
            Some(p) => Ok(Some(p)),
            None => self.read_packet(),
        }
    }

    fn seek(&mut self, secs: f64) -> Result<f64, String> {
        let secs = secs.max(0.0);
        self.peeked.clear();
        if self.path.is_none() {
            return self.try_seek(secs);
        }
        // A fresh reader is the one way to be at the start with nothing skipped.
        if secs == 0.0 {
            self.reopen()?;
            return Ok(0.0);
        }
        if self.ended {
            self.reopen()?;
        }
        let landed = match self.try_seek(secs) {
            Ok(landed) => Some(landed),
            Err(_) => {
                self.reopen()?;
                self.try_seek(secs).ok()
            }
        };
        if let Some(landed) = landed
            && landed <= secs + 1e-6
            && self.landing_is_sound(landed)
        {
            return Ok(landed);
        }
        self.seek_by_reading(secs)
    }
}

impl SymDemuxer {
    /// Reads a few packets ahead (keeping them) to confirm the reader really continues from
    /// `landed`.
    ///
    /// Symphonia's Matroska reader, given a file whose index has no entry near the target,
    /// reports a plausible landing point, returns one packet from there and then carries on
    /// from the start of the file. Only looking at what comes out reveals that.
    fn landing_is_sound(&mut self, landed: f64) -> bool {
        for _ in 0..6 {
            match self.read_packet() {
                Ok(Some(p)) => {
                    let sound = p.pts() >= landed - SEEK_SLACK;
                    self.peeked.push_back(p);
                    if !sound {
                        return false;
                    }
                }
                Ok(None) => break,
                Err(_) => return false,
            }
        }
        true
    }

    /// Fallback seek for files symphonia cannot reposition: start over and read forward,
    /// discarding packets, to just before the target. Linear in the target position, but it
    /// only touches the container, not the decoders.
    fn seek_by_reading(&mut self, secs: f64) -> Result<f64, String> {
        self.reopen()?;
        let from = secs - SEEK_SLACK;
        while let Some(p) = self.read_packet()? {
            if p.pts() >= from {
                let landed = p.pts().min(secs);
                self.peeked.push_back(p);
                return Ok(landed);
            }
        }
        Err("seek position is past the end".into())
    }

    fn read_packet(&mut self) -> Result<Option<Packet>, String> {
        loop {
            let packet = match self.fmt.next_packet() {
                Ok(Some(p)) => p,
                Ok(None) => {
                    self.ended = true;
                    return Ok(None);
                }
                Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    self.ended = true;
                    return Ok(None);
                }
                Err(e) => {
                    self.ended = true;
                    return Err(e.to_string());
                }
            };
            if let Some((id, tb)) = self.video
                && packet.track_id == id
            {
                return Ok(Some(Packet::Video { pts: ts_secs(tb, packet.pts), data: packet.data }));
            }
            if let Some((id, tb)) = self.audio
                && packet.track_id == id
            {
                let start = ts_secs(tb, packet.pts);
                let Some(rate) = self.audio_rate.filter(|_| self.mp4_trim).map(f64::from) else {
                    return Ok(Some(Packet::Audio { pts: start, packet, skip: 0, limit: None }));
                };
                // Container times are in the track's own timescale, which need not be the
                // sample rate; trimming counts decoded sample frames, so convert via seconds.
                //
                // Unless the timescale is a whole multiple of the sample rate it cannot express
                // a packet's length exactly: every time is rounded to a tick, up to `slack`
                // frames off (44.1 kHz audio on a 48 kHz timescale loses a sample here and
                // there just as a 1 kHz timescale loses tens). Trimming
                // by such times would shave samples off ordinary packets, so there it applies
                // only where the difference is larger than the rounding: the priming at the
                // very start and a clearly short final packet.
                let tick = ts_secs(tb, Timestamp::new(1));
                let timescale = if tick > 0.0 { (1.0 / tick).round() } else { 0.0 };
                let exact = timescale > 0.0 && (timescale % rate).abs() < 1e-6;
                let slack = if exact { 0.0 } else { tick * rate };
                let lead = (self.priming_secs - start).max(0.0) * rate;
                let skip = if lead > slack { lead.round() as u32 } else { 0 };
                let frames = tb.and_then(|tb| tb.calc_duration(packet.dur)).map_or(0.0, |t| t.as_secs_f64()) * rate;
                let short = frames + 2.0 * slack < self.nominal_frames;
                self.nominal_frames = self.nominal_frames.max(frames);
                let limit = (frames > 0.0 && (slack == 0.0 || short)).then(|| frames.round() as u32);
                // Presentation time of the first sample that is kept.
                let pts = (start + skip as f64 / rate - self.priming_secs).max(0.0);
                return Ok(Some(Packet::Audio { pts, packet, skip, limit }));
            }
        }
    }
}
