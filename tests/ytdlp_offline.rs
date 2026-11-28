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

/// The environment of a child: its folder, a private data folder and a `PATH`
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
        if script["fail"].as_array().is_some_and(|fail| fail.iter().any(|q| q == query)) {
            eprintln!("ERROR: [youtube:search] {query}: search refused");
            std::process::exit(1);
        }
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
        // Shell builtins only: the child's PATH holds just the test's `bin`, so there is no
        // dirname or cat to run.
        let script = "#!/bin/sh\nd=${0%/*}\necho \"$*\" >> \"$d/calls.log\"\nif [ -e \"$d/fail.txt\" ]; then echo 'ERROR: [youtube] fake refused' >&2; exit 1; fi\nwhile IFS= read -r line || [ -n \"$line\" ]; do printf '%s\\n' \"$line\"; done < \"$d/info.json\"\n";
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

    player.set_audio_only(true);
    let sound = open_and_wait(&player);
    let picture_requests_while_sound_only = server.requests().iter().filter(|r| r.path == "/v.mp4").count();
    player.set_audio_only(false);
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

fn search_call(query: &str) -> String {
    format!("-J --flat-playlist --no-warnings --no-progress -- ytsearch5:{query}")
}

#[test]
#[cfg(any(windows, target_os = "linux"))]
fn youtube_search_tries_the_queries_in_order_until_one_finds_a_result_of_the_right_length() {
    let test = "youtube_search_tries_the_queries_in_order_until_one_finds_a_result_of_the_right_length";
    if !is_child() {
        let dir = fresh_dir("search_queries");
        let hit = |id: &str, secs: f64| serde_json::json!([{ "id": id, "duration": secs }]);
        let script = serde_json::json!({ "search": {
            "a full": [], "a short": hit("A2short0000", 300.0),
            "b full": hit("B1toolong00", 900.0), "b short": hit("B2fits00000", 310.0),
            "c full": hit("C1toolong00", 900.0), "c short": hit("C2toolong00", 950.0), "c shortest": [],
            "d full": [], "d short": [], "d shortest": [],
            "e full": hit("E1anylen000", 900.0), "e short": hit("E2unused000", 300.0),
            "f full": hit("F1toolong00", 900.0), "f shortest": hit("F3unused000", 300.0),
            "g full": [{ "id": "G1nolength0" }], "g short": hit("G3unused000", 300.0),
            "h short": hit("H2unused000", 300.0),
        }, "fail": ["f short", "h full"] });
        let mut env = private_env(&dir);
        env.push(("PLAYR_YTDLP", install_exe_fake(&dir.join("fake"), &script).into_os_string()));
        run_child(test, env);
        return;
    }
    let fake = test_dir().join("fake");
    let watch = |id: &str| Ok(format!("https://www.youtube.com/watch?v={id}"));
    let cases: [(&str, Option<f64>, Result<String, String>, &[&str]); 8] = [
        ("full finds nothing", Some(300.0), watch("A2short0000"), &["a full", "a short"]),
        ("full finds the wrong length", Some(300.0), watch("B2fits00000"), &["b full", "b short"]),
        ("nothing fits", Some(300.0), watch("C1toolong00"), &["c full", "c short", "c shortest"]),
        ("nothing at all", Some(300.0), Err("nothing found on YouTube for \u{201c}d full\u{201d}".into()), &["d full", "d short", "d shortest"]),
        ("no expected length", None, watch("E1anylen000"), &["e full"]),
        ("a later query fails after a wrong length", Some(300.0), watch("F1toolong00"), &["f full", "f short"]),
        ("unknown length", Some(300.0), watch("G1nolength0"), &["g full"]),
        ("the first query fails", Some(300.0), Err("[youtube:search] h full: search refused".into()), &["h full"]),
    ];
    let mut bad = Vec::new();
    for (case, expected_secs, expected, searched) in cases {
        let prefix = &searched[0][..2];
        let queries: Vec<String> = ["full", "short", "shortest"].iter().map(|q| format!("{prefix}{q}")).collect();
        let before = calls(&fake).len();
        let actual = playr::youtube::search(&queries, expected_secs, &AtomicBool::new(false), &|_: &str| {});
        let made: Vec<String> = calls(&fake)[before..].to_vec();
        let wanted: Vec<String> = searched.iter().map(|q| search_call(q)).collect();
        expect!(bad, actual == expected, "{case}: {actual:?}, expected {expected:?}");
        expect!(bad, made == wanted, "{case}: searched {made:?}\n    expected {wanted:?}");
    }
    common::finish(bad);
}

