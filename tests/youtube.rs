//! `playr::youtube` without the network: address recognition, reading `yt-dlp -J` output,
//! choosing streams and captions, search-result choice and the yt-dlp download checks.
//! The tests that ask the real YouTube are `#[ignore]`d.

mod common;

use common::finish;
use playr::youtube::{
    AudioFormat, Caption, CaptionKind, Choice, Video, VideoFormat, duration_tolerance, expected_sha256, is_youtube_url,
    parse_info, parse_search, pick_search_result, release_asset, sha256_hex, unplayable_reason, video_id,
};
use serde_json::{Value, json};

const ID: &str = "aB3_-x9Zq0Y";

mod addresses {
    use super::*;

    #[test]
    fn every_supported_address_form_yields_the_video_id() {
        let cases = [
            ("watch page", format!("https://www.youtube.com/watch?v={ID}")),
            ("v not the first parameter", format!("https://youtube.com/watch?feature=share&v={ID}&t=42")),
            ("plain http, mobile host", format!("http://m.youtube.com/watch?v={ID}")),
            ("music host with a playlist", format!("https://music.youtube.com/watch?v={ID}&list=RDAMVM")),
            ("short link with a time", format!("https://youtu.be/{ID}?t=10")),
            ("short link with www", format!("https://www.youtu.be/{ID}")),
            ("shorts", format!("https://www.youtube.com/shorts/{ID}")),
            ("live", format!("https://www.youtube.com/live/{ID}?si=Xy7")),
            ("embed", format!("https://www.youtube.com/embed/{ID}")),
            ("old flash player path", format!("https://www.youtube.com/v/{ID}")),
            ("no-cookie embed", format!("https://www.youtube-nocookie.com/embed/{ID}")),
            ("host in capitals", format!("https://WWW.YouTube.COM/watch?v={ID}")),
            ("explicit port", format!("https://www.youtube.com:443/watch?v={ID}")),
            ("fragment after the id", format!("https://www.youtube.com/watch?v={ID}#t=30")),
            ("surrounding whitespace", format!("  https://youtu.be/{ID}\n")),
            ("all 64 id characters allowed", "https://youtu.be/09azAZ-_Kk1".to_string()),
        ];
        let mut bad = Vec::new();
        for (case, url) in &cases {
            let expected = if *case == "all 64 id characters allowed" { "09azAZ-_Kk1" } else { ID };
            let id = video_id(url);
            expect!(bad, id.as_deref() == Some(expected), "{case}: video_id({url:?}) = {id:?}, expected {expected:?}");
            expect!(bad, is_youtube_url(url), "{case}: is_youtube_url({url:?}) is false");
        }
        finish(bad);
    }

    #[test]
    fn other_hosts_paths_and_malformed_ids_are_not_youtube_videos() {
        let cases = [
            ("id of 10 characters", format!("https://www.youtube.com/watch?v={}", &ID[..10])),
            ("id of 12 characters", format!("https://www.youtube.com/watch?v={ID}x")),
            ("short link id of 12 characters", format!("https://youtu.be/{ID}x")),
            ("dot in the id", "https://youtu.be/aB3_-x9Zq.Y".to_string()),
            ("non-ascii letter in the id", "https://youtu.be/aB3_-x9Zqą".to_string()),
            ("empty v", "https://www.youtube.com/watch?v=&list=x".to_string()),
            ("vv instead of v", format!("https://www.youtube.com/watch?vv={ID}")),
            ("look-alike host", format!("https://youtube.com.evil.example/watch?v={ID}")),
            ("youtube.com as a path", format!("https://evil.example/youtube.com/watch?v={ID}")),
            ("host with a prefix", format!("https://notyoutube.com/watch?v={ID}")),
            ("youtube.com as user info", format!("https://youtube.com@evil.example/watch?v={ID}")),
            ("channel page", "https://www.youtube.com/channel/UCabcdefghij".to_string()),
            ("playlist page", "https://www.youtube.com/playlist?list=PLabcdefghi".to_string()),
            ("shorts without an id", "https://www.youtube.com/shorts/".to_string()),
            ("short link without an id", "https://youtu.be/".to_string()),
            ("other scheme", format!("ftp://youtu.be/{ID}")),
            ("empty", String::new()),
        ];
        let mut bad = Vec::new();
        for (case, url) in &cases {
            let id = video_id(url);
            expect!(bad, id.is_none(), "{case}: video_id({url:?}) = {id:?}, expected None");
            expect!(bad, !is_youtube_url(url), "{case}: is_youtube_url({url:?}) is true");
        }
        finish(bad);
    }
}

fn video_fmt(id: &str, height: u32, fps: f64, tbr: f64) -> Value {
    json!({
        "format_id": id, "url": format!("https://v.example/{id}"), "vcodec": "avc1.640028", "acodec": "none",
        "protocol": "https", "ext": "mp4", "width": height * 16 / 9, "height": height, "fps": fps, "tbr": tbr,
    })
}

