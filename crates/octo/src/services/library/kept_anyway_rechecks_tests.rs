//! Files kept anyway stay on the path that checks kept files again: a play or a heart of their song,
//! and a heart AcoustID's clear reading proves, check one again. Once AcoustID reads it clearly, or
//! once its person has played the song so a copy can be judged against that play's master, the
//! search for a proven copy to replace it starts; nothing else starts it.

use futures::FutureExt;
use octo_core::settings::AppSettings;
use octo_core::soulseek::{RoutingKind, SoulseekRouting};
use url::Url;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::services::fingerprint::{AcoustIdRateLimitHandler, AcoustIdRateLimiter};
use crate::services::library::library_action_test_support::RecordingLibrary;

/// The track's catalog recording, R of the hearted album's track.
const NIGHT: &str = "1b4d4e1f-3a0d-4d5e-9a3c-7a1c2b3d4e5f";

/// AcoustID's clear answer naming the track's own recording, by many submissions.
fn naming_night() -> String {
    format!(
        r#"{{"status":"ok","results":[{{"id":"fp","score":0.98,"recordings":[{{"id":"{NIGHT}","title":"The Night Before","duration":154,"sources":60,"artists":[{{"name":"The Beatles"}}]}}]}}]}}"#
    )
}

const UNKNOWN: &str = r#"{"status":"ok","results":[]}"#;

struct Rig {
    _dir: tempfile::TempDir,
    acoust_id: MockServer,
    registry: Arc<ExternalIdRegistry>,
    rechecks: Arc<KeptRechecks>,
    upgraded: Arc<Mutex<Vec<String>>>,
    path: String,
    song: String,
}

impl Rig {
    async fn lookups(&self) -> usize {
        self.acoust_id.received_requests().await.unwrap_or_default().len()
    }
}

/// brandon's "The Night Before", kept anyway from a hearted album's held track, its print kept with
/// it and AcoustID answering it `answer`.
async fn kept_anyway(answer: &str) -> Rig {
    let acoust_id = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_raw(answer.to_string(), "application/json"))
        .mount(&acoust_id)
        .await;
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir
        .path()
        .join("The Night Before.flac")
        .to_string_lossy()
        .replace('\\', "/");
    std::fs::write(&path, b"the held track, kept anyway").expect("written");
    let settings = Arc::new(SettingsStore::for_tests(AppSettings::default()));
    let limiter = Arc::new(
        AcoustIdRateLimiter::with_gaps(Duration::from_millis(1), Duration::from_millis(1))
            .with_jitter(|| 1.0),
    );
    let base = Url::parse(&format!("{}/", acoust_id.uri())).expect("parses");
    let client = Arc::new(AcoustIdClient::with_built_in_key(
        Arc::new(AcoustIdRateLimitHandler::with_base_url(limiter, base)),
        settings.clone(),
        Some("octo-key"),
    ));
    let registry = Arc::new(ExternalIdRegistry::in_memory());
    let song = registry.register(SoulseekRouting {
        kind: RoutingKind::Song,
        artist: Some("The Beatles".into()),
        title: Some("The Night Before".into()),
        album: Some("Help!".into()),
        duration: Some(154),
        ..Default::default()
    });
    let library = Arc::new(RecordingLibrary::default());
    library.mappings.lock().push(LocalSongMapping {
        external_provider: "soulseek".into(),
        external_id: song.clone(),
        local_path: path.clone(),
        artist: "The Beatles".into(),
        title: "The Night Before".into(),
        proof_mark: Some("kept by you, version not proven".into()),
        heart: Some(KeptHeart {
            users: vec!["brandon".into()],
            recording: Some(HeardRecording {
                ids: vec![NIGHT.into()],
                title: "The Night Before".into(),
                artist: "The Beatles".into(),
                ..Default::default()
            }),
            heard: None,
            as_heard: false,
            print: Some("AQADtEqk".into()),
            seconds: Some(154),
        }),
        ..Default::default()
    });
    let rechecks = Arc::new(KeptRechecks::new(library, client, settings, registry.clone()));
    let upgraded: Arc<Mutex<Vec<String>>> = Arc::default();
    let told = upgraded.clone();
    rechecks.use_upgrade(Arc::new(move |mapping: LocalSongMapping| {
        told.lock().push(mapping.local_path.clone());
        async {}.boxed()
    }));
    Rig {
        _dir: dir,
        acoust_id,
        registry,
        rechecks,
        upgraded,
        path,
        song,
    }
}

/// A play of the song checks its kept-anyway file again with one lookup of the print kept with it.
/// Read clearly as its own recording now, a copy can be proven by sound: the search starts, and its
/// gate accepts only a clear reading, never a pass against the kept file itself.
#[tokio::test]
async fn a_kept_anyway_file_acoust_id_now_reads_clearly_is_searched_to_be_replaced() {
    let rig = kept_anyway(&naming_night()).await;

    rig.rechecks.song_event("soulseek", &rig.song).await;

    assert_eq!(rig.lookups().await, 1, "checked again");
    assert_eq!(rig.upgraded.lock().as_slice(), std::slice::from_ref(&rig.path));
    let KeptFor::Gate(gate) = rig.rechecks.gate_for(&rig.path, "brandon").await else {
        panic!("a gate");
    };
    assert_eq!(gate.user, "brandon");
    assert_eq!(gate.heard, None, "never the kept file itself");
    assert!(
        matches!(gate.reading, HeardReading::RStays { .. }),
        "{:?}",
        gate.reading
    );
    assert_eq!(gate.r.map(|r| r.ids), Some(vec![NIGHT.to_string()]));
}

/// A kept-anyway file AcoustID still cannot read starts no search on its own; once its person has
/// played the song, the next check starts the search a copy is judged against that play's master
/// in. A heart a clear reading proves checks it too.
#[tokio::test]
async fn a_kept_anyway_file_its_person_has_played_is_searched_against_that_play() {
    let rig = kept_anyway(UNKNOWN).await;

    rig.rechecks.after_a_proof().await;
    assert_eq!(rig.lookups().await, 1, "a proof checks it");
    assert!(rig.upgraded.lock().is_empty(), "nothing could be proven yet");

    let played = HeardRecord {
        heard_at: Utc::now(),
        source: HeardSource::Video {
            id: "NIGHTVIDEO1".into(),
            size: Some(4_000_000),
            content_type: "audio/mp4".into(),
            itag: Some(140),
        },
        sample_id: None,
    };
    rig.registry
        .remember_heard(&rig.song, "brandon", played.clone(), None);
    rig.rechecks.song_event("soulseek", &rig.song).await;

    assert_eq!(rig.lookups().await, 2);
    assert_eq!(rig.upgraded.lock().as_slice(), std::slice::from_ref(&rig.path));
    let KeptFor::Gate(gate) = rig.rechecks.gate_for(&rig.path, "brandon").await else {
        panic!("a gate");
    };
    assert_eq!(gate.heard.map(|heard| heard.source), Some(played.source));
}