const PODCAST_SHOW: &str = "Zosia Cebulka - bajki do snu i zabawy";
const PODCAST_LONG_TITLE: &str = "[Łagodny początek] LIST DO ŚWIĘTEGO MIKOŁAJA 2 | Wyciszająca bajka";

fn episode_embed_page(title: &str, subtitle: &str, duration_ms: u64) -> Vec<u8> {
    let entity = serde_json::json!({ "type": "episode", "title": title, "subtitle": subtitle, "duration": duration_ms });
    let data = serde_json::json!({ "props": { "pageProps": { "state": { "data": { "entity": entity } } } } });
    format!("<html><body><script id=\"__NEXT_DATA__\" type=\"application/json\">{data}</script></body></html>").into_bytes()
}

struct PodcastSite {
    audio: RangeServer,
    feed: RangeServer,
    spotify: RangeServer,
}

impl PodcastSite {
    fn start(embed_path: &str, embed: Vec<u8>, show_in_directory: &str) -> Self {
        Self::start_with(embed_path, embed, show_in_directory, [None, None, None])
    }

    fn start_with(embed_path: &str, embed: Vec<u8>, show_in_directory: &str, enclosures: [Option<String>; 3]) -> Self {
        let audio = RangeServer::start(vec![
            ("/ep/a.mp3", Mode::Ranged, fixture_bytes("sine.mp3")),
            ("/ep/b.mp3", Mode::Ranged, fixture_bytes("sine.mp3")),
            ("/ep/c.mp3", Mode::Ranged, fixture_bytes("radio.mp3")),
            ("/a.m4a", Mode::Ranged, fixture_bytes("dash_audio.m4a")),
        ]);
        let rss = format!(
            r#"<rss xmlns:itunes="http://www.itunes.com/dtds/podcast-1.0.dtd"><channel><title>Feed name</title>
<item><title>Pierwsza bajka</title><enclosure url="{}" type="audio/mpeg"/><itunes:duration>3</itunes:duration></item>
<item><title>Bajka o smoku, część 2</title><enclosure url="{}" type="audio/mpeg"/><itunes:duration>00:00:03</itunes:duration></item>
<item><title>Kołysanka</title><enclosure url="{}" type="audio/mpeg"/><itunes:duration>4</itunes:duration></item>
</channel></rss>"#,
            enclosures[0].clone().unwrap_or_else(|| audio.url("/ep/a.mp3")),
            enclosures[1].clone().unwrap_or_else(|| audio.url("/ep/b.mp3")),
            enclosures[2].clone().unwrap_or_else(|| audio.url("/ep/c.mp3"))
        );
        let feed = RangeServer::start(vec![("/feed.xml", Mode::IgnoresRange, rss.into_bytes())]);
        let answer = serde_json::json!({ "resultCount": 1, "results": [
            { "kind": "podcast", "collectionName": show_in_directory, "feedUrl": feed.url("/feed.xml") },
        ]});
        let spotify = RangeServer::start(vec![
            (embed_path, Mode::IgnoresRange, embed),
            ("/search", Mode::IgnoresRange, answer.to_string().into_bytes()),
        ]);
        Self { audio, feed, spotify }
    }

