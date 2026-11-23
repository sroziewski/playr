//! Video pages of other sites: `playr::sites` and a `Player` opening a page, without the network.
//!
//! A page is served as `text/html` from 127.0.0.1, yt-dlp is a copy of this test binary (see
//! `act_as_fake_ytdlp`) answering `-J` from a canned script, and the formats it names are the
//! fixture files served with HTTP ranges. Tests that need the fake run in a fresh copy of this
//! binary, because `PLAYR_YTDLP` belongs to the whole process.

mod common;
mod own_ip;

use std::ffi::OsString;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::http::{RadioServer, STATION};
use common::range_http::{Mode, RangeServer};
use common::{finish, fixture_bytes};
use playr::player::{Player, State, Status};
use playr::sites::{Format, Page, is_html, parse_page, unplayable_reason};
use playr::youtube::Choice;
use serde_json::{Value, json};

const CHILD: &str = "PLAYR_TEST_CHILD";
const DIR: &str = "PLAYR_TEST_DIR";

fn format(id: &str, height: Option<u32>) -> Format {
    Format { id: id.into(), url: format!("https://cdn.example/{id}.mp4"), ext: Some("mp4".into()), width: None, height, tbr: None, hls: false, audio: None }
}

fn page(formats: Vec<Format>) -> Page {
    Page { site: "TestSite".into(), title: "T".into(), duration: None, headers: vec![], formats, only_unplayable: false }
}

#[test]
fn parse_page_keeps_single_file_and_muxed_hls_formats_with_picture_and_sound() {
    let answer = json!({
        "extractor_key": "TestSite", "extractor": "testsite", "title": "  A test video ", "duration": 1234.5,
        "http_headers": { "Referer": "https://site.example/watch/1", "User-Agent": "TestAgent/1.0", "X-Number": 5 },
        "formats": [
            { "format_id": "hls-720", "url": "https://cdn.example/720.m3u8", "protocol": "m3u8_native", "vcodec": "avc1", "acodec": "mp4a" },
            { "format_id": "v-only", "url": "https://cdn.example/v.mp4", "protocol": "https", "vcodec": "avc1", "acodec": "none" },
            { "format_id": "a-only", "url": "https://cdn.example/a.m4a", "protocol": "https", "vcodec": "none", "acodec": "mp4a" },
            { "url": "https://cdn.example/noid.mp4", "protocol": "https" },
            { "format_id": "relative", "url": "/files/r.mp4", "protocol": "https" },
            { "format_id": "no-protocol", "url": "https://cdn.example/np.mp4" },
            { "format_id": "low", "url": "http://cdn.example/low.mp4", "protocol": "http", "ext": "mp4", "width": 640, "height": 360, "tbr": 600.5 },
            { "format_id": "high", "url": "https://cdn.example/high.mp4", "protocol": "https", "ext": "mp4", "vcodec": "avc1.64001f", "acodec": "mp4a.40.2",
              "width": 0, "height": 720, "tbr": 0 },
        ],
    });

    let parsed = parse_page(&answer.to_string());

    let expected = Page {
        site: "TestSite".into(),
        title: "A test video".into(),
        duration: Some(1234.5),
        headers: vec![("Referer".into(), "https://site.example/watch/1".into()), ("User-Agent".into(), "TestAgent/1.0".into())],
        formats: vec![
            Format { id: "hls-720".into(), url: "https://cdn.example/720.m3u8".into(), ext: None, width: None, height: None, tbr: None, hls: true, audio: None },
            Format { id: "low".into(), url: "http://cdn.example/low.mp4".into(), ext: Some("mp4".into()), width: Some(640), height: Some(360), tbr: Some(600.5), hls: false, audio: None },
            Format { id: "high".into(), url: "https://cdn.example/high.mp4".into(), ext: Some("mp4".into()), width: None, height: Some(720), tbr: None, hls: false, audio: None },
        ],
        only_unplayable: false,
    };
    assert_eq!(parsed, Ok(expected));
}

