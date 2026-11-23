//! Playlist files (.pls, .m3u) read as lists to step through, rather than as one address.
//!
//! [`crate::net::parse_playlist`] takes only the first address, which is right for a radio
//! station whose playlist lists mirrors of one stream; a playlist the user opens is a list of
//! songs or stations, each played in turn.

use std::path::Path;

use crate::net;
use crate::spotify::Entry;

/// The entries of a .pls or .m3u text, in file order. Relative paths are taken from `base`
/// (the playlist's folder); without one only addresses are kept. HLS media playlists give no
/// entries, since their lines are pieces of one stream.
pub fn parse(text: &str, base: Option<&Path>) -> Vec<Entry> {
    parse_from(text, &Base { dir: base, url: None })
}

/// [`parse`] for a list fetched from `url`: relative lines are addresses next to it. Only
/// http(s) entries are kept: a list from the web must not make playr open files on this
/// computer or on a network share (`file://host/…` would make Windows sign in to that host).
pub fn parse_remote(text: &str, url: &str) -> Vec<Entry> {
    parse_from(text, &Base { dir: None, url: Some(url) })
}

struct Base<'a> {
    dir: Option<&'a Path>,
    url: Option<&'a str>,
}

fn parse_from(text: &str, base: &Base) -> Vec<Entry> {
    let text = text.trim_start_matches('\u{feff}');
    if text.contains("#EXT-X-") {
        return Vec::new();
    }
    let is_pls = text.lines().any(|l| l.trim().eq_ignore_ascii_case("[playlist]"));
    if is_pls { parse_pls(text, base) } else { parse_m3u(text, base) }
}

/// `relative` resolved against the address of the list it came from.
pub fn join_url(list_url: &str, relative: &str) -> Option<String> {
    let (scheme, rest) = list_url.split_once("://")?;
    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    let (host, path) = rest.split_once('/').map_or((rest, ""), |(h, p)| (h, p));
    if host.is_empty() {
        return None;
    }
    let relative = encode_path(relative);
    if let Some(absolute) = relative.strip_prefix("//") {
        return Some(format!("{scheme}://{absolute}"));
    }
    if relative.starts_with('/') {
        return Some(format!("{scheme}://{host}{relative}"));
    }
    let dir = path.rsplit_once('/').map_or("", |(d, _)| d);
    let joined = if dir.is_empty() { relative } else { format!("{dir}/{relative}") };
    Some(format!("{scheme}://{host}/{joined}"))
}

/// Percent-encodes what may not stand in a URL as it is (spaces, quotes, non-ASCII letters),
/// leaving existing escapes and the URL's own punctuation alone.
fn encode_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        let escape = b == b'%' && bytes.get(i + 1..i + 3).is_some_and(|h| h.iter().all(u8::is_ascii_hexdigit));
        let keep = escape || b.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:@/?[]".contains(&b);
        if keep {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn target(raw: &str, base: &Base) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if net::is_url(raw) {
        return Some(raw.to_string());
    }
    if let Some(url) = base.url {
        let has_scheme = raw.split_once(':').is_some_and(|(s, _)| s.len() > 1 && s.bytes().all(|b| b.is_ascii_alphabetic()));
        if has_scheme {
            return None;
        }
        return join_url(url, &raw.replace('\\', "/")).filter(|u| net::is_url(u));
    }
    let local = match raw.get(..7) {
        Some(scheme) if scheme.eq_ignore_ascii_case("file://") => Some(file_url_path(&raw[7..])),
        _ => None,
    };
    let raw = local.as_deref().unwrap_or(raw);
    let path = Path::new(raw);
    if path.is_absolute() {
        return Some(raw.to_string());
    }
    base.dir.map(|b| b.join(path).to_string_lossy().into_owned())
}

/// The path in a `file://` URL (what follows the scheme): percent escapes decoded, and on
/// Windows the slash before a drive letter (`/C:/…`) dropped and a host (`file://server/…`)
/// read as a network share (`//server/…`).
fn file_url_path(rest: &str) -> String {
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    if cfg!(windows) && !rest.is_empty() && !rest.starts_with('/') && !rest.contains(':') {
        return file_url_path(&format!("//{rest}"));
    }
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    let path = String::from_utf8_lossy(&out).into_owned();
    let drive = path.len() >= 3 && path.as_bytes()[0] == b'/' && path.as_bytes()[2] == b':' && path.as_bytes()[1].is_ascii_alphabetic();
    if cfg!(windows) && drive { path[1..].to_string() } else { path }
}

/// The last part of a playlist's path or address: what a list is called.
pub fn list_name(target: &str) -> String {
    let path = target.split(['?', '#']).next().unwrap_or(target).trim_end_matches(['/', '\\']);
    let last = path.rsplit(['/', '\\']).next().unwrap_or(path);
    if last.is_empty() { target.to_string() } else { last.to_string() }
}

fn name_of(target: &str) -> String {
    let last = list_name(target);
    if net::is_url(target) && !last.contains('.') { target.to_string() } else { last }
}

fn entry(target: String, title: Option<&str>, secs: Option<f64>) -> Entry {
    let title = title.map(str::trim).filter(|t| !t.is_empty()).map_or_else(|| name_of(&target), str::to_string);
    Entry { title, subtitle: String::new(), duration: secs.filter(|s| *s > 0.0), audio: Some(target) }
}

fn parse_pls(text: &str, base: &Base) -> Vec<Entry> {
    let mut rows: Vec<(u32, Option<String>, Option<String>, Option<f64>)> = Vec::new();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else { continue };
        let key = key.trim().to_ascii_lowercase();
        let (field, number) = if let Some(n) = key.strip_prefix("file") {
            ("file", n)
        } else if let Some(n) = key.strip_prefix("title") {
            ("title", n)
        } else if let Some(n) = key.strip_prefix("length") {
            ("length", n)
        } else {
            continue;
        };
        let Ok(n) = number.parse::<u32>() else { continue };
        let at = match rows.iter().position(|r| r.0 == n) {
            Some(i) => i,
            None => {
                rows.push((n, None, None, None));
                rows.len() - 1
            }
        };
        let value = value.trim();
        match field {
            "file" => rows[at].1 = target(value, base),
            "title" => rows[at].2 = Some(value.to_string()),
            _ => rows[at].3 = value.parse::<f64>().ok(),
        }
    }
    rows.sort_by_key(|r| r.0);
    rows.into_iter().filter_map(|(_, file, title, secs)| Some(entry(file?, title.as_deref(), secs))).collect()
}

fn parse_m3u(text: &str, base: &Base) -> Vec<Entry> {
    let mut entries = Vec::new();
    let mut info: Option<(Option<f64>, String)> = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            let (secs, title) = rest.split_once(',').unwrap_or((rest, ""));
            let secs = secs.split_whitespace().next().and_then(|s| s.parse::<f64>().ok());
            info = Some((secs, title.trim().to_string()));
            continue;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (secs, title) = info.take().unwrap_or((None, String::new()));
        if let Some(t) = target(line, base) {
            entries.push(entry(t, Some(&title), secs));
        }
    }
    entries
}
