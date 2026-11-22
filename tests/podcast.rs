//! `playr::podcast` and the podcast path of `playr::spotify::fetch`, without the network: the
//! iTunes Search API, the RSS feed, the episode files and the Spotify embed page are all
//! served by local test servers. The search and embed addresses come from environment
//! variables, which belong to the whole process, so the tests that need them run their body
//! in a fresh copy of this test binary (see [`run_child`]); the parent keeps the server and
//! checks what was asked of it afterwards.

mod common;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use common::finish;
use common::fixture_bytes;
use common::range_http::{Mode, RangeRequest, RangeServer};
use playr::demux::Demuxer;
use playr::podcast::{self, Episode, Feed, feed_from_search, find_episode, normalize, parse_duration, parse_rss};
use playr::spotify::{self, Entry, Playlist};
use serde_json::json;

const CHILD: &str = "PLAYR_TEST_CHILD";
const SHOW: &str = "Zosia Cebulka - bajki do snu i zabawy";
const SHOW_ID: &str = "033OZDMSatg3WCetEI9bge";
const EPISODE_ID: &str = "4kGynGaUBQwwRI3xTvpDPN";

fn is_child() -> bool {
    std::env::var_os(CHILD).is_some()
}

/// Runs `test` alone in a copy of this binary with `env` added and fails if it fails there.
fn run_child(test: &str, env: Vec<(&str, String)>) {
    let out = Command::new(std::env::current_exe().expect("test binary"))
        .args([test, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .env_remove(podcast::SEARCH_ENV)
        .env_remove(spotify::EMBED_ENV)
        .envs(env)
        .output()
        .expect("start the child test");
    let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "{test} failed in the child process:\n{stdout}\n{stderr}");
    assert!(stdout.contains("test result: ok. 1 passed"), "{test} did not run in the child process:\n{stdout}");
}

fn no_abort() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

fn paths(requests: &[RangeRequest]) -> Vec<String> {
    requests.iter().map(|r| r.path.clone()).collect()
}

fn itunes_answer(results: serde_json::Value) -> String {
    let count = results.as_array().map_or(0, Vec::len);
    json!({ "resultCount": count, "results": results }).to_string()
}

fn podcast_result(name: &str, feed: Option<&str>) -> serde_json::Value {
    let mut r = json!({ "wrapperType": "track", "kind": "podcast", "collectionId": 1_234_567, "collectionName": name });
    if let Some(feed) = feed {
        r["feedUrl"] = json!(feed);
    }
    r
}

mod normalizing {
    use super::*;

    #[test]
    fn keeps_lower_case_letters_and_digits_with_single_spaces() {
        let cases = [
            ("punctuation and dash", "Zosia Cebulka - bajki do snu i zabawy", "zosia cebulka bajki do snu i zabawy"),
            ("runs of spaces and edges", "  Hello,   World!! ", "hello world"),
            ("leading punctuation", "...Odcinek 12", "odcinek 12"),
            ("non-ASCII capitals", "ŁÓDŹ Nocą", "łódź nocą"),
            ("combining acute dropped without a space", "Cafe\u{301} Noir", "cafe noir"),
            ("combining mark from lower-casing İ", "İstanbul", "istanbul"),
            ("en dash and brackets", "[Łagodny początek] LIST – 2 | bajka", "łagodny początek list 2 bajka"),
            ("only punctuation", "!!! ???", ""),
            ("empty", "", ""),
        ];
        let mut bad = Vec::new();
        for (case, input, expected) in cases {
            let actual = normalize(input);
            expect!(bad, actual == expected, "{case}: normalize({input:?}) = {actual:?}, expected {expected:?}");
        }
        finish(bad);
    }
}

mod search_answer {
    use super::*;

    const FEED: &str = "https://feeds.example/zosia.xml";

    #[test]
    fn the_exact_normalized_name_match_wins_over_an_earlier_near_miss() {
        let answer = itunes_answer(json!([
            podcast_result("Zosia Cebulka", Some("https://feeds.example/near-miss.xml")),
            podcast_result("ZOSIA CEBULKA – bajki do snu i zabawy!", Some(FEED)),
            podcast_result("Zosia Cebulka - bajki do snu i zabawy", Some("https://feeds.example/second-exact.xml")),
        ]));

        assert_eq!(feed_from_search(&answer, SHOW), Some(FEED.to_string()));
    }

    #[test]
    fn an_exact_match_without_a_usable_feed_is_passed_over_for_the_next_one() {
        let cases = [
            ("no feedUrl", podcast_result(SHOW, None)),
            ("ftp feed", podcast_result(SHOW, Some("ftp://feeds.example/zosia.xml"))),
            ("feed is not a link", podcast_result(SHOW, Some("zosia.xml"))),
        ];
        let mut bad = Vec::new();
        for (case, unusable) in cases {
            let answer = itunes_answer(json!([unusable, podcast_result(SHOW, Some(FEED))]));
            let actual = feed_from_search(&answer, SHOW);
            expect!(bad, actual == Some(FEED.to_string()), "{case}: {actual:?}");
        }
        finish(bad);
    }

    #[test]
    fn answers_without_an_exact_match_or_usable_feed_give_none() {
        let cases = [
            ("only near misses", itunes_answer(json!([podcast_result("Zosia Cebulka", Some(FEED)), podcast_result("Zosia Cebulka - bajki do snu", Some(FEED))]))),
            ("name with more words", itunes_answer(json!([podcast_result("Zosia Cebulka - bajki do snu i zabawy dla dzieci", Some(FEED))]))),
            ("exact name, ftp feed only", itunes_answer(json!([podcast_result(SHOW, Some("ftp://feeds.example/zosia.xml"))]))),
            ("exact name, no feed", itunes_answer(json!([podcast_result(SHOW, None)]))),
            ("no results", itunes_answer(json!([]))),
            ("results not a list", json!({ "results": { "collectionName": SHOW, "feedUrl": FEED } }).to_string()),
            ("no results key", json!({ "resultCount": 0 }).to_string()),
            ("not JSON", "<html>rate limited</html>".to_string()),
            ("empty body", String::new()),
        ];
        let mut bad = Vec::new();
        for (case, answer) in cases {
            let actual = feed_from_search(&answer, SHOW);
            expect!(bad, actual.is_none(), "{case}: {actual:?}, expected None");
        }
        finish(bad);
    }
}

mod durations {
    use super::*;

    #[test]
    fn seconds_minutes_and_hours_forms_are_read() {
        let cases = [
            ("seconds", "862", 862.0),
            ("minutes", "14:22", 862.0),
            ("hours", "00:14:22", 862.0),
            ("one hour", "1:00:00", 3600.0),
            ("fraction", "1:30.5", 90.5),
            ("spaces around", " 95 ", 95.0),
        ];
        let mut bad = Vec::new();
        for (case, input, expected) in cases {
            let actual = parse_duration(input);
            expect!(bad, actual == Some(expected), "{case}: parse_duration({input:?}) = {actual:?}, expected Some({expected})");
        }
        finish(bad);
    }

    #[test]
    fn empty_non_numeric_negative_and_zero_values_are_none() {
        let cases = [
            ("empty", ""),
            ("blank", "   "),
            ("text", "abc"),
            ("negative", "-5"),
            ("negative part", "1:-2"),
            ("zero", "0"),
            ("all zero", "00:00:00"),
            ("trailing separator", "14:"),
            ("leading separator", ":14"),
            ("not a number", "NaN"),
            ("inf", "inf"),
            ("infinity", "infinity"),
            ("overflows to infinity", "1e400"),
        ];
        let mut bad = Vec::new();
        for (case, input) in cases {
            let actual = parse_duration(input);
            expect!(bad, actual.is_none(), "{case}: parse_duration({input:?}) = {actual:?}, expected None");
        }
        finish(bad);
    }
}

mod rss {
    use super::*;

    fn episode(title: &str, audio: &str, duration: Option<f64>) -> Episode {
        Episode { title: title.into(), audio: audio.into(), duration }
    }

    const FEED_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:itunes="http://www.itunes.com/dtds/podcast-1.0.dtd">
<channel>
  <image><title>Cover of the show</title><url>https://cdn.example/cover.png</url></image>
  <title>Zosia &amp; Cebulka</title>
  <item>
    <title><![CDATA[Odcinek <1> & "start" &amp;]]></title>
    <itunes:title>Not the episode title</itunes:title>
    <enclosure url="https://cdn.example/ep1.mp3?a=1&amp;b=2" type="audio/mpeg" length="100"/>
    <itunes:duration>00:14:22</itunes:duration>
  </item>
  <item>
    <title>Bajka o &#321;osiu &#x141; &lt;2&gt;</title>
    <enclosure url="http://cdn.example/ep2.m4a" type="Audio/MP4"></enclosure>
    <itunes:duration>65</itunes:duration>
  </item>
  <item>
    <title>Only a video</title>
    <enclosure url="https://cdn.example/v.mp4" type="video/mp4"/>
    <itunes:duration>30</itunes:duration>
  </item>
  <item>
    <enclosure url="https://cdn.example/untitled.mp3" type="audio/mpeg"/>
  </item>
  <item>
    <title>   </title>
    <enclosure url="https://cdn.example/blank-title.mp3" type="audio/mpeg"/>
  </item>
  <item>
    <title>Bez typu</title>
    <enclosure url="https://cdn.example/untyped.mp3"/>
  </item>
  <item>
    <title>Przez FTP</title>
    <enclosure url="ftp://cdn.example/ftp.mp3" type="audio/mpeg"/>
  </item>
  <item>
    <title>Wideo, potem dźwięk</title>
    <enclosure url="https://cdn.example/v2.mp4" type="video/mp4"/>
    <enclosure url="https://cdn.example/a2.mp3" type="audio/mpeg"/>
    <itunes:duration>abc</itunes:duration>
  </item>
  <item>
    <title>Dźwięk, potem wideo</title>
    <enclosure url="https://cdn.example/a3.mp3" type="audio/mpeg"/>
    <enclosure url="https://cdn.example/v3.mp4" type="video/mp4"/>
    <itunes:duration>1:40</itunes:duration>
  </item>
  <item>
    <title>Dwa pliki</title>
    <enclosure url="https://cdn.example/first.mp3" type="audio/mpeg"/>
    <enclosure url="https://cdn.example/second.mp3" type="audio/mpeg"/>
  </item>
</channel>
</rss>"#;

    #[test]
    fn reads_the_channel_name_and_each_titled_item_with_an_audio_enclosure_in_order() {
        let feed = parse_rss(FEED_XML).expect("parse");

        assert_eq!(
            feed,
            Feed {
                name: "Zosia & Cebulka".into(),
                episodes: vec![
                    episode("Odcinek <1> & \"start\" &amp;", "https://cdn.example/ep1.mp3?a=1&b=2", Some(862.0)),
                    episode("Bajka o Łosiu Ł <2>", "http://cdn.example/ep2.m4a", Some(65.0)),
                    episode("Bez typu", "https://cdn.example/untyped.mp3", None),
                    episode("Wideo, potem dźwięk", "https://cdn.example/a2.mp3", None),
                    episode("Dźwięk, potem wideo", "https://cdn.example/a3.mp3", Some(100.0)),
                    episode("Dwa pliki", "https://cdn.example/first.mp3", None),
                ],
            }
        );
    }

    #[test]
    fn a_channel_without_a_title_is_called_podcast() {
        let xml = r#"<rss><channel><item><title>Jedyny</title><enclosure url="https://cdn.example/1.mp3" type="audio/mpeg"/></item></channel></rss>"#;

        let feed = parse_rss(xml).expect("parse");

        assert_eq!(feed, Feed { name: "Podcast".into(), episodes: vec![episode("Jedyny", "https://cdn.example/1.mp3", None)] });
    }

    #[test]
    fn a_feed_without_playable_episodes_is_an_error() {
        let no_sound = "the podcast feed lists no episodes with sound".to_string();
        let cases = [
            ("only a video", r#"<rss><channel><title>S</title><item><title>V</title><enclosure url="https://cdn.example/v.mp4" type="video/mp4"/></item></channel></rss>"#),
            ("no items", "<rss><channel><title>S</title></channel></rss>"),
            ("item without title", r#"<rss><channel><item><enclosure url="https://cdn.example/a.mp3"/></item></channel></rss>"#),
            ("plain text", "not a feed at all"),
            ("empty", ""),
        ];
        let mut bad = Vec::new();
        for (case, xml) in cases {
            let actual = parse_rss(xml);
            expect!(bad, actual == Err(no_sound.clone()), "{case}: {actual:?}");
        }
        finish(bad);
    }

    #[test]
    fn mismatched_tags_are_reported_as_invalid_xml() {
        let xml = r#"<rss><channel><item><title>A</title><enclosure url="https://cdn.example/a.mp3"/></channel></rss>"#;

        let err = parse_rss(xml).expect_err("mismatched tags");

        // The middle is quick-xml's own description of the fault.
        assert!(err.starts_with("the podcast feed is not valid XML (") && err.ends_with(')'), "{err}");
    }
}

mod episode_lookup {
    use super::*;

    fn feed() -> Feed {
        let titles = ["Intro", "Bajka o smoku, część 2", "BAJKA O SMOKU – część 2", "Cafe\u{301} nocą"];
        let episodes = titles
            .iter()
            .enumerate()
            .map(|(n, t)| Episode { title: t.to_string(), audio: format!("https://cdn.example/{n}.mp3"), duration: None })
            .collect();
        Feed { name: "Show".into(), episodes }
    }

    #[test]
    fn finds_the_first_episode_whose_normalized_title_matches() {
        let cases = [
            ("punctuation and case differ", "bajka o SMOKU – część 2!", Some(1)),
            ("combining accent dropped", "Cafe nocą", Some(3)),
            ("first item", "intro", Some(0)),
            ("prefix only", "Bajka o smoku", None),
            ("longer title", "Intro extended", None),
            ("empty", "", None),
        ];
        let feed = feed();
        let mut bad = Vec::new();
        for (case, title, expected) in cases {
            let actual = find_episode(&feed, title);
            expect!(bad, actual == expected, "{case}: find_episode({title:?}) = {actual:?}, expected {expected:?}");
        }
        finish(bad);
    }
}

/// A server that answers every request with a redirect to `target` and counts the requests.
struct Redirector {
    url: String,
    hits: Arc<AtomicUsize>,
}

impl Redirector {
    fn start(target: String) -> Self {
        let listener = {
            let _ports = common::http::PORTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            TcpListener::bind("127.0.0.1:0").expect("bind redirect server")
        };
        let url = format!("http://{}/old/episode.mp3", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { continue };
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") && conn.read(&mut byte).is_ok_and(|n| n == 1) {
                    head.push(byte[0]);
                }
                counter.fetch_add(1, Ordering::SeqCst);
                let reply = format!("HTTP/1.0 302 Found\r\nLocation: {target}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                let _ = conn.write_all(reply.as_bytes());
            }
        });
        Self { url, hits }
    }
}

mod opening_episodes {
    use super::*;

    fn first_pts(demux: &mut dyn Demuxer) -> f64 {
        demux.next_packet().expect("read").expect("a packet").pts()
    }

    #[test]
    fn with_range_support_the_episode_is_seekable_and_lands_near_the_target() {
        let server = RangeServer::start(vec![("/audio/ep.mp3", Mode::Ranged, fixture_bytes("sine.mp3"))]);

        let mut demux = podcast::open_episode(&server.url("/audio/ep.mp3?token=a1b2"), no_abort()).expect("open");
        let (seekable, duration) = (demux.info().seekable, demux.info().duration);
        let landed = demux.seek(2.0).expect("seek");
        let pts = first_pts(demux.as_mut());

        assert!(seekable);
        // sine.mp3 is a 3 s tone (tests/fixtures/generate.ps1).
        assert!(duration.is_some_and(|d| (d - 3.0).abs() < 0.1), "{duration:?}");
        assert!((landed - 2.0).abs() <= 0.1, "landed at {landed}");
        assert!((pts - landed).abs() <= 0.03, "reported {landed}, first packet at {pts}");
        assert!(server.requests().iter().all(|r| r.header("range").is_some()), "{:?}", server.requests());
    }

    #[test]
    fn a_far_seek_in_a_long_episode_jumps_to_the_byte_offset_instead_of_reading_up_to_it() {
        // radio.mp3: 4 s at a constant 64 kb/s (8000 bytes a second), no ID3 or Xing header,
        // so 150 copies are one valid 600 s stream in which second s starts near byte 8000 s.
        let long = fixture_bytes("radio.mp3").repeat(150);
        let len = long.len() as f64;
        let server = RangeServer::start(vec![("/long.mp3", Mode::Ranged, long)]);
        let mut demux = podcast::open_episode(&server.url("/long.mp3"), no_abort()).expect("open");
        let before = server.requests().len();

        let landed = demux.seek(300.0).expect("seek");
        let pts = first_pts(demux.as_mut());

        assert!((landed - 300.0).abs() <= 1.0, "landed at {landed}");
        assert!((pts - landed).abs() <= 0.05, "reported {landed}, first packet at {pts}");
        let starts: Vec<f64> = server.requests()[before..]
            .iter()
            .filter_map(|r| r.header("range")?.strip_prefix("bytes=")?.split('-').next()?.parse().ok())
            .collect();
        let near_target = |from: &f64| (from - 2_400_000.0).abs() <= len * 0.01;
        assert!(starts.iter().any(near_target), "no download started near byte 2,400,000 after the seek: {starts:?}");
    }

    #[test]
    fn without_range_support_it_still_plays_from_the_start_but_not_seekably() {
        let cases = [("ignores Range", Mode::IgnoresRange), ("206 without Content-Range", Mode::NoContentRange)];
        let start = first_pts(playr::demux::open_path(&common::fixture("sine.mp3")).expect("local file").as_mut());
        let mut bad = Vec::new();
        for (case, mode) in cases {
            let server = RangeServer::start(vec![("/ep.mp3", mode, fixture_bytes("sine.mp3"))]);
            match podcast::open_episode(&server.url("/ep.mp3"), no_abort()) {
                Ok(mut demux) => {
                    let seekable = demux.info().seekable;
                    let pts = first_pts(demux.as_mut());
                    expect!(bad, !seekable && pts == start, "{case}: seekable {seekable}, first packet at {pts}, expected {start}");
                }
                Err(e) => bad.push(format!("{case}: {e}")),
            }
        }
        finish(bad);
    }

    #[test]
    fn a_missing_file_is_an_error_naming_the_address() {
        let server = RangeServer::start(vec![]);
        let url = server.url("/gone.mp3");

        let err = podcast::open_episode(&url, no_abort()).err();

        assert_eq!(err, Some(format!("cannot open {url}: http status: 404")));
    }

    #[test]
    fn after_a_redirect_the_range_requests_go_straight_to_the_final_address() {
        let server = RangeServer::start(vec![("/files/episode.mp3", Mode::Ranged, fixture_bytes("sine.mp3"))]);
        let redirect = Redirector::start(server.url("/files/episode.mp3"));

        let mut demux = podcast::open_episode(&redirect.url, no_abort()).expect("open");
        demux.seek(2.0).expect("seek");
        first_pts(demux.as_mut());

        let requests = server.requests();
        assert_eq!(redirect.hits.load(Ordering::SeqCst), 1);
        assert!(requests.len() >= 2, "{requests:?}");
        assert!(requests.iter().all(|r| r.path == "/files/episode.mp3" && r.header("range").is_some()), "{requests:?}");
    }
}

/// A CDN that hands out expiring addresses: `/origin` redirects to `/v<n>/ep.mp3` of the
/// current generation n, which serves byte ranges until [`Rotating::expire`] moves on to the
/// next generation; from then on it answers 403, like a signed URL past its lifetime.
struct Rotating {
    base: String,
    generation: Arc<AtomicUsize>,
    log: Arc<std::sync::Mutex<Vec<(String, u16)>>>,
}

impl Rotating {
    fn start(file: Vec<u8>) -> Self {
        let listener = {
            let _ports = common::http::PORTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            TcpListener::bind("127.0.0.1:0").expect("bind rotating server")
        };
        let base = format!("http://{}", listener.local_addr().unwrap());
        let generation = Arc::new(AtomicUsize::new(1));
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (gen_, log_, base_, file) = (generation.clone(), log.clone(), base.clone(), Arc::new(file));
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { continue };
                let (generation, log, base, file) = (gen_.clone(), log_.clone(), base_.clone(), file.clone());
                std::thread::spawn(move || {
                    let mut head = Vec::new();
                    let mut byte = [0u8; 1];
                    while !head.ends_with(b"\r\n\r\n") && conn.read(&mut byte).is_ok_and(|n| n == 1) {
                        head.push(byte[0]);
                    }
                    let text = String::from_utf8_lossy(&head).to_string();
                    let path = text.split(' ').nth(1).unwrap_or("").to_string();
                    let range = text
                        .lines()
                        .find_map(|l| l.to_ascii_lowercase().strip_prefix("range: bytes=").map(str::to_string))
                        .and_then(|r| {
                            let (a, b) = r.trim().split_once('-')?;
                            Some((a.parse::<usize>().ok()?, b.parse::<usize>().map_or(file.len(), |b| (b + 1).min(file.len()))))
                        });
                    let current = generation.load(Ordering::SeqCst);
                    let (status, reply): (u16, Vec<u8>) = if path == "/origin" {
                        let to = format!("{base}/v{current}/ep.mp3");
                        (302, format!("HTTP/1.0 302 Found\r\nLocation: {to}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").into_bytes())
                    } else if path == format!("/v{current}/ep.mp3") {
                        let (from, to) = range.unwrap_or((0, file.len()));
                        let mut r = format!(
                            "HTTP/1.0 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {from}-{}/{}\r\nConnection: close\r\n\r\n",
                            to - from,
                            to - 1,
                            file.len()
                        )
                        .into_bytes();
                        r.extend_from_slice(&file[from..to]);
                        (206, r)
                    } else {
                        (403, b"HTTP/1.0 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec())
                    };
                    log.lock().unwrap().push((path, status));
                    let _ = conn.write_all(&reply);
                });
            }
        });
        Self { base, generation, log }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn expire(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    fn log(&self) -> Vec<(String, u16)> {
        self.log.lock().unwrap().clone()
    }
}

mod re_resolving {
    use super::*;
    use playr::dash::{HttpFetch, RangeFetch};

    fn file() -> Vec<u8> {
        (0..4000u32).map(|n| (n % 251) as u8).collect()
    }

    fn read(fetch: &HttpFetch, from: u64, to: u64) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        fetch.open(from, to)?.read_to_end(&mut out).map_err(|e| e.to_string())?;
        Ok(out)
    }

    #[test]
    fn an_expired_final_address_is_resolved_again_through_the_original_once() {
        let server = Rotating::start(file());
        let (fetch, _) = HttpFetch::connect(&server.url("/origin"), &[], 10).expect("connect");
        server.expire();
        let before = server.log().len();

        let first = read(&fetch, 1000, 1100);
        let second = read(&fetch, 2000, 2050);

        assert_eq!(first, Ok(file()[1000..1100].to_vec()));
        assert_eq!(second, Ok(file()[2000..2050].to_vec()));
        let after: Vec<(String, u16)> = server.log()[before..].to_vec();
        let expected = [("/v1/ep.mp3", 403), ("/origin", 302), ("/v2/ep.mp3", 206), ("/v2/ep.mp3", 206)];
        assert_eq!(after, expected.map(|(p, s)| (p.to_string(), s)));
    }

    #[test]
    fn without_a_redirect_a_refused_range_is_not_retried() {
        let server = Rotating::start(file());
        let direct = server.url("/v1/ep.mp3");
        let (fetch, _) = HttpFetch::connect(&direct, &[], 10).expect("connect");
        server.expire();
        let before = server.log().len();

        let result = read(&fetch, 1000, 1100);

        assert_eq!(result, Err(format!("cannot download {direct}: http status: 403")));
        assert_eq!(server.log()[before..].to_vec(), [("/v1/ep.mp3".to_string(), 403)]);
    }
}