    fn env(&self, dir: &Path, script: &serde_json::Value) -> Vec<(&'static str, OsString)> {
        let mut env = private_env(dir);
        env.push(("PLAYR_YTDLP", install_exe_fake(&dir.join("fake"), script).into_os_string()));
        env.push((playr::spotify::EMBED_ENV, OsString::from(self.spotify.url("/embed"))));
        env.push((playr::podcast::SEARCH_ENV, OsString::from(self.spotify.url("/search"))));
        env
    }
}

fn play_entries_until(player: &Player, link: &str, last: usize) -> Vec<Status> {
    player.open(link);
    let mut played: Vec<Status> = Vec::new();
    wait_for_status(player, "the last entry to play", |s| {
        if s.state == State::Playing && s.source == link && played.last().map(|p| p.entry) != Some(s.entry) {
            played.push(s.clone());
        }
        s.state == State::Playing && s.entry == last
    });
    player.stop();
    played
}

fn synthetic_cookie_file() -> PathBuf {
    let path = test_dir().join("cookies.txt");
    std::fs::write(&path, "127.0.0.1\tFALSE\t/\tFALSE\t0\tsp_dc\tsynthetic-not-a-secret\n").unwrap();
    path
}

#[test]
#[cfg(any(windows, target_os = "linux"))]
fn a_spotify_show_plays_its_feed_episodes_in_order_seekably_without_ytdlp_or_cookies_off_spotify() {
    let test = "a_spotify_show_plays_its_feed_episodes_in_order_seekably_without_ytdlp_or_cookies_off_spotify";
    if !is_child() {
        let dir = fresh_dir("podcast_show");
        let site = PodcastSite::start("/embed/show/033OZDMSatg3WCetEI9bge", episode_embed_page("Kołysanka", PODCAST_SHOW, 4_000), PODCAST_SHOW);
        run_child(test, site.env(&dir, &serde_json::json!({})));
        assert_eq!(calls(&dir.join("fake")), Vec::<String>::new());
        let embed_cookie = site.spotify.requests().iter().find(|r| r.path.starts_with("/embed/")).map(|r| r.header("cookie").map(str::to_string));
        assert_eq!(embed_cookie, Some(Some("sp_dc=synthetic-not-a-secret".to_string())));
        let off_spotify: Vec<_> = [site.audio.requests(), site.feed.requests(), site.spotify.requests().into_iter().filter(|r| r.path.starts_with("/search")).collect()].concat();
        assert!(off_spotify.iter().any(|r| r.path == "/ep/b.mp3"), "{off_spotify:?}");
        let with_cookie: Vec<&str> = off_spotify.iter().filter(|r| r.header("cookie").is_some()).map(|r| r.path.as_str()).collect();
        assert_eq!(with_cookie, Vec::<&str>::new());
        return;
    }
    let player = Player::new(Box::new(|| {}));
    player.shared.audio.set_volume(0.0);
    player.set_speed(2.0);
    player.set_cookies(Some(synthetic_cookie_file())).unwrap();

    let played = play_entries_until(&player, "https://open.spotify.com/show/033OZDMSatg3WCetEI9bge", 1);

    let seen: Vec<(usize, &str, &str, bool)> = played.iter().map(|s| (s.entry, s.title.as_str(), s.info.as_str(), s.seekable)).collect();
    assert_eq!(
        seen,
        [
            (0, "Pierwsza bajka — Zosia Cebulka - bajki do snu i zabawy", "Spotify · Zosia Cebulka - bajki do snu i zabawy · 1/3 · podcast feed", true),
            (1, "Bajka o smoku, część 2 — Zosia Cebulka - bajki do snu i zabawy", "Spotify · Zosia Cebulka - bajki do snu i zabawy · 2/3 · podcast feed", true),
        ]
    );
    assert_eq!(played[1].notes, Vec::<String>::new());
}

