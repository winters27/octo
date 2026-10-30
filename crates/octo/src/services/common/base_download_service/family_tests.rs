//! The download path with the family pool: a member's download lands in the pool and is
//! linked for them, a song already on disk for the family is never fetched again, the shared
//! library takes a pool copy over, a real FLAC is the only FLAC, and with Family off nothing
//! about a download changes.

use std::path::Path;
use std::sync::Arc;

use octo_core::models::domain::Song;
use tokio_util::sync::CancellationToken;

use super::test_support::TestBackend;
use super::*;
use crate::services::common::test_fakes::FakeMetadata;
use crate::services::family::family_pool_store::PoolFile;
use crate::services::family::test_support::World;
use crate::services::i_download_service::IDownloadService;

struct Family {
    world: World,
    service: Arc<BaseDownloadService>,
    backend: Arc<TestBackend>,
}

fn song(id: &str) -> Song {
    Song {
        title: format!("Song {id}"),
        artist: "Artist".into(),
        album: "Album".into(),
        external_provider: Some("test".into()),
        external_id: Some(id.into()),
        ..Default::default()
    }
}

fn family(world: World) -> Family {
    let metadata = FakeMetadata::default();
    for id in ["1", "2", "3", "4"] {
        metadata
            .songs
            .lock()
            .insert(("test".to_string(), id.to_string()), song(id));
    }
    let core = DownloadCore {
        settings: world.settings.clone(),
        local_library: world.library.clone(),
        metadata: Arc::new(metadata),
        navidrome_identity: NavidromeIdentityService::new(world.settings.clone(), reqwest::Client::new()),
        history: Arc::new(DownloadHistoryService::new(world.dir.path().join("history.json"))),
        notifications: Arc::new(NotificationService::new(Vec::new(), world.settings.clone(), None)),
    };
    let backend = Arc::new(TestBackend::default());
    let services = DownloadServices {
        family: Some(world.pool.clone() as Arc<dyn FamilyPlacement>),
        ..Default::default()
    };
    let service = BaseDownloadService::new(core, services, Arc::clone(&backend) as Arc<dyn DownloadBackend>);
    Family {
        world,
        service,
        backend,
    }
}

impl Family {
    /// The transfer lands `bytes` as a file with this extension, marked as made from lossy
    /// audio when `transcoded`.
    fn lands(&self, extension: &'static str, transcoded: bool) {
        let incoming = self.world.dir.path().join("incoming");
        self.backend.land(move |song, track_id| {
            std::fs::create_dir_all(&incoming).expect("made");
            let landed = incoming.join(format!(
                "{track_id}-{}.{extension}",
                uuid::Uuid::new_v4().simple()
            ));
            let bytes = if extension == "flac" {
                crate::services::test_support::flac()
            } else {
                crate::services::test_support::mp3()
            };
            std::fs::write(&landed, bytes).expect("written");
            if transcoded {
                song.transcoded_from = Some("MP3 320".into());
            }
            landed.to_string_lossy().into_owned()
        });
    }

    async fn get(&self, id: &str, place: AcquisitionPlace) -> anyhow::Result<String> {
        self.service
            .execute_acquisition_to(
                "test",
                id,
                false,
                true,
                None,
                &CancellationToken::new(),
                Some(vec!["alex".into()]),
                false,
                None,
                place,
            )
            .await
    }
}

fn for_members(members: &[&str]) -> AcquisitionPlace {
    AcquisitionPlace {
        pool_members: Some(members.iter().map(|m| m.to_string()).collect()),
        flac_only: false,
        ..Default::default()
    }
}

