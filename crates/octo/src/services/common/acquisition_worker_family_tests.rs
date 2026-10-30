//! The worker with the family pool: a member's request is placed in the pool, everyone who
//! joined while it ran gets a link, and a request nobody in the family made goes as always.

use std::sync::Arc;

use async_trait::async_trait;
use octo_core::models::download::DownloadInfo;
use octo_core::settings::DownloadSource;
use parking_lot::Mutex;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::services::common::{AcquisitionPlace, AcquisitionTarget};
use crate::services::family::SharedLibraries;
use crate::services::family::test_support::{World, member, write_file};
use crate::services::i_download_service::{AudioStream, DirectStreamInfo};
use crate::services::library::ReplacementHandoff;
use octo_core::family::abilities::Preset;

/// Downloads that wait for a release, then land a file in the pool (for a place with members)
/// or in the shared folder, remembering which way they were asked.
struct PlacingDownloads {
    pool_root: String,
    shared_root: String,
    places: Mutex<Vec<Option<AcquisitionPlace>>>,
    release: watch::Receiver<bool>,
}

#[async_trait]
impl IDownloadService for PlacingDownloads {
    async fn download_song(&self, _: &str, _: &str, _: &CancellationToken) -> anyhow::Result<String> {
        anyhow::bail!("not set up")
    }

    async fn download_and_stream(
        &self,
        _: &str,
        _: &str,
        _: &CancellationToken,
    ) -> anyhow::Result<AudioStream> {
        anyhow::bail!("not set up")
    }

    fn download_remaining_album_tracks_in_background(&self, _: &str, _: &str, _: &str) {}

    async fn execute_acquisition(
        &self,
        _: &str,
        _: &str,
        _: bool,
        _: bool,
        _: Option<DownloadSource>,
        _: &CancellationToken,
        _: Option<Vec<String>>,
        _: bool,
        _: Option<Arc<ReplacementHandoff>>,
    ) -> anyhow::Result<String> {
        self.places.lock().push(None);
        let mut release = self.release.clone();
        let _ = release.wait_for(|released| *released).await;
        let path = format!("{}/Artist/Album/1.flac", self.shared_root);
        write_file(&path, 10);
        Ok(path)
    }

    async fn execute_acquisition_to(
        &self,
        _: &str,
        external_id: &str,
        _: bool,
        _: bool,
        _: Option<DownloadSource>,
        _: &CancellationToken,
        _: Option<Vec<String>>,
        _: bool,
        _: Option<Arc<ReplacementHandoff>>,
        place: AcquisitionPlace,
    ) -> anyhow::Result<String> {
        self.places.lock().push(Some(place));
        let mut release = self.release.clone();
        let _ = release.wait_for(|released| *released).await;
        let path = format!("{}/Artist/Album/{external_id}.flac", self.pool_root);
        write_file(&path, 10);
        Ok(path)
    }

    async fn download_album_with_source(
        &self,
        _: &str,
        _: &str,
        _: DownloadSource,
        _: bool,
        _: &CancellationToken,
        _: Option<Vec<String>>,
    ) -> anyhow::Result<bool> {
        Ok(false)
    }

    fn get_download_status(&self, _: &str) -> Option<DownloadInfo> {
        None
    }

    async fn get_local_path_if_exists(&self, _: &str, _: &str) -> Option<String> {
        None
    }

    async fn is_available(&self) -> bool {
        true
    }

    async fn get_direct_stream(
        &self,
        _: &str,
        _: &str,
        _: Option<&str>,
        _: &CancellationToken,
    ) -> anyhow::Result<Option<DirectStreamInfo>> {
        Ok(None)
    }
}

struct Rig {
    world: World,
    queue: Arc<TrackAcquisitionQueue>,
    downloads: Arc<PlacingDownloads>,
    release: watch::Sender<bool>,
    stopping: CancellationToken,
}

fn rig() -> Rig {
    let world = World::new(None);
    let queue = Arc::new(TrackAcquisitionQueue::new());
    let (release, released) = watch::channel(false);
    let downloads = Arc::new(PlacingDownloads {
        pool_root: world.pool.pool_root_path(),
        shared_root: world.music(),
        places: Mutex::new(Vec::new()),
        release: released,
    });
    let worker = Arc::new(
        AcquisitionWorker::new(
            Arc::clone(&queue),
            Arc::clone(&downloads) as Arc<dyn IDownloadService>,
            Arc::new(ExternalIdRegistry::in_memory()),
            Arc::new(NotificationService::new(Vec::new(), world.settings.clone(), None)),
            None,
        )
        .with_family(world.pool.clone()),
    );
    let stopping = CancellationToken::new();
    tokio::spawn(worker.run(stopping.clone()));
    Rig {
        world,
        queue,
        downloads,
        release,
        stopping,
    }
}

fn for_member(rig: &Rig, user: &str) -> crate::services::common::Completion {
    rig.queue.enqueue_for(
        &AcquisitionTarget::Member(user.into()),
        false,
        "test",
        "1",
        true,
        false,
        true,
        None,
        true,
        Some(user),
        false,
        None,
    )
}

