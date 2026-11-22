//! `playr::dash`: ranged downloads, the `sidx` fragment index, the spliced source that jumps
//! to a fragment, and the DASH demuxers over the checked-in fragmented MP4 fixtures
//! (`dash_video.mp4`: 6 fragments of 2.0 s; `dash_audio.m4a`: 4 fragments of 133120, 133120,
//! 133120 and 130864 ticks at 44100 Hz). The remote file is played by an in-memory
//! `RangeFetch` that records every request, or by a local HTTP server.

mod common;

use std::collections::VecDeque;
use std::io::{self, ErrorKind, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use common::range_http::{Mode, RangeServer};
use common::{finish, fixture_bytes};
use playr::dash::{
    ChunkedReader, DashDemuxer, DashFile, Fragment, HttpFetch, Layout, LayoutError, MergedDemuxer, RangeFetch, RangeSource,
    SplicedSource, content_range_total, free_box_header, parse_layout,
};
use playr::demux::Demuxer;
use playr::media::{AudioTrack, MediaInfo, Packet, VideoCodec, VideoTrack};
use symphonia::core::io::MediaSource;

/// What the in-memory server does with the next request.
#[derive(Clone, Copy, Debug)]
enum Answer {
    Whole,
    Refuse,
    /// Sends this many bytes, then ends the body early.
    Short(usize),
    /// Sends this many bytes, then the connection breaks.
    Break(usize),
    /// Reports this many `Interrupted` reads before sending everything.
    Interrupted(usize),
}

struct MemFetch {
    data: Vec<u8>,
    opens: Mutex<Vec<(u64, u64)>>,
    script: Mutex<VecDeque<Answer>>,
}

impl MemFetch {
    fn new(data: Vec<u8>) -> Arc<Self> {
        Self::scripted(data, &[])
    }

    fn scripted(data: Vec<u8>, answers: &[Answer]) -> Arc<Self> {
        Arc::new(Self { data, opens: Mutex::new(Vec::new()), script: Mutex::new(answers.iter().copied().collect()) })
    }

    fn opens(&self) -> Vec<(u64, u64)> {
        self.opens.lock().unwrap().clone()
    }
}

struct Body {
    bytes: Vec<u8>,
    pos: usize,
    stop_at: usize,
    breaks: bool,
    interrupts: usize,
}

impl Read for Body {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.interrupts > 0 {
            self.interrupts -= 1;
            return Err(io::Error::new(ErrorKind::Interrupted, "interrupted by test"));
        }
        if self.pos >= self.stop_at {
            return if self.breaks { Err(io::Error::new(ErrorKind::ConnectionReset, "connection reset by test")) } else { Ok(0) };
        }
        let n = buf.len().min(self.stop_at - self.pos);
        buf[..n].copy_from_slice(&self.bytes[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

impl RangeFetch for MemFetch {
    fn len(&self) -> u64 {
        self.data.len() as u64
    }

    fn open(&self, from: u64, to: u64) -> Result<Box<dyn Read + Send>, String> {
        self.opens.lock().unwrap().push((from, to));
        let answer = self.script.lock().unwrap().pop_front().unwrap_or(Answer::Whole);
        let bytes = self.data[from as usize..to as usize].to_vec();
        let len = bytes.len();
        let body = |stop_at: usize, breaks: bool, interrupts: usize| -> Box<dyn Read + Send> {
            Box::new(Body { bytes: bytes.clone(), pos: 0, stop_at: stop_at.min(len), breaks, interrupts })
        };
        match answer {
            Answer::Whole => Ok(body(len, false, 0)),
            Answer::Refuse => Err(format!("refused {from}-{to}")),
            Answer::Short(n) => Ok(body(n, false, 0)),
            Answer::Break(n) => Ok(body(n, true, 0)),
            Answer::Interrupted(k) => Ok(body(len, false, k)),
        }
    }
}

/// Bytes whose value depends on their position, so a byte from the wrong offset shows.
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| ((i % 251) as u8) ^ ((i / 251) as u8).wrapping_mul(17)).collect()
}

/// Reads to the end with a buffer of `buf_len`; returns the bytes and the final result.
fn read_all(reader: &mut impl Read, buf_len: usize) -> (Vec<u8>, io::Result<()>) {
    let mut out = Vec::new();
    let mut buf = vec![0u8; buf_len];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => return (out, Ok(())),
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) => return (out, Err(e)),
        }
    }
}

fn no_abort() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

#[test]
fn content_range_total_reads_the_length_after_the_slash() {
    let cases = [
        ("bytes 0-99/1234", Some(1234)),
        ("bytes */5000", Some(5000)),
        ("  bytes 10-19/77  ", Some(77)),
        ("bytes 0-99/ 88", Some(88)),
        ("bytes 0-99/*", None),
        ("bytes 0-99", None),
        ("bytes 0-99/12x", None),
        ("items 0-99/1234", None),
        ("", None),
    ];
    let mut bad = Vec::new();
    for (header, expected) in cases {
        let actual = content_range_total(header);
        expect!(bad, actual == expected, "content_range_total({header:?}) = {actual:?}, expected {expected:?}");
    }
    finish(bad);
}

mod chunked_reader {
    use super::*;

    #[test]
    fn reads_the_range_in_chunk_sized_requests() {
        let cases: [(&str, u64, u64, Vec<(u64, u64)>); 4] = [
            ("from the start", 0, 7, vec![(0, 7), (7, 14), (14, 21), (21, 28), (28, 30)]),
            ("from the middle", 10, 8, vec![(10, 18), (18, 26), (26, 30)]),
            ("one chunk larger than the file", 3, 1000, vec![(3, 30)]),
            ("chunk size 0 counts as 1", 27, 0, vec![(27, 28), (28, 29), (29, 30)]),
        ];
        let data = pattern(30);
        let mut bad = Vec::new();
        for (case, from, chunk, expected_opens) in cases {
            let fetch = MemFetch::new(data.clone());
            let mut reader = ChunkedReader::new(fetch.clone(), from, chunk, no_abort());
            let (bytes, end) = read_all(&mut reader, 64);
            expect!(bad, end.is_ok(), "{case}: {end:?}");
            expect!(bad, bytes == data[from as usize..], "{case}: wrong bytes {bytes:?}");
            expect!(bad, fetch.opens() == expected_opens, "{case}: requests {:?}, expected {expected_opens:?}", fetch.opens());
        }
        finish(bad);
    }

    #[test]
    fn starting_at_or_past_the_end_reads_nothing_and_asks_for_nothing() {
        for from in [30, 31, u64::MAX] {
            let fetch = MemFetch::new(pattern(30));
            let mut reader = ChunkedReader::new(fetch.clone(), from, 7, no_abort());

            let (bytes, end) = read_all(&mut reader, 16);

            assert!(end.is_ok() && bytes.is_empty(), "from {from}: {bytes:?} {end:?}");
            assert_eq!(fetch.opens(), Vec::<(u64, u64)>::new(), "from {from}");
        }
    }

