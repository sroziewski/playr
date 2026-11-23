//! `playr::hls` without the network: reading master and media playlists (RFC 8216), resolving
//! the addresses they hold, choosing a variant and an audio rendition, AES-128 decryption and
//! the ID3 timestamp of packed audio segments.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::finish;
use playr::hls::{
    InitSection, Key, Location, Master, Media, Playlist, Rendition, Segment, Variant, attributes, audio_rendition,
    codecs_supported, decrypt_aes128, hevc_sps_size, id3_timestamp, is_hls, parse, pick_variant,
};

fn url(u: &str) -> Location {
    Location::Url(u.to_string())
}

fn media(text: &str, base: &Location) -> Media {
    match parse(text, base) {
        Ok(Playlist::Media(m)) => m,
        other => panic!("expected a media playlist, got {other:?}"),
    }
}

fn master(text: &str, base: &Location) -> Master {
    match parse(text, base) {
        Ok(Playlist::Master(m)) => m,
        other => panic!("expected a master playlist, got {other:?}"),
    }
}

fn segment(uri: Location, duration: f64, start: f64, sequence: u64) -> Segment {
    Segment { uri, duration, start, sequence, range: None, key: None, init: None, discontinuity: false }
}

mod attribute_lists {
    use super::*;

    #[test]
    fn keys_are_upper_cased_and_quoted_values_keep_their_commas() {
        type Pairs = Vec<(&'static str, &'static str)>;
        let cases: [(&str, &str, Pairs); 7] = [
            (
                "stream-inf",
                "BANDWIDTH=1280000,CODECS=\"avc1.4d401f,mp4a.40.2\",RESOLUTION=1280x720, name=x",
                vec![("BANDWIDTH", "1280000"), ("CODECS", "avc1.4d401f,mp4a.40.2"), ("RESOLUTION", "1280x720"), ("NAME", "x")],
            ),
            ("unquoted value trimmed", "A = 1 ,B=two", vec![("A", "1"), ("B", "two")]),
            ("empty quoted value", "URI=\"\",METHOD=NONE", vec![("URI", ""), ("METHOD", "NONE")]),
            ("unterminated quote runs to the end", "URI=\"keys/a,b", vec![("URI", "keys/a,b")]),
            ("trailing comma", "X=1,", vec![("X", "1")]),
            ("a part without = ends the list", "X=1,JUNK", vec![("X", "1")]),
            ("empty", "", vec![]),
        ];
        let mut bad = Vec::new();
        for (case, list, expected) in cases {
            let expected: Vec<(String, String)> = expected.into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
            let actual = attributes(list);
            expect!(bad, actual == expected, "{case}: {actual:?}, expected {expected:?}");
        }
        finish(bad);
    }
}

mod addresses {
    use super::*;

    #[test]
    fn a_web_playlist_resolves_relative_absolute_and_protocol_relative_addresses_against_itself() {
        let base = url("https://h.example/a/b/index.m3u8?token=abc");
        let cases = [
            ("relative", "seg1.ts", Some(url("https://h.example/a/b/seg1.ts"))),
            ("relative with a query", "seg1.ts?part=2", Some(url("https://h.example/a/b/seg1.ts?part=2"))),
            ("relative folder", "720/index.m3u8", Some(url("https://h.example/a/b/720/index.m3u8"))),
            ("root-relative", "/x/seg.ts", Some(url("https://h.example/x/seg.ts"))),
            ("protocol-relative", "//cdn.example/s.ts", Some(url("https://cdn.example/s.ts"))),
            ("absolute http", "http://other.example/s.ts", Some(url("http://other.example/s.ts"))),
            ("surrounding blanks", "  seg1.ts \t", Some(url("https://h.example/a/b/seg1.ts"))),
        ];
        let mut bad = Vec::new();
        for (case, uri, expected) in cases {
            let actual = base.resolve(uri);
            expect!(bad, actual == expected, "{case}: resolve({uri:?}) = {actual:?}, expected {expected:?}");
        }
        finish(bad);
    }

    #[test]
    fn a_web_playlist_cannot_point_at_files_or_other_schemes() {
        let base = url("https://h.example/a/index.m3u8");
        let cases = [
            ("file URL", "file:///C:/Windows/win.ini"),
            ("file URL to a share", "file://server/share/x.ts"),
            ("FairPlay key", "skd://key-id"),
            ("data URL", "data:text/plain;base64,AAAA"),
            ("ftp", "ftp://ftp.example/x.ts"),
            ("javascript", "javascript:alert(1)"),
            ("empty", ""),
            ("blank", "   "),
        ];
        let mut bad = Vec::new();
        for (case, uri) in cases {
            let actual = base.resolve(uri);
            expect!(bad, actual.is_none(), "{case}: resolve({uri:?}) = {actual:?}, expected None");
        }
        // A UNC path is a relative web path: its backslashes are percent-encoded (%5C) and it
        // is joined to the playlist's folder, never opened as a file.
        let unc = base.resolve(r"\\server\share\x.ts");
        expect!(bad, unc == Some(url("https://h.example/a/%5C%5Cserver%5Cshare%5Cx.ts")), "UNC path: {unc:?}");
        finish(bad);
    }

