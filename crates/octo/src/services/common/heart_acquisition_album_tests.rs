//! Album hearts in the chain: the album's gate is built and held for the whole chain, the tracks
//! a walk held are filed as the album's held tracks under its row, and a held track ends only on
//! work. A play of it (found by its song id, or by its names and length from radio or search) runs
//! the play's heard answer first, then the heard master against the held file; AcoustID asked
//! again proves it or names it another recording; un-hearting the album deletes its held tracks.
//! Each re-check event checks a held item once and fetches nothing when nothing is found.

use std::sync::Arc;
use std::time::Duration;

use octo_core::common::Clock;
use octo_core::fingerprint::heart_gate::HeardSource;
use octo_core::models::domain::Song;
use octo_core::settings::{AppSettings, HeartDownloadSource, HeartDownloadStep, SubsonicSettings};
use octo_core::soulseek::{RoutingKind, SoulseekRouting};

use super::*;
use crate::services::common::album_gates::AlbumGates;
use crate::services::common::soulseek_hold_store::HeldAlbumTrack;
use crate::services::common::test_fakes::{FakeDownloads, FakeLidarr, FakeMetadata, until};
use crate::services::common::{AcquisitionRequest, SoulseekHoldResumer};
use crate::services::fingerprint::download_verification_service::DownloadVerificationService;
use crate::services::fingerprint::heart_test_support::{
    GateRig, GateSetup, frozen_body, gate_fixture, gate_rig, sound_bytes, sound_flac,
};
use crate::services::library::upgrade_queue::KeptRechecks;
use crate::services::prefetch::heard_samples::tests::items_of;
use crate::services::prefetch::heard_samples::{HeardPrints, sample_id_of};
use crate::services::soulseek::SharedRouting;

const HELP_2009: &str = "4416d38e-0f68-4c75-93b9-c1a793fccb49";
const HEARD: &str = "HEARDVIDEO1";

fn help_r() -> HeardRecording {
    HeardRecording {
        ids: vec![HELP_2009.into()],
        duplicates: vec![HELP_2009.into()],
        isrcs: vec!["GBAYE0601465".into()],
        title: "Help! (Remastered 2009)".into(),
        artist: "The Beatles".into(),
        ..Default::default()
    }
}

