//! YouTube: recognising its addresses, asking yt-dlp for a video's streams and captions, and
//! opening the chosen streams for playback.
//!
//! YouTube changes how its pages hand out stream addresses every few weeks, so the extraction
//! is left to yt-dlp, which keeps up. A yt-dlp installed system-wide is often stale (and then
//! finds only storyboard images), so playr keeps its own copy, fetched from the official
//! release on first use, checked against the release's SHA-256 list, and updated when it fails.
//!
//! Only streams playr can decode are offered: H.264 video and AAC-LC audio, served as separate
//! fragmented MP4 files (see [`crate::dash`]). YouTube's HE-AAC streams are left out because
//! symphonia decodes them at half rate, and live broadcasts because they are HLS only.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::dash::{self, DashDemuxer, DashFile, MergedDemuxer};
use crate::demux::Demuxer;

/// Environment variable naming a yt-dlp executable to use instead of playr's own copy.
pub const YTDLP_ENV: &str = "PLAYR_YTDLP";
/// How long yt-dlp may take to answer before it is stopped.
const YTDLP_TIMEOUT: Duration = Duration::from_secs(120);
const RELEASES: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest/download/";

/// Whether `url` is a YouTube video address (watch page, short link, Shorts, live, embed).
pub fn is_youtube_url(url: &str) -> bool {
    video_id(url).is_some()
}

/// The 11-character video id of a YouTube address.
pub fn video_id(url: &str) -> Option<String> {
    let url = url.trim();
    let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"))?;
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
    let host = host.split(':').next().unwrap_or(host).to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    let (path, query) = path.split_once('?').unwrap_or((path, ""));
    let path = path.split('#').next().unwrap_or(path);
    let query = query.split('#').next().unwrap_or(query);
    let candidate = match host {
        "youtu.be" => path.split('/').next(),
        "youtube.com" | "m.youtube.com" | "music.youtube.com" | "youtube-nocookie.com" => {
            let mut parts = path.split('/');
            match parts.next() {
                Some("watch") => query.split('&').find_map(|kv| kv.strip_prefix("v=")),
                Some("shorts" | "live" | "embed" | "v") => parts.next(),
                _ => None,
            }
        }
        _ => None,
    }?;
    let valid = candidate.len() == 11 && candidate.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    valid.then(|| candidate.to_string())
}

#[derive(Clone, Debug, PartialEq)]
pub struct VideoFormat {
    pub id: String,
    pub url: String,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    /// Kilobits per second, 0 when unknown.
    pub bitrate: f64,
}