#[test]
fn parse_page_keeps_hls_formats_one_per_height_preferring_a_single_file_then_the_higher_bitrate() {
    let hls = |id: &str, vcodec: &str, acodec: &str, height: u32, tbr: f64| {
        json!({ "format_id": id, "url": format!("https://cdn.example/{id}.m3u8"), "protocol": "m3u8_native", "ext": "mp4",
                "vcodec": vcodec, "acodec": acodec, "width": height * 16 / 9, "height": height, "tbr": tbr })
    };
    let file = |id: &str, height: u32, tbr: f64| {
        json!({ "format_id": id, "url": format!("https://cdn.example/{id}.mp4"), "protocol": "https", "ext": "mp4",
                "vcodec": "avc1.4d401e", "acodec": "mp4a.40.2", "height": height, "tbr": tbr })
    };
    let sound = |id: &str, abr: Option<f64>, tbr: Option<f64>, url: &str| {
        json!({ "format_id": id, "url": url, "protocol": "m3u8", "vcodec": "none", "acodec": "mp4a.40.2", "abr": abr, "tbr": tbr })
    };
    let answer = json!({ "extractor_key": "TestSite", "title": "Live show", "formats": [
        hls("hls-360", "avc1.4d401e", "mp4a.40.2", 360, 800.0),
        // A single file of the same height replaces the HLS one, even at a lower bitrate.
        file("http-360", 360, 500.0),
        // Picture only: joined with the richest HLS sound.
        hls("hls-720", "avc1.64001f", "none", 720, 2500.0),
        // Same height, both HLS, less bitrate: does not replace hls-720.
        hls("hls-720b", "avc1.64001f", "mp4a.40.2", 720, 1800.0),
        hls("hls-1080a", "avc1.640028", "mp4a.40.2", 1080, 4000.0),
        // Same height, both HLS, more bitrate: replaces hls-1080a.
        hls("hls-1080b", "avc1.640028", "mp4a.40.2", 1080, 5000.0),
        file("http-480", 480, 300.0),
        // An HLS format does not replace a single file of its height.
        hls("hls-480", "avc1.4d401e", "mp4a.40.2", 480, 1200.0),
        hls("hls-vp9", "vp09.00.40.08", "mp4a.40.2", 1440, 9000.0),
        hls("hls-ac3", "avc1.4d401e", "ac-3", 240, 400.0),
        sound("hls-audio-48", Some(48.0), Some(50.0), "https://cdn.example/a48.m3u8"),
        sound("hls-audio-128", Some(128.0), None, "https://cdn.example/a128.m3u8"),
        sound("hls-audio-tbr", None, Some(96.0), "https://cdn.example/a96.m3u8"),
        sound("hls-audio-relative", Some(999.0), None, "/a999.m3u8"),
    ]});

    let parsed = parse_page(&answer.to_string()).map(|p| (p.formats, p.only_unplayable));

    let f = |id: &str, ext: &str, width: Option<u32>, height: u32, tbr: f64, hls: bool, audio: Option<&str>| Format {
        id: id.into(),
        url: format!("https://cdn.example/{id}.{ext}"),
        ext: Some("mp4".into()),
        width,
        height: Some(height),
        tbr: Some(tbr),
        hls,
        audio: audio.map(str::to_string),
    };
    let expected = vec![
        f("http-360", "mp4", None, 360, 500.0, false, None),
        f("hls-720", "m3u8", Some(1280), 720, 2500.0, true, Some("https://cdn.example/a128.m3u8")),
        f("hls-1080b", "m3u8", Some(1920), 1080, 5000.0, true, None),
        f("http-480", "mp4", None, 480, 300.0, false, None),
    ];
    assert_eq!(parsed, Ok((expected, false)));
}

#[test]
fn a_picture_only_hls_format_takes_the_hls_sound_of_the_highest_rate_or_is_dropped_without_one() {
    let video = json!({ "format_id": "v", "url": "https://cdn.example/v.m3u8", "protocol": "m3u8_native", "vcodec": "avc1.64001f", "acodec": "none", "height": 720 });
    // A rate yt-dlp does not know is left out of its answer (it drops null fields from -J).
    let sound = |id: &str, abr: Option<f64>, tbr: Option<f64>, protocol: &str| {
        let mut f = json!({ "format_id": id, "url": format!("https://cdn.example/{id}.m3u8"), "protocol": protocol, "vcodec": "none", "acodec": "mp4a.40.2" });
        if let Some(abr) = abr {
            f["abr"] = json!(abr);
        }
        if let Some(tbr) = tbr {
            f["tbr"] = json!(tbr);
        }
        f
    };
    let cases = [
        ("abr decides", vec![sound("a64", Some(64.0), Some(300.0), "m3u8"), sound("a96", Some(96.0), None, "m3u8")], Some("https://cdn.example/a96.m3u8"), false),
        ("tbr when there is no abr", vec![sound("t200", None, Some(200.0), "m3u8_native"), sound("a150", Some(150.0), None, "m3u8")], Some("https://cdn.example/t200.m3u8"), false),
        ("single-file sound does not count", vec![sound("https-sound", Some(128.0), None, "https")], None, true),
        ("no sound at all", vec![], None, true),
    ];
    let mut bad = Vec::new();
    for (case, sounds, audio, only_unplayable) in cases {
        let formats: Vec<Value> = std::iter::once(video.clone()).chain(sounds).collect();
        let parsed = parse_page(&json!({ "extractor_key": "TestSite", "formats": formats }).to_string());
        let got = parsed.map(|p| (p.formats.iter().map(|f| (f.id.clone(), f.hls, f.audio.clone())).collect::<Vec<_>>(), p.only_unplayable));
        let expected_formats = audio.map(|a| vec![("v".to_string(), true, Some(a.to_string()))]).unwrap_or_default();
        expect!(bad, got == Ok((expected_formats.clone(), only_unplayable)), "{case}: {got:?}");
    }
    finish(bad);
}

