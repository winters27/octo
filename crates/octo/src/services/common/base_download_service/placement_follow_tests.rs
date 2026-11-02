//! While a Lidarr follow of Octo's may still import on an artist, a file Octo would place under one
//! of Lidarr's root folders for that artist waits in the one hold path, plays from there, and is
//! placed when the follow is over: no rescan can link it into the artist while Octo's own import
//! is coming. A file another source proved by its sound ends the artist's follows instead. A kept
//! copy whose song's Lidarr file passed the gate of someone it was kept for never lands beside
//! that file: in the shared library it gives way, in the family pool it lands only for the
//! members it is still for. A file is moved into place only where nothing lies, never over a file
//! that lands there meanwhile.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use octo_core::common::Clock;
use octo_core::fingerprint::{VerificationResult, VerificationVerdict};
use octo_core::models::subsonic::ScanStatus;
use octo_core::settings::{AppSettings, SubsonicSettings};
use parking_lot::Mutex;
use tempfile::TempDir;

use super::super::test_support::{Harness, build};
use super::super::{DownloadServices, RequestedIdentity};
use super::*;
use crate::services::common::AcquisitionTracker;
use crate::services::family::{FamilyPlacement, PoolFile};
use crate::services::lidarr::{LidarrPendingAlbums, PendingAlbum};
use crate::services::local::test_support::FakeLocalLibrary;
use crate::services::local::{
    ILocalLibraryService, KeptHeart, LocalSongMapping, ParsedExternalId, ParsedSongId,
};
use crate::services::test_support::flac;

/// The fake library, whose records a rewrite puts back as they are given.
struct Library(Arc<FakeLocalLibrary>);

#[async_trait]
impl ILocalLibraryService for Library {
    async fn get_local_path_for_external_song(&self, provider: &str, id: &str) -> Option<String> {
        self.0.get_local_path_for_external_song(provider, id).await
    }

    async fn register_downloaded_song(&self, song: &Song, path: &str) -> anyhow::Result<()> {
        self.0.register_downloaded_song(song, path).await
    }

    async fn get_local_id_for_external_song(&self, provider: &str, id: &str) -> Option<String> {
        self.0.get_local_id_for_external_song(provider, id).await
    }

    fn parse_song_id(&self, song_id: &str) -> ParsedSongId {
        self.0.parse_song_id(song_id)
    }

    fn parse_external_id(&self, id: &str) -> ParsedExternalId {
        self.0.parse_external_id(id)
    }

    async fn get_mappings(&self) -> Vec<LocalSongMapping> {
        self.0.get_mappings().await
    }

    async fn find_mapping_by_tags(
        &self,
        artist: Option<&str>,
        title: Option<&str>,
        album: Option<&str>,
    ) -> Option<LocalSongMapping> {
        self.0.find_mapping_by_tags(artist, title, album).await
    }

    async fn forget_mapping(&self, path: &str) -> anyhow::Result<Vec<LocalSongMapping>> {
        self.0.forget_mapping(path).await
    }

    async fn forget_mapping_keyed(&self, key: &str) -> anyhow::Result<bool> {
        self.0.forget_mapping_keyed(key).await
    }

    async fn restore_mappings(&self, mappings: &[LocalSongMapping], path: &str) -> anyhow::Result<()> {
        for mapping in mappings {
            self.0.by_tags.lock().push((
                mapping.artist.clone(),
                mapping.title.clone(),
                Some(mapping.album.clone()),
                LocalSongMapping {
                    local_path: path.to_string(),
                    ..mapping.clone()
                },
            ));
        }
        Ok(())
    }

    async fn trigger_library_scan(&self, force: bool) -> bool {
        self.0.trigger_library_scan(force).await
    }

    async fn update_local_path(&self, old_path: &str, new_path: &str) -> anyhow::Result<usize> {
        self.0.update_local_path(old_path, new_path).await
    }

    async fn get_scan_status(&self) -> Option<ScanStatus> {
        self.0.get_scan_status().await
    }
}

/// The family pool under `root`, which records each file placed in it and whom it was linked for.
struct Pool {
    root: String,
    placed: Mutex<Vec<(String, Vec<String>)>>,
}

