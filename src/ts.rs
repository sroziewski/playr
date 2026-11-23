//! MPEG transport stream, the container of most HLS segments: 188-byte packets carrying the
//! program tables and, per elementary stream, PES packets that each hold one access unit (a
//! picture) or a run of audio frames with its presentation time.
//!
//! [`TsDemux`] picks the first video and the first audio stream of the first program and
//! hands out their PES payloads; [`AdtsFramer`] cuts AAC payloads into frames.

/// Length of one transport stream packet.
pub const PACKET: usize = 188;
const SYNC: u8 = 0x47;

/// One tick of the 90 kHz clock PES timestamps count in.
pub const CLOCK: f64 = 90_000.0;
/// PES timestamps are 33 bits wide and wrap around after about 26.5 hours.
pub const WRAP: u64 = 1 << 33;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamKind {
    H264,
    Hevc,
    /// AAC in ADTS frames.
    Aac,
    /// MPEG-1/2 audio layers I-III.
    Mpa,
}

impl StreamKind {
    pub fn from_type(stream_type: u8) -> Option<Self> {
        Some(match stream_type {
            0x1B => StreamKind::H264,
            0x24 => StreamKind::Hevc,
            0x0F => StreamKind::Aac,
            0x03 | 0x04 => StreamKind::Mpa,
            _ => return None,
        })
    }

    pub fn is_video(self) -> bool {
        matches!(self, StreamKind::H264 | StreamKind::Hevc)
    }
}

/// What a stream type the demuxer leaves alone is called, for a note to the user; `None` for
/// streams that carry no picture or sound (timed metadata, subtitles).
pub fn unsupported_name(stream_type: u8) -> Option<&'static str> {
    Some(match stream_type {
        0x01 | 0x02 => "MPEG-2 video",
        0x10 => "MPEG-4 video",
        0x11 => "AAC (LATM) sound",
        0x81 => "AC-3 sound",
        0x87 => "E-AC-3 sound",
        0x42 => "AVS video",
        0xEA => "VC-1 video",
        _ => return None,
    })
}

/// A complete PES payload of the picked video or audio stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pes {
    pub kind: StreamKind,
    /// Presentation time in 90 kHz ticks.
    pub pts: Option<u64>,
    pub data: Vec<u8>,
}

#[derive(Default)]
struct Building {
    pts: Option<u64>,
    data: Vec<u8>,
    started: bool,
}

#[derive(Default)]
pub struct TsDemux {
    pmt_pid: Option<u16>,
    pub video: Option<(u16, StreamKind)>,
    pub audio: Option<(u16, StreamKind)>,
    /// Stream types of the program that are neither the picked video nor the picked audio.
    pub other_types: Vec<u8>,
    /// A program table split over several packets, being put together: (pid, bytes).
    section: Option<(u16, Vec<u8>)>,
    building_video: Building,
    building_audio: Building,
    out: Vec<Pes>,
}