    #[test]
    fn an_empty_buffer_reads_nothing_and_asks_for_nothing() {
        let fetch = MemFetch::new(pattern(30));
        let mut reader = ChunkedReader::new(fetch.clone(), 0, 7, no_abort());

        assert_eq!(reader.read(&mut []).unwrap(), 0);
        assert_eq!(fetch.opens(), Vec::<(u64, u64)>::new());
    }

    #[test]
    fn broken_requests_are_retried_from_where_they_stopped_up_to_three_attempts() {
        use Answer::*;
        // (case, answers, requests, the error kind and message when it gives up)
        let cases: Vec<(&str, Vec<Answer>, Vec<(u64, u64)>, Option<(ErrorKind, &str)>)> = vec![
            ("refused once", vec![Refuse], vec![(0, 10), (0, 10), (10, 20)], None),
            ("refused twice", vec![Refuse, Refuse], vec![(0, 10), (0, 10), (0, 10), (10, 20)], None),
            ("refused three times", vec![Refuse, Refuse, Refuse], vec![(0, 10), (0, 10), (0, 10)], Some((ErrorKind::Other, "refused 0-10"))),
            // A retry is a new request of a whole chunk from where the broken one stopped.
            ("cut short", vec![Short(4)], vec![(0, 10), (4, 14), (14, 20)], None),
            ("second chunk cut short", vec![Whole, Short(3)], vec![(0, 10), (10, 20), (13, 20)], None),
            ("empty three times", vec![Short(0), Short(0), Short(0)], vec![(0, 10), (0, 10), (0, 10)], Some((ErrorKind::UnexpectedEof, "download broke off"))),
            ("connection broke", vec![Break(6)], vec![(0, 10), (6, 16), (16, 20)], None),
            ("broke three times", vec![Break(0), Break(0), Break(0)], vec![(0, 10), (0, 10), (0, 10)], Some((ErrorKind::ConnectionReset, "connection reset by test"))),
            ("interruptions do not count", vec![Interrupted(5)], vec![(0, 10), (10, 20)], None),
            ("failures of different kinds add up", vec![Refuse, Short(0), Break(0)], vec![(0, 10), (0, 10), (0, 10)], Some((ErrorKind::ConnectionReset, "connection reset by test"))),
        ];
        let data = pattern(20);
        let mut bad = Vec::new();
        for (case, answers, expected_opens, failure) in cases {
            let fetch = MemFetch::scripted(data.clone(), &answers);
            let mut reader = ChunkedReader::new(fetch.clone(), 0, 10, no_abort());
            let (bytes, end) = read_all(&mut reader, 64);
            expect!(bad, fetch.opens() == expected_opens, "{case}: requests {:?}, expected {expected_opens:?}", fetch.opens());
            match (failure, end) {
                (None, Ok(())) => expect!(bad, bytes == data, "{case}: wrong bytes {bytes:?}"),
                (Some((kind, message)), Err(e)) => {
                    expect!(bad, e.kind() == kind && e.to_string() == message, "{case}: error {:?} {e}, expected {kind:?} {message}", e.kind());
                    expect!(bad, bytes.is_empty(), "{case}: bytes before giving up {bytes:?}");
                }
                (expected, actual) => bad.push(format!("{case}: ended with {actual:?}, expected failure {expected:?}")),
            }
        }
        finish(bad);
    }

    #[test]
    fn abort_stops_reading_with_connection_aborted() {
        let fetch = MemFetch::new(pattern(30));
        let abort = no_abort();
        let mut reader = ChunkedReader::new(fetch.clone(), 0, 10, abort.clone());
        let mut buf = [0u8; 64];
        let first = reader.read(&mut buf).expect("first chunk");

        abort.store(true, Ordering::Relaxed);
        let err = reader.read(&mut buf).expect_err("aborted");

        assert_eq!(first, 10);
        assert_eq!((err.kind(), err.to_string()), (ErrorKind::ConnectionAborted, "download cancelled".to_string()));
        assert_eq!(fetch.opens(), vec![(0, 10)]);
    }
}

mod range_source {
    use super::*;

    #[test]
    fn reads_the_whole_file_in_order_and_reports_its_length() {
        let data = pattern(250_000);
        let fetch = MemFetch::new(data.clone());
        let mut source = RangeSource::with_chunk_size(fetch.clone(), no_abort(), 100_000);

        let (bytes, end) = read_all(&mut source, 4096);

        assert!(end.is_ok(), "{end:?}");
        assert!(bytes == data, "the bytes differ from the file");
        assert_eq!(source.position(), 250_000);
        assert_eq!(fetch.opens(), vec![(0, 100_000), (100_000, 200_000), (200_000, 250_000)]);
        assert!(source.is_seekable());
        assert_eq!(source.byte_len(), Some(250_000));
    }

    #[test]
    fn seek_positions_from_start_end_and_current() {
        let data = pattern(1000);
        // (case, seeks, position, bytes read from there)
        let cases: [(&str, Vec<SeekFrom>, u64, std::ops::Range<usize>); 6] = [
            ("from the start", vec![SeekFrom::Start(250)], 250, 250..260),
            ("from the end", vec![SeekFrom::End(-5)], 995, 995..1000),
            ("back from the current position", vec![SeekFrom::Start(100), SeekFrom::Current(-30)], 70, 70..80),
            ("forward from the current position", vec![SeekFrom::Start(100), SeekFrom::Current(45)], 145, 145..155),
            ("to the end", vec![SeekFrom::End(0)], 1000, 1000..1000),
            ("past the end", vec![SeekFrom::Start(5000)], 5000, 1000..1000),
        ];
        let mut bad = Vec::new();
        for (case, seeks, position, expected) in cases {
            let mut source = RangeSource::new(MemFetch::new(data.clone()), no_abort());
            let mut at = None;
            for seek in seeks {
                at = Some(source.seek(seek).expect("seek"));
            }
            let mut buf = [0u8; 10];
            let mut got = Vec::new();
            while got.len() < expected.len() {
                let n = source.read(&mut buf[..expected.len() - got.len()]).expect("read");
                if n == 0 {
                    break;
                }
                got.extend_from_slice(&buf[..n]);
            }
            let after = source.read(&mut buf).expect("read");
            expect!(bad, at == Some(position), "{case}: seek returned {at:?}, expected {position}");
            expect!(bad, got == data[expected.clone()], "{case}: read {got:?}, expected bytes {expected:?}");
            expect!(bad, expected.end != 1000 || after == 0, "{case}: read {after} bytes at the end");
        }
        finish(bad);
    }

    #[test]
    fn seeking_before_the_start_is_an_error_that_keeps_the_position() {
        let mut source = RangeSource::new(MemFetch::new(pattern(1000)), no_abort());
        source.seek(SeekFrom::Start(20)).unwrap();

        let back = source.seek(SeekFrom::Current(-21)).expect_err("before the start");
        let end = source.seek(SeekFrom::End(-1001)).expect_err("before the start");

        for err in [back, end] {
            assert_eq!((err.kind(), err.to_string()), (ErrorKind::InvalidInput, "seek before the start".to_string()));
        }
        assert_eq!(source.position(), 20);
    }

