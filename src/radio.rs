//! The stations of the Radio menu: twenty genres and a Russian-language section.
//!
//! Every address was opened and decoded through [`crate::net::open_url`] before it went in, so
//! each is a plain Icecast/SHOUTcast stream (MP3 or AAC) that playr can play. Left out on
//! purpose: HLS-only stations, SHOUTcast v1 servers that answer `ICY 200 OK` (the HTTP client
//! rejects that status line), and HE-AAC streams below 32 kHz, which symphonia decodes without
//! their high band, so they would sound muffled.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Station {
    pub name: &'static str,
    pub url: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Section {
    pub name: &'static str,
    pub stations: &'static [Station],
}

const fn st(name: &'static str, url: &'static str) -> Station {
    Station { name, url }
}

/// Music and talk by genre, each with its ten best-suited stations.
pub const GENRES: &[Section] = &[
    Section {
        name: "Pop",
        stations: &[
            st("Capital FM London", "http://media-ice.musicradio.com/CapitalMP3"),
            st("Heart London", "http://media-ice.musicradio.com/HeartLondonMP3"),
            st("Heart 80s", "https://media-ssl.musicradio.com/Heart80sMP3"),
            st("Radio 105 Network", "http://icecast.unitedradio.it/Radio105.mp3"),
            st("SWR3", "https://liveradio.swr.de/sw282p3/swr3/play.mp3"),
            st("Radio Swiss Pop", "http://stream.srg-ssr.ch/m/rsp/mp3_128"),
            st("90s90s Hits", "http://streams.90s90s.de/pop/mp3-192/streams.90s90s.de/"),
            st("SomaFM PopTron", "https://ice6.somafm.com/poptron-128-mp3"),
            st("SomaFM Underground 80s", "https://ice6.somafm.com/u80s-128-mp3"),
            st("Chocolate FM", "http://streaming5.elitecomunicacion.es:8082/live.mp3"),
        ],
    },
    Section {
        name: "Rock",
        stations: &[
            st("Radio Paradise Rock Mix", "http://stream.radioparadise.com/rock-320"),
            st("Hard Rock Heaven", "http://hydra.cdnstream.com/1521_128"),
            st("Radio ROKS Hard'n'Heavy", "http://online.radioroks.ua/RadioROKS_HardnHeavy_HD"),
            st("Radio ROKS Rock Ballads", "http://online.radioroks.ua/RadioROKS_Ballads"),
            st("Best of Rock FM Alternative", "https://bestofrockfm.stream.vip/altrock/mp3-256/bestofrock.fm/"),
            st("1.FM High Voltage", "http://strm112.1.fm/highvoltage_mobile_mp3"),
            st("ROCK ANTENNE Classic Perlen", "http://mp3channels.webradio.rockantenne.de/classic-perlen"),
            st("KISS FM 100.6 Athens", "http://live.radiokiss.gr/kissfm1006hq.mp3"),
            st("Bandit Rock", "http://fm02-ice.stream.khz.se/fm02_mp3"),
            st("Radio Caroline", "http://78.129.202.200:8040/;"),
        ],
    },
    Section {
        name: "Classic Rock",
        stations: &[
            st("Virgin Radio Classic Rock", "http://icy.unitedradio.it/VirginRockClassics.mp3"),
            st("Virgin Radio Rock '70", "http://icy.unitedradio.it/VirginRock70.mp3"),
            st("Virgin Radio Rock '80", "http://icy.unitedradio.it/VirginRock80.mp3"),
            st("Virgin Radio Rock '90", "http://icy.unitedradio.it/Virgin_03.mp3"),
            st("Antenne Bayern Classic Rock Live", "http://mp3channels.webradio.antenne.de/classic-rock-live"),
            st("SomaFM Left Coast 70s", "https://ice4.somafm.com/seventies-320-mp3"),
            st("Megarock Radio", "https://stream3.megarockradio.net:80/"),
            st("St Louis Classic Rock", "http://74.208.89.18:8000/SLCR_highdef.mp3;"),
            st("Oxygen Classic Rock", "https://oxygenmusic.hu:8443/oxygenclassicrock"),
            st("1.FM Rock Classics", "http://strm112.1.fm/rockclassics_mobile_mp3"),
        ],
    },
    Section {
        name: "Metal",
        stations: &[
            st("La Grosse Radio Métal", "http://hd.lagrosseradio.info/lagrosseradio-metal-192.mp3"),
            st("RADIO BOB! Metal", "http://streams.radiobob.de/bob-metal/mp3-192/streams.radiobob.de/"),
            st("RADIO BOB! Metalcore", "http://streams.radiobob.de/metalcore/mp3-192/streams.radiobob.de/"),
            st("ChroniX Metalcore", "http://usa17.fastcast4u.com:5508/stream"),
            st("Big R Radio 80s Metal", "http://bigrradio.cdnstream1.com/5186_128"),
            st("181.FM 80s Hair Band", "https://listen.181fm.com/181-hairband_128k.mp3"),
            st("Exclusively Iron Maiden", "https://streaming.exclusive.radio/er/ironmaiden/icecast.audio"),
            st("Exclusively Black Sabbath", "https://streaming.exclusive.radio/er/blacksabbath/icecast.audio"),
            st("Crucial Velocity Radio", "http://ais-sa2.cdnstream1.com/1369_128"),
            st("Radio Heavy Metal", "https://manager7.streamradio.fr:1435/stream"),
        ],
    },
    Section {
        name: "Indie & Alternative",
        stations: &[
            st("KEXP 90.3 Seattle", "http://live-mp3-128.kexp.org/kexp128.mp3"),
            st("FluxFM", "http://streams.fluxfm.de/live/mp3-320/audio/"),
            st("ByteFM", "https://bytefm.cast.addradio.de/bytefm/main/high/stream"),
            st("SomaFM Indie Pop Rocks!", "https://ice5.somafm.com/indiepop-128-aac"),
            st("delta radio Indie", "http://streams.deltaradio.de/delta-indie/mp3-192/mediaplayer"),
            st("0N Indie", "https://0n-indie.radionetz.de/0n-indie.mp3"),
            st("Oxygen Indie", "https://oxygenmusic.hu:8443/oxygenindie"),
            st("egoFM", "http://cast.egofm.de/egofm.mp3?ar-distributor=ffa0&aw_0_req.gdpr=true"),
            st("Couleur3", "http://stream.srg-ssr.ch/m/couleur3/aacp_96"),
            st("Indie X FM", "http://kathy.torontocast.com:2690/stream"),
        ],
    },
    Section {
        name: "Electronic & Dance",
        stations: &[
            st("Dance Wave!", "https://dancewave.online/dance.mp3"),
            st("Dance Wave Retro!", "https://dancewave.online/retrodance.mp3"),
            st("Intense Radio", "https://secure.live-streams.nl/main"),
            st("Sunshine Live", "http://stream.sunshine-live.de/live/mp3-192/stream.sunshine-live.de/"),
            st("Sunshine Live Classics", "http://stream.sunshine-live.de/classics/mp3-192/stream.sunshine-live.de/"),
            st("Mixadance FM", "https://stream.mixadance.fm/mixadance"),
            st("Heart Dance", "https://media-ssl.musicradio.com/HeartDanceMP3"),
            st("LOS40 Dance", "http://playerservices.streamtheworld.com/api/livestream-redirect/LOS40_DANCE_SC"),
            st("Ibiza Global Radio", "http://ibizaglobalradio.streaming-pro.com:8024/"),
            st("FFH Eurodance", "http://mp3.ffh.de/ffhchannels/hqeurodance.mp3?aw_0_req.gdpr=true"),
        ],
    },
    Section {
        name: "House",
        stations: &[
            st("1.FM Deep House", "http://strm112.1.fm/deephouse_mobile_mp3"),
            st("DHR Deep House Radio", "http://live.dancemusic.ro:7000/radio.mp3"),
            st("Costa del Mar Deep House", "http://stream.cdm-chillout.com:8012/stream-mp3-Deep"),
            st("Technolovers Deep House", "https://stream.technolovers.fm/deep-house?ref=radiobrowser"),
            st("Sunshine Live House", "http://stream.sunshine-live.de/house/mp3-192/stream.sunshine-live.de/"),
            st("Sunshine Live Tech House", "http://stream.sunshine-live.de/sp4/mp3-192/stream.sunshine-live.de/"),
            st("REYFM #houseparty", "https://listen.reyfm.de/houseparty_192kbps.mp3"),
            st("Radioparty.pl House", "http://house.radioparty.pl:8010/"),
            st("SomaFM Beat Blender", "https://ice4.somafm.com/beatblender-128-mp3"),
            st("SLAM!", "http://stream.slam.nl/slam"),
        ],
    },
    Section {
        name: "Techno",
        stations: &[
            st("Techno.FM", "http://stream.techno.fm/radio.mp3"),
            st("Sunshine Live Techno", "http://stream.sunshine-live.de/techno/mp3-192/stream.sunshine-live.de/"),
            st("Technolovers Techno", "https://stream.technolovers.fm/techno?ref=radiobrowser"),
            st("90s90s Techno", "http://streams.90s90s.de/techno/mp3-192/radiode/"),
            st("Minimal Mix Radio", "http://orion.shoutca.st:8750/"),
            st("Deep Radio", "http://stream.deep.radio/hd"),
            st("RauteMusik Club", "http://club-high.rautemusik.fm/"),
            st("0N Electro", "https://0n-electro.radionetz.de/0n-electro.mp3"),
            st("Frisky", "https://stream.frisky.friskyradio.com/frisky_mp3_high"),
            st("Hardcore Radio", "http://shoutcast1.hardcoreradio.nl/"),
        ],
    },
    Section {
        name: "Trance",
        stations: &[
            st("1.FM Amsterdam Trance", "http://strm112.1.fm/atr_mobile_mp3"),
            st("1.FM Absolute Trance", "http://strm112.1.fm/trance_mobile_mp3"),
            st("1.FM Psytrance", "http://strm112.1.fm/psytrance_mobile_mp3"),
            st("TranceBase.FM", "http://listen.trancebase.fm/tunein-aac-hd-pls"),
            st("RauteMusik Trance", "https://trance-high.rautemusik.fm/?ref=radiobrowser"),
            st("DFM Vocal Trance", "https://dfm-trance.hostingradio.ru/trance96.aacp"),
            st("Discover Trance Radio", "http://paris.discovertrance.com:8006//"),
            st("1Mix Radio Trance", "http://fr2.1mix.co.uk:9000/320b"),
            st("TrancePulse FM", "http://live.trance.ie:8000/stream"),
            st("PulsRadio 90's", "http://213.251.190.165/puls90HD.mp3"),
        ],
    },
    Section {
        name: "Drum & Bass",
        stations: &[
            st("Bassdrive", "http://au.bassdrive.co:8000/"),
            st("Sunshine Live Drum 'n' Bass", "http://stream.sunshine-live.de/dnb/mp3-192/stream.sunshine-live.de/"),
            st("Technolovers Drum 'n' Bass", "https://stream.technolovers.fm/drummnbass?ref=radiobrowser"),
            st("DnB&EDM", "https://edmdnb.com:448/radio/8000/radio.mp3"),
            st("DNB FM", "https://air.dnbfm.ru/listen/player/play"),
            st("UK Bass Radio", "https://www.ukbassradio.com/stream"),
            st("DnBRadio Darkstep", "http://trace.dnbradio.com:8000/dnbradio_darkstep.mp3"),
            st("Dutch Delite Drum and Bass", "http://radio.dutchdelite.nl:8000/dnb"),
            st("Radio Caprice Drum 'n' Bass", "http://79.111.14.76:8000/drumbass"),
            st("BPMcast", "http://stream.laut.fm/bpmcast"),
        ],
    },
    Section {
        name: "Chillout & Ambient",
        stations: &[
            st("SomaFM Groove Salad", "https://ice5.somafm.com/groovesalad-128-mp3"),
            st("SomaFM Drone Zone", "https://ice5.somafm.com/dronezone-128-mp3"),
            st("Smooth Chill", "https://media-ssl.musicradio.com/ChillMP3"),
            st("Antenne Bayern Chillout", "http://mp3channels.webradio.antenne.de/chillout"),
            st("Sunshine Live Chillout", "http://stream.sunshine-live.de/sp8/mp3-192/stream.sunshine-live.de/"),
            st("Costa del Mar Chill Out", "http://stream.cdm-chillout.com:8020/stream-mp3-Chill"),
            st("Radio Art Chillout", "http://air.radioart.com/fChillout.mp3"),
            st("Frisky Chill", "http://chill.friskyradio.com/friskychill_mp3_high"),
            st("Stereoscenic Ambient Modern", "http://radio.stereoscenic.com/mod-h"),
            st("Chill FM Romania", "http://edge126.rdsnet.ro:84/profm/chillfm.mp3"),
        ],
    },
    Section {
        name: "Jazz",
        stations: &[
            st("FIP Jazz", "http://icecast.radiofrance.fr/fipjazz-hifi.aac"),
            st("Radio Swiss Jazz", "http://stream.srg-ssr.ch/m/rsj/mp3_128"),
            st("Jazz London Radio", "http://radio.canstream.co.uk:8075/live.mp3"),
            st("WDCB Jazz & Blues Chicago", "http://wdcb-ice.streamguys.org/wdcb128"),
            st(".977 Smooth Jazz", "http://29319.live.streamtheworld.com/977_SMOOJAZZ_SC"),
            st("101 Smooth Jazz", "http://jking.cdnstream1.com/b22139_128mp3"),
            st("Smooth Jazz 24/7", "http://jking.cdnstream1.com/b75154_128mp3"),
            st("Jazz Radio Only Women", "http://jazz-wr16.ice.infomaniak.ch/jazz-wr16-128.mp3"),
            st("100% Acid Jazz", "https://mpc1.mediacp.eu:8356/stream"),
            st("SomaFM Secret Agent", "https://ice6.somafm.com/secretagent-128-mp3"),
        ],
    },
    Section {
        name: "Blues",
        stations: &[
            st("1.FM Blues", "http://strm112.1.fm/blues_mobile_mp3"),
            st("181.FM True Blues", "http://listen.181fm.com/181-blues_128k.mp3"),
            st("WWOZ 90.7 New Orleans", "http://wwoz-sc.streamguys.com/wwoz-hi.mp3"),
            st("Allzic Blues", "http://allzic09.ice.infomaniak.ch/allzic09.mp3"),
            st("All Blues Radio", "http://equinox.shoutca.st:8788/;"),
            st("laut.fm BluesRockCafe", "http://stream.laut.fm/bluesrockcafe"),
            st("laut.fm Blues", "http://stream.laut.fm/blues"),
            st("Radio Caprice Texas Blues", "http://79.111.14.76:8004/texasblues"),
            st("A Mississippi Blues", "https://cast1.torontocast.com:4450/;"),
            st("Pinguin Blues", "https://samcloud.spacial.com/api/listen?sid=93462&m=sc&rid=168006"),
        ],
    },
    Section {
        name: "Soul, Funk & R&B",
        stations: &[
            st("SomaFM Seven Inch Soul", "https://ice4.somafm.com/7soul-128-mp3"),
            st("SomaFM Fluid", "https://ice6.somafm.com/fluid-128-mp3"),
            st("181.FM Soul", "https://listen.181fm.com/181-soul_128k.mp3"),
            st("Jazz Radio Soul", "http://jazz-wr10.ice.infomaniak.ch/jazz-wr10-128.mp3"),
            st("Funky Radio", "https://funkyradio.streamingmedia.it/audio.aac"),
            st("54 Funk Soul Dance", "http://54-funk-soul-dance.stream.laut.fm/54-funk-soul-dance"),
            st("The Funky Channel", "http://cast3.my-control-panel.com:8170/stream"),
            st(".977 Jamz", "http://26343.live.streamtheworld.com:3690/977_JAMZ_SC"),
            st("WEFUNK", "https://s-09.wefunkradio.com:8443/wefunk64.mp3"),
            st("0N Disco", "https://0n-disco.radionetz.de/0n-disco.mp3"),
        ],
    },
    Section {
        name: "Hip-Hop",
        stations: &[
            st("FIP Hip Hop", "http://icecast.radiofrance.fr/fiphiphop-hifi.aac"),
            st("181.FM Old School Hip-Hop/R&B", "http://listen.181fm.com/181-oldschool_128k.mp3"),
            st("Generations", "http://generationfm.ice.infomaniak.ch/generationfm-high.mp3"),
            st("Urban Radio Hip Hop / R&B", "http://hydra.cdnstream.com/1536_128"),
            st("GotRadio Hip Hop Stop", "http://pureplay.cdnstream1.com/6041_128.mp3"),
            st("All Underground Hip Hop", "http://stream.radiojar.com/c1912tk5rtzuv"),
            st("Kiss FM Oldschool Hip-Hop", "http://topradio-de-hz-fal-stream03-cluster01.radiohost.de/kissfm-hiphop_aac-64?"),
            st("laut.fm deutschrap", "http://stream.laut.fm/deutschrap"),
            st("Top Urbano", "https://radio.dominiserver.com/proxy/topurbano?mp=/stream"),
            st("Rap/Hip Hop", "http://185.32.188.17:8097/stream"),
        ],
    },
    Section {
        name: "Reggae",
        stations: &[
            st("SomaFM Heavyweight Reggae", "https://ice5.somafm.com/reggae-256-mp3"),
            st("FIP Reggae", "http://icecast.radiofrance.fr/fipreggae-hifi.aac"),
            st("La Grosse Radio Reggae", "http://hd.lagrosseradio.info/lagrosseradio-reggae-192.mp3"),
            st("181.FM Reggae Roots", "https://listen.181fm.com/181-reggae_128k.mp3"),
            st("Reggae Radio Rastamusic", "http://origin-rastamusic.streamguys1.com/rastamusic.mp3"),
            st("Rastapuls Reggae", "http://radio.rastapuls.com:8000/rastapuls"),
            st("Generations Reggae", "http://generations-caraibes.ice.infomaniak.ch/generations-caraibes-high.mp3"),
            st("Reggae141", "http://hestia2.cdnstream.com/1301_128"),
            st("King Dub Radio", "http://london-dedicated.myautodj.com:8862/stream"),
            st("Bob's Ska Radio", "http://51.255.235.165:5528/stream"),
        ],
    },
    Section {
        name: "Classical",
        stations: &[
            st("Classic FM", "http://media-ice.musicradio.com/ClassicFMMP3"),
            st("Radio Swiss Classic", "http://stream.srg-ssr.ch/m/rsc_de/mp3_128"),
            st("BR-Klassik", "https://dispatcher.rndfnk.com/br/brklassik/live/mp3/mid"),
            st("WQXR New York", "https://stream.wqxr.org/wqxr"),
            st("Klassik Radio", "http://stream.klassikradio.de/live/mp3-192/stream.klassikradio.de/"),
            st("Rai Radio 3", "http://icestreaming.rai.it/3.mp3"),
            st("Venice Classic Radio", "https://uk2.streamingpulse.com/ssl/vcr1"),
            st("Your Classical", "http://cms.stream.publicradio.org/cms.mp3"),
            st("France Musique Classique Easy", "http://direct.francemusique.fr/live/francemusiqueeasyclassique-hifi.mp3"),
            st("Concertzender Baroque", "http://streams.greenhost.nl:8080/barok"),
        ],
    },
    Section {
        name: "Country & Folk",
        stations: &[
            st(".977 Country", "http://15113.live.streamtheworld.com:3690/977_COUNTRY_SC"),
            st("181.FM Highway 181", "https://listen.181fm.com/181-highway_128k.mp3"),
            st("181.FM Real Country", "http://listen.181fm.com/181-realcountry_128k.mp3"),
            st("181.FM 90's Country", "https://listen.181fm.com/181-90scountry_128k.mp3"),
            st("1.FM Classic Country", "http://strm112.1.fm/ccountry_mobile_mp3"),
            st("Country 108", "http://tuner.country108.com/;"),
            st("Radio Country Live New York", "https://streaming.radiostreamlive.com/radiocountrylive_devices"),
            st("Today's Hot Country", "http://ice.stream101.com:9016/stream"),
            st("SomaFM Boot Liquor", "https://ice4.somafm.com/bootliquor-320-mp3"),
            st("SomaFM Folk Forward", "https://ice4.somafm.com/folkfwd-128-mp3"),
        ],
    },
    Section {
        name: "Oldies (60s–80s)",
        stations: &[
            st("Gold", "https://media-ssl.musicradio.com/GoldMP3"),
            st(".977 Oldies", "http://29318.live.streamtheworld.com:3690/977_OLDIES_SC"),
            st("0N 60s", "https://0n-60s.radionetz.de/0n-60s.mp3"),
            st("0N 70s", "https://0n-70s.radionetz.de/0n-70s.mp3"),
            st("laut.fm 1000 Oldies", "http://stream.laut.fm/1000oldies"),
            st("laut.fm 1000 Discohits", "http://stream.laut.fm/1000discohits"),
            st("Oldies Internet Radio", "http://162.244.81.98:8140/listen"),
            st("Classic Vinyl HD", "https://icecast.walmradio.com:8443/classic"),
            st("Poptarisznya Oldies", "http://adas.poptarisznya.hu:8200/oldies.mp3"),
            st("Radio Nostalgija", "http://nostalgie128.streaming.rs:9250/"),
        ],
    },
    Section {
        name: "News & Talk",
        stations: &[
            st("BBC World Service", "http://stream.live.vc.bbcmedia.co.uk/bbc_world_service"),
            st("NPR 24 Hour Program Stream", "http://npr-ice.streamguys1.com/live.mp3"),
            st("WNYC 93.9 FM", "https://fm939.wnyc.org/wnycfm"),
            st("KUOW Seattle", "https://playerservices.streamtheworld.com/api/livestream-redirect/KUOWFM_HIGH_MP3.mp3"),
            st("LBC London", "http://media-ice.musicradio.com/LBCUKMP3Low"),
            st("Deutschlandfunk", "https://st01.sslstream.dlf.de/dlf/01/128/mp3/stream.mp3?aggregator=web"),
            st("France Info", "http://icecast.radiofrance.fr/franceinfo-midfi.mp3"),
            st("France Inter", "http://icecast.radiofrance.fr/franceinter-midfi.mp3"),
            st("France Culture", "http://icecast.radiofrance.fr/franceculture-midfi.mp3"),
            st("Rai Radio 1", "http://icestreaming.rai.it/1.mp3"),
        ],
    },
];

