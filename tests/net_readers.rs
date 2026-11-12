//! The byte-stream adapters in `playr::net`: `IcyReader` (metadata stripping) and
//! `PipeReader` / `spawn_pipe` (the download thread's reading end).

use std::io::{Cursor, ErrorKind, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use playr::net::{DEFAULT_STALL_TIMEOUT, IcyReader, PipeReader, TitleCallback, spawn_pipe, spawn_pipe_with_timeout};

/// Audio bytes whose value depends on the position, so dropped, repeated or reordered bytes
/// show up. 0xFF never occurs, so audio can not be mistaken for a metadata length either.
fn audio(len: usize, seed: usize) -> Vec<u8> {
    (0..len).map(|i| ((i * 7 + seed * 13) % 251) as u8).collect()
}

/// One ICY metadata block: a length byte counting 16-byte units, then the NUL-padded text.
fn meta_block(text: &[u8]) -> Vec<u8> {
    let units = text.len().div_ceil(16);
    assert!(units <= 255, "metadata text too long for one block");
    let mut out = vec![units as u8];
    out.extend_from_slice(text);
    out.resize(1 + units * 16, 0);
    out
}

fn title_block(title: &str) -> Vec<u8> {
    meta_block(format!("StreamTitle='{title}';").as_bytes())
}

fn collector() -> (Arc<Mutex<Vec<String>>>, TitleCallback) {
    let titles = Arc::new(Mutex::new(Vec::new()));
    let sink = titles.clone();
    (titles, Box::new(move |t| sink.lock().unwrap().push(t)))
}

/// Hands out at most `max` bytes per call, to move the block boundaries across read calls.
struct Dribble<R> {
    inner: R,
    max: usize,
}

impl<R: Read> Read for Dribble<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = buf.len().min(self.max);
        self.inner.read(&mut buf[..n])
    }
}

