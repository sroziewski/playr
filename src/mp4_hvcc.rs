//! Lets symphonia's MP4 reader open HEVC files whose `hvcC` box is over its 1 KB limit.
//!
//! x265 writes its whole command line into a "user data" SEI and, by default, puts that SEI
//! into the HEVC configuration record (`hvcC`), so the box of an ordinary x265 encode is
//! 2–3 KB. symphonia-format-isomp4 0.6 refuses any `hvcC` over 1 KB ("atom size is greater than
//! 1 kb") and with it the whole file.
//!
//! The SEI is informational: decoding needs only the VPS/SPS/PPS arrays. So the file is read
//! through [`PatchedFile`], which serves a different version of each oversized box: the
//! record without its SEI arrays, then a `free` box over the bytes that freed up. The box keeps
//! its length, so no size or offset anywhere else in the file changes (rewriting `moov`
//! instead would shift `mdat` and every chunk offset with it).
//!
//! Rejected: raising the constant in a vendored copy of symphonia (a fork to maintain), and
//! rewriting the file to disk (slow for recordings of a gigabyte, and needs the space).

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use symphonia::core::io::MediaSource;

/// The largest `hvcC` body symphonia-format-isomp4 accepts (`MAX_HVCC_ATOM_SIZE`).
pub const MAX_HVCC_BODY: usize = 1024;
/// Header of a box with a 32-bit size.
const BOX_HEADER: usize = 8;
/// Bytes of a VisualSampleEntry before its child boxes: reserved(6), data reference index(2),
/// pre_defined/reserved(16), width/height(4), resolutions(8), reserved(4), frame count(2),
/// compressor name(32), depth(2), pre_defined(2).
const VISUAL_SAMPLE_ENTRY_FIELDS: usize = 78;
/// The fixed part of an HEVCDecoderConfigurationRecord, up to and including numOfArrays.
const HVCC_FIXED: usize = 23;
const NAL_PREFIX_SEI: u8 = 39;
const NAL_SUFFIX_SEI: u8 = 40;

/// Bytes served at `offset` in place of the file's own; always the same length as what they replace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Patch {
    pub offset: u64,
    pub bytes: Vec<u8>,
}

/// The patches that make every oversized `hvcC` in the file at `path` readable; empty for any
/// file that does not need them (or is not MP4 at all).
pub fn patches_for(path: &Path) -> Vec<Patch> {
    crate::demux::read_moov(path).map(|(at, moov)| patches_in(at, &moov)).unwrap_or_default()
}

/// [`patches_for`] given the `moov` body and the file offset where that body starts.
pub(crate) fn patches_in(moov_body_at: u64, moov: &[u8]) -> Vec<Patch> {
    find_hvcc(moov)
        .into_iter()
        .filter_map(|(at, size)| {
            let patched = shrink_hvcc_box(&moov[at..at + size])?;
            Some(Patch { offset: moov_body_at + at as u64, bytes: patched })
        })
        .collect()
}

/// Child boxes of `body[from..to]` as (type, offset of the box in `body`, box size). Only
/// 32-bit sizes: inside `moov` the boxes this walk descends into never need 64 bits.
fn children(body: &[u8], from: usize, to: usize) -> Vec<([u8; 4], usize, usize)> {
    let mut out = Vec::new();
    let mut pos = from;
    while pos + BOX_HEADER <= to {
        let size = u32::from_be_bytes(body[pos..pos + 4].try_into().expect("4 bytes")) as usize;
        if size < BOX_HEADER || pos + size > to {
            break;
        }
        out.push((body[pos + 4..pos + 8].try_into().expect("4 bytes"), pos, size));
        pos += size;
    }
    out
}

/// (offset, size) of every `hvcC` box at `moov/trak/mdia/minf/stbl/stsd/<entry>/hvcC`.
///
/// Only that one path is walked, a level at a time, as symphonia reads it. A walk that entered
/// these containers wherever they appear would follow a crafted `trak/trak/trak/...` chain as
/// deep as the file allows and overflow the stack.
fn find_hvcc(moov: &[u8]) -> Vec<(usize, usize)> {
    // Each level: the box to enter, and the bytes between its header and its children
    // (stsd: version/flags and entry count).
    const PATH: [(&[u8; 4], usize); 5] = [(b"trak", 0), (b"mdia", 0), (b"minf", 0), (b"stbl", 0), (b"stsd", 8)];
    // Sample entries symphonia reads an hvcC from: HEVC, and Dolby Vision over HEVC.
    const HEVC_ENTRIES: [&[u8; 4]; 4] = [b"hvc1", b"hev1", b"dvh1", b"dvhe"];

    let mut ranges = vec![(0usize, moov.len())];
    for (ty, skip) in PATH {
        ranges = ranges
            .into_iter()
            .flat_map(|(from, to)| children(moov, from, to))
            .filter(|(t, _, _)| t == ty)
            .map(|(_, at, size)| ((at + BOX_HEADER + skip).min(at + size), at + size))
            .collect();
    }
    let mut found = Vec::new();
    for (from, to) in ranges {
        for (_, at, size) in children(moov, from, to).into_iter().filter(|(t, _, _)| HEVC_ENTRIES.contains(&t)) {
            let end = at + size;
            let entry_children = (at + BOX_HEADER + VISUAL_SAMPLE_ENTRY_FIELDS).min(end);
            found.extend(
                children(moov, entry_children, end).into_iter().filter(|(t, _, _)| t == b"hvcC").map(|(_, at, size)| (at, size)),
            );
        }
    }
    found
}

