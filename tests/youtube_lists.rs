//! YouTube playlists: `playr::youtube::list_id` / `parse_playlist` / `playlist`, and a `Player`
//! stepping through a playlist link, without the network.
//!
//! yt-dlp is played by a copy of this test binary named yt-dlp (see `act_as_fake_ytdlp`), named
//! by `PLAYR_YTDLP`. Environment variables belong to the whole process, so every test that needs
//! the fake runs its body in a fresh copy of this test binary started with that environment.

mod common;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use common::finish;
use common::fixture_bytes;
use common::range_http::{Mode, RangeServer};
use playr::history::Item;
use playr::player::{Player, State, Status};
use playr::spotify::{Entry, Playlist};
use playr::youtube::{Choice, is_youtube_list_url, list_id, parse_playlist};
use serde_json::{Value, json};

const CHILD: &str = "PLAYR_TEST_CHILD";
const DIR: &str = "PLAYR_TEST_DIR";

mod list_ids {
    use super::*;

    #[test]
    fn watch_and_playlist_pages_on_the_youtube_hosts_give_their_list_id() {
        let cases = [
            ("watch page", "https://www.youtube.com/watch?v=aB3_-x9Zq0Y&list=PLabc-DEF_123", "PLabc-DEF_123"),
            ("list before v", "https://www.youtube.com/watch?list=PLfirst99&v=aB3_-x9Zq0Y&index=2", "PLfirst99"),
            ("playlist page", "https://youtube.com/playlist?list=PLpage", "PLpage"),
            ("mobile host", "https://m.youtube.com/watch?v=aB3_-x9Zq0Y&list=PLmobile", "PLmobile"),
            ("music host, mix", "https://music.youtube.com/watch?v=aB3_-x9Zq0Y&list=RDAMVMaB3_-x9Zq0Y", "RDAMVMaB3_-x9Zq0Y"),
            ("http and an upper-case host", "http://WWW.YouTube.COM/playlist?list=PLcase", "PLcase"),
            ("port and user", "https://user@www.youtube.com:443/playlist?list=PLport", "PLport"),
            ("fragment after the list", "https://www.youtube.com/playlist?list=PLfrag#t=30", "PLfrag"),
            ("two-character id", "https://www.youtube.com/playlist?list=WL", "WL"),
            ("spaces around", "  https://www.youtube.com/playlist?list=PLtrim \n", "PLtrim"),
            ("short link with a list", "https://youtu.be/aB3_-x9Zq0Y?list=PLshort&t=4", "PLshort"),
        ];
        let mut bad = Vec::new();
        for (case, url, id) in cases {
            expect!(bad, list_id(url).as_deref() == Some(id), "{case}: {:?}", list_id(url));
            expect!(bad, is_youtube_list_url(url), "{case}: not a list URL");
        }
        finish(bad);
    }

    #[test]
    fn other_addresses_and_bad_list_ids_give_none() {
        let cases = [
            ("no list parameter", "https://www.youtube.com/watch?v=aB3_-x9Zq0Y"),
            ("short link without a valid video id", "https://youtu.be/short?list=PLshort"),
            ("short link without a video", "https://youtu.be/?list=PLshort"),
            ("shorts path", "https://www.youtube.com/shorts/aB3_-x9Zq0Y?list=PLshorts"),
            ("embed path", "https://www.youtube.com/embed/aB3_-x9Zq0Y?list=PLembed"),
            ("watch with a trailing slash", "https://www.youtube.com/watch/?list=PLslash"),
            ("other host", "https://www.notyoutube.com/playlist?list=PLother"),
            ("look-alike host", "https://youtube.com.evil.example/playlist?list=PLevil"),
            ("list only in the fragment", "https://www.youtube.com/watch?v=aB3_-x9Zq0Y#list=PLfrag"),
            ("parameter ending in list", "https://www.youtube.com/watch?v=aB3_-x9Zq0Y&playlist=PLnot"),
            ("empty id", "https://www.youtube.com/playlist?list="),
            ("one-character id", "https://www.youtube.com/playlist?list=P"),
            ("id with a bad character", "https://www.youtube.com/playlist?list=PL%20x"),
            ("no scheme", "www.youtube.com/playlist?list=PLnoscheme"),
            ("local playlist file", "C:\\music\\list=PL.m3u"),
        ];
        let mut bad = Vec::new();
        for (case, url) in cases {
            expect!(bad, list_id(url).is_none(), "{case}: {:?}", list_id(url));
            expect!(bad, !is_youtube_list_url(url), "{case}: taken for a list URL");
        }
        finish(bad);
    }
}