fn audio_fmt(id: &str, language: Option<&str>, note: &str, preference: f64, abr: f64) -> Value {
    json!({
        "format_id": id, "url": format!("https://a.example/{id}"), "vcodec": "none", "acodec": "mp4a.40.2",
        "protocol": "https", "ext": "m4a", "language": language, "format_note": note,
        "language_preference": preference, "abr": abr,
    })
}

fn info_json(formats: Vec<Value>, extra: Value) -> String {
    let mut v = json!({ "id": ID, "title": "A title", "channel": "A channel", "duration": 212.0, "formats": formats });
    for (k, val) in extra.as_object().expect("object") {
        v[k] = val.clone();
    }
    v.to_string()
}

fn vf(id: &str, width: u32, height: u32, fps: f64, bitrate: f64) -> VideoFormat {
    VideoFormat { id: id.into(), url: format!("https://v.example/{id}"), width, height, fps, bitrate }
}

fn af(id: &str, language: Option<&str>, label: &str, default: bool, bitrate: f64) -> AudioFormat {
    AudioFormat {
        id: id.into(),
        url: format!("https://a.example/{id}"),
        language: language.map(str::to_string),
        label: label.into(),
        default,
        bitrate,
    }
}

mod video_formats {
    use super::*;

    #[test]
    fn keeps_only_separate_h264_mp4_over_https_best_per_height_highest_first() {
        let mut muxed = video_fmt("18", 360, 30.0, 500.0);
        muxed["acodec"] = json!("mp4a.40.2");
        let mut vp9 = video_fmt("400", 1440, 30.0, 9000.0);
        vp9["vcodec"] = json!("vp09.00.50.08");
        let mut hls = video_fmt("95", 480, 30.0, 1200.0);
        hls["protocol"] = json!("m3u8_native");
        let mut webm = video_fmt("244w", 480, 30.0, 1100.0);
        webm["ext"] = json!("webm");
        let mut no_url = video_fmt("397", 480, 30.0, 1000.0);
        no_url.as_object_mut().unwrap().remove("url");
        let mut no_height = video_fmt("398", 480, 30.0, 1000.0);
        no_height.as_object_mut().unwrap().remove("height");
        let formats = vec![
            video_fmt("160", 144, 25.0, 80.0),
            muxed,
            vp9,
            hls,
            webm,
            no_url,
            no_height,
            video_fmt("137", 1080, 30.0, 4000.0),
            video_fmt("136", 720, 30.0, 2000.0),
            // Same height, same fps, more bits: replaces 136.
            video_fmt("136b", 720, 30.0, 2500.0),
            // Same height, fewer bits: does not replace the 2500 one.
            video_fmt("136c", 720, 30.0, 1500.0),
            // Same height, higher frame rate though fewer bits: replaces 137.
            video_fmt("299", 1080, 60.0, 3900.0),
            // Equal to the kept 299 in fps and bitrate: the first one stays.
            video_fmt("299b", 1080, 60.0, 3900.0),
        ];

        let video = parse_info(&info_json(formats, json!({}))).expect("parse");

        assert_eq!(
            video.video,
            vec![vf("299", 1920, 1080, 60.0, 3900.0), vf("136b", 1280, 720, 30.0, 2500.0), vf("160", 256, 144, 25.0, 80.0)]
        );
    }

    fn live_fmt(id: &str, height: Option<u32>, vcodec: &str, acodec: &str, protocol: &str, tbr: f64) -> Value {
        json!({
            "format_id": id, "url": format!("https://v.example/{id}"), "vcodec": vcodec, "acodec": acodec,
            "protocol": protocol, "ext": "mp4", "width": height.map(|h| h * 16 / 9), "height": height, "fps": 30.0, "tbr": tbr,
        })
    }

    #[test]
    fn a_live_video_offers_its_h264_aac_hls_formats_one_per_height_highest_first() {
        let formats = vec![
            live_fmt("92", Some(240), "avc1.4d4015", "mp4a.40.5", "m3u8", 300.0),
            live_fmt("95", Some(720), "avc1.4d401f", "mp4a.40.2", "m3u8_native", 2500.0),
            // The same height again: the first one stays.
            live_fmt("95b", Some(720), "avc1.4d401f", "mp4a.40.2", "m3u8_native", 3000.0),
            live_fmt("96", Some(1080), "avc1.640028", "mp4a.40.2", "m3u8_native", 4500.0),
            live_fmt("vp9", Some(1440), "vp09.00.50.08", "mp4a.40.2", "m3u8_native", 9000.0),
            live_fmt("no-sound", Some(480), "avc1.4d401e", "none", "m3u8_native", 1200.0),
            live_fmt("opus", Some(360), "avc1.4d401e", "opus", "m3u8_native", 800.0),
            live_fmt("dash", Some(144), "avc1.4d400c", "none", "https", 100.0),
            live_fmt("no-height", None, "avc1.4d401e", "mp4a.40.2", "m3u8_native", 700.0),
        ];

        let live = parse_info(&info_json(formats.clone(), json!({ "is_live": true }))).expect("parse");
        let recorded = parse_info(&info_json(formats, json!({ "is_live": false }))).expect("parse");

        assert_eq!(
            live.video,
            vec![vf("96", 1920, 1080, 30.0, 4500.0), vf("95", 1280, 720, 30.0, 2500.0), vf("92", 426, 240, 30.0, 300.0)]
        );
        assert_eq!((live.live, unplayable_reason(&live)), (true, None));
        // Only a live video is played over HLS; a recording's HLS formats stay unused.
        assert_eq!(recorded.video, vec![vf("dash", 256, 144, 30.0, 100.0)]);
    }

