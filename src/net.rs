//! HTTP sources: internet radio (Icecast / SHOUTcast with in-band ICY metadata), plain HTTP
//! media downloads and playlist indirection (`.m3u`, `.pls`).

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use symphonia::core::io::MediaSource;

pub type TitleCallback = Box<dyn FnMut(String) + Send + Sync>;

pub fn is_url(s: &str) -> bool {
    let lower = s.trim_start().to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// What a failed connection means when the site's certificate came from an authority nobody
/// trusts: on a working network that is a web filter (a company firewall such as FortiGate)
/// answering in the site's place with its own certificate and a block page. Certificates that
/// are merely expired or for another name are not interception and are left as they are.
pub fn blocked_by_network(error: &str) -> Option<String> {
    const SIGNS: [&str; 4] = [
        "unable to get local issuer certificate",
        "self signed certificate in certificate chain",
        "self-signed certificate in certificate chain",
        "UnknownIssuer",
    ];
    SIGNS.iter().any(|s| error.contains(s)).then(|| {
        "Blocked by your network: the connection to this site was intercepted (its certificate is not from a trusted authority), most likely by a company or school web filter. It should play on another network.".to_string()
    })
}

/// Extracts `StreamTitle` from an ICY metadata block (`StreamTitle='Artist - Song';...`).
/// Returns `None` when the field is absent or empty.
pub fn parse_stream_title(meta: &[u8]) -> Option<String> {
    let text = match std::str::from_utf8(meta) {
        Ok(s) => s.to_string(),
        // Older stations send Latin-1.
        Err(_) => meta.iter().map(|&b| b as char).collect(),
    };
    let start = text.find("StreamTitle='")? + "StreamTitle='".len();
    let rest = &text[start..];
    // A title may itself contain an apostrophe, so the field ends at `';`, not at `'`.
    let end = rest.find("';").or_else(|| rest.trim_end_matches('\0').rfind('\''))?;
    let title = rest[..end].trim();
    (!title.is_empty()).then(|| title.to_string())
}

/// First stream URL of an M3U or PLS playlist. `Err` for HLS playlists, which are segment
/// lists rather than stream addresses.
pub fn parse_playlist(text: &str) -> Result<Option<String>, String> {
    if text.contains("#EXT-X-TARGETDURATION") || text.contains("#EXT-X-STREAM-INF") {
        return Err("HLS streams are not supported".into());
    }
    for line in text.lines() {
        let line = line.trim().trim_start_matches('\u{feff}');
        if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
            continue;
        }
        // PLS: `File1=http://...`; M3U: the bare address.
        let candidate = match line.split_once('=') {
            Some((key, value)) if key.trim().to_ascii_lowercase().starts_with("file") => value.trim(),
            _ => line,
        };
        if is_url(candidate) {
            return Ok(Some(candidate.to_string()));
        }
    }
    Ok(None)
}

pub fn is_playlist_name(name: &str) -> bool {
    let path = name.split(['?', '#']).next().unwrap_or(name).to_ascii_lowercase();
    path.ends_with(".m3u") || path.ends_with(".m3u8") || path.ends_with(".pls")
}

fn is_playlist_mime(mime: &str) -> bool {
    matches!(
        mime,
        "audio/x-mpegurl" | "audio/mpegurl" | "application/x-mpegurl" | "application/vnd.apple.mpegurl"
            | "audio/x-scpls" | "application/pls+xml" | "application/pls"
    )
}

/// File extension symphonia should be hinted with for a given `Content-Type`.
pub fn mime_to_ext(mime: &str) -> Option<&'static str> {
    Some(match mime {
        "audio/mpeg" | "audio/mp3" | "audio/x-mpeg" => "mp3",
        "audio/aac" | "audio/aacp" | "audio/x-aac" => "aac",
        "audio/ogg" | "application/ogg" | "audio/vorbis" => "ogg",
        "audio/flac" | "audio/x-flac" => "flac",
        "audio/wav" | "audio/x-wav" | "audio/wave" => "wav",
        "audio/mp4" | "audio/x-m4a" | "video/mp4" => "mp4",
        "video/x-matroska" | "audio/x-matroska" | "video/webm" | "audio/webm" => "mkv",
        _ => return None,
    })
}

/// Removes the ICY metadata blocks that a SHOUTcast-style server interleaves every `metaint`
/// audio bytes, reporting each new stream title through the callback.
pub struct IcyReader<R> {
    inner: R,
    metaint: usize,
    until_meta: usize,
    last_title: Option<String>,
    on_title: TitleCallback,
}

