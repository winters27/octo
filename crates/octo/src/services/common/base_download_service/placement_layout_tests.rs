//! Placement through the file layout: each kind's template, the album's folder found by any name
//! (by equivalence, or by the album its files hold), the disc folders, the file name that fits,
//! the album's facts pinned from the folder chosen, and a placement that never moves another
//! song. The files are real files in a temp folder, tagged as each case needs.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::FutureExt;
use octo_core::common::{LayoutKind, LayoutValues, SongIdentity};
use octo_core::models::domain::{Album, Song};
use octo_core::settings::{AppSettings, DownloadMode, FolderStructure, StorageMode, SubsonicSettings};
use octo_media::audio::{ILoudnessMeter, Loudness};
use octo_media::tags::TagFile;
use parking_lot::Mutex;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use super::test_support::{Harness, TEST_ALBUM, build, catalog};
use super::*;
use crate::services::common::album_identity::{LayoutTags, TAG_READS_HERE, read_layout_tags};
use crate::services::common::test_fakes::FakeMetadata;
use crate::services::i_download_service::HeldEnd;
use crate::services::library::library_write_gate::LibraryWriteGate;
use crate::services::lidarr::HeldPlacement;
use crate::services::local::test_support::FakeLocalLibrary;
use crate::services::local::{ILocalLibraryService, LocalLibraryService, LocalSongMapping};
use crate::services::test_support::mp3;

// ---- the rig ------------------------------------------------------------------------------

/// What one test file is tagged with.
#[derive(Clone, Default)]
struct Tags<'a> {
    title: &'a str,
    artist: &'a str,
    album: &'a str,
    album_artist: Option<&'a str>,
    year: u32,
    track: u32,
    disc: u32,
    disc_count: u32,
    release_group: Option<&'a str>,
}

/// An MP3 (or a FLAC, by its extension) at `path`, tagged with `tags`.
fn tagged(path: &Path, tags: &Tags) -> PathBuf {
    std::fs::create_dir_all(path.parent().expect("a folder")).expect("folders");
    let flac = path.extension().is_some_and(|extension| extension == "flac");
    let bytes = if flac {
        crate::services::test_support::flac()
    } else {
        mp3()
    };
    std::fs::write(path, bytes).expect("written");
    let mut file = TagFile::open(path).expect("an audio file");
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("x")
        .to_string();
    file.set_title(Some(if tags.title.is_empty() { &stem } else { tags.title }));
    file.set_album(Some(tags.album));
    file.set_performers(&[tags.artist.to_string()]);
    if let Some(album_artist) = tags.album_artist {
        file.set_album_artists(&[album_artist.to_string()]);
    }
    file.set_year(tags.year);
    file.set_track(tags.track);
    file.set_disc(tags.disc);
    file.set_disc_count(tags.disc_count);
    file.set_music_brainz_release_group_id(tags.release_group);
    file.save().expect("saved");
    path.to_path_buf()
}

/// A preset's settings.
fn preset(structure: FolderStructure) -> SubsonicSettings {
    SubsonicSettings {
        folder_structure: structure,
        download_mode: DownloadMode::Track,
        storage_mode: StorageMode::Permanent,
        ..Default::default()
    }
}

/// Custom settings with these templates for albums, singles and compilations (blank: as albums).
fn custom(albums: &str, singles: &str, compilations: &str) -> SubsonicSettings {
    SubsonicSettings {
        folder_structure: FolderStructure::Custom,
        folder_template: albums.to_string(),
        folder_template_singles: singles.to_string(),
        folder_template_compilations: compilations.to_string(),
        download_mode: DownloadMode::Track,
        storage_mode: StorageMode::Permanent,
        ..Default::default()
    }
}

const DISCS: &str = "{Artist}/{Album}/Disc {Disc}/{Track:00} - {Title}";
const BY_YEAR: &str = "{Artist}/{Album} ({Year})/{Track:00} - {Title}";

struct Rig {
    dir: TempDir,
    library: Arc<FakeLocalLibrary>,
    harness: Harness,
}

impl Rig {
    fn new(subsonic: SubsonicSettings) -> Rig {
        Self::with(subsonic, None, DownloadServices::default())
    }

    /// With a catalog for whole downloads, and these services.
    fn with(
        subsonic: SubsonicSettings,
        metadata: Option<Arc<FakeMetadata>>,
        services: DownloadServices,
    ) -> Rig {
        let dir = tempfile::tempdir().expect("a temp dir");
        let library = Arc::new(FakeLocalLibrary::default());
        let harness = build(
            dir.path(),
            AppSettings {
                subsonic,
                ..Default::default()
            },
            Some(library.clone() as Arc<dyn ILocalLibraryService>),
            metadata.map(|m| m as Arc<dyn crate::services::i_music_metadata_service::IMusicMetadataService>),
            services,
        );
        Rig {
            dir,
            library,
            harness,
        }
    }

    fn at(&self, relative: &str) -> PathBuf {
        self.dir.path().join(relative)
    }

    /// A file already in the library, tagged.
    fn has(&self, relative: &str, tags: &Tags) -> PathBuf {
        tagged(&self.at(relative), tags)
    }

    /// Octo's own record of a file in the library.
    fn octos(&self, path: &Path, artist: &str, title: &str) {
        self.library.by_tags.lock().push((
            artist.into(),
            title.into(),
            None,
            LocalSongMapping {
                local_path: path.to_string_lossy().into_owned(),
                artist: artist.into(),
                title: title.into(),
                ..Default::default()
            },
        ));
    }

    /// Every transfer of a whole download lands a fresh MP3 outside the library's folders.
    fn lands(&self) {
        let peer = self.at("peer");
        self.harness.backend.land(move |_, _| {
            std::fs::create_dir_all(&peer).expect("made");
            let path = peer.join(format!("{}.mp3", uuid::Uuid::new_v4().simple()));
            std::fs::write(&path, mp3()).expect("written");
            path.to_string_lossy().into_owned()
        });
    }

    /// A file a source delivered, outside the library's folders.
    fn landed(&self) -> String {
        let dir = self.at("peer");
        std::fs::create_dir_all(&dir).expect("made");
        let path = dir.join(format!("{}.mp3", uuid::Uuid::new_v4().simple()));
        std::fs::write(&path, mp3()).expect("written");
        path.to_string_lossy().into_owned()
    }

    async fn place(&self, song: &Song) -> Placement {
        self.harness
            .service
            .place_in_library(song, &asked(song), &self.landed())
            .await
    }

    /// The path under the music folder, with `/`.
    fn relative(&self, path: &str) -> String {
        let root = slashed(&self.dir.path().to_string_lossy());
        slashed(path)
            .strip_prefix(&format!("{root}/"))
            .unwrap_or(path)
            .to_string()
    }
}

fn slashed(path: &str) -> String {
    path.replace('\\', "/")
}

/// A track of an album, as the catalog gives it.
fn track(artist: &str, album: &str, title: &str, number: i32) -> Song {
    Song {
        artist: artist.into(),
        title: title.into(),
        album: album.into(),
        track: Some(number),
        ..Default::default()
    }
}

/// What the song was asked for under: its own names.
fn asked(song: &Song) -> RequestedIdentity {
    RequestedIdentity::new(&song.artist, &song.title, &song.album, song.track)
}

// ---- values and kinds -----------------------------------------------------------------------

#[test]
fn layout_values_fill_each_name() {
    let full = Song {
        album_artist: Some("Daft Punk".into()),
        year: Some(2001),
        disc_number: Some(1),
        total_discs: Some(1),
        release_type: Some("album".into()),
        music_brainz_release_group_id: Some("rg-discovery".into()),
        ..track("Daft Punk", "Discovery", "Daft Punk - One More Time", 1)
    };
    let values = BaseDownloadService::layout_values(&full, &asked(&full), false);
    assert_eq!(
        values,
        LayoutValues {
            artist: "Daft Punk".into(),
            track_artist: "Daft Punk".into(),
            album_artist: Some("Daft Punk".into()),
            album: "Discovery".into(),
            title: "One More Time".into(),
            track: Some(1),
            disc: Some(1),
            disc_count: Some(1),
            multi_disc: false,
            year: Some(2001),
            album_type: Some("Album".into()),
            kind: LayoutKind::Album,
            release_group: Some("rg-discovery".into()),
            tags: None,
        }
    );

    // A year from the release date; disc 2 or a count above 1 shows the disc.
    let dated = Song {
        release_date: Some("1997-01-20".into()),
        disc_number: Some(2),
        ..track("Daft Punk", "Homework", "Da Funk", 3)
    };
    let values = BaseDownloadService::layout_values(&dated, &asked(&dated), false);
    assert_eq!((values.year, values.multi_disc), (Some(1997), true));
    let counted = Song {
        disc_number: Some(1),
        total_discs: Some(2),
        ..track("Daft Punk", "Alive 2007", "Robot Rock", 1)
    };
    assert!(BaseDownloadService::layout_values(&counted, &asked(&counted), false).multi_disc);

    // No album: a single, typed so. No artist: Unknown Artist.
    let single = track("", "", "Untitled", 1);
    let values = BaseDownloadService::layout_values(&single, &asked(&single), false);
    assert_eq!(
        (values.kind, values.album_type.as_deref(), values.artist.as_str()),
        (LayoutKind::Single, Some("Single"), "Unknown Artist")
    );
    let typed = Song {
        release_type: Some("single".into()),
        ..track("Daft Punk", "One More Time", "One More Time", 1)
    };
    assert_eq!(
        BaseDownloadService::layout_values(&typed, &asked(&typed), false).kind,
        LayoutKind::Single
    );

    // A compilation by its flag, by its type, or by a Various Artists album artist.
    for song in [
        Song {
            is_compilation: true,
            ..track("Lizzo", "Now 115", "About Damn Time", 4)
        },
        Song {
            release_type: Some("album; compilation".into()),
            ..track("Lizzo", "Now 115", "About Damn Time", 4)
        },
        Song {
            album_artist: Some("VA".into()),
            ..track("Lizzo", "Now 115", "About Damn Time", 4)
        },
    ] {
        let values = BaseDownloadService::layout_values(&song, &asked(&song), false);
        assert_eq!(values.kind, LayoutKind::Compilation, "{song:?}");
        assert_eq!(values.album_type.as_deref(), Some("Compilation"));
    }

    // The folder artist is the first credit a source names; the file keeps the whole credit.
    let credits = Song {
        primary_artist: Some("Bizarrap".into()),
        ..track("Bizarrap, Rauw Alejandro", "Sessions", "Session 56", 1)
    };
    let values = BaseDownloadService::layout_values(&credits, &asked(&credits), false);
    assert_eq!(
        (values.artist.as_str(), values.track_artist.as_str()),
        ("Bizarrap", "Bizarrap, Rauw Alejandro")
    );
}

#[test]
fn release_facts_only_for_the_named_album() {
    // Identified to another release than the album it is filed under: none of its facts.
    let song = Song {
        album_artist: Some("Daft Punk".into()),
        year: Some(2007),
        disc_number: Some(2),
        release_type: Some("album; live".into()),
        music_brainz_release_group_id: Some("rg-alive".into()),
        ..track("Daft Punk", "Alive 2007", "One More Time", 7)
    };
    let requested = RequestedIdentity::new("Daft Punk", "One More Time", "Discovery", Some(1));
    let values = BaseDownloadService::layout_values(&song, &requested, false);
    assert_eq!(values.album, "Discovery");
    assert_eq!(
        (
            values.year,
            values.disc,
            values.album_artist,
            values.release_group,
            values.album_type
        ),
        (None, None, None, None, None)
    );
    assert!(!values.multi_disc);
}

#[tokio::test]
async fn singles_and_compilations_take_their_template() {
    let rig = Rig::new(custom(
        "{Artist}/{Album}/{Track:00} - {Title}",
        "{Artist}/Singles/{Title}",
        "Compilations/{Album}/{Track:00} - {Track Artist} - {Title}",
    ));
    let album = rig
        .place(&track("Daft Punk", "Discovery", "Aerodynamic", 2))
        .await;
    let single = rig.place(&track("Daft Punk", "", "Something About Us", 1)).await;
    let compilation = rig
        .place(&Song {
            is_compilation: true,
            ..track("Lizzo", "Now 115", "About Damn Time", 4)
        })
        .await;

    assert_eq!(
        rig.relative(&album.path),
        "Daft Punk/Discovery/02 - Aerodynamic.mp3"
    );
    assert_eq!(
        rig.relative(&single.path),
        "Daft Punk/Singles/Something About Us.mp3"
    );
    assert_eq!(
        rig.relative(&compilation.path),
        "Compilations/Now 115/04 - Lizzo - About Damn Time.mp3"
    );
    assert_eq!(
        compilation.values.as_ref().map(|values| values.kind),
        Some(LayoutKind::Compilation)
    );
}

#[tokio::test]
async fn a_various_artists_album_artist_files_as_a_compilation() {
    let rig = Rig::new(custom(
        "{Artist}/{Album}/{Track:00} - {Title}",
        "",
        "Compilations/{Album}/{Track:00} - {Track Artist} - {Title}",
    ));
    let placed = rig
        .place(&Song {
            album_artist: Some("Various Artists".into()),
            ..track("Lizzo", "Now 115", "About Damn Time", 4)
        })
        .await;
    assert_eq!(
        rig.relative(&placed.path),
        "Compilations/Now 115/04 - Lizzo - About Damn Time.mp3"
    );
}

#[tokio::test]
async fn a_broken_custom_layout_still_places() {
    let rig = Rig::new(custom("{Artist}/{Nope}/{Title}", "", ""));
    let placed = rig.place(&track("A", "Album", "T", 1)).await;
    assert_eq!(placed.failed, None);
    assert_eq!(
        rig.relative(&placed.path),
        "A/Album/01 - T.mp3",
        "filed as Organized"
    );
}

// ---- joining the album's folder -------------------------------------------------------------

