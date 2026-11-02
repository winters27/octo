//! Animated art: the lookup (barcode, then search, then the album page), what is kept and for
//! how long, one ask per album however many wait, the fields the apps read, and the relay.

use std::time::Duration;

use octo_core::common::Clock;
use octo_core::settings::{AppSettings, MetadataSettings};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

const SQUARE: &str = "/itunes-assets/HLSMusic1/v4/aa/P1_default.m3u8";
const TALL: &str = "/itunes-assets/HLSMusic1/v4/bb/P2_default.m3u8";

fn store(mode: &str) -> Arc<SettingsStore> {
    Arc::new(SettingsStore::for_tests(AppSettings {
        metadata: MetadataSettings {
            animated_art: mode.into(),
            ..Default::default()
        },
        ..Default::default()
    }))
}

/// An album page shaped as Apple's: the clips sit deep in the server data.
fn album_page(square: bool, tall: bool) -> String {
    let clip = |key: &str, video: &str, height: u32| {
        format!(
            r#""{key}":{{"previewFrame":{{"bgColor":"03091e","height":{height},"width":2000,"url":"https://is1-ssl.mzstatic.com/image/thumb/Video/x/{{w}}x{{h}}bb.{{f}}"}},"video":"https://mvod.itunes.apple.com{video}"}}"#
        )
    };
    let mut data = String::from(r#"{"data":[{"shelf":"#);
    if square {
        data.push_str(&format!(
            r#"{{"videoArtwork":{{"dictionary":{{{}}}}}}},"#,
            clip("motionDetailSquare", SQUARE, 2000)
        ));
    }
    if tall {
        data.push_str(&format!(
            r#"{{"tallVideoArtwork":{{"dictionary":{{{}}}}}}},"#,
            clip("motionDetailTall", TALL, 2666)
        ));
    }
    data.push_str(r#""end"}]}"#);
    format!(
        "<html><head></head><body><script type=\"application/json\" id=\"serialized-server-data\">{data}</script>{}</body></html>",
        "x".repeat(4096)
    )
}

fn search_answer(artist: &str, album: &str, id: u64) -> String {
    format!(
        r#"{{"results":[
            {{"wrapperType":"collection","collectionId":999,"artistName":"Karaoke Stars","collectionName":"{album}","artworkUrl100":"https://a/100x100bb.jpg"}},
            {{"wrapperType":"collection","collectionId":{id},"artistName":"{artist}","collectionName":"{album}","artworkUrl100":"https://a/100x100bb.jpg"}}
        ]}}"#
    )
}

struct Apple {
    itunes: MockServer,
    music: MockServer,
}

impl Apple {
    async fn new() -> Self {
        Apple {
            itunes: MockServer::start().await,
            music: MockServer::start().await,
        }
    }

    fn covers(&self, mode: &str, path: Option<PathBuf>) -> Arc<AnimatedArt> {
        let itunes = Arc::new(ITunesCoverArtLookup::with_base_url(
            None,
            &self.itunes.uri(),
            Clock::system(),
        ));
        let pages = Arc::new(AppleMusicPages::with_base_url(&self.music.uri(), Duration::ZERO));
        Arc::new(AnimatedArt::new(store(mode), itunes, pages, path))
    }

    async fn page(&self, id: u64, body: String, delay: Duration) {
        Mock::given(path(format!("/us/album/{id}")))
            .respond_with(ResponseTemplate::new(200).set_body_string(body).set_delay(delay))
            .mount(&self.music)
            .await;
    }

    async fn search(&self, artist: &str, album: &str, id: u64) {
        Mock::given(path("/search"))
            .respond_with(ResponseTemplate::new(200).set_body_string(search_answer(artist, album, id)))
            .mount(&self.itunes)
            .await;
    }

    async fn asked(server: &MockServer, at: &str) -> usize {
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.url.path() == at)
            .count()
    }
}

#[tokio::test]
async fn an_album_is_matched_and_both_loops_are_read_from_its_page() {
    let apple = Apple::new().await;
    apple.search("Taylor Swift", "Midnights", 1650841512).await;
    apple
        .page(1650841512, album_page(true, true), Duration::ZERO)
        .await;
    let covers = apple.covers("direct", None);

    let ArtLookup::Found(cover) = covers
        .find("Taylor Swift", "Midnights", None, Duration::from_secs(5))
        .await
    else {
        panic!("found");
    };
    let square = cover.square.clone().expect("square");
    assert_eq!(square.video, format!("https://mvod.itunes.apple.com{SQUARE}"));
    assert_eq!(
        square.poster.as_deref(),
        Some("https://is1-ssl.mzstatic.com/image/thumb/Video/x/1200x1200bb.jpg")
    );
    assert_eq!(square.color.as_deref(), Some("03091e"));
    let tall = cover.tall.clone().expect("tall");
    assert_eq!((tall.width, tall.height), (2000, 2666));
    assert_eq!(
        tall.poster.as_deref(),
        Some("https://is1-ssl.mzstatic.com/image/thumb/Video/x/1200x1599bb.jpg")
    );

    // Kept: the next ask costs Apple nothing.
    assert_eq!(covers.peek("taylor swift", "MIDNIGHTS", None), Some(cover));
    assert_eq!(Apple::asked(&apple.itunes, "/search").await, 1);
    assert_eq!(Apple::asked(&apple.music, "/us/album/1650841512").await, 1);
}