    #[test]
    fn a_playlist_on_disk_resolves_relative_paths_against_its_folder_and_keeps_web_addresses() {
        let folder = Path::new(env!("CARGO_TARGET_TMPDIR")).join("hls-dir");
        let base = Location::File(folder.join("index.m3u8"));
        let mut cases = vec![
            ("relative", "seg1.ts", Some(Location::File(folder.join("seg1.ts")))),
            ("subfolder, query cut", "sub/seg1.ts?x=1", Some(Location::File(folder.join("sub/seg1.ts")))),
            ("fragment cut", "seg2.ts#t=3", Some(Location::File(folder.join("seg2.ts")))),
            ("web address", "https://cdn.example/s.ts", Some(url("https://cdn.example/s.ts"))),
            ("other scheme", "ftp://ftp.example/x.ts", None),
            ("empty", "", None),
        ];
        if cfg!(windows) {
            cases.push(("absolute path with a drive", r"C:\media\s.ts", Some(Location::File(PathBuf::from(r"C:\media\s.ts")))));
        }
        let mut bad = Vec::new();
        for (case, uri, expected) in cases {
            let actual = base.resolve(uri);
            expect!(bad, actual == expected, "{case}: resolve({uri:?}) = {actual:?}, expected {expected:?}");
        }
        finish(bad);
    }
}

mod recognising {
    use super::*;

    #[test]
    fn hls_is_told_from_a_plain_list_of_addresses_by_its_tags() {
        let cases = [
            ("target duration", "#EXTM3U\n#EXT-X-TARGETDURATION:6\n", true),
            ("variant", "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nlow.m3u8\n", true),
            ("media sequence", "#EXT-X-MEDIA-SEQUENCE:3\n", true),
            ("map", "#EXT-X-MAP:URI=\"init.mp4\"\n", true),
            ("endlist", "#EXT-X-ENDLIST\n", true),
            ("plain m3u", "#EXTM3U\n#EXTINF:-1,Radio\nhttp://radio.example/stream\n", false),
            ("bare address", "http://radio.example/stream\n", false),
            ("pls", "[playlist]\nFile1=http://radio.example/stream\n", false),
            ("empty", "", false),
        ];
        let mut bad = Vec::new();
        for (case, text, expected) in cases {
            expect!(bad, is_hls(text) == expected, "{case}: is_hls = {}", !expected);
        }
        finish(bad);
    }

    #[test]
    fn text_that_is_not_hls_is_refused() {
        let base = url("https://h.example/x.m3u8");

        assert_eq!(parse("http://radio.example/stream\n", &base), Err("not an HLS playlist".to_string()));
        assert_eq!(parse("<html></html>", &base), Err("not an HLS playlist".to_string()));
    }

    #[test]
    fn a_byte_order_mark_before_the_header_is_ignored() {
        let base = url("https://h.example/x/index.m3u8");

        let parsed = media("\u{feff}#EXTM3U\n#EXTINF:2.5,\na.ts\n", &base);

        assert_eq!(parsed.segments, vec![segment(url("https://h.example/x/a.ts"), 2.5, 0.0, 0)]);
    }
}

mod media_playlists {
    use super::*;

    #[test]
    fn segments_get_their_durations_starts_sequence_numbers_ranges_and_discontinuities() {
        let base = url("https://h.example/path/to/index.m3u8?token=abc");
        let text = "#EXTM3U\n\
                    #EXT-X-VERSION:4\n\
                    #EXT-X-TARGETDURATION:6\n\
                    #EXT-X-MEDIA-SEQUENCE:120\n\
                    #EXTINF:5.5,first title, with a comma\n\
                    seg120.ts\n\
                    #EXTINF:6.0,\n\
                    #EXT-X-BYTERANGE:1000@200\n\
                    all.ts\n\
                    #EXTINF:4.25\n\
                    #EXT-X-BYTERANGE:500\n\
                    all.ts\n\
                    #EXT-X-DISCONTINUITY\n\
                    #EXTINF:3,\n\
                    https://other.example/abs.ts?x=1\n\
                    #EXTINF:2.0,\n\
                    //cdn.example/proto.ts\n\
                    \n\
                    # a comment\n\
                    #EXTINF:1.5,\n\
                    /root/seg.ts\n\
                    #EXT-X-ENDLIST\n";

        let parsed = media(text, &base);

        let all = url("https://h.example/path/to/all.ts");
        let expected = Media {
            target_duration: 6.0,
            first_sequence: 120,
            segments: vec![
                segment(url("https://h.example/path/to/seg120.ts"), 5.5, 0.0, 120),
                Segment { range: Some((200, 1000)), ..segment(all.clone(), 6.0, 5.5, 121) },
                // Without an offset the range follows on from the previous one of the same file.
                Segment { range: Some((1200, 500)), ..segment(all, 4.25, 11.5, 122) },
                Segment { discontinuity: true, ..segment(url("https://other.example/abs.ts?x=1"), 3.0, 15.75, 123) },
                segment(url("https://cdn.example/proto.ts"), 2.0, 18.75, 124),
                segment(url("https://h.example/root/seg.ts"), 1.5, 20.75, 125),
            ],
            ended: true,
        };
        assert_eq!(parsed, expected);
        assert_eq!(parsed.duration(), 22.25);
    }

    #[test]
    fn a_byte_range_without_an_offset_after_another_file_starts_at_the_beginning() {
        let base = url("https://h.example/v/index.m3u8");
        let text = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n\
                    #EXTINF:4,\n#EXT-X-BYTERANGE:700@50\na.ts\n\
                    #EXTINF:4,\n#EXT-X-BYTERANGE:300\nb.ts\n";

        let parsed = media(text, &base);

        let ranges: Vec<Option<(u64, u64)>> = parsed.segments.iter().map(|s| s.range).collect();
        assert_eq!(ranges, [Some((50, 700)), Some((0, 300))]);
    }

    #[test]
    fn a_byte_range_whose_end_does_not_fit_in_64_bits_is_ignored() {
        let base = url("https://h.example/v/index.m3u8");
        let max = u64::MAX;
        let text = format!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MAP:URI=\"init.mp4\",BYTERANGE=\"16@{max}\"\n\
             #EXTINF:4,\n#EXT-X-BYTERANGE:10@{}\na.m4s\n\
             #EXTINF:4,\n#EXT-X-BYTERANGE:{max}@1\nb.m4s\n\
             #EXTINF:4,\n#EXT-X-BYTERANGE:{max}@0\nc.m4s\n",
            max - 9
        );

