//! Playlist files (.pls, .m3u) as lists to step through: `playr::lists::parse`, and a `Player`
//! playing them entry by entry (local files and addresses served from 127.0.0.1).
//!
//! A `Player` opens the default audio device, so the player tests take `AUDIO_DEVICE` and play
//! at volume zero.

mod common;

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use common::http::{RadioServer, STATION, TITLES};
use common::range_http::{Mode, RangeServer};
use common::{finish, fixture, fixture_bytes};
use playr::lists::parse;
use playr::player::{Player, State, Status};
use playr::spotify::Entry;

fn entry(title: &str, duration: Option<f64>, audio: &str) -> Entry {
    Entry { title: title.to_string(), subtitle: String::new(), duration, audio: Some(audio.to_string()) }
}

fn fresh_dir(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("playr-lists").join(test);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create test dir");
    dir
}

fn joined(base: &Path, relative: &str) -> String {
    base.join(relative).to_string_lossy().into_owned()
}

#[test]
fn parse_reads_pls_and_m3u_text_into_entries() {
    let base = fresh_dir("parse_base");
    let flac = fixture("sine.flac").to_str().unwrap().to_string();
    let mp3 = fixture("sine.mp3").to_str().unwrap().to_string();
    let cases: Vec<(&str, String, Option<&Path>, Vec<Entry>)> = vec![
        (
            "pls rows grouped by number, sorted numerically, not by text order",
            "[playlist]\nNumberOfEntries=3\nFile10=http://ten.example/x.mp3\nTitle3=Three\nFile3=http://three.example/y.ogg\n\
             File1=http://one.example/z.flac\nLength1=61\nTitle10=Ten\nLength3=183\nTitle1=One\nVersion=2\n"
                .into(),
            None,
            vec![
                entry("One", Some(61.0), "http://one.example/z.flac"),
                entry("Three", Some(183.0), "http://three.example/y.ogg"),
                entry("Ten", None, "http://ten.example/x.mp3"),
            ],
        ),
        (
            "pls row without a file is dropped",
            "[playlist]\nFile1=http://a.example/a.mp3\nTitle2=Orphan title\nLength2=30\nFile3=http://c.example/c.mp3\nTitle3=Third\n".into(),
            None,
            vec![entry("a.mp3", None, "http://a.example/a.mp3"), entry("Third", None, "http://c.example/c.mp3")],
        ),
        (
            "pls keys in any case with spaces, values trimmed, BOM and CRLF",
            "\u{feff}[Playlist]\r\nfile1 = http://a.example/a.mp3 \r\nTITLE1=  Spaced title \r\nLENGTH1= 12.5\r\nFILE2=http://b.example/b.mp3\r\nTitle2=   \r\n"
                .into(),
            None,
            vec![entry("Spaced title", Some(12.5), "http://a.example/a.mp3"), entry("b.mp3", None, "http://b.example/b.mp3")],
        ),
        (
            "pls: a repeated FileN keeps the last address",
            "[playlist]\nFile1=http://a.example/first.mp3\nTitle1=One\nFile1=http://a.example/second.mp3\nFile2=http://b.example/b.mp3\n".into(),
            None,
            vec![entry("One", None, "http://a.example/second.mp3"), entry("b.mp3", None, "http://b.example/b.mp3")],
        ),
        (
            "pls: only a positive length is a duration",
            "[playlist]\nFile1=http://h.example/1.mp3\nLength1=-1\nFile2=http://h.example/2.mp3\nLength2=0\nFile3=http://h.example/3.mp3\nLength3=0.5\n\
             File4=http://h.example/4.mp3\nLength4=long\nFile5=http://h.example/5.mp3\nLength5=245\n"
                .into(),
            None,
            vec![
                entry("1.mp3", None, "http://h.example/1.mp3"),
                entry("2.mp3", None, "http://h.example/2.mp3"),
                entry("3.mp3", Some(0.5), "http://h.example/3.mp3"),
                entry("4.mp3", None, "http://h.example/4.mp3"),
                entry("5.mp3", Some(245.0), "http://h.example/5.mp3"),
            ],
        ),
        (
            "m3u: #EXTINF gives the next address its length and title, other comments skipped",
            "\u{feff}#EXTM3U\r\n#PLAYLIST:Ignored name\r\n#EXTINF:215,Artist One - Song One\r\nhttp://a.example/one.mp3\r\n\r\n\
             # a plain comment\r\nhttp://b.example/two.ogg\r\n#EXTINF:-1 tvg-id=\"x\",Live Radio\r\n#EXTGRP:news\r\nhttp://c.example/live\r\n"
                .into(),
            None,
            vec![
                entry("Artist One - Song One", Some(215.0), "http://a.example/one.mp3"),
                entry("two.ogg", None, "http://b.example/two.ogg"),
                entry("Live Radio", None, "http://c.example/live"),
            ],
        ),
        (
            "m3u: an #EXTINF is used up by a line that is dropped",
            "#EXTM3U\n#EXTINF:100,Belongs to the dropped line\nrelative\\song.mp3\nhttp://b.example/kept.mp3\n".into(),
            None,
            vec![entry("kept.mp3", None, "http://b.example/kept.mp3")],
        ),
        (
            "m3u: #EXTINF without a comma or with a blank title",
            "#EXTM3U\n#EXTINF:42\nhttp://a.example/x/first.mp3\n#EXTINF:43,   \nhttp://a.example/x/second.mp3\n".into(),
            None,
            vec![entry("first.mp3", Some(42.0), "http://a.example/x/first.mp3"), entry("second.mp3", Some(43.0), "http://a.example/x/second.mp3")],
        ),
        (
            "HLS media playlist",
            "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6.0,\nhttp://h.example/seg0.ts\n#EXTINF:6.0,\nhttp://h.example/seg1.ts\n".into(),
            None,
            vec![],
        ),
        (
            "HLS master playlist",
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=128000\nhttp://h.example/low.m3u8\nhttp://h.example/high.m3u8\n".into(),
            None,
            vec![],
        ),
        ("empty text", "".into(), None, vec![]),
        ("comments only", "#EXTM3U\n# nothing here\n\n".into(), None, vec![]),
        ("one address", "http://solo.example/only.mp3\n".into(), None, vec![entry("only.mp3", None, "http://solo.example/only.mp3")]),
        (
            "addresses kept as they are, even with a base",
            "HTTPS://Cdn.Example/Path/Song.MP3?sig=a%20b&x=1#t=5\nhttp://radio.example/live/stream\n".into(),
            Some(&base),
            vec![
                entry("Song.MP3", None, "HTTPS://Cdn.Example/Path/Song.MP3?sig=a%20b&x=1#t=5"),
                entry("http://radio.example/live/stream", None, "http://radio.example/live/stream"),
            ],
        ),
        (
            "relative paths joined to the base, absolute paths kept",
            format!("#EXTM3U\n#EXTINF:3,Sub folder\nsub/deeper/one.mp3\ntwo.wav\n{flac}\n"),
            Some(&base),
            vec![
                entry("Sub folder", Some(3.0), &joined(&base, "sub/deeper/one.mp3")),
                entry("two.wav", None, &joined(&base, "two.wav")),
                entry("sine.flac", None, &flac),
            ],
        ),
        (
            "without a base relative paths are dropped",
            format!("[playlist]\nFile1=relative.mp3\nFile2={mp3}\nFile3=..\\up.flac\nFile4=http://h.example/a.ogg\n"),
            None,
            vec![entry("sine.mp3", None, &mp3), entry("a.ogg", None, "http://h.example/a.ogg")],
        ),
    ];
    let mut bad = Vec::new();
    for (case, text, base, expected) in cases {
        let entries = parse(&text, base);
        expect!(bad, entries == expected, "{case}:\n    got      {entries:?}\n    expected {expected:?}");
    }
    finish(bad);
}

