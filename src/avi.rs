//! AVI 1.0 (RIFF) demuxer: one video and one audio stream, indexed up front so the two can be
//! read through independent cursors regardless of how badly the file interleaves them.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use symphonia::core::codecs::audio::AudioCodecParameters;
use symphonia::core::codecs::audio::well_known::{CODEC_ID_MP2, CODEC_ID_MP3};
use symphonia::core::units::{Duration, Timestamp};

use crate::demux::Demuxer;
use crate::media::{AudioPacket, AudioTrack, MediaInfo, Packet, VideoCodec, VideoTrack};

const AVIIF_KEYFRAME: u32 = 0x10;

#[derive(Clone, Copy, Debug)]
struct Entry {
    off: u64,
    size: u32,
    pts: f64,
    key: bool,
}

#[derive(Clone, Debug, Default)]
struct StreamHeader {
    kind: [u8; 4],
    handler: [u8; 4],
    scale: u32,
    rate: u32,
    start: u32,
    sample_size: u32,
    format: Vec<u8>,
}

enum AudioKind {
    Mpa(MpaFramer),
    Pcm { block: usize },
}

pub struct AviDemuxer {
    file: File,
    info: MediaInfo,
    video: Vec<Entry>,
    audio: Vec<Entry>,
    vpos: usize,
    apos: usize,
    audio_kind: Option<AudioKind>,
    audio_rate: u32,
    out: VecDeque<Packet>,
}