        let parsed = media(&text, &base);

        let ranges: Vec<Option<(u64, u64)>> = parsed.segments.iter().map(|s| s.range).collect();
        let init = parsed.segments[0].init.as_deref().cloned();
        // max - 9 + 10 = max + 1 overflows; max + 1 overflows; 0 + max fits.
        assert_eq!(ranges, [None, None, Some((0, max))]);
        assert_eq!(init, Some(InitSection { uri: url("https://h.example/v/init.mp4"), range: None }));
    }

    #[test]
    fn a_segment_whose_address_is_refused_is_left_out_without_its_tags_leaking_onto_the_next() {
        let base = url("https://h.example/v/index.m3u8");
        let text = "#EXTM3U\n#EXT-X-TARGETDURATION:8\n#EXT-X-MEDIA-SEQUENCE:5\n\
                    #EXTINF:2,\n#EXT-X-BYTERANGE:10@0\nfile:///etc/passwd\n\
                    #EXTINF:3,\nok.ts\n";

        let parsed = media(text, &base);

        type UriDurationRange = (Location, f64, Option<(u64, u64)>);
        let got: Vec<UriDurationRange> = parsed.segments.iter().map(|s| (s.uri.clone(), s.duration, s.range)).collect();
        assert_eq!(got, vec![(url("https://h.example/v/ok.ts"), 3.0, None)]);
    }

    #[test]
    fn a_left_out_segment_still_counts_towards_the_sequence_numbers_and_start_times_after_it() {
        let base = url("https://h.example/v/index.m3u8");
        let text = "#EXTM3U\n#EXT-X-TARGETDURATION:8\n#EXT-X-MEDIA-SEQUENCE:5\n\
                    #EXTINF:1.5,\nfirst.ts\n\
                    #EXTINF:2,\nfile:///etc/passwd\n\
                    #EXTINF:3,\nok.ts\n\
                    #EXTINF:4,\nlast.ts\n";

        let parsed = media(text, &base);

        // RFC 8216 6.3.2/4.3.3.2: a segment's number is EXT-X-MEDIA-SEQUENCE plus the number of
        // segments before it in the playlist, so ok.ts is 7 (its AES IV and its identity when a
        // live playlist is reloaded depend on it); its start still counts the 2 s left out.
        let got: Vec<(u64, f64)> = parsed.segments.iter().map(|s| (s.sequence, s.start)).collect();
        assert_eq!(got, [(5, 0.0), (7, 3.5), (8, 6.5)]);
    }

    #[test]
    fn a_missing_or_unreadable_duration_falls_back_to_the_target_duration() {
        let base = url("https://h.example/v/index.m3u8");
        let text = "#EXTM3U\n#EXT-X-TARGETDURATION:7\n\
                    a.ts\n\
                    #EXTINF:-1,\nb.ts\n\
                    #EXTINF:abc,\nc.ts\n\
                    #EXTINF:0.5,\nd.ts\n";

        let parsed = media(text, &base);

        let durations: Vec<f64> = parsed.segments.iter().map(|s| s.duration).collect();
        let starts: Vec<f64> = parsed.segments.iter().map(|s| s.start).collect();
        assert_eq!((durations, starts, parsed.ended), (vec![7.0, 7.0, 7.0, 0.5], vec![0.0, 7.0, 14.0, 21.0], false));
    }

    #[test]
    fn without_a_target_duration_the_longest_segment_is_used_but_at_least_one_second() {
        let base = url("https://h.example/v/index.m3u8");
        let cases = [
            ("short segments", "#EXTM3U\n#EXTINF:0.5,\na.ts\n#EXTINF:0.25,\nb.ts\n", 1.0),
            ("longest wins", "#EXTM3U\n#EXTINF:2.5,\na.ts\n#EXTINF:7.25,\nb.ts\n#EXTINF:3,\nc.ts\n", 7.25),
            ("no segments", "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:9\n", 1.0),
            ("unreadable value", "#EXTM3U\n#EXT-X-TARGETDURATION:x\n#EXTINF:4,\na.ts\n", 4.0),
        ];
        let mut bad = Vec::new();
        for (case, text, expected) in cases {
            let actual = media(text, &base).target_duration;
            expect!(bad, actual == expected, "{case}: {actual}, expected {expected}");
        }
        finish(bad);
    }

    #[test]
    fn keys_and_init_sections_apply_to_the_segments_after_them_until_replaced() {
        let base = url("https://h.example/live/index.m3u8");
        let text = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:3\n\
                    #EXT-X-KEY:METHOD=AES-128,URI=\"keys/k1.bin?a=1,b=2\",IV=0X0A0B\n\
                    #EXTINF:4,\ns3.ts\n\
                    #EXTINF:4,\ns4.ts\n\
                    #EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://drm\",KEYFORMAT=\"com.apple.streamingkeydelivery\"\n\
                    #EXTINF:4,\ns5.ts\n\
                    #EXT-X-KEY:METHOD=NONE\n\
                    #EXTINF:4,\ns6.ts\n\
                    #EXT-X-KEY:METHOD=aes-128,URI=\"https://keys.example/k2\"\n\
                    #EXT-X-MAP:URI=\"init.mp4\",BYTERANGE=\"720@16\"\n\
                    #EXTINF:4,\ns7.m4s\n\
                    #EXT-X-MAP:URI=\"init2.mp4\"\n\
                    #EXTINF:4,\ns8.m4s\n";

        let parsed = media(text, &base);

        let mut iv = [0u8; 16];
        iv[14..].copy_from_slice(&[0x0a, 0x0b]);
        let k1 = Key { method: "AES-128".into(), uri: Some(url("https://h.example/live/keys/k1.bin?a=1,b=2")), iv: Some(iv) };
        let sample_aes = Key { method: "SAMPLE-AES".into(), uri: None, iv: None };
        let k2 = Key { method: "AES-128".into(), uri: Some(url("https://keys.example/k2")), iv: None };
        let init1 = InitSection { uri: url("https://h.example/live/init.mp4"), range: Some((16, 720)) };
        let init2 = InitSection { uri: url("https://h.example/live/init2.mp4"), range: None };
        let got: Vec<(u64, Option<Key>, Option<InitSection>)> = parsed
            .segments
            .iter()
            .map(|s| (s.sequence, s.key.as_deref().cloned(), s.init.as_deref().cloned()))
            .collect();
        assert_eq!(
            got,
            vec![
                (3, Some(k1.clone()), None),
                (4, Some(k1), None),
                (5, Some(sample_aes), None),
                (6, None, None),
                (7, Some(k2.clone()), Some(init1)),
                (8, Some(k2), Some(init2)),
            ]
        );
        assert!(!parsed.ended, "no EXT-X-ENDLIST: a live playlist");
        assert!(Arc::ptr_eq(parsed.segments[0].key.as_ref().unwrap(), parsed.segments[1].key.as_ref().unwrap()), "one key shared");
    }

    #[test]
    fn an_iv_is_read_as_up_to_32_hex_digits_after_0x() {
        let base = url("https://h.example/v/index.m3u8");
        let full: [u8; 16] = [0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f];
        let mut short = [0u8; 16];
        short[15] = 0x07;
        let cases = [
            ("32 digits", "0x000102030405060708090a0b0c0d0e0f", Some(full)),
            ("upper-case digits and prefix", "0X000102030405060708090A0B0C0D0E0F", Some(full)),
            ("short value is right-aligned", "0x7", Some(short)),
            ("33 digits", "0x0000102030405060708090a0b0c0d0e0f", None),
            ("not hex", "0x00zz", None),
            ("no prefix", "000102030405060708090a0b0c0d0e0f", None),
            ("prefix only", "0x", None),
        ];
        let mut bad = Vec::new();
        for (case, value, expected) in cases {
            let text = format!("#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-KEY:METHOD=AES-128,URI=\"k\",IV={value}\n#EXTINF:2,\na.ts\n");
            let actual = media(&text, &base).segments[0].key.as_ref().and_then(|k| k.iv);
            expect!(bad, actual == expected, "{case}: {actual:?}, expected {expected:?}");
        }
        finish(bad);
    }

    #[test]
    fn segment_at_finds_the_last_segment_starting_at_or_before_the_time() {
        let base = url("https://h.example/v/index.m3u8");
        let parsed = media("#EXTM3U\n#EXTINF:5.5,\na.ts\n#EXTINF:6,\nb.ts\n#EXTINF:4.25,\nc.ts\n#EXT-X-ENDLIST\n", &base);
        let cases = [
            ("start", 0.0, 0),
            ("inside the first", 5.49, 0),
            ("exactly at the second", 5.5, 1),
            ("a rounding error before the second", 5.5 - 1e-7, 1),
            ("inside the third", 12.0, 2),
            ("past the end", 100.0, 2),
            ("before the start", -1.0, 0),
        ];
        let mut bad = Vec::new();
        for (case, secs, expected) in cases {
            let actual = parsed.segment_at(secs);
            expect!(bad, actual == expected, "{case}: segment_at({secs}) = {actual}, expected {expected}");
        }
        expect!(bad, parsed.duration() == 15.75, "duration {}", parsed.duration());
        finish(bad);
    }
}