#[test]
fn parse_page_names_the_site_and_title_and_tells_an_empty_page_from_an_unplayable_one() {
    let progressive = json!({ "format_id": "f", "url": "https://cdn.example/f.mp4", "protocol": "https" });
    let hls = json!({ "format_id": "h", "url": "https://cdn.example/h.m3u8", "protocol": "m3u8" });
    let cases = [
        ("extractor_key wins", json!({ "extractor_key": "Key", "extractor": "name", "formats": [progressive] }), "Key", "Video", None, 1, false),
        ("blank extractor_key", json!({ "extractor_key": " ", "extractor": "name", "title": "", "formats": [progressive] }), "name", "Video", None, 1, false),
        ("no extractor", json!({ "title": "Clip", "duration": 0, "formats": [progressive] }), "Web", "Clip", None, 1, false),
        ("negative duration", json!({ "duration": -3, "formats": [progressive] }), "Web", "Video", None, 1, false),
        ("only HLS, which plays", json!({ "extractor_key": "Key", "duration": 7, "formats": [hls] }), "Key", "Video", Some(7.0), 1, false),
        ("no formats, the page itself is the stream", json!({ "format_id": "self", "url": "https://cdn.example/s.mp4", "protocol": "https" }), "Web", "Video", None, 1, false),
        ("no formats and no stream", json!({ "extractor": "generic", "title": "Nothing" }), "generic", "Nothing", None, 0, false),
        ("empty formats", json!({ "formats": [] }), "Web", "Video", None, 0, false),
        (
            "only storyboards (mhtml, no picture or sound track)",
            json!({ "extractor_key": "Key", "formats": [
                { "format_id": "sb0", "url": "https://cdn.example/sb0.mhtml", "protocol": "mhtml", "ext": "mhtml", "vcodec": "none", "acodec": "none" },
            ]}),
            "Key", "Video", None, 0, false,
        ),
        (
            "an mhtml stream that claims a picture",
            json!({ "formats": [{ "format_id": "m", "url": "https://cdn.example/m.mhtml", "protocol": "mhtml", "ext": "mhtml", "vcodec": "avc1" }] }),
            "Web", "Video", None, 0, false,
        ),
        (
            "a stream without picture or sound",
            json!({ "formats": [{ "format_id": "x", "url": "https://cdn.example/x.bin", "protocol": "https", "vcodec": "none", "acodec": "none" }] }),
            "Web", "Video", None, 0, false,
        ),
        (
            "a split stream without a format_id",
            json!({ "formats": [{ "url": "https://cdn.example/v.mp4", "protocol": "https", "vcodec": "avc1", "acodec": "none" }] }),
            "Web", "Video", None, 0, false,
        ),
        (
            "a split stream with a format_id",
            json!({ "formats": [{ "format_id": "v", "url": "https://cdn.example/v.mp4", "protocol": "https", "vcodec": "avc1", "acodec": "none" }] }),
            "Web", "Video", None, 0, true,
        ),
    ];
    let mut bad = Vec::new();
    for (case, answer, site, title, duration, count, only_unplayable) in cases {
        match parse_page(&answer.to_string()) {
            Ok(p) => {
                let got = (p.site.as_str(), p.title.as_str(), p.duration, p.formats.len(), p.only_unplayable);
                expect!(bad, got == (site, title, duration, count, only_unplayable), "{case}: {got:?}");
            }
            Err(e) => bad.push(format!("{case}: {e}")),
        }
    }
    finish(bad);
}