#[test]
#[cfg(any(windows, target_os = "linux"))]
fn a_spotify_episode_link_starts_at_that_episode_of_the_feed_and_plays_on_to_the_next() {
    let test = "a_spotify_episode_link_starts_at_that_episode_of_the_feed_and_plays_on_to_the_next";
    if !is_child() {
        let dir = fresh_dir("podcast_episode");
        let page = episode_embed_page("BAJKA O SMOKU – część 2!", PODCAST_SHOW, 3_000);
        let site = PodcastSite::start("/embed/episode/4kGynGaUBQwwRI3xTvpDPN", page, PODCAST_SHOW);
        run_child(test, site.env(&dir, &serde_json::json!({})));
        assert_eq!(calls(&dir.join("fake")), Vec::<String>::new());
        let requested: Vec<String> = site.audio.requests().iter().map(|r| r.path.clone()).collect();
        assert!(!requested.is_empty());
        assert!(!requested.iter().any(|p| p == "/ep/a.mp3"), "{requested:?}");
        return;
    }
    let player = Player::new(Box::new(|| {}));
    player.shared.audio.set_volume(0.0);
    player.set_speed(2.0);

    let played = play_entries_until(&player, "https://open.spotify.com/episode/4kGynGaUBQwwRI3xTvpDPN", 2);

    let seen: Vec<(usize, &str)> = played.iter().map(|s| (s.entry, s.info.as_str())).collect();
    assert_eq!(
        seen,
        [
            (1, "Spotify · Zosia Cebulka - bajki do snu i zabawy · 2/3 · podcast feed"),
            (2, "Spotify · Zosia Cebulka - bajki do snu i zabawy · 3/3 · podcast feed"),
        ]
    );
    assert_eq!(played[1].title, "Kołysanka — Zosia Cebulka - bajki do snu i zabawy");
    assert_eq!(played[0].playlist.as_ref().map(|p| p.start), Some(1));
}

#[test]
#[cfg(any(windows, target_os = "linux"))]
fn a_spotify_show_missing_from_the_directory_plays_from_youtube_found_by_a_shorter_query() {
    let test = "a_spotify_show_missing_from_the_directory_plays_from_youtube_found_by_a_shorter_query";
    let short = "Zosia Cebulka LIST DO ŚWIĘTEGO MIKOŁAJA 2";
    if !is_child() {
        let dir = fresh_dir("podcast_fallback");
        let page = episode_embed_page(PODCAST_LONG_TITLE, PODCAST_SHOW, 12_000);
        let site = PodcastSite::start("/embed/show/033OZDMSatg3WCetEI9bge", page, "Zosia Cebulka");
        let script = serde_json::json!({
            "search": { short: [{ "id": "ZZZZZZZZZZ9", "duration": 12.0 }] },
            "videos": { "ZZZZZZZZZZ9": sound_only_info("ZZZZZZZZZZ9", &site.audio.url("/a.m4a")) },
        });
        run_child(test, site.env(&dir, &script));
        let made = calls(&dir.join("fake"));
        let searched: Vec<&String> = made.iter().filter(|c| c.contains("ytsearch5:")).collect();
        assert_eq!(searched, [&search_call(&format!("{PODCAST_SHOW} {PODCAST_LONG_TITLE}")), &search_call(short)]);
        assert_eq!(site.feed.requests().len(), 0);
        return;
    }
    let player = Player::new(Box::new(|| {}));
    player.shared.audio.set_volume(0.0);

    let played = play_entries_until(&player, "https://open.spotify.com/show/033OZDMSatg3WCetEI9bge", 0);

    assert_eq!(played.len(), 1);
    assert_eq!(played[0].title, format!("{PODCAST_LONG_TITLE} — {PODCAST_SHOW}"));
    assert_eq!(played[0].info, format!("Spotify · {PODCAST_SHOW} · 1/1 · sound from YouTube"));
}

