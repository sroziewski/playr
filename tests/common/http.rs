//! A throw-away HTTP server on 127.0.0.1 that plays the part of an internet radio station.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Bytes between two metadata blocks of the `/stream` route.
pub const METAINT: usize = 4096;
pub const STATION: &str = "Test Radio 99.9";
/// Titles the `/stream` route announces, in order (see `title_for_block`).
pub const TITLES: [&str; 3] = ["Artist One - Song One", "Artist Two - Song Two", "Björk - Jóga"];
/// Bytes the `/stall` route sends before it goes silent.
pub const STALL_PREFIX: usize = 1000;

#[derive(Clone, Debug)]
pub struct Request {
    pub path: String,
    /// Header names lower-cased.
    pub headers: Vec<(String, String)>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }
}

pub struct RadioServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<Request>>>,
}

/// Held while a port number is being picked. A test that needs a port nobody listens on takes
/// it too, from binding and releasing that port until it has used it, so that no server of a
/// test running in parallel can be handed the same number in between.
pub static PORTS: Mutex<()> = Mutex::new(());

impl RadioServer {
    /// Serves `mp3` as a radio stream plus the playlists that point at it.
    pub fn start(mp3: Vec<u8>) -> Self {
        let listener = {
            let _ports = PORTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            TcpListener::bind("127.0.0.1:0").expect("bind test server")
        };
        let addr = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (thread_stop, thread_requests) = (stop.clone(), requests.clone());
        let mp3 = Arc::new(mp3);
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                if thread_stop.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(conn) = conn else { continue };
                let (stop, requests, mp3) = (thread_stop.clone(), thread_requests.clone(), mp3.clone());
                std::thread::spawn(move || handle(conn, &format!("http://{addr}"), &mp3, &stop, &requests));
            }
        });
        Self { addr, stop, requests }
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }

    pub fn requests_for(&self, path: &str) -> Vec<Request> {
        self.requests().into_iter().filter(|r| r.path == path).collect()
    }
}

impl Drop for RadioServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Wakes the accept loop so it sees the flag.
        let _ = TcpStream::connect(self.addr);
    }
}

/// One ICY metadata block: a length byte counting 16-byte units, then the NUL-padded text.
pub fn meta_block(text: &[u8]) -> Vec<u8> {
    let units = text.len().div_ceil(16);
    let mut out = vec![units as u8];
    out.extend_from_slice(text);
    out.resize(1 + units * 16, 0);
    out
}

/// What the station sends after the n-th audio segment: a title, the same title again, an
/// empty block, a new title (twice), and finally one in Latin-1 as older stations send it.
fn title_for_block(n: usize) -> Vec<u8> {
    match n {
        0 | 1 => meta_block(format!("StreamTitle='{}';StreamUrl='http://station.example/one';", TITLES[0]).as_bytes()),
        2 => vec![0],
        3 | 4 => meta_block(format!("StreamTitle='{}';", TITLES[1]).as_bytes()),
        _ => meta_block(b"StreamTitle='Bj\xF6rk - J\xF3ga';"),
    }
}

/// `audio` with a metadata block after every full `METAINT` bytes.
fn with_metadata(audio: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    for (n, segment) in audio.chunks(METAINT).enumerate() {
        body.extend_from_slice(segment);
        if segment.len() == METAINT {
            body.extend(title_for_block(n));
        }
    }
    body
}

fn read_request(conn: &mut TcpStream) -> Option<Request> {
    conn.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if conn.read(&mut byte).ok()? == 0 || head.len() > 16 * 1024 {
            return None;
        }
        head.push(byte[0]);
    }
    let text = String::from_utf8_lossy(&head);
    let mut lines = text.split("\r\n");
    let path = lines.next()?.split(' ').nth(1)?.to_string();
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    Some(Request { path, headers })
}

fn respond(conn: &mut TcpStream, status: &str, headers: &[(&str, &str)], body: &[u8]) {
    let mut head = format!("HTTP/1.0 {status}\r\nConnection: close\r\n");
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let _ = conn.write_all(head.as_bytes());
    let _ = conn.write_all(body);
    let _ = conn.flush();
}

fn playlist(conn: &mut TcpStream, mime: &str, body: &str) {
    respond(conn, "200 OK", &[("Content-Type", mime), ("Content-Length", &body.len().to_string())], body.as_bytes());
}

fn handle(mut conn: TcpStream, base: &str, mp3: &[u8], stop: &AtomicBool, requests: &Mutex<Vec<Request>>) {
    let Some(req) = read_request(&mut conn) else { return };
    requests.lock().unwrap().push(req.clone());
    match req.path.as_str() {
        // Like a real station, metadata is interleaved only for clients that ask for it.
        "/stream" if req.header("icy-metadata") == Some("1") => {
            let metaint = METAINT.to_string();
            respond(&mut conn, "200 OK", &[("Content-Type", "audio/mpeg"), ("icy-name", STATION), ("icy-metaint", &metaint)], &with_metadata(mp3));
        }
        // An AAC station: ADTS frames (the 3 s `sine.aac` fixture) under the SHOUTcast media type.
        "/aac" => {
            let metaint = METAINT.to_string();
            let body = with_metadata(&super::fixture_bytes("sine.aac"));
            respond(&mut conn, "200 OK", &[("Content-Type", "audio/aacp"), ("icy-name", STATION), ("icy-metaint", &metaint)], &body);
        }
        "/stream" => respond(&mut conn, "200 OK", &[("Content-Type", "audio/mpeg"), ("icy-name", STATION)], mp3),
        "/plain.mp3" => respond(&mut conn, "200 OK", &[("Content-Length", &mp3.len().to_string())], mp3),
        "/list.m3u" => playlist(&mut conn, "audio/x-mpegurl", &format!("#EXTM3U\r\n#EXTINF:-1,{STATION}\r\n{base}/stream\r\n")),
        "/list.pls" => playlist(
            &mut conn,
            "audio/x-scpls",
            &format!("[playlist]\nNumberOfEntries=1\nFile1={base}/stream\nTitle1={STATION}\nLength1=-1\nVersion=2\n"),
        ),
        // Recognised by its name only.
        "/named.m3u" => playlist(&mut conn, "text/plain", &format!("{base}/stream\n")),
        // Recognised by its media type only, which carries a parameter.
        "/typed" => playlist(&mut conn, "Audio/X-MpegURL; charset=utf-8", &format!("{base}/stream\n")),
        "/chain.m3u" => playlist(&mut conn, "audio/x-mpegurl", &format!("{base}/list.pls\n")),
        "/loop.m3u" => playlist(&mut conn, "audio/x-mpegurl", &format!("{base}/loop.m3u\n")),
        "/empty.m3u" => playlist(&mut conn, "audio/x-mpegurl", "#EXTM3U\n# nothing on air\n"),
        "/hls.m3u8" => playlist(
            &mut conn,
            "application/vnd.apple.mpegurl",
            &format!("#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:10.0,\n{base}/segment0.ts\n"),
        ),
        "/stall" => {
            respond(&mut conn, "200 OK", &[("Content-Type", "audio/mpeg")], &mp3[..STALL_PREFIX]);
            // Keep the connection open and silent until the test is over.
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        _ => respond(&mut conn, "404 Not Found", &[("Content-Type", "text/plain")], b"no such mount point"),
    }
}