/// An album folder an older Octo named with a character Windows refuses: the new track's name
/// for it is the look-alike, and it joins the folder there, with the album's grouping values.
#[cfg(unix)]
#[tokio::test]
async fn a_new_track_joins_the_old_named_album_folder() {
    let rig = Rig::new(preset(FolderStructure::Organized));
    let old = rig.has(
        "Pearl Jam/Are You Ready?/01 - Intro.flac",
        &Tags {
            artist: "Pearl Jam",
            album: "Are You Ready?",
            album_artist: Some("Pearl Jam"),
            ..Tags::default()
        },
    );
    {
        let mut file = TagFile::open(&old).expect("opens");
        octo_media::tags::tag_writer_extras::set_or_clear_text(
            &mut file,
            octo_media::tags::TagFields::RELEASE_DATE,
            Some("2002-04-01"),
        );
        file.save().expect("saved");
    }
    let mut song = track("Pearl Jam", "Are You Ready?", "Thumbing My Way", 2);
    let requested = asked(&song);
    let landed = rig.landed();
    rig.harness
        .service
        .identify(&mut song, &requested, &landed, None)
        .await;
    let placed = rig
        .harness
        .service
        .place_in_library(&song, &requested, &landed)
        .await;

    assert_eq!(
        rig.relative(&placed.path),
        "Pearl Jam/Are You Ready?/02 - Thumbing My Way.mp3"
    );
    assert!(
        song.joins_album.is_some(),
        "the album's grouping values are carried"
    );
}

#[tokio::test]
async fn a_new_track_joins_its_album_folder_by_another_name() {
    let rig = Rig::new(preset(FolderStructure::Organized));
    rig.has(
        "Daft Punk/Discovery (Remastered)/01 - One More Time.mp3",
        &Tags {
            artist: "Daft Punk",
            album: "Discovery",
            album_artist: Some("Daft Punk"),
            ..Tags::default()
        },
    );
    let placed = rig
        .place(&track("Daft Punk", "Discovery", "Aerodynamic", 2))
        .await;
    assert_eq!(
        rig.relative(&placed.path),
        "Daft Punk/Discovery (Remastered)/02 - Aerodynamic.mp3"
    );
    assert!(!placed.created_folder);
}

/// Old rules kept a space at the end of a folder name, which Windows cannot show: the track joins
/// that folder rather than making a second one beside it.
#[cfg(unix)]
#[tokio::test]
async fn a_new_track_joins_an_old_folder_ending_in_a_space() {
    let rig = Rig::new(preset(FolderStructure::Organized));
    rig.has(
        "A/Album /01 - One.mp3",
        &Tags {
            artist: "A",
            album: "Album ",
            ..Tags::default()
        },
    );
    let placed = rig.place(&track("A", "Album ", "Two", 2)).await;
    assert_eq!(rig.relative(&placed.path), "A/Album /02 - Two.mp3");
}

#[tokio::test]
async fn a_case_twin_folder_is_joined_by_its_real_name() {
    let rig = Rig::new(preset(FolderStructure::Organized));
    rig.has("daft punk/DISCOVERY/01 - One More Time.mp3", &Tags::default());
    let placed = rig
        .place(&track("Daft Punk", "Discovery", "Aerodynamic", 2))
        .await;
    assert_eq!(
        rig.relative(&placed.path),
        "daft punk/DISCOVERY/02 - Aerodynamic.mp3"
    );
    let artists: Vec<String> = std::fs::read_dir(rig.dir.path())
        .expect("lists")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name != "peer")
        .collect();
    assert_eq!(artists, ["daft punk"], "no twin folder beside it");
}

#[tokio::test]
async fn a_case_twin_file_of_the_same_song_is_replaced_in_place() {
    let rig = Rig::new(preset(FolderStructure::Organized));
    let old = rig.has("Daft Punk/Discovery/01 - one more time.mp3", &Tags::default());
    rig.octos(&old, "Daft Punk", "One More Time");
    let placed = rig
        .place(&track("Daft Punk", "Discovery", "One More Time", 1))
        .await;
    assert_eq!(
        rig.relative(&placed.path),
        "Daft Punk/Discovery/01 - one more time.mp3",
        "the new file takes the old one's name, so its records stay right"
    );
    let files: Vec<_> = std::fs::read_dir(rig.at("Daft Punk/Discovery"))
        .expect("lists")
        .flatten()
        .collect();
    assert_eq!(files.len(), 1, "the old copy is gone once the new one is in");
    assert!(
        holds_no_record(&rig.at(".octo-incoming/replacing")),
        "no record of the replace is left"
    );
}

/// Whether the records folder `records` holds no record of a move under way (the folder itself
/// stays).
fn holds_no_record(records: &Path) -> bool {
    std::fs::read_dir(records).map_or(true, |entries| {
        !entries.flatten().any(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "json")
        })
    })
}

/// The new copy of a song fails to move in over its old copy (a full disk): the old file is
/// back under its own name, untouched, with its mapping, and nothing of the replace is left.
#[tokio::test]
async fn a_failed_replace_keeps_the_old_file_and_its_mapping() {
    failed_replace_keeps_the_old_file(false).await;
}

/// [`a_failed_replace_keeps_the_old_file_and_its_mapping`] across file systems, where the copy
/// fails: no unfinished copy is left either.
#[tokio::test]
async fn a_failed_replace_across_disks_keeps_the_old_file_and_its_mapping() {
    failed_replace_keeps_the_old_file(true).await;
}

async fn failed_replace_keeps_the_old_file(across_disks: bool) {
    let _across = across_disks.then(|| AcrossDisks::new(|_, _| {}));
    let rig = Rig::new(preset(FolderStructure::Organized));
    let old = rig.has("Daft Punk/Discovery/01 - One More Time.mp3", &Tags::default());
    let before = std::fs::read(&old).expect("read");
    rig.octos(&old, "Daft Punk", "One More Time");
    let landed = rig.landed();
    let gone = landed.clone();
    *rig.harness.service.settle_pause.lock() = Some(Arc::new(move |_| {
        let gone = gone.clone();
        async move {
            let _ = std::fs::remove_file(&gone);
        }
        .boxed()
    }));
    let song = track("Daft Punk", "Discovery", "One More Time", 1);

    let placed = rig
        .harness
        .service
        .place_in_library(&song, &asked(&song), &landed)
        .await;

    assert!(placed.failed.is_some(), "{placed:?}");
    assert_eq!(std::fs::read(&old).ok(), Some(before), "the old file is there");
    let names: Vec<String> = std::fs::read_dir(rig.at("Daft Punk/Discovery"))
        .expect("lists")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["01 - One More Time.mp3"]);
    assert!(
        rig.library
            .get_mappings()
            .await
            .iter()
            .any(|mapping| mapping.local_path == old.to_string_lossy()),
        "its mapping stays"
    );
    assert!(holds_no_record(&rig.at(".octo-incoming/replacing")));
}

/// Octo stopped between a replace's steps: when it starts again, an original path without its
/// file gets its old copy back, and one that holds the new copy has its old copy removed.
#[tokio::test]
async fn a_replace_cut_short_is_put_right_at_start() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let folder = dir.path().join("Daft Punk/Discovery");
    std::fs::create_dir_all(&folder).expect("made");
    let lost = folder.join("01 - One More Time.mp3");
    let lost_backup = folder.join(".octo-replacing-aaa");
    std::fs::write(&lost_backup, b"the old One More Time").expect("written");
    let landed = folder.join("02 - Aerodynamic.mp3");
    let landed_backup = folder.join(".octo-replacing-bbb");
    std::fs::write(&landed, b"the new Aerodynamic").expect("written");
    std::fs::write(&landed_backup, b"the old Aerodynamic").expect("written");
    let records = dir.path().join(".octo-incoming/replacing");
    std::fs::create_dir_all(&records).expect("made");
    for (id, original, backup) in [("aaa", &lost, &lost_backup), ("bbb", &landed, &landed_backup)] {
        let record = serde_json::json!({
            "original": original.to_string_lossy(),
            "backup": backup.to_string_lossy(),
        });
        std::fs::write(records.join(format!("{id}.json")), record.to_string()).expect("written");
    }

    let _started = build(
        dir.path(),
        AppSettings {
            subsonic: preset(FolderStructure::Organized),
            ..Default::default()
        },
        None,
        None,
        DownloadServices::default(),
    );
    for _ in 0..200 {
        if holds_no_record(&records) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    assert!(holds_no_record(&records), "every record is done with");
    assert_eq!(
        std::fs::read(&lost).ok().as_deref(),
        Some(&b"the old One More Time"[..]),
        "put back"
    );
    assert!(!lost_backup.exists());
    assert_eq!(std::fs::read(&landed).expect("kept"), b"the new Aerodynamic");
    assert!(!landed_backup.exists());
}

/// What a replace Octo was stopped in the middle of left under the music folder `root`: the old
/// copy of `Daft Punk/Discovery/01 - One More Time.mp3` renamed aside, the name empty, and its
/// record (with `staged`, a copy across file systems that never took the name, when given).
struct StoppedReplace {
    original: PathBuf,
    backup: PathBuf,
    staged: Option<PathBuf>,
    records: PathBuf,
}

impl StoppedReplace {
    fn under(root: &Path, id: &str, staged: Option<&[u8]>) -> StoppedReplace {
        let folder = root.join("Daft Punk/Discovery");
        std::fs::create_dir_all(&folder).expect("made");
        let original = folder.join("01 - One More Time.mp3");
        let backup = folder.join(format!(".octo-replacing-{id}"));
        std::fs::write(&backup, b"the old One More Time").expect("written");
        let staged = staged.map(|bytes| {
            let staged = folder.join(format!(".octo-replacing-{id}.new"));
            std::fs::write(&staged, bytes).expect("written");
            staged
        });
        let records = root.join(".octo-incoming/replacing");
        std::fs::create_dir_all(&records).expect("made");
        let record = serde_json::json!({
            "original": original.to_string_lossy(),
            "backup": backup.to_string_lossy(),
            "staged": staged.as_ref().map(|staged| staged.to_string_lossy().into_owned()).unwrap_or_default(),
        });
        std::fs::write(records.join(format!("{id}.json")), record.to_string()).expect("written");
        StoppedReplace {
            original,
            backup,
            staged,
            records,
        }
    }

    /// The old copy is back under its name, and nothing of the replace is left.
    #[track_caller]
    fn assert_put_back(&self) {
        assert_eq!(
            std::fs::read(&self.original).ok().as_deref(),
            Some(&b"the old One More Time"[..]),
            "put back"
        );
        assert!(!self.backup.exists());
        assert!(self.staged.as_ref().is_none_or(|staged| !staged.exists()));
        assert!(holds_no_record(&self.records));
    }
}

/// A replace cut short is put right by the next placement too, before it moves anything: the
/// music folder can be found only after Octo started.
#[tokio::test]
async fn a_replace_cut_short_is_put_right_by_the_next_placement() {
    let rig = Rig::new(preset(FolderStructure::Organized));
    let stopped = StoppedReplace::under(rig.dir.path(), "ccc", None);

    let placed = rig
        .place(&track("Daft Punk", "Discovery", "Aerodynamic", 2))
        .await;

    assert_eq!(
        rig.relative(&placed.path),
        "Daft Punk/Discovery/02 - Aerodynamic.mp3"
    );
    stopped.assert_put_back();
}

/// Octo stopped while the new copy of a replace was being copied across file systems: the copy
/// never took the song's name, so the old copy goes back and the unfinished copy goes.
#[tokio::test]
async fn a_replace_stopped_mid_copy_puts_the_old_copy_back() {
    let rig = Rig::new(preset(FolderStructure::Organized));
    let stopped = StoppedReplace::under(rig.dir.path(), "ddd", Some(b"the new One"));

    rig.place(&track("Daft Punk", "Discovery", "Aerodynamic", 2))
        .await;

    stopped.assert_put_back();
}

/// Octo stopped after a move across file systems took its name, before the file it was copied
/// from went: the next recovery removes that file from the staging folder when the name holds as
/// many bytes, and leaves one whose name holds fewer.
#[tokio::test]
async fn a_move_across_disks_stopped_after_its_rename_removes_its_source() {
    let rig = Rig::new(preset(FolderStructure::Organized));
    let staging = rig.at(".octo-incoming");
    let folder = rig.at("Daft Punk/Discovery");
    std::fs::create_dir_all(&folder).expect("made");
    let records = staging.join("replacing");
    std::fs::create_dir_all(&records).expect("made");
    let mut sources = Vec::new();
    for (id, name, copied) in [
        ("iii", "01 - One More Time.mp3", &b"the whole song"[..]),
        ("jjj", "02 - Aerodynamic.mp3", &b"part"[..]),
    ] {
        let source = staging.join(format!("slskd/job/{id}.mp3"));
        std::fs::create_dir_all(source.parent().expect("a folder")).expect("made");
        std::fs::write(&source, b"the whole song").expect("written");
        let target = folder.join(name);
        std::fs::write(&target, copied).expect("written");
        let record = serde_json::json!({
            "original": target.to_string_lossy(),
            "staged": folder.join(format!(".octo-moving-{id}")).to_string_lossy(),
            "source": source.to_string_lossy(),
        });
        std::fs::write(records.join(format!("{id}.json")), record.to_string()).expect("written");
        sources.push(source);
    }

    placement::recover_replacements(&staging);

    assert!(!sources[0].exists(), "the file the whole copy was made from goes");
    assert!(sources[1].exists(), "one whose copy holds fewer bytes stays");
    assert_eq!(
        std::fs::read(folder.join("01 - One More Time.mp3"))
            .ok()
            .as_deref(),
        Some(&b"the whole song"[..])
    );
    assert!(holds_no_record(&records));
}