#[tokio::test]
async fn a_barcode_finds_the_release_without_a_search() {
    let apple = Apple::new().await;
    Mock::given(path("/lookup"))
        .and(query_param("entity", "album"))
        .respond_with(ResponseTemplate::new(200).set_body_string(search_answer("Lorde", "Melodrama", 42)))
        .mount(&apple.itunes)
        .await;
    apple.page(42, album_page(true, false), Duration::ZERO).await;
    let covers = apple.covers("direct", None);

    let found = covers
        .find(
            "Lorde",
            "Melodrama",
            Some("0602557530032"),
            Duration::from_secs(5),
        )
        .await;
    let ArtLookup::Found(cover) = found else {
        panic!("found");
    };
    assert!(cover.square.is_some() && cover.tall.is_none());
    assert_eq!(Apple::asked(&apple.itunes, "/search").await, 0);
}

#[tokio::test]
async fn many_asks_for_one_album_at_once_cost_apple_one_lookup() {
    let apple = Apple::new().await;
    apple.search("Lorde", "Melodrama", 42).await;
    apple
        .page(42, album_page(true, true), Duration::from_millis(150))
        .await;
    let covers = apple.covers("direct", None);

    let asks: Vec<_> = (0..12)
        .map(|_| {
            let covers = covers.clone();
            tokio::spawn(async move {
                covers
                    .find("Lorde", "Melodrama", None, Duration::from_secs(5))
                    .await
            })
        })
        .collect();
    for ask in asks {
        assert!(matches!(ask.await.expect("joins"), ArtLookup::Found(_)));
    }
    assert_eq!(Apple::asked(&apple.itunes, "/search").await, 1);
    assert_eq!(Apple::asked(&apple.music, "/us/album/42").await, 1);
}

#[tokio::test]
async fn peek_never_waits_and_has_the_answer_the_next_time() {
    let apple = Apple::new().await;
    apple.search("Lorde", "Melodrama", 42).await;
    apple.page(42, album_page(true, true), Duration::ZERO).await;
    let covers = apple.covers("direct", None);

    assert!(
        covers.peek("Lorde", "Melodrama", None).is_none(),
        "nothing known yet"
    );
    for _ in 0..100 {
        if covers.peek("Lorde", "Melodrama", None).is_some() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the background lookup never landed");
}

#[tokio::test]
async fn no_loop_is_kept_as_none_and_an_outage_is_not_kept() {
    let apple = Apple::new().await;
    apple.search("Lorde", "Melodrama", 42).await;
    apple.page(42, album_page(false, false), Duration::ZERO).await;
    let covers = apple.covers("direct", None);
    assert_eq!(
        covers
            .find("Lorde", "Melodrama", None, Duration::from_secs(5))
            .await,
        ArtLookup::None
    );
    assert_eq!(
        covers
            .find("Lorde", "Melodrama", None, Duration::from_secs(5))
            .await,
        ArtLookup::None
    );
    assert_eq!(
        Apple::asked(&apple.music, "/us/album/42").await,
        1,
        "none is kept"
    );

    // iTunes answering 503 to every ask is an outage: asked again next time.
    let down = Apple::new().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&down.itunes)
        .await;
    let covers = down.covers("direct", None);
    for _ in 0..2 {
        assert_eq!(
            covers
                .find("Lorde", "Melodrama", None, Duration::from_secs(5))
                .await,
            ArtLookup::Unavailable
        );
    }
    assert_eq!(Apple::asked(&down.itunes, "/search").await, 2);
}

#[tokio::test]
async fn off_asks_nothing_and_offers_nothing() {
    let apple = Apple::new().await;
    let covers = apple.covers("off", None);
    assert!(!covers.offered());
    assert!(covers.peek("Lorde", "Melodrama", None).is_none());
    assert_eq!(
        covers
            .find("Lorde", "Melodrama", None, Duration::from_secs(1))
            .await,
        ArtLookup::Unavailable
    );
    assert!(
        apple
            .itunes
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty()
    );
}

