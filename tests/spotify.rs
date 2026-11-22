//! `playr::spotify`: recognising Spotify links and reading the track list out of the embed
//! page. The test against the real Spotify is `#[ignore]`d.

mod common;

use common::finish;
use playr::spotify::{Entry, Kind, Playlist, parse_embed, parse_url};
use serde_json::{Value, json};

const ID: &str = "37i9dQZF1DXcBWIGoYBM5M";

mod links {
    use super::*;

    #[test]
    fn every_supported_link_form_yields_kind_and_id() {
        let cases = [
            (format!("https://open.spotify.com/playlist/{ID}"), Kind::Playlist),
            (format!("https://open.spotify.com/playlist/{ID}?si=a1b2c3"), Kind::Playlist),
            (format!("https://open.spotify.com/album/{ID}/"), Kind::Album),
            (format!("https://open.spotify.com/intl-pl/track/{ID}"), Kind::Track),
            (format!("https://open.spotify.com/embed/episode/{ID}"), Kind::Episode),
            (format!("https://open.spotify.com/intl-de/embed/show/{ID}#top"), Kind::Show),
            (format!("http://OPEN.SPOTIFY.COM/playlist/{ID}"), Kind::Playlist),
            (format!("  https://open.spotify.com/track/{ID}\n"), Kind::Track),
            (format!("spotify:playlist:{ID}"), Kind::Playlist),
            (format!("spotify:album:{ID}"), Kind::Album),
            (format!("spotify:episode:{ID}"), Kind::Episode),
        ];
        let mut bad = Vec::new();
        for (url, kind) in &cases {
            let actual = parse_url(url);
            expect!(bad, actual == Some((*kind, ID.to_string())), "parse_url({url:?}) = {actual:?}, expected {kind:?}");
        }
        finish(bad);
    }

    #[test]
    fn other_hosts_kinds_and_malformed_ids_are_not_spotify_lists() {
        let cases = [
            ("other host", format!("https://spotify.com/playlist/{ID}")),
            ("look-alike host", format!("https://open.spotify.com.evil.example/playlist/{ID}")),
            ("spotify as a path", format!("https://evil.example/open.spotify.com/playlist/{ID}")),
            ("artist pages are not lists", format!("https://open.spotify.com/artist/{ID}")),
            ("id of 21 characters", format!("https://open.spotify.com/playlist/{}", &ID[..21])),
            ("id of 23 characters", format!("https://open.spotify.com/playlist/{ID}x")),
            ("dash in the id", "https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM-M".to_string()),
            ("no id", "https://open.spotify.com/playlist/".to_string()),
            ("only the host", "https://open.spotify.com".to_string()),
            ("only a market prefix", "https://open.spotify.com/intl-pl/".to_string()),
            ("user uri", "spotify:user:someone".to_string()),
            ("uri without an id", "spotify:playlist:".to_string()),
            ("no scheme", format!("open.spotify.com/playlist/{ID}")),
            ("empty", String::new()),
        ];
        let mut bad = Vec::new();
        for (case, url) in &cases {
            let actual = parse_url(url);
            expect!(bad, actual.is_none(), "{case}: parse_url({url:?}) = {actual:?}, expected None");
        }
        finish(bad);
    }
}

fn page(entity: Option<Value>) -> String {
    let data = match entity {
        Some(e) => json!({ "props": { "pageProps": { "state": { "data": { "entity": e } } } } }),
        None => json!({ "props": { "pageProps": { "status": 404 } } }),
    };
    format!(
        "<!DOCTYPE html><html><head><script src=\"/x.js\"></script></head><body><div id=\"__next\"></div>\
         <script id=\"__NEXT_DATA__\" type=\"application/json\">{data}</script><script>var x=1;</script></body></html>"
    )
}

fn entry(title: &str, subtitle: &str, duration: Option<f64>) -> Entry {
    Entry { title: title.into(), subtitle: subtitle.into(), duration }
}

mod embed_page {
    use super::*;