#[test]
fn an_unread_list_falls_back_to_its_video_only_when_the_link_names_one_and_nothing_cut_the_read_short() {
    let cases = [
        ("video named, not aborted", "https://www.youtube.com/watch?v=aB3_-x9Zq0Y&list=PLx", false, true),
        ("video named, aborted", "https://www.youtube.com/watch?v=aB3_-x9Zq0Y&list=PLx", true, false),
        ("no video, not aborted", "https://www.youtube.com/playlist?list=PLx", false, false),
        ("no video, aborted", "https://www.youtube.com/playlist?list=PLx", true, false),
        ("short link with a video", "https://youtu.be/aB3_-x9Zq0Y?list=PLx", false, true),
    ];
    let mut bad = Vec::new();
    for (case, url, aborted, expected) in cases {
        let actual = playr::youtube::plays_video_when_list_unread(url, aborted);
        expect!(bad, actual == expected, "{case}: {actual}, expected {expected}");
    }
    finish(bad);
}

fn watch(id: &str) -> String {
    format!("https://www.youtube.com/watch?v={id}")
}

fn yt_entry(title: &str, subtitle: &str, duration: Option<f64>, id: &str) -> Entry {
    Entry { title: title.to_string(), subtitle: subtitle.to_string(), duration, audio: Some(watch(id)) }
}

mod playlist_json {
    use super::*;

    #[test]
    fn a_playlist_answer_gives_its_name_and_the_entries_with_an_eleven_character_id() {
        let answer = json!({
            "_type": "playlist", "id": "PLx", "title": "  Road Trip  ",
            "entries": [
                { "_type": "url", "id": "AAAAAAAAAA1", "title": "Song A", "channel": "Channel A", "uploader": "Uploader A", "duration": 201.5 },
                { "_type": "url", "id": "short", "title": "Too short an id", "duration": 10 },
                { "_type": "url", "id": "BBBBBBBBBB2", "title": "  ", "uploader": "Uploader B", "duration": 0 },
                { "_type": "url", "title": "No id", "duration": 11 },
                { "_type": "url", "id": "CCCCCCCCCC3", "duration": -1 },
                { "_type": "url", "id": "DDDDDDDDDDD4", "title": "Twelve characters" },
                { "_type": "url", "id": "EEEEEEEEEE5", "title": "Song E", "channel": "  ", "uploader": "Uploader E", "duration": 7 },
            ],
        });

        let parsed = parse_playlist(&answer.to_string());

        let expected = Playlist {
            name: "Road Trip".to_string(),
            entries: vec![
                yt_entry("Song A", "Channel A", Some(201.5), "AAAAAAAAAA1"),
                yt_entry("BBBBBBBBBB2", "Uploader B", None, "BBBBBBBBBB2"),
                yt_entry("CCCCCCCCCC3", "", None, "CCCCCCCCCC3"),
                yt_entry("Song E", "Uploader E", Some(7.0), "EEEEEEEEEE5"),
            ],
            start: 0,
        };
        assert_eq!(parsed, Ok(Some(expected)));
    }

    #[test]
    fn a_playlist_without_a_title_is_called_youtube_playlist() {
        let answer = json!({ "_type": "playlist", "title": " ", "entries": [{ "id": "AAAAAAAAAA1", "title": "Song A" }] });

        let parsed = parse_playlist(&answer.to_string());

        assert_eq!(parsed.map(|p| p.map(|p| p.name)), Ok(Some("YouTube playlist".to_string())));
    }