#[test]
#[cfg(windows)]
fn windows_drive_and_network_paths_and_file_urls_are_kept_as_paths() {
    let base = Path::new("D:\\lists");
    let text = "file:///C:/Music/Album/03 Track.flac\nC:\\Music\\b.mp3\n\\\\nas\\share\\c.wav\nsub\\d.ogg\n";

    let entries = parse(text, Some(base));

    assert_eq!(
        entries,
        [
            entry("03 Track.flac", None, "C:/Music/Album/03 Track.flac"),
            entry("b.mp3", None, "C:\\Music\\b.mp3"),
            entry("c.wav", None, "\\\\nas\\share\\c.wav"),
            entry("d.ogg", None, "D:\\lists\\sub\\d.ogg"),
        ]
    );
}

#[test]
#[cfg(windows)]
fn file_urls_are_decoded_into_windows_paths() {
    let base = Path::new("D:\\lists");
    let cases = [
        ("percent escapes decoded", "file:///C:/My%20Music/a.mp3", "a.mp3", "C:/My Music/a.mp3".to_string()),
        ("upper-case scheme", "FILE:///E:/x/b.flac", "b.flac", "E:/x/b.flac".to_string()),
        ("localhost dropped", "file://localhost/F:/y/c.wav", "c.wav", "F:/y/c.wav".to_string()),
        ("UTF-8 escapes", "file:///G:/Muzyka/Ko%C5%82ysanka%2C%20cz.2.ogg", "Kołysanka, cz.2.ogg", "G:/Muzyka/Kołysanka, cz.2.ogg".to_string()),
        ("broken escapes kept", "file:///H:/odd%zz%2", "odd%zz%2", "H:/odd%zz%2".to_string()),
        ("lower-case hex", "file:///C:/a%c3%a9.mp3", "aé.mp3", "C:/aé.mp3".to_string()),
        ("a host is a network share", "file://sub/one%20two.mp3", "one two.mp3", "//sub/one two.mp3".to_string()),
        ("network share with escapes", "file://server/share/a%20b.mp3", "a b.mp3", "//server/share/a b.mp3".to_string()),
        ("localhost before a drive", "file://localhost/C:/x.mp3", "x.mp3", "C:/x.mp3".to_string()),
    ];
    let mut bad = Vec::new();
    for (case, line, title, path) in cases {
        let entries = parse(&format!("{line}\n"), Some(base));
        let expected = vec![entry(title, None, &path)];
        expect!(bad, entries == expected, "{case}:\n    got      {entries:?}\n    expected {expected:?}");
    }
    finish(bad);
}