mod master_playlists {
    use super::*;

    #[test]
    fn variants_and_audio_renditions_are_read_with_their_attributes() {
        let base = url("https://h.example/tv/master.m3u8");
        let text = "#EXTM3U\n\
            #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aac\",NAME=\"English\",LANGUAGE=\"en\",DEFAULT=YES,AUTOSELECT=YES,URI=\"audio/en.m3u8\"\n\
            #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aac\",NAME=\"Polski, lektor\",LANGUAGE=\"pl\",AUTOSELECT=yes,URI=\"audio/pl.m3u8\"\n\
            #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"muxed\",NAME=\"Main\",DEFAULT=YES\n\
            #EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"English\",URI=\"subs/en.m3u8\"\n\
            #EXT-X-STREAM-INF:BANDWIDTH=2500000,RESOLUTION=1280x720,CODECS=\"avc1.4d401f,mp4a.40.2\",AUDIO=\"aac\"\n\
            720/index.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=800000,CODECS=\"mp4a.40.2\"\n\
            audio-only.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=900000,RESOLUTION=bad\n\
            file:///etc/passwd\n\
            #EXT-X-STREAM-INF:BANDWIDTH=5000000,RESOLUTION=1920X1080\n\
            https://cdn2.example/1080.m3u8\n\
            stray.m3u8\n";

        let parsed = master(text, &base);

        let variant = |uri: &str, bandwidth, size: Option<(u32, u32)>, codecs: Option<&str>, audio: Option<&str>| Variant {
            uri: url(uri),
            bandwidth,
            width: size.map(|s| s.0),
            height: size.map(|s| s.1),
            codecs: codecs.map(str::to_string),
            audio: audio.map(str::to_string),
        };
        let expected = Master {
            variants: vec![
                variant("https://h.example/tv/720/index.m3u8", 2_500_000, Some((1280, 720)), Some("avc1.4d401f,mp4a.40.2"), Some("aac")),
                variant("https://h.example/tv/audio-only.m3u8", 800_000, None, Some("mp4a.40.2"), None),
                variant("https://cdn2.example/1080.m3u8", 5_000_000, Some((1920, 1080)), None, None),
            ],
            audio: vec![
                Rendition {
                    group: "aac".into(),
                    name: "English".into(),
                    language: Some("en".into()),
                    uri: Some(url("https://h.example/tv/audio/en.m3u8")),
                    default: true,
                    autoselect: true,
                },
                Rendition {
                    group: "aac".into(),
                    name: "Polski, lektor".into(),
                    language: Some("pl".into()),
                    uri: Some(url("https://h.example/tv/audio/pl.m3u8")),
                    default: false,
                    autoselect: true,
                },
                Rendition { group: "muxed".into(), name: "Main".into(), language: None, uri: None, default: true, autoselect: false },
            ],
        };
        assert_eq!(parsed, expected);
    }
}

