//! HLS (HTTP Live Streaming): a playlist (`.m3u8`) of short media segments, fetched one after
//! another. A master playlist lists the same programme at several qualities (variants), each
//! with a media playlist of its own; a media playlist that never ends is a live broadcast,
//! reloaded for the segments added since.
//!
//! Segments are MPEG-TS (demuxed by [`crate::ts`]), fragmented MP4 after an `EXT-X-MAP`
//! header (read by symphonia, one segment at a time) or bare AAC/MP3 ("packed audio").
//! AES-128 encrypted segments are decrypted; SAMPLE-AES and other DRM is refused.

use std::collections::VecDeque;
use std::io::{Cursor, Read};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use symphonia::core::codecs::audio::AudioCodecParameters;
use symphonia::core::codecs::audio::well_known::{CODEC_ID_AAC, CODEC_ID_MP2, CODEC_ID_MP3};
use symphonia::core::units::{Duration as Frames, Timestamp};

use crate::avi::{MpaFramer, mpa_frame_info};
use crate::dash::{MergedDemuxer, agent_with};
use crate::demux::{Demuxer, SymDemuxer};
use crate::media::{AudioPacket, AudioTrack, MediaInfo, Packet, VideoCodec, VideoTrack};
use crate::ts::{self, AdtsFramer, StreamKind, TsDemux};

/// The tallest picture picked from a master playlist: decoding more is too slow in software.
pub const MAX_HEIGHT: u32 = 1080;
/// A live stream is joined this many segments before its newest one, as players commonly do,
/// so that a late segment does not stop playback at once.
pub const LIVE_EDGE_SEGMENTS: usize = 3;
const PLAYLIST_LIMIT: u64 = 8 << 20;
const SEGMENT_LIMIT: u64 = 64 << 20;
const KEY_LIMIT: u64 = 1024;
/// Segments fetched ahead of the one being played.
const PREFETCH: usize = 2;
/// Recently fetched segments kept, so that seeking back to one (which the player does right
/// after scanning for a keyframe) does not download it again; at most this many bytes of them.
const CACHED_SEGMENTS: usize = 4;
const CACHE_BYTES: usize = 48 << 20;
/// A segment body that delivers nothing for this long is given up (and retried).
const BODY_STALL: Duration = Duration::from_secs(30);
const ATTEMPTS: u32 = 3;
/// Live segments that may fail to download in a row before the stream is given up.
const LIVE_MISSES: u32 = 3;
/// Packets that jump this far from where the playlist puts their segment do not continue
/// the previous segment's clock (a discontinuity, a timestamp reset); they are re-anchored.
const REANCHOR_SECS: f64 = 2.0;
/// Why a playlist was not opened when what it holds was all left out (a picture playlist
/// whose segments carry only sound, opened for its picture).
const NOTHING_PLAYABLE: &str = "the HLS stream holds nothing playr can play";
/// How far apart the clocks of a separate picture and sound playlist may start and still be
/// taken as one clock.
const SHARED_CLOCK_SLACK: f64 = 30.0;

/// Where a playlist, key or segment lives: on the web or, for a playlist opened from disk,
/// in a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Location {
    Url(String),
    File(PathBuf),
}

impl Location {
    /// `uri` as written in the playlist at `self`. A playlist from the web may only point at
    /// web addresses: it must not make playr open files on this computer or a network share.
    pub fn resolve(&self, uri: &str) -> Option<Location> {
        let uri = uri.trim();
        if uri.is_empty() {
            return None;
        }
        if crate::net::is_url(uri) {
            return Some(Location::Url(uri.to_string()));
        }
        let has_scheme = uri.split_once(':').is_some_and(|(s, _)| s.len() > 1 && s.bytes().all(|b| b.is_ascii_alphabetic()));
        match self {
            Location::Url(base) => {
                if has_scheme {
                    return None;
                }
                crate::lists::join_url(base, uri).filter(|u| crate::net::is_url(u)).map(Location::Url)
            }
            Location::File(path) => {
                if has_scheme && !std::path::Path::new(uri).is_absolute() {
                    return None;
                }
                let dir = path.parent().unwrap_or(std::path::Path::new(""));
                Some(Location::File(dir.join(uri.split(['?', '#']).next().unwrap_or(uri))))
            }
        }
    }

