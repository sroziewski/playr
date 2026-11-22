//! `playr::cookies`: reading a Netscape cookie file and choosing which cookies a request gets.
//! Only synthetic cookies are used; nothing is read from or written to the user's folders.

mod common;

use common::finish;
use playr::cookies::{Cookie, CookieJar, domain_matches, parse};

fn cookie(domain: &str, subdomains: bool, path: &str, secure: bool, expires: u64, name: &str, value: &str) -> Cookie {
    Cookie {
        domain: domain.into(),
        include_subdomains: subdomains,
        path: path.into(),
        secure,
        expires,
        name: name.into(),
        value: value.into(),
    }
}

fn jar(lines: &[&str]) -> CookieJar {
    parse(&lines.join("\n")).expect("parse")
}

mod parsing {
    use super::*;

    #[test]
    fn reads_every_cookie_line_and_skips_the_rest() {
        let text = [
            "# Netscape HTTP Cookie File",
            "# a comment\twith\ttabs\tbut\tnot\ta\tcookie",
            "",
            "   ",
            ".example.com\tTRUE\t/\tFALSE\t0\tsid\ts1",
            "#HttpOnly_.Shop.Example\tTRUE\t/cart\tTRUE\t2000000000\tcart\tc=2 x",
            "host.example\tFALSE\t/\tFALSE\t0\thostonly\th3",
            "not a cookie line",
            "three\tfields\tonly",
            "eight\tFALSE\t/\tFALSE\t0\tn\tv\textra",
            ".example.com\tTRUE\t/\tFALSE\t0\t\tempty-name",
            "\tTRUE\t/\tFALSE\t0\tnodomain\tv",
            ".dotted.example\tFALSE\t/\tFALSE\t0\tdot\td4",
            "sub.example\tTRUE\t/\tFALSE\t0\tflagged\tf8",
            "x.example\tFALSE\t\tFALSE\t1700000000.5\tf\tv5",
            "y.example\tfalse\t/p\ttrue\t-5\tg\tv6",
            "z.example\tFALSE\t/\tFALSE\tnever\th\t",
        ]
        .join("\r\n");

        let parsed = parse(&text).expect("parse");

        assert_eq!(
            parsed.cookies,
            vec![
                cookie(".example.com", true, "/", false, 0, "sid", "s1"),
                cookie(".shop.example", true, "/cart", true, 2_000_000_000, "cart", "c=2 x"),
                cookie("host.example", false, "/", false, 0, "hostonly", "h3"),
                cookie(".dotted.example", true, "/", false, 0, "dot", "d4"),
                cookie("sub.example", true, "/", false, 0, "flagged", "f8"),
                cookie("x.example", false, "/", false, 1_700_000_000, "f", "v5"),
                cookie("y.example", false, "/p", true, 0, "g", "v6"),
                cookie("z.example", false, "/", false, 0, "h", ""),
            ]
        );
    }

    #[test]
    fn text_without_a_single_cookie_is_an_error() {
        let message = "no cookies found; expected a Netscape cookies.txt file (tab-separated)";
        let cases = [
            ("empty", ""),
            ("comments only", "# Netscape HTTP Cookie File\n# nothing here\n"),
            ("a JSON export", "[{\"domain\": \".example.com\", \"name\": \"a\", \"value\": \"b\"}]"),
            ("space-separated", ".example.com TRUE / FALSE 0 sid s1"),
        ];
        let mut bad = Vec::new();
        for (case, text) in cases {
            let result = parse(text);
            expect!(bad, result == Err(message.to_string()), "{case}: {result:?}");
        }
        finish(bad);
    }
}

#[test]
fn domain_matches_the_host_itself_and_subdomains_only_when_allowed() {
    let cases = [
        (".example.com", true, "example.com", true),
        ("example.com", true, "www.example.com", true),
        ("example.com", true, "a.b.example.com", true),
        ("example.com", false, "www.example.com", false),
        ("example.com", false, "example.com", true),
        (".example.com", true, "badexample.com", false),
        ("example.com", true, "example.com.evil", false),
        ("example.com", true, "com", false),
        ("EXAMPLE.com", false, "Example.COM", true),
        ("", true, "example.com", false),
        (".", true, "example.com", false),
    ];
    let mut bad = Vec::new();
    for (domain, subdomains, host, expected) in cases {
        let actual = domain_matches(domain, subdomains, host);
        expect!(bad, actual == expected, "domain_matches({domain:?}, {subdomains}, {host:?}) = {actual}, expected {expected}");
    }
    finish(bad);
}

mod header_for {
    use super::*;

    const NOW: u64 = 1_800_000_000;

    fn site_jar() -> CookieJar {
        jar(&[
            ".site.example\tTRUE\t/\tFALSE\t0\tsession\t1",
            ".site.example\tTRUE\t/\tTRUE\t0\tsecure\t2",
            ".site.example\tTRUE\t/\tFALSE\t1799999999\texpired\t3",
            ".site.example\tTRUE\t/\tFALSE\t1800000000\texpiring\t4",
            ".site.example\tTRUE\t/\tFALSE\t1800000001\tfresh\t5",
            "www.site.example\tFALSE\t/\tFALSE\t0\thostonly\t6",
            ".site.example\tTRUE\t/docs\tFALSE\t0\tdocs\t7",
            ".site.example\tTRUE\t/docs/\tFALSE\t0\tdocsdir\t8",
            ".other.example\tTRUE\t/\tFALSE\t0\tother\t9",
        ])
    }

