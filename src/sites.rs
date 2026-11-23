//! Video pages of other sites (any of the ones yt-dlp knows), played like a file.
//!
//! An address that answers with an HTML page is handed to yt-dlp, which reports the page's
//! streams. playr plays those that are one file holding both picture and sound, fetched over
//! HTTP ranges so they can be sought in, and HLS streams (a playlist of short pieces), whose
//! picture-only qualities are joined with the page's best HLS sound. Split progressive streams
//! (picture and sound as two files) are not played; a page offering nothing else is explained.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use serde_json::Value;

use crate::dash::{HttpFetch, RangeSource};
use crate::demux::{self, Demuxer};
use crate::hls::{self, Location};

/// The tallest picture picked by default: decoding more is too slow in software.
pub const DEFAULT_MAX_HEIGHT: u32 = 1080;

#[derive(Clone, Debug, PartialEq)]
pub struct Format {
    pub id: String,
    pub url: String,
    pub ext: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Kilobits per second, when the site says.
    pub tbr: Option<f64>,
    /// An HLS playlist rather than one file.
    pub hls: bool,
    /// For an HLS format without sound: the HLS playlist of the sound played with it.
    pub audio: Option<String>,
}

impl Format {
    /// "720p", or the site's own name for it ("high") when the height is unknown.
    pub fn label(&self) -> String {
        match self.height {
            Some(h) => format!("{h}p"),
            None => self.id.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Page {
    pub site: String,
    pub title: String,
    pub duration: Option<f64>,
    /// Request headers the site wants with its streams (a Referer, a User-Agent).
    pub headers: Vec<(String, String)>,
    /// Playable formats, worst first, as yt-dlp orders them.
    pub formats: Vec<Format>,
    /// Some streams exist but none can be played here (split streams only).
    pub only_unplayable: bool,
}

impl Page {
    /// The best format no taller than [`DEFAULT_MAX_HEIGHT`] (a format of unknown height
    /// counts as fitting), or the smallest one if all are taller.
    pub fn default_format(&self) -> Option<&Format> {
        self.formats
            .iter()
            .rev()
            .find(|f| f.height.is_none_or(|h| h <= DEFAULT_MAX_HEIGHT))
            .or_else(|| self.formats.first())
    }

    pub fn pick(&self, id: Option<&str>) -> Option<&Format> {
        id.and_then(|id| self.formats.iter().find(|f| f.id == id)).or_else(|| self.default_format())
    }
}

fn text(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

fn has_track(codec: Option<&str>) -> bool {
    codec != Some("none")
}

/// Reads yt-dlp's `-J` answer for a page.
pub fn parse_page(json: &str) -> Result<Page, String> {
    let data: Value = serde_json::from_str(json).map_err(|e| format!("yt-dlp sent an unreadable answer ({e})"))?;
    let title = text(&data, "title").unwrap_or_else(|| "Video".into());
    let site = text(&data, "extractor_key").or_else(|| text(&data, "extractor")).unwrap_or_else(|| "Web".into());
    let duration = data.get("duration").and_then(Value::as_f64).filter(|d| *d > 0.0);
    let headers = data
        .get("http_headers")
        .and_then(Value::as_object)
        .map(|h| h.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect())
        .unwrap_or_default();
    let all: Vec<&Value> = match data.get("formats").and_then(Value::as_array) {
        Some(list) => list.iter().collect(),
        None => vec![&data],
    };
    let mut formats: Vec<Format> = Vec::new();
    let mut seen_any = false;
    let is_hls = |f: &Value| text(f, "protocol").is_some_and(|p| p.starts_with("m3u8"));
    let codec = |f: &Value, key: &str| f.get(key).and_then(Value::as_str).map(str::to_string);
    let number = |f: &Value, k: &str| f.get(k).and_then(Value::as_u64).map(|n| n as u32).filter(|n| *n > 0);
    // The sound joined to picture-only HLS formats: the HLS sound of the highest bitrate.
    let hls_audio = all
        .iter()
        .filter(|f| is_hls(f) && codec(f, "vcodec").as_deref() == Some("none") && has_track(codec(f, "acodec").as_deref()))
        .filter(|f| text(f, "url").is_some_and(|u| crate::net::is_url(&u)))
        .max_by(|a, b| {
            let rate = |f: &Value| f.get("abr").or(f.get("tbr")).and_then(Value::as_f64).unwrap_or(0.0);
            rate(a).total_cmp(&rate(b))
        })
        .and_then(|f| text(f, "url"));
    for f in all.iter().copied() {
        let Some(url) = text(f, "url") else { continue };
        let protocol = text(f, "protocol").unwrap_or_default();
        let progressive = matches!(protocol.as_str(), "https" | "http");
        let hls = is_hls(f);
        let vcodec = codec(f, "vcodec");
        let acodec = codec(f, "acodec");
        let media = (has_track(vcodec.as_deref()) || has_track(acodec.as_deref())) && !matches!(text(f, "ext").as_deref(), Some("mhtml"));
        seen_any |= media && text(f, "format_id").is_some();
        if !has_track(vcodec.as_deref()) || !crate::net::is_url(&url) {
            continue;
        }
        let audio = match (progressive, hls, has_track(acodec.as_deref())) {
            (true, _, true) | (_, true, true) => None,
            (_, true, false) if hls_audio.is_some() => hls_audio.clone(),
            _ => continue,
        };
        if hls && !crate::hls::codecs_supported(Some(&[vcodec.clone(), acodec.clone()].into_iter().flatten().filter(|c| c != "none").collect::<Vec<_>>().join(","))) {
            continue;
        }
        let Some(id) = text(f, "format_id") else { continue };
        let format = Format {
            id,
            url,
            ext: text(f, "ext"),
            width: number(f, "width"),
            height: number(f, "height"),
            tbr: f.get("tbr").and_then(Value::as_f64).filter(|t| *t > 0.0),
            hls,
            audio,
        };
        // One entry per height: a single file (quicker to seek in) over HLS.
        match formats.iter_mut().find(|g| g.height.is_some() && g.height == format.height) {
            Some(g) if g.hls && !format.hls => *g = format,
            Some(g) if g.hls == format.hls && format.tbr > g.tbr => *g = format,
            Some(_) => {}
            None => formats.push(format),
        }
    }
    let only_unplayable = formats.is_empty() && seen_any;
    Ok(Page { site, title, duration, headers, formats, only_unplayable })
}

/// Why a page cannot be played, if it cannot.
pub fn unplayable_reason(page: &Page) -> Option<String> {
    if !page.formats.is_empty() {
        return None;
    }
    Some(if page.only_unplayable {
        format!(
            "{} offers this video only as separate picture and sound files or in formats playr cannot decode",
            page.site
        )
    } else {
        "the page has no video that yt-dlp can find".into()
    })
}

/// How long a page's stream addresses are reused before yt-dlp is asked again (sites sign
/// them for a limited time).
pub const CACHE_FOR: std::time::Duration = std::time::Duration::from_secs(30 * 60);

pub fn cache_fresh(age: std::time::Duration) -> bool {
    age < CACHE_FOR
}

/// The page address handed to yt-dlp: upgraded to https the way playr's own fetches are.
pub fn ytdlp_address(url: &str) -> String {
    crate::privacy::attempts(url).first().map_or_else(|| url.trim().to_string(), |a| a.url.clone())
}

/// Opens `format` of `page` for playing: a single file is read over HTTP ranges (with the
/// site's headers), so it can be sought in; HLS is fetched piece by piece. Also says whether
/// the stream came over an encrypted connection.
pub fn open(page: &Page, format: &Format, abort: Arc<AtomicBool>) -> Result<(Box<dyn Demuxer>, bool), String> {
    if format.hls {
        let video = Location::Url(format.url.clone());
        let opened = match &format.audio {
            Some(audio) => hls::open_pair(&video, &Location::Url(audio.clone()), &page.headers, abort)?,
            None => hls::open(&video, None, &page.headers, false, abort)?,
        };
        return Ok((opened.demuxer, opened.secure));
    }
    let (fetch, _) = HttpFetch::connect(&format.url, &page.headers, 1)
        .map_err(|e| {
            let no_ranges = e.contains("does not support ranged downloads") || e.contains("ignored the byte range");
            if no_ranges { format!("{} does not let this video be fetched in parts ({e})", page.site) } else { e }
        })?;
    let secure = fetch.is_encrypted();
    let source = RangeSource::new(Arc::new(fetch), abort);
    Ok((demux::open_stream(Box::new(source), format.ext.as_deref(), None)?, secure))
}

pub fn is_html(mime: Option<&str>) -> bool {
    matches!(mime, Some("text/html" | "application/xhtml+xml"))
}
