//! Browser cookies in the Netscape `cookies.txt` format (what "Get cookies.txt"-style browser
//! extensions and yt-dlp write), chosen by the user and remembered between runs.
//!
//! Such an export usually holds every site the browser knows, banking and mail included, so
//! nothing here hands the whole file to anyone: a request gets only the cookies whose domain,
//! path and scheme match it, and yt-dlp gets a private copy holding only the YouTube and Google
//! cookies (it rewrites the file it is given, which must not be the user's own).

use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cookie {
    /// As written in the file: `.spotify.com` (with subdomains) or `open.spotify.com`.
    pub domain: String,
    pub include_subdomains: bool,
    pub path: String,
    pub secure: bool,
    /// Unix seconds; 0 for a session cookie.
    pub expires: u64,
    pub name: String,
    pub value: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CookieJar {
    pub cookies: Vec<Cookie>,
}

/// Parses a Netscape cookie file. Lines that are not cookies are skipped; a file without a
/// single cookie is an error, which is how a wrong file is caught.
pub fn parse(text: &str) -> Result<CookieJar, String> {
    let mut cookies = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        // curl and the browser extensions mark HttpOnly cookies with this prefix.
        let line = match line.strip_prefix("#HttpOnly_") {
            Some(rest) => rest,
            None if line.starts_with('#') || line.trim().is_empty() => continue,
            None => line,
        };
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() != 7 {
            continue;
        }
        let flag = |s: &str| s.eq_ignore_ascii_case("TRUE");
        let domain = fields[0].trim().to_ascii_lowercase();
        if domain.is_empty() || fields[5].is_empty() {
            continue;
        }
        cookies.push(Cookie {
            include_subdomains: flag(fields[1]) || domain.starts_with('.'),
            domain,
            path: if fields[2].is_empty() { "/".into() } else { fields[2].to_string() },
            secure: flag(fields[3]),
            expires: fields[4].trim().parse::<f64>().map(|e| e.max(0.0) as u64).unwrap_or(0),
            name: fields[5].to_string(),
            value: fields[6].to_string(),
        });
    }
    if cookies.is_empty() {
        return Err("no cookies found; expected a Netscape cookies.txt file (tab-separated)".into());
    }
    Ok(CookieJar { cookies })
}

pub fn load(path: &Path) -> Result<CookieJar, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    parse(&String::from_utf8_lossy(&bytes))
}

/// Whether a cookie set for `cookie_domain` is sent to `host`.
pub fn domain_matches(cookie_domain: &str, include_subdomains: bool, host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    let bare = cookie_domain.trim_start_matches('.').to_ascii_lowercase();
    if bare.is_empty() {
        return false;
    }
    host == bare || (include_subdomains && host.ends_with(&format!(".{bare}")))
}

/// Scheme, host and path of an http(s) URL.
fn split_url(url: &str) -> Option<(bool, String, String)> {
    let (secure, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else {
        (false, url.strip_prefix("http://")?)
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = host.split(':').next().unwrap_or(host).to_ascii_lowercase();
    let path = rest[end..].split(['?', '#']).next().unwrap_or("");
    let path = if path.is_empty() { "/".to_string() } else { path.to_string() };
    Some((secure, host, path))
}

fn path_matches(cookie_path: &str, request_path: &str) -> bool {
    request_path == cookie_path
        || (request_path.starts_with(cookie_path) && (cookie_path.ends_with('/') || request_path[cookie_path.len()..].starts_with('/')))
}

impl CookieJar {
    /// The `Cookie` header a browser would send with a request to `url` at `now` (Unix
    /// seconds), or `None` when no cookie applies.
    pub fn header_for(&self, url: &str, now: u64) -> Option<String> {
        let (secure, host, path) = split_url(url)?;
        let pairs: Vec<String> = self
            .cookies
            .iter()
            .filter(|c| c.expires == 0 || c.expires > now)
            .filter(|c| !c.secure || secure)
            .filter(|c| domain_matches(&c.domain, c.include_subdomains, &host))
            .filter(|c| path_matches(&c.path, &path))
            .map(|c| format!("{}={}", c.name, c.value))
            .collect();
        (!pairs.is_empty()).then(|| pairs.join("; "))
    }

    /// The cookies that would be sent to any of `sites` (bare domains such as `youtube.com`) or
    /// to their subdomains.
    pub fn for_sites(&self, sites: &[&str]) -> CookieJar {
        let cookies = self
            .cookies
            .iter()
            .filter(|c| {
                let bare = c.domain.trim_start_matches('.');
                sites.iter().any(|site| bare == *site || bare.ends_with(&format!(".{site}")))
            })
            .cloned()
            .collect();
        CookieJar { cookies }
    }

    /// The jar in Netscape format, as yt-dlp reads it.
    pub fn to_netscape(&self) -> String {
        let mut out = String::from("# Netscape HTTP Cookie File\n");
        let flag = |b: bool| if b { "TRUE" } else { "FALSE" };
        for c in &self.cookies {
            out.push_str(&format!(
                "{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                c.domain,
                flag(c.include_subdomains),
                c.path,
                flag(c.secure),
                c.expires,
                c.name,
                c.value
            ));
        }
        out
    }
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Where the path of the chosen cookie file is remembered.
fn choice_file() -> Option<PathBuf> {
    crate::youtube::data_dir().map(|d| d.join("cookies-file.txt"))
}

/// The cookie file chosen in an earlier run, if it is still there.
pub fn saved_path() -> Option<PathBuf> {
    let text = std::fs::read_to_string(choice_file()?).ok()?;
    let path = PathBuf::from(text.trim());
    path.is_file().then_some(path)
}

/// Remembers (or, with `None`, forgets) the chosen cookie file.
pub fn save_path(path: Option<&Path>) -> Result<(), String> {
    let file = choice_file().ok_or("no folder to save settings in")?;
    match path {
        Some(p) => {
            if let Some(dir) = file.parent() {
                std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
            }
            std::fs::write(&file, p.to_string_lossy().as_bytes()).map_err(|e| e.to_string())
        }
        None => match std::fs::remove_file(&file) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
            _ => Ok(()),
        },
    }
}

fn ytdlp_copy_path() -> Option<PathBuf> {
    crate::youtube::data_dir().map(|d| d.join("yt-dlp-cookies.txt"))
}

/// Deletes the copy [`ytdlp_copy`] wrote, so no signed-in cookies stay behind once the user
/// has asked to forget the cookie file.
pub fn remove_ytdlp_copy() -> Result<(), String> {
    let Some(path) = ytdlp_copy_path() else { return Ok(()) };
    match std::fs::remove_file(&path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(format!("cannot delete {}: {e}", path.display())),
        _ => Ok(()),
    }
}

/// Writes the YouTube and Google cookies of `jar` to playr's own copy for yt-dlp and returns
/// its path, or `None` when there are none.
pub fn ytdlp_copy(jar: &CookieJar) -> Option<PathBuf> {
    let subset = jar.for_sites(&["youtube.com", "google.com"]);
    if subset.cookies.is_empty() {
        let _ = remove_ytdlp_copy();
        return None;
    }
    let path = ytdlp_copy_path()?;
    std::fs::create_dir_all(path.parent()?).ok()?;
    std::fs::write(&path, subset.to_netscape()).ok()?;
    Some(path)
}