    #[test]
    fn an_answer_that_is_not_a_playlist_with_a_playable_entry_is_none() {
        let one = json!([{ "id": "AAAAAAAAAA1", "title": "Song A" }]);
        let cases = [
            ("a single video (a Mix)", json!({ "_type": "video", "id": "AAAAAAAAAA1", "title": "Song A", "entries": one })),
            ("no _type", json!({ "id": "AAAAAAAAAA1", "title": "Song A", "entries": one })),
            ("_type in another case", json!({ "_type": "Playlist", "title": "L", "entries": one })),
            ("no entries", json!({ "_type": "playlist", "title": "L" })),
            ("empty entries", json!({ "_type": "playlist", "title": "L", "entries": [] })),
            ("entries not an array", json!({ "_type": "playlist", "title": "L", "entries": { "id": "AAAAAAAAAA1" } })),
            ("no entry has an 11-character id", json!({ "_type": "playlist", "title": "L", "entries": [{ "id": "abc" }, { "title": "x" }, 5] })),
        ];
        let mut bad = Vec::new();
        for (case, answer) in cases {
            let parsed = parse_playlist(&answer.to_string());
            expect!(bad, parsed == Ok(None), "{case}: {parsed:?}");
        }
        finish(bad);
    }

    #[test]
    fn an_unreadable_answer_is_an_error_naming_it() {
        let parsed = parse_playlist("{\"_type\": \"playlist\", ");

        let prefix = "yt-dlp sent an unreadable playlist (";
        assert!(parsed.as_ref().is_err_and(|e| e.starts_with(prefix) && e.ends_with(')') && e.len() > prefix.len() + 1), "{parsed:?}");
    }
}

