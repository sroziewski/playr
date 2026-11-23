//! A throw-away HTTP server on 127.0.0.1 for HLS: fixed files (answering `Range` with 206),
//! playlists that change with every request (a live broadcast), redirects, 404s chosen per
//! request, responses held back until the test releases them (a stalled segment) and bodies
//! trickled out a byte at a time (a slow download). Every request is logged with its headers.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Builds the body of the n-th request (counting from 0) for a path.
pub type Dynamic = Box<dyn Fn(usize) -> Vec<u8> + Send + Sync>;
/// Like [`Dynamic`], with `None` answered as 404 Not Found.
pub type MaybeDynamic = Box<dyn Fn(usize) -> Option<Vec<u8>> + Send + Sync>;

pub enum Body {
    Bytes(Vec<u8>),
    Dynamic(Dynamic),
    Maybe(MaybeDynamic),
    /// The bytes, sent only once [`HlsServer::release`] is called.
    Held(Vec<u8>),
    /// The whole file with 200 even when a range is asked for.
    IgnoresRange(Vec<u8>),
    /// 302 Found to this path on the same server.
    Redirect(String),
    /// Headers and the first half of the bytes at once, then [`TRICKLE_PIECE`] bytes every
    /// 50 ms; when the client hangs up, [`HlsServer::hung_up`] turns true.
    Trickle(Vec<u8>),
}

pub struct Route {
    pub path: String,
    pub content_type: &'static str,
    pub body: Body,
}

pub fn bytes(path: &str, content_type: &'static str, body: Vec<u8>) -> Route {
    Route { path: path.to_string(), content_type, body: Body::Bytes(body) }
}

pub fn dynamic(path: &str, content_type: &'static str, body: Dynamic) -> Route {
    Route { path: path.to_string(), content_type, body: Body::Dynamic(body) }
}

pub fn maybe(path: &str, content_type: &'static str, body: MaybeDynamic) -> Route {
    Route { path: path.to_string(), content_type, body: Body::Maybe(body) }
}

pub fn held(path: &str, content_type: &'static str, body: Vec<u8>) -> Route {
    Route { path: path.to_string(), content_type, body: Body::Held(body) }
}

pub fn ignores_range(path: &str, content_type: &'static str, body: Vec<u8>) -> Route {
    Route { path: path.to_string(), content_type, body: Body::IgnoresRange(body) }
}

pub fn redirect(path: &str, to: &str) -> Route {
    Route { path: path.to_string(), content_type: "text/plain", body: Body::Redirect(to.to_string()) }
}

pub fn trickle(path: &str, content_type: &'static str, body: Vec<u8>) -> Route {
    Route { path: path.to_string(), content_type, body: Body::Trickle(body) }
}

pub const PLAYLIST: &str = "application/vnd.apple.mpegurl";

/// Bytes a trickled body sends every 50 ms after its first half: a slow 320 bytes a second.
pub const TRICKLE_PIECE: usize = 16;

/// Every file of `tests/fixtures/hls/<dir>` as `/<prefix>/<file name>`.
pub fn fixture_dir(dir: &str, prefix: &str) -> Vec<Route> {
    let path = super::fixture(&format!("hls/{dir}/index.m3u8"));
    let mut names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
        .expect("list the fixture folder")
        .map(|e| e.expect("folder entry").path())
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|p| {
            let name = p.file_name().unwrap().to_str().unwrap().to_string();
            let content_type = if name.ends_with(".m3u8") { PLAYLIST } else { "application/octet-stream" };
            bytes(&format!("/{prefix}/{name}"), content_type, std::fs::read(&p).expect("read fixture"))
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    /// With the query string.
    pub path: String,
    pub range: Option<String>,
    /// Names lower-cased.
    pub headers: Vec<(String, String)>,
}

impl Hit {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }
}

pub struct HlsServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
    hung_up: Arc<AtomicBool>,
    log: Arc<Mutex<Vec<Hit>>>,
}

struct Shared {
    addr: SocketAddr,
    routes: Vec<Route>,
    counts: Mutex<Vec<usize>>,
    stop: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
    hung_up: Arc<AtomicBool>,
    log: Arc<Mutex<Vec<Hit>>>,
}

