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
use crate::services::lidarr::{LidarrPendingAlbums, PendingAlbum, PlacedAs};
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
    assert_eq!(held[0].users, ["alice"], "who may stop waiting for it");
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

    assert!(rig.pending.request_end("rg-mezzanine", FollowEnd::Stopped));
    let free = rig.pending.release("rg-mezzanine");
    assert_eq!(free.len(), 1);
    let placed = rig
        .harness
        .service
        .place_held_file(&free[0])
        .await
        .expect("placed");
    let lyrics_placed = placed.beside.clone();
    let placed = placed.path;

    assert_eq!(slashed(&placed), slashed(&target));
    assert_eq!(
        lyrics_placed.iter().map(|path| slashed(path)).collect::<Vec<_>>(),
        [slashed(
            &Path::new(&target).with_extension("lrc").to_string_lossy()
        )],
        "what its placing moved beside it"
    );
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

/// The library's record of a file of Teardrop at `path`: a kept copy (as heard) or the proven
/// recording `rec-3`, kept for Alice either way.
fn teardrop_record(
    path: &str,
    kept: bool,
) -> (
    String,
    String,
    Option<String>,
    crate::services::local::LocalSongMapping,
) {
    (
        "Massive Attack".into(),
        "Teardrop".into(),
        Some("Mezzanine".into()),
        crate::services::local::LocalSongMapping {
            external_provider: "soulseek".into(),
            external_id: "teardrop".into(),
            local_path: path.to_string(),
            music_brainz_recording_id: (!kept).then(|| "rec-3".to_string()),
            heart: Some(crate::services::local::KeptHeart {
                users: vec!["alice".into()],
                as_heard: kept,
                ..Default::default()
            }),
            ..Default::default()
        },
    )
}

/// Alice's kept copy of Teardrop (`name` its file), placed during the follow of Mezzanine when
/// she stopped waiting, with lyrics written beside it that its placing moved along, a link of it
/// kept in the hold path, and the library's record of it. The copy as the follow lists it.
async fn placed_teardrop(rig: &Rig, name: &str) -> (PlacedHeld, String) {
    rig.follow("rg-mezzanine", "Massive Attack", "Mezzanine", false);
    let landed = rig.landed(name);
    std::fs::write(&landed, b"the version alice heard").expect("written");
    let placement = rig
        .harness
        .service
        .place_or_hold(
            &kept_teardrop(),
            &teardrop_request(),
            &landed,
            None,
            Some(DownloadSource::YouTube),
            &["alice".to_string()],
        )
        .await;
    std::fs::write(
        Path::new(&placement.path).with_extension("lrc"),
        "[00:01.00] alice's lyrics",
    )
    .expect("written");
    let stopped = rig.pending.stop_waiting("soulseek:teardrop", "alice", false);
    assert_eq!(stopped.place.len(), 1);
    let placed = rig
        .harness
        .service
        .place_held_file(&stopped.place[0])
        .await
        .expect("placed");
    assert_eq!(placed.beside.len(), 1, "its lyrics went along");
    let links = held_root(rig.dir.path()).join("lidarr-placed");
    std::fs::create_dir_all(&links).expect("made");
    let link = links.join("alice.link").to_string_lossy().into_owned();
    std::fs::hard_link(&placed.path, &link).expect("linked");
    assert_eq!(
        rig.pending.placed_copy(
            &stopped.place[0],
            &placed.path,
            Some((link.clone(), true)),
            placed.beside.clone(),
        ),
        PlacedAs::Octos
    );
    rig.library
        .by_tags
        .lock()
        .push(teardrop_record(&placed.path, true));
    (placed, link)
}

/// The library's records, by their keys.
fn record_keys(rig: &Rig) -> Vec<String> {
    let mut keys: Vec<String> = rig
        .library
        .by_tags
        .lock()
        .iter()
        .map(|(.., mapping)| mapping.key())
        .collect();
    keys.sort();
    keys
}