fn clip(video: &str) -> Clip {
    Clip {
        video: format!("https://mvod.itunes.apple.com{video}"),
        poster: Some("https://is1-ssl.mzstatic.com/image/thumb/x/1200x1200bb.jpg".into()),
        color: Some("ffffff".into()),
        width: 2000,
        height: 2000,
    }
}

#[tokio::test]
async fn the_field_is_apples_addresses_direct_and_this_servers_relayed() {
    let apple = Apple::new().await;
    let direct = apple.covers("direct", None);
    direct.keep("Lorde", "Melodrama", Some(clip(SQUARE)), None);
    let cover = direct.peek("Lorde", "Melodrama", None).expect("kept");
    assert_eq!(
        direct.field(&cover),
        json!({"square": {
            "video": format!("https://mvod.itunes.apple.com{SQUARE}"),
            "poster": "https://is1-ssl.mzstatic.com/image/thumb/x/1200x1200bb.jpg",
            "color": "ffffff", "width": 2000, "height": 2000,
        }})
    );

    let relayed = apple.covers("relay", None);
    let token = relayed.keep("Lorde", "Melodrama", Some(clip(SQUARE)), Some(clip(TALL)));
    let cover = relayed.peek("Lorde", "Melodrama", None).expect("kept");
    let field = relayed.field(&cover);
    assert_eq!(
        field["square"]["video"],
        json!(format!("animated/{token}/square.m3u8"))
    );
    assert_eq!(
        field["tall"]["poster"],
        json!(format!("animated/{token}/tall.jpg"))
    );
}

#[tokio::test]
async fn kept_answers_and_tokens_survive_a_restart() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let file = dir.path().join("animated-art.json");
    let apple = Apple::new().await;
    let covers = apple.covers("relay", Some(file.clone()));
    let token = covers.keep("Lorde", "Melodrama", Some(clip(SQUARE)), None);
    covers.flush().await;

    let again = apple.covers("relay", Some(file));
    assert_eq!(
        again.peek("Lorde", "Melodrama", None).map(|c| c.token),
        Some(token.clone())
    );
    assert!(again.relay_file(&token, "square.m3u8").is_some());
    // Found again later: the same token, so addresses an app holds keep working.
    assert_eq!(again.keep("Lorde", "Melodrama", Some(clip(SQUARE)), None), token);
}

#[tokio::test]
async fn past_the_cap_the_oldest_tenth_goes_with_its_tokens() {
    let apple = Apple::new().await;
    let covers = apple.covers("relay", None);
    let first = covers.keep("Artist 0", "Album 0", Some(clip(SQUARE)), None);
    covers
        .kept
        .write()
        .get_mut(&album_key("Artist 0", "Album 0"))
        .expect("kept")
        .at = 0;
    for n in 1..=MAX_ALBUMS {
        covers.remember(
            &album_key(&format!("Artist {n}"), "Album"),
            "a",
            "b",
            LoopsKept::default(),
        );
    }
    let left = covers.kept.read().len();
    assert!((MAX_ALBUMS * 8 / 10..=MAX_ALBUMS).contains(&left), "{left}");
    assert!(
        covers.relay_file(&first, "square.m3u8").is_none(),
        "its token went with it"
    );
    assert_eq!(
        covers.tokens.read().len(),
        covers.kept.read().values().filter(|k| k.token.is_some()).count()
    );
}

#[tokio::test]
async fn the_relay_only_ever_reaches_apples_asset_path() {
    let apple = Apple::new().await;
    let covers = apple.covers("relay", None);
    let token = covers.keep("Lorde", "Melodrama", Some(clip(SQUARE)), None);

    assert!(matches!(
        covers.relay_file(&token, "square.m3u8"),
        Some(RelayFile::Playlist(_))
    ));
    assert!(matches!(
        covers.relay_file(&token, "square.jpg"),
        Some(RelayFile::Media(_))
    ));
    assert!(
        covers.relay_file(&token, "tall.m3u8").is_none(),
        "no tall loop kept"
    );
    assert!(covers.relay_file("not-a-token", "square.m3u8").is_none());

    let name = |path: &str| format!("{}.ts", URL_SAFE_NO_PAD.encode(path));
    let Some(RelayFile::Media(url)) = covers.relay_file(&token, &name("HLSMusic1/v4/aa/seg1.ts")) else {
        panic!("a segment");
    };
    assert_eq!(
        url.as_str(),
        "https://mvod.itunes.apple.com/itunes-assets/HLSMusic1/v4/aa/seg1.ts"
    );
    assert!(covers.relay_file(&token, &name("../../etc/passwd")).is_none());
    assert!(covers.relay_file(&token, &name("x/../../../other")).is_none());

    // Direct mode serves nothing here.
    let direct = apple.covers("direct", None);
    let token = direct.keep("Lorde", "Melodrama", Some(clip(SQUARE)), None);
    assert!(direct.relay_file(&token, "square.m3u8").is_none());
}

