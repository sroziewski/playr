//! Seekable playback of fragmented MP4 over HTTP, the way YouTube serves video: picture and
//! sound as two separate files (DASH), each a header (`ftyp`, `moov`, a `sidx` fragment index)
//! followed by a few thousand `moof` + `mdat` fragments that each start on a keyframe.
//!
//! Symphonia can read such a file but finds a far fragment only by walking every fragment
//! before it, which over HTTP is one request per fragment. [`DashDemuxer`] seeks with the
//! `sidx` instead: it hands symphonia the cached header followed directly by the fragment that
//! holds the target time (see [`SplicedSource`]), so a seek costs one request.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use symphonia::core::io::MediaSource;

use crate::demux::{Demuxer, SymDemuxer};
use crate::media::{MediaInfo, Packet};
use crate::net::{self, PipeReader};

/// Bytes asked for per HTTP request. YouTube throttles a connection that asks for much more
/// than this at once; yt-dlp uses the same size.
pub const CHUNK: u64 = 10 << 20;
/// A forward jump this short is made by reading through the bytes in between rather than by
/// opening a new connection, which costs a round trip.
const SKIP_BY_READING: u64 = 512 << 10;
/// What is fetched first: enough for the header of every YouTube file seen so far. A longer
/// header is fetched in full in a second request.
const HEAD_GUESS: u64 = 256 << 10;
const MAX_HEAD: u64 = 16 << 20;
/// The end of the file kept in memory. Symphonia's probe looks at the last 160 bytes for
/// ID3v1 and APE tags each time a reader is opened, which would otherwise cost a request per
/// seek.
const TAIL: u64 = 4096;
/// Packets read since a reader was opened are kept, up to this many bytes, so that seeking
/// back to where it started (which the player does right after scanning for a keyframe)
/// replays them instead of downloading them again.
const REPLAY_LIMIT: usize = 16 << 20;
/// Attempts per chunk before a broken download is given up.
const ATTEMPTS: u32 = 3;

/// Byte-range access to one remote file.
pub trait RangeFetch: Send + Sync {
    /// Length of the file in bytes.
    fn len(&self) -> u64;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// A reader of the bytes `from..to` (`to` exclusive, at most [`len`](Self::len)).
    fn open(&self, from: u64, to: u64) -> Result<Box<dyn Read + Send>, String>;
}

/// The HTTP client every playr download outside the radio path uses.
pub fn agent_with(https_only: bool) -> ureq::Agent {
    // rustls is built without a bundled crypto backend; installing the pure-Rust one a second
    // time is a harmless error.
    let _ = rustls_rustcrypto::provider().install_default();
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_recv_response(Some(Duration::from_secs(20)))
        .max_redirects(8)
        .https_only(https_only)
        .user_agent("playr/0.1")
        .build()
        .into()
}

/// Reads a whole (small) resource, at most `limit` bytes of it.
pub fn get_bytes(url: &str, headers: &[(String, String)], limit: u64) -> Result<Vec<u8>, String> {
    let resp = crate::privacy::fetch(url, |a| {
        let mut req = agent_with(a.https_only).get(&a.url);
        for (name, value) in headers {
            req = req.header(name.as_str(), value.as_str());
        }
        req.call().map_err(|e| format!("cannot download {}: {e}", short(&a.url)))
    })?;
    let mut raw = Vec::new();
    resp.into_body()
        .into_reader()
        .take(limit)
        .read_to_end(&mut raw)
        .map_err(|e| format!("cannot download {}: {e}", short(url)))?;
    Ok(raw)
}

/// Reads a whole (small) text resource, at most `limit` bytes of it.
pub fn get_text(url: &str, headers: &[(String, String)], limit: u64) -> Result<String, String> {
    Ok(String::from_utf8_lossy(&get_bytes(url, headers, limit)?).into_owned())
}

/// A URL cut down for an error message (stream addresses run to a kilobyte of query string).
fn short(url: &str) -> &str {
    let mut end = url.find('?').unwrap_or(url.len()).min(80);
    while !url.is_char_boundary(end) {
        end -= 1;
    }
    &url[..end]
}