mod feed_lookup {
    use super::*;

    const ODD_SHOW: &str = "Ł&ó? 100% #1 a.b_c~d-e";

    fn search_server(show_in_answer: &str) -> RangeServer {
        let answer = itunes_answer(json!([podcast_result(show_in_answer, Some("https://feeds.example/odd.xml"))]));
        RangeServer::start(vec![("/search", Mode::IgnoresRange, answer.into_bytes())])
    }

    #[test]
    fn asks_for_podcasts_with_the_escaped_name_and_returns_the_matching_feed() {
        let test = "feed_lookup::asks_for_podcasts_with_the_escaped_name_and_returns_the_matching_feed";
        if !is_child() {
            let server = search_server(ODD_SHOW);
            run_child(test, vec![(podcast::SEARCH_ENV, server.url("/search"))]);
            let requests = server.requests();
            // Ł = C5 81, ó = C3 B3 in UTF-8; '-', '.', '_' and '~' are unreserved.
            let term = "%C5%81%26%C3%B3%3F+100%25+%231+a.b_c~d-e";
            assert_eq!(paths(&requests), [format!("/search?media=podcast&entity=podcast&limit=10&term={term}")]);
            assert_eq!(requests[0].header("cookie"), None);
            return;
        }

        assert_eq!(podcast::find_feed(ODD_SHOW), Ok(Some("https://feeds.example/odd.xml".to_string())));
    }