fn le32(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4).map_or(0, |s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn le16(b: &[u8], at: usize) -> u16 {
    b.get(at..at + 2).map_or(0, |s| u16::from_le_bytes([s[0], s[1]]))
}

/// Stream number encoded in a `##xx` chunk id, or `None` for ids that are not stream data.
fn chunk_stream(id: &[u8; 4]) -> Option<usize> {
    let digit = |c: u8| (c as char).to_digit(16).map(|d| d as usize);
    Some(digit(id[0])? * 16 + digit(id[1])?)
}

fn fourcc_codec(fourcc: &[u8; 4]) -> VideoCodec {
    let mut up = *fourcc;
    up.make_ascii_uppercase();
    match &up {
        b"XVID" | b"DIVX" | b"DX50" | b"FMP4" | b"MP4V" | b"M4S2" | b"MP4S" | b"3IV2" | b"RMP4" => {
            VideoCodec::Mpeg4
        }
        b"H264" | b"X264" | b"AVC1" | b"DAVC" => VideoCodec::H264,
        b"HEVC" | b"H265" | b"X265" | b"HVC1" => VideoCodec::Hevc,
        _ => VideoCodec::Unsupported(String::from_utf8_lossy(fourcc).trim().to_string()),
    }
}

impl AviDemuxer {
    pub fn open(mut file: File) -> Result<Self, String> {
        let io = |e: std::io::Error| format!("AVI read error: {e}");
        let file_len = file.metadata().map_err(io)?.len();
        let mut head = [0u8; 12];
        file.seek(SeekFrom::Start(0)).map_err(io)?;
        file.read_exact(&mut head).map_err(io)?;
        if &head[0..4] != b"RIFF" || &head[8..12] != b"AVI " {
            return Err("not an AVI file".into());
        }

        let mut streams: Vec<StreamHeader> = Vec::new();
        let mut movi: Option<(u64, u64)> = None; // (position of the 'movi' fourcc, end of list)
        let mut idx1: Vec<u8> = Vec::new();
        let mut pos = 12u64;
        while pos + 8 <= file_len {
            let mut h = [0u8; 8];
            file.seek(SeekFrom::Start(pos)).map_err(io)?;
            if file.read_exact(&mut h).is_err() {
                break;
            }
            let size = le32(&h, 4) as u64;
            let body = pos + 8;
            match &h[0..4] {
                b"LIST" => {
                    let mut ty = [0u8; 4];
                    if file.read_exact(&mut ty).is_err() {
                        break;
                    }
                    if &ty == b"hdrl" {
                        let len = size.saturating_sub(4).min(16 << 20) as usize;
                        let mut buf = vec![0u8; len];
                        file.read_exact(&mut buf).map_err(io)?;
                        parse_hdrl(&buf, &mut streams);
                    } else if &ty == b"movi" {
                        movi = Some((body, (body + size).min(file_len)));
                    }
                }
                b"idx1" => {
                    let len = size.min(file_len.saturating_sub(body)).min(256 << 20) as usize;
                    idx1 = vec![0u8; len];
                    file.read_exact(&mut idx1).map_err(io)?;
                }
                // A second RIFF chunk is an OpenDML extension segment; only the first is read.
                b"RIFF" => break,
                _ => {}
            }
            pos = body + size + (size & 1);
        }
        let (movi_pos, movi_end) = movi.ok_or("AVI has no movi list")?;

        let v_idx = streams.iter().position(|s| &s.kind == b"vids");
        let a_idx = streams.iter().position(|s| &s.kind == b"auds");
        let mut raw = index_from_idx1(&mut file, &idx1, movi_pos);
        if raw.is_empty() {
            raw = index_by_scanning(&mut file, movi_pos + 4, movi_end);
        }

        let mut info = MediaInfo { container: "avi".into(), seekable: true, ..Default::default() };
        let mut video = Vec::new();
        if let Some(vi) = v_idx {
            let sh = &streams[vi];
            // The stream format's compression field says what the data is; the handler tag in
            // strh only names a preferred codec driver, so it is consulted only when the
            // format leaves the field blank. Trusting the handler over a real format tag would
            // feed, say, JPEG data to the MPEG-4 decoder.
            let named = |tag: &&[u8]| tag.iter().any(|&b| b != 0 && b != b' ');
            let mut tag = [0u8; 4];
            tag.copy_from_slice(sh.format.get(16..20).filter(named).unwrap_or(&sh.handler));
            let codec = fourcc_codec(&tag);
            let frame_dur = if sh.rate > 0 { sh.scale.max(1) as f64 / sh.rate as f64 } else { 1.0 / 25.0 };
            let mut n = sh.start as f64;
            for e in raw.iter().filter(|e| e.0 == vi) {
                video.push(Entry { off: e.1, size: e.2, pts: n * frame_dur, key: e.3 });
                n += 1.0;
            }
            info.video = Some(VideoTrack {
                codec,
                width: le32(&sh.format, 4),
                height: (le32(&sh.format, 8) as i32).unsigned_abs(),
                extra: Vec::new(),
            });
        }

        let mut audio = Vec::new();
        let mut audio_kind = None;
        let mut audio_rate = 0;
        let mut audio_end = 0.0;
        if let Some(ai) = a_idx {
            let sh = &streams[ai];
            let tag = le16(&sh.format, 0);
            let channels = le16(&sh.format, 2);
            let rate = le32(&sh.format, 4);
            let avg_bytes = le32(&sh.format, 8);
            let block_align = le16(&sh.format, 12).max(1) as u64;
            let bits = le16(&sh.format, 14);
            audio_rate = rate;
            match tag {
                0x55 | 0x50 if rate > 0 => {
                    let mut p = AudioCodecParameters::new();
                    p.codec = if tag == 0x55 { CODEC_ID_MP3 } else { CODEC_ID_MP2 };
                    p.sample_rate = Some(rate);
                    info.audio = Some(AudioTrack::Symphonia(p));
                    audio_kind = Some(AudioKind::Mpa(MpaFramer::default()));
                }
                1 if rate > 0 && channels > 0 && matches!(bits, 8 | 16 | 24 | 32) => {
                    info.audio = Some(AudioTrack::Pcm { rate, channels, bits });
                    audio_kind = Some(AudioKind::Pcm { block: channels as usize * bits as usize / 8 });
                }
                other => info.notes.push(format!("audio format 0x{other:04x} in AVI is not supported")),
            }
            if audio_kind.is_some() {
                // CBR streams are timed by byte position, VBR ones by block count.
                let (mut bytes, mut blocks) = (0u64, 0u64);
                let per_block = if sh.rate > 0 { sh.scale.max(1) as f64 / sh.rate as f64 } else { 0.0 };
                let time_at = |bytes: u64, blocks: u64| {
                    if sh.sample_size != 0 && avg_bytes > 0 {
                        bytes as f64 / avg_bytes as f64
                    } else {
                        blocks as f64 * per_block
                    }
                };
                for e in raw.iter().filter(|e| e.0 == ai) {
                    audio.push(Entry { off: e.1, size: e.2, pts: time_at(bytes, blocks), key: true });
                    bytes += e.2 as u64;
                    blocks += (e.2 as u64).div_ceil(block_align).max(1);
                }
                // The stream ends where the last chunk ends, not where it starts.
                audio_end = time_at(bytes, blocks);
            }
        }

        if info.video.is_none() && info.audio.is_none() {
            return Err("AVI has no playable streams".into());
        }
        let v_end = match (video.last(), video.len()) {
            (Some(last), n) if n > 1 => last.pts + (last.pts - video[0].pts) / (n - 1) as f64,
            (Some(last), _) => last.pts,
            _ => 0.0,
        };
        let end = v_end.max(audio_end);
        info.duration = (end > 0.0).then_some(end);

        Ok(Self { file, info, video, audio, vpos: 0, apos: 0, audio_kind, audio_rate, out: VecDeque::new() })
    }

    fn read_chunk(&mut self, e: Entry) -> Result<Vec<u8>, String> {
        // An index may claim any size; never allocate more than the file could hold.
        let len = self.file.metadata().map_or(0, |m| m.len());
        if e.off.saturating_add(e.size as u64) > len {
            return Err("AVI chunk runs past the end of the file".into());
        }
        let mut buf = vec![0u8; e.size as usize];
        self.file
            .seek(SeekFrom::Start(e.off))
            .and_then(|_| self.file.read_exact(&mut buf))
            .map_err(|e| format!("AVI read error: {e}"))?;
        Ok(buf)
    }
}

fn parse_hdrl(buf: &[u8], streams: &mut Vec<StreamHeader>) {
    let mut pos = 0usize;
    while pos + 8 <= buf.len() {
        let id = &buf[pos..pos + 4];
        let size = le32(buf, pos + 4) as usize;
        let body = pos + 8;
        let end = body.saturating_add(size).min(buf.len());
        if id == b"LIST" && buf.get(body..body + 4) == Some(b"strl") {
            let mut sh = StreamHeader::default();
            let mut p = body + 4;
            while p + 8 <= end {
                let cid = &buf[p..p + 4];
                let csize = le32(buf, p + 4) as usize;
                let cbody = p + 8;
                let cend = cbody.saturating_add(csize).min(end);
                let data = &buf[cbody..cend];
                if cid == b"strh" && data.len() >= 48 {
                    sh.kind.copy_from_slice(&data[0..4]);
                    sh.handler.copy_from_slice(&data[4..8]);
                    sh.scale = le32(data, 20);
                    sh.rate = le32(data, 24);
                    sh.start = le32(data, 28);
                    sh.sample_size = le32(data, 44);
                } else if cid == b"strf" {
                    sh.format = data.to_vec();
                }
                p = cbody + csize + (csize & 1);
            }
            streams.push(sh);
        }
        pos = body + size + (size & 1);
    }
}

/// (stream, data offset, size, keyframe) for every data chunk listed in `idx1`.
fn index_from_idx1(file: &mut File, idx1: &[u8], movi_pos: u64) -> Vec<(usize, u64, u32, bool)> {
    let mut out = Vec::with_capacity(idx1.len() / 16);
    let mut base: Option<u64> = None;
    for e in idx1.as_chunks::<16>().0 {
        let id = [e[0], e[1], e[2], e[3]];
        let Some(stream) = chunk_stream(&id) else { continue };
        if !matches!(&id[2..4], b"dc" | b"db" | b"wb") {
            continue;
        }
        let off = le32(e, 8) as u64;
        // idx1 offsets are relative to the 'movi' fourcc in most files and absolute in some;
        // the first entry tells which, by where its chunk id is actually found.
        let base = *base.get_or_insert_with(|| {
            let mut probe = [0u8; 4];
            let relative = file
                .seek(SeekFrom::Start(movi_pos + off))
                .and_then(|_| file.read_exact(&mut probe))
                .is_ok_and(|_| probe == id);
            if relative { movi_pos } else { 0 }
        });
        out.push((stream, base + off + 8, le32(e, 12), le32(e, 4) & AVIIF_KEYFRAME != 0));
    }
    out
}

/// Fallback for files without an index: walk the chunk headers of the movi list.
fn index_by_scanning(file: &mut File, start: u64, end: u64) -> Vec<(usize, u64, u32, bool)> {
    let mut out = Vec::new();
    let mut pos = start;
    let mut head = [0u8; 40];
    while pos + 8 <= end {
        if file.seek(SeekFrom::Start(pos)).is_err() {
            break;
        }
        let got = read_up_to(file, &mut head);
        if got < 8 {
            break;
        }
        let id = [head[0], head[1], head[2], head[3]];
        let size = le32(&head, 4);
        if &id == b"LIST" {
            pos += 12; // descend into 'rec ' groups
            continue;
        }
        if let Some(stream) = chunk_stream(&id)
            && matches!(&id[2..4], b"dc" | b"db" | b"wb") {
                let prefix = &head[8..got.min(8 + size as usize)];
                out.push((stream, pos + 8, size, prefix_is_key(prefix)));
            }
        pos += 8 + size as u64 + (size as u64 & 1);
    }
    out
}

fn read_up_to(file: &mut File, buf: &mut [u8]) -> usize {
    let mut got = 0;
    while got < buf.len() {
        match file.read(&mut buf[got..]) {
            Ok(0) | Err(_) => break,
            Ok(n) => got += n,
        }
    }
    got
}

/// Keyframe guess from the first bytes of a chunk, for files that carry no index flags.
/// MPEG-4 keyframes open with configuration headers or an intra VOP; H.264 ones with SPS/IDR.
fn prefix_is_key(p: &[u8]) -> bool {
    let Some(at) = p.windows(3).position(|w| w == [0, 0, 1]) else { return false };
    let Some(&code) = p.get(at + 3) else { return false };
    match code {
        0xB6 => p.get(at + 4).is_some_and(|b| b >> 6 == 0),
        0x00..=0x2F | 0xB0 | 0xB3 | 0xB5 => true,
        c => matches!(c & 0x1f, 5 | 7),
    }
}

impl Demuxer for AviDemuxer {
    fn info(&self) -> &MediaInfo {
        &self.info
    }

    fn next_packet(&mut self) -> Result<Option<Packet>, String> {
        loop {
            if let Some(p) = self.out.pop_front() {
                return Ok(Some(p));
            }
            let v = self.video.get(self.vpos).copied();
            let a = self.audio.get(self.apos).copied();
            let take_video = match (v, a) {
                (None, None) => return Ok(None),
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (Some(v), Some(a)) => v.pts <= a.pts,
            };
            if take_video {
                let e = v.expect("checked above");
                self.vpos += 1;
                // Zero-length chunks mark dropped frames: they advance time and carry nothing.
                if e.size == 0 {
                    continue;
                }
                let data = self.read_chunk(e)?;
                return Ok(Some(Packet::Video { pts: e.pts, data: data.into_boxed_slice() }));
            }
            let e = a.expect("checked above");
            self.apos += 1;
            if e.size == 0 {
                continue;
            }
            let data = self.read_chunk(e)?;
            let rate = self.audio_rate.max(1) as f64;
            match self.audio_kind.as_mut() {
                Some(AudioKind::Mpa(framer)) => {
                    framer.push(&data, e.pts);
                    while let Some((pts, frame, samples)) = framer.next_frame() {
                        let ts = Timestamp::new((pts * rate).round() as i64);
                        let packet = AudioPacket::new(0, ts, Duration::new(samples as u64), frame);
                        self.out.push_back(Packet::Audio { pts, packet, skip: 0, limit: None });
                    }
                }
                Some(AudioKind::Pcm { block }) => {
                    let frames = data.len() / (*block).max(1);
                    let ts = Timestamp::new((e.pts * rate).round() as i64);
                    let packet = AudioPacket::new(0, ts, Duration::new(frames as u64), data);
                    return Ok(Some(Packet::Audio { pts: e.pts, packet, skip: 0, limit: None }));
                }
                None => {}
            }
        }
    }

    fn seek(&mut self, secs: f64) -> Result<f64, String> {
        self.out.clear();
        let mut landed = secs.max(0.0);
        if !self.video.is_empty() {
            let upto = self.video.partition_point(|e| e.pts <= landed + 1e-6);
            self.vpos = self.video[..upto].iter().rposition(|e| e.key && e.size > 0).unwrap_or(0);
            landed = self.video[self.vpos].pts;
        }
        // The audio chunk that contains `landed`, so sound is available from that instant.
        self.apos = self.audio.partition_point(|e| e.pts <= landed + 1e-6).saturating_sub(1);
        if let Some(AudioKind::Mpa(framer)) = self.audio_kind.as_mut() {
            framer.reset();
        }
        Ok(landed)
    }
}

// ---------------------------------------------------------------------------------------------

/// Length in bytes, samples per frame and sample rate of the MPEG audio frame that starts with
/// header `h`, or `None` if `h` is not a valid frame header.
pub fn mpa_frame_info(h: [u8; 4]) -> Option<(usize, u32, u32)> {
    if h[0] != 0xFF || h[1] & 0xE0 != 0xE0 {
        return None;
    }
    let version = (h[1] >> 3) & 3; // 0 = MPEG-2.5, 2 = MPEG-2, 3 = MPEG-1
    let layer = (h[1] >> 1) & 3; // 1 = III, 2 = II, 3 = I
    let br_idx = (h[2] >> 4) as usize;
    let sr_idx = ((h[2] >> 2) & 3) as usize;
    let pad = ((h[2] >> 1) & 1) as usize;
    if version == 1 || layer == 0 || br_idx == 0 || br_idx == 15 || sr_idx == 3 {
        return None;
    }
    const V1L1: [u32; 14] = [32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448];
    const V1L2: [u32; 14] = [32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384];
    const V1L3: [u32; 14] = [32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320];
    const V2L1: [u32; 14] = [32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256];
    const V2L23: [u32; 14] = [8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];
    let mpeg1 = version == 3;
    let kbps = match (mpeg1, layer) {
        (true, 3) => V1L1,
        (true, 2) => V1L2,
        (true, _) => V1L3,
        (false, 3) => V2L1,
        (false, _) => V2L23,
    }[br_idx - 1];
    let rate = [44100u32, 48000, 32000][sr_idx] >> match version {
        3 => 0,
        2 => 1,
        _ => 2,
    };
    let bits = kbps as usize * 1000;
    let (len, samples) = match layer {
        3 => ((12 * bits / rate as usize + pad) * 4, 384),
        2 => (144 * bits / rate as usize + pad, 1152),
        _ if mpeg1 => (144 * bits / rate as usize + pad, 1152),
        _ => (72 * bits / rate as usize + pad, 576),
    };
    (len > 4).then_some((len, samples, rate))
}

/// Reassembles whole MPEG audio frames from AVI chunks, which may split or batch frames at
/// arbitrary byte positions while the decoder needs exactly one frame per packet.
#[derive(Default)]
pub struct MpaFramer {
    buf: Vec<u8>,
    /// Offset of the first unconsumed byte in `buf`.
    start: usize,
    /// Timestamp of the next frame to be emitted; `None` until the first chunk after a reset.
    next_pts: Option<f64>,
}

impl MpaFramer {
    pub fn reset(&mut self) {
        self.buf.clear();
        self.start = 0;
        self.next_pts = None;
    }

    pub fn push(&mut self, data: &[u8], pts: f64) {
        if self.next_pts.is_none() {
            self.next_pts = Some(pts);
        }
        self.buf.extend_from_slice(data);
    }

    /// The next complete frame as (pts, bytes, samples), or `None` until more data arrives.
    pub fn next_frame(&mut self) -> Option<(f64, Vec<u8>, u32)> {
        // Frames are taken from `buf[start..]`; the consumed front is cut away only when the
        // buffer runs dry, so a chunk holding thousands of frames is not shifted once per frame.
        let mut at = self.start;
        loop {
            if at + 4 > self.buf.len() {
                // Keep a possible partial header for the next chunk.
                self.buf.drain(..at.min(self.buf.len()));
                self.start = 0;
                return None;
            }
            let h = [self.buf[at], self.buf[at + 1], self.buf[at + 2], self.buf[at + 3]];
            let Some((len, samples, rate)) = mpa_frame_info(h) else {
                at += 1;
                continue;
            };
            if at + len > self.buf.len() {
                self.buf.drain(..at);
                self.start = 0;
                return None;
            }
            let frame = self.buf[at..at + len].to_vec();
            self.start = at + len;
            let pts = self.next_pts.unwrap_or(0.0);
            self.next_pts = Some(pts + samples as f64 / rate as f64);
            return Some((pts, frame, samples));
        }
    }
}
