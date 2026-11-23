//! Internet radio through `playr::net::open_url` against a local HTTP server
//! (`tests/common/http.rs`) that serves an MP3 with interleaved ICY metadata.

mod common;

use std::io::{ErrorKind, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::http::{RadioServer, STALL_PREFIX, STATION, TITLES};
use common::*;
use playr::net::{HttpStream, TitleCallback, open_url, open_url_with_timeout};

fn mp3() -> Vec<u8> {
    fixture_bytes("radio.mp3")
}

fn collector() -> (Arc<Mutex<Vec<String>>>, TitleCallback) {
    let titles = Arc::new(Mutex::new(Vec::new()));
    let sink = titles.clone();
    (titles, Box::new(move |t| sink.lock().unwrap().push(t)))
}

fn no_abort() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

fn open(url: &str) -> (Result<HttpStream, String>, Arc<Mutex<Vec<String>>>) {
    let (titles, cb) = collector();
    (open_url(url, no_abort(), cb), titles)
}

fn read_to_end(stream: &mut HttpStream) -> Vec<u8> {
    let mut out = Vec::new();
    stream.source.read_to_end(&mut out).expect("read the stream to its end");
    out
}

#[test]
fn icy_stream_decodes_and_reports_each_new_title_once() {
    let server = RadioServer::start(mp3());
    let (stream, titles) = open(&server.url("/stream"));
    let stream = stream.expect("open the stream");

    assert_eq!(stream.url, server.url("/stream"));
    assert_eq!(stream.mime.as_deref(), Some("audio/mpeg"));
    assert_eq!(stream.ext.as_deref(), Some("mp3"));
    assert_eq!(stream.name.as_deref(), Some(STATION));
    assert!(!stream.finite, "a station is an endless stream");

    let mut demux = playr::demux::open_stream(stream.source, stream.ext.as_deref(), stream.mime.as_deref())
        .expect("demux the stream");
    assert_eq!(demux.info().container, "mp3");
    assert!(!demux.info().seekable, "a live stream is not seekable");
    assert_eq!(demux.info().duration, None, "a live stream has no duration");
    let (demuxed, err) = drain(demux.as_mut());
    assert_eq!(err, None, "the end of the broadcast is a clean end");

    let audio = decode_audio(&demuxed);
    assert_eq!((audio.rate, audio.channels, audio.failed_packets), (44100, 1, 0));
    // FFmpeg decodes 155 frames of 1152 samples from radio.mp3: a 4 s tone at -21.57 dBFS.
    assert!(audio.frames().abs_diff(178_560) <= 1152, "{} sample frames decoded, expected 178560 +/- 1152", audio.frames());
    let cycles = tone_cycles(&audio.samples, 0.02) as f64;
    assert!((cycles - 1760.0).abs() <= 35.0, "{cycles} cycles of the 440 Hz tone in 4 s, expected 1760 +/- 2%");
    let level = 20.0 * rms(&audio.samples).log10();
    assert!((level + 21.57).abs() <= 0.3, "RMS {level:.2} dBFS, FFmpeg measures -21.57");

    assert_eq!(*titles.lock().unwrap(), TITLES);
    let requests = server.requests_for("/stream");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].header("icy-metadata"), Some("1"), "metadata has to be requested from the station");
}