/// A power cut kept a copy's source removal on one disk and lost its rename on the other: the
/// copy is whole beside its name and its source is gone. The next recovery gives it its name
/// (for a replace too, whose old copy then goes) and never removes it; a copy short of its
/// source's size stays where it is, with its record.
#[tokio::test]
async fn a_whole_copy_whose_source_is_gone_takes_its_name() {
    let rig = Rig::new(preset(FolderStructure::Organized));
    let staging = rig.at(".octo-incoming");
    let folder = rig.at("Daft Punk/Discovery");
    std::fs::create_dir_all(&folder).expect("made");
    let records = staging.join("replacing");
    std::fs::create_dir_all(&records).expect("made");
    let song = b"the whole song";
    let record = |id: &str, name: &str, staged_bytes: &[u8], backup: Option<&[u8]>| {
        let staged = folder.join(format!(".octo-replacing-{id}.new"));
        std::fs::write(&staged, staged_bytes).expect("written");
        let backup = backup.map(|bytes| {
            let backup = folder.join(format!(".octo-replacing-{id}"));
            std::fs::write(&backup, bytes).expect("written");
            backup
        });
        let record = serde_json::json!({
            "original": folder.join(name).to_string_lossy(),
            "backup": backup.as_ref().map(|backup| backup.to_string_lossy().into_owned()).unwrap_or_default(),
            "staged": staged.to_string_lossy(),
            "source": staging.join(format!("slskd/job/{id}.mp3")).to_string_lossy(),
            "size": song.len(),
            "whole": true,
        });
        std::fs::write(records.join(format!("{id}.json")), record.to_string()).expect("written");
        (staged, backup)
    };
    let (moved, _) = record("kkk", "01 - One More Time.mp3", song, None);
    let (replaced, old) = record("lll", "02 - Aerodynamic.mp3", song, Some(b"the old one"));
    let (short, _) = record("mmm", "03 - Digital Love.mp3", b"the who", None);

    placement::recover_replacements(&staging);

    for (name, staged) in [
        ("01 - One More Time.mp3", &moved),
        ("02 - Aerodynamic.mp3", &replaced),
    ] {
        assert_eq!(
            std::fs::read(folder.join(name)).ok().as_deref(),
            Some(&song[..]),
            "{name} holds the song"
        );
        assert!(!staged.exists());
    }
    assert!(
        old.is_some_and(|old| !old.exists()),
        "the replaced song's old copy goes"
    );
    assert_eq!(
        std::fs::read(&short).ok().as_deref(),
        Some(&b"the who"[..]),
        "a copy short of its source's size stays"
    );
    assert!(!folder.join("03 - Digital Love.mp3").exists());
    assert!(records.join("mmm.json").is_file(), "its record stays");
    assert!(!records.join("kkk.json").exists() && !records.join("lll.json").exists());
}

/// A copy that holds its source's whole size, whose source is gone (a sweep took it after a
/// stop), but whose record never marked it flushed whole: the next recovery keeps it where it is
/// and says so once, since a power cut can leave a copy its full size without its sound.
#[tokio::test]
async fn a_copy_never_marked_whole_is_kept_when_its_source_is_gone() {
    let rig = Rig::new(preset(FolderStructure::Organized));
    let staging = rig.at(".octo-incoming");
    let folder = rig.at("Daft Punk/Discovery");
    std::fs::create_dir_all(&folder).expect("made");
    let records = staging.join("replacing");
    std::fs::create_dir_all(&records).expect("made");
    let song = b"the whole song";
    let staged = folder.join(".octo-moving-qqq");
    std::fs::write(&staged, song).expect("written");
    let record = serde_json::json!({
        "original": folder.join("01 - One More Time.mp3").to_string_lossy(),
        "staged": staged.to_string_lossy(),
        "source": staging.join("slskd/job/qqq.mp3").to_string_lossy(),
        "size": song.len(),
    });
    std::fs::write(records.join("qqq.json"), record.to_string()).expect("written");
    placement::KEPT_REPORTS.with(|reports| reports.set(0));

    placement::recover_replacements(&staging);

    assert!(
        !folder.join("01 - One More Time.mp3").exists(),
        "the copy does not take the name"
    );
    assert_eq!(std::fs::read(&staged).ok().as_deref(), Some(&song[..]));
    assert!(records.join("qqq.json").is_file(), "its record stays");
    assert_eq!(placement::KEPT_REPORTS.with(std::cell::Cell::get), 1, "said once");
}

/// A copy short of its source's size, whose source is gone, is kept and said once: the record is
/// marked, and a second recovery neither says it again nor touches the copy or its record.
#[tokio::test]
async fn a_kept_unfinished_copy_is_said_once() {
    let rig = Rig::new(preset(FolderStructure::Organized));
    let staging = rig.at(".octo-incoming");
    let folder = rig.at("Daft Punk/Discovery");
    std::fs::create_dir_all(&folder).expect("made");
    let records = staging.join("replacing");
    std::fs::create_dir_all(&records).expect("made");
    let staged = folder.join(".octo-moving-ppp");
    std::fs::write(&staged, b"the who").expect("written");
    let record_path = records.join("ppp.json");
    let record = serde_json::json!({
        "original": folder.join("01 - One More Time.mp3").to_string_lossy(),
        "staged": staged.to_string_lossy(),
        "source": staging.join("slskd/job/ppp.mp3").to_string_lossy(),
        "size": 14,
    });
    std::fs::write(&record_path, record.to_string()).expect("written");
    placement::KEPT_REPORTS.with(|reports| reports.set(0));

    placement::recover_replacements(&staging);

    assert_eq!(placement::KEPT_REPORTS.with(std::cell::Cell::get), 1, "said once");
    let marked = std::fs::read(&record_path).expect("the record stays");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&marked).expect("a record")["reported"],
        serde_json::json!(true)
    );
    let modified = |path: &Path| std::fs::metadata(path).and_then(|meta| meta.modified()).ok();
    let (staged_at, record_at) = (modified(&staged), modified(&record_path));

    placement::recover_replacements(&staging);

    assert_eq!(
        placement::KEPT_REPORTS.with(std::cell::Cell::get),
        1,
        "not said again"
    );
    assert_eq!(std::fs::read(&staged).ok().as_deref(), Some(&b"the who"[..]));
    assert_eq!(modified(&staged), staged_at, "the copy is not touched");
    assert_eq!(std::fs::read(&record_path).ok(), Some(marked));
    assert_eq!(modified(&record_path), record_at, "the record is not touched");
    assert!(!folder.join("01 - One More Time.mp3").exists());
}

/// A move across file systems whose source, outside the staging folder, the disk refuses to
/// remove is done, and its record is kept marked moved: the next recovery flushes the name's
/// folder, then removes that source.
#[test]
fn a_move_whose_source_cannot_go_is_finished_by_recovery() {
    let dir = TempDir::new().expect("a temp dir");
    let staging = dir.path().join(".octo-incoming");
    let song = b"the whole song";
    let from = dir.path().join("Lidarr/Daft Punk/Discovery/01.mp3");
    std::fs::create_dir_all(from.parent().expect("a folder")).expect("made");
    std::fs::write(&from, song).expect("written");
    let to = dir.path().join("Daft Punk/Discovery/01 - One More Time.mp3");
    std::fs::create_dir_all(to.parent().expect("a folder")).expect("made");
    let across = AcrossDisks::new(|_, _| {});
    PLACEMENT.refuse_removals(&[&from]);

    let moved = placement::move_file(&staging, &from.to_string_lossy(), &to.to_string_lossy());
    PLACEMENT.allow_all();
    drop(across);

    assert!(moved.is_ok(), "{moved:?}");
    assert!(from.is_file(), "its source waits for the next recovery");
    let marked: Vec<serde_json::Value> = std::fs::read_dir(staging.join("replacing"))
        .expect("the records folder")
        .flatten()
        .filter_map(|entry| std::fs::read(entry.path()).ok())
        .filter_map(|bytes| serde_json::from_slice(&bytes).ok())
        .map(|record: serde_json::Value| record["moved"].clone())
        .collect();
    assert_eq!(
        marked,
        [serde_json::json!(true)],
        "its record is kept, marked moved"
    );
    PLACEMENT.take_settling();

    placement::recover_replacements(&staging);

    let folder = to.parent().expect("a folder").to_string_lossy().into_owned();
    assert_eq!(
        PLACEMENT.take_settling(),
        [
            format!("flush {folder}"),
            format!("remove {}", from.to_string_lossy())
        ]
    );
    assert!(!from.exists(), "the source goes");
    assert_eq!(std::fs::read(&to).ok().as_deref(), Some(&song[..]));
    assert!(holds_no_record(&staging.join("replacing")));
}

/// A record marked moved whose copy was never marked whole: what holds its name was never known
/// to be that copy, so the next recovery leaves a source outside the staging folder where it is,
/// even when the name holds as many bytes.
#[test]
fn a_source_outside_staging_stays_unless_its_copy_was_marked_whole() {
    let dir = TempDir::new().expect("a temp dir");
    let staging = dir.path().join(".octo-incoming");
    let records = staging.join("replacing");
    std::fs::create_dir_all(&records).expect("made");
    let song = b"the whole song";
    let from = dir.path().join("Lidarr/Daft Punk/Discovery/01.mp3");
    std::fs::create_dir_all(from.parent().expect("a folder")).expect("made");
    std::fs::write(&from, song).expect("written");
    let folder = dir.path().join("Daft Punk/Discovery");
    std::fs::create_dir_all(&folder).expect("made");
    let to = folder.join("01 - One More Time.mp3");
    std::fs::write(&to, song).expect("written");
    let record = serde_json::json!({
        "original": to.to_string_lossy(),
        "staged": folder.join(".octo-moving-sss").to_string_lossy(),
        "source": from.to_string_lossy(),
        "size": song.len(),
        "moved": true,
    });
    std::fs::write(records.join("sss.json"), record.to_string()).expect("written");

    placement::recover_replacements(&staging);

    assert_eq!(
        std::fs::read(&from).ok().as_deref(),
        Some(&song[..]),
        "the source stays"
    );
    assert_eq!(std::fs::read(&to).ok().as_deref(), Some(&song[..]));
}

/// The side copy of a record that a stop left while the record was written again goes with the
/// next recovery.
#[test]
fn a_record_cut_short_while_written_again_leaves_no_side_copy() {
    let dir = TempDir::new().expect("a temp dir");
    let staging = dir.path().join(".octo-incoming");
    let records = staging.join("replacing");
    std::fs::create_dir_all(&records).expect("made");
    std::fs::write(records.join("rrr.json.tmp"), b"{\"original\":").expect("written");

    placement::recover_replacements(&staging);

    assert!(!records.join("rrr.json.tmp").exists());
}

/// The records folder stays once its last record goes, and a start looks for records in it, not
/// for the folder.
#[tokio::test]
async fn the_records_folder_stays_and_only_a_record_asks_for_recovery() {
    let rig = Rig::new(preset(FolderStructure::Organized));
    let old = rig.has("Daft Punk/Discovery/01 - One More Time.mp3", &Tags::default());
    rig.octos(&old, "Daft Punk", "One More Time");
    let staging = rig.at(".octo-incoming");
    let records = staging.join("replacing");

    rig.place(&track("Daft Punk", "Discovery", "One More Time", 1))
        .await;

    assert!(records.is_dir(), "the records folder stays");
    assert!(holds_no_record(&records));
    assert!(!placement::replacements_pending(&staging));
    std::fs::write(records.join("nnn.json"), "{}").expect("written");
    assert!(placement::replacements_pending(&staging));
}

/// Every rename on this thread is taken for one across file systems while this lives, and
/// `seen` is run with each copy as it is about to take its name.
struct AcrossDisks;

impl AcrossDisks {
    fn new(seen: impl FnMut(&Path, &str) + 'static) -> AcrossDisks {
        placement::COPY_ACROSS.with(|across| across.set(true));
        placement::AFTER_COPY.with(|hook| *hook.borrow_mut() = Some(Box::new(seen)));
        AcrossDisks
    }
}

impl Drop for AcrossDisks {
    fn drop(&mut self) {
        placement::COPY_ACROSS.with(|across| across.set(false));
        placement::AFTER_COPY.with(|hook| *hook.borrow_mut() = None);
    }
}

/// What a copy across file systems looked like as it was about to take its name: its own name,
/// its size, whether the name it takes held a file, and how many records under way marked their
/// copy whole.
type Copied = (String, u64, bool, usize);

/// Watches the copies across file systems a placement under `root` makes.
fn watch_copies(root: &Path) -> (AcrossDisks, std::rc::Rc<std::cell::RefCell<Vec<Copied>>>) {
    let seen: std::rc::Rc<std::cell::RefCell<Vec<Copied>>> = std::rc::Rc::default();
    let records = root.join(".octo-incoming/replacing");
    let across = {
        let seen = seen.clone();
        AcrossDisks::new(move |staged, to| {
            let under_way = std::fs::read_dir(&records).map_or(0, |entries| {
                entries
                    .flatten()
                    .filter(|entry| {
                        std::fs::read(entry.path())
                            .ok()
                            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                            .is_some_and(|record| record["whole"] == serde_json::json!(true))
                    })
                    .count()
            });
            seen.borrow_mut().push((
                staged
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                std::fs::metadata(staged).map_or(0, |meta| meta.len()),
                Path::new(to).exists(),
                under_way,
            ));
        })
    };
    (across, seen)
}

/// A download moved across file systems takes its name only once the whole copy is in a hidden
/// file beside it, with a record while it is under way; then nothing of the move is left.
#[tokio::test]
async fn a_move_across_disks_takes_its_name_only_once_whole() {
    let rig = Rig::new(preset(FolderStructure::Organized));
    let (across, seen) = watch_copies(rig.dir.path());
    let landed = rig.landed();
    let size = std::fs::metadata(&landed).expect("there").len();
    let song = track("Daft Punk", "Discovery", "One More Time", 1);

    let placed = rig
        .harness
        .service
        .place_in_library(&song, &asked(&song), &landed)
        .await;
    drop(across);

    assert_eq!(
        rig.relative(&placed.path),
        "Daft Punk/Discovery/01 - One More Time.mp3"
    );
    assert!(!Path::new(&landed).exists(), "the landed copy went last");
    let seen = seen.borrow().clone();
    assert_eq!(seen.len(), 1, "{seen:?}");
    let (name, copied, taken, under_way) = &seen[0];
    assert!(name.starts_with(".octo-moving-"), "{name}");
    assert_eq!(*copied, size);
    assert!(!taken, "the name stays free until the copy is whole");
    assert_eq!(*under_way, 1, "a record under way that marks it whole");
    let names: Vec<String> = std::fs::read_dir(rig.at("Daft Punk/Discovery"))
        .expect("lists")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["01 - One More Time.mp3"]);
    assert!(holds_no_record(&rig.at(".octo-incoming/replacing")));
}

/// A download moved across file systems writes its record and marks it whole, each with the
/// records folder flushed, and flushes its new name's folder before the file it was copied from
/// goes, so a power cut can never keep the removal and lose the rename, nor lose the record.
#[tokio::test]
async fn a_move_across_disks_flushes_its_name_before_its_source_goes() {
    let rig = Rig::new(preset(FolderStructure::Organized));
    let across = AcrossDisks::new(|_, _| {});
    placement::SETTLING.with(|log| log.borrow_mut().clear());
    let landed = rig.landed();
    let song = track("Daft Punk", "Discovery", "One More Time", 1);

    let placed = rig
        .harness
        .service
        .place_in_library(&song, &asked(&song), &landed)
        .await;
    drop(across);

    let log: Vec<String> = placement::SETTLING.with(|log| log.borrow_mut().drain(..).collect());
    let folder = placement::get_directory_name(&placed.path).expect("a folder");
    let root = slashed(&format!("flush {}", rig.harness.service.download_path()));
    let staging = slashed(&format!(
        "flush {}/.octo-incoming",
        rig.harness.service.download_path()
    ));
    let records = slashed(&format!(
        "flush {}/.octo-incoming/replacing",
        rig.harness.service.download_path()
    ));
    assert_eq!(
        log.iter().map(|step| slashed(step)).collect::<Vec<_>>(),
        [
            staging,
            root,
            records.clone(),
            records,
            slashed(&format!("flush {folder}")),
            slashed(&format!("remove {landed}"))
        ]
    );
    assert!(!Path::new(&landed).exists());
}

