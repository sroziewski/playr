//! `playr::history` and the history a `Player` keeps: what was opened, newest first, saved to
//! the file given to `Player::with_history`.
//!
//! A `Player` opens the default audio device, so the player tests take `AUDIO_DEVICE` and play
//! at volume zero. The test that needs its own data folder (`LOCALAPPDATA` / `XDG_DATA_HOME`)
//! runs its body in a fresh copy of this test binary started with that environment.

mod common;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use common::{finish, fixture, fixture_bytes};
use playr::history::{self, History, Item, LIMIT};
use playr::player::{Player, State, Status};
use serde_json::json;

const CHILD: &str = "PLAYR_TEST_CHILD";
const DIR: &str = "PLAYR_TEST_DIR";

fn item(target: &str, title: &str, at: u64) -> Item {
    Item { target: target.to_string(), title: title.to_string(), at }
}

fn fresh_dir(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("playr-history").join(test);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create test dir");
    dir
}

mod record {
    use super::*;

    #[test]
    fn a_new_target_goes_on_top_with_its_title_and_time() {
        let mut h = History::default();

        h.record("C:\\music\\one.mp3", "One", 1_700_000_001);
        h.record("https://radio.example/two", "Two", 1_700_000_002);

        assert_eq!(
            h.items,
            [item("https://radio.example/two", "Two", 1_700_000_002), item("C:\\music\\one.mp3", "One", 1_700_000_001)]
        );
    }

    #[test]
    fn replaying_moves_the_item_to_the_top_with_its_new_title_and_time_and_leaves_no_duplicate() {
        let mut h = History::default();
        h.record("a.mp3", "Song A", 10);
        h.record("b.mp3", "Song B", 20);
        h.record("c.mp3", "Song C", 30);

        h.record("b.mp3", "Song B again", 40);

        assert_eq!(h.items, [item("b.mp3", "Song B again", 40), item("c.mp3", "Song C", 30), item("a.mp3", "Song A", 10)]);
    }

    #[test]
    fn the_target_is_trimmed_so_a_replay_with_spaces_around_it_is_the_same_item() {
        let mut h = History::default();

        h.record("  C:\\music\\a.mp3\t", "First", 5);
        h.record("C:\\music\\a.mp3", "Second", 6);

        assert_eq!(h.items, [item("C:\\music\\a.mp3", "Second", 6)]);
    }

    #[test]
    fn a_blank_target_is_ignored() {
        let mut bad = Vec::new();
        for (case, target) in [("empty", ""), ("spaces", "   "), ("tab and newline", "\t\n")] {
            let mut h = History::default();
            h.record("kept.mp3", "Kept", 1);

            h.record(target, "Ignored", 2);

            expect!(bad, h.items == [item("kept.mp3", "Kept", 1)], "{case}: {:?}", h.items);
        }
        finish(bad);
    }

    #[test]
    fn a_blank_title_falls_back_to_the_target_and_a_title_is_trimmed() {
        let mut bad = Vec::new();
        let cases = [("empty", "", "https://radio.example/live"), ("spaces", "   ", "https://radio.example/live"), ("padded", "  Live FM \t", "Live FM")];
        for (case, title, expected) in cases {
            let mut h = History::default();

            h.record(" https://radio.example/live ", title, 7);

            expect!(bad, h.items == [item("https://radio.example/live", expected, 7)], "{case}: {:?}", h.items);
        }
        finish(bad);
    }

    fn numbered(count: u64) -> History {
        let mut h = History::default();
        for n in 1..=count {
            h.record(&format!("t{n}.mp3"), &format!("Track {n}"), 100 + n);
        }
        h
    }

    fn expected_numbered(newest_first: impl Iterator<Item = u64>) -> Vec<Item> {
        newest_first.map(|n| item(&format!("t{n}.mp3"), &format!("Track {n}"), 100 + n)).collect()
    }

    #[test]
    fn twenty_items_are_all_kept_and_the_twenty_first_drops_the_oldest() {
        assert_eq!(LIMIT, 20);

        let full = numbered(20);
        let over = numbered(21);

        assert_eq!(full.items, expected_numbered((1..=20).rev()));
        assert_eq!(over.items, expected_numbered((2..=21).rev()));
    }

