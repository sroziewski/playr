//! Video decoders behind one small trait, all pure Rust.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};

use crate::media::{VideoCodec, VideoFrame, VideoTrack};

pub trait VideoDecoder {
    /// Feeds one container packet; returns the pictures that became displayable, in display
    /// order. Bitstream errors are counted, never propagated: one bad packet must not stop
    /// playback.
    fn decode(&mut self, data: &[u8], pts: f64) -> Vec<VideoFrame>;
    fn flush(&mut self) -> Vec<VideoFrame>;
    fn errors(&self) -> u64;
    fn last_error(&self) -> Option<&str>;
}

pub fn make_decoder(track: &VideoTrack) -> Result<Box<dyn VideoDecoder>, String> {
    match &track.codec {
        VideoCodec::H264 => Ok(Box::new(H264Dec::new(track)?)),
        VideoCodec::Hevc => Ok(Box::new(HevcDec::new(track)?)),
        VideoCodec::Mpeg4 => Ok(Box::new(Mpeg4Dec::new(track))),
        VideoCodec::Unsupported(name) => Err(format!("no decoder for {name} video")),
    }
}

fn worker_threads() -> usize {
    let n = std::thread::available_parallelism().map_or(4, |n| n.get());
    // Leave headroom for the UI, the audio callback and the demuxer.
    (n * 3 / 4).max(1)
}

/// Whether decoding can start at this packet.
pub fn is_keyframe(track: &VideoTrack, data: &[u8]) -> bool {
    match track.codec {
        VideoCodec::H264 => match avcc_length_size(&track.extra) {
            Some(n) => avcc_nals(data, n).any(h264_nal_starts_decoding),
            None => annexb_nals(data).iter().any(|nal| h264_nal_starts_decoding(nal)),
        },
        VideoCodec::Hevc => {
            let irap = |nal: &[u8]| nal.first().is_some_and(|b| (16..=23).contains(&((b >> 1) & 0x3f)));
            match hvcc_length_size(&track.extra) {
                Some(n) => avcc_nals(data, n).any(irap),
                None => annexb_nals(data).iter().any(|nal| irap(nal)),
            }
        }
        VideoCodec::Mpeg4 => mpeg4_is_intra(data),
        VideoCodec::Unsupported(_) => true,
    }
}

/// An IDR slice, or a recovery-point SEI: encoders using open GOPs mark their random-access
/// I pictures with the latter instead of emitting IDR pictures.
fn h264_nal_starts_decoding(nal: &[u8]) -> bool {
    match nal.first().map(|b| b & 0x1f) {
        Some(5) => true,
        Some(6) => sei_has_recovery_point(&nal[1..]),
        _ => false,
    }
}

/// Walks the messages of an H.264 SEI NAL body looking for payload type 6 (recovery point).
fn sei_has_recovery_point(mut body: &[u8]) -> bool {
    // Each message is: type and size, each coded as a run of 0xFF bytes plus a final byte.
    let read_varint = |body: &mut &[u8]| -> Option<usize> {
        let mut value = 0usize;
        loop {
            let (&byte, rest) = body.split_first()?;
            *body = rest;
            value += byte as usize;
            if byte != 0xFF {
                return Some(value);
            }
        }
    };
    while body.len() > 2 {
        let Some(kind) = read_varint(&mut body) else { return false };
        let Some(size) = read_varint(&mut body) else { return false };
        if kind == 6 {
            return true;
        }
        if size > body.len() {
            return false;
        }
        body = &body[size..];
    }
    false
}

/// True when the first VOP in `data` is intra-coded.
pub fn mpeg4_is_intra(data: &[u8]) -> bool {
    data.windows(5)
        .find(|w| w[..4] == [0, 0, 1, 0xB6])
        .is_some_and(|w| w[4] >> 6 == 0)
}

fn avcc_length_size(extra: &[u8]) -> Option<usize> {
    (extra.len() >= 7 && extra[0] == 1).then(|| (extra[4] & 3) as usize + 1)
}

fn hvcc_length_size(extra: &[u8]) -> Option<usize> {
    (extra.len() >= 23).then(|| (extra[21] & 3) as usize + 1)
}

