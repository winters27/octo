use std::sync::Arc;

use async_trait::async_trait;
use axum::http::{Method, StatusCode};
use octo_core::fingerprint::heart_gate::{AcquisitionOrigin, HeartGate};
use octo_core::settings::{AppSettings, LidarrSettings};
use parking_lot::Mutex;

use super::*;
use crate::controllers::admin::test_app::{app, send};
use crate::services::admin::SignInThrottle;
use crate::services::lidarr::ILidarrHeartAcquisitionService;

/// What Lidarr's Webhook connection posts after importing an album: the album is singular,
/// the files are `trackFiles`, and the download and its client are named.
const DOWNLOAD: &str = r#"{
  "eventType": "Download",
  "instanceName": "Lidarr",
  "applicationUrl": "",
  "downloadClient": "qBittorrent",
  "downloadClientType": "qBittorrent",
  "downloadId": "ABC123",
  "artist": { "id": 5, "name": "Massive Attack", "path": "/data/music/Massive Attack", "mbId": "ma" },
  "album": { "id": 7, "mbId": "rg-mezzanine", "title": "Mezzanine", "albumType": "Album",
             "secondaryAlbumTypes": [], "releaseDate": "1998-04-20T00:00:00Z", "genres": [], "images": [] },
  "tracks": [ { "id": 1, "title": "Angel", "trackNumber": "1", "qualityVersion": 0 } ],
  "trackFiles": [
    { "id": 100, "path": "/data/music/Massive Attack/Mezzanine/01 - Angel.flac", "quality": "FLAC",
      "qualityVersion": 1, "size": 1000, "dateAdded": "2026-10-09T10:00:00Z" },
    { "id": 101, "path": "/data/music/Massive Attack/Mezzanine/03 - Teardrop.flac", "quality": "FLAC",
      "qualityVersion": 1, "size": 1000, "dateAdded": "2026-10-09T10:00:00Z" }
  ],
  "isUpgrade": false
}"#;

const SECRET: &str = "hush-1234";

#[derive(Default)]
struct Hearts {
    events: Mutex<Vec<LidarrImportEvent>>,
    /// How often Lidarr's webhook said its health changed or it grabbed.
    health_events: Mutex<usize>,
}

#[async_trait]
impl ILidarrHeartAcquisitionService for Hearts {
    async fn try_acquire_track(
        &self,
        _: &str,
        _: &str,
        _: bool,
        _: Option<&str>,
        _: AcquisitionOrigin,
        _: &[HeartGate],
    ) -> anyhow::Result<bool> {
        Ok(false)
    }

    async fn try_acquire_album(
        &self,
        _: &str,
        _: &str,
        _: bool,
        _: Option<&str>,
        _: AcquisitionOrigin,
        _: &[HeartGate],
    ) -> anyhow::Result<bool> {
        Ok(false)
    }

    async fn lidarr_imported(&self, event: LidarrImportEvent) -> LidarrImportOutcome {
        self.events.lock().push(event);
        LidarrImportOutcome::Woke
    }

    fn lidarr_health_event(&self) {
        *self.health_events.lock() += 1;
    }
}

fn basic(user: &str, password: &str) -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"))
    )
}

fn with_secret(secret: Option<&str>) -> (crate::http::pipeline::App, Arc<Hearts>) {
    let mut state = AppState::for_tests(AppSettings {
        lidarr: LidarrSettings {
            webhook_secret: secret.map(str::to_string),
            ..Default::default()
        },
        ..Default::default()
    });
    let hearts = Arc::new(Hearts::default());
    Arc::get_mut(&mut state.inner)
        .expect("nothing else holds a fresh test state")
        .lidarr_hearts = hearts.clone();
    (app(state), hearts)
}

#[tokio::test]
async fn the_webhook_is_off_until_a_secret_is_set() {
    let (app, hearts) = with_secret(None);
    let auth = basic("lidarr", SECRET);

    let reply = send(
        &app,
        Method::POST,
        WEBHOOK_PATH,
        &[("authorization", &auth)],
        Some(DOWNLOAD),
    )
    .await;

    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert!(hearts.events.lock().is_empty());
    let (blank, _) = with_secret(Some("  "));
    let reply = send(
        &blank,
        Method::POST,
        WEBHOOK_PATH,
        &[("authorization", &auth)],
        Some(DOWNLOAD),
    )
    .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_wrong_or_missing_password_is_refused() {
    let (app, hearts) = with_secret(Some(SECRET));

    let none = send(&app, Method::POST, WEBHOOK_PATH, &[], Some(DOWNLOAD)).await;
    let wrong_auth = basic("lidarr", "hush-1235");
    let wrong = send(
        &app,
        Method::POST,
        WEBHOOK_PATH,
        &[("authorization", &wrong_auth)],
        Some(DOWNLOAD),
    )
    .await;
    let longer_auth = basic("lidarr", "hush-12345");
    let longer = send(
        &app,
        Method::POST,
        WEBHOOK_PATH,
        &[("authorization", &longer_auth)],
        Some(DOWNLOAD),
    )
    .await;
    let bearer = format!("Bearer {SECRET}");
    let other_scheme = send(
        &app,
        Method::POST,
        WEBHOOK_PATH,
        &[("authorization", &bearer)],
        Some(DOWNLOAD),
    )
    .await;

    for reply in [none, wrong, longer, other_scheme] {
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
        assert_eq!(reply.header("www-authenticate"), Some("Basic realm=\"Octo\""));
    }
    assert!(hearts.events.lock().is_empty());
}

#[tokio::test]
async fn a_download_with_the_right_password_reaches_the_hearts() {
    let (app, hearts) = with_secret(Some(SECRET));
    let auth = basic("any-name", SECRET);

    let reply = send(
        &app,
        Method::POST,
        WEBHOOK_PATH,
        &[("authorization", &auth)],
        Some(DOWNLOAD),
    )
    .await;

    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json()["taken"], "waiting");
    let events = hearts.events.lock().clone();
    assert_eq!(
        events,
        [LidarrImportEvent {
            foreign_album_id: "rg-mezzanine".into(),
            artist: Some("Massive Attack".into()),
            album: Some("Mezzanine".into()),
            paths: vec![
                "/data/music/Massive Attack/Mezzanine/01 - Angel.flac".into(),
                "/data/music/Massive Attack/Mezzanine/03 - Teardrop.flac".into(),
            ],
            is_upgrade: false,
            download_id: Some("ABC123".into()),
            download_client: Some("qBittorrent".into()),
        }]
    );
}

