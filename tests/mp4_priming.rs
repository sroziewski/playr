//! `playr::demux::mp4_audio_priming_secs` on hand-built MP4 box trees: the encoder priming is
//! the media time of the first non-empty edit of the first audio track, in that track's
//! timescale; anything the walker cannot read means "no priming", never a panic.

mod common;

use common::*;
use playr::demux::mp4_audio_priming_secs;

fn mp4_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    out
}

/// The same box with the 64-bit size form: size field 1, then the real size as u64.
fn mp4_box64(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = 1u32.to_be_bytes().to_vec();
    out.extend_from_slice(kind);
    out.extend_from_slice(&((body.len() + 16) as u64).to_be_bytes());
    out.extend_from_slice(body);
    out
}

fn hdlr(kind: &[u8; 4]) -> Vec<u8> {
    // version/flags, pre_defined, handler type, three reserved words, empty name.
    mp4_box(b"hdlr", &[&[0u8; 8][..], kind, &[0u8; 13]].concat())
}

fn mdhd(version: u8, timescale: u32) -> Vec<u8> {
    let mut body = vec![version, 0, 0, 0];
    // creation and modification time: 32 bits each in version 0, 64 in version 1.
    body.extend(vec![0u8; if version == 1 { 16 } else { 8 }]);
    body.extend_from_slice(&timescale.to_be_bytes());
    // duration (same widths), language, pre_defined.
    body.extend(vec![0u8; if version == 1 { 8 } else { 4 } + 4]);
    mp4_box(b"mdhd", &body)
}

/// Edit list entries are (segment duration, media time); media time -1 is an empty edit.
fn elst(version: u8, entries: &[(u64, i64)]) -> Vec<u8> {
    let mut body = vec![version, 0, 0, 0];
    body.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for (duration, media_time) in entries {
        if version == 1 {
            body.extend_from_slice(&duration.to_be_bytes());
            body.extend_from_slice(&media_time.to_be_bytes());
        } else {
            body.extend_from_slice(&(*duration as u32).to_be_bytes());
            body.extend_from_slice(&(*media_time as i32).to_be_bytes());
        }
        body.extend_from_slice(&0x0001_0000u32.to_be_bytes());
    }
    mp4_box(b"elst", &body)
}

fn trak(handler: &[u8; 4], mdhd: &[u8], elst: Option<&[u8]>) -> Vec<u8> {
    let mut body = mp4_box(b"tkhd", &[0u8; 84]);
    if let Some(elst) = elst {
        body.extend(mp4_box(b"edts", elst));
    }
    body.extend(mp4_box(b"mdia", &[mdhd, &hdlr(handler)[..]].concat()));
    mp4_box(b"trak", &body)
}

fn audio_trak(timescale: u32, edits: &[(u64, i64)]) -> Vec<u8> {
    trak(b"soun", &mdhd(0, timescale), Some(&elst(0, edits)))
}

fn ftyp() -> Vec<u8> {
    mp4_box(b"ftyp", b"isom\0\0\x02\0isomiso2mp41")
}

fn moov_body(traks: &[Vec<u8>]) -> Vec<u8> {
    [mp4_box(b"mvhd", &[0u8; 100]), traks.concat()].concat()
}

fn file(traks: &[Vec<u8>]) -> Vec<u8> {
    [ftyp(), mp4_box(b"mdat", &[0xAB; 40]), mp4_box(b"moov", &moov_body(traks))].concat()
}

fn priming_of(name: &str, bytes: &[u8]) -> f64 {
    mp4_audio_priming_secs(&tmp_file(&format!("priming_{name}.m4a"), bytes))
}