#[async_trait]
impl FamilyPlacement for Pool {
    fn pool_root(&self) -> Option<String> {
        Some(self.root.clone())
    }

    async fn placement_ready(&self) -> bool {
        true
    }

    fn is_pool_path(&self, path: &str) -> bool {
        slashed(path).starts_with(&format!("{}/", slashed(&self.root)))
    }

    async fn find_song(&self, _: &str, _: &str, _: &Song) -> Option<PoolFile> {
        None
    }

    fn made_from_lossy(&self, _: &str) -> bool {
        false
    }

    fn link_path_for(&self, _: &str, _: &str) -> Option<String> {
        None
    }

    async fn for_owner(&self, path: &str) -> Option<String> {
        Some(path.to_string())
    }

    async fn placed(
        &self,
        path: &str,
        _: &Song,
        _: &str,
        _: &str,
        members: &[String],
        _: bool,
    ) -> Option<String> {
        self.placed.lock().push((path.to_string(), members.to_vec()));
        None
    }

    async fn settle(&self, path: &str, _: &[String], _: bool, _: Option<&str>) -> String {
        path.to_string()
    }
}

struct Rig {
    dir: TempDir,
    harness: Harness,
    pending: Arc<LidarrPendingAlbums>,
    library: Arc<FakeLocalLibrary>,
    tracker: Arc<AcquisitionTracker>,
}

impl Rig {
    fn new() -> Rig {
        Rig::with_family(None)
    }

    fn with_family(family: Option<Arc<dyn FamilyPlacement>>) -> Rig {
        let dir = tempfile::tempdir().expect("a temp dir");
        let pending = Arc::new(LidarrPendingAlbums::new(None));
        let library = Arc::new(FakeLocalLibrary::default());
        let tracker = Arc::new(AcquisitionTracker::new(None, Clock::system()));
        let harness = build(
            dir.path(),
            AppSettings {
                subsonic: SubsonicSettings {
                    folder_structure: FolderStructure::Organized,
                    ..Default::default()
                },
                ..Default::default()
            },
            Some(Arc::new(Library(library.clone()))),
            None,
            DownloadServices {
                lidarr_pending: Some(pending.clone()),
                tracker: Some(tracker.clone()),
                family,
                ..Default::default()
            },
        );
        Rig {
            dir,
            harness,
            pending,
            library,
            tracker,
        }
    }

    fn root(&self) -> String {
        self.dir.path().to_string_lossy().into_owned()
    }

    /// An open follow of Octo's on an album, whose Lidarr root folder is the music folder.
    fn follow(&self, id: &str, artist: &str, title: &str, various_artists: bool) {
        self.pending.remember(PendingAlbum {
            foreign_album_id: id.into(),
            artist: artist.into(),
            title: title.into(),
            search_command: 3,
            roots: vec![self.root()],
            various_artists,
            songs: vec!["soulseek:teardrop".into()],
            ..Default::default()
        });
    }

    /// A file a source delivered, outside the layout.
    fn landed(&self, name: &str) -> String {
        let dir = self.dir.path().join("peer");
        std::fs::create_dir_all(&dir).expect("made");
        let path = dir.join(name);
        std::fs::write(&path, flac()).expect("written");
        path.to_string_lossy().into_owned()
    }

    async fn place(&self, song: &Song, requested: &RequestedIdentity, source: DownloadSource) -> Placement {
        let landed = self.landed(&format!("{}.flac", song.title));
        self.harness
            .service
            .place_or_hold(
                song,
                requested,
                &landed,
                None,
                Some(source),
                &["alice".to_string()],
            )
            .await
    }