fn v(name: &str, bandwidth: u64, height: Option<u32>, codecs: Option<&str>) -> Variant {
    Variant {
        uri: url(&format!("https://h.example/{name}.m3u8")),
        bandwidth,
        width: height.map(|h| h * 16 / 9),
        height,
        codecs: codecs.map(str::to_string),
        audio: None,
    }
}

const AV: Option<&str> = Some("avc1.64001f,mp4a.40.2");

mod choosing {
    use super::*;

    #[test]
    fn codecs_are_playable_only_when_every_one_is_h264_hevc_aac_mp3_or_a_caption_format() {
        let cases = [
            ("not named", None, true),
            ("H.264 + AAC", Some("avc1.640028,mp4a.40.2"), true),
            ("H.264 avc3", Some("avc3.640028"), true),
            ("HEVC hvc1 + HE-AAC", Some("hvc1.1.6.L93.B0,mp4a.40.5"), true),
            ("HEVC hev1", Some("hev1.1.6.L93.B0"), true),
            ("MP3 in mp4a", Some("mp4a.69"), true),
            ("MPEG-1 audio in mp4a", Some("mp4a.6B"), true),
            ("upper case and blanks", Some(" AVC1.640028 , MP4A.40.2 "), true),
            ("WebVTT captions", Some("avc1.4d401f,mp4a.40.2,wvtt"), true),
            ("TTML captions", Some("stpp.ttml.im1t"), true),
            ("tx3g captions", Some("tx3g"), true),
            ("empty", Some(""), true),
            ("AC-3 in mp4a", Some("avc1.640028,mp4a.a5"), false),
            ("E-AC-3 in mp4a", Some("mp4a.A6"), false),
            ("AC-3", Some("avc1.640028,ac-3"), false),
            ("E-AC-3", Some("ec-3"), false),
            ("VP9", Some("vp09.00.40.08,mp4a.40.2"), false),
            ("AV1", Some("av01.0.08M.08"), false),
            ("Opus", Some("avc1.640028,opus"), false),
            ("FLAC", Some("fLaC"), false),
        ];
        let mut bad = Vec::new();
        for (case, codecs, expected) in cases {
            let actual = codecs_supported(codecs);
            expect!(bad, actual == expected, "{case}: codecs_supported({codecs:?}) = {actual}");
        }
        finish(bad);
    }

    #[test]
    fn the_variant_is_the_highest_bandwidth_with_playable_codecs_no_taller_than_the_limit() {
        let ladder = Master {
            variants: vec![
                v("240", 300_000, Some(240), AV),
                v("1080", 5_000_000, Some(1080), AV),
                v("2160", 15_000_000, Some(2160), AV),
                v("1080-vp9", 6_000_000, Some(1080), Some("vp09.00.40.08,mp4a.40.2")),
                v("720", 2_500_000, Some(720), AV),
                v("sound", 128_000, None, Some("mp4a.40.2")),
            ],
            audio: vec![],
        };
        let unknown_height = Master { variants: vec![v("1080", 5_000_000, Some(1080), AV), v("unknown", 9_000_000, None, AV)], audio: vec![] };
        let unnamed_codecs = Master { variants: vec![v("720", 2_500_000, Some(720), AV), v("unnamed", 3_000_000, Some(480), None)], audio: vec![] };
        let all_tall = Master { variants: vec![v("2160", 15_000_000, Some(2160), AV), v("1440", 8_000_000, Some(1440), AV)], audio: vec![] };
        let sound_richer = Master { variants: vec![v("sound", 900_000, None, Some("mp4a.40.2")), v("240", 300_000, Some(240), AV)], audio: vec![] };
        let nothing_playable = Master { variants: vec![v("vp9", 1_000_000, Some(720), Some("vp09.00.40.08"))], audio: vec![] };
        let empty = Master { variants: vec![], audio: vec![] };
        let cases = [
            ("1080p limit", &ladder, 1080, Some("1080")),
            ("720p limit", &ladder, 720, Some("720")),
            ("an unknown height counts as fitting", &unknown_height, 1080, Some("unknown")),
            ("unnamed codecs are given the benefit of the doubt", &unnamed_codecs, 1080, Some("unnamed")),
            ("all taller than the limit: the shortest", &all_tall, 1080, Some("1440")),
            ("a picture is preferred to a richer sound-only variant", &sound_richer, 1080, Some("240")),
            ("no playable codecs", &nothing_playable, 1080, None),
            ("no variants", &empty, 1080, None),
        ];
        let mut bad = Vec::new();
        for (case, m, limit, expected) in cases {
            let actual = pick_variant(m, limit, false).map(|v| v.uri.clone());
            let expected = expected.map(|n| url(&format!("https://h.example/{n}.m3u8")));
            expect!(bad, actual == expected, "{case}: {actual:?}, expected {expected:?}");
        }
        finish(bad);
    }

