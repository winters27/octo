//! Find songs picks as they reach the queues: a second pick of a song waiting in the upgrade
//! queue, picks no download used, and how many looks one person may run at once.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures::FutureExt;
use octo_core::fingerprint::heart_gate::{
    AcquisitionOrigin, HeardReading, HeardRecording, HeartGate, WaitingFor,
};
use octo_core::models::download::AcquisitionCandidate;
use octo_core::settings::{
    AppSettings, LibraryAction, LibraryActionDefinition, LibraryActionSettings, LidarrSettings,
    SettingsStore, SoulseekSettings, SubsonicSettings,
};
use tokio_util::sync::CancellationToken;

use super::acquisition_tracker::{AcquisitionTracker, key_of};
use super::download_log_tests::upgrade_worker;
use super::download_picks::{AnywaySong, DownloadPicks, LIDARR_SOURCE, PickedCopy, SOULSEEK_SOURCE};
use super::heart_acquisition_coordinator::{GateStart, HeartGates};
use super::song_finder::{
    ACOUST_ID_BUSY, FindSnapshot, FindTarget, FoundCopy, LIDARR_LEFT_ALONE, LOSSY_OVER_LOSSLESS,
    MUSIC_BRAINZ_BUSY, SearchAnswer, SongFinder, SongFinderServices, find_states, library_action_states,
};
use super::soulseek_hold_store::Holder;
use super::track_acquisition_queue::{AcquisitionHeld, TrackAcquisitionQueue};
use crate::services::library::LibraryActionState;
use crate::services::library::UpgradeSources;
use crate::services::library::library_action_executor::{LibraryActionExecutor, LibraryActionOutcome};
use crate::services::library::library_action_journal::{LibraryActionEntry, LibraryActionJournal};
use crate::services::library::upgrade_queue::{UpgradeAsk, UpgradeQueue};
use crate::services::lidarr::fake_lidarr::{CatalogAlbum, FakeLidarr};
use crate::services::lidarr::{LidarrAlbumClaims, LidarrArtistMusic, LidarrClient, LidarrTrackFetcher};
use crate::services::subsonic::NavidromeIdentityService;

fn owned() -> FindTarget {
    FindTarget {
        library_id: Some("nd-1".into()),
        owned_format: Some("mp3".into()),
        ..FindTarget::new("Air", "Sexy Boy", Some("Moon Safari"), None, "nd-1")
    }
}

struct Server {
    finder: Arc<SongFinder>,
    queue: Arc<UpgradeQueue>,
    picks: Arc<DownloadPicks>,
    settings: Arc<SettingsStore>,
}

fn flac(peer: &str, rank: i32) -> FoundCopy {
    let file = r"Music\Air\Moon Safari (1998)\02 - Sexy Boy.flac";
    FoundCopy::new(
        AcquisitionCandidate {
            source: SOULSEEK_SOURCE.into(),
            peer: Some(peer.into()),
            file: Some("02 - Sexy Boy.flac".into()),
            folder: Some("Moon Safari (1998)".into()),
            format: Some("flac".into()),
            size: Some(30_000_000),
            length: Some(298),
            rank: Some(rank),
            ..Default::default()
        },
        PickedCopy {
            source: SOULSEEK_SOURCE.into(),
            peer: Some(peer.into()),
            file: Some(file.into()),
            size: 30_000_000,
            format: Some("flac".into()),
            length: Some(298),
            ..Default::default()
        },
    )
}

fn three_flacs() -> anyhow::Result<SearchAnswer> {
    Ok((
        vec![flac("peer1", 1), flac("peer2", 2), flac("peer3", 3)],
        vec!["Air Sexy Boy".to_string()],
        Some("3 files from 3 peers; 3 fit the song".to_string()),
    ))
}

fn build(target: FindTarget, acquisitions: Option<Arc<TrackAcquisitionQueue>>) -> Server {
    build_with(target, acquisitions, SongFinderServices::default())
}

/// [`build`] with `more`'s gates, journal and tracker.
fn build_with(
    target: FindTarget,
    acquisitions: Option<Arc<TrackAcquisitionQueue>>,
    more: SongFinderServices,
) -> Server {
    let settings = Arc::new(SettingsStore::for_tests(AppSettings {
        library_actions: LibraryActionSettings {
            enabled: true,
            dry_run: false,
            allowed_users: vec!["alice".into(), "bob".into()],
            actions: vec![LibraryActionDefinition {
                action: LibraryAction::BetterQuality,
                enabled: true,
                ..Default::default()
            }],
            ..Default::default()
        },
        soulseek: SoulseekSettings {
            base_url: Some("http://slskd.test".into()),
            username: Some("u".into()),
            password: Some("p".into()),
            ..Default::default()
        },
        ..Default::default()
    }));
    let queue = Arc::new(UpgradeQueue::new());
    let picks = Arc::new(DownloadPicks::default());
    let finder = Arc::new(SongFinder::new(SongFinderServices {
        settings: Some(settings.clone()),
        upgrade_queue: Some(queue.clone()),
        upgrade_sources: Some(Arc::new(UpgradeSources::new(settings.clone(), None))),
        executor: true,
        acquisitions,
        picks: Some(picks.clone()),
        gates: more.gates,
        journal: more.journal,
        tracker: more.tracker,
        lidarr_fetcher: more.lidarr_fetcher,
        lidarr_client: more.lidarr_client,
        lidarr_music: more.lidarr_music,
        ..Default::default()
    }));
    finder.seams(|seams| {
        seams.resolve = Arc::new(move |_, _| {
            let target = target.clone();
            async move { Some(target) }.boxed()
        });
        seams.sources_for = Arc::new(|_| vec![(SOULSEEK_SOURCE.to_string(), None)]);
        seams.search_soulseek = Arc::new(|_, _| async { three_flacs() }.boxed());
    });
    Server {
        finder,
        queue,
        picks,
        settings,
    }
}

