//! Files kept as heard, checked again: one lookup of the print kept with the file, read as what
//! was heard is read, the search for a better copy only when one could then be proven, and the gate
//! every replacement of a kept file meets.

use octo_core::settings::AppSettings;
use octo_core::soulseek::{RoutingKind, SoulseekRouting};
use url::Url;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::services::fingerprint::{AcoustIdRateLimitHandler, AcoustIdRateLimiter};
use crate::services::library::library_action_test_support::RecordingLibrary;

/// The 2009 Help!'s catalog recording, R for a heart of its catalog row.
const HELP_2009: &str = "4416d38e-0f68-4c75-93b9-c1a793fccb49";
/// The 2015 mix of Help!.
const HELP_2015: &str = "01b6e4de-ff48-4ec0-862e-cd623c924d7b";
const BEATLES: &str = "b10bbbfc-cf9e-42e0-be17-e2c3e1d2600d";

fn help_r() -> HeardRecording {
    HeardRecording {
        ids: vec![HELP_2009.into()],
        title: "Help!".into(),
        artist: "The Beatles".into(),
        ..Default::default()
    }
}

/// AcoustID's answer to the 2009 Help! heard for 240 s, as it was recorded.
fn help_2009_240() -> String {
    let file: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../octo-core/tests/fixtures/heart_album/gate/answers/help-2009-240.json"
        ))
        .expect("the recorded answer"),
    )
    .expect("JSON");
    file["answer"].to_string()
}

/// An answer that names `recording` alone, clearly, with many submissions.
fn naming(recording: &str) -> String {
    format!(
        r#"{{"status":"ok","results":[{{"id":"fp","score":0.98,"recordings":[{{"id":"{recording}","title":"Help!","duration":139,"sources":60,"artists":[{{"id":"{BEATLES}","name":"The Beatles"}}]}}]}}]}}"#
    )
}

const UNKNOWN: &str = r#"{"status":"ok","results":[]}"#;

struct Rig {
    _dir: tempfile::TempDir,
    acoust_id: MockServer,
    library: Arc<RecordingLibrary>,
    registry: Arc<ExternalIdRegistry>,
    rechecks: Arc<KeptRechecks>,
    upgraded: Arc<Mutex<Vec<String>>>,
    path: String,
}

impl Rig {
    async fn lookups(&self) -> usize {
        self.acoust_id.received_requests().await.unwrap_or_default().len()
    }
}

/// A kept file at a temp path whose AcoustID answer is `answer`, recorded as `mapping` builds it.
async fn rig(answer: &str, mapping: impl FnOnce(&str) -> LocalSongMapping) -> Rig {
    let acoust_id = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_raw(answer.to_string(), "application/json"))
        .mount(&acoust_id)
        .await;
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("Help!.m4a").to_string_lossy().replace('\\', "/");
    std::fs::write(&path, b"the audio that played").expect("written");
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
    let library = Arc::new(RecordingLibrary::default());
    library.mappings.lock().push(mapping(&path));
    let registry = Arc::new(ExternalIdRegistry::in_memory());
    let rechecks = Arc::new(KeptRechecks::new(
        library.clone(),
        client,
        settings,
        registry.clone(),
    ));
    let upgraded: Arc<Mutex<Vec<String>>> = Arc::default();
    let told = upgraded.clone();
    rechecks.use_upgrade(Arc::new(move |mapping: LocalSongMapping| {
        told.lock().push(mapping.local_path.clone());
        async {}.boxed()
    }));
    Rig {
        _dir: dir,
        acoust_id,
        library,
        registry,
        rechecks,
        upgraded,
        path,
    }
}

/// Octo's kept-as-heard Help! for brandon, its print kept with it.
fn kept_as_heard(path: &str) -> LocalSongMapping {
    LocalSongMapping {
        external_provider: "soulseek".into(),
        external_id: "help".into(),
        local_path: path.to_string(),
        artist: "The Beatles".into(),
        title: "Help!".into(),
        proof_mark: Some("as heard on YouTube".into()),
        heart: Some(KeptHeart {
            users: vec!["brandon".into()],
            recording: Some(help_r()),
            heard: Some(HeardRecord {
                heard_at: Utc::now(),
                source: HeardSource::Video {
                    id: "u7K72X4eo_s".into(),
                    size: Some(4_000_000),
                    content_type: "audio/mp4".into(),
                    itag: Some(140),
                },
                sample_id: Some("sample-1".into()),
            }),
            as_heard: true,
            print: Some("AQADtEqk".into()),
            seconds: Some(140),
        }),
        ..Default::default()
    }
}