impl<R: Read> IcyReader<R> {
    /// `metaint` is the value of the `icy-metaint` response header and must be non-zero.
    pub fn new(inner: R, metaint: usize, on_title: TitleCallback) -> Self {
        let metaint = metaint.max(1);
        Self { inner, metaint, until_meta: metaint, last_title: None, on_title }
    }
}

impl<R: Read> Read for IcyReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.until_meta == 0 {
            let mut len = [0u8; 1];
            match self.inner.read_exact(&mut len) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(0),
                Err(e) => return Err(e),
            }
            let mut meta = vec![0u8; len[0] as usize * 16];
            self.inner.read_exact(&mut meta)?;
            if let Some(title) = parse_stream_title(&meta)
                && self.last_title.as_deref() != Some(title.as_str()) {
                    self.last_title = Some(title.clone());
                    (self.on_title)(title);
                }
            self.until_meta = self.metaint;
        }
        let want = buf.len().min(self.until_meta);
        let got = self.inner.read(&mut buf[..want])?;
        self.until_meta -= got;
        Ok(got)
    }
}

/// The reading end of a network download that runs on its own thread.
///
/// Decoupling the socket from the decoder gives three things a bare blocking read cannot:
/// a jitter buffer, detection of a silently stalled connection, and a way to abandon the
/// stream promptly when the user opens something else.
pub struct PipeReader {
    rx: Mutex<Receiver<Vec<u8>>>,
    chunk: Vec<u8>,
    pos: usize,
    abort: Arc<AtomicBool>,
    stall_timeout: Duration,
}

impl PipeReader {
    pub fn new(rx: Receiver<Vec<u8>>, abort: Arc<AtomicBool>, stall_timeout: Duration) -> Self {
        Self { rx: Mutex::new(rx), chunk: Vec::new(), pos: 0, abort, stall_timeout }
    }
}

impl Read for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.chunk.len() {
            let started = Instant::now();
            loop {
                if self.abort.load(Ordering::Relaxed) {
                    return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "stream closed"));
                }
                match self.rx.lock().unwrap().recv_timeout(Duration::from_millis(100)) {
                    Ok(chunk) if chunk.is_empty() => continue,
                    Ok(chunk) => {
                        self.chunk = chunk;
                        self.pos = 0;
                        break;
                    }
                    Err(RecvTimeoutError::Disconnected) => return Ok(0),
                    Err(RecvTimeoutError::Timeout) if started.elapsed() >= self.stall_timeout => {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "stream stalled"));
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                }
            }
        }
        let n = buf.len().min(self.chunk.len() - self.pos);
        buf[..n].copy_from_slice(&self.chunk[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// Starts a thread that copies `body` into a bounded channel and returns the reading end.
pub fn spawn_pipe<R: Read + Send + 'static>(body: R, abort: Arc<AtomicBool>) -> PipeReader {
    spawn_pipe_with_timeout(body, abort, DEFAULT_STALL_TIMEOUT)
}

/// How long a connection may deliver nothing before the stream is declared stalled.
pub const DEFAULT_STALL_TIMEOUT: Duration = Duration::from_secs(20);

/// [`spawn_pipe`] with an explicit stall timeout.
pub fn spawn_pipe_with_timeout<R: Read + Send + 'static>(
    mut body: R,
    abort: Arc<AtomicBool>,
    stall_timeout: Duration,
) -> PipeReader {
    // 256 x 8 KiB bounds the read-ahead at 2 MiB; a full channel blocks the download thread,
    // which is what throttles a finite file to playback speed.
    let (tx, rx) = sync_channel::<Vec<u8>>(256);
    let thread_abort = abort.clone();
    std::thread::Builder::new()
        .name("playr-net".into())
        .spawn(move || {
            let mut buf = vec![0u8; 8192];
            while !thread_abort.load(Ordering::Relaxed) {
                match body.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        })
        .expect("spawn network thread");
    PipeReader::new(rx, abort, stall_timeout)
}

/// Adapts any forward-only reader to symphonia's source trait.
pub struct StreamSource<R>(pub R);

impl<R: Read> Read for StreamSource<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl<R> Seek for StreamSource<R> {
    fn seek(&mut self, _: SeekFrom) -> io::Result<u64> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "stream is not seekable"))
    }
}

impl<R: Read + Send + Sync> MediaSource for StreamSource<R> {
    fn is_seekable(&self) -> bool {
        false
    }