/// A folder import names no download client, whatever download id Lidarr guessed for it: the
/// event says so, so the hearts never take it for one of Octo's own downloads.
#[test]
fn a_folder_imports_event_carries_its_guessed_id_and_no_client() {
    let mut payload: Value = serde_json::from_str(DOWNLOAD).expect("JSON");
    let fields = payload.as_object_mut().expect("an object");
    fields.remove("downloadClient");
    fields.remove("downloadClientType");
    fields.insert("downloadId".into(), Value::from("dl-1"));

    let event = import_event(&payload).expect("an album");

    assert_eq!(event.download_id.as_deref(), Some("dl-1"));
    assert_eq!(event.download_client, None);
}

/// Lidarr's `HealthRestored` and `Grab` events reach the hearts as news of Lidarr's health, so a
/// heart waiting for its indexers looks at once.
#[tokio::test]
async fn health_restored_and_grab_events_reach_the_hearts() {
    let (app, hearts) = with_secret(Some(SECRET));
    let auth = basic("lidarr", SECRET);

    for body in [
        r#"{"eventType":"HealthRestored","level":"error","message":"Indexers are available again","type":"IndexerStatusCheck"}"#,
        r#"{"eventType":"Grab","albums":[{"id":7,"mbId":"rg-mezzanine"}],"downloadId":"dl-2"}"#,
    ] {
        let reply = send(
            &app,
            Method::POST,
            WEBHOOK_PATH,
            &[("authorization", &auth)],
            Some(body),
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.json()["ok"], true);
    }

    assert_eq!(*hearts.health_events.lock(), 2);
    assert!(hearts.events.lock().is_empty());
}

#[tokio::test]
async fn lidarrs_test_event_is_answered_and_other_events_are_ignored() {
    let (app, hearts) = with_secret(Some(SECRET));
    let auth = basic("lidarr", SECRET);

    let test = send(
        &app,
        Method::POST,
        WEBHOOK_PATH,
        &[("authorization", &auth)],
        Some(r#"{"eventType":"Test","artist":{"id":1,"name":"Test Name"},"albums":[{"id":123,"title":"Test title"}]}"#),
    )
    .await;
    let rename = send(
        &app,
        Method::POST,
        WEBHOOK_PATH,
        &[("authorization", &auth)],
        Some(r#"{"eventType":"Rename","artist":{"id":5,"name":"Massive Attack"}}"#),
    )
    .await;
    let no_album = send(
        &app,
        Method::POST,
        WEBHOOK_PATH,
        &[("authorization", &auth)],
        Some(r#"{"eventType":"Download","trackFiles":[]}"#),
    )
    .await;
    let not_json = send(
        &app,
        Method::POST,
        WEBHOOK_PATH,
        &[("authorization", &auth)],
        Some("{"),
    )
    .await;

    assert_eq!(test.status, StatusCode::OK);
    assert_eq!(test.json()["ok"], true);
    assert_eq!(rename.status, StatusCode::OK);
    assert_eq!(rename.json()["taken"], "ignored");
    assert_eq!(no_album.json()["taken"], "ignored");
    assert_eq!(not_json.status, StatusCode::BAD_REQUEST);
    assert!(hearts.events.lock().is_empty());
}

#[test]
fn a_password_with_a_colon_is_read_whole() {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        basic("user", "a:b").parse().expect("a header"),
    );

    assert!(authorized(&headers, "a:b"));
    assert!(!authorized(&headers, "b"));
}

/// Guessing the password is slowed down: after ten wrong ones from an address, it waits, and
/// even the right password is turned away until then.
#[tokio::test]
async fn wrong_passwords_from_one_address_make_it_wait() {
    let (app, hearts) = with_secret(Some(SECRET));
    let wrong = basic("lidarr", "guess");
    for _ in 0..SignInThrottle::MAX_FAILURES {
        let reply = send(
            &app,
            Method::POST,
            WEBHOOK_PATH,
            &[("authorization", &wrong)],
            Some(DOWNLOAD),
        )
        .await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    }
    let right = basic("lidarr", SECRET);

    let reply = send(
        &app,
        Method::POST,
        WEBHOOK_PATH,
        &[("authorization", &right)],
        Some(DOWNLOAD),
    )
    .await;

    assert_eq!(reply.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(reply.header("retry-after").is_some());
    assert!(hearts.events.lock().is_empty());
}

#[tokio::test]
async fn the_right_password_is_never_counted_against_an_address() {
    let (app, hearts) = with_secret(Some(SECRET));
    let right = basic("lidarr", SECRET);
    for _ in 0..SignInThrottle::MAX_FAILURES + 2 {
        let reply = send(
            &app,
            Method::POST,
            WEBHOOK_PATH,
            &[("authorization", &right)],
            Some(DOWNLOAD),
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
    }
    assert_eq!(hearts.events.lock().len(), SignInThrottle::MAX_FAILURES + 2);
}