/// A fresh, empty folder for `test`.
fn fresh_dir(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("playr-youtube-lists").join(test);
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

/// A private data folder and a `PATH` holding only an empty `bin`, so no JavaScript runtime is
/// found and yt-dlp is asked exactly once per video.
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

/// The environment of a child whose yt-dlp answers from `script`: `{"playlist": answer to a
/// `--yes-playlist` call (absent: that call fails), "videos": {id: info}}`.
#[cfg(any(windows, target_os = "linux"))]
fn fake_env(dir: &Path, script: &Value) -> Vec<(&'static str, OsString)> {
    let fake = dir.join("fake");
    std::fs::create_dir_all(&fake).unwrap();
    let exe = fake.join(if cfg!(windows) { "yt-dlp.exe" } else { "yt-dlp" });
    std::fs::copy(std::env::current_exe().unwrap(), &exe).expect("copy the test binary");
    std::fs::write(fake.join("fake.json"), script.to_string()).unwrap();
    let mut env = private_env(dir);
    env.push(("PLAYR_YTDLP", exe.into_os_string()));
    env
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
/// arguments to `calls.log`, answers as yt-dlp and exits; any other run carries on as the tests.
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
    use std::io::Write;
    writeln!(log, "{}", args.join(" ")).unwrap();
    let last = args.last().cloned().unwrap_or_default();
    if args.iter().any(|a| a == "--yes-playlist") {
        if script["block_playlist"] == true {
            let started = std::time::Instant::now();
            while !dir.join("release").exists() && started.elapsed() < Duration::from_secs(30) {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        if script["fail_first_playlist"] == true && !dir.join("failed_once").exists() {
            std::fs::write(dir.join("failed_once"), "").unwrap();
            eprintln!("ERROR: [youtube:tab] {}: HTTP Error 503: Service Unavailable", list_id(&last).unwrap_or_default());
            std::process::exit(1);
        }
        match script.get("playlist") {
            Some(answer) => {
                println!("{answer}");
                std::process::exit(0);
            }
            None => {
                eprintln!("ERROR: [youtube:tab] {}: The playlist does not exist", list_id(&last).unwrap_or_default());
                std::process::exit(1);
            }
        }
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

fn calls(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("fake").join("calls.log"))
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

fn playlist_call(url: &str) -> String {
    format!("-J --flat-playlist --yes-playlist --playlist-end 200 --no-warnings --no-progress -- {url}")
}

fn video_call(url: &str) -> String {
    format!("-J --no-playlist --no-warnings --no-progress --ignore-no-formats-error -- {url}")
}

fn flat_entry(n: usize) -> Value {
    json!({ "_type": "url", "ie_key": "Youtube", "id": ID[n], "url": watch(ID[n]), "title": format!("Song {n}"), "channel": format!("Channel {n}"), "duration": 12.0 })
}

/// The ids of the fake playlist "Road Trip", in order.
const ID: [&str; 4] = ["AAAAAAAAAA1", "BBBBBBBBBB2", "CCCCCCCCCC3", "DDDDDDDDDD4"];

fn road_trip() -> Value {
    json!({ "_type": "playlist", "id": "PLroadtrip", "title": "Road Trip", "entries": (0..4).map(flat_entry).collect::<Vec<_>>() })
}

#[test]
#[cfg(any(windows, target_os = "linux"))]
fn playlist_asks_ytdlp_for_the_flat_list_and_starts_at_the_v_video() {
    let test = "playlist_asks_ytdlp_for_the_flat_list_and_starts_at_the_v_video";
    if !is_child() {
        let dir = fresh_dir("playlist_start");
        run_child(test, fake_env(&dir, &json!({ "playlist": road_trip() })));
        return;
    }
    let urls = [
        ("v= names the third entry", format!("https://www.youtube.com/watch?v={}&list=PLroadtrip", ID[2]), 2),
        ("v= not in the list", "https://www.youtube.com/watch?v=ZZZZZZZZZZ9&list=PLroadtrip".to_string(), 0),
        ("playlist page", "https://www.youtube.com/playlist?list=PLroadtrip".to_string(), 0),
        ("short link names the last entry", format!("https://youtu.be/{}?list=PLroadtrip", ID[3]), 3),
    ];
    let mut bad = Vec::new();
    for (case, url, start) in urls {
        let progress = Mutex::new(Vec::new());
        let before = calls(&test_dir()).len();

        let list = playr::youtube::playlist(&url, &AtomicBool::new(false), &|m: &str| progress.lock().unwrap().push(m.to_string()));

        let expected = Playlist {
            name: "Road Trip".to_string(),
            entries: (0..4).map(|n| yt_entry(&format!("Song {n}"), &format!("Channel {n}"), Some(12.0), ID[n])).collect(),
            start,
        };
        expect!(bad, list == Ok(Some(expected.clone())), "{case}: {list:?}");
        expect!(bad, calls(&test_dir())[before..] == [playlist_call(&url)], "{case}: {:?}", &calls(&test_dir())[before..]);
        expect!(bad, *progress.lock().unwrap() == ["Reading the YouTube playlist…"], "{case}: {:?}", progress.lock().unwrap());
    }
    finish(bad);
}

#[test]
#[cfg(any(windows, target_os = "linux"))]
fn playlist_gives_none_for_a_single_video_answer_and_the_error_when_ytdlp_fails() {
    let test = "playlist_gives_none_for_a_single_video_answer_and_the_error_when_ytdlp_fails";
    if !is_child() {
        let single = fresh_dir("playlist_single");
        run_child(test, fake_env(&single, &json!({ "playlist": { "_type": "video", "id": ID[0], "title": "Mix start" } })));
        let failing = fresh_dir("playlist_fails");
        let mut env = fake_env(&failing, &json!({}));
        env.push(("PLAYR_TEST_EXPECT_ERROR", OsString::from("1")));
        run_child(test, env);
        return;
    }
    let url = format!("https://www.youtube.com/watch?v={}&list=RD{}", ID[0], ID[0]);

    let list = playr::youtube::playlist(&url, &AtomicBool::new(false), &|_: &str| {});

    let expected = if std::env::var_os("PLAYR_TEST_EXPECT_ERROR").is_some() {
        Err(format!("[youtube:tab] RD{}: The playlist does not exist", ID[0]))
    } else {
        Ok(None)
    };
    assert_eq!(list, expected);
    assert_eq!(calls(&test_dir()), [playlist_call(&url)]);
}

fn video_info(id: &str, server: &RangeServer) -> Value {
    json!({
        "id": id, "title": format!("Video {id}"), "duration": 12.0,
        "formats": [
            { "format_id": "134", "url": server.url("/v.mp4"), "vcodec": "avc1.4d400d", "acodec": "none",
              "protocol": "https", "ext": "mp4", "width": 160, "height": 120, "fps": 25.0, "tbr": 100.0 },
            { "format_id": "140", "url": server.url("/a.m4a"), "vcodec": "none", "acodec": "mp4a.40.2",
              "protocol": "https", "ext": "m4a", "language": "en", "abr": 96.0 },
        ],
    })
}

fn dash_server() -> RangeServer {
    RangeServer::start(vec![
        ("/v.mp4", Mode::Ranged, fixture_bytes("dash_video.mp4")),
        ("/a.m4a", Mode::Ranged, fixture_bytes("dash_audio.m4a")),
    ])
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

/// Stops, runs `open` and waits until loading is over, whichever way it went.
fn reopen(player: &Player, open: impl FnOnce()) -> Status {
    player.stop();
    wait_for(player, "idle", |s| s.state == State::Idle);
    open();
    wait_for(player, "loading to finish", |s| !matches!(s.state, State::Idle | State::Loading))
}

/// (state, entry, title, info, notes, video size)
type Seen = (State, usize, String, String, Vec<String>, Option<(u32, u32)>);

fn seen(s: &Status) -> Seen {
    (s.state, s.entry, s.title.clone(), s.info.clone(), s.notes.clone(), s.video)
}

fn row(entry: usize, info: &str, notes: &[String], video: Option<(u32, u32)>) -> Seen {
    (State::Playing, entry, format!("Video {}", ID[entry]), info.to_string(), notes.to_vec(), video)
}

#[test]
#[cfg(any(windows, target_os = "linux"))]
fn a_youtube_playlist_link_starts_at_its_v_video_and_steps_through_the_list_keeping_one_history_item() {
    let test = "a_youtube_playlist_link_starts_at_its_v_video_and_steps_through_the_list_keeping_one_history_item";
    if !is_child() {
        let dir = fresh_dir("player_playlist");
        let server = dash_server();
        let videos: serde_json::Map<String, Value> =
            [0, 1, 3].iter().map(|&n| (ID[n].to_string(), video_info(ID[n], &server))).collect();
        run_child(test, fake_env(&dir, &json!({ "playlist": road_trip(), "videos": videos })));
        return;
    }
    let url = format!("https://www.youtube.com/watch?v={}&list=PLroadtrip", ID[1]);
    let history_file = test_dir().join("history.json");
    let player = Player::with_history(Box::new(|| {}), Some(history_file.clone()));
    player.shared.audio.set_volume(0.0);

    let started = reopen(&player, || player.open(url.clone()));
    reopen(&player, || player.open_entry(url.clone(), 2));
    let skipped = wait_for(&player, "the entry after the refused one", |s| s.state == State::Playing && s.entry == 3);
    let both = Choice { video: Some("134".into()), audio: Some("140".into()) };
    let requality = reopen(&player, || player.open_youtube_at(url.clone(), Some(3), both.clone()));
    player.set_audio_only(true);
    let sound_only = reopen(&player, || player.open_entry(url.clone(), 0));
    player.stop();

    let refused = format!("skipped Song 2 — Channel 2: [youtube] {}: Video unavailable", ID[2]);
    assert_eq!(
        [seen(&started), seen(&skipped), seen(&requality), seen(&sound_only)],
        [
            row(1, "YouTube · H.264 160×120 · playlist Road Trip · 2/4", &[], Some((160, 120))),
            row(3, "YouTube · H.264 160×120 · playlist Road Trip · 4/4", &[refused], Some((160, 120))),
            row(3, "YouTube · H.264 160×120 · playlist Road Trip · 4/4", &[], Some((160, 120))),
            row(0, "YouTube · sound only · playlist Road Trip · 1/4", &[], None),
        ]
    );
    assert_eq!(started.playlist.as_ref().map(|p| (p.entries.len(), p.start)), Some((4, 1)));
    assert_eq!(started.youtube.as_ref().map(|v| v.id.as_str()), Some(ID[1]));
    assert_eq!(requality.choice, both);
    let made = calls(&test_dir());
    assert_eq!(made.iter().filter(|c| c.contains("--yes-playlist")).cloned().collect::<Vec<_>>(), [playlist_call(&url)]);
    let history: Vec<(String, String)> = player.history().into_iter().map(|i: Item| (i.target, i.title)).collect();
    assert_eq!(history, [(url.clone(), "Road Trip".to_string())]);
    assert_eq!(playr::history::History::load(&history_file).items, player.history());
}

#[test]
#[cfg(any(windows, target_os = "linux"))]
fn a_list_link_yt_dlp_answers_with_a_single_video_plays_that_video_and_each_fresh_open_asks_for_the_list_again() {
    let test = "a_list_link_yt_dlp_answers_with_a_single_video_plays_that_video_and_each_fresh_open_asks_for_the_list_again";
    if !is_child() {
        let dir = fresh_dir("player_mix");
        let server = dash_server();
        let info = video_info(ID[0], &server);
        let mut single = info.clone();
        single["_type"] = json!("video");
        run_child(test, fake_env(&dir, &json!({ "playlist": single, "videos": { ID[0]: info } })));
        return;
    }
    let url = format!("https://www.youtube.com/watch?v={}&list=RD{}", ID[0], ID[0]);
    let player = Player::new(Box::new(|| {}));
    player.shared.audio.set_volume(0.0);

    let first = reopen(&player, || player.open(url.clone()));
    let second = reopen(&player, || player.open(url.clone()));
    player.stop();

    let expected = (State::Playing, 0, format!("Video {}", ID[0]), "YouTube · H.264 160×120".to_string(), vec![], Some((160, 120)));
    assert_eq!((seen(&first), seen(&second)), (expected.clone(), expected));
    assert_eq!((first.playlist.is_none(), second.playlist.is_none()), (true, true));
    assert_eq!(calls(&test_dir()), [playlist_call(&url), video_call(&url), playlist_call(&url)]);
}

#[test]
#[cfg(any(windows, target_os = "linux"))]
fn when_ytdlp_cannot_read_the_list_a_link_with_a_video_plays_that_video_and_a_playlist_page_fails() {
    let test = "when_ytdlp_cannot_read_the_list_a_link_with_a_video_plays_that_video_and_a_playlist_page_fails";
    if !is_child() {
        let dir = fresh_dir("player_list_refused");
        let server = dash_server();
        run_child(test, fake_env(&dir, &json!({ "videos": { ID[2]: video_info(ID[2], &server) } })));
        return;
    }
    let with_video = format!("https://www.youtube.com/watch?v={}&list=PLgone", ID[2]);
    let page = "https://www.youtube.com/playlist?list=PLpagegone";
    let player = Player::with_history(Box::new(|| {}), Some(test_dir().join("history.json")));
    player.shared.audio.set_volume(0.0);

    let first = reopen(&player, || player.open(with_video.clone()));
    let again = reopen(&player, || player.open(with_video.clone()));
    let failed = reopen(&player, || player.open(page));
    player.stop();

    let single = (State::Playing, 0, format!("Video {}", ID[2]), "YouTube · H.264 160×120".to_string(), vec![], Some((160, 120)));
    assert_eq!((seen(&first), seen(&again)), (single.clone(), single));
    assert_eq!((first.playlist.is_none(), again.playlist.is_none()), (true, true));
    assert_eq!((failed.state, failed.error.as_deref()), (State::Failed, Some("[youtube:tab] PLpagegone: The playlist does not exist")));
    assert_eq!(
        calls(&test_dir()),
        [playlist_call(&with_video), video_call(&with_video), playlist_call(&with_video), playlist_call(page)]
    );
    let history: Vec<(String, String)> = player.history().into_iter().map(|i| (i.target, i.title)).collect();
    assert_eq!(history, [(with_video.clone(), format!("Video {}", ID[2]))]);
}

#[test]
#[cfg(any(windows, target_os = "linux"))]
fn a_list_that_could_not_be_read_is_asked_for_again_and_plays_as_a_list_once_it_answers() {
    let test = "a_list_that_could_not_be_read_is_asked_for_again_and_plays_as_a_list_once_it_answers";
    if !is_child() {
        let dir = fresh_dir("player_list_retry");
        let server = dash_server();
        let videos: serde_json::Map<String, Value> = (0..4).map(|n| (ID[n].to_string(), video_info(ID[n], &server))).collect();
        run_child(test, fake_env(&dir, &json!({ "playlist": road_trip(), "videos": videos, "fail_first_playlist": true })));
        return;
    }
    let url = format!("https://www.youtube.com/watch?v={}&list=PLroadtrip", ID[2]);
    let player = Player::new(Box::new(|| {}));
    player.shared.audio.set_volume(0.0);

    let single = reopen(&player, || player.open(url.clone()));
    let listed = reopen(&player, || player.open(url.clone()));
    player.stop();

    assert_eq!(seen(&single), (State::Playing, 0, format!("Video {}", ID[2]), "YouTube · H.264 160×120".to_string(), vec![], Some((160, 120))));
    assert_eq!(single.playlist.is_none(), true);
    assert_eq!(seen(&listed), row(2, "YouTube · H.264 160×120 · playlist Road Trip · 3/4", &[], Some((160, 120))));
    assert_eq!(listed.playlist.as_ref().map(|p| (p.entries.len(), p.start)), Some((4, 2)));
    assert_eq!(calls(&test_dir()), [playlist_call(&url), video_call(&url), playlist_call(&url)]);
}

#[test]
#[cfg(any(windows, target_os = "linux"))]
fn a_list_read_cut_short_by_another_open_does_not_fall_back_to_the_lists_video() {
    let test = "a_list_read_cut_short_by_another_open_does_not_fall_back_to_the_lists_video";
    if !is_child() {
        let dir = fresh_dir("player_list_aborted");
        let server = dash_server();
        run_child(test, fake_env(&dir, &json!({ "playlist": road_trip(), "videos": { ID[1]: video_info(ID[1], &server) }, "block_playlist": true })));
        return;
    }
    let url = format!("https://www.youtube.com/watch?v={}&list=PLroadtrip", ID[1]);
    let other = common::fixture("sine.wav").to_str().unwrap().to_string();
    let player = Player::new(Box::new(|| {}));
    player.shared.audio.set_volume(0.0);

    player.open(url.clone());
    let started = Instant::now();
    while !calls(&test_dir()).contains(&playlist_call(&url)) {
        assert!(started.elapsed() < Duration::from_secs(30), "yt-dlp was never asked for the list");
        std::thread::sleep(Duration::from_millis(10));
    }
    player.open(other.clone());
    std::fs::write(test_dir().join("fake").join("release"), "").unwrap();
    let playing = wait_for(&player, "the other file to play", |s| s.source == other && s.state == State::Playing);
    player.stop();

    assert_eq!((playing.title.as_str(), playing.playlist.is_none()), ("sine.wav", true));
    assert_eq!(calls(&test_dir()), [playlist_call(&url)]);
}