impl TsDemux {
    /// Feeds a run of transport stream bytes (a whole HLS segment, say) and returns the PES
    /// payloads it completed. Call [`flush`](Self::flush) when no more bytes of this run follow.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Pes> {
        let mut at = 0;
        while at + PACKET <= bytes.len() {
            if bytes[at] != SYNC {
                at = match resync(bytes, at) {
                    Some(next) => next,
                    None => break,
                };
                continue;
            }
            self.packet(&bytes[at..at + PACKET]);
            at += PACKET;
        }
        std::mem::take(&mut self.out)
    }

    /// Ends the PES packets still being collected: their end is where the bytes stopped.
    pub fn flush(&mut self) -> Vec<Pes> {
        let video = self.video.map(|(_, k)| k);
        let audio = self.audio.map(|(_, k)| k);
        if let Some(kind) = video {
            finish(&mut self.building_video, kind, &mut self.out);
        }
        if let Some(kind) = audio {
            finish(&mut self.building_audio, kind, &mut self.out);
        }
        std::mem::take(&mut self.out)
    }

    /// Whether the program table has been read.
    pub fn has_program(&self) -> bool {
        self.video.is_some() || self.audio.is_some() || !self.other_types.is_empty()
    }

    fn packet(&mut self, p: &[u8]) {
        if p[1] & 0x80 != 0 {
            return; // transport error indicator
        }
        let start = p[1] & 0x40 != 0;
        let pid = u16::from(p[1] & 0x1F) << 8 | u16::from(p[2]);
        let control = (p[3] >> 4) & 3;
        if control & 1 == 0 {
            return; // no payload
        }
        let mut offset = 4;
        if control & 2 != 0 {
            offset += 1 + p[4] as usize;
        }
        if offset >= PACKET {
            return;
        }
        let payload = &p[offset..];
        if pid == 0 || Some(pid) == self.pmt_pid {
            self.section_bytes(pid, start, payload);
            return;
        }
        if let Some((vpid, kind)) = self.video
            && pid == vpid
        {
            pes_bytes(&mut self.building_video, kind, start, payload, &mut self.out);
        } else if let Some((apid, kind)) = self.audio
            && pid == apid
        {
            pes_bytes(&mut self.building_audio, kind, start, payload, &mut self.out);
        }
    }

    fn section_bytes(&mut self, pid: u16, start: bool, payload: &[u8]) {
        let mut bytes = if start {
            let pointer = payload[0] as usize;
            match payload.get(1 + pointer..) {
                Some(rest) => rest.to_vec(),
                None => return,
            }
        } else {
            match self.section.take() {
                Some((p, mut so_far)) if p == pid => {
                    so_far.extend_from_slice(payload);
                    so_far
                }
                _ => return,
            }
        };
        if bytes.len() < 3 {
            self.section = Some((pid, bytes));
            return;
        }
        let length = (usize::from(bytes[1] & 0x0F) << 8 | usize::from(bytes[2])) + 3;
        if bytes.len() < length {
            if length <= 4096 {
                self.section = Some((pid, bytes));
            }
            return;
        }
        bytes.truncate(length);
        match bytes[0] {
            0x00 if pid == 0 => self.read_pat(&bytes),
            0x02 if Some(pid) == self.pmt_pid => self.read_pmt(&bytes),
            _ => {}
        }
    }

    fn read_pat(&mut self, s: &[u8]) {
        // Header is 8 bytes, the CRC the last 4; each program is (number, pid).
        let Some(entries) = s.get(8..s.len().saturating_sub(4)) else { return };
        for e in entries.as_chunks::<4>().0 {
            let number = u16::from_be_bytes([e[0], e[1]]);
            if number != 0 {
                self.pmt_pid = Some(u16::from(e[2] & 0x1F) << 8 | u16::from(e[3]));
                return;
            }
        }
    }

    fn read_pmt(&mut self, s: &[u8]) {
        if self.has_program() {
            return; // the table repeats in every segment; the first reading stands
        }
        let end = s.len().saturating_sub(4);
        let Some(&[_, _, hi, lo]) = s.get(8..12) else { return };
        let info_len = usize::from(hi & 0x0F) << 8 | usize::from(lo);
        let mut at = 12 + info_len;
        while at + 5 <= end {
            let stream_type = s[at];
            let pid = u16::from(s[at + 1] & 0x1F) << 8 | u16::from(s[at + 2]);
            let es_len = usize::from(s[at + 3] & 0x0F) << 8 | usize::from(s[at + 4]);
            let descriptors = s.get(at + 5..(at + 5 + es_len).min(end)).unwrap_or(&[]);
            // AC-3 and E-AC-3 in DVB style: private data with an identifying descriptor.
            let stream_type = match stream_type {
                0x06 if has_descriptor(descriptors, 0x6A) => 0x81,
                0x06 if has_descriptor(descriptors, 0x7A) => 0x87,
                t => t,
            };
            match StreamKind::from_type(stream_type) {
                Some(kind) if kind.is_video() && self.video.is_none() => self.video = Some((pid, kind)),
                Some(kind) if !kind.is_video() && self.audio.is_none() => self.audio = Some((pid, kind)),
                _ => self.other_types.push(stream_type),
            }
            at += 5 + es_len;
        }
    }
}

fn has_descriptor(mut d: &[u8], tag: u8) -> bool {
    while d.len() >= 2 {
        if d[0] == tag {
            return true;
        }
        d = d.get(2 + d[1] as usize..).unwrap_or(&[]);
    }
    false
}

/// The next offset after `from` where packets line up again (two sync bytes 188 apart).
fn resync(bytes: &[u8], from: usize) -> Option<usize> {
    (from + 1..bytes.len()).find(|&i| bytes[i] == SYNC && bytes.get(i + PACKET).is_none_or(|&b| b == SYNC))
}

fn finish(b: &mut Building, kind: StreamKind, out: &mut Vec<Pes>) {
    if b.started && !b.data.is_empty() {
        out.push(Pes { kind, pts: b.pts, data: std::mem::take(&mut b.data) });
    }
    b.data.clear();
    b.started = false;
    b.pts = None;
}

