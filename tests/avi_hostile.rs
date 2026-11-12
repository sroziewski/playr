//! AVI files whose headers lie about sizes and offsets. Kept in a test binary of its own: a
//! reader that trusts a 4 GiB size field can take the whole process down, and that must not
//! hide the results of unrelated tests.

mod common;

use std::time::{Duration, Instant};

use common::avi::{Avi, Index, bitmap_info, chunk, strh};
use common::*;
use playr::demux::open_path;

fn vop(tag: u8) -> Vec<u8> {
    let mut v = vec![0, 0, 1, 0xB6, 0x10];
    v.resize(24, tag);
    v
}

/// Three intra pictures at 25 fps.
fn small_avi() -> Avi {
    Avi {
        streams: vec![(strh(b"vids", b"XVID", 1, 25, 0), bitmap_info(64, 48, b"XVID"))],
        chunks: (0..3).map(|n| (*b"00dc", vop(0xB0 + n), true)).collect(),
    }
}

fn find(bytes: &[u8], needle: &[u8; 4]) -> usize {
    bytes.windows(4).position(|w| w == needle).unwrap_or_else(|| panic!("{} not in the file", String::from_utf8_lossy(needle)))
}

fn put_u32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

enum Outcome {
    Rejected(String),
    /// Video payload tags that were read, and the error that ended reading, if any.
    Read(Vec<u8>, Option<String>),
}

/// Opens and reads the file to its end, however that end comes. Panics and hangs are the
/// failures; a rejected file or a read error must carry a message.
fn read_through(name: &str, bytes: &[u8]) -> Outcome {
    let started = Instant::now();
    let outcome = match open_path(&tmp_file(&format!("hostile_{name}.avi"), bytes)) {
        Err(e) => Outcome::Rejected(e),
        Ok(mut demux) => {
            let (d, err) = drain(demux.as_mut());
            Outcome::Read(d.video.iter().map(|(_, data)| data[5]).collect(), err)
        }
    };
    let took = started.elapsed();
    assert!(took < Duration::from_secs(10), "{name}: took {took:?}");
    match &outcome {
        Outcome::Rejected(e) | Outcome::Read(_, Some(e)) => assert!(e.trim().len() >= 10, "{name}: unreadable error {e:?}"),
        Outcome::Read(_, None) => {}
    }
    outcome
}

fn assert_plays_all_three_pictures(name: &str, bytes: &[u8]) {
    match read_through(name, bytes) {
        Outcome::Read(tags, None) => assert_eq!(tags, [0xB0, 0xB1, 0xB2], "{name}"),
        Outcome::Read(tags, Some(e)) => panic!("{name}: stopped after {tags:02X?} with {e:?}"),
        Outcome::Rejected(e) => panic!("{name}: rejected with {e:?}"),
    }
}

fn assert_rejected(name: &str, bytes: &[u8], message: &str) {
    match read_through(name, bytes) {
        Outcome::Rejected(e) => assert_eq!(e, message, "{name}"),
        Outcome::Read(tags, err) => panic!("{name}: opened and read {tags:02X?} (error {err:?}), expected Err({message:?})"),
    }
}

#[test]
fn the_unmodified_file_plays() {
    for index in [Index::Relative, Index::Missing] {
        assert_plays_all_three_pictures(&format!("baseline_{index:?}"), &small_avi().build(index));
    }
}

#[test]
fn size_fields_larger_than_the_file_do_not_stop_playback_when_the_data_is_there() {
    // What a writer leaves behind when it could not go back and patch the sizes.
    let mut riff_size = small_avi().build(Index::Relative);
    put_u32(&mut riff_size, 4, 0xFFFF_FFFF);
    assert_plays_all_three_pictures("riff_size", &riff_size);

    for index in [Index::Relative, Index::Missing] {
        let mut movi_size = small_avi().build(index);
        let at = find(&movi_size, b"movi") - 4;
        put_u32(&mut movi_size, at, 0xFFFF_FFFF);
        assert_plays_all_three_pictures(&format!("movi_size_{index:?}"), &movi_size);
    }

    let mut idx1_size = small_avi().build(Index::Relative);
    let at = find(&idx1_size, b"idx1") + 4;
    put_u32(&mut idx1_size, at, 0xFFFF_FFFF);
    assert_plays_all_three_pictures("idx1_size", &idx1_size);

    let mut strl_size = small_avi().build(Index::Relative);
    let at = find(&strl_size, b"strl") - 4;
    put_u32(&mut strl_size, at, 0xFFFF_FFFF);
    assert_plays_all_three_pictures("strl_size", &strl_size);
}