    /// The kept copy of Teardrop held for the follow of Mezzanine, for `users`, its record kept
    /// for them as the library writes it, placed under `root` (the music folder when None).
    async fn held_teardrop(&self, users: &[&str], root: Option<&str>) -> HeldPlacement {
        self.follow("rg-mezzanine", "Massive Attack", "Mezzanine", false);
        if let Some(root) = root {
            // Lidarr's root folder holds the pool too.
            self.pending
                .update("rg-mezzanine", |entry| entry.roots.push(root.to_string()));
        }
        let landed = self.landed("Teardrop.flac");
        let users: Vec<String> = users.iter().map(|user| user.to_string()).collect();
        let placement = self
            .harness
            .service
            .place_or_hold(
                &kept_teardrop(),
                &teardrop_request(),
                &landed,
                root,
                Some(DownloadSource::YouTube),
                &users,
            )
            .await;
        std::fs::write(
            Path::new(&placement.path).with_extension("lrc"),
            "[00:01.00] love, love is a verb",
        )
        .expect("written");
        self.library.by_tags.lock().push((
            "Massive Attack".into(),
            "Teardrop".into(),
            Some("Mezzanine".into()),
            LocalSongMapping {
                external_provider: "soulseek".into(),
                external_id: "teardrop".into(),
                local_path: placement.path.clone(),
                title: "Teardrop".into(),
                artist: "Massive Attack".into(),
                album: "Mezzanine".into(),
                heart: Some(KeptHeart {
                    users: users.clone(),
                    as_heard: true,
                    ..Default::default()
                }),
                ..Default::default()
            },
        ));
        let held = self.pending.held().remove(0);
        assert_eq!(held.held_path, placement.path);
        held
    }
}

fn as_proven(proof: RecordingProof) -> Option<Box<VerificationResult>> {
    Some(Box::new(VerificationResult {
        verdict: VerificationVerdict::Confirmed,
        proof,
        ..Default::default()
    }))
}

fn song(artist: &str, title: &str, album: &str, id: &str, proof: RecordingProof) -> Song {
    Song {
        artist: artist.into(),
        title: title.into(),
        album: album.into(),
        external_provider: Some("soulseek".into()),
        external_id: Some(id.into()),
        verification: as_proven(proof),
        ..Default::default()
    }
}

fn kept_teardrop() -> Song {
    song(
        "Massive Attack",
        "Teardrop",
        "Mezzanine",
        "teardrop",
        RecordingProof::AsHeard { video_id: None },
    )
}

fn teardrop_request() -> RequestedIdentity {
    RequestedIdentity::new("Massive Attack", "Teardrop", "Mezzanine", Some(3))
}

fn slashed(path: &str) -> String {
    path.replace('\\', "/")
}

#[tokio::test]
async fn a_kept_copy_for_a_followed_artist_waits_in_the_hold_path_and_is_placed_when_it_ends() {
    let rig = Rig::new();
    rig.follow("rg-mezzanine", "Massive Attack", "Mezzanine", false);
    rig.tracker.begin(
        "soulseek",
        "teardrop",
        None,
        Some("alice"),
        Some("Massive Attack"),
        Some("Teardrop"),
        Some("Mezzanine"),
    );

    let placement = rig
        .place(&kept_teardrop(), &teardrop_request(), DownloadSource::YouTube)
        .await;

    let target = format!("{}/Massive Attack/Mezzanine/03 - Teardrop.flac", rig.root());
    let held_root = held_root(rig.dir.path()).to_string_lossy().into_owned();
    assert!(
        slashed(&placement.path).starts_with(&slashed(&held_root)),
        "{} plays from the hold path",
        placement.path
    );
    assert!(Path::new(&placement.path).is_file());
    assert!(!Path::new(&target).exists(), "nothing a rescan could link");
    assert!(!placement.created_folder);
    let held = rig.pending.held();
    assert_eq!(held.len(), 1);
    assert!(held[0].kept);
    assert_eq!(held[0].song, "soulseek:teardrop");
    assert_eq!(slashed(&held[0].target), slashed(&target));
    assert_eq!(held[0].follows, ["rg-mezzanine"]);
    assert_eq!(held[0].users, ["alice"], "whose row says it waits");
    let row = rig.tracker.detail("soulseek:teardrop", None).expect("the row");
    let notes: Vec<String> = row
        .events
        .unwrap_or_default()
        .into_iter()
        .map(|event| event.text)
        .collect();
    assert!(notes.iter().any(|note| note == HELD_FOR_LIDARR), "{notes:?}");
    assert_eq!(
        row.note.as_deref(),
        Some(HELD_FOR_LIDARR),
        "the row says it while the copy waits"
    );
    // Lyrics written beside the held file go along with it.
    let lyrics = Path::new(&placement.path).with_extension("lrc");
    std::fs::write(&lyrics, "[00:01.00] love, love is a verb").expect("written");

    assert!(rig.pending.request_end("rg-mezzanine", FollowEnd::Done));
    let free = rig.pending.release("rg-mezzanine");
    assert_eq!(free.len(), 1);
    let HeldEnd::Placed(placed) = rig.harness.service.place_held_file(&free[0]).await else {
        panic!("placed");
    };

    assert_eq!(slashed(&placed), slashed(&target));
    assert!(Path::new(&target).is_file());
    assert!(Path::new(&target).with_extension("lrc").is_file());
    assert!(!Path::new(&placement.path).exists());
    assert!(
        !Path::new(&placement.path).parent().expect("its folder").exists(),
        "the hold folder goes once empty"
    );
    assert_eq!(
        rig.library.moved.lock().clone(),
        [(placement.path.clone(), placed.clone())]
    );
    assert!(rig.library.scans() > 0);
}