/// Stations grouped by language rather than genre. The Latin name in brackets lets a search
/// typed without a Cyrillic keyboard ("humor", "jumor") find them.
pub static RUSSIAN: Section = Section {
    name: "Russian / Русские",
    stations: &[
        st("Юмор FM (Humor FM, Jumor FM)", "http://pub0101.101.ru:8000/stream/air/aac/64/102"),
        st("Юмор FM Беларусь (Humor FM Belarus)", "http://live.humorfm.by:8000/veseloeradio"),
        st("Русское Радио (Russkoe Radio)", "https://rusradio.hostingradio.ru/rusradio128.mp3"),
        st("Европа Плюс (Europa Plus)", "http://ep128server.streamr.ru:8030/ep128"),
        st("Ретро FM (Retro FM)", "http://retroserver.streamr.ru:8043/retro128"),
        st("Наше Радио (Nashe Radio)", "http://nashe1.hostingradio.ru/nashe-128.mp3"),
        st("Радио Шансон (Radio Chanson)", "http://chanson.hostingradio.ru:8041/chanson128.mp3"),
        st("Дорожное радио (Dorozhnoe Radio)", "http://dorognoe.hostingradio.ru:8000/dorognoe"),
        st("Радио Рекорд Russian Mix (Radio Record)", "http://radiorecord.hostingradio.ru/rus96.aacp"),
        st("Маяк (Mayak)", "http://icecast.vgtrk.cdnvideo.ru/mayakfm_mp3_128kbps"),
    ],
};

impl Section {
    /// Whether the station at `url` is one of this section's, to mark the section that is playing.
    pub fn contains(&self, url: &str) -> bool {
        self.stations.iter().any(|s| s.url == url)
    }
}

/// Every section in menu order: the genres, then the Russian section.
pub fn sections() -> impl Iterator<Item = &'static Section> {
    GENRES.iter().chain(std::iter::once(&RUSSIAN))
}

/// Stations whose name or section name contains every word of `query`, ignoring case (also
/// for Cyrillic), in menu order. A blank query finds nothing: the menu shows the sections then.
pub fn search(query: &str) -> Vec<(&'static Section, &'static Station)> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    if words.is_empty() {
        return Vec::new();
    }
    sections()
        .flat_map(|section| section.stations.iter().map(move |station| (section, station)))
        .filter(|(section, station)| {
            let haystack = format!("{} {}", station.name, section.name).to_lowercase();
            words.iter().all(|w| haystack.contains(w.as_str()))
        })
        .collect()
}

/// The catalog entry for an address, so the menu can mark the station that is playing.
pub fn find_by_url(url: &str) -> Option<(&'static Section, &'static Station)> {
    sections().find_map(|section| section.stations.iter().find(|s| s.url == url).map(|s| (section, s)))
}
