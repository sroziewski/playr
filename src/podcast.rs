//! Podcasts behind Spotify show and episode links, played from their public RSS feed.
//!
//! Spotify's embed page for a show carries only its newest episode, and Spotify's own audio
//! is DRM-protected. Most podcasts on Spotify are also published as an open RSS feed whose
//! episodes are plain MP3/AAC files; Apple's public podcast directory (the iTunes Search API,
//! no sign-in) knows that feed by the show's name. Playing the feed gives every episode, at
//! its original quality, seekable, with no YouTube lookup.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use serde_json::Value;

use crate::dash::{self, HttpFetch, RangeSource};
use crate::demux::{self, Demuxer, SymDemuxer};
use crate::net;

/// The iTunes Search API; [`SEARCH_ENV`] points it elsewhere (a local server in tests).
pub const SEARCH_BASE: &str = "https://itunes.apple.com/search";
pub const SEARCH_ENV: &str = "PLAYR_PODCAST_SEARCH";
const FEED_LIMIT: u64 = 32 << 20;

#[derive(Clone, Debug, PartialEq)]
pub struct Episode {
    pub title: String,
    pub audio: String,
    pub duration: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Feed {
    pub name: String,
    /// In the feed's order, which is newest first for nearly every podcast.
    pub episodes: Vec<Episode>,
}

/// Lower case, letters and digits only, single spaces: titles compared this way survive the
/// punctuation and spacing that differ between Spotify and a feed. Combining accents are
/// dropped, but precomposed letters (é) are kept, so "é" and "e" still differ.
pub fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with(' ') && !is_combining(c) {
            out.push(' ');
        }
    }
    out.trim_end().to_string()
}

fn is_combining(c: char) -> bool {
    matches!(c as u32, 0x0300..=0x036F)
}

/// The feed address of the podcast named `show` in an iTunes Search API answer. Only an
/// exact name match counts: a near miss would play some other podcast.
pub fn feed_from_search(json: &str, show: &str) -> Option<String> {
    let data: Value = serde_json::from_str(json).ok()?;
    let wanted = normalize(show);
    data.get("results")?.as_array()?.iter().find_map(|r| {
        let name = r.get("collectionName")?.as_str()?;
        let feed = r.get("feedUrl")?.as_str()?;
        (normalize(name) == wanted && net::is_url(feed)).then(|| feed.to_string())
    })
}

