//! `playr::subtitles`: SRT and WebVTT parsing, and which lines are on screen at a time.

mod common;

use common::finish;
use playr::subtitles::{Cue, MAX_LINES, Subtitles, clean_text, parse, parse_timestamp};

fn cue(start: f64, end: f64, text: &str) -> Cue {
    Cue { start, end, text: text.into() }
}

mod timestamps {
    use super::*;

    #[test]
    fn reads_hours_minutes_seconds_and_up_to_three_fraction_digits() {
        let cases = [
            ("01:02:03,456", 3723.456),
            ("01:02:03.456", 3723.456),
            ("02:03.456", 123.456),
            ("00:00:00,000", 0.0),
            ("00:59:59.999", 3599.999),
            ("123:00:00,000", 442_800.0),
            ("00:01.5", 1.5),
            ("00:01.50", 1.5),
            ("00:00:07,008", 7.008),
            (" 00:00:02.250 ", 2.25),
        ];
        let mut bad = Vec::new();
        for (text, expected) in cases {
            let actual = parse_timestamp(text);
            expect!(bad, actual.is_some_and(|a| (a - expected).abs() < 1e-9), "parse_timestamp({text:?}) = {actual:?}, expected {expected}");
        }
        finish(bad);
    }

    #[test]
    fn rejects_out_of_range_fields_and_malformed_text() {
        let cases = [
            ("60 minutes", "00:60:00.000"),
            ("60 seconds", "00:00:60.000"),
            ("60 seconds without hours", "01:60.000"),
            ("four fraction digits", "00:00:01.1234"),
            ("empty fraction", "00:00:01."),
            ("letters in the fraction", "00:00:01.5x"),
            ("a single field", "01,000"),
            ("four fields", "01:02:03:04.000"),
            ("empty field", "00::01.000"),
            ("letters", "aa:bb.ccc"),
            ("negative", "-01:00.000"),
            ("empty", ""),
        ];
        let mut bad = Vec::new();
        for (case, text) in cases {
            let actual = parse_timestamp(text);
            expect!(bad, actual.is_none(), "{case}: parse_timestamp({text:?}) = {actual:?}, expected None");
        }
        finish(bad);
    }

    /// WebVTT (and the contract: "minutes/seconds < 60") allows minutes 00-59 in the short
    /// MM:SS.mmm form too; only the hours field is unbounded.
    #[test]
    fn minutes_of_a_time_without_hours_stay_below_60() {
        assert_eq!(parse_timestamp("60:00.000"), None);
        assert_eq!(parse_timestamp("75:30.000"), None);
        assert_eq!(parse_timestamp("59:59.500"), Some(3599.5));
    }
}

#[test]
fn clean_text_strips_markup_and_decodes_entities() {
    let cases = [
        ("<i>Hello</i> <b>world</b>", "Hello world"),
        ("<c.colorE5E5E5>word</c><00:00:01.500><c> next</c>", "word next"),
        ("{\\an8}Top line", "Top line"),
        ("{not a tag} stays", "{not a tag} stays"),
        ("&lt;3 &amp; &gt; &quot;q&quot; &#39;a&apos; a&nbsp;b", "<3 & > \"q\" 'a' a b"),
        ("&amp;lt; is decoded once", "&lt; is decoded once"),
        ("Zażółć <u>gęślą</u> jaźń", "Zażółć gęślą jaźń"),
        ("plain", "plain"),
        ("I <3 you", "I <3 you"),
        ("<i>closed</i> then <3 open", "closed then <3 open"),
        ("I <3 you <i>x</i>", "I <3 you x"),
    ];
    let mut bad = Vec::new();
    for (text, expected) in cases {
        let actual = clean_text(text);
        expect!(bad, actual == expected, "clean_text({text:?}) = {actual:?}, expected {expected:?}");
    }
    finish(bad);
}

mod parsing {
    use super::*;

    #[test]
    fn srt_with_bom_crlf_numbers_and_tags() {
        let text = "\u{feff}1\r\n00:00:01,000 --> 00:00:02,500\r\n<i>Hello</i>\r\n  world  \r\n\r\n2\r\n00:00:03,000 --> 00:00:04,000\r\nSecond\r\n";

        let subs = parse(text).expect("parse");

        assert_eq!(subs.cues, vec![cue(1.0, 2.5, "Hello\nworld"), cue(3.0, 4.0, "Second")]);
    }