    #[test]
    fn a_live_video_without_an_h264_aac_hls_format_says_so() {
        let formats = vec![
            live_fmt("vp9", Some(1440), "vp09.00.50.08", "mp4a.40.2", "m3u8_native", 9000.0),
            live_fmt("dash", Some(144), "avc1.4d400c", "none", "https", 100.0),
        ];

        let live = parse_info(&info_json(formats, json!({ "is_live": true }))).expect("parse");

        assert_eq!(live.video, vec![]);
        assert_eq!(unplayable_reason(&live).as_deref(), Some("YouTube offers this live stream in no format playr can play"));
    }

    #[test]
    fn label_is_the_height_with_the_frame_rate_only_above_31_fps() {
        let cases = [(1080, 30.0, "1080p"), (720, 31.0, "720p"), (720, 48.0, "720p48"), (1080, 59.94, "1080p60"), (144, 0.0, "144p")];
        let mut bad = Vec::new();
        for (height, fps, expected) in cases {
            let label = vf("x", 0, height, fps, 0.0).label();
            expect!(bad, label == expected, "{height} at {fps} fps: {label:?}, expected {expected:?}");
        }
        finish(bad);
    }
}

mod audio_formats {
    use super::*;

    #[test]
    fn keeps_aac_lc_audio_only_over_https_without_drc_one_per_language_default_first() {
        let mut opus = audio_fmt("251", Some("en"), "English, medium", 10.0, 160.0);
        opus["acodec"] = json!("opus");
        let mut he_aac = audio_fmt("139", Some("en"), "English, low", 10.0, 48.0);
        he_aac["acodec"] = json!("mp4a.40.5");
        let mut hls = audio_fmt("233", Some("en"), "English", 10.0, 200.0);
        hls["protocol"] = json!("m3u8_native");
        let mut muxed = audio_fmt("18", Some("en"), "English", 10.0, 300.0);
        muxed["vcodec"] = json!("avc1.42001E");
        let mut no_url = audio_fmt("140-9", Some("it"), "Italian", -1.0, 129.0);
        no_url.as_object_mut().unwrap().remove("url");
        let formats = vec![
            opus,
            he_aac,
            hls,
            muxed,
            no_url,
            audio_fmt("140-drc", Some("en"), "English (US) original (default), medium, DRC", 10.0, 999.0),
            audio_fmt("140-0", Some("en"), "English (US) original (default), medium", 10.0, 129.0),
            audio_fmt("140-5", Some("pl"), "Polish, medium", -1.0, 48.5),
            // More bits for the same language: replaces 140-5.
            audio_fmt("140-1", Some("pl"), "Polish, medium", -1.0, 129.5),
            audio_fmt("140-2", Some("de"), "German, ultralow, low, high", -1.0, 64.0),
            // No usable note: the language is the label (and sorts case-insensitively).
            audio_fmt("140-3", Some("fr"), "", -1.0, 96.0),
            // Neither note nor language.
            audio_fmt("140-4", None, "medium", -1.0, 32.0),
        ];

        let video = parse_info(&info_json(formats, json!({}))).expect("parse");

        assert_eq!(
            video.audio,
            vec![
                af("140-0", Some("en"), "English (US) original (default)", true, 129.0),
                af("140-4", None, "Default", false, 32.0),
                af("140-3", Some("fr"), "fr", false, 96.0),
                af("140-2", Some("de"), "German", false, 64.0),
                af("140-1", Some("pl"), "Polish", false, 129.5),
            ]
        );
    }

    #[test]
    fn a_note_saying_default_marks_the_default_even_with_a_low_preference() {
        let formats = vec![
            audio_fmt("140-0", Some("es"), "Spanish", 9.0, 129.0),
            audio_fmt("140-1", Some("ja"), "Japanese original (default)", -1.0, 128.0),
        ];

        let video = parse_info(&info_json(formats, json!({}))).expect("parse");

        assert_eq!(
            video.audio,
            vec![af("140-1", Some("ja"), "Japanese original (default)", true, 128.0), af("140-0", Some("es"), "Spanish", false, 129.0)]
        );
    }

    #[test]
    fn a_single_track_with_a_language_preference_of_10_is_the_default_without_a_note_saying_so() {
        let formats = vec![audio_fmt("140-1", Some("es"), "Spanish, medium", 10.0, 129.0)];

        let video = parse_info(&info_json(formats, json!({}))).expect("parse");

        assert_eq!(video.audio, vec![af("140-1", Some("es"), "Spanish", true, 129.0)]);
    }