/// Iterates the NAL units of a length-prefixed (AVCC / HVCC) sample. Stops at the first
/// length that runs past the buffer instead of panicking on truncated data.
pub fn avcc_nals(data: &[u8], length_size: usize) -> impl Iterator<Item = &[u8]> {
    let mut pos = 0usize;
    std::iter::from_fn(move || {
        if length_size == 0 || length_size > 4 || pos + length_size > data.len() {
            return None;
        }
        let len = data[pos..pos + length_size].iter().fold(0usize, |a, &b| a << 8 | b as usize);
        let start = pos + length_size;
        let end = start.checked_add(len).filter(|&e| e <= data.len())?;
        pos = end;
        Some(&data[start..end])
    })
}

/// Splits an Annex B buffer at `00 00 01` start codes.
pub fn annexb_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (n, &s) in starts.iter().enumerate() {
        let mut e = starts.get(n + 1).map_or(data.len(), |next| next - 3);
        // A four-byte start code leaves its leading zero on the previous NAL.
        while e > s && data[e - 1] == 0 {
            e -= 1;
        }
        if e > s {
            out.push(&data[s..e]);
        }
    }
    out
}

/// Pairs container timestamps with frames that a decoder returns in display order.
///
/// The threaded decoders reorder B-frames internally and expose no per-frame metadata, but
/// display order is ascending-PTS order, so the smallest outstanding timestamp always belongs
/// to the next frame out.
///
/// That pairing drifts when the decoder drops a picture and its timestamp stays behind. A
/// picture the decoder rejects outright is taken back out with [`discard`](Self::discard);
/// for anything the wrapper cannot see, the queue re-anchors at every IDR picture, where
/// picture order counts restart. IDR markers are matched oldest first: that is exact for
/// every undamaged stream, at the price of staying one group behind after a group of
/// pictures vanishes without an error. (Guessing from decoder latency instead was tried and
/// mis-timed clean streams, because latency varies with how fast packets are fed.)
///
/// One more limit, for damaged H.264 only: the threaded decoder reports a worker's
/// macroblock error from whichever call comes next, and the error type does not say so. A
/// packet blamed for its predecessor's damage is then discarded although its picture still
/// comes out, leaving the rest of that group one frame late until the next IDR.
pub struct PtsQueue {
    heap: BinaryHeap<Reverse<i64>>,
    cap: usize,
    last: f64,
    step: f64,
    /// Timestamps of the IDR pictures that have been fed and not yet seen coming out.
    idr: VecDeque<i64>,
    last_poc: Option<i32>,
}

/// Far above what any decoder holds in flight; only a backstop against unbounded growth.
const PTS_QUEUE_CAP: usize = 1024;

impl PtsQueue {
    pub fn new(cap: usize) -> Self {
        Self { heap: BinaryHeap::new(), cap: cap.max(1), last: 0.0, step: 1.0 / 30.0, idr: VecDeque::new(), last_poc: None }
    }

    fn micros(pts: f64) -> i64 {
        (pts * 1e6).round() as i64
    }

    pub fn push(&mut self, pts: f64) {
        self.heap.push(Reverse(Self::micros(pts)));
        while self.heap.len() > self.cap {
            self.heap.pop();
        }
    }

    /// [`push`](Self::push) for a packet that holds an IDR picture.
    pub fn push_idr(&mut self, pts: f64) {
        self.idr.push_back(Self::micros(pts));
        self.push(pts);
    }

    /// Takes back the timestamp of a packet the decoder rejected, so that no frame is ever
    /// paired with it.
    pub fn discard(&mut self, pts: f64) {
        let pts = Self::micros(pts);
        let mut removed = false;
        self.heap.retain(|Reverse(queued)| {
            let drop = !removed && *queued == pts;
            removed |= drop;
            !drop
        });
        if let Some(at) = self.idr.iter().position(|&marker| marker == pts) {
            self.idr.remove(at);
        }
    }

    /// Next timestamp in display order. If the queue is empty (more frames than timestamps)
    /// it extrapolates from the previous one.
    pub fn pop(&mut self) -> f64 {
        match self.heap.pop() {
            Some(Reverse(v)) => self.last = v as f64 / 1e6,
            None => self.last += self.step,
        }
        self.last
    }