    #[test]
    fn reads_the_name_and_every_titled_track() {
        let entity = json!({
            "name": "Road Trip",
            "title": "not the name",
            "trackList": [
                { "title": "Song A", "subtitle": "Artist X, Artist Y", "duration": 215_000 },
                { "name": "Song B", "artists": [{ "name": "P" }, { "name": "Q" }], "duration": 0 },
                { "title": "  ", "subtitle": "a track without a title" },
                { "title": " Song C ", "subtitle": "Sub", "artists": [{ "name": "ignored" }], "duration": 2500 },
                { "title": "Song D" },
            ],
        });

        let list = parse_embed(&page(Some(entity))).expect("parse");

        assert_eq!(
            list,
            Playlist {
                name: "Road Trip".into(),
                entries: vec![
                    entry("Song A", "Artist X, Artist Y", Some(215.0)),
                    entry("Song B", "P, Q", None),
                    entry("Song C", "Sub", Some(2.5)),
                    entry("Song D", "", None),
                ],
            }
        );
    }

    #[test]
    fn a_single_episode_page_is_a_list_of_one() {
        let entity = json!({ "title": "Episode 7", "subtitle": "The Show", "duration": 3_600_500 });

        let list = parse_embed(&page(Some(entity))).expect("parse");

        assert_eq!(list, Playlist { name: "Episode 7".into(), entries: vec![entry("Episode 7", "The Show", Some(3600.5))] });
    }

    #[test]
    fn a_list_without_a_name_is_called_spotify() {
        let entity = json!({ "trackList": [{ "title": "Only" }] });

        let list = parse_embed(&page(Some(entity))).expect("parse");

        assert_eq!(list.name, "Spotify");
    }

    #[test]
    fn pages_without_a_usable_list_are_errors() {
        let marker = "<script id=\"__NEXT_DATA__\" type=\"application/json\">";
        let cases = [
            ("no data script", "<html><body>Log in</body></html>".to_string(), "Spotify sent a page without the track list".to_string()),
            ("cut off", format!("<html>{marker}{{\"props\": "), "Spotify sent a cut-off page".to_string()),
            ("private or removed", page(None), "Spotify's page has no playlist in it (is the link private or removed?)".to_string()),
            ("empty list", page(Some(json!({ "name": "Empty", "trackList": [] }))), "this Spotify list is empty".to_string()),
            ("only untitled tracks", page(Some(json!({ "name": "X", "trackList": [{ "subtitle": "s" }] }))), "this Spotify list is empty".to_string()),
        ];
        let mut bad = Vec::new();
        for (case, html, expected) in cases {
            let actual = parse_embed(&html);
            expect!(bad, actual == Err(expected.clone()), "{case}: {actual:?}, expected Err({expected:?})");
        }
        finish(bad);
    }

    #[test]
    fn unreadable_json_is_an_error() {
        let html = "<script id=\"__NEXT_DATA__\" type=\"application/json\">{not json}</script>";

        let err = parse_embed(html).expect_err("not json");

        assert!(err.starts_with("Spotify sent an unreadable list (") && err.ends_with(')'), "{err}");
    }
}

#[test]
fn entry_query_and_label_put_artist_and_title_together() {
    let song = entry("Jóga", "Björk", Some(305.0));
    let untitled_artist = entry("Lonely Title", "", None);

    assert_eq!(song.query(), "Björk Jóga");
    assert_eq!(song.label(), "Jóga — Björk");
    assert_eq!(untitled_artist.query(), "Lonely Title");
    assert_eq!(untitled_artist.label(), "Lonely Title");
}

#[test]
#[ignore = "needs the internet and Spotify"]
fn fetch_reads_a_public_playlist() {
    let list = playr::spotify::fetch("https://open.spotify.com/playlist/1PdmdWWClhyLiIkK1fZeuV", None).expect("fetch");

    assert!(list.entries.len() > 1, "{list:?}");
    assert!(list.entries.iter().all(|e| !e.title.is_empty()), "{list:?}");
}