/// Reads to the end with a caller-sized buffer; returns the bytes and the final result.
fn read_all(reader: &mut impl Read, buf_len: usize) -> (Vec<u8>, std::io::Result<()>) {
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

mod icy_reader {
    use super::*;

    const METAINT: usize = 40;

    /// Three audio segments with a title, an empty block and another title between them.
    fn three_segment_stream() -> (Vec<u8>, Vec<u8>) {
        let (a, b, c, d) = (audio(METAINT, 1), audio(METAINT, 2), audio(METAINT, 3), audio(17, 4));
        let mut wire = Vec::new();
        wire.extend_from_slice(&a);
        wire.extend(title_block("First Artist - First Song"));
        wire.extend_from_slice(&b);
        wire.push(0);
        wire.extend_from_slice(&c);
        wire.extend(title_block("Second Artist - Second Song"));
        wire.extend_from_slice(&d);
        (wire, [a, b, c, d].concat())
    }

    #[test]
    fn strips_metadata_blocks_and_reports_titles_in_order() {
        let (wire, expected_audio) = three_segment_stream();
        let (titles, cb) = collector();
        let mut reader = IcyReader::new(Cursor::new(wire), METAINT, cb);

        let (got, end) = read_all(&mut reader, 4096);

        assert!(end.is_ok(), "clean end of stream expected, got {end:?}");
        assert_eq!(got, expected_audio);
        assert_eq!(*titles.lock().unwrap(), ["First Artist - First Song", "Second Artist - Second Song"]);
    }

    #[test]
    fn output_is_the_same_however_the_reads_straddle_the_block_boundaries() {
        let (wire, expected_audio) = three_segment_stream();
        // (bytes the source hands out per call, caller's buffer size)
        for (inner_max, buf_len) in [(1, 1), (1, 4096), (3, 5), (7, 64), (16, 39), (41, 40), (4096, 1), (4096, 4096)] {
            let (titles, cb) = collector();
            let source = Dribble { inner: Cursor::new(wire.clone()), max: inner_max };
            let mut reader = IcyReader::new(source, METAINT, cb);

            let (got, end) = read_all(&mut reader, buf_len);

            let case = format!("source chunk {inner_max}, buffer {buf_len}");
            assert!(end.is_ok(), "{case}: clean end of stream expected, got {end:?}");
            assert_eq!(got, expected_audio, "{case}");
            assert_eq!(
                *titles.lock().unwrap(),
                ["First Artist - First Song", "Second Artist - Second Song"],
                "{case}"
            );
        }
    }

    #[test]
    fn reports_a_title_only_when_it_changes() {
        let mut wire = Vec::new();
        let mut expected_audio = Vec::new();
        for (i, title) in ["Alpha", "Alpha", "Beta", "Beta", "Alpha"].iter().enumerate() {
            let seg = audio(METAINT, i);
            wire.extend_from_slice(&seg);
            wire.extend(title_block(title));
            expected_audio.extend(seg);
        }
        let (titles, cb) = collector();
        let mut reader = IcyReader::new(Cursor::new(wire), METAINT, cb);

        let (got, end) = read_all(&mut reader, 33);

        assert!(end.is_ok(), "clean end of stream expected, got {end:?}");
        assert_eq!(got, expected_audio);
        assert_eq!(*titles.lock().unwrap(), ["Alpha", "Beta", "Alpha"]);
    }

    #[test]
    fn empty_title_blocks_report_nothing_and_do_not_reset_the_current_title() {
        let mut wire = Vec::new();
        for (i, block) in [title_block("Kept"), title_block(""), vec![0], title_block("Kept")].into_iter().enumerate() {
            wire.extend(audio(METAINT, i));
            wire.extend(block);
        }
        let (titles, cb) = collector();
        let mut reader = IcyReader::new(Cursor::new(wire), METAINT, cb);

        let (got, end) = read_all(&mut reader, 4096);

        assert!(end.is_ok(), "clean end of stream expected, got {end:?}");
        assert_eq!(got.len(), 4 * METAINT);
        assert_eq!(*titles.lock().unwrap(), ["Kept"]);
    }

    #[test]
    fn handles_block_sizes_at_the_format_limits() {
        // 16 text bytes fill one unit exactly (no padding); 255 units is the largest block.
        let exact = "StreamTitle='x';";
        assert_eq!(exact.len(), 16);
        let long_title = "L".repeat(255 * 16 - "StreamTitle='';".len());
        let mut wire = audio(METAINT, 1);
        wire.extend(meta_block(exact.as_bytes()));
        wire.extend(audio(METAINT, 2));
        wire.extend(title_block(&long_title));
        wire.extend(audio(METAINT, 3));
        let (titles, cb) = collector();
        let mut reader = IcyReader::new(Dribble { inner: Cursor::new(wire), max: 100 }, METAINT, cb);

        let (got, end) = read_all(&mut reader, 4096);

        assert!(end.is_ok(), "clean end of stream expected, got {end:?}");
        assert_eq!(got, [audio(METAINT, 1), audio(METAINT, 2), audio(METAINT, 3)].concat());
        assert_eq!(*titles.lock().unwrap(), ["x".to_string(), long_title]);
    }

    #[test]
    fn decodes_latin1_titles() {
        let mut wire = audio(METAINT, 1);
        wire.extend(meta_block(b"StreamTitle='Bj\xF6rk - J\xF3ga';"));
        let (titles, cb) = collector();
        let mut reader = IcyReader::new(Cursor::new(wire), METAINT, cb);

        let (got, _) = read_all(&mut reader, 4096);

        assert_eq!(got, audio(METAINT, 1));
        assert_eq!(*titles.lock().unwrap(), ["Björk - Jóga"]);
    }

    #[test]
    fn stream_ending_at_a_block_boundary_or_inside_audio_is_a_clean_end() {
        let cases = [("exactly before a metadata block", METAINT), ("inside an audio segment", METAINT - 9)];
        for (name, tail) in cases {
            let mut wire = audio(METAINT, 1);
            wire.extend(title_block("Only Title"));
            wire.extend(audio(tail, 2));
            let (titles, cb) = collector();
            let mut reader = IcyReader::new(Cursor::new(wire), METAINT, cb);

            let (got, end) = read_all(&mut reader, 16);

            assert!(end.is_ok(), "{name}: clean end of stream expected, got {end:?}");
            assert_eq!(got, [audio(METAINT, 1), audio(tail, 2)].concat(), "{name}");
            assert_eq!(*titles.lock().unwrap(), ["Only Title"], "{name}");
        }
    }

    #[test]
    fn stream_ending_inside_a_metadata_block_keeps_the_audio_and_reports_no_partial_title() {
        let full = title_block("Cut Off - Halfway Through");
        // Cut after the length byte, and in the middle of the text.
        for cut in [1, 20] {
            let mut wire = audio(METAINT, 1);
            wire.extend_from_slice(&full[..cut]);
            let (titles, cb) = collector();
            let mut reader = IcyReader::new(Cursor::new(wire), METAINT, cb);

            // A block that is announced but incomplete is a broken stream, not a clean end:
            // all audio before it arrives, then an error, and no title made up from half a
            // block.
            let (got, end) = read_all(&mut reader, 16);

            assert_eq!(end.err().map(|e| e.kind()), Some(ErrorKind::UnexpectedEof), "cut after {cut} metadata bytes");
            assert_eq!(got, audio(METAINT, 1), "cut after {cut} metadata bytes");
            assert_eq!(*titles.lock().unwrap(), Vec::<String>::new(), "cut after {cut} metadata bytes");
        }
    }

    #[test]
    fn a_zero_length_read_consumes_nothing() {
        let mut wire = audio(METAINT, 1);
        wire.extend(title_block("After Empty Read"));
        wire.extend(audio(5, 2));
        let (titles, cb) = collector();
        let mut reader = IcyReader::new(Cursor::new(wire), METAINT, cb);
        let mut first = vec![0u8; METAINT];
        reader.read_exact(&mut first).unwrap();

        assert_eq!(reader.read(&mut []).unwrap(), 0);
        let (rest, end) = read_all(&mut reader, 16);

        assert!(end.is_ok());
        assert_eq!(first, audio(METAINT, 1));
        assert_eq!(rest, audio(5, 2));
        assert_eq!(*titles.lock().unwrap(), ["After Empty Read"]);
    }
}

mod pipe_reader {
    use super::*;

    #[test]
    fn delivers_chunks_in_order_and_ends_when_the_sender_is_gone() {
        let (tx, rx) = sync_channel::<Vec<u8>>(8);
        tx.send(vec![1, 2, 3]).unwrap();
        tx.send(Vec::new()).unwrap();
        tx.send(vec![4]).unwrap();
        tx.send(vec![5, 6, 7, 8, 9]).unwrap();
        drop(tx);
        let mut reader = PipeReader::new(rx, Arc::new(AtomicBool::new(false)), Duration::from_secs(30));

        let (got, end) = read_all(&mut reader, 2);

        assert!(end.is_ok(), "clean end expected, got {end:?}");
        assert_eq!(got, [1, 2, 3, 4, 5, 6, 7, 8, 9]);
    }

    #[test]
    fn reports_a_stalled_stream_after_the_stall_timeout() {
        let (tx, rx) = sync_channel::<Vec<u8>>(8);
        tx.send(vec![42]).unwrap();
        let mut reader = PipeReader::new(rx, Arc::new(AtomicBool::new(false)), Duration::from_millis(400));
        let mut buf = [0u8; 8];
        assert_eq!(reader.read(&mut buf).unwrap(), 1);
        assert_eq!(buf[0], 42);

        let started = Instant::now();
        let err = reader.read(&mut buf).expect_err("a silent sender must time out");
        let waited = started.elapsed();
        drop(tx);

        assert_eq!(err.kind(), ErrorKind::TimedOut);
        assert_eq!(err.to_string(), "stream stalled");
        assert!(waited >= Duration::from_millis(400), "gave up after only {waited:?}");
        assert!(waited < Duration::from_secs(5), "took {waited:?} to notice a 400 ms stall");
    }

    #[test]
    fn data_arriving_before_the_timeout_is_not_a_stall() {
        let (tx, rx) = sync_channel::<Vec<u8>>(8);
        let mut reader = PipeReader::new(rx, Arc::new(AtomicBool::new(false)), Duration::from_secs(10));
        let sender = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(350));
            tx.send(vec![7, 8]).unwrap();
        });
        let mut buf = [0u8; 8];

        let n = reader.read(&mut buf).unwrap();
        sender.join().unwrap();

        assert_eq!(&buf[..n], [7, 8]);
    }

    #[test]
    fn abort_interrupts_a_blocked_read_promptly() {
        let (tx, rx) = sync_channel::<Vec<u8>>(8);
        let abort = Arc::new(AtomicBool::new(false));
        let mut reader = PipeReader::new(rx, abort.clone(), Duration::from_secs(60));
        let setter = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(250));
            abort.store(true, Ordering::Relaxed);
        });

        let started = Instant::now();
        let err = reader.read(&mut [0u8; 8]).expect_err("an aborted read must fail");
        let waited = started.elapsed();
        setter.join().unwrap();
        drop(tx);

        // Not `Interrupted`: readers retry that kind, and an abandoned stream must stay closed.
        assert_eq!(err.kind(), ErrorKind::ConnectionAborted);
        assert_eq!(err.to_string(), "stream closed");
        assert!(waited < Duration::from_secs(5), "abort took {waited:?} to be noticed");
    }

    /// A body that delivers `prefix` and then blocks until the test drops the sending end.
    struct Stalling {
        prefix: Cursor<Vec<u8>>,
        hold: std::sync::mpsc::Receiver<()>,
    }

    impl Read for Stalling {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            match self.prefix.read(buf)? {
                0 => {
                    let _ = self.hold.recv();
                    Ok(0)
                }
                n => Ok(n),
            }
        }
    }

    #[test]
    fn spawn_pipe_with_timeout_reports_a_body_that_goes_silent() {
        let (release, hold) = std::sync::mpsc::channel::<()>();
        let body = Stalling { prefix: Cursor::new(vec![9, 8, 7]), hold };
        let mut reader = spawn_pipe_with_timeout(body, Arc::new(AtomicBool::new(false)), Duration::from_millis(400));
        let mut buf = [0u8; 8];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], [9, 8, 7]);

        let started = Instant::now();
        let err = reader.read(&mut buf).expect_err("a silent body must time out");
        let waited = started.elapsed();
        drop(release);

        assert_eq!(err.kind(), ErrorKind::TimedOut);
        assert_eq!(err.to_string(), "stream stalled");
        assert!(waited >= Duration::from_millis(400) && waited < Duration::from_secs(5), "gave up after {waited:?}");
    }

    #[test]
    fn the_default_stall_timeout_is_twenty_seconds() {
        assert_eq!(DEFAULT_STALL_TIMEOUT, Duration::from_secs(20));
    }

    #[test]
    fn spawn_pipe_carries_the_whole_body_through_the_download_thread() {
        // Larger than one 8 KiB hop, and not a multiple of it.
        let body = audio(100_003, 5);
        let mut reader = spawn_pipe(Cursor::new(body.clone()), Arc::new(AtomicBool::new(false)));

        let (got, end) = read_all(&mut reader, 1000);

        assert!(end.is_ok(), "clean end expected, got {end:?}");
        assert_eq!(got.len(), body.len());
        assert!(got == body, "bytes differ from the body that was sent");
    }
}