#[tokio::test]
async fn a_file_of_another_artist_or_under_no_lidarr_root_is_placed_as_usual() {
    let rig = Rig::new();
    rig.follow("rg-dummy", "Portishead", "Dummy", false);

    let placement = rig
        .place(&kept_teardrop(), &teardrop_request(), DownloadSource::YouTube)
        .await;

    assert_eq!(
        slashed(&placement.path),
        slashed(&format!(
            "{}/Massive Attack/Mezzanine/03 - Teardrop.flac",
            rig.root()
        ))
    );
    assert!(rig.pending.held().is_empty());

    let elsewhere = Rig::new();
    elsewhere.pending.remember(PendingAlbum {
        foreign_album_id: "rg-mezzanine".into(),
        artist: "Massive Attack".into(),
        title: "Mezzanine".into(),
        search_command: 3,
        roots: vec!["/somewhere/else".into()],
        ..Default::default()
    });
    let placement = elsewhere
        .place(&kept_teardrop(), &teardrop_request(), DownloadSource::YouTube)
        .await;
    assert!(Path::new(&placement.path).is_file());
    assert!(elsewhere.pending.held().is_empty());
}

/// A follow that ended but may still import (a download of Octo's not accounted for yet) holds a
/// new file for its artist as an open one does; once it is over, the file is placed as usual.
#[tokio::test]
async fn a_file_waits_for_an_ended_follow_that_may_still_import() {
    let rig = Rig::new();
    rig.follow("rg-mezzanine", "Massive Attack", "Mezzanine", false);
    assert!(rig.pending.request_end("rg-mezzanine", FollowEnd::DeadDownload));

    let placement = rig
        .place(&kept_teardrop(), &teardrop_request(), DownloadSource::YouTube)
        .await;

    let held_root = held_root(rig.dir.path()).to_string_lossy().into_owned();
    assert!(
        slashed(&placement.path).starts_with(&slashed(&held_root)),
        "{} plays from the hold path",
        placement.path
    );
    assert_eq!(rig.pending.held().len(), 1);

    rig.pending.update("rg-mezzanine", |entry| entry.released = true);
    let again = rig
        .place(
            &song(
                "Massive Attack",
                "Angel",
                "Mezzanine",
                "angel",
                RecordingProof::AsHeard { video_id: None },
            ),
            &RequestedIdentity::new("Massive Attack", "Angel", "Mezzanine", Some(1)),
            DownloadSource::YouTube,
        )
        .await;
    assert_eq!(
        slashed(&again.path),
        slashed(&format!(
            "{}/Massive Attack/Mezzanine/01 - Angel.flac",
            rig.root()
        )),
        "the follow is over: placed as usual"
    );
}

/// A Various Artists pick: the song's own artist is not the album's, so the kept copy is held by
/// the album's title.
#[tokio::test]
async fn a_kept_copy_filed_under_a_various_artists_pick_is_held_by_its_album() {
    let rig = Rig::new();
    rig.follow("rg-live-aid", "Various Artists", "Live Aid", true);
    let song = song(
        "Phil Collins",
        "Against All Odds",
        "Live Aid",
        "against",
        RecordingProof::AsHeard { video_id: None },
    );

    let placement = rig
        .place(
            &song,
            &RequestedIdentity::new("Phil Collins", "Against All Odds", "Live Aid", Some(1)),
            DownloadSource::YouTube,
        )
        .await;

    assert!(
        !slashed(&placement.path).starts_with(&slashed(&format!("{}/Phil Collins", rig.root()))),
        "{}",
        placement.path
    );
    assert_eq!(rig.pending.held().len(), 1);
}

