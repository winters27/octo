//! Keep anyway over the Subsonic API (octoAcquisitions 4): getAcquisitions and getAcquisition carry
//! `keepAnyway` only for a caller waiting in a hold that offers it, and no more once they asked,
//! and an album row its `heldTracks`; keepAcquisitionAnyway saves the keep before it answers a
//! plain ok, refuses anyone not waiting in the row's holds with 50, asks the admin to name the
//! person with 10, and answers 70 once nothing waits.

use std::path::PathBuf;

use chrono::Utc;
use octo_core::fingerprint::heart_gate::{
    AcquisitionOrigin, HeardRecord, HeardRecording, HeardSource, WaitingFor,
};
use serde_json::{Value, json};
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request as MockRequest, Respond, ResponseTemplate};

use super::downloads::{NAME_THE_PERSON, NOTHING_WAITING};
use super::test_app::*;
use crate::app::AppState;
use crate::http::pipeline::App;
use crate::services::common::album_gates::tracks_wait_line;
use crate::services::common::soulseek_hold_store::HeldAlbumTrack;
use crate::services::common::test_fakes::until;
use crate::services::common::{HeldAcquisition, HeldKind, Holder};

/// Navidrome as far as these calls need it: a ping that accepts the token "good", and getUser
/// saying only "admin" is an admin.
struct KeepNavidrome;

impl Respond for KeepNavidrome {
    fn respond(&self, request: &MockRequest) -> ResponseTemplate {
        let path = request.url.path();
        let query = query_of(request);
        if path.ends_with("/rest/ping") {
            return if query.get("t").map(String::as_str) == Some("good") {
                navidrome_json(r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#)
            } else {
                navidrome_json(
                    r#"{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":40,"message":"Wrong username or password"}}}"#,
                )
            };
        }
        if path.ends_with("/rest/getUser") {
            let username = query.get("username").cloned().unwrap_or_default();
            return navidrome_json(
                &json!({"subsonic-response": {"status": "ok", "version": "1.16.1", "user": {
                    "username": username, "adminRole": username == "admin",
                }}})
                .to_string(),
            );
        }
        ResponseTemplate::new(404)
    }
}

struct KeepApp {
    state: AppState,
    app: App,
    _navidrome: MockServer,
    files: tempfile::TempDir,
}

impl KeepApp {
    async fn new() -> KeepApp {
        let navidrome = MockServer::builder().start().await;
        Mock::given(any())
            .respond_with(KeepNavidrome)
            .mount(&navidrome)
            .await;
        let state = state_with(settings(&navidrome.uri()), |_| {});
        KeepApp {
            app: app(&state),
            state,
            _navidrome: navidrome,
            files: tempfile::tempdir().expect("a temp dir"),
        }
    }

    /// The song "help" held while AcoustID is off for alice, who heard a video of it, its row held.
    fn hold_song(&self) -> HeldAcquisition {
        let record = self.state.soulseek_holds.hold(HeldAcquisition {
            kind: HeldKind::Track,
            provider: "soulseek".into(),
            external_id: "help".into(),
            requested_by: Some("alice".into()),
            held_since_utc: Utc::now(),
            origin: Some(AcquisitionOrigin::Heart),
            waiting_for: Some(WaitingFor::AcoustIdOff),
            holders: vec![Holder {
                user: "alice".into(),
                heard: Some(HeardRecord {
                    heard_at: Utc::now(),
                    source: HeardSource::Video {
                        id: "ALICEVIDEO1".into(),
                        size: Some(1000),
                        content_type: "audio/mp4".into(),
                        itag: Some(140),
                    },
                    sample_id: None,
                }),
                ..Default::default()
            }],
            line: Some("AcoustID is off, so Octo cannot check what played".into()),
            ..Default::default()
        });
        self.state.heart_acquisition_coordinator.hold_row_for(&record);
        record
    }

    /// The held track `id` of alice's hearted album "help-album", its row and its album's held.
    fn hold_track(&self, id: &str) -> HeldAcquisition {
        let file: PathBuf = self.files.path().join(format!("{id}.flac"));
        std::fs::write(&file, b"a held track").expect("written");
        let record = self.state.soulseek_holds.hold(HeldAcquisition {
            kind: HeldKind::Track,
            provider: "soulseek".into(),
            external_id: id.into(),
            requested_by: Some("alice".into()),
            held_since_utc: Utc::now(),
            origin: Some(AcquisitionOrigin::AlbumHeart),
            waiting_for: Some(WaitingFor::NotYetProven),
            holders: vec![Holder {
                user: "alice".into(),
                ..Default::default()
            }],
            held_files: vec![file.to_string_lossy().to_string()],
            album_track: Some(HeldAlbumTrack {
                album_key: "soulseek:help-album".into(),
                song_ids: vec![id.into()],
                artist: "The Beatles".into(),
                title: id.into(),
                duration_seconds: Some(139),
                r: HeardRecording::default(),
            }),
            line: Some(tracks_wait_line(1)),
            ..Default::default()
        });
        self.state.heart_acquisition_coordinator.hold_row_for(&record);
        record
    }