    #[test]
    fn replaying_the_oldest_of_a_full_history_drops_nothing() {
        let mut h = numbered(20);

        h.record("t1.mp3", "Track 1", 200);

        let mut expected = vec![item("t1.mp3", "Track 1", 200)];
        expected.extend(expected_numbered((2..=20).rev()));
        assert_eq!(h.items, expected);
    }

    #[test]
    fn clear_empties_the_history() {
        let mut h = numbered(3);

        h.clear();

        assert_eq!(h.items, Vec::<Item>::new());
    }
}

mod json_text {
    use super::*;

    fn three() -> History {
        History {
            items: vec![
                item("https://www.youtube.com/watch?v=aB3_-x9Zq0Y&list=PLx", "Zażółć \"gęślą\" jaźń", 1_700_000_300),
                item("D:\\Muzyka\\Kołysanka.flac", "Kołysanka.flac", 1_700_000_200),
                item("http://radio.example/listen.pls", "Radio \\ 99,9", 1_700_000_100),
            ],
        }
    }

    #[test]
    fn to_json_writes_an_items_array_of_target_title_and_at_in_order() {
        let text = three().to_json();

        let value: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(
            value,
            json!({ "items": [
                { "target": "https://www.youtube.com/watch?v=aB3_-x9Zq0Y&list=PLx", "title": "Zażółć \"gęślą\" jaźń", "at": 1_700_000_300u64 },
                { "target": "D:\\Muzyka\\Kołysanka.flac", "title": "Kołysanka.flac", "at": 1_700_000_200u64 },
                { "target": "http://radio.example/listen.pls", "title": "Radio \\ 99,9", "at": 1_700_000_100u64 },
            ]})
        );
        let empty: serde_json::Value = serde_json::from_str(&History::default().to_json()).expect("valid JSON");
        assert_eq!(empty, json!({ "items": [] }));
    }

    #[test]
    fn parse_reads_back_what_to_json_wrote() {
        assert_eq!(History::parse(&three().to_json()), three());
    }

