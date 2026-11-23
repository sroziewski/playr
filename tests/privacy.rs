//! Encrypted connections only: `playr::privacy` and what it does to the fetches playr makes.
//!
//! The setting is one process-wide flag that starts on. Tests in this process never switch it
//! off; every test that needs it off runs its body in a fresh copy of this test binary.
//!
//! The local test servers listen on 127.0.0.1, which is exempt, so a real upgrade to HTTPS cannot
//! be shown offline. The refusal end to end uses `http://0.0.0.0:9/...`: not loopback, so the
//! setting applies, and connecting to it fails at once without any packet leaving the machine
//! (Windows rejects 0.0.0.0 as a destination; elsewhere it means this host, where the discard
//! port is closed).

mod common;
mod own_ip;

use std::cell::RefCell;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use common::http::{RadioServer, STATION};
use common::{finish, fixture_bytes};
use playr::player::{Player, State, Status};
use playr::privacy::{self, Attempt, attempts_with, is_loopback, is_unencrypted, refused};

const CHILD: &str = "PLAYR_TEST_CHILD";
const DIR: &str = "PLAYR_TEST_DIR";
const UNENCRYPTED_NOTE: &str = "\u{1f513} unencrypted connection (http): the network can see what is playing";
const NOWHERE: &str = "http://0.0.0.0:9/live.mp3";

fn attempt(url: &str, https_only: bool) -> Attempt {
    Attempt { url: url.to_string(), https_only }
}

/// The refusal text up to the reason in brackets, filled in by hand from the agreed wording.
fn refusal_prefix(host: &str) -> String {
    format!(
        "{host} could not be opened over an encrypted connection (https), and playr is set to encrypted connections only, \
         so it was not opened unencrypted. Untick \u{1f512} Encrypted connections only to play it anyway. ("
    )
}

fn fresh_dir(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("playr-privacy").join(test);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create test dir");
    dir
}

fn is_child() -> bool {
    std::env::var_os(CHILD).is_some()
}