    /// `user`'s getAcquisitions rows, by key.
    async fn rows(&self, user: &str) -> serde_json::Map<String, Value> {
        let envelope = get(
            &self.app,
            &format!(
                "/rest/getAcquisitions.view?{}",
                auth(user, "good", "octo-android")
            ),
        )
        .await
        .envelope();
        envelope["acquisitions"]["acquisition"]
            .as_array()
            .expect("a list of acquisitions")
            .iter()
            .map(|row| (row["key"].as_str().expect("a key").to_string(), row.clone()))
            .collect()
    }

    /// keepAcquisitionAnyway as `user`, with `query`.
    async fn keep(&self, user: &str, query: &str) -> Value {
        get(
            &self.app,
            &format!(
                "/rest/keepAcquisitionAnyway.view?{}&{query}",
                auth(user, "good", "octo-android")
            ),
        )
        .await
        .envelope()
    }
}

fn error_of(envelope: &Value) -> (i64, String) {
    assert_eq!(envelope["status"], "failed", "{envelope}");
    (
        envelope["error"]["code"].as_i64().expect("a code"),
        envelope["error"]["message"]
            .as_str()
            .expect("a message")
            .to_string(),
    )
}

/// The rows carry `keepAnyway` only for someone waiting in a hold offering it (an admin who asked
/// for the song too, waiting in none of its holds, gets none) and `heldTracks` only on the album's
/// row; getAcquisition carries the same; once the keep is asked for, the row offers it no more.
#[tokio::test]
async fn the_rows_carry_keep_anyway_only_for_a_holder_and_held_tracks_only_on_an_album_row() {
    let t = KeepApp::new().await;
    t.state.acquisition_tracker.begin(
        "soulseek",
        "help",
        None,
        Some("admin"),
        Some("The Beatles"),
        Some("Help!"),
        None,
    );
    let song = t.hold_song();
    t.hold_track("night");
    t.hold_track("day");

    let rows = t.rows("alice").await;
    assert_eq!(rows["soulseek:help"]["keepAnyway"], true);
    assert!(rows["soulseek:help"].get("heldTracks").is_none(), "a song row");
    assert_eq!(rows["soulseek:help-album"]["keepAnyway"], true);
    assert_eq!(rows["soulseek:help-album"]["heldTracks"], 2);
    assert_eq!(rows["soulseek:night"]["keepAnyway"], true);
    assert!(
        rows["soulseek:night"].get("heldTracks").is_none(),
        "a track's own row"
    );

    let admin = t.rows("admin").await;
    assert!(
        admin["soulseek:help"].get("keepAnyway").is_none(),
        "the admin waits in no hold of it"
    );

    let detail = get(
        &t.app,
        &format!(
            "/rest/getAcquisition.view?{}&key=soulseek:help-album",
            auth("alice", "good", "octo-android")
        ),
    )
    .await
    .envelope();
    assert_eq!(detail["acquisition"]["keepAnyway"], true);
    assert_eq!(detail["acquisition"]["heldTracks"], 2);

    t.state
        .soulseek_holds
        .update(&song.key(), |record| record.keep_requested.push("alice".into()))
        .expect("held");
    let rows = t.rows("alice").await;
    assert!(rows["soulseek:help"].get("keepAnyway").is_none(), "asked already");
    assert_eq!(rows["soulseek:help-album"]["keepAnyway"], true);
}

/// keepAcquisitionAnyway: a missing key is error 10; someone waiting in none of the row's holds
/// gets 50, and so does a member naming another; the admin who names nobody gets 10. The holder's
/// own call saves the keep before it answers a plain ok and starts carrying it out; asking again
/// answers 70. The admin keeps a holder's by naming them, and naming someone with nothing
/// waiting answers 70.
#[tokio::test]
async fn keep_acquisition_anyway_saves_the_keep_before_ok_and_answers_10_50_and_70() {
    let t = KeepApp::new().await;
    let song = t.hold_song();
    let night = t.hold_track("night");

    assert_eq!(
        error_of(&t.keep("alice", "f=json").await),
        (10, "Required parameter is missing: key".to_string())
    );
    assert_eq!(error_of(&t.keep("bob", "key=soulseek:help").await).0, 50);
    assert_eq!(
        error_of(&t.keep("bob", "key=soulseek:help&user=alice").await).0,
        50,
        "only the admin keeps another person's"
    );
    assert_eq!(
        error_of(&t.keep("admin", "key=soulseek:help").await),
        (10, NAME_THE_PERSON.to_string())
    );

    let ok = t.keep("alice", "key=soulseek:help").await;
    assert_eq!(ok["status"], "ok", "{ok}");
    assert!(ok.get("error").is_none());
    assert_eq!(
        t.state
            .soulseek_holds
            .get(&song.key())
            .expect("still held while it is carried out")
            .keep_requested,
        ["alice"],
        "saved before the answer"
    );
    until(|| t.state.track_acquisition_queue.is_in_flight("soulseek", "help")).await;
    assert_eq!(
        error_of(&t.keep("alice", "key=soulseek:help").await),
        (70, NOTHING_WAITING.to_string())
    );

    assert_eq!(
        error_of(&t.keep("admin", "key=soulseek:help-album&user=bob").await),
        (70, NOTHING_WAITING.to_string())
    );
    let ok = t.keep("admin", "key=soulseek:help-album&user=alice").await;
    assert_eq!(ok["status"], "ok", "{ok}");
    assert_eq!(
        t.state
            .soulseek_holds
            .get(&night.key())
            .expect("held")
            .keep_requested,
        ["alice"]
    );
}