impl VideoFormat {
    /// "1080p", "720p60".
    pub fn label(&self) -> String {
        if self.fps > 31.0 { format!("{}p{}", self.height, self.fps.round() as u32) } else { format!("{}p", self.height) }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AudioFormat {
    pub id: String,
    pub url: String,
    pub language: Option<String>,
    /// "English (US) original (default)", "Polish".
    pub label: String,
    /// The track YouTube plays unless asked otherwise (the original language).
    pub default: bool,
    pub bitrate: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptionKind {
    /// Written by the uploader.
    Manual,
    /// Speech recognition in the video's own language.
    Auto,
    /// Speech recognition machine-translated into another language.
    Translated,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Caption {
    /// Language code as YouTube gives it ("en", "pl-orig", "zh-Hans").
    pub code: String,
    pub label: String,
    pub kind: CaptionKind,
    pub url: String,
}

/// What yt-dlp reported about a video, cut down to what playr can play.
#[derive(Clone, Debug, PartialEq)]
pub struct Video {
    pub id: String,
    pub title: String,
    pub channel: Option<String>,
    pub duration: Option<f64>,
    pub live: bool,
    /// Playable picture streams, highest first.
    pub video: Vec<VideoFormat>,
    /// One playable sound stream per audio language, the default one first.
    pub audio: Vec<AudioFormat>,
    /// Uploaded captions first, then automatic ones, then automatic translations.
    pub captions: Vec<Caption>,
    /// Request headers yt-dlp says the stream addresses expect.
    pub headers: Vec<(String, String)>,
}

/// The streams to play, by format id; `None` picks the default.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Choice {
    pub video: Option<String>,
    pub audio: Option<String>,
}

impl Video {
    /// The picture stream played unless another is chosen: the best one.
    pub fn default_video(&self) -> Option<&VideoFormat> {
        self.video.first()
    }

    pub fn default_audio(&self) -> Option<&AudioFormat> {
        self.audio.iter().find(|a| a.default).or(self.audio.first())
    }

    /// The streams `choice` selects, falling back to the defaults for anything it leaves open
    /// or names wrongly.
    pub fn pick(&self, choice: &Choice) -> (Option<&VideoFormat>, Option<&AudioFormat>) {
        let video = choice.video.as_ref().and_then(|id| self.video.iter().find(|f| &f.id == id));
        let audio = choice.audio.as_ref().and_then(|id| self.audio.iter().find(|f| &f.id == id));
        (video.or_else(|| self.default_video()), audio.or_else(|| self.default_audio()))
    }

    /// The caption track shown when captions are switched on without picking one: the
    /// uploaded track in the video's language, any uploaded track, then the automatic one.
    pub fn default_caption(&self, language: Option<&str>) -> Option<&Caption> {
        let manual = |c: &&Caption| c.kind == CaptionKind::Manual;
        let same_language = |c: &&Caption| language.is_some_and(|l| primary(&c.code) == primary(l));
        self.captions
            .iter()
            .filter(manual)
            .find(same_language)
            .or_else(|| self.captions.iter().find(manual))
            .or_else(|| self.captions.iter().find(|c| c.kind == CaptionKind::Auto))
    }
}

fn primary(code: &str) -> &str {
    code.split(['-', '_']).next().unwrap_or(code)
}

fn text(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string)
}

fn number(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(Value::as_f64)
}

/// "English (US) original (default), medium, DRC" → "English (US) original (default)".
fn audio_label(note: &str) -> String {
    let mut label = note.trim();
    while let Some((head, tail)) = label.rsplit_once(", ") {
        if matches!(tail.trim().to_ascii_lowercase().as_str(), "low" | "medium" | "high" | "ultralow" | "drc") {
            label = head;
        } else {
            break;
        }
    }
    if matches!(label.to_ascii_lowercase().as_str(), "low" | "medium" | "high" | "ultralow" | "drc") {
        return String::new();
    }
    label.to_string()
}

/// The caption download of one language entry, preferring SRT (which YouTube serves without
/// the word-by-word markup of its WebVTT).
fn caption_url(entries: &Value) -> Option<String> {
    let entries = entries.as_array()?;
    ["srt", "vtt"].iter().find_map(|ext| {
        entries.iter().find(|e| e.get("ext").and_then(Value::as_str) == Some(ext)).and_then(|e| text(e, "url"))
    })
}

fn caption_name(entries: &Value, code: &str) -> String {
    entries
        .as_array()
        .and_then(|a| a.iter().find_map(|e| text(e, "name")))
        .unwrap_or_else(|| code.to_string())
}

/// Turns the output of `yt-dlp -J` into a [`Video`].
pub fn parse_info(json: &str) -> Result<Video, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("yt-dlp gave unreadable output ({e})"))?;
    let formats = v.get("formats").and_then(Value::as_array).cloned().unwrap_or_default();
    let is = |f: &Value, key: &str, want: &str| f.get(key).and_then(Value::as_str) == Some(want);

