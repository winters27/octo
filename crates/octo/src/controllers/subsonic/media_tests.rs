//! `stream` and `getCoverArt` through the app: outside playback, the sign-in check in front of
//! outside songs, plain covers for the Octo app, and a mix's cover.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, StatusCode};
use indexmap::IndexMap;
use octo_core::settings::{
    AppSettings, GeneratedPlaylistSettings, HeartDownloadSource, HeartDownloadStep, SubsonicSettings,
};
use octo_core::soulseek::soulseek_metadata_service::{RoutingKind, SoulseekRouting};
use octo_subsonic::xml::XElement;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request as MockRequest, Respond, ResponseTemplate};

use super::caller_checks::is_first_byte_request;
use super::media::draws_its_own_marks_in;
use super::test_app::*;
use crate::services::fingerprint::music_brainz_client::MusicBrainzClient;

/// Navidrome accepting every sign-in: these tests are about what plays, not who.
async fn ping_ok() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(navidrome_json(
            r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#,
        ))
        .mount(&server)
        .await;
    server
}

fn playback_settings(url: &str, wait_for_lossless: bool, download_on_play: bool) -> AppSettings {
    let mut settings = settings(url);
    settings.subsonic = SubsonicSettings {
        wait_for_lossless_on_play: wait_for_lossless,
        download_on_play,
        ..settings.subsonic
    };
    // A play first looks at slskd's login; nothing answers there, and it must say so at once
    // (slskd's default, localhost:5030, can wait out a timeout instead; see refused_address).
    settings.soulseek.base_url = Some(format!(
        "http://{}",
        crate::services::test_support::refused_address()
    ));
    // AcoustID on, by a key of the user's own: a play's copy can be proven.
    settings.soulseek.acoust_id_api_key = "user-key".into();
    settings
}

// ---- Outside playback --------------------------------------------------------------------

#[tokio::test]
async fn external_playback_uses_you_tube_regardless_of_legacy_storage_mode() {
    let navidrome = ping_ok().await;
    let downloads = Arc::new(FakeStreams::streaming(
        "soulseek",
        "track-id",
        &[1, 2, 3],
        "audio/mp4",
    ));
    let library = Arc::new(FakeLibrary::with_external(
        "external-track",
        "soulseek",
        "track-id",
    ));
    let state = state_with(playback_settings(&navidrome.uri(), false, false), |inner| {
        inner.download_service = downloads.clone();
        inner.local_library = library.clone();
    });
    let app = app(&state);

    let response = get(
        &app,
        "/rest/stream?id=external-track&f=json&u=alice&t=good&s=salt&v=1.16.1&c=test",
    )
    .await;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body.as_ref(), [1, 2, 3]);
    assert_eq!(
        downloads.stream_calls(),
        [("soulseek".to_string(), "track-id".to_string(), None)]
    );
    assert_eq!(*downloads.download_and_stream_calls.lock(), 0);
}

#[tokio::test]
async fn an_outside_song_downloads_as_it_streams() {
    let navidrome = ping_ok().await;
    let downloads = Arc::new(FakeStreams::streaming(
        "soulseek",
        "track-id",
        &[1, 2, 3],
        "audio/mp4",
    ));
    let library = Arc::new(FakeLibrary::with_external(
        "external-track",
        "soulseek",
        "track-id",
    ));
    let state = state_with(playback_settings(&navidrome.uri(), false, false), |inner| {
        inner.download_service = downloads.clone();
        inner.local_library = library.clone();
    });
    let app = app(&state);

    for uri in [
        "/rest/download?id=external-track&u=alice&t=good&s=salt&v=1.16.1&c=Amperfy",
        "/rest/download.view?id=external-track&u=alice&t=good&s=salt&v=1.16.1&c=Amperfy",
    ] {
        let response = get(&app, uri).await;
        assert_eq!(response.status, StatusCode::OK, "{uri}");
        assert_eq!(response.body.as_ref(), [1, 2, 3], "{uri}");
        assert_eq!(response.header("content-type"), Some("audio/mp4"), "{uri}");
    }
    assert_eq!(downloads.stream_calls().len(), 2);
}

#[tokio::test]
async fn an_outside_download_sent_as_a_form_is_the_song_too() {
    let navidrome = ping_ok().await;
    let downloads = Arc::new(FakeStreams::streaming(
        "soulseek",
        "track-id",
        &[4, 5],
        "audio/mp4",
    ));
    let library = Arc::new(FakeLibrary::with_external(
        "external-track",
        "soulseek",
        "track-id",
    ));
    let state = state_with(playback_settings(&navidrome.uri(), false, false), |inner| {
        inner.download_service = downloads.clone();
        inner.local_library = library.clone();
    });
    let app = app(&state);

    let response = send(
        &app,
        Method::POST,
        "/rest/download",
        &[("Content-Type", "application/x-www-form-urlencoded")],
        Body::from("id=external-track&u=alice&t=good&s=salt&v=1.16.1&c=test"),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body.as_ref(), [4, 5]);
}

/// Playback settings whose Lidarr takes album hearts, and song hearts too when `song_hearts`,
/// with its address and key set when `set_up`.
fn lidarr_heart_settings(url: &str, song_hearts: bool, set_up: bool) -> AppSettings {
    let mut settings = playback_settings(url, false, false);
    settings.subsonic.heart_download_sources = vec![HeartDownloadStep {
        source: HeartDownloadSource::Lidarr,
        enabled: None,
        song_enabled: Some(song_hearts),
        album_enabled: Some(true),
    }];
    if set_up {
        settings.lidarr.base_url = Some(format!(
            "http://{}",
            crate::services::test_support::refused_address()
        ));
        settings.lidarr.api_key = Some("key".into());
    }
    settings
}

/// A play of an outside song from its first byte asks ahead what a heart of it asks MusicBrainz
/// first, and only while song hearts can go to Lidarr.
#[tokio::test]
async fn a_play_warms_a_hearts_questions_only_while_song_hearts_can_go_to_lidarr() {
    let navidrome = ping_ok().await;
    for (song_hearts, set_up, warms) in [(true, true, true), (false, true, false), (true, false, false)] {
        // MusicBrainz knows nothing, so the code lookup is the warm-up's one question.
        let music_brainz = MockServer::start().await;
        let base = url::Url::parse(&format!("{}/ws/2/", music_brainz.uri())).expect("parses");
        let pace = Duration::from_millis(10);
        let client =
            Arc::new(MusicBrainzClient::with_base_url(base).with_pace(pace, Duration::from_secs(5), pace));
        let library = Arc::new(FakeLibrary::default());
        let state = state_with(
            lidarr_heart_settings(&navidrome.uri(), song_hearts, set_up),
            |inner| {
                inner.download_service = Arc::new(FakeStreams::default());
                inner.local_library = library.clone();
                inner.music_brainz = client.clone();
            },
        );
        let id = state.external_id_registry.register(SoulseekRouting {
            kind: RoutingKind::Song,
            artist: Some("The Beatles".into()),
            title: Some("Yesterday".into()),
            isrc: Some("GBAYE0601477".into()),
            ..Default::default()
        });
        library.add_external("external-track", "soulseek", &id);
        let app = app(&state);
        let uri = "/rest/stream?id=external-track&u=alice&t=good&s=salt&v=1.16.1&c=test";
        let asked = || async {
            music_brainz
                .received_requests()
                .await
                .unwrap_or_default()
                .iter()
                .map(|request| format!("{}?{}", request.url.path(), request.url.query().unwrap_or("")))
                .collect::<Vec<String>>()
        };

        // A later range is no play.
        send(&app, Method::GET, uri, &[("Range", "bytes=100-")], Body::empty()).await;
        client.warmed_up().await;
        assert!(asked().await.is_empty(), "{song_hearts} {set_up}");

        get(&app, uri).await;
        client.warmed_up().await;
        let expected: &[&str] = if warms {
            &["/ws/2/isrc/GBAYE0601477?inc=artist-credits&fmt=json"]
        } else {
            &[]
        };
        assert_eq!(asked().await, expected, "{song_hearts} {set_up}");
    }
}

#[tokio::test]
async fn a_library_download_still_goes_to_navidromes_download() {
    let navidrome = MockServer::start().await;
    Mock::given(wiremock::matchers::path("/rest/download"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(vec![9, 8, 7], "audio/flac"))
        .mount(&navidrome)
        .await;
    Mock::given(any())
        .respond_with(navidrome_json(NAVIDROME_OK))
        .mount(&navidrome)
        .await;
    let downloads = Arc::new(FakeStreams::default());
    let state = state_with(playback_settings(&navidrome.uri(), false, false), |inner| {
        inner.download_service = downloads.clone();
        inner.local_library = Arc::new(FakeLibrary::default());
    });
    let app = app(&state);

    let response = get(
        &app,
        "/rest/download?id=lib-1&u=alice&t=good&s=salt&v=1.16.1&c=Amperfy",
    )
    .await;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body.as_ref(), [9, 8, 7]);
    assert!(
        received_paths(&navidrome)
            .await
            .iter()
            .any(|path| path == "/rest/download"),
        "relayed to Navidrome's download"
    );
    assert!(downloads.stream_calls().is_empty());
}