    /// [`pop`](Self::pop) for a frame whose picture order count is `poc`.
    ///
    /// A count that does not increase marks an IDR frame coming out. That frame takes the
    /// timestamp of the oldest outstanding IDR packet outright and everything older is
    /// discarded, which undoes drift in either direction: timestamps left behind by dropped
    /// pictures, and timestamps used up by frames the queue had none for.
    pub fn pop_poc(&mut self, poc: i32) -> f64 {
        let first = self.last_poc.is_none();
        let restarted = self.last_poc.is_none_or(|last| poc <= last);
        self.last_poc = Some(poc);
        let Some(&start) = self.idr.front().filter(|_| restarted) else { return self.pop() };
        // A stream joined mid-sequence produces frames before its first IDR; those must not
        // consume the marker.
        if first && self.heap.peek().is_none_or(|Reverse(min)| *min != start) {
            return self.pop();
        }
        self.idr.pop_front();
        while self.heap.peek().is_some_and(|Reverse(pts)| *pts <= start) {
            self.heap.pop();
        }
        self.last = start as f64 / 1e6;
        self.last
    }

    pub fn len(&self) -> usize {
        self.heap.len()
    }

    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }
}
/// Reduces a 16-bit little-endian plane to 8 bits; 8-bit planes pass through untouched.
fn plane_to_8bit(src: Vec<u8>, bit_depth: u8) -> Vec<u8> {
    if bit_depth <= 8 {
        return src;
    }
    let shift = bit_depth - 8;
    src.as_chunks::<2>().0.iter().map(|c| (u16::from_le_bytes([c[0], c[1]]) >> shift) as u8).collect()
}

// ---------------------------------------------------------------------------------------------

struct H264Dec {
    dec: rust_h264::threading::ThreadedDecoder,
    length_size: Option<usize>,
    pts: PtsQueue,
    errors: u64,
    last_error: Option<String>,
}

impl H264Dec {
    fn new(track: &VideoTrack) -> Result<Self, String> {
        let threads = worker_threads();
        let mut dec = rust_h264::threading::ThreadedDecoder::new(threads);
        let mut length_size = None;
        if !track.extra.is_empty() {
            let cfg = rust_h264::nal::parse_avcc_config(&track.extra)
                .map_err(|e| format!("bad H.264 configuration record: {e}"))?;
            for nal in cfg.sps_nals.iter().chain(cfg.pps_nals.iter()) {
                dec.decode_nal(nal).map_err(|e| format!("bad H.264 parameter set: {e:?}"))?;
            }
            length_size = Some(cfg.length_size);
        }
        Ok(Self { dec, length_size, pts: PtsQueue::new(PTS_QUEUE_CAP), errors: 0, last_error: None })
    }

    fn convert(&mut self, f: rust_h264::decoder::Frame) -> VideoFrame {
        let (cw, ch) = (f.chroma_width(), f.chroma_height());
        let depth = f.bit_depth;
        VideoFrame {
            pts: self.pts.pop_poc(f.pic_order_cnt),
            width: f.width,
            height: f.height,
            chroma_width: cw,
            chroma_height: ch,
            y_stride: f.width,
            c_stride: cw,
            y: plane_to_8bit(f.y, depth),
            u: plane_to_8bit(f.u, depth),
            v: plane_to_8bit(f.v, depth),
        }
    }
}

impl VideoDecoder for H264Dec {
    fn decode(&mut self, data: &[u8], pts: f64) -> Vec<VideoFrame> {
        let nals = match self.length_size {
            Some(n) => rust_h264::nal::parse_avcc(data, n),
            None => rust_h264::nal::parse_annex_b(data),
        };
        let types: Vec<u8> = match self.length_size {
            Some(n) => avcc_nals(data, n).filter_map(|nal| nal.first().map(|b| b & 0x1f)).collect(),
            None => annexb_nals(data).iter().filter_map(|nal| nal.first().map(|b| b & 0x1f)).collect(),
        };
        if types.contains(&5) {
            self.pts.push_idr(pts);
        } else if types.iter().any(|t| matches!(t, 1..=4)) {
            self.pts.push(pts);
        }
        let mut out = Vec::new();
        let (mut accepted, mut rejected) = (0, 0);
        for nal in &nals {
            use rust_h264::nal::NalUnitType;
            let is_slice = matches!(nal.nal_unit_type, NalUnitType::SliceIdr | NalUnitType::Slice);
            match self.dec.decode_nal(nal) {
                Ok(frames) => {
                    accepted += is_slice as u32;
                    for f in frames {
                        let vf = self.convert(f);
                        out.push(vf);
                    }
                }
                Err(e) => {
                    rejected += is_slice as u32;
                    self.errors += 1;
                    self.last_error = Some(format!("{e:?}"));
                }
            }
        }
        // Slice-header errors are reported at once and mean the picture was never started, so
        // no frame will come out for this packet's timestamp.
        if accepted == 0 && rejected > 0 {
            self.pts.discard(pts);
        }
        out
    }