    #[test]
    fn a_single_track_with_a_language_preference_below_10_is_not_marked_default() {
        let formats = vec![audio_fmt("140-1", Some("es"), "Spanish, medium", 9.9, 129.0)];

        let video = parse_info(&info_json(formats, json!({}))).expect("parse");

        assert_eq!(video.audio, vec![af("140-1", Some("es"), "Spanish", false, 129.0)]);
    }

    #[test]
    fn without_a_marked_default_the_most_preferred_language_becomes_it() {
        let formats = vec![
            audio_fmt("140-0", Some("cs"), "Czech", 2.0, 129.0),
            audio_fmt("140-1", Some("hu"), "Hungarian", 5.0, 130.0),
            audio_fmt("140-2", Some("bg"), "Bulgarian", 3.0, 131.0),
        ];

        let video = parse_info(&info_json(formats, json!({}))).expect("parse");

        assert_eq!(
            video.audio,
            vec![
                af("140-1", Some("hu"), "Hungarian", true, 130.0),
                af("140-2", Some("bg"), "Bulgarian", false, 131.0),
                af("140-0", Some("cs"), "Czech", false, 129.0),
            ]
        );
        assert_eq!(video.default_audio().map(|a| a.id.as_str()), Some("140-1"));
    }

    #[test]
    fn no_default_is_invented_when_no_language_is_preferred_or_there_is_one_track() {
        let unpreferred = vec![
            audio_fmt("140-0", Some("sk"), "Slovak", -1.0, 129.0),
            audio_fmt("140-1", Some("ro"), "Romanian", -1.0, 130.0),
        ];
        let single = vec![audio_fmt("140-7", Some("nl"), "Dutch", 7.0, 129.0)];

        let two = parse_info(&info_json(unpreferred, json!({}))).expect("parse");
        let one = parse_info(&info_json(single, json!({}))).expect("parse");

        assert_eq!(two.audio, vec![af("140-1", Some("ro"), "Romanian", false, 130.0), af("140-0", Some("sk"), "Slovak", false, 129.0)]);
        assert_eq!(two.default_audio().map(|a| a.id.as_str()), Some("140-1"), "the first one stands in for the default");
        assert_eq!(one.audio, vec![af("140-7", Some("nl"), "Dutch", false, 129.0)]);
    }

    #[test]
    fn a_better_copy_of_the_default_language_stays_the_default() {
        let formats = vec![
            audio_fmt("140-0", Some("en"), "English original (default), low", 10.0, 48.0),
            audio_fmt("140-1", Some("en"), "English, high", -1.0, 256.0),
            audio_fmt("140-2", Some("sv"), "Swedish", 4.0, 129.0),
        ];

        let video = parse_info(&info_json(formats, json!({}))).expect("parse");

        assert_eq!(
            video.audio,
            vec![af("140-1", Some("en"), "English", true, 256.0), af("140-2", Some("sv"), "Swedish", false, 129.0)]
        );
    }
}

fn track(ext: &str, url: &str, name: Option<&str>) -> Value {
    match name {
        Some(n) => json!({ "ext": ext, "url": url, "name": n }),
        None => json!({ "ext": ext, "url": url }),
    }
}

fn caption(code: &str, label: &str, kind: CaptionKind, url: &str) -> Caption {
    Caption { code: code.into(), label: label.into(), kind, url: url.into() }
}

mod captions {
    use super::*;

    fn by_kind(captions: &[Caption], kind: CaptionKind) -> Vec<Caption> {
        let mut out: Vec<Caption> = captions.iter().filter(|c| c.kind == kind).cloned().collect();
        out.sort_by(|a, b| a.code.cmp(&b.code));
        out
    }

    #[test]
    fn uploaded_then_automatic_original_then_translations_with_srt_preferred() {
        let extra = json!({
            "language": "en",
            "subtitles": {
                "en": [track("vtt", "https://c/en.vtt", Some("English")), track("srt", "https://c/en.srt", Some("English"))],
                "live_chat": [track("json", "https://c/chat", None)],
                "de": [track("json3", "https://c/de.json3", Some("German"))],
                "pl": [track("vtt", "https://c/pl.vtt", Some("Polish"))],
                "uk": [track("srt", "https://c/uk.srt", None)],
            },
            "automatic_captions": {
                "en-orig": [track("vtt", "https://c/en-orig.vtt", Some("English (Original)")), track("srt", "https://c/en-orig.srt", Some("English (Original)"))],
                "en": [track("vtt", "https://c/a-en.vtt", Some("English"))],
                "fr": [track("srt", "https://c/a-fr.srt", Some("French"))],
                "ab": [track("vtt", "https://c/a-ab.vtt", Some("abkhazian"))],
                "zz": [track("json3", "https://c/a-zz.json3", Some("Zz"))],
            },
        });

        let captions = parse_info(&info_json(vec![], extra)).expect("parse").captions;

        let kinds: Vec<CaptionKind> = captions.iter().map(|c| c.kind).collect();
        use CaptionKind::*;
        assert_eq!(kinds, [Manual, Manual, Manual, Auto, Translated, Translated, Translated]);
        assert_eq!(
            by_kind(&captions, Manual),
            vec![
                caption("en", "English", Manual, "https://c/en.srt"),
                caption("pl", "Polish", Manual, "https://c/pl.vtt"),
                caption("uk", "uk", Manual, "https://c/uk.srt"),
            ]
        );
        assert_eq!(
            captions[3..],
            [
                caption("en-orig", "English (auto-generated)", Auto, "https://c/en-orig.srt"),
                caption("ab", "abkhazian", Translated, "https://c/a-ab.vtt"),
                caption("en", "English", Translated, "https://c/a-en.vtt"),
                caption("fr", "French", Translated, "https://c/a-fr.srt"),
            ]
        );
    }

