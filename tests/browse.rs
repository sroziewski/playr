//! `playr::browse`: reading channel addresses, the yt-dlp arguments and answers of the YouTube
//! browser, its labels, its thumbnails and the recent channels, without the network.
//!
//! yt-dlp is a copy of this test binary (see `act_as_fake_ytdlp`) that logs its exact
//! arguments and answers from a canned script; tests that need it run in a fresh copy of this
//! binary, because `PLAYR_YTDLP` belongs to the whole process. Thumbnails come from a local
//! HTTP server serving `tests/fixtures/solid_50x30.jpg`.

mod common;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;

use common::range_http::{Mode, RangeServer};
use common::{finish, fixture_bytes};
use playr::browse::{
    Arrival, Back, Channel, Failed, Item, Listing, MAX_ITEMS, Nav, PAGE, Query, RECENT_LIMIT, Recent, View, age_label, channel_url, decode_jpeg, fetch, fetch_thumbnail,
    parse_listing, query_for, recent_path, thumbnail_url, views_label, ytdlp_args,
};
use serde_json::{Value, json};

const CHILD: &str = "PLAYR_TEST_CHILD";
const DIR: &str = "PLAYR_TEST_DIR";

fn channel(name: &str, url: &str) -> Channel {
    Channel { name: name.into(), url: url.into() }
}

mod channel_addresses {
    use super::*;

    #[test]
    fn channel_url_gives_the_bare_www_address_of_every_youtube_channel_form() {
        let cases = [
            ("handle", "https://www.youtube.com/@LinusTechTips", "https://www.youtube.com/@LinusTechTips"),
            ("handle with a tab, over http, without www", "http://youtube.com/@LinusTechTips/videos", "https://www.youtube.com/@LinusTechTips"),
            ("mobile host, channel id, tab, query and fragment", "https://m.youtube.com/channel/UCXuqSBlHAE6Xw-yeJA0Tunw/featured?view=0#top", "https://www.youtube.com/channel/UCXuqSBlHAE6Xw-yeJA0Tunw"),
            ("host in mixed case, custom name with a tab", "https://WWW.YouTube.Com/c/Kurzgesagt/playlists", "https://www.youtube.com/c/Kurzgesagt"),
            ("port, legacy user name", "https://www.youtube.com:443/user/PewDiePie", "https://www.youtube.com/user/PewDiePie"),
            ("userinfo and port", "https://someone:secret@youtube.com:8443/@Veritasium/shorts", "https://www.youtube.com/@Veritasium"),
            ("handle with a query only", "https://www.youtube.com/@3blue1brown?si=Ab12", "https://www.youtube.com/@3blue1brown"),
            ("handle with a fragment only", "https://www.youtube.com/@numberphile#about", "https://www.youtube.com/@numberphile"),
            ("channel id with a query", "http://m.youtube.com/channel/UC_x5XG1OV2P6uZZ5FSM9Ttw?sub_confirmation=1", "https://www.youtube.com/channel/UC_x5XG1OV2P6uZZ5FSM9Ttw"),
            ("bare handle", "@MrBeast", "https://www.youtube.com/@MrBeast"),
            ("bare handle with every unreserved mark", "@my.handle-1_~", "https://www.youtube.com/@my.handle-1_~"),
            ("bare handle with diacritics, by UTF-8 bytes in upper-case hex", "@Zażółć", "https://www.youtube.com/@Za%C5%BC%C3%B3%C5%82%C4%87"),
            ("bare handle with reserved ASCII", "@a+b%c", "https://www.youtube.com/@a%2Bb%25c"),
        ];
        let mut bad = Vec::new();
        for (case, input, expected) in cases {
            let actual = channel_url(input);
            expect!(bad, actual.as_deref() == Some(expected), "{case}: channel_url({input:?}) = {actual:?}, expected {expected:?}");
        }
        finish(bad);
    }

    #[test]
    fn channel_url_reads_addresses_typed_without_a_scheme_or_with_one_in_capitals() {
        let cases = [
            ("no scheme, no www, handle", "youtube.com/@name", "https://www.youtube.com/@name"),
            ("no scheme, www, channel id with a tab", "www.youtube.com/channel/UCXuqSBlHAE6Xw-yeJA0Tunw/videos", "https://www.youtube.com/channel/UCXuqSBlHAE6Xw-yeJA0Tunw"),
            ("no scheme, mobile host, custom name", "m.youtube.com/c/Kurzgesagt", "https://www.youtube.com/c/Kurzgesagt"),
            ("no scheme, host in mixed case, port, query", "YouTube.com:443/user/PewDiePie?si=Zx9", "https://www.youtube.com/user/PewDiePie"),
            ("no scheme, userinfo", "someone@www.youtube.com/@numberphile/about", "https://www.youtube.com/@numberphile"),
            ("no scheme, another address in the query", "youtube.com/@Veritasium?next=https://example.com/a", "https://www.youtube.com/@Veritasium"),
            ("scheme and host in capitals, path case kept", "HTTPS://WWW.YOUTUBE.COM/@Veritasium", "https://www.youtube.com/@Veritasium"),
            ("http in mixed case", "Http://youtube.com/user/PewDiePie/videos", "https://www.youtube.com/user/PewDiePie"),
            ("https in mixed case", "hTTpS://m.youtube.com/@3blue1brown#top", "https://www.youtube.com/@3blue1brown"),
            ("whitespace only after the name", "youtube.com/@mkbhd/videos latest", "https://www.youtube.com/@mkbhd"),
        ];
        let mut bad = Vec::new();
        for (case, input, expected) in cases {
            let actual = channel_url(input);
            expect!(bad, actual.as_deref() == Some(expected), "{case}: channel_url({input:?}) = {actual:?}, expected {expected:?}");
        }
        finish(bad);
    }

    #[test]
    fn channel_url_without_a_scheme_or_with_another_one_is_none_for_anything_but_a_channel() {
        let cases = [
            ("host only", "youtube.com"),
            ("host and a slash", "www.youtube.com/"),
            ("watch page", "youtube.com/watch?v=aB3_-x9Zq0Y"),
            ("short", "m.youtube.com/shorts/aB3_-x9Zq0Y"),
            ("short link host", "youtu.be/aB3_-x9Zq0Y"),
            ("YouTube Music", "music.youtube.com/channel/UCXuqSBlHAE6Xw-yeJA0Tunw"),
            ("look-alike suffix", "www.youtube.com.evil.example/@LinusTechTips"),
            ("youtube.com in the path of another host", "evil.example/youtube.com/@LinusTechTips"),
            ("youtube.com as userinfo of another host", "youtube.com@evil.example/@LinusTechTips"),
            ("channel without an id", "youtube.com/channel/"),
            ("@ with nothing after it", "youtube.com/@"),
            ("ftp", "ftp://youtube.com/@LinusTechTips"),
            ("ftp in capitals", "FTP://www.youtube.com/@LinusTechTips"),
            ("app scheme", "youtube://www.youtube.com/@LinusTechTips"),
            ("http with one slash", "http:/youtube.com/@LinusTechTips"),
        ];
        let mut bad = Vec::new();
        for (case, input) in cases {
            let actual = channel_url(input);
            expect!(bad, actual.is_none(), "{case}: channel_url({input:?}) = {actual:?}, expected None");
        }
        finish(bad);
    }