#[test]
fn a_playlist_points_every_asset_address_back_at_the_relay() {
    let base =
        Url::parse("https://mvod.itunes.apple.com/itunes-assets/HLSMusic1/v4/aa/P1_default.m3u8").unwrap();
    let playlist = concat!(
        "#EXTM3U\n",
        "#EXT-X-MEDIA:TYPE=AUDIO,URI=\"audio/index.m3u8\",NAME=\"a\"\n",
        "#EXT-X-STREAM-INF:BANDWIDTH=1000\n",
        "P1_1080.m3u8\n",
        "\n",
        "https://elsewhere.example.com/x.m3u8\n",
        "https://mvod.itunes.apple.com.evil.example/itunes-assets/x.ts\n",
    );
    let out = rewrite_playlist(playlist, &base, "https://mvod.itunes.apple.com");
    let lines: Vec<&str> = out.lines().collect();
    let named = |path: &str, ext: &str| format!("{}.{ext}", URL_SAFE_NO_PAD.encode(path));
    assert_eq!(lines[0], "#EXTM3U");
    assert_eq!(
        lines[1],
        format!(
            "#EXT-X-MEDIA:TYPE=AUDIO,URI=\"{}\",NAME=\"a\"",
            named("HLSMusic1/v4/aa/audio/index.m3u8", "m3u8")
        )
    );
    assert_eq!(lines[3], named("HLSMusic1/v4/aa/P1_1080.m3u8", "m3u8"));
    assert_eq!(lines[4], "");
    assert_eq!(
        lines[5], "https://elsewhere.example.com/x.m3u8",
        "another host is left alone"
    );
    assert_eq!(
        lines[6],
        "https://mvod.itunes.apple.com.evil.example/itunes-assets/x.ts"
    );
}

#[tokio::test]
async fn a_relayed_playlist_is_fetched_once_and_served_from_memory() {
    let apple = Apple::new().await;
    let video = MockServer::start().await;
    Mock::given(path(SQUARE))
        .respond_with(ResponseTemplate::new(200).set_body_string("#EXTM3U\nP1_1080.m3u8\n"))
        .mount(&video)
        .await;
    let covers = {
        let itunes = Arc::new(ITunesCoverArtLookup::with_base_url(
            None,
            &apple.itunes.uri(),
            Clock::system(),
        ));
        let pages = Arc::new(AppleMusicPages::with_base_url(&apple.music.uri(), Duration::ZERO));
        Arc::new(AnimatedArt::new(store("relay"), itunes, pages, None).with_video_origin(&video.uri()))
    };
    let token = covers.keep("Lorde", "Melodrama", Some(clip(SQUARE)), None);
    let Some(RelayFile::Playlist(url)) = covers.relay_file(&token, "square.m3u8") else {
        panic!("a playlist");
    };
    for _ in 0..3 {
        let text = covers.relayed_playlist(&url).await.expect("relayed");
        let expected = format!(
            "#EXTM3U\n{}.m3u8\n",
            URL_SAFE_NO_PAD.encode("HLSMusic1/v4/aa/P1_1080.m3u8")
        );
        assert_eq!(std::str::from_utf8(&text).unwrap(), expected);
    }
    assert_eq!(Apple::asked(&video, SQUARE).await, 1);
}

#[tokio::test]
async fn a_request_joining_a_sweep_lookup_turns_it_urgent() {
    let apple = Apple::new().await;
    // The match is slow to answer, so the lookup is still running when the request arrives.
    Mock::given(path("/search"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(search_answer("Lorde", "Melodrama", 1))
                .set_delay(Duration::from_secs(3)),
        )
        .mount(&apple.itunes)
        .await;
    let covers = apple.covers("direct", None);
    let key = album_key("Lorde", "Melodrama");

    // The background lookup, as the sweep and getAlbum start it.
    assert!(covers.peek("Lorde", "Melodrama", None).is_none());
    let background = covers
        .urgent
        .lock()
        .get(&key)
        .cloned()
        .expect("a lane is recorded");
    assert!(!background.is_now());

    // The same album asked for by a request.
    let lookup = covers
        .find_on(
            "Lorde",
            "Melodrama",
            None,
            Duration::from_millis(100),
            &LaneCell::now(),
        )
        .await;

    assert_eq!(
        lookup,
        ArtLookup::Unavailable,
        "still running, so the request gave up waiting"
    );
    assert!(background.is_now(), "the running lookup reads interactively now");
    assert_eq!(covers.urgent.lock().len(), 1, "one lookup, one lane");
}