    #[test]
    fn without_an_orig_track_the_one_in_the_video_language_is_the_automatic_one() {
        let extra = json!({
            "language": "pl",
            "automatic_captions": {
                "en": [track("vtt", "https://c/a-en.vtt", Some("English"))],
                "pl": [track("vtt", "https://c/a-pl.vtt", Some("Polish"))],
            },
        });

        let captions = parse_info(&info_json(vec![], extra)).expect("parse").captions;

        assert_eq!(
            captions,
            vec![
                caption("pl", "Polish (auto-generated)", CaptionKind::Auto, "https://c/a-pl.vtt"),
                caption("en", "English", CaptionKind::Translated, "https://c/a-en.vtt"),
            ]
        );
    }

    #[test]
    fn without_an_orig_track_or_a_video_language_every_automatic_track_is_a_translation() {
        let extra = json!({
            "automatic_captions": {
                "it": [track("srt", "https://c/a-it.srt", Some("Italian"))],
                "es": [track("srt", "https://c/a-es.srt", Some("Spanish"))],
            },
        });

        let captions = parse_info(&info_json(vec![], extra)).expect("parse").captions;

        assert_eq!(
            captions,
            vec![
                caption("it", "Italian", CaptionKind::Translated, "https://c/a-it.srt"),
                caption("es", "Spanish", CaptionKind::Translated, "https://c/a-es.srt"),
            ]
        );
    }

    #[test]
    fn no_caption_fields_give_no_captions() {
        let captions = parse_info(&info_json(vec![], json!({}))).expect("parse").captions;

        assert_eq!(captions, Vec::<Caption>::new());
    }
}

mod video_fields {
    use super::*;

    #[test]
    fn id_title_channel_duration_headers_and_live_are_read() {
        let extra = json!({
            "is_live": true,
            "http_headers": { "User-Agent": "UA/7", "Accept-Language": "pl-PL", "X-Number": 5 },
        });

        let video = parse_info(&info_json(vec![], extra)).expect("parse");

        assert_eq!(video.id, ID);
        assert_eq!(video.title, "A title");
        assert_eq!(video.channel.as_deref(), Some("A channel"));
        assert_eq!(video.duration, Some(212.0));
        assert!(video.live);
        let mut headers = video.headers.clone();
        headers.sort();
        assert_eq!(
            headers,
            vec![("Accept-Language".to_string(), "pl-PL".to_string()), ("User-Agent".to_string(), "UA/7".to_string())],
            "non-string header values are left out"
        );
    }

    #[test]
    fn missing_fields_fall_back() {
        let json = json!({ "uploader": "An uploader", "duration": 0, "formats": [] }).to_string();

        let video = parse_info(&json).expect("parse");

        assert_eq!(video.id, "");
        assert_eq!(video.title, "YouTube video");
        assert_eq!(video.channel.as_deref(), Some("An uploader"));
        assert_eq!(video.duration, None);
        assert!(!video.live);
        assert_eq!(video.headers, Vec::<(String, String)>::new());
        assert_eq!((video.video.len(), video.audio.len(), video.captions.len()), (0, 0, 0));
    }

    #[test]
    fn unreadable_output_is_an_error() {
        let err = parse_info("ERROR: [youtube] not json").expect_err("not json");

        assert!(err.starts_with("yt-dlp gave unreadable output (") && err.ends_with(')'), "{err}");
    }
}

fn sample_video() -> Video {
    Video {
        id: ID.into(),
        title: "t".into(),
        channel: None,
        duration: Some(100.0),
        live: false,
        video: vec![vf("299", 1920, 1080, 60.0, 4000.0), vf("136", 1280, 720, 30.0, 2000.0)],
        audio: vec![af("140-0", Some("en"), "English", true, 129.0), af("140-1", Some("pl"), "Polish", false, 128.0)],
        captions: vec![],
        headers: vec![],
    }
}

mod choosing {
    use super::*;

    fn picked(video: &Video, v: Option<&str>, a: Option<&str>) -> (Option<String>, Option<String>) {
        let choice = Choice { video: v.map(str::to_string), audio: a.map(str::to_string) };
        let (pv, pa) = video.pick(&choice);
        (pv.map(|f| f.id.clone()), pa.map(|f| f.id.clone()))
    }