/// A remote file read with HTTP `Range` requests.
pub struct HttpFetch {
    agent: ureq::Agent,
    url: std::sync::Mutex<String>,
    origin: String,
    headers: Vec<(String, String)>,
    len: u64,
}

impl HttpFetch {
    /// Connects and reads the first `head` bytes, which also reveals the file's length.
    pub fn connect(url: &str, headers: &[(String, String)], head: u64) -> Result<(Self, Vec<u8>), String> {
        crate::privacy::fetch(url, |a| Self::connect_once(&a.url, a.https_only, headers, head))
    }

    fn connect_once(url: &str, https_only: bool, headers: &[(String, String)], head: u64) -> Result<(Self, Vec<u8>), String> {
        let mut fetch = Self {
            agent: agent_with(https_only),
            url: std::sync::Mutex::new(url.to_string()),
            origin: url.to_string(),
            headers: headers.to_vec(),
            len: 0,
        };
        let resp = fetch.request_at(url, 0, head)?;
        let total = resp
            .headers()
            .get("content-range")
            .and_then(|v| v.to_str().ok())
            .and_then(content_range_total)
            .ok_or_else(|| format!("{} does not support ranged downloads", short(url)))?;
        fetch.len = total;
        *fetch.url.lock().unwrap() = ureq::ResponseExt::get_uri(&resp).to_string();
        let mut bytes = Vec::new();
        resp.into_body()
            .into_reader()
            .take(head)
            .read_to_end(&mut bytes)
            .map_err(|e| format!("cannot download {}: {e}", short(url)))?;
        Ok((fetch, bytes))
    }

    pub fn is_encrypted(&self) -> bool {
        !crate::privacy::is_unencrypted(&self.url.lock().unwrap())
    }

    fn request(&self, from: u64, to: u64) -> Result<ureq::http::Response<ureq::Body>, String> {
        let cached = self.url.lock().unwrap().clone();
        match self.request_at(&cached, from, to) {
            Err(_) if cached != self.origin => {
                let resp = self.request_at(&self.origin, from, to)?;
                *self.url.lock().unwrap() = ureq::ResponseExt::get_uri(&resp).to_string();
                Ok(resp)
            }
            result => result,
        }
    }

    fn request_at(&self, url: &str, from: u64, to: u64) -> Result<ureq::http::Response<ureq::Body>, String> {
        let mut req = self.agent.get(url).header("Range", format!("bytes={from}-{}", to.saturating_sub(1)));
        for (name, value) in &self.headers {
            req = req.header(name.as_str(), value.as_str());
        }
        let resp = req.call().map_err(|e| format!("cannot download {}: {e}", short(&self.origin)))?;
        if resp.status().as_u16() != 206 {
            return Err(format!("{} ignored the byte range (HTTP {})", short(&self.origin), resp.status().as_u16()));
        }
        Ok(resp)
    }
}

impl RangeFetch for HttpFetch {
    fn len(&self) -> u64 {
        self.len
    }

    fn open(&self, from: u64, to: u64) -> Result<Box<dyn Read + Send>, String> {
        Ok(Box::new(self.request(from, to)?.into_body().into_reader()))
    }
}

/// The total from a `Content-Range: bytes 0-99/1234` header.
pub fn content_range_total(value: &str) -> Option<u64> {
    value.trim().strip_prefix("bytes")?.rsplit_once('/')?.1.trim().parse().ok()
}

/// Reads `from..to` of a remote file as one stream, in [`CHUNK`]-sized requests, reconnecting
/// where a request breaks off.
pub struct ChunkedReader {
    fetch: Arc<dyn RangeFetch>,
    pos: u64,
    end: u64,
    chunk_size: u64,
    current: Option<(Box<dyn Read + Send>, u64)>,
    abort: Arc<AtomicBool>,
}

impl ChunkedReader {
    pub fn new(fetch: Arc<dyn RangeFetch>, from: u64, chunk_size: u64, abort: Arc<AtomicBool>) -> Self {
        let end = fetch.len();
        Self { fetch, pos: from.min(end), end, chunk_size: chunk_size.max(1), current: None, abort }
    }
}