#[test]
#[cfg(unix)]
fn file_urls_are_decoded_into_unix_paths() {
    let entries = parse("file:///home/u/My%20Music/a.mp3\nfile://localhost/srv/b.flac\n", Some(Path::new("/lists")));

    assert_eq!(entries, [entry("a.mp3", None, "/home/u/My Music/a.mp3"), entry("b.flac", None, "/srv/b.flac")]);
}

#[test]
fn join_url_resolves_a_relative_line_against_the_lists_address() {
    let list = "https://cdn.example:8443/music/lists/mix.m3u?token=7#top";
    let cases = [
        ("name in the same folder", list, "two.flac", Some("https://cdn.example:8443/music/lists/two.flac")),
        ("sub folder", list, "sub/three.ogg", Some("https://cdn.example:8443/music/lists/sub/three.ogg")),
        ("from the host's root", list, "/radio/live.mp3", Some("https://cdn.example:8443/radio/live.mp3")),
        ("another host, same scheme", list, "//mirror.example/a.mp3", Some("https://mirror.example/a.mp3")),
        ("list at the host's root", "http://h.example/mix.pls", "a.mp3", Some("http://h.example/a.mp3")),
        ("list address without a path", "http://h.example", "a.mp3", Some("http://h.example/a.mp3")),
        ("list address ending in a slash", "http://h.example/lists/", "a.mp3", Some("http://h.example/lists/a.mp3")),
        ("no host", "http:///mix.m3u", "a.mp3", None),
        ("not an address", "mix.m3u", "a.mp3", None),
    ];
    let mut bad = Vec::new();
    for (case, list_url, relative, expected) in cases {
        let actual = playr::lists::join_url(list_url, relative);
        expect!(bad, actual.as_deref() == expected, "{case}: {actual:?}, expected {expected:?}");
    }
    finish(bad);
}