    #[test]
    fn a_short_jump_forward_reads_through_and_a_longer_one_or_any_jump_back_reconnects() {
        const SKIP: u64 = 512 << 10;
        let data = pattern(1_600_000);
        let len = data.len() as u64;
        let fetch = MemFetch::new(data.clone());
        let mut source = RangeSource::with_chunk_size(fetch.clone(), no_abort(), 4 << 20);
        let mut buf = [0u8; 100];
        let mut read_at = |source: &mut RangeSource, at: u64| {
            source.seek(SeekFrom::Start(at)).unwrap();
            source.read_exact(&mut buf).unwrap();
            assert!(buf[..] == data[at as usize..at as usize + 100], "bytes at {at}");
        };

        read_at(&mut source, 0);
        read_at(&mut source, 100 + SKIP);
        let opens_after_short_jump = fetch.opens();
        read_at(&mut source, 200 + SKIP + SKIP + 1);
        read_at(&mut source, 50);

        assert_eq!(opens_after_short_jump, vec![(0, len)], "a jump of exactly 512 KiB reads through");
        assert_eq!(fetch.opens(), vec![(0, len), (200 + 2 * SKIP + 1, len), (50, len)]);
    }
}

// ---- the sidx and the fragment layout ----

fn bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    [&((payload.len() + 8) as u32).to_be_bytes()[..], kind, payload].concat()
}

fn large_bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    [&1u32.to_be_bytes()[..], kind, &((payload.len() + 16) as u64).to_be_bytes()[..], payload].concat()
}

/// A `sidx` payload: `refs` are (reference word, duration in ticks).
fn sidx(version: u8, timescale: u32, first_offset: u64, refs: &[(u32, u32)], count: u16) -> Vec<u8> {
    let mut p = vec![version, 0, 0, 0];
    p.extend(1u32.to_be_bytes());
    p.extend(timescale.to_be_bytes());
    if version == 0 {
        p.extend(0u32.to_be_bytes());
        p.extend((first_offset as u32).to_be_bytes());
    } else {
        p.extend(0u64.to_be_bytes());
        p.extend(first_offset.to_be_bytes());
    }
    p.extend(0u16.to_be_bytes());
    p.extend(count.to_be_bytes());
    for (word, duration) in refs {
        p.extend(word.to_be_bytes());
        p.extend(duration.to_be_bytes());
        p.extend(0x9000_0000u32.to_be_bytes());
    }
    p
}

const REFS: [(u32, u32); 3] = [(100, 2000), (150, 2500), (120, 1500)];

fn ftyp() -> Vec<u8> {
    bx(b"ftyp", b"iso6\0\0\0\0")
}

fn moov() -> Vec<u8> {
    bx(b"moov", &[7u8; 24])
}

fn moof() -> Vec<u8> {
    bx(b"moof", &[9u8; 8])
}

fn sidx_box(version: u8) -> Vec<u8> {
    bx(b"sidx", &sidx(version, 1000, 0, &REFS, 3))
}

/// ftyp 0..16, moov 16..48, then the sidx, then a moof.
fn file(parts: &[Vec<u8>]) -> Vec<u8> {
    parts.concat()
}

/// The layout of `REFS` at 1000 ticks per second with the first fragment at `head_len`.
fn layout_at(head_len: u64) -> Layout {
    Layout {
        head_len,
        fragments: vec![
            Fragment { offset: head_len, size: 100, start: 0.0, duration: 2.0 },
            Fragment { offset: head_len + 100, size: 150, start: 2.0, duration: 2.5 },
            Fragment { offset: head_len + 250, size: 120, start: 4.5, duration: 1.5 },
        ],
    }
}

mod layout {
    use super::*;

    #[test]
    fn reads_the_fragments_of_version_0_and_1_indexes() {
        let free24 = bx(b"free", &[0u8; 16]);
        let cases: [(&str, Vec<u8>, u64); 6] = [
            ("version 0", file(&[ftyp(), moov(), sidx_box(0), moof()]), 116),
            ("version 1", file(&[ftyp(), moov(), sidx_box(1), moof()]), 124),
            ("sidx with a 64-bit size", file(&[ftyp(), moov(), large_bx(b"sidx", &sidx(0, 1000, 0, &REFS, 3)), moof()]), 124),
            (
                "a gap between the sidx and the first fragment",
                file(&[ftyp(), moov(), bx(b"sidx", &sidx(0, 1000, 24, &REFS, 3)), free24.clone(), moof()]),
                140,
            ),
            ("moov after the sidx, media data first", file(&[ftyp(), sidx_box(0), moov(), bx(b"mdat", &[1; 8])]), 84),
            ("a 64-bit sized box before the index", file(&[ftyp(), large_bx(b"moov", &[7; 16]), sidx_box(0), moof()]), 116),
        ];
        let mut bad = Vec::new();
        for (case, bytes, head_len) in cases {
            let actual = parse_layout(&bytes);
            let expected = Ok(layout_at(head_len));
            expect!(bad, actual == expected, "{case}: {actual:?}\nexpected {expected:?}");
        }
        finish(bad);
    }

    #[test]
    fn a_short_start_asks_for_the_bytes_up_to_the_next_box_header() {
        let full = file(&[ftyp(), moov(), sidx_box(0), moof()]);
        let cases: [(&str, usize, u64); 5] = [
            ("nothing", 0, 16),
            ("less than one header", 15, 16),
            ("the first box and a bit", 20, 32),
            ("into the moov", 40, 64),
            ("one byte short of the moof header", 131, 132),
        ];
        let mut bad = Vec::new();
        for (case, have, need) in cases {
            let actual = parse_layout(&full[..have]);
            expect!(bad, actual == Err(LayoutError::NeedBytes(need)), "{case}: {actual:?}, expected NeedBytes({need})");
        }
        finish(bad);
    }

    #[test]
    fn a_header_of_exactly_16_mb_is_still_read_and_one_byte_more_is_not() {
        let at_limit = file(&[ftyp(), [&16_777_200u32.to_be_bytes()[..], b"free", &[0; 8]].concat()]);
        let over = file(&[ftyp(), [&16_777_201u32.to_be_bytes()[..], b"free", &[0; 8]].concat()]);

        assert_eq!(parse_layout(&at_limit), Err(LayoutError::NeedBytes(16_777_232)));
        assert_eq!(parse_layout(&over), Err(LayoutError::Invalid("not a DASH MP4 file: no fragment in the first 16 MB".into())));
    }