#[tokio::test]
async fn wait_for_lossless_still_acquires_before_playback_when_enabled() {
    let navidrome = ping_ok().await;
    let downloads = Arc::new(FakeStreams::default());
    let library = Arc::new(FakeLibrary::with_external(
        "external-track",
        "soulseek",
        "track-id",
    ));
    let state = state_with(playback_settings(&navidrome.uri(), true, false), |inner| {
        inner.download_service = downloads.clone();
        inner.local_library = library.clone();
    });
    let app = app(&state);
    let response = tokio::spawn(async move {
        get(
            &app,
            "/rest/stream?id=external-track&f=json&u=alice&t=good&s=salt&v=1.16.1&c=test",
        )
        .await
    });

    let queue = state.track_acquisition_queue.clone();
    let request = tokio::time::timeout(Duration::from_secs(30), queue.dequeue(&CancellationToken::new()))
        .await
        .expect("the stream queued an acquisition")
        .expect("a request");
    assert!(!request.is_star);
    assert!(!request.trigger_album_download);
    assert!(request.force_permanent);

    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("octo-playback.flac");
    std::fs::write(&path, [4, 5, 6]).expect("written");
    request
        .completion
        .try_set_result(path.to_string_lossy().into_owned());
    queue.release(&request);

    let response = response.await.expect("the request ran");
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body.as_ref(), [4, 5, 6]);
    assert_eq!(response.header("content-type"), Some("audio/flac"));
    assert!(downloads.stream_calls().is_empty());
}

#[tokio::test]
async fn head_on_an_outside_song_gives_the_play_headers_without_a_body_or_a_play() {
    let navidrome = ping_ok().await;
    let downloads = Arc::new(FakeStreams::streaming(
        "soulseek",
        "track-id",
        &[7, 8, 9, 10],
        "audio/flac",
    ));
    let library = Arc::new(FakeLibrary::with_external(
        "external-track",
        "soulseek",
        "track-id",
    ));
    let state = state_with(playback_settings(&navidrome.uri(), false, true), |inner| {
        inner.download_service = downloads.clone();
        inner.local_library = library.clone();
    });

    for endpoint in ["stream", "stream.view"] {
        let url = format!("/rest/{endpoint}?id=external-track&f=json&u=alice&t=good&s=salt&v=1.16.1&c=test");
        let reply = send(&app(&state), Method::HEAD, &url, &[], Body::empty()).await;

        assert_eq!(reply.status, StatusCode::OK, "{endpoint}");
        assert_eq!(reply.header("content-type"), Some("audio/flac"), "{endpoint}");
        assert!(reply.body.is_empty(), "{endpoint}");
    }

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(state.track_acquisition_queue.waiting_plays(), 0);
    let relayed: Vec<String> = navidrome
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| r.url.path().to_string())
        .collect();
    assert!(
        !relayed.iter().any(|path| path.starts_with("/rest/stream")),
        "{relayed:?}"
    );
}

#[tokio::test]
async fn head_on_a_library_song_is_relayed_to_navidrome() {
    let navidrome = ping_ok().await;
    let state = state_with(playback_settings(&navidrome.uri(), false, false), |_| {});

    let reply = send(
        &app(&state),
        Method::HEAD,
        "/rest/stream?id=library-track&u=alice&t=good&s=salt&v=1.16.1&c=test",
        &[],
        Body::empty(),
    )
    .await;

    assert_eq!(reply.status, StatusCode::OK);
    let relayed: Vec<(String, String)> = navidrome
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| (r.method.to_string(), r.url.path().to_string()))
        .collect();
    assert!(
        relayed.contains(&("HEAD".to_string(), "/rest/stream".to_string())),
        "{relayed:?}"
    );
}

#[tokio::test]
async fn streaming_an_outside_song_queues_its_download_only_from_the_first_byte() {
    for (range, waiting) in [(None, 1), (Some("bytes=0-"), 1), (Some("bytes=4096-"), 0)] {
        let navidrome = ping_ok().await;
        let downloads = Arc::new(FakeStreams::streaming(
            "soulseek",
            "track-id",
            &[1, 2, 3],
            "audio/mp4",
        ));
        let library = Arc::new(FakeLibrary::with_external(
            "external-track",
            "soulseek",
            "track-id",
        ));
        let state = state_with(playback_settings(&navidrome.uri(), false, true), |inner| {
            inner.download_service = downloads.clone();
            inner.local_library = library.clone();
        });
        let headers: Vec<(&str, &str)> = range.map(|r| ("Range", r)).into_iter().collect();

        let response = send(
            &app(&state),
            Method::GET,
            "/rest/stream?id=external-track&f=json&u=alice&t=good&s=salt&v=1.16.1&c=test",
            &headers,
            Body::empty(),
        )
        .await;

        assert_eq!(response.status, StatusCode::OK, "{range:?}");
        // The play is queued behind a look at slskd's login, which the request does not wait
        // for, so give it a moment.
        for _ in 0..100 {
            if state.track_acquisition_queue.waiting_plays() == waiting && waiting > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            state.track_acquisition_queue.waiting_plays(),
            waiting,
            "{range:?}"
        );
    }
}

#[test]
fn only_a_request_from_the_first_byte_is_a_play() {
    for (method, range, play) in [
        ("GET", None, true),
        ("GET", Some(""), true),
        ("GET", Some("bytes=0-"), true),
        ("GET", Some("bytes=0-1"), true),
        ("GET", Some("bytes=10-20"), false),
        ("HEAD", None, false),
    ] {
        assert_eq!(is_first_byte_request(method, range), play, "{method} {range:?}");
    }
}

// ---- The sign-in check in front of outside songs -----------------------------------------

/// Navidrome as these calls need it: a ping that accepts the token "good" or the API key
/// "bob-key", tokenInfo for that key, and a library stream.
struct SignInNavidrome;

impl Respond for SignInNavidrome {
    fn respond(&self, request: &MockRequest) -> ResponseTemplate {
        let query = query_of(request);
        let path = request.url.path();
        if path.ends_with("/rest/ping") {
            let ok = query.get("t").map(String::as_str) == Some("good")
                || query.get("apiKey").map(String::as_str) == Some("bob-key");
            return navidrome_json(if ok {
                NAVIDROME_OK
            } else {
                NAVIDROME_WRONG_PASSWORD
            });
        }
        if path.ends_with("/rest/tokenInfo") {
            return navidrome_json(if query.get("apiKey").map(String::as_str) == Some("bob-key") {
                r#"{"subsonic-response":{"status":"ok","version":"1.16.1","tokenInfo":{"username":"bob"}}}"#
            } else {
                r#"{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":44,"message":"Invalid API key"}}}"#
            });
        }
        if path.ends_with("/rest/stream") {
            return ResponseTemplate::new(200).set_body_raw(vec![7, 8, 9], "audio/flac");
        }
        ResponseTemplate::new(404)
    }
}

struct SignIn {
    navidrome: MockServer,
    state: crate::app::AppState,
    downloads: Arc<FakeStreams>,
}

impl SignIn {
    async fn new() -> SignIn {
        let navidrome = MockServer::builder().start().await;
        Mock::given(any())
            .respond_with(SignInNavidrome)
            .mount(&navidrome)
            .await;
        let downloads = Arc::new(FakeStreams::streaming(
            "soulseek",
            "track-id",
            &[1, 2, 3],
            "audio/mp4",
        ));
        let library = Arc::new(FakeLibrary::with_external(
            "external-track",
            "soulseek",
            "track-id",
        ));
        let state = state_with(settings(&navidrome.uri()), |inner| {
            inner.download_service = downloads.clone();
            inner.local_library = library.clone();
        });
        SignIn {
            navidrome,
            state,
            downloads,
        }
    }

    async fn pings(&self) -> usize {
        received_paths(&self.navidrome)
            .await
            .iter()
            .filter(|p| p.ends_with("/rest/ping"))
            .count()
    }

    async fn streams(&self) -> usize {
        received_paths(&self.navidrome)
            .await
            .iter()
            .filter(|p| p.ends_with("/rest/stream"))
            .count()
    }

    /// Navidrome off: every call fails as a refused connection does.
    fn navidrome_down(&self, down: bool) {
        let url = if down {
            "http://127.0.0.1:1".to_string()
        } else {
            self.navidrome.uri()
        };
        self.state.settings.set(settings(&url));
    }
}

fn sign_in(token: &str) -> String {
    format!("u=alice&t={token}&s=salt&v=1.16.1&c=test")
}

fn json_error_code(reply: &Reply) -> i64 {
    let envelope = reply.envelope();
    assert_eq!(envelope["status"], "failed", "{}", reply.text());
    envelope["error"]["code"].as_i64().expect("a code")
}

#[tokio::test]
async fn star_on_an_outside_song_with_a_wrong_password_starts_nothing() {
    let fixture = SignIn::new().await;

    let response = get(
        &app(&fixture.state),
        &format!("/rest/star.view?id=external-track&{}&f=json", sign_in("bad")),
    )
    .await;

    assert_eq!(json_error_code(&response), 40);
    assert!(fixture.state.acquisition_tracker.all().is_empty());
}

#[tokio::test]
async fn star_with_no_sign_in_is_refused_without_a_ping() {
    let fixture = SignIn::new().await;

    let response = get(
        &app(&fixture.state),
        "/rest/star.view?id=external-track&u=alice&v=1.16.1&c=test&f=json",
    )
    .await;

    assert_eq!(json_error_code(&response), 40);
    assert_eq!(fixture.pings().await, 0);
    assert!(fixture.state.acquisition_tracker.all().is_empty());
}

#[tokio::test]
async fn star_on_an_outside_album_with_a_wrong_password_starts_nothing() {
    let fixture = SignIn::new().await;
    let album_id = fixture.state.external_id_registry.register(SoulseekRouting {
        kind: RoutingKind::Album,
        artist: Some("Massive Attack".into()),
        album: Some("Mezzanine".into()),
        ..Default::default()
    });

    let response = get(
        &app(&fixture.state),
        &format!("/rest/star.view?albumId={album_id}&{}&f=json", sign_in("bad")),
    )
    .await;

    assert_eq!(json_error_code(&response), 40);
    assert!(fixture.state.acquisition_tracker.all().is_empty());
    tokio::time::sleep(Duration::from_millis(100)).await;
    // Nothing was queued for the album: the acquisition queue holds no request.
    assert!(fixture.state.track_acquisition_queue.is_idle());
}

