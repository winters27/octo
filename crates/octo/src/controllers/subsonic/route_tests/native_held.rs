//! Held playlists through Navidrome's own API, by way of the catch-all: list counts, tracks per
//! caller (no other member's songs, the kid rule), adds in the Subsonic routes' order, removal and moves by
//! place, the owner check, and a family action playlist reached natively.

use axum::body::Body;
use axum::http::{Method, StatusCode};
use serde_json::{Value, json};

use super::held_playlists::{
    ALEX_MIX, Fixture, KID_LIST, OUTSIDE, OUTSIDE_ALBUM, OUTSIDE_EXPLICIT, ROAD, SMART, fixture,
    fixture_with, native_token,
};
use super::{Reply, send};

async fn native(f: &Fixture, user: &str, method: Method, path: &str, body: Option<Value>) -> Reply {
    let token = native_token(user);
    let mut headers = vec![("x-nd-authorization", token.as_str())];
    if body.is_some() {
        headers.push(("content-type", "application/json"));
    }
    let body = body.map_or_else(Body::empty, |body| Body::from(body.to_string()));
    send(&f.app, method, path, &headers, body).await
}

fn ids(rows: &Value, key: &str) -> Vec<String> {
    rows.as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row[key].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn t24_native_add_serve_count_remove_and_move_by_place() {
    let f = fixture().await;
    let path = format!("/api/playlist/{ROAD}/tracks");
    let added = native(
        &f,
        "winters",
        Method::POST,
        &path,
        Some(json!({"ids": [OUTSIDE]})),
    )
    .await;
    assert_eq!(added.status, StatusCode::OK, "{}", added.body);
    assert_eq!(added.json()["added"], 1);
    assert_eq!(f.held(ROAD).unwrap(), ["s-a", "s-b", OUTSIDE]);
    assert_eq!(f.tracks(ROAD), ["s-a", "s-b"]);

    let tracks = native(&f, "winters", Method::GET, &path, None).await;
    assert_eq!(tracks.header("x-total-count"), Some("3"));
    let rows = tracks.json();
    assert_eq!(ids(&rows, "mediaFileId"), ["s-a", "s-b", OUTSIDE]);
    assert_eq!(ids(&rows, "id"), ["1", "2", "3"]);

    let listed = native(&f, "winters", Method::GET, "/api/playlist?_start=0&_end=50", None).await;
    let road = listed
        .json()
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == ROAD)
        .cloned()
        .unwrap();
    // Two library tracks and one streamed song.
    assert_eq!(road["songCount"], 3);
    let one = native(&f, "winters", Method::GET, &format!("/api/playlist/{ROAD}"), None).await;
    assert_eq!(one.json()["songCount"], 3);

    // Move place 3 (the outside song) before place 1, then remove place 2 (s-a).
    let moved = native(
        &f,
        "winters",
        Method::PUT,
        &format!("{path}/3"),
        Some(json!({"insert_before": "1"})),
    )
    .await;
    assert_eq!(moved.status, StatusCode::OK, "{}", moved.body);
    assert_eq!(f.held(ROAD).unwrap(), [OUTSIDE, "s-a", "s-b"]);
    let removed = native(&f, "winters", Method::DELETE, &format!("{path}?id=2"), None).await;
    assert_eq!(removed.status, StatusCode::OK, "{}", removed.body);
    assert_eq!(f.held(ROAD).unwrap(), [OUTSIDE, "s-b"]);
    assert_eq!(f.tracks(ROAD), ["s-b"]);
}