    let mut video: Vec<VideoFormat> = Vec::new();
    for f in &formats {
        let avc = f.get("vcodec").and_then(Value::as_str).is_some_and(|c| c.starts_with("avc1"));
        if !(avc && is(f, "acodec", "none") && is(f, "protocol", "https") && is(f, "ext", "mp4")) {
            continue;
        }
        let (Some(id), Some(url), Some(height)) = (text(f, "format_id"), text(f, "url"), number(f, "height")) else {
            continue;
        };
        let format = VideoFormat {
            id,
            url,
            width: number(f, "width").unwrap_or(0.0) as u32,
            height: height as u32,
            fps: number(f, "fps").unwrap_or(0.0),
            bitrate: number(f, "tbr").unwrap_or(0.0),
        };
        match video.iter_mut().find(|g| g.height == format.height) {
            Some(g) if (format.fps, format.bitrate) > (g.fps, g.bitrate) => *g = format,
            Some(_) => {}
            None => video.push(format),
        }
    }
    video.sort_by(|a, b| b.height.cmp(&a.height).then(b.fps.total_cmp(&a.fps)));

    let mut audio: Vec<(AudioFormat, f64)> = Vec::new();
    for f in &formats {
        let id = text(f, "format_id").unwrap_or_default();
        if !(is(f, "acodec", "mp4a.40.2") && is(f, "vcodec", "none") && is(f, "protocol", "https")) || id.contains("drc") {
            continue;
        }
        let Some(url) = text(f, "url") else { continue };
        let language = text(f, "language");
        let note = text(f, "format_note").unwrap_or_default();
        let preference = number(f, "language_preference").unwrap_or(-1.0);
        let default = preference >= 10.0 || note.contains("default");
        let label = Some(audio_label(&note)).filter(|l| !l.is_empty()).or_else(|| language.clone()).unwrap_or_else(|| "Default".into());
        let format = AudioFormat { id, url, language, label, default, bitrate: number(f, "abr").or(number(f, "tbr")).unwrap_or(0.0) };
        match audio.iter_mut().find(|(g, _)| g.language == format.language) {
            Some((g, p)) if format.bitrate > g.bitrate => {
                *p = p.max(preference);
                *g = AudioFormat { default: g.default || format.default, ..format };
            }
            Some((g, p)) => {
                *p = p.max(preference);
                g.default |= format.default;
            }
            None => audio.push((format, preference)),
        }
    }
    if !audio.iter().any(|(a, _)| a.default) && audio.len() > 1 {
        // Without a marked default the most preferred language is the original.
        let best = audio.iter().enumerate().max_by(|x, y| x.1.1.total_cmp(&y.1.1)).map(|(i, _)| i);
        if let Some(i) = best.filter(|&i| audio[i].1 > -1.0) {
            audio[i].0.default = true;
        }
    }
    let mut audio: Vec<AudioFormat> = audio.into_iter().map(|(a, _)| a).collect();
    audio.sort_by(|a, b| b.default.cmp(&a.default).then_with(|| a.label.to_lowercase().cmp(&b.label.to_lowercase())));

    let mut captions = Vec::new();
    if let Some(subs) = v.get("subtitles").and_then(Value::as_object) {
        for (code, entries) in subs {
            if code == "live_chat" {
                continue;
            }
            if let Some(url) = caption_url(entries) {
                captions.push(Caption { code: code.clone(), label: caption_name(entries, code), kind: CaptionKind::Manual, url });
            }
        }
    }
    let language = text(&v, "language");
    if let Some(auto) = v.get("automatic_captions").and_then(Value::as_object) {
        let has_orig = auto.keys().any(|k| k.ends_with("-orig"));
        let mut translated = Vec::new();
        for (code, entries) in auto {
            let Some(url) = caption_url(entries) else { continue };
            let original = if has_orig { code.ends_with("-orig") } else { language.as_deref() == Some(code.as_str()) };
            if original {
                let name = caption_name(entries, code);
                let name = name.trim_end_matches(" (Original)").to_string();
                captions.push(Caption { code: code.clone(), label: format!("{name} (auto-generated)"), kind: CaptionKind::Auto, url });
            } else {
                translated.push(Caption { code: code.clone(), label: caption_name(entries, code), kind: CaptionKind::Translated, url });
            }
        }
        translated.sort_by_key(|c| c.label.to_lowercase());
        captions.extend(translated);
    }
    captions.sort_by_key(|c| match c.kind {
        CaptionKind::Manual => 0,
        CaptionKind::Auto => 1,
        CaptionKind::Translated => 2,
    });