#[tokio::test]
async fn stream_of_an_outside_song_with_a_wrong_password_serves_nothing() {
    let fixture = SignIn::new().await;

    // No f: the error comes back in XML, as the client asked.
    let response = get(
        &app(&fixture.state),
        &format!("/rest/stream?id=external-track&{}", sign_in("bad")),
    )
    .await;

    let xml = XElement::parse(&response.text()).expect("XML");
    assert_eq!(xml.attribute("status"), Some("failed"));
    let error = xml.elements().find(|e| e.name == "error").expect("an error");
    assert_eq!(error.attribute("code"), Some("40"));
    assert!(fixture.downloads.stream_calls().is_empty());
}

#[tokio::test]
async fn stream_of_an_outside_song_same_address_twice_pings_once() {
    let fixture = SignIn::new().await;
    let app = app(&fixture.state);
    let url = format!("/rest/stream?id=external-track&{}&f=json", sign_in("good"));

    let first = get(&app, &url).await;
    let second = get(&app, &url).await;

    assert_eq!(first.body.as_ref(), [1, 2, 3]);
    assert_eq!(second.body.as_ref(), [1, 2, 3]);
    assert_eq!(fixture.pings().await, 1);
}

#[tokio::test]
async fn stream_while_navidrome_is_down_is_refused_and_recovers_at_once() {
    let fixture = SignIn::new().await;
    let app = app(&fixture.state);
    let url = format!("/rest/stream?id=external-track&{}&f=json", sign_in("good"));

    fixture.navidrome_down(true);
    let refused = get(&app, &url).await;
    assert_eq!(json_error_code(&refused), 0);
    assert!(fixture.downloads.stream_calls().is_empty());

    fixture.navidrome_down(false);
    let served = get(&app, &url).await;
    assert_eq!(served.body.as_ref(), [1, 2, 3]);
}

#[tokio::test]
async fn stream_of_a_library_song_is_left_to_navidrome() {
    let fixture = SignIn::new().await;

    let response = get(
        &app(&fixture.state),
        &format!("/rest/stream?id=library-track&{}", sign_in("good")),
    )
    .await;

    assert_eq!(response.body.as_ref(), [7, 8, 9]);
    assert_eq!(fixture.pings().await, 0);
    assert_eq!(fixture.streams().await, 1);
}

#[tokio::test]
async fn star_from_an_api_key_is_filed_under_the_keys_owner() {
    let fixture = SignIn::new().await;

    let response = get(
        &app(&fixture.state),
        "/rest/star.view?id=external-track&apiKey=bob-key&v=1.16.1&c=test&f=json",
    )
    .await;

    assert_eq!(response.status, StatusCode::OK, "{}", response.text());
    assert_eq!(fixture.state.acquisition_tracker.for_user("bob").len(), 1);
}

// ---- Plain covers for the Octo app -------------------------------------------------------

