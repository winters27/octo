//! Hearts and plays for family members: whose library a song counts as already in, where a
//! member's chain sends the file, and which plays keep a copy.

use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use octo_core::common::Clock;
use octo_core::family::abilities::Preset;
use octo_core::models::domain::{Album, Song};
use octo_core::settings::{HeartDownloadSource, HeartDownloadStep};
use serde_json::json;

use super::*;
use crate::services::common::acquisition_tracker::AcquisitionTracker;
use crate::services::common::test_fakes::{FakeDownloads, FakeLidarr, FakeMetadata};
use crate::services::common::{AcquisitionPlace, AcquisitionRequest};
use crate::services::family::library_index::ScopeResolver;
use crate::services::family::request_service::{LiveRequestFetcher, RequestFetcher};
use crate::services::family::test_support::{
    World, eventually, member, navidrome_with_songs, song_row, write_file,
};
use crate::services::family::{LibraryWriteGate, PersonalLibrary};
use crate::services::library::LibraryOwnership;
use crate::services::library::UpgradeQueue;
use crate::services::library::library_ownership::{Candidate, search_from};

fn step(source: HeartDownloadSource) -> HeartDownloadStep {
    HeartDownloadStep {
        source,
        song_enabled: Some(true),
        album_enabled: Some(true),
        ..Default::default()
    }
}

/// The world's settings with these heart sources, download on play, and the owned check on.
fn configure(world: &World, sources: &[HeartDownloadSource], download_on_play: bool) {
    let mut settings = world.settings.current().as_ref().clone();
    settings.subsonic.heart_download_sources = sources.iter().map(|s| step(*s)).collect();
    settings.subsonic.download_on_play = download_on_play;
    settings.subsonic.skip_owned_songs = true;
    world.settings.set(settings);
}

struct Chain {
    world: World,
    metadata: Arc<FakeMetadata>,
    queue: Arc<TrackAcquisitionQueue>,
    lidarr: Arc<FakeLidarr>,
    coordinator: Arc<HeartAcquisitionCoordinator>,
}

/// A coordinator over the world. The library holds "Song 1" by Artist only in Alex's own
/// library (the file `<members>/alex/Artist/Album/1.flac`).
fn chain(world: World) -> Chain {
    let metadata = Arc::new(FakeMetadata::default());
    metadata.songs.lock().insert(
        ("test".into(), "1".into()),
        Song {
            title: "Song 1".into(),
            artist: "Artist".into(),
            album: "Album".into(),
            duration: Some(200),
            ..Default::default()
        },
    );
    let alex_copy = format!("{}/alex/Artist/Album/1.flac", world.members());
    write_file(&alex_copy, 10);
    let ownership = LibraryOwnership::new(world.settings.clone(), None, world.library.clone());
    ownership.set_search(search_from(Some(vec![Candidate {
        id: "alex-1".into(),
        artist: "Artist".into(),
        title: "Song 1".into(),
        album: Some("Album".into()),
        duration: Some(200),
        suffix: "flac".into(),
        bit_rate: 900,
        isrcs: Vec::new(),
    }])));
    ownership.set_resolve(Arc::new(move |_| {
        let path = alex_copy.clone();
        async move { Ok(Some(path)) }.boxed()
    }));
    ownership.set_scope(ScopeResolver::new(
        world.settings.clone(),
        world.family.clone(),
        world.index.clone(),
    ));
    let owned = Arc::new(HeartOwnership::new(
        Arc::new(ownership),
        metadata.clone(),
        world.settings.clone(),
        None,
        None,
    ));
    let queue = Arc::new(TrackAcquisitionQueue::new());
    let lidarr = Arc::new(FakeLidarr::default().tracks(|_, _, _| Ok(true)));
    let coordinator = HeartAcquisitionCoordinator::new(
        world.settings.clone(),
        queue.clone(),
        Arc::new(FakeDownloads::default()),
        lidarr.clone(),
        HeartCoordinatorExtras {
            owned: Some(owned),
            ..Default::default()
        },
    );
    let personal = Arc::new(PersonalLibrary::new(
        world.settings.clone(),
        world.family.clone(),
        world.index.clone(),
        world.pool.clone(),
    ));
    coordinator.use_family(FamilyHooks {
        gate: Arc::new(LibraryWriteGate::new(
            world.settings.clone(),
            world.family.clone(),
            personal,
        )),
        pool: world.pool.clone(),
        metadata: metadata.clone(),
    });
    Chain {
        world,
        metadata,
        queue,
        lidarr,
        coordinator,
    }
}