#[test]
fn aac_station_decodes_and_reports_its_titles() {
    let server = RadioServer::start(mp3());
    let (stream, titles) = open(&server.url("/aac"));
    let stream = stream.expect("open the stream");

    assert_eq!(stream.mime.as_deref(), Some("audio/aacp"));
    assert_eq!(stream.ext.as_deref(), Some("aac"));
    assert!(!stream.finite);

    let mut demux = playr::demux::open_stream(stream.source, stream.ext.as_deref(), stream.mime.as_deref())
        .expect("demux the stream");
    assert_eq!(demux.info().container, "aac");
    assert_eq!((demux.info().seekable, demux.info().duration), (false, None));
    let (demuxed, err) = drain(demux.as_mut());
    assert_eq!(err, None, "the end of the broadcast is a clean end");

    let audio = decode_audio(&demuxed);
    assert_eq!((audio.rate, audio.channels, audio.failed_packets), (44100, 1, 0));
    // FFmpeg decodes 131 frames of 1024 samples from sine.aac: a 3 s tone at -21.14 dBFS.
    assert!(audio.frames().abs_diff(134_144) <= 1024, "{} sample frames decoded, expected 134144 +/- 1024", audio.frames());
    let cycles = tone_cycles(&audio.samples, 0.02) as f64;
    assert!((cycles - 1320.0).abs() <= 26.0, "{cycles} cycles of the 440 Hz tone in 3 s, expected 1320 +/- 2%");
    let level = 20.0 * rms(&audio.samples).log10();
    assert!((level + 21.14).abs() <= 0.3, "RMS {level:.2} dBFS, FFmpeg measures -21.14");
    // sine.aac is 37545 bytes: nine full 4096-byte segments, so all three titles are sent.
    assert_eq!(*titles.lock().unwrap(), TITLES);
}

#[test]
fn icy_metadata_is_removed_from_the_audio_bytes() {
    let server = RadioServer::start(mp3());
    let (stream, titles) = open(&server.url("/stream"));

    let body = read_to_end(&mut stream.expect("open the stream"));

    assert_eq!(body.len(), mp3().len());
    assert!(body == mp3(), "the bytes handed to the demuxer differ from the MP3 the station sent");
    assert_eq!(*titles.lock().unwrap(), TITLES);
}

#[test]
fn playlist_links_resolve_to_the_stream_they_name() {
    let server = RadioServer::start(mp3());
    let mut bad = Vec::new();
    for path in ["/list.m3u", "/list.pls", "/named.m3u", "/typed", "/chain.m3u"] {
        let (stream, titles) = open(&server.url(path));
        let mut stream = match stream {
            Ok(s) => s,
            Err(e) => {
                bad.push(format!("{path}: failed with {e:?}"));
                continue;
            }
        };
        expect!(bad, stream.url == server.url("/stream"), "{path}: resolved to {}", stream.url);
        expect!(bad, stream.name.as_deref() == Some(STATION), "{path}: station name {:?}", stream.name);
        expect!(bad, stream.ext.as_deref() == Some("mp3"), "{path}: extension hint {:?}", stream.ext);
        expect!(bad, read_to_end(&mut stream) == mp3(), "{path}: audio bytes differ from the station's MP3");
        expect!(bad, *titles.lock().unwrap() == TITLES, "{path}: titles {:?}", titles.lock().unwrap());
    }
    finish(bad);
}

#[test]
fn playlists_that_lead_nowhere_are_reported() {
    let server = RadioServer::start(mp3());
    let cases = [
        ("/empty.m3u", "playlist contains no stream address"),
        ("/loop.m3u", "too many playlist redirections"),
    ];
    for (path, message) in cases {
        let (stream, _) = open(&server.url(path));
        assert_eq!(stream.err().as_deref(), Some(message), "{path}");
    }
}

#[test]
fn an_hls_playlist_is_handed_back_as_text_for_the_hls_player_without_fetching_a_segment() {
    let server = RadioServer::start(mp3());
    let url = server.url("/hls.m3u8");

    let (stream, titles) = open(&url);

    let mut stream = stream.expect("an HLS playlist opens");
    let expected = format!("#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:10.0,\n{}\n", server.url("/segment0.ts"));
    assert_eq!(stream.hls.as_deref(), Some(expected.as_str()));
    assert_eq!((stream.url.as_str(), stream.mime.as_deref(), stream.secure), (url.as_str(), Some("application/vnd.apple.mpegurl"), true));
    assert_eq!(read_to_end(&mut stream), Vec::<u8>::new(), "the source of an HLS stream holds nothing");
    assert_eq!(*titles.lock().unwrap(), Vec::<String>::new());
    assert_eq!(server.requests_for("/segment0.ts").len(), 0, "open_url must not fetch a segment");
}