#[test]
fn join_url_percent_encodes_what_a_url_may_not_hold_and_keeps_escapes_and_punctuation() {
    let list = "http://h.example/music/mix.m3u";
    let cases = [
        ("space", "My Song.mp3", "http://h.example/music/My%20Song.mp3"),
        ("non-ASCII letter", "Kołysanka.mp3", "http://h.example/music/Ko%C5%82ysanka.mp3"),
        ("quotes and angle brackets", "\"a\"<b>.mp3", "http://h.example/music/%22a%22%3Cb%3E.mp3"),
        ("backslash, caret, backtick, braces and bar", "x\\^`{|}.mp3", "http://h.example/music/x%5C%5E%60%7B%7C%7D.mp3"),
        ("an existing escape is kept", "My%20Song.mp3", "http://h.example/music/My%20Song.mp3"),
        ("a percent sign at the end", "50%", "http://h.example/music/50%25"),
        ("a percent sign with one hex digit", "x%4", "http://h.example/music/x%254"),
        ("a percent sign before a letter that is not hex", "%G1.mp3", "http://h.example/music/%25G1.mp3"),
        ("URL punctuation is kept, a hash is encoded", "a-b_c.~!$&'()*+,;=:@/d.mp3?x=1#t", "http://h.example/music/a-b_c.~!$&'()*+,;=:@/d.mp3?x=1%23t"),
        ("a hash is part of the file name", "Track #1.mp3", "http://h.example/music/Track%20%231.mp3"),
        ("encoded from the host's root too", "/All Hits/1.mp3", "http://h.example/All%20Hits/1.mp3"),
    ];
    let mut bad = Vec::new();
    for (case, relative, expected) in cases {
        let actual = playr::lists::join_url(list, relative);
        expect!(bad, actual.as_deref() == Some(expected), "{case}: {actual:?}, expected {expected:?}");
    }
    finish(bad);
}

#[test]
fn join_url_encodes_a_percent_sign_that_starts_no_escape() {
    let actual = playr::lists::join_url("http://h.example/music/mix.m3u", "100% Hits.mp3");

    assert_eq!(actual.as_deref(), Some("http://h.example/music/100%25%20Hits.mp3"));
}

#[test]
fn parse_remote_plays_relative_lines_from_the_lists_address_and_nothing_but_http() {
    let list = "http://h.example/music/lists/mix.m3u";
    let text = "#EXTM3U\n#EXTINF:3,Same folder\none.mp3\nsub\\two.flac\n/root/three.ogg\n//cdn.example/four.wav\n\
                http://other.example/five.opus\nfile:///C:/Local/six.mp3\nfile://evil.example/share/a.mp3\nsmb://evil.example/x\n\
                ftp://files.example/b.mp3\njavascript:alert(1)\nC:\\Music\\seven.mp3\nHTTPS://Upper.example/eight.mp3\n";

    let entries = playr::lists::parse_remote(text, list);

    assert_eq!(
        entries,
        [
            entry("Same folder", Some(3.0), "http://h.example/music/lists/one.mp3"),
            entry("two.flac", None, "http://h.example/music/lists/sub/two.flac"),
            entry("three.ogg", None, "http://h.example/root/three.ogg"),
            entry("four.wav", None, "http://cdn.example/four.wav"),
            entry("five.opus", None, "http://other.example/five.opus"),
            entry("seven.mp3", None, "http://h.example/music/lists/C:/Music/seven.mp3"),
            entry("eight.mp3", None, "HTTPS://Upper.example/eight.mp3"),
        ]
    );
}

#[test]
fn no_station_in_the_radio_catalogue_is_a_playlist_file() {
    let urls: Vec<&str> = playr::radio::sections().flat_map(|s| s.stations.iter()).map(|s| s.url).collect();

    let playlists: Vec<&str> = urls.iter().copied().filter(|url| playr::net::is_playlist_name(url)).collect();

    assert!(urls.len() >= 20, "the catalogue has only {} stations", urls.len());
    assert_eq!(playlists, Vec::<&str>::new());
}

#[test]
fn list_name_is_the_last_part_of_the_path_or_address_without_query_or_fragment() {
    let cases = [
        ("address with query and fragment", "http://h.example/lists/mix.m3u?token=1#top", "mix.m3u"),
        ("address ending in a slash", "https://h.example/radio/lists/", "lists"),
        ("Windows path", "C:\\Users\\u\\Music\\road trip.pls", "road trip.pls"),
        ("forward-slash path", "/home/u/lists/night.m3u8", "night.m3u8"),
        ("bare name", "solo.pls", "solo.pls"),
        ("nothing after the slashes", "//", "//"),
        ("empty", "", ""),
    ];
    let mut bad = Vec::new();
    for (case, target, expected) in cases {
        let name = playr::lists::list_name(target);
        expect!(bad, name == expected, "{case}: {name:?}, expected {expected:?}");
    }
    finish(bad);
}

