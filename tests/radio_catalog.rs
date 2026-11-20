//! The station catalog of the Radio menu (`playr::radio`): its shape, search and lookup, and an
//! ignored network test that plays every station.

mod common;

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use playr::media::{AudioTrack, Packet};
use playr::radio::{GENRES, RUSSIAN, Section, Station, find_by_url, search, sections};
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::codecs::audio::well_known::CODEC_ID_AAC;

fn all_stations() -> Vec<(&'static Section, &'static Station)> {
    sections().flat_map(|s| s.stations.iter().map(move |st| (s, st))).collect()
}

fn names(found: Vec<(&'static Section, &'static Station)>) -> Vec<(&'static str, &'static str)> {
    found.into_iter().map(|(section, station)| (section.name, station.name)).collect()
}

const JAZZ: &str = "Jazz";
const SOUL: &str = "Soul, Funk & R&B";
const RUSSIAN_NAME: &str = "Russian / Русские";
const HUMOR_FM: &str = "Юмор FM (Humor FM, Jumor FM)";
const HUMOR_FM_BY: &str = "Юмор FM Беларусь (Humor FM Belarus)";

// ---- catalog shape (criterion 4) ----

#[test]
fn there_are_twenty_genres_of_ten_stations_each() {
    let sizes: Vec<(&str, usize)> = GENRES.iter().map(|g| (g.name, g.stations.len())).collect();

    assert_eq!(GENRES.len(), 20, "genres: {sizes:?}");
    let wrong: Vec<&(&str, usize)> = sizes.iter().filter(|(_, n)| *n != 10).collect();
    assert_eq!(wrong, Vec::<&(&str, usize)>::new(), "genres without exactly 10 stations");
}

#[test]
fn the_russian_section_has_ten_stations_including_humor_fm() {
    let station_names: Vec<&str> = RUSSIAN.stations.iter().map(|s| s.name).collect();

    assert_eq!(RUSSIAN.name, RUSSIAN_NAME);
    assert_eq!(RUSSIAN.stations.len(), 10, "{station_names:?}");
    assert!(station_names.iter().any(|n| n.starts_with("Юмор FM")), "no Юмор FM in {station_names:?}");
}

#[test]
fn sections_lists_the_genres_then_the_russian_section() {
    let listed: Vec<&str> = sections().map(|s| s.name).collect();
    let mut expected: Vec<&str> = GENRES.iter().map(|g| g.name).collect();
    expected.push(RUSSIAN_NAME);

    assert_eq!(listed, expected);
}

#[test]
fn section_names_are_unique() {
    let mut seen = HashSet::new();
    let duplicates: Vec<&str> = sections().map(|s| s.name).filter(|n| !seen.insert(*n)).collect();

    assert_eq!(seen.len(), 21);
    assert_eq!(duplicates, Vec::<&str>::new());
}

#[test]
fn no_url_appears_twice_in_the_catalog() {
    let stations = all_stations();
    let mut seen = HashSet::new();
    let duplicates: Vec<String> =
        stations.iter().filter(|(_, st)| !seen.insert(st.url)).map(|(s, st)| format!("{} / {}: {}", s.name, st.name, st.url)).collect();

    assert_eq!(stations.len(), 210);
    assert_eq!(duplicates, Vec::<String>::new());
}