    #[test]
    fn for_sound_only_the_richest_sound_only_variant_is_taken_else_the_cheapest_playable_one() {
        let with_sound = Master {
            variants: vec![
                v("720", 2_500_000, Some(720), AV),
                v("sound-low", 64_000, None, Some("mp4a.40.5")),
                v("sound-high", 160_000, None, Some("mp4a.40.2")),
                v("sound-ac3", 384_000, None, Some("ac-3")),
            ],
            audio: vec![],
        };
        let pictures_only = Master {
            variants: vec![
                v("720", 2_500_000, Some(720), AV),
                v("vp9-low", 100_000, Some(144), Some("vp09.00.10.08")),
                v("240", 300_000, Some(240), AV),
                v("2160", 15_000_000, Some(2160), AV),
            ],
            audio: vec![],
        };
        let cases = [("sound-only variants", &with_sound, Some("sound-high")), ("pictures only", &pictures_only, Some("240"))];
        let mut bad = Vec::new();
        for (case, m, expected) in cases {
            let actual = pick_variant(m, 1080, true).map(|v| v.uri.clone());
            let expected = expected.map(|n| url(&format!("https://h.example/{n}.m3u8")));
            expect!(bad, actual == expected, "{case}: {actual:?}, expected {expected:?}");
        }
        finish(bad);
    }

    fn rendition(group: &str, name: &str, uri: bool, default: bool, autoselect: bool) -> Rendition {
        Rendition {
            group: group.into(),
            name: name.into(),
            language: None,
            uri: uri.then(|| url(&format!("https://h.example/audio/{name}.m3u8"))),
            default,
            autoselect,
        }
    }

    #[test]
    fn the_audio_rendition_is_the_groups_default_else_autoselect_else_first_and_only_with_an_address() {
        let in_group = |group: Option<&str>| Variant { audio: group.map(str::to_string), ..v("720", 1, Some(720), AV) };
        let cases = [
            (
                "default wins over autoselect and order",
                vec![rendition("a", "first", true, false, false), rendition("a", "auto", true, false, true), rendition("a", "def", true, true, false)],
                Some("a"),
                Some("def"),
            ),
            (
                "autoselect wins over order",
                vec![rendition("a", "first", true, false, false), rendition("a", "auto", true, false, true)],
                Some("a"),
                Some("auto"),
            ),
            ("first when nothing is flagged", vec![rendition("a", "first", true, false, false), rendition("a", "second", true, false, false)], Some("a"), Some("first")),
            (
                "another group's default is not taken",
                vec![rendition("other", "def", true, true, true), rendition("a", "mine", true, false, false)],
                Some("a"),
                Some("mine"),
            ),
            (
                "the picked rendition travels in the variant (no address)",
                vec![rendition("a", "def", false, true, false), rendition("a", "alt", true, false, false)],
                Some("a"),
                None,
            ),
            ("no rendition in the group", vec![rendition("other", "x", true, true, true)], Some("a"), None),
            ("variant names no group", vec![rendition("a", "def", true, true, true)], None, None),
        ];
        let mut bad = Vec::new();
        for (case, audio, group, expected) in cases {
            let m = Master { variants: vec![], audio };
            let actual = audio_rendition(&m, &in_group(group)).map(|r| r.name.clone());
            expect!(bad, actual.as_deref() == expected, "{case}: {actual:?}, expected {expected:?}");
        }
        finish(bad);
    }
}

fn hex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

/// The FIPS-197 / SP 800-38A example key and IV.
const KEY: [u8; 16] = [0x2b, 0x7e, 0x15, 0x16, 0x28, 0xae, 0xd2, 0xa6, 0xab, 0xf7, 0x15, 0x88, 0x09, 0xcf, 0x4f, 0x3c];
const IV: [u8; 16] = [0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f];

mod decryption {
    use super::*;

    // Ciphertexts from `openssl enc -aes-128-cbc -K <KEY> -iv <IV>` (PKCS#7 padding). The first
    // block of the first one is the SP 800-38A F.2.1 vector 7649abac8119b246cee98e9b12e9197d.
    const SP800_BLOCK_AND_TAIL: &str = "7649abac8119b246cee98e9b12e9197d1c1c38cc130203da44e6b9bd7b411c83";
    const SIXTEEN_BYTES: &str = "64768548007aef9f3d258e5c34cdc21bde0a1268436e159434fc21de3696d928";

    #[test]
    fn aes_128_cbc_with_pkcs7_padding_is_undone() {
        let mut first = hex("6bc1bee22e409f96e93d7e117393172a");
        first.extend_from_slice(b"playr HLS");
        let cases = [("a block and a 9-byte tail", SP800_BLOCK_AND_TAIL, first), ("16 bytes and a whole padding block", SIXTEEN_BYTES, b"0123456789abcdef".to_vec())];
        let mut bad = Vec::new();
        for (case, cipher, plain) in cases {
            let actual = decrypt_aes128(&hex(cipher), &KEY, &IV);
            expect!(bad, actual == Ok(plain.clone()), "{case}: {actual:?}");
        }
        finish(bad);
    }

    #[test]
    fn the_iv_is_xored_into_the_first_block_only() {
        let zero_iv = [0u8; 16];

        let actual = decrypt_aes128(&hex(SIXTEEN_BYTES), &KEY, &zero_iv);

        // CBC: P1 = D(C1) xor IV, so with a zero IV the first block comes out xored with IV.
        let expected: Vec<u8> = b"0123456789abcdef".iter().zip(IV).map(|(p, i)| p ^ i).collect();
        assert_eq!(actual, Ok(expected));
    }