    fn describe(&self) -> String {
        match self {
            Location::Url(u) => u.split('?').next().unwrap_or(u).chars().take(80).collect(),
            Location::File(p) => p.display().to_string(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Variant {
    pub uri: Location,
    pub bandwidth: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub codecs: Option<String>,
    /// The `GROUP-ID` of the audio renditions that go with this variant.
    pub audio: Option<String>,
}

/// An alternative audio track (`EXT-X-MEDIA:TYPE=AUDIO`).
#[derive(Clone, Debug, PartialEq)]
pub struct Rendition {
    pub group: String,
    pub name: String,
    pub language: Option<String>,
    /// `None` when the sound travels inside the variant's own segments.
    pub uri: Option<Location>,
    pub default: bool,
    pub autoselect: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Master {
    pub variants: Vec<Variant>,
    pub audio: Vec<Rendition>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Key {
    /// `AES-128`, or another method that is refused when its segment is fetched.
    pub method: String,
    pub uri: Option<Location>,
    pub iv: Option<[u8; 16]>,
}

/// The header (`EXT-X-MAP`) fragmented MP4 segments are read after.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InitSection {
    pub uri: Location,
    /// (offset, length) within the file.
    pub range: Option<(u64, u64)>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Segment {
    pub uri: Location,
    pub duration: f64,
    /// Seconds from the start of the playlist (from where a live stream was joined).
    pub start: f64,
    pub sequence: u64,
    /// (offset, length) within the file, for `EXT-X-BYTERANGE`.
    pub range: Option<(u64, u64)>,
    pub key: Option<Arc<Key>>,
    pub init: Option<Arc<InitSection>>,
    /// The timestamps need not follow on from the previous segment's.
    pub discontinuity: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Media {
    pub target_duration: f64,
    pub first_sequence: u64,
    pub segments: Vec<Segment>,
    /// `EXT-X-ENDLIST`: nothing will be added (a recording, not a live broadcast).
    pub ended: bool,
}

impl Media {
    pub fn duration(&self) -> f64 {
        self.segments.iter().map(|s| s.duration).sum()
    }

    /// The segment holding `secs`: the last one starting at or before it.
    pub fn segment_at(&self, secs: f64) -> usize {
        self.segments.iter().rposition(|s| s.start <= secs + 1e-6).unwrap_or(0)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Playlist {
    Master(Master),
    Media(Media),
}

/// Whether a playlist text is HLS rather than a plain list of addresses.
pub fn is_hls(text: &str) -> bool {
    ["#EXT-X-TARGETDURATION", "#EXT-X-STREAM-INF", "#EXT-X-MEDIA-SEQUENCE", "#EXT-X-MAP", "#EXT-X-ENDLIST"]
        .iter()
        .any(|tag| text.contains(tag))
}

/// `KEY=value,KEY="quoted, value"` pairs of a tag.
pub fn attributes(list: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = list.trim();
    while !rest.is_empty() {
        let Some((key, after)) = rest.split_once('=') else { break };
        let key = key.trim().to_ascii_uppercase();
        let (value, next) = if let Some(quoted) = after.strip_prefix('"') {
            match quoted.split_once('"') {
                Some((v, tail)) => (v.to_string(), tail),
                None => (quoted.to_string(), ""),
            }
        } else {
            match after.split_once(',') {
                Some((v, tail)) => (v.trim().to_string(), tail),
                None => (after.trim().to_string(), ""),
            }
        };
        out.push((key, value));
        rest = next.trim_start_matches(|c: char| c == ',' || c.is_whitespace());
    }
    out
}

fn attr<'a>(attrs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// `<length>[@<offset>]`; without an offset the range follows on from `previous_end`.
fn byte_range(value: &str, previous_end: Option<u64>) -> Option<(u64, u64)> {
    let (len, offset) = match value.trim().split_once('@') {
        Some((l, o)) => (l.trim().parse().ok()?, o.trim().parse().ok()?),
        None => (value.trim().parse().ok()?, previous_end.unwrap_or(0)),
    };
    offset.checked_add(len)?;
    Some((offset, len))
}

fn hex_iv(value: &str) -> Option<[u8; 16]> {
    let hex = value.trim().strip_prefix("0x").or_else(|| value.trim().strip_prefix("0X"))?;
    if hex.is_empty() || hex.len() > 32 {
        return None;
    }
    let padded = format!("{hex:0>32}");
    let mut iv = [0u8; 16];
    for (i, byte) in iv.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&padded[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(iv)
}

/// Reads a master or media playlist found at `base`.
pub fn parse(text: &str, base: &Location) -> Result<Playlist, String> {
    let text = text.trim_start_matches('\u{feff}');
    if !text.trim_start().starts_with("#EXTM3U") && !is_hls(text) {
        return Err("not an HLS playlist".into());
    }
    if text.contains("#EXT-X-STREAM-INF") {
        return Ok(Playlist::Master(parse_master(text, base)));
    }
    Ok(Playlist::Media(parse_media(text, base)))
}

fn parse_master(text: &str, base: &Location) -> Master {
    let mut variants = Vec::new();
    let mut audio = Vec::new();
    let mut pending: Option<Vec<(String, String)>> = None;
    for line in text.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            pending = Some(attributes(rest));
        } else if let Some(rest) = line.strip_prefix("#EXT-X-MEDIA:") {
            let a = attributes(rest);
            if attr(&a, "TYPE") != Some("AUDIO") {
                continue;
            }
            let yes = |k: &str| attr(&a, k).is_some_and(|v| v.eq_ignore_ascii_case("YES"));
            audio.push(Rendition {
                group: attr(&a, "GROUP-ID").unwrap_or_default().to_string(),
                name: attr(&a, "NAME").unwrap_or_default().to_string(),
                language: attr(&a, "LANGUAGE").map(str::to_string),
                uri: attr(&a, "URI").and_then(|u| base.resolve(u)),
                default: yes("DEFAULT"),
                autoselect: yes("AUTOSELECT"),
            });
        } else if !line.is_empty() && !line.starts_with('#') {
            let Some(a) = pending.take() else { continue };
            let Some(uri) = base.resolve(line) else { continue };
            let (width, height) = attr(&a, "RESOLUTION")
                .and_then(|r| r.split_once(['x', 'X']))
                .and_then(|(w, h)| Some((w.trim().parse().ok()?, h.trim().parse().ok()?)))
                .map_or((None, None), |(w, h)| (Some(w), Some(h)));
            variants.push(Variant {
                uri,
                bandwidth: attr(&a, "BANDWIDTH").and_then(|b| b.parse().ok()).unwrap_or(0),
                width,
                height,
                codecs: attr(&a, "CODECS").map(str::to_string),
                audio: attr(&a, "AUDIO").map(str::to_string),
            });
        }
    }
    Master { variants, audio }
}

fn parse_media(text: &str, base: &Location) -> Media {
    let mut media = Media { target_duration: 0.0, first_sequence: 0, segments: Vec::new(), ended: false };
    let mut duration = None;
    let mut range = None;
    let mut discontinuity = false;
    let mut key: Option<Arc<Key>> = None;
    let mut init: Option<Arc<InitSection>> = None;
    let mut start = 0.0;
    let mut listed = 0u64;
    let mut last_end: Option<(Location, u64)> = None;
    for line in text.lines().map(str::trim) {
        if let Some(v) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
            media.target_duration = v.trim().parse().unwrap_or(0.0);
        } else if let Some(v) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            media.first_sequence = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("#EXTINF:") {
            duration = v.split(',').next().and_then(|d| d.trim().parse::<f64>().ok()).filter(|d| d.is_finite() && *d >= 0.0);
        } else if let Some(v) = line.strip_prefix("#EXT-X-BYTERANGE:") {
            range = Some(v.to_string());
        } else if line == "#EXT-X-DISCONTINUITY" {
            discontinuity = true;
        } else if line == "#EXT-X-ENDLIST" {
            media.ended = true;
        } else if let Some(v) = line.strip_prefix("#EXT-X-KEY:") {
            let a = attributes(v);
            let method = attr(&a, "METHOD").unwrap_or("NONE").to_ascii_uppercase();
            key = (method != "NONE").then(|| {
                Arc::new(Key {
                    method,
                    uri: attr(&a, "URI").and_then(|u| base.resolve(u)),
                    iv: attr(&a, "IV").and_then(hex_iv),
                })
            });
        } else if let Some(v) = line.strip_prefix("#EXT-X-MAP:") {
            let a = attributes(v);
            init = attr(&a, "URI").and_then(|u| base.resolve(u)).map(|uri| {
                let range = attr(&a, "BYTERANGE").and_then(|r| byte_range(r, None));
                Arc::new(InitSection { uri, range })
            });
        } else if !line.is_empty() && !line.starts_with('#') {
            // A segment left out still has its place in the numbering and on the timeline.
            let sequence = media.first_sequence + listed;
            listed += 1;
            let Some(uri) = base.resolve(line) else {
                start += duration.take().unwrap_or(media.target_duration);
                range = None;
                continue;
            };
            let previous_end = last_end.as_ref().filter(|(u, _)| *u == uri).map(|(_, e)| *e);
            let range = range.take().and_then(|r| byte_range(&r, previous_end));
            last_end = range.map(|(offset, len)| (uri.clone(), offset.saturating_add(len)));
            let d = duration.take().unwrap_or(media.target_duration);
            media.segments.push(Segment {
                uri,
                duration: d,
                start,
                sequence,
                range,
                key: key.clone(),
                init: init.clone(),
                discontinuity: std::mem::take(&mut discontinuity),
            });
            start += d;
        }
    }
    if media.target_duration <= 0.0 {
        media.target_duration = media.segments.iter().map(|s| s.duration).fold(0.0, f64::max).max(1.0);
    }
    media
}

/// Whether every codec a variant names can be played here. A variant that names none is
/// given the benefit of the doubt.
pub fn codecs_supported(codecs: Option<&str>) -> bool {
    let Some(codecs) = codecs else { return true };
    codecs.split(',').map(|c| c.trim().to_ascii_lowercase()).filter(|c| !c.is_empty()).all(|c| {
        let family = c.split('.').next().unwrap_or("");
        match family {
            "avc1" | "avc3" | "hvc1" | "hev1" => true,
            // mp4a.40.x is AAC, .69/.6b MPEG audio; .a5/.a6 are AC-3/E-AC-3.
            "mp4a" => !matches!(c.as_str(), "mp4a.a5" | "mp4a.a6"),
            // Captions and subtitles travel separately and do not stop the variant from playing.
            "wvtt" | "stpp" | "tx3g" => true,
            _ => false,
        }
    })
}

fn has_video(v: &Variant) -> bool {
    v.height.is_some()
        || v.codecs.as_deref().is_some_and(|c| {
            c.split(',').any(|c| matches!(c.trim().split('.').next(), Some("avc1" | "avc3" | "hvc1" | "hev1")))
        })
}

/// The variant to play: the highest bandwidth no taller than `max_height` (an unknown height
/// counts as fitting) among those whose codecs can be played, else the shortest such one.
/// With `audio_only`, the variant with the least bandwidth will do, its picture unused.
pub fn pick_variant(master: &Master, max_height: u32, audio_only: bool) -> Option<&Variant> {
    let playable: Vec<&Variant> = master.variants.iter().filter(|v| codecs_supported(v.codecs.as_deref())).collect();
    if audio_only {
        let sound_only = playable.iter().copied().filter(|v| !has_video(v)).max_by_key(|v| v.bandwidth);
        return sound_only.or_else(|| playable.iter().copied().min_by_key(|v| v.bandwidth));
    }
    playable
        .iter()
        .copied()
        .filter(|v| v.height.is_none_or(|h| h <= max_height))
        .max_by_key(|v| (has_video(v), v.bandwidth))
        .or_else(|| playable.iter().copied().min_by_key(|v| v.height.unwrap_or(0)))
}

/// The separate audio track that goes with `variant`, if its sound is not in its own segments.
pub fn audio_rendition<'a>(master: &'a Master, variant: &Variant) -> Option<&'a Rendition> {
    let group = variant.audio.as_deref()?;
    let in_group: Vec<&Rendition> = master.audio.iter().filter(|r| r.group == group).collect();
    let pick = in_group.iter().find(|r| r.default).or_else(|| in_group.iter().find(|r| r.autoselect)).or(in_group.first())?;
    pick.uri.as_ref().map(|_| *pick)
}

/// Fails reads once `stopped` says so, so that a download being abandoned ends at its next
/// chunk rather than when the whole segment has arrived.
struct Stoppable<'a, R> {
    inner: R,
    stopped: &'a dyn Fn() -> bool,
}

impl<R: Read> Read for Stoppable<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if (self.stopped)() {
            return Err(std::io::Error::other("cancelled"));
        }
        self.inner.read(buf)
    }
}

/// Downloads `loc` (or the byte range of it), at most `limit` bytes, until `stopped` says
/// otherwise. `unencrypted` is set when the bytes crossed the network in the clear. Also
/// returns where the bytes finally came from (after redirects): addresses in a playlist are
/// relative to that.
fn fetch_from(
    loc: &Location,
    headers: &[(String, String)],
    range: Option<(u64, u64)>,
    limit: u64,
    unencrypted: &AtomicBool,
    stopped: &dyn Fn() -> bool,
) -> Result<(Vec<u8>, Location), String> {
    let range = range.filter(|(offset, len)| offset.checked_add(*len).is_some());
    match loc {
        Location::File(path) => {
            let mut file = std::fs::File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
            let (offset, len) = range.unwrap_or((0, limit));
            std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(offset)).map_err(|e| e.to_string())?;
            let mut out = Vec::new();
            file.take(len.min(limit)).read_to_end(&mut out).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            Ok((out, loc.clone()))
        }
        Location::Url(url) => {
            let resp = crate::privacy::fetch(url, |a| {
                let mut req = agent_with(a.https_only).get(&a.url);
                for (name, value) in headers {
                    req = req.header(name.as_str(), value.as_str());
                }
                if let Some((offset, len)) = range {
                    req = req.header("Range", format!("bytes={offset}-{}", (offset + len).saturating_sub(1)));
                }
                req.config()
                    .timeout_recv_body(Some(BODY_STALL))
                    .build()
                    .call()
                    .map_err(|e| format!("cannot download {}: {e}", loc.describe()))
            })?;
            let landed = ureq::ResponseExt::get_uri(&resp).to_string();
            if crate::privacy::is_unencrypted(&landed) {
                unencrypted.store(true, Ordering::Relaxed);
            }
            let partial = resp.status().as_u16() == 206;
            let mut out = Vec::new();
            let reader = Stoppable { inner: resp.into_body().into_reader(), stopped };
            match range {
                // A server that ignores the range sends the whole file; cut the range out.
                Some((offset, len)) if !partial => {
                    let mut reader = reader.take((offset + len).min(limit));
                    std::io::copy(&mut (&mut reader).take(offset), &mut std::io::sink()).map_err(|e| e.to_string())?;
                    reader.read_to_end(&mut out)
                }
                _ => reader.take(limit).read_to_end(&mut out),
            }
            .map_err(|e| format!("cannot download {}: {e}", loc.describe()))?;
            Ok((out, Location::Url(landed)))
        }
    }
}

/// A playlist's text and the address it finally came from.
fn fetch_text(loc: &Location, headers: &[(String, String)], unencrypted: &AtomicBool) -> Result<(String, Location), String> {
    let (bytes, landed) = fetch_from(loc, headers, None, PLAYLIST_LIMIT, unencrypted, &|| false)?;
    Ok((String::from_utf8_lossy(&bytes).into_owned(), landed))
}

/// Undoes AES-128-CBC with PKCS#7 padding, the encryption `METHOD=AES-128` names.
pub fn decrypt_aes128(data: &[u8], key: &[u8; 16], iv: &[u8; 16]) -> Result<Vec<u8>, String> {
    use aes::cipher::{Array, BlockCipherDecrypt, KeyInit};
    if data.is_empty() || !data.len().is_multiple_of(16) {
        return Err("an encrypted segment is not a whole number of AES blocks".into());
    }
    let cipher = aes::Aes128::new(&Array::from(*key));
    let mut out = data.to_vec();
    let mut previous = *iv;
    for block in out.as_chunks_mut::<16>().0 {
        let mut this = [0u8; 16];
        this.copy_from_slice(block);
        let mut b = Array::from(this);
        cipher.decrypt_block(&mut b);
        for (o, (d, p)) in block.iter_mut().zip(b.iter().zip(previous)) {
            *o = d ^ p;
        }
        previous = this;
    }
    let pad = *out.last().unwrap_or(&0) as usize;
    if pad == 0 || pad > 16 || pad > out.len() || !out[out.len() - pad..].iter().all(|&b| b as usize == pad) {
        return Err("an encrypted segment did not decrypt (wrong key?)".into());
    }
    out.truncate(out.len() - pad);
    Ok(out)
}

/// A fetched segment, decrypted, with the header fragmented MP4 needs before it.
struct Fetched {
    segment: Segment,
    init: Option<Arc<[u8]>>,
    data: Arc<Vec<u8>>,
}

type Cache = Arc<Mutex<VecDeque<(u64, Arc<Vec<u8>>)>>>;

/// What the fetching thread works from.
#[derive(Clone)]
enum Source {
    /// A recording: its playlist does not change.
    Recorded(Arc<Media>),
    /// A broadcast: its playlist is reloaded from here.
    Live(Location),
}

struct Fetcher {
    headers: Vec<(String, String)>,
    abort: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    unencrypted: Arc<AtomicBool>,
    cache: Cache,
    tx: SyncSender<Result<Fetched, String>>,
    key: Option<(Location, [u8; 16])>,
    init: Option<(Arc<InitSection>, Arc<[u8]>)>,
}

impl Fetcher {
    fn stopped(&self) -> bool {
        self.abort.load(Ordering::Relaxed) || self.stop.load(Ordering::Relaxed)
    }