    #[test]
    fn a_directory_without_the_show_gives_none() {
        let test = "feed_lookup::a_directory_without_the_show_gives_none";
        if !is_child() {
            let server = search_server("Some Other Show");
            run_child(test, vec![(podcast::SEARCH_ENV, server.url("/search"))]);
            assert_eq!(server.requests().len(), 1);
            return;
        }

        assert_eq!(podcast::find_feed(SHOW), Ok(None));
    }

    #[test]
    fn a_failing_directory_is_an_error() {
        let test = "feed_lookup::a_failing_directory_is_an_error";
        if !is_child() {
            let server = RangeServer::start(vec![]);
            run_child(test, vec![(podcast::SEARCH_ENV, server.url("/search")), ("PLAYR_TEST_BASE", server.url("/search"))]);
            return;
        }

        let base = std::env::var("PLAYR_TEST_BASE").unwrap();
        assert_eq!(podcast::find_feed(SHOW), Err(format!("cannot download {base}: http status: 404")));
    }
}

mod spotify_fetch {
    use super::*;

    const NEWEST: &str = "Kołysanka na dobranoc";
    const LONG_TITLE: &str = "[Łagodny początek] LIST DO ŚWIĘTEGO MIKOŁAJA 2 | Wyciszająca bajka";

    /// The embed page of a show or an episode: one entity of type "episode" whose subtitle is
    /// the show, and no track list.
    fn episode_page(title: &str, subtitle: &str, duration_ms: u64) -> Vec<u8> {
        let entity = json!({ "type": "episode", "uri": "spotify:episode:x", "title": title, "subtitle": subtitle, "duration": duration_ms });
        let data = json!({ "props": { "pageProps": { "state": { "data": { "entity": entity } } } } });
        format!("<html><body><script id=\"__NEXT_DATA__\" type=\"application/json\">{data}</script></body></html>").into_bytes()
    }

