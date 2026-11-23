//! A plain-HTTP server on this computer's own network address (not 127.0.0.1), for tests of the
//! encrypted-only setting: `playr::privacy` treats that address as another computer, while the
//! traffic never leaves the machine.
//!
//! The address is found without sending anything: connecting a UDP socket only picks the
//! outgoing interface. Each connection is logged as either a TLS ClientHello (the first byte of a
//! TLS record is 0x16; the connection is then closed, as a server without TLS would fail it) or
//! the path of a plain GET.
#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Seen {
    Tls,
    Get(String),
}

pub struct Route {
    pub path: String,
    pub content_type: &'static str,
    pub body: Vec<u8>,
    /// Answer `Range` with 206 and `Content-Range`, like a file server.
    pub ranged: bool,
    /// Send `Content-Length` (a file) or not (a radio stream).
    pub with_length: bool,
}

pub fn route(path: &str, content_type: &'static str, body: Vec<u8>, ranged: bool, with_length: bool) -> Route {
    Route { path: path.to_string(), content_type, body, ranged, with_length }
}

/// This computer's address on the network it would use to reach the internet, if it has one.
pub fn own_ip() -> Option<IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

pub struct OwnServer {
    addr: SocketAddr,
    log: Arc<Mutex<Vec<Seen>>>,
}

impl OwnServer {
    /// `None` when the machine has no network address other than loopback.
    pub fn start(routes: Vec<Route>) -> Option<Self> {
        let ip = own_ip()?;
        let listener = TcpListener::bind((ip, 0)).ok()?;
        let addr = listener.local_addr().ok()?;
        let log = Arc::new(Mutex::new(Vec::new()));
        let routes = Arc::new(routes);
        let seen = log.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(conn) = conn else { continue };
                let (seen, routes) = (seen.clone(), routes.clone());
                std::thread::spawn(move || handle(conn, &routes, &seen));
            }
        });
        Some(Self { addr, log })
    }

    pub fn url(&self, path: &str) -> String {
        let host = match self.addr.ip() {
            IpAddr::V6(v6) => format!("[{v6}]"),
            IpAddr::V4(v4) => v4.to_string(),
        };
        format!("http://{host}:{}{path}", self.addr.port())
    }

    pub fn host(&self) -> String {
        self.addr.ip().to_string()
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.log.lock().unwrap().clone()
    }
}

fn handle(mut conn: TcpStream, routes: &[Route], log: &Mutex<Vec<Seen>>) {
    let _ = conn.set_read_timeout(Some(Duration::from_secs(5)));
    let mut first = [0u8; 1];
    if conn.read(&mut first).ok() != Some(1) {
        return;
    }
    if first[0] == 0x16 {
        log.lock().unwrap().push(Seen::Tls);
        return;
    }
    let mut head = vec![first[0]];
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") && head.len() < 16 * 1024 {
        match conn.read(&mut byte) {
            Ok(1) => head.push(byte[0]),
            _ => return,
        }
    }
    let text = String::from_utf8_lossy(&head).into_owned();
    let path = text.split(' ').nth(1).unwrap_or("").to_string();
    log.lock().unwrap().push(Seen::Get(path.clone()));
    let range = text
        .lines()
        .find_map(|l| l.split_once(':').filter(|(n, _)| n.trim().eq_ignore_ascii_case("range")).map(|(_, v)| v.trim().to_string()));
    let bare = path.split('?').next().unwrap_or("");
    let Some(route) = routes.iter().find(|r| r.path == bare) else {
        let _ = conn.write_all(b"HTTP/1.0 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return;
    };
    let len = route.body.len();
    let wanted = range.as_deref().and_then(|r| r.strip_prefix("bytes=")).and_then(|r| r.split_once('-')).map(|(a, b)| {
        let from = a.parse::<usize>().unwrap_or(0).min(len);
        let to = b.parse::<usize>().map_or(len, |b| (b + 1).min(len));
        (from, to.max(from))
    });
    let (head, body) = match (route.ranged, wanted) {
        (true, Some((from, to))) => (
            format!(
                "HTTP/1.0 206 Partial Content\r\nContent-Type: {}\r\nContent-Length: {}\r\nContent-Range: bytes {from}-{}/{len}\r\nConnection: close\r\n\r\n",
                route.content_type,
                to - from,
                to.saturating_sub(1)
            ),
            &route.body[from..to],
        ),
        _ => {
            let length = if route.with_length { format!("Content-Length: {len}\r\n") } else { String::new() };
            (format!("HTTP/1.0 200 OK\r\nContent-Type: {}\r\n{length}Connection: close\r\n\r\n", route.content_type), &route.body[..])
        }
    };
    let _ = conn.write_all(head.as_bytes());
    let _ = conn.write_all(body);
    let _ = conn.flush();
}
