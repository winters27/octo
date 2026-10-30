//! A download is placed after it is tagged and from the Song (#48), a list of artists never names
//! a folder (#49), and a path that is already taken is only ever replaced by the same song. Each
//! of these used to be decided from the search request before anything knew what the file was.
//!
//! The tag writer's recording id and multi-value tests are in
//! `octo_media::tags::tag_writer_extras_tests`, beside the frame models they read.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::FutureExt;
use octo_core::fingerprint::verification::RecordingProof;
use octo_core::fingerprint::{
    AcoustIdRecording, InconclusiveReason, VerificationResult, VerificationVerdict,
};
use octo_core::models::domain::Song;
use octo_core::settings::{AppSettings, FolderStructure, SubsonicSettings};
use octo_media::tags::{KeptIdentity, KeptIdentityTags, TagFile, tag_writer_extras};
use parking_lot::Mutex;

use super::test_support::{Harness, build};
use super::*;
use crate::services::local::LocalSongMapping;
use crate::services::local::test_support::FakeLocalLibrary;
use crate::services::soulseek::soulseek_download_service::INCOMING_FOLDER_NAME;
use crate::services::test_support::{flac, mp3};

fn requested(artist: &str, title: &str, album: &str, track: Option<i32>) -> RequestedIdentity {
    RequestedIdentity::new(artist, title, album, track)
}

fn confirmed(matched: AcoustIdRecording) -> Box<VerificationResult> {
    Box::new(VerificationResult {
        verdict: VerificationVerdict::Confirmed,
        recording_id: Some(matched.recording_id.clone()),
        r#match: Some(matched),
        ..Default::default()
    })
}

fn song(artist: &str, title: &str) -> Song {
    Song {
        artist: artist.into(),
        title: title.into(),
        ..Default::default()
    }
}

// ---- PrimaryCredit: split only on proof ---------------------------------------------------

#[test]
fn primary_credit_whole_string_named_by_a_source_is_kept_whole() {
    for (requested, structured) in [
        ("Earth, Wind & Fire", "Earth, Wind & Fire"),
        ("Tyler, The Creator", "Tyler, The Creator"),
        ("Simon & Garfunkel", "Simon & Garfunkel"),
    ] {
        assert_eq!(
            BaseDownloadService::primary_credit(requested, &[Some(structured)]),
            requested,
            "{requested}"
        );
    }
}

#[test]
fn primary_credit_source_names_the_first_credit_splits_there() {
    for (requested, structured, expected) in [
        ("Bizarrap, Rauw Alejandro", "Bizarrap", "Bizarrap"),
        ("Kavinsky & Lovefoxxx", "Kavinsky", "Kavinsky"),
        ("Bizarrap x Rauw Alejandro", "Bizarrap", "Bizarrap"),
        ("Drake feat. Rihanna", "Drake", "Drake"),
    ] {
        assert_eq!(
            BaseDownloadService::primary_credit(requested, &[Some(structured)]),
            expected,
            "{requested}"
        );
    }
}

/// "Tyler, The Creator" would become "Tyler" under any rule that splits on a comma.
#[test]
fn primary_credit_no_structured_source_never_splits() {
    for requested in ["Tyler, The Creator", "Bizarrap, Rauw Alejandro"] {
        assert_eq!(
            BaseDownloadService::primary_credit(requested, &[None, None]),
            requested
        );
    }
}

#[test]
fn primary_credit_source_naming_a_later_credit_does_not_split() {
    assert_eq!(
        BaseDownloadService::primary_credit("Bizarrap, Rauw Alejandro", &[Some("Rauw Alejandro")]),
        "Bizarrap, Rauw Alejandro"
    );
}

/// A source that names the whole credit wins over one that names only its first artist.
#[test]
fn primary_credit_any_source_naming_the_whole_credit_keeps_it_whole() {
    assert_eq!(
        BaseDownloadService::primary_credit(
            "Tyler, The Creator",
            &[Some("Tyler"), Some("Tyler, The Creator")]
        ),
        "Tyler, The Creator"
    );
}

// ---- ChooseLayout -------------------------------------------------------------------------

#[test]
fn choose_layout_name_from_match_off_uses_the_request_but_the_primary_folder() {
    let song = Song {
        primary_artist: Some("Bizarrap".into()),
        ..self::song("Bizarrap, Rauw Alejandro", "Tagged Title")
    };
    let choice = BaseDownloadService::choose_layout(
        &song,
        &requested(
            "Bizarrap, Rauw Alejandro",
            "Requested Title",
            "Requested Album",
            Some(4),
        ),
        false,
    );

    assert_eq!(choice.folder_artist, "Bizarrap");
    assert_eq!(choice.file_artist, "Bizarrap, Rauw Alejandro");
    assert_eq!(choice.title, "Requested Title");
    assert_eq!(choice.album, "Requested Album");
    assert_eq!(choice.track, Some(4));
}

#[test]
fn choose_layout_name_from_match_on_confirmed_uses_the_recording() {
    let matched = AcoustIdRecording::new(
        "rec",
        "Teardrop",
        ["Massive Attack"],
        Some("Mezzanine"),
        Some(1998),
    );
    let song = Song {
        album: "Mezzanine".into(),
        track: Some(3),
        verification: Some(confirmed(matched)),
        ..self::song("Massive Attack", "Teardrop")
    };
    let choice = BaseDownloadService::choose_layout(
        &song,
        &requested("massive attack", "Teardrop (Official Video)", "", None),
        true,
    );

    assert_eq!(choice.folder_artist, "Massive Attack");
    assert_eq!(choice.title, "Teardrop");
    assert_eq!(choice.album, "Mezzanine");
    assert_eq!(choice.track, Some(3));
}