    #[test]
    fn files_that_are_not_dash_are_invalid_with_the_reason() {
        let zero_size = [&0u32.to_be_bytes()[..], b"moov", &[0; 8]].concat();
        let tiny = [&4u32.to_be_bytes()[..], b"ftyp", &[0; 8]].concat();
        let tiny_large = [&1u32.to_be_bytes()[..], b"ftyp", &7u64.to_be_bytes()[..]].concat();
        let huge = [&1u32.to_be_bytes()[..], b"free", &u64::MAX.to_be_bytes()[..]].concat();
        let cut = sidx(0, 1000, 0, &REFS[..2], 3);
        let cases: [(&str, Vec<u8>, &str); 10] = [
            ("box size 0", file(&[ftyp(), zero_size]), "a box runs to the end of the file before any fragment"),
            ("box shorter than its header", tiny, "a box is shorter than its header"),
            ("64-bit size shorter than its header", tiny_large, "a box is shorter than its header"),
            ("box size overflows", file(&[ftyp(), huge, moof()]), "a box size overflows"),
            ("no moov", file(&[ftyp(), sidx_box(0), moof()]), "no moov box before the first fragment"),
            ("no sidx", file(&[ftyp(), moov(), moof()]), "no sidx fragment index"),
            ("zero timescale", file(&[ftyp(), moov(), bx(b"sidx", &sidx(0, 0, 0, &REFS, 3)), moof()]), "the sidx has a zero timescale"),
            (
                "nested index",
                file(&[ftyp(), moov(), bx(b"sidx", &sidx(0, 1000, 0, &[(100, 2000), (0x8000_0000 | 150, 2500)], 2)), moof()]),
                "nested fragment indexes are not supported",
            ),
            ("no references", file(&[ftyp(), moov(), bx(b"sidx", &sidx(1, 1000, 0, &[], 0)), moof()]), "the sidx lists no fragments"),
            ("fewer references than counted", file(&[ftyp(), moov(), bx(b"sidx", &cut), moof()]), "the sidx is cut short"),
        ];
        let mut bad = Vec::new();
        for (case, bytes, reason) in cases {
            let actual = parse_layout(&bytes);
            let expected = Err(LayoutError::Invalid(format!("not a DASH MP4 file: {reason}")));
            expect!(bad, actual == expected, "{case}: {actual:?}, expected {expected:?}");
        }
        finish(bad);
    }

    #[test]
    fn fragment_at_is_the_last_fragment_starting_at_or_before_the_time() {
        let layout = layout_at(116);
        let cases = [(-1.0, 0), (0.0, 0), (1.999, 0), (2.0, 1), (4.499, 1), (4.5, 2), (5.99, 2), (6.0, 2), (100.0, 2)];
        let mut bad = Vec::new();
        for (secs, expected) in cases {
            let actual = layout.fragment_at(secs);
            expect!(bad, actual == expected, "fragment_at({secs}) = {actual}, expected {expected}");
        }
        finish(bad);
        assert_eq!(layout.duration(), 6.0);
    }

    #[test]
    fn an_empty_layout_has_no_duration_and_points_at_the_first_fragment() {
        let empty = Layout { head_len: 0, fragments: vec![] };

        assert_eq!(empty.duration(), 0.0);
        assert_eq!(empty.fragment_at(3.0), 0);
    }

    #[test]
    fn free_box_header_spans_the_gap_or_refuses_gaps_too_small_for_a_header() {
        let max32 = u32::MAX as u64;
        let cases: [(u64, Option<Vec<u8>>); 8] = [
            (0, Some(vec![])),
            (1, None),
            (7, None),
            (8, Some([&8u32.to_be_bytes()[..], b"free"].concat())),
            (1000, Some([&1000u32.to_be_bytes()[..], b"free"].concat())),
            (max32, Some([&u32::MAX.to_be_bytes()[..], b"free"].concat())),
            (max32 + 1, Some([&1u32.to_be_bytes()[..], b"free", &(max32 + 1).to_be_bytes()[..]].concat())),
            (u64::MAX, Some([&1u32.to_be_bytes()[..], b"free", &u64::MAX.to_be_bytes()[..]].concat())),
        ];
        let mut bad = Vec::new();
        for (gap, expected) in cases {
            let actual = free_box_header(gap);
            expect!(bad, actual == expected, "free_box_header({gap}) = {actual:?}, expected {expected:?}");
        }
        finish(bad);
    }
}

mod spliced_source {
    use super::*;

    const LEN: usize = 1000;
    const HEAD: usize = 100;
    const TAIL_FROM: usize = 950;
    const JUMP: u64 = 400;

    fn spliced(fetch: Arc<MemFetch>, jump_to: u64, with_tail: bool) -> Result<SplicedSource, String> {
        let data = &fetch.data;
        let head: Arc<[u8]> = data[..HEAD].into();
        let tail: Arc<[u8]> = if with_tail { data[TAIL_FROM..].into() } else { Arc::from(Vec::new()) };
        SplicedSource::new(head, tail, jump_to, RangeSource::new(fetch.clone(), no_abort()))
    }

    /// What the splice at 400 must look like: the header, a `free` box of 300 bytes (its
    /// 8-byte header, then zeros), then the file from 400 on.
    fn expected_bytes(data: &[u8]) -> Vec<u8> {
        let mut out = data[..HEAD].to_vec();
        out.extend([0, 0, 1, 44]);
        out.extend(b"free");
        out.extend(vec![0u8; 292]);
        out.extend_from_slice(&data[400..]);
        out
    }

    #[test]
    fn serves_the_header_a_free_box_over_the_skipped_part_then_the_file() {
        let fetch = MemFetch::new(pattern(LEN));
        let mut source = spliced(fetch.clone(), JUMP, true).expect("splice");

        let (bytes, end) = read_all(&mut source, 64);

        assert!(end.is_ok(), "{end:?}");
        assert_eq!(bytes, expected_bytes(&fetch.data));
        assert_eq!(fetch.opens(), vec![(400, LEN as u64)], "only the part after the jump is downloaded");
        assert!(source.is_seekable());
        assert_eq!(source.byte_len(), Some(LEN as u64));
    }

    #[test]
    fn every_byte_keeps_its_offset_after_a_seek() {
        let data = pattern(LEN);
        let spliced_view = expected_bytes(&data);
        // (case, seek, bytes to read, downloads)
        let cases: [(&str, SeekFrom, usize, Vec<(u64, u64)>); 8] = [
            ("inside the header", SeekFrom::Start(50), 30, vec![]),
            ("across the header into the free box", SeekFrom::Start(95), 12, vec![]),
            ("inside the free box header", SeekFrom::Start(102), 6, vec![]),
            ("across the free box into the file", SeekFrom::Start(390), 20, vec![(400, 1000)]),
            ("inside the file", SeekFrom::Start(600), 25, vec![(600, 1000)]),
            ("across the file into the tail", SeekFrom::Start(940), 20, vec![(940, 1000)]),
            ("inside the tail", SeekFrom::Start(960), 40, vec![]),
            ("from the end", SeekFrom::End(-3), 3, vec![]),
        ];
        let mut bad = Vec::new();
        for (case, seek, n, expected_opens) in cases {
            let fetch = MemFetch::new(data.clone());
            let mut source = spliced(fetch.clone(), JUMP, true).expect("splice");
            let at = source.seek(seek).expect("seek") as usize;
            let mut got = vec![0u8; n];
            let read = source.read_exact(&mut got);
            expect!(bad, read.is_ok(), "{case}: {read:?}");
            expect!(bad, got == spliced_view[at..at + n], "{case}: at {at} read {got:?}, expected {:?}", &spliced_view[at..at + n]);
            expect!(bad, fetch.opens() == expected_opens, "{case}: downloads {:?}, expected {expected_opens:?}", fetch.opens());
        }
        finish(bad);
    }