/// Another source's file proven by its sound, for another album of a followed artist: it ends
/// that artist's follow at once and waits only for the follow to remove Octo's items.
#[tokio::test]
async fn a_proven_file_of_another_source_ends_the_artists_follow() {
    let rig = Rig::new();
    rig.follow("rg-mezzanine", "Massive Attack", "Mezzanine", false);
    let _running = rig.pending.start_running("rg-mezzanine");
    let karmacoma = song(
        "Massive Attack",
        "Karmacoma",
        "Protection",
        "karmacoma",
        RecordingProof::Proven,
    );

    let placement = rig
        .place(
            &karmacoma,
            &RequestedIdentity::new("Massive Attack", "Karmacoma", "Protection", Some(1)),
            DownloadSource::Soulseek,
        )
        .await;

    let entry = rig.pending.get("rg-mezzanine").expect("listed");
    assert_eq!(entry.ended, Some(FollowEnd::ProvenElsewhere));
    let held = rig.pending.held();
    assert_eq!(held.len(), 1);
    assert!(!held[0].kept);
    assert_eq!(held[0].held_path, placement.path);
}

/// With no follow running to remove Octo's items (nothing follows Lidarr right now), a proven
/// file is placed at once, and the follow is asked to end.
#[tokio::test]
async fn a_proven_file_with_no_follow_running_is_placed_at_once() {
    let rig = Rig::new();
    rig.follow("rg-mezzanine", "Massive Attack", "Mezzanine", false);
    let teardrop = song(
        "Massive Attack",
        "Teardrop",
        "Mezzanine",
        "teardrop",
        RecordingProof::Proven,
    );

    let placement = rig
        .place(&teardrop, &teardrop_request(), DownloadSource::Soulseek)
        .await;

    assert_eq!(
        slashed(&placement.path),
        slashed(&format!(
            "{}/Massive Attack/Mezzanine/03 - Teardrop.flac",
            rig.root()
        ))
    );
    assert!(rig.pending.held().is_empty());
    assert_eq!(
        rig.pending.get("rg-mezzanine").expect("listed").ended,
        Some(FollowEnd::ProvenElsewhere)
    );
}

/// Lidarr's own import of Octo's download, proven by its sound, waits with the rest while the
/// follow is open, and never ends the follow it came from.
#[tokio::test]
async fn lidarrs_own_import_waits_and_never_ends_its_follow() {
    let rig = Rig::new();
    rig.follow("rg-mezzanine", "Massive Attack", "Mezzanine", false);
    let _running = rig.pending.start_running("rg-mezzanine");
    let teardrop = song(
        "Massive Attack",
        "Teardrop",
        "Mezzanine",
        "teardrop",
        RecordingProof::Proven,
    );

    rig.place(&teardrop, &teardrop_request(), DownloadSource::Lidarr)
        .await;

    assert!(rig.pending.get("rg-mezzanine").expect("listed").following());
    assert_eq!(rig.pending.held().len(), 1);
}

/// In the shared library a kept copy held for Alice and Bob gives way once Lidarr's file passes
/// the gate of anyone it was kept for (the library holds one file of the song): Bob's proof alone
/// drops its file, what was written beside it and its record.
#[tokio::test]
async fn a_shared_kept_copy_gives_way_to_a_file_proven_for_anyone_it_was_kept_for() {
    let rig = Rig::new();
    let held = rig.held_teardrop(&["alice", "bob"], None).await;
    let lyrics = Path::new(&held.held_path).with_extension("lrc");

    assert!(
        !rig.harness
            .service
            .drop_held_copy(&held, &["carol".to_string()])
            .await,
        "carol's proof is no proof for a copy kept for alice and bob"
    );
    assert!(Path::new(&held.held_path).is_file());

    assert!(
        rig.harness
            .service
            .drop_held_copy(&held, &["Bob".to_string()])
            .await
    );
    assert!(!Path::new(&held.held_path).exists());
    assert!(!lyrics.exists());
    assert!(rig.library.by_tags.lock().is_empty(), "its record went");
}

