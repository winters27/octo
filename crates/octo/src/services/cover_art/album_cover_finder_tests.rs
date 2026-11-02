//! The album cover finder: the three sources against one mock server standing for Apple, the
//! Cover Art Archive, Deezer and their CDNs.

use std::io::Cursor;
use std::sync::Arc;

use octo_core::common::Clock;
use octo_core::settings::{AppSettings, SettingsStore};
use wiremock::matchers::path;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::services::metadata::{DeezerRateLimitHandler, DeezerRateLimiter};

fn jpeg(width: u32, height: u32) -> Vec<u8> {
    let image = image::RgbImage::from_pixel(width, height, image::Rgb([90, 90, 90]));
    let mut out = Cursor::new(Vec::new());
    image
        .write_to(&mut out, image::ImageFormat::Jpeg)
        .expect("a JPEG encodes");
    out.into_inner()
}

fn finder(server: &MockServer) -> AlbumCoverFinder {
    let settings = Arc::new(SettingsStore::for_tests(AppSettings::default()));
    let deezer = DeezerMetadataService::with_base_url(
        Arc::new(DeezerRateLimitHandler::new(Arc::new(DeezerRateLimiter::new()))),
        settings,
        server.uri(),
    );
    AlbumCoverFinder::new(
        Arc::new(ITunesCoverArtLookup::with_base_url(
            None,
            &server.uri(),
            Clock::system(),
        )),
        Arc::new(CoverArtArchiveLookup::with_base_url(
            url::Url::parse(&format!("{}/", server.uri())).expect("the mock address parses"),
        )),
        Arc::new(deezer),
        reqwest::Client::new(),
    )
    .with_apple_batch_idle(Duration::from_secs(1))
}

async fn serve(server: &MockServer, at: &str, body: Vec<u8>) {
    Mock::given(path(at))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
        .mount(server)
        .await;
}

async fn serve_json(server: &MockServer, at: &str, body: String) {
    Mock::given(path(at))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(server)
        .await;
}

fn deezer_hits(server: &MockServer, id: i64, title: &str, cover: &str) -> String {
    format!(
        r#"{{"data":[{{"id":{id},"title":"{title}","artist":{{"name":"Daft Punk"}},"cover_xl":"{}{cover}"}}]}}"#,
        server.uri()
    )
}

