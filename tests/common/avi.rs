//! Builds small AVI (RIFF) files byte by byte, for layouts FFmpeg does not write on demand.

pub fn chunk(id: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut out = id.to_vec();
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(data);
    if data.len() % 2 == 1 {
        out.push(0);
    }
    out
}

pub fn list(kind: &[u8; 4], children: &[u8]) -> Vec<u8> {
    chunk(b"LIST", &[kind, children].concat())
}

/// AVISTREAMHEADER (56 bytes).
pub fn strh(kind: &[u8; 4], handler: &[u8; 4], scale: u32, rate: u32, sample_size: u32) -> Vec<u8> {
    let mut h = vec![0u8; 56];
    h[0..4].copy_from_slice(kind);
    h[4..8].copy_from_slice(handler);
    h[20..24].copy_from_slice(&scale.to_le_bytes());
    h[24..28].copy_from_slice(&rate.to_le_bytes());
    h[44..48].copy_from_slice(&sample_size.to_le_bytes());
    h
}

/// BITMAPINFOHEADER (40 bytes). A negative height means a top-down picture.
pub fn bitmap_info(width: i32, height: i32, compression: &[u8; 4]) -> Vec<u8> {
    let mut b = vec![0u8; 40];
    b[0..4].copy_from_slice(&40u32.to_le_bytes());
    b[4..8].copy_from_slice(&width.to_le_bytes());
    b[8..12].copy_from_slice(&height.to_le_bytes());
    b[12..14].copy_from_slice(&1u16.to_le_bytes());
    b[14..16].copy_from_slice(&24u16.to_le_bytes());
    b[16..20].copy_from_slice(compression);
    b
}

/// WAVEFORMATEX (18 bytes, no extra data).
pub fn wave_format(tag: u16, channels: u16, rate: u32, avg_bytes: u32, block_align: u16, bits: u16) -> Vec<u8> {
    let mut w = Vec::with_capacity(18);
    w.extend_from_slice(&tag.to_le_bytes());
    w.extend_from_slice(&channels.to_le_bytes());
    w.extend_from_slice(&rate.to_le_bytes());
    w.extend_from_slice(&avg_bytes.to_le_bytes());
    w.extend_from_slice(&block_align.to_le_bytes());
    w.extend_from_slice(&bits.to_le_bytes());
    w.extend_from_slice(&0u16.to_le_bytes());
    w
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Index {
    /// `idx1` with offsets relative to the `movi` fourcc (what most writers produce).
    Relative,
    /// `idx1` with offsets from the start of the file.
    Absolute,
    /// No `idx1`: the reader has to walk the `movi` list.
    Missing,
    /// No `idx1`, and the chunks sit in `LIST rec ` groups of two.
    MissingWithRecLists,
}

#[derive(Clone, Default)]
pub struct Avi {
    /// (strh, strf) per stream, in stream-number order.
    pub streams: Vec<(Vec<u8>, Vec<u8>)>,
    /// (chunk id, payload, keyframe flag for the index) in file order.
    pub chunks: Vec<([u8; 4], Vec<u8>, bool)>,
}

impl Avi {
    pub fn header_list(&self) -> Vec<u8> {
        let mut hdrl = chunk(b"avih", &[0u8; 56]);
        for (strh, strf) in &self.streams {
            hdrl.extend(list(b"strl", &[chunk(b"strh", strh), chunk(b"strf", strf)].concat()));
        }
        list(b"hdrl", &hdrl)
    }

    pub fn build(&self, index: Index) -> Vec<u8> {
        let hdrl = self.header_list();
        // Position of the 'movi' fourcc: RIFF header, hdrl list, then "LIST" and its size.
        let movi_fourcc = 12 + hdrl.len() + 8;
        let mut body = Vec::new();
        let mut idx1 = Vec::new();
        let mut group = Vec::new();
        for (n, (id, data, key)) in self.chunks.iter().enumerate() {
            let relative = 4 + body.len() + group.len();
            let offset = if index == Index::Absolute { movi_fourcc + relative } else { relative };
            idx1.extend_from_slice(id);
            idx1.extend_from_slice(&(if *key { 0x10u32 } else { 0 }).to_le_bytes());
            idx1.extend_from_slice(&(offset as u32).to_le_bytes());
            idx1.extend_from_slice(&(data.len() as u32).to_le_bytes());
            if index == Index::MissingWithRecLists {
                group.extend(chunk(id, data));
                if n % 2 == 1 || n + 1 == self.chunks.len() {
                    body.extend(list(b"rec ", &group));
                    group.clear();
                }
            } else {
                body.extend(chunk(id, data));
            }
        }
        let mut payload = b"AVI ".to_vec();
        payload.extend(hdrl);
        payload.extend(list(b"movi", &body));
        if matches!(index, Index::Relative | Index::Absolute) {
            payload.extend(chunk(b"idx1", &idx1));
        }
        chunk(b"RIFF", &payload)
    }
}