#[test]
fn an_unreadable_answer_is_an_error_naming_it() {
    let parsed = parse_page("{\"title\": ");

    let prefix = "yt-dlp sent an unreadable answer (";
    assert!(parsed.as_ref().is_err_and(|e| e.starts_with(prefix) && e.ends_with(')') && e.len() > prefix.len() + 1), "{parsed:?}");
}

#[test]
fn the_reason_a_page_cannot_be_played_depends_on_what_it_offers() {
    let unplayable = Page { only_unplayable: true, ..page(vec![]) };
    let cases = [
        ("playable", page(vec![format("f", None)]), None),
        ("only split streams or undecodable formats", unplayable, Some("TestSite offers this video only as separate picture and sound files or in formats playr cannot decode")),
        ("nothing at all", page(vec![]), Some("the page has no video that yt-dlp can find")),
    ];
    let mut bad = Vec::new();
    for (case, p, expected) in cases {
        let actual = unplayable_reason(&p);
        expect!(bad, actual.as_deref() == expected, "{case}: {actual:?}");
    }
    finish(bad);
}

#[test]
fn the_default_is_the_best_format_up_to_1080p_and_a_pick_falls_back_to_it() {
    let ladder = page(vec![format("240", Some(240)), format("720", Some(720)), format("1080", Some(1080)), format("2160", Some(2160))]);
    let unknown_on_top = page(vec![format("sd", None), format("2160", Some(2160))]);
    let all_tall = page(vec![format("1440", Some(1440)), format("2160", Some(2160))]);
    let cases = [
        ("ladder", &ladder, None, Some("1080")),
        ("unknown height counts as fitting", &unknown_on_top, None, Some("sd")),
        ("all taller than 1080p: the smallest", &all_tall, None, Some("1440")),
        ("no formats", &page(vec![]), None, None),
        ("a pick that exists", &ladder, Some("720"), Some("720")),
        ("a pick taller than the default", &ladder, Some("2160"), Some("2160")),
        ("a pick that does not exist", &ladder, Some("4320"), Some("1080")),
    ];
    let mut bad = Vec::new();
    for (case, p, pick, expected) in cases {
        let actual = match pick {
            None => p.default_format(),
            Some(id) => p.pick(Some(id)),
        };
        expect!(bad, actual.map(|f| f.id.as_str()) == expected, "{case}: {:?}", actual.map(|f| &f.id));
        if pick.is_none() {
            expect!(bad, p.pick(None).map(|f| f.id.as_str()) == expected, "{case}: pick(None) differs from the default");
        }
    }
    finish(bad);
}

#[test]
fn yt_dlp_is_given_the_page_address_upgraded_to_https() {
    let cases = [
        ("http with a query", "http://site.example/v?x=1", "https://site.example/v?x=1"),
        ("the plain-http port is dropped", "http://site.example:80/v", "https://site.example/v"),
        ("another port is kept", "http://site.example:8080/v", "https://site.example:8080/v"),
        ("https as it is", "https://site.example/v", "https://site.example/v"),
        ("loopback as it is", "http://127.0.0.1:8000/watch/1", "http://127.0.0.1:8000/watch/1"),
        ("spaces trimmed", "  http://site.example/v ", "https://site.example/v"),
    ];
    let mut bad = Vec::new();
    for (case, url, expected) in cases {
        let actual = playr::sites::ytdlp_address(url);
        expect!(bad, actual == expected, "{case}: {actual:?}, expected {expected:?}");
    }
    finish(bad);
}

#[test]
fn a_resolved_page_is_reused_for_thirty_minutes() {
    let cases = [
        (Duration::ZERO, true),
        (Duration::from_secs(29 * 60 + 59), true),
        (Duration::from_secs(30 * 60) - Duration::from_millis(1), true),
        (Duration::from_secs(30 * 60), false),
        (Duration::from_secs(3 * 3600), false),
    ];
    let actual: Vec<(Duration, bool)> = cases.iter().map(|&(age, _)| (age, playr::sites::cache_fresh(age))).collect();

    assert_eq!(playr::sites::CACHE_FOR, Duration::from_secs(1800));
    assert_eq!(actual, cases);
}

#[test]
fn a_format_is_labelled_by_its_height_or_else_its_name() {
    assert_eq!((format("hd", Some(720)).label(), format("high", None).label()), ("720p".to_string(), "high".to_string()));
}

#[test]
fn only_html_pages_are_pages() {
    let cases = [
        (Some("text/html"), true),
        (Some("application/xhtml+xml"), true),
        (Some("text/plain"), false),
        (Some("audio/mpeg"), false),
        (Some("video/mp4"), false),
        (None, false),
    ];
    let actual: Vec<(Option<&str>, bool)> = cases.iter().map(|&(m, _)| (m, is_html(m))).collect();

    assert_eq!(actual, cases);
}

