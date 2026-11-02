//! The dashboard's light, the newest songs in the fetched-songs log with the colors of their
//! covers. The Status page shows them as Latest fetched, and every page takes its light from the
//! one chosen. Behind the same guard as the rest of `/api/admin`.

use std::collections::HashSet;

use axum::extract::State;
use axum::response::Response;
use axum::routing::get;
use octo_core::common::dotnet::{is_blank, ordinal_ignore_case_key};
use octo_core::models::download::DownloadHistoryEntry;
use serde::Serialize;

use super::request_binding::ok;
use crate::app::AppState;
use crate::http::routes::RouteSet;
use crate::services::admin::fetched_covers::{LARGE_SIDE, entry_cover_path};

pub fn routes() -> RouteSet {
    RouteSet::new().route("/api/admin/ambient", get(get_ambient))
}

/// How many songs Latest fetched offers.
pub const SONG_COUNT: usize = 6;
// How far back the log is read to find that many different covers.
const LOOK_BACK: i32 = 100;

/// `AmbientSong`: one song of Latest fetched with its cover's lights and accent.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AmbientSong {
    pub artist: String,
    pub title: String,
    pub album: Option<String>,
    pub format: String,
    pub source: String,
    pub cover_art_url: Option<String>,
    /// Where Octo serves the song's cover, None when it has no picture for it.
    pub cover: Option<String>,
    pub size_bytes: i64,
    pub downloaded_at: String,
    pub colors: Option<Vec<String>>,
    pub accent: Option<String>,
}

#[derive(Serialize)]
struct AmbientAnswer {
    songs: Vec<AmbientSong>,
}

async fn get_ambient(State(state): State<AppState>) -> Response {
    let recent = newest_per_cover(state.download_history.get_recent(LOOK_BACK), SONG_COUNT);
    // The large copy Latest fetched draws is the one read for the colors.
    let found = futures::future::join_all(
        recent
            .iter()
            .map(|entry| state.fetched_covers.entry_cover(entry, LARGE_SIDE)),
    )
    .await;
    let songs = recent
        .into_iter()
        .zip(found)
        .map(|(e, picture)| {
            let palette = picture
                .as_deref()
                .and_then(|picture| state.ambient_palettes.palette_of(picture));
            AmbientSong {
                cover: picture.is_some().then(|| entry_cover_path(&e.downloaded_at)),
                artist: e.artist,
                title: e.title,
                album: e.album,
                format: e.format,
                source: e.source,
                cover_art_url: e.cover_art_url,
                size_bytes: e.size_bytes,
                downloaded_at: e.downloaded_at,
                colors: palette.as_ref().map(|p| p.colors.clone()),
                accent: palette.map(|p| p.accent),
            }
        })
        .collect();
    ok(&AmbientAnswer { songs })
}

/// The newest song of each cover, newest first. Four songs from one album are one choice of
/// light, not four, so an album fetched whole does not fill Recently added with one cover. A
/// song with no cover counts by its artist and album.
pub fn newest_per_cover(newest_first: Vec<DownloadHistoryEntry>, count: usize) -> Vec<DownloadHistoryEntry> {
    let mut seen = HashSet::new();
    let mut picked = Vec::new();
    for entry in newest_first {
        let key = match entry.cover_art_url.as_deref() {
            Some(url) if !is_blank(url) => url.to_string(),
            _ => format!(
                "{}\n{}",
                entry.artist,
                entry.album.as_deref().unwrap_or(&entry.title)
            ),
        };
        if !seen.insert(ordinal_ignore_case_key(&key)) {
            continue;
        }
        picked.push(entry);
        if picked.len() == count {
            break;
        }
    }
    picked
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, StatusCode};
    use octo_core::models::download::DownloadHistoryEntry;
    use octo_core::settings::AppSettings;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::app::AppState;
    use crate::controllers::admin::test_app::{app, send};

    fn entry(title: &str, cover: Option<String>) -> DownloadHistoryEntry {
        DownloadHistoryEntry {
            artist: "Zephyr Echo".into(),
            title: title.into(),
            album: Some(title.into()),
            format: "FLAC".into(),
            source: "Soulseek".into(),
            cover_art_url: cover,
            size_bytes: 1024,
            downloaded_at: "2021-06-03T12:00:00.0000000Z".into(),
            ..Default::default()
        }
    }

    /// The newest songs of the log with their covers' lights: a cover read once, a cover that
    /// is missing or not a web address left without color, and the log never touched.
    #[tokio::test]
    async fn the_endpoint_lights_the_newest_songs_from_their_covers() {
        let server = MockServer::builder().start().await;
        let cover = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/rust-migration/fixtures/covers/sample-cover.jpg"
        ))
        .expect("the sample cover");
        Mock::given(method("GET"))
            .and(path("/glass.jpg"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(cover))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/gone.jpg"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let state = AppState::for_tests(AppSettings::default());
        // The log is newest first: the last recorded is the first listed.
        state
            .download_history
            .record(entry("Odd", Some("not a web address".into())));
        state.download_history.record(entry("Bare", None));
        state
            .download_history
            .record(entry("Gone", Some(format!("{}/gone.jpg", server.uri()))));
        state
            .download_history
            .record(entry("Glass", Some(format!("{}/glass.jpg", server.uri()))));
        let app = app(state.clone());
        let token = state.browse_sessions.create("admin");

        for _ in 0..2 {
            let reply = send(
                &app,
                Method::GET,
                "/api/admin/ambient",
                &[("X-Octo-Browse-Token", &token)],
                None,
            )
            .await;
            assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
            let songs = reply.json()["songs"].clone();
            let titles: Vec<&str> = songs
                .as_array()
                .unwrap()
                .iter()
                .map(|s| s["title"].as_str().unwrap())
                .collect();
            assert_eq!(titles, ["Glass", "Gone", "Bare", "Odd"]);
            assert_eq!(songs[0]["colors"], json!(["#a967be", "#c85c87", "#727bdb"]));
            assert_eq!(songs[0]["accent"], "#e49ffa");
            assert_eq!(songs[0]["sizeBytes"], 1024);
            assert_eq!(songs[0]["downloadedAt"], "2021-06-03T12:00:00.0000000Z");
            for song in &songs.as_array().unwrap()[1..] {
                assert_eq!(song["colors"], serde_json::Value::Null);
                assert_eq!(song["accent"], serde_json::Value::Null);
            }
        }
        // The glass cover was fetched once (`expect(1)`), the second answer came from memory.
        server.verify().await;
    }

    #[tokio::test]
    async fn an_empty_log_has_no_songs() {
        let state = AppState::for_tests(AppSettings::default());
        let app = app(state.clone());
        let token = state.browse_sessions.create_recovery();
        let reply = send(
            &app,
            Method::GET,
            "/api/admin/ambient",
            &[("X-Octo-Browse-Token", &token)],
            None,
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.text(), r#"{"songs":[]}"#);
    }
}