    #[test]
    fn parse_never_fails_skips_what_it_cannot_read_and_keeps_the_first_of_duplicates() {
        let unreadable = json!({ "items": [
            { "target": "a.mp3", "title": "Song A", "at": 5 },
            42,
            { "title": "no target", "at": 6 },
            { "target": 7, "title": "numeric target" },
            { "target": "   ", "title": "blank target" },
            { "target": " b.mp3 ", "title": "  ", "at": 8 },
            { "target": "c.mp3", "title": "Song C" },
            { "target": "d.mp3", "title": "Song D", "at": -3 },
            { "target": "e.mp3", "title": "Song E", "at": "9" },
            { "target": "f.mp3", "title": 11, "at": 12 },
        ]})
        .to_string();
        let duplicates = json!({ "items": [
            { "target": "a.mp3", "title": "First A", "at": 3 },
            { "target": "b.mp3", "title": "Song B", "at": 2 },
            { "target": " a.mp3", "title": "Second A", "at": 1 },
        ]})
        .to_string();
        let cases: Vec<(&str, &str, Vec<Item>)> = vec![
            ("empty text", "", vec![]),
            ("not JSON", "{\"items\": [ {\"target\": \"a.mp3\"", vec![]),
            ("null", "null", vec![]),
            ("no items key", r#"{"list": [{"target": "a.mp3"}]}"#, vec![]),
            ("items is an object", r#"{"items": {"target": "a.mp3"}}"#, vec![]),
            ("top-level array", r#"[{"target": "a.mp3"}]"#, vec![]),
            (
                "unreadable entries skipped, missing or unreadable at is 0, blank or missing title is the target",
                &unreadable,
                vec![
                    item("a.mp3", "Song A", 5),
                    item("b.mp3", "b.mp3", 8),
                    item("c.mp3", "Song C", 0),
                    item("d.mp3", "Song D", 0),
                    item("e.mp3", "Song E", 0),
                    item("f.mp3", "f.mp3", 12),
                ],
            ),
            ("only the first of duplicate targets", &duplicates, vec![item("a.mp3", "First A", 3), item("b.mp3", "Song B", 2)]),
        ];
        let mut bad = Vec::new();
        for (case, text, expected) in cases {
            let parsed = History::parse(text);
            expect!(bad, parsed.items == expected, "{case}:\n    got      {:?}\n    expected {expected:?}", parsed.items);
        }
        finish(bad);
    }

    #[test]
    fn reading_stops_at_twenty_distinct_items_and_duplicates_do_not_count() {
        let mut entries = vec![json!({ "target": "t1.mp3", "title": "dup", "at": 1 })];
        entries.extend((1..=25).map(|n| json!({ "target": format!("t{n}.mp3"), "title": format!("Track {n}"), "at": n })));
        let text = json!({ "items": entries }).to_string();

        let parsed = History::parse(&text);

        let mut expected = vec![item("t1.mp3", "dup", 1)];
        expected.extend((2..=20).map(|n| item(&format!("t{n}.mp3"), &format!("Track {n}"), n)));
        assert_eq!(parsed.items, expected);
    }
}

mod files {
    use super::*;

    fn sample() -> History {
        History { items: vec![item("b.flac", "B", 22), item("a.mp3", "A", 11)] }
    }

    #[test]
    fn a_missing_or_unreadable_file_loads_as_an_empty_history() {
        let dir = fresh_dir("load_nothing");
        let damaged = dir.join("damaged.json");
        std::fs::write(&damaged, b"\xff\xfe{ not json").unwrap();

        let mut bad = Vec::new();
        for (case, path) in [("missing", dir.join("absent.json")), ("a folder", dir.clone()), ("damaged", damaged)] {
            let loaded = History::load(&path);
            expect!(bad, loaded.items.is_empty(), "{case}: {:?}", loaded.items);
        }
        finish(bad);
    }

    #[test]
    fn save_creates_the_folders_and_load_reads_the_history_back() {
        let path = fresh_dir("save_nested").join("a").join("b").join("history.json");

        let saved = sample().save(&path);

        assert_eq!(saved, Ok(()));
        assert_eq!(History::load(&path), sample());
        assert!(!path.with_file_name("history.json.part").exists(), "the partial file is left behind");
    }

    #[test]
    fn save_replaces_an_older_file_and_a_stale_partial_file() {
        let dir = fresh_dir("save_replace");
        let path = dir.join("history.json");
        std::fs::write(&path, History { items: vec![item("old.mp3", "Old", 1)] }.to_json()).unwrap();
        std::fs::write(dir.join("history.json.part"), "left over from a crash").unwrap();

        let saved = sample().save(&path);

        assert_eq!(saved, Ok(()));
        assert_eq!(History::load(&path), sample());
        assert!(!dir.join("history.json.part").exists());
    }

    #[test]
    fn save_writes_through_history_json_part_so_a_failed_write_leaves_the_old_file_whole() {
        let dir = fresh_dir("save_through_part");
        let path = dir.join("history.json");
        let old = History { items: vec![item("old.mp3", "Old", 1)] }.to_json();
        std::fs::write(&path, &old).unwrap();
        std::fs::create_dir_all(dir.join("history.json.part").join("in the way")).unwrap();

        let saved = sample().save(&path);

        let prefix = "cannot save the history: ";
        assert!(saved.as_ref().is_err_and(|e| e.starts_with(prefix) && e.len() > prefix.len()), "{saved:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), old);
    }

    #[test]
    fn save_reports_a_folder_that_cannot_be_created() {
        let dir = fresh_dir("save_no_folder");
        let blocker = dir.join("a file");
        std::fs::write(&blocker, "not a folder").unwrap();
        let path = blocker.join("history.json");

        let saved = sample().save(&path);

        let prefix = format!("cannot create {}: ", blocker.display());
        assert!(saved.as_ref().is_err_and(|e| e.starts_with(&prefix) && e.len() > prefix.len()), "{saved:?}");
    }
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
    vec![
        (DIR, dir.as_os_str().to_owned()),
        ("LOCALAPPDATA", data.as_os_str().to_owned()),
        ("XDG_DATA_HOME", data.as_os_str().to_owned()),
        ("HOME", dir.join("home").into_os_string()),
    ]
}

fn test_dir() -> PathBuf {
    PathBuf::from(std::env::var_os(DIR).expect("PLAYR_TEST_DIR"))
}

#[test]
fn default_path_is_history_json_in_the_data_folder() {
    let dir = fresh_dir("default_path");
    if !in_child("default_path_is_history_json_in_the_data_folder", private_env(&dir)) {
        return;
    }

    assert_eq!(history::default_path(), Some(test_dir().join("appdata").join("playr").join("history.json")));
}

#[test]
#[cfg(windows)]
fn without_a_data_folder_there_is_no_default_path() {
    let dir = fresh_dir("default_path_none");
    let mut env = private_env(&dir);
    env.retain(|(k, _)| *k != "LOCALAPPDATA");
    env.push(("LOCALAPPDATA", OsString::new()));
    if !in_child("without_a_data_folder_there_is_no_default_path", env) {
        return;
    }

    assert_eq!(history::default_path(), None);
}

static AUDIO_DEVICE: Mutex<()> = Mutex::new(());

struct Session {
    player: Player,
    _device: MutexGuard<'static, ()>,
}

fn start(history_file: Option<PathBuf>) -> Session {
    let device = AUDIO_DEVICE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let player = Player::with_history(Box::new(|| {}), history_file);
    player.shared.audio.set_volume(0.0);
    Session { player, _device: device }
}

fn wait_for(player: &Player, what: &str, mut done: impl FnMut(&Status) -> bool) -> Status {
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

impl Session {
    /// Opens `target` from a stopped player and waits until loading is over, whichever way.
    fn open(&self, target: &str) -> Status {
        self.player.stop();
        wait_for(&self.player, "idle", |s| s.state == State::Idle);
        self.player.open(target);
        wait_for(&self.player, &format!("{target} to load"), |s| s.source == target && !matches!(s.state, State::Idle | State::Loading))
    }
}

fn now() -> u64 {
    playr::cookies::now_unix()
}

fn path_of(name: &str) -> String {
    fixture(name).to_str().expect("utf-8 path").to_string()
}

fn missing_file(dir: &Path) -> String {
    dir.join("no-such-file.wav").to_str().unwrap().to_string()
}

/// (target, title) of each item, newest first.
fn targets_and_titles(items: &[Item]) -> Vec<(String, String)> {
    items.iter().map(|i| (i.target.clone(), i.title.clone())).collect()
}

fn pair(target: &str, title: &str) -> (String, String) {
    (target.to_string(), title.to_string())
}

#[test]
fn a_successful_open_is_recorded_on_top_with_the_time_and_saved_and_a_failed_open_is_not() {
    let dir = fresh_dir("player_records");
    let path = dir.join("history.json");
    let s = start(Some(path.clone()));
    let wav = path_of("sine.wav");

    let before = now();
    let played = s.open(&wav);
    let after = now();
    let failed = s.open(&missing_file(&dir));

    assert_eq!((played.state, failed.state), (State::Playing, State::Failed), "{played:?}\n{failed:?}");
    let items = s.player.history();
    assert_eq!(targets_and_titles(&items), [pair(&wav, "sine.wav")]);
    assert!((before..=after).contains(&items[0].at), "at {} is not between {before} and {after}", items[0].at);
    assert_eq!(History::load(&path).items, items);
}

#[test]
fn a_failed_first_open_records_nothing_and_writes_no_file() {
    let dir = fresh_dir("player_first_fails");
    let path = dir.join("history.json");
    let s = start(Some(path.clone()));

    let failed = s.open(&missing_file(&dir));

    assert_eq!(failed.state, State::Failed, "{failed:?}");
    assert_eq!(s.player.history(), Vec::<Item>::new());
    assert!(!path.exists(), "a history file was written for a failed open");
}

#[test]
fn replaying_moves_an_item_to_the_top_without_a_duplicate_and_a_seek_adds_nothing() {
    let dir = fresh_dir("player_replay");
    let path = dir.join("history.json");
    let s = start(Some(path.clone()));
    let (wav, flac) = (path_of("sine.wav"), path_of("sine.flac"));

    s.open(&wav);
    s.open(&flac);
    let again = s.open(&format!("  {wav} "));
    s.player.seek(1.5);
    wait_for(&s.player, "the seek", |_| (s.player.shared.position() - 1.5).abs() < 0.2);

    assert_eq!(again.state, State::Playing, "{again:?}");
    let items = s.player.history();
    assert_eq!(targets_and_titles(&items), [pair(&wav, "sine.wav"), pair(&flac, "sine.flac")]);
    assert_eq!(History::load(&path).items, items);
}

#[test]
fn entry_changes_in_a_list_keep_one_item_titled_with_the_list_name() {
    let dir = fresh_dir("player_list");
    std::fs::write(dir.join("one.wav"), fixture_bytes("sine.wav")).unwrap();
    std::fs::write(dir.join("two.flac"), fixture_bytes("sine.flac")).unwrap();
    let pls = dir.join("road trip.pls");
    std::fs::write(&pls, "[playlist]\nFile1=one.wav\nTitle1=First\nFile2=two.flac\nTitle2=Second\nNumberOfEntries=2\n").unwrap();
    let pls = pls.to_str().unwrap().to_string();
    let path = dir.join("history.json");
    let s = start(Some(path.clone()));

    let first = s.open(&pls);
    s.player.open_entry(pls.clone(), 1);
    let second = wait_for(&s.player, "the second entry", |st| st.state == State::Playing && st.entry == 1);

    assert_eq!((first.state, first.entry, first.title.as_str()), (State::Playing, 0, "First"), "{first:?}");
    assert_eq!(second.title, "Second");
    let items = s.player.history();
    assert_eq!(targets_and_titles(&items), [pair(&pls, "road trip.pls")]);
    assert_eq!(History::load(&path).items, items);
}

#[test]
fn a_full_history_from_the_file_keeps_twenty_items_with_the_new_one_on_top() {
    let dir = fresh_dir("player_full");
    let path = dir.join("history.json");
    let old: Vec<Item> = (1..=20).rev().map(|n| item(&format!("https://old.example/{n}.mp3"), &format!("Old {n}"), n)).collect();
    std::fs::write(&path, History { items: old.clone() }.to_json()).unwrap();
    let s = start(Some(path.clone()));
    let wav = path_of("sine.wav");

    let loaded = s.player.history();
    s.open(&wav);

    assert_eq!(loaded, old);
    let items = s.player.history();
    let mut expected = vec![pair(&wav, "sine.wav")];
    expected.extend(old[..19].iter().map(|i| pair(&i.target, &i.title)));
    assert_eq!(targets_and_titles(&items), expected);
    assert_eq!(History::load(&path).items, items);
}

#[test]
fn the_history_carries_over_to_the_next_player_through_its_file() {
    let dir = fresh_dir("player_persists");
    let path = dir.join("history.json");
    let (wav, flac) = (path_of("sine.wav"), path_of("sine.flac"));
    let first_run = {
        let s = start(Some(path.clone()));
        s.open(&wav);
        s.open(&flac);
        s.player.history()
    };

    let s = start(Some(path.clone()));

    assert_eq!(targets_and_titles(&first_run), [pair(&flac, "sine.flac"), pair(&wav, "sine.wav")]);
    assert_eq!(s.player.history(), first_run);
}

#[test]
fn a_damaged_history_file_gives_what_can_be_read_and_playback_goes_on() {
    let dir = fresh_dir("player_damaged");
    let wav = path_of("sine.wav");
    let readable = json!({ "items": [{ "target": "https://kept.example/a.mp3", "title": "Kept", "at": 77 }, "junk", { "at": 1 }] });
    let cases: [(&str, String, Vec<Item>); 2] = [
        ("garbage", "{\"items\": [ {\"target\": ".to_string(), vec![]),
        ("partly readable", readable.to_string(), vec![item("https://kept.example/a.mp3", "Kept", 77)]),
    ];
    let mut bad = Vec::new();
    for (case, text, expected) in cases {
        let path = dir.join(format!("{}.json", case.replace(' ', "_")));
        std::fs::write(&path, text).unwrap();
        let s = start(Some(path.clone()));

        let loaded = s.player.history();
        let played = s.open(&wav);

        expect!(bad, loaded == expected, "{case}: loaded {loaded:?}");
        expect!(bad, played.state == State::Playing, "{case}: {played:?}");
        let mut after = vec![pair(&wav, "sine.wav")];
        after.extend(targets_and_titles(&expected));
        expect!(bad, targets_and_titles(&s.player.history()) == after, "{case}: {:?}", s.player.history());
        expect!(bad, History::load(&path).items == s.player.history(), "{case}: the file holds {:?}", History::load(&path).items);
    }
    finish(bad);
}

#[test]
fn clearing_empties_the_history_and_its_file_for_the_next_player() {
    let dir = fresh_dir("player_clear");
    let path = dir.join("history.json");
    let cleared = {
        let s = start(Some(path.clone()));
        s.open(&path_of("sine.wav"));
        let result = s.player.clear_history();
        (result, s.player.history())
    };

    let s = start(Some(path.clone()));

    assert_eq!(cleared, (Ok(()), vec![]));
    assert_eq!(History::load(&path).items, Vec::<Item>::new());
    assert!(path.exists(), "clearing must save the empty history, not leave the old file");
    assert_eq!(s.player.history(), Vec::<Item>::new());
}

#[test]
fn a_history_file_that_cannot_be_written_does_not_stop_playback() {
    let dir = fresh_dir("player_unwritable");
    let blocker = dir.join("a file");
    std::fs::write(&blocker, "not a folder").unwrap();
    let s = start(Some(blocker.join("history.json")));
    let wav = path_of("sine.wav");

    let played = s.open(&wav);
    let cleared = s.player.clear_history();

    assert_eq!(played.state, State::Playing, "{played:?}");
    let prefix = format!("cannot create {}: ", blocker.display());
    assert!(cleared.as_ref().is_err_and(|e| e.starts_with(&prefix) && e.len() > prefix.len()), "{cleared:?}");
    assert_eq!(s.player.history(), Vec::<Item>::new());
}

#[test]
fn player_new_keeps_the_history_in_memory_and_writes_nothing_to_the_data_folder() {
    let dir = fresh_dir("player_new_in_memory");
    if !in_child("player_new_keeps_the_history_in_memory_and_writes_nothing_to_the_data_folder", private_env(&dir)) {
        let leftovers: Vec<PathBuf> = walk(&dir.join("appdata")).into_iter().filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("history"))).collect();
        assert_eq!(leftovers, Vec::<PathBuf>::new());
        return;
    }
    let data_file = test_dir().join("appdata").join("playr").join("history.json");
    assert_eq!(history::default_path(), Some(data_file.clone()), "the private data folder is not in effect");
    let wav = path_of("sine.wav");
    let player = Player::new(Box::new(|| {}));
    player.shared.audio.set_volume(0.0);

    player.open(wav.clone());
    let played = wait_for(&player, "the file to load", |s| s.source == wav && !matches!(s.state, State::Idle | State::Loading));
    let in_memory = player.history();
    let cleared = player.clear_history();
    drop(player);

    assert_eq!(played.state, State::Playing, "{played:?}");
    assert_eq!(targets_and_titles(&in_memory), [pair(&wav, "sine.wav")]);
    assert_eq!(cleared, Ok(()));
    assert!(!data_file.exists(), "Player::new wrote {}", data_file.display());
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    entries
        .flatten()
        .flat_map(|e| {
            let path = e.path();
            if path.is_dir() { walk(&path) } else { vec![path] }
        })
        .collect()
}