#[test]
fn reads_the_first_real_edit_of_the_audio_track() {
    let video = trak(b"vide", &mdhd(0, 12_800), Some(&elst(0, &[(2000, 512)])));
    let cases: Vec<(&str, Vec<u8>, f64)> = vec![
        ("aac_1024_at_44100", file(&[audio_trak(44_100, &[(132_300, 1024)])]), 1024.0 / 44_100.0),
        ("he_aac_2112_at_48000", file(&[audio_trak(48_000, &[(96_000, 2112)])]), 2112.0 / 48_000.0),
        // The media time is in the track's own timescale, whatever the sample rate is.
        ("timescale_22050", file(&[audio_trak(22_050, &[(132_300, 512)])]), 512.0 / 22_050.0),
        ("no_priming", file(&[audio_trak(44_100, &[(132_300, 0)])]), 0.0),
        ("video_track_first", file(&[video.clone(), audio_trak(44_100, &[(132_300, 1024)])]), 1024.0 / 44_100.0),
        ("video_track_last", file(&[audio_trak(44_100, &[(132_300, 1024)]), video.clone()]), 1024.0 / 44_100.0),
        (
            "version_1_mdhd_and_elst",
            file(&[trak(b"soun", &mdhd(1, 48_000), Some(&elst(1, &[(96_000, 2112)])))]),
            2112.0 / 48_000.0,
        ),
        (
            "version_1_elst_beyond_32_bits",
            file(&[trak(b"soun", &mdhd(1, 1_000_000), Some(&elst(1, &[(1, 6_000_000_000)])))]),
            6000.0,
        ),
        ("empty_edit_then_real", file(&[audio_trak(44_100, &[(500, -1), (132_300, 1024)])]), 1024.0 / 44_100.0),
        (
            "empty_edit_then_real_version_1",
            file(&[trak(b"soun", &mdhd(1, 44_100), Some(&elst(1, &[(500, -1), (132_300, 1024)])))]),
            1024.0 / 44_100.0,
        ),
        ("first_real_edit_wins", file(&[audio_trak(44_100, &[(1000, 1024), (1000, 9000)])]), 1024.0 / 44_100.0),
        (
            "twenty_edits",
            file(&[audio_trak(44_100, &(0..20).map(|n| (100, 1024 + n * 4096)).collect::<Vec<_>>())]),
            1024.0 / 44_100.0,
        ),
    ];
    for (name, bytes, expected) in cases {
        let got = priming_of(name, &bytes);
        assert!((got - expected).abs() < 1e-9, "{name}: priming {got} s, expected {expected}");
    }
}

#[test]
fn finds_moov_behind_every_legal_box_size_form() {
    let traks = [audio_trak(44_100, &[(132_300, 1024)])];
    let moov = mp4_box(b"moov", &moov_body(&traks));
    let mut moov_to_end_of_file = moov.clone();
    moov_to_end_of_file[..4].copy_from_slice(&0u32.to_be_bytes());
    let mut mdat_to_end_of_file = mp4_box(b"mdat", &[0xAB; 40]);
    mdat_to_end_of_file[..4].copy_from_slice(&0u32.to_be_bytes());
    let cases: Vec<(&str, Vec<u8>, f64)> = vec![
        ("moov_first", [ftyp(), moov.clone(), mp4_box(b"mdat", &[0xAB; 40])].concat(), 1024.0 / 44_100.0),
        ("moov_64_bit_size", [ftyp(), mp4_box64(b"moov", &moov_body(&traks))].concat(), 1024.0 / 44_100.0),
        ("box_64_bit_size_before_moov", [ftyp(), mp4_box64(b"mdat", &[0xAB; 40]), moov.clone()].concat(), 1024.0 / 44_100.0),
        ("moov_size_0_to_end_of_file", [ftyp(), mp4_box(b"free", &[0; 12]), moov_to_end_of_file].concat(), 1024.0 / 44_100.0),
        // A size-0 box takes the rest of the file, so the moov after it is not a box at all.
        ("moov_behind_a_size_0_box", [ftyp(), mdat_to_end_of_file, moov.clone()].concat(), 0.0),
    ];
    for (name, bytes, expected) in cases {
        let got = priming_of(name, &bytes);
        assert!((got - expected).abs() < 1e-9, "{name}: priming {got} s, expected {expected}");
    }
}