    const CDN: &str = "https://cdn.example/zosia";
    const SMOK: &str = "Bajka o smoku, część 2";

    fn rss() -> Vec<u8> {
        format!(
            r#"<rss xmlns:itunes="http://www.itunes.com/dtds/podcast-1.0.dtd"><channel><title>Zosia (feed name)</title>
<item><title>{LONG_TITLE}</title><enclosure url="{CDN}/a.mp3" type="audio/mpeg"/><itunes:duration>00:14:22</itunes:duration></item>
<item><title>{SMOK}</title><enclosure url="{CDN}/b.mp3" type="audio/mpeg"/><itunes:duration>65</itunes:duration></item>
<item><title>{NEWEST}</title><enclosure url="{CDN}/c.mp3"/></item>
</channel></rss>"#
        )
        .into_bytes()
    }

    fn feed_list(start: usize) -> Playlist {
        let e = |title: &str, file: &str, duration| Entry {
            title: title.into(),
            subtitle: SHOW.into(),
            duration,
            audio: Some(format!("{CDN}/{file}")),
        };
        let entries = vec![e(LONG_TITLE, "a.mp3", Some(862.0)), e(SMOK, "b.mp3", Some(65.0)), e(NEWEST, "c.mp3", None)];
        Playlist { name: SHOW.into(), entries, start }
    }

