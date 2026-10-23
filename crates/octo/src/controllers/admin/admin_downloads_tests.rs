//! The download log and Find songs routes (the Recently removed ones, and the activity card
//! asserts, are the library edits' and the activity view's): every route needs a Navidrome
//! admin's own sign-in, the log route reads the saved log or the live one, and Find songs runs as
//! the signed-in admin.

use std::sync::Arc;
use std::time::Duration;

use axum::http::{Method, StatusCode};
use chrono::Utc;
use futures::FutureExt;
use octo_core::models::download::{
    AcquisitionCandidate, AcquisitionEvent, AcquisitionHeard, DownloadHistoryEntry,
};
use octo_core::settings::{
    AppSettings, LibraryAction, LibraryActionDefinition, LibraryActionSettings, SoulseekSettings,
    SubsonicSettings,
};
use parking_lot::Mutex;
use serde_json::{Value, json};

use super::test_app::{Reply, app, send};
use crate::app::{AppState, TestServices};
use crate::http::pipeline::App;
use crate::services::common::acquisition_tracker::{AcquisitionState, acquisition_event_kinds};
use crate::services::common::song_finder::{FindTarget, FoundCopy, PickOutcome, library_action_states};
use crate::services::common::{PickedCopy, SongFinder};
use crate::services::test_support::refused_address;

struct Dashboard {
    state: AppState,
    app: App,
    _directory: tempfile::TempDir,
}

