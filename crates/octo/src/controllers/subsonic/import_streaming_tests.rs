//! Real Subsonic routes over fake Navidrome and streaming services. No music services,
//! Soulseek peers, client-specific extensions or automatic downloads are needed.

use std::sync::Arc;

use axum::body::Body;
use axum::http::Method;
use octo_core::settings::GeneratedPlaylistSettings;
use parking_lot::Mutex;
use serde_json::json;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use super::test_app::*;
use crate::app::{AppState, TestServices};
use crate::http::pipeline::App;
use crate::services::imports::ImportTrack;
use crate::services::imports::import_streaming::preview_song;
use crate::services::imports::test_support::row;
use crate::services::library::quality_upgrade_worker::LibrarySongRow;
use crate::services::soulseek::ExternalIdRegistry;

const AUTH: &str = "u=alice&t=good&s=salt&v=1.16.1&c=test";

struct Rig {
    server: MockServer,
    state: AppState,
    app: App,
    library: Arc<Mutex<Vec<LibrarySongRow>>>,
    preview: String,
    streams: Arc<FakeStreams>,
    import_id: String,
    playlist_id: String,
}

async fn rig() -> Rig {
    rig_with_duration(true).await
}

async fn rig_with_duration(known: bool) -> Rig {
    let server = MockServer::start().await;
    Mock::given(any()).respond_with(|request: &Request| {
        let query = query_of(request);
        let json = query.get("f").map(String::as_str) == Some("json");
            let api_key = query.get("apiKey").map(String::as_str);
            let key_auth = matches!(api_key, Some("alice-key" | "bob-key")) && !query.contains_key("u");
            if query.get("t").map(String::as_str) != Some("good") && !key_auth {
            return if json { navidrome_json(NAVIDROME_WRONG_PASSWORD) } else {
                ResponseTemplate::new(200).set_body_raw(
                    r#"<subsonic-response status="failed" version="1.16.1"><error code="40" message="Wrong password"/></subsonic-response>"#, "application/xml")
            };
        }
        match request.url.path().trim_end_matches(".view") {
                "/rest/tokenInfo" => navidrome_json(&json!({"subsonic-response": {
                    "status": "ok", "version": "1.16.1", "tokenInfo": {"username": if api_key == Some("alice-key") {"alice"} else {"bob"}}
                }}).to_string()),
            "/rest/getPlaylists" if json => navidrome_json(
                r#"{"subsonic-response":{"status":"ok","version":"1.16.1","playlists":{"playlist":[{"id":"nd-list","name":"Existing","songCount":0}]}}}"#),
            "/rest/getPlaylists" => ResponseTemplate::new(200).set_body_raw(
                r#"<subsonic-response xmlns="http://subsonic.org/restapi" status="ok" version="1.16.1"><playlists><playlist id="nd-list" name="Existing" songCount="0"/></playlists></subsonic-response>"#, "application/xml"),
            "/rest/getSong" => {
                let id = query.get("id").map(String::as_str).unwrap_or_default();
                navidrome_json(&json!({"subsonic-response": {"status": "ok", "version": "1.16.1", "song": {
                    "id": id, "title": if id == "nd-teardrop" {"Teardrop"} else {"Angel"},
                    "artist": "Massive Attack", "album": "Mezzanine", "duration": 330,
                    "suffix": "flac", "contentType": "audio/flac", "bitRate": 900,
                }}}).to_string())
            }
            _ if !json => ResponseTemplate::new(200).set_body_raw(
                r#"<subsonic-response xmlns="http://subsonic.org/restapi" status="ok" version="1.16.1"/>"#, "application/xml"),
            _ => navidrome_json(NAVIDROME_OK),
        }
    }).mount(&server).await;
    let registry = Arc::new(ExternalIdRegistry::in_memory());
    let preview = preview_song(
        &registry,
        &ImportTrack {
            title: "Angel".into(),
            artist: "Massive Attack".into(),
            seconds: known.then_some(379),
            ..Default::default()
        },
    )
    .id;
    let streams = Arc::new(FakeStreams::streaming(
        "soulseek",
        &preview,
        b"preview audio",
        "audio/mp4",
    ));
    let mut config = settings(&server.uri());
    config.subsonic.auto_detect_download_path = false;
    config.subsonic.admin_username = Some("admin".into());
    config.subsonic.admin_password = Some("synthetic".into());
    config.soulseek.base_url = Some(server.uri());
    config.generated_playlists = GeneratedPlaylistSettings {
        enabled: false,
        new_releases: false,
        rediscover: false,
        deep_cuts: false,
        liked_songs: false,
        popular_now: false,
        charts: String::new(),
        ..Default::default()
    };
    let state = AppState::for_tests_with(
        config,
        TestServices {
            external_id_registry: Some(registry),
            download_service: Some(streams.clone()),
            deezer_base_url: Some(server.uri()),
            raw_settings: vec![("YouTube:ShimUrl".into(), server.uri())],
            ..Default::default()
        },
    );
    let library = Arc::new(Mutex::new(vec![row(
        "nd-teardrop",
        "Massive Attack",
        "Teardrop",
        Some(330),
        Some("Mezzanine"),
        "flac",
        900,
    )]));
    let pages = library.clone();
    state
        .imports
        .matcher()
        .set_read_page(Arc::new(move |start, count| {
            let page: Vec<_> = pages
                .lock()
                .iter()
                .skip(start as usize)
                .take(count as usize)
                .cloned()
                .collect();
            let n = page.len();
            Box::pin(async move { Some((page, n)) })
        }));
    let csv = if known {
        "Track Name,Artist Name(s),Track Duration (ms)\nTeardrop,Massive Attack,330000\nAngel,Massive Attack,379000\nTeardrop,Massive Attack,330000\n"
    } else {
        "Track Name,Artist Name(s)\nTeardrop,Massive Attack\nAngel,Massive Attack\nTeardrop,Massive Attack\n"
    };
    let imported = state
        .imports
        .add_file("alice", "Road trip.csv", csv.as_bytes())
        .await;
    assert!(imported.ok, "{}", imported.message);
    let import_id = imported.list_id.unwrap();
    assert!(state.imports.set_streaming("alice", &import_id, true).ok);
    let playlist_id = state.imports.overview("alice").lists[0]
        .streaming_playlist_id
        .clone()
        .unwrap();
    Rig {
        app: app(&state),
        state,
        server,
        library,
        preview,
        streams,
        import_id,
        playlist_id,
    }
}

#[tokio::test]
async fn import_streaming_standard_clients_get_all_entries_in_order_as_json_and_xml() {
    let r = rig().await;
    for client in ["Arpeggi", "Feishin", "Psysonic"] {
        let auth = auth("alice", "good", client);
        let listing = get(&r.app, &format!("/rest/getPlaylists.view?{auth}&f=json"))
            .await
            .envelope();
        let rows = listing["playlists"]["playlist"].as_array().unwrap();
        assert!(rows.iter().any(|row| row["id"] == "nd-list"));
        let row = rows.iter().find(|row| row["id"] == r.playlist_id).unwrap();
        assert_eq!(row["songCount"], 3);
        assert_eq!(row["owner"], "alice");
        assert_eq!(row["public"], false);

        let playlist = get(
            &r.app,
            &format!("/rest/getPlaylist.view?{auth}&f=json&id={}", r.playlist_id),
        )
        .await
        .envelope()["playlist"]
            .clone();
        let entries = playlist["entry"].as_array().unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|song| song["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["nd-teardrop", &r.preview, "nd-teardrop"]
        );
        assert_eq!(entries[0]["suffix"], "flac");
        assert_eq!(entries[1]["isExternal"], true);
        assert_eq!(entries[1]["duration"], 379);
        assert_ne!(entries[1]["suffix"], "flac");
        assert_eq!(playlist["readonly"], true);

        let xml = get(
            &r.app,
            &format!("/rest/getPlaylist.view?{auth}&id={}", r.playlist_id),
        )
        .await
        .text();
        assert_eq!(xml.matches("<entry ").count(), 3, "{xml}");
        assert!(xml.contains(&format!(r#"id="{}""#, r.preview)), "{xml}");
        let listed_xml = get(&r.app, &format!("/rest/getPlaylists.view?{auth}"))
            .await
            .text();
        assert!(listed_xml.contains(&r.playlist_id), "{listed_xml}");
    }
    // POST is part of the standard API too.
    let posted = send(
        &r.app,
        Method::POST,
        "/rest/getPlaylist.view",
        &[("content-type", "application/x-www-form-urlencoded")],
        Body::from(format!("{AUTH}&f=json&id={}", r.playlist_id)),
    )
    .await;
    assert_eq!(posted.envelope()["playlist"]["songCount"], 3);
    assert!(r.state.acquisition_tracker.all().is_empty());
    assert_eq!(r.state.imports.overview("alice").trickle.queued, 0);
    assert!(
        r.streams.stream_calls().is_empty(),
        "listing never resolves a stream"
    );
}

#[tokio::test]
async fn import_streaming_snapshot_rows_carry_octo_import_in_json_and_xml() {
    let r = rig().await;
    let listing = get(&r.app, &format!("/rest/getPlaylists.view?{AUTH}&f=json"))
        .await
        .envelope();
    let row = listing["playlists"]["playlist"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == r.playlist_id)
        .cloned()
        .unwrap();
    assert_eq!(row["octoImport"], true);
    let other = listing["playlists"]["playlist"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == "nd-list")
        .cloned()
        .unwrap();
    assert!(other.get("octoImport").is_none());
    let playlist = get(
        &r.app,
        &format!("/rest/getPlaylist.view?{AUTH}&f=json&id={}", r.playlist_id),
    )
    .await
    .envelope();
    assert_eq!(playlist["playlist"]["octoImport"], true);
    let xml = get(&r.app, &format!("/rest/getPlaylists.view?{AUTH}"))
        .await
        .text();
    assert!(xml.contains(r#"octoImport="true""#), "{xml}");
}

#[tokio::test]
async fn import_streaming_unknown_duration_is_numeric_for_psysonic_playback() {
    let r = rig_with_duration(false).await;
    let auth = auth("alice", "good", "Psysonic");
    let endpoint = format!("/rest/getPlaylist.view?{auth}&id={}", r.playlist_id);
    let playlist = get(&r.app, &format!("{endpoint}&f=json")).await.envelope()["playlist"].clone();
    let entries = playlist["entry"].as_array().unwrap();
    assert_eq!(entries[0]["duration"], 330, "keep native metadata");
    assert_eq!(entries[1]["id"], r.preview, "keep the original routing ID");
    assert_eq!(entries[1]["duration"].as_f64(), Some(0.0));
    assert_eq!(entries[2]["duration"], 330);
    assert_eq!(
        r.state
            .external_id_registry
            .lookup(&r.preview)
            .unwrap()
            .snapshot()
            .duration,
        None
    );
    let xml = get(&r.app, &endpoint).await.text();
    assert!(xml.contains(&format!(r#"id="{}""#, r.preview)));
    assert!(xml.contains(r#"duration="0""#));
    let stream = get(&r.app, &format!("/rest/stream.view?{auth}&id={}", r.preview)).await;
    assert_eq!(stream.body.as_ref(), b"preview audio");
    assert!(r.state.acquisition_tracker.all().is_empty());
    assert_eq!(r.state.imports.overview("alice").trickle.queued, 0);
}

#[tokio::test]
async fn import_streaming_checks_auth_owner_and_read_only_ids() {
    let r = rig().await;
    for (auth, code) in [
        (auth("alice", "bad", "test"), 40),
        (auth("bob", "good", "test"), 70),
    ] {
        let reply = get(
            &r.app,
            &format!("/rest/getPlaylist?{auth}&f=json&id={}", r.playlist_id),
        )
        .await
        .envelope();
        assert_eq!(reply["error"]["code"], code);
        assert!(reply.get("playlist").is_none());
    }
    let bob = get(
        &r.app,
        &format!(
            "/rest/getPlaylists?{}&username=alice&f=json",
            auth("bob", "good", "test")
        ),
    )
    .await
    .text();
    assert!(!bob.contains(&r.playlist_id));
    for (endpoint, key) in [
        ("updatePlaylist", "playlistId"),
        ("deletePlaylist", "id"),
        ("createPlaylist", "playlistId"),
    ] {
        let reply = get(
            &r.app,
            &format!("/rest/{endpoint}?{AUTH}&f=json&{key}={}", r.playlist_id),
        )
        .await
        .envelope();
        assert_eq!(reply["error"]["code"], 70, "{endpoint}: {reply}");
    }
    assert!(r.state.imports.set_streaming("alice", &r.import_id, false).ok);
    let reply = get(
        &r.app,
        &format!("/rest/getPlaylist?{AUTH}&f=json&id={}", r.playlist_id),
    )
    .await
    .envelope();
    assert_eq!(reply["error"]["code"], 70);
}

#[tokio::test]
async fn import_streaming_api_key_resolves_the_authenticated_owner() {
    let r = rig().await;
    let alice = "apiKey=alice-key&v=1.16.1&c=Feishin&f=json";
    let listing = get(&r.app, &format!("/rest/getPlaylists?{alice}")).await.text();
    assert!(listing.contains(&r.playlist_id), "{listing}");
    let detail = get(&r.app, &format!("/rest/getPlaylist?{alice}&id={}", r.playlist_id))
        .await
        .envelope();
    assert_eq!(detail["playlist"]["songCount"], 3);
    let bob = get(
        &r.app,
        &format!("/rest/getPlaylist?apiKey=bob-key&f=json&id={}", r.playlist_id),
    )
    .await
    .envelope();
    assert_eq!(bob["error"]["code"], 70);
}

#[tokio::test]
async fn import_streaming_unavailable_local_song_falls_back_to_preview_without_dropping_its_position() {
    let r = rig().await;
    Mock::given(wiremock::matchers::path("/rest/getSong"))
        .and(wiremock::matchers::query_param("id", "nd-teardrop"))
        .respond_with(navidrome_json(
            r#"{"subsonic-response":{"status":"failed","error":{"code":70,"message":"Gone"}}}"#,
        ))
        .with_priority(1)
        .mount(&r.server)
        .await;
    let detail = get(
        &r.app,
        &format!("/rest/getPlaylist?{AUTH}&f=json&id={}", r.playlist_id),
    )
    .await
    .envelope();
    let entries = detail["playlist"]["entry"].as_array().unwrap();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0]["isExternal"], true);
    assert_eq!(entries[0]["id"], entries[2]["id"]);
    assert_eq!(entries[1]["id"], r.preview);
    assert!(r.state.acquisition_tracker.all().is_empty());
}

#[tokio::test]
async fn import_streaming_recovers_an_evicted_preview_without_reopening_the_playlist() {
    let r = rig().await;
    for i in 0..ExternalIdRegistry::MAX_ENTRIES + 1 {
        r.state
            .external_id_registry
            .register(octo_core::soulseek::SoulseekRouting {
                title: Some(format!("Unrelated {i}")),
                ..Default::default()
            });
    }
    let routing = r
        .state
        .external_id_registry
        .lookup(&r.preview)
        .expect("restored from the durable import");
    assert_eq!(routing.snapshot().title.as_deref(), Some("Angel"));
    let stream = get(&r.app, &format!("/rest/stream?{AUTH}&id={}", r.preview)).await;
    assert_eq!(stream.body.as_ref(), b"preview audio");
    assert!(r.state.acquisition_tracker.all().is_empty());
}

#[tokio::test]
async fn import_streaming_preview_plays_without_downloading_and_heart_uses_existing_acquisition() {
    let r = rig().await;
    let reply = get(&r.app, &format!("/rest/stream?{AUTH}&id={}", r.preview)).await;
    assert!(reply.status.is_success(), "{}", reply.text());
    assert_eq!(reply.body.as_ref(), b"preview audio");
    assert_eq!(r.streams.stream_calls().len(), 1);
    assert_eq!(*r.streams.download_and_stream_calls.lock(), 0);
    assert!(r.state.acquisition_tracker.all().is_empty());
    let heart = get(&r.app, &format!("/rest/star?{AUTH}&f=json&id={}", r.preview))
        .await
        .envelope();
    assert_eq!(heart["status"], "ok");
    assert!(
        r.state
            .acquisition_tracker
            .for_user("alice")
            .iter()
            .any(|row| row.external_id == r.preview)
    );
}

#[tokio::test]
async fn import_streaming_replaces_preview_with_local_after_a_heart_download_without_changing_order_or_playlist_id()
 {
    let r = rig().await;
    let before = get(
        &r.app,
        &format!("/rest/getPlaylist?{AUTH}&f=json&id={}", r.playlist_id),
    )
    .await
    .envelope();
    assert_eq!(before["playlist"]["entry"][1]["id"], r.preview);
    // Model the existing downloader's completion after Navidrome has scanned the file.
    r.library.lock().push(row(
        "nd-angel",
        "Massive Attack",
        "Angel",
        Some(379),
        Some("Mezzanine"),
        "flac",
        900,
    ));
    let tracker = &r.state.acquisition_tracker;
    tracker.begin(
        "soulseek",
        &r.preview,
        None,
        Some("alice"),
        Some("Massive Attack"),
        Some("Angel"),
        Some("Mezzanine"),
    );
    tracker.complete("soulseek", &r.preview, Some("nd-angel"));
    let after = get(
        &r.app,
        &format!("/rest/getPlaylist?{AUTH}&f=json&id={}", r.playlist_id),
    )
    .await
    .envelope();
    assert_eq!(after["playlist"]["id"], r.playlist_id);
    let entries = after["playlist"]["entry"].as_array().unwrap();
    assert_eq!(
        entries
            .iter()
            .map(|song| song["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["nd-teardrop", "nd-angel", "nd-teardrop"]
    );
    assert_eq!(entries[1]["suffix"], "flac");
    // A queue opened earlier still uses the preview's advertised audio format.
    let old_queue = get(&r.app, &format!("/rest/stream?{AUTH}&id={}", r.preview)).await;
    assert_eq!(old_queue.body.as_ref(), b"preview audio");
}

async fn get_song_calls(r: &Rig) -> usize {
    r.server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|request| request.url.path().trim_end_matches(".view") == "/rest/getSong")
        .count()
}

#[tokio::test]
async fn import_streaming_a_second_open_reuses_the_library_songs_it_read() {
    let r = rig().await;
    let open = format!("/rest/getPlaylist?{AUTH}&f=json&id={}", r.playlist_id);
    let first = get(&r.app, &open).await.envelope();
    let after_first = get_song_calls(&r).await;
    assert!(after_first > 0);
    let second = get(&r.app, &open).await.envelope();
    assert_eq!(
        get_song_calls(&r).await,
        after_first,
        "the second open asks Navidrome nothing"
    );
    assert_eq!(first["playlist"]["entry"], second["playlist"]["entry"]);
}

#[tokio::test]
async fn import_streaming_a_heart_reads_that_song_again_on_the_next_open() {
    let r = rig().await;
    let open = format!("/rest/getPlaylist?{AUTH}&f=json&id={}", r.playlist_id);
    get(&r.app, &open).await;
    get(&r.app, &format!("/rest/star?{AUTH}&f=json&id=nd-teardrop")).await;
    let before = get_song_calls(&r).await;
    get(&r.app, &open).await;
    // The list holds the hearted song twice; the two entries may each read it.
    let after = get_song_calls(&r).await;
    assert!(
        after > before && after <= before + 2,
        "only the hearted song is read again"
    );
}

#[tokio::test]
async fn import_streaming_another_songs_download_never_rereads_the_library_on_open() {
    let r = rig().await;
    let open = format!("/rest/getPlaylist?{AUTH}&f=json&id={}", r.playlist_id);
    get(&r.app, &open).await;
    let reads = Arc::new(Mutex::new(0usize));
    let counted = reads.clone();
    let pages = r.library.clone();
    r.state
        .imports
        .matcher()
        .set_read_page(Arc::new(move |start, count| {
            *counted.lock() += 1;
            let page: Vec<_> = pages
                .lock()
                .iter()
                .skip(start as usize)
                .take(count as usize)
                .cloned()
                .collect();
            let n = page.len();
            Box::pin(async move { Some((page, n)) })
        }));
    let tracker = &r.state.acquisition_tracker;
    tracker.begin(
        "soulseek",
        "unrelated",
        None,
        Some("bob"),
        Some("Portishead"),
        Some("Roads"),
        Some("Dummy"),
    );
    tracker.complete("soulseek", "unrelated", Some("nd-roads"));
    let entries = get(&r.app, &open).await.envelope()["playlist"]["entry"].clone();
    assert_eq!(
        *reads.lock(),
        0,
        "a song no streaming list misses leaves the library index alone"
    );
    assert_eq!(entries[1]["id"], r.preview);
}