    fn embed_only(name: &str, title: &str, subtitle: &str, secs: f64) -> Playlist {
        let entry = Entry { title: title.into(), subtitle: subtitle.into(), duration: Some(secs), audio: None };
        Playlist { name: name.into(), entries: vec![entry], start: 0 }
    }

    /// The feed server and the server for the embed pages and the directory, whose answer
    /// names the feed server's `/near-miss.xml` first and its `/feed.xml` second.
    struct Site {
        feed: RangeServer,
        spotify: RangeServer,
    }

    impl Site {
        fn start(embed: Vec<(String, Vec<u8>)>, search: Option<Vec<u8>>, feed: Option<Vec<u8>>) -> Self {
            let feed_files = feed.map(|f| ("/feed.xml", Mode::IgnoresRange, f)).into_iter().collect();
            let feed = RangeServer::start(feed_files);
            let answer = search.unwrap_or_else(|| {
                itunes_answer(json!([
                    podcast_result("Zosia Cebulka", Some(feed.url("/near-miss.xml").as_str())),
                    podcast_result("ZOSIA CEBULKA – Bajki do snu i zabawy", Some(feed.url("/feed.xml").as_str())),
                ]))
                .into_bytes()
            });
            let mut files: Vec<(&str, Mode, Vec<u8>)> = embed.iter().map(|(p, b)| (p.as_str(), Mode::IgnoresRange, b.clone())).collect();
            files.push(("/search", Mode::IgnoresRange, answer));
            Self { feed, spotify: RangeServer::start(files) }
        }