/// All three are asked, a cover that is not square is passed over, and the largest left wins.
#[tokio::test]
async fn the_largest_square_cover_of_the_three_sources_wins() {
    let server = MockServer::start().await;
    serve_json(&server, "/search", r#"{"results":[]}"#.into()).await;
    serve(&server, "/release/rel-1/front-1200", jpeg(1200, 1200)).await;
    serve_json(
        &server,
        "/search/album",
        deezer_hits(&server, 7, "Discovery", "/cdn/discovery.jpg"),
    )
    .await;
    serve(&server, "/cdn/discovery.jpg", jpeg(1000, 1000)).await;
    let finder = finder(&server);

    let query = AlbumCoverQuery {
        music_brainz_release_id: Some("rel-1".into()),
        ..AlbumCoverQuery::new(
            "Daft Punk",
            Some("Discovery".into()),
            Some("One More Time".into()),
        )
    };
    let found = finder.find(&query).await.expect("a cover");
    assert_eq!((found.source.as_str(), found.side), ("Cover Art Archive", 1200));

    // Without the release id only the catalog answers.
    let found = finder
        .find(&AlbumCoverQuery::new("Daft Punk", Some("Discovery".into()), None))
        .await
        .expect("a cover");
    assert_eq!((found.source.as_str(), found.side), ("Deezer", 1000));
}

/// A turned-down picture is passed over for the next largest, and so is any picture that looks
/// like it (the two sources here serve the same grey artwork at two sizes).
#[tokio::test]
async fn a_rejected_cover_and_its_look_alikes_are_passed_over() {
    let server = MockServer::start().await;
    serve_json(&server, "/search", r#"{"results":[]}"#.into()).await;
    let largest = jpeg(1200, 1200);
    serve(&server, "/release/rel-1/front-1200", largest.clone()).await;
    serve_json(
        &server,
        "/search/album",
        deezer_hits(&server, 7, "Discovery", "/cdn/discovery.jpg"),
    )
    .await;
    serve(&server, "/cdn/discovery.jpg", jpeg(1000, 1000)).await;
    let finder = finder(&server);
    let query = AlbumCoverQuery {
        music_brainz_release_id: Some("rel-1".into()),
        ..AlbumCoverQuery::new("Daft Punk", Some("Discovery".into()), None)
    };

    let by_hash = RejectedCovers {
        hashes: [RejectedCovers::sha256(&largest)].into(),
        looks: Vec::new(),
    };
    let found = finder
        .find_excluding(&query, &by_hash)
        .await
        .expect("the next cover");
    assert_eq!((found.source.as_str(), found.side), ("Deezer", 1000));

    let by_look = RejectedCovers {
        looks: vec![cover_image::looks_hash(Some(&largest)).expect("a look")],
        ..by_hash
    };
    assert!(finder.find_excluding(&query, &by_look).await.is_none());
}

#[tokio::test]
async fn a_wide_picture_is_no_cover() {
    let server = MockServer::start().await;
    serve_json(&server, "/search", r#"{"results":[]}"#.into()).await;
    serve_json(
        &server,
        "/search/album",
        deezer_hits(&server, 7, "Discovery", "/cdn/wide.jpg"),
    )
    .await;
    serve(&server, "/cdn/wide.jpg", jpeg(1600, 900)).await;
    let finder = finder(&server);

    assert_eq!(
        finder
            .find(&AlbumCoverQuery::new("Daft Punk", Some("Discovery".into()), None))
            .await,
        None
    );
}

/// Priming takes one album's barcode from its tags and the other's from the catalog, asks
/// Apple about both in one lookup, says what it is doing, and a find afterwards fetches
/// Apple's master without a search.
#[tokio::test]
async fn priming_matches_barcodes_at_apple_in_bulk() {
    let server = MockServer::builder().start().await;
    let art = |name: &str| format!("{}/image/{name}/100x100bb.jpg", server.uri());
    serve_json(
        &server,
        "/lookup",
        format!(
            r#"{{"results":[
                {{"wrapperType":"collection","artistName":"Daft Punk","collectionName":"Discovery","artworkUrl100":"{}"}},
                {{"wrapperType":"collection","artistName":"Daft Punk","collectionName":"Homework","artworkUrl100":"{}"}}
            ]}}"#,
            art("discovery"),
            art("homework")
        ),
    )
    .await;
    serve_json(
        &server,
        "/search/album",
        deezer_hits(&server, 8, "Homework", "/cdn/homework.jpg"),
    )
    .await;
    serve_json(&server, "/album/8", r#"{"id":8,"upc":"724384960651"}"#.into()).await;
    serve(&server, "/cdn/homework.jpg", jpeg(500, 500)).await;
    serve(&server, "/image/discovery/5000x5000bb.jpg", jpeg(1500, 1500)).await;
    let finder = finder(&server);

    let said = parking_lot::Mutex::new(Vec::<String>::new());
    let status = |text: String| said.lock().push(text);
    let albums = [
        AlbumCoverQuery {
            barcode: Some("0724384960650".into()),
            ..AlbumCoverQuery::new("Daft Punk", Some("Discovery".into()), None)
        },
        AlbumCoverQuery::new("Daft Punk", Some("Homework".into()), None),
        // Twice the same album, and one with no name: neither is primed again.
        AlbumCoverQuery::new("Daft Punk", Some("Homework".into()), None),
        AlbumCoverQuery::new("Daft Punk", None, Some("Da Funk".into())),
    ];
    finder.prime(&albums, Some(&status)).await.expect("primed");

    let requests = server.received_requests().await.unwrap_or_default();
    let lookups: Vec<&wiremock::Request> = requests.iter().filter(|r| r.url.path() == "/lookup").collect();
    assert_eq!(lookups.len(), 1);
    let upc = lookups[0]
        .url
        .query_pairs()
        .find(|(name, _)| name == "upc")
        .map(|(_, value)| value.into_owned())
        .unwrap_or_default();
    assert_eq!(upc, "0724384960650,724384960650,724384960651");
    assert_eq!(
        said.lock().last().map(String::as_str),
        Some("barcodes 2 of 2 · Apple matched 2")
    );

    let found = finder
        .find(&AlbumCoverQuery::new("Daft Punk", Some("Discovery".into()), None))
        .await
        .expect("a cover");
    assert_eq!((found.source.as_str(), found.side), ("iTunes", 1500));
    let requests = server.received_requests().await.unwrap_or_default();
    assert!(
        !requests.iter().any(|r| r.url.path() == "/search"),
        "Apple was not searched"
    );
}

/// A preview takes Apple's small copy, with the master's size read from its first bytes.
#[tokio::test]
async fn a_preview_reports_the_masters_size_with_a_small_copy() {
    let server = MockServer::start().await;
    serve_json(
        &server,
        "/search",
        format!(
            r#"{{"results":[{{"wrapperType":"collection","artistName":"Daft Punk","collectionName":"Discovery","artworkUrl100":"{}/image/d/100x100bb.jpg"}}]}}"#,
            server.uri()
        ),
    )
    .await;
    serve(&server, "/image/d/5000x5000bb.jpg", jpeg(3000, 3000)).await;
    serve(&server, "/image/d/320x320bb.jpg", jpeg(320, 320)).await;
    serve_json(&server, "/search/album", r#"{"data":[]}"#.into()).await;
    let finder = finder(&server);

    let found = finder
        .preview(&AlbumCoverQuery::new("Daft Punk", Some("Discovery".into()), None))
        .await
        .expect("a cover");
    assert_eq!((found.source.as_str(), found.side), ("iTunes", 3000));
    assert_eq!(cover_image::measure(&found.bytes), Some((320, 320)));
}

#[test]
fn n0_groups_thousands() {
    assert_eq!(
        [n0(0), n0(12), n0(1234), n0(1_234_567)],
        ["0", "12", "1,234", "1,234,567"]
    );
}