    let headers = v
        .get("http_headers")
        .or_else(|| formats.iter().find_map(|f| f.get("http_headers")))
        .and_then(Value::as_object)
        .map(|h| h.iter().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))).collect())
        .unwrap_or_default();
    let live = v.get("is_live").and_then(Value::as_bool).unwrap_or(false);
    Ok(Video {
        id: text(&v, "id").unwrap_or_default(),
        title: text(&v, "title").unwrap_or_else(|| "YouTube video".into()),
        channel: text(&v, "channel").or_else(|| text(&v, "uploader")),
        duration: number(&v, "duration").filter(|d| *d > 0.0),
        live,
        video,
        audio,
        captions,
        headers,
    })
}

/// Why a video that yt-dlp could read still has nothing playr can play, or `None` when it has.
pub fn unplayable_reason(video: &Video) -> Option<String> {
    if video.live {
        return Some("YouTube live streams are not supported (YouTube sends them only as HLS)".into());
    }
    if video.video.is_empty() && video.audio.is_empty() {
        return Some("YouTube offers no H.264 or AAC stream for this video".into());
    }
    None
}

/// The streams a [`Video`] was opened with.
pub struct Opened {
    pub demuxer: Box<dyn Demuxer>,
    pub choice: Choice,
}

/// Connects to the chosen streams and returns them as one demuxer. With `audio_only` the
/// picture is not downloaded at all, so the video plays like a radio station; a video without
/// any sound stream is still opened with its picture.
pub fn open(video: &Video, choice: &Choice, audio_only: bool, abort: Arc<AtomicBool>) -> Result<Opened, String> {
    let (v, a) = video.pick(choice);
    let v = v.filter(|_| !(audio_only && a.is_some()));
    // The two streams come from different servers; each costs a few round trips to open.
    let open_one = |url: &str| -> Result<Box<dyn Demuxer + Send>, String> {
        Ok(Box::new(DashDemuxer::open(DashFile::connect(url, &video.headers)?, abort.clone())?))
    };
    let (vd, ad) = std::thread::scope(|s| {
        let audio = a.map(|a| s.spawn(|| open_one(&a.url)));
        let vd = v.map(|v| open_one(&v.url));
        let ad = audio.map(|h| h.join().unwrap_or_else(|_| Err("internal error while opening the sound".into())));
        (vd, ad)
    });
    let (vd, ad) = (vd.transpose()?, ad.transpose()?);
    let demuxer: Box<dyn Demuxer> = match (vd, ad) {
        (Some(vd), Some(ad)) => Box::new(MergedDemuxer::new(vd, ad)),
        (Some(only), None) | (None, Some(only)) => only as Box<dyn Demuxer>,
        (None, None) => return Err(unplayable_reason(video).unwrap_or_else(|| "no playable stream".into())),
    };
    Ok(Opened { demuxer, choice: Choice { video: v.map(|f| f.id.clone()), audio: a.map(|f| f.id.clone()) } })
}

/// Downloads the caption track and parses it.
pub fn fetch_caption(caption: &Caption, headers: &[(String, String)]) -> Result<crate::subtitles::Subtitles, String> {
    let text = dash::get_text(&caption.url, headers, 16 << 20)?;
    crate::subtitles::parse(&text)
}

/// Where playr keeps its own yt-dlp.
pub fn managed_ytdlp_path() -> Option<PathBuf> {
    let name = if cfg!(windows) { "yt-dlp.exe" } else { "yt-dlp" };
    data_dir().map(|d| d.join(name))
}