fn parameters(pairs: &[(&str, &str)]) -> octo_subsonic::Parameters {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn only_the_octo_app_gets_plain_covers() {
    for (client, plain) in [
        ("Octo", true),
        ("octo", true),
        (" Octo ", true),
        ("Symfonium", false),
        ("Feishin", false),
        ("OctoPlayer", false),
        ("", false),
    ] {
        assert_eq!(
            draws_its_own_marks_in(&parameters(&[("id", "abc"), ("c", client)])),
            plain,
            "{client:?}"
        );
    }
}

#[test]
fn a_request_without_a_client_name_keeps_the_badge() {
    assert!(!draws_its_own_marks_in(&parameters(&[("id", "abc")])));
}

// ---- getCoverArt -------------------------------------------------------------------------

/// A library with 40 Rock songs, and a cover for each.
struct LibraryNavidrome;

fn library_ok(inner: &str) -> String {
    format!(r#"{{"subsonic-response":{{"status":"ok","version":"1.16.1",{inner}}}}}"#)
}

impl Respond for LibraryNavidrome {
    fn respond(&self, request: &MockRequest) -> ResponseTemplate {
        let path = request.url.path().trim_matches('/').to_string();
        let songs = (0..40)
            .map(|i| {
                format!(
                    r#"{{"id":"lib{i}","title":"Track {i}","artist":"Artist {}","artistId":"ar{}","coverArt":"al-{}","duration":200,"playCount":5,"suffix":"flac","bitRate":900}}"#,
                    i % 20,
                    i % 20,
                    i % 5
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let body = match path.as_str() {
            "rest/getGenres" => library_ok(
                r#""genres":{"genre":[{"value":"Rock","songCount":40},{"value":"Polka","songCount":3}]}"#,
            ),
            "rest/getSongsByGenre" => library_ok(&format!(r#""songsByGenre":{{"song":[{songs}]}}"#)),
            "rest/getRandomSongs" => library_ok(r#""randomSongs":{"song":[]}"#),
            "rest/getCoverArt" => {
                let mut image = image::RgbImage::new(8, 8);
                for pixel in image.pixels_mut() {
                    *pixel = image::Rgb([200, 40, 40]);
                }
                let mut jpeg = Vec::new();
                image::DynamicImage::ImageRgb8(image)
                    .write_to(&mut std::io::Cursor::new(&mut jpeg), image::ImageFormat::Jpeg)
                    .expect("encoded");
                return ResponseTemplate::new(200).set_body_raw(jpeg, "image/jpeg");
            }
            _ => library_ok(r#""ping":{}"#),
        };
        ResponseTemplate::new(200).set_body_raw(body.into_bytes(), "application/json")
    }
}

#[tokio::test]
async fn get_cover_art_for_a_mix_is_an_image() {
    let navidrome = MockServer::start().await;
    Mock::given(any())
        .respond_with(LibraryNavidrome)
        .mount(&navidrome)
        .await;
    let state = state_with(
        AppSettings {
            generated_playlists: GeneratedPlaylistSettings {
                enabled: true,
                ..Default::default()
            },
            ..settings(&navidrome.uri())
        },
        |_| {},
    );
    let auth: IndexMap<String, String> = [
        ("u", "alice"),
        ("t", "token"),
        ("s", "salt"),
        ("v", "1.16.1"),
        ("c", "test"),
        ("f", "json"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    // getPlaylists lists the mixes; the service is what it asks.
    let mixes = state.generated_playlists.list("alice", &auth).await;
    let mix = mixes
        .iter()
        .find(|mix| mix.name == "Rock Mix")
        .unwrap_or_else(|| {
            panic!(
                "a Rock Mix in {:?}",
                mixes.iter().map(|m| &m.name).collect::<Vec<_>>()
            )
        });

    let cover = get(
        &app(&state),
        &format!(
            "/rest/getCoverArt.view?u=alice&t=token&s=salt&v=1.16.1&c=test&f=json&id={}",
            mix.id
        ),
    )
    .await;

    assert_eq!(cover.status, StatusCode::OK);
    assert_eq!(cover.media_type().as_deref(), Some("image/jpeg"));
    assert!(image::load_from_memory(&cover.body).is_ok(), "a picture");
}

/// The local relay, the octo-radio and placeholder answers, and a missing id.
#[tokio::test]
async fn get_cover_art_answers_each_kind_of_id() {
    let navidrome = MockServer::start().await;
    Mock::given(wiremock::matchers::path("/rest/getCoverArt"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(vec![1, 2, 3], "image/png"))
        .mount(&navidrome)
        .await;
    let state = state_with(settings(&navidrome.uri()), |_| {});
    let app = app(&state);

    let local = get(&app, "/rest/getCoverArt?id=al-1&u=a&t=good&s=s").await;
    assert_eq!(local.status, StatusCode::OK);
    assert_eq!(local.header("content-type"), Some("image/png"));
    assert_eq!(local.body.as_ref(), [1, 2, 3]);

    let missing = get(&app, "/rest/getCoverArt?u=a").await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert_eq!(
        missing.header("content-type"),
        Some("application/problem+json; charset=utf-8")
    );

    for id in ["octo-radio", "ext-album-unknown"] {
        let placeholder = get(&app, &format!("/rest/getCoverArt?id={id}")).await;
        assert_eq!(placeholder.status, StatusCode::OK, "{id}");
        assert_eq!(placeholder.header("content-type"), Some("image/jpeg"), "{id}");
        assert!(image::load_from_memory(&placeholder.body).is_ok(), "{id}");
    }
    // The Octo app draws its own tile for a cover Octo cannot find.
    let plain = get(&app, "/rest/getCoverArt?id=ext-album-unknown&c=Octo").await;
    assert_eq!(plain.status, StatusCode::NOT_FOUND);

    // A relay failure is the unbranded placeholder, still a 200.
    state.settings.set(settings("http://127.0.0.1:1"));
    let unreachable = get(&app, "/rest/getCoverArt?id=al-1").await;
    assert_eq!(unreachable.status, StatusCode::OK);
    assert_eq!(unreachable.header("content-type"), Some("image/jpeg"));
}

/// A local file served for an outside id under WaitForLosslessOnPlay honors one byte range,
/// and types the file by its extension.
#[tokio::test]
async fn a_lossless_copy_on_disk_is_served_with_ranges() {
    let navidrome = ping_ok().await;
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("song.m4a");
    std::fs::write(&path, (0u8..100).collect::<Vec<_>>()).expect("written");
    let library = Arc::new(FakeLibrary::with_external(
        "external-track",
        "soulseek",
        "track-id",
    ));
    library.local_paths.lock().insert(
        ("soulseek".into(), "track-id".into()),
        path.to_string_lossy().into_owned(),
    );
    let state = state_with(playback_settings(&navidrome.uri(), true, false), |inner| {
        inner.local_library = library.clone();
    });
    let app = app(&state);
    let url = "/rest/stream?id=external-track&u=alice&t=good&s=salt&v=1.16.1&c=test";

    let whole = get(&app, url).await;
    assert_eq!(whole.status, StatusCode::OK);
    assert_eq!(whole.body.len(), 100);
    assert_eq!(whole.header("content-type"), Some("audio/mp4"));
    assert_eq!(whole.header("accept-ranges"), Some("bytes"));

    let mut cases: HashMap<&str, (StatusCode, Option<&str>, usize)> = HashMap::new();
    cases.insert(
        "bytes=10-19",
        (StatusCode::PARTIAL_CONTENT, Some("bytes 10-19/100"), 10),
    );
    cases.insert(
        "bytes=-5",
        (StatusCode::PARTIAL_CONTENT, Some("bytes 95-99/100"), 5),
    );
    cases.insert(
        "bytes=200-",
        (StatusCode::RANGE_NOT_SATISFIABLE, Some("bytes */100"), 0),
    );
    for (range, (status, content_range, length)) in cases {
        let reply = send(&app, Method::GET, url, &[("Range", range)], Body::empty()).await;
        assert_eq!(reply.status, status, "{range}");
        assert_eq!(reply.header("content-range"), content_range, "{range}");
        assert_eq!(reply.body.len(), length, "{range}");
    }
}

/// One picture, the same every time it is asked.
struct OnePicture;

#[async_trait::async_trait]
impl crate::services::cover_art::ICoverArtSource for OnePicture {
    fn name(&self) -> &str {
        "one"
    }

    async fn try_fetch(&self, _: &SoulseekRouting, _: bool) -> Option<bytes::Bytes> {
        let mut picture = Vec::new();
        image::DynamicImage::new_rgb8(64, 64)
            .write_to(&mut std::io::Cursor::new(&mut picture), image::ImageFormat::Jpeg)
            .ok()?;
        Some(picture.into())
    }
}

/// A badged outside cover is kept by id and cover design and served from memory the next time,
/// at any size asked (the badge is drawn on the same picture whatever the size); the Octo app's
/// plain covers are never the badged ones.
#[tokio::test]
async fn a_badged_outside_cover_is_kept_and_served_again_from_memory() {
    let navidrome = MockServer::start().await;
    let state = state_with(settings(&navidrome.uri()), |inner| {
        inner.cover_art_aggregator = Arc::new(crate::services::cover_art::CoverArtAggregator::new(vec![
            Arc::new(OnePicture),
        ]));
        // The logo the badge is drawn from, as the image ships it.
        let logo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../octo/Assets/octo_logo.png");
        inner.cover_art_service =
            Arc::new(octo_media::cover::CoverArtService::new(None).with_logo_paths(vec![logo]));
    });
    let id = state.external_id_registry.register(SoulseekRouting {
        kind: RoutingKind::Song,
        artist: Some("Artist".into()),
        title: Some("Title".into()),
        ..Default::default()
    });
    let app = app(&state);
    let key = state.cover_art_service.badged_key(&id);

    let first = get(&app, &format!("/rest/getCoverArt?id={id}&size=300")).await;
    assert_eq!(first.status, StatusCode::OK);
    assert_eq!(
        state.cover_art_service.kept_badged(&key).as_deref(),
        Some(first.body.as_ref())
    );

    // What is kept is what the next request gets.
    state.cover_art_service.keep_badged(key.clone(), b"kept");
    let second = get(&app, &format!("/rest/getCoverArt?id={id}&size=300")).await;
    assert_eq!(second.body.as_ref(), b"kept");
    // Another size is the same picture.
    let other = get(&app, &format!("/rest/getCoverArt?id={id}&size=600")).await;
    assert_eq!(other.body.as_ref(), b"kept");
    // The Octo app gets the plain picture, never the badged one.
    let plain = get(&app, &format!("/rest/getCoverArt?id={id}&size=300&c=Octo")).await;
    assert_ne!(plain.body.as_ref(), b"kept");
}

#[test]
fn a_badged_cover_is_kept_by_design_version_and_id() {
    let service = octo_media::cover::CoverArtService::new(None);
    let version = octo_media::cover::CoverBook::shipped().version;

    assert_eq!(service.badged_key("ext-1"), format!("{version}|ext-1"));
}

/// A picture that is not one the badge can be drawn on.
struct NotAPicture;

#[async_trait::async_trait]
impl crate::services::cover_art::ICoverArtSource for NotAPicture {
    fn name(&self) -> &str {
        "broken"
    }

    async fn try_fetch(&self, _: &SoulseekRouting, _: bool) -> Option<bytes::Bytes> {
        Some(bytes::Bytes::from_static(b"not a picture"))
    }
}

/// A cover the badge could not be drawn on goes out as it came, and is not kept: the next
/// request tries the badge again.
#[tokio::test]
async fn a_cover_the_badge_failed_on_is_not_kept() {
    let navidrome = MockServer::start().await;
    let state = state_with(settings(&navidrome.uri()), |inner| {
        inner.cover_art_aggregator = Arc::new(crate::services::cover_art::CoverArtAggregator::new(vec![
            Arc::new(NotAPicture),
        ]));
    });
    let id = state.external_id_registry.register(SoulseekRouting {
        kind: RoutingKind::Song,
        artist: Some("Artist".into()),
        title: Some("Broken".into()),
        ..Default::default()
    });
    let app = app(&state);

    let reply = get(&app, &format!("/rest/getCoverArt?id={id}")).await;

    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body.as_ref(), b"not a picture");
    assert_eq!(
        state
            .cover_art_service
            .kept_badged(&state.cover_art_service.badged_key(&id)),
        None
    );
}

/// The badged-cover state: the shipped logo and one picture for every outside cover.
fn badging_state(settings: AppSettings) -> crate::app::AppState {
    state_with(settings, |inner| {
        inner.cover_art_aggregator = Arc::new(crate::services::cover_art::CoverArtAggregator::new(vec![
            Arc::new(OnePicture),
        ]));
        let logo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../octo/Assets/octo_logo.png");
        inner.cover_art_service =
            Arc::new(octo_media::cover::CoverArtService::new(None).with_logo_paths(vec![logo]));
    })
}

/// OnePicture's bytes, as an outside cover with no badge goes out.
async fn one_picture() -> Vec<u8> {
    use crate::services::cover_art::ICoverArtSource;
    OnePicture
        .try_fetch(&SoulseekRouting::default(), false)
        .await
        .expect("picture")
        .to_vec()
}

/// With the badge turned off, every app gets the plain picture and nothing badged is kept.
#[tokio::test]
async fn with_the_badge_off_outside_covers_go_out_plain() {
    let navidrome = MockServer::start().await;
    let mut off = settings(&navidrome.uri());
    off.subsonic.cover_badge = false;
    let state = badging_state(off);
    let id = state.external_id_registry.register(SoulseekRouting {
        kind: RoutingKind::Song,
        artist: Some("Artist".into()),
        title: Some("Title".into()),
        ..Default::default()
    });
    let app = app(&state);
    let key = state.cover_art_service.badged_key(&id);
    // A copy badged while the badge was on is not served once it is off.
    state
        .cover_art_service
        .keep_badged(key.clone(), b"badged earlier");

    let reply = get(&app, &format!("/rest/getCoverArt?id={id}&size=300&c=Symfonium")).await;

    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body.as_ref(), one_picture().await.as_slice());
    // A missing outside cover is still the drawn placeholder for an app that needs a picture.
    let missing = get(&app, "/rest/getCoverArt?id=ext-album-unknown&c=Symfonium").await;
    assert_eq!(missing.status, StatusCode::OK);
    assert!(image::load_from_memory(&missing.body).is_ok());

    // Turned back on, the badged copy is served again.
    let mut on = settings(&navidrome.uri());
    on.subsonic.cover_badge = true;
    state.settings.set(on);
    let badged = get(&app, &format!("/rest/getCoverArt?id={id}&size=300&c=Symfonium")).await;
    assert_eq!(badged.body.as_ref(), b"badged earlier");
}

/// Navidrome's own web UI asks for `mf-<id>`, `al-<id>` and `ar-<id>` whatever id it was given.
/// An outside id under one of them is Octo's cover; a library id under one is still Navidrome's.
#[tokio::test]
async fn navidromes_web_ui_ids_of_outside_items_get_their_covers() {
    let navidrome = MockServer::start().await;
    Mock::given(wiremock::matchers::path("/rest/getCoverArt"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(vec![1, 2, 3], "image/png"))
        .mount(&navidrome)
        .await;
    let mut plain = settings(&navidrome.uri());
    plain.subsonic.cover_badge = false;
    let state = badging_state(plain);
    let register = |kind| {
        state.external_id_registry.register(SoulseekRouting {
            kind,
            artist: Some("Dua Lipa".into()),
            title: Some("Future Nostalgia".into()),
            album: Some("Future Nostalgia".into()),
            ..Default::default()
        })
    };
    let song = register(RoutingKind::Song);
    let album = register(RoutingKind::Album);
    let artist = register(RoutingKind::Artist);
    let app = app(&state);

    for id in [
        format!("mf-{song}"),
        format!("al-{album}"),
        format!("ar-{artist}"),
    ] {
        let reply = get(
            &app,
            &format!("/rest/getCoverArt?id={id}&size=300&_=2026-10-16T09:00:00Z&c=NavidromeUI"),
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK, "{id}");
        assert_eq!(reply.header("content-type"), Some("image/jpeg"), "{id}");
        assert_eq!(reply.body.as_ref(), one_picture().await.as_slice(), "{id}");
    }

    // A library id keeps its prefix and is Navidrome's to answer.
    let library = get(&app, "/rest/getCoverArt?id=al-1&u=a&t=good&s=s&c=NavidromeUI").await;
    assert_eq!(library.body.as_ref(), [1, 2, 3]);
    // So is a prefix before an id Octo does not know.
    let unknown = get(
        &app,
        "/rest/getCoverArt?id=mf-0123456789abcdefghijkl&u=a&t=good&s=s",
    )
    .await;
    assert_eq!(unknown.body.as_ref(), [1, 2, 3]);
}

// ---- An outside stream that cannot play, or a range past its end ---------------------------

#[tokio::test]
async fn an_unplayable_outside_stream_is_a_404_with_the_subsonic_error() {
    let navidrome = ping_ok().await;
    let downloads = Arc::new(FakeStreams::default());
    let library = Arc::new(FakeLibrary::with_external(
        "external-track",
        "soulseek",
        "track-id",
    ));
    let state = state_with(playback_settings(&navidrome.uri(), false, false), |inner| {
        inner.download_service = downloads.clone();
        inner.local_library = library.clone();
    });
    let app = app(&state);

    let response = get(
        &app,
        "/rest/stream?id=external-track&f=json&u=alice&t=good&s=salt&v=1.16.1&c=test",
    )
    .await;

    assert_eq!(response.status, StatusCode::NOT_FOUND);
    let body: serde_json::Value = serde_json::from_slice(&response.body).expect("JSON");
    assert_eq!(body["subsonic-response"]["status"], "failed");
    assert_eq!(body["subsonic-response"]["error"]["code"], 70);
}

#[tokio::test]
async fn a_size_no_play_has_confirmed_never_refuses_a_range() {
    let navidrome = ping_ok().await;
    let downloads = Arc::new(FakeStreams::default());
    let library = Arc::new(FakeLibrary::default());
    let state = state_with(playback_settings(&navidrome.uri(), false, false), |inner| {
        inner.download_service = downloads.clone();
        inner.local_library = library.clone();
    });
    let id = state.external_id_registry.register(SoulseekRouting {
        kind: RoutingKind::Song,
        artist: Some("Daft Punk".into()),
        title: Some("Veridis Quo".into()),
        ..Default::default()
    });
    state.external_id_registry.set_you_tube_id(&id, "vid-b");
    // A size a lookup reported, before any of the stream was played.
    state.external_id_registry.remember_stream(
        &id,
        "vid-b",
        &octo_core::soulseek::StreamFacts {
            size: Some(1000),
            ..Default::default()
        },
    );
    library.add_external("external-track", "soulseek", &id);
    let app = app(&state);

    let past = send(
        &app,
        Method::GET,
        "/rest/stream?id=external-track&u=alice&t=good&s=salt&v=1.16.1&c=test",
        &[("Range", "bytes=1000-")],
        Body::empty(),
    )
    .await;

    assert_ne!(past.status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(downloads.stream_calls().len(), 1, "relayed");
}

#[tokio::test]
async fn a_range_past_the_end_of_a_known_stream_is_a_416_with_its_size() {
    let navidrome = ping_ok().await;
    let downloads = Arc::new(FakeStreams::default());
    let library = Arc::new(FakeLibrary::default());
    let state = state_with(playback_settings(&navidrome.uri(), false, false), |inner| {
        inner.download_service = downloads.clone();
        inner.local_library = library.clone();
    });
    let id = state.external_id_registry.register(SoulseekRouting {
        kind: RoutingKind::Song,
        artist: Some("Daft Punk".into()),
        title: Some("Emotion".into()),
        ..Default::default()
    });
    state.external_id_registry.set_you_tube_id(&id, "vid-a");
    // A size the bytes of a play confirmed.
    state.external_id_registry.remember_stream(
        &id,
        "vid-a",
        &octo_core::soulseek::StreamFacts {
            size: Some(1000),
            size_confirmed: true,
            ..Default::default()
        },
    );
    library.add_external("external-track", "soulseek", &id);
    let app = app(&state);

    let past = send(
        &app,
        Method::GET,
        "/rest/stream?id=external-track&u=alice&t=good&s=salt&v=1.16.1&c=test",
        &[("Range", "bytes=1000-")],
        Body::empty(),
    )
    .await;
    assert_eq!(past.status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(past.header("content-range"), Some("bytes */1000"));
    assert!(downloads.stream_calls().is_empty(), "nothing relayed");

    // A range inside the stream is relayed as before.
    send(
        &app,
        Method::GET,
        "/rest/stream?id=external-track&u=alice&t=good&s=salt&v=1.16.1&c=test",
        &[("Range", "bytes=999-")],
        Body::empty(),
    )
    .await;
    assert_eq!(downloads.stream_calls().len(), 1);
}

// ---- What a play heard ---------------------------------------------------------------------

mod heard {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use bytes::Bytes;
    use octo_core::fingerprint::heart_gate::HeardSource;

    use super::*;
    use crate::services::i_download_service::DirectStreamInfo;
    use crate::services::prefetch::heard_samples::tests::{BytePrinter, song_bytes};
    use crate::services::prefetch::sample_id_of;
    use crate::services::soulseek::soulseek_metadata_service::SoulseekMetadataService;

    pub(super) const VIDEO_A: &str = "AAAAAAAAAA1";
    pub(super) const VIDEO_B: &str = "BBBBBBBBBB2";
    const SIGNED: &str = "u=alice&t=good&s=salt&v=1.16.1&c=test";

    /// A direct stream of `video`: `bytes`, whole (200) or as the first-byte range `range_end`.
    pub(super) fn video_stream(video: &str, bytes: &[u8], range_end: Option<usize>) -> DirectStreamInfo {
        let total = bytes.len();
        let (status, sent, content_range) = match range_end {
            Some(end) => (206, &bytes[..=end], Some(format!("bytes 0-{end}/{total}"))),
            None => (200, bytes, None),
        };
        let chunks: Vec<std::io::Result<Bytes>> =
            sent.chunks(1024).map(|c| Ok(Bytes::copy_from_slice(c))).collect();
        DirectStreamInfo {
            audio_stream: Box::pin(futures::stream::iter(chunks)),
            content_type: "audio/mp4".into(),
            content_length: Some(sent.len() as u64),
            quality: None,
            status_code: status,
            content_range,
            video_id: Some(video.to_string()),
            itag: None,
        }
    }

    pub(super) struct Played {
        pub state: crate::app::AppState,
        pub downloads: Arc<FakeStreams>,
        pub library: Arc<FakeLibrary>,
        pub id: String,
        _navidrome: MockServer,
    }

    /// A registered outside song, Help!, listed as `external-track`, its plays heard by alice.
    pub(super) async fn played(wait_for_lossless: bool) -> Played {
        played_with(wait_for_lossless, true, None).await
    }

    /// [`played`] with AcoustID `on` or off, and the preview copies `copies` when given.
    pub(super) async fn played_with(
        wait_for_lossless: bool,
        on: bool,
        copies: Option<Arc<crate::services::prefetch::PreviewCopies>>,
    ) -> Played {
        let navidrome = ping_ok().await;
        let downloads = Arc::new(FakeStreams::default());
        let library = Arc::new(FakeLibrary::default());
        let mut settings = playback_settings(&navidrome.uri(), wait_for_lossless, false);
        if !on {
            settings.soulseek.acoust_id_api_key = "off".into();
        }
        let state = state_with(settings, |inner| {
            inner.download_service = downloads.clone();
            inner.local_library = library.clone();
            if let Some(copies) = copies {
                inner.preview_copies = copies;
            }
        });
        state.heard_samples.use_printer(Arc::new(BytePrinter));
        let id = state.external_id_registry.register(SoulseekRouting {
            kind: RoutingKind::Song,
            artist: Some("The Beatles".into()),
            title: Some("Help!".into()),
            duration: Some(200),
            ..Default::default()
        });
        library.add_external("external-track", "soulseek", &id);
        Played {
            state,
            downloads,
            library,
            id,
            _navidrome: navidrome,
        }
    }

    pub(super) async fn stream(state: &crate::app::AppState, range: Option<&str>) -> Reply {
        let headers: Vec<(&str, &str)> = range.map(|r| ("Range", r)).into_iter().collect();
        send(
            &app(state),
            Method::GET,
            &format!("/rest/stream?id=external-track&{SIGNED}"),
            &headers,
            Body::empty(),
        )
        .await
    }

    fn heard_video(state: &crate::app::AppState, id: &str) -> Option<String> {
        state
            .external_id_registry
            .heard_by(id, "alice")
            .and_then(|record| record.source.video_id().map(str::to_string))
    }

    async fn until(condition: impl Fn() -> bool) {
        for _ in 0..500 {
            if condition() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the condition never held");
    }

    #[tokio::test]
    async fn a_stream_keeps_the_video_that_played_and_a_later_re_pin_does_not_change_it() {
        let p = played(false).await;
        let bytes = song_bytes(200.0, 1);
        let answer = bytes.clone();
        p.downloads.answer_with(
            "soulseek",
            &p.id,
            Box::new(move || Some(video_stream(VIDEO_A, &answer, None))),
        );
        let reply = stream(&p.state, None).await;
        assert_eq!(reply.status, StatusCode::OK);
        let record = p
            .state
            .external_id_registry
            .heard_by(&p.id, "alice")
            .expect("heard");
        assert_eq!(
            record.source,
            HeardSource::Video {
                id: VIDEO_A.into(),
                size: Some(bytes.len() as u64),
                content_type: "audio/mp4".into(),
                itag: None,
            }
        );
        p.state.external_id_registry.set_you_tube_id(&p.id, VIDEO_B);
        p.state.external_id_registry.refuse_video(&p.id, VIDEO_A);
        assert_eq!(heard_video(&p.state, &p.id).as_deref(), Some(VIDEO_A));
        // A later range of the same play is no new play.
        stream(&p.state, Some("bytes=1000-")).await;
        assert_eq!(heard_video(&p.state, &p.id).as_deref(), Some(VIDEO_A));
    }

    #[tokio::test]
    async fn a_probe_then_an_open_read_that_swaps_in_another_video_keeps_the_swapped_in_one() {
        let p = played(false).await;
        let bytes = song_bytes(200.0, 1);
        let calls = Arc::new(AtomicUsize::new(0));
        let (answer, count) = (bytes.clone(), calls.clone());
        p.downloads.answer_with(
            "soulseek",
            &p.id,
            Box::new(move || {
                // The probe plays the pinned video; the open read that follows swaps in the
                // pending one.
                Some(match count.fetch_add(1, Ordering::SeqCst) {
                    0 => video_stream(VIDEO_A, &answer, Some(1)),
                    _ => video_stream(VIDEO_B, &answer, None),
                })
            }),
        );
        stream(&p.state, Some("bytes=0-1")).await;
        assert_eq!(heard_video(&p.state, &p.id).as_deref(), Some(VIDEO_A));
        stream(&p.state, Some("bytes=0-")).await;
        assert_eq!(heard_video(&p.state, &p.id).as_deref(), Some(VIDEO_B));
    }

    #[tokio::test]
    async fn a_stream_of_a_decoded_id_then_a_heart_of_the_raw_id_finds_what_was_heard() {
        let p = played(false).await;
        let raw = SoulseekMetadataService::encode_external_id(&SoulseekRouting {
            kind: RoutingKind::Song,
            you_tube_id: Some(VIDEO_A.into()),
            artist: Some("The Beatles".into()),
            title: Some("Yesterday".into()),
            duration: Some(125),
            ..Default::default()
        });
        p.library.add_external("external-track", "soulseek", &raw);
        let answer = song_bytes(125.0, 2);
        p.downloads.answer_with(
            "soulseek",
            &raw,
            Box::new(move || Some(video_stream(VIDEO_A, &answer, None))),
        );
        assert_eq!(
            p.state
                .external_id_registry
                .resolve_song(&raw, false)
                .map(|r| r.lock().you_tube_id.clone()),
            Some(Some(VIDEO_A.into()))
        );
        let before = p.state.external_id_registry.count();
        stream(&p.state, None).await;
        assert_eq!(
            p.state.external_id_registry.count(),
            before + 1,
            "the stream registered it"
        );
        // A heart holds the raw id too, and finds the record through the same resolver.
        assert_eq!(heard_video(&p.state, &raw).as_deref(), Some(VIDEO_A));
    }

    #[tokio::test]
    async fn a_search_re_registering_mid_stream_keeps_the_print_on_the_registered_routing() {
        let p = played(false).await;
        let bytes = song_bytes(200.0, 3);
        let (sender, receiver) = futures::channel::mpsc::unbounded::<std::io::Result<Bytes>>();
        let receiver = Arc::new(parking_lot::Mutex::new(Some(receiver)));
        let total = bytes.len();
        p.downloads.answer_with(
            "soulseek",
            &p.id,
            Box::new(move || {
                let body = receiver.lock().take()?;
                Some(DirectStreamInfo {
                    audio_stream: Box::pin(body),
                    content_type: "audio/mp4".into(),
                    content_length: Some(total as u64),
                    quality: None,
                    status_code: 200,
                    content_range: None,
                    video_id: Some(VIDEO_A.into()),
                    itag: None,
                })
            }),
        );
        let state = p.state.clone();
        let reply = tokio::spawn(async move { stream(&state, None).await });
        let (first, rest) = bytes.split_at(bytes.len() / 2);
        sender.unbounded_send(Ok(Bytes::copy_from_slice(first))).unwrap();
        until(|| p.state.external_id_registry.heard_by(&p.id, "alice").is_some()).await;
        // A search lists the song meanwhile: a new routing object takes the id.
        let before = p.state.external_id_registry.lookup(&p.id).unwrap();
        let again = p.state.external_id_registry.register(SoulseekRouting {
            kind: RoutingKind::Song,
            artist: Some("The Beatles".into()),
            title: Some("Help!".into()),
            duration: Some(200),
            ..Default::default()
        });
        assert_eq!(again, p.id);
        let after = p.state.external_id_registry.lookup(&p.id).unwrap();
        assert!(!before.ptr_eq(&after), "the search swapped in a new routing");
        sender.unbounded_send(Ok(Bytes::copy_from_slice(rest))).unwrap();
        drop(sender);
        assert_eq!(reply.await.unwrap().body.len(), bytes.len());
        let id = p.id.clone();
        let registry = p.state.external_id_registry.clone();
        until(move || {
            registry
                .lookup(&id)
                .and_then(|r| r.lock().heard_by("alice").and_then(|h| h.sample_id.clone()))
                .is_some()
        })
        .await;
        let sample = after
            .lock()
            .heard_by("alice")
            .and_then(|h| h.sample_id.clone())
            .unwrap();
        let prints = p
            .state
            .heard_samples
            .prints(&sample)
            .expect("the print is stored");
        assert!(prints.whole && prints.reaches_end);
    }

    #[tokio::test]
    async fn a_whole_play_then_a_short_read_from_the_first_byte_keeps_the_whole_print() {
        let p = played(false).await;
        let bytes = song_bytes(200.0, 4);
        let calls = Arc::new(AtomicUsize::new(0));
        let (answer, count) = (bytes.clone(), calls.clone());
        p.downloads.answer_with(
            "soulseek",
            &p.id,
            Box::new(move || {
                Some(match count.fetch_add(1, Ordering::SeqCst) {
                    0 => video_stream(VIDEO_A, &answer, None),
                    _ => video_stream(VIDEO_A, &answer, Some(1023)),
                })
            }),
        );
        stream(&p.state, None).await;
        let id = sample_id_of(&HeardSource::Video {
            id: VIDEO_A.into(),
            size: Some(bytes.len() as u64),
            content_type: "audio/mp4".into(),
            itag: None,
        });
        let samples = p.state.heard_samples.clone();
        let whole_id = id.clone();
        until(move || samples.prints(&whole_id).is_some_and(|prints| prints.whole)).await;
        let whole = p.state.heard_samples.prints(&id).unwrap();

        stream(&p.state, Some("bytes=0-1023")).await;
        // Its sample is printed short, and never takes the whole one's place.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(p.state.heard_samples.prints(&id), Some(whole));
        let record = p.state.external_id_registry.heard_by(&p.id, "alice").unwrap();
        assert_eq!(record.sample_id.as_deref(), Some(id.as_str()));
    }
}

// ---- Play outcomes ---------------------------------------------------------------------------

mod outcomes {
    use octo_core::fingerprint::heart_gate::AcquisitionOrigin;

    use super::heard::{VIDEO_A, played_with, stream};
    use super::*;
    use crate::services::common::track_acquisition_queue::PlayOutcome;
    use crate::services::prefetch::PreviewCopies;

    /// The pinned preview's whole copy, a real WAV ffmpeg can make a FLAC of, in its own copies
    /// folder.
    fn copies_with_preview(dir: &std::path::Path) -> Arc<PreviewCopies> {
        let folder = dir.join("copies");
        std::fs::create_dir_all(&folder).unwrap();
        let rate = 8_000u32;
        let frames: Vec<i16> = (0..rate * 3)
            .map(|i| {
                ((f64::from(i) * 2.0 * std::f64::consts::PI * 440.0 / f64::from(rate)).sin() * 8_000.0) as i16
            })
            .collect();
        let data = frames.len() as u32 * 2;
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&rate.to_le_bytes());
        wav.extend_from_slice(&(rate * 2).to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data.to_le_bytes());
        for frame in frames {
            wav.extend_from_slice(&frame.to_le_bytes());
        }
        std::fs::write(folder.join(format!("{VIDEO_A}.preview")), wav).unwrap();
        PreviewCopies::new(
            folder,
            Arc::new(crate::services::you_tube::YouTubeResolver::with_base_url(Some(
                "http://127.0.0.1:1",
            ))),
            octo_core::common::Clock::system(),
            || 0,
        )
    }

    /// A Wait for lossless play of the song pinned to its preview, with the preview's copy at
    /// hand and AcoustID `on` or off.
    async fn lossless_play(on: bool, dir: &std::path::Path) -> super::heard::Played {
        let p = played_with(true, on, Some(copies_with_preview(dir))).await;
        p.state.external_id_registry.set_you_tube_id(&p.id, VIDEO_A);
        p
    }

    fn is_flac(reply: &Reply) -> bool {
        reply.body.starts_with(b"fLaC")
    }

    #[tokio::test]
    async fn with_acoust_id_off_wait_for_lossless_skips_the_wait_and_the_flac_stand_in_streams() {
        let dir = tempfile::tempdir().unwrap();
        let p = lossless_play(false, dir.path()).await;
        let reply = stream(&p.state, None).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.header("content-type"), Some("audio/flac"));
        assert!(is_flac(&reply));
        assert_eq!(
            p.state.track_acquisition_queue.waiting_plays(),
            0,
            "nothing waited for"
        );
        assert!(
            dir.path()
                .join("copies")
                .join("flac")
                .join(format!("{VIDEO_A}.flac"))
                .is_file()
        );
        // The play heard the preview's video.
        let heard = p.state.external_id_registry.heard_by(&p.id, "alice").unwrap();
        assert_eq!(heard.source.video_id(), Some(VIDEO_A));
    }

    #[tokio::test]
    async fn a_copy_the_gate_could_not_prove_resolves_the_wait_at_once_and_the_flac_stand_in_streams() {
        let dir = tempfile::tempdir().unwrap();
        let p = lossless_play(true, dir.path()).await;
        let state = p.state.clone();
        let reply = tokio::spawn(async move { stream(&state, None).await });
        let request = tokio::time::timeout(
            Duration::from_secs(30),
            p.state.track_acquisition_queue.dequeue(&CancellationToken::new()),
        )
        .await
        .expect("queued")
        .expect("a request");
        assert_eq!(request.origin, Some(AcquisitionOrigin::Play));
        p.state.track_acquisition_queue.release(&request);
        // Refused, held or not proven: the gate resolves the play's completion at once.
        request
            .completion
            .try_set_error(anyhow::Error::new(PlayOutcome::NotProven));
        let reply = tokio::time::timeout(Duration::from_secs(30), reply)
            .await
            .expect("the play is answered")
            .unwrap();
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.header("content-type"), Some("audio/flac"));
        assert!(is_flac(&reply));
    }

    #[tokio::test]
    async fn the_flac_stand_in_answers_a_probe_a_seek_and_a_head_as_a_file_does() {
        let dir = tempfile::tempdir().unwrap();
        let p = lossless_play(false, dir.path()).await;
        let url = "/rest/stream?id=external-track&u=alice&t=good&s=salt&v=1.16.1&c=test";
        // Before it is built, a HEAD gets the type alone and builds nothing.
        let head = send(&app(&p.state), Method::HEAD, url, &[], Body::empty()).await;
        assert_eq!(head.status, StatusCode::OK);
        assert_eq!(head.header("content-type"), Some("audio/flac"));
        assert_eq!(head.header("content-length"), None);
        let flac = dir
            .path()
            .join("copies")
            .join("flac")
            .join(format!("{VIDEO_A}.flac"));
        assert!(!flac.exists());

        // The probe counts as the first byte: it builds the file and gets its first two bytes.
        let probe = stream(&p.state, Some("bytes=0-1")).await;
        assert_eq!(probe.status, StatusCode::PARTIAL_CONTENT);
        let length = std::fs::metadata(&flac).expect("built").len();
        assert_eq!(
            probe.header("content-range"),
            Some(format!("bytes 0-1/{length}").as_str())
        );
        assert_eq!(probe.body.as_ref(), b"fL");
        let seek = stream(&p.state, Some("bytes=100-")).await;
        assert_eq!(seek.status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            seek.header("content-range"),
            Some(format!("bytes 100-{}/{length}", length - 1).as_str())
        );
        assert_eq!(seek.body.len() as u64, length - 100);
        let head = send(&app(&p.state), Method::HEAD, url, &[], Body::empty()).await;
        assert_eq!(head.header("content-length"), Some(length.to_string().as_str()));
        assert!(head.body.is_empty());
    }

    #[tokio::test]
    async fn a_play_with_no_playable_source_still_queues_its_download() {
        let navidrome = ping_ok().await;
        let library = Arc::new(FakeLibrary::with_external(
            "external-track",
            "soulseek",
            "track-id",
        ));
        let state = state_with(playback_settings(&navidrome.uri(), false, true), |inner| {
            inner.download_service = Arc::new(FakeStreams::default());
            inner.local_library = library.clone();
        });
        let reply = stream(&state, None).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        for _ in 0..100 {
            if state.track_acquisition_queue.waiting_plays() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(state.track_acquisition_queue.waiting_plays(), 1);
    }
}

// ---- A kept outside song in Octo's apps --------------------------------------------------

/// An outside song with a preview to stream, a library that can be given its file, and the
/// state over both, with getting songs ahead off.
fn kept_song_state(
    navidrome: &MockServer,
    wait_for_lossless: bool,
) -> (crate::app::AppState, Arc<FakeStreams>, Arc<FakeLibrary>) {
    let downloads = Arc::new(FakeStreams::streaming(
        "soulseek",
        "track-id",
        &[1, 2, 3],
        "audio/mp4",
    ));
    let library = Arc::new(FakeLibrary::with_external(
        "external-track",
        "soulseek",
        "track-id",
    ));
    let state = state_with(
        playback_settings(&navidrome.uri(), wait_for_lossless, false),
        |inner| {
            inner.download_service = downloads.clone();
            inner.local_library = library.clone();
        },
    );
    (state, downloads, library)
}

/// The song's file in the library, as a heart leaves it.
fn keep_file(library: &FakeLibrary, dir: &tempfile::TempDir) {
    let path = dir.path().join("song.mp3");
    std::fs::write(&path, [7, 7, 7, 7]).expect("written");
    library.local_paths.lock().insert(
        ("soulseek".into(), "track-id".into()),
        path.to_string_lossy().into_owned(),
    );
}

#[tokio::test]
async fn an_octo_app_plays_a_kept_songs_file_with_getting_songs_ahead_off() {
    let navidrome = ping_ok().await;
    let (state, downloads, library) = kept_song_state(&navidrome, false);
    let dir = tempfile::tempdir().expect("a temp dir");
    keep_file(&library, &dir);
    let app = app(&state);

    let reply = get(
        &app,
        "/rest/stream?id=external-track&u=alice&t=good&s=salt&v=1.16.1&c=Octo",
    )
    .await;

    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body.as_ref(), [7, 7, 7, 7]);
    assert_eq!(reply.header("content-type"), Some("audio/mpeg"));
    assert!(downloads.stream_calls().is_empty(), "no preview opened");
}

#[tokio::test]
async fn other_apps_keep_a_kept_songs_preview() {
    let navidrome = ping_ok().await;
    let (state, downloads, library) = kept_song_state(&navidrome, false);
    let dir = tempfile::tempdir().expect("a temp dir");
    keep_file(&library, &dir);
    let app = app(&state);

    let reply = get(
        &app,
        "/rest/stream?id=external-track&u=alice&t=good&s=salt&v=1.16.1&c=NaviBeat",
    )
    .await;

    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body.as_ref(), [1, 2, 3]);
    assert_eq!(reply.header("content-type"), Some("audio/mp4"));
    assert_eq!(downloads.stream_calls().len(), 1);
}

#[tokio::test]
async fn an_octo_app_play_that_started_on_the_preview_keeps_it_once_the_file_lands() {
    let navidrome = ping_ok().await;
    let (state, downloads, library) = kept_song_state(&navidrome, false);
    let app = app(&state);
    let url = "/rest/stream?id=external-track&u=alice&t=good&s=salt&v=1.16.1&c=Octo";

    let first = send(&app, Method::GET, url, &[("Range", "bytes=0-")], Body::empty()).await;
    assert_eq!(first.body.as_ref(), [1, 2, 3]);

    let dir = tempfile::tempdir().expect("a temp dir");
    keep_file(&library, &dir);
    let later = send(&app, Method::GET, url, &[("Range", "bytes=2-")], Body::empty()).await;

    assert_eq!(later.body.as_ref(), [1, 2, 3], "still the preview");
    assert_eq!(later.header("content-type"), Some("audio/mp4"));
    assert_eq!(downloads.stream_calls().len(), 2);
}

#[tokio::test]
async fn an_octo_app_still_waits_for_lossless_on_a_song_not_kept_yet() {
    let navidrome = ping_ok().await;
    let (state, downloads, _library) = kept_song_state(&navidrome, true);
    let app = app(&state);
    let response = tokio::spawn(async move {
        get(
            &app,
            "/rest/stream?id=external-track&u=alice&t=good&s=salt&v=1.16.1&c=Octo",
        )
        .await
    });

    let queue = state.track_acquisition_queue.clone();
    let request = tokio::time::timeout(Duration::from_secs(30), queue.dequeue(&CancellationToken::new()))
        .await
        .expect("the stream queued an acquisition")
        .expect("a request");
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("octo-playback.flac");
    std::fs::write(&path, [4, 5, 6]).expect("written");
    request
        .completion
        .try_set_result(path.to_string_lossy().into_owned());
    queue.release(&request);

    let response = response.await.expect("the request ran");
    assert_eq!(response.body.as_ref(), [4, 5, 6]);
    assert_eq!(response.header("content-type"), Some("audio/flac"));
    assert!(
        downloads.stream_calls().is_empty(),
        "the play waited for the lossless copy"
    );
}

#[tokio::test]
async fn an_octo_app_play_served_its_lossless_copy_keeps_it_on_the_next_range() {
    let navidrome = ping_ok().await;
    let (state, downloads, library) = kept_song_state(&navidrome, true);
    let app = app(&state);
    let url = "/rest/stream?id=external-track&u=alice&t=good&s=salt&v=1.16.1&c=Octo";
    let first = {
        let app = app.clone();
        tokio::spawn(async move { get(&app, url).await })
    };
    let queue = state.track_acquisition_queue.clone();
    let request = tokio::time::timeout(Duration::from_secs(30), queue.dequeue(&CancellationToken::new()))
        .await
        .expect("the stream queued an acquisition")
        .expect("a request");
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("octo-playback.flac");
    std::fs::write(&path, [4, 5, 6]).expect("written");
    request
        .completion
        .try_set_result(path.to_string_lossy().into_owned());
    queue.release(&request);
    assert_eq!(first.await.expect("ran").body.as_ref(), [4, 5, 6]);
    // The copy is now the song's file in the library.
    library.local_paths.lock().insert(
        ("soulseek".into(), "track-id".into()),
        path.to_string_lossy().into_owned(),
    );

    let later = send(&app, Method::GET, url, &[("Range", "bytes=2-")], Body::empty()).await;
    assert_eq!(later.header("content-type"), Some("audio/flac"));
    assert_eq!(later.body.as_ref(), [6]);
    assert!(downloads.stream_calls().is_empty(), "never the preview");
}

#[test]
fn a_pinned_chart_cover_says_its_country() {
    use super::media::chart_cover_words;
    assert_eq!(
        chart_cover_words("34", Some("jp"), "Popular right now · Japan").as_deref(),
        Some("Popular · Japan")
    );
    assert_eq!(
        chart_cover_words("34", Some("ae"), "Popular right now · United Arab Emirates").as_deref(),
        Some("Popular · United Arab Emirates")
    );
    assert_eq!(
        chart_cover_words("15", Some("jp"), "Top R&B/Soul · Japan").as_deref(),
        Some("Top R&B/Soul · Japan")
    );
    // The listener's own lists say what they always said.
    assert_eq!(
        chart_cover_words("34", None, "Popular right now").as_deref(),
        Some("Popular")
    );
    assert_eq!(chart_cover_words("18", None, "Top Hip-Hop/Rap"), None);
}

// ---- Chart lists for an app that signs in with an API key ----------------------------------

mod api_key_charts {
    use std::sync::Arc;
    use std::time::Duration;

    use octo_core::common::Clock;
    use octo_core::settings::GeneratedPlaylistSettings;
    use octo_subsonic::subsonic_request_parser::Parameters;
    use wiremock::matchers::{method, path, query_param_is_missing};

    use super::*;
    use crate::services::charts::ChartCountryStore;
    use crate::services::charts::apple_music_pages::AppleMusicPages;
    use crate::services::common::TopSongsService;
    use crate::services::common::test_fakes::FakeMetadata;
    use crate::services::cover_art::ArtworkHints;
    use crate::services::library::ChartPlaylistService;
    use crate::services::metadata::deezer_metadata_service::DeezerMetadataService;
    use crate::services::metadata::deezer_rate_limit_handler::DeezerRateLimitHandler;
    use crate::services::metadata::deezer_rate_limiter::DeezerRateLimiter;
    use crate::services::subsonic::SubsonicProxyService;

    const US_ALL: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/rust-migration/fixtures/apple-music/us_all.html"
    ));

    /// Navidrome with one API key, "alice-key", that tokenInfo names alice; a normal sign-in
    /// with the token "good" is accepted too. Every library search finds nothing.
    struct ChartNavidrome;

    impl Respond for ChartNavidrome {
        fn respond(&self, request: &MockRequest) -> ResponseTemplate {
            let query = query_of(request);
            let path = request.url.path();
            let key_ok = query.get("apiKey").map(String::as_str) == Some("alice-key");
            if path.ends_with("/rest/ping") {
                let ok = key_ok || query.get("t").map(String::as_str) == Some("good");
                return navidrome_json(if ok {
                    NAVIDROME_OK
                } else {
                    NAVIDROME_WRONG_PASSWORD
                });
            }
            if path.ends_with("/rest/tokenInfo") {
                return navidrome_json(if key_ok {
                    r#"{"subsonic-response":{"status":"ok","version":"1.16.1","tokenInfo":{"username":"alice"}}}"#
                } else {
                    r#"{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":44,"message":"Invalid API key"}}}"#
                });
            }
            if path.ends_with("/rest/getPlaylists") {
                return navidrome_json(
                    r#"{"subsonic-response":{"status":"ok","version":"1.16.1","playlists":{"playlist":[]}}}"#,
                );
            }
            if path.ends_with("/rest/search3") {
                return navidrome_json(
                    r#"{"subsonic-response":{"status":"ok","version":"1.16.1","searchResult3":{}}}"#,
                );
            }
            ResponseTemplate::new(404)
        }
    }

    struct Fixture {
        navidrome: MockServer,
        state: crate::app::AppState,
        _apple: MockServer,
    }

    impl Fixture {
        async fn new() -> Fixture {
            let navidrome = MockServer::builder().start().await;
            Mock::given(any())
                .respond_with(ChartNavidrome)
                .mount(&navidrome)
                .await;
            let apple = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/us/new/top-charts/songs"))
                .and(query_param_is_missing("genreId"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_raw(US_ALL.as_bytes().to_vec(), "text/html"),
                )
                .mount(&apple)
                .await;
            let mut settings = settings(&navidrome.uri());
            settings.subsonic.enable_search_discovery = true;
            settings.generated_playlists = GeneratedPlaylistSettings {
                charts: String::new(),
                chart_country: "us".to_string(),
                ..Default::default()
            };
            let apple_url = apple.uri();
            let state = state_with(settings, |inner| {
                let settings = Arc::clone(&inner.settings);
                let registry = Arc::clone(&inner.external_id_registry);
                let http = Arc::new(DeezerRateLimitHandler::new(Arc::new(DeezerRateLimiter::new())));
                let deezer = DeezerMetadataService::with_base_url(
                    http,
                    Arc::clone(&settings),
                    format!("http://{}", crate::services::test_support::refused_address()),
                );
                let top_songs = TopSongsService::new(
                    deezer,
                    None,
                    Arc::new(FakeMetadata::default()),
                    Arc::clone(&registry),
                    Arc::new(AppleMusicPages::with_base_url(&apple_url, Duration::ZERO)),
                    Arc::new(ArtworkHints::new()),
                    Arc::clone(&settings),
                )
                .with_listeners(Arc::new(ChartCountryStore::new(None)));
                inner.chart_playlists = Arc::new(ChartPlaylistService::new(
                    top_songs.clone(),
                    Arc::clone(&inner.subsonic_response_builder),
                    registry,
                    SubsonicProxyService::new(Arc::clone(&settings)),
                    settings,
                    Clock::system(),
                ));
                inner.top_songs = top_songs;
            });
            Fixture {
                navidrome,
                state,
                _apple: apple,
            }
        }

        async fn token_infos(&self) -> usize {
            received_paths(&self.navidrome)
                .await
                .iter()
                .filter(|p| p.ends_with("/rest/tokenInfo"))
                .count()
        }

        /// alice's chart list id, with the list made for her.
        async fn alices_list(&self) -> String {
            let sign_in: Parameters = [
                ("u", "alice"),
                ("t", "good"),
                ("s", "salt"),
                ("v", "1.16.1"),
                ("c", "test"),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
            self.state
                .chart_playlists
                .songs("alice", octo_core::charts::chart_settings::OVERALL, &sign_in)
                .await;
            let lists = self.state.chart_playlists.list("alice", &sign_in).await;
            assert_eq!(lists.len(), 1, "one chart list is made");
            lists[0].id.clone()
        }
    }

    const KEY: &str = "apiKey=alice-key&v=1.16.1&c=test&f=json";

    #[tokio::test]
    async fn an_api_key_app_lists_its_chart_lists_and_mixes() {
        let fixture = Fixture::new().await;
        let id = fixture.alices_list().await;

        let reply = get(&app(&fixture.state), &format!("/rest/getPlaylists.view?{KEY}")).await;

        let text = reply.text();
        assert!(text.contains("Popular right now"), "{text}");
        assert!(text.contains(&id), "{text}");
    }

    #[tokio::test]
    async fn an_api_key_app_opens_a_chart_list() {
        let fixture = Fixture::new().await;
        let id = fixture.alices_list().await;

        let reply = get(
            &app(&fixture.state),
            &format!("/rest/getPlaylist.view?id={id}&{KEY}"),
        )
        .await;

        let envelope = reply.envelope();
        assert_eq!(envelope["status"], "ok", "{}", reply.text());
        assert_eq!(
            envelope["playlist"]["name"],
            "Popular right now",
            "{}",
            reply.text()
        );
        assert!(
            !envelope["playlist"]["entry"]
                .as_array()
                .expect("songs")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn an_api_key_app_gets_a_chart_cover() {
        let fixture = Fixture::new().await;
        let id = fixture.alices_list().await;

        let reply = get(
            &app(&fixture.state),
            &format!("/rest/getCoverArt.view?id={id}&size=64&{KEY}"),
        )
        .await;

        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.header("content-type"), Some("image/jpeg"));
        assert!(
            !received_paths(&fixture.navidrome)
                .await
                .iter()
                .any(|p| p.ends_with("/rest/getCoverArt")),
            "the chart cover is Octo's own"
        );
    }

    #[tokio::test]
    async fn an_unsigned_cover_request_never_asks_token_info() {
        let fixture = Fixture::new().await;
        let id = fixture.alices_list().await;
        let before = fixture.token_infos().await;

        get(
            &app(&fixture.state),
            &format!("/rest/getCoverArt.view?id={id}&apiKey=wrong-key&v=1.16.1&c=test"),
        )
        .await;
        get(
            &app(&fixture.state),
            &format!("/rest/getCoverArt.view?id={id}&v=1.16.1&c=test"),
        )
        .await;

        assert_eq!(fixture.token_infos().await, before, "tokenInfo was never asked");
    }

    #[tokio::test]
    async fn a_normal_sign_in_never_asks_token_info() {
        let fixture = Fixture::new().await;
        let id = fixture.alices_list().await;

        let lists = get(
            &app(&fixture.state),
            "/rest/getPlaylists.view?u=alice&t=good&s=salt&v=1.16.1&c=test&f=json",
        )
        .await;
        let cover = get(
            &app(&fixture.state),
            &format!("/rest/getCoverArt.view?id={id}&size=64&u=alice&t=good&s=salt&v=1.16.1&c=test"),
        )
        .await;

        assert!(lists.text().contains("Popular right now"), "{}", lists.text());
        assert_eq!(cover.header("content-type"), Some("image/jpeg"));
        assert_eq!(fixture.token_infos().await, 0);
    }
}
