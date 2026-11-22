//! A throw-away HTTP server on 127.0.0.1 that serves files the way YouTube's stream servers do:
//! byte ranges answered with `206 Partial Content` and a `Content-Range` header.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// 206 with `Content-Range`, as it should be.
    Ranged,
    /// Ignores `Range` and sends the whole file with 200.
    IgnoresRange,
    /// 206 but no `Content-Range` header.
    NoContentRange,
}

#[derive(Clone, Debug)]
pub struct RangeRequest {
    pub path: String,
    /// Header names lower-cased.
    pub headers: Vec<(String, String)>,
}

impl RangeRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }
}

type Files = Arc<Vec<(String, Mode, Arc<Vec<u8>>)>>;

pub struct RangeServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<RangeRequest>>>,
}

impl RangeServer {
    /// Serves each `(path, mode, bytes)`; anything else is a 404.
    pub fn start(files: Vec<(&str, Mode, Vec<u8>)>) -> Self {
        let listener = {
            let _ports = super::http::PORTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            TcpListener::bind("127.0.0.1:0").expect("bind test server")
        };
        let addr = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let files: Files = Arc::new(files.into_iter().map(|(p, m, b)| (p.to_string(), m, Arc::new(b))).collect());
        let (thread_stop, thread_requests) = (stop.clone(), requests.clone());
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                if thread_stop.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(conn) = conn else { continue };
                let (requests, files) = (thread_requests.clone(), files.clone());
                std::thread::spawn(move || handle(conn, &files, &requests));
            }
        });
        Self { addr, stop, requests }
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    pub fn requests(&self) -> Vec<RangeRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for RangeServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(self.addr);
    }
}

fn read_request(conn: &mut TcpStream) -> Option<RangeRequest> {
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
    Some(RangeRequest { path, headers })
}

/// `bytes=a-b` (inclusive) as `a..b+1`, clipped to the file.
fn range(header: Option<&str>, len: usize) -> (usize, usize) {
    let parsed = header.and_then(|h| h.strip_prefix("bytes=")).and_then(|r| r.split_once('-')).map(|(a, b)| {
        let from = a.parse::<usize>().unwrap_or(0);
        let to = b.parse::<usize>().map_or(len, |b| b + 1);
        (from.min(len), to.min(len))
    });
    parsed.unwrap_or((0, len))
}

fn handle(mut conn: TcpStream, files: &Files, requests: &Mutex<Vec<RangeRequest>>) {
    let Some(req) = read_request(&mut conn) else { return };
    requests.lock().unwrap().push(req.clone());
    let path = req.path.split('?').next().unwrap_or("");
    let Some((_, mode, bytes)) = files.iter().find(|(p, _, _)| p == path) else {
        let _ = conn.write_all(b"HTTP/1.0 404 Not Found\r\nConnection: close\r\nContent-Length: 0\r\n\r\n");
        return;
    };
    let len = bytes.len();
    let (from, to) = if *mode == Mode::IgnoresRange { (0, len) } else { range(req.header("range"), len) };
    let body = &bytes[from..to.max(from)];
    let head = match mode {
        Mode::Ranged => format!(
            "HTTP/1.0 206 Partial Content\r\nConnection: close\r\nContent-Length: {}\r\nContent-Range: bytes {from}-{}/{len}\r\n\r\n",
            body.len(),
            (from + body.len()).saturating_sub(1)
        ),
        Mode::IgnoresRange => format!("HTTP/1.0 200 OK\r\nConnection: close\r\nContent-Length: {len}\r\n\r\n"),
        Mode::NoContentRange => format!("HTTP/1.0 206 Partial Content\r\nConnection: close\r\nContent-Length: {}\r\n\r\n", body.len()),
    };
    let _ = conn.write_all(head.as_bytes());
    let _ = conn.write_all(body);
    let _ = conn.flush();
}