fn pes_bytes(b: &mut Building, kind: StreamKind, start: bool, payload: &[u8], out: &mut Vec<Pes>) {
    if start {
        finish(b, kind, out);
        let Some((pts, body)) = pes_header(payload) else { return };
        b.started = true;
        b.pts = pts;
        b.data.extend_from_slice(body);
    } else if b.started {
        b.data.extend_from_slice(payload);
    }
}

/// The PTS and the payload after the PES header, or `None` if this is not a PES start.
fn pes_header(p: &[u8]) -> Option<(Option<u64>, &[u8])> {
    if p.len() < 9 || p[0..3] != [0, 0, 1] {
        return None;
    }
    let flags = p[7];
    let header_len = p[8] as usize;
    let body = p.get(9 + header_len..)?;
    let pts = if flags & 0x80 != 0 { p.get(9..14).map(read_timestamp) } else { None };
    Some((pts, body))
}

fn read_timestamp(b: &[u8]) -> u64 {
    (u64::from(b[0] >> 1) & 7) << 30
        | u64::from(b[1]) << 22
        | u64::from(b[2] >> 1) << 15
        | u64::from(b[3]) << 7
        | u64::from(b[4] >> 1)
}

/// The sample rates an ADTS header (and an AudioSpecificConfig) can name by index.
const RATES: [u32; 13] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdtsConfig {
    /// MPEG-4 audio object type (2 = AAC LC).
    pub object_type: u8,
    pub rate_index: u8,
    pub rate: u32,
    pub channels: u8,
}

impl AdtsConfig {
    /// The two-byte AudioSpecificConfig that tells a decoder the same thing as the header.
    pub fn audio_specific_config(&self) -> [u8; 2] {
        [self.object_type << 3 | self.rate_index >> 1, (self.rate_index & 1) << 7 | (self.channels & 0x0F) << 3]
    }
}

/// Reads an ADTS header: the stream settings, the header length and the whole frame's length.
pub fn adts_header(b: &[u8]) -> Option<(AdtsConfig, usize, usize)> {
    if b.len() < 7 || b[0] != 0xFF || b[1] & 0xF6 != 0xF0 {
        return None;
    }
    let protection_absent = b[1] & 1 == 1;
    let object_type = (b[2] >> 6) + 1;
    let rate_index = (b[2] >> 2) & 0x0F;
    let rate = *RATES.get(rate_index as usize)?;
    let channels = (b[2] & 1) << 2 | b[3] >> 6;
    let frame_len = usize::from(b[3] & 3) << 11 | usize::from(b[4]) << 3 | usize::from(b[5] >> 5);
    let header_len = if protection_absent { 7 } else { 9 };
    if frame_len < header_len {
        return None;
    }
    Some((AdtsConfig { object_type, rate_index, rate, channels }, header_len, frame_len))
}

/// Cuts AAC data in ADTS framing into raw frames, each with its presentation time.
#[derive(Default)]
pub struct AdtsFramer {
    buf: Vec<u8>,
    /// Offset of the first unconsumed byte; the consumed front is cut away only when the
    /// buffer runs dry, so a segment of a thousand frames is not shifted once per frame.
    start: usize,
    next_pts: Option<f64>,
}

impl AdtsFramer {
    pub fn reset(&mut self) {
        self.buf.clear();
        self.start = 0;
        self.next_pts = None;
    }

    /// Adds bytes whose first whole frame starts at `pts` seconds (if known). A frame left
    /// over from the previous push keeps the time that run of frames gave it.
    pub fn push(&mut self, data: &[u8], pts: Option<f64>) {
        if self.start >= self.buf.len()
            && let Some(pts) = pts
        {
            self.next_pts = Some(pts);
        }
        self.buf.extend_from_slice(data);
    }

    /// The next complete frame as (pts, raw frame without the header, settings).
    pub fn next_frame(&mut self) -> Option<(f64, Vec<u8>, AdtsConfig)> {
        let mut at = self.start;
        loop {
            if at + 7 > self.buf.len() {
                self.buf.drain(..at.min(self.buf.len()));
                self.start = 0;
                return None;
            }
            let Some((config, header, len)) = adts_header(&self.buf[at..]) else {
                at += 1;
                continue;
            };
            if at + len > self.buf.len() {
                self.buf.drain(..at);
                self.start = 0;
                return None;
            }
            let raw = self.buf[at + header..at + len].to_vec();
            self.start = at + len;
            let pts = self.next_pts.unwrap_or(0.0);
            self.next_pts = Some(pts + 1024.0 / f64::from(config.rate));
            return Some((pts, raw, config));
        }
    }
}