#[test]
fn choose_layout_name_from_match_on_inconclusive_uses_the_request() {
    let song = Song {
        album: "Tagged Album".into(),
        verification: Some(Box::new(VerificationResult {
            reason: InconclusiveReason::NoEntry,
            ..Default::default()
        })),
        ..self::song("X", "Tagged")
    };
    let choice = BaseDownloadService::choose_layout(
        &song,
        &requested("Req Artist", "Req Title", "Req Album", None),
        true,
    );

    assert_eq!(choice.folder_artist, "Req Artist");
    assert_eq!(choice.title, "Req Title");
    assert_eq!(choice.album, "Req Album");
}

/// A request with no album takes the album it was tagged with, and that album's track (#50).
#[test]
fn choose_layout_request_without_album_takes_the_tagged_album_and_its_track() {
    let song = Song {
        album: "Deezer Album".into(),
        track: Some(7),
        ..self::song("A", "T")
    };
    let choice = BaseDownloadService::choose_layout(&song, &requested("A", "T", "", None), false);

    assert_eq!(choice.album, "Deezer Album");
    assert_eq!(choice.track, Some(7));
}

// ---- placing in the library---------------------------------------------------------------

struct Root {
    dir: tempfile::TempDir,
}

impl Root {
    fn new() -> Root {
        Root {
            dir: tempfile::tempdir().expect("a temp dir"),
        }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn join(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn text(&self, name: &str) -> String {
        self.join(name).to_string_lossy().into_owned()
    }

    /// The service with the transfer stubbed out, in this layout.
    fn service(&self, layout: FolderStructure, mappings: Vec<LocalSongMapping>) -> Harness {
        let library = FakeLocalLibrary::default();
        for mapping in mappings {
            library
                .by_tags
                .lock()
                .push((mapping.artist.clone(), mapping.title.clone(), None, mapping));
        }
        build(
            self.path(),
            AppSettings {
                subsonic: SubsonicSettings {
                    folder_structure: layout,
                    ..Default::default()
                },
                ..Default::default()
            },
            Some(Arc::new(library)),
            None,
            DownloadServices::default(),
        )
    }

    /// A file a peer delivered, in the peer's own folders.
    fn landed(&self, name: &str, bytes: &[u8]) -> String {
        let dir = self.join("peer share").join("some folder");
        std::fs::create_dir_all(&dir).expect("made");
        let path = dir.join(name);
        std::fs::write(&path, bytes).expect("written");
        path.to_string_lossy().into_owned()
    }
}

async fn place(harness: &Harness, song: &Song, requested: &RequestedIdentity, path: &str) -> Placement {
    harness.service.place_in_library(song, requested, path).await
}

#[tokio::test]
async fn place_in_library_organized_files_under_the_primary_artist_and_keeps_the_version() {
    let root = Root::new();
    let harness = root.service(FolderStructure::Organized, vec![]);
    let landed = root.landed("x.mp3", &mp3());
    let song = Song {
        primary_artist: Some("Bizarrap".into()),
        ..song("Bizarrap, Rauw Alejandro", "Session (Live)")
    };

    let placement = place(
        &harness,
        &song,
        &requested("Bizarrap, Rauw Alejandro", "Session (Live)", "An Album", Some(2)),
        &landed,
    )
    .await;

    assert_eq!(
        placement.path,
        root.text("Bizarrap/An Album/02 - Session (Live).mp3")
    );
    assert!(Path::new(&placement.path).is_file());
    assert!(placement.created_folder);
    // The peer's own folders are gone once empty; the music root is never touched.
    assert!(!root.join("peer share").exists());
    assert!(root.path().is_dir());
}

/// Flat has no folder to scatter, so the whole credit stays in the file name (#49).
#[tokio::test]
async fn place_in_library_flat_keeps_the_whole_credit_in_the_file_name() {
    let root = Root::new();
    let harness = root.service(FolderStructure::Flat, vec![]);
    let song = Song {
        primary_artist: Some("Bizarrap".into()),
        ..song("Bizarrap, Rauw Alejandro", "T")
    };

    let placement = place(
        &harness,
        &song,
        &requested("Bizarrap, Rauw Alejandro", "T", "", None),
        &root.landed("x.mp3", &mp3()),
    )
    .await;

    assert_eq!(placement.path, root.text("Bizarrap, Rauw Alejandro - T.mp3"));
}

/// The bug this replaces: "Song (Live)" used to be named "Song" and delete it.
#[tokio::test]
async fn place_in_library_foreign_file_at_the_target_keeps_both() {
    let root = Root::new();
    let harness = root.service(FolderStructure::Flat, vec![]);
    let existing = root.join("A - Song.mp3");
    std::fs::write(&existing, mp3()).expect("written");
    let before = std::fs::read(&existing).expect("read");

    let placement = place(
        &harness,
        &song("A", "Song"),
        &requested("A", "Song", "", None),
        &root.landed("new.mp3", &mp3()),
    )
    .await;

    assert_eq!(placement.path, root.text("A - Song (1).mp3"));
    assert_eq!(std::fs::read(&existing).expect("read"), before);
}

/// A second download of a song Octo itself placed replaces it rather than doubling it.
#[tokio::test]
async fn place_in_library_octo_owned_same_song_replaces() {
    let root = Root::new();
    let existing = root.text("A - Song.mp3");
    std::fs::write(&existing, mp3()).expect("written");
    let harness = root.service(
        FolderStructure::Flat,
        vec![LocalSongMapping {
            local_path: existing.clone(),
            artist: "A".into(),
            title: "Song".into(),
            ..Default::default()
        }],
    );

    let placement = place(
        &harness,
        &song("A", "Song"),
        &requested("A", "Song", "", None),
        &root.landed("new.mp3", &mp3()),
    )
    .await;

    assert_eq!(placement.path, existing);
    assert!(!root.join("A - Song (1).mp3").exists());
}

fn tagged_with_recording(path: &Path, recording: &str) {
    std::fs::write(path, mp3()).expect("written");
    let mut file = TagFile::open(path).expect("opens");
    tag_writer_extras::set_recording_id(&mut file, recording);
    file.save().expect("saved");
}

#[tokio::test]
async fn place_in_library_same_recording_id_replaces() {
    let root = Root::new();
    let harness = root.service(FolderStructure::Flat, vec![]);
    let existing = root.join("A - Song.mp3");
    tagged_with_recording(&existing, "rec-1");

    let song = Song {
        music_brainz_recording_id: Some("rec-1".into()),
        ..song("A", "Song")
    };
    let placement = place(
        &harness,
        &song,
        &requested("A", "Song", "", None),
        &root.landed("new.mp3", &mp3()),
    )
    .await;

    assert_eq!(placement.path, existing.to_string_lossy());
}

#[tokio::test]
async fn place_in_library_different_recording_id_keeps_both() {
    let root = Root::new();
    let harness = root.service(FolderStructure::Flat, vec![]);
    tagged_with_recording(&root.join("A - Song.mp3"), "rec-1");

    let song = Song {
        music_brainz_recording_id: Some("rec-2".into()),
        ..song("A", "Song")
    };
    let placement = place(
        &harness,
        &song,
        &requested("A", "Song", "", None),
        &root.landed("new.mp3", &mp3()),
    )
    .await;

    assert_eq!(placement.path, root.text("A - Song (1).mp3"));
}

/// A path with forward slashes, as the layout writes it on every platform.
fn slashed(path: &str) -> String {
    path.replace('\\', "/")
}

fn proven_as(proof: RecordingProof) -> Option<Box<VerificationResult>> {
    Some(Box::new(VerificationResult {
        verdict: VerificationVerdict::Confirmed,
        proof,
        ..Default::default()
    }))
}

fn octo_file(path: &str, artist: &str, title: &str) -> LocalSongMapping {
    LocalSongMapping {
        local_path: path.to_string(),
        artist: artist.into(),
        title: title.into(),
        ..Default::default()
    }
}

/// An import the shipped checks kept while its sound could not settle the version carries no
/// recording id, and is still the same song as Octo's own file of that artist and title.
#[tokio::test]
async fn an_ambiguous_import_of_a_song_octo_downloaded_is_the_same_song() {
    let root = Root::new();
    let existing = root.text("A - Song.mp3");
    tagged_with_recording(Path::new(&existing), "rec-1");
    let harness = root.service(FolderStructure::Flat, vec![octo_file(&existing, "A", "Song")]);
    let import = Song {
        verification: proven_as(RecordingProof::NotProven),
        ..song("A", "Song")
    };

    let placement = place(
        &harness,
        &import,
        &requested("A", "Song", "", None),
        &root.landed("new.mp3", &mp3()),
    )
    .await;

    assert_eq!(slashed(&placement.path), slashed(&existing));
    assert!(!root.join("A - Song (1).mp3").exists());
}

/// The same import beside a file kept as heard lands beside it: the kept file is the very audio a
/// person heard, never the same song as another file by its name.
#[tokio::test]
async fn an_ambiguous_import_beside_a_file_kept_as_heard_lands_beside_it() {
    let root = Root::new();
    let existing = root.text("A - Song.mp3");
    std::fs::write(&existing, mp3()).expect("written");
    let before = std::fs::read(&existing).expect("read");
    let kept = LocalSongMapping {
        proof_mark: Some("as heard on YouTube".into()),
        ..octo_file(&existing, "A", "Song")
    };
    let harness = root.service(FolderStructure::Flat, vec![kept]);
    let import = Song {
        verification: proven_as(RecordingProof::NotProven),
        ..song("A", "Song")
    };

    let placement = place(
        &harness,
        &import,
        &requested("A", "Song", "", None),
        &root.landed("new.mp3", &mp3()),
    )
    .await;

    assert_eq!(slashed(&placement.path), slashed(&root.text("A - Song (1).mp3")));
    assert_eq!(std::fs::read(&existing).expect("read"), before);
}

/// A file a heart keeps as heard never replaces Octo's own file of the song, and a file the sound
/// proved never replaces one whose recording is not known: each is another version.
#[tokio::test]
async fn a_file_kept_as_heard_or_proven_never_replaces_an_unproven_file_by_name() {
    for proof in [
        RecordingProof::AsHeard {
            video_id: Some("v2015".into()),
        },
        RecordingProof::Proven,
    ] {
        let root = Root::new();
        let existing = root.text("A - Song.mp3");
        std::fs::write(&existing, mp3()).expect("written");
        let harness = root.service(FolderStructure::Flat, vec![octo_file(&existing, "A", "Song")]);
        let kept = Song {
            verification: proven_as(proof.clone()),
            music_brainz_recording_id: (proof == RecordingProof::Proven).then(|| "rec-2009".into()),
            ..song("A", "Song")
        };

        let placement = place(
            &harness,
            &kept,
            &requested("A", "Song", "", None),
            &root.landed("new.mp3", &mp3()),
        )
        .await;

        assert_eq!(
            slashed(&placement.path),
            slashed(&root.text("A - Song (1).mp3")),
            "{proof:?}"
        );
        assert!(Path::new(&existing).is_file(), "{proof:?}");
    }
}

/// A file a heart's gate passed remembers that heart: whose gate passed it, the R it was judged
/// against, what they heard, whether it is kept as heard, and its own print for a later check.
/// A file no gate judged remembers no heart.
#[test]
fn a_kept_file_remembers_the_heart_whose_gate_passed_it() {
    use crate::services::common::{AcquisitionPlace, GateLedger, GateOutcome};
    use octo_core::fingerprint::heart_gate::{
        AcquisitionOrigin, HeardReading, HeardRecord, HeardRecording, HeardSource, HeartGate,
    };

    let heard = HeardRecord {
        heard_at: chrono::Utc::now(),
        source: HeardSource::Video {
            id: "v2015".into(),
            size: Some(1),
            content_type: "audio/mp4".into(),
            itag: None,
        },
        sample_id: Some("s".into()),
    };
    let r = HeardRecording {
        ids: vec!["rec-2009".into()],
        ..Default::default()
    };
    let gate = |user: &str| HeartGate {
        user: user.into(),
        origin: AcquisitionOrigin::Heart,
        r: Some(r.clone()),
        heard: Some(heard.clone()),
        heard_seconds: Some(139),
        reading: HeardReading::OnlyHeardOrMaster,
        stand_in: None,
    };
    let ledger = GateLedger::default();
    ledger.record("bo", AcquisitionOrigin::Heart, GateOutcome::Accepted);
    ledger.record("cy", AcquisitionOrigin::Heart, GateOutcome::Undecided);
    let place = AcquisitionPlace {
        origin: Some(AcquisitionOrigin::Heart),
        gates: vec![gate("cy"), gate("bo")],
        ledger,
        ..Default::default()
    };
    let song = Song {
        verification: Some(Box::new(VerificationResult {
            proof: RecordingProof::AsHeard {
                video_id: Some("v2015".into()),
            },
            fingerprint: Some("AQAD".into()),
            duration_seconds: 139,
            ..Default::default()
        })),
        ..song("The Beatles", "Help!")
    };

    let kept = super::kept_heart(&place, &song).expect("a heart passed it");
    assert_eq!(kept.users, ["bo"]);
    assert_eq!(kept.recording, Some(r));
    assert_eq!(kept.heard, Some(heard));
    assert!(kept.as_heard);
    assert_eq!(kept.print.as_deref(), Some("AQAD"));
    assert_eq!(kept.seconds, Some(139));

    assert_eq!(super::kept_heart(&AcquisitionPlace::default(), &song), None);
}

/// Two people kept two mixes of one song as they heard them: both files stay under the one album
/// path, side by side, and each person's lookup of the song finds their own.
#[tokio::test]
async fn two_kept_as_heard_mixes_of_one_song_both_stay_and_each_person_finds_their_own() {
    use crate::services::local::{ILocalLibraryService, KeptHeart, LocalLibraryService};
    use octo_core::fingerprint::heart_gate::{HeardRecord, HeardSource};

    let root = Root::new();
    let store = Arc::new(octo_core::settings::SettingsStore::for_tests(
        AppSettings::default(),
    ));
    store.set_raw("Library:DownloadPath", Some(&root.path().to_string_lossy()));
    let http = reqwest::Client::new();
    let library = Arc::new(LocalLibraryService::new(
        store.clone(),
        http.clone(),
        Arc::new(crate::services::soulseek::ExternalIdRegistry::in_memory()),
        crate::services::subsonic::NavidromeIdentityService::new(store, http),
    ));
    let harness = build(
        root.path(),
        AppSettings {
            subsonic: SubsonicSettings {
                folder_structure: FolderStructure::Organized,
                ..Default::default()
            },
            ..Default::default()
        },
        Some(library.clone()),
        None,
        DownloadServices::default(),
    );
    let mix = |video: &str| Song {
        external_provider: Some("soulseek".into()),
        external_id: Some("help".into()),
        verification: proven_as(RecordingProof::AsHeard {
            video_id: Some(video.into()),
        }),
        ..song("The Beatles", "Help!")
    };
    let kept_for = |user: &str, video: &str| KeptHeart {
        users: vec![user.into()],
        heard: Some(HeardRecord {
            heard_at: chrono::Utc::now(),
            source: HeardSource::Video {
                id: video.into(),
                size: Some(1),
                content_type: "audio/mp4".into(),
                itag: Some(140),
            },
            sample_id: None,
        }),
        as_heard: true,
        ..Default::default()
    };
    let asked = requested("The Beatles", "Help!", "Help!", Some(1));

    let ana = place(&harness, &mix("v2009"), &asked, &root.landed("a.m4a", &mp3())).await;
    library
        .register_downloaded_song_as(&mix("v2009"), &ana.path, Some(kept_for("ana", "v2009")))
        .await
        .expect("registered");
    let bo = place(&harness, &mix("v2015"), &asked, &root.landed("b.m4a", &mp3())).await;
    library
        .register_downloaded_song_as(&mix("v2015"), &bo.path, Some(kept_for("bo", "v2015")))
        .await
        .expect("registered");

    assert_eq!(
        slashed(&ana.path),
        slashed(&root.text("The Beatles/Help!/01 - Help!.m4a"))
    );
    assert_eq!(
        slashed(&bo.path),
        slashed(&root.text("The Beatles/Help!/01 - Help! (1).m4a"))
    );
    assert!(Path::new(&ana.path).is_file() && Path::new(&bo.path).is_file());
    let lookup = |user: Option<&'static str>| {
        let library = library.clone();
        async move {
            library
                .get_local_path_for_external_song_as(user, "soulseek", "help")
                .await
        }
    };
    assert_eq!(lookup(Some("ana")).await, Some(ana.path.clone()));
    assert_eq!(lookup(Some("BO")).await, Some(bo.path.clone()));
    assert_eq!(
        lookup(None).await,
        Some(bo.path.clone()),
        "the most recently landed"
    );
    assert_eq!(library.get_mappings().await.len(), 2);
}

#[tokio::test]
async fn place_in_library_into_an_album_folder_that_already_has_music_is_not_a_new_folder() {
    let root = Root::new();
    let harness = root.service(FolderStructure::Organized, vec![]);
    let album_dir = root.join("A/Album");
    std::fs::create_dir_all(&album_dir).expect("made");
    std::fs::write(album_dir.join("01 - Other.mp3"), mp3()).expect("written");

    let placement = place(
        &harness,
        &song("A", "T"),
        &requested("A", "T", "Album", Some(2)),
        &root.landed("x.mp3", &mp3()),
    )
    .await;

    assert!(!placement.created_folder);
}

#[tokio::test]
async fn place_in_library_missing_file_returns_the_landed_path() {
    let root = Root::new();
    let harness = root.service(FolderStructure::Flat, vec![]);
    let missing = root.text("nope.mp3");

    let placement = place(
        &harness,
        &song("A", "T"),
        &requested("A", "T", "", None),
        &missing,
    )
    .await;

    assert_eq!(placement.path, missing);
}

// ---- Tags ---------------------------------------------------------------------------------

#[tokio::test]
async fn write_metadata_confirmed_match_writes_ids_artists_and_no_album_id() {
    let root = Root::new();
    let harness = root.service(FolderStructure::Flat, vec![]);
    let path = root.text("t.flac");
    std::fs::write(&path, flac()).expect("written");
    let mut song = Song {
        album: "Session".into(),
        primary_artist: Some("Bizarrap".into()),
        artists: vec!["Bizarrap".into(), "Rauw Alejandro".into()],
        music_brainz_recording_id: Some("rec-1".into()),
        music_brainz_release_id: Some("rel-1".into()),
        music_brainz_release_group_id: Some("rg-1".into()),
        music_brainz_album_title: Some("Session".into()),
        ..self::song("Bizarrap, Rauw Alejandro", "Session")
    };

    harness.service.write_metadata(&path, &mut song).await;

    assert_eq!(vorbis(&path, "MUSICBRAINZ_TRACKID").as_deref(), Some("rec-1"));
    assert_eq!(vorbis_all(&path, "ARTISTS"), ["Bizarrap", "Rauw Alejandro"]);
    let read = TagFile::open(&path).expect("opens");
    assert_eq!(read.music_brainz_release_group_id().as_deref(), Some("rg-1"));
    // Navidrome groups albums by MUSICBRAINZ_ALBUMID before the album name.
    assert!(read.music_brainz_release_id().is_none_or(|id| id.is_empty()));
    // With no album artist of its own, the first credit stands in, not the list.
    assert_eq!(read.album_artists(), ["Bizarrap"]);
}

/// A group id beside an album name from somewhere else would describe another album.
#[tokio::test]
async fn write_metadata_album_from_another_source_gets_no_group_id() {
    let root = Root::new();
    let harness = root.service(FolderStructure::Flat, vec![]);
    let path = root.text("t.flac");
    std::fs::write(&path, flac()).expect("written");
    let mut song = Song {
        album: "Now That's What I Call Music! 42".into(),
        music_brainz_recording_id: Some("rec-1".into()),
        music_brainz_release_group_id: Some("rg-1".into()),
        music_brainz_album_title: Some("The Real Album".into()),
        ..self::song("A", "Song")
    };

    harness.service.write_metadata(&path, &mut song).await;

    let read = TagFile::open(&path).expect("opens");
    assert!(
        read.music_brainz_release_group_id()
            .is_none_or(|id| id.is_empty())
    );
}

/// Every value of one Vorbis field of a FLAC, read with lofty itself.
pub(super) fn vorbis_all(path: &str, field: &str) -> Vec<String> {
    use lofty::file::AudioFile;
    let mut reader = std::fs::File::open(path).expect("opens");
    let flac =
        lofty::flac::FlacFile::read_from(&mut reader, lofty::config::ParseOptions::new()).expect("a FLAC");
    flac.vorbis_comments()
        .map(|comments| {
            comments
                .items()
                .filter(|(key, _)| key.eq_ignore_ascii_case(field))
                .map(|(_, value)| value.to_string())
                .collect()
        })
        .unwrap_or_default()
}

pub(super) fn vorbis(path: &str, field: &str) -> Option<String> {
    vorbis_all(path, field).into_iter().next()
}

// ---- A track that arrives without an album (#50) ------------------------------------------

#[test]
fn apply_single_fallback_no_album_files_under_the_title() {
    let mut song = Song {
        primary_artist: Some("Bizarrap".into()),
        ..song("Bizarrap, Rauw Alejandro", "Session 56")
    };
    BaseDownloadService::apply_single_fallback(&mut song, true);

    assert_eq!(song.album, "Session 56");
    assert_eq!(song.album_artist.as_deref(), Some("Bizarrap"));
}

#[test]
fn apply_single_fallback_compilation_leaves_it_empty() {
    let mut song = Song {
        is_compilation: true,
        ..song("A", "T")
    };
    BaseDownloadService::apply_single_fallback(&mut song, true);
    assert_eq!(song.album, "");
}

#[test]
fn apply_single_fallback_various_artists_album_artist_leaves_it_empty() {
    for album_artist in ["Various Artists", "various", "VA"] {
        let mut song = Song {
            album_artist: Some(album_artist.into()),
            ..song("A", "T")
        };
        BaseDownloadService::apply_single_fallback(&mut song, true);
        assert_eq!(song.album, "", "{album_artist}");
    }
}

#[test]
fn apply_single_fallback_off_changes_nothing() {
    let mut song = song("A", "T");
    BaseDownloadService::apply_single_fallback(&mut song, false);
    assert_eq!(song.album, "");
}

#[test]
fn apply_single_fallback_album_already_known_is_left_alone() {
    let mut song = Song {
        album: "Real Album".into(),
        ..song("A", "T")
    };
    BaseDownloadService::apply_single_fallback(&mut song, true);
    assert_eq!(song.album, "Real Album");
}

async fn enrich(harness: &Harness, path: &str, song: &mut Song) {
    let requested = RequestedIdentity::new(&song.artist, &song.title, &song.album, song.track);
    harness.service.identify(song, &requested, path, None).await;
}

/// A source's own album tag beats filing the track under its title.
#[tokio::test]
async fn enrich_files_own_album_beats_the_title() {
    let root = Root::new();
    let harness = root.service(FolderStructure::Flat, vec![]);
    let path = root.text("t.flac");
    std::fs::write(&path, flac()).expect("written");
    {
        let mut file = TagFile::open(&path).expect("opens");
        file.set_album(Some("Peer Album"));
        file.set_album_artists(&["Peer Artist".to_string()]);
        file.save().expect("saved");
    }

    let mut song = song("A", "T");
    enrich(&harness, &path, &mut song).await;

    assert_eq!(song.album, "Peer Album");
    assert_eq!(song.album_artist.as_deref(), Some("Peer Artist"));
}

#[tokio::test]
async fn enrich_compilation_flag_on_the_file_keeps_the_title_out_of_the_album() {
    let root = Root::new();
    let harness = root.service(FolderStructure::Flat, vec![]);
    let path = root.text("t.mp3");
    std::fs::write(&path, mp3()).expect("written");
    {
        let mut file = TagFile::open(&path).expect("opens");
        tag_writer_extras::set_compilation(&mut file, true);
        file.save().expect("saved");
    }

    let mut song = song("A", "T");
    enrich(&harness, &path, &mut song).await;

    assert!(song.is_compilation);
    assert_eq!(song.album, "");
}

#[tokio::test]
async fn enrich_nothing_known_files_the_track_as_a_single() {
    let root = Root::new();
    let harness = root.service(FolderStructure::Flat, vec![]);
    let path = root.text("t.mp3");
    std::fs::write(&path, mp3()).expect("written");

    let mut song = song("A", "T");
    enrich(&harness, &path, &mut song).await;

    assert_eq!(song.album, "T");
}

// ---- a library action's replacement -------------------------------------------------------

struct Replacement {
    harness: Harness,
    original: String,
    staged: String,
    identity: KeptIdentity,
}

fn replacement(root: &Root) -> Replacement {
    let harness = root.service(FolderStructure::Organized, vec![]);
    let original = root.join("Odd Folder/03 teardrop old.mp3");
    std::fs::create_dir_all(original.parent().expect("a folder")).expect("made");
    std::fs::write(&original, mp3()).expect("written");
    {
        let mut file = TagFile::open(&original).expect("opens");
        file.set_title(Some("Teardrop"));
        file.save().expect("saved");
    }
    let staged = harness
        .service
        .stage_replacement(&root.landed("peer upload.flac", &flac()))
        .expect("staged")
        .path;
    let identity = KeptIdentityTags::read(&original, None).expect("an identity");
    Replacement {
        harness,
        original: original.to_string_lossy().into_owned(),
        staged,
        identity,
    }
}

#[tokio::test]
async fn a_replacement_moves_in_under_the_originals_folder_and_name() {
    let root = Root::new();
    let Replacement {
        harness,
        original,
        staged,
        identity,
    } = replacement(&root);
    assert!(staged.contains(INCOMING_FOLDER_NAME));
    let announced = Arc::new(Mutex::new(None::<String>));
    let heard = Arc::clone(&announced);
    let gone = original.clone();
    let handoff = ReplacementHandoff::new(
        original.clone(),
        identity,
        Arc::new(move |_| {
            let gone = gone.clone();
            async move {
                std::fs::remove_file(&gone).expect("removed");
                None
            }
            .boxed()
        }),
        Some(Arc::new(move |path: &str| *heard.lock() = Some(path.to_string()))),
    );

    let placed = harness
        .service
        .reveal_replacement(
            &song("Massive Attack", "Teardrop"),
            &requested("Massive Attack", "Teardrop", "", None),
            &staged,
            &handoff,
        )
        .await
        .expect("revealed");

    assert_eq!(placed.path, root.text("Odd Folder/03 teardrop old.flac"));
    assert_eq!(handoff.revealed_path().as_deref(), Some(placed.path.as_str()));
    // Told at once, so the library action can record the swap before anything else runs.
    assert_eq!(announced.lock().as_deref(), Some(placed.path.as_str()));
    assert!(!Path::new(&staged).exists());
    assert_eq!(
        TagFile::open(&placed.path).expect("opens").title().as_deref(),
        Some("Teardrop")
    );
}

#[tokio::test]
async fn a_refused_replacement_is_deleted_before_any_scan_could_see_it() {
    let root = Root::new();
    let Replacement {
        harness,
        original,
        staged,
        identity,
    } = replacement(&root);
    let handoff = ReplacementHandoff::new(
        original.clone(),
        identity,
        Arc::new(|_| async { Some("is not lossless".to_string()) }.boxed()),
        None,
    );

    let refused = harness
        .service
        .reveal_replacement(
            &Song::default(),
            &requested("A", "T", "", None),
            &staged,
            &handoff,
        )
        .await
        .expect_err("refused");

    assert_eq!(
        refused
            .downcast_ref::<ReplacementRejectedException>()
            .map(|r| r.problem.as_str()),
        Some("is not lossless")
    );
    assert!(!Path::new(&staged).exists());
    assert!(Path::new(&original).exists());
    assert!(flacs_under(root.path()).is_empty());
}

fn flacs_under(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut folders = vec![dir.to_path_buf()];
    while let Some(folder) = folders.pop() {
        for entry in std::fs::read_dir(folder).expect("lists").flatten() {
            let path = entry.path();
            if path.is_dir() {
                folders.push(path);
            } else if path.extension().is_some_and(|e| e == "flac") {
                found.push(path);
            }
        }
    }
    found
}

// ---- cover.jpg (#51) ----------------------------------------------------------------------

const COVER_BYTES: [u8; 6] = [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];

#[tokio::test]
async fn write_sidecars_new_album_folder_in_organized_gets_a_cover_file() {
    let root = Root::new();
    let harness = root.service(FolderStructure::Organized, vec![]);
    let placement = place(
        &harness,
        &song("A", "T"),
        &requested("A", "T", "Album", Some(1)),
        &root.landed("x.mp3", &mp3()),
    )
    .await;

    harness
        .service
        .write_sidecars(&Song::default(), &placement, Some(&COVER_BYTES));

    assert!(root.join("A/Album/cover.jpg").is_file());
}

/// Navidrome ranks cover.* above embedded art, so an album that was already there must not
/// change cover.
#[tokio::test]
async fn write_sidecars_existing_album_folder_gets_no_cover_file() {
    let root = Root::new();
    let harness = root.service(FolderStructure::Organized, vec![]);
    let album_dir = root.join("A/Album");
    std::fs::create_dir_all(&album_dir).expect("made");
    std::fs::write(album_dir.join("01 - Other.mp3"), mp3()).expect("written");
    let placement = place(
        &harness,
        &song("A", "T"),
        &requested("A", "T", "Album", Some(2)),
        &root.landed("x.mp3", &mp3()),
    )
    .await;

    harness
        .service
        .write_sidecars(&Song::default(), &placement, Some(&COVER_BYTES));

    assert!(!album_dir.join("cover.jpg").exists());
}

/// In Flat every download shares one folder: one cover.jpg would cover every album.
#[tokio::test]
async fn write_sidecars_flat_never_gets_a_cover_file() {
    let root = Root::new();
    let harness = root.service(FolderStructure::Flat, vec![]);
    let placement = Placement::new(root.text("A - T.mp3"), true);
    std::fs::write(&placement.path, mp3()).expect("written");

    harness
        .service
        .write_sidecars(&Song::default(), &placement, Some(&COVER_BYTES));

    assert!(!root.join("cover.jpg").exists());
}

#[tokio::test]
async fn write_sidecars_never_replaces_an_existing_cover() {
    let root = Root::new();
    let harness = root.service(FolderStructure::Organized, vec![]);
    let placement = place(
        &harness,
        &song("A", "T"),
        &requested("A", "T", "Album", Some(1)),
        &root.landed("x.mp3", &mp3()),
    )
    .await;
    let existing = root.join("A/Album/folder.png");
    std::fs::write(&existing, [1, 2, 3]).expect("written");

    harness
        .service
        .write_sidecars(&Song::default(), &placement, Some(&COVER_BYTES));

    assert!(!root.join("A/Album/cover.jpg").exists());
    assert_eq!(std::fs::read(&existing).expect("read"), [1, 2, 3]);
}

/// The small helpers the placement stands on, at their edges: a root, a bare name, no file.
#[test]
fn the_path_helpers_answer_at_their_edges() {
    use super::placement::get_directory_name;
    assert_eq!(get_directory_name("/music/A - T.mp3").as_deref(), Some("/music"));
    assert_eq!(get_directory_name("/a").as_deref(), Some("/"));
    assert_eq!(get_directory_name("/"), None);
    assert_eq!(get_directory_name("name.mp3").as_deref(), Some(""));
    assert!(BaseDownloadService::is_staged_upload(
        "/music/.OCTO-INCOMING/upload.mp3"
    ));
    assert!(!BaseDownloadService::is_staged_upload("/music/A/upload.mp3"));
    assert_eq!(
        BaseDownloadService::ensure_on_disk(None)
            .expect_err("no file")
            .message,
        "The download returned no file"
    );
}

// ---- The cover a download keeps -----------------------------------------------------------

mod cover_keep {
    use octo_core::settings::MetadataSettings;
    use octo_media::tags::{TagPicture, front_cover};
    use url::Url;
    use wiremock::matchers::path;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::services::cover_art::cover_keep::tests::art;
    use crate::services::cover_art::{CoverArtAggregator, CoverArtArchiveLookup, DownloadCoverResolver};
    use crate::services::http_client_factory;