static AUDIO_DEVICE: Mutex<()> = Mutex::new(());

struct Session {
    player: Player,
    _device: MutexGuard<'static, ()>,
}

fn start() -> Session {
    let device = AUDIO_DEVICE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let player = Player::new(Box::new(|| {}));
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
        assert!(started.elapsed() < Duration::from_secs(40), "timed out waiting for {what}: {status:?}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

impl Session {
    fn open(&self, target: &str) -> Status {
        self.player.stop();
        wait_for(&self.player, "idle", |s| s.state == State::Idle);
        self.player.open(target);
        wait_for(&self.player, &format!("{target} to load"), |s| s.source == target && !matches!(s.state, State::Idle | State::Loading))
    }

    fn jump(&self, target: &str, index: usize) -> Status {
        self.player.stop();
        wait_for(&self.player, "idle", |s| s.state == State::Idle);
        self.player.open_entry(target, index);
        wait_for(&self.player, &format!("entry {index} to load"), |s| s.source == target && !matches!(s.state, State::Idle | State::Loading))
    }
}

/// (entry, title, info, notes) of a status.
fn seen(s: &Status) -> (usize, String, String, Vec<String>) {
    (s.entry, s.title.clone(), s.info.clone(), s.notes.clone())
}

fn row(entry: usize, title: &str, info: &str, notes: &[String]) -> (usize, String, String, Vec<String>) {
    (entry, title.to_string(), info.to_string(), notes.to_vec())
}

#[test]
fn a_local_pls_plays_entry_by_entry_skips_a_missing_file_with_a_note_and_ends_after_the_last() {
    let dir = fresh_dir("play_local_pls");
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("first.wav"), fixture_bytes("sine.wav")).unwrap();
    std::fs::write(dir.join("sub").join("last.flac"), fixture_bytes("sine.flac")).unwrap();
    let mp3 = fixture("sine.mp3").to_str().unwrap().to_string();
    let pls = dir.join("mix.pls");
    std::fs::write(
        &pls,
        format!("[playlist]\nNumberOfEntries=4\nFile1=first.wav\nTitle1=Opening tone\nFile2={mp3}\nFile3=gone.ogg\nTitle3=Gone tone\nFile4=sub/last.flac\nVersion=2\n"),
    )
    .unwrap();
    let pls = pls.to_str().unwrap().to_string();
    let missing = joined(&dir, "gone.ogg");
    let why = format!("cannot open {missing}: {}", std::fs::File::open(&missing).unwrap_err());
    let s = start();
    s.player.set_speed(2.0);

    s.player.open(pls.clone());
    let mut played: Vec<Status> = Vec::new();
    let ended = wait_for(&s.player, "the end of the list", |st| {
        if st.state == State::Playing && st.source == pls && played.last().map(|p| p.entry) != Some(st.entry) {
            played.push(st.clone());
        }
        st.state == State::Ended && st.entry == 3
    });

    let rows: Vec<_> = played.iter().map(seen).collect();
    assert_eq!(
        rows,
        [
            row(0, "Opening tone", "wave · mix.pls · 1/4", &[]),
            row(1, "sine.mp3", "mp3 · mix.pls · 2/4", &[]),
            row(3, "last.flac", "flac · mix.pls · 4/4", &[format!("skipped Gone tone: {why}")]),
        ]
    );
    assert_eq!((ended.entry, ended.source.as_str()), (3, pls.as_str()));
    assert!(played.iter().all(|p| p.seekable && p.playlist.as_ref().map(|l| l.entries.len()) == Some(4)), "{played:#?}");
}

#[test]
fn open_entry_jumps_within_a_local_m3u_whose_list_is_read_once_for_the_hour() {
    let dir = fresh_dir("play_local_m3u");
    for (name, fixture_name) in [("a.wav", "sine.wav"), ("b.flac", "sine.flac"), ("c.mp3", "sine.mp3")] {
        std::fs::write(dir.join(name), fixture_bytes(fixture_name)).unwrap();
    }
    let m3u = dir.join("three.m3u");
    std::fs::write(&m3u, "#EXTM3U\n#EXTINF:3,Song A\na.wav\n#EXTINF:3,Song B\nb.flac\n#EXTINF:3,Song C\nc.mp3\n").unwrap();
    let m3u = m3u.to_str().unwrap().to_string();
    let s = start();

    let first = s.open(&m3u);
    std::fs::write(&m3u, "#EXTM3U\n#EXTINF:3,Changed\nc.mp3\nb.flac\n").unwrap();
    let third = s.jump(&m3u, 2);
    let second = s.jump(&m3u, 1);
    let reopened = s.open(&m3u);

    let rows: Vec<_> = [&first, &third, &second, &reopened].into_iter().map(seen).collect();
    assert_eq!(
        rows,
        [
            row(0, "Song A", "wave · three.m3u · 1/3", &[]),
            row(2, "Song C", "mp3 · three.m3u · 3/3", &[]),
            row(1, "Song B", "flac · three.m3u · 2/3", &[]),
            row(0, "Changed", "mp3 · three.m3u · 1/2", &[]),
        ]
    );
    assert_eq!([first.state, third.state, second.state, reopened.state], [State::Playing; 4]);
}

#[test]
fn a_remote_m3u_plays_its_addresses_and_its_relative_lines_over_http_as_seekable_files() {
    let media = RangeServer::start(vec![
        ("/media/one.mp3", Mode::Ranged, fixture_bytes("sine.mp3")),
        ("/media/two.flac", Mode::Ranged, fixture_bytes("sine.flac")),
    ]);
    let text = format!(
        "#EXTM3U\n#EXTINF:3,Remote one\n{}\n#EXTINF:3,Relative\nc.wav\n#EXTINF:3,Remote two\n{}\n",
        media.url("/media/one.mp3"),
        media.url("/media/two.flac")
    );
    let lists = RangeServer::start(vec![
        ("/lists/mix.m3u", Mode::IgnoresRange, text.into_bytes()),
        ("/lists/c.wav", Mode::Ranged, fixture_bytes("sine.wav")),
    ]);
    let link = lists.url("/lists/mix.m3u");
    let s = start();

    let first = s.open(&link);
    let second = s.jump(&link, 1);
    let third = s.jump(&link, 2);

    assert_eq!(
        [seen(&first), seen(&second), seen(&third)],
        [
            row(0, "Remote one", "mp3 · mix.m3u · 1/3", &[]),
            row(1, "Relative", "wave · mix.m3u · 2/3", &[]),
            row(2, "Remote two", "flac · mix.m3u · 3/3", &[]),
        ]
    );
    let kinds: Vec<(State, bool, bool)> = [&first, &second, &third].iter().map(|s| (s.state, s.seekable, s.live)).collect();
    assert_eq!(kinds, [(State::Playing, true, false); 3]);
    assert!(lists.requests().iter().any(|r| r.path == "/lists/c.wav"), "the relative line was not fetched from the list's folder");
}

#[test]
fn a_local_pls_of_radio_streams_plays_them_live_with_their_song_titles() {
    let station = RadioServer::start(fixture_bytes("radio.mp3"));
    let dir = fresh_dir("play_radio_pls");
    let pls = dir.join("radio.pls");
    std::fs::write(
        &pls,
        format!("[playlist]\nFile1={}\nTitle1=Station A\nFile2={}\nTitle2=Plain download\n", station.url("/stream"), station.url("/plain.mp3")),
    )
    .unwrap();
    let pls = pls.to_str().unwrap().to_string();
    let s = start();

    let radio = s.open(&pls);
    let titled = wait_for(&s.player, "the station's first song title", |st| st.now_playing.is_some());
    let download = s.jump(&pls, 1);

    assert_eq!(seen(&radio), row(0, "Station A", "mp3 · radio.pls · 1/2", &[]));
    assert_eq!((radio.state, radio.live, radio.seekable), (State::Playing, true, false));
    assert_eq!((titled.entry, titled.now_playing.as_deref()), (0, Some(TITLES[0])));
    assert_eq!(seen(&download), row(1, "Plain download", "mp3 · radio.pls · 2/2", &[]));
    assert_eq!((download.state, download.live, download.now_playing.as_deref()), (State::Playing, false, None));
}

#[test]
fn a_local_playlist_whose_one_entry_is_a_local_file_plays_it_as_a_one_entry_list() {
    let dir = fresh_dir("play_one_local");
    std::fs::write(dir.join("only.flac"), fixture_bytes("sine.flac")).unwrap();
    let m3u = dir.join("solo.m3u");
    std::fs::write(&m3u, "#EXTM3U\n#EXTINF:3,The only one\nonly.flac\n").unwrap();
    let m3u = m3u.to_str().unwrap().to_string();
    let s = start();
    s.player.set_speed(2.0);

    let playing = s.open(&m3u);
    let ended = wait_for(&s.player, "the end", |st| st.state != State::Playing);

    assert_eq!(seen(&playing), row(0, "The only one", "flac · solo.m3u · 1/1", &[]));
    assert_eq!((playing.state, playing.playlist.as_ref().map(|l| l.entries.len())), (State::Playing, Some(1)));
    assert_eq!((ended.state, ended.entry, ended.error), (State::Ended, 0, None));
}

#[test]
fn a_list_opened_with_spaces_around_its_name_still_skips_a_failed_entry_and_plays_on() {
    let dir = fresh_dir("play_spaced_target");
    std::fs::write(dir.join("second.wav"), fixture_bytes("sine.wav")).unwrap();
    let pls = dir.join("spaced.pls");
    std::fs::write(&pls, "[playlist]\nFile1=first-missing.mp3\nTitle1=Missing one\nFile2=second.wav\nTitle2=Second one\n").unwrap();
    let missing = joined(&dir, "first-missing.mp3");
    let why = format!("cannot open {missing}: {}", std::fs::File::open(&missing).unwrap_err());
    let s = start();

    s.player.open(format!("  {}\t", pls.to_str().unwrap()));
    let played = wait_for(&s.player, "the second entry to play", |st| st.state == State::Playing);

    assert_eq!(seen(&played), row(1, "Second one", "wave · spaced.pls · 2/2", &[format!("skipped Missing one: {why}")]));
}

#[test]
fn a_remote_list_of_only_a_file_url_is_not_a_list_and_opens_nothing_local() {
    let lists = RangeServer::start(vec![("/lists/evil.m3u", Mode::IgnoresRange, b"#EXTM3U\nfile://evil.example/share/a.mp3\n".to_vec())]);
    let link = lists.url("/lists/evil.m3u");
    let s = start();

    let status = s.open(&link);

    assert_eq!((status.state, status.error.as_deref(), status.playlist.is_none()), (State::Failed, Some("playlist contains no stream address"), true));
}

#[test]
fn a_remote_list_of_one_relative_line_plays_it_from_the_lists_folder() {
    let lists = RangeServer::start(vec![
        ("/lists/one.m3u", Mode::IgnoresRange, b"#EXTM3U\n#EXTINF:3,Only song\nsong.flac\n".to_vec()),
        ("/lists/song.flac", Mode::Ranged, fixture_bytes("sine.flac")),
    ]);
    let link = lists.url("/lists/one.m3u");
    let s = start();

    let status = s.open(&link);

    assert_eq!(seen(&status), row(0, "Only song", "flac · one.m3u · 1/1", &[]));
    assert_eq!((status.state, status.seekable, status.playlist.as_ref().map(|l| l.entries.len())), (State::Playing, true, Some(1)));
    assert!(lists.requests().iter().any(|r| r.path == "/lists/song.flac"), "the song was not fetched from the list's folder");
}

#[test]
fn a_one_entry_remote_playlist_plays_the_station_it_names_as_before() {
    let server = RadioServer::start(fixture_bytes("radio.mp3"));
    let s = start();
    let mut bad = Vec::new();
    for path in ["/list.m3u", "/list.pls"] {
        let link = server.url(path);

        let status = s.open(&link);

        let got = (status.state, status.title.as_str(), status.live, status.playlist.is_none(), status.entry);
        expect!(bad, got == (State::Playing, STATION, true, true, 0), "{path}: {status:?}");
    }
    finish(bad);
}