    fn flush(&mut self) -> Vec<VideoFrame> {
        let frames = self.dec.flush();
        frames.into_iter().map(|f| self.convert(f)).collect()
    }

    fn errors(&self) -> u64 {
        self.errors
    }

    fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }
}

// ---------------------------------------------------------------------------------------------

struct HevcDec {
    dec: rust_h265::decoder::Decoder,
    length_size: Option<usize>,
    pts: PtsQueue,
    errors: u64,
    last_error: Option<String>,
}

/// Parameter-set NAL units of an `hvcC` record (ISO/IEC 14496-15 §8.3.3.1).
pub fn hvcc_parameter_sets(extra: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    if extra.len() < 23 {
        return out;
    }
    let arrays = extra[22];
    let mut pos = 23;
    for _ in 0..arrays {
        if pos + 3 > extra.len() {
            break;
        }
        let count = u16::from_be_bytes([extra[pos + 1], extra[pos + 2]]);
        pos += 3;
        for _ in 0..count {
            if pos + 2 > extra.len() {
                return out;
            }
            let len = u16::from_be_bytes([extra[pos], extra[pos + 1]]) as usize;
            pos += 2;
            if pos + len > extra.len() {
                return out;
            }
            out.push(&extra[pos..pos + len]);
            pos += len;
        }
    }
    out
}

impl HevcDec {
    fn new(track: &VideoTrack) -> Result<Self, String> {
        let threads = worker_threads();
        let mut dec = rust_h265::decoder::Decoder::new().with_display_order().with_threads(threads);
        let length_size = hvcc_length_size(&track.extra);
        for ps in hvcc_parameter_sets(&track.extra) {
            if let Some(nal) = rust_h265::nal::parse_nal(ps) {
                dec.decode_nal(&nal).map_err(|e| format!("bad HEVC parameter set: {e:?}"))?;
            }
        }
        Ok(Self { dec, length_size, pts: PtsQueue::new(PTS_QUEUE_CAP), errors: 0, last_error: None })
    }

    fn convert(&mut self, f: rust_h265::decoder::Frame) -> Option<VideoFrame> {
        let shift = f.bit_depth.saturating_sub(8);
        macro_rules! plane {
            ($p:expr) => {
                match $p.as_u8() {
                    Some(b) => b.to_vec(),
                    None => $p.as_u16()?.iter().map(|&s| (s >> shift) as u8).collect::<Vec<u8>>(),
                }
            };
        }
        let (y, u, v) = (plane!(f.y), plane!(f.u), plane!(f.v));
        let (w, h) = (f.width, f.height);
        let (cw, ch) = if f.chroma_format_idc == 3 { (w, h) } else { (w.div_ceil(2), h.div_ceil(2)) };
        // Trust the plane sizes over the arithmetic: a decoder that pads rows would otherwise
        // shear the picture.
        let y_stride = if h > 0 { (y.len() as u32 / h).max(w) } else { w };
        let c_stride = if ch > 0 { (u.len() as u32 / ch).max(cw) } else { cw };
        Some(VideoFrame {
            pts: self.pts.pop_poc(f.pic_order_cnt),
            width: w,
            height: h,
            chroma_width: cw,
            chroma_height: ch,
            y,
            u,
            v,
            y_stride,
            c_stride,
        })
    }

    fn drain(&mut self, first: Option<rust_h265::decoder::Frame>, out: &mut Vec<VideoFrame>) {
        if let Some(f) = first {
            out.extend(self.convert(f));
        }
        while let Some(f) = self.dec.next_output() {
            out.extend(self.convert(f));
        }
        while let Some(e) = self.dec.next_error() {
            self.errors += 1;
            self.last_error = Some(format!("{e:?}"));
        }
    }
}