    #[test]
    fn at_and_past_the_end_reads_nothing_and_before_the_start_is_an_error() {
        let fetch = MemFetch::new(pattern(LEN));
        let mut source = spliced(fetch.clone(), JUMP, true).expect("splice");
        let mut buf = [7u8; 8];

        assert_eq!(source.seek(SeekFrom::End(0)).unwrap(), 1000);
        assert_eq!(source.read(&mut buf).unwrap(), 0);
        assert_eq!(source.seek(SeekFrom::Start(1500)).unwrap(), 1500);
        assert_eq!(source.read(&mut buf).unwrap(), 0);
        let err = source.seek(SeekFrom::End(-1001)).expect_err("before the start");
        assert_eq!((err.kind(), err.to_string()), (ErrorKind::InvalidInput, "seek before the start".to_string()));
        assert_eq!(fetch.opens(), Vec::<(u64, u64)>::new());
    }

    #[test]
    fn jumping_to_the_first_fragment_needs_no_free_box() {
        let fetch = MemFetch::new(pattern(LEN));
        let mut source = spliced(fetch.clone(), HEAD as u64, true).expect("splice");

        let (bytes, end) = read_all(&mut source, 64);

        assert!(end.is_ok(), "{end:?}");
        assert!(bytes == fetch.data, "the bytes differ from the file");
        assert_eq!(fetch.opens(), vec![(100, 1000)]);
    }

    #[test]
    fn without_a_cached_tail_the_end_is_downloaded() {
        let fetch = MemFetch::new(pattern(LEN));
        let mut source = spliced(fetch.clone(), JUMP, false).expect("splice");
        source.seek(SeekFrom::Start(960)).unwrap();

        let (bytes, end) = read_all(&mut source, 64);

        assert!(end.is_ok(), "{end:?}");
        assert!(bytes == fetch.data[960..], "the bytes differ from the file");
        assert_eq!(fetch.opens(), vec![(960, 1000)]);
    }

    #[test]
    fn a_jump_into_the_header_or_leaving_a_gap_too_small_for_a_box_is_refused() {
        let message = "cannot jump into the middle of the file header";
        for jump_to in [0, 50, 99, 101, 107] {
            let result = spliced(MemFetch::new(pattern(LEN)), jump_to, true).err();
            assert_eq!(result.as_deref(), Some(message), "jump to {jump_to}");
        }
        let mut smallest = spliced(MemFetch::new(pattern(LEN)), 108, true).expect("a gap of 8 is the smallest free box");
        let mut filler = [0u8; 8];
        smallest.seek(SeekFrom::Start(100)).unwrap();
        smallest.read_exact(&mut filler).unwrap();
        assert_eq!(&filler, b"\0\0\0\x08free");
    }
}

// ---- DASH files and demuxers over the fixtures ----

const VIDEO_OFFSETS: [u64; 6] = [856, 34048, 74619, 109615, 150507, 184979];
const VIDEO_SIZES: [u64; 6] = [33192, 40571, 34996, 40892, 34472, 36874];
const VIDEO_LEN: u64 = 222_015;
const AUDIO_TICKS: [u64; 4] = [133_120, 133_120, 133_120, 130_864];

fn video_layout() -> Layout {
    Layout {
        head_len: 856,
        fragments: (0..6)
            .map(|i| Fragment { offset: VIDEO_OFFSETS[i], size: VIDEO_SIZES[i], start: 2.0 * i as f64, duration: 2.0 })
            .collect(),
    }
}

fn dash_file(name: &str, first: usize) -> (Arc<MemFetch>, DashFile) {
    let data = fixture_bytes(name);
    let head = data[..first.min(data.len())].to_vec();
    let fetch = MemFetch::new(data);
    let file = DashFile::from_fetch(fetch.clone(), head).unwrap_or_else(|e| panic!("{name}: {e}"));
    (fetch, file)
}

fn open_dash(name: &str) -> (Arc<MemFetch>, DashDemuxer) {
    let (fetch, file) = dash_file(name, 4096);
    let demux = DashDemuxer::open(file, no_abort()).unwrap_or_else(|e| panic!("{name}: {e}"));
    (fetch, demux)
}

/// Every packet left, as (pts, bytes, is video).
fn rest(demux: &mut dyn Demuxer) -> Vec<(f64, Vec<u8>, bool)> {
    let mut out = Vec::new();
    while let Some(p) = demux.next_packet().expect("packet") {
        out.push(match p {
            Packet::Video { pts, data } => (pts, data.to_vec(), true),
            Packet::Audio { pts, packet, .. } => (pts, packet.data.to_vec(), false),
        });
        assert!(out.len() < 100_000, "runaway demuxer");
    }
    out
}

fn take(demux: &mut dyn Demuxer, n: usize) -> Vec<(f64, Vec<u8>, bool)> {
    (0..n)
        .map(|_| match demux.next_packet().expect("packet").expect("not the end yet") {
            Packet::Video { pts, data } => (pts, data.to_vec(), true),
            Packet::Audio { pts, packet, .. } => (pts, packet.data.to_vec(), false),
        })
        .collect()
}

/// The whole video fixture as symphonia reads it from disk: the reference packets.
fn reference_video() -> Vec<(f64, Vec<u8>, bool)> {
    let mut demux = playr::demux::open_path(&common::fixture("dash_video.mp4")).expect("open from disk");
    rest(demux.as_mut())
}

/// Where two packet lists differ: count, kind, bytes, or a time more than a nanosecond off
/// (a time computed as fragment start + offset may differ from the file's in the last bit).
fn differences(actual: &[(f64, Vec<u8>, bool)], expected: &[(f64, Vec<u8>, bool)]) -> Vec<String> {
    let mut out = Vec::new();
    if actual.len() != expected.len() {
        out.push(format!("{} packets, expected {}", actual.len(), expected.len()));
    }
    for (i, (a, e)) in actual.iter().zip(expected).enumerate() {
        if a.2 != e.2 || a.1 != e.1 || (a.0 - e.0).abs() > 1e-9 {
            out.push(format!("packet {i}: at {} ({} bytes, video {}), expected at {} ({} bytes, video {})", a.0, a.1.len(), a.2, e.0, e.1.len(), e.2));
        }
        if out.len() > 5 {
            break;
        }
    }
    out
}

/// Whether an AVCC packet (4-byte NAL lengths) holds an IDR slice.
fn has_idr(data: &[u8]) -> bool {
    let mut at = 0;
    while at + 5 <= data.len() {
        let len = u32::from_be_bytes(data[at..at + 4].try_into().unwrap()) as usize;
        if data[at + 4] & 0x1f == 5 {
            return true;
        }
        at += 4 + len;
    }
    false
}

mod dash_file {
    use super::*;

    #[test]
    fn a_header_given_in_full_is_parsed_and_only_the_tail_is_fetched() {
        let (fetch, file) = dash_file("dash_video.mp4", 2000);

        assert_eq!(file.layout(), &video_layout());
        assert_eq!(fetch.opens(), vec![(VIDEO_LEN - 4096, VIDEO_LEN)]);
    }