async fn look(finder: &Arc<SongFinder>, user: &str) -> FindSnapshot {
    let id = finder
        .start("nd-1", user, &CancellationToken::new())
        .await
        .expect("a look")
        .id;
    finder.wait(&id, Duration::from_secs(10)).await.expect("the look")
}

fn queued_peer(queue: &UpgradeQueue) -> Option<String> {
    let jobs = queue.snapshot();
    assert_eq!(jobs.len(), 1);
    jobs[0].pick.as_ref().and_then(|p| p.peer.clone())
}

// ---- A second pick ----------------------------------------------------------------------

#[tokio::test]
async fn a_second_pick_before_the_upgrade_starts_takes_the_place_of_the_first() {
    let server = build(owned(), None);
    let look = look(&server.finder, "alice").await;

    assert_eq!(
        server
            .finder
            .pick_by_id(&look.id, &look.copies[0].id, "alice")
            .await
            .state,
        library_action_states::QUEUED
    );
    assert_eq!(queued_peer(&server.queue).as_deref(), Some("peer1"));

    let second = server
        .finder
        .pick_by_id(&look.id, &look.copies[1].id, "alice")
        .await;

    assert_eq!(second.state, library_action_states::QUEUED);
    assert!(
        second.detail.starts_with("Changed to the copy you picked"),
        "{}",
        second.detail
    );
    assert_eq!(queued_peer(&server.queue).as_deref(), Some("peer2"));
}

#[tokio::test]
async fn a_pick_for_an_upgrade_already_running_or_someone_elses_says_another_copy_is_on_its_way() {
    let server = build(owned(), None);
    let alice = look(&server.finder, "alice").await;
    server
        .finder
        .pick_by_id(&alice.id, &alice.copies[0].id, "alice")
        .await;

    let bobs = look(&server.finder, "bob").await;
    let bob = server
        .finder
        .pick_by_id(&bobs.id, &bobs.copies[2].id, "bob")
        .await;
    assert_eq!(bob.state, library_action_states::SKIPPED);
    assert_eq!(
        bob.detail,
        "Another copy of this song is already on its way. Pick again once it ends."
    );

    assert!(server.queue.take_next().is_some());
    let late = server
        .finder
        .pick_by_id(&alice.id, &alice.copies[1].id, "alice")
        .await;
    assert_eq!(late.state, library_action_states::SKIPPED);
    assert_eq!(queued_peer(&server.queue).as_deref(), Some("peer1"));

    // The same copy again is the pick already waiting, not another one.
    assert_eq!(
        server
            .finder
            .pick_by_id(&alice.id, &alice.copies[0].id, "alice")
            .await
            .state,
        library_action_states::QUEUED
    );
}

// ---- Picks no download used ---------------------------------------------------------------

