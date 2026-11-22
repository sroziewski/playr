//! Spotify links played as radio: the track list comes from Spotify, the sound from YouTube.
//!
//! Spotify encrypts its own audio (Widevine DRM), which no open player may decrypt, and its
//! public pages offer only 30–60 second preview clips. What Spotify does show without signing
//! in is the list itself, in the page its "embed" player loads. So each entry is looked up on
//! YouTube by artist and title and played from there as sound only (see
//! [`crate::youtube::search`]), one after the other.

use serde_json::Value;

use crate::cookies::CookieJar;

/// Where the embed pages are fetched from; the variable [`EMBED_ENV`] points it elsewhere
/// (a mirror, or a local server in tests).
pub const EMBED_BASE: &str = "https://open.spotify.com/embed/";
pub const EMBED_ENV: &str = "PLAYR_SPOTIFY_EMBED";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Playlist,
    Album,
    Track,
    Episode,
    Show,
}

impl Kind {
    fn path(self) -> &'static str {
        match self {
            Kind::Playlist => "playlist",
            Kind::Album => "album",
            Kind::Track => "track",
            Kind::Episode => "episode",
            Kind::Show => "show",
        }
    }
}

/// The kind and id of a Spotify link: `https://open.spotify.com/playlist/<id>` (also with an
/// `intl-xx/` prefix or a query string) or `spotify:playlist:<id>`.
pub fn parse_url(url: &str) -> Option<(Kind, String)> {
    let url = url.trim();
    let mut parts: Vec<&str> = if let Some(rest) = url.strip_prefix("spotify:") {
        rest.split(':').collect()
    } else {
        let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"))?;
        let (host, path) = rest.split_once('/')?;
        if !host.eq_ignore_ascii_case("open.spotify.com") {
            return None;
        }
        path.split(['?', '#']).next().unwrap_or("").split('/').filter(|p| !p.is_empty()).collect()
    };
    if parts.first().is_some_and(|p| p.starts_with("intl-")) {
        parts.remove(0);
    }
    if parts.first() == Some(&"embed") {
        parts.remove(0);
    }
    let kind = match *parts.first()? {
        "playlist" => Kind::Playlist,
        "album" => Kind::Album,
        "track" => Kind::Track,
        "episode" => Kind::Episode,
        "show" => Kind::Show,
        _ => return None,
    };
    let id = *parts.get(1)?;
    let valid = id.len() == 22 && id.bytes().all(|b| b.is_ascii_alphanumeric());
    valid.then(|| (kind, id.to_string()))
}

pub fn is_spotify_url(url: &str) -> bool {
    parse_url(url).is_some()
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub title: String,
    /// The artists of a song, the show of a podcast episode.
    pub subtitle: String,
    /// Seconds, as Spotify gives it; used to pick the right recording on YouTube.
    pub duration: Option<f64>,
}

impl Entry {
    /// What to search YouTube for.
    pub fn query(&self) -> String {
        if self.subtitle.is_empty() { self.title.clone() } else { format!("{} {}", self.subtitle, self.title) }
    }

    /// "Title — Artist".
    pub fn label(&self) -> String {
        if self.subtitle.is_empty() { self.title.clone() } else { format!("{} — {}", self.title, self.subtitle) }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Playlist {
    pub name: String,
    pub entries: Vec<Entry>,
}

fn text(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

fn entry(v: &Value) -> Option<Entry> {
    let title = text(v, "title").or_else(|| text(v, "name"))?;
    let subtitle = text(v, "subtitle")
        .or_else(|| {
            v.get("artists")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|x| text(x, "name")).collect::<Vec<_>>().join(", "))
        })
        .unwrap_or_default();
    let duration = v.get("duration").and_then(Value::as_f64).filter(|d| *d > 0.0).map(|ms| ms / 1000.0);
    Some(Entry { title, subtitle, duration })
}

/// Reads the list out of a Spotify embed page (the JSON its `__NEXT_DATA__` script carries).
pub fn parse_embed(html: &str) -> Result<Playlist, String> {
    let marker = "<script id=\"__NEXT_DATA__\" type=\"application/json\">";
    let start = html.find(marker).ok_or("Spotify sent a page without the track list")? + marker.len();
    let end = html[start..].find("</script>").ok_or("Spotify sent a cut-off page")? + start;
    let data: Value = serde_json::from_str(&html[start..end]).map_err(|e| format!("Spotify sent an unreadable list ({e})"))?;
    let entity = data
        .pointer("/props/pageProps/state/data/entity")
        .ok_or("Spotify's page has no playlist in it (is the link private or removed?)")?;
    let name = text(entity, "name").or_else(|| text(entity, "title")).unwrap_or_else(|| "Spotify".into());
    let entries: Vec<Entry> = match entity.get("trackList").and_then(Value::as_array) {
        Some(list) => list.iter().filter_map(entry).collect(),
        None => entry(entity).into_iter().collect(),
    };
    if entries.is_empty() {
        return Err("this Spotify list is empty".into());
    }
    Ok(Playlist { name, entries })
}

/// Fetches the list behind a Spotify link. Cookies for spotify.com, if given, go along with
/// the request.
pub fn fetch(url: &str, cookies: Option<&CookieJar>) -> Result<Playlist, String> {
    let (kind, id) = parse_url(url).ok_or("not a Spotify playlist, album, track or episode link")?;
    let base = std::env::var(EMBED_ENV).ok().filter(|b| !b.is_empty()).unwrap_or_else(|| EMBED_BASE.to_string());
    let embed = format!("{}/{}/{id}", base.trim_end_matches('/'), kind.path());
    let mut headers = vec![("User-Agent".to_string(), "Mozilla/5.0 (Windows NT 10.0; Win64; x64)".to_string())];
    if let Some(header) = cookies.and_then(|jar| jar.header_for(&embed, crate::cookies::now_unix())) {
        headers.push(("Cookie".to_string(), header));
    }
    parse_embed(&crate::dash::get_text(&embed, &headers, 8 << 20)?)
}