/// A download whose landed file a sweep took while it was copied across file systems is placed
/// from its whole copy, and the move leaves no record: a source already gone counts as removed.
#[tokio::test]
async fn a_source_taken_during_its_copy_leaves_no_record() {
    let rig = Rig::new(preset(FolderStructure::Organized));
    let landed = rig.landed();
    let gone = landed.clone();
    let across = AcrossDisks::new(move |_, _| {
        let _ = std::fs::remove_file(&gone);
    });
    let song = track("Daft Punk", "Discovery", "One More Time", 1);

    let placed = rig
        .harness
        .service
        .place_in_library(&song, &asked(&song), &landed)
        .await;
    drop(across);

    assert_eq!(
        rig.relative(&placed.path),
        "Daft Punk/Discovery/01 - One More Time.mp3"
    );
    assert!(Path::new(&placed.path).is_file());
    assert!(holds_no_record(&rig.at(".octo-incoming/replacing")));
}

/// The new copy of a song replacing its old copy across file systems takes the name only once it
/// is whole, while the old copy waits aside; then the old copy goes.
#[tokio::test]
async fn a_replace_across_disks_takes_the_name_only_once_whole() {
    let rig = Rig::new(preset(FolderStructure::Organized));
    let old = rig.has("Daft Punk/Discovery/01 - One More Time.mp3", &Tags::default());
    rig.octos(&old, "Daft Punk", "One More Time");
    let (across, seen) = watch_copies(rig.dir.path());
    let landed = rig.landed();
    let new = std::fs::read(&landed).expect("read");
    let song = track("Daft Punk", "Discovery", "One More Time", 1);

    let placed = rig
        .harness
        .service
        .place_in_library(&song, &asked(&song), &landed)
        .await;
    drop(across);

    assert_eq!(slashed(&placed.path), slashed(&old.to_string_lossy()));
    assert_eq!(std::fs::read(&old).expect("the new copy"), new);
    let seen = seen.borrow().clone();
    assert_eq!(seen.len(), 1, "{seen:?}");
    let (name, copied, taken, under_way) = &seen[0];
    assert!(
        name.starts_with(".octo-replacing-") && name.ends_with(".new"),
        "{name}"
    );
    assert_eq!(*copied, new.len() as u64);
    assert!(!taken, "the name stays free until the copy is whole");
    assert_eq!(*under_way, 1, "a record under way that marks it whole");
    let names: Vec<String> = std::fs::read_dir(rig.at("Daft Punk/Discovery"))
        .expect("lists")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["01 - One More Time.mp3"]);
    assert!(holds_no_record(&rig.at(".octo-incoming/replacing")));
}

#[tokio::test]
async fn a_case_twin_file_of_another_song_is_kept_beside() {
    let rig = Rig::new(preset(FolderStructure::Organized));
    let old = rig.has("A/Album/01 - song.mp3", &Tags::default());
    let before = std::fs::read(&old).expect("read");
    let placed = rig.place(&track("A", "Album", "Song", 1)).await;
    assert_eq!(rig.relative(&placed.path), "A/Album/01 - Song (1).mp3");
    assert_eq!(std::fs::read(&old).expect("read"), before);
}

/// The landed file already sits at its old name, which is the new name in other capitals (or,
/// on Linux, with a character Windows refuses): it is never taken for another file and removed.
#[tokio::test]
async fn a_file_already_at_its_old_rule_name_is_never_removed() {
    let rig = Rig::new(preset(FolderStructure::Organized));
    let mut names = vec!["A/Album/01 - song.mp3"];
    if cfg!(unix) {
        names.push("A/Album/02 - Are You Ready?.mp3");
    }
    for (number, name) in names.into_iter().enumerate() {
        let there = rig.has(name, &Tags::default());
        let title = if number == 0 { "Song" } else { "Are You Ready?" };
        let song = track("A", "Album", title, number as i32 + 1);
        let placed = rig
            .harness
            .service
            .place_in_library(&song, &asked(&song), &there.to_string_lossy())
            .await;
        assert!(there.is_file(), "{name} is still there");
        assert_eq!(slashed(&placed.path), slashed(&there.to_string_lossy()));
    }
}

#[tokio::test]
async fn vol_1_never_joins_vol_12() {
    let rig = Rig::new(preset(FolderStructure::Organized));
    rig.has(
        "Artist/Vol. 12/01 - Twelve.mp3",
        &Tags {
            artist: "Artist",
            album: "Vol. 12",
            ..Tags::default()
        },
    );
    let placed = rig.place(&track("Artist", "Vol. 1", "One", 1)).await;
    assert_eq!(rig.relative(&placed.path), "Artist/Vol. 1/01 - One.mp3");
}

#[tokio::test]
async fn studio_never_joins_live() {
    let rig = Rig::new(preset(FolderStructure::Organized));
    rig.has(
        "Nirvana/Unplugged/01 - About a Girl.mp3",
        &Tags {
            artist: "Nirvana",
            album: "Nevermind (Live)",
            ..Tags::default()
        },
    );
    let placed = rig.place(&track("Nirvana", "Nevermind", "Lithium", 5)).await;
    assert_eq!(rig.relative(&placed.path), "Nirvana/Nevermind/05 - Lithium.mp3");
}

#[tokio::test]
async fn a_mixed_folder_is_never_an_album() {
    let rig = Rig::new(preset(FolderStructure::Organized));
    for (name, album) in [
        ("01 - One More Time.mp3", "Discovery"),
        ("02 - Da Funk.mp3", "Homework"),
    ] {
        rig.has(
            &format!("Daft Punk/Stuff/{name}"),
            &Tags {
                artist: "Daft Punk",
                album,
                ..Tags::default()
            },
        );
    }
    let placed = rig
        .place(&track("Daft Punk", "Discovery", "Aerodynamic", 2))
        .await;
    assert_eq!(
        rig.relative(&placed.path),
        "Daft Punk/Discovery/02 - Aerodynamic.mp3"
    );
}

#[tokio::test]
async fn beyonce_joins_beyonce_accented() {
    let rig = Rig::new(preset(FolderStructure::Organized));
    rig.has("Beyoncé/Lemonade/01 - Pray You Catch Me.mp3", &Tags::default());
    let placed = rig.place(&track("Beyonce", "Lemonade", "Hold Up", 2)).await;
    assert_eq!(rig.relative(&placed.path), "Beyoncé/Lemonade/02 - Hold Up.mp3");
}

/// A key of two letters is too short to say two artists are one: `Mo` and `MØ` stay apart.
#[tokio::test]
async fn mo_never_joins_mo_slashed() {
    let rig = Rig::new(preset(FolderStructure::Organized));
    rig.has("MØ/Forever Neverland/01 - Sun in Our Eyes.mp3", &Tags::default());
    let placed = rig.place(&track("Mo", "Forever Neverland", "Nostalgia", 2)).await;
    assert_eq!(
        rig.relative(&placed.path),
        "Mo/Forever Neverland/02 - Nostalgia.mp3"
    );
}

/// Two spellings of one artist's folder: the one holding more songs, counted through the whole
/// folder, wins, though its name sorts later.
#[tokio::test]
async fn ties_count_files_through_the_whole_folder() {
    let rig = Rig::new(preset(FolderStructure::Organized));
    rig.has("Beyoncé/4/01 - 1+1.mp3", &Tags::default());
    rig.has("beyonce/Lemonade/01 - Pray You Catch Me.mp3", &Tags::default());
    rig.has("beyonce/Lemonade/02 - Hold Up.mp3", &Tags::default());
    let placed = rig
        .place(&track("BEYONCE", "Renaissance", "Alien Superstar", 2))
        .await;
    assert_eq!(
        rig.relative(&placed.path),
        "beyonce/Renaissance/02 - Alien Superstar.mp3"
    );
}

#[tokio::test]
async fn weezer_albums_keep_their_folders_under_year() {
    let rig = Rig::new(custom(BY_YEAR, "", ""));
    rig.has(
        "Weezer/Weezer (1994)/01 - My Name Is Jonas.mp3",
        &Tags {
            artist: "Weezer",
            album: "Weezer",
            album_artist: Some("Weezer"),
            year: 1994,
            release_group: Some("rg-blue"),
            ..Tags::default()
        },
    );
    let green = rig
        .place(&Song {
            year: Some(2001),
            music_brainz_release_group_id: Some("rg-green".into()),
            ..track("Weezer", "Weezer", "Hash Pipe", 3)
        })
        .await;
    let blue = rig
        .place(&Song {
            year: Some(1994),
            music_brainz_release_group_id: Some("rg-blue".into()),
            ..track("Weezer", "Weezer", "Buddy Holly", 4)
        })
        .await;
    assert_eq!(
        rig.relative(&green.path),
        "Weezer/Weezer (2001)/03 - Hash Pipe.mp3"
    );
    assert_eq!(
        rig.relative(&blue.path),
        "Weezer/Weezer (1994)/04 - Buddy Holly.mp3"
    );
}

#[tokio::test]
async fn one_album_on_two_releases_keeps_one_folder_under_year() {
    let rig = Rig::new(custom(BY_YEAR, "", ""));
    rig.has(
        "The Beatles/Abbey Road (1969)/01 - Come Together.mp3",
        &Tags {
            artist: "The Beatles",
            album: "Abbey Road",
            album_artist: Some("The Beatles"),
            year: 1969,
            release_group: Some("rg-abbey"),
            ..Tags::default()
        },
    );
    let remaster = rig
        .place(&Song {
            year: Some(2009),
            music_brainz_release_group_id: Some("RG-ABBEY".into()),
            ..track("The Beatles", "Abbey Road", "Something", 2)
        })
        .await;
    assert_eq!(
        rig.relative(&remaster.path),
        "The Beatles/Abbey Road (1969)/02 - Something.mp3"
    );
}

#[tokio::test]
async fn a_track_without_a_release_group_joins_its_album_under_year() {
    let rig = Rig::new(custom(BY_YEAR, "", ""));
    rig.has(
        "Fleetwood Mac/Rumours (1977)/01 - Second Hand News.mp3",
        &Tags {
            artist: "Fleetwood Mac",
            album: "Rumours",
            album_artist: Some("Fleetwood Mac"),
            year: 1977,
            release_group: Some("rg-rumours"),
            ..Tags::default()
        },
    );
    let reissue_dated = rig
        .place(&Song {
            year: Some(2004),
            ..track("Fleetwood Mac", "Rumours", "Dreams", 2)
        })
        .await;
    assert_eq!(
        rig.relative(&reissue_dated.path),
        "Fleetwood Mac/Rumours (1977)/02 - Dreams.mp3"
    );
}

/// Between two folders of one album, the year most of a folder's files carry is its year, not
/// its first file's.
#[tokio::test]
async fn a_folder_whose_first_file_is_the_minority_year_still_joins() {
    let rig = Rig::new(custom(BY_YEAR, "", ""));
    let beatles = |year| Tags {
        artist: "The Beatles",
        album: "Abbey Road",
        album_artist: Some("The Beatles"),
        year,
        ..Tags::default()
    };
    rig.has("The Beatles/Abbey Road A/01 - Come Together.mp3", &beatles(2009));
    rig.has("The Beatles/Abbey Road A/02 - Something.mp3", &beatles(1969));
    rig.has("The Beatles/Abbey Road A/03 - Maxwell.mp3", &beatles(1969));
    rig.has("The Beatles/Abbey Road B/01 - Come Together.mp3", &beatles(2009));
    let placed = rig
        .place(&Song {
            year: Some(1969),
            ..track("The Beatles", "Abbey Road", "Oh! Darling", 4)
        })
        .await;
    assert_eq!(
        rig.relative(&placed.path),
        "The Beatles/Abbey Road A/04 - Oh! Darling.mp3"
    );
}

/// A rip with no album artist whose songs are by many artists is one Various Artists album.
#[tokio::test]
async fn a_various_artists_rip_without_album_artist_is_one_album() {
    let rig = Rig::new(custom(
        "{Artist}/{Album}/{Track:00} - {Title}",
        "",
        "{Album Artist}/{Album}/{Track:00} - {Track Artist} - {Title}",
    ));
    for (number, artist) in ["Miley Cyrus", "Harry Styles"].iter().enumerate() {
        rig.has(
            &format!("Various Artists/Now 115 (UK)/0{number} - Song.mp3"),
            &Tags {
                artist,
                album: "Now That's What I Call Music! 115",
                ..Tags::default()
            },
        );
    }
    let placed = rig
        .place(&Song {
            album_artist: Some("Various Artists".into()),
            is_compilation: true,
            ..track("Lizzo", "Now That's What I Call Music! 115", "About Damn Time", 4)
        })
        .await;
    assert_eq!(
        rig.relative(&placed.path),
        "Various Artists/Now 115 (UK)/04 - Lizzo - About Damn Time.mp3"
    );
}

/// Under a parent that is every artist's, a folder of the song's exact name joins, and one of
/// another name joins when its songs are the song's artist's own.
#[tokio::test]
async fn an_exact_named_folder_under_an_artistless_parent_joins_when_the_album_matches() {
    let rig = Rig::new(custom("{Album Artist} - {Album}/{Track:00} - {Title}", "", ""));
    let queen = Tags {
        artist: "Queen",
        album: "Greatest Hits",
        album_artist: Some("Queen"),
        ..Tags::default()
    };
    rig.has("Queen - Greatest Hits/01 - Bohemian Rhapsody.mp3", &queen);
    let exact = rig
        .place(&track("Queen", "Greatest Hits", "Another One Bites the Dust", 2))
        .await;
    assert_eq!(
        rig.relative(&exact.path),
        "Queen - Greatest Hits/02 - Another One Bites the Dust.mp3"
    );

    let rig = Rig::new(custom("{Album Artist} - {Album}/{Track:00} - {Title}", "", ""));
    rig.has("Greatest Hits (Queen, 1981)/01 - Bohemian Rhapsody.mp3", &queen);
    let other = rig
        .place(&track("Queen", "Greatest Hits", "Killer Queen", 3))
        .await;
    assert_eq!(
        rig.relative(&other.path),
        "Greatest Hits (Queen, 1981)/03 - Killer Queen.mp3"
    );
}