#[test]
fn a_missing_mount_point_and_a_dead_server_are_reported_with_the_address() {
    let server = RadioServer::start(mp3());
    let missing = server.url("/no-such-mount");

    let (not_found, _) = open(&missing);
    let (dead, refused) = {
        // A port that was just released: nothing listens there, and no other test's server
        // can be given it while the lock is held.
        let _ports = common::http::PORTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let dead = format!("http://{}/stream", listener.local_addr().unwrap());
        drop(listener);
        let (refused, _) = open(&dead);
        (dead, refused)
    };

    // The text after the address is the HTTP client's and names the cause.
    let not_found = not_found.err().expect("404 must be an error");
    assert!(not_found.starts_with(&format!("cannot open {missing}: ")), "{not_found:?}");
    assert!(not_found.contains("404"), "the status is not mentioned: {not_found:?}");
    let refused = refused.err().expect("a refused connection must be an error");
    let prefix = format!("cannot open {dead}: ");
    assert!(refused.starts_with(&prefix) && refused.len() > prefix.len(), "{refused:?}");
}

#[test]
fn a_plain_http_file_is_passed_through_with_the_extension_as_hint() {
    let server = RadioServer::start(mp3());
    let url = server.url("/plain.mp3");
    let (stream, titles) = open(&format!("  {url}\n"));
    let mut stream = stream.expect("open the file");

    assert_eq!(stream.url, url, "surrounding white space is not part of the address");
    assert!(stream.finite, "a response with Content-Length and no ICY metadata is a finite file");
    assert_eq!((stream.mime.as_deref(), stream.name.as_deref()), (None, None));
    assert_eq!(stream.ext.as_deref(), Some("mp3"));
    assert!(read_to_end(&mut stream) == mp3(), "the downloaded bytes differ from the file");
    assert_eq!(*titles.lock().unwrap(), Vec::<String>::new());
}

#[test]
fn abort_releases_a_reader_blocked_on_a_silent_connection() {
    let server = RadioServer::start(mp3());
    let abort = no_abort();
    let (_, cb) = collector();
    let mut stream = open_url(&server.url("/stall"), abort.clone(), cb).expect("headers arrive before the stall");
    let mut prefix = vec![0u8; STALL_PREFIX];
    stream.source.read_exact(&mut prefix).expect("the bytes sent before the stall");
    assert!(prefix == mp3()[..STALL_PREFIX]);

    let setter = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        abort.store(true, Ordering::Relaxed);
    });
    let started = Instant::now();
    let err = stream.source.read(&mut [0u8; 64]).expect_err("an aborted read must fail");
    let waited = started.elapsed();
    setter.join().unwrap();

    assert_eq!(err.kind(), ErrorKind::ConnectionAborted);
    assert_eq!(err.to_string(), "stream closed");
    assert!(waited < Duration::from_secs(5), "abort took {waited:?} to be noticed");
}

#[test]
fn a_connection_that_goes_silent_is_reported_as_stalled_instead_of_hanging() {
    let server = RadioServer::start(mp3());
    let (_, cb) = collector();
    let mut stream = open_url_with_timeout(&server.url("/stall"), no_abort(), cb, Duration::from_millis(1500))
        .expect("headers arrive before the stall");
    let mut prefix = vec![0u8; STALL_PREFIX];
    stream.source.read_exact(&mut prefix).expect("the bytes sent before the stall");

    let started = Instant::now();
    let err = stream.source.read(&mut [0u8; 64]).expect_err("a silent connection must not block forever");
    let waited = started.elapsed();

    assert_eq!(err.kind(), ErrorKind::TimedOut);
    assert_eq!(err.to_string(), "stream stalled");
    assert!(waited >= Duration::from_millis(1500) && waited <= Duration::from_secs(8), "gave up after {waited:?}, expected about 1.5 s");
}