fn fresh_dir(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("playr-sites").join(test);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create test dir");
    dir
}

fn is_child() -> bool {
    std::env::var_os(CHILD).is_some()
}

fn run_child(test: &str, env: Vec<(&str, OsString)>) {
    let out = Command::new(std::env::current_exe().expect("test binary"))
        .args([test, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .env_remove("PLAYR_YTDLP")
        .envs(env)
        .output()
        .expect("start the child test");
    let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "{test} failed in the child process:\n{stdout}\n{stderr}");
    assert!(stdout.contains("test result: ok. 1 passed"), "{test} did not run in the child process:\n{stdout}");
}

fn test_dir() -> PathBuf {
    PathBuf::from(std::env::var_os(DIR).expect("PLAYR_TEST_DIR"))
}

/// A private data folder, a `PATH` without JavaScript runtimes, and a fake yt-dlp answering
/// `-J` for each page in `pages` (`{url: answer}`) and failing for any other.
#[cfg(any(windows, target_os = "linux"))]
fn fake_env(dir: &Path, pages: &Value) -> Vec<(&'static str, OsString)> {
    let fake = dir.join("fake");
    let bin = dir.join("bin");
    std::fs::create_dir_all(&fake).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    let exe = fake.join(if cfg!(windows) { "yt-dlp.exe" } else { "yt-dlp" });
    std::fs::copy(std::env::current_exe().unwrap(), &exe).expect("copy the test binary");
    std::fs::write(fake.join("fake.json"), json!({ "pages": pages }).to_string()).unwrap();
    let data = dir.join("appdata");
    vec![
        (DIR, dir.as_os_str().to_owned()),
        ("LOCALAPPDATA", data.as_os_str().to_owned()),
        ("XDG_DATA_HOME", data.into_os_string()),
        ("PATH", bin.into_os_string()),
        ("PLAYR_YTDLP", exe.into_os_string()),
    ]
}

#[cfg(windows)]
#[used]
#[unsafe(link_section = ".CRT$XCU")]
static FAKE_YTDLP_HOOK: extern "C" fn() = act_as_fake_ytdlp;

#[cfg(target_os = "linux")]
#[used]
#[unsafe(link_section = ".init_array")]
static FAKE_YTDLP_HOOK: extern "C" fn() = act_as_fake_ytdlp;

/// Runs before the test harness: a copy named yt-dlp with a `fake.json` beside it logs its
/// arguments to `calls.log`, answers as yt-dlp and exits.
#[cfg(any(windows, target_os = "linux"))]
extern "C" fn act_as_fake_ytdlp() {
    let Ok(exe) = std::env::current_exe() else { return };
    if exe.file_stem().and_then(|s| s.to_str()) != Some("yt-dlp") {
        return;
    }
    let dir = exe.parent().expect("folder").to_path_buf();
    let Ok(script) = std::fs::read_to_string(dir.join("fake.json")) else { return };
    let script: Value = serde_json::from_str(&script).expect("fake.json");
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut log = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("calls.log")).unwrap();
    writeln!(log, "{}", args.join(" ")).unwrap();
    let last = args.last().cloned().unwrap_or_default();
    match script["pages"].get(&last) {
        Some(answer) => {
            println!("{answer}");
            std::process::exit(0);
        }
        None => {
            eprintln!("ERROR: [generic] Unsupported URL: {last}");
            std::process::exit(1);
        }
    }
}

fn calls() -> Vec<String> {
    std::fs::read_to_string(test_dir().join("fake").join("calls.log"))
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

fn page_call(url: &str) -> String {
    format!("-J --no-playlist --no-warnings --no-progress -- {url}")
}

/// Serves an HTML page for any path, as a video site does.
struct HtmlServer {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<String>>>,
}

impl HtmlServer {
    fn start() -> Self {
        let listener = {
            let _ports = common::http::PORTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            TcpListener::bind("127.0.0.1:0").expect("bind the page server")
        };
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { continue };
                let seen = seen.clone();
                std::thread::spawn(move || {
                    let mut head = Vec::new();
                    let mut byte = [0u8; 1];
                    while !head.ends_with(b"\r\n\r\n") && conn.read(&mut byte).is_ok_and(|n| n == 1) {
                        head.push(byte[0]);
                    }
                    let path = String::from_utf8_lossy(&head).split(' ').nth(1).unwrap_or("").to_string();
                    seen.lock().unwrap().push(path);
                    let body = b"<!doctype html><html><head><title>A test video</title></head><body><video></video></body></html>";
                    let head = format!("HTTP/1.0 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                    let _ = conn.write_all(head.as_bytes());
                    let _ = conn.write_all(body);
                });
            }
        });
        Self { addr, requests }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }
}