#[test]
#[cfg(any(windows, target_os = "linux"))]
fn a_missing_episode_file_at_the_start_of_an_episode_link_is_skipped_with_a_note_naming_it() {
    let test = "a_missing_episode_file_at_the_start_of_an_episode_link_is_skipped_with_a_note_naming_it";
    if !is_child() {
        let dir = fresh_dir("podcast_episode_gone");
        let page = episode_embed_page("Bajka o smoku, część 2", PODCAST_SHOW, 3_000);
        let gone = RangeServer::start(vec![]);
        let gone_url = gone.url("/ep/gone.mp3");
        let site = PodcastSite::start_with("/embed/episode/4kGynGaUBQwwRI3xTvpDPN", page, PODCAST_SHOW, [None, Some(gone_url.clone()), None]);
        let mut env = site.env(&dir, &serde_json::json!({}));
        env.push(("PLAYR_TEST_GONE", OsString::from(gone_url)));
        run_child(test, env);
        assert_eq!(calls(&dir.join("fake")), Vec::<String>::new());
        return;
    }
    let gone_url = std::env::var("PLAYR_TEST_GONE").unwrap();
    let player = Player::new(Box::new(|| {}));
    player.shared.audio.set_volume(0.0);

    let played = play_entries_until(&player, "https://open.spotify.com/episode/4kGynGaUBQwwRI3xTvpDPN", 2);

    let seen: Vec<(usize, &str)> = played.iter().map(|s| (s.entry, s.title.as_str())).collect();
    assert_eq!(seen, [(2, "Kołysanka — Zosia Cebulka - bajki do snu i zabawy")]);
    assert_eq!(
        played[0].notes,
        [format!("skipped Bajka o smoku, część 2 — Zosia Cebulka - bajki do snu i zabawy: cannot open {gone_url}: http status: 404")]
    );
}

fn slow_not_found_server(delay: Duration) -> String {
    let listener = {
        let _ports = common::http::PORTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        std::net::TcpListener::bind("127.0.0.1:0").expect("bind slow server")
    };
    let url = format!("http://{}/ep/slow.mp3", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { continue };
            std::thread::spawn(move || {
                use std::io::{Read, Write};
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") && conn.read(&mut byte).is_ok_and(|n| n == 1) {
                    head.push(byte[0]);
                }
                std::thread::sleep(delay);
                let _ = conn.write_all(b"HTTP/1.0 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            });
        }
    });
    url
}

#[test]
#[cfg(any(windows, target_os = "linux"))]
fn while_a_feed_episode_opens_the_title_says_opening_it_not_finding_it_on_youtube() {
    let test = "while_a_feed_episode_opens_the_title_says_opening_it_not_finding_it_on_youtube";
    if !is_child() {
        let dir = fresh_dir("podcast_opening_title");
        let page = episode_embed_page("Kołysanka", PODCAST_SHOW, 4_000);
        let slow = slow_not_found_server(Duration::from_millis(400));
        let site = PodcastSite::start_with("/embed/show/033OZDMSatg3WCetEI9bge", page, PODCAST_SHOW, [Some(slow), None, None]);
        run_child(test, site.env(&dir, &serde_json::json!({})));
        return;
    }
    let link = "https://open.spotify.com/show/033OZDMSatg3WCetEI9bge";
    let player = Player::new(Box::new(|| {}));
    player.shared.audio.set_volume(0.0);

    player.open(link);
    let mut loading_titles: Vec<String> = Vec::new();
    let last = wait_for_status(&player, "the second entry to play", |s| {
        if s.state == State::Loading && s.source == link && loading_titles.last() != Some(&s.title) {
            loading_titles.push(s.title.clone());
        }
        s.state == State::Playing && s.entry == 1
    });
    player.stop();

    let opening_first = "Opening \u{201c}Pierwsza bajka — Zosia Cebulka - bajki do snu i zabawy\u{201d}…".to_string();
    assert!(loading_titles.contains(&opening_first), "{loading_titles:?}");
    assert!(!loading_titles.iter().any(|t| t.starts_with("Finding")), "{loading_titles:?}");
    assert_eq!(last.info, "Spotify · Zosia Cebulka - bajki do snu i zabawy · 2/3 · podcast feed");
}