    #[test]
    fn webvtt_with_header_notes_cue_ids_and_cue_settings() {
        let text = "WEBVTT\nKind: captions\nLanguage: en\n\nNOTE written by hand\n\n00:01.000 --> 00:02.000 align:start position:0%\n{\\an8}Top &amp; more\n\nintro-cue\n00:00:05.000 --> 00:00:06.250\nWith an id\n";

        let subs = parse(text).expect("parse");

        assert_eq!(subs.cues, vec![cue(1.0, 2.0, "Top & more"), cue(5.0, 6.25, "With an id")]);
    }

    #[test]
    fn unreadable_blocks_are_skipped_and_the_rest_sorted_by_start() {
        let text = [
            "00:00:09.000 --> 00:00:10.000\nlast",
            "00:00:04.000 --> 00:00:04.000\nzero length",
            "00:00:06.000 --> 00:00:05.000\nends before it starts",
            "00:00:0x.000 --> 00:00:08.000\nbad start",
            "00:00:07.000 --> later\nbad end",
            "00:00:02.000 --> 00:00:03.000\n<i></i>\n   ",
            "an id\nanother line\n00:00:01.000 --> 00:00:02.000\ntiming on the third line",
            "00:00:01.000 --> 00:00:01.001\nfirst",
            "just text",
        ]
        .join("\n\n");

        let subs = parse(&text).expect("parse");

        assert_eq!(subs.cues, vec![cue(1.0, 1.001, "first"), cue(9.0, 10.0, "last")]);
    }

    #[test]
    fn old_mac_line_endings_are_read() {
        let subs = parse("1\r00:00:01,000 --> 00:00:02,000\rMac\r\r2\r00:00:03,000 --> 00:00:04,000\rOS 9\r").expect("parse");

        assert_eq!(subs.cues, vec![cue(1.0, 2.0, "Mac"), cue(3.0, 4.0, "OS 9")]);
    }

    #[test]
    fn text_without_any_cue_is_an_error() {
        let cases = [
            ("empty", ""),
            ("header only", "WEBVTT\n\n"),
            ("only broken cues", "1\n00:00:02,000 --> 00:00:01,000\nbackwards\n"),
            ("not subtitles", "<html><body>404</body></html>"),
        ];
        let mut bad = Vec::new();
        for (case, text) in cases {
            let result = parse(text);
            expect!(bad, result == Err("no subtitles found in the file".to_string()), "{case}: {result:?}");
        }
        finish(bad);
    }
}

mod lines_at {
    use super::*;

    fn rolling() -> Subtitles {
        Subtitles {
            cues: vec![
                cue(1.0, 4.0, "one"),
                cue(2.0, 5.0, "two\nthree"),
                cue(3.0, 6.0, "four"),
                cue(3.5, 4.5, "two"),
                cue(7.0, 8.0, "  seven  \n\n"),
            ],
        }
    }

    #[test]
    fn max_lines_is_three() {
        assert_eq!(MAX_LINES, 3);
    }

    #[test]
    fn shows_started_cues_until_their_end_newest_three_lines_without_repeats() {
        let subs = rolling();
        let cases: [(f64, &[&str]); 12] = [
            (0.5, &[]),
            (0.999, &[]),
            (1.0, &["one"]),
            (2.0, &["one", "two", "three"]),
            (2.9, &["one", "two", "three"]),
            (3.0, &["two", "three", "four"]),
            (4.2, &["two", "three", "four"]),
            (4.999, &["two", "three", "four"]),
            (5.0, &["four"]),
            (6.0, &[]),
            (7.5, &["seven"]),
            (100.0, &[]),
        ];
        let mut bad = Vec::new();
        for (t, expected) in cases {
            let actual = subs.lines_at(t);
            expect!(bad, actual == expected, "lines_at({t}) = {actual:?}, expected {expected:?}");
        }
        finish(bad);
    }

    #[test]
    fn a_repeated_line_keeps_its_first_place() {
        let subs = Subtitles { cues: vec![cue(0.0, 10.0, "same"), cue(1.0, 10.0, "other"), cue(2.0, 10.0, "same")] };

        assert_eq!(subs.lines_at(3.0), ["same", "other"]);
    }

    #[test]
    fn no_cues_show_nothing() {
        assert_eq!(Subtitles::default().lines_at(1.0), Vec::<&str>::new());
    }
}