#[test]
fn an_index_that_names_no_stream_chunks_falls_back_to_reading_the_movi_list() {
    let mut bytes = small_avi().build(Index::Relative);
    let idx1 = find(&bytes, b"idx1");
    for entry in 0..3 {
        bytes[idx1 + 8 + entry * 16..][..4].copy_from_slice(b"JUNK");
    }
    assert_plays_all_three_pictures("index_of_junk", &bytes);
}

#[test]
fn a_header_list_or_filler_chunk_claiming_4_gib_is_rejected() {
    let mut hdrl = small_avi().build(Index::Relative);
    let at = find(&hdrl, b"hdrl") - 4;
    put_u32(&mut hdrl, at, 0xFFFF_FFFF);
    // The header list is the first thing in the file, so everything after it is "inside" it.
    match read_through("hdrl_size", &hdrl) {
        Outcome::Rejected(e) => assert!(e.starts_with("AVI read error: "), "{e:?}"),
        Outcome::Read(tags, err) => panic!("opened and read {tags:02X?} (error {err:?})"),
    }

    let junk = chunk(b"RIFF", &[&b"AVI "[..], b"JUNK", &0xFFFF_FFFFu32.to_le_bytes(), &[0u8; 64]].concat());
    assert_rejected("junk_size", &junk, "AVI has no movi list");

    let noise = [&chunk(b"RIFF", b"AVI ")[..], &[0xFFu8; 65536]].concat();
    assert_rejected("all_ff", &noise, "AVI has no movi list");
}

#[test]
fn a_truncated_stream_header_leaves_no_playable_stream() {
    let avi = Avi { streams: vec![(strh(b"vids", b"XVID", 1, 25, 0)[..20].to_vec(), bitmap_info(64, 48, b"XVID"))], ..small_avi() };
    assert_rejected("short_strh", &avi.build(Index::Relative), "AVI has no playable streams");
}

#[test]
fn a_chunk_claiming_4_gib_ends_in_an_error_or_is_skipped() {
    // In the index ...
    let mut indexed = small_avi().build(Index::Relative);
    let at = find(&indexed, b"idx1") + 8 + 12;
    put_u32(&mut indexed, at, 0xFFFF_FFF0);
    // ... and in the chunk header itself, with no index to fall back on.
    let mut scanned = small_avi().build(Index::Missing);
    let at = find(&scanned, b"00dc") + 4;
    put_u32(&mut scanned, at, 0xFFFF_FFF0);

    // The oversized chunk is the first picture, so nothing is delivered before the error.
    for (name, bytes) in [("chunk_size_in_index", indexed), ("chunk_size_in_movi", scanned)] {
        assert_stops_at_the_chunk_past_the_end(name, &bytes);
    }
}

fn assert_stops_at_the_chunk_past_the_end(name: &str, bytes: &[u8]) {
    match read_through(name, bytes) {
        Outcome::Read(tags, err) => {
            assert_eq!(err.as_deref(), Some("AVI chunk runs past the end of the file"), "{name}");
            assert_eq!(tags, Vec::<u8>::new(), "{name}: no picture can come before the first chunk");
        }
        Outcome::Rejected(e) => panic!("{name}: rejected at open with {e:?}; the headers are intact"),
    }
}

#[test]
fn a_chunk_cut_off_by_the_end_of_the_file_is_reported_after_the_pictures_before_it() {
    // The last picture's 24 bytes are short by one; the two before it are complete.
    let whole = small_avi().build(Index::Missing);
    match read_through("last_chunk_cut", &whole[..whole.len() - 1]) {
        Outcome::Read(tags, err) => {
            assert_eq!(tags, [0xB0, 0xB1]);
            assert_eq!(err.as_deref(), Some("AVI chunk runs past the end of the file"));
        }
        Outcome::Rejected(e) => panic!("rejected at open with {e:?}"),
    }
}

#[test]
fn index_offsets_past_the_end_of_the_file_end_in_an_error_or_are_skipped() {
    let mut bytes = small_avi().build(Index::Relative);
    let idx1 = find(&bytes, b"idx1");
    for entry in 0..3 {
        put_u32(&mut bytes, idx1 + 8 + entry * 16 + 8, 0x7FFF_FF00);
    }
    assert_stops_at_the_chunk_past_the_end("offsets_past_eof", &bytes);
}