#[tokio::test]
async fn a_member_who_joins_while_it_runs_gets_a_link_too() {
    let rig = rig();
    let alex = for_member(&rig, "alex");
    crate::services::common::test_fakes::until(|| !rig.downloads.places.lock().is_empty()).await;
    let sam = for_member(&rig, "sam");
    rig.release.send(true).expect("released");
    let path = alex.wait().await.expect("landed");
    assert_eq!(sam.wait().await.expect("landed"), path, "one download");
    assert_eq!(
        rig.downloads.places.lock().as_slice(),
        [Some(AcquisitionPlace {
            pool_members: Some(vec!["alex".into()]),
            flac_only: false,
            ..Default::default()
        })]
    );
    for user in ["alex", "sam"] {
        let link = rig.world.pool.link_path(&path, user).expect("a link path");
        assert!(std::fs::symlink_metadata(&link).is_ok(), "{user} has it");
    }
    rig.stopping.cancel();
}

#[tokio::test]
async fn a_request_nobody_in_the_family_made_goes_as_always() {
    let rig = rig();
    rig.release.send(true).expect("released");
    let path = rig
        .queue
        .enqueue(
            "test",
            "2",
            true,
            false,
            true,
            None,
            true,
            Some("winters"),
            false,
            None,
        )
        .wait()
        .await
        .expect("landed");
    assert_eq!(path, format!("{}/Artist/Album/1.flac", rig.world.music()));
    assert_eq!(rig.downloads.places.lock().as_slice(), [None]);
    assert!(rig.world.pool.store().list().is_empty());
    rig.stopping.cancel();
}

/// The owner's heart for the song the members ask for, into the shared library.
fn for_owner(rig: &Rig) -> crate::services::common::Completion {
    rig.queue.enqueue_for(
        &AcquisitionTarget::Shared,
        false,
        "test",
        "1",
        true,
        false,
        true,
        None,
        true,
        Some("winters"),
        false,
        None,
    )
}

/// Makes `name` a member who sees shared library 2 only, not the one the download lands in.
fn sees_library_two_only(rig: &Rig, name: &str, preset: Preset, library: i64) {
    let mut only_two = member(name, preset, library);
    only_two.shared_libraries = SharedLibraries::Ids(vec![2]);
    rig.world.family.upsert_member(only_two);
}

/// Where a link or file ends up once every link is followed.
fn resolved(path: &str) -> std::path::PathBuf {
    std::fs::canonicalize(path).expect("resolves")
}

#[tokio::test]
async fn a_member_heart_joining_the_owners_download_gets_a_pool_copy() {
    let rig = rig();
    sees_library_two_only(&rig, "alex", Preset::Member, 3);
    sees_library_two_only(&rig, "sam", Preset::Listener, 4);
    let owner = for_owner(&rig);
    crate::services::common::test_fakes::until(|| !rig.downloads.places.lock().is_empty()).await;
    let alex = for_member(&rig, "alex");
    let sam = for_member(&rig, "sam");
    rig.release.send(true).expect("released");

    let shared = format!("{}/Artist/Album/1.flac", rig.world.music());
    assert_eq!(owner.wait().await.expect("landed"), shared, "it stays shared");
    assert_eq!(alex.wait().await.expect("landed"), shared);
    assert_eq!(sam.wait().await.expect("landed"), shared);
    assert_eq!(rig.downloads.places.lock().as_slice(), [None], "one download");
    assert!(std::path::Path::new(&shared).is_file());

    let pooled = format!("{}/Artist/Album/1.flac", rig.world.pool.pool_root_path());
    let files = rig.world.pool.store().list();
    assert_eq!(files.len(), 1, "one pool file: {files:?}");
    assert_eq!(files[0].path, pooled);
    assert!(files[0].outside_ids.contains(&"test:1".to_string()));
    let mut refs = files[0].refs.clone();
    refs.sort();
    assert_eq!(refs, ["alex", "sam"]);
    for user in ["alex", "sam"] {
        let link = format!("{}/{user}/Artist/Album/1.flac", rig.world.members());
        assert_eq!(resolved(&link), resolved(&pooled), "{user} has the pool copy");
    }
    rig.stopping.cancel();
}

#[tokio::test]
async fn a_member_heart_that_sees_the_shared_library_gets_no_copy() {
    let rig = rig();
    let owner = for_owner(&rig);
    crate::services::common::test_fakes::until(|| !rig.downloads.places.lock().is_empty()).await;
    let alex = for_member(&rig, "alex");
    rig.release.send(true).expect("released");

    let shared = format!("{}/Artist/Album/1.flac", rig.world.music());
    assert_eq!(owner.wait().await.expect("landed"), shared);
    assert_eq!(alex.wait().await.expect("landed"), shared);
    assert!(rig.world.pool.store().list().is_empty(), "nothing in the pool");
    let link = format!("{}/alex/Artist/Album/1.flac", rig.world.members());
    assert!(std::fs::symlink_metadata(&link).is_err(), "no copy of their own");
    let pooled = format!("{}/Artist/Album/1.flac", rig.world.pool.pool_root_path());
    assert!(!std::path::Path::new(&pooled).exists());
    rig.stopping.cancel();
}