/// The follow's end checks the proof saved on a kept copy before placing it: in the shared
/// library a copy whose song's Lidarr file passed Bob's gate gives way instead of landing beside
/// that file, and nothing lies at its target.
#[tokio::test]
async fn a_proven_kept_copy_gives_way_at_the_follows_end_in_the_shared_library() {
    let rig = Rig::new();
    let held = rig.held_teardrop(&["alice", "bob"], None).await;

    let end = rig
        .harness
        .service
        .place_held_file(&HeldPlacement {
            proven_for: vec!["bob".into()],
            ..held.clone()
        })
        .await;

    assert_eq!(end, HeldEnd::GaveWay);
    assert!(!Path::new(&held.held_path).exists());
    assert!(!Path::new(&held.target).exists(), "never placed");
    assert!(rig.library.by_tags.lock().is_empty());
}

/// A restart came before the follow saved that Lidarr's Teardrop passed Bob's gate, but the
/// library records that proven file for him: at the follow's end the kept copy gives way to it all
/// the same, never landing beside it.
#[tokio::test]
async fn a_kept_copy_gives_way_to_a_proven_file_the_library_records_for_someone_it_was_kept_for() {
    let rig = Rig::new();
    let held = rig.held_teardrop(&["alice", "bob"], None).await;
    let proven = format!("{}/Massive Attack - Teardrop.flac", rig.root());
    std::fs::write(&proven, flac()).expect("written");
    rig.library.by_tags.lock().push((
        "Massive Attack".into(),
        "Teardrop".into(),
        Some("Mezzanine".into()),
        LocalSongMapping {
            external_provider: "soulseek".into(),
            external_id: "teardrop".into(),
            local_path: proven.clone(),
            music_brainz_recording_id: Some("rec-3".into()),
            heart: Some(KeptHeart {
                users: vec!["bob".into()],
                ..Default::default()
            }),
            ..Default::default()
        },
    ));

    let end = rig.harness.service.place_held_file(&held).await;

    assert_eq!(end, HeldEnd::GaveWay);
    assert!(!Path::new(&held.target).exists(), "never placed");
    assert!(Path::new(&proven).is_file(), "the proven file stays");
    let records: Vec<String> = rig
        .library
        .get_mappings()
        .await
        .into_iter()
        .map(|mapping| mapping.local_path)
        .collect();
    assert_eq!(records, [proven]);
}

/// With Family on, a kept copy held for the members Alice and Bob, whose song's Lidarr file passed
/// Bob's gate alone, lands in the family pool for Alice alone: recorded and linked for her, and
/// its library record names her alone. Had it passed both their gates, it gives way.
#[tokio::test]
async fn a_family_copy_lands_only_for_the_members_whose_gate_did_not_pass_lidarrs_file() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let pool = Arc::new(Pool {
        root: format!("{}/members/.pool", slashed(&dir.path().to_string_lossy())),
        placed: Mutex::new(Vec::new()),
    });
    let rig = Rig::with_family(Some(pool.clone()));
    let pool_root = pool.root.clone();
    let held = rig.held_teardrop(&["alice", "bob"], Some(&pool_root)).await;
    assert!(
        slashed(&held.target).starts_with(&pool_root),
        "{} lies in the pool",
        held.target
    );

    let end = rig
        .harness
        .service
        .place_held_file(&HeldPlacement {
            proven_for: vec!["bob".into()],
            ..held.clone()
        })
        .await;

    assert_eq!(end, HeldEnd::Placed(held.target.clone()));
    assert!(Path::new(&held.target).is_file());
    assert_eq!(
        *pool.placed.lock(),
        [(held.target.clone(), vec!["alice".to_string()])],
        "linked for alice alone"
    );
    let records = rig.library.get_mappings().await;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].heart.as_ref().expect("kept for her").users, ["alice"]);

    let both = Rig::with_family(Some(pool.clone()));
    let held = both.held_teardrop(&["alice", "bob"], Some(&pool_root)).await;
    let end = both
        .harness
        .service
        .place_held_file(&HeldPlacement {
            proven_for: vec!["alice".into(), "bob".into()],
            ..held.clone()
        })
        .await;
    assert_eq!(end, HeldEnd::GaveWay, "nobody is left it is for");
    assert!(!Path::new(&held.held_path).exists());
    assert_eq!(pool.placed.lock().len(), 1, "nothing more lands in the pool");
}