    fn byte_len(&self) -> Option<u64> {
        None
    }
}

pub struct HttpStream {
    pub source: Box<dyn MediaSource>,
    pub ext: Option<String>,
    pub mime: Option<String>,
    /// Station name from the `icy-name` header.
    pub name: Option<String>,
    /// The address that was finally opened, after playlist indirection.
    pub url: String,
    /// A download of known length (a file) rather than an endless broadcast.
    pub finite: bool,
    pub secure: bool,
    /// The text of an HLS playlist found at `url`: the stream is to be opened with
    /// [`crate::hls::open`], and `source` holds nothing.
    pub hls: Option<String>,
}

/// Connects to `url`, following redirects and up to three levels of playlist indirection.
/// Dropping the stream does not stop the download thread; setting `abort` does.
pub fn open_url(url: &str, abort: Arc<AtomicBool>, on_title: TitleCallback) -> Result<HttpStream, String> {
    open_url_with_timeout(url, abort, on_title, DEFAULT_STALL_TIMEOUT)
}

/// [`open_url`] with an explicit stall timeout for the stream body.
pub fn open_url_with_timeout(
    url: &str,
    abort: Arc<AtomicBool>,
    on_title: TitleCallback,
    stall_timeout: Duration,
) -> Result<HttpStream, String> {
    // rustls is built without a bundled crypto backend; install the pure-Rust one once.
    static TLS: std::sync::Once = std::sync::Once::new();
    TLS.call_once(|| {
        let _ = rustls_rustcrypto::provider().install_default();
    });
    let agent = crate::dash::agent_with;

    let mut url = url.trim().to_string();
    let mut every_hop_secure = true;
    for _ in 0..4 {
        let resp = crate::privacy::fetch(&url, |a| {
            agent(a.https_only)
                .get(&a.url)
                .header("Icy-MetaData", "1")
                .call()
                .map_err(|e| format!("cannot open {}: {e}", a.url))
        })?;
        every_hop_secure &= !crate::privacy::is_unencrypted(&ureq::ResponseExt::get_uri(&resp).to_string());
        let secure = every_hop_secure;
        let header = |name: &str| {
            resp.headers().get(name).and_then(|v| v.to_str().ok()).map(|v| v.trim().to_string())
        };
        let mime = header("content-type")
            .map(|v| v.split(';').next().unwrap_or("").trim().to_ascii_lowercase())
            .filter(|v| !v.is_empty());
        let metaint = header("icy-metaint").and_then(|v| v.parse::<usize>().ok()).filter(|&n| n > 0);
        let name = header("icy-name").filter(|v| !v.is_empty());
        let finite = metaint.is_none() && header("content-length").is_some_and(|v| v.parse::<u64>().is_ok());

        let landed = ureq::ResponseExt::get_uri(&resp).to_string();
        if mime.as_deref().is_some_and(is_playlist_mime) || is_playlist_name(&url) {
            let mut text = String::new();
            // HLS recordings list every segment: a two-hour film runs to hundreds of kilobytes.
            let mut limited = resp.into_body().into_reader().take(8 << 20);
            let mut raw = Vec::new();
            limited.read_to_end(&mut raw).map_err(|e| format!("cannot read playlist: {e}"))?;
            text.push_str(&String::from_utf8_lossy(&raw));
            if crate::hls::is_hls(&text) {
                // Segment addresses are relative to where the playlist was finally found.
                let url = landed;
                let source: Box<dyn MediaSource> = Box::new(StreamSource(io::empty()));
                return Ok(HttpStream { source, ext: None, mime, name, url, finite: false, secure, hls: Some(text) });
            }
            url = parse_playlist(&text)?.ok_or("playlist contains no stream address")?;
            continue;
        }

        let ext = mime
            .as_deref()
            .and_then(mime_to_ext)
            .map(str::to_string)
            .or_else(|| {
                let path = url.split(['?', '#']).next().unwrap_or(&url);
                let last = path.rsplit('/').next().unwrap_or("");
                last.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).filter(|e| e.len() <= 5)
            });
        let pipe = spawn_pipe_with_timeout(resp.into_body().into_reader(), abort, stall_timeout);
        let source: Box<dyn MediaSource> = match metaint {
            Some(n) => Box::new(StreamSource(IcyReader::new(pipe, n, on_title))),
            None => Box::new(StreamSource(pipe)),
        };
        return Ok(HttpStream { source, ext, mime, name, url, finite, secure, hls: None });
    }
    Err("too many playlist redirections".into())
}