    #[test]
    fn pick_takes_the_named_streams_and_falls_back_to_the_defaults() {
        let video = sample_video();
        let s = |x: &str| Some(x.to_string());
        let cases = [
            ("nothing chosen", None, None, s("299"), s("140-0")),
            ("both chosen", Some("136"), Some("140-1"), s("136"), s("140-1")),
            ("only the picture chosen", Some("136"), None, s("136"), s("140-0")),
            ("only the sound chosen", None, Some("140-1"), s("299"), s("140-1")),
            ("unknown ids", Some("999"), Some("140-9"), s("299"), s("140-0")),
            ("ids swapped between kinds", Some("140-1"), Some("136"), s("299"), s("140-0")),
        ];
        let mut bad = Vec::new();
        for (case, v, a, ev, ea) in cases {
            let actual = picked(&video, v, a);
            expect!(bad, actual == (ev.clone(), ea.clone()), "{case}: {actual:?}, expected {:?}", (ev, ea));
        }
        finish(bad);
    }

    #[test]
    fn pick_on_a_video_without_streams_gives_nothing() {
        let mut video = sample_video();
        video.video.clear();
        video.audio.clear();

        assert_eq!(picked(&video, Some("136"), Some("140-1")), (None, None));
        assert_eq!(video.default_video(), None);
        assert_eq!(video.default_audio(), None);
    }

    #[test]
    fn default_audio_is_the_marked_one_wherever_it_is_else_the_first() {
        let mut video = sample_video();
        video.audio = vec![af("a", Some("de"), "German", false, 1.0), af("b", Some("it"), "Italian", true, 2.0)];
        assert_eq!(video.default_audio().map(|a| a.id.as_str()), Some("b"));

        video.audio[1].default = false;
        assert_eq!(video.default_audio().map(|a| a.id.as_str()), Some("a"));
    }

    fn captions_video() -> Video {
        let mut video = sample_video();
        video.captions = vec![
            caption("de", "German", CaptionKind::Manual, "u1"),
            caption("pl", "Polish", CaptionKind::Manual, "u2"),
            caption("en-GB", "English (UK)", CaptionKind::Manual, "u3"),
            caption("en-orig", "English (auto-generated)", CaptionKind::Auto, "u4"),
            caption("fr", "French", CaptionKind::Translated, "u5"),
        ];
        video
    }

    #[test]
    fn default_caption_prefers_uploaded_in_the_language_then_any_uploaded_then_automatic() {
        let full = captions_video();
        let mut auto_only = captions_video();
        auto_only.captions.retain(|c| c.kind != CaptionKind::Manual);
        let mut translated_only = captions_video();
        translated_only.captions.retain(|c| c.kind == CaptionKind::Translated);
        let cases = [
            ("same language", &full, Some("pl"), Some("pl")),
            ("same primary language, other region", &full, Some("en-US"), Some("en-GB")),
            ("underscore region", &full, Some("pl_PL"), Some("pl")),
            ("no uploaded track in the language", &full, Some("fr"), Some("de")),
            ("language unknown", &full, None, Some("de")),
            ("no uploaded track at all", &auto_only, Some("pl"), Some("en-orig")),
            ("only translations", &translated_only, Some("fr"), None),
        ];
        let mut bad = Vec::new();
        for (case, video, language, expected) in cases {
            let actual = video.default_caption(language).map(|c| c.code.as_str());
            expect!(bad, actual == expected, "{case}: default_caption({language:?}) = {actual:?}, expected {expected:?}");
        }
        finish(bad);
    }

    #[test]
    fn unplayable_reason_explains_live_videos_and_videos_without_usable_streams_and_lets_live_hls_play() {
        let playable = sample_video();
        let mut audio_only = sample_video();
        audio_only.video.clear();
        let mut video_only = sample_video();
        video_only.audio.clear();
        let mut nothing = sample_video();
        nothing.video.clear();
        nothing.audio.clear();
        let mut live = sample_video();
        live.live = true;
        let mut live_and_nothing = nothing.clone();
        live_and_nothing.live = true;
        // A live video's HLS qualities are its `video` formats; sound travels inside them.
        let mut live_without_hls = live.clone();
        live_without_hls.video.clear();
        let live_msg = "YouTube offers this live stream in no format playr can play";
        let none_msg = "YouTube offers no H.264 or AAC stream for this video";

        assert_eq!(unplayable_reason(&playable), None);
        assert_eq!(unplayable_reason(&audio_only), None);
        assert_eq!(unplayable_reason(&video_only), None);
        assert_eq!(unplayable_reason(&nothing).as_deref(), Some(none_msg));
        assert_eq!(unplayable_reason(&live), None);
        assert_eq!(unplayable_reason(&live_without_hls).as_deref(), Some(live_msg));
        assert_eq!(unplayable_reason(&live_and_nothing).as_deref(), Some(live_msg));
    }
}

mod search {
    use super::*;

    #[test]
    fn duration_tolerance_is_a_tenth_but_at_least_15_s() {
        let cases = [(0.0, 15.0), (100.0, 15.0), (150.0, 15.0), (151.0, 15.1), (600.0, 60.0), (3600.0, 360.0)];
        let mut bad = Vec::new();
        for (expected_len, tolerance) in cases {
            let actual = duration_tolerance(expected_len);
            expect!(bad, (actual - tolerance).abs() < 1e-9, "duration_tolerance({expected_len}) = {actual}, expected {tolerance}");
        }
        finish(bad);
    }