/// Alice's placed copy is an `.m4a`; Lidarr's Teardrop, proven by her gate, landed beside it as a
/// `.flac` of the same name, sharing the lyrics file. The copy gives way through the download
/// service's own drop: the `.m4a` and the kept record go, and the proven `.flac`, the lyrics its
/// name shares and its record all stay.
#[tokio::test]
async fn a_placed_copy_gives_way_without_touching_the_proven_file_beside_it() {
    let rig = Rig::new();
    let (placed, _link) = placed_teardrop(&rig, "Teardrop.m4a").await;
    let proven = Path::new(&placed.path).with_extension("flac");
    std::fs::write(&proven, b"Lidarr's proven Teardrop").expect("written");
    rig.library
        .by_tags
        .lock()
        .push(teardrop_record(&proven.to_string_lossy(), false));

    let giving = rig.pending.giving_way("soulseek:teardrop", &["alice".into()]);
    assert_eq!(giving.len(), 1);
    assert!(rig.harness.service.drop_placed_copy(&giving[0]).await);

    assert!(!Path::new(&placed.path).exists(), "the copy Octo placed goes");
    assert!(proven.is_file(), "the proven file stays");
    assert!(
        Path::new(&placed.beside[0]).is_file(),
        "the lyrics its name shares with the proven file stay"
    );
    assert_eq!(record_keys(&rig), ["soulseek:teardrop#recording:rec-3"]);
}

/// Alice's placed copy is a `.flac`, and Lidarr's import put its proven Teardrop at that very path
/// (Octo never learned the copy was replaced), with lyrics of its own. The file there is not the
/// one Octo placed (another file than its link), so nothing on disk goes: only the kept record.
#[tokio::test]
async fn a_placed_copy_never_deletes_a_file_of_the_same_name_it_did_not_place() {
    let rig = Rig::new();
    let (placed, link) = placed_teardrop(&rig, "Teardrop.flac").await;
    std::fs::remove_file(&placed.path).expect("Lidarr deleted it");
    std::fs::write(&placed.path, b"Lidarr's proven Teardrop").expect("written");
    std::fs::write(&placed.beside[0], "[00:01.00] Lidarr's lyrics").expect("written");
    rig.library
        .by_tags
        .lock()
        .push(teardrop_record(&placed.path, false));

    let giving = rig.pending.giving_way("soulseek:teardrop", &["alice".into()]);
    assert_eq!(giving.len(), 1);
    assert!(!giving[0].replaced);
    assert!(rig.harness.service.drop_placed_copy(&giving[0]).await);

    assert_eq!(
        std::fs::read(&placed.path).expect("still there"),
        b"Lidarr's proven Teardrop"
    );
    assert!(Path::new(&placed.beside[0]).is_file(), "its lyrics stay");
    assert_eq!(record_keys(&rig), ["soulseek:teardrop#recording:rec-3"]);
    assert!(Path::new(&link).is_file(), "the caller lets go of the link");
}

/// The copy is still the one Octo placed, alone in its folder: it goes with the lyrics its placing
/// moved beside it and its kept record, and a file of the user's in that folder stays.
#[tokio::test]
async fn a_placed_copy_alone_goes_with_what_its_placing_moved_beside_it() {
    let rig = Rig::new();
    let (placed, _link) = placed_teardrop(&rig, "Teardrop.flac").await;
    let users = Path::new(&placed.path).with_file_name("03 - Teardrop (Live).flac");
    std::fs::write(&users, b"the user's live take").expect("written");

    let giving = rig.pending.giving_way("soulseek:teardrop", &["alice".into()]);
    assert!(rig.harness.service.drop_placed_copy(&giving[0]).await);

    assert!(!Path::new(&placed.path).exists());
    assert!(!Path::new(&placed.beside[0]).exists());
    assert!(users.is_file());
    assert!(record_keys(&rig).is_empty());
}

/// A copy Lidarr's import replaced (Octo learned so) lets go of its kept record alone: the file at
/// its path, Lidarr's, stays.
#[tokio::test]
async fn a_replaced_placed_copy_lets_go_of_its_record_alone() {
    let rig = Rig::new();
    let (placed, _link) = placed_teardrop(&rig, "Teardrop.flac").await;
    std::fs::remove_file(&placed.path).expect("Lidarr deleted it");
    assert!(rig.pending.mark_replaced(&placed.path));
    std::fs::write(&placed.path, b"Lidarr's proven Teardrop").expect("written");

    let giving = rig.pending.giving_way("soulseek:teardrop", &["alice".into()]);
    assert!(giving[0].replaced);
    assert!(rig.harness.service.drop_placed_copy(&giving[0]).await);

    assert!(Path::new(&placed.path).is_file());
    assert!(Path::new(&placed.beside[0]).is_file());
    assert!(record_keys(&rig).is_empty());
}