impl Read for ChunkedReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut failures = 0;
        loop {
            if self.pos >= self.end || buf.is_empty() {
                return Ok(0);
            }
            if self.abort.load(Ordering::Relaxed) {
                return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "download cancelled"));
            }
            if self.current.is_none() {
                let to = (self.pos + self.chunk_size).min(self.end);
                match self.fetch.open(self.pos, to) {
                    Ok(reader) => self.current = Some((reader, to)),
                    Err(e) => {
                        failures += 1;
                        if failures >= ATTEMPTS {
                            return Err(io::Error::other(e));
                        }
                        continue;
                    }
                }
            }
            let (reader, to) = self.current.as_mut().expect("opened above");
            let want = buf.len().min((*to - self.pos) as usize);
            match reader.read(&mut buf[..want]) {
                Ok(0) => {
                    // The server closed the chunk early (or it is done): ask again from here.
                    let done = self.pos >= *to;
                    self.current = None;
                    if !done {
                        failures += 1;
                        if failures >= ATTEMPTS {
                            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "download broke off"));
                        }
                    }
                }
                Ok(n) => {
                    self.pos += n as u64;
                    if self.pos >= *to {
                        self.current = None;
                    }
                    return Ok(n);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    self.current = None;
                    failures += 1;
                    if failures >= ATTEMPTS {
                        return Err(e);
                    }
                }
            }
        }
    }
}

/// A remote file as a seekable [`MediaSource`]. Reading runs a download thread a couple of
/// megabytes ahead of the reader; a seek that leaves that window starts a new download.
pub struct RangeSource {
    fetch: Arc<dyn RangeFetch>,
    len: u64,
    pos: u64,
    pipe: Option<(PipeReader, u64)>,
    abort: Arc<AtomicBool>,
    chunk_size: u64,
}

impl RangeSource {
    pub fn new(fetch: Arc<dyn RangeFetch>, abort: Arc<AtomicBool>) -> Self {
        Self::with_chunk_size(fetch, abort, CHUNK)
    }

    pub fn with_chunk_size(fetch: Arc<dyn RangeFetch>, abort: Arc<AtomicBool>, chunk_size: u64) -> Self {
        let len = fetch.len();
        Self { fetch, len, pos: 0, pipe: None, abort, chunk_size }
    }

    /// Byte offset of the next read.
    pub fn position(&self) -> u64 {
        self.pos
    }
}

impl Read for RangeSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.len || buf.is_empty() {
            return Ok(0);
        }
        if let Some((_, at)) = &self.pipe
            && !(*at <= self.pos && self.pos - *at <= SKIP_BY_READING)
        {
            // Dropping the reading end makes the old download thread stop at its next chunk.
            self.pipe = None;
        }
        if self.pipe.is_none() {
            let reader = ChunkedReader::new(self.fetch.clone(), self.pos, self.chunk_size, self.abort.clone());
            self.pipe = Some((net::spawn_pipe(reader, self.abort.clone()), self.pos));
        }
        let (pipe, at) = self.pipe.as_mut().expect("set above");
        let mut scratch = [0u8; 8192];
        while *at < self.pos {
            let want = scratch.len().min((self.pos - *at) as usize);
            let n = pipe.read(&mut scratch[..want])?;
            if n == 0 {
                return Ok(0);
            }
            *at += n as u64;
        }
        let n = pipe.read(buf)?;
        *at += n as u64;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for RangeSource {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let target = match to {
            SeekFrom::Start(p) => Some(p),
            SeekFrom::End(d) => self.len.checked_add_signed(d),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        };
        self.pos = target.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek before the start"))?;
        Ok(self.pos)
    }
}

impl MediaSource for RangeSource {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.len)
    }
}

/// One fragment as the `sidx` describes it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fragment {
    /// Byte offset of its `moof` in the file.
    pub offset: u64,
    pub size: u64,
    /// Seconds from the start of the first fragment.
    pub start: f64,
    pub duration: f64,
}

/// Where the fragments of a DASH file are.
#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    /// Length of the header: everything before the first fragment.
    pub head_len: u64,
    pub fragments: Vec<Fragment>,
}

impl Layout {
    pub fn duration(&self) -> f64 {
        self.fragments.last().map_or(0.0, |f| f.start + f.duration)
    }

