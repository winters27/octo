//! Replace now: an open Review question answered with Wrong song from the dashboard, as the
//! signed-in Navidrome admin and held to the same gates as the Wrong song playlist.

use axum::http::{Method, StatusCode};
use octo_core::models::domain::Song;
use octo_core::settings::{
    AppSettings, LibraryAction, LibraryActionDefinition, LibraryActionSettings, SubsonicSettings,
};
use serde_json::json;
use wiremock::matchers::{any, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::test_app::{Reply, app, send};
use crate::app::AppState;
use crate::services::library::notice_queue::NoticeQueue;

const FILE: &str = "/music/Massive Attack - Teardrop.flac";

/// Library actions on for the admin, Wrong song on, no dry run.
fn replacing() -> LibraryActionSettings {
    LibraryActionSettings {
        enabled: true,
        dry_run: false,
        allowed_users: vec!["admin".into()],
        actions: vec![LibraryActionDefinition {
            action: LibraryAction::WrongSong,
            name: String::new(),
            enabled: true,
            rating: None,
        }],
        ..Default::default()
    }
}

/// An app with `actions`, an open Review question about Teardrop for the admin, and the admin's
/// sign-in: the state, the sign-in token and the question's key.
fn asked(actions: LibraryActionSettings) -> (AppState, String, String) {
    asked_of("admin", actions, None)
}

/// [`asked`], the question put to `asked`, with Navidrome at `navidrome` when given.
fn asked_of(
    asked: &str,
    actions: LibraryActionSettings,
    navidrome: Option<&MockServer>,
) -> (AppState, String, String) {
    let subsonic = navidrome.map_or_else(SubsonicSettings::default, |server| SubsonicSettings {
        url: Some(server.uri()),
        admin_username: Some("admin".into()),
        admin_password: Some("secret".into()),
        auto_detect_download_path: false,
        ..Default::default()
    });
    let state = AppState::for_tests(AppSettings {
        library_actions: actions,
        subsonic,
        ..Default::default()
    });
    let song = Song {
        artist: "Massive Attack".into(),
        title: "Teardrop".into(),
        album: "Mezzanine".into(),
        ..Default::default()
    };
    assert!(state.notice_queue.add_library_question(
        asked,
        FILE,
        &song,
        "Sounds nothing like the song on YouTube"
    ));
    let token = state.browse_sessions.create("admin");
    (state, token, NoticeQueue::review_key(asked, FILE))
}

/// A Navidrome whose admin is its only user, with song nd-1 `starred` or not.
async fn navidrome(starred: bool) -> MockServer {
    let server = MockServer::start().await;
    let answer = |body: serde_json::Value| ResponseTemplate::new(200).set_body_json(body);
    Mock::given(path("/auth/login"))
        .respond_with(answer(json!({
            "token": "jwt", "isAdmin": true, "username": "admin",
            "subsonicToken": "st", "subsonicSalt": "ss",
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/song/nd-1"))
        .respond_with(answer(json!({
            "id": "nd-1", "title": "Teardrop", "artist": "Massive Attack", "album": "Mezzanine",
            "starred": starred, "rating": 0,
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/user"))
        .respond_with(answer(json!([{"id": "u1", "userName": "admin"}])))
        .mount(&server)
        .await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_raw("[]", "application/json"))
        .mount(&server)
        .await;
    server
}

async fn replace_now(state: &AppState, headers: &[(&str, &str)], key: &str) -> Reply {
    let body = json!({ "key": key }).to_string();
    send(
        &app(state.clone()),
        Method::POST,
        "/api/admin/library-actions/replace-now",
        headers,
        Some(&body),
    )
    .await
}

fn signed_in(token: &str) -> [(&'static str, String); 2] {
    [
        ("X-Octo-Admin", "1".to_string()),
        ("Cookie", format!("octo_browse={token}")),
    ]
}

async fn as_admin(state: &AppState, token: &str, key: &str) -> Reply {
    let headers = signed_in(token);
    let headers: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    replace_now(state, &headers, key).await
}

#[tokio::test]
async fn the_questions_say_their_key_whether_they_are_open_and_the_song_navidrome_shows() {
    let (state, token, key) = asked(replacing());
    let cookie = format!("octo_browse={token}");
    let list = |state: AppState| {
        let cookie = cookie.clone();
        async move {
            send(
                &app(state),
                Method::GET,
                "/api/admin/notices",
                &[("Cookie", &cookie)],
                None,
            )
            .await
            .json()["entries"][0]
                .clone()
        }
    };

    let entry = list(state.clone()).await;
    assert_eq!(entry["key"], key.as_str());
    assert_eq!(entry["open"], true);
    assert_eq!(entry["mine"], true);
    assert!(entry.get("navidromeId").is_none(), "{entry}");

    state.notice_queue.set_navidrome_id(&key, "nd-1");
    assert_eq!(list(state).await["navidromeId"], "nd-1");
}

#[tokio::test]
async fn replace_now_needs_the_dashboard_sign_in() {
    let (state, _, key) = asked(replacing());
    state.notice_queue.set_navidrome_id(&key, "nd-1");

    let refused = replace_now(&state, &[("X-Octo-Admin", "1")], &key).await;

    assert_eq!(refused.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn replace_now_keeps_to_the_wrong_song_gates() {
    for (actions, words) in [
        (
            LibraryActionSettings::default(),
            "Library actions are off. Turn them on to replace songs.",
        ),
        (
            LibraryActionSettings {
                allowed_users: vec!["bob".into()],
                ..replacing()
            },
            "admin is not on the library actions allowlist.",
        ),
        (
            LibraryActionSettings {
                actions: Vec::new(),
                ..replacing()
            },
            "The Wrong song action is off under Library actions. Turn it on to replace songs.",
        ),
        (
            LibraryActionSettings {
                dry_run: true,
                ..replacing()
            },
            "Dry run is on, so nothing is replaced.",
        ),
    ] {
        let (state, token, key) = asked(actions);
        state.notice_queue.set_navidrome_id(&key, "nd-1");

        let refused = as_admin(&state, &token, &key).await;

        assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{words}");
        assert_eq!(refused.json()["error"], words);
    }
}

#[tokio::test]
async fn replace_now_answers_only_an_open_question_navidrome_shows() {
    let (state, token, key) = asked(replacing());

    let unknown = as_admin(&state, &token, "no such question").await;
    assert_eq!(unknown.status, StatusCode::BAD_REQUEST);
    assert_eq!(unknown.json()["error"], "That question is not open in Review.");

    let unscanned = as_admin(&state, &token, &key).await;
    assert_eq!(unscanned.status, StatusCode::CONFLICT);
    assert_eq!(
        unscanned.json()["error"],
        "Navidrome does not show this song yet. Try again after its next scan."
    );
}

#[tokio::test]
async fn replace_now_answers_only_a_question_put_to_the_person_signed_in() {
    let (state, token, key) = asked_of("bob", replacing(), None);
    state.notice_queue.set_navidrome_id(&key, "nd-1");
    let cookie = format!("octo_browse={token}");
    let listed = send(
        &app(state.clone()),
        Method::GET,
        "/api/admin/notices",
        &[("Cookie", &cookie)],
        None,
    )
    .await
    .json();
    assert_eq!(listed["entries"][0]["mine"], false);

    let refused = as_admin(&state, &token, &key).await;

    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    assert_eq!(
        refused.json()["error"],
        "That question is for bob; only they can answer it, from their Review playlist."
    );
}

#[tokio::test]
async fn replace_now_leaves_a_starred_song_or_one_whose_marks_cannot_be_read_a_question() {
    let navidrome = navidrome(true).await;
    let (state, token, key) = asked_of("admin", replacing(), Some(&navidrome));
    state.notice_queue.set_navidrome_id(&key, "nd-1");

    let starred = as_admin(&state, &token, &key).await;

    assert_eq!(starred.status, StatusCode::CONFLICT, "{}", starred.text());
    assert_eq!(
        starred.json()["error"],
        "Someone starred or rated this song, so it stays a question. Take the star or rating off first to replace it."
    );

    // No Navidrome to read the marks from.
    let (state, token, key) = asked(replacing());
    state.notice_queue.set_navidrome_id(&key, "nd-1");
    let unread = as_admin(&state, &token, &key).await;
    assert_eq!(unread.status, StatusCode::CONFLICT);
    assert_eq!(
        unread.json()["error"],
        "The stars and ratings on this song could not be read, so it stays a question."
    );
}

#[tokio::test]
async fn replace_now_takes_an_unmarked_song_of_one_s_own_question() {
    let navidrome = navidrome(false).await;
    let (state, token, key) = asked_of("admin", replacing(), Some(&navidrome));
    state.notice_queue.set_navidrome_id(&key, "nd-1");

    let accepted = as_admin(&state, &token, &key).await;

    assert_eq!(accepted.status, StatusCode::ACCEPTED, "{}", accepted.text());
    assert_eq!(accepted.json()["ok"], true);
}