    #[test]
    fn channel_url_is_none_when_the_channel_name_holds_whitespace() {
        let cases = [
            ("handle and words, no scheme", "youtube.com/@mkbhd latest videos"),
            ("custom name with a space", "https://www.youtube.com/c/some name"),
            ("channel id with a space, then a tab", "https://www.youtube.com/channel/UC abc/videos"),
            ("user name with a tab, no scheme", "www.youtube.com/user/Pew\tDiePie"),
            ("handle with a no-break space, capital scheme", "HTTPS://m.youtube.com/@two\u{a0}words"),
        ];
        let mut bad = Vec::new();
        for (case, input) in cases {
            let actual = channel_url(input);
            expect!(bad, actual.is_none(), "{case}: channel_url({input:?}) = {actual:?}, expected None");
        }
        finish(bad);
    }

    #[test]
    fn channel_url_is_none_for_anything_but_a_channel_address() {
        let cases = [
            ("watch page", "https://www.youtube.com/watch?v=aB3_-x9Zq0Y"),
            ("short", "https://www.youtube.com/shorts/aB3_-x9Zq0Y"),
            ("playlist", "https://www.youtube.com/playlist?list=PLrAXtmErZgOeiKm4sgNOknGvNjby9efdf"),
            ("short link host", "https://youtu.be/aB3_-x9Zq0Y"),
            ("YouTube Music", "https://music.youtube.com/channel/UCXuqSBlHAE6Xw-yeJA0Tunw"),
            ("look-alike suffix", "https://www.youtube.com.evil.example/@LinusTechTips"),
            ("look-alike prefix", "https://notyoutube.com/@LinusTechTips"),
            ("youtube.com as userinfo of another host", "https://youtube.com@evil.example/@LinusTechTips"),
            ("other scheme", "ftp://www.youtube.com/@LinusTechTips"),
            ("@ with nothing after it", "https://www.youtube.com/@"),
            ("@ with nothing after it, then a tab", "https://www.youtube.com/@/videos"),
            ("channel without an id", "https://www.youtube.com/channel"),
            ("channel without an id, trailing slash", "https://www.youtube.com/channel/"),
            ("custom name missing", "https://www.youtube.com/c/"),
            ("user name missing", "https://www.youtube.com/user"),
            ("home page", "https://www.youtube.com/"),
            ("host only", "https://www.youtube.com"),
            ("plain text", "lofi hip hop radio"),
            ("empty", ""),
            ("lone @", "@"),
            ("handle with a space", "@two words"),
            ("handle with a slash", "@abc/videos"),
            ("handle with a question mark", "@abc?x"),
            ("handle with a hash", "@abc#x"),
            ("handle with an ampersand", "@abc&x"),
            ("handle with a second @", "@abc@x"),
        ];
        let mut bad = Vec::new();
        for (case, input) in cases {
            let actual = channel_url(input);
            expect!(bad, actual.is_none(), "{case}: channel_url({input:?}) = {actual:?}, expected None");
        }
        finish(bad);
    }

    #[test]
    fn query_for_is_none_for_blank_a_channel_for_a_channel_address_and_a_trimmed_search_otherwise() {
        let channel_q = |url: &str| Some(Query::Channel { url: url.into() });
        let search_q = |text: &str| Some(Query::Search { text: text.into() });
        let cases = [
            ("empty", "", None),
            ("spaces", "   ", None),
            ("tabs and newlines", "\t\r\n", None),
            ("channel address with a tab", "https://m.youtube.com/@Veritasium/videos", channel_q("https://www.youtube.com/@Veritasium")),
            ("bare handle in spaces", "  @MrBeast  ", channel_q("https://www.youtube.com/@MrBeast")),
            ("channel address without a scheme", " www.youtube.com/@numberphile/videos ", channel_q("https://www.youtube.com/@numberphile")),
            ("a host without a channel is searched for", "youtube.com", search_q("youtube.com")),
            ("a channel address followed by words is searched for", " youtube.com/@mkbhd latest videos ", search_q("youtube.com/@mkbhd latest videos")),
            ("a custom name with a space is searched for", "https://www.youtube.com/c/some name", search_q("https://www.youtube.com/c/some name")),
            ("words in spaces", "  lofi hip  hop \t", search_q("lofi hip  hop")),
            ("a video address is searched for", "https://www.youtube.com/watch?v=aB3_-x9Zq0Y", search_q("https://www.youtube.com/watch?v=aB3_-x9Zq0Y")),
            ("a handle with a space is searched for", "@two words", search_q("@two words")),
        ];
        let mut bad = Vec::new();
        for (case, input, expected) in cases {
            let actual = query_for(input);
            expect!(bad, actual == expected, "{case}: query_for({input:?}) = {actual:?}, expected {expected:?}");
        }
        finish(bad);
    }
}