/// A copy Lidarr's import replaced comes back from its link where it was. Where the song's own
/// landed file for its people lies now, it gives way; where another file lies, it waits for a
/// later try and is never put beside it.
#[tokio::test]
async fn a_replaced_placed_copy_comes_back_where_it_was_or_waits() {
    let rig = Rig::new();
    let (placed, link) = placed_teardrop(&rig, "Teardrop.flac").await;
    std::fs::remove_file(&placed.path).expect("Lidarr deleted it");
    assert!(rig.pending.mark_replaced(&placed.path));
    let copy = rig.pending.replaced_copies(None, None).remove(0);

    std::fs::write(&placed.path, b"a file of the user's").expect("written");
    assert_eq!(
        rig.harness.service.restore_placed_copy(&copy).await,
        Restored::Later
    );
    let folder = Path::new(&placed.path).parent().expect("its folder");
    assert_eq!(
        std::fs::read_dir(folder)
            .expect("read")
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|e| e == "flac"))
            .count(),
        1,
        "never put beside it"
    );

    rig.library
        .by_tags
        .lock()
        .push(teardrop_record(&placed.path, false));
    assert_eq!(
        rig.harness.service.restore_placed_copy(&copy).await,
        Restored::GaveWay
    );

    std::fs::remove_file(&placed.path).expect("gone");
    assert_eq!(
        rig.harness.service.restore_placed_copy(&copy).await,
        Restored::Back
    );
    assert_eq!(
        std::fs::read(&placed.path).expect("back"),
        std::fs::read(&link).expect("its link")
    );
}

/// A kept copy that waits for a follow gives way to a proven copy of its song only when that copy
/// passed the gate of everyone it was kept for: its file, what was written beside it and its
/// library record go. A copy kept for someone else too stays as it is.
#[tokio::test]
async fn a_kept_copy_is_dropped_for_a_proven_copy_only_for_everyone_it_was_kept_for() {
    let rig = Rig::new();
    rig.follow("rg-mezzanine", "Massive Attack", "Mezzanine", false);
    let placement = rig
        .place(&kept_teardrop(), &teardrop_request(), DownloadSource::YouTube)
        .await;
    let held = rig.pending.held().remove(0);
    let lyrics = Path::new(&placement.path).with_extension("lrc");
    std::fs::write(&lyrics, "[00:01.00] love, love is a verb").expect("written");
    rig.library.by_tags.lock().push((
        "Massive Attack".into(),
        "Teardrop".into(),
        Some("Mezzanine".into()),
        crate::services::local::LocalSongMapping {
            external_provider: "soulseek".into(),
            external_id: "teardrop".into(),
            local_path: placement.path.clone(),
            heart: Some(crate::services::local::KeptHeart {
                users: vec!["alice".into(), "bob".into()],
                as_heard: true,
                ..Default::default()
            }),
            ..Default::default()
        },
    ));

    assert!(
        !rig.harness
            .service
            .drop_held_copy(&held, &["alice".to_string()])
            .await,
        "it is Bob's too"
    );
    assert!(Path::new(&placement.path).is_file());
    assert_eq!(rig.library.by_tags.lock().len(), 1);

    assert!(
        rig.harness
            .service
            .drop_held_copy(&held, &["Alice".to_string(), "bob".to_string()])
            .await
    );
    assert!(!Path::new(&placement.path).exists());
    assert!(!lyrics.exists());
    assert!(rig.library.by_tags.lock().is_empty(), "its record went");
}

/// Alice stopped waiting with a library of her own, so a copy of the kept file landed there for
/// her, and the original waits for Bob alone though its record still names her: a proven copy
/// that passed Bob's gate makes it give way.
#[tokio::test]
async fn a_kept_copy_someone_took_a_copy_of_gives_way_for_the_rest() {
    let rig = Rig::new();
    rig.follow("rg-mezzanine", "Massive Attack", "Mezzanine", false);
    let placement = rig
        .place(&kept_teardrop(), &teardrop_request(), DownloadSource::YouTube)
        .await;
    let held = HeldPlacement {
        users: vec!["bob".into()],
        own_copies: vec!["alice".into()],
        ..rig.pending.held().remove(0)
    };
    rig.library.by_tags.lock().push((
        "Massive Attack".into(),
        "Teardrop".into(),
        Some("Mezzanine".into()),
        crate::services::local::LocalSongMapping {
            external_provider: "soulseek".into(),
            external_id: "teardrop".into(),
            local_path: placement.path.clone(),
            heart: Some(crate::services::local::KeptHeart {
                users: vec!["alice".into(), "bob".into()],
                as_heard: true,
                ..Default::default()
            }),
            ..Default::default()
        },
    ));

    assert!(
        rig.harness
            .service
            .drop_held_copy(&held, &["bob".to_string()])
            .await
    );
    assert!(!Path::new(&placement.path).exists());
}