        fn env(&self) -> Vec<(&'static str, String)> {
            vec![(spotify::EMBED_ENV, self.spotify.url("/embed")), (podcast::SEARCH_ENV, self.spotify.url("/search"))]
        }

        fn searches(&self) -> usize {
            self.spotify.requests().iter().filter(|r| r.path.starts_with("/search")).count()
        }
    }

    fn show_link() -> String {
        format!("https://open.spotify.com/show/{SHOW_ID}")
    }

    fn episode_link() -> String {
        format!("https://open.spotify.com/episode/{EPISODE_ID}")
    }

    fn show_page() -> (String, Vec<u8>) {
        (format!("/embed/show/{SHOW_ID}"), episode_page(NEWEST, SHOW, 300_000))
    }

    fn episode_page_at(title: &str, subtitle: &str) -> (String, Vec<u8>) {
        (format!("/embed/episode/{EPISODE_ID}"), episode_page(title, subtitle, 65_000))
    }

    #[test]
    fn a_show_link_lists_every_feed_episode_starting_with_the_first() {
        let test = "spotify_fetch::a_show_link_lists_every_feed_episode_starting_with_the_first";
        if !is_child() {
            let site = Site::start(vec![show_page()], None, Some(rss()));
            run_child(test, site.env());
            let term = "Zosia+Cebulka+-+bajki+do+snu+i+zabawy";
            let expected = [format!("/embed/show/{SHOW_ID}"), format!("/search?media=podcast&entity=podcast&limit=10&term={term}")];
            assert_eq!(paths(&site.spotify.requests()), expected);
            assert_eq!(paths(&site.feed.requests()), ["/feed.xml"]);
            return;
        }

        assert_eq!(spotify::fetch(&show_link(), None), Ok(feed_list(0)));
    }