fn args_with(items: &str, target: &str) -> Vec<String> {
    [
        "-J",
        "--flat-playlist",
        "--playlist-items",
        items,
        "--extractor-args",
        "youtubetab:approximate_date",
        "--extractor-args",
        "youtube:lang=en",
        "--no-warnings",
        "--no-progress",
        "--",
        target,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

#[test]
fn ytdlp_args_ask_for_the_next_page_of_the_query() {
    let url = "https://www.youtube.com/@Veritasium";
    let cases = [
        ("channel, first page", Query::Channel { url: url.into() }, 0, args_with("1:10", "https://www.youtube.com/@Veritasium/videos")),
        ("channel, third page", Query::Channel { url: url.into() }, 20, args_with("21:30", "https://www.youtube.com/@Veritasium/videos")),
        (
            "search in a channel, second page, text encoded",
            Query::InChannel { url: url.into(), text: "  café & tea 50% ".into() },
            10,
            args_with("11:20", "https://www.youtube.com/@Veritasium/search?query=caf%C3%A9%20%26%20tea%2050%25"),
        ),
        ("search, first page", Query::Search { text: " lofi hip hop  ".into() }, 0, args_with("1:10", "ytsearch10:lofi hip hop")),
        ("search, fourth page", Query::Search { text: "rust & café".into() }, 30, args_with("31:40", "ytsearch40:rust & café")),
    ];
    let mut bad = Vec::new();
    for (case, query, have, expected) in cases {
        let actual = ytdlp_args(&query, have);
        expect!(bad, actual == expected, "{case}: {actual:?}\n    expected {expected:?}");
    }
    assert_eq!(PAGE, 10);
    finish(bad);
}

mod listings {
    use super::*;

    fn owner() -> Channel {
        channel("Owner Channel", "https://www.youtube.com/@OwnerHandle")
    }

    fn item(id: &str, title: &str, channel: Option<Channel>) -> Item {
        Item { id: id.into(), title: title.into(), channel, duration: None, views: None, uploaded: None, live: false, upcoming: false }
    }

    fn channel_tab() -> Value {
        json!({
            "id": "UCowner000000000000000", "title": "Owner Channel - Videos",
            "channel": "Owner Channel", "channel_id": "UCowner000000000000000",
            "uploader_url": "https://www.youtube.com/@OwnerHandle/videos",
            "channel_url": "https://www.youtube.com/channel/UCowner000000000000000",
            "entries": [
                { "ie_key": "Youtube", "id": "vid0aaaaaa1", "title": "First video", "duration": 125.5, "view_count": 1234,
                  "timestamp": 1_700_000_000, "live_status": "was_live" },
                { "ie_key": "YoutubeTab", "id": "UCnotavideo", "title": "A channel" },
                { "id": "vid2cccccc3", "duration": 0, "release_timestamp": 1_800_000_000, "live_status": "is_upcoming" },
                { "ie_key": "Youtube", "id": "tooShort10", "title": "Ten-character id" },
                { "ie_key": "Youtube", "id": "twelvechars1", "title": "Twelve-character id" },
                { "ie_key": "Youtube", "id": "vid5ffffff6", "title": "Live now", "duration": -5, "view_count": 0,
                  "timestamp": 1_700_100_000, "release_timestamp": 1_600_000_000, "live_status": "is_live" },
                { "id": "vid6gggggg7", "title": "Guest video", "channel": "Guest Channel",
                  "channel_url": "https://www.youtube.com/channel/UCguest000/videos", "view_count": 77 },
                { "id": "vid7hhhhhh8", "title": "Uploader only", "uploader": "Uploader Name",
                  "uploader_url": "https://m.youtube.com/@UpHandle/featured", "channel_url": "https://www.youtube.com/channel/UCupl0000" },
                { "id": "vid8iiiiii9", "title": "Name but no address", "channel": "Named Channel", "uploader": "Other Uploader" },
                { "id": "vid9jjjjjj0", "title": "Both names", "channel": "Channel Name", "uploader": "Uploader Other",
                  "uploader_url": "https://www.youtube.com/@BothHandle" },
            ],
        })
    }

    #[test]
    fn a_channel_tab_keeps_its_videos_in_order_with_their_fields_and_the_owner_for_those_without_a_channel() {
        let parsed = parse_listing(&channel_tab().to_string());

        let expected = Listing {
            channel: Some(owner()),
            items: vec![
                Item { duration: Some(125.5), views: Some(1234), uploaded: Some(1_700_000_000), ..item("vid0aaaaaa1", "First video", Some(owner())) },
                Item { uploaded: Some(1_800_000_000), upcoming: true, ..item("vid2cccccc3", "vid2cccccc3", Some(owner())) },
                Item { views: Some(0), uploaded: Some(1_700_100_000), live: true, ..item("vid5ffffff6", "Live now", Some(owner())) },
                Item {
                    views: Some(77),
                    ..item("vid6gggggg7", "Guest video", Some(channel("Guest Channel", "https://www.youtube.com/channel/UCguest000")))
                },
                item("vid7hhhhhh8", "Uploader only", Some(channel("Uploader Name", "https://www.youtube.com/@UpHandle"))),
                item("vid8iiiiii9", "Name but no address", Some(owner())),
                item("vid9jjjjjj0", "Both names", Some(channel("Channel Name", "https://www.youtube.com/@BothHandle"))),
            ],
            more: true,
        };
        assert_eq!(parsed, Ok(expected));
    }

    #[test]
    fn a_search_leaves_out_channels_and_playlists_and_has_no_owner() {
        let answer = json!({
            "id": "lofi", "title": "lofi", "channel": "Not An Owner", "uploader_url": "https://www.youtube.com/@NotAnOwner",
            "entries": [
                { "ie_key": "Youtube", "id": "srch0000001", "title": "Lofi mix", "channel": "Lofi Girl",
                  "uploader_url": "https://www.youtube.com/@LofiGirl", "duration": 3600.0, "view_count": 5_000_000, "timestamp": 1_690_000_000 },
                { "ie_key": "YoutubeTab", "id": "UCLofiGirl1", "title": "Lofi Girl", "url": "https://www.youtube.com/@LofiGirl" },
                { "ie_key": "YoutubePlaylist", "id": "PLlofi00001", "title": "Lofi playlist" },
                { "ie_key": "Youtube", "id": "srch0000004", "title": "   ", "channel": "Some Channel",
                  "uploader_url": "https://www.youtube.com/watch?v=srch0000004" },
                { "id": "srch0000005", "title": "No channel at all", "live_status": "not_live" },
            ],
        });

        let parsed = parse_listing(&answer.to_string());

        let expected = Listing {
            channel: None,
            items: vec![
                Item {
                    duration: Some(3600.0),
                    views: Some(5_000_000),
                    uploaded: Some(1_690_000_000),
                    ..item("srch0000001", "Lofi mix", Some(channel("Lofi Girl", "https://www.youtube.com/@LofiGirl")))
                },
                item("srch0000004", "srch0000004", None),
                item("srch0000005", "No channel at all", None),
            ],
            more: false,
        };
        assert_eq!(parsed, Ok(expected));
    }

    #[test]
    fn a_videos_channel_address_falls_back_to_channel_url_when_uploader_url_is_not_a_channel() {
        let guest = |url: &str| Some(channel("Guest Name", url));
        let cases = [
            (
                "uploader_url is a watch page",
                json!({ "uploader_url": "https://www.youtube.com/watch?v=aB3_-x9Zq0Y", "channel_url": "https://www.youtube.com/channel/UCfall0001" }),
                guest("https://www.youtube.com/channel/UCfall0001"),
            ),
            (
                "uploader_url is another site, channel_url has a tab",
                json!({ "uploader_url": "https://example.com/u", "channel_url": "https://m.youtube.com/@FallHandle/videos" }),
                guest("https://www.youtube.com/@FallHandle"),
            ),
            (
                "both are channels: uploader_url wins",
                json!({ "uploader_url": "https://www.youtube.com/@UpFirst", "channel_url": "https://www.youtube.com/channel/UCsecond01" }),
                guest("https://www.youtube.com/@UpFirst"),
            ),
            ("only channel_url", json!({ "channel_url": "https://www.youtube.com/c/OnlyChannelUrl" }), guest("https://www.youtube.com/c/OnlyChannelUrl")),
            ("uploader_url not a channel and no channel_url", json!({ "uploader_url": "https://www.youtube.com/watch?v=aB3_-x9Zq0Y" }), None),
            ("neither is a channel", json!({ "uploader_url": "https://example.com/u", "channel_url": "https://www.youtube.com/playlist?list=PL1" }), None),
        ];
        let mut bad = Vec::new();
        for (case, mut entry, expected) in cases {
            entry["id"] = json!("guestvideo1");
            entry["channel"] = json!("Guest Name");
            let actual = parse_listing(&json!({ "entries": [entry] }).to_string()).map(|l| l.items.into_iter().map(|i| i.channel).collect::<Vec<_>>());
            let wanted = Ok(vec![expected]);
            expect!(bad, actual == wanted, "{case}: {actual:?}, expected {wanted:?}");
        }
        finish(bad);
    }

    #[test]
    fn the_owner_is_the_top_level_channel_only_when_the_top_level_has_a_channel_id() {
        let video = json!({ "id": "abcdefghijk", "title": "Only video" });
        let cases = [
            (
                "channel id, uploader name and channel address",
                json!({ "channel_id": "UCtop00000", "uploader": "Top Uploader", "channel_url": "https://www.youtube.com/channel/UCtop00000/videos" }),
                Some(channel("Top Uploader", "https://www.youtube.com/channel/UCtop00000")),
            ),
            ("no channel id", json!({ "channel": "Top Name", "uploader_url": "https://www.youtube.com/@TopHandle" }), None),
            ("channel id without an address", json!({ "channel_id": "UCtop00000", "channel": "Top Name" }), None),
            ("channel id without a name", json!({ "channel_id": "UCtop00000", "uploader_url": "https://www.youtube.com/@TopHandle" }), None),
        ];
        let mut bad = Vec::new();
        for (case, mut top, expected) in cases {
            top["entries"] = json!([video.clone()]);
            let actual = parse_listing(&top.to_string()).map(|l| (l.channel, l.items.into_iter().map(|i| i.channel).collect::<Vec<_>>()));
            let wanted = Ok((expected.clone(), vec![expected]));
            expect!(bad, actual == wanted, "{case}: {actual:?}, expected {wanted:?}");
        }
        finish(bad);
    }

    #[test]
    fn more_is_set_when_yt_dlp_sent_a_full_page_of_entries_counting_the_ones_left_out() {
        let video = |n: usize| json!({ "id": format!("video{n:06}"), "title": format!("Video {n}") });
        let playlist = |n: usize| json!({ "ie_key": "YoutubePlaylist", "id": format!("PLlist{n:05}") });
        let cases: [(&str, Value, usize, bool); 6] = [
            ("no entries key", json!({}), 0, false),
            ("empty entries", json!({ "entries": [] }), 0, false),
            ("one entry", json!({ "entries": [video(1)] }), 1, false),
            ("nine entries", json!({ "entries": (1..=9).map(video).collect::<Vec<_>>() }), 9, false),
            ("ten entries, three of them playlists", json!({ "entries": (1..=7).map(video).chain((8..=10).map(playlist)).collect::<Vec<_>>() }), 7, true),
            ("eleven entries", json!({ "entries": (1..=11).map(video).collect::<Vec<_>>() }), 11, true),
        ];
        let mut bad = Vec::new();
        for (case, answer, items, more) in cases {
            let actual = parse_listing(&answer.to_string()).map(|l| (l.items.len(), l.more));
            expect!(bad, actual == Ok((items, more)), "{case}: {actual:?}, expected {:?}", (items, more));
        }
        finish(bad);
    }

    #[test]
    fn an_unreadable_answer_is_an_error_naming_yt_dlp() {
        let mut bad = Vec::new();
        for answer in ["", "not json", "{\"entries\": [", "ERROR: [youtube:tab] gone"] {
            let actual = parse_listing(answer);
            let ok = actual.as_ref().is_err_and(|e| e.starts_with("yt-dlp sent an unreadable answer (") && e.ends_with(')'));
            expect!(bad, ok, "{answer:?}: {actual:?}");
        }
        finish(bad);
    }

    #[test]
    fn a_video_has_its_watch_address_and_thumbnail() {
        let video = item("aB3_-x9Zq0Y", "A video", None);

        assert_eq!(video.url(), "https://www.youtube.com/watch?v=aB3_-x9Zq0Y");
        assert_eq!(video.thumbnail_url(), "https://i.ytimg.com/vi/aB3_-x9Zq0Y/mqdefault.jpg");
        assert_eq!(thumbnail_url("zz9_-Q8yX7w"), "https://i.ytimg.com/vi/zz9_-Q8yX7w/mqdefault.jpg");
    }
}

mod labels {
    use super::*;

    const DAY: i64 = 86_400;

    #[test]
    fn age_label_counts_whole_utc_days_in_days_weeks_months_and_years() {
        let today = 20_418;
        let now = today * DAY + 13 * 3600;
        let cases = [
            (0, "today"),
            (1, "yesterday"),
            (2, "2 days ago"),
            (6, "6 days ago"),
            (7, "1 week ago"),
            (13, "1 week ago"),
            (14, "2 weeks ago"),
            (29, "4 weeks ago"),
            (30, "1 month ago"),
            (59, "1 month ago"),
            (60, "2 months ago"),
            (359, "11 months ago"),
            (360, "11 months ago"),
            (364, "11 months ago"),
            (365, "1 year ago"),
            (729, "1 year ago"),
            (730, "2 years ago"),
            (3653, "10 years ago"),
            (-3, "today"),
        ];
        let mut bad = Vec::new();
        for (days, expected) in cases {
            let uploaded = (today - days) * DAY + 20 * 3600;
            let actual = age_label(now, uploaded);
            expect!(bad, actual == expected, "{days} days: {actual:?}, expected {expected:?}");
        }
        finish(bad);
    }

    #[test]
    fn age_label_goes_by_the_calendar_day_not_by_24_hours() {
        let cases = [
            ("two minutes, across midnight", 20_418 * DAY + 60, 20_418 * DAY - 60, "yesterday"),
            ("23 hours, the same day", 20_418 * DAY + 23 * 3600, 20_418 * DAY + 60, "today"),
            ("47 hours, two midnights", 20_419 * DAY + 23 * 3600, 20_418 * DAY - 3600, "2 days ago"),
            ("before 1970, across midnight", 3600, -1, "yesterday"),
            ("both before 1970", -DAY + 60, -2 * DAY + 60, "yesterday"),
        ];
        let mut bad = Vec::new();
        for (case, now, uploaded, expected) in cases {
            let actual = age_label(now, uploaded);
            expect!(bad, actual == expected, "{case}: age_label({now}, {uploaded}) = {actual:?}, expected {expected:?}");
        }
        finish(bad);
    }

    #[test]
    fn views_label_shortens_with_one_truncated_decimal_below_ten() {
        let cases: [(u64, &str); 27] = [
            (0, "0 views"),
            (1, "1 view"),
            (2, "2 views"),
            (999, "999 views"),
            (1_000, "1K views"),
            (1_099, "1K views"),
            (1_250, "1.2K views"),
            (1_999, "1.9K views"),
            (2_300, "2.3K views"),
            (4_100, "4.1K views"),
            (9_999, "9.9K views"),
            (10_000, "10K views"),
            (12_345, "12K views"),
            (999_999, "999K views"),
            (1_000_000, "1M views"),
            (1_050_000, "1M views"),
            (1_100_000, "1.1M views"),
            (1_999_999, "1.9M views"),
            (9_999_999, "9.9M views"),
            (12_500_000, "12M views"),
            (999_999_999, "999M views"),
            (1_000_000_000, "1B views"),
            (2_500_000_000, "2.5B views"),
            (9_990_000_000, "9.9B views"),
            (25_000_000_000, "25B views"),
            (999_000_000_000, "999B views"),
            (1_234_000_000_000, "1234B views"),
        ];
        let mut bad = Vec::new();
        for (views, expected) in cases {
            let actual = views_label(views);
            expect!(bad, actual == expected, "views_label({views}) = {actual:?}, expected {expected:?}");
        }
        finish(bad);
    }
}

mod navigation {
    use super::*;

    fn search(text: &str) -> Query {
        Query::Search { text: text.into() }
    }

    fn chan(handle: &str) -> Query {
        Query::Channel { url: format!("https://www.youtube.com/@{handle}") }
    }

    fn video(id: &str) -> Item {
        Item { id: id.into(), title: format!("Video {id}"), channel: None, duration: None, views: None, uploaded: None, live: false, upcoming: false }
    }

    fn listing(ids: &[&str], more: bool) -> Listing {
        Listing { channel: None, items: ids.iter().map(|id| video(id)).collect(), more }
    }

    fn view(query: Query, ids: &[&str], more: bool) -> View {
        View { query, listing: listing(ids, more) }
    }

    fn results() -> View {
        view(search("lofi"), &["res1", "res2"], true)
    }

    fn failed(query: Query, message: &str) -> Failed {
        Failed { query, append: false, back: Back::Clear, message: message.into() }
    }

    /// A Nav showing `results()` after a search that arrived.
    fn showing_results() -> Nav {
        let mut nav = Nav::default();
        let r = nav.ask(search("lofi"), false, Back::Clear);
        assert_eq!(nav.arrived(r.id, Ok(listing(&["res1", "res2"], true))), Arrival::New);
        nav
    }

    #[test]
    fn ask_clears_the_error_gives_increasing_ids_counts_have_only_when_appending_and_replaces_the_wait() {
        let mut nav = Nav::default();
        nav.view = Some(view(search("lofi"), &["res1", "res2", "res3"], true));
        nav.error = Some(failed(chan("Earlier"), "earlier failure"));

        let first = nav.ask(chan("Veritasium"), false, Back::FromResults);
        let error_after_first = nav.error.clone();
        let second = nav.ask(search("lofi"), true, Back::Keep);

        assert_eq!(error_after_first, None);
        assert_eq!((first.query.clone(), first.have, first.append), (chan("Veritasium"), 0, false));
        assert_eq!((second.query.clone(), second.have, second.append), (search("lofi"), 3, true));
        assert!(second.id > first.id, "ids {} then {}", first.id, second.id);
        assert_eq!(nav.pending(), Some(&second));
        assert_eq!(nav.arrived(first.id, Ok(listing(&["chan1"], false))), Arrival::Stale);
    }

    #[test]
    fn ask_to_append_with_nothing_shown_has_nothing() {
        let mut nav = Nav::default();

        let request = nav.ask(search("lofi"), true, Back::Keep);

        assert_eq!((request.have, request.append), (0, true));
    }

    #[test]
    fn an_answer_that_is_not_the_pending_one_is_stale_and_changes_nothing() {
        let cases: [(&str, fn(&mut Nav) -> u64, Option<Query>); 4] = [
            ("cancelled", |nav| {
                let r = nav.ask(chan("Veritasium"), false, Back::FromResults);
                nav.cancel();
                r.id
            }, None),
            ("superseded by a newer ask", |nav| {
                let old = nav.ask(chan("Veritasium"), false, Back::FromResults);
                nav.ask(chan("numberphile"), false, Back::FromResults);
                old.id
            }, Some(chan("numberphile"))),
            ("never asked", |nav| nav.ask(chan("Veritasium"), false, Back::FromResults).id + 1000, Some(chan("Veritasium"))),
            ("answered already", |nav| {
                let r = nav.ask(chan("Veritasium"), false, Back::FromResults);
                nav.arrived(r.id, Ok(listing(&["chan1"], false)));
                nav.view = Some(results());
                nav.back = None;
                r.id
            }, None),
        ];
        let mut bad = Vec::new();
        for (case, setup, still_pending) in cases {
            let mut nav = showing_results();
            let id = setup(&mut nav);

            let arrival = nav.arrived(id, Ok(listing(&["late1", "late2"], true)));

            let state = (arrival, nav.view.clone(), nav.back.clone(), nav.error.clone(), nav.pending().map(|r| r.query.clone()));
            let wanted = (Arrival::Stale, Some(results()), None, None, still_pending);
            expect!(bad, state == wanted, "{case}: {state:?}\n    expected {wanted:?}");
        }
        finish(bad);
    }

    #[test]
    fn cancel_drops_only_the_wait() {
        let mut nav = showing_results();
        nav.back = Some(view(search("older"), &["old1"], false));
        let request = nav.ask(chan("Veritasium"), false, Back::FromResults);
        nav.error = Some(failed(chan("Earlier"), "earlier failure"));

        nav.cancel();

        assert_eq!(nav.pending(), None);
        assert_eq!(nav.view, Some(results()));
        assert_eq!(nav.back, Some(view(search("older"), &["old1"], false)));
        assert_eq!(nav.error, Some(failed(chan("Earlier"), "earlier failure")));
        assert_eq!(nav.arrived(request.id, Ok(listing(&["chan1"], false))), Arrival::Stale);
    }

    #[test]
    fn a_channel_opened_from_results_that_fails_keeps_the_results_and_its_retry_leads_back_to_them() {
        let mut nav = showing_results();
        let asked = nav.ask(chan("Veritasium"), false, Back::FromResults);

        let failure = nav.arrived(asked.id, Err("[youtube:tab] Veritasium: HTTP Error 503".into()));
        let after_failure = (nav.view.clone(), nav.back.clone(), nav.pending().cloned());
        let retried = nav.retry().expect("a failed request is retried");
        let error_after_retry = nav.error.clone();
        let success = nav.arrived(retried.id, Ok(listing(&["chan1", "chan2"], true)));

        assert_eq!(failure, Arrival::Failed);
        assert_eq!(after_failure, (Some(results()), None, None));
        assert_eq!((retried.query.clone(), retried.have, retried.append), (chan("Veritasium"), 0, false));
        assert!(retried.id > asked.id, "ids {} then {}", asked.id, retried.id);
        assert_eq!(error_after_retry, None);
        assert_eq!(success, Arrival::New);
        assert_eq!(nav.view, Some(view(chan("Veritasium"), &["chan1", "chan2"], true)));
        assert_eq!(nav.back, Some(results()));
    }

    #[test]
    fn a_failure_records_the_query_append_back_and_message() {
        let mut nav = showing_results();
        let asked = nav.more().expect("the results have more");

        let arrival = nav.arrived(asked.id, Err("[youtube:search] lofi: timed out".into()));

        assert_eq!(arrival, Arrival::Failed);
        assert_eq!(
            nav.error,
            Some(Failed { query: search("lofi"), append: true, back: Back::Keep, message: "[youtube:search] lofi: timed out".into() })
        );
        assert_eq!((nav.view.clone(), nav.back.clone(), nav.pending().cloned()), (Some(results()), None, None));
        let retried = nav.retry().expect("a failed request is retried");
        assert_eq!((retried.query, retried.have, retried.append), (search("lofi"), 2, true));
    }

    #[test]
    fn retry_without_an_error_asks_nothing() {
        let mut nav = showing_results();

        assert_eq!(nav.retry(), None);
        assert_eq!(nav.pending(), None);
    }

    #[test]
    fn a_channel_opened_from_results_after_a_cancel_leads_back_to_the_results() {
        let mut nav = showing_results();
        nav.ask(chan("Veritasium"), false, Back::FromResults);
        nav.cancel();
        let shown_after_cancel = nav.view.clone();
        let asked = nav.ask(chan("Veritasium"), false, Back::FromResults);

        let arrival = nav.arrived(asked.id, Ok(listing(&["chan1"], false)));

        assert_eq!(shown_after_cancel, Some(results()));
        assert_eq!(arrival, Arrival::New);
        assert_eq!(nav.view, Some(view(chan("Veritasium"), &["chan1"], false)));
        assert_eq!(nav.back, Some(results()));
    }

    #[test]
    fn a_new_list_sets_the_way_back_as_its_request_says() {
        let older = || view(search("older"), &["old1"], false);
        let other_channel = || view(chan("numberphile"), &["num1"], true);
        let cases: [(&str, Option<View>, Option<View>, Back, Option<View>); 8] = [
            ("from search results", Some(results()), None, Back::FromResults, Some(results())),
            ("from results, replacing an older way back", Some(results()), Some(older()), Back::FromResults, Some(results())),
            ("from a channel: the way back stays", Some(other_channel()), Some(older()), Back::FromResults, Some(older())),
            ("from nothing shown", None, None, Back::FromResults, None),
            ("a channel opened directly after a search", Some(results()), None, Back::Clear, None),
            ("a fresh start forgets the way back", Some(other_channel()), Some(older()), Back::Clear, None),
            ("within a channel the way back stays", Some(other_channel()), Some(older()), Back::Keep, Some(older())),
            ("keep with search results shown adds no way back", Some(results()), None, Back::Keep, None),
        ];
        let mut bad = Vec::new();
        for (case, shown, back_before, back, expected_back) in cases {
            let mut nav = Nav::default();
            nav.view = shown;
            nav.back = back_before;
            let asked = nav.ask(chan("Veritasium"), false, back);

            let arrival = nav.arrived(asked.id, Ok(listing(&["new1", "new2"], false)));

            let state = (arrival, nav.view.clone(), nav.back.clone(), nav.pending().cloned());
            let wanted = (Arrival::New, Some(view(chan("Veritasium"), &["new1", "new2"], false)), expected_back, None);
            expect!(bad, state == wanted, "{case}: {state:?}\n    expected {wanted:?}");
        }
        finish(bad);
    }

    #[test]
    fn a_next_page_is_added_after_the_list_without_repeats() {
        let cases: [(&str, &[&str], bool, &[&str], bool); 5] = [
            ("new videos", &["new1", "new2"], false, &["res1", "res2", "res3", "new1", "new2"], false),
            ("still more after it", &["new1"], true, &["res1", "res2", "res3", "new1"], true),
            (
                "repeats of shown videos and within the page",
                &["res3", "new1", "new1", "res1", "new2"],
                true,
                &["res1", "res2", "res3", "new1", "new2"],
                true,
            ),
            ("only repeats", &["res2", "res1"], false, &["res1", "res2", "res3"], false),
            ("an empty page", &[], false, &["res1", "res2", "res3"], false),
        ];
        let mut bad = Vec::new();
        for (case, page, page_more, expected_ids, expected_more) in cases {
            let mut nav = Nav::default();
            nav.view = Some(view(search("lofi"), &["res1", "res2", "res3"], true));
            let asked = nav.more().expect("the list has more");

            let arrival = nav.arrived(asked.id, Ok(listing(page, page_more)));

            let state = (arrival, nav.view.clone());
            let wanted = (Arrival::More, Some(view(search("lofi"), expected_ids, expected_more)));
            expect!(bad, state == wanted, "{case}: {state:?}\n    expected {wanted:?}");
        }
        finish(bad);
    }

    #[test]
    fn a_next_page_for_a_list_no_longer_shown_is_not_added() {
        let mut nav = Nav::default();
        nav.view = Some(view(search("lofi"), &["res1"], true));
        let asked = nav.more().expect("the list has more");
        nav.view = Some(view(chan("numberphile"), &["num1"], true));

        let arrival = nav.arrived(asked.id, Ok(listing(&["new1"], false)));

        assert_eq!(arrival, Arrival::More);
        assert_eq!(nav.view, Some(view(chan("numberphile"), &["num1"], true)));
    }

    #[test]
    fn more_asks_for_the_next_page_only_while_there_is_one_and_the_list_is_under_the_cap() {
        let ids = |n: usize| (0..n).map(|i| format!("v{i:010}")).collect::<Vec<_>>();
        let cases: [(&str, Option<(usize, bool)>, Option<usize>); 7] = [
            ("nothing shown", None, None),
            ("no more", Some((3, false)), None),
            ("more", Some((3, true)), Some(3)),
            ("empty list with more", Some((0, true)), Some(0)),
            ("one under the cap", Some((MAX_ITEMS - 1, true)), Some(MAX_ITEMS - 1)),
            ("at the cap", Some((MAX_ITEMS, true)), None),
            ("over the cap", Some((MAX_ITEMS + 1, true)), None),
        ];
        let mut bad = Vec::new();
        for (case, shown, expected_have) in cases {
            let mut nav = Nav::default();
            nav.view = shown.map(|(n, more)| {
                let ids = ids(n);
                view(chan("Veritasium"), &ids.iter().map(String::as_str).collect::<Vec<_>>(), more)
            });

            let can = nav.can_show_more();
            let asked = nav.more().map(|r| (r.query, r.have, r.append));

            let wanted = expected_have.map(|have| (chan("Veritasium"), have, true));
            expect!(bad, asked == wanted, "{case}: more() = {asked:?}, expected {wanted:?}");
            expect!(bad, can == wanted.is_some(), "{case}: can_show_more() = {can}");
            expect!(bad, nav.pending().is_some() == wanted.is_some(), "{case}: pending {:?}", nav.pending());
        }
        assert_eq!(MAX_ITEMS, 200);
        finish(bad);
    }

    #[test]
    fn more_keeps_the_way_back() {
        let mut nav = Nav::default();
        nav.view = Some(view(chan("Veritasium"), &["chan1"], true));
        nav.back = Some(results());
        let asked = nav.more().expect("the channel has more");

        let arrival = nav.arrived(asked.id, Ok(listing(&["chan2"], false)));

        assert_eq!(arrival, Arrival::More);
        assert_eq!(nav.view, Some(view(chan("Veritasium"), &["chan1", "chan2"], false)));
        assert_eq!(nav.back, Some(results()));
    }

    #[test]
    fn go_back_without_a_way_back_changes_nothing() {
        let mut nav = showing_results();
        let asked = nav.ask(chan("Veritasium"), false, Back::Clear);
        nav.error = Some(failed(chan("Earlier"), "earlier failure"));

        let went = nav.go_back();

        assert!(!went);
        assert_eq!(nav.view, Some(results()));
        assert_eq!(nav.back, None);
        assert_eq!(nav.pending(), Some(&asked));
        assert_eq!(nav.error, Some(failed(chan("Earlier"), "earlier failure")));
    }

    #[test]
    fn go_back_shows_the_results_and_drops_the_wait_and_the_error() {
        let mut nav = Nav::default();
        nav.view = Some(view(chan("Veritasium"), &["chan1"], true));
        nav.back = Some(results());
        let asked = nav.ask(chan("numberphile"), false, Back::Keep);
        nav.error = Some(failed(chan("Earlier"), "earlier failure"));

        let went = nav.go_back();

        assert!(went);
        assert_eq!(nav.view, Some(results()));
        assert_eq!(nav.back, None);
        assert_eq!(nav.pending(), None);
        assert_eq!(nav.error, None);
        assert_eq!(nav.arrived(asked.id, Ok(listing(&["num1"], false))), Arrival::Stale);
    }

    #[test]
    fn shown_ids_are_the_videos_of_the_list_shown_and_the_way_back() {
        let set = |ids: &[&str]| ids.iter().map(|s| s.to_string()).collect::<std::collections::HashSet<String>>();
        let cases: [(&str, Option<View>, Option<View>, &[&str]); 4] = [
            ("nothing", None, None, &[]),
            ("the list shown", Some(view(chan("Veritasium"), &["chan1", "chan2"], true)), None, &["chan1", "chan2"]),
            ("the way back only", None, Some(results()), &["res1", "res2"]),
            ("both, sharing a video", Some(view(chan("Veritasium"), &["chan1", "res2"], true)), Some(results()), &["chan1", "res1", "res2"]),
        ];
        let mut bad = Vec::new();
        for (case, shown, back, expected) in cases {
            let mut nav = Nav::default();
            nav.view = shown;
            nav.back = back;

            let actual = nav.shown_ids();

            expect!(bad, actual == set(expected), "{case}: {actual:?}, expected {expected:?}");
        }
        finish(bad);
    }
}

mod thumbnails {
    use super::*;

    const WIDTH: usize = 50;
    const HEIGHT: usize = 30;
    /// The colour `tests/fixtures/generate.ps1` paints the fixture with.
    const COLOUR: [u8; 3] = [60, 140, 220];
    /// JPEG stores the colour as YCbCr and quantises it.
    const TOLERANCE: u8 = 4;

    fn off_colour_pixels(rgb: &[u8]) -> Vec<(usize, [u8; 3])> {
        rgb.chunks(3)
            .enumerate()
            .filter(|(_, p)| p.iter().zip(COLOUR).any(|(a, b)| a.abs_diff(b) > TOLERANCE))
            .map(|(i, p)| (i, [p[0], p[1], p[2]]))
            .collect()
    }

    #[test]
    fn decode_jpeg_gives_the_size_and_the_colour_of_every_pixel() {
        let decoded = decode_jpeg(&fixture_bytes("solid_50x30.jpg"));

        let (w, h, rgb) = decoded.expect("the fixture decodes");
        assert_eq!((w, h, rgb.len()), (WIDTH, HEIGHT, WIDTH * HEIGHT * 3));
        assert_eq!(off_colour_pixels(&rgb), Vec::new());
    }

    #[test]
    fn decode_jpeg_refuses_bytes_that_are_not_a_jpeg() {
        let png_signature = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        let cases: [(&str, Vec<u8>); 3] = [("empty", Vec::new()), ("text", b"<html>not a picture</html>".to_vec()), ("a PNG header", png_signature)];
        let mut bad = Vec::new();
        for (case, bytes) in cases {
            let actual = decode_jpeg(&bytes);
            expect!(bad, actual.as_ref().is_err_and(|e| e.starts_with("unreadable picture")), "{case}: {:?}", actual.map(|(w, h, _)| (w, h)));
        }
        finish(bad);
    }

    #[test]
    fn fetch_thumbnail_downloads_and_decodes_the_picture() {
        let jpeg = fixture_bytes("solid_50x30.jpg");
        let server = RangeServer::start(vec![("/vi/aB3_-x9Zq0Y/mqdefault.jpg", Mode::IgnoresRange, jpeg.clone())]);

        let fetched = fetch_thumbnail(&server.url("/vi/aB3_-x9Zq0Y/mqdefault.jpg"));

        let (w, h, rgb) = fetched.expect("the thumbnail downloads and decodes");
        assert_eq!((w, h, rgb.len()), (WIDTH, HEIGHT, WIDTH * HEIGHT * 3));
        assert_eq!(off_colour_pixels(&rgb), Vec::new());
        let paths: Vec<String> = server.requests().iter().map(|r| r.path.clone()).collect();
        assert_eq!(paths, ["/vi/aB3_-x9Zq0Y/mqdefault.jpg"]);
    }

    #[test]
    fn fetch_thumbnail_reports_a_missing_picture_and_one_that_is_not_a_jpeg() {
        let server = RangeServer::start(vec![("/vi/notajpeg000/mqdefault.jpg", Mode::IgnoresRange, b"<html>gone</html>".to_vec())]);
        let missing = server.url("/vi/missing0000/mqdefault.jpg");

        let not_found = fetch_thumbnail(&missing);
        let not_a_jpeg = fetch_thumbnail(&server.url("/vi/notajpeg000/mqdefault.jpg"));

        assert_eq!(not_found.map(|(w, h, _)| (w, h)), Err(format!("cannot download {missing}: http status: 404")));
        assert!(not_a_jpeg.as_ref().is_err_and(|e| e.starts_with("unreadable picture")), "{:?}", not_a_jpeg.map(|(w, h, _)| (w, h)));
    }
}

mod recent_channels {
    use super::*;

    fn numbered(n: usize) -> Channel {
        channel(&format!("Channel {n}"), &format!("https://www.youtube.com/@handle{n}"))
    }

    fn recent(channels: Vec<Channel>) -> Recent {
        Recent { channels }
    }

    fn fresh_dir(test: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("playr-browse").join(test);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    #[test]
    fn record_puts_the_channel_first_once_and_keeps_the_newest_eight() {
        let mut list = recent(vec![numbered(1), numbered(2), numbered(3)]);
        list.record(&numbered(4));
        let after_new = list.clone();
        list.record(&channel("Channel 2 renamed", "https://www.youtube.com/@handle2"));
        let after_again = list.clone();
        let mut full = recent((1..=8).map(numbered).collect());
        full.record(&numbered(9));

        assert_eq!(after_new, recent(vec![numbered(4), numbered(1), numbered(2), numbered(3)]));
        assert_eq!(after_again, recent(vec![channel("Channel 2 renamed", "https://www.youtube.com/@handle2"), numbered(4), numbered(1), numbered(3)]));
        assert_eq!(RECENT_LIMIT, 8);
        assert_eq!(full, recent(vec![numbered(9), numbered(1), numbered(2), numbered(3), numbered(4), numbered(5), numbered(6), numbered(7)]));
    }

    #[test]
    fn record_into_an_empty_list_and_of_the_first_channel_again() {
        let mut list = Recent::default();
        list.record(&numbered(5));
        list.record(&numbered(5));

        assert_eq!(list, recent(vec![numbered(5)]));
    }

    #[test]
    fn remove_drops_only_the_channel_with_that_address() {
        let mut list = recent(vec![numbered(1), numbered(2), numbered(3)]);
        list.remove("https://www.youtube.com/@handle2");
        let after_remove = list.clone();
        list.remove("https://www.youtube.com/@handle9");

        assert_eq!(after_remove, recent(vec![numbered(1), numbered(3)]));
        assert_eq!(list, recent(vec![numbered(1), numbered(3)]));
    }

    #[test]
    fn to_json_and_parse_round_trip() {
        let list = recent(vec![
            channel("Zażółć \"gęślą\" jaźń", "https://www.youtube.com/@Za%C5%BC%C3%B3%C5%82%C4%87"),
            channel("Kurzgesagt – In a Nutshell", "https://www.youtube.com/c/Kurzgesagt"),
            channel("Google for Developers", "https://www.youtube.com/channel/UC_x5XG1OV2P6uZZ5FSM9Ttw"),
        ]);

        assert_eq!(Recent::parse(&list.to_json()), list);
        assert_eq!(Recent::parse(&Recent::default().to_json()), Recent::default());
    }

    #[test]
    fn parse_skips_entries_without_a_name_or_a_channel_address_and_repeats_and_keeps_eight() {
        let mut entries = vec![
            json!({ "name": "Kept First", "url": "https://m.youtube.com/@first/videos" }),
            json!({ "url": "https://www.youtube.com/@noname" }),
            json!({ "name": "   ", "url": "https://www.youtube.com/@blankname" }),
            json!({ "name": "No Address" }),
            json!({ "name": "Video Address", "url": "https://www.youtube.com/watch?v=aB3_-x9Zq0Y" }),
            json!({ "name": "Number Address", "url": 42 }),
            json!({ "name": "Repeat Of First", "url": "https://www.youtube.com/@first" }),
        ];
        entries.extend((2..=9).map(|n| json!({ "name": format!("Channel {n}"), "url": format!("https://www.youtube.com/@handle{n}") })));
        let text = json!({ "channels": entries }).to_string();

        let parsed = Recent::parse(&text);

        let mut expected = vec![channel("Kept First", "https://www.youtube.com/@first")];
        expected.extend((2..=8).map(numbered));
        assert_eq!(parsed, recent(expected));
    }

    #[test]
    fn parse_of_damaged_json_is_an_empty_list() {
        let mut bad = Vec::new();
        for text in ["", "{\"channels\": [", "[]", "{\"channels\": {\"name\": \"x\"}}", "{\"other\": []}", "null"] {
            let actual = Recent::parse(text);
            expect!(bad, actual == Recent::default(), "{text:?}: {actual:?}");
        }
        finish(bad);
    }

    #[test]
    fn save_creates_the_folder_and_load_reads_the_list_back_leaving_no_partial_file() {
        let path = fresh_dir("save_load").join("nested").join("channels.json");
        let list = recent(vec![numbered(3), channel("Zażółć", "https://www.youtube.com/@Za%C5%BC%C3%B3%C5%82%C4%87")]);

        let saved = list.save(&path);
        let loaded = Recent::load(&path);

        assert_eq!(saved, Ok(()));
        assert_eq!(loaded, list);
        assert!(!path.with_extension("json.part").exists(), "the partial file was left behind");
    }

    #[test]
    fn save_writes_through_a_partial_file_and_leaves_the_old_list_when_that_fails() {
        let path = fresh_dir("save_partial").join("channels.json");
        let old = recent(vec![numbered(1)]);
        old.save(&path).unwrap();
        std::fs::create_dir_all(path.with_extension("json.part")).unwrap();

        let saved = recent(vec![numbered(2)]).save(&path);

        assert!(saved.as_ref().is_err_and(|e| e.starts_with("cannot save the channels: ")), "{saved:?}");
        assert_eq!(Recent::load(&path), old);
    }

    #[test]
    fn load_of_a_missing_file_is_an_empty_list() {
        let path = fresh_dir("load_missing").join("channels.json");

        assert_eq!(Recent::load(&path), Recent::default());
    }
}

fn is_child() -> bool {
    std::env::var_os(CHILD).is_some()
}

fn test_dir() -> PathBuf {
    PathBuf::from(std::env::var_os(DIR).expect("PLAYR_TEST_DIR"))
}

/// In the test process: runs `test` alone in a copy of this binary with a fake yt-dlp scripted
/// by `script` and a private data folder, and returns false. In that copy: returns true.
fn in_child(test: &str, script: Value) -> bool {
    if is_child() {
        return true;
    }
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("playr-browse-child").join(test.replace("::", "_"));
    let _ = std::fs::remove_dir_all(&dir);
    let fake = dir.join("fake");
    std::fs::create_dir_all(&fake).unwrap();
    let exe = fake.join(if cfg!(windows) { "yt-dlp.exe" } else { "yt-dlp" });
    std::fs::copy(std::env::current_exe().unwrap(), &exe).expect("copy the test binary");
    std::fs::write(fake.join("fake.json"), script.to_string()).unwrap();
    let data = dir.join("appdata");
    let env: Vec<(&str, OsString)> = vec![
        (DIR, dir.as_os_str().to_owned()),
        ("LOCALAPPDATA", data.as_os_str().to_owned()),
        ("XDG_DATA_HOME", data.into_os_string()),
        ("PLAYR_YTDLP", exe.into_os_string()),
    ];
    let out = Command::new(std::env::current_exe().unwrap())
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

#[cfg(windows)]
#[used]
#[unsafe(link_section = ".CRT$XCU")]
static FAKE_YTDLP_HOOK: extern "C" fn() = act_as_fake_ytdlp;

#[cfg(target_os = "linux")]
#[used]
#[unsafe(link_section = ".init_array")]
static FAKE_YTDLP_HOOK: extern "C" fn() = act_as_fake_ytdlp;

/// Runs before the test harness: a copy named yt-dlp with a `fake.json` beside it appends its
/// arguments as one JSON array per line to `calls.jsonl`, then writes `stderr` and exits with
/// 1 when the script has it, and prints `answer` otherwise.
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
    let mut log = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("calls.jsonl")).unwrap();
    use std::io::Write;
    writeln!(log, "{}", json!(args)).unwrap();
    if let Some(stderr) = script["stderr"].as_str() {
        eprint!("{stderr}");
        std::process::exit(1);
    }
    match &script["answer"] {
        Value::String(text) => println!("{text}"),
        answer => println!("{answer}"),
    }
    std::process::exit(0);
}

fn calls() -> Vec<Vec<String>> {
    std::fs::read_to_string(test_dir().join("fake").join("calls.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).expect("a logged call"))
        .collect()
}

mod fetching {
    use super::*;

    fn quiet(_: &str) {}

    #[test]
    #[cfg(any(windows, target_os = "linux"))]
    fn fetch_passes_the_exact_arguments_to_ytdlp_and_reads_its_answer() {
        let answer = json!({
            "channel": "Owner Channel", "channel_id": "UCowner000000000000000",
            "uploader_url": "https://www.youtube.com/@OwnerHandle",
            "entries": [
                { "id": "vid0aaaaaa1", "title": "Newest", "duration": 61.0, "view_count": 12, "timestamp": 1_764_000_000 },
                { "ie_key": "YoutubeTab", "id": "UCnotavideo" },
            ],
        });
        if !in_child("fetching::fetch_passes_the_exact_arguments_to_ytdlp_and_reads_its_answer", json!({ "answer": answer })) {
            return;
        }
        let query = Query::Channel { url: "https://www.youtube.com/@OwnerHandle".into() };

        let listing = fetch(&query, 20, &AtomicBool::new(false), &quiet);

        let owner = channel("Owner Channel", "https://www.youtube.com/@OwnerHandle");
        let expected = Listing {
            channel: Some(owner.clone()),
            items: vec![Item {
                id: "vid0aaaaaa1".into(),
                title: "Newest".into(),
                channel: Some(owner),
                duration: Some(61.0),
                views: Some(12),
                uploaded: Some(1_764_000_000),
                live: false,
                upcoming: false,
            }],
            more: false,
        };
        assert_eq!(listing, Ok(expected));
        assert_eq!(calls(), vec![args_with("21:30", "https://www.youtube.com/@OwnerHandle/videos")]);
    }

    #[test]
    #[cfg(any(windows, target_os = "linux"))]
    fn fetch_hands_searches_with_spaces_and_non_ascii_text_to_ytdlp_as_single_arguments() {
        if !in_child("fetching::fetch_hands_searches_with_spaces_and_non_ascii_text_to_ytdlp_as_single_arguments", json!({ "answer": { "entries": [] } })) {
            return;
        }
        let in_channel = Query::InChannel { url: "https://www.youtube.com/@Veritasium".into(), text: " café & tea ".into() };
        let search = Query::Search { text: "  zażółć \"gęślą\" jaźń ".into() };

        let first = fetch(&in_channel, 10, &AtomicBool::new(false), &quiet);
        let second = fetch(&search, 0, &AtomicBool::new(false), &quiet);

        assert_eq!(first, Ok(Listing::default()));
        assert_eq!(second, Ok(Listing::default()));
        assert_eq!(
            calls(),
            vec![
                args_with("11:20", "https://www.youtube.com/@Veritasium/search?query=caf%C3%A9%20%26%20tea"),
                args_with("1:10", "ytsearch10:zażółć \"gęślą\" jaźń"),
            ]
        );
    }

    #[test]
    #[cfg(any(windows, target_os = "linux"))]
    fn fetch_reports_ytdlp_s_error_line_when_it_fails() {
        let stderr = "WARNING: [youtube:tab] slow answer\nERROR: [youtube:tab] @nobody: This channel does not exist.\n";
        if !in_child("fetching::fetch_reports_ytdlp_s_error_line_when_it_fails", json!({ "stderr": stderr })) {
            return;
        }
        let query = Query::Channel { url: "https://www.youtube.com/@nobody".into() };

        let listing = fetch(&query, 0, &AtomicBool::new(false), &quiet);

        assert_eq!(listing, Err("[youtube:tab] @nobody: This channel does not exist.".to_string()));
        assert_eq!(calls(), vec![args_with("1:10", "https://www.youtube.com/@nobody/videos")]);
    }

    #[test]
    #[cfg(any(windows, target_os = "linux"))]
    fn fetch_reports_an_answer_that_is_not_json() {
        if !in_child("fetching::fetch_reports_an_answer_that_is_not_json", json!({ "answer": "Usage: yt-dlp [OPTIONS] URL" })) {
            return;
        }

        let listing = fetch(&Query::Search { text: "anything".into() }, 0, &AtomicBool::new(false), &quiet);

        assert!(listing.as_ref().is_err_and(|e| e.starts_with("yt-dlp sent an unreadable answer (")), "{listing:?}");
        assert_eq!(calls(), vec![args_with("1:10", "ytsearch10:anything")]);
    }
}

mod data_folder {
    use super::*;

    #[test]
    fn recent_path_is_channels_json_in_playrs_data_folder() {
        if !in_child("data_folder::recent_path_is_channels_json_in_playrs_data_folder", json!({})) {
            return;
        }

        assert_eq!(recent_path(), Some(test_dir().join("appdata").join("playr").join("channels.json")));
    }
}