#[tokio::test]
async fn a_pick_is_let_go_when_its_download_ends_without_using_it() {
    let target = FindTarget {
        external_id: Some("ext1".into()),
        ..FindTarget::new("Air", "Sexy Boy", Some("Moon Safari"), Some(298), "ext1")
    };
    let acquisitions = Arc::new(TrackAcquisitionQueue::new());
    let server = build(target, Some(acquisitions.clone()));
    let look = look(&server.finder, "alice").await;

    assert_eq!(
        server
            .finder
            .pick_by_id(&look.id, &look.copies[0].id, "alice")
            .await
            .state,
        library_action_states::QUEUED
    );
    assert!(server.picks.has("ext1"));

    // The download ends at once, "already in your library", and never takes the pick.
    let request = acquisitions
        .dequeue(&CancellationToken::new())
        .await
        .expect("the request");
    acquisitions.release(&request);
    request
        .completion
        .try_set_result("/music/Air/Moon Safari/02 - Sexy Boy.flac");
    for _ in 0..200 {
        if !server.picks.has("ext1") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    assert!(!server.picks.has("ext1"));
}

#[test]
fn forgetting_a_pick_leaves_a_newer_one_for_the_same_song() {
    let picks = DownloadPicks::default();
    let copy = |peer: &str| {
        Arc::new(PickedCopy {
            source: SOULSEEK_SOURCE.into(),
            peer: Some(peer.into()),
            file: Some("a.flac".into()),
            ..Default::default()
        })
    };
    let (first, second) = (copy("p1"), copy("p2"));
    picks.pin("ext1", first.clone());
    picks.pin("ext1", second);

    assert!(!picks.forget("ext1", &first));
    assert_eq!(
        picks
            .take("ext1", "Soulseek")
            .and_then(|p| p.peer.clone())
            .as_deref(),
        Some("p2")
    );
}

#[tokio::test]
async fn an_upgrade_lets_go_of_a_pick_it_never_used() {
    let queue = Arc::new(UpgradeQueue::new());
    let picks = Arc::new(DownloadPicks::default());
    queue.add(
        vec![UpgradeAsk {
            title: Some("Sexy Boy".into()),
            artist: Some("Air".into()),
            album: Some("Moon Safari".into()),
            suffix: Some("mp3".into()),
            pick: Some((*flac("peer1", 1).pick).clone()),
            ..UpgradeAsk::new("nd-1")
        }],
        "alice",
        "find",
    );
    let worker = upgrade_worker(&queue, None, picks.clone(), |request| {
        (request.on_replacement_queued.as_ref().expect("a follower"))("soulseek", "ext1");
        // Joined a download already running, so this pick was never taken.
        LibraryActionOutcome::new(LibraryActionState::Failed, Some("joined".into()))
    });

    worker.tick(&CancellationToken::new()).await;
    worker.drain().await;

    assert!(!picks.has("ext1"));
}

// ---- Replacing anyway ------------------------------------------------------------------------

fn owned_flac() -> FindTarget {
    FindTarget {
        library_id: Some("nd-1".into()),
        owned_format: Some("flac".into()),
        owned_quality: Some("FLAC 16-bit 44.1 kHz".into()),
        owned_path: Some("/music/Air/Moon Safari/02 - Sexy Boy.flac".into()),
        ..FindTarget::new("Air", "Sexy Boy", Some("Moon Safari"), Some(298), "nd-1")
    }
}

const SAME_QUALITY: &str = "This copy is FLAC like yours, so it is not better. Replace yours anyway?";

#[tokio::test]
async fn an_app_that_cannot_ask_hears_the_old_refusal_and_one_that_can_is_asked_first() {
    let server = build(owned_flac(), None);
    let look = look(&server.finder, "alice").await;
    let copy = &look.copies[0].id;

    let old = server.finder.pick_by_id(&look.id, copy, "alice").await;
    assert_eq!(old.state, library_action_states::SKIPPED);
    assert_eq!(
        old.detail,
        "Your copy is already lossless, so nothing would be better."
    );

    let asked = server
        .finder
        .pick_copy(&look.id, copy, "alice", Some(false))
        .await;
    assert_eq!(asked.state, library_action_states::CONFIRM);
    assert_eq!(asked.detail, SAME_QUALITY);
    assert!(
        server.queue.snapshot().is_empty(),
        "nothing is queued before the person says yes"
    );

    // Picking by place asks the same way.
    let by_place = server.finder.pick_place(&look.id, 0, "alice", Some(false)).await;
    assert_eq!(by_place.state, library_action_states::CONFIRM);
}

#[tokio::test]
async fn replace_anyway_queues_the_copy_held_to_the_library_song_with_no_quality_check() {
    let server = build(owned_flac(), None);
    let look = look(&server.finder, "alice").await;

    let picked = server
        .finder
        .pick_copy(&look.id, &look.copies[1].id, "alice", Some(true))
        .await;

    assert_eq!(picked.state, library_action_states::QUEUED, "{}", picked.detail);
    let jobs = server.queue.snapshot();
    assert_eq!(jobs.len(), 1);
    let pick = jobs[0].pick.clone().expect("the pick");
    assert_eq!(pick.peer.as_deref(), Some("peer2"));
    assert_eq!(
        pick.anyway,
        Some(AnywaySong {
            title: "Sexy Boy".into(),
            album: Some("Moon Safari".into()),
            length: Some(298),
            retitle: false,
        })
    );
}

#[tokio::test]
async fn a_better_copy_asked_about_is_ready_and_then_an_ordinary_pick() {
    let server = build(owned(), None);
    let look = look(&server.finder, "alice").await;

    let asked = server
        .finder
        .pick_copy(&look.id, &look.copies[0].id, "alice", Some(false))
        .await;
    assert_eq!(asked.state, library_action_states::READY);
    assert!(server.queue.snapshot().is_empty(), "asking fetches nothing");
    let compare = asked.compare.expect("the two copies");
    assert_eq!(compare.yours.quality.as_deref(), Some("MP3"));
    assert_eq!(compare.copy.quality.as_deref(), Some("FLAC"));
    assert_eq!(compare.copy.length, Some(298));
    assert_eq!(compare.copy.take, None);

    let picked = server
        .finder
        .pick_copy(&look.id, &look.copies[0].id, "alice", Some(true))
        .await;
    assert_eq!(picked.state, library_action_states::QUEUED);
    let jobs = server.queue.snapshot();
    assert_eq!(jobs[0].pick.as_ref().and_then(|p| p.anyway.clone()), None);
}

#[tokio::test]
async fn a_live_take_in_the_library_is_named_on_the_look_and_in_the_question() {
    let target = FindTarget {
        library_id: Some("nd-1".into()),
        owned_format: Some("flac".into()),
        owned_quality: Some("FLAC 16-bit 44.1 kHz".into()),
        owned_path: Some("/music/Air/Live at the Olympia/05 - Sexy Boy (Live).flac".into()),
        ..FindTarget::new(
            "Air",
            "Sexy Boy (Live)",
            Some("Live at the Olympia"),
            Some(402),
            "nd-1",
        )
    };
    let server = build(target, None);
    let look = look(&server.finder, "alice").await;
    assert_eq!(look.take, Some(super::replace_anyway::OtherTake::Live));

    let asked = server
        .finder
        .pick_copy(&look.id, &look.copies[0].id, "alice", Some(false))
        .await;
    assert_eq!(asked.state, library_action_states::CONFIRM);
    let compare = asked.compare.expect("the two copies");
    assert_eq!(compare.yours.take.as_deref(), Some("live recording"));
    assert_eq!(compare.yours.length, Some(402));
    assert_eq!(compare.copy.take.as_deref(), Some("the plain song"));

    let replaced = server
        .finder
        .pick_copy(&look.id, &look.copies[0].id, "alice", Some(true))
        .await;
    assert_eq!(replaced.state, library_action_states::QUEUED);
    let song = server.queue.snapshot()[0]
        .pick
        .as_ref()
        .and_then(|p| p.anyway.clone())
        .expect("forced");
    assert!(song.retitle);
    assert_eq!(song.title, "Sexy Boy");
}

#[tokio::test]
async fn the_wrong_song_is_refused_even_with_anyway() {
    let server = build(owned_flac(), None);
    server.finder.seams(|seams| {
        seams.search_soulseek = Arc::new(|_, _| {
            async {
                let mut other = flac("peer1", 1);
                let mut pick = (*other.pick).clone();
                pick.file = Some(r"Music\Air\Moon Safari (1998)\04 - Kelly Watch the Stars.flac".into());
                other.pick = Arc::new(pick);
                Ok((vec![other], Vec::new(), None))
            }
            .boxed()
        });
    });
    let look = look(&server.finder, "alice").await;

    for anyway in [Some(false), Some(true)] {
        let picked = server
            .finder
            .pick_copy(&look.id, &look.copies[0].id, "alice", anyway)
            .await;
        assert_eq!(picked.state, library_action_states::SKIPPED);
        assert_eq!(
            picked.detail,
            "The file name does not match the title, so it cannot take the place of the one in your library."
        );
    }
    assert!(server.queue.snapshot().is_empty());
}

#[tokio::test]
async fn someone_who_may_no_longer_replace_songs_is_told_so_and_never_asked() {
    let server = build(owned_flac(), None);
    let look = look(&server.finder, "alice").await;
    let mut changed = (*server.settings.current()).clone();
    changed.library_actions.allowed_users = vec!["bob".into()];
    server.settings.set(changed);

    for anyway in [Some(false), Some(true)] {
        let picked = server
            .finder
            .pick_copy(&look.id, &look.copies[0].id, "alice", anyway)
            .await;
        assert_eq!(picked.state, library_action_states::SKIPPED);
        assert_eq!(picked.detail, "alice is not on the library actions allowed list.");
    }
    assert!(server.queue.snapshot().is_empty());
}

#[tokio::test]
async fn the_upgrade_worker_hands_the_anyway_song_to_the_replacement() {
    let queue = Arc::new(UpgradeQueue::new());
    let picks = Arc::new(DownloadPicks::default());
    let song = AnywaySong {
        title: "Sexy Boy".into(),
        album: None,
        length: Some(298),
        retitle: true,
    };
    queue.add(
        vec![UpgradeAsk {
            title: Some("Sexy Boy (Live)".into()),
            artist: Some("Air".into()),
            suffix: Some("flac".into()),
            pick: Some(PickedCopy {
                anyway: Some(song.clone()),
                ..(*flac("peer1", 1).pick).clone()
            }),
            ..UpgradeAsk::new("nd-1")
        }],
        "alice",
        "find",
    );
    let heard = Arc::new(parking_lot::Mutex::new(None));
    let seen = heard.clone();
    let worker = upgrade_worker(&queue, None, picks, move |request| {
        *seen.lock() = Some((request.anyway.clone(), request.only_source));
        LibraryActionOutcome::new(LibraryActionState::Failed, Some("not this time".into()))
    });

    worker.tick(&CancellationToken::new()).await;
    worker.drain().await;

    assert_eq!(
        heard.lock().clone(),
        Some((Some(song), Some(octo_core::settings::DownloadSource::Soulseek)))
    );
}

#[test]
fn a_pick_with_no_anyway_song_is_written_as_before() {
    let plain = serde_json::to_string(&*flac("peer1", 1).pick).expect("json");
    assert!(!plain.contains("Anyway"), "{plain}");
    let forced = PickedCopy {
        anyway: Some(AnywaySong {
            title: "Sexy Boy".into(),
            album: None,
            length: Some(298),
            retitle: false,
        }),
        ..(*flac("peer1", 1).pick).clone()
    };
    let text = serde_json::to_string(&forced).expect("json");
    assert!(
        text.contains(r#""Anyway":{"Title":"Sexy Boy","Album":null,"Length":298}"#),
        "{text}"
    );
    let back: PickedCopy = serde_json::from_str(&text).expect("read back");
    assert_eq!(back, forced);
}

// ---- How many looks run at once --------------------------------------------------------------

#[tokio::test]
async fn one_person_runs_two_looks_at_once_and_another_persons_look_is_never_dropped_for_room() {
    let slow_song = FindTarget {
        external_id: Some("ext1".into()),
        ..FindTarget::new("Air", "Sexy Boy", Some("Moon Safari"), Some(298), "ext1")
    };
    let quick_song = FindTarget {
        title: "Kelly Watch the Stars".into(),
        external_id: Some("ext2".into()),
        ..slow_song.clone()
    };
    let server = build(slow_song.clone(), None);
    let (release, released) = tokio::sync::watch::channel(false);
    let slow_title = slow_song.title.clone();
    server.finder.seams(|seams| {
        // Looks for "slow" wait; every other one ends at once.
        seams.resolve = Arc::new(move |id, _| {
            let found = if id == "slow" {
                slow_song.clone()
            } else {
                quick_song.clone()
            };
            async move { Some(found) }.boxed()
        });
        seams.search_soulseek = Arc::new(move |song, _| {
            let slow = song.title == slow_title;
            let mut released = released.clone();
            async move {
                if slow {
                    let _ = released.wait_for(|done| *done).await;
                }
                Ok((Vec::new(), Vec::new(), Some("Nothing found".to_string())))
            }
            .boxed()
        });
    });
    let ct = CancellationToken::new();

    let bobs = server.finder.start("slow", "bob", &ct).await.expect("bob's look");
    for _ in 0..2 {
        assert_eq!(
            server
                .finder
                .start("slow", "alice", &ct)
                .await
                .expect("a look")
                .state,
            find_states::SEARCHING
        );
    }
    let third = server.finder.start("slow", "alice", &ct).await.expect("a look");

    assert_eq!(third.state, find_states::FAILED);
    assert_eq!(
        third.error.as_deref(),
        Some("You already have 2 searches running. Search again once one of them ends.")
    );

    // Many finished looks later, bob's look, still running, is still there to follow.
    for i in 0..SongFinder::CAPACITY + 5 {
        let id = server
            .finder
            .start("quick", &format!("user{i}"), &ct)
            .await
            .expect("a look")
            .id;
        server.finder.wait(&id, Duration::from_secs(10)).await;
    }

    assert_eq!(
        server.finder.get(&bobs.id, "bob").expect("bob's look").state,
        find_states::SEARCHING
    );
    release.send(true).expect("released");
}

// ---- Lidarr left alone for an artist with music in it ----------------------------------------

/// Lidarr's choice of the album, as its search lists it.
fn lidarrs_choice() -> FoundCopy {
    FoundCopy::new(
        AcquisitionCandidate {
            source: LIDARR_SOURCE.into(),
            file: Some("Air - Moon Safari (1998)".into()),
            rank: Some(1),
            ..Default::default()
        },
        PickedCopy {
            source: LIDARR_SOURCE.into(),
            ..Default::default()
        },
    )
}

/// A look over Soulseek and Lidarr, whether Lidarr is left alone read from the fake Lidarr; how
/// many times Lidarr was searched.
fn both_sources(server: &Server) -> Arc<AtomicUsize> {
    let searched = Arc::new(AtomicUsize::new(0));
    let counted = searched.clone();
    server.finder.seams(|seams| {
        seams.sources_for = Arc::new(|_| {
            vec![
                (SOULSEEK_SOURCE.to_string(), None),
                (LIDARR_SOURCE.to_string(), None),
            ]
        });
        seams.search_lidarr = Arc::new(move |_, _| {
            counted.fetch_add(1, Ordering::SeqCst);
            async {
                Ok((
                    vec![lidarrs_choice()],
                    vec!["Air - Moon Safari".to_string()],
                    None,
                ))
            }
            .boxed()
        });
    });
    searched
}

/// Lidarr's side of a look, over the fake Lidarr: Air's Moon Safari is in Lidarr's metadata, and
/// Octo's library is where Lidarr's root folder lies.
async fn lidarr_for_air() -> (
    tempfile::TempDir,
    FakeLidarr,
    wiremock::MockServer,
    SongFinderServices,
) {
    let root = tempfile::tempdir().expect("a temp dir");
    let (fake, server) = FakeLidarr::start(root.path()).await;
    fake.catalog_artist("air", "Air", 20);
    fake.catalog_album(CatalogAlbum::new(
        "rg-moon-safari",
        "Moon Safari",
        "air",
        21,
        &[(210, "La Femme d'Argent", 1), (212, "Sexy Boy", 2)],
    ));
    let settings = Arc::new(SettingsStore::for_tests(AppSettings {
        lidarr: LidarrSettings {
            base_url: Some(server.uri()),
            api_key: Some("k".into()),
            root_folder_path: Some("/data/music".into()),
            quality_profile_id: 1,
            metadata_profile_id: 1,
            ..Default::default()
        },
        subsonic: SubsonicSettings {
            auto_detect_download_path: false,
            ..Default::default()
        },
        ..Default::default()
    }));
    settings.set_raw("Library:DownloadPath", Some(&root.path().to_string_lossy()));
    let identity = NavidromeIdentityService::new(settings.clone(), reqwest::Client::new());
    let client = Arc::new(LidarrClient::new(settings.clone()));
    let fetcher = Arc::new(LidarrTrackFetcher::new(
        client.clone(),
        settings.clone(),
        identity.clone(),
        Arc::new(LidarrAlbumClaims::new()),
    ));
    let services = SongFinderServices {
        lidarr_fetcher: Some(fetcher),
        lidarr_client: Some(client.clone()),
        lidarr_music: Some(LidarrArtistMusic::new(client, settings, identity)),
        ..Default::default()
    };
    (root, fake, server, services)
}

/// Air's own album in the user's Lidarr, with a track file.
fn air_has_music(lidarr: &FakeLidarr) {
    lidarr.user_album("rg-moon-safari", false);
    lidarr.import(210, "La Femme d'Argent", "FLAC");
}

/// An artist with music in Lidarr gets no Lidarr copy on the list, and Lidarr is not searched;
/// the list says why, and Soulseek's copies are there as ever.
#[tokio::test]
async fn no_lidarr_copy_is_listed_for_an_artist_with_music_in_lidarr() {
    let (_root, lidarr, _lidarr_server, services) = lidarr_for_air().await;
    air_has_music(&lidarr);
    let server = build_with(owned(), None, services);
    let searched = both_sources(&server);

    let look = look(&server.finder, "alice").await;

    assert!(
        look.copies
            .iter()
            .all(|copy| copy.shown.source == SOULSEEK_SOURCE),
        "{:?}",
        look.copies
    );
    assert_eq!(look.copies.len(), 3);
    let lidarr = look
        .sources
        .iter()
        .find(|source| source.name == LIDARR_SOURCE)
        .expect("Lidarr's row");
    assert_eq!(lidarr.text.as_deref(), Some(LIDARR_LEFT_ALONE));
    assert_eq!(searched.load(Ordering::SeqCst), 0, "Lidarr is not searched");
}

/// A Lidarr copy listed before its artist gained music in Lidarr is refused when picked, with the
/// line; the other copies stay on the list and can still be picked.
#[tokio::test]
async fn a_lidarr_pick_listed_before_the_artist_had_music_in_lidarr_is_refused_with_the_line() {
    let (_root, lidarr, _lidarr_server, services) = lidarr_for_air().await;
    let server = build_with(owned(), None, services);
    both_sources(&server);
    let look = look(&server.finder, "alice").await;
    let listed = look
        .copies
        .iter()
        .find(|copy| copy.shown.source == LIDARR_SOURCE)
        .expect("Lidarr's choice is listed")
        .id
        .clone();

    air_has_music(&lidarr);
    let refused = server.finder.pick_by_id(&look.id, &listed, "alice").await;

    assert_eq!(refused.state, library_action_states::SKIPPED);
    assert_eq!(refused.detail, LIDARR_LEFT_ALONE);
    assert!(server.queue.snapshot().is_empty(), "nothing queued");
    let still = server.finder.get(&look.id, "alice").expect("the look");
    assert_eq!(still.copies.len(), look.copies.len(), "the list stays");
    let soulseek = still
        .copies
        .iter()
        .find(|copy| copy.shown.source == SOULSEEK_SOURCE)
        .expect("a Soulseek copy");
    assert_eq!(
        server
            .finder
            .pick_by_id(&look.id, &soulseek.id, "alice")
            .await
            .state,
        library_action_states::QUEUED
    );
}

// ---- A pick of a song not in the library -------------------------------------------------------

/// Gates for picks: R is the song's 2009 recording; held for `hold` when set. Each origin built
/// is noted.
#[derive(Default)]
struct PickGates {
    hold: Option<WaitingFor>,
    built: parking_lot::Mutex<Vec<AcquisitionOrigin>>,
}

fn pick_r() -> HeardRecording {
    HeardRecording {
        ids: vec!["rec-sexy-boy".into()],
        title: "Sexy Boy".into(),
        artist: "Air".into(),
        ..Default::default()
    }
}

#[async_trait::async_trait]
impl HeartGates for PickGates {
    async fn build(&self, _: &str, _: &str, holder: &Holder, origin: AcquisitionOrigin) -> GateStart {
        self.built.lock().push(origin);
        let gate = HeartGate {
            user: holder.user.clone(),
            origin,
            r: Some(pick_r()),
            heard: None,
            heard_seconds: None,
            reading: HeardReading::NotHeard,
            stand_in: None,
        };
        match self.hold {
            Some(waiting_for) => GateStart::Hold {
                gate,
                waiting_for,
                line: "Waiting".into(),
                question: None,
            },
            None => GateStart::Ready(gate),
        }
    }
}

fn outside() -> FindTarget {
    FindTarget {
        external_id: Some("ext1".into()),
        ..FindTarget::new("Air", "Sexy Boy", Some("Moon Safari"), Some(298), "ext1")
    }
}

/// A pick runs at the person's pace as a Find songs pick, its file held to the recording the song
/// is by the import rule. A file the check refuses ends the pick's row with the check's reason; a
/// file AcoustID did not answer for is thrown away and ends the row with "AcoustID is busy, try
/// again", and nothing of it is kept.
#[tokio::test]
async fn a_pick_is_held_to_its_songs_recording_and_its_row_ends_with_the_checks_words() {
    let acquisitions = Arc::new(TrackAcquisitionQueue::new());
    let tracker = Arc::new(AcquisitionTracker::new(None, octo_core::common::Clock::system()));
    let gates = Arc::new(PickGates::default());
    let server = build_with(
        outside(),
        Some(acquisitions.clone()),
        SongFinderServices {
            gates: Some(gates.clone()),
            tracker: Some(tracker.clone()),
            ..Default::default()
        },
    );
    let look = look(&server.finder, "alice").await;
    let error_of = || {
        tracker
            .detail(&key_of("soulseek", "ext1"), None)
            .and_then(|row| row.error)
    };

    // AcoustID's reading names another recording.
    let picked = server
        .finder
        .pick_by_id(&look.id, &look.copies[0].id, "alice")
        .await;
    assert_eq!(picked.state, library_action_states::QUEUED);
    let request = acquisitions
        .dequeue(&CancellationToken::new())
        .await
        .expect("the request");
    assert_eq!(request.origin, Some(AcquisitionOrigin::SongFinderPick));
    assert_eq!(gates.built.lock().as_slice(), [AcquisitionOrigin::SongFinderPick]);
    let held_to: Vec<Option<HeardRecording>> = request.gates().into_iter().map(|gate| gate.r).collect();
    assert_eq!(held_to, [Some(pick_r())]);
    acquisitions.release(&request);
    let refused = "No copy that fits was found: the file is another recording by AcoustID's reading";
    request.completion.try_set_error(anyhow::anyhow!(refused));
    for _ in 0..200 {
        if error_of().is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(error_of().as_deref(), Some(refused));

    // AcoustID busy for the file.
    let picked = server
        .finder
        .pick_by_id(&look.id, &look.copies[1].id, "alice")
        .await;
    assert_eq!(picked.state, library_action_states::QUEUED);
    let request = acquisitions
        .dequeue(&CancellationToken::new())
        .await
        .expect("the request");
    let dir = tempfile::tempdir().expect("a temp dir");
    let held = dir.path().join("held.flac");
    std::fs::write(&held, b"a file nobody could check").expect("written");
    acquisitions.release(&request);
    request
        .completion
        .try_set_error(anyhow::Error::new(AcquisitionHeld {
            waiting_for: WaitingFor::AcoustId,
            line: ACOUST_ID_BUSY.into(),
            files: vec![held.to_string_lossy().into_owned()],
            lidarr: Vec::new(),
            users: Vec::new(),
        }));
    for _ in 0..200 {
        if error_of().as_deref() == Some(ACOUST_ID_BUSY) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(error_of().as_deref(), Some(ACOUST_ID_BUSY));
    assert!(!held.exists(), "nothing of it is kept");
}

/// A pick whose song MusicBrainz does not answer for is never held: nothing is queued, no row
/// opens, and the person hears "MusicBrainz is busy, try again".
#[tokio::test]
async fn a_pick_with_musicbrainz_busy_is_refused_at_once_and_queues_nothing() {
    let acquisitions = Arc::new(TrackAcquisitionQueue::new());
    let tracker = Arc::new(AcquisitionTracker::new(None, octo_core::common::Clock::system()));
    let server = build_with(
        outside(),
        Some(acquisitions.clone()),
        SongFinderServices {
            gates: Some(Arc::new(PickGates {
                hold: Some(WaitingFor::MusicBrainz),
                ..Default::default()
            })),
            tracker: Some(tracker.clone()),
            ..Default::default()
        },
    );
    let look = look(&server.finder, "alice").await;

    let picked = server
        .finder
        .pick_by_id(&look.id, &look.copies[0].id, "alice")
        .await;

    assert_eq!(picked.state, library_action_states::SKIPPED);
    assert_eq!(picked.detail, MUSIC_BRAINZ_BUSY);
    assert!(acquisitions.is_idle(), "nothing queued");
    assert!(
        tracker.detail(&key_of("soulseek", "ext1"), None).is_none(),
        "no row"
    );
    assert!(!server.picks.has("ext1"));
}

// ---- A song marked "wrong version asked" -----------------------------------------------------

fn mp3(peer: &str, rank: i32) -> FoundCopy {
    let copy = flac(peer, rank);
    FoundCopy::new(
        AcquisitionCandidate {
            file: Some("02 - Sexy Boy.mp3".into()),
            format: Some("mp3".into()),
            size: Some(9_000_000),
            ..copy.shown
        },
        PickedCopy {
            file: Some(r"Music\Air\Moon Safari (1998)\02 - Sexy Boy.mp3".into()),
            format: Some("mp3".into()),
            size: 9_000_000,
            ..(*copy.pick).clone()
        },
    )
}

/// A journal in which Octo answered alice's Wrong version on nd-1 by sending her to Find songs.
fn marked() -> Arc<LibraryActionJournal> {
    let journal = Arc::new(LibraryActionJournal::new());
    journal.record(LibraryActionEntry {
        key: LibraryActionJournal::make_key(LibraryAction::WrongVersion, "nd-1", "1:2"),
        action: LibraryAction::WrongVersion,
        navidrome_id: "nd-1".into(),
        username: "alice".into(),
        title: "Sexy Boy".into(),
        artist: "Air".into(),
        album: "Moon Safari".into(),
        source_path: None,
        quarantine_path: None,
        resolution: None,
        state: LibraryActionState::Skipped,
        detail: Some(LibraryActionExecutor::WRONG_VERSION_FIND_SONGS.into()),
        dry_run: false,
        at_utc: chrono::Utc::now(),
        history_kept: None,
        revealed_path: None,
        copy: false,
        download_allowed: false,
        denied_source: None,
    });
    journal
}

/// A pick for a song its person said is the wrong version, which Octo could not tell apart by
/// sound, replaces it as Wrong version does: no lossless-only rule, so a lossy copy over the
/// lossless file is asked about first and goes ahead after `anyway`, run by the upgrade worker as
/// that Wrong version's pick. An app that cannot ask hears the question as the refusal.
#[tokio::test]
async fn a_pick_for_a_marked_song_replaces_it_as_wrong_version_asking_first_over_lossless() {
    let server = build_with(
        owned_flac(),
        None,
        SongFinderServices {
            journal: Some(marked()),
            ..Default::default()
        },
    );
    server.finder.seams(|seams| {
        seams.search_soulseek = Arc::new(|_, _| {
            async { Ok((vec![mp3("peer1", 1)], vec!["Air Sexy Boy".to_string()], None)) }.boxed()
        })
    });
    let look = look(&server.finder, "alice").await;
    let copy = &look.copies[0].id;

    let old = server.finder.pick_by_id(&look.id, copy, "alice").await;
    assert_eq!(old.state, library_action_states::SKIPPED);
    assert_eq!(old.detail, LOSSY_OVER_LOSSLESS);
    let asked = server
        .finder
        .pick_copy(&look.id, copy, "alice", Some(false))
        .await;
    assert_eq!(asked.state, library_action_states::CONFIRM);
    assert_eq!(asked.detail, LOSSY_OVER_LOSSLESS);
    assert!(server.queue.snapshot().is_empty(), "nothing queued yet");

    let anyway = server.finder.pick_copy(&look.id, copy, "alice", Some(true)).await;
    assert_eq!(anyway.state, library_action_states::QUEUED);
    let jobs = server.queue.snapshot();
    assert!(
        jobs[0].pick.as_ref().is_some_and(|pick| pick.wrong_version),
        "{jobs:?}"
    );

    let ran = Arc::new(parking_lot::Mutex::new(None));
    let seen = ran.clone();
    let worker = upgrade_worker(&server.queue, None, server.picks.clone(), move |request| {
        *seen.lock() = Some((request.action, request.picked, request.only_source));
        LibraryActionOutcome::new(LibraryActionState::Failed, Some("not this time".into()))
    });
    worker.tick(&CancellationToken::new()).await;
    worker.drain().await;
    assert_eq!(
        ran.lock().clone(),
        Some((
            LibraryAction::WrongVersion,
            true,
            Some(octo_core::settings::DownloadSource::Soulseek)
        ))
    );
}

#[tokio::test]
async fn a_library_songs_look_is_only_for_the_people_who_may_replace_it() {
    let server = build(owned(), None);
    let ct = CancellationToken::new();

    let carol = server.finder.start("nd-1", "carol", &ct).await.expect("a look");

    assert_eq!(carol.state, find_states::FAILED);
    assert_eq!(
        carol.error.as_deref(),
        Some("carol is not on the library actions allowed list.")
    );
    assert_eq!(
        server
            .finder
            .start("nd-1", "alice", &ct)
            .await
            .expect("a look")
            .state,
        find_states::SEARCHING
    );
}