/// playr's own folder for downloaded tools and remembered settings.
pub fn data_dir() -> Option<PathBuf> {
    let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    if cfg!(windows) {
        env("LOCALAPPDATA").map(|d| d.join("playr"))
    } else if cfg!(target_os = "macos") {
        env("HOME").map(|h| h.join("Library/Application Support/playr"))
    } else {
        env("XDG_DATA_HOME").or_else(|| env("HOME").map(|h| h.join(".local/share"))).map(|d| d.join("playr"))
    }
}

/// The name of the standalone yt-dlp build for this platform on the release page.
pub fn release_asset() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => Some("yt-dlp.exe"),
        ("windows", "x86") => Some("yt-dlp_x86.exe"),
        ("windows", "aarch64") => Some("yt-dlp_arm64.exe"),
        ("linux", "x86_64") => Some("yt-dlp_linux"),
        ("linux", "aarch64") => Some("yt-dlp_linux_aarch64"),
        ("macos", _) => Some("yt-dlp_macos"),
        _ => None,
    }
}

/// The SHA-256 the release's `SHA2-256SUMS` file lists for `asset`.
pub fn expected_sha256(sums: &str, asset: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (hash, name) = line.trim().split_once(char::is_whitespace)?;
        let name = name.trim().trim_start_matches('*');
        (name == asset && hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit())).then(|| hash.to_ascii_lowercase())
    })
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// Fetches the current yt-dlp release into `dest`, verified against its published checksum.
pub fn download_ytdlp(dest: &Path) -> Result<(), String> {
    let asset = release_asset().ok_or("there is no yt-dlp build for this system; install yt-dlp and set PLAYR_YTDLP")?;
    download_ytdlp_from(RELEASES, asset, dest)
}

/// [`download_ytdlp`] from the release folder `base` (ending in `/`), for the build `asset`.
pub fn download_ytdlp_from(base: &str, asset: &str, dest: &Path) -> Result<(), String> {
    let sums = dash::get_text(&format!("{base}SHA2-256SUMS"), &[], 1 << 20)?;
    let expected = expected_sha256(&sums, asset).ok_or("the yt-dlp release lists no checksum for this system")?;
    let resp = dash::agent()
        .get(&format!("{base}{asset}"))
        .call()
        .map_err(|e| format!("cannot download yt-dlp: {e}"))?;
    let mut bytes = Vec::new();
    resp.into_body()
        .into_reader()
        .take(256 << 20)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("cannot download yt-dlp: {e}"))?;
    if sha256_hex(&bytes) != expected {
        return Err("the downloaded yt-dlp does not match its published checksum".into());
    }
    let dir = dest.parent().ok_or("bad yt-dlp path")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let partial = dest.with_extension("part");
    std::fs::write(&partial, &bytes).map_err(|e| format!("cannot save yt-dlp: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&partial, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    }
    std::fs::rename(&partial, dest).map_err(|e| format!("cannot save yt-dlp: {e}"))
}

fn on_path(name: &str) -> Option<PathBuf> {
    let exts: &[&str] = if cfg!(windows) { &[".exe", ".cmd", ".bat"] } else { &[""] };
    std::env::split_paths(&std::env::var_os("PATH")?).find_map(|dir| {
        exts.iter().map(|ext| dir.join(format!("{name}{ext}"))).find(|p| p.is_file())
    })
}

/// Runs a command without a console window, giving up after `timeout` or when `abort` is set.
/// Returns its standard output, or its last error line when it fails.
pub fn run_quiet(program: &Path, args: &[&str], timeout: Duration, abort: &AtomicBool) -> Result<String, String> {
    let mut cmd = Command::new(program);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd.spawn().map_err(|e| format!("cannot run {}: {e}", program.display()))?;
    let mut stdout = child.stdout.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");
    let out = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout.read_to_string(&mut s);
        s
    });
    let err = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            break status;
        }
        if abort.load(Ordering::Relaxed) || started.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(if abort.load(Ordering::Relaxed) { "cancelled".into() } else { "yt-dlp took too long to answer".into() });
        }
        std::thread::sleep(Duration::from_millis(30));
    };
    let stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
    if status.success() {
        Ok(stdout)
    } else {
        let line = stderr.lines().rev().find(|l| l.contains("ERROR")).or(stderr.lines().rev().find(|l| !l.trim().is_empty()));
        Err(line.map_or_else(|| format!("yt-dlp failed ({status})"), |l| l.trim().trim_start_matches("ERROR: ").to_string()))
    }
}

