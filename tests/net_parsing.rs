//! Pure parsing in `playr::net`: ICY metadata, playlists, URL / name / MIME classification.

use playr::net::{is_playlist_name, is_url, mime_to_ext, parse_playlist, parse_stream_title};

mod stream_title {
    use super::*;

    #[test]
    fn extracts_the_title_field() {
        let cases: [(&str, &[u8], &str); 8] = [
            ("title then url", b"StreamTitle='Artist - Song';StreamUrl='http://radio.example/art.jpg';", "Artist - Song"),
            ("url then title", b"StreamUrl='http://radio.example/';StreamTitle='Second Field';", "Second Field"),
            ("nul padding to the 16-byte block", b"StreamTitle='Padded - Title';\0\0\0\0\0", "Padded - Title"),
            ("apostrophe inside the title", b"StreamTitle='Don't Stop Me Now';StreamUrl='';", "Don't Stop Me Now"),
            ("two apostrophes inside the title", b"StreamTitle='Rock 'n' Roll';StreamUrl='http://radio.example/x';", "Rock 'n' Roll"),
            ("utf-8 title", "StreamTitle='Zażółć - Gęślą Jaźń';".as_bytes(), "Zażółć - Gęślą Jaźń"),
            ("latin-1 title", b"StreamTitle='Caf\xE9 del Mar - \xDCber';", "Café del Mar - Über"),
            ("no closing semicolon", b"StreamTitle='Unterminated'\0\0\0", "Unterminated"),
        ];
        for (name, meta, expected) in cases {
            assert_eq!(parse_stream_title(meta).as_deref(), Some(expected), "case: {name}");
        }
    }

    #[test]
    fn returns_none_when_there_is_no_title() {
        let cases: [(&str, &[u8]); 5] = [
            ("empty block", b""),
            ("nul-only block", b"\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0"),
            ("empty title", b"StreamTitle='';StreamUrl='http://radio.example/';"),
            ("blank title", b"StreamTitle='   ';"),
            ("other fields only", b"StreamUrl='http://radio.example/';"),
        ];
        for (name, meta) in cases {
            assert_eq!(parse_stream_title(meta), None, "case: {name}");
        }
    }
}

mod playlist {
    use super::*;

    #[test]
    fn returns_the_first_stream_address() {
        let cases = [
            ("plain m3u, two entries", "http://one.example/stream\nhttp://two.example/stream\n", "http://one.example/stream"),
            (
                "extended m3u with BOM, comments and CRLF",
                "\u{feff}#EXTM3U\r\n#EXTINF:-1,Radio Example\r\nhttp://radio.example:8000/live\r\nhttp://backup.example/live\r\n",
                "http://radio.example:8000/live",
            ),
            ("BOM directly before the address", "\u{feff}http://bom.example/s", "http://bom.example/s"),
            ("relative entry before the address", "local.mp3\n\nhttps://later.example/s\n", "https://later.example/s"),
            ("query string with '=' in an m3u", "http://q.example/listen?id=5&fmt=mp3\n", "http://q.example/listen?id=5&fmt=mp3"),
            ("indented line", "   http://indent.example/s  \n", "http://indent.example/s"),
            (
                "pls",
                "[playlist]\nNumberOfEntries=2\nFile1=http://pls-one.example/a\nTitle1=One\nLength1=-1\nFile2=http://pls-two.example/b\nVersion=2\n",
                "http://pls-one.example/a",
            ),
            ("pls, lower-case key and spaces", "[playlist]\nfile1 = http://pls-spaced.example/a\n", "http://pls-spaced.example/a"),
            ("pls, '=' in the address", "[playlist]\nFile1=http://pls-q.example/a?k=v\n", "http://pls-q.example/a?k=v"),
            (
                "pls, first entry is a local file",
                "[playlist]\nFile1=C:\\music\\a.mp3\nFile2=http://pls-second.example/b\n",
                "http://pls-second.example/b",
            ),
        ];
        for (name, text, expected) in cases {
            assert_eq!(parse_playlist(text), Ok(Some(expected.to_string())), "case: {name}");
        }
    }