    #[test]
    fn sends_what_a_browser_would_send() {
        let jar = site_jar();
        let s = |x: &str| Some(x.to_string());
        let cases = [
            ("https, host-only host", "https://www.site.example/", s("session=1; secure=2; fresh=5; hostonly=6")),
            ("plain http: no secure cookie", "http://www.site.example/", s("session=1; fresh=5; hostonly=6")),
            ("subdomain of the host-only host", "https://a.www.site.example/", s("session=1; secure=2; fresh=5")),
            ("bare domain", "http://site.example", s("session=1; fresh=5")),
            ("exact cookie path", "http://site.example/docs", s("session=1; fresh=5; docs=7")),
            ("below the cookie path", "http://site.example/docs/a", s("session=1; fresh=5; docs=7; docsdir=8")),
            ("path sharing only a prefix", "http://site.example/docsx", s("session=1; fresh=5")),
            ("query after the path", "http://site.example/docs?x=/docs/", s("session=1; fresh=5; docs=7")),
            ("port, user info and capitals", "https://me@WWW.Site.Example:8443/", s("session=1; secure=2; fresh=5; hostonly=6")),
            ("unrelated host", "https://unrelated.example/", None),
            ("not http", "ftp://www.site.example/", None),
        ];
        let mut bad = Vec::new();
        for (case, url, expected) in cases {
            let actual = jar.header_for(url, NOW);
            expect!(bad, actual == expected, "{case}: header_for({url:?}) = {actual:?}, expected {expected:?}");
        }
        finish(bad);
    }

    #[test]
    fn a_cookie_expires_at_its_expiry_second() {
        let jar = jar(&["a.example\tFALSE\t/\tFALSE\t1000\tk\tv"]);

        assert_eq!(jar.header_for("http://a.example/", 999).as_deref(), Some("k=v"));
        assert_eq!(jar.header_for("http://a.example/", 1000), None);
    }
}

mod for_sites {
    use super::*;

    #[test]
    fn keeps_only_the_named_sites_and_their_subdomains() {
        let jar = jar(&[
            "youtube.com\tFALSE\t/\tFALSE\t0\ta\t1",
            "bank.example\tFALSE\t/\tTRUE\t0\tbank\t2",
            ".youtube.com\tTRUE\t/\tTRUE\t0\tb\t3",
            "notyoutube.com\tFALSE\t/\tFALSE\t0\tc\t4",
            "accounts.google.com\tFALSE\t/\tTRUE\t0\td\t5",
            "youtube.com.evil.example\tFALSE\t/\tFALSE\t0\te\t6",
            ".google.com\tTRUE\t/\tFALSE\t0\tf\t7",
            "google.co.uk\tFALSE\t/\tFALSE\t0\tg\t8",
            "mail.example\tFALSE\t/\tFALSE\t0\th\t9",
        ]);

        let subset = jar.for_sites(&["youtube.com", "google.com"]);

        let names: Vec<&str> = subset.cookies.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["a", "b", "d", "f"]);
        assert_eq!(subset.cookies[1], jar.cookies[2], "cookies are copied whole");
    }

    #[test]
    fn no_sites_keep_nothing() {
        let jar = jar(&["youtube.com\tFALSE\t/\tFALSE\t0\ta\t1"]);

        assert_eq!(jar.for_sites(&[]), CookieJar::default());
        assert_eq!(jar.for_sites(&["spotify.com"]), CookieJar::default());
    }
}

mod netscape_output {
    use super::*;

    #[test]
    fn writes_the_header_and_one_tab_separated_line_per_cookie() {
        let jar = CookieJar {
            cookies: vec![
                cookie(".a.example", true, "/", true, 1_800_000_000, "n1", "v1"),
                cookie("b.example", false, "/x", false, 0, "n2", "v=2"),
            ],
        };

        assert_eq!(
            jar.to_netscape(),
            "# Netscape HTTP Cookie File\n.a.example\tTRUE\t/\tTRUE\t1800000000\tn1\tv1\nb.example\tFALSE\t/x\tFALSE\t0\tn2\tv=2\n"
        );
    }

    #[test]
    fn parse_reads_back_what_to_netscape_wrote() {
        let original = jar(&[
            "#HttpOnly_.youtube.com\tTRUE\t/\tTRUE\t1800000000\tSID\tabc",
            "www.youtube.com\tFALSE\t/watch\tFALSE\t0\tPREF\tf6=40000000&hl=pl",
            ".google.com\tTRUE\t/\tFALSE\t1900000000\tNID\tx y",
        ]);

        let again = parse(&original.to_netscape()).expect("parse");

        assert_eq!(again, original);
    }
}
