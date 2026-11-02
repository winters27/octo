//! Every cover the dashboard draws comes from Octo's own address: a fetched song's (its file,
//! else its saved cover address), an outside song's on Find songs and Listening, and an Animated
//! art still. None is a redirect to another host.

use axum::http::{Method, StatusCode};
use octo_core::models::download::DownloadHistoryEntry;
use octo_core::settings::AppSettings;
use octo_core::soulseek::{RoutingKind, SoulseekRouting};
use octo_media::cover::cover_image;
use octo_media::tags::{TagFile, TagPicture};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::test_app::{Reply, app, send};
use crate::app::{AppState, TestServices};
use crate::http::pipeline::App;
use crate::services::admin::fetched_covers::entry_cover_path;
use crate::services::cover_art::animated_art::Clip;
use crate::services::soulseek::SoulseekMetadataService;
use crate::services::test_support::flac;

const AT: &str = "2026-10-30T01:30:00.9766532Z";

fn sample_cover() -> Vec<u8> {
    std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/rust-migration/fixtures/covers/sample-cover.jpg"
    ))
    .expect("the sample cover")
}

fn side_of(picture: &[u8]) -> u32 {
    let (width, height) = cover_image::measure(picture).expect("a picture");
    width.max(height)
}

struct Dashboard {
    state: AppState,
    app: App,
    folder: tempfile::TempDir,
}

impl Dashboard {
    fn new(deezer: Option<String>) -> Dashboard {
        let folder = tempfile::tempdir().expect("a temp dir");
        let state = AppState::for_tests_with(
            AppSettings::default(),
            TestServices {
                sign_in: Some("required".into()),
                config_dir: Some(folder.path().to_path_buf()),
                deezer_base_url: deezer,
                ..Default::default()
            },
        );
        Dashboard {
            app: app(state.clone()),
            state,
            folder,
        }
    }

    async fn get(&self, token: Option<&str>, path: &str) -> Reply {
        let headers: Vec<(&str, &str)> = token.map(|t| ("X-Octo-Browse-Token", t)).into_iter().collect();
        send(&self.app, Method::GET, path, &headers, None).await
    }

    /// A fetched song whose file holds the sample cover.
    fn fetched_song(&self) -> String {
        let path = self
            .folder
            .path()
            .join("song.flac")
            .to_string_lossy()
            .into_owned();
        std::fs::write(&path, flac()).expect("written");
        let mut file = TagFile::open(&path).expect("opens");
        file.set_pictures(&[TagPicture::front_cover(sample_cover(), "image/jpeg")]);
        file.save().expect("saved");
        self.state.download_history.record(DownloadHistoryEntry {
            artist: "Zephyr Echo".into(),
            title: "Glass".into(),
            path: path.clone(),
            format: "FLAC".into(),
            source: "Soulseek".into(),
            downloaded_at: AT.into(),
            ..Default::default()
        });
        path
    }
}

fn is_cover(reply: &Reply) -> bool {
    reply.status == StatusCode::OK
        && reply.header("content-type") == Some("image/jpeg")
        && reply.header("cache-control") == Some("private, max-age=3600")
        && reply.header("location").is_none()
        && cover_image::measure(&reply.body).is_some()
}