/// `hvcc` (a whole box, header included) rebuilt without SEI arrays and followed by a `free`
/// box to its original length. `None` when the box is within the limit already, cannot be
/// parsed, or is still too large (or saves too little to fit a `free` header) without its SEI.
pub fn shrink_hvcc_box(hvcc: &[u8]) -> Option<Vec<u8>> {
    if hvcc.len() < BOX_HEADER || hvcc.len() - BOX_HEADER <= MAX_HVCC_BODY {
        return None;
    }
    let record = &hvcc[BOX_HEADER..];
    let fixed = record.get(..HVCC_FIXED)?;
    let arrays = fixed[HVCC_FIXED - 1] as usize;
    let mut kept_arrays = 0u8;
    let mut kept = Vec::new();
    let mut pos = HVCC_FIXED;
    for _ in 0..arrays {
        let start = pos;
        let nal_type = record.get(pos)? & 0x3f;
        let count = u16::from_be_bytes(record.get(pos + 1..pos + 3)?.try_into().ok()?) as usize;
        pos += 3;
        for _ in 0..count {
            let len = u16::from_be_bytes(record.get(pos..pos + 2)?.try_into().ok()?) as usize;
            pos = pos.checked_add(2 + len)?;
            if pos > record.len() {
                return None;
            }
        }
        if nal_type != NAL_PREFIX_SEI && nal_type != NAL_SUFFIX_SEI {
            kept.extend_from_slice(&record[start..pos]);
            kept_arrays += 1;
        }
    }
    // Bytes after the last array (none in a well-formed record) are kept as they are.
    kept.extend_from_slice(&record[pos..]);

    let body_len = HVCC_FIXED + kept.len();
    let free_len = hvcc.len() - BOX_HEADER - body_len;
    if body_len > MAX_HVCC_BODY || free_len < BOX_HEADER {
        return None;
    }
    let mut out = Vec::with_capacity(hvcc.len());
    out.extend_from_slice(&((BOX_HEADER + body_len) as u32).to_be_bytes());
    out.extend_from_slice(b"hvcC");
    out.extend_from_slice(&fixed[..HVCC_FIXED - 1]);
    out.push(kept_arrays);
    out.extend_from_slice(&kept);
    out.extend_from_slice(&(free_len as u32).to_be_bytes());
    out.extend_from_slice(b"free");
    out.resize(hvcc.len(), 0);
    Some(out)
}

/// A file read with some byte ranges replaced (see [`Patch`]).
pub struct PatchedFile {
    file: File,
    len: u64,
    pos: u64,
    patches: Vec<Patch>,
}

impl PatchedFile {
    pub fn new(file: File, patches: Vec<Patch>) -> io::Result<Self> {
        let len = file.metadata()?.len();
        Ok(Self { file, len, pos: 0, patches })
    }
}

impl Read for PatchedFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // The OS position can differ from `pos` after a failed read; seek explicitly.
        self.file.seek(SeekFrom::Start(self.pos))?;
        let n = self.file.read(buf)?;
        let (start, end) = (self.pos, self.pos + n as u64);
        for p in &self.patches {
            let (p_start, p_end) = (p.offset, p.offset + p.bytes.len() as u64);
            if p_end <= start || p_start >= end {
                continue;
            }
            let from = p_start.max(start);
            let to = p_end.min(end);
            buf[(from - start) as usize..(to - start) as usize]
                .copy_from_slice(&p.bytes[(from - p_start) as usize..(to - p_start) as usize]);
        }
        self.pos = end;
        Ok(n)
    }
}

impl Seek for PatchedFile {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let target = match to {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::End(d) => self.len.checked_add_signed(d),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        };
        self.pos = target.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek before the start of the file"))?;
        Ok(self.pos)
    }
}

impl MediaSource for PatchedFile {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.len)
    }
}