/// The next queued request, or `None` within 300 ms.
async fn next_queued(queue: &TrackAcquisitionQueue) -> Option<Arc<AcquisitionRequest>> {
    tokio::time::timeout(
        Duration::from_millis(300),
        queue.dequeue(&CancellationToken::new()),
    )
    .await
    .ok()
    .flatten()
}

#[tokio::test]
async fn owner_heart_downloads_even_if_a_member_owns_it() {
    let world = World::new(None);
    configure(&world, &[HeartDownloadSource::YouTube], false);
    let chain = chain(world);
    let coordinator = chain.coordinator.clone();
    let running = tokio::spawn(async move {
        coordinator
            .acquire_track_outcome(
                "test",
                "1",
                Some("winters"),
                None,
                &AcquisitionTarget::Shared,
                AcquisitionOrigin::Heart,
            )
            .await
    });
    let request = next_queued(&chain.queue)
        .await
        .expect("queued: Alex's copy is not the library's");
    assert_eq!(
        request.place(),
        AcquisitionPlace {
            origin: Some(AcquisitionOrigin::Heart),
            ..Default::default()
        },
        "into the shared library"
    );
    request.completion.try_set_result("/music/Artist/Album/1.flac");
    let outcome = running.await.expect("joins").expect("ran");
    assert_eq!(outcome, TrackOutcome::Landed("/music/Artist/Album/1.flac".into()));
}

#[tokio::test]
async fn a_member_heart_counts_their_own_copy_and_never_anothers() {
    let world = World::new(None);
    configure(&world, &[HeartDownloadSource::YouTube], false);
    let chain = chain(world);
    let alex = chain
        .coordinator
        .acquire_track_outcome(
            "test",
            "1",
            Some("alex"),
            None,
            &AcquisitionTarget::Member("alex".into()),
            AcquisitionOrigin::Heart,
        )
        .await
        .expect("ran");
    assert_eq!(alex, TrackOutcome::AlreadyThere);
    assert!(next_queued(&chain.queue).await.is_none());

    // Sam cannot see Alex's library: their heart downloads, into the pool, for them.
    let coordinator = chain.coordinator.clone();
    tokio::spawn(async move {
        coordinator
            .acquire_track_outcome(
                "test",
                "1",
                Some("sam"),
                None,
                &AcquisitionTarget::Member("sam".into()),
                AcquisitionOrigin::Heart,
            )
            .await
    });
    let request = next_queued(&chain.queue).await.expect("queued");
    assert_eq!(request.pool_destination(), Some(vec!["sam".to_string()]));
}

#[tokio::test]
async fn member_chain_skips_lidarr() {
    let world = World::new(None);
    configure(
        &world,
        &[HeartDownloadSource::Lidarr, HeartDownloadSource::YouTube],
        false,
    );
    let chain = chain(world);
    let coordinator = chain.coordinator.clone();
    tokio::spawn(async move {
        coordinator
            .acquire_track_outcome(
                "test",
                "2",
                Some("sam"),
                None,
                &AcquisitionTarget::Member("sam".into()),
                AcquisitionOrigin::Heart,
            )
            .await
    });
    let request = next_queued(&chain.queue).await.expect("queued");
    assert_eq!(request.source_override, Some(DownloadSource::YouTube));
    assert_eq!(chain.lidarr.calls(), 0, "Lidarr fills the shared library only");

    // The owner's chain still starts with Lidarr.
    chain
        .coordinator
        .acquire_track_outcome(
            "test",
            "3",
            Some("winters"),
            None,
            &AcquisitionTarget::Shared,
            AcquisitionOrigin::Heart,
        )
        .await
        .expect("ran");
    assert_eq!(chain.lidarr.calls(), 1);
}