fn gate_of(kept: KeptFor) -> HeartGate {
    match kept {
        KeptFor::Gate(gate) => *gate,
        other => panic!("no gate: {other:?}"),
    }
}

/// `help-2009-240.json` replayed as the kept file's answer reads "R stays", and the search for a
/// better copy runs. The search's gate takes that very reading: no second lookup.
#[tokio::test]
async fn help_2009_240_as_the_kept_files_answer_reads_r_stays_and_the_upgrade_runs() {
    let rig = rig(&help_2009_240(), kept_as_heard).await;

    rig.rechecks.song_event("soulseek", "help").await;

    assert_eq!(rig.upgraded.lock().as_slice(), std::slice::from_ref(&rig.path));
    assert_eq!(rig.lookups().await, 1);
    let gate = gate_of(rig.rechecks.gate_for(&rig.path, "admin").await);
    assert!(
        matches!(gate.reading, HeardReading::RStays { .. }),
        "{:?}",
        gate.reading
    );
    assert_eq!(gate.r, Some(help_r()));
    assert_eq!(gate.user, "brandon", "the heart's own person");
    assert_eq!(gate.origin, AcquisitionOrigin::Heart);
    let heard = gate.heard.expect("the kept file is the heard audio");
    assert_eq!(
        heard.source,
        HeardSource::Library {
            path: rig.path.clone()
        }
    );
    assert_eq!(
        heard.sample_id.as_deref(),
        Some("sample-1"),
        "proof 2 against what played"
    );
    assert_eq!(rig.lookups().await, 1, "the search sent no second lookup");
}

/// On a default install (the weekly upgrade off), a kept file whose song is played again, and
/// whose answer now names R clearly, is upgraded: the search runs with R stays and its tag.
#[tokio::test]
async fn a_kept_file_played_again_whose_answer_now_names_r_is_upgraded_on_a_default_install() {
    let rig = rig(&naming(HELP_2009), kept_as_heard).await;
    assert_eq!(
        octo_core::settings::LibraryActionSettings::default().upgrade_per_week,
        0,
        "the weekly upgrade is off by default"
    );
    let rechecks = rig.rechecks.clone();
    rig.registry
        .on_heard(Arc::new(move |id: &str, _: &str, _: &HeardRecord| {
            let (rechecks, id) = (rechecks.clone(), id.to_string());
            tokio::spawn(async move { rechecks.song_event("soulseek", &id).await });
        }));
    let id = rig.registry.register(SoulseekRouting {
        kind: RoutingKind::Song,
        artist: Some("The Beatles".into()),
        title: Some("Help!".into()),
        ..Default::default()
    });
    rig.library.mappings.lock()[0].external_id = id.clone();

    // Someone plays it.
    rig.registry.remember_heard(
        &id,
        "anna",
        HeardRecord {
            heard_at: Utc::now(),
            source: HeardSource::Video {
                id: "u7K72X4eo_s".into(),
                size: Some(4_000_000),
                content_type: "audio/mp4".into(),
                itag: Some(140),
            },
            sample_id: None,
        },
        None,
    );
    crate::services::common::test_fakes::until(|| !rig.upgraded.lock().is_empty()).await;
    let gate = gate_of(rig.rechecks.gate_for(&rig.path, "admin").await);
    assert_eq!(
        gate.reading,
        HeardReading::RStays {
            tag: Some(HELP_2009.into())
        }
    );
}

/// A check that finds nothing new (AcoustID still does not know the kept file) downloads
/// nothing, and sends no second lookup until the next event.
#[tokio::test]
async fn a_recheck_that_finds_nothing_downloads_nothing_and_asks_once_per_event() {
    let rig = rig(UNKNOWN, kept_as_heard).await;

    let kept = rig.library.mappings.lock()[0].clone();
    let read = rig.rechecks.recheck(kept).await;

    assert_eq!(read, Reread::OnlyHeardOrMaster);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(rig.upgraded.lock().is_empty(), "nothing downloaded");
    assert_eq!(rig.lookups().await, 1, "one lookup, no loop");
    rig.rechecks.song_event("soulseek", "help").await;
    assert_eq!(rig.lookups().await, 2, "the next event asks again");
    assert!(rig.upgraded.lock().is_empty());
}

/// A kept file AcoustID now names as another recording of the song (Y) is judged against Y: an
/// X copy whose clear reading is {X} cannot replace it.
#[tokio::test]
async fn a_kept_file_now_named_as_y_is_replaced_only_against_y() {
    let rig = rig(&naming(HELP_2015), kept_as_heard).await;

    let gate = gate_of(rig.rechecks.gate_for(&rig.path, "brandon").await);

    assert!(
        matches!(gate.reading, HeardReading::RBecame { .. }),
        "{:?}",
        gate.reading
    );
    let r = gate.r.expect("R");
    assert_eq!(r.ids, [HELP_2015]);
    assert!(!r.holds(HELP_2009), "the X copy's recording is not R");
}