#[test]
fn every_url_is_plain_http_or_https_without_whitespace_and_not_hls() {
    let mut bad = Vec::new();
    for (section, station) in all_stations() {
        let url = station.url;
        let what = format!("{} / {}: {url:?}", section.name, station.name);
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            bad.push(format!("{what} is not http:// or https://"));
        }
        if url.chars().any(char::is_whitespace) {
            bad.push(format!("{what} contains white space"));
        }
        if url.to_ascii_lowercase().contains(".m3u8") {
            bad.push(format!("{what} is an HLS playlist"));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

#[test]
fn every_station_name_is_non_empty_and_trimmed() {
    let mut bad = Vec::new();
    for (section, station) in all_stations() {
        if station.name.is_empty() || station.name.trim() != station.name {
            bad.push(format!("{} / {:?} ({})", section.name, station.name, station.url));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

// ---- search (criteria 4 and 5) ----

#[test]
fn search_cases() {
    let humor_both = vec![(RUSSIAN_NAME, HUMOR_FM), (RUSSIAN_NAME, HUMOR_FM_BY)];
    let mut jazz: Vec<(&str, &str)> =
        GENRES.iter().find(|g| g.name == JAZZ).expect("a Jazz genre").stations.iter().map(|s| (JAZZ, s.name)).collect();
    jazz.push((SOUL, "Jazz Radio Soul"));

    let cases: Vec<(&str, &str, Vec<(&str, &str)>)> = vec![
        ("Cyrillic lower case", "юмор", humor_both.clone()),
        ("Cyrillic capitalised", "Юмор", humor_both.clone()),
        ("Cyrillic upper case", "ЮМОР", humor_both.clone()),
        ("Latin transcription", "humor", humor_both.clone()),
        ("other Latin transcription", "jumor", vec![(RUSSIAN_NAME, HUMOR_FM)]),
        ("Cyrillic word only in one station", "русское", vec![(RUSSIAN_NAME, "Русское Радио (Russkoe Radio)")]),
        ("Cyrillic upper case, one station", "РУССКОЕ", vec![(RUSSIAN_NAME, "Русское Радио (Russkoe Radio)")]),
        // The 10 Jazz stations through the section name, then Jazz Radio Soul in a later section.
        ("section name, in menu order", "jazz", jazz.clone()),
        ("ASCII upper case", "JAZZ", jazz.clone()),
        ("both words required", "jazz soul", vec![(SOUL, "Jazz Radio Soul")]),
        ("both words required, other order", "techno sunshine", vec![("Techno", "Sunshine Live Techno")]),
        ("one word only in the section name", "slam house", vec![("House", "SLAM!")]),
        ("surrounding and repeated white space", "  sunshine \t  techno \n", vec![("Techno", "Sunshine Live Techno")]),
        ("one word matches nothing", "jazz qqzzxq", vec![]),
        ("empty query", "", vec![]),
        ("blank query", "   ", vec![]),
        ("white space only", " \t\n ", vec![]),
    ];
    let mut bad = Vec::new();
    for (case, query, expected) in cases {
        let actual = names(search(query));
        if actual != expected {
            bad.push(format!("{case}: search({query:?}) = {actual:?}\n    expected {expected:?}"));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

#[test]
fn search_results_follow_the_menu_order() {
    let order: Vec<(&str, &str)> = names(all_stations());
    let found = names(search("radio"));
    let positions: Vec<usize> = found.iter().map(|f| order.iter().position(|o| o == f).expect("a catalog entry")).collect();

    // Counted by hand in src/radio.rs: 58 station names contain "radio" in any case (Latin
    // letters; "Радио" is Cyrillic and does not count), and no section name does.
    assert_eq!(found.len(), 58, "{found:?}");
    let sections_hit: HashSet<&str> = found.iter().map(|(s, _)| *s).collect();
    assert!(sections_hit.len() >= 5, "\"radio\" should span several sections: {sections_hit:?}");
    assert!(positions.windows(2).all(|w| w[0] < w[1]), "results out of menu order: {found:?}");
}

// ---- find_by_url (criterion 6) ----

#[test]
fn find_by_url_returns_the_section_and_station() {
    let cases = [
        ("http://stream.deep.radio/hd", ("Techno", "Deep Radio")),
        ("http://pub0101.101.ru:8000/stream/air/aac/64/102", (RUSSIAN_NAME, HUMOR_FM)),
        ("http://icy.unitedradio.it/VirginRock80.mp3", ("Classic Rock", "Virgin Radio Rock '80")),
    ];
    let mut bad = Vec::new();
    for (url, expected) in cases {
        let actual = find_by_url(url).map(|(s, st)| (s.name, st.name, st.url));
        if actual != Some((expected.0, expected.1, url)) {
            bad.push(format!("find_by_url({url:?}) = {actual:?}, expected {expected:?}"));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

#[test]
fn find_by_url_finds_every_catalog_entry() {
    let stations = all_stations();
    let mut bad = Vec::new();
    for (section, station) in &stations {
        match find_by_url(station.url) {
            Some((s, st)) if s == *section && st == *station => {}
            other => bad.push(format!("{}: {:?}", station.url, other.map(|(s, st)| (s.name, st.name)))),
        }
    }
    assert_eq!(stations.len(), 210);
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

#[test]
fn find_by_url_returns_none_for_an_unknown_address() {
    let cases = ["http://example.invalid/stream", "", "http://stream.deep.radio/hd/", "HTTP://STREAM.DEEP.RADIO/hd"];
    for url in cases {
        assert_eq!(find_by_url(url).map(|(s, st)| (s.name, st.name)), None, "{url:?}");
    }
}

#[test]
fn section_contains_its_own_stations_only() {
    let techno = GENRES.iter().find(|g| g.name == "Techno").expect("a Techno genre");
    let deep_radio = "http://stream.deep.radio/hd";
    let humor_fm = "http://pub0101.101.ru:8000/stream/air/aac/64/102";

    assert!(techno.contains(deep_radio));
    assert!(RUSSIAN.contains(humor_fm));
    assert!(!techno.contains(humor_fm), "another section's station");
    assert!(!RUSSIAN.contains(deep_radio), "another section's station");
    assert!(!techno.contains("http://example.invalid/stream"));
    assert!(!techno.contains(""));
    assert!(!techno.contains("http://stream.deep.radio/hd/"));
}

#[test]
fn every_section_contains_exactly_the_urls_listed_under_it() {
    let stations = all_stations();
    let mut bad = Vec::new();
    for section in sections() {
        for (owner, station) in &stations {
            let expected = owner.name == section.name;
            if section.contains(station.url) != expected {
                bad.push(format!("{}.contains({}) != {expected}", section.name, station.url));
            }
        }
    }
    assert_eq!(stations.len(), 210);
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

// ---- every station plays (criterion 7) ----

const STALL_TIMEOUT: Duration = Duration::from_secs(8);
const AUDIO_NEEDED: f64 = 3.0;
const PROBE_DEADLINE: Duration = Duration::from_secs(45);
const THREADS: usize = 32;
const MIN_AAC_RATE: u32 = 32_000;

/// Opens `url`, demuxes it and decodes audio until [`AUDIO_NEEDED`] seconds are decoded.
/// Returns a short description of what played, or why it did not.
fn probe(url: &str) -> Result<String, String> {
    let abort = Arc::new(AtomicBool::new(false));
    let result = probe_with(url, &abort);
    // Lets the download thread behind the stream stop.
    abort.store(true, Ordering::Relaxed);
    result
}

fn probe_with(url: &str, abort: &Arc<AtomicBool>) -> Result<String, String> {
    let started = Instant::now();
    let stream = playr::net::open_url_with_timeout(url, abort.clone(), Box::new(|_| {}), STALL_TIMEOUT)?;
    let (ext, mime) = (stream.ext.clone(), stream.mime.clone());
    let mut demux = playr::demux::open_stream(stream.source, ext.as_deref(), mime.as_deref())
        .map_err(|e| format!("demux (ext {ext:?}, mime {mime:?}): {e}"))?;
    let container = demux.info().container.clone();
    let track = demux.info().audio.clone().ok_or_else(|| format!("{container}: no audio track"))?;
    let mut decoder = match &track {
        AudioTrack::Symphonia(params) => Some(
            common::audio_codecs()
                .make_audio_decoder(params, &AudioDecoderOptions::default())
                .map_err(|e| format!("{container}: no decoder: {e}"))?,
        ),
        AudioTrack::Pcm { .. } => None,
    };
    let (mut seconds, mut packets, mut failed) = (0.0f64, 0usize, 0usize);
    let mut last_error = None;
    let mut rate = 0;
    while seconds < AUDIO_NEEDED {
        if started.elapsed() > PROBE_DEADLINE {
            return Err(format!("{container}: only {seconds:.2} s decoded after {PROBE_DEADLINE:?} ({packets} packets, {failed} failed)"));
        }
        let packet = match demux.next_packet() {
            Ok(Some(Packet::Audio { packet, .. })) => packet,
            Ok(Some(Packet::Video { .. })) => continue,
            Ok(None) => return Err(format!("{container}: stream ended after {seconds:.2} s of audio")),
            Err(e) => return Err(format!("{container}: demux error after {seconds:.2} s of audio: {e}")),
        };
        packets += 1;
        match (&mut decoder, &track) {
            (Some(dec), _) => match dec.decode(&packet) {
                Ok(buf) => {
                    rate = buf.spec().rate();
                    if rate > 0 {
                        seconds += buf.frames() as f64 / f64::from(rate);
                    }
                }
                Err(e) => {
                    failed += 1;
                    last_error = Some(e.to_string());
                }
            },
            (None, AudioTrack::Pcm { rate: r, channels, bits }) => {
                rate = *r;
                let frame_bytes = usize::from(*channels) * (*bits as usize / 8);
                seconds += (packet.data.len() / frame_bytes.max(1)) as f64 / f64::from((*r).max(1));
            }
            (None, AudioTrack::Symphonia(_)) => unreachable!("a decoder is made for every Symphonia track"),
        }
    }
    // HE-AAC is decoded without its SBR band, so it comes out at half its rate and muffled.
    if let AudioTrack::Symphonia(params) = &track
        && params.codec == CODEC_ID_AAC
        && rate < MIN_AAC_RATE
    {
        return Err(format!("{container}: AAC decodes at {rate} Hz, below {MIN_AAC_RATE} Hz (HE-AAC without SBR)"));
    }
    if failed * 2 > packets {
        return Err(format!("{container}: {failed} of {packets} packets failed to decode, last: {last_error:?}"));
    }
    Ok(format!("{container} {rate} Hz, {seconds:.1} s from {packets} packets ({failed} failed) in {:.1?}", started.elapsed()))
}

#[test]
#[ignore = "network: opens every station of the catalog (15-60 s with 32 threads)"]
fn every_station_plays() {
    let stations = all_stations();
    let next = AtomicUsize::new(0);
    let failures = Mutex::new(Vec::new());
    let retried = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..THREADS {
            scope.spawn(|| {
                loop {
                    let n = next.fetch_add(1, Ordering::Relaxed);
                    let Some((section, station)) = stations.get(n) else { break };
                    let what = format!("{} / {} <{}>", section.name, station.name, station.url);
                    match probe(station.url) {
                        Ok(_) => {}
                        Err(first) => match probe(station.url) {
                            Ok(_) => retried.lock().unwrap().push(format!("{what}: first try failed with {first}")),
                            Err(second) => failures.lock().unwrap().push(format!("{what}:\n    1st: {first}\n    2nd: {second}")),
                        },
                    }
                }
            });
        }
    });
    let retried = retried.into_inner().unwrap();
    if !retried.is_empty() {
        eprintln!("{} station(s) played only on the second try:\n{}", retried.len(), retried.join("\n"));
    }
    let mut failures = failures.into_inner().unwrap();
    failures.sort();
    assert!(failures.is_empty(), "{} of {} stations did not play:\n{}", failures.len(), stations.len(), failures.join("\n"));
}