#[tokio::test]
async fn plays_keep_a_copy_only_for_those_who_add_at_once() {
    let world = World::new(None);
    configure(&world, &[HeartDownloadSource::YouTube], true);
    let chain = chain(world);

    chain
        .coordinator
        .queue_play("test", "9", Some("sam"), None, Some("sam"));
    assert!(
        next_queued(&chain.queue).await.is_none(),
        "a listener's play keeps no copy"
    );

    chain
        .coordinator
        .queue_play("test", "9", Some("alex"), None, Some("alex"));
    let alex = next_queued(&chain.queue).await.expect("queued");
    assert_eq!(alex.pool_destination(), Some(vec!["alex".to_string()]));

    chain
        .coordinator
        .queue_play("test", "8", Some("winters"), None, Some("winters"));
    let owner = next_queued(&chain.queue).await.expect("queued");
    assert_eq!(
        owner.place(),
        AcquisitionPlace {
            origin: Some(AcquisitionOrigin::Play),
            ..Default::default()
        }
    );
}

#[tokio::test]
async fn import_songs_go_where_the_member_may_add_them() {
    let world = World::new(None);
    configure(&world, &[HeartDownloadSource::YouTube], false);
    let mut pat = member("pat", Preset::Listener, 6);
    pat.edits.insert("AddToLibrary".into(), json!("SaveOnly"));
    world.family.upsert_member(pat);
    let mut full = member("max", Preset::Member, 7);
    full.edits.insert("StorageLimitGb".into(), json!(1));
    world.family.upsert_member(full);
    world
        .index
        .note_placed(7, &format!("{}/max/A/B/c.flac", world.members()), 2_000_000_000);
    let chain = chain(world);
    let coordinator = &chain.coordinator;
    assert_eq!(
        coordinator.import_target("winters"),
        Ok(AcquisitionTarget::Shared)
    );
    assert_eq!(
        coordinator.import_target("alex"),
        Ok(AcquisitionTarget::Member("alex".into()))
    );
    assert_eq!(
        coordinator.import_target("sam"),
        Ok(AcquisitionTarget::Member("sam".into()))
    );
    assert_eq!(
        coordinator.import_target("pat"),
        Err(crate::services::family::library_write_gate::SAVE_ONLY_MESSAGE.to_string())
    );
    assert_eq!(
        coordinator.import_target("max"),
        Err("Your library is full (2.0 of 1 GB)".to_string())
    );
    drop(chain.world);
}

#[tokio::test]
async fn family_off_plays_and_hearts_as_always() {
    let world = World::with_family(None, false);
    // Nobody has a library of their own: Family was never used here.
    world.family.remove_member("alex");
    world.family.remove_member("sam");
    configure(&world, &[HeartDownloadSource::YouTube], true);
    let chain = chain(world);
    chain
        .coordinator
        .queue_play("test", "9", Some("sam"), None, Some("sam"));
    let request = next_queued(&chain.queue).await.expect("queued");
    assert_eq!(
        request.place(),
        AcquisitionPlace {
            origin: Some(AcquisitionOrigin::Play),
            ..Default::default()
        }
    );
    // The file under the members folder is just the library's.
    let owner = chain
        .coordinator
        .acquire_track_outcome(
            "test",
            "1",
            Some("winters"),
            None,
            &AcquisitionTarget::Shared,
            AcquisitionOrigin::Heart,
        )
        .await
        .expect("ran");
    assert_eq!(owner, TrackOutcome::AlreadyThere);
}

#[tokio::test]
async fn family_turned_off_never_counts_a_members_own_copy_as_the_librarys() {
    let world = World::with_family(None, false);
    configure(&world, &[HeartDownloadSource::YouTube], true);
    let chain = chain(world);
    let coordinator = chain.coordinator.clone();
    let owner = tokio::spawn(async move {
        coordinator
            .acquire_track_outcome(
                "test",
                "1",
                Some("winters"),
                None,
                &AcquisitionTarget::Shared,
                AcquisitionOrigin::Heart,
            )
            .await
    });
    // Alex's library is still there, so the shared library does not have the song: it is fetched.
    let request = next_queued(&chain.queue).await.expect("queued");
    assert_eq!(request.external_id, "1");
    assert_eq!(
        request.place(),
        AcquisitionPlace {
            origin: Some(AcquisitionOrigin::Heart),
            ..Default::default()
        }
    );
    owner.abort();
}

