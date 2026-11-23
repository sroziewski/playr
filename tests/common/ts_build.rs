//! Builds MPEG transport stream packets by hand (ISO/IEC 13818-1) for tests of `playr::ts` and
//! of HLS segments with chosen timestamps. CRCs are written as zeros: the demuxer does not check
//! them.

pub const PACKET: usize = 188;

/// One 188-byte packet. A payload shorter than 184 bytes is padded with an adaptation field of
/// stuffing bytes (`pad_with_adaptation`) or with 0xFF after it (program tables).
pub fn packet(pid: u16, start: bool, payload: &[u8], pad_with_adaptation: bool) -> Vec<u8> {
    assert!(payload.len() <= 184, "payload of {} bytes", payload.len());
    let mut p = vec![0x47, (u8::from(start) << 6) | (pid >> 8) as u8 & 0x1F, pid as u8];
    let room = 184 - payload.len();
    if pad_with_adaptation && room > 0 {
        p.push(0x30);
        p.push((room - 1) as u8);
        if room > 1 {
            p.push(0x00);
            p.extend(std::iter::repeat_n(0xFF, room - 2));
        }
        p.extend_from_slice(payload);
    } else {
        p.push(0x10);
        p.extend_from_slice(payload);
        p.resize(PACKET, 0xFF);
    }
    assert_eq!(p.len(), PACKET);
    p
}

/// A section split over as many packets as it needs, with a pointer field in the first.
pub fn psi_packets(pid: u16, section: &[u8]) -> Vec<u8> {
    let mut payload = vec![0];
    payload.extend_from_slice(section);
    payload.chunks(184).enumerate().flat_map(|(i, chunk)| packet(pid, i == 0, chunk, false)).collect()
}

fn section(table_id: u8, id: u16, body: &[u8]) -> Vec<u8> {
    let length = 5 + body.len() + 4;
    let mut s = vec![table_id, 0xB0 | (length >> 8) as u8, length as u8, (id >> 8) as u8, id as u8, 0xC1, 0, 0];
    s.extend_from_slice(body);
    s.extend_from_slice(&[0, 0, 0, 0]);
    s
}

/// A program association table listing (program number, PMT pid).
pub fn pat(programs: &[(u16, u16)]) -> Vec<u8> {
    let body: Vec<u8> = programs.iter().flat_map(|&(n, pid)| [(n >> 8) as u8, n as u8, 0xE0 | (pid >> 8) as u8, pid as u8]).collect();
    psi_packets(0, &section(0x00, 1, &body))
}

/// A program map section with program descriptors `info` and (stream type, pid, descriptors).
pub fn pmt_section(info: &[u8], streams: &[(u8, u16, Vec<u8>)]) -> Vec<u8> {
    let mut body = vec![0xE1, 0x00, 0xF0 | (info.len() >> 8) as u8, info.len() as u8];
    body.extend_from_slice(info);
    for (stream_type, pid, descriptors) in streams {
        body.extend_from_slice(&[*stream_type, 0xE0 | (pid >> 8) as u8, *pid as u8, 0xF0 | (descriptors.len() >> 8) as u8, descriptors.len() as u8]);
        body.extend_from_slice(descriptors);
    }
    section(0x02, 1, &body)
}

pub fn pmt(pid: u16, info: &[u8], streams: &[(u8, u16, Vec<u8>)]) -> Vec<u8> {
    psi_packets(pid, &pmt_section(info, streams))
}

/// A 33-bit timestamp in the 5-byte PES form with the given 4-bit prefix.
pub fn timestamp(prefix: u8, t: u64) -> [u8; 5] {
    [
        (prefix << 4) | (((t >> 30) as u8 & 7) << 1) | 1,
        (t >> 22) as u8,
        (((t >> 15) as u8) << 1) | 1,
        (t >> 7) as u8,
        ((t as u8) << 1) | 1,
    ]
}

fn pes(pts: Option<u64>, dts: Option<u64>, data: &[u8]) -> Vec<u8> {
    let mut header = Vec::new();
    let flags = match (pts, dts) {
        (Some(p), Some(d)) => {
            header.extend_from_slice(&timestamp(3, p));
            header.extend_from_slice(&timestamp(1, d));
            0xC0
        }
        (Some(p), None) => {
            header.extend_from_slice(&timestamp(2, p));
            0x80
        }
        _ => 0x00,
    };
    let mut out = vec![0, 0, 1, 0xE0, 0, 0, 0x80, flags, header.len() as u8];
    out.extend_from_slice(&header);
    out.extend_from_slice(data);
    out
}

fn split_into_packets(pid: u16, bytes: &[u8]) -> Vec<u8> {
    bytes.chunks(184).enumerate().flat_map(|(i, chunk)| packet(pid, i == 0, chunk, true)).collect()
}

/// A PES packet (PTS only, or none) carrying `data`, split over as many packets as it needs.
pub fn pes_packets(pid: u16, pts: Option<u64>, data: &[u8]) -> Vec<u8> {
    split_into_packets(pid, &pes(pts, None, data))
}

pub fn pes_packets_with_dts(pid: u16, pts: u64, dts: u64, data: &[u8]) -> Vec<u8> {
    split_into_packets(pid, &pes(Some(pts), Some(dts), data))
}

/// A packet continuing the PES of `pid`.
pub fn continuation(pid: u16, data: &[u8]) -> Vec<u8> {
    packet(pid, false, data, true)
}

/// A packet with an adaptation field and no payload.
pub fn adaptation_only(pid: u16) -> Vec<u8> {
    let mut p = vec![0x47, (pid >> 8) as u8 & 0x1F, pid as u8, 0x20, 183, 0x00];
    p.resize(PACKET, 0xFF);
    p
}