    /// Index of the fragment that holds `secs`: the last one starting at or before it (the
    /// first one for times before the start, the last one for times past the end).
    pub fn fragment_at(&self, secs: f64) -> usize {
        self.fragments.partition_point(|f| f.start <= secs).saturating_sub(1)
    }
}

#[derive(Debug, PartialEq)]
pub enum LayoutError {
    /// The header is longer than the bytes given: this many are needed.
    NeedBytes(u64),
    Invalid(String),
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    b.get(at..at + 4).map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

fn be64(b: &[u8], at: usize) -> Option<u64> {
    b.get(at..at + 8).map(|s| u64::from_be_bytes(s.try_into().expect("8 bytes")))
}

/// Reads the top-level boxes at the start of a fragmented MP4 up to the first fragment and
/// returns its fragment index.
pub fn parse_layout(bytes: &[u8]) -> Result<Layout, LayoutError> {
    let invalid = |what: &str| LayoutError::Invalid(format!("not a DASH MP4 file: {what}"));
    let mut pos = 0u64;
    let mut sidx: Option<(u64, u64)> = None;
    let mut moov = false;
    loop {
        let at = pos as usize;
        if bytes.len() < at + 16 {
            return Err(LayoutError::NeedBytes(pos + 16));
        }
        let mut size = be32(bytes, at).expect("length checked") as u64;
        let kind = &bytes[at + 4..at + 8];
        if size == 1 {
            size = be64(bytes, at + 8).expect("length checked");
        } else if size == 0 {
            return Err(invalid("a box runs to the end of the file before any fragment"));
        }
        if size < 8 {
            return Err(invalid("a box is shorter than its header"));
        }
        match kind {
            b"moof" | b"mdat" => break,
            b"moov" => moov = true,
            b"sidx" if sidx.is_none() => sidx = Some((pos, size)),
            _ => {}
        }
        pos = pos.checked_add(size).ok_or_else(|| invalid("a box size overflows"))?;
        if pos > MAX_HEAD {
            return Err(invalid("no fragment in the first 16 MB"));
        }
    }
    if !moov {
        return Err(invalid("no moov box before the first fragment"));
    }
    let (sidx_at, sidx_size) = sidx.ok_or_else(|| invalid("no sidx fragment index"))?;
    let need = sidx_at + sidx_size;
    if (bytes.len() as u64) < need {
        return Err(LayoutError::NeedBytes(need));
    }
    let b = &bytes[sidx_at as usize..need as usize];
    let header = if be32(b, 0) == Some(1) { 16 } else { 8 };
    let short = || invalid("the sidx is cut short");
    let version = *b.get(header).ok_or_else(short)?;
    let timescale = be32(b, header + 8).ok_or_else(short)?;
    if timescale == 0 {
        return Err(invalid("the sidx has a zero timescale"));
    }
    let (first_offset, mut at) = if version == 0 {
        (be32(b, header + 16).ok_or_else(short)? as u64, header + 20)
    } else {
        (be64(b, header + 20).ok_or_else(short)?, header + 28)
    };
    let count = b.get(at + 2..at + 4).map(|c| u16::from_be_bytes([c[0], c[1]])).ok_or_else(short)? as usize;
    at += 4;
    let mut offset = need.checked_add(first_offset).ok_or_else(|| invalid("the sidx offset overflows"))?;
    let mut ticks = 0u64;
    let mut fragments = Vec::with_capacity(count);
    for i in 0..count {
        let e = at + i * 12;
        let reference = be32(b, e).ok_or_else(short)?;
        let duration = be32(b, e + 4).ok_or_else(short)? as u64;
        if reference >> 31 == 1 {
            return Err(invalid("nested fragment indexes are not supported"));
        }
        let size = (reference & 0x7fff_ffff) as u64;
        fragments.push(Fragment {
            offset,
            size,
            start: ticks as f64 / timescale as f64,
            duration: duration as f64 / timescale as f64,
        });
        offset += size;
        ticks += duration;
    }
    if fragments.is_empty() {
        return Err(invalid("the sidx lists no fragments"));
    }
    Ok(Layout { head_len: fragments[0].offset, fragments })
}

/// The bytes a [`SplicedSource`] puts between the header and the fragment it jumps to: one
/// `free` box spanning exactly the skipped fragments, which an MP4 reader steps over.
pub fn free_box_header(gap: u64) -> Option<Vec<u8>> {
    if gap == 0 {
        return Some(Vec::new());
    }
    if gap < 8 {
        return None;
    }
    if gap <= u32::MAX as u64 {
        return Some([&(gap as u32).to_be_bytes()[..], b"free"].concat());
    }
    if gap < 16 {
        return None;
    }
    Some([&1u32.to_be_bytes()[..], b"free", &gap.to_be_bytes()[..]].concat())
}

/// A fragmented MP4 file with the fragments before `jump_to` hidden: the cached header, then a
/// `free` box covering the skipped fragments, then the file from `jump_to` on. Every byte keeps
/// its offset in the real file, so offsets inside the file stay valid.
pub struct SplicedSource {
    head: Arc<[u8]>,
    tail: Arc<[u8]>,
    filler: Vec<u8>,
    jump_to: u64,
    inner: RangeSource,
    pos: u64,
}

impl SplicedSource {
    /// `head` must be the file's bytes before its first fragment, `tail` its last bytes (may be
    /// empty) and `jump_to` the offset of a fragment.
    pub fn new(head: Arc<[u8]>, tail: Arc<[u8]>, jump_to: u64, inner: RangeSource) -> Result<Self, String> {
        let head_len = head.len() as u64;
        let filler = jump_to
            .checked_sub(head_len)
            .and_then(free_box_header)
            .ok_or("cannot jump into the middle of the file header")?;
        Ok(Self { head, tail, filler, jump_to, inner, pos: 0 })
    }
}

impl Read for SplicedSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let head_len = self.head.len() as u64;
        let tail_from = self.inner.len.saturating_sub(self.tail.len() as u64);
        if buf.is_empty() || self.pos >= self.inner.len {
            return Ok(0);
        }
        let n = if self.pos < head_len {
            let from = &self.head[self.pos as usize..];
            let n = buf.len().min(from.len());
            buf[..n].copy_from_slice(&from[..n]);
            n
        } else if self.pos < self.jump_to {
            let into_gap = self.pos - head_len;
            let n = buf.len().min((self.jump_to - self.pos).min(usize::MAX as u64) as usize);
            for (i, byte) in buf[..n].iter_mut().enumerate() {
                *byte = self.filler.get(into_gap as usize + i).copied().unwrap_or(0);
            }
            n
        } else if self.pos >= tail_from {
            let from = &self.tail[(self.pos - tail_from) as usize..];
            let n = buf.len().min(from.len());
            buf[..n].copy_from_slice(&from[..n]);
            n
        } else {
            let until_tail = buf.len().min((tail_from - self.pos).min(usize::MAX as u64) as usize);
            let buf = &mut buf[..until_tail];
            if self.inner.position() != self.pos {
                self.inner.seek(SeekFrom::Start(self.pos))?;
            }
            self.inner.read(buf)?
        };
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for SplicedSource {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let len = self.inner.len;
        let target = match to {
            SeekFrom::Start(p) => Some(p),
            SeekFrom::End(d) => len.checked_add_signed(d),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        };
        self.pos = target.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek before the start"))?;
        Ok(self.pos)
    }
}