fn ask(exe: &Path, url: &str, js_runtime: Option<&str>, cookies: Option<&Path>, abort: &AtomicBool) -> Result<Video, String> {
    let mut args = vec!["-J", "--no-playlist", "--no-warnings", "--no-progress", "--ignore-no-formats-error"];
    if let Some(runtime) = js_runtime {
        args.extend(["--js-runtimes", runtime]);
    }
    let cookies = cookies.map(|c| c.to_string_lossy().into_owned());
    if let Some(c) = &cookies {
        args.extend(["--cookies", c.as_str()]);
    }
    args.extend(["--", url]);
    parse_info(&run_quiet(exe, &args, YTDLP_TIMEOUT, abort)?)
}

/// How far a search result's length may be from the length Spotify gives before it counts as a
/// different recording (a live version, a mix, a compilation).
pub fn duration_tolerance(expected: f64) -> f64 {
    (expected * 0.1).max(15.0)
}

/// Which search result to play: the first (YouTube ranks by relevance) whose length is within
/// [`duration_tolerance`] of `expected`, else the first one. `results` holds each result's
/// length, if known.
pub fn pick_search_result(results: &[Option<f64>], expected: Option<f64>) -> Option<usize> {
    if results.is_empty() {
        return None;
    }
    let Some(expected) = expected else { return Some(0) };
    let tolerance = duration_tolerance(expected);
    Some(results.iter().position(|d| d.is_some_and(|d| (d - expected).abs() <= tolerance)).unwrap_or(0))
}

/// Ids and lengths of the videos in a `yt-dlp -J --flat-playlist ytsearchN:` answer.
pub fn parse_search(json: &str) -> Result<Vec<(String, Option<f64>)>, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("yt-dlp gave unreadable output ({e})"))?;
    Ok(v.get("entries")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|e| Some((text(e, "id").filter(|id| id.len() == 11)?, number(e, "duration"))))
                .collect()
        })
        .unwrap_or_default())
}

/// Finds `query` on YouTube and returns the watch address of the best match for a recording
/// `expected` seconds long.
pub fn search(queries: &[String], expected: Option<f64>, abort: &AtomicBool, progress: &dyn Fn(&str)) -> Result<String, String> {
    let exe = ytdlp_candidates(progress).into_iter().next().map(|(exe, _)| exe).ok_or("searching YouTube needs yt-dlp, which could not be downloaded")?;
    let mut fallback = None;
    for query in queries {
        let search = format!("ytsearch5:{query}");
        let args = ["-J", "--flat-playlist", "--no-warnings", "--no-progress", "--", search.as_str()];
        let results = match run_quiet(&exe, &args, YTDLP_TIMEOUT, abort).and_then(|json| parse_search(&json)) {
            Ok(results) => results,
            Err(_) if fallback.is_some() && !abort.load(Ordering::Relaxed) => break,
            Err(e) => return Err(e),
        };
        let lengths: Vec<Option<f64>> = results.iter().map(|r| r.1).collect();
        let Some(best) = pick_search_result(&lengths, expected) else { continue };
        let url = format!("https://www.youtube.com/watch?v={}", results[best].0);
        let fits = match (expected, lengths[best]) {
            (Some(want), Some(got)) => (got - want).abs() <= duration_tolerance(want),
            _ => true,
        };
        if fits {
            return Ok(url);
        }
        fallback.get_or_insert(url);
    }
    let first = queries.first().map_or("", String::as_str);
    fallback.ok_or_else(|| format!("nothing found on YouTube for \u{201c}{first}\u{201d}"))
}