#[tokio::test]
async fn a_single_and_its_album_track_share_one_folder() {
    let rig = Rig::new(custom(
        "{Artist}/{Album}/{Track:00} - {Title}",
        "{Artist}/Singles/{Title}",
        "",
    ));
    let album_track = rig
        .place(&track("Daft Punk", "Discovery", "One More Time", 1))
        .await;
    // Its tags, as the download writes them after placing it.
    tagged(
        Path::new(&album_track.path),
        &Tags {
            artist: "Daft Punk",
            album: "Discovery",
            album_artist: Some("Daft Punk"),
            ..Tags::default()
        },
    );
    let single = rig
        .place(&Song {
            release_type: Some("single".into()),
            ..track("Daft Punk", "Discovery", "Digital Love", 3)
        })
        .await;
    assert_eq!(
        rig.relative(&album_track.path),
        "Daft Punk/Discovery/01 - One More Time.mp3"
    );
    assert_eq!(
        rig.relative(&single.path),
        "Daft Punk/Discovery/03 - Digital Love.mp3"
    );
    assert_eq!(
        single.values.as_ref().map(|values| values.kind),
        Some(LayoutKind::Album),
        "the values say which template filed it"
    );
}

/// Another kind's folder of the same title joins only on a positive sign: the same release
/// group, or every song by the song's own artist.
#[tokio::test]
async fn another_kinds_folder_never_joins_on_title_alone() {
    let layout = || {
        custom(
            "{Album Artist}/{Album}/{Track:00} - {Title}",
            "",
            "Compilations/{Album}/{Track:00} - {Track Artist} - {Title}",
        )
    };
    let compilation = |group: Option<&str>| Song {
        album_artist: Some("Various Artists".into()),
        is_compilation: true,
        music_brainz_release_group_id: group.map(str::to_string),
        ..track("Queen", "Greatest Hits", "Bohemian Rhapsody", 1)
    };
    let other_compilation = |group: Option<&'static str>| {
        move |rig: &Rig| {
            for artist in ["ABBA", "Blondie"] {
                rig.has(
                    &format!("Various Artists/Greatest Hits/{artist}.mp3"),
                    &Tags {
                        artist,
                        album: "Greatest Hits",
                        album_artist: Some("Various Artists"),
                        release_group: group,
                        ..Tags::default()
                    },
                );
            }
        }
    };

    let rig = Rig::new(layout());
    other_compilation(None)(&rig);
    let apart = rig.place(&compilation(None)).await;
    assert_eq!(
        rig.relative(&apart.path),
        "Compilations/Greatest Hits/01 - Queen - Bohemian Rhapsody.mp3"
    );

    let rig = Rig::new(layout());
    other_compilation(Some("rg-hits"))(&rig);
    let joined = rig.place(&compilation(Some("rg-hits"))).await;
    assert_eq!(
        rig.relative(&joined.path),
        "Various Artists/Greatest Hits/01 - Bohemian Rhapsody.mp3"
    );
}

/// The album search reads one file of each folder it passes over, and only a passing folder
/// whole, so a folder far past the fiftieth is still found.
#[tokio::test]
async fn the_album_search_finds_a_folder_past_fifty_with_one_read_each() {
    let rig = Rig::new(preset(FolderStructure::Organized));
    for n in 0..60 {
        rig.has(
            &format!("Prolific/Old {n:02}/01 - Song.mp3"),
            &Tags {
                artist: "Prolific",
                album: &format!("Album {n:02}"),
                ..Tags::default()
            },
        );
    }
    let before = TAG_READS_HERE.with(|reads| reads.get());
    let placed = rig.place(&track("Prolific", "Album 59", "Second", 2)).await;
    let reads = TAG_READS_HERE.with(|reads| reads.get()) - before;
    assert_eq!(rig.relative(&placed.path), "Prolific/Old 59/02 - Second.mp3");
    assert!(reads <= 61, "one read a folder: {reads}");
}

/// Placing the tracks of one album one after another reads each file of the folder once.
#[tokio::test]
async fn the_album_search_reads_each_file_once_per_walk() {
    let rig = Rig::new(preset(FolderStructure::Organized));
    for n in 1..=10 {
        rig.has(
            &format!("Artist/Old Name/{n:02} - Song.mp3"),
            &Tags {
                artist: "Artist",
                album: "Album",
                ..Tags::default()
            },
        );
    }
    let reads = || TAG_READS_HERE.with(|reads| reads.get());
    let start = reads();
    let first = rig.place(&track("Artist", "Album", "Eleven", 11)).await;
    let after_first = reads();
    assert!(after_first - start <= 10, "{}", after_first - start);
    // The landed file got no tags here, so it reads as of no album; tag it as the album's.
    tagged(
        Path::new(&first.path),
        &Tags {
            artist: "Artist",
            album: "Album",
            ..Tags::default()
        },
    );
    let second = rig.place(&track("Artist", "Album", "Twelve", 12)).await;
    assert_eq!(rig.relative(&second.path), "Artist/Old Name/12 - Twelve.mp3");
    assert!(
        reads() - after_first <= 1,
        "only the new file is read: {}",
        reads() - after_first
    );
}

/// The search reads what the files' tags say now, never what a record saved when they were
/// placed.
#[tokio::test]
async fn the_hook_reads_tags_not_a_stale_saved_value() {
    let rig = Rig::new(preset(FolderStructure::Organized));
    let retagged = rig.has(
        "Daft Punk/Old Name/01 - One More Time.mp3",
        &Tags {
            artist: "Daft Punk",
            album: "Discovery",
            ..Tags::default()
        },
    );
    rig.library.by_tags.lock().push((
        "Daft Punk".into(),
        "One More Time".into(),
        None,
        LocalSongMapping {
            local_path: retagged.to_string_lossy().into_owned(),
            artist: "Daft Punk".into(),
            title: "One More Time".into(),
            album: "Homework".into(),
            layout: Some(LayoutValues {
                artist: "Daft Punk".into(),
                album: "Homework".into(),
                ..LayoutValues::default()
            }),
            ..Default::default()
        },
    ));
    let placed = rig
        .place(&track("Daft Punk", "Discovery", "Aerodynamic", 2))
        .await;
    assert_eq!(
        rig.relative(&placed.path),
        "Daft Punk/Old Name/02 - Aerodynamic.mp3"
    );
}

// ---- names that fit -------------------------------------------------------------------------

/// Every track of an album with a long name renders one folder name, whatever its own title's
/// length, in either order: only the file name shrinks.
#[tokio::test]
async fn every_track_of_a_long_named_album_shares_one_folder() {
    let album = format!(
        "The {} Collection",
        "Extraordinarily Long Named ".repeat(5).trim()
    );
    let short = track("An Artist", &album, "Short", 1);
    let long = track("An Artist", &album, &"Long Title Words ".repeat(14), 2);
    for order in [[&short, &long], [&long, &short]] {
        let rig = Rig::new(preset(FolderStructure::Organized));
        let mut folders = Vec::new();
        for song in order {
            let placed = rig.place(song).await;
            let relative = rig.relative(&placed.path);
            assert!(relative.encode_utf16().count() <= 230, "{relative}");
            folders.push(Path::new(&relative).parent().expect("a folder").to_path_buf());
        }
        assert_eq!(folders[0], folders[1]);
    }
}

/// A deep, long path: the title is cut before the other names in the file name.
#[tokio::test]
async fn the_title_shrinks_before_the_other_names() {
    let rig = Rig::new(custom("{Artist}/{Album}/{Track:00} - {Artist} - {Title}", "", ""));
    let artist = "An Artist Whose Name Runs Forty Units!!";
    let album = "An Album Title That Goes On And On And On Well Past What Fits In A Folder Name Today";
    let title = "A Title So Long That It Has To Be Cut Down To Size Before Anything Else In The Name Is Touched At All";
    let placed = rig.place(&track(artist, album, title, 1)).await;
    let relative = rig.relative(&placed.path);
    assert!(relative.encode_utf16().count() <= 230, "{relative}");
    let file = Path::new(&relative)
        .file_name()
        .and_then(|name| name.to_str())
        .expect("a file name")
        .to_string();
    assert!(
        file.starts_with(&format!("01 - {artist} - A Title So Long")),
        "{file}"
    );
    assert!(
        file.len() < format!("01 - {artist} - {title}.mp3").len(),
        "{file}"
    );
}

// ---- the album folder's cover ---------------------------------------------------------------

const COVER: [u8; 6] = [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];

#[tokio::test]
async fn custom_album_folder_writes_cover() {
    let rig = Rig::new(custom("{Album Artist} - {Album}/{Track:00} - {Title}", "", ""));
    let placed = rig
        .place(&track("Daft Punk", "Discovery", "One More Time", 1))
        .await;
    rig.harness
        .service
        .write_sidecars(&Song::default(), &placed, Some(&COVER));
    assert!(rig.at("Daft Punk - Discovery/cover.jpg").is_file());
}

#[tokio::test]
async fn custom_without_album_folder_writes_none() {
    let rig = Rig::new(custom("{Artist}/{Album} - {Track:00} - {Title}", "", ""));
    let placed = rig
        .place(&track("Daft Punk", "Discovery", "One More Time", 1))
        .await;
    assert_eq!(placed.album_folder, None);
    rig.harness
        .service
        .write_sidecars(&Song::default(), &placed, Some(&COVER));
    assert!(!rig.at("Daft Punk/cover.jpg").exists());
}

#[tokio::test]
async fn disc_layout_writes_one_cover_in_the_album_folder() {
    let rig = Rig::new(custom(DISCS, "", ""));
    for (disc, title) in [(1, "In the Flesh"), (2, "Hey You")] {
        let placed = rig
            .place(&Song {
                disc_number: Some(disc),
                total_discs: Some(2),
                ..track("Pink Floyd", "The Wall", title, 1)
            })
            .await;
        rig.harness
            .service
            .write_sidecars(&Song::default(), &placed, Some(&COVER));
    }
    assert!(rig.at("Pink Floyd/The Wall/cover.jpg").is_file());
    assert!(
        rig.at("Pink Floyd/The Wall/Disc 1/01 - In the Flesh.mp3")
            .is_file()
    );
    assert!(rig.at("Pink Floyd/The Wall/Disc 2/01 - Hey You.mp3").is_file());
    assert!(!rig.at("Pink Floyd/The Wall/Disc 1/cover.jpg").exists());
    assert!(!rig.at("Pink Floyd/The Wall/Disc 2/cover.jpg").exists());
}

// ---- disc folders ---------------------------------------------------------------------------

#[tokio::test]
async fn a_new_disc_two_track_joins_cd2() {
    let rig = Rig::new(custom(DISCS, "", ""));
    let wall = |disc| Tags {
        artist: "Pink Floyd",
        album: "The Wall",
        disc,
        disc_count: 2,
        ..Tags::default()
    };
    rig.has("Pink Floyd/The Wall/CD1/01 - In the Flesh.mp3", &wall(1));
    rig.has("Pink Floyd/The Wall/CD2/01 - Hey You.mp3", &wall(2));
    let placed = rig
        .place(&Song {
            disc_number: Some(2),
            total_discs: Some(2),
            ..track("Pink Floyd", "The Wall", "Is There Anybody Out There?", 2)
        })
        .await;
    assert_eq!(
        rig.relative(&placed.path),
        "Pink Floyd/The Wall/CD2/02 - Is There Anybody Out There？.mp3"
    );
}

/// A disc 1 track whose release did not say how many discs there are joins disc 1's folder of
/// an album already filed by disc.
#[tokio::test]
async fn disc_one_joins_its_disc_folder() {
    let rig = Rig::new(custom(DISCS, "", ""));
    let wall = |disc| Tags {
        artist: "Pink Floyd",
        album: "The Wall",
        disc,
        disc_count: 2,
        ..Tags::default()
    };
    rig.has("Pink Floyd/The Wall/Disc 1/01 - In the Flesh.mp3", &wall(1));
    rig.has("Pink Floyd/The Wall/Disc 2/01 - Hey You.mp3", &wall(2));
    let placed = rig
        .place(&Song {
            disc_number: Some(1),
            ..track("Pink Floyd", "The Wall", "The Thin Ice", 2)
        })
        .await;
    assert_eq!(
        rig.relative(&placed.path),
        "Pink Floyd/The Wall/Disc 1/02 - The Thin Ice.mp3"
    );
    assert_eq!(placed.values.map(|values| values.multi_disc), Some(true));
}

/// Disc 2 is downloaded where Octo placed disc 1 loose: it lands loose beside it, so the album
/// stays one folder, and no file moves.
#[tokio::test]
async fn disc_two_joins_a_loose_album() {
    let song = Song {
        external_provider: Some("test".into()),
        external_id: Some("1".into()),
        disc_number: Some(2),
        total_discs: Some(2),
        ..track("Pink Floyd", "The Wall", "Hey You", 1)
    };
    let rig = Rig::with(
        custom(DISCS, "", ""),
        Some(catalog(vec![("1", song)])),
        DownloadServices::default(),
    );
    rig.lands();
    let wall = Tags {
        artist: "Pink Floyd",
        album: "The Wall",
        disc: 1,
        ..Tags::default()
    };
    let loose = rig.has("Pink Floyd/The Wall/01 - In the Flesh.mp3", &wall);
    rig.octos(&loose, "Pink Floyd", "In the Flesh");

    let path = fetch(&rig).await;

    assert_eq!(rig.relative(&path), "Pink Floyd/The Wall/01 - Hey You.mp3");
    assert!(loose.is_file(), "placement never moves another song");
    assert_eq!(
        names_in(&rig.at("Pink Floyd/The Wall")),
        ["01 - Hey You.mp3", "01 - In the Flesh.mp3"],
        "one folder, no disc folder"
    );
}

/// Disc 2's "Intro" is downloaded where Octo placed disc 1's "Intro" loose, neither with a
/// recording id: they are two songs of the album, so disc 1 is never replaced. Disc 2 takes the
/// next free name, and disc 1's file and record stay as they were.
#[tokio::test]
async fn a_new_disc_never_replaces_the_loose_track_of_the_same_name() {
    new_disc_beside_its_loose_namesake(1).await;
}