mod blocked_by_network {
    use super::*;

    /// A yt-dlp stand-in that always fails, writing `stderr.txt` from its folder to stderr.
    fn failing_ytdlp(dir: &Path, stderr: &str) -> PathBuf {
        std::fs::write(dir.join("stderr.txt"), stderr).unwrap();
        if cfg!(windows) {
            let path = dir.join("yt-dlp.cmd");
            std::fs::write(&path, "@echo off\r\ntype \"%~dp0stderr.txt\" 1>&2\r\nexit /b 1\r\n").unwrap();
            path
        } else {
            let path = dir.join("yt-dlp");
            // Shell builtins only, like `fake_ytdlp`: no cat or dirname on the child's PATH.
            let script = "#!/bin/sh\nwhile IFS= read -r line || [ -n \"$line\" ]; do printf '%s\\n' \"$line\" >&2; done < \"${0%/*}/stderr.txt\"\nexit 1\n";
            std::fs::write(&path, script).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            path
        }
    }

    /// Runs `test` in a child whose yt-dlp fails with `stderr`.
    fn in_child_failing_with(test: &str, folder: &str, stderr: &str) -> bool {
        if is_child() {
            return true;
        }
        let dir = fresh_dir(folder);
        let mut env = private_env(&dir);
        env.push(("PLAYR_YTDLP", failing_ytdlp(&dir, stderr).into_os_string()));
        run_child(test, env);
        false
    }

    fn failed_open(player: &Player) -> Status {
        player.open(URL);
        wait_for_status(player, "the video to fail", |s| s.source == URL && !matches!(s.state, State::Idle | State::Loading))
    }

    #[test]
    fn a_youtube_video_behind_a_web_filter_fails_with_the_blocked_message() {
        let stderr = "ERROR: [youtube] aB3_-x9Zq0Y: Unable to download webpage: Failed to perform, curl: (60) SSL certificate OpenSSL verify result: unable to get local issuer certificate (20). See https://curl.se/libcurl/c/libcurl-errors.html first for more details.\n";
        if !in_child_failing_with("blocked_by_network::a_youtube_video_behind_a_web_filter_fails_with_the_blocked_message", "player_blocked", stderr) {
            return;
        }
        let player = Player::new(Box::new(|| {}));
        player.shared.audio.set_volume(0.0);

        let status = failed_open(&player);

        assert_eq!(status.state, State::Failed, "{status:?}");
        let error = status.error.unwrap_or_default();
        assert!(error.starts_with("Blocked by your network"), "{error}");
        // The same explanation whichever tool reported the interception.
        assert_eq!(Some(error), playr::net::blocked_by_network("invalid peer certificate: UnknownIssuer"));
    }

    #[test]
    fn an_expired_certificate_keeps_ytdlps_own_error() {
        let stderr = "ERROR: [youtube] aB3_-x9Zq0Y: Unable to download webpage: [SSL: CERTIFICATE_VERIFY_FAILED] certificate verify failed: certificate has expired (_ssl.c:1006)\n";
        if !in_child_failing_with("blocked_by_network::an_expired_certificate_keeps_ytdlps_own_error", "player_expired", stderr) {
            return;
        }
        let player = Player::new(Box::new(|| {}));
        player.shared.audio.set_volume(0.0);

        let status = failed_open(&player);

        assert_eq!(status.state, State::Failed, "{status:?}");
        let error = status.error.unwrap_or_default();
        // How the player words the rest of the error is not part of this rule; yt-dlp's reason must survive.
        assert!(
            error.contains("certificate verify failed: certificate has expired") && !error.starts_with("Blocked by your network"),
            "{error}"
        );
    }
}