#[tokio::test]
async fn member_heart_downloads_to_pool_and_links() {
    let family = family(World::new(None));
    family.lands("mp3", false);
    let path = family.get("1", for_members(&["alex"])).await.expect("landed");
    let pool = family.world.pool.pool_root_path();
    assert!(path.starts_with(&format!("{pool}/")), "{path}");
    assert!(Path::new(&path).is_file());
    let link = family.world.pool.link_path(&path, "alex").expect("a link path");
    assert!(
        std::fs::symlink_metadata(&link)
            .expect("linked")
            .file_type()
            .is_symlink()
    );
    let record = family.world.pool.store().get(&path).expect("recorded");
    assert_eq!(record.refs, ["alex"]);
    assert_eq!(record.outside_ids, ["test:1"]);
    assert_eq!(record.title, "Song 1");
    assert_eq!(
        family.world.library.targeted(),
        [3],
        "only Alex's library is scanned"
    );
    assert_eq!(family.world.library.full_scans(), 0, "the pool is no library");
    // Nothing went to the shared music folder.
    assert!(
        std::fs::read_dir(family.world.music())
            .expect("there")
            .next()
            .is_none()
    );
}

#[tokio::test]
async fn owner_download_unchanged_while_family_is_off() {
    let family = family(World::with_family(None, false));
    family.lands("mp3", false);
    // Even asked for members, Family off places it in the shared library as always.
    let path = family.get("1", for_members(&["alex"])).await.expect("landed");
    assert!(path.starts_with(&format!("{}/", family.world.music())), "{path}");
    assert!(family.world.pool.store().list().is_empty());
    assert!(family.world.library.targeted().is_empty());
    assert!(
        crate::services::family::test_support::eventually(|| family.world.library.full_scans() == 1).await
    );
}

#[tokio::test]
async fn owner_heart_unchanged_with_family_on() {
    let family = family(World::new(None));
    family.lands("mp3", false);
    let path = family
        .get("1", AcquisitionPlace::default())
        .await
        .expect("landed");
    assert!(path.starts_with(&format!("{}/", family.world.music())), "{path}");
    assert!(family.world.pool.store().list().is_empty());
}

#[tokio::test]
async fn a_song_the_family_has_is_never_downloaded_again() {
    let family = family(World::new(None));
    let pooled = family.world.pool_file("Artist/Album/Song 2.mp3", 10);
    family.world.pool.record(PoolFile {
        path: pooled.clone(),
        size: 10,
        format: "mp3".into(),
        outside_ids: vec!["test:other".into()],
        isrc: None,
        title: "Song 2".into(),
        artist: "Artist".into(),
        album: "Album".into(),
        refs: vec!["sam".into()],
        transcoded_from: None,
    });
    family.lands("mp3", false);
    let path = family.get("2", for_members(&["alex"])).await.expect("found");
    assert_eq!(path, pooled);
    assert_eq!(family.backend.transfers(), 0);

    // The shared library asking for it takes the pool file over: still one file.
    let shared = family.get("2", AcquisitionPlace::default()).await.expect("moved");
    assert!(
        shared.starts_with(&format!("{}/", family.world.music())),
        "{shared}"
    );
    assert!(!Path::new(&pooled).exists());
    assert_eq!(family.backend.transfers(), 0);
    assert!(!Path::new(&format!("{}/sam/Artist", family.world.members())).exists());
}

#[tokio::test]
async fn flac_request_rejects_mp3_and_fake_flac() {
    let family = family(World::new(None));
    let flac_only = AcquisitionPlace {
        pool_members: Some(vec!["alex".into()]),
        flac_only: true,
        ..Default::default()
    };
    family.lands("mp3", false);
    let mp3 = family.get("3", flac_only.clone()).await.expect_err("refused");
    assert_eq!(mp3.to_string(), NO_REAL_FLAC);

    family.lands("flac", true);
    let fake = family.get("3", flac_only.clone()).await.expect_err("refused");
    assert_eq!(fake.to_string(), NO_REAL_FLAC);
    assert!(family.world.pool.store().list().is_empty(), "nothing was kept");
    let incoming = family.world.dir.path().join("incoming");
    assert_eq!(
        std::fs::read_dir(&incoming).expect("there").count(),
        0,
        "both were thrown away"
    );

    family.lands("flac", false);
    let real = family.get("3", flac_only).await.expect("a real FLAC");
    assert!(real.ends_with(".flac"), "{real}");
    assert!(real.starts_with(&family.world.pool.pool_root_path()));
}

