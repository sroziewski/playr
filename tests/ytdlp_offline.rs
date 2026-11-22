//! The yt-dlp side of `playr::youtube` and the files `playr::cookies` keeps, without the
//! network: a fake yt-dlp script (named by `PLAYR_YTDLP`) that logs its arguments, a local
//! server for the DASH fixtures and the yt-dlp release, and a private data folder.
//!
//! Environment variables belong to the whole process, so every test that needs its own
//! (`PLAYR_YTDLP`, `PATH`, `LOCALAPPDATA` / `XDG_DATA_HOME`) runs its body in a fresh copy of
//! this test binary started with that environment.

mod common;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use common::fixture_bytes;
use common::range_http::{Mode, RangeServer};
use playr::cookies::{self, CookieJar};
use playr::player::{Player, State, Status};
use playr::youtube::{attempts, download_ytdlp_from, resolve, sha256_hex};

const CHILD: &str = "PLAYR_TEST_CHILD";
const DIR: &str = "PLAYR_TEST_DIR";
const URL: &str = "https://www.youtube.com/watch?v=aB3_-x9Zq0Y";

/// A fresh, empty folder for `test`.
fn fresh_dir(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("playr-ytdlp-offline").join(test);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create test dir");
    dir
}

fn is_child() -> bool {
    std::env::var_os(CHILD).is_some()
}

/// In the test process: runs `test` alone in a copy of this binary with `env` added and
/// returns false. In that copy: returns true, and the caller runs the test body.
fn in_child(test: &str, env: Vec<(&str, OsString)>) -> bool {
    if is_child() {
        return true;
    }
    run_child(test, env);
    false
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

/// The environment of a child: its folder, a private data folder and, on Windows, a `PATH`
/// holding only `bin` (so which JavaScript runtimes exist is up to the test).
fn child_env(dir: &Path) -> Vec<(&'static str, OsString)> {
    let mut env = private_env(dir);
    env.push(("PLAYR_YTDLP", fake_ytdlp(dir).into_os_string()));
    env
}

fn private_env(dir: &Path) -> Vec<(&'static str, OsString)> {
    let data = dir.join("appdata");
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    vec![
        (DIR, dir.as_os_str().to_owned()),
        ("LOCALAPPDATA", data.as_os_str().to_owned()),
        ("XDG_DATA_HOME", data.as_os_str().to_owned()),
        ("PATH", bin.as_os_str().to_owned()),
    ]
}

/// A copy of this test binary named yt-dlp, which [`act_as_fake_ytdlp`] turns into a yt-dlp
/// stand-in scripted by `fake.json` in its folder (`{"search": {query: [entries]}, "videos":
/// {id: info}}`): it logs each call's arguments to `calls.log`, succeeds on `-U`, answers
/// searches and `-J` for the videos listed, and refuses any other video.
#[cfg(any(windows, target_os = "linux"))]
fn install_exe_fake(dir: &Path, script: &serde_json::Value) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(if cfg!(windows) { "yt-dlp.exe" } else { "yt-dlp" });
    std::fs::copy(std::env::current_exe().unwrap(), &path).expect("copy the test binary");
    std::fs::write(dir.join("fake.json"), script.to_string()).unwrap();
    path
}

#[cfg(windows)]
#[used]
#[unsafe(link_section = ".CRT$XCU")]
static FAKE_YTDLP_HOOK: extern "C" fn() = act_as_fake_ytdlp;

#[cfg(target_os = "linux")]
#[used]
#[unsafe(link_section = ".init_array")]
static FAKE_YTDLP_HOOK: extern "C" fn() = act_as_fake_ytdlp;