    /// Waits up to `d`, returning early (false) when told to stop.
    fn wait(&self, d: Duration) -> bool {
        let until = Instant::now() + d;
        while Instant::now() < until {
            if self.stopped() {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50).min(until - Instant::now()));
        }
        !self.stopped()
    }

    /// Downloads on a thread of its own, so that being stopped returns at once even when the
    /// HTTP client sits in a read (it hands back a slowly trickling small body only once it is
    /// all there); the abandoned download ends at its next read or at [`BODY_STALL`].
    fn fetch(&self, loc: &Location, range: Option<(u64, u64)>, limit: u64) -> Result<Vec<u8>, String> {
        self.fetch_located(loc, range, limit).map(|(bytes, _)| bytes)
    }

    /// [`fetch`](Self::fetch), also returning where the bytes finally came from.
    fn fetch_located(&self, loc: &Location, range: Option<(u64, u64)>, limit: u64) -> Result<(Vec<u8>, Location), String> {
        let (tx, rx) = sync_channel(1);
        let (loc, headers, unencrypted) = (loc.clone(), self.headers.clone(), self.unencrypted.clone());
        let (abort, stop) = (self.abort.clone(), self.stop.clone());
        std::thread::Builder::new()
            .name("playr-hls-get".into())
            .spawn(move || {
                let stopped = || abort.load(Ordering::Relaxed) || stop.load(Ordering::Relaxed);
                let _ = tx.send(fetch_from(&loc, &headers, range, limit, &unencrypted, &stopped));
            })
            .map_err(|e| format!("cannot start a download: {e}"))?;
        loop {
            if self.stopped() {
                return Err("cancelled".into());
            }
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(result) => return result,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Err("the download ended unexpectedly".into()),
            }
        }
    }

    fn with_retries<T>(&self, mut f: impl FnMut() -> Result<T, String>) -> Result<T, String> {
        let mut last = String::new();
        for attempt in 0..ATTEMPTS {
            if attempt > 0 && !self.wait(Duration::from_millis(500 * u64::from(attempt))) {
                return Err("cancelled".into());
            }
            match f() {
                Ok(v) => return Ok(v),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    fn segment(&mut self, segment: &Segment) -> Result<Fetched, String> {
        let init = match &segment.init {
            None => None,
            Some(section) => match &self.init {
                Some((cached, bytes)) if **cached == **section => Some(bytes.clone()),
                _ => {
                    let bytes: Arc<[u8]> = self
                        .with_retries(|| self.fetch(&section.uri, section.range, SEGMENT_LIMIT))?
                        .into();
                    self.init = Some((section.clone(), bytes.clone()));
                    Some(bytes)
                }
            },
        };
        let cached = self.cache.lock().unwrap().iter().find(|(s, _)| *s == segment.sequence).map(|(_, d)| d.clone());
        let data = match cached {
            Some(data) => data,
            None => {
                let raw = self.with_retries(|| self.fetch(&segment.uri, segment.range, SEGMENT_LIMIT))?;
                let data = Arc::new(match &segment.key {
                    None => raw,
                    Some(key) => self.decrypt(key, segment.sequence, &raw)?,
                });
                let mut cache = self.cache.lock().unwrap();
                cache.push_back((segment.sequence, data.clone()));
                while cache.len() > CACHED_SEGMENTS
                    || cache.len() > 1 && cache.iter().map(|(_, d)| d.len()).sum::<usize>() > CACHE_BYTES
                {
                    cache.pop_front();
                }
                data
            }
        };
        Ok(Fetched { segment: segment.clone(), init, data })
    }

    fn decrypt(&mut self, key: &Key, sequence: u64, raw: &[u8]) -> Result<Vec<u8>, String> {
        if key.method != "AES-128" {
            return Err(format!("this stream is protected with {} (DRM), which playr cannot play", key.method));
        }
        let uri = key.uri.as_ref().ok_or("an encrypted segment names no key")?;
        let secret = match &self.key {
            Some((at, secret)) if at == uri => *secret,
            _ => {
                let bytes = self.with_retries(|| self.fetch(uri, None, KEY_LIMIT))?;
                let secret: [u8; 16] = bytes.as_slice().try_into().map_err(|_| "the stream's decryption key is not 16 bytes")?;
                self.key = Some((uri.clone(), secret));
                secret
            }
        };
        let iv = key.iv.unwrap_or_else(|| {
            let mut iv = [0u8; 16];
            iv[8..].copy_from_slice(&sequence.to_be_bytes());
            iv
        });
        decrypt_aes128(raw, &secret, &iv)
    }

    fn send(&self, item: Result<Fetched, String>) -> bool {
        !self.stopped() && self.tx.send(item).is_ok()
    }

    fn run_recorded(mut self, media: Arc<Media>, from: usize) {
        for segment in media.segments.iter().skip(from) {
            if self.stopped() {
                return;
            }
            let item = self.segment(segment);
            let failed = item.is_err();
            if !self.send(item) || failed {
                return;
            }
        }
    }

    fn run_live(mut self, location: Location, mut media: Media) {
        let mut next = media.first_sequence + media.segments.len().saturating_sub(LIVE_EDGE_SEGMENTS) as u64;
        let mut clock = 0.0;
        let mut misses = 0;
        let mut last_new = Instant::now();
        loop {
            if next < media.first_sequence {
                next = media.first_sequence; // fell behind what the server still lists
            }
            let mut added = false;
            let from = next;
            for segment in media.segments.iter().filter(|s| s.sequence >= from) {
                if self.stopped() {
                    return;
                }
                next = segment.sequence + 1;
                added = true;
                match self.segment(segment) {
                    Ok(mut fetched) => {
                        misses = 0;
                        fetched.segment.start = clock;
                        clock += segment.duration;
                        if !self.send(Ok(fetched)) {
                            return;
                        }
                    }
                    // A segment that is gone is skipped: live playback carries on with the next,
                    // unless several in a row are, which is a stream that cannot be fetched.
                    Err(e) => {
                        misses += 1;
                        if misses >= LIVE_MISSES {
                            self.send(Err(e));
                            return;
                        }
                    }
                }
            }
            if media.ended {
                return;
            }
            if added {
                last_new = Instant::now();
            } else if last_new.elapsed() > Duration::from_secs_f64(stopped_updating_after(media.target_duration)) {
                self.send(Err("the live stream stopped updating".into()));
                return;
            }
            let pause = if added { media.target_duration } else { media.target_duration / 2.0 };
            if !self.wait(Duration::from_secs_f64(pause.clamp(0.5, 10.0))) {
                return;
            }
            let reloaded = self.fetch_located(&location, None, PLAYLIST_LIMIT);
            if let Ok(Playlist::Media(fresh)) = reloaded.and_then(|(bytes, at)| parse(&String::from_utf8_lossy(&bytes), &at)) {
                media = fresh;
            }
        }
    }
}

/// Turns fetched segments into packets, keeping their timestamps on the playlist's timeline.
#[derive(Default)]
struct SegmentReader {
    ts: TsDemux,
    adts: AdtsFramer,
    mpa: MpaFramer,
    /// Added to a segment's own timestamps to place it on the playlist's timeline.
    offset: Option<f64>,
    /// Where the previous segment's packets ended on that timeline: the next segment
    /// continues from there unless its timestamps jump.
    last_end: Option<f64>,
    /// The offset the first segment of a stream played together with this one (picture and
    /// sound from separate playlists) was given; both share one clock.
    shared: Arc<Mutex<Option<f64>>>,
    video: Option<VideoTrack>,
    audio: Option<AudioTrack>,
    audio_rate: u32,
    notes: Vec<String>,
    keep_video: bool,
    keep_audio: bool,
}

fn shift(packet: Packet, by: f64) -> Packet {
    match packet {
        Packet::Video { pts, data } => Packet::Video { pts: pts + by, data },
        Packet::Audio { pts, packet, skip, limit } => Packet::Audio { pts: pts + by, packet, skip, limit },
    }
}

fn audio_packet(pts: f64, rate: u32, frames: u64, data: Vec<u8>) -> Packet {
    let ts = Timestamp::new((pts * f64::from(rate.max(1))).round() as i64);
    Packet::Audio { pts, packet: AudioPacket::new(0, ts, Frames::new(frames), data), skip: 0, limit: None }
}

/// The 90 kHz timestamp in an ID3 tag's `com.apple.streaming.transportStreamTimestamp` frame,
/// and the length of the tag. Packed audio segments start with one.
pub fn id3_timestamp(data: &[u8]) -> (Option<u64>, usize) {
    if data.len() < 10 || &data[..3] != b"ID3" {
        return (None, 0);
    }
    let size = data[6..10].iter().fold(0usize, |acc, &b| acc << 7 | usize::from(b & 0x7F));
    let end = (10 + size).min(data.len());
    let owner = b"com.apple.streaming.transportStreamTimestamp\0";
    let ts = data[10..end]
        .windows(owner.len())
        .position(|w| w == owner)
        .and_then(|at| data.get(10 + at + owner.len()..10 + at + owner.len() + 8))
        .map(|b| u64::from_be_bytes(b.try_into().expect("8 bytes")) & (ts::WRAP - 1));
    (ts, end)
}

fn mpa_params(first: [u8; 4]) -> Option<(AudioCodecParameters, u32)> {
    let (_, _, rate) = mpa_frame_info(first)?;
    let mut p = AudioCodecParameters::new();
    p.codec = if (first[1] >> 1) & 3 == 1 { CODEC_ID_MP3 } else { CODEC_ID_MP2 };
    p.sample_rate = Some(rate);
    Some((p, rate))
}

impl SegmentReader {
    fn read(&mut self, f: &Fetched) -> Result<Vec<Packet>, String> {
        let data = f.data.as_slice();
        let mp4 = f.init.is_some() || is_mp4(data);
        let mut packets = if mp4 {
            self.read_mp4(f.init.as_deref(), data)?
        } else if data.first() == Some(&0x47) || data.get(188) == Some(&0x47) {
            self.read_ts(data)
        } else {
            self.read_packed_audio(data, f.segment.start)
        };
        packets.retain(|p| match p {
            Packet::Video { .. } => self.keep_video,
            Packet::Audio { .. } => self.keep_audio,
        });
        // A segment starts with its first picture; the sound may begin a little before it.
        let pictures = packets.iter().filter(|p| matches!(p, Packet::Video { .. })).map(Packet::pts).reduce(f64::min);
        let Some(first) = pictures.or_else(|| packets.iter().map(Packet::pts).reduce(f64::min)) else { return Ok(packets) };
        let continues = match (self.offset, self.last_end) {
            (Some(o), Some(end)) => (first + o - end).abs() <= REANCHOR_SECS,
            _ => false,
        };
        if f.segment.discontinuity || !continues {
            let own = f.segment.start - first;
            let mut shared = self.shared.lock().unwrap();
            // The first segment joins the other half of the stream on its clock, unless that is
            // far off (the two playlists then do not share timestamps).
            let offset = match *shared {
                Some(o) if self.offset.is_none() && (o - own).abs() <= SHARED_CLOCK_SLACK => o,
                _ => own,
            };
            if shared.is_none() {
                *shared = Some(offset);
            }
            self.offset = Some(offset);
        }
        let by = self.offset.unwrap_or(0.0);
        let packets: Vec<Packet> = packets.into_iter().map(|p| shift(p, by)).collect();
        // The end is one packet past the last start; a packet lasts about the gap between two.
        let mut starts: Vec<f64> = packets.iter().map(Packet::pts).collect();
        starts.sort_by(f64::total_cmp);
        let step = starts.windows(2).map(|w| w[1] - w[0]).filter(|d| *d > 0.0).fold(f64::INFINITY, f64::min);
        self.last_end = starts.last().map(|l| l + if step.is_finite() { step } else { 0.0 });
        Ok(packets)
    }

    fn read_mp4(&mut self, init: Option<&[u8]>, data: &[u8]) -> Result<Vec<Packet>, String> {
        let mut bytes = Vec::with_capacity(init.map_or(0, <[u8]>::len) + data.len());
        bytes.extend_from_slice(init.unwrap_or(&[]));
        bytes.extend_from_slice(data);
        let starts = fragment_starts(&bytes);
        let mut reader = SymDemuxer::open(Box::new(Cursor::new(bytes)), Some("mp4"), None)?;
        let info = reader.info().clone();
        if self.video.is_none() && self.audio.is_none() {
            self.video = info.video.clone();
            self.audio = info.audio.clone();
            self.notes.extend(info.notes.iter().cloned());
        }
        // symphonia reads each segment on its own and counts its time from zero; the decode
        // time its fragment header records puts it back on the stream's clock.
        let mut out = Vec::new();
        while let Some(p) = reader.next_packet()? {
            let start = match p {
                Packet::Video { .. } => starts.video,
                Packet::Audio { .. } => starts.audio,
            };
            out.push(shift(p, start.unwrap_or(0.0)));
        }
        Ok(out)
    }

    fn read_ts(&mut self, data: &[u8]) -> Vec<Packet> {
        let mut pes = self.ts.push(data);
        pes.extend(self.ts.flush());
        if self.video.is_none() && self.audio.is_none() {
            if let Some((_, kind)) = self.ts.video {
                let codec = if kind == StreamKind::H264 { VideoCodec::H264 } else { VideoCodec::Hevc };
                self.video = Some(VideoTrack { codec, width: 0, height: 0, extra: Vec::new() });
            }
            for t in &self.ts.other_types {
                if let Some(name) = ts::unsupported_name(*t) {
                    self.notes.push(format!("{name} in this stream cannot be played"));
                }
            }
        }
        // A wrap of the 33-bit clock inside the segment is undone against its first timestamp.
        let first = pes.iter().find_map(|p| p.pts);
        let secs = |t: u64| {
            let t = match first {
                Some(f) if t + ts::WRAP / 2 < f => t + ts::WRAP,
                _ => t,
            };
            t as f64 / ts::CLOCK
        };
        if let Some(track) = self.video.as_mut().filter(|t| t.width == 0)
            && let Some((w, h)) = pes.iter().filter(|p| p.kind.is_video()).find_map(|p| picture_size(p.kind, &p.data))
        {
            track.width = w;
            track.height = h;
        }
        let mut out = Vec::new();
        self.adts.reset();
        self.mpa.reset();
        for p in pes {
            let pts = p.pts.map(secs);
            match p.kind {
                StreamKind::H264 | StreamKind::Hevc => {
                    // Muxers need not start a PES on a picture: what comes before the first
                    // start code ends the previous picture, and the PTS belongs to the picture
                    // that starts in this PES.
                    // A PES without a PTS carries more of the picture before it (a large picture
                    // split over several PES packets).
                    let lead = if pts.is_some() { start_code_at(&p.data).unwrap_or(p.data.len()) } else { p.data.len() };
                    if lead > 0
                        && let Some(Packet::Video { data, .. }) = out.iter_mut().rev().find(|q| matches!(q, Packet::Video { .. }))
                    {
                        let mut joined = std::mem::take(data).into_vec();
                        joined.extend_from_slice(&p.data[..lead]);
                        *data = joined.into_boxed_slice();
                    }
                    if let Some(pts) = pts
                        && lead < p.data.len()
                    {
                        out.push(Packet::Video { pts, data: p.data[lead..].to_vec().into_boxed_slice() });
                    }
                }
                StreamKind::Aac => {
                    self.adts.push(&p.data, pts);
                    self.aac_frames(&mut out);
                }
                StreamKind::Mpa => {
                    self.mpa.push(&p.data, pts.unwrap_or(0.0));
                    self.mpa_frames(&mut out, &p.data);
                }
            }
        }
        out
    }

    fn read_packed_audio(&mut self, data: &[u8], start: f64) -> Vec<Packet> {
        let (stamp, skip) = id3_timestamp(data);
        let body = &data[skip..];
        let pts = stamp.map_or(start, |t| t as f64 / ts::CLOCK);
        let mut out = Vec::new();
        self.adts.reset();
        self.mpa.reset();
        if ts::adts_header(body).is_some() || body.windows(2).take(4096).any(|w| w[0] == 0xFF && w[1] & 0xF6 == 0xF0) {
            self.adts.push(body, Some(pts));
            self.aac_frames(&mut out);
        }
        if out.is_empty() {
            self.mpa.push(body, pts);
            self.mpa_frames(&mut out, body);
        }
        out
    }

    fn aac_frames(&mut self, out: &mut Vec<Packet>) {
        while let Some((pts, raw, config)) = self.adts.next_frame() {
            if self.audio.is_none() {
                let mut p = AudioCodecParameters::new();
                p.codec = CODEC_ID_AAC;
                p.sample_rate = Some(config.rate);
                p.extra_data = Some(config.audio_specific_config().to_vec().into_boxed_slice());
                self.audio = Some(AudioTrack::Symphonia(p));
                self.audio_rate = config.rate;
            }
            out.push(audio_packet(pts, config.rate, 1024, raw));
        }
    }

    fn mpa_frames(&mut self, out: &mut Vec<Packet>, sample: &[u8]) {
        if self.audio.is_none()
            && let Some((params, rate)) = sample.windows(4).find_map(|w| mpa_params([w[0], w[1], w[2], w[3]]))
        {
            self.audio = Some(AudioTrack::Symphonia(params));
            self.audio_rate = rate;
        }
        while let Some((pts, frame, samples)) = self.mpa.next_frame() {
            out.push(audio_packet(pts, self.audio_rate, u64::from(samples), frame));
        }
    }
}

#[derive(Default)]
struct FragmentStarts {
    video: Option<f64>,
    audio: Option<f64>,
}

/// The decode time (`moof/traf/tfdt`) of the first fragment of each track in fragmented MP4
/// bytes, in seconds of that track's timescale (`moov/trak/mdia/mdhd`).
fn fragment_starts(bytes: &[u8]) -> FragmentStarts {
    use crate::demux::{be32, be64, boxes, child};
    // (track id, timescale, is video)
    let mut tracks = Vec::new();
    for (_, moov) in boxes(bytes).filter(|(t, _)| t == b"moov") {
        for (_, trak) in boxes(moov).filter(|(t, _)| t == b"trak") {
            let Some(tkhd) = child(trak, b"tkhd") else { continue };
            let id = if tkhd.first() == Some(&1) { be32(tkhd, 20) } else { be32(tkhd, 12) };
            let Some(mdia) = child(trak, b"mdia") else { continue };
            let Some(mdhd) = child(mdia, b"mdhd") else { continue };
            let scale = if mdhd.first() == Some(&1) { be32(mdhd, 20) } else { be32(mdhd, 12) };
            let kind = child(mdia, b"hdlr").and_then(|h| h.get(8..12));
            if let (Some(id), Some(scale)) = (id, scale.filter(|s| *s > 0)) {
                tracks.push((id, scale, kind == Some(b"vide".as_slice())));
            }
        }
    }
    let mut out = FragmentStarts::default();
    for (_, moof) in boxes(bytes).filter(|(t, _)| t == b"moof") {
        for (_, traf) in boxes(moof).filter(|(t, _)| t == b"traf") {
            let (Some(id), Some(tfdt)) = (child(traf, b"tfhd").and_then(|h| be32(h, 4)), child(traf, b"tfdt")) else {
                continue;
            };
            let base = if tfdt.first() == Some(&1) { be64(tfdt, 4) } else { be32(tfdt, 4).map(u64::from) };
            let (Some(base), Some(&(_, scale, video))) = (base, tracks.iter().find(|t| t.0 == id)) else { continue };
            let slot = if video { &mut out.video } else { &mut out.audio };
            slot.get_or_insert(base as f64 / f64::from(scale));
        }
    }
    out
}

/// The picture size a sequence parameter set in this Annex B data gives.
fn picture_size(kind: StreamKind, data: &[u8]) -> Option<(u32, u32)> {
    match kind {
        StreamKind::H264 => rust_h264::nal::parse_annex_b(data)
            .iter()
            .filter(|n| n.nal_unit_type == rust_h264::nal::NalUnitType::Sps)
            .find_map(|n| rust_h264::nal::parse_sps_info(n).ok())
            .map(|i| (i.width, i.height)),
        StreamKind::Hevc => rust_h265::nal::parse_annex_b(data)
            .iter()
            .filter(|n| n.nal_unit_type == rust_h265::nal::NalUnitType::Sps)
            .find_map(|n| hevc_sps_size(&n.rbsp)),
        _ => None,
    }
    .filter(|&(w, h)| w > 0 && h > 0)
}

/// Width and height from an HEVC sequence parameter set (its payload after the NAL header),
/// with the conformance window cropped off (ITU-T H.265 7.3.2.2).
pub fn hevc_sps_size(rbsp: &[u8]) -> Option<(u32, u32)> {
    let mut r = Bits { data: rbsp, at: 0 };
    r.skip(4); // sps_video_parameter_set_id
    let sub_layers = r.bits(3)? as usize;
    r.skip(1);
    // profile_tier_level: the general part, then per sub-layer presence flags and parts.
    r.skip(88);
    r.skip(8); // general_level_idc
    let mut present = Vec::with_capacity(sub_layers);
    for _ in 0..sub_layers {
        present.push((r.bits(1)? == 1, r.bits(1)? == 1));
    }
    if sub_layers > 0 {
        r.skip(2 * (8 - sub_layers));
    }
    for (profile, level) in present {
        if profile {
            r.skip(88);
        }
        if level {
            r.skip(8);
        }
    }
    r.ue()?; // sps_seq_parameter_set_id
    let chroma = r.ue()?;
    if chroma == 3 {
        r.skip(1);
    }
    let (mut w, mut h) = (r.ue()?, r.ue()?);
    if r.bits(1)? == 1 {
        let (sub_w, sub_h) = match chroma {
            1 => (2, 2),
            2 => (2, 1),
            _ => (1, 1),
        };
        let (left, right, top, bottom) = (r.ue()?, r.ue()?, r.ue()?, r.ue()?);
        w = w.checked_sub(left.checked_add(right)?.checked_mul(sub_w)?)?;
        h = h.checked_sub(top.checked_add(bottom)?.checked_mul(sub_h)?)?;
    }
    Some((w, h))
}

struct Bits<'a> {
    data: &'a [u8],
    at: usize,
}

impl Bits<'_> {
    fn skip(&mut self, n: usize) {
        self.at += n;
    }

    fn bits(&mut self, n: usize) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = *self.data.get(self.at / 8)?;
            v = v << 1 | u32::from(byte >> (7 - self.at % 8) & 1);
            self.at += 1;
        }
        Some(v)
    }

    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bits(1)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1u32 << zeros) - 1 + self.bits(zeros)?)
    }
}