/// In the test process: runs `test` alone in a copy of this binary with `env` added and returns
/// false. In that copy: returns true, and the caller runs the test body.
fn in_child(test: &str, env: Vec<(&str, OsString)>) -> bool {
    if is_child() {
        return true;
    }
    let out = Command::new(std::env::current_exe().expect("test binary"))
        .args([test, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .envs(env)
        .output()
        .expect("start the child test");
    let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "{test} failed in the child process:\n{stdout}\n{stderr}");
    assert!(stdout.contains("test result: ok. 1 passed"), "{test} did not run in the child process:\n{stdout}");
    false
}

fn private_env(dir: &Path) -> Vec<(&'static str, OsString)> {
    let data = dir.join("appdata");
    vec![(DIR, dir.as_os_str().to_owned()), ("LOCALAPPDATA", data.as_os_str().to_owned()), ("XDG_DATA_HOME", data.into_os_string())]
}

#[test]
fn loopback_addresses_are_this_computer_and_nothing_else_is() {
    let cases = [
        ("localhost", "http://localhost/radio", true),
        ("localhost in capitals with a port", "http://LOCALHOST:8000/radio", true),
        ("a .localhost name", "https://player.localhost/x", true),
        ("127.0.0.1", "http://127.0.0.1:8000/stream", true),
        ("elsewhere in 127/8", "http://127.8.9.10/stream", true),
        ("IPv6 loopback", "http://[::1]:8080/stream", true),
        ("user before the host", "http://me:pw@127.0.0.1/stream", true),
        ("a public name", "http://radio.example/stream", false),
        ("a private network address", "http://10.0.0.1/stream", false),
        ("the any address", "http://0.0.0.0:9/stream", false),
        ("localhost as a subdomain", "http://localhost.example.com/stream", false),
        ("a name ending in localhost", "http://notlocalhost/stream", false),
        ("another IPv6 address", "http://[::2]/stream", false),
        ("128/8", "http://128.0.0.1/stream", false),
        ("loopback as a user name", "http://127.0.0.1@radio.example/stream", false),
        ("no scheme", "localhost:8000/radio", false),
    ];
    let mut bad = Vec::new();
    for (case, url, expected) in cases {
        expect!(bad, is_loopback(url) == expected, "{case}: is_loopback({url:?}) = {}, expected {expected}", is_loopback(url));
    }
    finish(bad);
}

#[test]
fn a_host_name_that_only_starts_with_127_is_not_loopback() {
    let cases = ["http://127.attacker.example/stream", "http://127.0.0.1.nip.io/stream"];
    let found: Vec<(&str, bool)> = cases.iter().map(|&u| (u, is_loopback(u))).collect();

    assert_eq!(found, [(cases[0], false), (cases[1], false)]);
}

#[test]
fn attempts_try_https_first_and_plain_http_only_when_not_strict() {
    let cases = [
        ("http, strict", "http://radio.example/live", true, vec![attempt("https://radio.example/live", true)]),
        (
            "http, not strict",
            "http://radio.example/live",
            false,
            vec![attempt("https://radio.example/live", true), attempt("http://radio.example/live", false)],
        ),
        (
            "upper-case scheme, port and query kept",
            "HTTP://Radio.Example:8000/a?b=1",
            false,
            vec![attempt("https://Radio.Example:8000/a?b=1", true), attempt("HTTP://Radio.Example:8000/a?b=1", false)],
        ),
        ("spaces trimmed", "  http://a.example/x \n", true, vec![attempt("https://a.example/x", true)]),
        (
            "the plain-http port 80 is dropped for https",
            "http://radio.example:80/live",
            false,
            vec![attempt("https://radio.example/live", true), attempt("http://radio.example:80/live", false)],
        ),
        ("another port is kept", "http://radio.example:8000/live", true, vec![attempt("https://radio.example:8000/live", true)]),
        ("https, strict", "https://radio.example/live", true, vec![attempt("https://radio.example/live", true)]),
        ("https, not strict", "https://radio.example/live", false, vec![attempt("https://radio.example/live", false)]),
        ("other scheme, strict", "rtmp://radio.example/live", true, vec![attempt("rtmp://radio.example/live", true)]),
        ("other scheme, not strict", "rtmp://radio.example/live", false, vec![attempt("rtmp://radio.example/live", false)]),
        ("loopback http, strict", "http://127.0.0.1:8000/x", true, vec![attempt("http://127.0.0.1:8000/x", false)]),
        ("loopback https, strict", "https://localhost/x", true, vec![attempt("https://localhost/x", false)]),
    ];
    let mut bad = Vec::new();
    for (case, url, strict, expected) in cases {
        let actual = attempts_with(url, strict);
        expect!(bad, actual == expected, "{case}:\n    got      {actual:?}\n    expected {expected:?}");
    }
    finish(bad);
}

#[test]
fn the_refusal_names_the_host_and_the_reason() {
    let cases = [
        ("user, port, path and query", "http://user@radio.example:8000/live?x=1", "connection refused", "radio.example"),
        ("IPv6 host", "http://[2001:db8::1]:8000/live", "timed out", "2001:db8::1"),
    ];
    let mut bad = Vec::new();
    for (case, url, why, host) in cases {
        let actual = refused(url, why);
        let expected = format!("{}{why})", refusal_prefix(host));
        expect!(bad, actual == expected, "{case}:\n    got      {actual:?}\n    expected {expected:?}");
    }
    finish(bad);
}

#[test]
fn unencrypted_means_plain_http_to_another_computer() {
    let cases = [
        ("http", "http://radio.example/live", true),
        ("HTTP in capitals", "HTTP://radio.example/live", true),
        ("http after spaces", "  http://radio.example/live", true),
        ("https", "https://radio.example/live", false),
        ("loopback http", "http://127.0.0.1:8000/live", false),
        ("a local file", "C:\\Music\\a.mp3", false),
        ("another scheme", "rtmp://radio.example/live", false),
    ];
    let mut bad = Vec::new();
    for (case, url, expected) in cases {
        expect!(bad, is_unencrypted(url) == expected, "{case}: {}", is_unencrypted(url));
    }
    finish(bad);
}

/// Runs `privacy::fetch` with a stand-in for the network that answers each URL from `answers`
/// (missing: "no route") and returns the result and the attempts it saw.
fn fetch_with(url: &str, answers: &[(&str, Result<&str, &str>)]) -> (Result<String, String>, Vec<Attempt>) {
    let seen = RefCell::new(Vec::new());
    let result = privacy::fetch(url, |a| {
        seen.borrow_mut().push(a.clone());
        let answer = answers.iter().find(|(u, _)| *u == a.url).map_or(Err("no route"), |(_, r)| *r);
        answer.map(str::to_string).map_err(str::to_string)
    });
    (result, seen.into_inner())
}

#[test]
fn strict_fetch_stops_after_https_and_says_why() {
    assert!(privacy::https_only(), "the setting starts on");
    let refused_http = format!("{}tls handshake failed)", refusal_prefix("radio.example"));
    let cases = [
        (
            "https answers",
            "http://radio.example/live",
            vec![("https://radio.example/live", Ok("encrypted body"))],
            Ok("encrypted body".to_string()),
            vec![attempt("https://radio.example/live", true)],
        ),
        (
            "only plain http would answer",
            "http://radio.example/live",
            vec![("https://radio.example/live", Err("tls handshake failed")), ("http://radio.example/live", Ok("plain body"))],
            Err(refused_http),
            vec![attempt("https://radio.example/live", true)],
        ),
        (
            "an https address that fails keeps its own error",
            "https://radio.example/live",
            vec![("https://radio.example/live", Err("http status: 404"))],
            Err("http status: 404".to_string()),
            vec![attempt("https://radio.example/live", true)],
        ),
        (
            "loopback is used as it is",
            "http://127.0.0.1:8000/live",
            vec![("http://127.0.0.1:8000/live", Err("connection refused"))],
            Err("connection refused".to_string()),
            vec![attempt("http://127.0.0.1:8000/live", false)],
        ),
    ];
    let mut bad = Vec::new();
    for (case, url, answers, expected, expected_attempts) in cases {
        let (result, seen) = fetch_with(url, &answers);
        expect!(bad, result == expected, "{case}: {result:?}, expected {expected:?}");
        expect!(bad, seen == expected_attempts, "{case}: attempts {seen:?}, expected {expected_attempts:?}");
    }
    finish(bad);
}

#[test]
fn with_the_setting_off_fetch_falls_back_to_plain_http() {
    if !in_child("with_the_setting_off_fetch_falls_back_to_plain_http", vec![]) {
        return;
    }
    privacy::set_https_only(false);
    let both = vec![attempt("https://radio.example/live", true), attempt("http://radio.example/live", false)];
    let cases = [
        (
            "https answers",
            vec![("https://radio.example/live", Ok("encrypted body"))],
            Ok("encrypted body".to_string()),
            vec![attempt("https://radio.example/live", true)],
        ),
        (
            "plain http answers",
            vec![("https://radio.example/live", Err("tls handshake failed")), ("http://radio.example/live", Ok("plain body"))],
            Ok("plain body".to_string()),
            both.clone(),
        ),
        (
            "neither answers: the first error",
            vec![("https://radio.example/live", Err("tls handshake failed")), ("http://radio.example/live", Err("http status: 404"))],
            Err("tls handshake failed".to_string()),
            both,
        ),
    ];
    let mut bad = Vec::new();
    for (case, answers, expected, expected_attempts) in cases {
        let (result, seen) = fetch_with("http://radio.example/live", &answers);
        expect!(bad, result == expected, "{case}: {result:?}, expected {expected:?}");
        expect!(bad, seen == expected_attempts, "{case}: attempts {seen:?}, expected {expected_attempts:?}");
    }
    let switched_off = privacy::https_only();
    privacy::set_https_only(true);
    expect!(bad, !switched_off && privacy::https_only(), "set_https_only did not switch the setting");
    finish(bad);
}

#[test]
fn every_kind_of_fetch_refuses_a_plain_http_address_when_strict() {
    let started = Instant::now();
    let text = playr::dash::get_text("http://0.0.0.0:9/list.m3u", &[], 1024).map(|_| ());
    let ranged = playr::dash::HttpFetch::connect("http://0.0.0.0:9/episode.mp3", &[], 1).map(|_| ());
    let stream = playr::net::open_url(NOWHERE, Arc::new(AtomicBool::new(false)), Box::new(|_| {})).map(|_| ());

    let cases = [
        ("dash::get_text", text, "cannot download https://0.0.0.0:9/list.m3u: "),
        ("HttpFetch::connect", ranged, "cannot download https://0.0.0.0:9/episode.mp3: "),
        ("net::open_url", stream, "cannot open https://0.0.0.0:9/live.mp3: "),
    ];
    let mut bad = Vec::new();
    for (case, result, why_starts) in cases {
        let prefix = format!("{}{why_starts}", refusal_prefix("0.0.0.0"));
        expect!(
            bad,
            result.as_ref().is_err_and(|e| e.starts_with(&prefix) && e.ends_with(')') && e.len() > prefix.len() + 1),
            "{case}: {result:?}"
        );
    }
    finish(bad);
    assert!(started.elapsed() < Duration::from_secs(10), "refusing took {:?}", started.elapsed());
}

static AUDIO_DEVICE: Mutex<()> = Mutex::new(());

fn player() -> (Player, MutexGuard<'static, ()>) {
    let device = AUDIO_DEVICE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let player = Player::new(Box::new(|| {}));
    player.shared.audio.set_volume(0.0);
    (player, device)
}

fn open_and_wait(player: &Player, target: &str) -> Status {
    player.open(target);
    let started = Instant::now();
    loop {
        let status = player.shared.status();
        if status.source == target && !matches!(status.state, State::Idle | State::Loading) {
            return status;
        }
        assert!(started.elapsed() < Duration::from_secs(30), "timed out opening {target}: {status:?}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn the_player_refuses_a_plain_http_station_when_strict() {
    let (player, _device) = player();

    let status = open_and_wait(&player, NOWHERE);

    assert_eq!(status.state, State::Failed, "{status:?}");
    let error = status.error.unwrap_or_default();
    let prefix = format!("{}cannot open https://0.0.0.0:9/live.mp3: ", refusal_prefix("0.0.0.0"));
    assert!(error.starts_with(&prefix) && error.ends_with(')') && error.len() > prefix.len() + 1, "{error:?}");
}

#[test]
fn with_the_setting_off_a_failed_station_reports_the_connection_error_not_a_refusal() {
    if !in_child("with_the_setting_off_a_failed_station_reports_the_connection_error_not_a_refusal", vec![]) {
        return;
    }
    privacy::set_https_only(false);
    let (player, _device) = player();

    let status = open_and_wait(&player, NOWHERE);

    assert_eq!(status.state, State::Failed, "{status:?}");
    let error = status.error.unwrap_or_default();
    let prefix = "cannot open https://0.0.0.0:9/live.mp3: ";
    assert!(error.starts_with(prefix) && error.len() > prefix.len(), "{error:?}");
}

#[test]
fn a_station_on_this_computer_plays_as_it_is_without_the_unencrypted_note() {
    let server = RadioServer::start(fixture_bytes("radio.mp3"));
    let stream = playr::net::open_url(&server.url("/stream"), Arc::new(AtomicBool::new(false)), Box::new(|_| {})).expect("open the local station");
    let (player, _device) = player();

    let status = open_and_wait(&player, &server.url("/stream"));

    assert!(stream.secure, "a loopback stream never crosses the network");
    assert_eq!((status.state, status.title.as_str(), status.notes.clone()), (State::Playing, STATION, Vec::<String>::new()));
    assert_eq!(server.requests_for("/stream").len(), 2, "one request for open_url, one for the player");
}

#[test]
fn with_the_setting_off_a_station_on_this_computer_still_gets_no_unencrypted_note() {
    if !in_child("with_the_setting_off_a_station_on_this_computer_still_gets_no_unencrypted_note", vec![]) {
        return;
    }
    privacy::set_https_only(false);
    let server = RadioServer::start(fixture_bytes("radio.mp3"));
    let (player, _device) = player();

    let status = open_and_wait(&player, &server.url("/stream"));

    assert_eq!((status.state, status.notes.clone()), (State::Playing, Vec::<String>::new()));
    assert!(!status.notes.iter().any(|n| n == UNENCRYPTED_NOTE));
}

#[test]
fn the_setting_is_saved_in_privacy_json_and_reads_as_on_unless_it_says_off() {
    let dir = fresh_dir("settings");
    let cases = [
        ("missing file", None, true),
        ("not JSON", Some("{ https_only: false"), true),
        ("off", Some(r#"{"https_only": false}"#), false),
        ("on", Some(r#"{"https_only": true}"#), true),
        ("a string, not a bool", Some(r#"{"https_only": "false"}"#), true),
        ("no such key", Some("{}"), true),
    ];
    let mut bad = Vec::new();
    for (n, (case, text, expected)) in cases.into_iter().enumerate() {
        let path = dir.join(format!("case{n}.json"));
        if let Some(text) = text {
            std::fs::write(&path, text).unwrap();
        }
        expect!(bad, privacy::load(&path) == expected, "{case}: {}", privacy::load(&path));
    }
    expect!(bad, privacy::load(&dir), "a folder in the file's place reads as on");
    finish(bad);
}

#[test]
fn saving_creates_the_folder_and_writes_the_choice_back() {
    let path = fresh_dir("save").join("nested").join("privacy.json");

    let off = privacy::save(&path, false);
    let off_text: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let off_loaded = privacy::load(&path);
    let on = privacy::save(&path, true);
    let on_loaded = privacy::load(&path);

    assert_eq!((off, on), (Ok(()), Ok(())));
    assert_eq!(off_text, serde_json::json!({ "https_only": false }));
    assert_eq!((off_loaded, on_loaded), (false, true));
}

#[test]
fn saving_reports_a_folder_that_cannot_be_created() {
    let dir = fresh_dir("save_blocked");
    let blocker = dir.join("a file");
    std::fs::write(&blocker, "not a folder").unwrap();

    let saved = privacy::save(&blocker.join("privacy.json"), false);

    let prefix = format!("cannot create {}: ", blocker.display());
    assert!(saved.as_ref().is_err_and(|e| e.starts_with(&prefix) && e.len() > prefix.len()), "{saved:?}");
}

#[test]
fn the_settings_file_is_privacy_json_in_the_data_folder() {
    let dir = fresh_dir("settings_path");
    if !in_child("the_settings_file_is_privacy_json_in_the_data_folder", private_env(&dir)) {
        return;
    }
    let dir = PathBuf::from(std::env::var_os(DIR).unwrap());

    assert_eq!(privacy::settings_path(), Some(dir.join("appdata").join("playr").join("privacy.json")));
}

#[test]
fn the_https_only_client_refuses_a_plain_http_address_that_the_ordinary_client_fetches() {
    let server = common::range_http::RangeServer::start(vec![("/", common::range_http::Mode::IgnoresRange, b"plain body".to_vec())]);
    let url = server.url("/");

    let strict = playr::dash::agent_with(true).get(&url).call();
    let plain = playr::dash::agent_with(false).get(&url).call().map(|r| r.status().as_u16());

    assert!(matches!(strict, Err(ureq::Error::RequireHttpsOnly(_))), "{strict:?}");
    assert_eq!(plain.map_err(|e| e.to_string()), Ok(200));
    assert_eq!(server.requests().len(), 1, "the https-only client must not send anything");
}

use own_ip::{OwnServer, Seen, route};

/// The station `radio.mp3` as an endless plain-HTTP stream on this computer's own address.
fn own_station() -> Option<OwnServer> {
    let server = OwnServer::start(vec![route("/stream", "audio/mpeg", fixture_bytes("radio.mp3"), false, false)]);
    if server.is_none() {
        eprintln!("skipped: this computer has no network address other than loopback");
    }
    server
}

#[test]
fn with_the_setting_on_a_station_that_answers_only_plain_http_is_refused_without_a_plain_request() {
    let Some(server) = own_station() else { return };
    let url = server.url("/stream");
    let (player, _device) = player();

    let status = open_and_wait(&player, &url);

    assert_eq!(status.state, State::Failed, "{status:?}");
    let error = status.error.unwrap_or_default();
    let prefix = format!("{}cannot open {}: ", refusal_prefix(&server.host()), url.replacen("http://", "https://", 1));
    assert!(error.starts_with(&prefix) && error.ends_with(')') && error.len() > prefix.len() + 1, "{error:?}");
    assert_eq!(server.seen(), [Seen::Tls]);
}

#[test]
fn with_the_setting_off_a_plain_http_station_plays_after_trying_https_and_says_it_is_unencrypted() {
    if !in_child("with_the_setting_off_a_plain_http_station_plays_after_trying_https_and_says_it_is_unencrypted", vec![]) {
        return;
    }
    privacy::set_https_only(false);
    let Some(server) = own_station() else { return };
    let url = server.url("/stream");
    let (player, _device) = player();

    let status = open_and_wait(&player, &url);
    player.stop();

    assert_eq!((status.state, status.notes.clone()), (State::Playing, vec![UNENCRYPTED_NOTE.to_string()]), "{status:?}");
    assert_eq!(server.seen(), [Seen::Tls, Seen::Get("/stream".into())]);
}

#[test]
fn with_the_setting_off_a_plain_http_playlist_leading_to_a_local_stream_still_says_it_is_unencrypted() {
    if !in_child("with_the_setting_off_a_plain_http_playlist_leading_to_a_local_stream_still_says_it_is_unencrypted", vec![]) {
        return;
    }
    privacy::set_https_only(false);
    let station = RadioServer::start(fixture_bytes("radio.mp3"));
    let pls = format!("[playlist]\nNumberOfEntries=1\nFile1={}\n", station.url("/stream"));
    let Some(server) = OwnServer::start(vec![route("/radio.pls", "audio/x-scpls", pls.into_bytes(), false, true)]) else {
        eprintln!("skipped: this computer has no network address other than loopback");
        return;
    };
    let (player, _device) = player();

    let status = open_and_wait(&player, &server.url("/radio.pls"));
    player.stop();

    let got = (status.state, status.title.as_str(), status.playlist.is_none(), status.notes.clone());
    assert_eq!(got, (State::Playing, STATION, true, vec![UNENCRYPTED_NOTE.to_string()]), "{status:?}");
    assert_eq!(station.requests_for("/stream").len(), 1);
}

/// A yt-dlp release folder on this computer's own address.
fn own_release(asset: &[u8]) -> Option<OwnServer> {
    let sums = format!("{}  yt-dlp_test.bin\n", playr::youtube::sha256_hex(asset));
    let server = OwnServer::start(vec![
        route("/rel/SHA2-256SUMS", "text/plain", sums.into_bytes(), false, true),
        route("/rel/yt-dlp_test.bin", "application/octet-stream", asset.to_vec(), false, true),
    ]);
    if server.is_none() {
        eprintln!("skipped: this computer has no network address other than loopback");
    }
    server
}

#[test]
fn the_ytdlp_download_is_refused_from_a_plain_http_release_folder_when_strict() {
    let Some(server) = own_release(b"#!fake build") else { return };
    let dest = fresh_dir("download_strict").join("yt-dlp_test.bin");

    let result = playr::youtube::download_ytdlp_from(&server.url("/rel/"), "yt-dlp_test.bin", &dest);

    let prefix = format!("{}cannot download {}: ", refusal_prefix(&server.host()), server.url("/rel/SHA2-256SUMS").replacen("http://", "https://", 1));
    assert!(result.as_ref().is_err_and(|e| e.starts_with(&prefix) && e.ends_with(')')), "{result:?}");
    assert_eq!(server.seen(), [Seen::Tls]);
    assert!(!dest.exists());
}

#[test]
fn with_the_setting_off_the_ytdlp_download_tries_https_before_each_plain_request() {
    if !in_child("with_the_setting_off_the_ytdlp_download_tries_https_before_each_plain_request", vec![]) {
        return;
    }
    privacy::set_https_only(false);
    let asset = b"#!fake build 2026.11.23".repeat(50);
    let Some(server) = own_release(&asset) else { return };
    let dest = fresh_dir("download_plain").join("yt-dlp_test.bin");

    let result = playr::youtube::download_ytdlp_from(&server.url("/rel/"), "yt-dlp_test.bin", &dest);

    assert_eq!(result, Ok(()));
    assert!(std::fs::read(&dest).unwrap() == asset, "the saved file differs from the download");
    assert_eq!(
        server.seen(),
        [Seen::Tls, Seen::Get("/rel/SHA2-256SUMS".into()), Seen::Tls, Seen::Get("/rel/yt-dlp_test.bin".into())]
    );
}

#[test]
#[ignore = "needs the internet: a real http:// station that also answers over https"]
fn a_real_http_station_with_https_plays_encrypted_without_the_note() {
    let (player, _device) = player();

    let status = open_and_wait(&player, "http://media-ice.musicradio.com/CapitalMP3");
    player.stop();

    assert_eq!(status.state, State::Playing, "{status:?}");
    assert!(!status.notes.iter().any(|n| n == UNENCRYPTED_NOTE), "{:?}", status.notes);
}
