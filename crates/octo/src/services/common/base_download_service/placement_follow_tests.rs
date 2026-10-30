//! While a Lidarr follow of Octo's is open on an artist, a file Octo would place under one of
//! Lidarr's root folders for that artist waits in the one hold path, plays from there, and is
//! placed when the follow ends: no rescan can link it into the artist while Octo's own import is
//! coming. A file another source proved by its sound ends the artist's follows instead.

use std::path::Path;
use std::sync::Arc;

use octo_core::common::Clock;
use octo_core::fingerprint::{VerificationResult, VerificationVerdict};
use octo_core::settings::{AppSettings, SubsonicSettings};
use tempfile::TempDir;

use super::super::test_support::{Harness, build};
use super::super::{DownloadServices, RequestedIdentity};
use super::*;
use crate::services::common::AcquisitionTracker;
use crate::services::lidarr::{LidarrPendingAlbums, PendingAlbum};
use crate::services::local::test_support::FakeLocalLibrary;
use crate::services::test_support::flac;

struct Rig {
    dir: TempDir,
    harness: Harness,
    pending: Arc<LidarrPendingAlbums>,
    library: Arc<FakeLocalLibrary>,
    tracker: Arc<AcquisitionTracker>,
}

impl Rig {
    fn new() -> Rig {
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
            Some(library.clone()),
            None,
            DownloadServices {
                lidarr_pending: Some(pending.clone()),
                tracker: Some(tracker.clone()),
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
            .place_or_hold(song, requested, &landed, None, Some(source))
            .await
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
    let notes: Vec<String> = rig
        .tracker
        .detail("soulseek:teardrop", None)
        .and_then(|row| row.events)
        .unwrap_or_default()
        .into_iter()
        .map(|event| event.text)
        .collect();
    assert!(notes.iter().any(|note| note == HELD_FOR_LIDARR), "{notes:?}");
    // Lyrics written beside the held file go along with it.
    let lyrics = Path::new(&placement.path).with_extension("lrc");
    std::fs::write(&lyrics, "[00:01.00] love, love is a verb").expect("written");

    assert!(rig.pending.request_end("rg-mezzanine", FollowEnd::Stopped));
    let free = rig.pending.release("rg-mezzanine");
    assert_eq!(free.len(), 1);
    let placed = rig
        .harness
        .service
        .place_held_file(&free[0])
        .await
        .expect("placed");

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