/// A member who hearts one song of an album gets the whole album into their own library, but
/// only the song they hearted is starred for them. A heart on the album stars every song.
#[tokio::test]
async fn a_members_song_heart_brings_its_album_without_starring_its_songs() {
    let world = World::new(None);
    configure(&world, &[HeartDownloadSource::YouTube], false);
    let members = world.members();
    let alex = format!("{members}/alex");
    let navidrome = navidrome_with_songs(json!([
        song_row("alex-2", 3, &alex, "Artist/Album/2.flac", 10),
        song_row("alex-3", 3, &alex, "Artist/Album/3.flac", 10),
    ]))
    .await;
    world.point_at(&navidrome.uri());
    world.index.refresh().await.expect("read");
    let chain = chain(world);
    let song = |id: &str| Song {
        title: format!("Song {id}"),
        artist: "Artist".into(),
        album: "Album".into(),
        external_provider: Some("test".into()),
        external_id: Some(id.into()),
        ..Default::default()
    };
    chain.metadata.albums.lock().insert(
        ("test".into(), "album-1".into()),
        Album {
            id: "album-1".into(),
            title: "Album".into(),
            artist: "Artist".into(),
            songs: vec![song("2"), song("3")],
            ..Default::default()
        },
    );
    let land = |expected: &'static str| {
        let queue = chain.queue.clone();
        let path = format!("{alex}/Artist/Album/{expected}.flac");
        async move {
            let request = next_queued(&queue).await.expect("queued");
            assert_eq!(request.external_id, expected);
            assert_eq!(request.pool_destination(), Some(vec!["alex".to_string()]));
            queue.release(&request);
            request.completion.try_set_result(path);
        }
    };

    chain.coordinator.queue_tracks_then_album_for(
        &[("test".to_string(), "9".to_string())],
        "test",
        "album-1",
        Some("alex"),
        Some("alex"),
        AcquisitionTarget::Member("alex".into()),
    );
    // The hearted song first, then the album's songs one by one.
    land("9").await;
    land("2").await;
    land("3").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        chain.world.stars.asked.lock().is_empty(),
        "the album's songs are not starred for a song heart"
    );

    chain.coordinator.queue_album_for(
        "test",
        "album-1",
        Some("alex"),
        AcquisitionTarget::Member("alex".into()),
    );
    land("2").await;
    land("3").await;
    let stars = chain.world.stars.clone();
    assert!(
        eventually(|| {
            let asked = stars.asked.lock();
            asked.contains(&("alex".into(), "alex-2".into()))
                && asked.contains(&("alex".into(), "alex-3".into()))
        })
        .await,
        "an album heart stars every song"
    );
}

/// Gates that are ready, and pass a file that landed with nobody judging it for them.
struct PassingLanded;

#[async_trait]
impl HeartGates for PassingLanded {
    async fn build(&self, _: &str, _: &str, holder: &Holder, origin: AcquisitionOrigin) -> GateStart {
        GateStart::Ready(HeartGate {
            user: holder.user.clone(),
            origin,
            r: None,
            heard: None,
            heard_seconds: None,
            reading: HeardReading::NotHeard,
            stand_in: None,
        })
    }

    async fn judge_landed(&self, _: &str, _: LabelSource, _: &str, gates: &[HeartGate]) -> Vec<GateVerdict> {
        gates
            .iter()
            .map(|_| GateVerdict::Accept {
                by: Acceptance::Sound,
                tags: octo_core::fingerprint::heart_gate::TagClaim::AsHeard,
            })
            .collect()
    }
}

/// A member's heart whose file landed with nobody judging it for them has it judged by their own
/// gate, and a pass links it into their own library, as the worker links a member whose gate
/// passed it: their row's Done means the file is theirs.
#[tokio::test]
async fn a_members_late_judged_landed_file_is_linked_into_their_own_library() {
    let world = World::new(None);
    configure(&world, &[HeartDownloadSource::YouTube], false);
    let chain = chain(world);
    let coordinator = HeartAcquisitionCoordinator::new(
        chain.world.settings.clone(),
        chain.queue.clone(),
        Arc::new(FakeDownloads::default()),
        chain.lidarr.clone(),
        HeartCoordinatorExtras {
            gates: Some(Arc::new(PassingLanded)),
            ..Default::default()
        },
    );
    coordinator.use_family(chain.coordinator.family.get().expect("family").clone());
    let pooled = chain.world.pool_file("Artist/Album/Song 1.flac", 10);
    chain
        .world
        .pool
        .record(crate::services::family::family_pool_store::PoolFile {
            path: pooled.clone(),
            size: 10,
            format: "flac".into(),
            outside_ids: vec!["test:1".into()],
            isrc: None,
            title: "Song 1".into(),
            artist: "Artist".into(),
            album: "Album".into(),
            refs: vec!["alex".into()],
            transcoded_from: None,
        });
    let queue = chain.queue.clone();
    let landed = pooled.clone();
    tokio::spawn(async move {
        // The file lands with nothing judged for sam, and nobody links it for her.
        let request = next_queued(&queue).await.expect("queued");
        queue.release(&request);
        request.completion.try_set_result(landed);
    });

    let outcome = coordinator
        .acquire_track_outcome(
            "test",
            "1",
            Some("sam"),
            None,
            &AcquisitionTarget::Member("sam".into()),
            AcquisitionOrigin::Heart,
        )
        .await
        .expect("ran");

    assert_eq!(outcome, TrackOutcome::Landed(pooled.clone()));
    let link = chain.world.pool.link_path(&pooled, "sam").expect("a link path");
    assert!(
        std::fs::symlink_metadata(&link).is_ok(),
        "sam's own library has the file: {link}"
    );
}