/// The yt-dlp executables to try, best first, each flagged when it is playr's own copy (which
/// may be updated). Downloads playr's copy if there is none yet.
fn ytdlp_candidates(progress: &dyn Fn(&str)) -> Vec<(PathBuf, bool)> {
    let mut candidates: Vec<(PathBuf, bool)> = Vec::new();
    if let Some(p) = std::env::var_os(YTDLP_ENV).filter(|p| !p.is_empty()) {
        candidates.push((PathBuf::from(p), false));
        return candidates;
    }
    if let Some(managed) = managed_ytdlp_path() {
        if !managed.is_file() {
            progress("Downloading yt-dlp, which playr uses to read YouTube (one time)…");
            if let Err(e) = download_ytdlp(&managed) {
                progress(&format!("Could not download yt-dlp: {e}"));
            }
        }
        if managed.is_file() {
            candidates.push((managed, true));
        }
    }
    if let Some(system) = on_path("yt-dlp") {
        candidates.push((system, false));
    }
    candidates
}

/// Set once playr's yt-dlp has been updated in this run; one update is enough.
static UPDATED: AtomicBool = AtomicBool::new(false);

/// A JavaScript runtime yt-dlp can use for YouTube's signature puzzles, if one is installed.
fn js_runtime() -> Option<&'static str> {
    ["deno", "node", "bun"].into_iter().find(|name| on_path(name).is_some())
}

/// The (JavaScript runtime, cookie file) pairs to try yt-dlp with, in order: plain first, then
/// with the runtime, then with the cookies (and the runtime, which signed-in requests need).
pub fn attempts<'a>(runtime: Option<&'a str>, cookies: Option<&'a Path>) -> Vec<(Option<&'a str>, Option<&'a Path>)> {
    let mut list = vec![(None, None)];
    if runtime.is_some() {
        list.push((runtime, None));
    }
    if cookies.is_some() {
        list.push((runtime, cookies));
    }
    list
}

/// Asks yt-dlp about `url`, sending the cookies in the Netscape file `cookies` if given.
/// `progress` is told what is going on while it takes long.
pub fn resolve(url: &str, cookies: Option<&Path>, abort: &AtomicBool, progress: &dyn Fn(&str)) -> Result<Video, String> {
    let candidates = ytdlp_candidates(progress);
    if candidates.is_empty() {
        return Err("playing YouTube needs yt-dlp, which could not be downloaded; install it or set PLAYR_YTDLP".into());
    }
    // Signed-in requests make YouTube demand a JavaScript puzzle be solved before it hands out
    // stream addresses, so cookies are only a fallback for videos that refuse anonymous
    // viewers (age limits, "confirm you are not a bot"), and go together with a runtime.
    let attempts = attempts(js_runtime(), cookies);
    let mut first_error = None;
    for (exe, managed) in candidates {
        loop {
            for &(runtime, cookies) in &attempts {
                progress(if cookies.is_some() {
                    "Asking YouTube for the video's streams (with your cookies)…"
                } else {
                    "Asking YouTube for the video's streams…"
                });
                let attempt = ask(&exe, url, runtime, cookies, abort);
                if abort.load(Ordering::Relaxed) {
                    return Err("cancelled".into());
                }
                let failure = match attempt {
                    Ok(video) => match unplayable_reason(&video) {
                        None => return Ok(video),
                        Some(why) if video.live => return Err(why),
                        Some(why) => why,
                    },
                    Err(e) => e,
                };
                first_error.get_or_insert(failure);
            }
            // YouTube may have changed under this yt-dlp: update it (once per run) and retry.
            if !managed || UPDATED.swap(true, Ordering::Relaxed) {
                break;
            }
            progress("Updating yt-dlp…");
            if run_quiet(&exe, &["-U"], YTDLP_TIMEOUT, abort).is_err() && download_ytdlp(&exe).is_err() {
                break;
            }
        }
    }
    Err(first_error.unwrap_or_else(|| "yt-dlp failed".into()))
}