impl MediaSource for SplicedSource {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.inner.len)
    }
}

/// A remote DASH file, ready to be read from any fragment.
#[derive(Clone)]
pub struct DashFile {
    fetch: Arc<dyn RangeFetch>,
    head: Arc<[u8]>,
    tail: Arc<[u8]>,
    layout: Arc<Layout>,
    chunk_size: u64,
    replay_limit: usize,
}

impl DashFile {
    /// Reads the header of a remote file over HTTP.
    pub fn connect(url: &str, headers: &[(String, String)]) -> Result<Self, String> {
        let (fetch, bytes) = HttpFetch::connect(url, headers, HEAD_GUESS)?;
        Self::from_fetch(Arc::new(fetch), bytes)
    }

    /// `first` is the start of the file as far as it has already been read.
    pub fn from_fetch(fetch: Arc<dyn RangeFetch>, mut first: Vec<u8>) -> Result<Self, String> {
        let layout = loop {
            match parse_layout(&first) {
                Ok(layout) => break layout,
                Err(LayoutError::Invalid(e)) => return Err(e),
                Err(LayoutError::NeedBytes(need)) => {
                    let have = first.len() as u64;
                    if need > fetch.len() || need > MAX_HEAD || need <= have {
                        return Err("not a DASH MP4 file: the header is cut short".into());
                    }
                    let mut more = Vec::new();
                    fetch.open(have, need)?.take(need - have).read_to_end(&mut more).map_err(|e| e.to_string())?;
                    if more.is_empty() {
                        return Err("not a DASH MP4 file: the header is cut short".into());
                    }
                    first.extend_from_slice(&more);
                }
            }
        };
        first.truncate(layout.head_len as usize);
        let len = fetch.len();
        let tail_from = len.saturating_sub(TAIL).max(layout.head_len);
        let mut tail = Vec::new();
        if tail_from < len {
            fetch
                .open(tail_from, len)?
                .take(len - tail_from)
                .read_to_end(&mut tail)
                .map_err(|e| format!("cannot read the end of the file: {e}"))?;
            if tail.len() as u64 != len - tail_from {
                tail.clear();
            }
        }
        Ok(Self { fetch, head: first.into(), tail: tail.into(), layout: Arc::new(layout), chunk_size: CHUNK, replay_limit: REPLAY_LIMIT })
    }

