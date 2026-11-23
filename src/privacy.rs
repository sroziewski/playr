//! Encrypted connections only: what playr fetches over the network goes over HTTPS, so a
//! Wi-Fi or VPN operator watching the traffic sees which site is contacted but not what is
//! played on it.
//!
//! An `http://` address is tried as `https://` first (most radio servers answer both). With
//! [`https_only`] on, a server that speaks only plain HTTP is refused, and so is any redirect
//! from HTTPS back to HTTP; with it off, playr falls back to plain HTTP. Addresses on this
//! computer (`localhost`, `127.0.0.1`, `[::1]`) never cross the network and are left alone.
//!
//! The site's name still shows: the DNS lookup and the TLS handshake carry the host name.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{Value, json};

static HTTPS_ONLY: AtomicBool = AtomicBool::new(true);

pub fn https_only() -> bool {
    HTTPS_ONLY.load(Ordering::Relaxed)
}

pub fn set_https_only(on: bool) {
    HTTPS_ONLY.store(on, Ordering::Relaxed);
}

fn host_of(url: &str) -> Option<&str> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    if let Some(v6) = host.strip_prefix('[') {
        return v6.split(']').next();
    }
    Some(host.split(':').next().unwrap_or(host))
}

/// An address on this computer: its traffic never leaves the machine.
pub fn is_loopback(url: &str) -> bool {
    let Some(host) = host_of(url) else { return false };
    let host = host.to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

fn is_plain_http(url: &str) -> bool {
    url.get(..7).is_some_and(|s| s.eq_ignore_ascii_case("http://"))
}

/// One way to fetch an address: the URL to ask and whether the client must refuse anything
/// that is not HTTPS (redirects included).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attempt {
    pub url: String,
    pub https_only: bool,
}

/// The ways to fetch `url`, in order, under the current setting.
pub fn attempts(url: &str) -> Vec<Attempt> {
    attempts_with(url, https_only())
}

pub fn attempts_with(url: &str, strict: bool) -> Vec<Attempt> {
    let url = url.trim();
    if is_loopback(url) {
        return vec![Attempt { url: url.to_string(), https_only: false }];
    }
    if !is_plain_http(url) {
        return vec![Attempt { url: url.to_string(), https_only: strict }];
    }
    let rest = &url[7..];
    let (authority, path) = rest.find(['/', '?', '#']).map_or((rest, ""), |i| rest.split_at(i));
    let authority = authority.strip_suffix(":80").unwrap_or(authority);
    let secure = Attempt { url: format!("https://{authority}{path}"), https_only: true };
    if strict { vec![secure] } else { vec![secure, Attempt { url: url.to_string(), https_only: false }] }
}

/// Why `url` was not played when only its unencrypted form answered.
pub fn refused(url: &str, why: &str) -> String {
    let host = host_of(url).unwrap_or(url);
    format!(
        "{host} could not be opened over an encrypted connection (https), and playr is set to encrypted \
         connections only, so it was not opened unencrypted. Untick \u{1f512} Encrypted connections only to \
         play it anyway. ({why})"
    )
}

/// Runs `fetch` for each of [`attempts`] until one succeeds. When every one fails under the
/// encrypted-only setting, the error says so.
pub fn fetch<T>(url: &str, mut fetch: impl FnMut(&Attempt) -> Result<T, String>) -> Result<T, String> {
    let strict = https_only();
    let mut first_error = None;
    for attempt in attempts_with(url, strict) {
        match fetch(&attempt) {
            Ok(value) => return Ok(value),
            Err(e) => {
                first_error.get_or_insert(e);
            }
        }
    }
    let why = first_error.unwrap_or_else(|| "no way to connect".into());
    Err(if strict && is_unencrypted(url) { refused(url, &why) } else { why })
}

/// Whether playing from `url` would send what is played across the network unencrypted.
pub fn is_unencrypted(url: &str) -> bool {
    is_plain_http(url.trim()) && !is_loopback(url)
}

pub fn settings_path() -> Option<PathBuf> {
    crate::youtube::data_dir().map(|d| d.join("privacy.json"))
}

/// The saved setting; on when nothing was saved or the file cannot be read.
pub fn load(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("https_only").and_then(Value::as_bool))
        .unwrap_or(true)
}

pub fn save(path: &Path, on: bool) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    std::fs::write(path, json!({ "https_only": on }).to_string()).map_err(|e| format!("cannot save the setting: {e}"))
}