/// Seconds a live playlist may go without a new segment before it is given up.
fn stopped_updating_after(target_duration: f64) -> f64 {
    (target_duration * 6.0).max(30.0)
}

/// Where the first Annex B start code (`00 00 01`, with a leading zero if there is one) begins.
fn start_code_at(data: &[u8]) -> Option<usize> {
    let at = data.windows(3).position(|w| w == [0, 0, 1])?;
    Some(if at > 0 && data[at - 1] == 0 { at - 1 } else { at })
}

fn is_mp4(data: &[u8]) -> bool {
    matches!(data.get(4..8), Some(b"ftyp" | b"styp" | b"moof" | b"sidx" | b"moov"))
}

/// Plays one HLS media playlist.
pub struct HlsDemuxer {
    info: MediaInfo,
    source: Source,
    headers: Vec<(String, String)>,
    abort: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    rx: Receiver<Result<Fetched, String>>,
    queue: VecDeque<Packet>,
    reader: SegmentReader,
    cache: Cache,
    unencrypted: Arc<AtomicBool>,
    stall: Duration,
}

impl Drop for HlsDemuxer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl HlsDemuxer {
    /// Starts playing `media`, found at `location`, from its start (a recording) or near its
    /// newest segment (a broadcast). Picture or sound can be left out, for a stream whose
    /// other half comes from another playlist.
    pub fn open(
        location: Location,
        media: Media,
        headers: &[(String, String)],
        keep_video: bool,
        keep_audio: bool,
        shared: &Shared,
        abort: Arc<AtomicBool>,
    ) -> Result<Self, String> {
        if media.segments.is_empty() && media.ended {
            return Err("the HLS playlist lists no segments".into());
        }
        let live = !media.ended;
        // Longer than the fetcher's own "stopped updating" limit, so that a live stream that
        // stops growing is reported as such rather than as a stall.
        let stall = Duration::from_secs_f64(stopped_updating_after(media.target_duration) + 15.0);
        let mut info = MediaInfo {
            container: "HLS".into(),
            duration: (!live).then(|| media.duration()),
            seekable: !live,
            ..Default::default()
        };
        let source = if live { Source::Live(location) } else { Source::Recorded(Arc::new(media.clone())) };
        let cache: Cache = Default::default();
        let unencrypted = shared.unencrypted.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let rx = spawn(&source, Some(media), 0, headers, &abort, &stop, &cache, &unencrypted);
        let mut this = Self {
            info: MediaInfo::default(),
            source,
            headers: headers.to_vec(),
            abort,
            stop,
            rx,
            queue: VecDeque::new(),
            reader: SegmentReader { keep_video, keep_audio, shared: shared.clock.clone(), ..Default::default() },
            cache,
            unencrypted,
            stall,
        };
        // Segments are read until one shows what the stream holds.
        while this.reader.video.is_none() && this.reader.audio.is_none() && this.reader.notes.is_empty() {
            if !this.fill()? {
                return Err("the HLS stream ended before any picture or sound arrived".into());
            }
        }
        info.video = this.reader.video.clone().filter(|_| keep_video);
        info.audio = this.reader.audio.clone().filter(|_| keep_audio);
        info.notes = std::mem::take(&mut this.reader.notes);
        if info.video.is_none() && info.audio.is_none() {
            let why = if info.notes.is_empty() { NOTHING_PLAYABLE.into() } else { info.notes.join("; ") };
            return Err(why);
        }
        this.info = info;
        Ok(this)
    }