fn query_escape(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Looks the podcast named `show` up in Apple's directory; `None` when it is not listed.
pub fn find_feed(show: &str) -> Result<Option<String>, String> {
    let base = std::env::var(SEARCH_ENV).ok().filter(|b| !b.is_empty()).unwrap_or_else(|| SEARCH_BASE.to_string());
    let url = format!("{base}?media=podcast&entity=podcast&limit=10&term={}", query_escape(show));
    Ok(feed_from_search(&dash::get_text(&url, &[], 4 << 20)?, show))
}

/// `862`, `14:22` or `00:14:22` in seconds.
pub fn parse_duration(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let mut secs = 0.0;
    for part in s.split(':') {
        secs = secs * 60.0 + part.trim().parse::<f64>().ok().filter(|v| v.is_finite() && *v >= 0.0)?;
    }
    (secs > 0.0).then_some(secs)
}

fn attribute(e: &BytesStart, name: &[u8]) -> Option<String> {
    e.try_get_attribute(name).ok()??.normalized_value(quick_xml::XmlVersion::default()).ok().map(|v| v.trim().to_string())
}

/// Reads the episodes out of an RSS feed; items without an audio enclosure are left out.
pub fn parse_rss(xml: &str) -> Result<Feed, String> {
    let mut reader = Reader::from_str(xml);
    let mut name = None;
    let mut episodes = Vec::new();
    let mut in_item = false;
    let mut title = None;
    let mut audio = None;
    let mut duration = None;
    let mut text: Option<String> = None;
    let mut path: Vec<Vec<u8>> = Vec::new();
    loop {
        let event = reader.read_event().map_err(|e| format!("the podcast feed is not valid XML ({e})"))?;
        match event {
            Event::Start(e) => {
                let tag = e.name().as_ref().to_vec();
                if tag == b"item" {
                    in_item = true;
                    (title, audio, duration) = (None, None, None);
                }
                if tag == b"enclosure" {
                    audio = audio.or_else(|| enclosure(&e));
                }
                text = Some(String::new());
                path.push(tag);
            }
            Event::Empty(e) => {
                if in_item && e.name().as_ref() == b"enclosure" {
                    audio = audio.or_else(|| enclosure(&e));
                }
            }
            Event::Text(t) => {
                if let Some(buf) = text.as_mut() {
                    buf.push_str(&t.decode().map_err(|e| e.to_string())?);
                }
            }
            Event::GeneralRef(r) => {
                if let Some(buf) = text.as_mut() {
                    buf.push('&');
                    buf.push_str(&r.decode().map_err(|e| e.to_string())?);
                    buf.push(';');
                }
            }
            Event::CData(c) => {
                if let Some(buf) = text.as_mut() {
                    let raw = c.decode().map_err(|e| e.to_string())?;
                    buf.push_str(&raw.replace('&', "&amp;").replace('<', "&lt;"));
                }
            }
            Event::End(e) => {
                let tag = e.name().as_ref().to_vec();
                let value = text.take().map(|t| {
                    let t = t.trim();
                    quick_xml::escape::unescape(t).map(|u| u.trim().to_string()).unwrap_or_else(|_| t.to_string())
                });
                let parent = path.len().checked_sub(2).map(|i| path[i].as_slice());
                match tag.as_slice() {
                    b"title" if in_item && parent == Some(b"item") => title = value.filter(|v| !v.is_empty()),
                    b"title" if !in_item && parent == Some(b"channel") && name.is_none() => {
                        name = value.filter(|v| !v.is_empty())
                    }
                    b"item" => {
                        in_item = false;
                        if let (Some(t), Some(a)) = (title.take(), audio.take()) {
                            episodes.push(Episode { title: t, audio: a, duration: duration.take() });
                        }
                    }
                    _ if in_item && e.local_name().as_ref() == b"duration" => {
                        duration = value.as_deref().and_then(parse_duration).or(duration);
                    }
                    _ => {}
                }
                path.pop();
                text = path.last().map(|_| String::new());
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if episodes.is_empty() {
        return Err("the podcast feed lists no episodes with sound".into());
    }
    Ok(Feed { name: name.unwrap_or_else(|| "Podcast".into()), episodes })
}

fn enclosure(e: &BytesStart) -> Option<String> {
    let url = attribute(e, b"url").filter(|u| net::is_url(u))?;
    let kind = attribute(e, b"type").unwrap_or_default().to_ascii_lowercase();
    (kind.is_empty() || kind.starts_with("audio/")).then_some(url)
}

pub fn fetch_feed(url: &str) -> Result<Feed, String> {
    parse_rss(&dash::get_text(url, &[], FEED_LIMIT)?)
}

/// The index of the episode titled `title`, compared as [`normalize`] does.
pub fn find_episode(feed: &Feed, title: &str) -> Option<usize> {
    let wanted = normalize(title);
    feed.episodes.iter().position(|e| normalize(&e.title) == wanted)
}

fn extension(url: &str) -> Option<String> {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let last = path.rsplit('/').next().unwrap_or("");
    last.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).filter(|e| (1..=5).contains(&e.len()))
}

/// Opens an episode's audio file: seekable over HTTP ranges when the server allows them,
/// otherwise as a plain download that plays from the start.
pub fn open_episode(url: &str, abort: Arc<AtomicBool>) -> Result<Box<dyn Demuxer>, String> {
    if let Ok((fetch, _)) = HttpFetch::connect(url, &[], 1) {
        let source = RangeSource::new(Arc::new(fetch), abort.clone());
        // Symphonia's accurate MP3 seek reads every frame up to the target, which over the
        // network means downloading it; the coarse seek jumps by the bitrate instead.
        if let Ok(demux) = SymDemuxer::open(Box::new(source), extension(url).as_deref(), None) {
            return Ok(Box::new(demux.with_coarse_seeking()));
        }
    }
    let stream = net::open_url(url, abort, Box::new(|_| {}))?;
    demux::open_stream(stream.source, stream.ext.as_deref().or(extension(url).as_deref()), stream.mime.as_deref())
}
