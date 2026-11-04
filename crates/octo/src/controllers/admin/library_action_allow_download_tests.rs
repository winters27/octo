//! A song removed with Delete from disk is not downloaded again. The download's log used to say
//! "clear that entry from the dashboard", which had no control for it; the Library actions
//! history now has Allow downloading again.

use axum::http::{Method, StatusCode};
use chrono::Utc;
use octo_core::settings::{AppSettings, LibraryAction};

use super::test_app::{app, send};
use crate::app::AppState;
use crate::services::library::{LibraryActionEntry, LibraryActionJournal, LibraryActionState, PathSource};

fn removed(id: &str, artist: &str, title: &str, copy: bool) -> LibraryActionEntry {
    LibraryActionEntry {
        key: LibraryActionJournal::make_key(LibraryAction::Delete, id, "1:2"),
        action: LibraryAction::Delete,
        navidrome_id: id.into(),
        username: "alice".into(),
        title: title.into(),
        artist: artist.into(),
        album: "Discovery".into(),
        source_path: Some(format!("/music/{id}.flac")),
        quarantine_path: Some(format!("/music/.octo-trash/{id}.flac")),
        resolution: Some(PathSource::NativeApi),
        state: LibraryActionState::Applied,
        detail: Some("Removed.".into()),
        dry_run: false,
        at_utc: Utc::now(),
        history_kept: None,
        revealed_path: None,
        copy,
        download_allowed: false,
        denied_source: None,
    }
}

#[test]
fn allowing_a_song_lifts_every_removal_of_it_and_only_it() {
    let journal = LibraryActionJournal::new();
    let first = removed("nd-1", "Daft Punk", "Digital Love", false);
    journal.record(first.clone());
    journal.record(removed("nd-2", "Daft Punk", "Digital Love", false));
    journal.record(removed("nd-3", "Daft Punk", "Aerodynamic", false));
    assert!(journal.is_never_requested(Some("Daft Punk"), Some("Digital Love")));

    assert_eq!(journal.allow_downloads(&first.key), 2);

    assert!(!journal.is_never_requested(Some("Daft Punk"), Some("Digital Love")));
    assert!(journal.is_never_requested(Some("Daft Punk"), Some("Aerodynamic")));
    assert_eq!(journal.allow_downloads(&first.key), 0);
}

#[test]
fn a_heart_lifts_every_removal_of_its_song_and_only_it() {
    let journal = LibraryActionJournal::new();
    journal.record(removed("nd-1", "Daft Punk", "Digital Love", false));
    journal.record(removed("nd-2", "Daft Punk", "Digital Love", false));
    journal.record(removed("nd-3", "Daft Punk", "Aerodynamic", false));

    assert_eq!(journal.allow_song("Daft Punk", " "), 0);
    assert_eq!(journal.allow_song("Daft Punk", "Digital Love"), 2);

    assert!(!journal.is_never_requested(Some("Daft Punk"), Some("Digital Love")));
    assert!(journal.is_never_requested(Some("Daft Punk"), Some("Aerodynamic")));
    assert_eq!(journal.allow_song("Daft Punk", "Digital Love"), 0);
}

#[tokio::test]
async fn the_history_offers_allow_downloading_again_and_it_works_only_when_signed_in() {
    let state = AppState::for_tests(AppSettings::default());
    let journal = &state.library_action_journal;
    let removed_song = removed("nd-1", "Daft Punk", "Digital Love", false);
    journal.record(removed_song.clone());
    journal.record(removed("nd-2", "Daft Punk", "Aerodynamic", true));
    let token = state.browse_sessions.create("admin");
    let app = app(state.clone());
    let anonymous = [("X-Octo-Admin", "1")];
    let signed_in = [("X-Octo-Admin", "1"), ("X-Octo-Browse-Token", token.as_str())];

    let history = send(&app, Method::GET, "/api/admin/library-actions", &signed_in, None).await;
    let rows = history.json()["entries"].as_array().expect("entries").clone();
    let blocking = rows
        .iter()
        .find(|row| row["navidromeId"] == "nd-1")
        .expect("the removal");
    assert_eq!(blocking["blocksDownloads"], true);
    assert_eq!(blocking["key"], removed_song.key.as_str());
    // A second copy removed never kept the song from downloading.
    assert_eq!(
        rows.iter()
            .find(|row| row["navidromeId"] == "nd-2")
            .expect("the copy")["blocksDownloads"],
        false
    );

    let body = serde_json::json!({ "key": removed_song.key }).to_string();
    let refused = send(
        &app,
        Method::POST,
        "/api/admin/library-actions/allow-download",
        &anonymous,
        Some(&body),
    )
    .await;
    assert_eq!(refused.status, StatusCode::UNAUTHORIZED);
    assert!(journal.is_never_requested(Some("Daft Punk"), Some("Digital Love")));

    let allowed = send(
        &app,
        Method::POST,
        "/api/admin/library-actions/allow-download",
        &signed_in,
        Some(&body),
    )
    .await;

    assert_eq!(allowed.status, StatusCode::OK, "{}", allowed.text());
    assert_eq!(allowed.text(), r#"{"ok":true}"#);
    assert!(!journal.is_never_requested(Some("Daft Punk"), Some("Digital Love")));
    let again = send(
        &app,
        Method::POST,
        "/api/admin/library-actions/allow-download",
        &signed_in,
        Some(&body),
    )
    .await;
    assert_eq!(again.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        again.text(),
        r#"{"error":"That song is not kept from downloading."}"#
    );
}