/// Runs before the test harness starts: a copy named yt-dlp with a `fake.json` beside it
/// answers as yt-dlp and exits; any other run of this binary carries on as the test binary.
#[cfg(any(windows, target_os = "linux"))]
extern "C" fn act_as_fake_ytdlp() {
    let Ok(exe) = std::env::current_exe() else { return };
    if exe.file_stem().and_then(|s| s.to_str()) != Some("yt-dlp") {
        return;
    }
    let dir = exe.parent().expect("folder").to_path_buf();
    let Ok(script) = std::fs::read_to_string(dir.join("fake.json")) else { return };
    let script: serde_json::Value = serde_json::from_str(&script).expect("fake.json");
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut log = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("calls.log")).unwrap();
    use std::io::Write;
    writeln!(log, "{}", args.join(" ")).unwrap();
    if args.iter().any(|a| a == "-U") {
        std::process::exit(0);
    }
    let last = args.last().cloned().unwrap_or_default();
    if let Some(query) = last.strip_prefix("ytsearch5:") {
        let entries = script["search"].get(query).cloned().unwrap_or(serde_json::json!([]));
        println!("{}", serde_json::json!({ "entries": entries }));
        std::process::exit(0);
    }
    let id = playr::youtube::video_id(&last).unwrap_or_default();
    match script["videos"].get(&id) {
        Some(info) => {
            println!("{info}");
            std::process::exit(0);
        }
        None => {
            eprintln!("ERROR: [youtube] {id}: Video unavailable");
            std::process::exit(1);
        }
    }
}

