//! The playlists lock: a download adding its song to a playlist and the library tidy naming its
//! moved songs in the playlists take the same lock, so neither writes over the other's change to
//! a playlist file.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use octo_core::models::domain::Song;
use octo_core::models::subsonic::ScanStatus;
use octo_core::settings::{AppSettings, FolderStructure, SubsonicSettings};
use octo_media::tags::TagFile;
use parking_lot::Mutex;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use super::test_support::{build, catalog};
use super::{DownloadServices, PlaylistTrackSync};
use crate::services::i_download_service::IDownloadService;
use crate::services::i_music_metadata_service::IMusicMetadataService;
use crate::services::library::library_write_gate::LibraryWriteGate;
use crate::services::library::tidy::{
    TidyJournal, TidyMode, TidyRequest, TidyScope, TidyServices, TidyStatus, TidyStore, TidyTask, TidyWorker,
};
use crate::services::local::scan_wait::ScanWaitTiming;
use crate::services::local::{ILocalLibraryService, LocalSongMapping, ParsedExternalId, ParsedSongId};
use crate::services::subsonic::navidrome_version::ServerInfo;
use crate::services::test_support::mp3;

/// Octo's records and Navidrome as both the download and the tidy see them: a download's
/// registration is a mapping the tidy reads, Navidrome is new enough, idle, last scanned five
/// minutes ago, and finishes a scan as soon as it is asked for one. Paths are kept with `/`, as
/// the tidy's walk joins them.
struct Library {
    mappings: Mutex<Vec<LocalSongMapping>>,
    /// The last scan's start and finish.
    scans: Mutex<(DateTime<Utc>, DateTime<Utc>)>,
}

impl Library {
    fn new() -> Self {
        let long_ago = Utc::now() - TimeDelta::minutes(5);
        Library {
            mappings: Mutex::new(Vec::new()),
            scans: Mutex::new((long_ago, long_ago + TimeDelta::seconds(1))),
        }
    }

    fn own(&self, path: &str) {
        self.mappings.lock().push(LocalSongMapping {
            external_provider: "test".into(),
            external_id: path.to_string(),
            local_path: slashed(path),
            ..LocalSongMapping::default()
        });
    }
}

#[async_trait]
impl ILocalLibraryService for Library {
    async fn get_local_path_for_external_song(&self, _: &str, _: &str) -> Option<String> {
        None
    }
    async fn register_downloaded_song(&self, _: &Song, local_path: &str) -> anyhow::Result<()> {
        self.own(local_path);
        Ok(())
    }
    async fn get_local_id_for_external_song(&self, _: &str, _: &str) -> Option<String> {
        None
    }
    fn parse_song_id(&self, _: &str) -> ParsedSongId {
        (false, None, None)
    }
    fn parse_external_id(&self, _: &str) -> ParsedExternalId {
        (false, None, None, None)
    }
    async fn get_mappings(&self) -> Vec<LocalSongMapping> {
        self.mappings.lock().clone()
    }
    async fn find_mapping_by_tags(
        &self,
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Option<LocalSongMapping> {
        None
    }
    async fn forget_mapping(&self, _: &str) -> anyhow::Result<Vec<LocalSongMapping>> {
        Ok(Vec::new())
    }
    async fn trigger_library_scan(&self, _: bool) -> bool {
        let now = Utc::now();
        *self.scans.lock() = (now, now + TimeDelta::milliseconds(1));
        true
    }
    async fn get_scan_status(&self) -> Option<ScanStatus> {
        let (began, finished) = *self.scans.lock();
        Some(ScanStatus {
            scanning: false,
            count: Some(1),
            last_scan: Some(finished.to_rfc3339()),
            elapsed_time: (finished - began).num_nanoseconds(),
            folder_count: None,
        })
    }
    async fn get_server_info(&self) -> Option<ServerInfo> {
        Some(ServerInfo::new(Some("navidrome"), Some("0.64.2 (abc)")))
    }
    async fn update_local_path(&self, old_path: &str, new_path: &str) -> anyhow::Result<usize> {
        let mut moved = 0;
        for mapping in self.mappings.lock().iter_mut() {
            if mapping.local_path == slashed(old_path) {
                mapping.local_path = slashed(new_path);
                moved += 1;
            }
        }
        Ok(moved)
    }
}

fn slashed(path: &str) -> String {
    path.replace('\\', "/")
}

/// An MP3 at `path` of one album track, last written ten minutes ago.
fn track(path: &str, artist: &str, album: &str, title: &str, number: u32) {
    std::fs::create_dir_all(Path::new(path).parent().expect("a folder")).expect("folders");
    std::fs::write(path, mp3()).expect("written");
    let mut file = TagFile::open(path).expect("an audio file");
    file.set_title(Some(title));
    file.set_album(Some(album));
    file.set_performers(&[artist.to_string()]);
    file.set_album_artists(&[artist.to_string()]);
    file.set_track(number);
    file.save().expect("saved");
    std::fs::File::options()
        .write(true)
        .open(path)
        .and_then(|file| file.set_modified(SystemTime::now() - Duration::from_secs(600)))
        .expect("aged");
}

/// A playlist the downloads add their song to, as a playlist sync does: the file is read, then,
/// once the test lets it go on, the song's line is added and the file written whole.
struct Mix {
    path: String,
    read: Notify,
    go: Notify,
}

#[async_trait]
impl PlaylistTrackSync for Mix {
    fn get_playlist_id_for_track(&self, _: &str) -> Option<String> {
        Some("mix".to_string())
    }