    #[test]
    fn an_episode_link_starts_at_that_episode_matched_by_normalized_title() {
        let test = "spotify_fetch::an_episode_link_starts_at_that_episode_matched_by_normalized_title";
        if !is_child() {
            let site = Site::start(vec![episode_page_at("bajka o SMOKU – część 2!", SHOW)], None, Some(rss()));
            run_child(test, site.env());
            assert_eq!(paths(&site.feed.requests()), ["/feed.xml"]);
            return;
        }

        assert_eq!(spotify::fetch(&episode_link(), None), Ok(feed_list(1)));
    }

    #[test]
    fn an_episode_missing_from_the_feed_keeps_the_embed_list() {
        let test = "spotify_fetch::an_episode_missing_from_the_feed_keeps_the_embed_list";
        if !is_child() {
            let site = Site::start(vec![episode_page_at("Odcinek spoza kanału", SHOW)], None, Some(rss()));
            run_child(test, site.env());
            assert_eq!(paths(&site.feed.requests()), ["/feed.xml"]);
            return;
        }

        let expected = embed_only("Odcinek spoza kanału", "Odcinek spoza kanału", SHOW, 65.0);
        assert_eq!(spotify::fetch(&episode_link(), None), Ok(expected));
    }

    #[test]
    fn a_show_whose_feed_cannot_be_used_falls_back_to_the_embed_entry_named_after_the_show() {
        let test = "spotify_fetch::a_show_whose_feed_cannot_be_used_falls_back_to_the_embed_entry_named_after_the_show";
        if !is_child() {
            let near_miss_only = itunes_answer(json!([podcast_result("Zosia Cebulka", Some("https://feeds.example/z.xml"))]));
            let video_only = r#"<rss><channel><item><title>V</title><enclosure url="https://cdn.example/v.mp4" type="video/mp4"/></item></channel></rss>"#;
            let cases: [(&str, Option<Vec<u8>>, Option<Vec<u8>>, &str); 5] = [
                ("no exact name match", Some(near_miss_only.into_bytes()), Some(rss()), "/search"),
                ("directory answers 404", None, Some(rss()), "/no-directory"),
                ("directory answers HTML", Some(b"<html>busy</html>".to_vec()), Some(rss()), "/search"),
                ("feed answers 404", None, None, "/search"),
                ("feed without sound", None, Some(video_only.as_bytes().to_vec()), "/search"),
            ];
            for (case, search, feed, search_path) in cases {
                let site = Site::start(vec![show_page()], search, feed);
                let env = vec![(spotify::EMBED_ENV, site.spotify.url("/embed")), (podcast::SEARCH_ENV, site.spotify.url(search_path))];
                eprintln!("case: {case}");
                run_child(test, env);
            }
            return;
        }

        assert_eq!(spotify::fetch(&show_link(), None), Ok(embed_only(SHOW, NEWEST, SHOW, 300.0)));
    }