    #[test]
    fn pick_search_result_takes_the_first_result_of_about_the_right_length() {
        let cases: [(&str, Vec<Option<f64>>, Option<f64>, Option<usize>); 9] = [
            ("no results", vec![], Some(100.0), None),
            ("no results, length unknown", vec![], None, None),
            ("length unknown: the first", vec![Some(500.0), Some(100.0)], None, Some(0)),
            ("115 is just inside 100 +- 15", vec![Some(130.0), Some(115.0), Some(100.0)], Some(100.0), Some(1)),
            ("85 is just inside, 116 just outside", vec![Some(116.0), Some(85.0)], Some(100.0), Some(1)),
            ("a tenth of 400 is the tolerance", vec![Some(441.0), Some(360.0)], Some(400.0), Some(1)),
            ("results of unknown length are passed over", vec![None, Some(101.0)], Some(100.0), Some(1)),
            ("nothing close: the first", vec![Some(300.0), None, Some(10.0)], Some(100.0), Some(0)),
            ("several close: the first close one", vec![Some(50.0), Some(98.0), Some(100.0)], Some(100.0), Some(1)),
        ];
        let mut bad = Vec::new();
        for (case, results, expected_len, expected) in cases {
            let actual = pick_search_result(&results, expected_len);
            expect!(bad, actual == expected, "{case}: {actual:?}, expected {expected:?}");
        }
        finish(bad);
    }

    #[test]
    fn parse_search_keeps_entries_with_an_eleven_character_id() {
        let json = json!({ "entries": [
            { "id": "AAAAAAAAAA1", "duration": 245.0 },
            { "id": "short", "duration": 10.0 },
            { "id": "BBBBBBBBBB22", "duration": 11.0 },
            { "title": "no id" },
            { "id": "CCCCCCCCCC3", "duration": null },
            { "id": "DDDDDDDDDD4" },
        ]})
        .to_string();

        let results = parse_search(&json).expect("parse");

        assert_eq!(
            results,
            vec![("AAAAAAAAAA1".to_string(), Some(245.0)), ("CCCCCCCCCC3".to_string(), None), ("DDDDDDDDDD4".to_string(), None)]
        );
    }

    #[test]
    fn parse_search_without_entries_is_empty_and_garbage_is_an_error() {
        assert_eq!(parse_search("{}").expect("parse"), Vec::<(String, Option<f64>)>::new());
        let err = parse_search("<html>").expect_err("not json");
        assert!(err.starts_with("yt-dlp gave unreadable output (") && err.ends_with(')'), "{err}");
    }
}

mod ytdlp_download {
    use super::*;

    const H1: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const H2: &str = "2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a";