    #[test]
    fn data_that_is_not_whole_blocks_or_does_not_unpad_is_an_error() {
        let not_blocks = "an encrypted segment is not a whole number of AES blocks";
        let wrong_key = "an encrypted segment did not decrypt (wrong key?)";
        let mut other_key = KEY;
        other_key[0] ^= 1;
        let cases = [
            ("empty", Vec::new(), KEY, not_blocks),
            ("17 bytes", hex(SIXTEEN_BYTES)[..17].to_vec(), KEY, not_blocks),
            ("31 bytes", hex(SIXTEEN_BYTES)[..31].to_vec(), KEY, not_blocks),
            ("wrong key", hex(SIXTEEN_BYTES), other_key, wrong_key),
            // Without the padding block the last plaintext byte is 'f' (0x66), not a pad length.
            ("padding block cut off", hex(SIXTEEN_BYTES)[..16].to_vec(), KEY, wrong_key),
        ];
        let mut bad = Vec::new();
        for (case, data, key, expected) in cases {
            let actual = decrypt_aes128(&data, &key, &IV);
            expect!(bad, actual == Err(expected.to_string()), "{case}: {actual:?}");
        }
        finish(bad);
    }
}

/// An ID3v2.4 tag holding one PRIV frame for `owner` with `data`, its size written syncsafe.
pub fn id3_priv(owner: &str, data: &[u8]) -> Vec<u8> {
    let mut frame = Vec::new();
    frame.extend_from_slice(b"PRIV");
    let body = owner.len() + 1 + data.len();
    frame.extend_from_slice(&(body as u32).to_be_bytes());
    frame.extend_from_slice(&[0, 0]);
    frame.extend_from_slice(owner.as_bytes());
    frame.push(0);
    frame.extend_from_slice(data);
    let size = frame.len();
    let mut tag = b"ID3\x04\x00\x00".to_vec();
    tag.extend_from_slice(&[(size >> 21) as u8 & 0x7f, (size >> 14) as u8 & 0x7f, (size >> 7) as u8 & 0x7f, size as u8 & 0x7f]);
    tag.extend_from_slice(&frame);
    tag
}

mod packed_audio {
    use super::*;

    const OWNER: &str = "com.apple.streaming.transportStreamTimestamp";

    #[test]
    fn the_transport_stream_timestamp_is_read_from_the_id3_tag_masked_to_33_bits() {
        let stamp = 900_000u64 + 12_345;
        let tag = id3_priv(OWNER, &stamp.to_be_bytes());
        let mut with_audio = tag.clone();
        with_audio.extend_from_slice(&[0xFF, 0xF1, 0x50, 0x80]);
        let wide = id3_priv(OWNER, &((1u64 << 33) + 77).to_be_bytes());
        let other_owner = id3_priv("com.example.other", &stamp.to_be_bytes());
        let cut_short = tag[..tag.len() - 3].to_vec();
        let cases = [
            ("tag then audio", with_audio, (Some(stamp), tag.len())),
            ("bits above 33 dropped", wide.clone(), (Some(77), wide.len())),
            ("another PRIV owner", other_owner.clone(), (None, other_owner.len())),
            ("timestamp cut off", cut_short.clone(), (None, cut_short.len())),
            ("no tag", vec![0xFF, 0xF1, 0x50, 0x80, 0, 0, 0, 0, 0, 0, 0], (None, 0)),
            ("too short for a header", b"ID3\x04\x00".to_vec(), (None, 0)),
        ];
        let mut bad = Vec::new();
        for (case, data, expected) in cases {
            let actual = id3_timestamp(&data);
            expect!(bad, actual == expected, "{case}: {actual:?}, expected {expected:?}");
        }
        finish(bad);
    }

    #[test]
    fn the_tag_length_is_read_as_a_syncsafe_integer() {
        // 0x00 0x00 0x02 0x01 is 2 * 128 + 1 = 257 bytes after the 10-byte header.
        let mut data = b"ID3\x04\x00\x00\x00\x00\x02\x01".to_vec();
        data.resize(10 + 257 + 5, 0x55);

        assert_eq!(id3_timestamp(&data), (None, 267));
    }
}

mod hevc_sps {
    use super::*;

    #[derive(Default)]
    struct BitWriter {
        bytes: Vec<u8>,
        bits: usize,
    }

    impl BitWriter {
        fn put(&mut self, value: u64, n: usize) {
            for i in (0..n).rev() {
                if self.bits.is_multiple_of(8) {
                    self.bytes.push(0);
                }
                let bit = ((value >> i) & 1) as u8;
                *self.bytes.last_mut().unwrap() |= bit << (7 - self.bits % 8);
                self.bits += 1;
            }
        }

        /// Exp-Golomb ue(v).
        fn ue(&mut self, v: u32) {
            let x = u64::from(v) + 1;
            let len = 64 - x.leading_zeros() as usize;
            self.put(0, len - 1);
            self.put(x, len);
        }
    }

    /// Sub-layer `i` carries (profile present, level present).
    struct Sps {
        sub_layers: Vec<(bool, bool)>,
        chroma: u32,
        size: (u32, u32),
        /// (left, right, top, bottom) in chroma units.
        window: Option<(u32, u32, u32, u32)>,
    }