/// The list names where each song's cover is served, and that address answers every dashboard
/// sign-in that may read the list (the recovery code too), small or large.
#[tokio::test]
async fn a_fetched_songs_cover_is_served_by_octo_to_whoever_reads_the_list() {
    let dash = Dashboard::new(None);
    dash.fetched_song();
    let admin = dash.state.browse_sessions.create("admin");
    let recovery = dash.state.browse_sessions.create_recovery();

    let list = dash.get(Some(&admin), "/api/admin/downloads").await;
    assert_eq!(list.status, StatusCode::OK, "{}", list.text());
    let cover = list.json()["downloads"][0]["cover"]
        .as_str()
        .expect("a cover address")
        .to_string();
    assert_eq!(cover, entry_cover_path(AT));

    let small = dash.get(Some(&admin), &cover).await;
    assert!(is_cover(&small), "{:?}", small.status);
    assert_eq!(side_of(&small.body), 168);
    // The sample is 300 px, under the large side, so it comes back whole.
    let large = dash.get(Some(&recovery), &format!("{cover}&size=504")).await;
    assert!(is_cover(&large), "{:?}", large.status);
    assert_eq!(side_of(&large.body), 300);

    assert_eq!(dash.get(None, &cover).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        dash.get(
            Some(&admin),
            "/api/admin/downloads/cover?at=2020-01-01T00%3A00%3A00Z"
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
}

/// A song the log saved no cover address for still lights the page and shows its cover on
/// Latest fetched, from its file.
#[tokio::test]
async fn latest_fetched_draws_and_lights_from_the_songs_file() {
    let dash = Dashboard::new(None);
    dash.fetched_song();
    let admin = dash.state.browse_sessions.create("admin");

    let reply = dash.get(Some(&admin), "/api/admin/ambient").await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let song = reply.json()["songs"][0].clone();
    assert_eq!(song["coverArtUrl"], serde_json::Value::Null);
    assert_eq!(song["cover"], json!(entry_cover_path(AT)));
    assert_eq!(song["colors"], json!(["#a967be", "#c85c87", "#727bdb"]));
    assert_eq!(song["accent"], "#e49ffa");
}

/// An outside song's cover on Find songs and Listening is fetched by Octo and answered as a
/// picture, never a redirect to the catalog's host.
#[tokio::test]
async fn an_outside_songs_cover_is_served_by_octo_not_redirected() {
    let server = MockServer::builder().start().await;
    let cover_url = format!("{}/xl.jpg", server.uri());
    Mock::given(method("GET"))
        .and(path("/search"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(
                json!({ "data": [{
                    "title": "Glass",
                    "duration": 200,
                    "album": { "id": 711108, "title": "Lights", "cover_xl": cover_url },
                    "artist": { "name": "Zephyr Echo" }
                }]})
                .to_string(),
            ),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/xl.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(sample_cover()))
        .expect(2)
        .mount(&server)
        .await;
    let dash = Dashboard::new(Some(server.uri()));
    let admin = dash.state.browse_sessions.create("admin");
    let id = SoulseekMetadataService::encode_external_id(&SoulseekRouting {
        kind: RoutingKind::Song,
        artist: Some("Zephyr Echo".into()),
        title: Some("Glass".into()),
        you_tube_id: Some("abc123".into()),
        ..Default::default()
    });
    let id: String = url::form_urlencoded::byte_serialize(id.as_bytes()).collect();

    for size in ["96", "264"] {
        let reply = dash
            .get(
                Some(&admin),
                &format!("/api/admin/find/cover?id={id}&size={size}"),
            )
            .await;
        assert!(is_cover(&reply), "{:?} {}", reply.status, reply.text());
    }
    // Fetched once for each size the page asked for, then kept.
    let again = dash
        .get(Some(&admin), &format!("/api/admin/find/cover?id={id}&size=96"))
        .await;
    assert!(is_cover(&again));
    server.verify().await;
}

/// An Animated art tile's still is served by Octo, named on the album's row.
#[tokio::test]
async fn an_animated_art_still_is_served_by_octo() {
    let server = MockServer::builder().start().await;
    Mock::given(method("GET"))
        .and(path("/still.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(sample_cover()))
        .mount(&server)
        .await;
    let dash = Dashboard::new(None);
    let admin = dash.state.browse_sessions.create("admin");
    dash.state.animated_art.keep(
        "Zephyr Echo",
        "Lights",
        Some(Clip {
            video: format!("{}/loop.m3u8", server.uri()),
            poster: Some(format!("{}/still.jpg", server.uri())),
            color: None,
            width: 1080,
            height: 1080,
        }),
        None,
    );

    let page = dash.get(Some(&admin), "/api/admin/animated-art/albums").await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.text());
    let still = page.json()["albums"][0]["still"]
        .as_str()
        .expect("a still address")
        .to_string();
    assert!(still.starts_with("/api/admin/animated-art/still?key="), "{still}");

    let reply = dash.get(Some(&admin), &still).await;
    assert!(is_cover(&reply), "{:?}", reply.status);
    assert_eq!(
        dash.get(Some(&admin), "/api/admin/animated-art/still?key=nobody")
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}