    #[test]
    fn a_header_given_in_part_is_completed_box_by_box() {
        // ftyp 0..32, moov 32..744, sidx 744..856, first moof at 856: with 100 bytes the next
        // header is the sidx's at 744 (needs 760), then the moof's at 856 (needs 872).
        let (fetch, file) = dash_file("dash_video.mp4", 100);

        assert_eq!(file.layout(), &video_layout());
        assert_eq!(fetch.opens(), vec![(100, 760), (760, 872), (VIDEO_LEN - 4096, VIDEO_LEN)]);
    }

    #[test]
    fn the_audio_index_gives_four_fragments_of_the_listed_lengths() {
        let (_, file) = dash_file("dash_audio.m4a", 4096);
        let layout = file.layout();

        let starts: Vec<f64> = layout.fragments.iter().map(|f| f.start).collect();
        let durations: Vec<f64> = layout.fragments.iter().map(|f| f.duration).collect();
        let at = |ticks: u64| ticks as f64 / 44100.0;
        assert_eq!(layout.head_len, 764);
        assert_eq!(starts, [0.0, at(133_120), at(266_240), at(399_360)]);
        assert_eq!(durations, AUDIO_TICKS.map(at));
        assert_eq!(layout.duration(), at(530_224));
    }

    #[test]
    fn files_that_cannot_be_read_as_dash_are_errors() {
        let video = fixture_bytes("dash_video.mp4");
        let cut_short = "not a DASH MP4 file: the header is cut short";
        let cases: [(&str, Vec<u8>, usize, Vec<Answer>, String); 3] = [
            ("file shorter than its header", video[..500].to_vec(), 100, vec![], cut_short.to_string()),
            ("server sends nothing more", video.clone(), 100, vec![Answer::Short(0)], cut_short.to_string()),
            ("not an MP4 file", fixture_bytes("sine.wav"), 4096, vec![], "not a DASH MP4 file: no fragment in the first 16 MB".to_string()),
        ];
        let mut bad = Vec::new();
        for (case, data, first, answers, expected) in cases {
            let head = data[..first].to_vec();
            let result = DashFile::from_fetch(MemFetch::scripted(data, &answers), head).err();
            expect!(bad, result.as_deref() == Some(expected.as_str()), "{case}: {result:?}, expected {expected:?}");
        }
        finish(bad);
    }
}

mod dash_demuxer {
    use super::*;

    #[test]
    fn video_info_comes_from_the_header_and_the_duration_from_the_index() {
        let (_, demux) = open_dash("dash_video.mp4");
        let info = demux.info();

        assert_eq!(info.duration, Some(12.0));
        assert!(info.seekable);
        let video = info.video.as_ref().expect("video track");
        assert_eq!((&video.codec, video.width, video.height), (&VideoCodec::H264, 160, 120));
        assert!(info.audio.is_none());
    }

    #[test]
    fn plays_the_same_packets_as_the_file_read_from_disk_with_continuous_times() {
        let (_, mut demux) = open_dash("dash_video.mp4");
        let reference = reference_video();

        let packets = rest(&mut demux);

        assert_eq!(packets.len(), 300, "12 s at 25 fps");
        assert_eq!(differences(&packets, &reference), Vec::<String>::new(), "against the file read from disk");
        let mut times: Vec<f64> = packets.iter().map(|p| p.0).collect();
        times.sort_by(f64::total_cmp);
        let off: Vec<(usize, f64)> = times.iter().copied().enumerate().filter(|&(i, t)| (t - i as f64 * 0.04).abs() > 1e-6).collect();
        assert_eq!(off, vec![], "presentation times are 0, 0.04, 0.08, ...");
    }

    #[test]
    fn audio_packets_follow_one_another_from_the_start() {
        let (_, mut demux) = open_dash("dash_audio.m4a");
        let step = 1024.0 / 44100.0;

        let packets = rest(&mut demux);

        assert_eq!(demux.info().duration, Some(530_224.0 / 44100.0));
        assert!(matches!(demux.info().audio, Some(AudioTrack::Symphonia(_))));
        assert_eq!(packets.len(), 530_224usize.div_ceil(1024));
        let gaps: Vec<(usize, f64)> = packets
            .windows(2)
            .enumerate()
            .filter(|(_, w)| (w[1].0 - w[0].0 - step).abs() > 1e-6)
            .map(|(i, w)| (i, w[1].0 - w[0].0))
            .collect();
        assert_eq!(gaps, vec![], "every packet starts 1024 samples after the one before");
        assert!(packets[0].0.abs() <= step, "first packet at {}", packets[0].0);
    }

    #[test]
    fn seek_lands_on_the_start_of_the_fragment_holding_the_time_at_a_keyframe() {
        let reference = reference_video();
        let cases = [(5.0, 4.0), (2.0, 2.0), (11.9, 10.0), (50.0, 10.0), (1.99, 0.0), (-3.0, 0.0), (7.5, 6.0)];
        let (_, mut demux) = open_dash("dash_video.mp4");
        let mut bad = Vec::new();
        for (target, landing) in cases {
            let landed = demux.seek(target).expect("seek");
            let first = take(&mut demux, 1).remove(0);
            expect!(bad, landed == landing, "seek({target}) = {landed}, expected {landing}");
            expect!(bad, first.0 == landing && first.2, "seek({target}): next packet at {} (video {})", first.0, first.2);
            expect!(bad, has_idr(&first.1), "seek({target}): the packet at {} is not a keyframe", first.0);
            let index = (landing * 25.0) as usize;
            let diff = differences(std::slice::from_ref(&first), &reference[index..=index]);
            expect!(bad, diff.is_empty(), "seek({target}): against packet {index} of the file: {diff:?}");
        }
        finish(bad);
    }

    #[test]
    fn after_a_seek_the_rest_of_the_file_plays_unchanged() {
        let reference = reference_video();
        let (_, mut demux) = open_dash("dash_video.mp4");

        demux.seek(5.0).expect("seek");
        let packets = rest(&mut demux);

        assert_eq!(differences(&packets, &reference[100..]), Vec::<String>::new(), "against the last 200 packets of the file");
    }

    #[test]
    fn a_seek_back_into_the_fragment_being_read_replays_without_downloading_again() {
        let reference = reference_video();
        let (fetch, mut demux) = open_dash("dash_video.mp4");
        let tail = (VIDEO_LEN - 4096, VIDEO_LEN);

        let before = take(&mut demux, 30);
        let opens_before = fetch.opens();
        let landed = demux.seek(1.0).expect("seek");
        let again = rest(&mut demux);

        assert_eq!(differences(&before, &reference[..30]), Vec::<String>::new(), "before the seek");
        assert_eq!(landed, 0.0);
        assert_eq!(differences(&again, &reference), Vec::<String>::new(), "after the seek back: the whole file again");
        assert_eq!(opens_before, vec![tail, (856, VIDEO_LEN)]);
        assert_eq!(fetch.opens(), opens_before, "no request for the replay");
    }