    pub fn set_size(&mut self, width: Option<u32>, height: Option<u32>) {
        if let (Some(track), Some(w), Some(h)) = (self.info.video.as_mut(), width, height)
            && track.width == 0
        {
            track.width = w;
            track.height = h;
        }
    }

    /// Reads the next segment into the queue; false at the end of the stream.
    fn fill(&mut self) -> Result<bool, String> {
        let started = Instant::now();
        loop {
            if self.abort.load(Ordering::Relaxed) {
                return Err("stream closed".into());
            }
            match self.rx.recv_timeout(Duration::from_millis(100)) {
                Ok(Ok(fetched)) => {
                    let packets = self.reader.read(&fetched)?;
                    self.queue.extend(packets);
                    return Ok(true);
                }
                Ok(Err(e)) => return Err(e),
                // An aborted fetcher also hangs up; that is not the end of the stream.
                Err(RecvTimeoutError::Disconnected) if self.abort.load(Ordering::Relaxed) => {
                    return Err("stream closed".into());
                }
                Err(RecvTimeoutError::Disconnected) => return Ok(false),
                Err(RecvTimeoutError::Timeout) if started.elapsed() >= self.stall => {
                    return Err("the HLS stream stalled".into());
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn(
    source: &Source,
    first_playlist: Option<Media>,
    from: usize,
    headers: &[(String, String)],
    abort: &Arc<AtomicBool>,
    stop: &Arc<AtomicBool>,
    cache: &Cache,
    unencrypted: &Arc<AtomicBool>,
) -> Receiver<Result<Fetched, String>> {
    let (tx, rx) = sync_channel(PREFETCH);
    let fetcher = Fetcher {
        headers: headers.to_vec(),
        abort: abort.clone(),
        stop: stop.clone(),
        unencrypted: unencrypted.clone(),
        cache: cache.clone(),
        tx,
        key: None,
        init: None,
    };
    let source = source.clone();
    std::thread::Builder::new()
        .name("playr-hls".into())
        .spawn(move || match source {
            Source::Recorded(media) => fetcher.run_recorded(media, from),
            Source::Live(location) => {
                let media = first_playlist.or_else(|| {
                    let (text, at) = fetch_text(&location, &fetcher.headers, &fetcher.unencrypted).ok()?;
                    match parse(&text, &at).ok()? {
                        Playlist::Media(m) => Some(m),
                        Playlist::Master(_) => None,
                    }
                });
                if let Some(media) = media {
                    fetcher.run_live(location, media);
                }
            }
        })
        .expect("spawn HLS thread");
    rx
}

impl Demuxer for HlsDemuxer {
    fn info(&self) -> &MediaInfo {
        &self.info
    }

    fn unencrypted(&self) -> bool {
        self.unencrypted.load(Ordering::Relaxed)
    }

    fn next_packet(&mut self) -> Result<Option<Packet>, String> {
        loop {
            if let Some(p) = self.queue.pop_front() {
                return Ok(Some(p));
            }
            if !self.fill()? {
                return Ok(None);
            }
        }
    }

    fn seek(&mut self, secs: f64) -> Result<f64, String> {
        let Source::Recorded(media) = &self.source else {
            return Err("a live stream cannot be sought in".into());
        };
        let media = media.clone();
        let index = media.segment_at(secs.max(0.0));
        self.stop.store(true, Ordering::Relaxed);
        self.stop = Arc::new(AtomicBool::new(false));
        self.rx = spawn(&self.source, None, index, &self.headers, &self.abort, &self.stop, &self.cache, &self.unencrypted);
        self.queue.clear();
        self.reader.offset = None;
        self.reader.last_end = None;
        // The landing segment is placed at its own start; the first side of a picture and sound
        // pair to read after the seek sets the shared clock again for the other.
        *self.reader.shared.lock().unwrap() = None;
        Ok(media.segments.get(index).map_or(0.0, |s| s.start))
    }
}

/// An opened HLS stream.
pub struct Opened {
    pub demuxer: Box<dyn Demuxer + Send>,
    /// Every playlist, key and segment fetched while opening came encrypted (or from this
    /// computer); later fetches are reported by [`Demuxer::unencrypted`].
    pub secure: bool,
    pub live: bool,
}

/// What the playlists played together (a picture and a sound playlist) share.
#[derive(Clone, Default)]
pub struct Shared {
    /// Set once anything came over plain HTTP.
    pub unencrypted: Arc<AtomicBool>,
    /// The offset that puts the first segment on the common timeline.
    clock: Arc<Mutex<Option<f64>>>,
}

/// Opens the HLS playlist at `location` (whose text is `text`, if already fetched): picks a
/// variant of a master playlist and joins a separate audio track to it.
pub fn open(
    location: &Location,
    text: Option<String>,
    headers: &[(String, String)],
    audio_only: bool,
    abort: Arc<AtomicBool>,
) -> Result<Opened, String> {
    let shared = Shared::default();
    let (demuxer, live) = open_with(location, text, headers, !audio_only, true, &shared, abort)?;
    let secure = !shared.unencrypted.load(Ordering::Relaxed);
    Ok(Opened { demuxer, secure, live })
}

fn open_with(
    location: &Location,
    text: Option<String>,
    headers: &[(String, String)],
    keep_video: bool,
    keep_audio: bool,
    shared: &Shared,
    abort: Arc<AtomicBool>,
) -> Result<(Box<dyn Demuxer + Send>, bool), String> {
    let (text, at) = match text {
        Some(t) => (t, location.clone()),
        None => fetch_text(location, headers, &shared.unencrypted)?,
    };
    let (variant, media_at, media, audio) = match parse(&text, &at)? {
        Playlist::Media(media) => (None, at, media, None),
        Playlist::Master(master) => {
            let variant = pick_variant(&master, MAX_HEIGHT, !keep_video).cloned().ok_or_else(|| {
                let codecs: Vec<&str> = master.variants.iter().filter_map(|v| v.codecs.as_deref()).collect();
                if master.variants.is_empty() {
                    "the HLS playlist lists no streams".to_string()
                } else {
                    format!("the HLS stream uses codecs playr cannot play ({})", codecs.join("; "))
                }
            })?;
            let rendition = audio_rendition(&master, &variant).and_then(|r| r.uri.clone()).filter(|_| keep_audio);
            let (media, media_at) = media_playlist(&variant.uri, headers, &shared.unencrypted)?;
            let audio = match rendition {
                Some(uri) => Some(media_playlist(&uri, headers, &shared.unencrypted)?),
                None => None,
            };
            (Some(variant), media_at, media, audio)
        }
    };
    let live = !media.ended;
    let size = |d: &mut HlsDemuxer| {
        if let Some(v) = &variant {
            d.set_size(v.width, v.height);
        }
    };
    let Some((audio_media, audio_at)) = audio else {
        let mut main = HlsDemuxer::open(media_at, media, headers, keep_video, keep_audio, shared, abort)?;
        size(&mut main);
        return Ok((Box::new(main), live));
    };
    let sound = HlsDemuxer::open(audio_at, audio_media, headers, false, true, shared, abort.clone())?;
    if !keep_video {
        return Ok((Box::new(sound), live));
    }
    let mut picture = match HlsDemuxer::open(media_at, media, headers, true, false, shared, abort) {
        Err(e) if e == NOTHING_PLAYABLE => return Ok((Box::new(sound), live)),
        other => other?,
    };
    size(&mut picture);
    if picture.info().video.is_none() {
        return Ok((Box::new(sound), live));
    }
    Ok((Box::new(MergedDemuxer::new(Box::new(picture), Box::new(sound))), live))
}

/// Opens a video-only and an audio-only HLS media playlist (two formats of a site) together.
/// Any sound in the picture playlist is left out, so only one sound track is decoded.
pub fn open_pair(
    video: &Location,
    audio: &Location,
    headers: &[(String, String)],
    abort: Arc<AtomicBool>,
) -> Result<Opened, String> {
    let shared = Shared::default();
    let (sound, sound_live) = open_with(audio, None, headers, false, true, &shared, abort.clone())?;
    let (picture, picture_live) = match open_with(video, None, headers, true, false, &shared, abort) {
        Err(e) if e == NOTHING_PLAYABLE => (None, false),
        other => other.map(|(d, live)| (Some(d), live))?,
    };
    let live = picture_live || sound_live;
    let secure = !shared.unencrypted.load(Ordering::Relaxed);
    match picture.filter(|p| p.info().video.is_some()) {
        Some(picture) => Ok(Opened { demuxer: Box::new(MergedDemuxer::new(picture, sound)), secure, live }),
        None => Ok(Opened { demuxer: sound, secure, live }),
    }
}

/// A media playlist and the address it came from.
fn media_playlist(at: &Location, headers: &[(String, String)], unencrypted: &AtomicBool) -> Result<(Media, Location), String> {
    let (text, landed) = fetch_text(at, headers, unencrypted)?;
    match parse(&text, &landed)? {
        Playlist::Media(media) => Ok((media, landed)),
        Playlist::Master(_) => Err("an HLS variant playlist lists further variants".into()),
    }
}