fn sources(steps: &[HeartDownloadSource]) -> Arc<SettingsStore> {
    Arc::new(SettingsStore::for_tests(AppSettings {
        subsonic: SubsonicSettings {
            heart_download_sources: steps
                .iter()
                .map(|&source| HeartDownloadStep {
                    source,
                    song_enabled: Some(true),
                    album_enabled: Some(true),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        },
        ..Default::default()
    }))
}

struct AlbumRig {
    gate: GateRig,
    coordinator: Arc<HeartAcquisitionCoordinator>,
    queue: Arc<TrackAcquisitionQueue>,
    tracker: Arc<AcquisitionTracker>,
    album_gates: Arc<AlbumGates>,
    metadata: Arc<FakeMetadata>,
    downloads: Arc<FakeDownloads>,
    /// The held tracks' files.
    files: tempfile::TempDir,
}

/// A coordinator whose gates are the real heart gate over the sound rig, with the album gates
/// and `downloads` as the album walks.
async fn album_rig_with(downloads: FakeDownloads, steps: &[HeartDownloadSource]) -> AlbumRig {
    album_rig_full(downloads, steps, |service| service as Arc<dyn HeartGates>, None).await
}

/// [`album_rig_with`], its gates `wrap` of the heart gate, checking `kept`'s items again.
async fn album_rig_full(
    downloads: FakeDownloads,
    steps: &[HeartDownloadSource],
    wrap: impl FnOnce(Arc<DownloadVerificationService>) -> Arc<dyn HeartGates>,
    kept: Option<Arc<KeptRechecks>>,
) -> AlbumRig {
    let gate = gate_rig(GateSetup::default()).await;
    let queue = Arc::new(TrackAcquisitionQueue::new());
    let tracker = Arc::new(AcquisitionTracker::new(None, Clock::system()));
    let album_gates = Arc::new(AlbumGates::new());
    let metadata = Arc::new(FakeMetadata::default());
    let downloads = Arc::new(downloads);
    let coordinator = HeartAcquisitionCoordinator::new(
        sources(steps),
        queue.clone(),
        downloads.clone(),
        Arc::new(FakeLidarr::default()),
        HeartCoordinatorExtras {
            tracker: Some(tracker.clone()),
            id_registry: Some(gate.rig.registry.clone()),
            holds: Some(gate.holds.clone()),
            gates: Some(wrap(gate.service.clone())),
            album_gates: Some(album_gates.clone()),
            metadata: Some(metadata.clone()),
            kept: kept.clone(),
            ..Default::default()
        },
    );
    if let Some(kept) = kept {
        let (waiting, recheck) = (Arc::downgrade(&coordinator), Arc::downgrade(&coordinator));
        kept.use_holds(
            Arc::new(move || {
                waiting
                    .upgrade()
                    .map(|coordinator| coordinator.rechecked_holds())
                    .unwrap_or_default()
            }),
            Arc::new(move |key: &str| {
                if let Some(coordinator) = recheck.upgrade() {
                    coordinator.resume_held(key);
                }
            }),
        );
    }
    AlbumRig {
        gate,
        coordinator,
        queue,
        tracker,
        album_gates,
        metadata,
        downloads,
        files: tempfile::tempdir().expect("a temp dir"),
    }
}

async fn album_rig() -> AlbumRig {
    album_rig_with(FakeDownloads::default(), &[HeartDownloadSource::Soulseek]).await
}

impl AlbumRig {
    /// Registers a song of the album "Help!" by The Beatles, answering its id.
    fn song(&self, title: &str, seconds: i32) -> String {
        self.gate
            .rig
            .registry
            .register(SharedRouting::new(SoulseekRouting {
                kind: RoutingKind::Song,
                artist: Some("The Beatles".into()),
                title: Some(title.into()),
                album: Some("Help!".into()),
                duration: Some(seconds),
                ..Default::default()
            }))
    }

    /// A held file whose print is `print` (its frozen items and answer when it names a frozen
    /// one), its master `master`.
    fn held_file(&self, name: &str, print: &str, master: &str) -> String {
        let path = self.files.path().join(format!("{name}.flac"));
        sound_flac(&path, 139, &sound_bytes(print, master, print), None);
        path.to_string_lossy().to_string()
    }

    /// Files printed `name` print and are answered as the frozen `print` is.
    fn frozen_as(&self, name: &str, print: &str) {
        self.gate
            .printer
            .register(name, items_of(&gate_fixture(&format!("prints/{print}.json"))));
        self.gate.answers.answer(name, &frozen_body(print));
    }

    /// `print`'s frozen AcoustID answer, and its frozen items when a print of it is kept.
    fn frozen(&self, print: &str) {
        let kept = gate_fixture(&format!("prints/{print}.json"));
        if kept.is_file() {
            self.gate.printer.register(print, items_of(&kept));
        }
        self.gate.answers.answer(print, &frozen_body(print));
    }

    /// Brandon's held track `id` of the album "help-album", holding `file`, waiting since `since`
    /// minutes ago.
    fn hold_track(&self, id: &str, file: &str, users: &[&str], since: i64) -> HeldAcquisition {
        self.gate.holds.hold(HeldAcquisition {
            kind: HeldKind::Track,
            provider: "soulseek".into(),
            external_id: id.into(),
            requested_by: users.first().map(|user| user.to_string()),
            held_since_utc: Utc::now() - TimeDelta::minutes(since),
            origin: Some(AcquisitionOrigin::AlbumHeart),
            waiting_for: Some(WaitingFor::NotYetProven),
            holders: users
                .iter()
                .map(|user| Holder {
                    user: user.to_string(),
                    ..Default::default()
                })
                .collect(),
            held_files: vec![file.into()],
            album_track: Some(HeldAlbumTrack {
                album_key: "soulseek:help-album".into(),
                song_ids: vec![id.into()],
                artist: "The Beatles".into(),
                title: "Help! (Remastered 2009)".into(),
                duration_seconds: Some(139),
                r: help_r(),
            }),
            line: Some(tracks_wait_line(1)),
            ..Default::default()
        })
    }

    /// What `user` heard when they streamed a song: the video `HEARD`, its whole print `print`
    /// (its frozen items and answer), the shim sending `master` as its fresh stream.
    async fn heard(&self, print: &str, master: &str) -> HeardRecord {
        self.frozen(print);
        let source = HeardSource::Video {
            id: HEARD.into(),
            size: Some(4_000_000),
            content_type: "audio/mp4".into(),
            itag: None,
        };
        let sample = sample_id_of(&source);
        self.gate.rig.samples.store(
            &sample,
            &HeardPrints {
                raw: self.gate.printer.items(print),
                compressed: print.into(),
                seconds: 240,
                whole: true,
                reaches_end: false,
            },
            Some(HEARD),
        );
        self.gate
            .shim_streams(HEARD, sound_bytes(print, master, "heard"))
            .await;
        HeardRecord {
            heard_at: Utc::now(),
            source,
            sample_id: Some(sample),
        }
    }

    fn held_count(&self) -> usize {
        self.gate.holds.snapshot().len()
    }
}

/// Every request the queue hands out, kept, and landed at its held file.
fn land_held(
    queue: &Arc<TrackAcquisitionQueue>,
) -> (Arc<Mutex<Vec<Arc<AcquisitionRequest>>>>, CancellationToken) {
    let taken: Arc<Mutex<Vec<Arc<AcquisitionRequest>>>> = Arc::default();
    let stop = CancellationToken::new();
    let (queue, seen, token) = (queue.clone(), taken.clone(), stop.clone());
    tokio::spawn(async move {
        while let Some(request) = queue.dequeue(&token).await {
            seen.lock().push(request.clone());
            let ledger = request.completion.ledger().clone();
            for gate in request.gates() {
                ledger.record_accepted(&gate.user, gate.origin, Acceptance::HeardMaster);
            }
            queue.release(&request);
            let file = request.place().held_files.first().cloned().unwrap_or_default();
            request.completion.try_set_result(file);
        }
    });
    (taken, stop)
}

/// How many lookups AcoustID was asked once `wanted` arrived, or after a short wait for more.
async fn asked_settle(rig: &AlbumRig, wanted: usize) -> usize {
    for _ in 0..300 {
        if rig.gate.answers.asked().len() >= wanted {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    rig.gate.answers.asked().len()
}

// ---- The album's gate in the chain ------------------------------------------------------------

/// An album heart's chain builds the album's gate before any source runs and holds it for the
/// whole chain; the track a walk held is filed as the album's held track with its files and the
/// track's own recording, the album's row says so, and the next source's walk leaves it alone.
#[tokio::test]
async fn an_album_heart_files_what_its_walk_held_and_no_later_source_fetches_it_again() {
    let seen: Arc<Mutex<Vec<(DownloadSource, bool, Vec<String>)>>> = Arc::default();
    let gates_seen = seen.clone();
    let album_gates: Arc<Mutex<Option<Arc<AlbumGates>>>> = Arc::default();
    let reach = album_gates.clone();
    let held_file: Arc<Mutex<String>> = Arc::default();
    let file = held_file.clone();
    let downloads = FakeDownloads::albums(move |source, _| {
        let store = reach.lock().clone().expect("the album gates");
        let users = vec!["brandon".to_string()];
        let gated: Vec<String> = store
            .track_gates("soulseek", "help-album", &users, "night")
            .into_iter()
            .map(|gate| gate.r.map(|r| r.title).unwrap_or_default())
            .collect();
        let settled = store.settled("soulseek", "help-album", &users, "night");
        gates_seen.lock().push((source, settled, gated));
        if source == DownloadSource::Soulseek {
            store.held(
                "soulseek",
                "help-album",
                &users,
                "night",
                AcquisitionHeld {
                    waiting_for: WaitingFor::NotYetProven,
                    line: tracks_wait_line(1),
                    files: vec![file.lock().clone()],
                    users: Vec::new(),
                },
            );
            return Ok(false);
        }
        Ok(true)
    });
    let rig = album_rig_with(
        downloads,
        &[HeartDownloadSource::Soulseek, HeartDownloadSource::YouTube],
    )
    .await;
    *album_gates.lock() = Some(rig.album_gates.clone());
    *held_file.lock() = rig.held_file("night", "night", "night-master");
    rig.metadata.albums.lock().insert(
        ("soulseek".into(), "help-album".into()),
        Album {
            id: "help-album".into(),
            title: "Help!".into(),
            artist: "The Beatles".into(),
            songs: vec![
                Song {
                    external_provider: Some("soulseek".into()),
                    external_id: Some("help".into()),
                    title: "Help!".into(),
                    artist: "The Beatles".into(),
                    duration: Some(139),
                    track: Some(1),
                    ..Default::default()
                },
                Song {
                    external_provider: Some("soulseek".into()),
                    external_id: Some("night".into()),
                    title: "The Night Before".into(),
                    artist: "The Beatles".into(),
                    duration: Some(154),
                    track: Some(2),
                    ..Default::default()
                },
            ],
            ..Default::default()
        },
    );

    rig.coordinator
        .acquire_album("soulseek", "help-album", Some("brandon"), None)
        .await
        .expect("ran");

    assert_eq!(
        rig.downloads.album_sources(),
        [DownloadSource::Soulseek, DownloadSource::YouTube]
    );
    let seen = seen.lock().clone();
    assert_eq!(
        seen,
        [
            (
                DownloadSource::Soulseek,
                false,
                vec!["The Night Before".to_string()]
            ),
            (
                DownloadSource::YouTube,
                true,
                vec!["The Night Before".to_string()]
            ),
        ],
        "both walks were gated, and the second left the held track alone"
    );
    assert!(
        rig.album_gates
            .gate("soulseek", "help-album", "brandon")
            .is_none(),
        "the gate goes with the chain"
    );
    let record = rig
        .gate
        .holds
        .get("Track:soulseek:night:NotYetProven")
        .expect("held as the album's track");
    assert_eq!(record.origin(), AcquisitionOrigin::AlbumHeart);
    assert_eq!(record.held_files, [held_file.lock().clone()]);
    let track = record.album_track.expect("an album track");
    assert_eq!(track.album_key, "soulseek:help-album");
    assert_eq!(track.title, "The Night Before");
    assert_eq!(track.song_ids, ["night"]);
    assert_eq!(
        record.line.as_deref(),
        Some("1 track waits: play it once, or keep it anyway")
    );
    assert!(rig.tracker.is_held("soulseek", "help-album"));
    let row = rig
        .tracker
        .detail("soulseek:help-album", None)
        .expect("the album's row");
    assert_eq!(
        row.note.as_deref(),
        Some("1 track waits: play it once, or keep it anyway")
    );
    assert!(row.keep_anyway);
}

/// The album's row counts every held track of it.
#[tokio::test]
async fn the_albums_row_counts_its_held_tracks() {
    let rig = album_rig().await;
    let first = rig.held_file("help", "help", "help-master");
    let second = rig.held_file("night", "night", "night-master");
    rig.hold_track("help", &first, &["brandon"], 2);
    let record = rig.hold_track("night", &second, &["brandon"], 1);

    rig.coordinator.hold_row_for(&record);

    let row = rig
        .tracker
        .detail("soulseek:help-album", None)
        .expect("the album's row");
    assert_eq!(
        row.note.as_deref(),
        Some("2 tracks wait: play them once, or keep them anyway")
    );
    assert!(row.keep_anyway);
    assert_eq!(row.state, AcquisitionState::Searching);
}

// ---- How a held album track ends -------------------------------------------------------------

/// A radio play of a held track, its id another than the album's, is found by its artist, core
/// title and length; the play's heard answer reads R stays, and the held file is the master that
/// played: it lands, and the held track ends.
#[tokio::test]
async fn a_radio_play_of_a_held_track_proves_it_by_the_master_that_played() {
    let rig = album_rig().await;
    let (taken, stop) = land_held(&rig.queue);
    let track = rig.song("Help! (Remastered 2009)", 139);
    let radio = rig
        .gate
        .rig
        .registry
        .register(SharedRouting::new(SoulseekRouting {
            kind: RoutingKind::Song,
            artist: Some("The Beatles".into()),
            title: Some("Help!".into()),
            album: Some("1".into()),
            duration: Some(140),
            ..Default::default()
        }));
    rig.frozen("help-2009-120");
    let file = rig.held_file("help", "help-2009-120", "help-2009-master");
    rig.hold_track(&track, &file, &["brandon"], 1);
    let played = rig.heard("help-2009-240", "help-2009-master").await;

    rig.coordinator.heard_again(&radio, "brandon", &played);

    until(|| rig.held_count() == 0).await;
    let taken = taken.lock().clone();
    assert_eq!(taken.len(), 1, "the held file, and nothing else");
    assert_eq!(taken[0].external_id, track);
    assert_eq!(taken[0].origin, Some(AcquisitionOrigin::AlbumHeart));
    assert_eq!(taken[0].place().held_files, [file]);
    let gates = taken[0].gates();
    assert_eq!(
        gates[0].heard.as_ref().map(|heard| heard.source.clone()),
        Some(played.source)
    );
    assert_eq!(
        rig.gate.streams_of(HEARD).await,
        1,
        "the heard master was fetched once"
    );
    stop.cancel();
}

/// A play whose heard master is not the held file's: an A2 fail is undecided, so the track stays
/// held with what the person heard, and nothing is fetched.
#[tokio::test]
async fn an_a2_fail_leaves_the_held_track_held() {
    let rig = album_rig().await;
    let (taken, stop) = land_held(&rig.queue);
    let track = rig.song("Help! (Remastered 2009)", 139);
    rig.frozen("help-2009-120");
    let file = rig.held_file("help", "help-2009-120", "help-2009-master");
    rig.hold_track(&track, &file, &["brandon"], 1);
    let played = rig.heard("help-2009-240", "another-master").await;

    rig.coordinator.heard_again(&track, "brandon", &played);

    until(|| rig.gate.answers.asked().contains(&"help-2009-120".to_string())).await;
    until(|| rig.coordinator.resuming.lock().is_empty()).await;
    let record = rig
        .gate
        .holds
        .get(&format!("Track:soulseek:{track}:NotYetProven"))
        .expect("still held");
    assert_eq!(
        record.holders[0].heard.as_ref().map(|heard| heard.source.clone()),
        Some(played.source)
    );
    assert!(std::path::Path::new(&file).is_file());
    assert!(taken.lock().is_empty(), "nothing fetched");
    stop.cancel();
}

/// A play that was another song (a believed different song) proves nothing: the track stays held,
/// its file kept, nothing fetched.
#[tokio::test]
async fn a_play_that_was_another_song_leaves_the_held_track_held() {
    let rig = album_rig().await;
    let (taken, stop) = land_held(&rig.queue);
    let track = rig.song("Help! (Remastered 2009)", 139);
    rig.frozen("help-2009-120");
    let file = rig.held_file("help", "help-2009-120", "help-2009-master");
    rig.hold_track(&track, &file, &["brandon"], 1);
    let played = rig
        .heard("constructed-another-song-240", "help-2009-master")
        .await;

    rig.coordinator.heard_again(&track, "brandon", &played);

    until(|| rig.gate.answers.asked().contains(&"help-2009-120".to_string())).await;
    until(|| rig.coordinator.resuming.lock().is_empty()).await;
    assert_eq!(rig.held_count(), 1, "still held");
    assert!(std::path::Path::new(&file).is_file());
    assert_eq!(rig.gate.streams_of(HEARD).await, 0, "no heard master is compared");
    assert!(taken.lock().is_empty(), "nothing fetched");
    stop.cancel();
}

/// AcoustID asked again: a held track whose file now reads clear against its recording lands; one
/// whose file AcoustID now names as another recording is deleted, and its held track ends.
#[tokio::test]
async fn a_held_track_now_clear_lands_and_one_named_another_recording_is_deleted() {
    let rig = album_rig().await;
    let (taken, stop) = land_held(&rig.queue);
    let help = rig.song("Help! (Remastered 2009)", 139);
    let clear = rig.held_file("help", "constructed-clear-2009-120", "help-2009-master");
    rig.gate.printer.register(
        "constructed-clear-2009-120",
        items_of(&gate_fixture("prints/help-2009-120.json")),
    );
    rig.gate.answers.answer(
        "constructed-clear-2009-120",
        &frozen_body("constructed-clear-2009-120"),
    );
    rig.hold_track(&help, &clear, &["brandon"], 2);
    let night = rig.song("The Night Before", 154);
    let other = rig.held_file("night", "night-other", "night-master");
    rig.gate.answers.answer(
        "night-other",
        r#"{"status":"ok","results":[{"id":"fp","score":0.97,"recordings":[{"id":"rec-ticket","title":"Ticket to Ride","duration":190,"sources":40,"artists":[{"name":"The Beatles"}]}]}]}"#,
    );
    rig.hold_track(&night, &other, &["brandon"], 1);

    rig.coordinator
        .resume_held(&format!("Track:soulseek:{help}:NotYetProven"));
    rig.coordinator
        .resume_held(&format!("Track:soulseek:{night}:NotYetProven"));

    until(|| rig.held_count() == 0).await;
    let taken = taken.lock().clone();
    assert_eq!(taken.len(), 1, "only the proven file lands");
    assert_eq!(taken[0].external_id, help);
    assert!(
        !std::path::Path::new(&other).exists(),
        "another recording is deleted"
    );
    until(|| !rig.tracker.is_held("soulseek", "help-album")).await;
    stop.cancel();
}

/// Un-hearting the album deletes every held track of it nobody else holds; a track another person
/// holds stays held for them, its file kept, and another album's held track is left alone.
#[tokio::test]
async fn un_hearting_the_album_deletes_its_held_tracks() {
    let rig = album_rig().await;
    let help = rig.held_file("help", "help", "help-master");
    let night = rig.held_file("night", "night", "night-master");
    let shared = rig.held_file("yesterday", "yesterday", "yesterday-master");
    rig.hold_track("help", &help, &["brandon"], 3);
    rig.hold_track("night", &night, &["brandon"], 2);
    rig.hold_track("yesterday", &shared, &["brandon", "anna"], 1);
    let elsewhere = rig.held_file("elsewhere", "elsewhere", "elsewhere-master");
    rig.gate.holds.hold(HeldAcquisition {
        kind: HeldKind::Track,
        provider: "soulseek".into(),
        external_id: "elsewhere".into(),
        requested_by: Some("brandon".into()),
        held_since_utc: Utc::now(),
        origin: Some(AcquisitionOrigin::AlbumHeart),
        waiting_for: Some(WaitingFor::NotYetProven),
        held_files: vec![elsewhere.clone()],
        album_track: Some(HeldAlbumTrack {
            album_key: "soulseek:other-album".into(),
            r: help_r(),
            ..Default::default()
        }),
        ..Default::default()
    });

    let left = rig
        .coordinator
        .leave_album_holds("soulseek", "help-album", "Brandon");

    assert_eq!(left, 3);
    assert!(!std::path::Path::new(&help).exists());
    assert!(!std::path::Path::new(&night).exists());
    assert!(std::path::Path::new(&shared).is_file(), "anna still holds it");
    let keys: Vec<String> = rig
        .gate
        .holds
        .snapshot()
        .iter()
        .map(HeldAcquisition::key)
        .collect();
    assert_eq!(
        keys,
        [
            "Track:soulseek:yesterday:NotYetProven",
            "Track:soulseek:elsewhere:NotYetProven"
        ]
    );
    assert!(std::path::Path::new(&elsewhere).is_file());
}

// ---- The re-check events -----------------------------------------------------------------------

/// Another person's play of a held track checks it again with one AcoustID question and takes
/// nothing they heard; it finds nothing, so nothing is fetched and AcoustID is not asked again
/// until the next event, which asks once more.
#[tokio::test]
async fn a_recheck_that_finds_nothing_fetches_nothing_and_asks_once_per_event() {
    let rig = album_rig().await;
    let (taken, stop) = land_held(&rig.queue);
    let track = rig.song("Help! (Remastered 2009)", 139);
    rig.frozen("help-2009-120");
    let file = rig.held_file("help", "help-2009-120", "help-2009-master");
    rig.hold_track(&track, &file, &["brandon"], 1);
    let played = HeardRecord {
        heard_at: Utc::now(),
        source: HeardSource::Video {
            id: "ANNASVIDEO1".into(),
            size: Some(10),
            content_type: "audio/mp4".into(),
            itag: None,
        },
        sample_id: None,
    };

    rig.coordinator.heard_again(&track, "anna", &played);

    assert_eq!(asked_settle(&rig, 1).await, 1, "one question for the event");
    until(|| rig.coordinator.resuming.lock().is_empty()).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        rig.gate.answers.asked().len(),
        1,
        "no second question before the next event"
    );
    assert!(taken.lock().is_empty(), "nothing fetched");
    let record = rig
        .gate
        .holds
        .get(&format!("Track:soulseek:{track}:NotYetProven"))
        .expect("still held");
    assert_eq!(
        record
            .holders
            .iter()
            .map(|holder| holder.user.as_str())
            .collect::<Vec<_>>(),
        ["brandon"]
    );
    assert_eq!(
        record.holders[0].heard, None,
        "another person's play is never taken"
    );

    rig.coordinator.heard_again(&track, "carl", &played);
    assert_eq!(asked_settle(&rig, 2).await, 2, "the next event asks once more");
    stop.cancel();
}

/// A heart of a held album track's song (the person's own Retry among them) checks it again,
/// once.
#[tokio::test]
async fn a_heart_of_a_held_album_tracks_song_checks_it_again_once() {
    let rig = album_rig().await;
    let (taken, stop) = land_held(&rig.queue);
    let track = rig.song("Help! (Remastered 2009)", 139);
    rig.frozen("help-2009-120");
    let file = rig.held_file("help", "help-2009-120", "help-2009-master");
    rig.hold_track(&track, &file, &["brandon"], 1);

    let outcome = rig
        .coordinator
        .acquire_track_outcome(
            "soulseek",
            &track,
            Some("brandon"),
            None,
            &AcquisitionTarget::Shared,
            AcquisitionOrigin::Heart,
        )
        .await
        .expect("ran");

    assert_eq!(outcome, TrackOutcome::Held(WaitingFor::NotYetProven));
    assert_eq!(asked_settle(&rig, 1).await, 1, "checked again once");
    assert!(taken.lock().is_empty());
    assert_eq!(rig.held_count(), 1);
    stop.cancel();
}

/// The heart gate, noting the place in AcoustID's lane every held file's check asks from.
struct Watching {
    inner: Arc<DownloadVerificationService>,
    places: Mutex<Vec<LookupClass>>,
}

#[async_trait]
impl HeartGates for Watching {
    async fn build(&self, provider: &str, id: &str, holder: &Holder, origin: AcquisitionOrigin) -> GateStart {
        self.inner.build(provider, id, holder, origin).await
    }

    async fn build_for_track(
        &self,
        provider: &str,
        id: &str,
        holder: &Holder,
        origin: AcquisitionOrigin,
        r: &HeardRecording,
    ) -> GateStart {
        self.inner.build_for_track(provider, id, holder, origin, r).await
    }

    async fn judge_held(&self, path: &str, acquisition_key: &str, gates: &[HeartGate]) -> Vec<GateVerdict> {
        self.places
            .lock()
            .push(AcoustIdRateLimiter::floored(LookupClass::Heart));
        self.inner.judge_held(path, acquisition_key, gates).await
    }

    fn open_rounds(&self, acquisition_key: &str, gates: &[HeartGate]) -> Box<dyn Send> {
        self.inner.open_rounds(acquisition_key, gates)
    }
}

/// Checks of files kept as heard over an AcoustID that counts its lookups: one kept file of
/// "kept-song", its print kept with it.
async fn kept_checks(dir: &std::path::Path) -> (Arc<KeptRechecks>, wiremock::MockServer) {
    use crate::services::fingerprint::{AcoustIdClient, AcoustIdRateLimitHandler, AcoustIdRateLimiter};
    use crate::services::library::library_action_test_support::RecordingLibrary;
    use crate::services::local::{KeptHeart, LocalSongMapping};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let acoust_id = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(r#"{"status":"ok","results":[]}"#, "application/json"),
        )
        .mount(&acoust_id)
        .await;
    let path = dir.join("kept.m4a");
    std::fs::write(&path, b"the audio that played").expect("written");
    let settings = Arc::new(SettingsStore::for_tests(AppSettings::default()));
    let limiter = Arc::new(
        AcoustIdRateLimiter::with_gaps(Duration::from_millis(1), Duration::from_millis(1))
            .with_jitter(|| 1.0),
    );
    let client = Arc::new(AcoustIdClient::with_built_in_key(
        Arc::new(AcoustIdRateLimitHandler::with_base_url(
            limiter,
            url::Url::parse(&format!("{}/", acoust_id.uri())).expect("parses"),
        )),
        settings.clone(),
        Some("octo-key"),
    ));
    let library = Arc::new(RecordingLibrary::default());
    library.mappings.lock().push(LocalSongMapping {
        external_provider: "soulseek".into(),
        external_id: "kept-song".into(),
        local_path: path.to_string_lossy().replace('\\', "/"),
        downloaded_at: Utc::now() - TimeDelta::days(30),
        heart: Some(KeptHeart {
            users: vec!["brandon".into()],
            as_heard: true,
            print: Some("AQADtEqk".into()),
            seconds: Some(139),
            ..Default::default()
        }),
        ..Default::default()
    });
    let kept = Arc::new(KeptRechecks::new(
        library,
        client,
        settings,
        Arc::new(ExternalIdRegistry::in_memory()),
    ));
    (kept, acoust_id)
}

/// After a restart, each hold a person waits on is checked again once, one at a time, oldest
/// first, at the AcoustID lane's lowest place: a held album track is re-asked without anyone
/// playing it. A kept-as-heard file is never checked at start.
#[tokio::test]
async fn at_start_each_held_item_is_checked_once_oldest_first_at_the_lowest_place() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let (kept, kept_acoust_id) = kept_checks(dir.path()).await;
    let watching: Arc<Mutex<Option<Arc<Watching>>>> = Arc::default();
    let seen = watching.clone();
    let rig = album_rig_full(
        FakeDownloads::default(),
        &[HeartDownloadSource::Soulseek],
        move |inner| {
            let gates = Arc::new(Watching {
                inner,
                places: Mutex::new(Vec::new()),
            });
            *seen.lock() = Some(gates.clone());
            gates as Arc<dyn HeartGates>
        },
        Some(kept),
    )
    .await;
    let watching = watching.lock().clone().expect("the gates");
    let (taken, stop) = land_held(&rig.queue);
    let older = rig.song("Help! (Remastered 2009)", 139);
    let newer = rig.song("Help! (2009 Remaster)", 139);
    rig.frozen_as("help-a", "help-2009-120");
    rig.frozen_as("help-b", "help-2009-120");
    let first = rig.held_file("help", "help-a", "help-2009-master");
    let second = rig.held_file("help-again", "help-b", "help-2009-master");
    rig.hold_track(&newer, &second, &["brandon"], 1);
    rig.hold_track(&older, &first, &["brandon"], 5);

    Arc::new(SoulseekHoldResumer::new(
        rig.gate.holds.clone(),
        rig.coordinator.clone(),
    ))
    .run(CancellationToken::new())
    .await
    .expect("ran");

    assert_eq!(asked_settle(&rig, 2).await, 2, "each once");
    assert_eq!(rig.gate.answers.asked(), ["help-a", "help-b"], "oldest first");
    assert_eq!(
        watching.places.lock().as_slice(),
        [LookupClass::Bulk, LookupClass::Bulk],
        "each at the lane's lowest place"
    );
    assert!(
        rig.tracker.is_held("soulseek", "help-album"),
        "the album's row is back"
    );
    assert_eq!(rig.held_count(), 2, "nothing proved them");
    assert!(taken.lock().is_empty());
    assert_eq!(
        kept_acoust_id.received_requests().await.unwrap_or_default().len(),
        0,
        "a kept file is never checked at start"
    );
    stop.cancel();
}

/// A held item checked by any event goes to the back of the order a heart's proof checks items
/// in: a play of the older held track checks it, so the next proof checks the newer one.
#[tokio::test]
async fn a_held_track_another_event_checked_goes_to_the_back_of_the_proofs_order() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let (kept, _kept_acoust_id) = kept_checks(dir.path()).await;
    let rig = album_rig_full(
        FakeDownloads::default(),
        &[HeartDownloadSource::Soulseek],
        |service| service as Arc<dyn HeartGates>,
        Some(kept.clone()),
    )
    .await;
    let (_taken, stop) = land_held(&rig.queue);
    let older = rig.song("Help! (Remastered 2009)", 139);
    let newer = rig.song("Help! (2009 Remaster)", 139);
    rig.frozen_as("help-a", "help-2009-120");
    rig.frozen_as("help-b", "help-2009-120");
    let first = rig.held_file("help", "help-a", "help-2009-master");
    let second = rig.held_file("help-again", "help-b", "help-2009-master");
    rig.hold_track(&older, &first, &["brandon"], 50_000);
    rig.hold_track(&newer, &second, &["brandon"], 40_000);
    assert_eq!(
        rig.coordinator
            .rechecked_holds()
            .iter()
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>(),
        [
            format!("Track:soulseek:{older}:NotYetProven"),
            format!("Track:soulseek:{newer}:NotYetProven")
        ],
        "held album tracks are items a proof checks"
    );
    let played = HeardRecord {
        heard_at: Utc::now(),
        source: HeardSource::Video {
            id: "ANNASVIDEO1".into(),
            size: Some(10),
            content_type: "audio/mp4".into(),
            itag: None,
        },
        sample_id: None,
    };
    rig.coordinator.heard_again(&older, "anna", &played);
    assert_eq!(asked_settle(&rig, 1).await, 1);
    until(|| rig.coordinator.resuming.lock().is_empty()).await;

    // The kept file was checked by its own event since: the proof's item is the newer held track.
    kept.song_event("soulseek", "kept-song").await;
    kept.after_a_proof().await;

    assert_eq!(asked_settle(&rig, 2).await, 2);
    assert_eq!(rig.gate.answers.asked(), ["help-a", "help-b"]);
    stop.cancel();
}