    #[test]
    fn a_seek_back_replays_only_while_the_packets_read_fit_the_replay_limit() {
        let reference = reference_video();
        let read: usize = reference[..30].iter().map(|p| p.1.len()).sum();
        let tail = (VIDEO_LEN - 4096, VIDEO_LEN);
        let cases = [
            ("exactly at the limit: replayed", read, vec![tail, (856, VIDEO_LEN)]),
            ("one byte over the limit: downloaded again", read - 1, vec![tail, (856, VIDEO_LEN), (856, VIDEO_LEN)]),
        ];
        let mut bad = Vec::new();
        for (case, limit, expected_opens) in cases {
            let (fetch, file) = dash_file("dash_video.mp4", 4096);
            let mut demux = DashDemuxer::open(file.with_replay_limit(limit), no_abort()).expect("open");
            take(&mut demux, 30);
            let landed = demux.seek(1.0).expect("seek");
            let again = rest(&mut demux);
            expect!(bad, landed == 0.0, "{case}: landed at {landed}");
            let diff = differences(&again, &reference);
            expect!(bad, diff.is_empty(), "{case}: after the seek back: {diff:?}");
            expect!(bad, fetch.opens() == expected_opens, "{case}: requests {:?}, expected {expected_opens:?}", fetch.opens());
        }
        finish(bad);
    }

    #[test]
    fn seeking_to_another_fragment_downloads_from_it_and_never_asks_for_the_tail_again() {
        let (fetch, mut demux) = open_dash("dash_video.mp4");
        let tail = (VIDEO_LEN - 4096, VIDEO_LEN);
        take(&mut demux, 5);

        demux.seek(5.0).expect("seek");
        take(&mut demux, 5);
        demux.seek(0.5).expect("seek");
        take(&mut demux, 5);
        demux.seek(11.0).expect("seek");
        take(&mut demux, 5);

        assert_eq!(
            fetch.opens(),
            vec![tail, (856, VIDEO_LEN), (VIDEO_OFFSETS[2], VIDEO_LEN), (856, VIDEO_LEN), (VIDEO_OFFSETS[5], VIDEO_LEN)]
        );
    }

    #[test]
    fn audio_seek_lands_on_its_own_fragment_start() {
        let (_, mut demux) = open_dash("dash_audio.m4a");
        let start1 = 133_120.0 / 44100.0;

        let landed = demux.seek(5.0).expect("seek");
        let first = take(&mut demux, 1).remove(0);

        assert_eq!(landed, start1);
        assert!((first.0 - start1).abs() < 1e-6 && !first.2, "next packet {:?}", (first.0, first.2));
    }
}

// ---- MergedDemuxer ----

/// Hands out video packets tagged with `tag` at the given times; a seek lands on the last
/// multiple of `grid` at or before the target.
struct Fake {
    info: MediaInfo,
    times: Vec<f64>,
    tag: u8,
    pos: usize,
    grid: f64,
    fail: Option<&'static str>,
    rendezvous: Option<Arc<(Mutex<u32>, Condvar)>>,
}

impl Fake {
    fn new(tag: u8, times: &[f64], grid: f64) -> Self {
        Self { info: MediaInfo::default(), times: times.to_vec(), tag, pos: 0, grid, fail: None, rendezvous: None }
    }
}

impl Demuxer for Fake {
    fn info(&self) -> &MediaInfo {
        &self.info
    }

    fn next_packet(&mut self) -> Result<Option<Packet>, String> {
        let p = self.times.get(self.pos).map(|&pts| Packet::Video { pts, data: vec![self.tag].into() });
        self.pos += 1;
        Ok(p)
    }

    fn seek(&mut self, secs: f64) -> Result<f64, String> {
        if let Some(pair) = &self.rendezvous {
            let (count, arrived) = &**pair;
            let mut n = count.lock().unwrap();
            *n += 1;
            arrived.notify_all();
            let (n, timeout) = arrived.wait_timeout_while(n, Duration::from_secs(5), |n| *n < 2).unwrap();
            if timeout.timed_out() && *n < 2 {
                return Err("the other side was not seeking at the same time".into());
            }
        }
        if let Some(e) = self.fail {
            return Err(e.into());
        }
        let landing = (secs / self.grid).floor() * self.grid;
        self.pos = self.times.iter().position(|&t| t >= landing).unwrap_or(self.times.len());
        Ok(landing)
    }
}

fn merged_order(m: &mut MergedDemuxer) -> Vec<(char, f64)> {
    let mut out = Vec::new();
    while let Some(Packet::Video { pts, data }) = m.next_packet().expect("packet") {
        out.push((data[0] as char, pts));
    }
    out
}

mod merged_demuxer {
    use super::*;

    #[test]
    fn hands_out_the_earlier_of_the_two_next_packets_video_first_on_a_tie() {
        let video = Fake::new(b'v', &[0.0, 0.08, 0.04, 0.12, 0.16], 2.0);
        let audio = Fake::new(b'a', &[0.0, 0.05, 0.10, 0.15, 0.16, 0.25], 2.0);
        let mut merged = MergedDemuxer::new(Box::new(video), Box::new(audio));

        let order = merged_order(&mut merged);

        assert_eq!(
            order,
            [
                ('v', 0.0),
                ('a', 0.0),
                ('a', 0.05),
                ('v', 0.08),
                ('v', 0.04),
                ('a', 0.10),
                ('v', 0.12),
                ('a', 0.15),
                ('v', 0.16),
                ('a', 0.16),
                ('a', 0.25)
            ]
        );
        assert!(merged.next_packet().unwrap().is_none(), "the end stays the end");
    }

    #[test]
    fn one_side_empty_gives_the_other_side_alone() {
        let mut no_video = MergedDemuxer::new(Box::new(Fake::new(b'v', &[], 1.0)), Box::new(Fake::new(b'a', &[0.0, 1.0], 1.0)));
        let mut no_audio = MergedDemuxer::new(Box::new(Fake::new(b'v', &[0.0, 0.5], 1.0)), Box::new(Fake::new(b'a', &[], 1.0)));

        assert_eq!(merged_order(&mut no_video), [('a', 0.0), ('a', 1.0)]);
        assert_eq!(merged_order(&mut no_audio), [('v', 0.0), ('v', 0.5)]);
    }

    #[test]
    fn info_takes_the_picture_from_the_first_and_the_sound_from_the_second() {
        let mut video = Fake::new(b'v', &[], 1.0);
        video.info = MediaInfo {
            container: "video-box".into(),
            duration: Some(10.0),
            seekable: true,
            video: Some(VideoTrack { codec: VideoCodec::H264, width: 640, height: 360, extra: vec![1, 2] }),
            audio: Some(AudioTrack::Pcm { rate: 8000, channels: 1, bits: 8 }),
            notes: vec!["video note".into()],
        };
        let mut audio = Fake::new(b'a', &[], 1.0);
        audio.info = MediaInfo {
            container: "audio-box".into(),
            duration: Some(10.5),
            seekable: true,
            video: Some(VideoTrack { codec: VideoCodec::Hevc, width: 2, height: 3, extra: vec![] }),
            audio: Some(AudioTrack::Pcm { rate: 48000, channels: 2, bits: 16 }),
            notes: vec!["audio note".into()],
        };

        let merged = MergedDemuxer::new(Box::new(video), Box::new(audio));
        let info = merged.info();

        assert_eq!(info.container, "video-box");
        assert_eq!(info.duration, Some(10.5));
        assert!(info.seekable);
        let v = info.video.as_ref().expect("video");
        assert_eq!((&v.codec, v.width, v.height, &v.extra), (&VideoCodec::H264, 640, 360, &vec![1, 2]));
        assert!(matches!(info.audio, Some(AudioTrack::Pcm { rate: 48000, channels: 2, bits: 16 })), "{:?}", info.audio);
        assert_eq!(info.notes, ["video note", "audio note"]);
    }