/// A yt-dlp stand-in: appends its arguments to `calls.log`, then fails if `fail.txt` exists
/// and prints `info.json` otherwise.
fn fake_ytdlp(dir: &Path) -> PathBuf {
    if cfg!(windows) {
        let path = dir.join("yt-dlp.cmd");
        let script = "@echo off\r\necho %*>>\"%~dp0calls.log\"\r\nif exist \"%~dp0fail.txt\" (\r\n  echo ERROR: [youtube] fake refused 1>&2\r\n  exit /b 1\r\n)\r\ntype \"%~dp0info.json\"\r\n";
        std::fs::write(&path, script).unwrap();
        path
    } else {
        let path = dir.join("yt-dlp");
        let script = "#!/bin/sh\nd=$(dirname \"$0\")\necho \"$*\" >> \"$d/calls.log\"\nif [ -e \"$d/fail.txt\" ]; then echo 'ERROR: [youtube] fake refused' >&2; exit 1; fi\ncat \"$d/info.json\"\n";
        std::fs::write(&path, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }
}

/// A JavaScript runtime stand-in yt-dlp would be told about; it is never run.
fn put_runtime_on_path(dir: &Path, name: &str) {
    let file = if cfg!(windows) { format!("{name}.cmd") } else { name.to_string() };
    std::fs::write(dir.join("bin").join(file), "").unwrap();
}

/// The fake's calls, one argument list per line, without the quotes a shell may have added.
fn calls(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("calls.log"))
        .unwrap_or_default()
        .lines()
        .map(|l| l.replace('"', "").trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

#[test]
fn attempts_go_plain_then_with_the_runtime_then_with_cookies_and_the_runtime() {
    let cookies = Path::new("c.txt");
    let none: Option<&Path> = None;
    assert_eq!(attempts(None, None), vec![(None, none)]);
    assert_eq!(attempts(Some("deno"), None), vec![(None, None), (Some("deno"), None)]);
    assert_eq!(attempts(None, Some(cookies)), vec![(None, None), (None, Some(cookies))]);
    assert_eq!(attempts(Some("node"), Some(cookies)), vec![(None, None), (Some("node"), None), (Some("node"), Some(cookies))]);
}

mod resolve_attempts {
    use super::*;

    fn refused_resolve(cookies: bool) -> (Result<String, String>, Vec<String>) {
        let dir = test_dir();
        std::fs::write(dir.join("fail.txt"), "").unwrap();
        let cookie_file = dir.join("cookies.txt");
        std::fs::write(&cookie_file, "youtube.com\tFALSE\t/\tFALSE\t0\tsynthetic\tx\n").unwrap();
        let progress = std::sync::Mutex::new(Vec::new());
        let result = resolve(URL, cookies.then_some(cookie_file.as_path()), &AtomicBool::new(false), &|m: &str| {
            progress.lock().unwrap().push(m.to_string())
        });
        (result.map(|v| v.id), progress.into_inner().unwrap())
    }

    fn plain(extra: &str) -> String {
        format!("-J --no-playlist --no-warnings --no-progress --ignore-no-formats-error {extra}-- {URL}")
    }

    #[test]
    fn without_a_runtime_cookies_are_the_second_and_last_attempt_and_no_update_is_tried() {
        let dir = fresh_dir("resolve_without_runtime");
        if !in_child("resolve_attempts::without_a_runtime_cookies_are_the_second_and_last_attempt_and_no_update_is_tried", child_env(&dir)) {
            return;
        }

        let (result, progress) = refused_resolve(true);

        let cookie_file = test_dir().join("cookies.txt");
        assert_eq!(result, Err("[youtube] fake refused".to_string()));
        assert_eq!(calls(&test_dir()), vec![plain(""), plain(&format!("--cookies {} ", cookie_file.display()))]);
        assert_eq!(
            progress,
            ["Asking YouTube for the video's streams…", "Asking YouTube for the video's streams (with your cookies)…"]
        );
    }

    #[test]
    fn with_a_runtime_it_is_tried_alone_and_then_together_with_the_cookies() {
        let dir = fresh_dir("resolve_with_runtime");
        let env = child_env(&dir);
        put_runtime_on_path(&dir, "deno");
        if !in_child("resolve_attempts::with_a_runtime_it_is_tried_alone_and_then_together_with_the_cookies", env) {
            return;
        }

        let (result, progress) = refused_resolve(true);

        let cookie_file = test_dir().join("cookies.txt");
        assert_eq!(result, Err("[youtube] fake refused".to_string()));
        assert_eq!(
            calls(&test_dir()),
            vec![
                plain(""),
                plain("--js-runtimes deno "),
                plain(&format!("--js-runtimes deno --cookies {} ", cookie_file.display())),
            ]
        );
        assert!(!progress.iter().any(|m| m.contains("Updating")), "{progress:?}");
    }

    #[test]
    fn without_cookies_or_a_runtime_there_is_one_attempt() {
        let dir = fresh_dir("resolve_single");
        if !in_child("resolve_attempts::without_cookies_or_a_runtime_there_is_one_attempt", child_env(&dir)) {
            return;
        }

        let (result, progress) = refused_resolve(false);

        assert_eq!(result, Err("[youtube] fake refused".to_string()));
        assert_eq!(calls(&test_dir()), vec![plain("")]);
        assert_eq!(progress, ["Asking YouTube for the video's streams…"]);
    }
}

mod release_download {
    use super::*;

    fn release(sums: String, asset: &[u8]) -> RangeServer {
        RangeServer::start(vec![
            ("/rel/SHA2-256SUMS", Mode::IgnoresRange, sums.into_bytes()),
            ("/rel/yt-dlp_test.bin", Mode::IgnoresRange, asset.to_vec()),
        ])
    }

    #[test]
    fn a_build_matching_its_checksum_is_saved_whole() {
        let asset = b"#!fake yt-dlp build 2026.11.01".repeat(100);
        let server = release(format!("{}  yt-dlp_other\n{} *yt-dlp_test.bin\n", "0".repeat(64), sha256_hex(&asset)), &asset);
        let dest = fresh_dir("download_ok").join("nested").join("yt-dlp_test.bin");

        let result = download_ytdlp_from(&server.url("/rel/"), "yt-dlp_test.bin", &dest);

        assert_eq!(result, Ok(()));
        assert!(std::fs::read(&dest).unwrap() == asset, "the saved file differs from the download");
        assert!(!dest.with_extension("part").exists(), "the partial file is left behind");
    }

    #[test]
    fn a_build_not_matching_or_not_listed_is_refused_and_nothing_is_saved() {
        let asset = b"tampered build".to_vec();
        let cases = [
            ("wrong checksum", format!("{}  yt-dlp_test.bin\n", sha256_hex(b"the real build")), "the downloaded yt-dlp does not match its published checksum"),
            ("not listed", format!("{}  yt-dlp_other\n", sha256_hex(&asset)), "the yt-dlp release lists no checksum for this system"),
        ];
        for (n, (case, sums, expected)) in cases.into_iter().enumerate() {
            let server = release(sums, &asset);
            let dest = fresh_dir(&format!("download_refused_{n}")).join("yt-dlp_test.bin");

            let result = download_ytdlp_from(&server.url("/rel/"), "yt-dlp_test.bin", &dest);

            assert_eq!(result, Err(expected.to_string()), "{case}");
            assert!(!dest.exists() && !dest.with_extension("part").exists(), "{case}: a file was saved");
        }
    }
}

fn wait_for_status(player: &Player, what: &str, mut done: impl FnMut(&Status) -> bool) -> Status {
    let started = Instant::now();
    loop {
        let status = player.shared.status();
        if done(&status) {
            return status;
        }
        assert!(started.elapsed() < Duration::from_secs(30), "timed out waiting for {what}: {status:?}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn open_and_wait(player: &Player) -> Status {
    player.stop();
    wait_for_status(player, "idle", |s| s.state == State::Idle);
    player.open(URL);
    wait_for_status(player, "the video to load", |s| !matches!(s.state, State::Idle | State::Loading))
}

#[test]
fn player_opens_youtube_with_or_without_the_picture_and_asks_ytdlp_once_per_video() {
    let dir = fresh_dir("player_youtube");
    if !in_child("player_opens_youtube_with_or_without_the_picture_and_asks_ytdlp_once_per_video", child_env(&dir)) {
        return;
    }
    let dir = test_dir();
    let server = RangeServer::start(vec![
        ("/v.mp4", Mode::Ranged, fixture_bytes("dash_video.mp4")),
        ("/a.m4a", Mode::Ranged, fixture_bytes("dash_audio.m4a")),
    ]);
    let info = serde_json::json!({
        "id": "aB3_-x9Zq0Y", "title": "Fake video", "duration": 12.0,
        "formats": [
            { "format_id": "134", "url": server.url("/v.mp4"), "vcodec": "avc1.4d400d", "acodec": "none",
              "protocol": "https", "ext": "mp4", "width": 160, "height": 120, "fps": 25.0, "tbr": 100.0 },
            { "format_id": "140", "url": server.url("/a.m4a"), "vcodec": "none", "acodec": "mp4a.40.2",
              "protocol": "https", "ext": "m4a", "language": "en", "format_note": "English (default)",
              "language_preference": 10, "abr": 96.0 },
        ],
    });
    std::fs::write(dir.join("info.json"), info.to_string()).unwrap();
    let player = Player::new(Box::new(|| {}));
    player.shared.audio.set_volume(0.0);

    player.set_youtube_audio_only(true);
    let sound = open_and_wait(&player);
    let picture_requests_while_sound_only = server.requests().iter().filter(|r| r.path == "/v.mp4").count();
    player.set_youtube_audio_only(false);
    let both = open_and_wait(&player);
    player.stop();

    assert_eq!(sound.state, State::Playing, "{sound:?}");
    assert_eq!((sound.title.as_str(), sound.info.as_str()), ("Fake video", "YouTube · sound only"));
    assert_eq!((sound.video, sound.has_audio, sound.seekable), (None, true, true));
    assert_eq!(sound.youtube.as_ref().map(|v| v.id.as_str()), Some("aB3_-x9Zq0Y"));
    assert_eq!((sound.choice.video.as_deref(), sound.choice.audio.as_deref()), (None, Some("140")));
    assert_eq!(picture_requests_while_sound_only, 0);
    assert_eq!(both.state, State::Playing, "{both:?}");
    assert_eq!((both.info.as_str(), both.video, both.has_audio), ("YouTube · H.264 160×120", Some((160, 120)), true));
    assert_eq!((both.choice.video.as_deref(), both.choice.audio.as_deref()), (Some("134"), Some("140")));
    assert_eq!(calls(&dir).len(), 1, "the second open of the same video is served from the cache: {:?}", calls(&dir));
}

mod data_folder {
    use super::*;

    fn data_dir() -> PathBuf {
        test_dir().join("appdata").join("playr")
    }

    fn mixed_jar() -> CookieJar {
        cookies::parse(
            &[
                ".youtube.com\tTRUE\t/\tTRUE\t0\tyt\tsynthetic-1",
                "bank.example\tFALSE\t/\tTRUE\t0\tbank\tsynthetic-2",
                "accounts.google.com\tFALSE\t/\tTRUE\t0\tgg\tsynthetic-3",
            ]
            .join("\n"),
        )
        .unwrap()
    }

    #[test]
    fn the_chosen_cookie_file_is_remembered_until_forgotten_or_gone() {
        let dir = fresh_dir("saved_path");
        if !in_child("data_folder::the_chosen_cookie_file_is_remembered_until_forgotten_or_gone", child_env(&dir)) {
            return;
        }
        let chosen = test_dir().join("my cookies.txt");
        std::fs::write(&chosen, "# synthetic\n").unwrap();

        let before = cookies::saved_path();
        cookies::save_path(Some(&chosen)).unwrap();
        let remembered = cookies::saved_path();
        let choice_file = std::fs::read_to_string(data_dir().join("cookies-file.txt")).unwrap();
        std::fs::remove_file(&chosen).unwrap();
        let after_the_file_is_gone = cookies::saved_path();
        std::fs::write(&chosen, "# synthetic\n").unwrap();
        cookies::save_path(None).unwrap();
        let forgotten = cookies::saved_path();
        let forget_again = cookies::save_path(None);

        assert_eq!(before, None);
        assert_eq!(remembered, Some(chosen.clone()));
        assert_eq!(choice_file, chosen.to_string_lossy());
        assert_eq!(after_the_file_is_gone, None);
        assert_eq!(forgotten, None);
        assert!(!data_dir().join("cookies-file.txt").exists());
        assert_eq!(forget_again, Ok(()));
    }

    #[test]
    fn the_ytdlp_copy_holds_only_youtube_and_google_cookies_and_goes_away_when_there_are_none() {
        let dir = fresh_dir("ytdlp_copy");
        if !in_child("data_folder::the_ytdlp_copy_holds_only_youtube_and_google_cookies_and_goes_away_when_there_are_none", child_env(&dir)) {
            return;
        }
        let copy_path = data_dir().join("yt-dlp-cookies.txt");

        let written = cookies::ytdlp_copy(&mixed_jar());
        let copied = cookies::parse(&std::fs::read_to_string(&copy_path).unwrap()).unwrap();
        let bank_only = cookies::parse("bank.example\tFALSE\t/\tTRUE\t0\tbank\tsynthetic-2").unwrap();
        let none = cookies::ytdlp_copy(&bank_only);
        let stale_copy_left = copy_path.exists();
        cookies::ytdlp_copy(&mixed_jar());
        let removed = cookies::remove_ytdlp_copy();
        let removed_again = cookies::remove_ytdlp_copy();

        assert_eq!(written, Some(copy_path.clone()));
        let names: Vec<&str> = copied.cookies.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["yt", "gg"]);
        assert_eq!(none, None);
        assert!(!stale_copy_left, "the copy from before stayed behind");
        assert_eq!((removed, removed_again), (Ok(()), Ok(())));
        assert!(!copy_path.exists());
    }

    #[test]
    fn player_set_cookies_loads_and_remembers_a_file_and_none_forgets_it_and_deletes_the_copy() {
        let dir = fresh_dir("set_cookies");
        if !in_child("data_folder::player_set_cookies_loads_and_remembers_a_file_and_none_forgets_it_and_deletes_the_copy", child_env(&dir)) {
            return;
        }
        let chosen = test_dir().join("export.txt");
        std::fs::write(&chosen, mixed_jar().to_netscape()).unwrap();
        let copy_path = data_dir().join("yt-dlp-cookies.txt");
        let player = Player::new(Box::new(|| {}));

        let count = player.set_cookies(Some(chosen.clone()));
        let (path_after_set, saved_after_set) = (player.shared.cookies_path(), cookies::saved_path());
        cookies::ytdlp_copy(&mixed_jar());
        let copy_before_forget = copy_path.exists();
        let forgot = player.set_cookies(None);
        let not_cookies = test_dir().join("notes.txt");
        std::fs::write(&not_cookies, "just text").unwrap();
        let wrong_file = player.set_cookies(Some(not_cookies));

        assert_eq!(count, Ok(3));
        assert_eq!((path_after_set, saved_after_set), (Some(chosen.clone()), Some(chosen)));
        assert!(copy_before_forget);
        assert_eq!(forgot, Ok(0));
        assert_eq!((player.shared.cookies_path(), cookies::saved_path()), (None, None));
        assert!(!copy_path.exists(), "the yt-dlp copy of the cookies stayed behind");
        assert_eq!(wrong_file, Err("no cookies found; expected a Netscape cookies.txt file (tab-separated)".to_string()));
        assert_eq!(player.shared.cookies_path(), None);
    }

    #[test]
    fn player_set_cookies_none_forgets_the_file_even_when_the_copy_cannot_be_deleted() {
        let dir = fresh_dir("set_cookies_undeletable_copy");
        if !in_child("data_folder::player_set_cookies_none_forgets_the_file_even_when_the_copy_cannot_be_deleted", child_env(&dir)) {
            return;
        }
        let chosen = test_dir().join("export.txt");
        std::fs::write(&chosen, mixed_jar().to_netscape()).unwrap();
        let copy_path = data_dir().join("yt-dlp-cookies.txt");
        let player = Player::new(Box::new(|| {}));
        player.set_cookies(Some(chosen)).unwrap();
        std::fs::create_dir_all(copy_path.join("in the way")).unwrap();

        let forgot = player.set_cookies(None);

        let prefix = format!("cannot delete {}: ", copy_path.display());
        assert!(forgot.as_ref().is_err_and(|e| e.starts_with(&prefix)), "{forgot:?}");
        assert_eq!((player.shared.cookies_path(), cookies::saved_path()), (None, None));
        assert!(!data_dir().join("cookies-file.txt").exists());
    }
}

#[test]
#[cfg(any(windows, target_os = "linux"))]
fn a_managed_ytdlp_that_fails_is_updated_once_per_run_and_asked_again() {
    let dir = fresh_dir("managed_update");
    if !in_child("a_managed_ytdlp_that_fails_is_updated_once_per_run_and_asked_again", private_env(&dir)) {
        return;
    }
    let managed = playr::youtube::managed_ytdlp_path().expect("data folder");
    let exe_name = if cfg!(windows) { "yt-dlp.exe" } else { "yt-dlp" };
    assert_eq!(managed, test_dir().join("appdata").join("playr").join(exe_name));
    install_exe_fake(managed.parent().unwrap(), &serde_json::json!({}));
    let ask = |progress: &std::sync::Mutex<Vec<String>>| {
        resolve(URL, None, &AtomicBool::new(false), &|m: &str| progress.lock().unwrap().push(m.to_string()))
    };

    let first_progress = std::sync::Mutex::new(Vec::new());
    let first = ask(&first_progress).map(|v| v.id);
    let calls_after_first = calls(managed.parent().unwrap());
    let second_progress = std::sync::Mutex::new(Vec::new());
    let second = ask(&second_progress).map(|v| v.id);
    let calls_after_second = calls(managed.parent().unwrap());

    let refused = Err("[youtube] aB3_-x9Zq0Y: Video unavailable".to_string());
    let json = format!("-J --no-playlist --no-warnings --no-progress --ignore-no-formats-error -- {URL}");
    assert_eq!((first, second), (refused.clone(), refused));
    assert_eq!(calls_after_first, vec![json.clone(), "-U".to_string(), json.clone()]);
    assert_eq!(calls_after_second[3..], [json]);
    assert_eq!(
        first_progress.into_inner().unwrap(),
        ["Asking YouTube for the video's streams…", "Updating yt-dlp…", "Asking YouTube for the video's streams…"]
    );
    assert_eq!(second_progress.into_inner().unwrap(), ["Asking YouTube for the video's streams…"]);
}

const SPOTIFY_LIST: &str = "https://open.spotify.com/playlist/1PdmdWWClhyLiIkK1fZeuV";

fn embed_page(name: &str, numbers: &[&str]) -> String {
    let tracks: Vec<serde_json::Value> = numbers
        .iter()
        .map(|n| serde_json::json!({ "title": format!("Title {n}"), "subtitle": format!("Artist {n}"), "duration": 12_000 }))
        .collect();
    let entity = serde_json::json!({ "name": name, "trackList": tracks });
    let data = serde_json::json!({ "props": { "pageProps": { "state": { "data": { "entity": entity } } } } });
    format!("<html><body><script id=\"__NEXT_DATA__\" type=\"application/json\">{data}</script></body></html>")
}

fn sound_only_info(id: &str, sound_url: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id, "title": format!("Video {id}"), "duration": 12.0,
        "formats": [{ "format_id": "140", "url": sound_url, "vcodec": "none", "acodec": "mp4a.40.2",
                      "protocol": "https", "ext": "m4a", "language": "en", "abr": 96.0 }],
    })
}

#[test]
#[cfg(any(windows, target_os = "linux"))]
fn the_next_spotify_entry_plays_from_its_prefetched_lookup_without_asking_ytdlp_again() {
    let list = "https://open.spotify.com/playlist/2PdmdWWClhyLiIkK1fZeuV";
    let test = "the_next_spotify_entry_plays_from_its_prefetched_lookup_without_asking_ytdlp_again";
    if !is_child() {
        let dir = fresh_dir("spotify_prefetch");
        let server = RangeServer::start(vec![
            ("/embed/playlist/2PdmdWWClhyLiIkK1fZeuV", Mode::IgnoresRange, embed_page("Two Songs", &["One", "Two"]).into_bytes()),
            ("/a.m4a", Mode::Ranged, fixture_bytes("dash_audio.m4a")),
        ]);
        let found = |id: &str| serde_json::json!([{ "id": id, "duration": 12.0 }]);
        let script = serde_json::json!({
            "search": { "Artist One Title One": found("AAAAAAAAAA1"), "Artist Two Title Two": found("DDDDDDDDDD4") },
            "videos": {
                "AAAAAAAAAA1": sound_only_info("AAAAAAAAAA1", &server.url("/a.m4a")),
                "DDDDDDDDDD4": sound_only_info("DDDDDDDDDD4", &server.url("/a.m4a")),
            },
        });
        let mut env = private_env(&dir);
        env.push(("PLAYR_YTDLP", install_exe_fake(&dir.join("fake"), &script).into_os_string()));
        env.push((playr::spotify::EMBED_ENV, OsString::from(server.url("/embed"))));
        run_child(test, env);
        return;
    }
    let player = Player::new(Box::new(|| {}));
    player.shared.audio.set_volume(0.0);
    player.set_speed(2.0);

    player.open(list);
    let mut played = Vec::new();
    let last = wait_for_status(&player, "the second entry to play", |s| {
        if s.state == State::Playing && s.source == list && played.last() != Some(&s.entry) {
            played.push(s.entry);
        }
        s.state == State::Playing && s.entry == 1
    });
    player.stop();

    assert_eq!(played, [0, 1]);
    assert_eq!((last.title.as_str(), last.notes.len()), ("Title Two — Artist Two", 0));
    let calls = calls(&test_dir().join("fake"));
    let count = |needle: &str| calls.iter().filter(|c| c.contains(needle)).count();
    assert_eq!(count("ytsearch5:Artist Two Title Two"), 1, "{calls:#?}");
    assert_eq!(calls.iter().filter(|c| c.starts_with("-J ") && c.contains("DDDDDDDDDD4")).count(), 1, "{calls:#?}");
    assert_eq!(calls.iter().filter(|c| c.starts_with("-J ") && c.contains("AAAAAAAAAA1")).count(), 1, "{calls:#?}");
}

#[test]
#[cfg(any(windows, target_os = "linux"))]
fn a_spotify_list_plays_on_to_the_next_entry_and_skips_one_youtube_refuses_with_a_note() {
    let test = "a_spotify_list_plays_on_to_the_next_entry_and_skips_one_youtube_refuses_with_a_note";
    if !is_child() {
        let dir = fresh_dir("spotify_list");
        let server = RangeServer::start(vec![
            ("/embed/playlist/1PdmdWWClhyLiIkK1fZeuV", Mode::IgnoresRange, embed_page("Road Trip", &["One", "Two", "Three"]).into_bytes()),
            ("/a.m4a", Mode::Ranged, fixture_bytes("dash_audio.m4a")),
        ]);
        let found = |id: &str| serde_json::json!([{ "id": id, "duration": 12.0 }]);
        let script = serde_json::json!({
            "search": {
                "Artist One Title One": found("AAAAAAAAAA1"),
                "Artist Two Title Two": found("BBBBBBBBBB2"),
                "Artist Three Title Three": found("CCCCCCCCCC3"),
            },
            "videos": {
                "AAAAAAAAAA1": sound_only_info("AAAAAAAAAA1", &server.url("/a.m4a")),
                "CCCCCCCCCC3": sound_only_info("CCCCCCCCCC3", &server.url("/a.m4a")),
            },
        });
        let mut env = private_env(&dir);
        env.push(("PLAYR_YTDLP", install_exe_fake(&dir.join("fake"), &script).into_os_string()));
        env.push((playr::spotify::EMBED_ENV, OsString::from(server.url("/embed"))));
        run_child(test, env);
        return;
    }
    let player = Player::new(Box::new(|| {}));
    player.shared.audio.set_volume(0.0);

    player.open(SPOTIFY_LIST);
    let mut played = Vec::new();
    let mut seeked = false;
    let last = wait_for_status(&player, "the third entry to play", |s| {
        if s.state == State::Playing && s.source == SPOTIFY_LIST && played.last() != Some(&s.entry) {
            played.push(s.entry);
        }
        if s.state == State::Playing && s.entry == 0 && !seeked {
            player.seek(11.5);
            seeked = true;
        }
        s.state == State::Playing && s.entry == 2
    });
    player.stop();

    assert_eq!(played, [0, 2]);
    assert_eq!(last.title, "Title Three — Artist Three");
    assert_eq!(last.notes, ["skipped Title Two — Artist Two: [youtube] BBBBBBBBBB2: Video unavailable"]);
    assert_eq!(last.info, "Spotify · Road Trip · 3/3 · sound from YouTube");
    assert_eq!(last.playlist.as_ref().map(|p| p.entries.len()), Some(3));
    let calls = calls(&test_dir().join("fake"));
    let searched: Vec<&str> = calls.iter().filter_map(|c| c.split("ytsearch5:").nth(1)).collect();
    assert_eq!(searched.first(), Some(&"Artist One Title One"));
    assert_eq!(searched.last(), Some(&"Artist Three Title Three"));
    assert!(searched.contains(&"Artist Two Title Two"), "{searched:?}");
    let asked = |id: &str| calls.iter().filter(|c| c.starts_with("-J ") && c.contains(id)).count();
    assert_eq!((asked("AAAAAAAAAA1"), asked("CCCCCCCCCC3")), (1, 1));
}