    /// A service that judges covers against a catalog cover, and the server that has it.
    async fn cover_service(root: &Root, catalog: Vec<u8>) -> (Harness, MockServer) {
        let server = MockServer::start().await;
        Mock::given(path("/deezer.example/cover.jpg"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(catalog))
            .mount(&server)
            .await;
        let settings = Arc::new(octo_core::settings::SettingsStore::for_tests(AppSettings {
            metadata: MetadataSettings::default(),
            ..Default::default()
        }));
        let archive = CoverArtArchiveLookup::with_base_url(
            Url::parse(&format!("{}/coverartarchive/", server.uri())).expect("a base address"),
        );
        let resolver = DownloadCoverResolver::new(
            Arc::new(archive),
            Arc::new(CoverArtAggregator::new(Vec::new())),
            http_client_factory::default_client(),
            settings,
            None,
        );
        let harness = build(
            root.path(),
            AppSettings::default(),
            None,
            None,
            DownloadServices {
                cover_resolver: Some(Arc::new(resolver)),
                ..Default::default()
            },
        );
        (harness, server)
    }

    fn flac_with_cover(root: &Root, name: &str, cover: Option<&[u8]>) -> String {
        let path = root.text(name);
        std::fs::write(&path, flac()).expect("written");
        if let Some(cover) = cover {
            let mut file = TagFile::open(&path).expect("opens");
            file.set_pictures(&[TagPicture::front_cover(cover.to_vec(), "image/jpeg")]);
            file.save().expect("saved");
        }
        path
    }

    fn cover_of(path: &str) -> Option<Vec<u8>> {
        front_cover(&TagFile::open(path).expect("opens")).map(|picture| picture.data)
    }

    fn cover_song(server: &MockServer) -> Song {
        Song {
            album: "Dandelion".into(),
            cover_art_url_large: Some(format!("{}/deezer.example/cover.jpg", server.uri())),
            external_provider: Some("test".into()),
            external_id: Some("x".into()),
            ..song("Ella Langley", "Choosin' Texas")
        }
    }

    /// The one cover line the downloads log shows, read back from the service.
    fn cover_line(harness: &Harness) -> (String, Option<String>) {
        let steps = harness.service.cover_steps.lock();
        assert_eq!(steps.len(), 1, "{steps:?}");
        steps[0].clone()
    }

    #[tokio::test]
    async fn write_metadata_a_good_cover_of_the_same_art_stays_byte_for_byte_and_the_timeline_says_so() {
        let root = Root::new();
        let (harness, server) = cover_service(&root, art(1, 1000)).await;
        let own = art(1, 1400);
        let path = flac_with_cover(&root, "a.flac", Some(&own));

        harness
            .service
            .write_metadata(&path, &mut cover_song(&server))
            .await;

        assert_eq!(cover_of(&path), Some(own));
        let (text, detail) = cover_line(&harness);
        assert_eq!(text, "Kept the cover it came with");
        let pattern = regex::Regex::new(
            r"^1400 x 1400 px, \d+ KB, the same art\. Also looked at: the catalog 1000 px\.$",
        )
        .expect("a pattern");
        assert!(
            pattern.is_match(detail.as_deref().unwrap_or_default()),
            "{detail:?}"
        );
    }

    #[tokio::test]
    async fn write_metadata_another_picture_is_replaced_by_the_catalogs_cover_and_the_timeline_says_why() {
        let root = Root::new();
        let catalog = art(1, 1000);
        let (harness, server) = cover_service(&root, catalog.clone()).await;
        let path = flac_with_cover(&root, "b.flac", Some(&art(2, 1400)));

        harness
            .service
            .write_metadata(&path, &mut cover_song(&server))
            .await;

        assert_eq!(cover_of(&path), Some(catalog));
        let (text, detail) = cover_line(&harness);
        assert_eq!(
            text,
            "Replaced its 1400 px cover with a 1000 x 1000 px one from the catalog"
        );
        assert_eq!(
            detail.as_deref(),
            Some(
                "It was a different picture from the album's cover. Also looked at: the catalog 1000 px. \
                 Now embedded in the song."
            )
        );
    }

    /// Better quality: the new copy gets the old copy's cover, the picture the album already
    /// shows, instead of the peer's.
    #[tokio::test]
    async fn write_metadata_a_replacement_takes_the_old_copys_good_cover() {
        let root = Root::new();
        let (harness, server) = cover_service(&root, art(1, 1000)).await;
        let old_cover = art(1, 1200);
        let original = flac_with_cover(&root, "old.flac", Some(&old_cover));
        let staged = flac_with_cover(&root, "new.flac", Some(&art(5, 300)));

        harness
            .service
            .write_metadata_replacing(&staged, &mut cover_song(&server), Some(&original))
            .await;

        assert_eq!(cover_of(&staged), Some(old_cover));
        assert_eq!(cover_line(&harness).0, "Kept your old copy's cover");
    }
}