    #[test]
    fn playlist_album_and_track_links_never_ask_the_podcast_directory() {
        let test = "spotify_fetch::playlist_album_and_track_links_never_ask_the_podcast_directory";
        if !is_child() {
            let page = |kind: &str, name: &str| {
                let tracks = json!([{ "title": format!("{name} song"), "subtitle": SHOW, "duration": 61_000 }]);
                let data = json!({ "props": { "pageProps": { "state": { "data": { "entity": { "name": name, "trackList": tracks } } } } } });
                (format!("/embed/{kind}/{SHOW_ID}"), format!("<script id=\"__NEXT_DATA__\" type=\"application/json\">{data}</script>").into_bytes())
            };
            let site = Site::start(vec![page("playlist", "Lista"), page("album", "Album"), page("track", "Utwór")], None, Some(rss()));
            run_child(test, site.env());
            assert_eq!(site.searches(), 0);
            assert_eq!(site.feed.requests().len(), 0);
            return;
        }

        let mut bad = Vec::new();
        for (kind, name) in [("playlist", "Lista"), ("album", "Album"), ("track", "Utwór")] {
            let actual = spotify::fetch(&format!("https://open.spotify.com/{kind}/{SHOW_ID}"), None);
            let expected = embed_only(name, &format!("{name} song"), SHOW, 61.0);
            expect!(bad, actual == Ok(expected.clone()), "{kind}: {actual:?}\n    expected {expected:?}");
        }
        finish(bad);
    }

    #[test]
    fn an_episode_without_a_show_name_does_not_ask_the_directory() {
        let test = "spotify_fetch::an_episode_without_a_show_name_does_not_ask_the_directory";
        if !is_child() {
            let site = Site::start(vec![episode_page_at(SMOK, "")], None, Some(rss()));
            run_child(test, site.env());
            assert_eq!(site.searches(), 0);
            return;
        }

        assert_eq!(spotify::fetch(&episode_link(), None), Ok(embed_only(SMOK, SMOK, "", 65.0)));
    }

    #[test]
    fn a_show_without_a_show_name_keeps_the_embed_name_and_does_not_ask_the_directory() {
        let test = "spotify_fetch::a_show_without_a_show_name_keeps_the_embed_name_and_does_not_ask_the_directory";
        if !is_child() {
            let entity = json!({ "type": "episode", "name": "Nazwa z embedu", "title": NEWEST, "subtitle": "", "duration": 300_000 });
            let data = json!({ "props": { "pageProps": { "state": { "data": { "entity": entity } } } } });
            let page = format!("<script id=\"__NEXT_DATA__\" type=\"application/json\">{data}</script>").into_bytes();
            let site = Site::start(vec![(format!("/embed/show/{SHOW_ID}"), page)], None, Some(rss()));
            run_child(test, site.env());
            assert_eq!(site.searches(), 0);
            assert_eq!(site.feed.requests().len(), 0);
            return;
        }

        assert_eq!(spotify::fetch(&show_link(), None), Ok(embed_only("Nazwa z embedu", NEWEST, "", 300.0)));
    }

    #[test]
    fn spotify_cookies_go_to_the_embed_page_only_never_to_the_directory_or_the_feed() {
        let test = "spotify_fetch::spotify_cookies_go_to_the_embed_page_only_never_to_the_directory_or_the_feed";
        if !is_child() {
            let site = Site::start(vec![show_page()], None, Some(rss()));
            run_child(test, site.env());
            let spotify_requests = site.spotify.requests();
            let cookie_of = |prefix: &str| spotify_requests.iter().find(|r| r.path.starts_with(prefix)).map(|r| r.header("cookie"));
            assert_eq!(cookie_of("/embed/"), Some(Some("sp_dc=synthetic-not-a-secret")));
            assert_eq!(cookie_of("/search"), Some(None));
            let feed_requests = site.feed.requests();
            assert_eq!(feed_requests.len(), 1);
            assert_eq!(feed_requests[0].header("cookie"), None);
            return;
        }
        // Every test server is on 127.0.0.1, so a cookie for that host would match them all.
        let jar = playr::cookies::parse("127.0.0.1\tFALSE\t/\tFALSE\t0\tsp_dc\tsynthetic-not-a-secret").expect("jar");

        assert_eq!(spotify::fetch(&show_link(), Some(&jar)), Ok(feed_list(0)));
    }
}

#[test]
#[ignore = "needs the internet, Spotify and Apple's podcast directory"]
fn the_reported_show_and_episode_links_play_from_the_podcast_feed() {
    let show = spotify::fetch(&format!("https://open.spotify.com/show/{SHOW_ID}"), None).expect("show");
    let episode = spotify::fetch(&format!("https://open.spotify.com/episode/{EPISODE_ID}"), None).expect("episode");

    assert_eq!((show.name.as_str(), show.start), (SHOW, 0));
    assert!(show.entries.len() > 1, "{show:?}");
    assert!(show.entries.iter().all(|e| e.subtitle == SHOW && e.audio.as_deref().is_some_and(|a| a.starts_with("http"))), "{show:?}");
    assert_eq!(episode.entries, show.entries);
    assert!(episode.start < episode.entries.len(), "{episode:?}");
}