/// The weekly upgrade over a kept file AcoustID does not know: only the very master the kept file
/// is can replace it, so an unprovable FLAC and an other-mix FLAC never do.
#[tokio::test]
async fn an_upgrade_of_a_kept_file_acoust_id_does_not_know_accepts_only_its_own_master() {
    let rig = rig(UNKNOWN, kept_as_heard).await;

    let gate = gate_of(rig.rechecks.gate_for(&rig.path, "brandon").await);

    assert_eq!(gate.reading, HeardReading::OnlyHeardOrMaster);
    assert_eq!(
        gate.heard.map(|heard| heard.source),
        Some(HeardSource::Library {
            path: rig.path.clone()
        })
    );
}

/// A kept file whose answer now names another song entirely: nothing is tried for it.
#[tokio::test]
async fn a_kept_file_now_named_as_another_song_is_not_upgraded() {
    let other = r#"{"status":"ok","results":[{"id":"fp","score":0.98,"recordings":[{"id":"aaaaaaaa-0000-0000-0000-000000000000","title":"Yellow Submarine","duration":139,"sources":60,"artists":[{"id":"b10bbbfc-cf9e-42e0-be17-e2c3e1d2600d","name":"The Beatles"}]}]}]}"#;
    let rig = rig(other, kept_as_heard).await;

    let kept = rig.library.mappings.lock()[0].clone();
    assert_eq!(rig.rechecks.recheck(kept).await, Reread::AnotherSong);
    assert!(rig.upgraded.lock().is_empty());
    assert_eq!(
        rig.rechecks.gate_for(&rig.path, "brandon").await,
        KeptFor::Nothing(KEPT_IS_ANOTHER_SONG)
    );
}

/// A "Keep these anyway" track nobody played is never replaced on a pass against itself: it is not
/// heard audio, so its gate has nothing heard and only a clear reading accepts. Once the person
/// who hearted it plays the song, a copy is judged against that play.
#[tokio::test]
async fn a_kept_anyway_track_is_judged_against_its_persons_play_and_never_against_itself() {
    let rig = rig(UNKNOWN, |path| LocalSongMapping {
        external_provider: "soulseek".into(),
        external_id: "help".into(),
        local_path: path.to_string(),
        proof_mark: Some("kept by you, version not proven".into()),
        heart: Some(KeptHeart {
            users: vec!["brandon".into()],
            recording: Some(help_r()),
            ..Default::default()
        }),
        ..Default::default()
    })
    .await;
    let id = rig.registry.register(SoulseekRouting {
        kind: RoutingKind::Song,
        artist: Some("The Beatles".into()),
        title: Some("Help!".into()),
        ..Default::default()
    });
    rig.library.mappings.lock()[0].external_id = id.clone();

    let gate = gate_of(rig.rechecks.gate_for(&rig.path, "brandon").await);
    assert_eq!(gate.heard, None, "never the kept file itself");
    assert_eq!(
        gate.reading,
        HeardReading::RStays { tag: None },
        "a clear reading accepts"
    );

    let played = HeardRecord {
        heard_at: Utc::now(),
        source: HeardSource::Video {
            id: "u7K72X4eo_s".into(),
            size: Some(4_000_000),
            content_type: "audio/mp4".into(),
            itag: Some(140),
        },
        sample_id: None,
    };
    rig.registry.remember_heard(&id, "brandon", played.clone(), None);
    let gate = gate_of(rig.rechecks.gate_for(&rig.path, "brandon").await);
    assert_eq!(gate.heard.map(|heard| heard.source), Some(played.source));
    assert_eq!(rig.lookups().await, 0, "its own answer is never the heard one");
}

/// Each heart AcoustID's clear reading accepts checks the one kept file checked longest ago.
#[tokio::test]
async fn each_proof_checks_the_kept_file_checked_longest_ago() {
    let rig = rig(UNKNOWN, kept_as_heard).await;
    let second = rig.path.replace("Help!.m4a", "Help! (1).m4a");
    std::fs::write(&second, b"another kept file").expect("written");
    let mut other = kept_as_heard(&second);
    other.external_id = "help-again".into();
    rig.library.mappings.lock().push(other);

    rig.rechecks.after_a_proof().await;
    rig.rechecks.after_a_proof().await;

    assert_eq!(rig.lookups().await, 2, "one lookup per proof");
    let turns = rig.rechecks.checked.lock().clone();
    assert_eq!(turns.len(), 2, "both kept files, one per proof: {turns:?}");
}