    #[test]
    fn returns_none_when_no_entry_is_a_stream_address() {
        let cases = [
            ("empty text", ""),
            ("comments only", "#EXTM3U\n# nothing here\n"),
            ("local files only", "song.mp3\nC:\\music\\other.flac\n"),
            ("pls without File entries", "[playlist]\nNumberOfEntries=0\nVersion=2\n"),
            ("address under a non-File key", "[playlist]\nTitle1=http://not-a-file.example/x\n"),
        ];
        for (name, text) in cases {
            assert_eq!(parse_playlist(text), Ok(None), "case: {name}");
        }
    }

    #[test]
    fn rejects_hls_playlists_with_a_readable_error() {
        let cases = [
            ("media playlist", "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:9.9,\nhttp://hls.example/seg0.ts\n"),
            (
                "master playlist",
                "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=128000\nhttp://hls.example/low/index.m3u8\n",
            ),
        ];
        for (name, text) in cases {
            assert_eq!(parse_playlist(text), Err("HLS streams are not supported".to_string()), "case: {name}");
        }
    }
}

#[test]
fn is_url_accepts_only_http_and_https() {
    let cases = [
        ("http://radio.example/live", true),
        ("https://radio.example/live", true),
        ("HTTP://RADIO.EXAMPLE/LIVE", true),
        ("  https://padded.example/", true),
        ("ftp://files.example/a.mp3", false),
        ("C:\\music\\a.mp3", false),
        ("http:/one-slash.example", false),
        ("xhttp://prefix.example", false),
        ("music/http://inside.mp3", false),
        ("", false),
    ];
    for (input, expected) in cases {
        assert_eq!(is_url(input), expected, "is_url({input:?})");
    }
}

#[test]
fn is_playlist_name_looks_at_the_extension_before_query_and_fragment() {
    let cases = [
        ("station.m3u", true),
        ("STATION.M3U8", true),
        ("station.pls", true),
        ("http://radio.example/listen.pls?sid=1", true),
        ("http://radio.example/listen.m3u#top", true),
        ("station.mp3", false),
        ("m3u", false),
        ("station.plsx", false),
        ("http://radio.example/stream.mp3?list=x.m3u", false),
        ("", false),
    ];
    for (input, expected) in cases {
        assert_eq!(is_playlist_name(input), expected, "is_playlist_name({input:?})");
    }
}

#[test]
fn mime_to_ext_maps_media_types_to_demuxer_hints() {
    let cases = [
        ("audio/mpeg", Some("mp3")),
        ("audio/mp3", Some("mp3")),
        ("audio/x-mpeg", Some("mp3")),
        ("audio/aac", Some("aac")),
        ("audio/aacp", Some("aac")),
        ("audio/x-aac", Some("aac")),
        ("audio/ogg", Some("ogg")),
        ("application/ogg", Some("ogg")),
        ("audio/vorbis", Some("ogg")),
        ("audio/flac", Some("flac")),
        ("audio/x-flac", Some("flac")),
        ("audio/wav", Some("wav")),
        ("audio/x-wav", Some("wav")),
        ("audio/wave", Some("wav")),
        ("audio/mp4", Some("mp4")),
        ("audio/x-m4a", Some("mp4")),
        ("video/mp4", Some("mp4")),
        ("video/x-matroska", Some("mkv")),
        ("audio/x-matroska", Some("mkv")),
        ("video/webm", Some("mkv")),
        ("audio/webm", Some("mkv")),
        ("text/html", None),
        ("audio/x-mpegurl", None),
        ("application/octet-stream", None),
        ("", None),
    ];
    for (mime, expected) in cases {
        assert_eq!(mime_to_ext(mime), expected, "mime_to_ext({mime:?})");
    }
}