#[tokio::test]
async fn a_flac_request_is_never_met_by_a_pool_flac_made_from_mp3() {
    let family = family(World::new(None));
    family.lands("flac", true);
    let fake = family.get("4", for_members(&["sam"])).await.expect("landed");
    let record = family.world.pool.store().get(&fake).expect("recorded");
    assert_eq!(record.transcoded_from.as_deref(), Some("MP3 320"));
    assert!(!record.is_real_flac());

    family.lands("flac", false);
    let real = family
        .get(
            "4",
            AcquisitionPlace {
                pool_members: Some(vec!["alex".into()]),
                flac_only: true,
                ..Default::default()
            },
        )
        .await
        .expect("a real FLAC");
    assert_eq!(
        family.backend.transfers(),
        2,
        "the FLAC made from an MP3 was not taken as a real one"
    );
    assert_ne!(real, fake);
    let better = family.world.pool.store().get(&real).expect("recorded");
    assert!(better.is_real_flac());
    assert_eq!(
        better.refs,
        ["alex", "sam"],
        "the real FLAC replaced it for Sam too"
    );
    assert!(!Path::new(&fake).exists());
}

#[tokio::test]
async fn a_flac_for_a_song_the_pool_has_as_mp3_replaces_it_for_everyone() {
    let family = family(World::new(None));
    family.lands("mp3", false);
    let mp3 = family.get("1", for_members(&["sam"])).await.expect("landed");
    let sam_mp3 = family.world.pool.link_path(&mp3, "sam").expect("a link");
    assert!(Path::new(&sam_mp3).exists());

    family.lands("flac", false);
    let flac = family
        .get(
            "1",
            AcquisitionPlace {
                pool_members: Some(vec!["alex".into()]),
                flac_only: true,
                ..Default::default()
            },
        )
        .await
        .expect("a real FLAC");
    assert_eq!(family.backend.transfers(), 2, "the MP3 was not taken as the FLAC");
    assert!(!Path::new(&mp3).exists(), "the MP3 went to the trash");
    let record = family.world.pool.store().get(&flac).expect("recorded");
    assert_eq!(record.refs, ["alex", "sam"]);
    assert!(!Path::new(&sam_mp3).exists());
    let sam_flac = family.world.pool.link_path(&flac, "sam").expect("a link");
    assert!(
        std::fs::symlink_metadata(&sam_flac).is_ok(),
        "Sam has the FLAC now"
    );
}

/// Family was on, a member downloaded a song into the pool, and Family was turned off with the
/// member's data left. The owner hearting the same song gets it in the shared library: the pool
/// copy moves there, still one file, and the pool is never the owner's copy.
#[tokio::test]
async fn family_off_owner_heart_of_a_pooled_song_reaches_the_shared_library() {
    let family = family(World::new(None));
    family.lands("mp3", false);
    let pooled = family.get("1", for_members(&["alex"])).await.expect("landed");
    assert!(pooled.starts_with(&format!("{}/", family.world.pool.pool_root_path())));
    let mut off = family.world.settings.current().as_ref().clone();
    off.family.enabled = false;
    family.world.settings.set(off);
    let path = family
        .get("1", AcquisitionPlace::default())
        .await
        .expect("answered");
    assert!(
        path.starts_with(&format!("{}/", family.world.music())),
        "the owner's heart was answered with {path}"
    );
    assert!(Path::new(&path).is_file());
    assert!(!Path::new(&pooled).exists(), "one file: it moved");
    assert!(family.world.pool.store().get(&pooled).is_none());
}

#[tokio::test]
async fn a_member_download_waits_while_the_pool_folder_has_no_marker() {
    let family = family(World::new(None));
    family.lands("mp3", false);
    let marker = format!(
        "{}/{}",
        family.world.pool.pool_root_path(),
        crate::services::family::pool::POOL_MARKER
    );
    std::fs::remove_file(&marker).expect("the marker went");
    let refused = family
        .get("1", for_members(&["alex"]))
        .await
        .expect_err("refused");
    assert_eq!(refused.to_string(), crate::services::family::pool::POOL_NOT_READY);
    assert!(family.world.pool.store().list().is_empty(), "nothing placed");
}
