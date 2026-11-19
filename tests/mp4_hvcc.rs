//! `playr::mp4_hvcc`: an `hvcC` box over symphonia's 1024-byte limit is served without its SEI
//! arrays, followed by a `free` box to its original length, through `PatchedFile`.
//!
//! Expected bytes follow ISO/IEC 14496-15 (HEVCDecoderConfigurationRecord: 22 bytes of fixed
//! fields, numOfArrays, then per array one byte of completeness/reserved/NAL type, a 16-bit NAL
//! count and per NAL a 16-bit length) and ISO/IEC 14496-12 (box = 32-bit size, 4-letter type).

mod common;

use std::io::{ErrorKind, Read, Seek, SeekFrom};

use common::*;
use playr::mp4_hvcc::{MAX_HVCC_BODY, Patch, PatchedFile, patches_for, shrink_hvcc_box};
use symphonia::core::io::MediaSource;

fn mp4_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    out
}

/// One array of the record: `type_byte` is the whole first byte (completeness, reserved, type).
fn nal_array(type_byte: u8, nals: &[Vec<u8>]) -> Vec<u8> {
    let mut out = vec![type_byte];
    out.extend_from_slice(&(nals.len() as u16).to_be_bytes());
    for nal in nals {
        out.extend_from_slice(&(nal.len() as u16).to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

/// A NAL of `len` bytes whose content starts at `seed`, so two NALs never look alike.
fn nal(seed: u8, len: usize) -> Vec<u8> {
    (0..len).map(|i| seed.wrapping_add(i as u8)).collect()
}

/// The 22 bytes before numOfArrays, each different, so a shifted copy shows.
fn fixed_fields() -> Vec<u8> {
    (1..=22).collect()
}

/// An `hvcC` box: fixed fields, numOfArrays = `arrays.len()` (unless `declared` says otherwise),
/// the arrays and `trailing`.
fn hvcc_box_declaring(declared: u8, arrays: &[Vec<u8>], trailing: &[u8]) -> Vec<u8> {
    let mut body = fixed_fields();
    body.push(declared);
    for a in arrays {
        body.extend_from_slice(a);
    }
    body.extend_from_slice(trailing);
    mp4_box(b"hvcC", &body)
}

fn hvcc_box(arrays: &[Vec<u8>], trailing: &[u8]) -> Vec<u8> {
    hvcc_box_declaring(arrays.len() as u8, arrays, trailing)
}

/// What the contract says the shrunk box is: an `hvcC` holding only `kept` (and `trailing`),
/// then a `free` box whose size field covers the rest of `original_len`, its body zeros.
fn expected_shrunk(original_len: usize, kept: &[Vec<u8>], trailing: &[u8]) -> Vec<u8> {
    let mut out = hvcc_box(kept, trailing);
    let free = original_len - out.len();
    out.extend_from_slice(&(free as u32).to_be_bytes());
    out.extend_from_slice(b"free");
    out.resize(original_len, 0);
    out
}

fn be32(bytes: &[u8], at: usize) -> usize {
    u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()) as usize
}

const VPS: u8 = 0xa0; // completeness 1, type 32
const SPS: u8 = 0xa1; // type 33
const PPS: u8 = 0xa2; // type 34
const PREFIX_SEI: u8 = 0x27; // completeness 0, type 39
const SUFFIX_SEI: u8 = 0x28; // type 40

mod shrink {
    use super::*;

    #[test]
    fn drops_the_prefix_sei_and_fills_the_freed_bytes_with_a_free_box() {
        // VPS(24), SPS(42), PPS(7) arrays: 29 + 47 + 12 = 88 bytes; the SEI array 3 + 2 + 1100.
        let (vps, sps, pps) = (nal_array(VPS, &[nal(10, 24)]), nal_array(SPS, &[nal(50, 42)]), nal_array(PPS, &[nal(110, 7)]));
        let original = hvcc_box(&[vps.clone(), sps.clone(), pps.clone(), nal_array(PREFIX_SEI, &[nal(150, 1100)])], &[]);
        assert_eq!(original.len(), 8 + 23 + 88 + 1105, "test precondition");

        let out = shrink_hvcc_box(&original).expect("the SEI is dropped");

        assert_eq!(out.len(), 1224, "the box keeps its length");
        assert_eq!(be32(&out, 0), 8 + 23 + 88, "hvcC size field");
        assert_eq!(&out[4..8], b"hvcC");
        assert_eq!(&out[8..30], &fixed_fields()[..], "fixed fields copied unchanged");
        assert_eq!(out[30], 3, "numOfArrays without the SEI");
        assert_eq!(be32(&out, 119), 1224 - 119, "free size field");
        assert_eq!(&out[123..127], b"free");
        assert_eq!(out, expected_shrunk(1224, &[vps, sps, pps], &[]));
    }

    #[test]
    fn drops_only_sei_arrays_and_keeps_the_rest_in_order() {
        let vps = nal_array(VPS, &[nal(10, 24)]);
        let sps = nal_array(SPS, &[nal(50, 42)]);
        let pps = nal_array(PPS, &[nal(110, 7)]);
        // Types 38 and 41, either side of 39/40, with the completeness bit set.
        let (t38, t41) = (nal_array(0x80 | 38, &[nal(3, 30)]), nal_array(0x80 | 41, &[nal(70, 20)]));
        let trailing = vec![0xde, 0xad, 0xbe];
        // (case, arrays in the box, bytes after them, arrays expected to stay)
        let rows: [(&str, Vec<Vec<u8>>, Vec<u8>, Vec<Vec<u8>>); 5] = [
            (
                "arrays of no NALs: an empty SEI array goes, an empty VPS array stays",
                vec![nal_array(VPS, &[]), nal_array(PREFIX_SEI, &[]), sps.clone(), nal_array(SUFFIX_SEI, &[nal(150, 1100)])],
                vec![],
                vec![nal_array(VPS, &[]), sps.clone()],
            ),
            (
                "a suffix and a prefix SEI between the other arrays",
                vec![vps.clone(), nal_array(SUFFIX_SEI, &[nal(200, 600)]), sps.clone(), nal_array(PREFIX_SEI, &[nal(150, 500)]), pps.clone()],
                vec![],
                vec![vps.clone(), sps.clone(), pps.clone()],
            ),
            (
                "an SEI array of three NALs",
                vec![nal_array(PREFIX_SEI, &[nal(1, 300), nal(90, 400), nal(170, 500)]), sps.clone()],
                vec![],
                vec![sps.clone()],
            ),
            (
                "an SEI with completeness and reserved bits set (0xc0 | 39) between types 38 and 41",
                vec![t38.clone(), nal_array(0xc0 | 39, &[nal(150, 1100)]), t41.clone()],
                vec![],
                vec![t38, t41],
            ),
            (
                "bytes after the last array",
                vec![sps.clone(), nal_array(PREFIX_SEI, &[nal(150, 1100)])],
                trailing.clone(),
                vec![sps.clone()],
            ),
        ];
        let mut bad = Vec::new();
        for (case, arrays, after, kept) in rows {
            let original = hvcc_box(&arrays, &after);
            let got = shrink_hvcc_box(&original);
            let want = expected_shrunk(original.len(), &kept, &after);
            expect!(bad, got.as_ref() == Some(&want), "{case}: got {:?}, expected {:?}", got.map(|o| o[..40].to_vec()), &want[..40]);
        }
        finish(bad);
    }

    /// A record of `body_len` bytes: an SPS array padded so that, with an SEI array holding one
    /// NAL of `sei_nal` bytes (5 + `sei_nal` bytes in all), the body comes out at `body_len`.
    fn record_of(body_len: usize, sei_nal: usize) -> (Vec<u8>, Vec<u8>) {
        let sps_nal = body_len - 23 - 5 - (5 + sei_nal);
        let sps = nal_array(SPS, &[nal(50, sps_nal)]);
        let original = hvcc_box(&[sps.clone(), nal_array(PREFIX_SEI, &[nal(150, sei_nal)])], &[]);
        assert_eq!(original.len() - 8, body_len, "test precondition");
        (original, sps)
    }

    #[test]
    fn size_and_saving_limits() {
        // (case, body length, SEI NAL length, shrinks); kept body = body - (5 + SEI NAL).
        let max = MAX_HVCC_BODY;
        let rows = [
            ("body exactly at the limit is left alone", max, 100, false),
            ("body one byte over the limit is shrunk", max + 1, 100, true),
            ("kept body exactly at the limit is shrunk", max + 100, 95, true),
            ("kept body one byte over the limit is left alone", max + 100, 94, false),
            ("saving exactly 8 bytes (a bare free header) is shrunk", max + 4, 3, true),
            ("saving 7 bytes is left alone", max + 4, 2, false),
        ];
        let mut bad = Vec::new();
        for (case, body, sei_nal, shrinks) in rows {
            let (original, sps) = record_of(body, sei_nal);
            let got = shrink_hvcc_box(&original);
            let want = shrinks.then(|| expected_shrunk(original.len(), &[sps], &[]));
            expect!(bad, got == want, "{case}: got {:?}, expected {:?}", got.map(|o| (o.len(), be32(&o, 0))), want.map(|o| (o.len(), be32(&o, 0))));
        }
        finish(bad);
    }

    #[test]
    fn records_it_cannot_use_are_left_alone() {
        let sps = nal_array(SPS, &[nal(50, 42)]);
        let sei = nal_array(PREFIX_SEI, &[nal(150, 1100)]);
        let mut overrun = hvcc_box(&[sps.clone(), sei.clone()], &[]);
        // The SEI NAL length (after box header, 23 record bytes, the SPS array and 3 bytes of
        // SEI array header) raised by one: the NAL ends past the record.
        let len_at = 8 + 23 + sps.len() + 3;
        overrun[len_at..len_at + 2].copy_from_slice(&1101u16.to_be_bytes());
        let rows: [(&str, Vec<u8>); 6] = [
            ("no SEI array: nothing to drop", hvcc_box(&[sps.clone(), nal_array(PPS, &[nal(110, 1100)])], &[])),
            ("a NAL longer than the record", overrun),
            ("more arrays declared than present", hvcc_box_declaring(3, &[sps.clone(), sei.clone()], &[])),
            ("record cut inside an SEI NAL", { let mut b = hvcc_box(&[sps.clone(), sei.clone()], &[]); b.truncate(1100); b }),
            ("empty input", Vec::new()),
            ("shorter than a box header", b"\0\0\0".to_vec()),
        ];
        let mut bad = Vec::new();
        for (case, input) in rows {
            let got = shrink_hvcc_box(&input);
            expect!(bad, got.is_none(), "{case}: got Some({} bytes)", got.map_or(0, |o| o.len()));
        }
        finish(bad);
    }
}

/// `ftyp`, then `boxes`, as one file under the test temp directory.
fn mp4_file(name: &str, boxes: &[Vec<u8>]) -> (std::path::PathBuf, Vec<u8>) {
    let mut bytes = mp4_box(b"ftyp", b"isom\0\0\0\x01isomhvc1");
    for b in boxes {
        bytes.extend_from_slice(b);
    }
    (tmp_file(name, &bytes), bytes)
}

/// A visual sample entry of type `kind` holding `children` after its 78 bytes of fields.
fn sample_entry(kind: &[u8; 4], children: &[Vec<u8>]) -> Vec<u8> {
    let mut body = vec![0u8; 78];
    body[24..28].copy_from_slice(&[0x01, 0x40, 0x00, 0xf0]); // 320 x 240
    for c in children {
        body.extend_from_slice(c);
    }
    mp4_box(kind, &body)
}

/// `trak/mdia/minf/stbl/stsd` around the given sample entries.
fn trak(entries: &[Vec<u8>]) -> Vec<u8> {
    let mut stsd = vec![0, 0, 0, 0];
    stsd.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for e in entries {
        stsd.extend_from_slice(e);
    }
    let stbl = mp4_box(b"stbl", &mp4_box(b"stsd", &stsd));
    let minf = mp4_box(b"minf", &[mp4_box(b"vmhd", &[0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0]), stbl].concat());
    let mdia = mp4_box(b"mdia", &[mp4_box(b"mdhd", &[0u8; 24]), minf].concat());
    mp4_box(b"trak", &[mp4_box(b"tkhd", &[0u8; 84]), mdia].concat())
}

fn moov(traks: &[Vec<u8>]) -> Vec<u8> {
    mp4_box(b"moov", &[vec![mp4_box(b"mvhd", &[0u8; 100])], traks.to_vec()].concat().concat())
}

/// An oversized hvcC whose SEI NAL starts at `seed`, so every such box in a file is unique.
fn big_hvcc(seed: u8) -> Vec<u8> {
    hvcc_box(&[nal_array(VPS, &[nal(seed, 24)]), nal_array(SPS, &[nal(seed ^ 0x55, 42)]), nal_array(PREFIX_SEI, &[nal(seed ^ 0xaa, 1100)])], &[])
}

/// The patch the contract asks for: the box found at its place in `file`, shrunk.
fn patch_for(file: &[u8], hvcc: &[u8]) -> Patch {
    let offset = file.windows(hvcc.len()).position(|w| w == hvcc).expect("box is in the file");
    assert_eq!(file.windows(hvcc.len()).filter(|w| *w == hvcc).count(), 1, "test precondition: box occurs once");
    Patch { offset: offset as u64, bytes: shrink_hvcc_box(hvcc).expect("shrinkable") }
}

mod find {
    use super::*;

    #[test]
    fn the_x265_fixture_gets_one_patch_over_its_hvcc_box() {
        let bytes = fixture_bytes("hevc_big_hvcc_aac.mp4");
        let at = bytes.windows(4).position(|w| w == b"hvcC").unwrap() - 4;
        let original = &bytes[at..at + be32(&bytes, at)];

        let patches = patches_for(&fixture("hevc_big_hvcc_aac.mp4"));

        assert_eq!(patches.len(), 1, "patches at {:?}", patches.iter().map(|p| p.offset).collect::<Vec<_>>());
        assert_eq!(patches[0].offset, at as u64);
        // The fixture's record: VPS 24, SPS 42, PPS 7 bytes (88 bytes of arrays) and a 2332-byte
        // SEI, in a 2456-byte box. Kept: 8 + 23 + 88 = 119 bytes; free: 2456 - 119 = 2337.
        let p = &patches[0].bytes;
        assert_eq!((p.len(), be32(p, 0), p[30], be32(p, 119), &p[123..127]), (2456, 119, 3, 2337, &b"free"[..]));
        assert_eq!(&p[8..30], &original[8..30], "fixed fields");
        assert_eq!(&p[31..119], &original[31..119], "VPS, SPS and PPS arrays, which precede the SEI");
        assert!(p[127..].iter().all(|b| *b == 0), "free body is zeros");
    }

    #[test]
    fn every_oversized_hvcc_of_hvc1_hev1_dvh1_and_dvhe_entries_is_found_in_any_track_and_entry() {
        let (first, second, third, fourth, fifth) = (big_hvcc(1), big_hvcc(2), big_hvcc(3), big_hvcc(11), big_hvcc(12));
        let colr = mp4_box(b"colr", b"nclx\0\x01\0\x01\0\x01\0");
        let small = hvcc_box(&[nal_array(SPS, &[nal(9, 42)])], &[]);
        let (path, bytes) = mp4_file(
            "hvcc_many.mp4",
            &[
                mp4_box(b"mdat", &[0x11; 300]),
                moov(&[
                    trak(&[sample_entry(b"hvc1", &[first.clone()])]),
                    // Second track: a small hvcC first, the oversized one in the second entry
                    // behind another child box.
                    trak(&[sample_entry(b"hvc1", &[small]), sample_entry(b"hev1", &[colr.clone(), second.clone()])]),
                    trak(&[sample_entry(b"hev1", &[third.clone(), colr.clone()])]),
                    // Dolby Vision over HEVC: dvh1 alone, dvhe as the second entry.
                    trak(&[sample_entry(b"dvh1", &[fourth.clone()])]),
                    trak(&[sample_entry(b"avc1", &[colr]), sample_entry(b"dvhe", &[fifth.clone()])]),
                ]),
            ],
        );

        let patches = patches_for(&path);

        let want = [&first, &second, &third, &fourth, &fifth].map(|h| patch_for(&bytes, h));
        assert_eq!(patches, want);
    }

    #[test]
    fn hvcc_boxes_outside_an_hevc_sample_entry_are_not_patched() {
        let (in_avc1, in_udta, in_moov, in_mdat) = (big_hvcc(4), big_hvcc(5), big_hvcc(6), big_hvcc(7));
        let mut traks = vec![trak(&[sample_entry(b"avc1", &[in_avc1])])];
        traks.push(mp4_box(b"udta", &in_udta));
        traks.push(in_moov);
        let (path, _) = mp4_file("hvcc_elsewhere.mp4", &[mp4_box(b"mdat", &in_mdat), moov(&traks)]);

        assert_eq!(patches_for(&path), Vec::new());
    }

    /// `inner` wrapped in boxes of the given types, the first outermost; `stsd` gets its
    /// version/flags and an entry count of 1, a sample entry its 78 bytes of fields.
    fn nest(path: &[&[u8; 4]], inner: &[u8]) -> Vec<u8> {
        path.iter().rev().fold(inner.to_vec(), |body, &kind| match kind {
            b"stsd" => mp4_box(kind, &[&[0, 0, 0, 0, 0, 0, 0, 1][..], &body].concat()),
            b"hvc1" | b"hev1" | b"dvh1" | b"dvhe" => mp4_box(kind, &[&[0u8; 78][..], &body].concat()),
            _ => mp4_box(kind, &body),
        })
    }

    #[test]
    fn only_the_moov_trak_mdia_minf_stbl_stsd_entry_path_is_walked() {
        // (case, boxes from inside moov down to the hvcC, patches expected)
        let rows: [(&str, &[&[u8; 4]], usize); 8] = [
            ("the sample description path (control)", &[b"trak", b"mdia", b"minf", b"stbl", b"stsd", b"hvc1"], 1),
            ("trak inside trak", &[b"trak", b"trak", b"mdia", b"minf", b"stbl", b"stsd", b"hvc1"], 0),
            ("stbl directly in trak", &[b"trak", b"stbl", b"stsd", b"hvc1"], 0),
            ("mdia directly in moov", &[b"mdia", b"minf", b"stbl", b"stsd", b"hvc1"], 0),
            ("minf inside minf", &[b"trak", b"mdia", b"minf", b"minf", b"stbl", b"stsd", b"hvc1"], 0),
            ("sample entry directly in stbl", &[b"trak", b"mdia", b"minf", b"stbl", b"hvc1"], 0),
            ("hvcC directly in stsd", &[b"trak", b"mdia", b"minf", b"stbl", b"stsd"], 0),
            ("sample entry inside a sample entry", &[b"trak", b"mdia", b"minf", b"stbl", b"stsd", b"hvc1", b"hvc1"], 0),
        ];
        let mut bad = Vec::new();
        for (n, (case, path, want)) in rows.into_iter().enumerate() {
            let hvcc = big_hvcc(20 + n as u8);
            let (file, bytes) = mp4_file(&format!("hvcc_nesting_{n}.mp4"), &[moov(&[nest(path, &hvcc)])]);
            let got = patches_for(&file);
            let want: Vec<Patch> = (0..want).map(|_| patch_for(&bytes, &hvcc)).collect();
            expect!(bad, got == want, "{case}: patches at {:?}, expected {:?}", got.iter().map(|p| p.offset).collect::<Vec<_>>(), want.iter().map(|p| p.offset).collect::<Vec<_>>());
        }
        finish(bad);
    }

    #[test]
    fn a_deep_chain_of_nested_trak_boxes_is_not_followed() {
        // 200,000 trak headers (1.6 MB) around a proper sample description path: a walk that
        // followed them recursively would overflow the stack.
        const DEPTH: usize = 200_000;
        let hvcc = big_hvcc(40);
        let inner = nest(&[b"trak", b"mdia", b"minf", b"stbl", b"stsd", b"hvc1"], &hvcc);
        let mut moov_body = mp4_box(b"mvhd", &[0u8; 100]);
        for level in 0..DEPTH {
            moov_body.extend_from_slice(&((8 * (DEPTH - level) + inner.len()) as u32).to_be_bytes());
            moov_body.extend_from_slice(b"trak");
        }
        moov_body.extend_from_slice(&inner);
        let (file, _) = mp4_file("hvcc_deep_trak.mp4", &[mp4_box(b"moov", &moov_body)]);

        assert_eq!(patches_for(&file), Vec::new());
    }

    #[test]
    fn moov_is_found_wherever_it_is_and_whatever_its_size_form() {
        let hvcc = big_hvcc(8);
        let entry = trak(&[sample_entry(b"hvc1", &[hvcc.clone()])]);
        let moov_box = moov(&[entry]);
        let mut large_mdat = 1u32.to_be_bytes().to_vec();
        large_mdat.extend_from_slice(b"mdat");
        large_mdat.extend_from_slice(&(16u64 + 40).to_be_bytes());
        large_mdat.extend_from_slice(&[0x22; 40]);
        let mut moov_to_end = moov_box.clone();
        moov_to_end[..4].copy_from_slice(&0u32.to_be_bytes());
        let rows = [
            ("moov first", vec![moov_box.clone(), mp4_box(b"mdat", &[0x33; 50])]),
            ("moov after mdat", vec![mp4_box(b"mdat", &[0x33; 50]), moov_box.clone()]),
            ("moov after a 64-bit mdat", vec![large_mdat, moov_box.clone()]),
            ("moov with size 0 (to the end of the file)", vec![mp4_box(b"free", &[0; 12]), moov_to_end]),
        ];
        let mut bad = Vec::new();
        for (n, (case, boxes)) in rows.into_iter().enumerate() {
            let (path, bytes) = mp4_file(&format!("hvcc_moov_{n}.mp4"), &boxes);
            let got = patches_for(&path);
            let want = vec![patch_for(&bytes, &hvcc)];
            expect!(bad, got == want, "{case}: patches at {:?}, expected at {}", got.iter().map(|p| p.offset).collect::<Vec<_>>(), want[0].offset);
        }
        finish(bad);
    }

    #[test]
    fn files_without_a_usable_moov_get_no_patch() {
        let moov_box = moov(&[trak(&[sample_entry(b"hvc1", &[big_hvcc(9)])])]);
        let whole = mp4_file("hvcc_cut_source.mp4", &[mp4_box(b"mdat", &[0x44; 64]), moov_box]).1;
        let cut = tmp_file("hvcc_cut.mp4", &whole[..whole.len() - 1]);
        let no_moov = mp4_file("hvcc_no_moov.mp4", &[mp4_box(b"mdat", &[0x44; 64])]).0;
        let small = mp4_file("hvcc_small.mp4", &[moov(&[trak(&[sample_entry(b"hvc1", &[hvcc_box(&[nal_array(SPS, &[nal(9, 900)])], &[])])])])]).0;
        let rows = [
            ("missing file", fixture("sine.wav").with_file_name("no-such-file.mp4")),
            ("empty file", tmp_file("hvcc_empty.mp4", &[])),
            ("moov cut off by one byte", cut),
            ("no moov", no_moov),
            ("hvcC within the limit", small),
            ("not MP4", fixture("sine.flac")),
        ];
        let mut bad = Vec::new();
        for (case, path) in rows {
            let got = patches_for(&path);
            expect!(bad, got.is_empty(), "{case}: {} patch(es)", got.len());
        }
        finish(bad);
    }

    /// A file of `moov_body` + 8 bytes: a `moov` whose body starts with `first_child` and is
    /// zeros after it (the file is extended with `set_len`, never built in memory).
    fn moov_of_body(name: &str, moov_body: u64, first_child: &[u8]) -> std::path::PathBuf {
        let mut head = ((moov_body + 8) as u32).to_be_bytes().to_vec();
        head.extend_from_slice(b"moov");
        head.extend_from_slice(first_child);
        let path = tmp_file(name, &head);
        std::fs::OpenOptions::new().write(true).open(&path).unwrap().set_len(moov_body + 8).unwrap();
        path
    }

    #[test]
    fn moov_up_to_64_mib_is_scanned_and_a_larger_one_is_not() {
        // The moov reader's cap (64 << 20, in `playr::demux`) keeps a huge moov out of memory.
        let hvcc = big_hvcc(10);
        let entry = trak(&[sample_entry(b"hvc1", &[hvcc.clone()])]);
        let at_cap = moov_of_body("hvcc_moov_at_cap.mp4", 64 << 20, &entry);
        let over_cap = moov_of_body("hvcc_moov_over_cap.mp4", (64 << 20) + 1, &entry);

        let got = patches_for(&at_cap);
        let over = patches_for(&over_cap);
        let _ = std::fs::remove_file(&at_cap);
        let _ = std::fs::remove_file(&over_cap);

        let offset = 8 + entry.windows(hvcc.len()).position(|w| w == hvcc.as_slice()).unwrap() as u64;
        assert_eq!(got, [Patch { offset, bytes: shrink_hvcc_box(&hvcc).unwrap() }], "moov body of exactly 64 MiB");
        assert_eq!(over, Vec::new(), "moov body of 64 MiB + 1");
    }
}

mod patched_file {
    use super::*;

    const LEN: usize = 100;

    fn file_bytes() -> Vec<u8> {
        (0..LEN).map(|i| (i * 7 + 3) as u8).collect()
    }

    fn patch(offset: u64, len: usize, seed: u8) -> Patch {
        Patch { offset, bytes: (0..len).map(|i| seed + i as u8).collect() }
    }

    /// The file as the patches say it reads: its bytes with each patch written over them.
    fn model(patches: &[Patch]) -> Vec<u8> {
        let mut out = file_bytes();
        for p in patches {
            out[p.offset as usize..p.offset as usize + p.bytes.len()].copy_from_slice(&p.bytes);
        }
        out
    }

    fn open(name: &str, patches: &[Patch]) -> PatchedFile {
        let path = tmp_file(name, &file_bytes());
        PatchedFile::new(std::fs::File::open(path).unwrap(), patches.to_vec()).unwrap()
    }

    /// Patches at 20..30 and 50..55, handed over out of order.
    fn two_patches() -> Vec<Patch> {
        vec![patch(50, 5, 220), patch(20, 10, 200)]
    }

    #[test]
    fn single_reads_return_the_patched_bytes_wherever_they_start_and_end() {
        let want_all = model(&two_patches());
        // (case, start, buffer length)
        let rows = [
            ("before the patch", 0, 10),
            ("ending where the patch starts", 10, 10),
            ("starting at the patch start, inside it", 20, 5),
            ("inside the patch", 22, 4),
            ("from inside the patch to past it", 25, 10),
            ("straddling the whole patch", 15, 20),
            ("starting where the patch ends", 30, 10),
            ("over both patches", 10, 60),
            ("the whole file", 0, LEN),
            ("short read at the end of the file", 95, 10),
            ("at the end of the file", LEN, 4),
            ("past the end of the file", LEN + 20, 4),
        ];
        let mut bad = Vec::new();
        for (case, start, len) in rows {
            let mut f = open("patched_single.bin", &two_patches());
            f.seek(SeekFrom::Start(start as u64)).unwrap();
            let mut buf = vec![0xeeu8; len];
            let n = f.read(&mut buf).unwrap();
            let want = &want_all[start.min(LEN)..(start + len).min(LEN)];
            expect!(bad, &buf[..n] == want, "{case}: read {n} bytes {:?}, expected {} bytes {want:?}", &buf[..n], want.len());
            expect!(bad, buf[n..].iter().all(|b| *b == 0xee), "{case}: bytes written past the {n} read");
            let pos = f.stream_position().unwrap();
            expect!(bad, pos == (start + n) as u64, "{case}: position {pos} after reading {n} from {start}");
        }
        finish(bad);
    }

    #[test]
    fn consecutive_small_reads_return_the_patched_file_in_order() {
        let patches = vec![patch(0, 3, 240), patch(20, 10, 200), patch(97, 3, 230)];
        let mut f = open("patched_consecutive.bin", &patches);
        let mut got = Vec::new();
        let mut buf = [0u8; 7];
        loop {
            let n = f.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(got, model(&patches));
    }

    #[test]
    fn seeks_from_start_end_and_current_position() {
        // (case, position before, seek, position after)
        let rows = [
            ("Start", 40, SeekFrom::Start(23), 23),
            ("End minus 3", 40, SeekFrom::End(-3), 97),
            ("End", 40, SeekFrom::End(0), 100),
            ("past End", 40, SeekFrom::End(5), 105),
            ("Current forward", 10, SeekFrom::Current(14), 24),
            ("Current back", 30, SeekFrom::Current(-4), 26),
            ("Current back to 0", 30, SeekFrom::Current(-30), 0),
        ];
        let want_all = model(&two_patches());
        let mut bad = Vec::new();
        for (case, before, to, after) in rows {
            let mut f = open("patched_seek.bin", &two_patches());
            f.seek(SeekFrom::Start(before)).unwrap();
            let got = f.seek(to).unwrap();
            expect!(bad, got == after, "{case}: seek returned {got}, expected {after}");
            let mut buf = [0u8; 4];
            let n = f.read(&mut buf).unwrap();
            let a = (after as usize).min(LEN);
            let want = &want_all[a..(a + 4).min(LEN)];
            expect!(bad, &buf[..n] == want, "{case}: then read {:?}, expected {want:?}", &buf[..n]);
        }
        finish(bad);
    }

    #[test]
    fn seeking_before_the_start_fails_and_keeps_the_position() {
        let want_all = model(&two_patches());
        let mut bad = Vec::new();
        for (case, to) in [("Current -11 from 10", SeekFrom::Current(-11)), ("End -101", SeekFrom::End(-101))] {
            let mut f = open("patched_seek_before.bin", &two_patches());
            f.seek(SeekFrom::Start(10)).unwrap();
            match f.seek(to) {
                Ok(p) => bad.push(format!("{case}: seek succeeded at {p}")),
                Err(e) => {
                    expect!(bad, e.kind() == ErrorKind::InvalidInput, "{case}: error kind {:?}", e.kind());
                    expect!(bad, e.to_string() == "seek before the start of the file", "{case}: error {:?}", e.to_string());
                }
            }
            let mut buf = [0u8; 15];
            let n = f.read(&mut buf).unwrap();
            expect!(bad, buf[..n] == want_all[10..25], "{case}: next read {:?}, expected bytes 10..25", &buf[..n]);
        }
        finish(bad);
    }

    #[test]
    fn reports_the_file_length_and_seekability() {
        let f = open("patched_len.bin", &two_patches());
        assert_eq!((f.byte_len(), f.is_seekable()), (Some(LEN as u64), true));
    }
}