fn wait_for(player: &Player, what: &str, mut done: impl FnMut(&Status) -> bool) -> Status {
    let started = Instant::now();
    loop {
        let status = player.shared.status();
        if done(&status) {
            return status;
        }
        assert!(started.elapsed() < Duration::from_secs(40), "timed out waiting for {what}: {status:?}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn reopen(player: &Player, open: impl FnOnce()) -> Status {
    player.stop();
    wait_for(player, "idle", |s| s.state == State::Idle);
    open();
    wait_for(player, "loading to finish", |s| !matches!(s.state, State::Idle | State::Loading))
}

/// (state, title, info, chosen picture format, notes)
fn seen(s: &Status) -> (State, String, String, Option<String>, Vec<String>) {
    (s.state, s.title.clone(), s.info.clone(), s.choice.video.clone(), s.notes.clone())
}

#[test]
#[cfg(any(windows, target_os = "linux"))]
fn a_video_page_plays_its_best_single_file_format_with_the_sites_headers_and_reopens_at_another_quality() {
    let test = "a_video_page_plays_its_best_single_file_format_with_the_sites_headers_and_reopens_at_another_quality";
    if !is_child() {
        let dir = fresh_dir("page_plays");
        let media = RangeServer::start(vec![
            ("/low.mp4", Mode::Ranged, fixture_bytes("h264_longgop_aac.mp4")),
            ("/mid.mp4", Mode::Ranged, fixture_bytes("h264_aac.mp4")),
        ]);
        let pages = HtmlServer::start();
        let page_url = pages.url("/watch/123");
        let answer = json!({
            "extractor_key": "TestSite", "title": "A test video", "duration": 2.0,
            "http_headers": { "Referer": "https://site.example/watch/123", "User-Agent": "TestAgent/1.0" },
            "formats": [
                { "format_id": "low", "url": media.url("/low.mp4"), "protocol": "http", "ext": "mp4", "height": 120 },
                { "format_id": "mid", "url": media.url("/mid.mp4"), "protocol": "http", "ext": "mp4", "height": 240 },
                { "format_id": "huge", "url": media.url("/huge.mp4"), "protocol": "http", "ext": "mp4", "height": 2160 },
            ],
        });
        let mut env = fake_env(&dir, &json!({ page_url.clone(): answer }));
        env.push(("PLAYR_TEST_PAGE", OsString::from(&page_url)));
        run_child(test, env);
        let mid: Vec<_> = media.requests().into_iter().filter(|r| r.path == "/mid.mp4").collect();
        assert!(!mid.is_empty(), "the default format was never fetched");
        let headers: Vec<(Option<&str>, Option<&str>)> = mid.iter().map(|r| (r.header("referer"), r.header("user-agent"))).collect();
        assert!(
            headers.iter().all(|h| *h == (Some("https://site.example/watch/123"), Some("TestAgent/1.0"))),
            "the site's headers were not sent with every range request: {headers:?}"
        );
        assert!(mid.iter().all(|r| r.header("range").is_some()), "a request for the video was not a range request");
        assert!(!media.requests().iter().any(|r| r.path == "/huge.mp4"), "the 2160p format was fetched");
        let page_requests = pages.requests.lock().unwrap().clone();
        assert!(!page_requests.is_empty() && page_requests.iter().all(|p| p == "/watch/123"), "{page_requests:?}");
        return;
    }
    let page_url = std::env::var("PLAYR_TEST_PAGE").unwrap();
    let station = RadioServer::start(fixture_bytes("radio.mp3"));
    let player = Player::new(Box::new(|| {}));
    player.shared.audio.set_volume(0.0);

    let radio = reopen(&player, || player.open(station.url("/stream")));
    let calls_after_radio = calls();
    let first = reopen(&player, || player.open(page_url.clone()));
    let low = reopen(&player, || player.open_youtube_at(page_url.clone(), None, Choice { video: Some("low".into()), audio: None }));
    let again = reopen(&player, || player.open(page_url.clone()));
    player.stop();

    assert_eq!((radio.state, radio.title.as_str(), radio.page.is_none()), (State::Playing, STATION, true));
    assert_eq!(calls_after_radio, Vec::<String>::new(), "a radio stream was taken for a page");
    let expected = |info: &str, id: &str| (State::Playing, "A test video".to_string(), info.to_string(), Some(id.to_string()), vec![]);
    assert_eq!(
        [seen(&first), seen(&low), seen(&again)],
        [expected("TestSite · H.264 320×240", "mid"), expected("TestSite · H.264 160×120", "low"), expected("TestSite · H.264 320×240", "mid")]
    );
    assert_eq!((first.seekable, first.live), (true, false));
    assert_eq!(first.page.as_ref().map(|p| (p.site.as_str(), p.formats.len())), Some(("TestSite", 3)));
    assert_eq!(calls(), [page_call(&page_url)], "the page was asked about more than once within the half hour");
}

#[test]
#[cfg(any(windows, target_os = "linux"))]
fn a_page_without_a_playable_format_says_why() {
    let test = "a_page_without_a_playable_format_says_why";
    if !is_child() {
        let dir = fresh_dir("page_unplayable");
        let pages = HtmlServer::start();
        let hls = pages.url("/hls");
        let empty = pages.url("/empty");
        let plain = pages.url("/plain-http");
        let whole = pages.url("/no-ranges");
        let no_content_range = pages.url("/no-content-range");
        let media = RangeServer::start(vec![
            ("/whole.mp4", Mode::IgnoresRange, fixture_bytes("h264_aac.mp4")),
            ("/partial.mp4", Mode::NoContentRange, fixture_bytes("h264_aac.mp4")),
        ]);
        let script = json!({
            no_content_range.clone(): { "extractor_key": "TestSite", "formats": [
                { "format_id": "sd", "url": media.url("/partial.mp4"), "protocol": "http", "ext": "mp4" },
            ]},
            whole.clone(): { "extractor_key": "TestSite", "formats": [
                { "format_id": "sd", "url": media.url("/whole.mp4"), "protocol": "http", "ext": "mp4" },
            ]},
            hls.clone(): { "extractor_key": "TestSite", "formats": [
                { "format_id": "hls-720", "url": "https://cdn.example/720.m3u8", "protocol": "m3u8_native", "vcodec": "vp09.00.40.08", "acodec": "mp4a.40.2" },
                { "format_id": "hls-v", "url": "https://cdn.example/v.m3u8", "protocol": "m3u8_native", "vcodec": "avc1.4d401f", "acodec": "none" },
                { "format_id": "v", "url": "https://cdn.example/v.mp4", "protocol": "https", "vcodec": "avc1", "acodec": "none" },
            ]},
            empty.clone(): { "extractor": "generic", "title": "Nothing here" },
            plain.clone(): { "extractor_key": "TestSite", "formats": [
                { "format_id": "sd", "url": "http://0.0.0.0:9/v.mp4", "protocol": "http", "ext": "mp4" },
            ]},
        });
        let mut env = fake_env(&dir, &script);
        env.push(("PLAYR_TEST_PAGES", OsString::from(format!("{hls} {empty} {plain} {} {whole} {no_content_range}", pages.url("/unknown")))));
        env.push(("PLAYR_TEST_WHOLE", OsString::from(media.url("/whole.mp4"))));
        env.push(("PLAYR_TEST_PARTIAL", OsString::from(media.url("/partial.mp4"))));
        run_child(test, env);
        return;
    }
    let whole_file = std::env::var("PLAYR_TEST_WHOLE").unwrap();
    let partial_file = std::env::var("PLAYR_TEST_PARTIAL").unwrap();
    let urls: Vec<String> = std::env::var("PLAYR_TEST_PAGES").unwrap().split(' ').map(str::to_string).collect();
    let player = Player::new(Box::new(|| {}));
    player.shared.audio.set_volume(0.0);

    let results: Vec<(State, Option<String>)> = urls
        .iter()
        .map(|u| reopen(&player, || player.open(u.clone())))
        .map(|s| (s.state, s.error))
        .collect();

    let refusal = "0.0.0.0 could not be opened over an encrypted connection (https), and playr is set to encrypted connections only, \
                   so it was not opened unencrypted. Untick \u{1f512} Encrypted connections only to play it anyway. (cannot download https://0.0.0.0:9/v.mp4: ";
    let mut bad = Vec::new();
    let failed = |e: &str| (State::Failed, Some(e.to_string()));
    expect!(
        bad,
        results[0] == failed("TestSite offers this video only as separate picture and sound files or in formats playr cannot decode"),
        "VP9 HLS, picture-only HLS without HLS sound and a split stream: {:?}",
        results[0]
    );
    expect!(bad, results[1] == failed("the page has no video that yt-dlp can find"), "nothing: {:?}", results[1]);
    expect!(
        bad,
        results[2].0 == State::Failed && results[2].1.as_deref().is_some_and(|e| e.starts_with(refusal) && e.ends_with(')') && e.len() > refusal.len() + 1),
        "plain-http format with encrypted only: {:?}",
        results[2]
    );
    expect!(bad, results[3] == failed(&format!("[generic] Unsupported URL: {}", urls[3])), "unknown page: {:?}", results[3]);
    expect!(
        bad,
        results[4] == failed(&format!("TestSite does not let this video be fetched in parts ({whole_file} ignored the byte range (HTTP 200))")),
        "server that ignores Range: {:?}",
        results[4]
    );
    expect!(
        bad,
        results[5] == failed(&format!("TestSite does not let this video be fetched in parts ({partial_file} does not support ranged downloads)")),
        "server that answers 206 without Content-Range: {:?}",
        results[5]
    );
    expect!(bad, calls() == urls.iter().map(|u| page_call(u)).collect::<Vec<_>>(), "yt-dlp calls: {:?}", calls());
    finish(bad);
}

const UNENCRYPTED_NOTE: &str = "\u{1f513} unencrypted connection (http): the network can see what is playing";

#[test]
#[cfg(any(windows, target_os = "linux"))]
fn with_the_setting_off_a_page_or_a_video_fetched_over_plain_http_says_it_is_unencrypted() {
    let test = "with_the_setting_off_a_page_or_a_video_fetched_over_plain_http_says_it_is_unencrypted";
    if !is_child() {
        let media_route = own_ip::route("/v.mp4", "video/mp4", fixture_bytes("h264_aac.mp4"), true, true);
        let page_route = own_ip::route("/watch/9", "text/html; charset=utf-8", b"<html><body><video></video></body></html>".to_vec(), false, true);
        let Some(remote) = own_ip::OwnServer::start(vec![media_route, page_route]) else {
            eprintln!("skipped: this computer has no network address other than loopback");
            return;
        };
        let dir = fresh_dir("page_unencrypted");
        let local_media = RangeServer::start(vec![("/v.mp4", Mode::Ranged, fixture_bytes("h264_aac.mp4"))]);
        let pages = HtmlServer::start();
        let remote_page = remote.url("/watch/9");
        let local_page = pages.url("/watch/8");
        let answer = |url: String| json!({ "extractor_key": "TestSite", "title": "Plain", "formats": [{ "format_id": "sd", "url": url, "protocol": "http", "ext": "mp4" }] });
        // yt-dlp is handed the page address upgraded to https, so that is what the fake knows.
        let script = json!({
            remote_page.replacen("http://", "https://", 1): answer(local_media.url("/v.mp4")),
            local_page.clone(): answer(remote.url("/v.mp4")),
        });
        let mut env = fake_env(&dir, &script);
        env.push(("PLAYR_TEST_PAGES", OsString::from(format!("{remote_page} {local_page}"))));
        run_child(test, env);
        let seen = remote.seen();
        let watch = seen.iter().position(|s| *s == own_ip::Seen::Get("/watch/9".into()));
        let video = seen.iter().position(|s| *s == own_ip::Seen::Get("/v.mp4".into()));
        assert!(watch.is_some_and(|i| i > 0 && seen[i - 1] == own_ip::Seen::Tls), "the page was not tried over https first: {seen:?}");
        assert!(video.is_some_and(|i| i > 0 && seen[i - 1] == own_ip::Seen::Tls), "the video was not tried over https first: {seen:?}");
        return;
    }
    playr::privacy::set_https_only(false);
    let urls: Vec<String> = std::env::var("PLAYR_TEST_PAGES").unwrap().split(' ').map(str::to_string).collect();
    let player = Player::new(Box::new(|| {}));
    player.shared.audio.set_volume(0.0);

    let remote_page = reopen(&player, || player.open(urls[0].clone()));
    let remote_video = reopen(&player, || player.open(urls[1].clone()));
    player.stop();

    let expected = (State::Playing, "Plain".to_string(), "TestSite · H.264 320×240".to_string(), Some("sd".to_string()), vec![UNENCRYPTED_NOTE.to_string()]);
    assert_eq!(seen(&remote_page), expected, "page over plain http");
    assert_eq!(seen(&remote_video), expected, "video over plain http");
    assert_eq!(calls(), [page_call(&urls[0].replacen("http://", "https://", 1)), page_call(&urls[1])]);
}