/// As [`a_new_disc_never_replaces_the_loose_track_of_the_same_name`] with disc 1's file tagged
/// with no disc and Octo's record of it saying none: a file that says no disc is disc 1.
#[tokio::test]
async fn a_new_disc_never_replaces_an_untagged_loose_track_of_the_same_name() {
    new_disc_beside_its_loose_namesake(0).await;
}

/// Disc 2's "Intro" downloaded beside disc 1's loose "Intro", whose file carries `disc_tag` (0
/// for none): both stay, disc 2 under the next free name.
async fn new_disc_beside_its_loose_namesake(disc_tag: u32) {
    let song = Song {
        external_provider: Some("test".into()),
        external_id: Some("1".into()),
        disc_number: Some(2),
        total_discs: Some(2),
        ..track("Pink Floyd", "The Wall", "Intro", 1)
    };
    let rig = Rig::with(
        custom(DISCS, "", ""),
        Some(catalog(vec![("1", song)])),
        DownloadServices::default(),
    );
    rig.lands();
    let disc_one = Tags {
        title: "Intro",
        artist: "Pink Floyd",
        album: "The Wall",
        track: 1,
        disc: disc_tag,
        ..Tags::default()
    };
    let loose = rig.has("Pink Floyd/The Wall/01 - Intro.mp3", &disc_one);
    rig.octos(&loose, "Pink Floyd", "Intro");
    let before = std::fs::read(&loose).expect("read");

    let path = fetch(&rig).await;

    assert_eq!(
        rig.relative(&path),
        "Pink Floyd/The Wall/01 - Intro (1).mp3",
        "disc 2 takes the next free name"
    );
    assert!(Path::new(&path).is_file());
    assert_eq!(
        std::fs::read(&loose).ok(),
        Some(before),
        "disc 1's file is as it was"
    );
    assert_eq!(
        names_in(&rig.at("Pink Floyd/The Wall")).len(),
        2,
        "both songs stay"
    );
    let loose_path = slashed(&loose.to_string_lossy());
    assert!(
        rig.library
            .by_tags
            .lock()
            .iter()
            .any(|(.., mapping)| slashed(&mapping.local_path) == loose_path),
        "disc 1's record still names its file"
    );
    assert!(
        !rig.library
            .registered
            .lock()
            .iter()
            .any(|registered| slashed(registered) == loose_path),
        "nothing else is recorded at disc 1's file"
    );
}

/// A song of disc 1 still replaces Octo's own copy of it whose file says no disc and whose
/// record saved none: a file that says no disc is disc 1.
#[tokio::test]
async fn a_disc_one_song_replaces_its_untagged_copy() {
    replaces_its_untagged_copy(1, None).await;
}

/// A song of disc 2 replaces Octo's own copy of it whose file says no disc when Octo's record of
/// that file saved disc 2.
#[tokio::test]
async fn a_song_replaces_its_untagged_copy_on_the_disc_octo_saved() {
    replaces_its_untagged_copy(2, Some(2)).await;
}

/// A song with no disc number is disc 1: placed where Octo's loose file of the same artist,
/// album, track and title is tagged disc 2, it never replaces that file. Both stay, the new one
/// under the next free name.
#[tokio::test]
async fn a_song_without_a_disc_never_replaces_a_loose_track_of_disc_two() {
    let rig = Rig::new(custom("{Artist}/{Album}/{Track:00} - {Title}", "", ""));
    let disc_two = Tags {
        title: "Intro",
        artist: "Pink Floyd",
        album: "The Wall",
        track: 1,
        disc: 2,
        ..Tags::default()
    };
    let loose = rig.has("Pink Floyd/The Wall/01 - Intro.mp3", &disc_two);
    rig.octos(&loose, "Pink Floyd", "Intro");
    let before = std::fs::read(&loose).expect("read");

    let placed = rig
        .place(&Song {
            disc_number: None,
            ..track("Pink Floyd", "The Wall", "Intro", 1)
        })
        .await;

    assert_eq!(
        rig.relative(&placed.path),
        "Pink Floyd/The Wall/01 - Intro (1).mp3",
        "the new song takes the next free name"
    );
    assert_eq!(
        std::fs::read(&loose).ok(),
        Some(before),
        "disc 2's file is as it was"
    );
    assert_eq!(
        names_in(&rig.at("Pink Floyd/The Wall")),
        ["01 - Intro (1).mp3", "01 - Intro.mp3"]
    );
}

/// A song of `disc` placed where Octo's own copy of it lies, whose file carries no disc tag and
/// whose record saved `saved_disc`: the new file takes its place, one file under its name.
async fn replaces_its_untagged_copy(disc: i32, saved_disc: Option<i32>) {
    let rig = Rig::new(custom("{Artist}/{Album}/{Track:00} - {Title}", "", ""));
    let untagged = Tags {
        title: "Intro",
        artist: "Pink Floyd",
        album: "The Wall",
        track: 1,
        ..Tags::default()
    };
    let old = rig.has("Pink Floyd/The Wall/01 - Intro.mp3", &untagged);
    rig.library.by_tags.lock().push((
        "Pink Floyd".into(),
        "Intro".into(),
        None,
        LocalSongMapping {
            local_path: old.to_string_lossy().into_owned(),
            artist: "Pink Floyd".into(),
            title: "Intro".into(),
            layout: saved_disc.map(|disc| LayoutValues {
                disc: Some(disc),
                ..LayoutValues::default()
            }),
            ..Default::default()
        },
    ));
    let before = std::fs::read(&old).expect("read");

    let placed = rig
        .place(&Song {
            disc_number: Some(disc),
            ..track("Pink Floyd", "The Wall", "Intro", 1)
        })
        .await;

    assert_eq!(
        slashed(&placed.path),
        slashed(&old.to_string_lossy()),
        "it takes its copy's name"
    );
    assert_ne!(
        std::fs::read(&old).ok(),
        Some(before),
        "the new file is in its place"
    );
    assert_eq!(names_in(&rig.at("Pink Floyd/The Wall")), ["01 - Intro.mp3"]);
}