#[tokio::test]
async fn a_members_music_that_goes_to_shared_is_added_to_the_shared_library() {
    let world = World::new(None);
    configure(&world, &[HeartDownloadSource::YouTube], true);
    let mut ben = member("ben", Preset::Member, 8);
    ben.edits.insert("MusicGoesTo".into(), json!("Shared"));
    world.family.upsert_member(ben);
    let mut ria = member("ria", Preset::Listener, 9);
    ria.edits.insert("MusicGoesTo".into(), json!("Shared"));
    world.family.upsert_member(ria);
    let chain = chain(world);
    let coordinator = chain.coordinator.clone();
    assert_eq!(coordinator.request_target("ben"), AcquisitionTarget::Shared);
    assert_eq!(coordinator.request_target("ria"), AcquisitionTarget::Shared);
    assert_eq!(
        coordinator.request_target("alex"),
        AcquisitionTarget::Member("alex".into())
    );
    assert_eq!(coordinator.import_target("ria"), Ok(AcquisitionTarget::Shared));
    assert_eq!(
        coordinator.import_target("alex"),
        Ok(AcquisitionTarget::Member("alex".into()))
    );

    // An approved song of theirs is fetched into the shared library, not their own.
    let fetcher = LiveRequestFetcher::new(
        chain.queue.clone(),
        coordinator.clone(),
        Arc::new(AcquisitionTracker::new(None, Clock::system())),
        Arc::new(UpgradeQueue::new()),
    );
    let asked = tokio::spawn(async move {
        fetcher
            .song(
                "ben",
                "test",
                "7",
                octo_core::family::abilities::RequestQuality::Flac,
                None,
            )
            .await
    });
    let request = next_queued(&chain.queue).await.expect("queued");
    assert_eq!(
        request.pool_destination(),
        None,
        "the shared library, not Ben's folder"
    );
    asked.abort();

    // Their play keeps its copy in the shared library too.
    coordinator.queue_play("test", "6", Some("ben"), None, Some("ben"));
    let played = next_queued(&chain.queue).await.expect("queued");
    assert_eq!(played.pool_destination(), None);
}

#[tokio::test]
async fn an_approved_song_for_the_shared_library_is_not_handed_to_lidarr() {
    let world = World::new(None);
    configure(
        &world,
        &[HeartDownloadSource::Lidarr, HeartDownloadSource::YouTube],
        false,
    );
    let mut ben = member("ben", Preset::Member, 8);
    ben.edits.insert("MusicGoesTo".into(), json!("Shared"));
    world.family.upsert_member(ben);
    let chain = chain(world);
    let fetcher = LiveRequestFetcher::new(
        chain.queue.clone(),
        chain.coordinator.clone(),
        Arc::new(AcquisitionTracker::new(None, Clock::system())),
        Arc::new(UpgradeQueue::new()),
    );
    let asked = tokio::spawn(async move {
        fetcher
            .song(
                "ben",
                "test",
                "7",
                octo_core::family::abilities::RequestQuality::Best,
                None,
            )
            .await
    });
    // The request waits for a file, which Lidarr never hands back.
    let request = next_queued(&chain.queue).await.expect("queued");
    assert_eq!(request.source_override, Some(DownloadSource::YouTube));
    assert_eq!(request.pool_destination(), None);
    assert_eq!(chain.lidarr.calls(), 0);
    asked.abort();
}