/// In the family pool a kept copy is dropped for Lidarr's proven file only once that file passed
/// the gate of everyone it was kept for; a member still waiting on it keeps it.
#[tokio::test]
async fn a_family_kept_copy_is_dropped_only_for_everyone_it_was_kept_for() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let pool = Arc::new(Pool {
        root: format!("{}/members/.pool", slashed(&dir.path().to_string_lossy())),
        placed: Mutex::new(Vec::new()),
    });
    let pool_root = pool.root.clone();
    let rig = Rig::with_family(Some(pool));
    let held = rig.held_teardrop(&["alice", "bob"], Some(&pool_root)).await;

    assert!(
        !rig.harness
            .service
            .drop_held_copy(&held, &["bob".to_string()])
            .await,
        "alice still waits on it"
    );
    assert!(Path::new(&held.held_path).is_file());
    assert!(
        rig.harness
            .service
            .drop_held_copy(&held, &["alice".to_string(), "bob".to_string()])
            .await
    );
    assert!(!Path::new(&held.held_path).exists());
}

/// A file moved into the library goes only where nothing lies: a file lying there stays as it is,
/// and the moved file stays where it was.
#[test]
fn a_move_never_lands_over_a_file() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let from = dir.path().join("held.flac");
    let to = dir.path().join("03 - Teardrop.flac");
    std::fs::write(&from, b"the kept copy").expect("written");
    std::fs::write(&to, b"Lidarr's import").expect("written");

    let moved = move_file(&from.to_string_lossy(), &to.to_string_lossy());

    assert_eq!(
        moved.expect_err("refused").kind(),
        std::io::ErrorKind::AlreadyExists
    );
    assert_eq!(std::fs::read(&to).expect("still there"), b"Lidarr's import");
    assert_eq!(std::fs::read(&from).expect("still held"), b"the kept copy");

    std::fs::remove_file(&to).expect("removed");
    move_file(&from.to_string_lossy(), &to.to_string_lossy()).expect("moved");
    assert_eq!(std::fs::read(&to).expect("moved"), b"the kept copy");
    assert!(!from.exists());
}

/// A copy (across disks, or where hard links cannot be made) is written under a temporary name in
/// the target's folder and linked into place: a file that lies at the target stays as it is, a
/// copy that fails leaves nothing behind, and the temporary file never stays.
#[test]
fn a_copy_never_lands_over_a_file_or_leaves_half_a_file() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let from = dir.path().join("held.flac");
    let to = dir.path().join("03 - Teardrop.flac");
    std::fs::write(&from, b"the kept copy").expect("written");
    std::fs::write(&to, b"Lidarr's import").expect("written");

    let copied = copy_new(&from, &to);

    assert_eq!(
        copied.expect_err("refused").kind(),
        std::io::ErrorKind::AlreadyExists
    );
    assert_eq!(std::fs::read(&to).expect("still there"), b"Lidarr's import");
    let names = |dir: &Path| {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("read")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    };
    assert_eq!(
        names(dir.path()),
        ["03 - Teardrop.flac", "held.flac"],
        "no temporary file"
    );

    let missing = dir.path().join("gone.flac");
    let elsewhere = dir.path().join("Angel.flac");
    assert!(copy_new(&missing, &elsewhere).is_err());
    assert!(!elsewhere.exists(), "nothing at the target");
    assert_eq!(names(dir.path()), ["03 - Teardrop.flac", "held.flac"]);

    copy_new(&from, &elsewhere).expect("copied");
    assert_eq!(std::fs::read(&elsewhere).expect("copied"), b"the kept copy");
    assert!(from.is_file(), "a copy leaves its source");
}