impl VideoDecoder for HevcDec {
    fn decode(&mut self, data: &[u8], pts: f64) -> Vec<VideoFrame> {
        let nals = match self.length_size {
            Some(n) => rust_h265::nal::parse_hvcc(data, n as u8),
            None => rust_h265::nal::parse_annex_b(data),
        };
        // VCL NAL unit types are 0..=31.
        let types: Vec<u8> = match self.length_size {
            Some(n) => avcc_nals(data, n).filter_map(|nal| nal.first().map(|b| (b >> 1) & 0x3f)).collect(),
            None => annexb_nals(data).iter().filter_map(|nal| nal.first().map(|b| (b >> 1) & 0x3f)).collect(),
        };
        // Types 19 and 20 are IDR pictures; every type below 32 carries picture data.
        if types.iter().any(|t| matches!(t, 19 | 20)) {
            self.pts.push_idr(pts);
        } else if types.iter().any(|&t| t < 32) {
            self.pts.push(pts);
        }
        let mut out = Vec::new();
        let (mut accepted, mut rejected) = (0, 0);
        for nal in &nals {
            let is_slice = nal.nal_unit_type.is_vcl();
            match self.dec.decode_nal(nal) {
                Ok(first) => {
                    accepted += is_slice as u32;
                    self.drain(first, &mut out);
                }
                Err(e) => {
                    rejected += is_slice as u32;
                    self.errors += 1;
                    self.last_error = Some(format!("{e:?}"));
                }
            }
        }
        if accepted == 0 && rejected > 0 {
            self.pts.discard(pts);
        }
        out
    }

    fn flush(&mut self) -> Vec<VideoFrame> {
        let mut out = Vec::new();
        while let Some(f) = self.dec.flush() {
            out.extend(self.convert(f));
        }
        self.drain(None, &mut out);
        out
    }

    fn errors(&self) -> u64 {
        self.errors
    }

    fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }
}

// ---------------------------------------------------------------------------------------------

struct Mpeg4Dec {
    dec: oxideav_mpeg4video::Mpeg4VideoDecoder,
    width: u32,
    height: u32,
    last_pts: f64,
    errors: u64,
    last_error: Option<String>,
}

impl Mpeg4Dec {
    fn new(track: &VideoTrack) -> Self {
        // Xvid/DivX/FFmpeg streams follow de-facto behaviour where it differs from the spec text.
        let mut dec = oxideav_mpeg4video::Mpeg4VideoDecoder::with_options(
            oxideav_mpeg4video::DecodeOptions::ecosystem(),
        );
        let mut errors = 0;
        if !track.extra.is_empty() && dec.decode(&track.extra).is_err() {
            errors = 1;
        }
        Self { dec, width: track.width, height: track.height, last_pts: 0.0, errors, last_error: None }
    }

    fn convert(&mut self, f: oxideav_mpeg4video::DecodedFrame) -> VideoFrame {
        let (pw, ph) = (f.width() as u32, f.height() as u32);
        // The decoder returns macroblock-padded planes; the container knows the real size.
        let w = if self.width > 0 { self.width.min(pw) } else { pw };
        let h = if self.height > 0 { self.height.min(ph) } else { ph };
        let pts = match f.pts() {
            Some(us) => us as f64 / 1e6,
            None => self.last_pts + 1.0 / 25.0,
        };
        self.last_pts = pts;
        VideoFrame {
            pts,
            width: w,
            height: h,
            chroma_width: w.div_ceil(2),
            chroma_height: h.div_ceil(2),
            y: f.luma_samples().to_vec(),
            u: f.cb_samples().to_vec(),
            v: f.cr_samples().to_vec(),
            y_stride: pw,
            c_stride: pw / 2,
        }
    }
}

impl VideoDecoder for Mpeg4Dec {
    fn decode(&mut self, data: &[u8], pts: f64) -> Vec<VideoFrame> {
        match self.dec.decode_with_pts(data, Some((pts * 1e6).round() as i64)) {
            Ok(frames) => frames.into_iter().map(|f| self.convert(f)).collect(),
            Err(e) => {
                self.errors += 1;
                self.last_error = Some(e.to_string());
                Vec::new()
            }
        }
    }

    fn flush(&mut self) -> Vec<VideoFrame> {
        let frames = self.dec.flush();
        frames.into_iter().map(|f| self.convert(f)).collect()
    }

    fn errors(&self) -> u64 {
        self.errors
    }

    fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }
}