/// The names in `folder`, sorted.
fn names_in(folder: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(folder)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// A file a Lidarr follow held, placed as disc 2 beside its album's loose disc 1 once the follow
/// ends, lands loose beside it, and the library's record of it moves to where it went.
#[tokio::test]
async fn a_held_new_disc_lands_loose_beside_its_album() {
    let rig = Rig::new(custom(DISCS, "", ""));
    let wall = Tags {
        artist: "Pink Floyd",
        album: "The Wall",
        disc: 1,
        ..Tags::default()
    };
    let loose = rig.has("Pink Floyd/The Wall/01 - In the Flesh.mp3", &wall);
    rig.octos(&loose, "Pink Floyd", "In the Flesh");
    let held_path = rig.at(".octo-incoming/held/lidarr-follow-test_1/hey.mp3");
    std::fs::create_dir_all(held_path.parent().expect("a folder")).expect("made");
    std::fs::write(&held_path, mp3()).expect("written");
    let held_path = held_path.to_string_lossy().into_owned();
    let song = Song {
        disc_number: Some(2),
        total_discs: Some(2),
        ..track("Pink Floyd", "The Wall", "Hey You", 1)
    };
    let held = HeldPlacement {
        song: "test:1".into(),
        held_path: held_path.clone(),
        target: String::new(),
        kept: false,
        follows: Vec::new(),
        users: Vec::new(),
        proven_for: Vec::new(),
        values: Some(BaseDownloadService::layout_values(&song, &asked(&song), false)),
        root: String::new(),
    };

    let HeldEnd::Placed(placed) = rig.harness.service.place_held_file(&held).await else {
        panic!("the held file was not placed");
    };

    assert_eq!(rig.relative(&placed), "Pink Floyd/The Wall/01 - Hey You.mp3");
    assert!(loose.is_file(), "placement never moves another song");
    assert!(
        rig.library
            .moved
            .lock()
            .iter()
            .any(|(from, to)| slashed(from) == slashed(&held_path) && slashed(to) == slashed(&placed)),
        "the record moved with it"
    );
}

/// A held file placed once its Lidarr follow ends puts right a replace a stop left half done
/// before it moves anything, as every route that places does.
#[tokio::test]
async fn the_held_file_route_puts_a_stopped_replace_right_first() {
    let rig = Rig::new(preset(FolderStructure::Organized));
    let stopped = StoppedReplace::under(rig.dir.path(), "eee", None);
    let held_path = rig.at(".octo-incoming/held/lidarr-follow-test_2/aero.mp3");
    std::fs::create_dir_all(held_path.parent().expect("a folder")).expect("made");
    std::fs::write(&held_path, mp3()).expect("written");
    let song = track("Daft Punk", "Discovery", "Aerodynamic", 2);
    let held = HeldPlacement {
        song: "test:2".into(),
        held_path: held_path.to_string_lossy().into_owned(),
        target: String::new(),
        kept: false,
        follows: Vec::new(),
        users: Vec::new(),
        proven_for: Vec::new(),
        values: Some(BaseDownloadService::layout_values(&song, &asked(&song), false)),
        root: String::new(),
    };

    let HeldEnd::Placed(placed) = rig.harness.service.place_held_file(&held).await else {
        panic!("the held file was not placed");
    };

    assert_eq!(rig.relative(&placed), "Daft Punk/Discovery/02 - Aerodynamic.mp3");
    stopped.assert_put_back();
}

/// Disc 2 arrives where disc 1 sits loose among the person's own files: it lands loose beside
/// them, so the album keeps one shape.
#[tokio::test]
async fn disc_two_joins_a_loose_album_of_your_own_files() {
    let rig = Rig::new(custom(DISCS, "", ""));
    let wall = Tags {
        artist: "Pink Floyd",
        album: "The Wall",
        disc: 1,
        ..Tags::default()
    };
    let theirs = rig.has("Pink Floyd/The Wall/01 - In the Flesh.mp3", &wall);
    let ours = rig.has("Pink Floyd/The Wall/02 - The Thin Ice.mp3", &wall);
    rig.octos(&ours, "Pink Floyd", "The Thin Ice");
    let placed = rig
        .place(&Song {
            disc_number: Some(2),
            total_discs: Some(2),
            ..track("Pink Floyd", "The Wall", "Hey You", 1)
        })
        .await;

    assert_eq!(rig.relative(&placed.path), "Pink Floyd/The Wall/01 - Hey You.mp3");
    assert!(theirs.is_file() && ours.is_file());
    assert_eq!(placed.values.map(|values| values.multi_disc), Some(false));
}

/// A member's download into the family pool, where disc 1 of its album sits loose, lands loose
/// beside it, and the pool's files stay where they are.
#[tokio::test]
async fn a_member_download_lands_loose_beside_the_pools_album() {
    let rig = Rig::new(custom(DISCS, "", ""));
    let pool = rig.at("members/.pool");
    let wall = Tags {
        artist: "Pink Floyd",
        album: "The Wall",
        disc: 1,
        ..Tags::default()
    };
    let pooled = tagged(&pool.join("Pink Floyd/The Wall/01 - In the Flesh.mp3"), &wall);
    rig.octos(&pooled, "Pink Floyd", "In the Flesh");
    let song = Song {
        disc_number: Some(2),
        total_discs: Some(2),
        ..track("Pink Floyd", "The Wall", "Hey You", 1)
    };
    let placed = rig
        .harness
        .service
        .place_in_root(&song, &asked(&song), &rig.landed(), &pool.to_string_lossy())
        .await;

    assert_eq!(
        slashed(&placed.path),
        slashed(
            &pool
                .join("Pink Floyd/The Wall/01 - Hey You.mp3")
                .to_string_lossy()
        )
    );
    assert!(pooled.is_file(), "the pool's file is where it was");
}

// ---- failures -------------------------------------------------------------------------------

/// The landed file is gone by the time it is moved: the folders the placement made go again,
/// deepest first, and only while empty.
#[tokio::test]
async fn a_failed_placement_removes_the_folders_it_made() {
    let rig = Rig::new(preset(FolderStructure::Organized));
    rig.has("Artist/Other Album/01 - Kept.mp3", &Tags::default());
    let landed = rig.landed();
    let gone = landed.clone();
    *rig.harness.service.settle_pause.lock() = Some(Arc::new(move |_| {
        let gone = gone.clone();
        async move {
            let _ = std::fs::remove_file(&gone);
        }
        .boxed()
    }));
    let song = track("Artist", "New Album", "Song", 1);
    let placed = rig
        .harness
        .service
        .place_in_library(&song, &asked(&song), &landed)
        .await;

    assert!(placed.failed.is_some());
    assert!(!rig.at("Artist/New Album").exists(), "the folder it made is gone");
    assert!(rig.at("Artist/Other Album/01 - Kept.mp3").is_file());
}

/// A song that could not be filed fails its download, though the file it came as is still there
/// (a file sits where its artist's folder must be made): no history, no record of the song in
/// the staging folder no scan sees, and the next source runs.
#[tokio::test]
async fn a_failed_placement_fails_the_download() {
    let song = Song {
        external_provider: Some("test".into()),
        external_id: Some("1".into()),
        ..track("Artist", "Album", "Song", 1)
    };
    let rig = Rig::with(
        preset(FolderStructure::Organized),
        Some(catalog(vec![("1", song)])),
        DownloadServices::default(),
    );
    std::fs::write(rig.at("Artist"), b"not a folder").expect("written");
    let incoming = rig.at(".octo-incoming/slskd/job");
    rig.harness.backend.land(move |_, _| {
        std::fs::create_dir_all(&incoming).expect("made");
        let path = incoming.join("song.mp3");
        std::fs::write(&path, mp3()).expect("written");
        path.to_string_lossy().into_owned()
    });

    let downloaded = rig
        .harness
        .service
        .download_song("test", "1", &CancellationToken::new())
        .await;

    assert!(downloaded.is_err(), "{downloaded:?}");
    assert!(rig.harness.history.get_recent(1).is_empty());
    let registered = rig.library.registered.lock().clone();
    assert!(
        !registered
            .iter()
            .any(|path| slashed(path).contains("/.octo-incoming/")),
        "{registered:?}"
    );
}

// ---- one placement at a time ----------------------------------------------------------------

/// Two placements of one album under two spellings of its artist: the second waits for the
/// first, then joins the folder the first made.
#[tokio::test]
async fn concurrent_placements_make_one_folder() {
    let rig = Arc::new(Rig::new(preset(FolderStructure::Organized)));
    let paused = Arc::new(AtomicBool::new(false));
    let go = Arc::new(tokio::sync::Notify::new());
    {
        let (paused, go) = (paused.clone(), go.clone());
        *rig.harness.service.settle_pause.lock() = Some(Arc::new(move |_| {
            let first = !paused.swap(true, Ordering::SeqCst);
            let go = go.clone();
            async move {
                if first {
                    let _ = tokio::time::timeout(Duration::from_secs(5), go.notified()).await;
                }
            }
            .boxed()
        }));
    }
    let first = {
        let rig = rig.clone();
        tokio::spawn(async move {
            rig.place(&track("daft punk", "Discovery", "One More Time", 1))
                .await
        })
    };
    while !paused.load(Ordering::SeqCst) {
        tokio::task::yield_now().await;
    }
    let second = {
        let rig = rig.clone();
        tokio::spawn(async move {
            rig.place(&track("Daft Punk", "Discovery", "Aerodynamic", 2))
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    go.notify_one();
    let first = first.await.expect("placed");
    let second = second.await.expect("placed");

    assert_eq!(
        rig.relative(&first.path),
        "daft punk/Discovery/01 - One More Time.mp3"
    );
    assert_eq!(
        rig.relative(&second.path),
        "daft punk/Discovery/02 - Aerodynamic.mp3"
    );
}

// ---- the album's facts and the saved values -------------------------------------------------

/// A meter that, while the song is identified, makes the album's folder the way a pool promotion
/// or a tidy rename would, outside the download lock, with these files of Rumours by their
/// years, then measures nothing.
struct FolderMaker {
    folder: PathBuf,
    files: Vec<(&'static str, u32)>,
}

impl FolderMaker {
    fn shared(folder: &Path, files: &[(&'static str, u32)]) -> Arc<dyn ILoudnessMeter> {
        Arc::new(FolderMaker {
            folder: folder.to_path_buf(),
            files: files.to_vec(),
        })
    }
}

#[async_trait]
impl ILoudnessMeter for FolderMaker {
    async fn measure(&self, _: &Path, _: i32, _: &CancellationToken) -> Option<Loudness> {
        tokio::time::sleep(Duration::from_millis(500)).await;
        for (number, (name, year)) in self.files.iter().enumerate() {
            tagged(
                &self.folder.join(name),
                &Tags {
                    artist: "Fleetwood Mac",
                    album: "Rumours",
                    album_artist: Some("Fleetwood Mac"),
                    year: *year,
                    track: number as u32 + 1,
                    disc: 1,
                    disc_count: 1,
                    release_group: Some("rg-rumours"),
                    ..Tags::default()
                },
            );
        }
        None
    }
}

/// A download of Dreams (dated by a 2004 reissue) under `subsonic`, with the meter making the
/// album's folder `folder` (under the music folder) holding `files` while the song is
/// identified. Answers the music folder and where the song went.
async fn dreams_beside(
    subsonic: SubsonicSettings,
    folder: &str,
    files: &[(&'static str, u32)],
) -> (TempDir, String) {
    let dreams = Song {
        external_provider: Some("test".into()),
        external_id: Some("1".into()),
        year: Some(2004),
        ..track("Fleetwood Mac", "Rumours", "Dreams", 2)
    };
    let dir = tempfile::tempdir().expect("a temp dir");
    let harness = build(
        dir.path(),
        AppSettings {
            subsonic,
            ..Default::default()
        },
        None,
        Some(catalog(vec![("1", dreams)])),
        DownloadServices {
            loudness_meter: Some(FolderMaker::shared(&dir.path().join(folder), files)),
            ..Default::default()
        },
    );
    let peer = dir.path().join("peer");
    harness.backend.land(move |_, _| {
        std::fs::create_dir_all(&peer).expect("made");
        let path = peer.join("dreams.mp3");
        std::fs::write(&path, mp3()).expect("written");
        path.to_string_lossy().into_owned()
    });
    let path = harness
        .service
        .download_song("test", "1", &CancellationToken::new())
        .await
        .expect("downloaded");
    (dir, path)
}

/// `{Year}` in the file name: the album's folder appears while the song is identified, its
/// first file of a minority year. Placement takes the album's year from a file of the year most
/// of the folder's files carry and names the file again with it, so the name says what the tags
/// say.
#[tokio::test]
async fn a_year_in_the_file_name_follows_the_album_facts_placement_takes() {
    let (dir, path) = dreams_beside(
        custom("{Artist}/{Album}/{Year} - {Track:00} - {Title}", "", ""),
        "Fleetwood Mac/Rumours",
        &[
            ("01 - Second Hand News.mp3", 2004),
            ("03 - Never Going Back Again.mp3", 1977),
            ("04 - Don't Stop.mp3", 1977),
        ],
    )
    .await;

    let root = slashed(&dir.path().to_string_lossy());
    assert_eq!(
        slashed(&path),
        format!("{root}/Fleetwood Mac/Rumours/1977 - 02 - Dreams.mp3")
    );
    assert_eq!(TagFile::open(&path).expect("opens").year(), 1977);
}

/// Identify's album sibling is the file placement takes the album's facts from: of the year
/// most of the folder's files carry, not the folder's first file.
#[tokio::test]
async fn identify_takes_the_album_facts_from_a_majority_year_file() {
    let rig = Rig::new(custom("{Artist}/{Album}/{Year} - {Track:00} - {Title}", "", ""));
    let beatles = |year| Tags {
        artist: "The Beatles",
        album: "Abbey Road",
        album_artist: Some("The Beatles"),
        year,
        ..Tags::default()
    };
    rig.has("The Beatles/Abbey Road/01 - Come Together.mp3", &beatles(2009));
    rig.has("The Beatles/Abbey Road/02 - Something.mp3", &beatles(1969));
    rig.has("The Beatles/Abbey Road/03 - Maxwell.mp3", &beatles(1969));
    let mut song = Song {
        year: Some(2004),
        ..track("The Beatles", "Abbey Road", "Oh! Darling", 4)
    };
    let requested = asked(&song);
    let landed = rig.landed();

    rig.harness
        .service
        .identify(&mut song, &requested, &landed, None)
        .await;

    assert!(song.tag_plan.is_some(), "identified");
    assert_eq!(song.year, Some(1969));
}

/// The album's facts come from a file that carries the folder's own album artist, so the song
/// is never given a spelling that would make it another album than the folder's.
#[tokio::test]
async fn identify_takes_the_album_artist_the_folder_has() {
    let rig = Rig::new(preset(FolderStructure::Organized));
    let beatles = |album_artist| Tags {
        artist: "The Beatles",
        album: "Abbey Road",
        album_artist: Some(album_artist),
        year: 1969,
        ..Tags::default()
    };
    rig.has(
        "The Beatles/Abbey Road/01 - Come Together.mp3",
        &beatles("The Beatles and Friends"),
    );
    rig.has(
        "The Beatles/Abbey Road/02 - Something.mp3",
        &beatles("The Beatles"),
    );
    rig.has("The Beatles/Abbey Road/03 - Maxwell.mp3", &beatles("The Beatles"));
    let mut song = track("The Beatles", "Abbey Road", "Oh! Darling", 4);
    let requested = asked(&song);
    let landed = rig.landed();

    rig.harness
        .service
        .identify(&mut song, &requested, &landed, None)
        .await;

    assert!(song.tag_plan.is_some(), "identified");
    assert_eq!(song.album_artist.as_deref(), Some("The Beatles"));
}

/// A song's values from its tags alone, as a promotion or a shared copy files it: a Various
/// Artists album artist makes it a compilation as placement decides one, and tags with no title
/// or no artist name no song.
#[test]
fn values_from_tags_file_a_various_artists_album_as_a_compilation() {
    let tags = LayoutTags {
        title: Some("About Damn Time".into()),
        performers: vec!["Lizzo".into()],
        album: Some("Now 115".into()),
        album_artist: Some("VA".into()),
        release_type: Some("album".into()),
        ..LayoutTags::default()
    };
    let values = placement::values_from_tags(&tags).expect("a song");
    assert_eq!(
        (values.kind, values.album_type.as_deref()),
        (LayoutKind::Compilation, Some("Compilation"))
    );
    assert_eq!(values.artist, "Lizzo");
    assert!(
        placement::values_from_tags(&LayoutTags {
            title: None,
            ..tags.clone()
        })
        .is_none()
    );
    assert!(
        placement::values_from_tags(&LayoutTags {
            performers: Vec::new(),
            ..tags
        })
        .is_none()
    );
}

#[tokio::test]
async fn album_facts_are_pinned_from_the_folder_settle_chose() {
    let dreams = Song {
        external_provider: Some("test".into()),
        external_id: Some("1".into()),
        year: Some(2004),
        ..track("Fleetwood Mac", "Rumours", "Dreams", 2)
    };
    let dir = tempfile::tempdir().expect("a temp dir");
    let folder = dir.path().join("Fleetwood Mac/Rumours (Deluxe)");
    let harness = build(
        dir.path(),
        AppSettings {
            subsonic: preset(FolderStructure::Organized),
            ..Default::default()
        },
        None,
        Some(catalog(vec![("1", dreams)])),
        DownloadServices {
            loudness_meter: Some(FolderMaker::shared(
                &folder,
                &[("01 - Second Hand News.mp3", 1977)],
            )),
            ..Default::default()
        },
    );
    let peer = dir.path().join("peer");
    harness.backend.land(move |_, _| {
        std::fs::create_dir_all(&peer).expect("made");
        let path = peer.join("dreams.mp3");
        std::fs::write(&path, mp3()).expect("written");
        path.to_string_lossy().into_owned()
    });

    let path = harness
        .service
        .download_song("test", "1", &CancellationToken::new())
        .await
        .expect("downloaded");

    assert_eq!(
        slashed(&path),
        slashed(&folder.join("02 - Dreams.mp3").to_string_lossy())
    );
    let written = TagFile::open(&path).expect("opens");
    assert_eq!(written.year(), 1977, "the album's year, not the reissue's");
    assert_eq!(written.first_album_artist().as_deref(), Some("Fleetwood Mac"));
}

/// The mapping keeps what the names were filled from, with the tags as written.
#[tokio::test]
async fn the_mapping_keeps_the_layout_values() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let store = Arc::new(octo_core::settings::SettingsStore::for_tests(
        AppSettings::default(),
    ));
    store.set_raw("Library:DownloadPath", Some(&dir.path().to_string_lossy()));
    let http = reqwest::Client::new();
    let library = Arc::new(LocalLibraryService::new(
        store.clone(),
        http.clone(),
        Arc::new(crate::services::soulseek::ExternalIdRegistry::in_memory()),
        crate::services::subsonic::NavidromeIdentityService::new(store, http),
    ));
    let song = Song {
        external_provider: Some("test".into()),
        external_id: Some("1".into()),
        year: Some(2001),
        disc_number: Some(1),
        total_discs: Some(1),
        ..track("Daft Punk", "Discovery", "One More Time", 1)
    };
    let harness = build(
        dir.path(),
        AppSettings {
            subsonic: preset(FolderStructure::Organized),
            ..Default::default()
        },
        Some(library.clone() as Arc<dyn ILocalLibraryService>),
        Some(catalog(vec![("1", song)])),
        DownloadServices::default(),
    );
    let peer = dir.path().join("peer");
    harness.backend.land(move |_, _| {
        std::fs::create_dir_all(&peer).expect("made");
        let path = peer.join("one.mp3");
        std::fs::write(&path, mp3()).expect("written");
        path.to_string_lossy().into_owned()
    });

    let path = harness
        .service
        .download_song("test", "1", &CancellationToken::new())
        .await
        .expect("downloaded");

    let mappings = library.get_mappings().await;
    let mapping = mappings
        .iter()
        .find(|mapping| slashed(&mapping.local_path) == slashed(&path))
        .expect("registered");
    let values = mapping.layout.clone().expect("the layout values are kept");
    assert_eq!(
        (
            values.artist.as_str(),
            values.album.as_str(),
            values.track,
            values.kind
        ),
        ("Daft Punk", "Discovery", Some(1), LayoutKind::Album)
    );
    let tags = values.tags.expect("the tags as written");
    let written = TagFile::open(&path).expect("opens");
    let number = |value: u32| (value > 0).then_some(value as i32);
    assert_eq!(
        tags.title,
        SongIdentity::key(&written.title().unwrap_or_default())
    );
    assert_eq!(
        tags.album,
        SongIdentity::key(&written.album().unwrap_or_default())
    );
    assert_eq!(tags.title, SongIdentity::key("One More Time"));
    assert_eq!(
        (tags.track, tags.disc, tags.disc_count, tags.year),
        (
            number(written.track()),
            number(written.disc()),
            number(written.disc_count()),
            number(written.year())
        )
    );
    assert_eq!(tags.track, Some(1));
}

/// The folders under `root` and their entries, as the File layout task's walk lists them, with
/// `/` between names.
fn listed(root: &Path) -> std::collections::BTreeMap<String, Vec<(String, bool)>> {
    let mut folders = std::collections::BTreeMap::new();
    let mut waiting = vec![root.to_path_buf()];
    while let Some(folder) = waiting.pop() {
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(&folder).expect("lists").flatten() {
            let is_dir = entry.path().is_dir();
            if is_dir {
                waiting.push(entry.path());
            }
            entries.push((entry.file_name().to_string_lossy().into_owned(), is_dir));
        }
        entries.sort();
        folders.insert(slashed(&folder.to_string_lossy()), entries);
    }
    folders
}

/// Songs downloaded and tagged by the real tag writer are already in the layout: the File layout
/// task plans no move for them. The writer leaves some tags otherwise than the song says (an
/// album artist it fills in from the main artist, a release type in several values), and the
/// values saved with the mapping keep the tags as the file has them, so none of that reads as a
/// later edit. A song with only a release date carries that date's year in its tags, as in its
/// path.
#[tokio::test]
async fn songs_tagged_by_the_writer_are_already_in_the_layout() {
    use crate::services::library::tidy::layout_pass::{
        self, LayoutDisk, LayoutFile, LayoutMapping, LayoutOptions,
    };
    use crate::services::library::tidy::planner::TidyKind;

    let dir = tempfile::tempdir().expect("a temp dir");
    let store = Arc::new(octo_core::settings::SettingsStore::for_tests(
        AppSettings::default(),
    ));
    store.set_raw("Library:DownloadPath", Some(&dir.path().to_string_lossy()));
    let http = reqwest::Client::new();
    let library = Arc::new(LocalLibraryService::new(
        store.clone(),
        http.clone(),
        Arc::new(crate::services::soulseek::ExternalIdRegistry::in_memory()),
        crate::services::subsonic::NavidromeIdentityService::new(store, http),
    ));
    // The catalog names Kendrick Lamar as the main artist, though the credit starts with SZA:
    // the writer fills the album artist in from the main artist, while the folder is named by
    // the credit.
    let stars = Song {
        external_provider: Some("test".into()),
        external_id: Some("1".into()),
        primary_artist: Some("Kendrick Lamar".into()),
        release_date: Some("2018-02-09".into()),
        release_type: Some("album".into()),
        ..track(
            "SZA, Kendrick Lamar",
            "Black Panther The Album",
            "All The Stars",
            14,
        )
    };
    let compiled = Song {
        external_provider: Some("test".into()),
        external_id: Some("2".into()),
        year: Some(2006),
        album_artist: Some("Daft Punk".into()),
        release_type: Some("album; compilation".into()),
        ..track("Daft Punk", "Musique Vol. 1", "Da Funk", 1)
    };
    let subsonic = custom(
        "{Album Artist}/{Album} ({Year})/{Track:00} - {Title}",
        "",
        "Compilations/{Album} ({Year})/{Track:00} - {Track Artist} - {Title}",
    );
    let layout = octo_core::common::Layout::from_settings(&subsonic);
    let harness = build(
        dir.path(),
        AppSettings {
            subsonic,
            ..Default::default()
        },
        Some(library.clone() as Arc<dyn ILocalLibraryService>),
        Some(catalog(vec![("1", stars), ("2", compiled)])),
        DownloadServices::default(),
    );
    let peer = dir.path().join("peer");
    harness.backend.land(move |_, _| {
        std::fs::create_dir_all(&peer).expect("made");
        let path = peer.join(format!("{}.mp3", uuid::Uuid::new_v4().simple()));
        std::fs::write(&path, mp3()).expect("written");
        path.to_string_lossy().into_owned()
    });

    let mut placed = Vec::new();
    for id in ["1", "2"] {
        placed.push(
            harness
                .service
                .download_song("test", id, &CancellationToken::new())
                .await
                .expect("downloaded"),
        );
    }

    let root = slashed(&dir.path().to_string_lossy());
    assert_eq!(
        placed.iter().map(|path| slashed(path)).collect::<Vec<_>>(),
        [
            format!("{root}/SZA, Kendrick Lamar/Black Panther The Album (2018)/14 - All The Stars.mp3"),
            format!("{root}/Compilations/Musique Vol. 1 (2006)/01 - Daft Punk - Da Funk.mp3"),
        ]
    );
    assert_eq!(TagFile::open(&placed[0]).expect("opens").year(), 2018);
    let mappings = library.get_mappings().await;
    let files: Vec<LayoutFile> = placed
        .iter()
        .map(|path| {
            let mapping = mappings
                .iter()
                .find(|mapping| slashed(&mapping.local_path) == slashed(path))
                .expect("registered");
            LayoutFile {
                path: slashed(path),
                size: std::fs::metadata(path).expect("there").len(),
                in_scope: true,
                tags: read_layout_tags(Path::new(path)),
                mapping: Some(LayoutMapping {
                    title: mapping.title.clone(),
                    album: mapping.album.clone(),
                    artist: mapping.artist.clone(),
                    values: mapping.layout.clone(),
                }),
            }
        })
        .collect();
    let disk = LayoutDisk {
        folders: listed(dir.path()),
        ignored: Vec::new(),
    };
    let plan = layout_pass::plan(&root, &files, &disk, &LayoutOptions::new(layout));

    let moves: Vec<(String, String)> = plan
        .plan
        .moves
        .iter()
        .map(|m| (m.from.clone(), m.to.clone()))
        .chain(
            plan.plan
                .file_moves
                .iter()
                .map(|m| (m.from.clone(), m.to.clone())),
        )
        .collect();
    assert!(moves.is_empty(), "{moves:?}");
    assert!(
        !plan.plan.rows.iter().any(|row| row.kind == TidyKind::Layout),
        "{:?}",
        plan.plan.rows
    );
}

// ---- the disc count -------------------------------------------------------------------------

/// A catalog that knows the song, and lists its album with two discs.
fn the_wall(listed: bool) -> Arc<FakeMetadata> {
    let song = |id: &str, disc, title: &str| Song {
        external_provider: Some("test".into()),
        external_id: Some(id.into()),
        album_id: Some(format!("{TEST_ALBUM}wall")),
        disc_number: Some(disc),
        ..track("Pink Floyd", "The Wall", title, 1)
    };
    let metadata = catalog(vec![(
        "1",
        Song {
            disc_number: None,
            ..song("1", 1, "In the Flesh")
        },
    )]);
    if listed {
        metadata.albums.lock().insert(
            ("test".to_string(), "wall".to_string()),
            Album {
                title: "The Wall".into(),
                songs: vec![song("1", 1, "In the Flesh"), song("2", 2, "Hey You")],
                ..Default::default()
            },
        );
    }
    metadata
}

fn the_wall_rig(metadata: Arc<FakeMetadata>) -> Rig {
    let rig = Rig::with(custom(DISCS, "", ""), Some(metadata), DownloadServices::default());
    let peer = rig.at("peer");
    rig.harness.backend.land(move |_, _| {
        std::fs::create_dir_all(&peer).expect("made");
        let path = peer.join(format!("{}.mp3", uuid::Uuid::new_v4().simple()));
        std::fs::write(&path, mp3()).expect("written");
        path.to_string_lossy().into_owned()
    });
    rig
}

async fn fetch(rig: &Rig) -> String {
    rig.harness
        .service
        .download_song("test", "1", &CancellationToken::new())
        .await
        .expect("downloaded")
}

#[tokio::test]
async fn disc_one_first_with_unknown_count_lands_in_its_disc_folder() {
    let rig = the_wall_rig(the_wall(true));
    let path = fetch(&rig).await;
    assert_eq!(
        rig.relative(&path),
        "Pink Floyd/The Wall/Disc 1/01 - In the Flesh.mp3"
    );
}

#[tokio::test]
async fn the_disc_count_is_written_to_disctotal() {
    let rig = the_wall_rig(the_wall(true));
    let path = fetch(&rig).await;
    let written = TagFile::open(&path).expect("opens");
    assert_eq!((written.disc(), written.disc_count()), (1, 2));
}

/// A listing that never comes, or comes too late, leaves the count unknown: the song lands as a
/// one-disc album's would, and the download is not held up past the wait.
#[tokio::test]
async fn a_failed_catalog_lookup_leaves_the_count_unknown() {
    let rig = the_wall_rig(the_wall(false));
    let path = fetch(&rig).await;
    assert_eq!(rig.relative(&path), "Pink Floyd/The Wall/01 - In the Flesh.mp3");

    let slow = the_wall(true);
    *slow.album_wait.lock() = Some(Duration::from_secs(5));
    let rig = the_wall_rig(slow);
    rig.harness
        .service
        .set_disc_count_wait(Duration::from_millis(300));
    let started = Instant::now();
    let path = fetch(&rig).await;
    assert_eq!(rig.relative(&path), "Pink Floyd/The Wall/01 - In the Flesh.mp3");
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
}

// ---- a replacement's fallback ---------------------------------------------------------------

/// What a library action hands over for a replacement of `original`, judged by `before`.
fn handoff(
    original: &Path,
    before: crate::services::library::replacement_handoff::BeforeReveal,
    on_revealed: Option<Arc<dyn Fn(&str) + Send + Sync>>,
) -> ReplacementHandoff {
    ReplacementHandoff::new(
        original.to_string_lossy().into_owned(),
        octo_media::tags::KeptIdentityTags::read(original, None).expect("an identity"),
        before,
        on_revealed,
    )
}

/// The original's name is taken by another spelling of it, so the replacement is filed by the
/// layout, into the album's folder by its real name.
#[tokio::test]
async fn replacement_fallback_joins_equivalent_folder() {
    let rig = Rig::new(preset(FolderStructure::Organized));
    let original = rig.has("Odd Folder/03 teardrop old.mp3", &Tags::default());
    std::fs::write(rig.at("Odd Folder/03 Teardrop Old.flac"), b"another file").expect("written");
    rig.has("massive attack/mezzanine/01 - Angel.mp3", &Tags::default());
    let upload = rig.at("peer/upload.flac");
    std::fs::create_dir_all(rig.at("peer")).expect("made");
    std::fs::write(&upload, crate::services::test_support::flac()).expect("written");
    let staged = rig
        .harness
        .service
        .stage_replacement(&upload.to_string_lossy())
        .expect("staged")
        .path;
    let song = track("Massive Attack", "Mezzanine", "Teardrop", 3);

    let placed = rig
        .harness
        .service
        .reveal_replacement(
            &song,
            &asked(&song),
            &staged,
            &handoff(&original, Arc::new(|_| async { None }.boxed()), None),
        )
        .await
        .expect("revealed");

    assert_eq!(
        rig.relative(&placed.path),
        "massive attack/mezzanine/03 - Teardrop.flac"
    );
}

/// A replacement moving in puts right a replace a stop left half done before it moves anything,
/// as every route that places does.
#[tokio::test]
async fn the_replacement_route_puts_a_stopped_replace_right_first() {
    let rig = Rig::new(preset(FolderStructure::Organized));
    let stopped = StoppedReplace::under(rig.dir.path(), "fff", None);
    let original = rig.has("Odd Folder/03 teardrop old.mp3", &Tags::default());
    std::fs::write(rig.at("Odd Folder/03 Teardrop Old.flac"), b"another file").expect("written");
    let upload = rig.at("peer/upload.flac");
    std::fs::create_dir_all(rig.at("peer")).expect("made");
    std::fs::write(&upload, crate::services::test_support::flac()).expect("written");
    let staged = rig
        .harness
        .service
        .stage_replacement(&upload.to_string_lossy())
        .expect("staged")
        .path;
    let song = track("Massive Attack", "Mezzanine", "Teardrop", 3);

    let placed = rig
        .harness
        .service
        .reveal_replacement(
            &song,
            &asked(&song),
            &staged,
            &handoff(&original, Arc::new(|_| async { None }.boxed()), None),
        )
        .await
        .expect("revealed");

    assert_eq!(
        rig.relative(&placed.path),
        "Massive Attack/Mezzanine/03 - Teardrop.flac"
    );
    stopped.assert_put_back();
}

/// The replacement's fallback files disc 2 of an album whose disc 1 Octo placed loose: it lands
/// loose beside it, so the album stays one folder.
#[tokio::test]
async fn the_replacement_fallback_lands_a_new_disc_loose_beside_its_album() {
    let rig = Rig::new(custom(DISCS, "", ""));
    let wall = Tags {
        artist: "Pink Floyd",
        album: "The Wall",
        disc: 1,
        ..Tags::default()
    };
    let loose = rig.has("Pink Floyd/The Wall/01 - In the Flesh.mp3", &wall);
    rig.octos(&loose, "Pink Floyd", "In the Flesh");
    let original = rig.has("Odd Folder/01 hey you old.mp3", &Tags::default());
    std::fs::write(rig.at("Odd Folder/01 Hey You Old.flac"), b"another file").expect("written");
    let upload = rig.at("peer/upload.flac");
    std::fs::create_dir_all(rig.at("peer")).expect("made");
    std::fs::write(&upload, crate::services::test_support::flac()).expect("written");
    let staged = rig
        .harness
        .service
        .stage_replacement(&upload.to_string_lossy())
        .expect("staged")
        .path;
    let song = Song {
        disc_number: Some(2),
        total_discs: Some(2),
        ..track("Pink Floyd", "The Wall", "Hey You", 1)
    };

    let placed = rig
        .harness
        .service
        .reveal_replacement(
            &song,
            &asked(&song),
            &staged,
            &handoff(&original, Arc::new(|_| async { None }.boxed()), None),
        )
        .await
        .expect("revealed");

    assert_eq!(
        rig.relative(&placed.path),
        "Pink Floyd/The Wall/01 - Hey You.flac"
    );
    assert!(loose.is_file(), "placement never moves another song");
}

/// The replacement's checks run before it takes the gate's share; from then until it is in its
/// place, no library job can start.
#[tokio::test]
async fn reveal_holds_the_gate_share() {
    let gate = LibraryWriteGate::new();
    let rig = Rig::with(
        preset(FolderStructure::Organized),
        None,
        DownloadServices {
            write_gate: Some(gate.clone()),
            ..Default::default()
        },
    );
    let original = rig.has("Odd Folder/03 teardrop old.mp3", &Tags::default());
    let staged = rig
        .harness
        .service
        .stage_replacement(&rig.landed())
        .expect("staged")
        .path;
    let free_while_judged: Arc<Mutex<Option<bool>>> = Arc::default();
    let free_when_revealed: Arc<Mutex<Option<bool>>> = Arc::default();
    let before = {
        let (gate, seen, gone) = (gate.clone(), free_while_judged.clone(), original.clone());
        Arc::new(move |_: String| {
            let (gate, seen, gone) = (gate.clone(), seen.clone(), gone.clone());
            async move {
                let job = tokio::time::timeout(Duration::from_secs(2), gate.begin_job("A test's job")).await;
                *seen.lock() = Some(matches!(job, Ok(Ok(_))));
                drop(job);
                let _ = std::fs::remove_file(&gone);
                None
            }
            .boxed()
        })
    };
    let on_revealed = {
        let (gate, seen) = (gate.clone(), free_when_revealed.clone());
        Arc::new(move |_: &str| {
            let free = gate
                .begin_job("A test's job")
                .now_or_never()
                .is_some_and(|job| job.is_ok());
            *seen.lock() = Some(free);
        }) as Arc<dyn Fn(&str) + Send + Sync>
    };
    let song = track("Massive Attack", "Mezzanine", "Teardrop", 3);

    rig.harness
        .service
        .reveal_replacement(
            &song,
            &asked(&song),
            &staged,
            &handoff(&original, before, Some(on_revealed)),
        )
        .await
        .expect("revealed");

    assert_eq!(
        *free_while_judged.lock(),
        Some(true),
        "no share while it is judged"
    );
    assert_eq!(
        *free_when_revealed.lock(),
        Some(false),
        "the share while it moves in"
    );
}