#[test]
fn files_the_walker_cannot_read_have_no_priming() {
    let good_trak = audio_trak(44_100, &[(132_300, 1024)]);
    let moov = mp4_box(b"moov", &moov_body(&[good_trak.clone()]));

    // A box whose size (4) is smaller than its own header, in front of a good moov.
    let undersized = [ftyp(), 4u32.to_be_bytes().to_vec(), b"free".to_vec(), moov.clone()].concat();
    // 64-bit size smaller than the 16-byte header.
    let undersized64 = [ftyp(), 1u32.to_be_bytes().to_vec(), b"free".to_vec(), 9u64.to_be_bytes().to_vec(), moov.clone()].concat();
    // moov claims 80 MB (the walker reads at most 64 MB) in a file of a few hundred bytes.
    let mut huge_moov = moov.clone();
    huge_moov[..4].copy_from_slice(&(80u32 << 20).to_be_bytes());
    // moov claims more than the file holds.
    let mut long_moov = moov.clone();
    let claimed = moov.len() as u32 + 1;
    long_moov[..4].copy_from_slice(&claimed.to_be_bytes());
    // 64-bit moov size of 2^63.
    let huge_moov64 = [ftyp(), 1u32.to_be_bytes().to_vec(), b"moov".to_vec(), (1u64 << 63).to_be_bytes().to_vec(), moov_body(&[good_trak.clone()])].concat();
    // An edit list announcing 1000 entries but holding none.
    let mut empty_elst = elst(0, &[]);
    let at = empty_elst.len() - 4;
    empty_elst[at..].copy_from_slice(&1000u32.to_be_bytes());

    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("undersized_box", undersized),
        ("undersized_64_bit_box", undersized64),
        ("moov_claims_80_mb", [ftyp(), huge_moov].concat()),
        ("moov_longer_than_the_file", [ftyp(), long_moov].concat()),
        ("moov_claims_2_pow_63", huge_moov64),
        ("no_moov", [ftyp(), mp4_box(b"mdat", &[0xAB; 40])].concat()),
        ("empty_moov", [ftyp(), mp4_box(b"moov", &[])].concat()),
        ("no_tracks", file(&[])),
        ("video_only", file(&[trak(b"vide", &mdhd(0, 12_800), Some(&elst(0, &[(2000, 512)])))])),
        ("audio_without_edit_list", file(&[trak(b"soun", &mdhd(0, 44_100), None)])),
        ("timescale_0", file(&[audio_trak(0, &[(132_300, 1024)])])),
        ("only_empty_edits", file(&[audio_trak(44_100, &[(500, -1), (700, -1)])])),
        ("no_edits", file(&[audio_trak(44_100, &[])])),
        ("edit_count_without_entries", file(&[trak(b"soun", &mdhd(0, 44_100), Some(&empty_elst))])),
        ("eight_bytes", b"\0\0\0\x08moov".to_vec()),
        ("seven_bytes", b"\0\0\0\x08moo".to_vec()),
    ];
    for (name, bytes) in cases {
        assert_eq!(priming_of(name, &bytes), 0.0, "{name}");
    }
}

#[test]
fn every_truncation_of_a_small_mp4_is_read_without_panicking() {
    let whole = file(&[audio_trak(44_100, &[(500, -1), (132_300, 1024)])]);
    for cut in 0..whole.len() {
        let got = priming_of("truncated", &whole[..cut]);
        // moov is the last box: until it is complete there is nothing to read.
        assert_eq!(got, 0.0, "cut to {cut} of {} bytes", whole.len());
    }
    assert!((priming_of("truncated", &whole) - 1024.0 / 44_100.0).abs() < 1e-9);
}