    /// Requests of `chunk_size` bytes instead of [`CHUNK`] (tests use this to cross chunk
    /// boundaries with small files).
    pub fn with_chunk_size(mut self, chunk_size: u64) -> Self {
        self.chunk_size = chunk_size;
        self
    }

    /// Stops recording packets for replay after `bytes` instead of [`REPLAY_LIMIT`].
    pub fn with_replay_limit(mut self, bytes: usize) -> Self {
        self.replay_limit = bytes;
        self
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }
}

/// Reads a [`DashFile`] with symphonia and seeks it fragment by fragment.
pub struct DashDemuxer {
    file: DashFile,
    inner: SymDemuxer,
    /// The fragment `inner` began reading at.
    fragment: usize,
    /// Start time of that fragment; symphonia counts time from there.
    offset: f64,
    info: MediaInfo,
    abort: Arc<AtomicBool>,
    /// Every packet `inner` has produced, while they fit in [`REPLAY_LIMIT`]; `None` once they
    /// did not.
    recorded: Option<Vec<Packet>>,
    recorded_bytes: usize,
    /// Recorded packets being handed out again after a seek back to `fragment`.
    replay: std::collections::VecDeque<Packet>,
}

fn clone_packet(packet: &Packet) -> Packet {
    match packet {
        Packet::Video { pts, data } => Packet::Video { pts: *pts, data: data.clone() },
        Packet::Audio { pts, packet, skip, limit } => {
            Packet::Audio { pts: *pts, packet: packet.clone(), skip: *skip, limit: *limit }
        }
    }
}

fn packet_bytes(packet: &Packet) -> usize {
    match packet {
        Packet::Video { data, .. } => data.len(),
        Packet::Audio { packet, .. } => packet.data.len(),
    }
}

impl DashDemuxer {
    pub fn open(file: DashFile, abort: Arc<AtomicBool>) -> Result<Self, String> {
        let inner = Self::reader_at(&file, 0, &abort)?;
        let mut info = inner.info().clone();
        info.duration = Some(file.layout.duration());
        info.seekable = true;
        Ok(Self {
            file,
            inner,
            fragment: 0,
            offset: 0.0,
            info,
            abort,
            recorded: Some(Vec::new()),
            recorded_bytes: 0,
            replay: Default::default(),
        })
    }

    fn reader_at(file: &DashFile, fragment: usize, abort: &Arc<AtomicBool>) -> Result<SymDemuxer, String> {
        let source = RangeSource::with_chunk_size(file.fetch.clone(), abort.clone(), file.chunk_size);
        let jump_to = file.layout.fragments[fragment].offset;
        let spliced = SplicedSource::new(file.head.clone(), file.tail.clone(), jump_to, source)?;
        SymDemuxer::open(Box::new(spliced), Some("mp4"), None)
    }
}