    #[test]
    fn expected_sha256_finds_the_line_of_the_asset() {
        let sums = format!(
            "{H1}  yt-dlp\r\n{H2} *yt-dlp.exe\r\n{}  yt-dlp_linux\n{}  yt-dlp_macos\n{}  yt-dlp_x86.exe\n{}  yt-dlp_arm64.exe\n",
            "ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789",
            &H1[..63],
            "g".repeat(64),
            format!("{H1}0"),
        );
        let cases = [
            ("plain name", "yt-dlp", Some(H1.to_string())),
            ("binary marker, CRLF line", "yt-dlp.exe", Some(H2.to_string())),
            ("upper-case hash is lower-cased", "yt-dlp_linux", Some("abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string())),
            ("hash one digit short", "yt-dlp_macos", None),
            ("hash not hex", "yt-dlp_x86.exe", None),
            ("hash one digit long", "yt-dlp_arm64.exe", None),
            ("asset not listed", "yt-dlp_linux_aarch64", None),
            ("prefix of a listed name", "yt-dlp_", None),
        ];
        let mut bad = Vec::new();
        for (case, asset, expected) in cases {
            let actual = expected_sha256(&sums, asset);
            expect!(bad, actual == expected, "{case}: expected_sha256(.., {asset:?}) = {actual:?}, expected {expected:?}");
        }
        finish(bad);
    }

    #[test]
    fn sha256_hex_matches_the_published_test_vectors() {
        assert_eq!(sha256_hex(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[test]
    fn release_asset_names_the_build_for_this_system() {
        let expected = if cfg!(all(windows, target_arch = "x86_64")) {
            Some("yt-dlp.exe")
        } else if cfg!(all(windows, target_arch = "aarch64")) {
            Some("yt-dlp_arm64.exe")
        } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            Some("yt-dlp_linux")
        } else if cfg!(target_os = "macos") {
            Some("yt-dlp_macos")
        } else {
            release_asset()
        };

        assert_eq!(release_asset(), expected);
    }
}

/// `open` against a local server that serves the DASH fixtures the way YouTube does.
mod open_streams {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use common::fixture_bytes;
    use common::range_http::{Mode, RangeServer};
    use playr::media::{AudioTrack, VideoCodec};
    use playr::youtube::open;

    use super::*;

    fn served() -> (RangeServer, Video) {
        let server = RangeServer::start(vec![
            ("/v.mp4", Mode::Ranged, fixture_bytes("dash_video.mp4")),
            ("/a.m4a", Mode::Ranged, fixture_bytes("dash_audio.m4a")),
        ]);
        let mut video = sample_video();
        video.video = vec![VideoFormat { url: server.url("/v.mp4"), ..vf("136", 160, 120, 25.0, 100.0) }];
        video.audio = vec![AudioFormat { url: server.url("/a.m4a"), ..af("140-0", Some("en"), "English", true, 96.0) }];
        video.headers = vec![("X-Origin".to_string(), "yt-7".to_string())];
        (server, video)
    }

    fn abort() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

    #[test]
    fn picture_and_sound_open_as_one_demuxer_with_the_headers_sent() {
        let (server, video) = served();

        let opened = open(&video, &Choice::default(), false, abort()).unwrap_or_else(|e| panic!("{e}"));
        let info = opened.demuxer.info();

        let v = info.video.as_ref().expect("picture");
        assert_eq!((&v.codec, v.width, v.height), (&VideoCodec::H264, 160, 120));
        assert!(matches!(&info.audio, Some(AudioTrack::Symphonia(p)) if p.sample_rate == Some(44100)), "{:?}", info.audio);
        assert_eq!(info.duration, Some(530_224.0 / 44100.0));
        assert_eq!(opened.choice, Choice { video: Some("136".into()), audio: Some("140-0".into()) });
        let requests = server.requests();
        let paths: std::collections::BTreeSet<&str> = requests.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths.into_iter().collect::<Vec<_>>(), ["/a.m4a", "/v.mp4"]);
        let without: Vec<&str> = requests.iter().filter(|r| r.header("x-origin") != Some("yt-7")).map(|r| r.path.as_str()).collect();
        assert_eq!(without, Vec::<&str>::new(), "requests without the yt-dlp headers");
    }

    #[test]
    fn sound_only_does_not_download_the_picture() {
        let (server, video) = served();

        let opened = open(&video, &Choice::default(), true, abort()).unwrap_or_else(|e| panic!("{e}"));

        assert!(opened.demuxer.info().video.is_none());
        assert!(matches!(&opened.demuxer.info().audio, Some(AudioTrack::Symphonia(p)) if p.sample_rate == Some(44100)));
        assert_eq!(opened.choice, Choice { video: None, audio: Some("140-0".into()) });
        let picture_requests = server.requests().iter().filter(|r| r.path == "/v.mp4").count();
        assert_eq!(picture_requests, 0);
    }

    #[test]
    fn sound_only_keeps_the_picture_of_a_video_without_sound() {
        let (_server, mut video) = served();
        video.audio.clear();

        let opened = open(&video, &Choice::default(), true, abort()).unwrap_or_else(|e| panic!("{e}"));

        let v = opened.demuxer.info().video.as_ref().expect("picture");
        assert_eq!((&v.codec, v.width, v.height), (&VideoCodec::H264, 160, 120));
        assert!(opened.demuxer.info().audio.is_none());
        assert_eq!(opened.choice, Choice { video: Some("136".into()), audio: None });
    }

    #[test]
    fn a_video_without_streams_fails_with_the_reason() {
        let (_server, mut video) = served();
        video.video.clear();
        video.audio.clear();

        let result = open(&video, &Choice::default(), false, abort()).err();

        assert_eq!(result.as_deref(), Some("YouTube offers no H.264 or AAC stream for this video"));
    }
}

/// Real YouTube through yt-dlp (downloads it on first use). Not run by default.
mod network {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use playr::youtube::{open, resolve};

    use super::*;

    /// "Me at the zoo", the first YouTube video: 19 s, stable for almost two decades.
    const ZOO: &str = "https://www.youtube.com/watch?v=jNQXAC9IVRw";

    fn resolved() -> Video {
        let abort = AtomicBool::new(false);
        resolve(ZOO, None, &abort, &|_| {}).expect("resolve")
    }

    #[test]
    #[ignore = "needs the internet, YouTube and yt-dlp"]
    fn resolve_finds_playable_streams() {
        let video = resolved();

        assert_eq!(video.id, "jNQXAC9IVRw");
        assert!(!video.live);
        assert_eq!(unplayable_reason(&video), None);
        assert!(!video.audio.is_empty(), "{video:?}");
    }

    #[test]
    #[ignore = "needs the internet, YouTube and yt-dlp"]
    fn open_with_and_without_the_picture() {
        let video = resolved();

        let both = open(&video, &Choice::default(), false, Arc::new(AtomicBool::new(false))).expect("open");
        let sound = open(&video, &Choice::default(), true, Arc::new(AtomicBool::new(false))).expect("open audio only");

        assert!(both.demuxer.info().video.is_some() && both.demuxer.info().audio.is_some());
        assert_eq!(both.choice.video, video.default_video().map(|f| f.id.clone()));
        assert!(sound.demuxer.info().video.is_none() && sound.demuxer.info().audio.is_some());
        assert_eq!(sound.choice, Choice { video: None, audio: video.default_audio().map(|f| f.id.clone()) });
    }
}
