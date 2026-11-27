//! Browsing YouTube without a browser: a search, or a channel's newest videos (and a search
//! inside the channel), read through yt-dlp a page of [`PAGE`] videos at a time, with their
//! thumbnails, and the channels opened recently remembered across runs.
//!
//! yt-dlp is asked in English: it dates a channel's videos only from YouTube's English
//! "3 days ago" (the `approximate_date` option). The price is that YouTube may show some
//! titles auto-translated; the video's own title shows once it plays.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use serde_json::{Value, json};

/// Videos asked for at a time.
pub const PAGE: usize = 10;
/// The longest list "Show more" grows: each video holds a thumbnail texture (~230 KB).
pub const MAX_ITEMS: usize = 200;
/// Channels remembered.
pub const RECENT_LIMIT: usize = 8;
const THUMBNAIL_LIMIT: u64 = 1 << 20;

#[derive(Clone, Debug, PartialEq)]
pub enum Query {
    /// A channel's videos, newest first.
    Channel { url: String },
    /// A search among one channel's videos.
    InChannel { url: String, text: String },
    /// A search of all of YouTube.
    Search { text: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Channel {
    pub name: String,
    /// `https://www.youtube.com/@handle` or `…/channel/UC…`.
    pub url: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub id: String,
    pub title: String,
    pub channel: Option<Channel>,
    pub duration: Option<f64>,
    pub views: Option<u64>,
    /// When it was published, seconds since the Unix epoch; YouTube gives only the day.
    pub uploaded: Option<i64>,
    pub live: bool,
    /// Announced but not started: nothing to play yet.
    pub upcoming: bool,
}

impl Item {
    pub fn url(&self) -> String {
        format!("https://www.youtube.com/watch?v={}", self.id)
    }

    pub fn thumbnail_url(&self) -> String {
        thumbnail_url(&self.id)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Listing {
    /// The channel the videos come from, for a channel's own list.
    pub channel: Option<Channel>,
    pub items: Vec<Item>,
    /// A full page came back, so asking for the next one may find more.
    pub more: bool,
}

/// What `input` asks for: a channel (a channel address or `@handle`), or else a search.
/// `None` for blank input.
pub fn query_for(input: &str) -> Option<Query> {
    let text = input.trim();
    if text.is_empty() {
        return None;
    }
    Some(match channel_url(text) {
        Some(url) => Query::Channel { url },
        None => Query::Search { text: text.to_string() },
    })
}

/// The channel address of `input`: a YouTube channel address in any of its forms (`/@handle`,
/// `/channel/UC…`, `/c/name`, `/user/name`, with or without a tab such as `/videos`), or a bare
/// `@handle`. Always `https://www.youtube.com/…`, without the tab.
pub fn channel_url(input: &str) -> Option<String> {
    let input = input.trim();
    if let Some(handle) = input.strip_prefix('@') {
        let ok = !handle.is_empty() && !handle.contains(|c: char| c.is_whitespace() || "/?#&@".contains(c));
        return ok.then(|| format!("https://www.youtube.com/@{}", encode(handle)));
    }
    // Typed addresses often lack the scheme ("youtube.com/@name") or shout it ("HTTPS://").
    let lower = input.get(..8).unwrap_or(input).to_ascii_lowercase();
    let rest = if lower.starts_with("https://") {
        &input[8..]
    } else if lower.starts_with("http://") {
        &input[7..]
    } else if input.find("://").is_some_and(|i| !input[..i].contains(['/', '?', '#'])) {
        return None;
    } else {
        input
    };
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
    let host = host.split(':').next().unwrap_or(host).to_ascii_lowercase();
    if !matches!(host.as_str(), "youtube.com" | "www.youtube.com" | "m.youtube.com") {
        return None;
    }
    let path = path.split(['?', '#']).next().unwrap_or("");
    let mut parts = path.split('/').filter(|p| !p.is_empty());
    let first = parts.next()?;
    let base = match first {
        handle if handle.len() > 1 && handle.starts_with('@') => handle.to_string(),
        "channel" | "c" | "user" => format!("{first}/{}", parts.next()?),
        _ => return None,
    };
    // "youtube.com/@name latest videos" is a search someone typed, not an address.
    if base.contains(char::is_whitespace) {
        return None;
    }
    Some(format!("https://www.youtube.com/{base}"))
}

/// Percent-encodes everything but the unreserved characters of RFC 3986.
fn encode(text: &str) -> String {
    let mut out = String::new();
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The yt-dlp arguments for the page of `query` after the first `have` videos.
pub fn ytdlp_args(query: &Query, have: usize) -> Vec<String> {
    let (first, last) = (have + 1, have + PAGE);
    let target = match query {
        Query::Channel { url } => format!("{url}/videos"),
        Query::InChannel { url, text } => format!("{url}/search?query={}", encode(text.trim())),
        Query::Search { text } => format!("ytsearch{last}:{}", text.trim()),
    };
    [
        "-J",
        "--flat-playlist",
        "--playlist-items",
        &format!("{first}:{last}"),
        "--extractor-args",
        "youtubetab:approximate_date",
        "--extractor-args",
        "youtube:lang=en",
        "--no-warnings",
        "--no-progress",
        "--",
        &target,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

fn text(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

fn channel_of(v: &Value) -> Option<Channel> {
    let name = text(v, "channel").or_else(|| text(v, "uploader"))?;
    let url = text(v, "uploader_url").and_then(|u| channel_url(&u)).or_else(|| text(v, "channel_url").and_then(|u| channel_url(&u)))?;
    Some(Channel { name, url })
}

/// Reads yt-dlp's `-J --flat-playlist` answer for a search or a channel tab. Entries that are
/// not videos (channels, playlists in search results) are left out.
pub fn parse_listing(json: &str) -> Result<Listing, String> {
    let data: Value = serde_json::from_str(json).map_err(|e| format!("yt-dlp sent an unreadable answer ({e})"))?;
    let entries = data.get("entries").and_then(Value::as_array).cloned().unwrap_or_default();
    let owner = text(&data, "channel_id").and(channel_of(&data));
    let items = entries
        .iter()
        .filter(|e| text(e, "ie_key").is_none_or(|k| k == "Youtube"))
        .filter_map(|e| {
            let id = text(e, "id").filter(|id| id.len() == 11)?;
            let status = text(e, "live_status");
            Some(Item {
                title: text(e, "title").unwrap_or_else(|| id.clone()),
                channel: channel_of(e).or_else(|| owner.clone()),
                duration: e.get("duration").and_then(Value::as_f64).filter(|d| *d > 0.0),
                views: e.get("view_count").and_then(Value::as_u64),
                uploaded: e.get("timestamp").or(e.get("release_timestamp")).and_then(Value::as_i64),
                live: status.as_deref() == Some("is_live"),
                upcoming: status.as_deref() == Some("is_upcoming"),
                id,
            })
        })
        .collect();
    Ok(Listing { channel: owner, items, more: entries.len() >= PAGE })
}

/// Asks yt-dlp for the page of `query` after the first `have` videos.
pub fn fetch(query: &Query, have: usize, abort: &AtomicBool, progress: &dyn Fn(&str)) -> Result<Listing, String> {
    let args = ytdlp_args(query, have);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    parse_listing(&crate::youtube::run_ytdlp(&args, abort, progress)?)
}

/// What a new list does to the way back to search results once it arrives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Back {
    /// A fresh start (a new search, a channel opened directly): no way back.
    Clear,
    /// A channel opened from search results: back leads to those results.
    FromResults,
    /// Paging or searching within the same channel: the way back stays as it was.
    Keep,
}

#[derive(Clone, Debug, PartialEq)]
pub struct View {
    pub query: Query,
    pub listing: Listing,
}

/// A page to fetch: the `have` is how many videos the list already holds.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    pub id: u64,
    pub query: Query,
    pub have: usize,
    pub append: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Failed {
    pub query: Query,
    pub append: bool,
    pub back: Back,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arrival {
    /// An answer to a request that was cancelled or superseded: ignored.
    Stale,
    /// A new list is shown.
    New,
    /// More videos were added to the list shown.
    More,
    Failed,
}

/// What the YouTube window shows and is waiting for. The list on screen stays until the next
/// one has arrived, so cancelling or a failure leaves it (and the way back) as it was.
#[derive(Debug, Default)]
pub struct Nav {
    pub view: Option<View>,
    /// The search results a channel was opened from.
    pub back: Option<View>,
    pub error: Option<Failed>,
    pending: Option<(Request, Back)>,
    last_id: u64,
}

impl Nav {
    /// Starts waiting for `query` (its next page with `append`), forgetting any earlier wait.
    pub fn ask(&mut self, query: Query, append: bool, back: Back) -> Request {
        self.error = None;
        self.last_id += 1;
        let have = if append { self.view.as_ref().map_or(0, |v| v.listing.items.len()) } else { 0 };
        let request = Request { id: self.last_id, query, have, append };
        self.pending = Some((request.clone(), back));
        request
    }

    pub fn pending(&self) -> Option<&Request> {
        self.pending.as_ref().map(|(r, _)| r)
    }

    pub fn cancel(&mut self) {
        self.pending = None;
    }

    pub fn arrived(&mut self, id: u64, result: Result<Listing, String>) -> Arrival {
        let Some((request, back)) = self.pending.take_if(|(r, _)| r.id == id) else { return Arrival::Stale };
        match result {
            Err(message) => {
                self.error = Some(Failed { query: request.query, append: request.append, back, message });
                Arrival::Failed
            }
            Ok(listing) if request.append => {
                if let Some(view) = self.view.as_mut().filter(|v| v.query == request.query) {
                    let mut known: HashSet<String> = view.listing.items.iter().map(|i| i.id.clone()).collect();
                    view.listing.items.extend(listing.items.into_iter().filter(|i| known.insert(i.id.clone())));
                    view.listing.more = listing.more;
                }
                Arrival::More
            }
            Ok(listing) => {
                let shown = self.view.take();
                match back {
                    Back::Clear => self.back = None,
                    Back::FromResults => {
                        if let Some(results) = shown.filter(|v| matches!(v.query, Query::Search { .. })) {
                            self.back = Some(results);
                        }
                    }
                    Back::Keep => {}
                }
                self.view = Some(View { query: request.query, listing });
                Arrival::New
            }
        }
    }

    /// The request that failed, asked again.
    pub fn retry(&mut self) -> Option<Request> {
        let failed = self.error.take()?;
        Some(self.ask(failed.query, failed.append, failed.back))
    }

    /// The next page of the list shown, while there may be one and the list is not at
    /// [`MAX_ITEMS`].
    pub fn more(&mut self) -> Option<Request> {
        let query = self.view.as_ref().filter(|v| v.listing.more && v.listing.items.len() < MAX_ITEMS)?.query.clone();
        Some(self.ask(query, true, Back::Keep))
    }

    pub fn can_show_more(&self) -> bool {
        self.view.as_ref().is_some_and(|v| v.listing.more && v.listing.items.len() < MAX_ITEMS)
    }

    /// Returns to the search results a channel was opened from. Whatever was being waited for
    /// is dropped.
    pub fn go_back(&mut self) -> bool {
        let Some(results) = self.back.take() else { return false };
        self.pending = None;
        self.error = None;
        self.view = Some(results);
        true
    }

    /// The videos of the lists that can be on screen: the one shown and the one back leads to.
    pub fn shown_ids(&self) -> HashSet<String> {
        [&self.view, &self.back].into_iter().flatten().flat_map(|v| v.listing.items.iter().map(|i| i.id.clone())).collect()
    }
}

/// "today", "yesterday", "3 days ago", "2 weeks ago", "5 months ago", "1 year ago", counted in
/// whole days from `uploaded` to `now` (both seconds since the Unix epoch).
pub fn age_label(now: i64, uploaded: i64) -> String {
    let days = now.div_euclid(86_400) - uploaded.div_euclid(86_400);
    let ago = |n: i64, unit: &str| format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" });
    match days {
        ..=0 => "today".into(),
        1 => "yesterday".into(),
        2..=6 => ago(days, "day"),
        7..=29 => ago(days / 7, "week"),
        30..=364 => ago((days / 30).min(11), "month"),
        _ => ago(days / 365, "year"),
    }
}

/// "1 view", "950 views", "12K views", "1.2M views", "3.4B views".
pub fn views_label(views: u64) -> String {
    let short = |n: f64, unit: &str| {
        let s = if n < 10.0 { format!("{:.1}", (n * 10.0).floor() / 10.0) } else { format!("{:.0}", n.floor()) };
        format!("{}{unit} views", s.trim_end_matches(".0"))
    };
    match views {
        1 => "1 view".into(),
        0..1_000 => format!("{views} views"),
        1_000..1_000_000 => short(views as f64 / 1e3, "K"),
        1_000_000..1_000_000_000 => short(views as f64 / 1e6, "M"),
        _ => short(views as f64 / 1e9, "B"),
    }
}

/// The 320×180 picture YouTube keeps for every video.
pub fn thumbnail_url(id: &str) -> String {
    format!("https://i.ytimg.com/vi/{id}/mqdefault.jpg")
}

/// A decoded picture: width, height and RGB bytes, row by row.
pub type Rgb = (usize, usize, Vec<u8>);

pub fn decode_jpeg(bytes: &[u8]) -> Result<Rgb, String> {
    use zune_jpeg::JpegDecoder;
    use zune_jpeg::zune_core::bytestream::ZCursor;
    use zune_jpeg::zune_core::colorspace::ColorSpace;
    use zune_jpeg::zune_core::options::DecoderOptions;
    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGB);
    let mut decoder = JpegDecoder::new_with_options(ZCursor::new(bytes), options);
    let pixels = decoder.decode().map_err(|e| format!("unreadable picture ({e:?})"))?;
    let (w, h) = decoder.dimensions().ok_or("unreadable picture")?;
    if pixels.len() != w * h * 3 {
        return Err("unreadable picture".into());
    }
    Ok((w, h, pixels))
}

pub fn fetch_thumbnail(url: &str) -> Result<Rgb, String> {
    decode_jpeg(&crate::dash::get_bytes(url, &[], THUMBNAIL_LIMIT)?)
}

/// The channels opened lately, newest first, kept in `channels.json` in playr's data folder.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Recent {
    pub channels: Vec<Channel>,
}

impl Recent {
    /// Puts `channel` on top; one entry per address, the newest [`RECENT_LIMIT`] kept.
    pub fn record(&mut self, channel: &Channel) {
        self.channels.retain(|c| c.url != channel.url);
        self.channels.insert(0, channel.clone());
        self.channels.truncate(RECENT_LIMIT);
    }

    pub fn remove(&mut self, url: &str) {
        self.channels.retain(|c| c.url != url);
    }

    pub fn to_json(&self) -> String {
        let list: Vec<Value> = self.channels.iter().map(|c| json!({ "name": c.name, "url": c.url })).collect();
        serde_json::to_string_pretty(&json!({ "channels": list })).unwrap_or_default()
    }

    /// Reads [`to_json`](Self::to_json)'s output; unreadable entries are skipped.
    pub fn parse(json: &str) -> Recent {
        let mut recent = Recent::default();
        let Ok(data) = serde_json::from_str::<Value>(json) else { return recent };
        for v in data.get("channels").and_then(Value::as_array).into_iter().flatten() {
            let (Some(name), Some(url)) = (text(v, "name"), text(v, "url").and_then(|u| channel_url(&u))) else {
                continue;
            };
            if recent.channels.len() < RECENT_LIMIT && !recent.channels.iter().any(|c| c.url == url) {
                recent.channels.push(Channel { name, url });
            }
        }
        recent
    }

    pub fn load(path: &Path) -> Recent {
        std::fs::read_to_string(path).map(|t| Recent::parse(&t)).unwrap_or_default()
    }

    /// Writes through a temporary file, so a crash mid-write never leaves half a list.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        }
        let partial = path.with_extension("json.part");
        std::fs::write(&partial, self.to_json()).map_err(|e| format!("cannot save the channels: {e}"))?;
        std::fs::rename(&partial, path).map_err(|e| format!("cannot save the channels: {e}"))
    }
}

pub fn recent_path() -> Option<PathBuf> {
    crate::youtube::data_dir().map(|d| d.join("channels.json"))
}