    async fn add_track_to_m3u(&self, _: &str, _: &Song, local_path: &str, _: bool) -> anyhow::Result<()> {
        let mut text = std::fs::read_to_string(&self.path)?;
        self.read.notify_one();
        self.go.notified().await;
        text.push_str(&slashed(local_path));
        text.push('\n');
        std::fs::write(&self.path, text)?;
        Ok(())
    }
}

/// A download has read a playlist to add its song and not yet written it back when the person's
/// File layout apply moves a song that playlist names: the apply's playlist follow waits for the
/// download's write, then names the moved song where it is now, so the playlist ends with both
/// changes.
#[tokio::test]
async fn a_tidy_follow_waits_for_a_download_writing_to_a_playlist() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let root = slashed(&dir.path().to_string_lossy());
    let at = |relative: &str| format!("{root}/{relative}");
    let library = Arc::new(Library::new());
    // A song whose number the File layout writes with two digits.
    let song = at("Band/Record/1 - Song.mp3");
    track(&song, "Band", "Record", "Song", 1);
    library.own(&song);
    let moved = at("Band/Record/01 - Song.mp3");
    let playlist = at("Mixes/mix.m3u");
    std::fs::create_dir_all(at("Mixes")).expect("made");
    std::fs::write(&playlist, "../Band/Record/1 - Song.mp3\n").expect("written");
    let mix = Arc::new(Mix {
        path: playlist.clone(),
        read: Notify::new(),
        go: Notify::new(),
    });
    let four = Song {
        external_provider: Some("test".into()),
        external_id: Some("4".into()),
        artist: "Artist".into(),
        album: "Album".into(),
        title: "Four".into(),
        track: Some(4),
        disc_number: Some(1),
        total_discs: Some(1),
        ..Default::default()
    };
    let gate = LibraryWriteGate::new();
    let harness = build(
        Path::new(&root),
        AppSettings {
            subsonic: SubsonicSettings {
                folder_structure: FolderStructure::Custom,
                folder_template: "{Artist}/{Album}/{Track:00} - {Title}".into(),
                ..Default::default()
            },
            ..Default::default()
        },
        Some(library.clone() as Arc<dyn ILocalLibraryService>),
        Some(catalog(vec![("4", four)]) as Arc<dyn IMusicMetadataService>),
        DownloadServices {
            write_gate: Some(gate.clone()),
            playlist_sync: Some(mix.clone() as Arc<dyn PlaylistTrackSync>),
            ..DownloadServices::default()
        },
    );
    {
        let peer = dir.path().join("peer");
        harness.backend.land(move |_, id| {
            std::fs::create_dir_all(&peer).expect("made");
            let path = peer.join(format!("{id}.mp3"));
            std::fs::write(&path, mp3()).expect("written");
            path.to_string_lossy().into_owned()
        });
    }
    let worker = TidyWorker::new(
        Arc::new(TidyStore::new(None, None)),
        Arc::new(TidyJournal::new(None)),
        harness.service.settings().clone(),
        TidyServices {
            library: Some(library.clone()),
            ..TidyServices::default()
        },
    )
    .with_write_gate(gate)
    .with_timing(ScanWaitTiming {
        poll: Duration::from_millis(1),
        timeout: Duration::from_secs(3),
    });
    let following = Arc::new(AtomicBool::new(false));
    {
        let following = following.clone();
        worker.in_playlist_walk(move || following.store(true, Ordering::SeqCst));
    }
    let token = CancellationToken::new();
    let layout = |mode| TidyRequest {
        mode,
        scope: TidyScope::OctoDownloads,
        task: TidyTask::FileLayout,
        ..TidyRequest::default()
    };
    worker.run_once(layout(TidyMode::Preview), &token).await;
    assert!(
        worker.current().is_finished_preview(),
        "{:?}",
        worker.current().reason
    );

    let downloading = tokio::time::timeout(
        Duration::from_secs(10),
        harness.service.download_song("test", "4", &token),
    );
    let tidying = async {
        tokio::time::timeout(Duration::from_secs(5), mix.read.notified())
            .await
            .expect("the download read the playlist");
        let watching = async {
            tokio::time::timeout(Duration::from_secs(5), async {
                while !Path::new(&moved).exists() {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .expect("the apply moved the song");
            tokio::time::sleep(Duration::from_millis(500)).await;
            let followed_meanwhile = following.load(Ordering::SeqCst);
            mix.go.notify_one();
            followed_meanwhile
        };
        let ((), followed_meanwhile) =
            tokio::join!(worker.run_once(layout(TidyMode::Apply), &token), watching);
        followed_meanwhile
    };
    let (downloaded, followed_meanwhile) = tokio::join!(downloading, tidying);

    assert!(
        !followed_meanwhile,
        "the tidy's playlist follow waited for the download's playlist write"
    );
    let run = worker.current();
    assert_eq!(run.status, TidyStatus::Completed, "{:?}", run.reason);
    assert_eq!(run.moved_files, 1);
    let placed = slashed(&downloaded.expect("finished").expect("downloaded"));
    let text = std::fs::read_to_string(&playlist).expect("read");
    assert!(text.contains(&placed), "the new song is in the playlist: {text}");
    #[cfg(unix)]
    assert!(
        text.starts_with("../Band/Record/01 - Song.mp3\n"),
        "the moved song is named where it is now: {text}"
    );
}