impl HlsServer {
    pub fn start(routes: Vec<Route>) -> Self {
        let listener = {
            let _ports = super::http::PORTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            TcpListener::bind("127.0.0.1:0").expect("bind test server")
        };
        let addr = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let hung_up = Arc::new(AtomicBool::new(false));
        let log = Arc::new(Mutex::new(Vec::new()));
        let shared = Arc::new(Shared {
            addr,
            counts: Mutex::new(vec![0; routes.len()]),
            routes,
            stop: stop.clone(),
            release: release.clone(),
            hung_up: hung_up.clone(),
            log: log.clone(),
        });
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                if shared.stop.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(conn) = conn else { continue };
                let shared = shared.clone();
                std::thread::spawn(move || handle(conn, &shared));
            }
        });
        Self { addr, stop, release, hung_up, log }
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    pub fn hits(&self) -> Vec<Hit> {
        self.log.lock().unwrap().clone()
    }

    /// Paths requested, in order, without query strings.
    pub fn paths(&self) -> Vec<String> {
        self.hits().into_iter().map(|h| h.path.split('?').next().unwrap_or("").to_string()).collect()
    }

    pub fn count(&self, path: &str) -> usize {
        self.paths().iter().filter(|p| *p == path).count()
    }

    /// Lets the held responses go out.
    pub fn release(&self) {
        self.release.store(true, Ordering::Relaxed);
    }

    /// Whether a client closed the connection of a trickled body before it was complete.
    pub fn hung_up(&self) -> bool {
        self.hung_up.load(Ordering::Relaxed)
    }
}

impl Drop for HlsServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.release.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(self.addr);
    }
}

fn handle(mut conn: TcpStream, shared: &Shared) {
    let _ = conn.set_read_timeout(Some(Duration::from_secs(5)));
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if conn.read(&mut byte).ok() != Some(1) || head.len() > 16 * 1024 {
            return;
        }
        head.push(byte[0]);
    }
    let text = String::from_utf8_lossy(&head).into_owned();
    let path = text.split(' ').nth(1).unwrap_or("").to_string();
    let headers: Vec<(String, String)> = text
        .split("\r\n")
        .skip(1)
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let range = headers.iter().find(|(n, _)| n == "range").map(|(_, v)| v.clone());
    shared.log.lock().unwrap().push(Hit { path: path.clone(), range: range.clone(), headers });
    let bare = path.split('?').next().unwrap_or("");
    let not_found = b"HTTP/1.0 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    let Some(index) = shared.routes.iter().position(|r| r.path == bare) else {
        let _ = conn.write_all(not_found);
        return;
    };
    let route = &shared.routes[index];
    let nth = || {
        let mut counts = shared.counts.lock().unwrap();
        counts[index] += 1;
        counts[index] - 1
    };
    let body = match &route.body {
        Body::Bytes(b) | Body::IgnoresRange(b) => b.clone(),
        Body::Dynamic(make) => make(nth()),
        Body::Maybe(make) => match make(nth()) {
            Some(b) => b,
            None => {
                let _ = conn.write_all(not_found);
                return;
            }
        },
        Body::Held(b) => {
            while !shared.release.load(Ordering::Relaxed) && !shared.stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(20));
            }
            b.clone()
        }
        Body::Redirect(to) => {
            let head = format!("HTTP/1.0 302 Found\r\nLocation: http://{}{to}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", shared.addr);
            let _ = conn.write_all(head.as_bytes());
            return;
        }
        Body::Trickle(b) => {
            let head = format!("HTTP/1.0 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", route.content_type, b.len());
            let half = b.len() / 2;
            if conn.write_all(head.as_bytes()).and_then(|_| conn.write_all(&b[..half])).is_err() {
                shared.hung_up.store(true, Ordering::Relaxed);
                return;
            }
            for piece in b[half..].chunks(TRICKLE_PIECE) {
                if shared.stop.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
                if conn.write_all(piece).and_then(|_| conn.flush()).is_err() {
                    shared.hung_up.store(true, Ordering::Relaxed);
                    return;
                }
            }
            return;
        }
    };
    let len = body.len();
    let honours_range = !matches!(route.body, Body::IgnoresRange(_));
    let wanted = range.as_deref().filter(|_| honours_range).and_then(|r| r.strip_prefix("bytes=")).and_then(|r| r.split_once('-')).map(|(a, b)| {
        let from = a.parse::<usize>().unwrap_or(0).min(len);
        let to = b.parse::<usize>().map_or(len, |b| (b + 1).min(len));
        (from, to.max(from))
    });
    let (head, part) = match wanted {
        Some((from, to)) => (
            format!(
                "HTTP/1.0 206 Partial Content\r\nContent-Type: {}\r\nContent-Length: {}\r\nContent-Range: bytes {from}-{}/{len}\r\nConnection: close\r\n\r\n",
                route.content_type,
                to - from,
                to.saturating_sub(1)
            ),
            &body[from..to],
        ),
        None => (
            format!("HTTP/1.0 200 OK\r\nContent-Type: {}\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n", route.content_type),
            &body[..],
        ),
    };
    let _ = conn.write_all(head.as_bytes());
    let _ = conn.write_all(part);
    let _ = conn.flush();
}