impl Demuxer for DashDemuxer {
    fn info(&self) -> &MediaInfo {
        &self.info
    }

    fn next_packet(&mut self) -> Result<Option<Packet>, String> {
        if let Some(packet) = self.replay.pop_front() {
            return Ok(Some(packet));
        }
        let Some(packet) = self.inner.next_packet()? else { return Ok(None) };
        let packet = match packet {
            Packet::Video { pts, data } => Packet::Video { pts: pts + self.offset, data },
            Packet::Audio { pts, packet, skip, limit } => Packet::Audio { pts: pts + self.offset, packet, skip, limit },
        };
        if let Some(recorded) = self.recorded.as_mut() {
            self.recorded_bytes += packet_bytes(&packet);
            if self.recorded_bytes > self.file.replay_limit {
                self.recorded = None;
            } else {
                recorded.push(clone_packet(&packet));
            }
        }
        Ok(Some(packet))
    }

    fn seek(&mut self, secs: f64) -> Result<f64, String> {
        let fragment = self.file.layout.fragment_at(secs.max(0.0));
        if fragment == self.fragment
            && let Some(recorded) = &self.recorded
        {
            self.replay = recorded.iter().map(clone_packet).collect();
            return Ok(self.offset);
        }
        self.inner = Self::reader_at(&self.file, fragment, &self.abort)?;
        self.fragment = fragment;
        self.offset = self.file.layout.fragments[fragment].start;
        self.recorded = Some(Vec::new());
        self.recorded_bytes = 0;
        self.replay.clear();
        Ok(self.offset)
    }
}

/// Plays a video-only and an audio-only demuxer as one, handing out their packets in
/// presentation order.
pub struct MergedDemuxer {
    video: Box<dyn Demuxer + Send>,
    audio: Box<dyn Demuxer + Send>,
    next_video: Option<Packet>,
    next_audio: Option<Packet>,
    video_done: bool,
    audio_done: bool,
    info: MediaInfo,
}

impl MergedDemuxer {
    pub fn new(video: Box<dyn Demuxer + Send>, audio: Box<dyn Demuxer + Send>) -> Self {
        let (v, a) = (video.info(), audio.info());
        let duration = match (v.duration, a.duration) {
            (Some(x), Some(y)) => Some(x.max(y)),
            (x, y) => x.or(y),
        };
        let info = MediaInfo {
            container: v.container.clone(),
            duration,
            seekable: v.seekable && a.seekable,
            video: v.video.clone(),
            audio: a.audio.clone(),
            notes: v.notes.iter().chain(&a.notes).cloned().collect(),
        };
        Self { video, audio, next_video: None, next_audio: None, video_done: false, audio_done: false, info }
    }
}

impl Demuxer for MergedDemuxer {
    fn info(&self) -> &MediaInfo {
        &self.info
    }

    fn unencrypted(&self) -> bool {
        self.video.unencrypted() || self.audio.unencrypted()
    }

    fn next_packet(&mut self) -> Result<Option<Packet>, String> {
        if self.next_video.is_none() && !self.video_done {
            self.next_video = self.video.next_packet()?;
            self.video_done = self.next_video.is_none();
        }
        if self.next_audio.is_none() && !self.audio_done {
            self.next_audio = self.audio.next_packet()?;
            self.audio_done = self.next_audio.is_none();
        }
        let video_first = match (&self.next_video, &self.next_audio) {
            (Some(v), Some(a)) => v.pts() <= a.pts(),
            (Some(_), None) => true,
            _ => false,
        };
        Ok(if video_first { self.next_video.take() } else { self.next_audio.take() })
    }

    fn seek(&mut self, secs: f64) -> Result<f64, String> {
        self.next_video = None;
        self.next_audio = None;
        self.video_done = false;
        self.audio_done = false;
        // Each side may wait a second for its server; they wait at the same time.
        let (video, audio) = (&mut self.video, &mut self.audio);
        let (video, audio) = std::thread::scope(|s| {
            let audio = s.spawn(move || audio.seek(secs));
            let video = video.seek(secs);
            (video, audio.join().unwrap_or_else(|_| Err("internal error while seeking the sound".into())))
        });
        Ok(video?.min(audio?))
    }
}