    /// The SPS payload after the 2-byte NAL header, laid out as ITU-T H.265 7.3.2.2 / 7.3.3,
    /// with distinct non-zero filler in the fields the size does not depend on.
    fn sps(s: &Sps) -> Vec<u8> {
        let mut w = BitWriter::default();
        w.put(0b0101, 4); // sps_video_parameter_set_id
        w.put(s.sub_layers.len() as u64, 3); // sps_max_sub_layers_minus1
        w.put(1, 1); // sps_temporal_id_nesting_flag
        w.put(0b0000_0001, 8); // profile space 0, tier 0, profile idc 1 (Main)
        w.put(0x6000_0000, 32); // compatibility flags
        w.put(0b1011, 4); // progressive, interlaced, non-packed, frame-only
        w.put(0, 44); // reserved bits and the inbld flag
        w.put(93, 8); // general_level_idc
        for &(profile, level) in &s.sub_layers {
            w.put(u64::from(profile), 1);
            w.put(u64::from(level), 1);
        }
        if !s.sub_layers.is_empty() {
            w.put(0, 2 * (8 - s.sub_layers.len()));
        }
        for &(profile, level) in &s.sub_layers {
            if profile {
                w.put(0x0123_4567_89AB, 48);
                w.put(0x00CD_EF01_2345, 40);
            }
            if level {
                w.put(0x7B, 8);
            }
        }
        w.ue(3); // sps_seq_parameter_set_id
        w.ue(s.chroma);
        if s.chroma == 3 {
            w.put(1, 1); // separate_colour_plane_flag
        }
        w.ue(s.size.0);
        w.ue(s.size.1);
        match s.window {
            Some((l, r, t, b)) => {
                w.put(1, 1);
                for v in [l, r, t, b] {
                    w.ue(v);
                }
            }
            None => w.put(0, 1),
        }
        w.ue(0); // bit_depth_luma_minus8 and beyond: present so the input does not just end
        w.put(0b1000_0000, 8);
        w.bytes
    }

    /// The first SPS (NAL type 33) of an Annex B byte run, after its 2-byte header, with the
    /// emulation-prevention bytes (00 00 03) taken out.
    fn first_sps(annex_b: &[u8]) -> Vec<u8> {
        let at = annex_b.windows(5).position(|w| w[..3] == [0, 0, 1] && (w[3] >> 1) & 0x3F == 33).expect("an SPS in the segment") + 5;
        let end = annex_b[at..].windows(3).position(|w| w == [0, 0, 1]).map_or(annex_b.len(), |e| at + e);
        let mut out = Vec::new();
        let mut zeros = 0;
        for &b in &annex_b[at..end] {
            if zeros >= 2 && b == 3 {
                zeros = 0;
                continue;
            }
            zeros = if b == 0 { zeros + 1 } else { 0 };
            out.push(b);
        }
        out
    }

    #[test]
    fn the_size_is_the_coded_size_less_the_conformance_window_in_chroma_units() {
        let none: Vec<(bool, bool)> = vec![];
        let cases = [
            ("1080p coded as 1088 lines, 4:2:0", Sps { sub_layers: none.clone(), chroma: 1, size: (1920, 1088), window: Some((0, 0, 0, 4)) }, Some((1920, 1080))),
            ("no window", Sps { sub_layers: none.clone(), chroma: 1, size: (1280, 720), window: None }, Some((1280, 720))),
            ("4:2:2 crops twice across, once down", Sps { sub_layers: none.clone(), chroma: 2, size: (1280, 720), window: Some((1, 2, 3, 4)) }, Some((1274, 713))),
            ("4:4:4 with a separate-planes flag", Sps { sub_layers: none.clone(), chroma: 3, size: (640, 480), window: Some((1, 0, 0, 2)) }, Some((639, 478))),
            ("monochrome", Sps { sub_layers: none.clone(), chroma: 0, size: (352, 288), window: Some((0, 3, 5, 0)) }, Some((349, 283))),
            (
                "two sub-layers, profile on the first, level on the second",
                Sps { sub_layers: vec![(true, false), (false, true)], chroma: 1, size: (3840, 2176), window: Some((0, 0, 0, 8)) },
                Some((3840, 2160)),
            ),
            ("one sub-layer with both parts", Sps { sub_layers: vec![(true, true)], chroma: 1, size: (720, 576), window: None }, Some((720, 576))),
            ("a window larger than the picture", Sps { sub_layers: none.clone(), chroma: 1, size: (64, 64), window: Some((0, 0, 20, 20)) }, None),
            // Offsets whose sum or chroma multiple does not fit in 32 bits.
            ("offsets that overflow when added", Sps { sub_layers: none.clone(), chroma: 0, size: (64, 64), window: Some((0, 0, 0x8000_0000, 0x8000_0000)) }, None),
            ("an offset that overflows when doubled", Sps { sub_layers: none.clone(), chroma: 1, size: (64, 64), window: Some((0xFFFF_FFF0, 0, 0, 0)) }, None),
        ];
        let mut bad = Vec::new();
        for (case, s, expected) in cases {
            let actual = hevc_sps_size(&sps(&s));
            expect!(bad, actual == expected, "{case}: {actual:?}, expected {expected:?}");
        }
        finish(bad);
    }

    #[test]
    fn input_that_stops_before_the_size_is_none() {
        let whole = sps(&Sps { sub_layers: vec![], chroma: 1, size: (1920, 1088), window: Some((0, 0, 0, 4)) });
        // Bits: 8 of ids, 96 of profile_tier_level, then the ids (8), the width (21 bits, to
        // bit 133), the height (21, to 154) and the window (9, to 163).
        let cases = [("empty", 0), ("one byte", 1), ("in the profile", 8), ("in the width", 15), ("in the height", 18), ("in the window", 20)];
        let mut bad = Vec::new();
        for (case, len) in cases {
            let actual = hevc_sps_size(&whole[..len]);
            expect!(bad, actual.is_none(), "{case} ({len} bytes): {actual:?}");
        }
        finish(bad);
    }

    #[test]
    fn the_sps_of_the_hevc_fixture_gives_its_160x120_picture() {
        let seg = common::fixture_bytes("hls/hevc/seg0.ts");
        let mut demux = playr::ts::TsDemux::default();
        let video: Vec<u8> = [demux.push(&seg), demux.flush()].concat().into_iter().filter(|p| p.kind.is_video()).flat_map(|p| p.data).collect();

        assert_eq!(hevc_sps_size(&first_sps(&video)), Some((160, 120)));
    }
}