#[tokio::test]
async fn t24_t44_native_refusals_owner_album_smart_and_upkeep() {
    let f = fixture().await;
    let album = native(
        &f,
        "winters",
        Method::POST,
        &format!("/api/playlist/{ROAD}/tracks"),
        Some(json!({"albumIds": [OUTSIDE_ALBUM]})),
    )
    .await;
    assert_eq!(album.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        album.json()["error"],
        "Only songs found online can be added one by one."
    );

    let smart = native(
        &f,
        "winters",
        Method::POST,
        &format!("/api/playlist/{SMART}/tracks"),
        Some(json!({"ids": [OUTSIDE]})),
    )
    .await;
    assert_eq!(smart.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        smart.json()["error"],
        "This playlist is made by Navidrome's rules, so songs can't be added to it."
    );

    let not_owner = native(
        &f,
        "winters",
        Method::POST,
        &format!("/api/playlist/{ALEX_MIX}/tracks"),
        Some(json!({"ids": [OUTSIDE]})),
    )
    .await;
    assert_eq!(not_owner.status, StatusCode::FORBIDDEN);
    assert_eq!(
        not_owner.json()["error"],
        "Only the playlist's owner can change it."
    );
    assert!(f.none_held());
}

#[tokio::test]
async fn t44_native_upkeep_playlists_refuse_outside_songs() {
    let f = fixture_with(false, |settings| {
        // Review on brings the Keep action playlist with it.
        settings.library_actions = octo_core::settings::LibraryActionSettings {
            enabled: true,
            playlists_enabled: true,
            review_enabled: true,
            allowed_users: vec!["winters".into()],
            ..Default::default()
        };
    })
    .await;
    let title = crate::services::library::LibraryActionPlaylistProvisioner::new(
        f.state.subsonic_proxy.clone(),
        f.state.settings.clone(),
    )
    .upkeep_titles("winters")
    .actions
    .first()
    .cloned()
    .expect("an action title");
    f.nd.0
        .lock()
        .add("eupkeepaaaaaaaaaaaaaaa", &title, "winters", false, false, &[]);
    let refused = native(
        &f,
        "winters",
        Method::POST,
        "/api/playlist/eupkeepaaaaaaaaaaaaaaa/tracks",
        Some(json!({"ids": [OUTSIDE]})),
    )
    .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        refused.json()["error"],
        "Songs found online can't go in Octo's library upkeep playlists."
    );
    assert!(f.none_held());
}

#[tokio::test]
async fn t24_native_tracks_follow_the_caller_and_the_kid_rule() {
    let f = fixture_with(true, |_| {}).await;
    let path = format!("/api/playlist/{KID_LIST}/tracks");
    let added = native(
        &f,
        "kid",
        Method::POST,
        &path,
        Some(json!({"ids": [OUTSIDE, OUTSIDE_EXPLICIT]})),
    )
    .await;
    assert_eq!(added.status, StatusCode::OK, "{}", added.body);
    let kid = native(&f, "kid", Method::GET, &path, None).await.json();
    assert_eq!(ids(&kid, "mediaFileId"), ["s-a", OUTSIDE]);
    let owner = native(&f, "winters", Method::GET, &path, None).await.json();
    assert_eq!(
        ids(&owner, "mediaFileId"),
        ["s-a", "s-explicit", OUTSIDE, OUTSIDE_EXPLICIT]
    );
}

#[tokio::test]
async fn t48_a_native_add_to_a_members_request_a_copy_playlist_makes_a_request_and_empties_it() {
    let f = fixture_with(true, |_| {}).await;
    let name = crate::services::library::library_action_playlist_provisioner::REQUEST_PLAYLIST;
    f.nd.0
        .lock()
        .add("frequestaaaaaaaaaaaaaa", name, "alex", false, false, &["s-a"]);
    let asked = native(
        &f,
        "alex",
        Method::POST,
        "/api/playlist/frequestaaaaaaaaaaaaaa/tracks",
        Some(json!({"ids": [OUTSIDE]})),
    )
    .await;
    assert_eq!(asked.status, StatusCode::OK, "{}", asked.body);
    assert!(f.tracks("frequestaaaaaaaaaaaaaa").is_empty());
    assert!(f.held("frequestaaaaaaaaaaaaaa").is_none());
}