impl Dashboard {
    fn new() -> Dashboard {
        let directory = tempfile::tempdir().expect("a temp dir");
        let closed = format!("http://{}", refused_address());
        let settings = AppSettings {
            subsonic: SubsonicSettings {
                url: Some(closed.clone()),
                auto_detect_download_path: false,
                ..Default::default()
            },
            soulseek: SoulseekSettings {
                base_url: Some(closed.clone()),
                ..Default::default()
            },
            library_actions: LibraryActionSettings {
                enabled: true,
                dry_run: false,
                allowed_users: vec!["admin".into()],
                quarantine_retention_days: 30,
                actions: vec![LibraryActionDefinition {
                    action: LibraryAction::Delete,
                    enabled: true,
                    ..Default::default()
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        let state = AppState::for_tests_with(
            settings,
            TestServices {
                sign_in: Some("required".into()),
                config_dir: Some(directory.path().to_path_buf()),
                raw_settings: vec![("Soulseek:BaseUrl".into(), closed)],
                ..Default::default()
            },
        );
        Dashboard {
            app: app(state.clone()),
            state,
            _directory: directory,
        }
    }

    fn token(&self, who: Option<&str>) -> Option<String> {
        match who {
            None => None,
            Some("recovery") => Some(self.state.browse_sessions.create_recovery()),
            Some(user) => Some(self.state.browse_sessions.create(user)),
        }
    }

    async fn get(&self, token: Option<&str>, path: &str) -> Reply {
        let headers: Vec<(&str, &str)> = token.map(|t| ("X-Octo-Browse-Token", t)).into_iter().collect();
        send(&self.app, Method::GET, path, &headers, None).await
    }

    async fn post(&self, token: Option<&str>, path: &str, body: &Value) -> Reply {
        let mut headers = vec![("X-Octo-Admin", "1")];
        if let Some(token) = token {
            headers.push(("X-Octo-Browse-Token", token));
        }
        send(&self.app, Method::POST, path, &headers, Some(&body.to_string())).await
    }
}

fn entry(title: &str, key: Option<&str>, at: chrono::DateTime<Utc>) -> DownloadHistoryEntry {
    DownloadHistoryEntry {
        artist: "Daft Punk".into(),
        title: title.into(),
        album: Some("Discovery".into()),
        path: format!("/music/{title}.flac"),
        format: "FLAC".into(),
        source: "Soulseek".into(),
        downloaded_at: format!(
            "{}.{:07}Z",
            at.format("%Y-%m-%dT%H:%M:%S"),
            at.timestamp_subsec_nanos() / 100
        ),
        key: key.map(str::to_string),
        ..Default::default()
    }
}

fn line(at: chrono::DateTime<Utc>, kind: &str, text: &str, copies: usize) -> AcquisitionEvent {
    AcquisitionEvent {
        at,
        kind: kind.into(),
        text: text.into(),
        detail: None,
        candidates: (copies > 0).then(|| {
            (1..=copies)
                .map(|i| AcquisitionCandidate {
                    source: "Soulseek".into(),
                    peer: Some(format!("peer{i}")),
                    file: Some(format!("/share/{i}.flac")),
                    format: Some("flac".into()),
                    bit_depth: Some(16),
                    sample_rate: Some(44100),
                    size: Some(30_000_000),
                    rank: Some(i as i32),
                    ..Default::default()
                })
                .collect()
        }),
        heard: None,
    }
}

const READS: [&str; 4] = [
    "/api/admin/downloads/log?key=soulseek:a",
    "/api/admin/find/abc",
    "/api/admin/find/outside?q=daft",
    "/api/admin/find/cover?id=nd-1",
];

#[tokio::test]
async fn every_route_refuses_no_sign_in_and_the_recovery_code() {
    let dash = Dashboard::new();
    let writes: [(&str, Value); 2] = [
        ("/api/admin/find", json!({ "id": "nd-1" })),
        ("/api/admin/find/abc/pick", json!({ "copy": "c1" })),
    ];
    for who in [None, Some("recovery")] {
        let token = dash.token(who);
        for path in READS {
            assert_eq!(
                dash.get(token.as_deref(), path).await.status,
                StatusCode::UNAUTHORIZED,
                "GET {path} as {who:?}"
            );
        }
        for (path, body) in &writes {
            assert_eq!(
                dash.post(token.as_deref(), path, body).await.status,
                StatusCode::UNAUTHORIZED,
                "POST {path} as {who:?}"
            );
        }
    }
}

#[tokio::test]
async fn the_list_leaves_logs_out_and_the_log_route_reads_the_saved_one_or_the_live_one() {
    let dash = Dashboard::new();
    let now = Utc::now();
    let history = &dash.state.download_history;
    history.record(entry("Old song", None, now - chrono::TimeDelta::days(3)));
    history.record(entry(
        "Digital Love",
        Some("soulseek:saved"),
        now - chrono::TimeDelta::minutes(1),
    ));
    assert!(history.attach_log(
        "soulseek:saved",
        now - chrono::TimeDelta::minutes(2),
        &[line(now, acquisition_event_kinds::FOUND, "Found 3 copies", 3)],
    ));
    let tracker = &dash.state.acquisition_tracker;
    tracker.begin(
        "soulseek",
        "live",
        Some("live"),
        Some("alice"),
        Some("Daft Punk"),
        Some("One More Time"),
        None,
    );
    tracker.stage(
        "soulseek",
        "live",
        AcquisitionState::Searching,
        Some("Soulseek"),
        None,
    );
    let token = dash.token(Some("admin"));
    let token = token.as_deref();

    let list = dash.get(token, "/api/admin/downloads").await.json();
    let rows = list["downloads"].as_array().expect("downloads").clone();
    assert!(rows[0].get("log").is_none());
    assert_eq!(rows[0]["hasLog"], true);
    assert_eq!(rows[0]["key"], "soulseek:saved");
    assert_eq!(rows[1]["hasLog"], false);

    let saved_at = history.get_recent(1)[0].downloaded_at.clone();
    let saved = dash
        .get(
            token,
            &format!(
                "/api/admin/downloads/log?key=soulseek:saved&at={}",
                octo_core::common::dotnet::escape_data_string(&saved_at)
            ),
        )
        .await
        .json();
    assert_eq!(saved["kept"], "saved");
    let lines = saved["event"].as_array().expect("lines");
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["candidate"].as_array().map(Vec::len), Some(3));
    assert_eq!(lines[0]["candidate"][0]["peer"], "peer1");

    let live = dash
        .get(token, "/api/admin/downloads/log?key=soulseek:live")
        .await
        .json();
    assert_eq!(live["kept"], "live");
    assert_eq!(live["row"]["state"], "searching");
    assert!(
        live["event"]
            .as_array()
            .expect("lines")
            .iter()
            .any(|e| e["text"] == "Looking on Soulseek")
    );

    let none = dash
        .get(
            token,
            "/api/admin/downloads/log?key=soulseek:gone&at=2020-01-01T00:00:00Z",
        )
        .await
        .json();
    assert_eq!(none["kept"], "none");
}

#[tokio::test]
async fn a_search_of_the_list_reaches_past_the_newest_200_and_keeps_the_tag_report() {
    let dash = Dashboard::new();
    let now = Utc::now();
    let history = &dash.state.download_history;
    history.record(DownloadHistoryEntry {
        tagging: Some(octo_core::tagging::TagReport {
            confidence: "Strong".into(),
            distance: Some(0.042),
            release_title: Some("Discovery".into()),
            ..Default::default()
        }),
        ..entry(
            "Harder Better Faster Stronger",
            Some("soulseek:old"),
            now - chrono::TimeDelta::days(9),
        )
    });
    for i in 0..210 {
        history.record(entry(&format!("Filler {i}"), None, now));
    }
    let token = dash.token(Some("admin"));
    let token = token.as_deref();

    let all = dash.get(token, "/api/admin/downloads").await.json();
    assert_eq!(all["downloads"].as_array().map(Vec::len), Some(200));

    let found = dash
        .get(token, "/api/admin/downloads?q=harder%20STRONGER")
        .await
        .json();
    let rows = found["downloads"].as_array().expect("downloads");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["title"], "Harder Better Faster Stronger");
    assert_eq!(rows[0]["tagging"]["confidence"], "Strong");
    assert_eq!(rows[0]["tagging"]["distance"], 0.042);
    assert!(rows[0].get("log").is_none());

    let none = dash
        .get(token, "/api/admin/downloads?q=nothing%20like%20it")
        .await
        .json();
    assert_eq!(none["downloads"].as_array().map(Vec::len), Some(0));
    let blank = dash.get(token, "/api/admin/downloads?q=%20").await.json();
    assert_eq!(
        blank["downloads"].as_array().map(Vec::len),
        Some(200),
        "a blank search is the list"
    );
}

#[tokio::test]
async fn find_songs_runs_as_the_signed_in_admin_and_only_they_can_follow_or_pick_from_it() {
    let dash = Dashboard::new();
    let finder: Arc<SongFinder> = dash.state.song_finder.clone();
    let picked_by: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
    let heard = picked_by.clone();
    finder.seams(|seams| {
        seams.resolve = Arc::new(|id, _| {
            let target = match id.as_str() {
                "nd-1" => Some(FindTarget {
                    library_id: Some("nd-1".into()),
                    owned_format: Some("mp3".into()),
                    ..FindTarget::new("Daft Punk", "Digital Love", Some("Discovery"), Some(301), "nd-1")
                }),
                "soulseek:ext-1" => Some(FindTarget {
                    external_id: Some("ext-1".into()),
                    ..FindTarget::new("Daft Punk", "Digital Love", Some("Discovery"), Some(301), "ext-1")
                }),
                _ => None,
            };
            async move { target }.boxed()
        });
        seams.sources_for = Arc::new(|_| vec![("Soulseek".to_string(), None)]);
        seams.search_soulseek = Arc::new(|_, _| {
            async {
                Ok((
                    vec![FoundCopy::new(
                        AcquisitionCandidate {
                            source: "Soulseek".into(),
                            peer: Some("peer1".into()),
                            file: Some("/share/Digital Love.flac".into()),
                            format: Some("flac".into()),
                            bit_depth: Some(16),
                            sample_rate: Some(44100),
                            rank: Some(1),
                            ..Default::default()
                        },
                        PickedCopy {
                            peer: Some("peer1".into()),
                            file: Some("/share/Digital Love.flac".into()),
                            format: Some("flac".into()),
                            ..Default::default()
                        },
                    )],
                    vec!["Daft Punk Digital Love".to_string()],
                    Some("1 file from 1 peer; 1 fits the song".to_string()),
                ))
            }
            .boxed()
        });
        seams.fetch = Arc::new(move |_, copy, user, _| {
            heard
                .lock()
                .push((user, copy.pick.peer.clone().unwrap_or_default()));
            async {
                Ok(PickOutcome::new(
                    library_action_states::QUEUED,
                    "Getting the copy you picked.",
                ))
            }
            .boxed()
        });
    });
    let admin = dash.token(Some("admin"));
    let admin = admin.as_deref();
    let other = dash.token(Some("other-admin"));
    let other = other.as_deref();

    assert_eq!(
        dash.post(admin, "/api/admin/find", &json!({ "id": "nope" }))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    // A library song's copy would replace its file, which needs Better quality on for this admin.
    let library = dash
        .post(admin, "/api/admin/find", &json!({ "id": "nd-1" }))
        .await
        .json();
    assert_eq!(library["state"], "failed");
    assert!(
        library["error"]
            .as_str()
            .is_some_and(|e| e.contains("Better quality")),
        "{library}"
    );
    let started = dash
        .post(admin, "/api/admin/find", &json!({ "id": "soulseek:ext-1" }))
        .await;
    assert_eq!(started.status, StatusCode::OK);
    let id = started.json()["id"].as_str().expect("an id").to_string();
    finder.wait(&id, Duration::from_secs(5)).await;

    let found = dash.get(admin, &format!("/api/admin/find/{id}")).await.json();
    assert_eq!(found["state"], "done");
    assert!(found["song"]["libraryId"].is_null());
    let copies = found["candidate"].as_array().expect("copies");
    assert_eq!(copies.len(), 1);
    assert_eq!(copies[0]["id"], "c1");
    assert_eq!(copies[0]["quality"], "FLAC 16-bit 44.1 kHz");

    // Another admin's search is not theirs to follow or pick from.
    assert_eq!(
        dash.get(other, &format!("/api/admin/find/{id}")).await.status,
        StatusCode::NOT_FOUND
    );
    let refused = dash
        .post(
            other,
            &format!("/api/admin/find/{id}/pick"),
            &json!({ "copy": "c1" }),
        )
        .await
        .json();
    assert_eq!(refused["state"], "skipped");

    let picked = dash
        .post(
            admin,
            &format!("/api/admin/find/{id}/pick"),
            &json!({ "copy": "c1" }),
        )
        .await
        .json();
    assert_eq!(picked["state"], "queued");
    assert_eq!(*picked_by.lock(), [("admin".to_string(), "peer1".to_string())]);
    assert_eq!(
        dash.post(
            admin,
            &format!("/api/admin/find/{id}/pick"),
            &json!({ "copy": "" })
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(finder.for_user("admin").len(), 2);
    assert!(finder.for_user("other-admin").is_empty());
}

#[tokio::test]
async fn a_dashboard_pick_passes_anyway_through_and_relays_the_question() {
    let dash = Dashboard::new();
    let finder: Arc<SongFinder> = dash.state.song_finder.clone();
    let asked: Arc<Mutex<Vec<Option<bool>>>> = Arc::default();
    let heard = asked.clone();
    finder.seams(|seams| {
        seams.resolve = Arc::new(|id, _| {
            let target = FindTarget {
                external_id: Some(id.clone()),
                ..FindTarget::new("Daft Punk", "Digital Love", None, None, id)
            };
            async move { Some(target) }.boxed()
        });
        seams.sources_for = Arc::new(|_| vec![("Soulseek".to_string(), None)]);
        seams.search_soulseek = Arc::new(|_, _| {
            async {
                Ok((
                    vec![FoundCopy::new(
                        AcquisitionCandidate {
                            source: "Soulseek".into(),
                            peer: Some("peer1".into()),
                            file: Some("Digital Love.flac".into()),
                            format: Some("flac".into()),
                            ..Default::default()
                        },
                        PickedCopy {
                            peer: Some("peer1".into()),
                            file: Some("/share/Digital Love.flac".into()),
                            format: Some("flac".into()),
                            ..Default::default()
                        },
                    )],
                    Vec::new(),
                    None,
                ))
            }
            .boxed()
        });
        seams.fetch = Arc::new(move |_, _, _, ask| {
            heard.lock().push(ask.anyway);
            let outcome = match ask.anyway {
                Some(false) => PickOutcome::new(library_action_states::CONFIRM, "Replace yours anyway?"),
                _ => PickOutcome::new(library_action_states::QUEUED, "Getting the copy you picked."),
            };
            async move { Ok(outcome) }.boxed()
        });
    });
    let admin = dash.token(Some("admin"));
    let admin = admin.as_deref();
    let started = dash
        .post(admin, "/api/admin/find", &json!({ "id": "ext-1" }))
        .await
        .json();
    let id = started["id"].as_str().expect("an id").to_string();
    finder.wait(&id, Duration::from_secs(5)).await;
    let pick = format!("/api/admin/find/{id}/pick");

    let old = dash.post(admin, &pick, &json!({ "copy": "c1" })).await.json();
    assert_eq!(old["state"], "queued");
    let question = dash
        .post(admin, &pick, &json!({ "copy": "c1", "anyway": false }))
        .await
        .json();
    assert_eq!(question["state"], "confirm");
    assert_eq!(question["detail"], "Replace yours anyway?");
    let anyway = dash
        .post(admin, &pick, &json!({ "copy": "c1", "anyway": true }))
        .await
        .json();
    assert_eq!(anyway["state"], "queued");

    assert_eq!(*asked.lock(), [None, Some(false), Some(true)]);
}

#[tokio::test]
async fn a_find_started_while_two_run_answers_why() {
    let dash = Dashboard::new();
    let finder = dash.state.song_finder.clone();
    let (release, released) = tokio::sync::watch::channel(false);
    finder.seams(|seams| {
        seams.resolve = Arc::new(|id, _| {
            let target = FindTarget {
                external_id: Some(id.clone()),
                ..FindTarget::new("Daft Punk", id.clone(), None, None, id)
            };
            async move { Some(target) }.boxed()
        });
        seams.sources_for = Arc::new(|_| vec![("Soulseek".to_string(), None)]);
        seams.search_soulseek = Arc::new(move |_, _| {
            let mut released = released.clone();
            async move {
                let _ = released.wait_for(|done| *done).await;
                Ok((Vec::new(), Vec::new(), Some("Nothing found".to_string())))
            }
            .boxed()
        });
    });
    let admin = dash.token(Some("admin"));
    let admin = admin.as_deref();

    dash.post(admin, "/api/admin/find", &json!({ "id": "one" })).await;
    dash.post(admin, "/api/admin/find", &json!({ "id": "two" })).await;
    let third = dash
        .post(admin, "/api/admin/find", &json!({ "id": "three" }))
        .await
        .json();

    assert_eq!(third["state"], "failed");
    assert!(
        third["error"]
            .as_str()
            .is_some_and(|e| e.contains("already have")),
        "{third}"
    );
    release.send(true).expect("released");
}

/// The dashboard's own script, which reads the fields below.
fn admin_js() -> String {
    std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../octo/wwwroot/admin/admin.js"
    ))
    .expect("the dashboard script")
}

#[tokio::test]
async fn a_check_line_carries_what_acoust_id_heard_and_the_page_reads_it() {
    let dash = Dashboard::new();
    let now = Utc::now();
    let history = &dash.state.download_history;
    history.record(entry(
        "Teardrop",
        Some("soulseek:heard"),
        now - chrono::TimeDelta::minutes(1),
    ));
    let check = AcquisitionEvent {
        detail: Some(
            "the right length; AcoustID: no match above 85% (best 61%), so the name and length decide".into(),
        ),
        heard: Some(vec![
            AcquisitionHeard {
                title: "Teardrop".into(),
                artist: "Massive Attack".into(),
                score: 61,
            },
            AcquisitionHeard {
                title: "Angel".into(),
                artist: "Massive Attack".into(),
                score: 40,
            },
        ]),
        ..line(now, acquisition_event_kinds::CHECK, "Passed the checks", 0)
    };
    assert!(history.attach_log(
        "soulseek:heard",
        now - chrono::TimeDelta::minutes(2),
        &[
            line(now, acquisition_event_kinds::FOUND, "Found 1 copy", 1),
            check
        ],
    ));
    let token = dash.token(Some("admin"));
    let saved_at = history.get_recent(1)[0].downloaded_at.clone();
    let saved = dash
        .get(
            token.as_deref(),
            &format!(
                "/api/admin/downloads/log?key=soulseek:heard&at={}",
                octo_core::common::dotnet::escape_data_string(&saved_at)
            ),
        )
        .await
        .json();
    let lines = saved["event"].as_array().expect("lines");
    assert!(
        lines[0].get("heard").is_none(),
        "a line without any leaves it out"
    );
    assert_eq!(
        lines[1]["detail"],
        "the right length; AcoustID: no match above 85% (best 61%), so the name and length decide"
    );
    assert_eq!(
        lines[1]["heard"],
        json!([
            { "title": "Teardrop", "artist": "Massive Attack", "score": 61 },
            { "title": "Angel", "artist": "Massive Attack", "score": 40 },
        ])
    );

    let js = admin_js();
    for read in ["line.heard", "h.artist", "h.title", "h.score", "Also heard: "] {
        assert!(js.contains(read), "admin.js reads {read}");
    }
}

#[tokio::test]
async fn a_review_question_shows_acoust_id_numbers_and_the_page_reads_them() {
    use octo_core::fingerprint::VerificationResult;
    use octo_core::fingerprint::{AcoustIdLookup, AcoustIdRecording, AcoustIdResult, InconclusiveReason};
    use octo_core::models::domain::Song;

    let dash = Dashboard::new();
    let song = Song {
        artist: "Massive Attack".into(),
        title: "Teardrop".into(),
        ..Default::default()
    };
    let below = VerificationResult {
        reason: InconclusiveReason::BelowThreshold,
        lookup: Some(AcoustIdLookup::new(
            true,
            None,
            vec![AcoustIdResult::new(
                0.64,
                vec![AcoustIdRecording::new(
                    "rec",
                    "Angel",
                    ["Massive Attack"],
                    None,
                    None,
                )],
            )],
        )),
        threshold: 0.85,
        ..Default::default()
    };
    assert!(
        dash.state
            .notice_queue
            .add_review("alice", "/music/a.flac", &song, &below)
    );
    let no_entry = VerificationResult {
        reason: InconclusiveReason::NoEntry,
        ..Default::default()
    };
    assert!(
        dash.state
            .notice_queue
            .add_review("alice", "/music/b.flac", &song, &no_entry)
    );

    let token = dash.token(Some("admin"));
    let body = dash.get(token.as_deref(), "/api/admin/notices").await.json();
    let entries = body["entries"].as_array().expect("entries");
    let scored = entries
        .iter()
        .find(|e| e["reason"] == "AcoustID was not sure what this is")
        .expect("the scored question");
    assert_eq!(scored["acoustIdScore"], 64);
    assert_eq!(scored["threshold"], 85);
    // Below the threshold nothing matched: the best recording heard is only the closest.
    assert!(scored.get("matched").is_none(), "{scored}");
    assert_eq!(scored["closest"], "Massive Attack - Angel");
    let unscored = entries
        .iter()
        .find(|e| e["reason"] == "AcoustID has never heard this recording")
        .expect("the question without a score");
    for key in ["acoustIdScore", "threshold", "matched", "closest"] {
        assert!(unscored.get(key).is_none(), "{key} left out");
    }

    let js = admin_js();
    for read in [
        "entry.acoustIdScore",
        "entry.threshold",
        "entry.matched",
        "entry.closest",
        "closest: ",
        "% sure, needs ",
    ] {
        assert!(js.contains(read), "admin.js reads {read}");
    }
}