    #[test]
    fn duration_is_the_longer_known_one_and_seekable_needs_both() {
        // (video duration, audio duration, video seekable, audio seekable, duration, seekable)
        let cases = [
            (Some(12.0), Some(11.0), true, true, Some(12.0), true),
            (None, Some(3.0), true, false, Some(3.0), false),
            (Some(4.0), None, false, true, Some(4.0), false),
            (None, None, false, false, None, false),
        ];
        let mut bad = Vec::new();
        for (vd, ad, vs, as_, duration, seekable) in cases {
            let mut video = Fake::new(b'v', &[], 1.0);
            video.info = MediaInfo { duration: vd, seekable: vs, ..Default::default() };
            let mut audio = Fake::new(b'a', &[], 1.0);
            audio.info = MediaInfo { duration: ad, seekable: as_, ..Default::default() };
            let merged = MergedDemuxer::new(Box::new(video), Box::new(audio));
            let actual = (merged.info().duration, merged.info().seekable);
            expect!(bad, actual == (duration, seekable), "{vd:?}/{ad:?}, {vs}/{as_}: {actual:?}, expected {:?}", (duration, seekable));
        }
        finish(bad);
    }

    #[test]
    fn seek_moves_both_sides_returns_the_earlier_landing_and_drops_packets_read_before() {
        let video = Fake::new(b'v', &[0.0, 2.0, 4.0, 6.0, 8.0], 2.0);
        let audio = Fake::new(b'a', &[0.0, 2.5, 5.0, 7.5], 2.5);
        let mut merged = MergedDemuxer::new(Box::new(video), Box::new(audio));
        merged.next_packet().unwrap();

        let landed = merged.seek(7.0).expect("seek");

        assert_eq!(landed, 5.0);
        assert_eq!(merged_order(&mut merged), [('a', 5.0), ('v', 6.0), ('a', 7.5), ('v', 8.0)]);
    }

    #[test]
    fn both_sides_seek_at_the_same_time() {
        let pair = Arc::new((Mutex::new(0u32), Condvar::new()));
        let mut video = Fake::new(b'v', &[0.0, 2.0], 2.0);
        video.rendezvous = Some(pair.clone());
        let mut audio = Fake::new(b'a', &[0.0, 3.0], 3.0);
        audio.rendezvous = Some(pair);
        let mut merged = MergedDemuxer::new(Box::new(video), Box::new(audio));

        assert_eq!(merged.seek(3.5), Ok(2.0));
    }

    #[test]
    fn a_failed_seek_on_either_side_is_the_error() {
        let cases = [(Some("video broke"), None, "video broke"), (None, Some("audio broke"), "audio broke")];
        let mut bad = Vec::new();
        for (video_fail, audio_fail, expected) in cases {
            let mut video = Fake::new(b'v', &[0.0], 1.0);
            video.fail = video_fail;
            let mut audio = Fake::new(b'a', &[0.0], 1.0);
            audio.fail = audio_fail;
            let mut merged = MergedDemuxer::new(Box::new(video), Box::new(audio));
            let actual = merged.seek(0.5);
            expect!(bad, actual == Err(expected.to_string()), "{actual:?}, expected Err({expected:?})");
        }
        finish(bad);
    }

    #[test]
    fn the_dash_fixtures_play_together() {
        let (_, video) = open_dash("dash_video.mp4");
        let (_, audio) = open_dash("dash_audio.m4a");
        let mut merged = MergedDemuxer::new(Box::new(video), Box::new(audio));

        let info = merged.info().clone();
        let landed = merged.seek(5.0).expect("seek");
        let packets = rest(&mut merged);

        let v = info.video.as_ref().expect("video");
        assert_eq!((&v.codec, v.width, v.height), (&VideoCodec::H264, 160, 120));
        assert!(matches!(&info.audio, Some(AudioTrack::Symphonia(p)) if p.sample_rate == Some(44100)), "{:?}", info.audio);
        assert_eq!(info.duration, Some(530_224.0 / 44100.0));
        assert_eq!(landed, 133_120.0 / 44100.0, "the audio fragment starts before the video one");
        assert_eq!(packets.first().map(|p| (p.0, p.2)), Some((133_120.0 / 44100.0, false)));
        let video_packets: Vec<_> = packets.iter().filter(|p| p.2).cloned().collect();
        assert_eq!(differences(&video_packets, &reference_video()[100..]), Vec::<String>::new(), "the picture resumes at its 4 s fragment");
    }
}

mod http_fetch {
    use super::*;

    #[test]
    fn connect_reads_the_start_and_the_length_and_sends_the_headers() {
        let data = fixture_bytes("dash_video.mp4");
        let server = RangeServer::start(vec![("/v.mp4", Mode::Ranged, data.clone())]);
        let headers = vec![("X-Token".to_string(), "t-7".to_string())];

        let (fetch, first) = HttpFetch::connect(&server.url("/v.mp4?sig=1"), &headers, 100).unwrap_or_else(|e| panic!("{e}"));
        let mut middle = Vec::new();
        fetch.open(500, 600).expect("open").read_to_end(&mut middle).unwrap();

        assert!(first == data[..100]);
        assert_eq!(fetch.len(), data.len() as u64);
        assert!(middle == data[500..600]);
        let requests = server.requests();
        let seen: Vec<(Option<&str>, Option<&str>)> = requests.iter().map(|r| (r.header("range"), r.header("x-token"))).collect();
        assert_eq!(seen, [(Some("bytes=0-99"), Some("t-7")), (Some("bytes=500-599"), Some("t-7"))]);
    }

    #[test]
    fn a_server_without_byte_ranges_is_refused_with_the_reason() {
        let data = pattern(500);
        let server = RangeServer::start(vec![("/whole", Mode::IgnoresRange, data.clone()), ("/norange", Mode::NoContentRange, data)]);
        let cases = [
            ("/whole?sig=secret", format!("{} ignored the byte range (HTTP 200)", server.url("/whole"))),
            ("/norange?sig=secret", format!("{} does not support ranged downloads", server.url("/norange"))),
        ];
        let mut bad = Vec::new();
        for (path, expected) in cases {
            let actual = HttpFetch::connect(&server.url(path), &[], 100).err();
            expect!(bad, actual.as_deref() == Some(expected.as_str()), "{path}: {actual:?}, expected {expected:?}");
        }
        finish(bad);
    }

    #[test]
    fn dash_file_connect_reads_the_layout_over_http() {
        let server = RangeServer::start(vec![("/v.mp4", Mode::Ranged, fixture_bytes("dash_video.mp4"))]);

        let file = DashFile::connect(&server.url("/v.mp4"), &[]).unwrap_or_else(|e| panic!("{e}"));

        assert_eq!(file.layout(), &video_layout());
    }
}
