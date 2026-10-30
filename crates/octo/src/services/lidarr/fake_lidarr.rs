//! A fake Lidarr: a model of the parts of Lidarr's v1 API Octo uses, served by a mock server and
//! answering from one state the way Lidarr does, so a test fails wherever Octo would hurt a
//! real Lidarr's files.
//!
//! - Paths: Lidarr's root folder [`LIDARR_ROOT`] lies at the octo root `start` is given; its
//!   downloads ([`LIDARR_DOWNLOADS`]) and recycle bin ([`LIDARR_RECYCLE_BIN`]) lie in
//!   `.fake-lidarr` under it. Lidarr's scans skip dot folders, and so do the fake's.
//! - Commands run as Lidarr runs them: queued, started, then ended, with one disk command
//!   (RescanFolders, ProcessMonitoredDownloads, ManualImport, DownloadedAlbumsScan) running at
//!   a time. Each runs to its end within the request that queued it, unless
//!   [`FakeLidarr::set_command_mode`] moves them one step per command read, or only when the
//!   test steps them.
//! - Adding an album refreshes it, or its new artist (the tracks appear), then rescans the
//!   artist's folder, which links the audio files lying there with no history row (rewriting
//!   their tags when Lidarr writes tags) and drops the records of files no longer there.
//!   Changing an album's monitored releases, or letting it take any release, unlinks its files
//!   and rescans that folder too.
//! - A grab is a tracked download: its `grabbed` row at once, its queue item from the next
//!   refresh of the downloads. The queue is that refresh's snapshot: it never says an import is
//!   running, and after a restart it is empty until the first refresh. Each refresh queues a
//!   ProcessMonitoredDownloads that imports every finished download (unless Completed Download
//!   Handling is off): the import replaces every file the album had, inserts the new track
//!   files, and only then writes one `trackFileImported` row per file and the download's own row.
//! - Deleting a track file deletes only a file at its recorded path (into the recycle bin when
//!   one is set), and answers 409 while the artist's root folder is missing or empty. Deleting
//!   an album or an artist with `deleteFiles=false` leaves the disk alone.
//! - [`FakeLidarr::on_search`] keeps a quick Lidarr: a search's files appear at once, with no
//!   queue item and no history row, and nothing is replaced.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, TimeDelta, Utc};
use octo_core::common::dotnet;
use octo_core::json::datetime;
use octo_core::lidarr::lidarr_client::LidarrHistoryEvent;
use parking_lot::Mutex;
use serde_json::{Map, Value, json};
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// The fake album's foreign id.
pub(crate) const ALBUM_ID: &str = "rg-mezzanine";
/// The fake artist's foreign id.
pub(crate) const ARTIST_ID: &str = "ma";
/// Lidarr's root folder, which lies at the octo root.
pub(crate) const LIDARR_ROOT: &str = "/data/music";
/// Where the download clients put their downloads, in Lidarr's paths.
pub(crate) const LIDARR_DOWNLOADS: &str = "/data/downloads";
/// Lidarr's recycle bin, once [`FakeLidarr::set_recycle_bin`] sets one.
pub(crate) const LIDARR_RECYCLE_BIN: &str = "/data/recycle";
/// What Lidarr's tag writing leaves at the end of a file, so a test can see it.
pub(crate) const RETAGGED: &[u8] = b"\n(tags written by Lidarr)";

/// The fake's own folder under the octo root: the downloads and the recycle bin.
const FAKE_FOLDER: &str = ".fake-lidarr";
/// The commands that take Lidarr's disk lock: one of them runs at a time.
const DISK_COMMANDS: [&str; 4] = [
    "RescanFolders",
    "ProcessMonitoredDownloads",
    "ManualImport",
    "DownloadedAlbumsScan",
];
/// The audio files Lidarr's scans pick up.
const AUDIO: [&str; 8] = ["flac", "mp3", "m4a", "opus", "ogg", "wav", "aac", "wv"];

/// How the fake runs its commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandMode {
    /// Each runs to its end within the request, or the test's call, that queued it.
    Instant,
    /// Each read of the command list, or of one command, moves every command one step.
    PerPoll,
    /// Only [`FakeLidarr::step`] and [`FakeLidarr::run_commands`] move them.
    Held,
}

/// An album Lidarr's metadata knows: what a lookup finds and what adding it brings.
#[derive(Debug, Clone)]
pub(crate) struct CatalogAlbum {
    pub foreign_id: String,
    pub title: String,
    /// The artist's foreign id.
    pub artist: String,
    /// The id Lidarr gives the album once it holds it.
    pub library_id: i32,
    pub release_date: String,
    /// (id, foreign id) of each release. Lidarr monitors the first when it adds the album.
    pub releases: Vec<(i32, String)>,
    /// (id, title, number, length in milliseconds, recording id) of each track.
    pub tracks: Vec<(i32, String, i32, i32, String)>,
    /// False: no refresh ever brings the album's tracks.
    pub tracks_appear: bool,
    /// False: refreshing the artist does not add the album (its metadata profile leaves it out).
    pub in_profile: bool,
}

impl CatalogAlbum {
    /// An album of `artist`'s with one release and these tracks (id, title, number), each
    /// 330 seconds long, recorded as `rec-<track id>`.
    pub fn new(
        foreign_id: &str,
        title: &str,
        artist: &str,
        library_id: i32,
        tracks: &[(i32, &str, i32)],
    ) -> CatalogAlbum {
        CatalogAlbum {
            foreign_id: foreign_id.to_string(),
            title: title.to_string(),
            artist: artist.to_string(),
            library_id,
            release_date: "2000-01-01".to_string(),
            releases: vec![(library_id * 10, format!("{foreign_id}-release"))],
            tracks: tracks
                .iter()
                .map(|(id, title, number)| (*id, title.to_string(), *number, 330_000, format!("rec-{id}")))
                .collect(),
            tracks_appear: true,
            in_profile: true,
        }
    }
}

/// One file of a download: the album and track Lidarr matches it to, its title and quality.
#[derive(Debug, Clone)]
pub(crate) struct GrabFile {
    pub album_id: i32,
    pub track_id: i32,
    pub title: String,
    pub quality: String,
    /// Why a manual import would refuse the file.
    pub rejection: Option<String>,
}

/// What a grab brings in: one tracked download.
#[derive(Debug, Clone)]
pub(crate) struct Grab {
    pub download_id: String,
    /// The client's type, which import rows carry as `downloadClient`.
    pub client: String,
    /// The name the client has in Lidarr, which queue items carry as `downloadClient`.
    pub client_name: String,
    pub protocol: String,
    pub title: String,
    pub guid: String,
    pub size: i64,
    pub files: Vec<GrabFile>,
    /// The release the files match; importing them makes it the album's one monitored release.
    pub release_id: Option<i32>,
    /// The client keeps its copy after Lidarr's import (a seeding torrent), so the dropped file
    /// stays where the download put it.
    pub keeps_files: bool,
}

impl Grab {
    /// A qBittorrent download of tracks of the fake album: (track id, title, quality).
    pub fn of(download_id: &str, tracks: &[(i32, &str, &str)]) -> Grab {
        Grab {
            download_id: download_id.to_string(),
            client: "QBittorrent".to_string(),
            client_name: "qBittorrent".to_string(),
            protocol: "torrent".to_string(),
            title: format!("Massive Attack - Mezzanine ({download_id})"),
            guid: format!("guid-{download_id}"),
            size: 300_000_000,
            files: tracks
                .iter()
                .map(|(track_id, title, quality)| GrabFile {
                    album_id: 7,
                    track_id: *track_id,
                    title: title.to_string(),
                    quality: quality.to_string(),
                    rejection: None,
                })
                .collect(),
            release_id: None,
            keeps_files: true,
        }
    }

    /// The same download from another client: its type, its name in Lidarr, its protocol, and
    /// whether it keeps its copy after the import.
    pub fn by_client(mut self, client: &str, client_name: &str, protocol: &str, keeps_files: bool) -> Grab {
        self.client = client.to_string();
        self.client_name = client_name.to_string();
        self.protocol = protocol.to_string();
        self.keeps_files = keeps_files;
        self
    }

    /// Every file matched to another album, as Lidarr's import picks the album from the files.
    pub fn into_album(mut self, album_id: i32) -> Grab {
        for file in &mut self.files {
            file.album_id = album_id;
        }
        self
    }
}

/// One request the fake answered.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Recorded {
    pub method: String,
    /// The path and the query, as sent.
    pub path: String,
    pub body: Option<Value>,
}

#[derive(Clone)]
pub(crate) struct FakeLidarr {
    state: Arc<Mutex<State>>,
}

struct Artist {
    id: i32,
    foreign_id: String,
    name: String,
    path: String,
    monitored: bool,
    monitor_new_items: String,
    added: DateTime<Utc>,
    quality_profile_id: i32,
    metadata_profile_id: i32,
    /// The add's `monitor` choice and the albums it names, applied after the artist's first
    /// refresh.
    first_refresh: Option<(String, Vec<String>)>,
}

#[derive(Clone)]
struct Release {
    id: i32,
    foreign_id: String,
    title: String,
    monitored: bool,
}

struct Album {
    id: i32,
    foreign_id: String,
    artist_id: i32,
    title: String,
    release_date: String,
    monitored: bool,
    any_release_ok: bool,
    releases: Vec<Release>,
}

struct Track {
    id: i32,
    album_id: i32,
    title: String,
    number: i32,
    duration_ms: i32,
    recording: String,
    /// 0: no file.
    file_id: i32,
}

struct TrackFile {
    id: i32,
    /// 0: unlinked, waiting for a rescan to match it again.
    album_id: i32,
    track_id: i32,
    /// In Lidarr's paths.
    path: String,
    quality: String,
    size: i64,
    added: DateTime<Utc>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fetch {
    Downloading,
    Completed,
    Failed,
    Imported,
}

#[derive(Clone)]
struct Download {
    queue_id: i32,
    grab: Grab,
    album_id: i32,
    artist_id: i32,
    release_source: String,
    state: Fetch,
    warning: bool,
    size_left: i64,
    /// Lidarr no longer tracks it: removed in the client, or through the queue.
    removed: bool,
    /// `importBlocked` or `importFailed`, set by a test.
    blocked: Option<String>,
    /// The files imported so far, by their path in the download.
    imported: BTreeSet<String>,
}

struct PendingRelease {
    queue_id: i32,
    album_id: i32,
    artist_id: i32,
    title: String,
    reason: String,
}

#[derive(Clone)]
struct Row {
    id: i32,
    event: LidarrHistoryEvent,
    date: DateTime<Utc>,
    album_id: i32,
    artist_id: i32,
    track_id: i32,
    download_id: Option<String>,
    source_title: String,
    data: Map<String, Value>,
}

struct Command {
    id: i32,
    name: String,
    body: Map<String, Value>,
    trigger: String,
    high_priority: bool,
    status: String,
    queued: DateTime<Utc>,
    started: Option<DateTime<Utc>>,
    ended: Option<DateTime<Utc>>,
    /// Listed by `GET /api/v1/command`: false after a restart.
    listed: bool,
    /// Rows the command still has to write, one per step.
    rows: Vec<Row>,
    /// A folder import of the user's: the files, and the download id Lidarr guessed for them.
    folder_import: Option<(Vec<GrabFile>, Option<String>)>,
}

struct State {
    /// Lidarr path prefix, and where it lies on Octo's side.
    mounts: Vec<(String, PathBuf)>,
    roots: Vec<String>,
    /// Foreign id, name and library id of each artist the metadata knows.
    catalog_artists: Vec<(String, String, i32)>,
    catalog_albums: Vec<CatalogAlbum>,
    /// Foreign album ids every lookup misses.
    unknown: BTreeSet<String>,
    artists: BTreeMap<i32, Artist>,
    albums: BTreeMap<i32, Album>,
    tracks: BTreeMap<i32, Track>,
    files: BTreeMap<i32, TrackFile>,
    next_file: i32,
    /// `statistics.trackFileCount` an artist reports instead of its real count.
    stale_statistics: BTreeMap<i32, i32>,
    downloads: Vec<Download>,
    /// What the next search of an album grabs.
    plans: Vec<(i32, Grab)>,
    pending: Vec<PendingRelease>,
    /// The queue as the last refresh of the downloads left it; None after a restart.
    snapshot: Option<Vec<Value>>,
    /// Clients whose next refresh fails, dropping all their items from it.
    failing_clients: BTreeSet<String>,
    next_queue: i32,
    history: Vec<Row>,
    next_row: i32,
    commands: BTreeMap<i32, Command>,
    next_command: i32,
    mode: CommandMode,
    rows_one_per_step: bool,
    completed_download_handling: bool,
    write_audio_tags: String,
    recycle_bin: bool,
    health: Value,
    start_time: DateTime<Utc>,
    clock: DateTime<Utc>,
    on_search: Vec<(i32, String, String)>,
    hold_imports: bool,
    searches: usize,
    deleted: Vec<i32>,
    monitor_changes: Vec<bool>,
    /// The releases an interactive search lists now: (guid, indexer id, title, size). A grab by
    /// any other guid is refused the way Lidarr refuses a release it has forgotten.
    offered: Vec<(String, i32, String, i64)>,
    grabs: Vec<String>,
    requests: Vec<Recorded>,
}

impl FakeLidarr {
    /// The fake, and the server answering for it. Lidarr's metadata knows the fake artist
    /// (library id 5) and its album (library id 7, release 70) with Angel (track 1) and Teardrop
    /// (track 3); Lidarr holds neither yet.
    pub async fn start(octo_root: impl Into<PathBuf>) -> (FakeLidarr, MockServer) {
        let octo_root: PathBuf = octo_root.into();
        let fake_folder = octo_root.join(FAKE_FOLDER);
        let now = Utc::now();
        let mut mezzanine = CatalogAlbum::new(
            ALBUM_ID,
            "Mezzanine",
            ARTIST_ID,
            7,
            &[(1, "Angel", 1), (3, "Teardrop", 3)],
        );
        mezzanine.release_date = "1998-04-20".to_string();
        mezzanine.releases = vec![(70, "mezzanine-uk".to_string())];
        mezzanine.tracks[0].3 = 380_000;
        let fake = FakeLidarr {
            state: Arc::new(Mutex::new(State {
                mounts: vec![
                    (LIDARR_ROOT.to_string(), octo_root.clone()),
                    (LIDARR_DOWNLOADS.to_string(), fake_folder.join("downloads")),
                    (LIDARR_RECYCLE_BIN.to_string(), fake_folder.join("recycle")),
                ],
                roots: vec![LIDARR_ROOT.to_string()],
                catalog_artists: vec![(ARTIST_ID.to_string(), "Massive Attack".to_string(), 5)],
                catalog_albums: vec![mezzanine],
                unknown: BTreeSet::new(),
                artists: BTreeMap::new(),
                albums: BTreeMap::new(),
                tracks: BTreeMap::new(),
                files: BTreeMap::new(),
                next_file: 100,
                stale_statistics: BTreeMap::new(),
                downloads: Vec::new(),
                plans: Vec::new(),
                pending: Vec::new(),
                snapshot: Some(Vec::new()),
                failing_clients: BTreeSet::new(),
                next_queue: 1,
                history: Vec::new(),
                next_row: 1,
                commands: BTreeMap::new(),
                next_command: 1,
                mode: CommandMode::Instant,
                rows_one_per_step: false,
                completed_download_handling: true,
                write_audio_tags: "no".to_string(),
                recycle_bin: false,
                health: json!([]),
                start_time: now - TimeDelta::hours(1),
                clock: now,
                on_search: Vec::new(),
                hold_imports: false,
                searches: 0,
                deleted: Vec::new(),
                monitor_changes: Vec::new(),
                offered: Vec::new(),
                grabs: Vec::new(),
                requests: Vec::new(),
            })),
        };
        let server = MockServer::builder().start().await;
        Mock::given(any()).respond_with(fake.clone()).mount(&server).await;
        (fake, server)
    }

    /// Runs `change` on the state, then whatever it queued when commands run at once.
    fn with<T>(&self, change: impl FnOnce(&mut State) -> T) -> T {
        let mut state = self.state.lock();
        let answer = change(&mut state);
        if state.mode == CommandMode::Instant {
            state.drain();
        }
        answer
    }

    // ---- The metadata and what Lidarr holds --------------------------------------------------

    /// When false, every lookup misses the fake album.
    pub fn set_knows_album(&self, knows: bool) {
        self.with(|state| {
            if knows {
                state.unknown.remove(ALBUM_ID);
            } else {
                state.unknown.insert(ALBUM_ID.to_string());
            }
        });
    }

    /// Another artist Lidarr's metadata knows, by foreign id, name and the id Lidarr gives it.
    pub fn catalog_artist(&self, foreign_id: &str, name: &str, library_id: i32) {
        self.with(|state| {
            state
                .catalog_artists
                .push((foreign_id.to_string(), name.to_string(), library_id));
        });
    }

    /// Another album Lidarr's metadata knows.
    pub fn catalog_album(&self, album: CatalogAlbum) {
        self.with(|state| state.catalog_albums.push(album));
    }

    /// The user's Lidarr holds the fake album (and its artist), monitored or not, with its
    /// tracks. The name the shipped tests use.
    pub fn add_album(&self, monitored: bool) {
        self.user_album(ALBUM_ID, monitored);
    }

    /// The user's Lidarr holds an artist the metadata knows, added an hour before the fake
    /// started.
    pub fn user_artist(&self, foreign_artist_id: &str, monitored: bool) {
        self.with(|state| {
            state.ensure_artist(foreign_artist_id, monitored, "all", LIDARR_ROOT, (1, 1), None);
            if let Some(artist) = state
                .artists
                .values_mut()
                .find(|a| dotnet::eq_ignore_case(&a.foreign_id, foreign_artist_id))
            {
                artist.added = state.start_time;
            }
        });
    }

    /// The user's Lidarr holds an album the metadata knows (and its artist, monitored), with
    /// its tracks.
    pub fn user_album(&self, foreign_album_id: &str, monitored: bool) {
        self.with(|state| {
            let artist = state
                .catalog_album(foreign_album_id)
                .map(|album| album.artist.clone())
                .expect("an album the metadata knows");
            let artist_id = state.ensure_artist(&artist, true, "all", LIDARR_ROOT, (1, 1), None);
            if let Some(album_id) = state.insert_album(foreign_album_id, artist_id, monitored) {
                state.add_tracks(album_id);
            }
        });
    }

    /// The artist's `statistics.trackFileCount` says `count` whatever its files are, as a stale
    /// statistic does.
    pub fn set_stale_statistics(&self, artist_id: i32, count: i32) {
        self.with(|state| {
            state.stale_statistics.insert(artist_id, count);
        });
    }

    /// Another root folder: its Lidarr path and where it lies on Octo's side (or anywhere a
    /// test chooses, for a folder Octo cannot see).
    pub fn add_root_folder(&self, lidarr_path: &str, local: impl Into<PathBuf>) {
        self.with(|state| {
            state.roots.push(lidarr_path.to_string());
            state.mounts.push((lidarr_path.to_string(), local.into()));
        });
    }

    /// Where a Lidarr path lies on Octo's side.
    pub fn local(&self, lidarr_path: &str) -> PathBuf {
        self.state
            .lock()
            .local(lidarr_path)
            .expect("a path under a folder the fake maps")
    }

    /// A file lying on disk at a Lidarr path, which a rescan of its folder links. Answers where
    /// it lies on Octo's side.
    pub fn place_file(&self, lidarr_path: &str, contents: &[u8]) -> PathBuf {
        let local = self.local(lidarr_path);
        write_file(&local, contents);
        local
    }

    // ---- Settings ----------------------------------------------------------------------------

    pub fn set_completed_download_handling(&self, on: bool) {
        self.with(|state| state.completed_download_handling = on);
    }

    /// "Write Metadata to Audio Files": `no`, `newFiles`, `allFiles` or `sync`.
    pub fn set_write_audio_tags(&self, value: &str) {
        self.with(|state| state.write_audio_tags = value.to_string());
    }

    /// Sets or clears the recycle bin at [`LIDARR_RECYCLE_BIN`].
    pub fn set_recycle_bin(&self, on: bool) {
        self.with(|state| state.recycle_bin = on);
    }

    /// What `GET /api/v1/health` answers: an array of checks.
    pub fn set_health(&self, checks: Value) {
        self.with(|state| state.health = checks);
    }

    /// Imports write their rows one per command step instead of all at once.
    pub fn set_rows_one_per_step(&self, on: bool) {
        self.with(|state| state.rows_one_per_step = on);
    }

    // ---- Commands ----------------------------------------------------------------------------

    pub fn set_command_mode(&self, mode: CommandMode) {
        self.with(|state| state.mode = mode);
    }

    /// Moves every command one step: the running ones write a row or end, then the queued ones
    /// start, one disk command at a time.
    pub fn step(&self) {
        self.state.lock().step();
    }

    /// Runs every queued and running command, and what they queue, to its end.
    pub fn run_commands(&self) {
        self.state.lock().drain();
    }

    /// A command Lidarr queues itself or the user starts (`trigger` `scheduled`, `unspecified`
    /// or `manual`), such as Lidarr's daily RescanFolders with no folders. Answers its id.
    pub fn queue_command(&self, name: &str, body: Value, trigger: &str) -> i32 {
        self.with(|state| {
            let body = body.as_object().cloned().unwrap_or_default();
            state.queue(name, body, trigger, false)
        })
    }

    /// Lidarr's clean up forgets an ended command: reading it by id answers 404.
    pub fn forget_command(&self, id: i32) {
        self.with(|state| {
            state.commands.remove(&id);
        });
    }

    /// Lidarr restarts: a new start time, the command list empty (running commands orphaned),
    /// and the queue empty until the first refresh of the downloads. The clients keep their
    /// downloads.
    pub fn restart(&self) {
        self.with(|state| {
            let now = state.tick();
            state.start_time = now;
            state.snapshot = None;
            for command in state.commands.values_mut() {
                command.listed = false;
                if matches!(command.status.as_str(), "queued" | "started") {
                    command.status = "orphaned".to_string();
                    command.ended = Some(now);
                    command.rows.clear();
                }
            }
        });
    }

    // ---- Downloads ---------------------------------------------------------------------------

    /// What the next search of the album (an AlbumSearch, or a picked release grabbed) grabs.
    pub fn plan_grab(&self, album_id: i32, grab: Grab) {
        self.with(|state| state.plans.push((album_id, grab)));
    }

    /// A grab of the user's own for the album now, outside any search of Octo's:
    /// `release_source` `Rss`, `InteractiveSearch`, `ReleasePush` and the like.
    pub fn user_grab(&self, album_id: i32, grab: Grab, release_source: &str) {
        self.with(|state| state.grab(album_id, grab, release_source));
    }

    /// A release Lidarr holds back (`delay`, `downloadClientUnavailable`, `fallback`): a queue
    /// item with no download id. Answers its queue id.
    pub fn pending_release(&self, album_id: i32, reason: &str) -> i32 {
        self.with(|state| {
            let artist_id = state.albums.get(&album_id).map_or(0, |a| a.artist_id);
            let queue_id = 2_000_000_000 + state.next_queue;
            state.next_queue += 1;
            state.pending.push(PendingRelease {
                queue_id,
                album_id,
                artist_id,
                title: format!("Pending release {queue_id}"),
                reason: reason.to_string(),
            });
            queue_id
        })
    }

    /// Lidarr's RSS sync drops a pending release.
    pub fn drop_pending(&self, queue_id: i32) {
        self.with(|state| state.pending.retain(|p| p.queue_id != queue_id));
    }

    /// The client reports how much is left.
    pub fn progress(&self, download_id: &str, size_left: i64) {
        self.with(|state| state.download(download_id).size_left = size_left);
    }

    /// The download finishes: its files lie in its folder, and the next refresh shows it ready
    /// to import.
    pub fn complete(&self, download_id: &str) {
        self.with(|state| {
            let root = state.local(LIDARR_DOWNLOADS).expect("the downloads folder");
            let download = state.download(download_id);
            download.state = Fetch::Completed;
            download.size_left = 0;
            let id = download.grab.download_id.clone();
            let names: Vec<(String, String)> = download
                .grab
                .files
                .iter()
                .map(|f| (file_name(f.track_id, &f.title, &f.quality), f.title.clone()))
                .collect();
            for (name, title) in names {
                write_file(
                    &root.join(&id).join(&name),
                    format!("{title} from {id}").as_bytes(),
                );
            }
        });
    }

    /// The client's own warning on the download, as qBittorrent gives a stalled torrent.
    pub fn stall(&self, download_id: &str) {
        self.with(|state| state.download(download_id).warning = true);
    }

    /// The download fails: its `downloadFailed` row, and Lidarr's own new search of the album
    /// when the grab came from a search.
    pub fn fail(&self, download_id: &str) {
        self.with(|state| {
            let download = state.download(download_id);
            download.state = Fetch::Failed;
            let download = download.clone();
            state.row_for_download(LidarrHistoryEvent::DownloadFailed, &download, None);
            if matches!(download.release_source.as_str(), "Search" | "UserInvokedSearch") {
                state.queue(
                    "AlbumSearch",
                    object(json!({ "albumIds": [download.album_id] })),
                    "unspecified",
                    false,
                );
            }
        });
    }

    /// Lidarr's state for a finished download it will not import: `importBlocked` or
    /// `importFailed`.
    pub fn block_import(&self, download_id: &str, tracked_state: &str) {
        self.with(|state| state.download(download_id).blocked = Some(tracked_state.to_string()));
    }

    /// The download is removed in the client itself: no history row, and gone from the next
    /// refresh.
    pub fn remove_in_client(&self, download_id: &str) {
        self.with(|state| {
            let folder = state
                .local(LIDARR_DOWNLOADS)
                .expect("the downloads folder")
                .join(download_id);
            state.download(download_id).removed = true;
            let _ = std::fs::remove_dir_all(folder);
        });
    }

    /// Lidarr's refresh of the downloads (RefreshMonitoredDownloads), which then queues the
    /// import of every finished one. Answers the command's id.
    pub fn refresh(&self) -> i32 {
        self.with(|state| state.queue("RefreshMonitoredDownloads", Map::new(), "scheduled", false))
    }

    /// The next refresh fails for one client: every item of that client is missing from it.
    pub fn client_fails_next_refresh(&self, client_name: &str) {
        self.with(|state| {
            state.failing_clients.insert(client_name.to_string());
        });
    }

    /// The user imports files from a folder (a ManualImport with no download chosen): their
    /// rows name no client, and carry the download id Lidarr guessed, if any.
    pub fn user_folder_import(&self, files: &[GrabFile], guessed_download_id: Option<&str>) -> i32 {
        self.with(|state| {
            let id = state.queue(
                "ManualImport",
                object(json!({ "importMode": "move" })),
                "manual",
                false,
            );
            if let Some(command) = state.commands.get_mut(&id) {
                command.folder_import = Some((files.to_vec(), guessed_download_id.map(str::to_string)));
            }
            id
        })
    }

    // ---- The quick Lidarr --------------------------------------------------------------------

    /// What a search imports at once: (track id, title, quality). Empty: nothing arrives.
    pub fn on_search(&self, imports: &[(i32, &str, &str)]) {
        self.with(|state| {
            state.on_search = imports
                .iter()
                .map(|(id, title, quality)| (*id, title.to_string(), quality.to_string()))
                .collect();
        });
    }

    /// A search's quick imports wait until [`FakeLidarr::release_imports`].
    pub fn hold_imports(&self) {
        self.with(|state| state.hold_imports = true);
    }

    /// Lets the held quick imports in now.
    pub fn release_imports(&self) {
        self.with(|state| {
            state.hold_imports = false;
            for (track_id, title, quality) in state.on_search.clone() {
                state.quick_import(track_id, &title, &quality);
            }
        });
    }

    /// A file for a track of an album Lidarr holds, at Lidarr's path, written where Octo sees
    /// it, with no history row. Answers Octo's path.
    pub fn import(&self, track_id: i32, title: &str, quality: &str) -> String {
        self.with(|state| state.quick_import(track_id, title, quality))
    }

    /// Lidarr's own upgrade of a track: the file it had goes, and a new one with a new id takes
    /// its place. Answers Octo's path of the new file.
    pub fn upgrade(&self, track_id: i32, title: &str, quality: &str) -> String {
        self.with(|state| {
            let old: Vec<i32> = state
                .files
                .values()
                .filter(|f| f.track_id == track_id)
                .map(|f| f.id)
                .collect();
            for id in old {
                if let Some(file) = state.files.remove(&id)
                    && let Some(local) = state.local(&file.path)
                {
                    let _ = std::fs::remove_file(local);
                }
            }
            if let Some(track) = state.tracks.get_mut(&track_id) {
                track.file_id = 0;
            }
            state.quick_import(track_id, title, quality)
        })
    }

    /// `Offered.Add((guid, indexerId, title, size))`.
    pub fn offer(&self, guid: &str, indexer_id: i32, title: &str, size: i64) {
        self.with(|state| {
            state
                .offered
                .push((guid.to_string(), indexer_id, title.to_string(), size));
        });
    }

    // ---- What Lidarr saw ---------------------------------------------------------------------

    pub fn searches(&self) -> usize {
        self.state.lock().searches
    }

    pub fn grabs(&self) -> Vec<String> {
        self.state.lock().grabs.clone()
    }

    /// The track files deleted through the API.
    pub fn deleted(&self) -> Vec<i32> {
        self.state.lock().deleted.clone()
    }

    /// Every change of an album's own monitored flag, in order.
    pub fn monitor_changes(&self) -> Vec<bool> {
        self.state.lock().monitor_changes.clone()
    }

    /// Every request, in order.
    pub fn requests(&self) -> Vec<Recorded> {
        self.state.lock().requests.clone()
    }

    /// Every request that could change Lidarr: anything but a GET.
    pub fn changes(&self) -> Vec<Recorded> {
        self.requests()
            .into_iter()
            .filter(|r| r.method != "GET")
            .collect()
    }
}

fn object(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

fn write_file(path: &Path, contents: &[u8]) {
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("the folder");
    std::fs::write(path, contents).expect("the file");
}

fn extension(quality: &str) -> &'static str {
    if quality.starts_with("FLAC") {
        "flac"
    } else {
        "mp3"
    }
}

/// The file name a download or an import gives a track.
fn file_name(number: i32, title: &str, quality: &str) -> String {
    format!("{number:02} - {title}.{}", extension(quality))
}

/// A time as Lidarr writes it: UTC, to the tenth of a microsecond, so no two of the fake's
/// times read alike.
fn at(time: DateTime<Utc>) -> Value {
    Value::String(datetime::format_utc(&time))
}

/// The ids a body names: `single` and every number in `many`.
fn ids(body: &Map<String, Value>, single: &str, many: &str) -> Vec<i32> {
    let mut found: Vec<i32> = body
        .get(single)
        .and_then(Value::as_i64)
        .into_iter()
        .chain(
            body.get(many)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_i64),
        )
        .filter_map(|n| i32::try_from(n).ok())
        .collect();
    found.dedup();
    found
}

fn answer(status: u16, body: Value) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_raw(body.to_string(), "application/json")
}

fn missing(what: &str) -> ResponseTemplate {
    answer(404, json!({ "message": format!("{what} not found") }))
}

impl State {
    /// Lidarr's clock: now, and always later than the last time it gave.
    fn tick(&mut self) -> DateTime<Utc> {
        let now = Utc::now().max(self.clock + TimeDelta::microseconds(1));
        self.clock = now;
        now
    }

    /// Where a Lidarr path lies on Octo's side.
    fn local(&self, lidarr_path: &str) -> Option<PathBuf> {
        let path = lidarr_path.replace('\\', "/");
        self.mounts
            .iter()
            .filter(|(prefix, _)| path == *prefix || path.starts_with(&format!("{prefix}/")))
            .max_by_key(|(prefix, _)| prefix.len())
            .map(|(prefix, local)| {
                let rest = path[prefix.len()..].trim_start_matches('/');
                if rest.is_empty() {
                    local.clone()
                } else {
                    local.join(rest)
                }
            })
    }

    /// The Lidarr path of a file on Octo's side.
    fn lidarr_path(&self, local: &Path) -> Option<String> {
        self.mounts
            .iter()
            .filter_map(|(prefix, root)| local.strip_prefix(root).ok().map(|rest| (prefix, root, rest)))
            .max_by_key(|(_, root, _)| root.as_os_str().len())
            .map(|(prefix, _, rest)| {
                let rest: Vec<String> = rest
                    .iter()
                    .map(|part| part.to_string_lossy().into_owned())
                    .collect();
                if rest.is_empty() {
                    prefix.clone()
                } else {
                    format!("{prefix}/{}", rest.join("/"))
                }
            })
    }

    fn catalog_album(&self, foreign_id: &str) -> Option<&CatalogAlbum> {
        self.catalog_albums
            .iter()
            .find(|a| dotnet::eq_ignore_case(&a.foreign_id, foreign_id))
    }

    fn artist_by_foreign_id(&self, foreign_id: &str) -> Option<&Artist> {
        self.artists
            .values()
            .find(|a| dotnet::eq_ignore_case(&a.foreign_id, foreign_id))
    }

    /// Puts a catalog artist in the library, unless it is there, and answers its id.
    fn ensure_artist(
        &mut self,
        foreign_id: &str,
        monitored: bool,
        monitor_new_items: &str,
        root: &str,
        profiles: (i32, i32),
        first_refresh: Option<(String, Vec<String>)>,
    ) -> i32 {
        if let Some(artist) = self.artist_by_foreign_id(foreign_id) {
            return artist.id;
        }
        let (name, id) = self
            .catalog_artists
            .iter()
            .find(|(id, ..)| dotnet::eq_ignore_case(id, foreign_id))
            .map(|(_, name, id)| (name.clone(), *id))
            .unwrap_or_else(|| (foreign_id.to_string(), 1000 + self.artists.len() as i32));
        let added = self.tick();
        self.artists.insert(
            id,
            Artist {
                id,
                foreign_id: foreign_id.to_string(),
                path: format!("{}/{name}", root.trim_end_matches('/')),
                name,
                monitored,
                monitor_new_items: monitor_new_items.to_string(),
                added,
                quality_profile_id: profiles.0,
                metadata_profile_id: profiles.1,
                first_refresh,
            },
        );
        id
    }

    /// Puts a catalog album in the library under an artist, without its tracks; Lidarr monitors
    /// its first release. Answers its id, or None when the metadata does not know it.
    fn insert_album(&mut self, foreign_id: &str, artist_id: i32, monitored: bool) -> Option<i32> {
        let catalog = self.catalog_album(foreign_id)?.clone();
        self.albums.entry(catalog.library_id).or_insert_with(|| Album {
            id: catalog.library_id,
            foreign_id: catalog.foreign_id.clone(),
            artist_id,
            title: catalog.title.clone(),
            release_date: catalog.release_date.clone(),
            monitored,
            any_release_ok: true,
            releases: catalog
                .releases
                .iter()
                .enumerate()
                .map(|(i, (id, foreign_id))| Release {
                    id: *id,
                    foreign_id: foreign_id.clone(),
                    title: catalog.title.clone(),
                    monitored: i == 0,
                })
                .collect(),
        });
        Some(catalog.library_id)
    }

    /// The album's tracks appear, as a refresh brings them.
    fn add_tracks(&mut self, album_id: i32) {
        let Some(album) = self.albums.get(&album_id) else {
            return;
        };
        let Some(catalog) = self.catalog_album(&album.foreign_id).cloned() else {
            return;
        };
        if !catalog.tracks_appear {
            return;
        }
        for (id, title, number, duration_ms, recording) in catalog.tracks {
            self.tracks.entry(id).or_insert(Track {
                id,
                album_id,
                title,
                number,
                duration_ms,
                recording,
                file_id: 0,
            });
        }
    }

    fn artist_of_album(&self, album_id: i32) -> Option<&Artist> {
        self.albums
            .get(&album_id)
            .and_then(|album| self.artists.get(&album.artist_id))
    }

    /// Where an import puts a track: `<artist folder>/<album title>/<number> - <title>`.
    fn library_path(&self, album_id: i32, track_id: i32, title: &str, quality: &str) -> String {
        let album = self.albums.get(&album_id).expect("an album Lidarr holds");
        let artist = self.artists.get(&album.artist_id).expect("its artist");
        let number = self.tracks.get(&track_id).map_or(track_id, |t| t.number);
        format!(
            "{}/{}/{}",
            artist.path,
            album.title,
            file_name(number, title, quality)
        )
    }

    fn insert_file(&mut self, album_id: i32, track_id: i32, path: String, quality: &str, size: i64) -> i32 {
        let id = self.next_file;
        self.next_file += 1;
        let added = self.tick();
        self.files.insert(
            id,
            TrackFile {
                id,
                album_id,
                track_id,
                path,
                quality: quality.to_string(),
                size,
                added,
            },
        );
        id
    }

    /// A file appears for a track, linked unless the track has one already, with no row.
    fn quick_import(&mut self, track_id: i32, title: &str, quality: &str) -> String {
        let album_id = self
            .tracks
            .get(&track_id)
            .map(|t| t.album_id)
            .expect("a track of an album Lidarr holds");
        let album = self.albums.get(&album_id).expect("its album");
        let artist = self.artists.get(&album.artist_id).expect("its artist");
        let path = format!(
            "{}/{}/{track_id:02} - {title}.{}",
            artist.path,
            album.title,
            extension(quality)
        );
        let local = self.local(&path).expect("a path in the root folder");
        write_file(&local, title.as_bytes());
        let id = self.insert_file(album_id, track_id, path, quality, 1000);
        if let Some(track) = self.tracks.get_mut(&track_id)
            && track.file_id == 0
        {
            track.file_id = id;
        }
        local.to_string_lossy().into_owned()
    }

    fn row(
        &mut self,
        event: LidarrHistoryEvent,
        album_id: i32,
        track_id: i32,
        download_id: Option<String>,
        source_title: String,
        data: Map<String, Value>,
    ) -> Row {
        let artist_id = self.albums.get(&album_id).map_or(0, |a| a.artist_id);
        let id = self.next_row;
        self.next_row += 1;
        Row {
            id,
            event,
            date: self.tick(),
            album_id,
            artist_id,
            track_id,
            download_id,
            source_title,
            data,
        }
    }

    fn row_for_download(
        &mut self,
        event: LidarrHistoryEvent,
        download: &Download,
        extra: Option<Map<String, Value>>,
    ) {
        let mut data = object(json!({
            "downloadClient": download.grab.client,
            "downloadClientName": download.grab.client_name,
        }));
        data.extend(extra.unwrap_or_default());
        let row = self.row(
            event,
            download.album_id,
            0,
            Some(download.grab.download_id.clone()),
            download.grab.title.clone(),
            data,
        );
        self.history.push(row);
    }

    fn download(&mut self, download_id: &str) -> &mut Download {
        self.downloads
            .iter_mut()
            .find(|d| d.grab.download_id == download_id)
            .expect("a download the fake grabbed")
    }

    /// A grab: the download in its client, its `grabbed` row, and a refresh of the downloads
    /// queued, as Lidarr refreshes them right after every grab.
    fn grab(&mut self, album_id: i32, grab: Grab, release_source: &str) {
        let artist_id = self.albums.get(&album_id).map_or(0, |a| a.artist_id);
        let queue_id = self.next_queue;
        self.next_queue += 1;
        let download = Download {
            queue_id,
            album_id,
            artist_id,
            release_source: release_source.to_string(),
            state: Fetch::Downloading,
            warning: false,
            size_left: grab.size,
            removed: false,
            blocked: None,
            imported: BTreeSet::new(),
            grab,
        };
        let protocol = if download.grab.protocol == "usenet" {
            "1"
        } else {
            "2"
        };
        self.row_for_download(
            LidarrHistoryEvent::Grabbed,
            &download,
            Some(object(json!({
                "indexer": "Fake indexer",
                "guid": download.grab.guid,
                "protocol": protocol,
                "releaseSource": release_source,
                "size": download.grab.size.to_string(),
            }))),
        );
        self.downloads.push(download);
        self.queue("RefreshMonitoredDownloads", Map::new(), "unspecified", false);
    }

    /// A search: the quick imports, then one planned grab for each album searched.
    fn search(&mut self, album_ids: &[i32], release_source: &str) {
        self.searches += 1;
        if !self.hold_imports {
            for (track_id, title, quality) in self.on_search.clone() {
                self.quick_import(track_id, &title, &quality);
            }
        }
        for album_id in album_ids {
            if let Some(position) = self.plans.iter().position(|(album, _)| album == album_id) {
                let (_, grab) = self.plans.remove(position);
                self.grab(*album_id, grab, release_source);
            }
        }
    }

    // ---- Commands ----------------------------------------------------------------------------

    fn queue(&mut self, name: &str, body: Map<String, Value>, trigger: &str, high_priority: bool) -> i32 {
        let id = self.next_command;
        self.next_command += 1;
        let queued = self.tick();
        self.commands.insert(
            id,
            Command {
                id,
                name: name.to_string(),
                body,
                trigger: trigger.to_string(),
                high_priority,
                status: "queued".to_string(),
                queued,
                started: None,
                ended: None,
                listed: true,
                rows: Vec::new(),
                folder_import: None,
            },
        );
        id
    }

    fn disk_busy(&self) -> bool {
        self.commands
            .values()
            .any(|c| c.status == "started" && DISK_COMMANDS.contains(&c.name.as_str()))
    }

    /// One step: the running commands write a row or end, then the queued ones start (the high
    /// priority ones first, one disk command at a time). Answers whether anything moved.
    fn step(&mut self) -> bool {
        let mut moved = false;
        let running: Vec<i32> = self
            .commands
            .values()
            .filter(|c| c.status == "started")
            .map(|c| c.id)
            .collect();
        for id in running {
            let next_row = self
                .commands
                .get_mut(&id)
                .and_then(|c| (!c.rows.is_empty()).then(|| c.rows.remove(0)));
            match next_row {
                Some(mut row) => {
                    row.date = self.tick();
                    self.history.push(row);
                }
                None => self.finish(id),
            }
            moved = true;
        }
        let mut queued: Vec<(bool, i32)> = self
            .commands
            .values()
            .filter(|c| c.status == "queued")
            .map(|c| (!c.high_priority, c.id))
            .collect();
        queued.sort_unstable();
        for (_, id) in queued {
            let disk = self
                .commands
                .get(&id)
                .is_some_and(|c| DISK_COMMANDS.contains(&c.name.as_str()));
            if disk && self.disk_busy() {
                continue;
            }
            self.start(id);
            moved = true;
        }
        moved
    }

    fn drain(&mut self) {
        for _ in 0..10_000 {
            if !self.step() {
                return;
            }
        }
        panic!("the fake's commands never end");
    }

    fn start(&mut self, id: i32) {
        let now = self.tick();
        let Some(command) = self.commands.get_mut(&id) else {
            return;
        };
        command.status = "started".to_string();
        command.started = Some(now);
        let name = command.name.clone();
        let body = command.body.clone();
        let trigger = command.trigger.clone();
        let folder_import = command.folder_import.take();
        let rows = match name.as_str() {
            "RefreshAlbum" => {
                for album_id in ids(&body, "albumId", "albumIds") {
                    self.add_tracks(album_id);
                }
                Vec::new()
            }
            "RefreshArtist" => {
                for artist_id in self.refreshed_artists(&body) {
                    self.refresh_artist(artist_id);
                }
                Vec::new()
            }
            "RescanFolders" => {
                self.rescan(&body);
                Vec::new()
            }
            "AlbumSearch" => {
                let source = if trigger == "manual" {
                    "UserInvokedSearch"
                } else {
                    "Search"
                };
                self.search(&ids(&body, "albumId", "albumIds"), source);
                Vec::new()
            }
            "ArtistSearch" => {
                let artists = ids(&body, "artistId", "artistIds");
                let albums: Vec<i32> = self
                    .albums
                    .values()
                    .filter(|a| a.monitored && artists.contains(&a.artist_id))
                    .map(|a| a.id)
                    .collect();
                let source = if trigger == "manual" {
                    "UserInvokedSearch"
                } else {
                    "Search"
                };
                self.search(&albums, source);
                Vec::new()
            }
            "RefreshMonitoredDownloads" => {
                self.take_snapshot();
                Vec::new()
            }
            "ProcessMonitoredDownloads" => self.import_finished(),
            "ManualImport" => match folder_import {
                Some((files, guessed)) => self.folder_import(&files, guessed),
                None => self.manual_import(&body),
            },
            _ => Vec::new(),
        };
        if self.rows_one_per_step {
            if let Some(command) = self.commands.get_mut(&id) {
                command.rows = rows;
            }
        } else {
            self.history.extend(rows);
        }
    }

    fn finish(&mut self, id: i32) {
        let now = self.tick();
        let Some(command) = self.commands.get_mut(&id) else {
            return;
        };
        command.status = "completed".to_string();
        command.ended = Some(now);
        let name = command.name.clone();
        let body = command.body.clone();
        match name.as_str() {
            "RefreshAlbum" => {
                let mut artists: Vec<i32> = ids(&body, "albumId", "albumIds")
                    .into_iter()
                    .filter_map(|album_id| self.artist_of_album(album_id).map(|a| a.id))
                    .collect();
                artists.dedup();
                for artist_id in artists {
                    self.queue_artist_rescan(artist_id, true);
                }
            }
            "RefreshArtist" => {
                let new_artist = body.get("isNewArtist").and_then(Value::as_bool) == Some(true);
                for artist_id in self.refreshed_artists(&body) {
                    self.apply_first_refresh(artist_id);
                    self.queue_artist_rescan(artist_id, new_artist);
                }
            }
            "RefreshMonitoredDownloads" => {
                self.queue("ProcessMonitoredDownloads", Map::new(), "unspecified", true);
            }
            _ => {}
        }
    }

    fn refreshed_artists(&self, body: &Map<String, Value>) -> Vec<i32> {
        let named = ids(body, "artistId", "artistIds");
        if named.is_empty() {
            self.artists.keys().copied().collect()
        } else {
            named
        }
    }

    /// RefreshArtist's work: the artist's albums get their tracks, and every album its
    /// metadata profile allows is added, monitored only when the artist monitors new items.
    fn refresh_artist(&mut self, artist_id: i32) {
        let Some(artist) = self.artists.get(&artist_id) else {
            return;
        };
        let foreign = artist.foreign_id.clone();
        let monitor_new = artist.monitor_new_items == "all";
        let missing: Vec<String> = self
            .catalog_albums
            .iter()
            .filter(|a| a.in_profile && dotnet::eq_ignore_case(&a.artist, &foreign))
            .map(|a| a.foreign_id.clone())
            .collect();
        for foreign_album in missing {
            self.insert_album(&foreign_album, artist_id, monitor_new);
        }
        let albums: Vec<i32> = self
            .albums
            .values()
            .filter(|a| a.artist_id == artist_id)
            .map(|a| a.id)
            .collect();
        for album_id in albums {
            self.add_tracks(album_id);
        }
    }

    /// After a new artist's first refresh, its add's `monitor` choice decides which albums are
    /// monitored.
    fn apply_first_refresh(&mut self, artist_id: i32) {
        let Some((monitor, named)) = self
            .artists
            .get_mut(&artist_id)
            .and_then(|a| a.first_refresh.take())
        else {
            return;
        };
        for album in self.albums.values_mut().filter(|a| a.artist_id == artist_id) {
            album.monitored = match monitor.as_str() {
                "all" => true,
                "none" => false,
                _ => named
                    .iter()
                    .any(|id| dotnet::eq_ignore_case(id, &album.foreign_id)),
            };
        }
    }

    /// The rescan a refresh queues: the artist's folder for a new artist or album, every root
    /// folder for a refresh of an artist Lidarr had.
    fn queue_artist_rescan(&mut self, artist_id: i32, artist_folder: bool) {
        let Some(artist) = self.artists.get(&artist_id) else {
            return;
        };
        let folders: Vec<String> = if artist_folder {
            vec![artist.path.clone()]
        } else {
            self.roots.clone()
        };
        self.queue(
            "RescanFolders",
            object(json!({
                "folders": folders, "artistIds": [artist_id], "filter": "matched", "addNewArtists": false,
            })),
            "unspecified",
            false,
        );
    }

    /// A rescan: records of covered files no longer on disk are dropped, and every audio file
    /// lying in the folders that a covered artist's track wants is linked where it lies, with
    /// no history row. No folders scans every root folder; no artist ids covers every artist.
    fn rescan(&mut self, body: &Map<String, Value>) {
        let mut folders: Vec<String> = body
            .get("folders")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if folders.is_empty() {
            folders = self.roots.clone();
        }
        let named = ids(body, "artistId", "artistIds");
        let artists: Vec<i32> = if named.is_empty() {
            self.artists.keys().copied().collect()
        } else {
            named
        };
        let under = |path: &str| {
            folders
                .iter()
                .any(|f| octo_core::lidarr::lidarr_client::folder_holds(f, path))
        };
        let gone: Vec<i32> = self
            .files
            .values()
            .filter(|f| {
                let artist = self.artist_of_album(f.album_id).map(|a| a.id);
                (f.album_id == 0 || artist.is_some_and(|a| artists.contains(&a)))
                    && under(&f.path)
                    && self.local(&f.path).is_none_or(|local| !local.is_file())
            })
            .map(|f| f.id)
            .collect();
        for id in gone {
            self.delete_file_record(id, "MissingFromDisk");
        }
        let mut found: Vec<PathBuf> = Vec::new();
        for folder in &folders {
            if let Some(local) = self.local(folder) {
                collect_audio(&local, &mut found);
            }
        }
        found.sort();
        for local in found {
            let Some(path) = self.lidarr_path(&local) else {
                continue;
            };
            if self
                .files
                .values()
                .any(|f| f.album_id != 0 && dotnet::eq_ignore_case(&f.path, &path))
            {
                continue;
            }
            let Some((album_id, track_id)) = self.identify(&local, &artists) else {
                continue;
            };
            if self.write_audio_tags != "no" {
                let mut bytes = std::fs::read(&local).unwrap_or_default();
                bytes.extend_from_slice(RETAGGED);
                write_file(&local, &bytes);
            }
            let unlinked = self
                .files
                .values()
                .find(|f| f.album_id == 0 && dotnet::eq_ignore_case(&f.path, &path))
                .map(|f| f.id);
            let file_id = match unlinked {
                Some(id) => {
                    if let Some(file) = self.files.get_mut(&id) {
                        file.album_id = album_id;
                        file.track_id = track_id;
                    }
                    id
                }
                None => {
                    let size = std::fs::metadata(&local).map_or(0, |m| m.len() as i64);
                    let quality = if path.to_ascii_lowercase().ends_with(".flac") {
                        "FLAC"
                    } else {
                        "MP3-320"
                    };
                    self.insert_file(album_id, track_id, path, quality, size)
                }
            };
            if let Some(track) = self.tracks.get_mut(&track_id) {
                track.file_id = file_id;
            }
        }
    }

    /// The track a file lying on disk is for: a track with no file, of a covered artist, whose
    /// title the file's name holds after its number (`03 - Teardrop`) or its artist
    /// (`Massive Attack - Teardrop`); the album named by the file's folder first.
    fn identify(&self, local: &Path, artists: &[i32]) -> Option<(i32, i32)> {
        let stem = local.file_stem()?.to_string_lossy().into_owned();
        let title = stem
            .trim_start_matches(|c: char| c.is_ascii_digit())
            .trim_start_matches([' ', '-', '.'])
            .to_string();
        let folder = local
            .parent()
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut candidates: Vec<(bool, i32, i32)> = Vec::new();
        for track in self.tracks.values().filter(|t| t.file_id == 0) {
            let Some(album) = self.albums.get(&track.album_id) else {
                continue;
            };
            let Some(artist) = self.artists.get(&album.artist_id) else {
                continue;
            };
            let with_artist = format!("{} - {}", artist.name, track.title);
            if artists.contains(&artist.id)
                && (dotnet::eq_ignore_case(&title, &track.title)
                    || dotnet::eq_ignore_case(&stem, &with_artist))
            {
                candidates.push((!dotnet::eq_ignore_case(&folder, &album.title), album.id, track.id));
            }
        }
        candidates.sort_unstable();
        candidates.first().map(|(_, album, track)| (*album, *track))
    }

    /// Drops a track file's record, unlinks its track, and writes its `trackFileDeleted` row.
    fn delete_file_record(&mut self, id: i32, reason: &str) {
        let Some(file) = self.files.remove(&id) else {
            return;
        };
        if let Some(track) = self.tracks.get_mut(&file.track_id)
            && track.file_id == id
        {
            track.file_id = 0;
        }
        let row = self.row(
            LidarrHistoryEvent::TrackFileDeleted,
            file.album_id,
            file.track_id,
            None,
            file.path.clone(),
            object(json!({ "reason": reason })),
        );
        self.history.push(row);
    }

    /// Lidarr deleting a file it has: from disk when it lies at its recorded path, into the
    /// recycle bin when one is set; then the record.
    fn remove_file(&mut self, id: i32, reason: &str) {
        let Some(file) = self.files.get(&id) else {
            return;
        };
        if let Some(local) = self.local(&file.path)
            && local.is_file()
        {
            if self.recycle_bin {
                let bin = self.local(LIDARR_RECYCLE_BIN).expect("the recycle bin");
                let name = local.file_name().expect("a file name").to_owned();
                std::fs::create_dir_all(&bin).expect("the recycle bin");
                std::fs::rename(&local, bin.join(name)).expect("moved to the recycle bin");
            } else {
                std::fs::remove_file(&local).expect("deleted");
            }
        }
        self.delete_file_record(id, reason);
    }

    // ---- Downloads and imports ---------------------------------------------------------------

    fn queue_item(&self, download: &Download) -> Value {
        let (status, tracked) = match download.state {
            Fetch::Downloading => ("downloading", "downloading".to_string()),
            Fetch::Completed => (
                "completed",
                download
                    .blocked
                    .clone()
                    .unwrap_or_else(|| "importPending".to_string()),
            ),
            Fetch::Failed => ("failed", "downloadFailed".to_string()),
            Fetch::Imported => ("completed", "imported".to_string()),
        };
        let health = if download.warning {
            "warning"
        } else if download.state == Fetch::Failed {
            "error"
        } else {
            "ok"
        };
        json!({
            "id": download.queue_id, "artistId": download.artist_id, "albumId": download.album_id,
            "title": download.grab.title, "size": download.grab.size as f64,
            "sizeleft": download.size_left as f64, "status": status, "trackedDownloadStatus": health,
            "trackedDownloadState": tracked, "statusMessages": [], "downloadId": download.grab.download_id,
            "protocol": download.grab.protocol, "downloadClient": download.grab.client_name,
            "indexer": "Fake indexer",
            "outputPath": format!("{LIDARR_DOWNLOADS}/{}", download.grab.download_id),
        })
    }

    /// RefreshMonitoredDownloads: the queue as the clients report it now. A client that fails
    /// this refresh drops all its items; with Completed Download Handling off a finished
    /// download is no longer tracked.
    fn take_snapshot(&mut self) {
        let failing = std::mem::take(&mut self.failing_clients);
        let items: Vec<Value> = self
            .downloads
            .iter()
            .filter(|d| {
                !d.removed
                    && d.state != Fetch::Imported
                    && !failing.contains(&d.grab.client_name)
                    && !(d.state == Fetch::Completed && !self.completed_download_handling)
            })
            .map(|d| self.queue_item(d))
            .collect();
        self.snapshot = Some(items);
    }

    /// ProcessMonitoredDownloads: imports every finished download, replacing the album's files.
    fn import_finished(&mut self) -> Vec<Row> {
        if !self.completed_download_handling {
            return Vec::new();
        }
        let ready: Vec<String> = self
            .downloads
            .iter()
            .filter(|d| d.state == Fetch::Completed && !d.removed && d.blocked.is_none())
            .map(|d| d.grab.download_id.clone())
            .collect();
        let mut rows = Vec::new();
        for download_id in ready {
            rows.extend(self.import_download(&download_id, None, true));
        }
        rows
    }

    /// Imports a download's files (only those at `paths` when given) as that tracked download.
    /// With `replace`, every file each album had goes first. The track files are inserted
    /// before any row; the rows come back for the command to write, the download's own row last
    /// once every file of it is in.
    fn import_download(&mut self, download_id: &str, paths: Option<&[String]>, replace: bool) -> Vec<Row> {
        let download = self.download(download_id).clone();
        let folder = format!("{LIDARR_DOWNLOADS}/{download_id}");
        let files: Vec<(String, GrabFile)> = download
            .grab
            .files
            .iter()
            .map(|f| {
                (
                    format!("{folder}/{}", file_name(f.track_id, &f.title, &f.quality)),
                    f.clone(),
                )
            })
            .filter(|(path, _)| !download.imported.contains(path))
            .filter(|(path, _)| paths.is_none_or(|paths| paths.contains(path)))
            .collect();
        if replace {
            let mut albums: Vec<i32> = files.iter().map(|(_, f)| f.album_id).collect();
            albums.sort_unstable();
            albums.dedup();
            for album_id in albums {
                let existing: Vec<i32> = self
                    .files
                    .values()
                    .filter(|f| f.album_id == album_id)
                    .map(|f| f.id)
                    .collect();
                for id in existing {
                    self.remove_file(id, "Upgrade");
                }
            }
        }
        let mut rows = Vec::new();
        for (dropped, file) in files {
            if !replace && self.tracks.get(&file.track_id).is_some_and(|t| t.file_id != 0) {
                continue;
            }
            let dropped_local = self.local(&dropped).expect("the downloads folder");
            let mut bytes = std::fs::read(&dropped_local).unwrap_or_else(|_| file.title.clone().into_bytes());
            if self.write_audio_tags != "no" {
                bytes.extend_from_slice(RETAGGED);
            }
            let imported = self.library_path(file.album_id, file.track_id, &file.title, &file.quality);
            write_file(&self.local(&imported).expect("a path in the root folder"), &bytes);
            if !download.grab.keeps_files {
                let _ = std::fs::remove_file(&dropped_local);
            }
            let file_id = self.insert_file(
                file.album_id,
                file.track_id,
                imported.clone(),
                &file.quality,
                bytes.len() as i64,
            );
            if let Some(track) = self.tracks.get_mut(&file.track_id) {
                track.file_id = file_id;
            }
            let row = self.row(
                LidarrHistoryEvent::TrackFileImported,
                file.album_id,
                file.track_id,
                Some(download.grab.download_id.clone()),
                download.grab.title.clone(),
                object(json!({
                    "fileId": file_id.to_string(), "droppedPath": dropped, "importedPath": imported,
                    "downloadClient": download.grab.client, "downloadClientName": download.grab.client_name,
                    "size": bytes.len().to_string(),
                })),
            );
            rows.push(row);
            self.download(download_id).imported.insert(dropped);
        }
        let done = self.download(download_id).imported.len() == download.grab.files.len();
        if done && download.state != Fetch::Imported {
            self.download(download_id).state = Fetch::Imported;
            let row = self.row(
                LidarrHistoryEvent::DownloadImported,
                download.album_id,
                0,
                Some(download.grab.download_id.clone()),
                download.grab.title.clone(),
                object(json!({
                    "downloadClient": download.grab.client, "downloadClientName": download.grab.client_name,
                })),
            );
            rows.push(row);
            if let Some(release_id) = download.grab.release_id
                && let Some(album) = self
                    .albums
                    .values_mut()
                    .find(|a| a.releases.iter().any(|r| r.id == release_id))
            {
                // The import's SetMonitored: the imported release alone is monitored, and no
                // rescan follows.
                for release in &mut album.releases {
                    release.monitored = release.id == release_id;
                }
            }
        }
        rows
    }

    /// Octo's ManualImport: each file of a tracked download is imported as that download.
    fn manual_import(&mut self, body: &Map<String, Value>) -> Vec<Row> {
        let replace = body
            .get("replaceExistingFiles")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut by_download: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for file in body.get("files").and_then(Value::as_array).into_iter().flatten() {
            if let (Some(path), Some(download_id)) = (
                file.get("path").and_then(Value::as_str),
                file.get("downloadId").and_then(Value::as_str),
            ) && self.downloads.iter().any(|d| d.grab.download_id == download_id)
            {
                by_download
                    .entry(download_id.to_string())
                    .or_default()
                    .push(path.to_string());
            }
        }
        let mut rows = Vec::new();
        for (download_id, paths) in by_download {
            rows.extend(self.import_download(&download_id, Some(&paths), replace));
        }
        rows
    }

    /// The user's folder import: the files land in the library with rows that name no client.
    fn folder_import(&mut self, files: &[GrabFile], guessed: Option<String>) -> Vec<Row> {
        let mut rows = Vec::new();
        for file in files {
            let imported = self.library_path(file.album_id, file.track_id, &file.title, &file.quality);
            let bytes = format!("{} from the user's folder", file.title).into_bytes();
            write_file(&self.local(&imported).expect("a path in the root folder"), &bytes);
            let file_id = self.insert_file(
                file.album_id,
                file.track_id,
                imported.clone(),
                &file.quality,
                bytes.len() as i64,
            );
            if let Some(track) = self.tracks.get_mut(&file.track_id) {
                track.file_id = file_id;
            }
            let row = self.row(
                LidarrHistoryEvent::TrackFileImported,
                file.album_id,
                file.track_id,
                guessed.clone(),
                file.title.clone(),
                object(json!({
                    "fileId": file_id.to_string(), "droppedPath": format!("/import/{}", file.title),
                    "importedPath": imported, "downloadClient": null, "downloadClientName": null,
                })),
            );
            rows.push(row);
        }
        rows
    }

    // ---- Answers -----------------------------------------------------------------------------

    fn artist_json(&self, artist: &Artist) -> Value {
        let albums: Vec<i32> = self
            .albums
            .values()
            .filter(|a| a.artist_id == artist.id)
            .map(|a| a.id)
            .collect();
        let files = self
            .files
            .values()
            .filter(|f| albums.contains(&f.album_id))
            .count() as i32;
        let tracks = self
            .tracks
            .values()
            .filter(|t| albums.contains(&t.album_id))
            .count();
        let root = artist.path.rsplit_once('/').map_or("", |(root, _)| root);
        json!({
            "id": artist.id, "artistName": artist.name, "foreignArtistId": artist.foreign_id,
            "path": artist.path, "rootFolderPath": root, "monitored": artist.monitored,
            "monitorNewItems": artist.monitor_new_items, "qualityProfileId": artist.quality_profile_id,
            "metadataProfileId": artist.metadata_profile_id, "added": at(artist.added), "tags": [],
            "statistics": {
                "albumCount": albums.len(),
                "trackFileCount": self.stale_statistics.get(&artist.id).copied().unwrap_or(files),
                "trackCount": tracks, "sizeOnDisk": 0,
            },
        })
    }

    fn album_json(&self, album: &Album) -> Value {
        let tracks = self.tracks.values().filter(|t| t.album_id == album.id).count();
        let files = self.files.values().filter(|f| f.album_id == album.id).count();
        json!({
            "id": album.id, "title": album.title, "foreignAlbumId": album.foreign_id,
            "artistId": album.artist_id, "monitored": album.monitored, "anyReleaseOk": album.any_release_ok,
            "releaseDate": album.release_date,
            "releases": album.releases.iter().map(|r| json!({
                "id": r.id, "albumId": album.id, "foreignReleaseId": r.foreign_id, "title": r.title,
                "monitored": r.monitored,
            })).collect::<Vec<_>>(),
            "artist": self.artists.get(&album.artist_id).map(|a| self.artist_json(a)),
            "statistics": { "trackCount": tracks, "trackFileCount": files, "totalTrackCount": tracks },
        })
    }

    /// A lookup row: the album as Lidarr holds it, else as the metadata knows it (id 0).
    fn lookup_json(&self, catalog: &CatalogAlbum) -> Value {
        if let Some(album) = self.albums.get(&catalog.library_id) {
            return self.album_json(album);
        }
        let artist = match self.artist_by_foreign_id(&catalog.artist) {
            Some(artist) => self.artist_json(artist),
            None => {
                let name = self
                    .catalog_artists
                    .iter()
                    .find(|(id, ..)| dotnet::eq_ignore_case(id, &catalog.artist))
                    .map_or(catalog.artist.clone(), |(_, name, _)| name.clone());
                json!({ "id": 0, "artistName": name, "foreignArtistId": catalog.artist, "monitored": false })
            }
        };
        json!({
            "id": 0, "foreignAlbumId": catalog.foreign_id, "title": catalog.title, "monitored": false,
            "anyReleaseOk": true, "releaseDate": catalog.release_date,
            "releases": catalog.releases.iter().map(|(_, foreign)| json!({
                "id": 0, "foreignReleaseId": foreign, "title": catalog.title, "monitored": false,
            })).collect::<Vec<_>>(),
            "artist": artist,
        })
    }

    fn track_json(&self, track: &Track) -> Value {
        let artist = self.artist_of_album(track.album_id);
        json!({
            "id": track.id, "albumId": track.album_id, "artistId": artist.map_or(0, |a| a.id),
            "title": track.title, "trackNumber": track.number.to_string(),
            "absoluteTrackNumber": track.number, "duration": track.duration_ms,
            "hasFile": track.file_id > 0, "trackFileId": track.file_id,
            "foreignRecordingId": track.recording,
            "artist": { "artistName": artist.map(|a| a.name.clone()) },
        })
    }

    fn file_json(&self, file: &TrackFile) -> Value {
        json!({
            "id": file.id, "artistId": self.artist_of_album(file.album_id).map_or(0, |a| a.id),
            "albumId": file.album_id, "path": file.path, "size": file.size, "dateAdded": at(file.added),
            "quality": { "quality": { "name": file.quality } },
        })
    }

    fn command_json(&self, command: &Command) -> Value {
        let mut body = command.body.clone();
        body.insert("name".into(), Value::from(command.name.clone()));
        body.insert("trigger".into(), Value::from(command.trigger.clone()));
        body.insert(
            "requiresDiskAccess".into(),
            Value::Bool(DISK_COMMANDS.contains(&command.name.as_str())),
        );
        json!({
            "id": command.id, "name": command.name, "commandName": command.name, "body": body,
            "priority": if command.high_priority { "high" } else { "normal" }, "status": command.status,
            "queued": at(command.queued), "started": command.started.map(at), "ended": command.ended.map(at),
            "trigger": command.trigger,
        })
    }

    fn row_json(row: &Row) -> Value {
        json!({
            "id": row.id, "albumId": row.album_id, "artistId": row.artist_id, "trackId": row.track_id,
            "sourceTitle": row.source_title, "date": at(row.date), "downloadId": row.download_id,
            "eventType": row.event.name(), "data": row.data,
        })
    }

    fn rows_newest_first(&self) -> Vec<&Row> {
        let mut rows: Vec<&Row> = self.history.iter().collect();
        rows.sort_by(|a, b| b.date.cmp(&a.date).then(b.id.cmp(&a.id)));
        rows
    }

    /// Every request goes through here: a PerPoll command read moves the commands one step
    /// first, and anything but a GET runs what it queued at once when commands are instant.
    fn handle(
        &mut self,
        method: &str,
        path: &str,
        query: &[(String, String)],
        body: Option<&Value>,
    ) -> ResponseTemplate {
        let lower = path.to_ascii_lowercase();
        let parts: Vec<&str> = lower.trim_end_matches('/').split('/').skip(3).collect();
        if self.mode == CommandMode::PerPoll && method == "GET" && parts.first() == Some(&"command") {
            self.step();
        }
        let response = self.route(method, &parts, query, body);
        if self.mode == CommandMode::Instant && method != "GET" {
            self.drain();
        }
        response
    }

    fn route(
        &mut self,
        method: &str,
        parts: &[&str],
        query: &[(String, String)],
        body: Option<&Value>,
    ) -> ResponseTemplate {
        let q = |key: &str| {
            query
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .map(|(_, v)| v.as_str())
        };
        let q_all = |key: &str| -> Vec<i32> {
            query
                .iter()
                .filter(|(k, _)| k.eq_ignore_ascii_case(key))
                .filter_map(|(_, v)| v.parse().ok())
                .collect()
        };
        let q_id = |key: &str| q(key).and_then(|v| v.parse::<i32>().ok());
        let empty = Value::Object(Map::new());
        let body = body.unwrap_or(&empty);
        match (method, parts) {
            ("GET", ["album", "lookup"]) => {
                let term = q("term").unwrap_or_default();
                let wanted = term.strip_prefix("lidarr:");
                let rows: Vec<Value> = self
                    .catalog_albums
                    .iter()
                    .filter(|a| !self.unknown.contains(&a.foreign_id))
                    .filter(|a| wanted.is_none_or(|id| dotnet::eq_ignore_case(id, &a.foreign_id)))
                    .map(|a| self.lookup_json(a))
                    .collect();
                answer(200, Value::Array(rows))
            }
            ("GET", ["album"]) => {
                let foreign = q("foreignAlbumId");
                let artist = q_id("artistId");
                let wanted = q_all("albumIds");
                let rows: Vec<Value> = self
                    .albums
                    .values()
                    .filter(|a| foreign.is_none_or(|id| dotnet::eq_ignore_case(id, &a.foreign_id)))
                    .filter(|a| artist.is_none_or(|id| id == a.artist_id))
                    .filter(|a| wanted.is_empty() || wanted.contains(&a.id))
                    .map(|a| self.album_json(a))
                    .collect();
                answer(200, Value::Array(rows))
            }
            ("GET", ["album", id]) => match id.parse().ok().and_then(|id: i32| self.albums.get(&id)) {
                Some(album) => answer(200, self.album_json(album)),
                None => missing("Album"),
            },
            ("POST", ["album"]) => self.post_album(body),
            ("PUT", ["album", "monitor"]) => {
                let monitored = body["monitored"].as_bool().unwrap_or(false);
                for id in ids(body.as_object().unwrap_or(&Map::new()), "", "albumIds") {
                    if let Some(album) = self.albums.get_mut(&id) {
                        album.monitored = monitored;
                    }
                }
                self.monitor_changes.push(monitored);
                answer(202, json!([]))
            }
            ("PUT", ["album", id]) => match id.parse::<i32>() {
                Ok(id) => self.put_album(id, body),
                Err(_) => missing("Album"),
            },
            ("DELETE", ["album", id]) => {
                let Some(id) = id.parse::<i32>().ok().filter(|id| self.albums.contains_key(id)) else {
                    return missing("Album");
                };
                self.delete_album(id, q("deleteFiles") == Some("true"));
                answer(200, json!({}))
            }
            ("GET", ["artist"]) => answer(
                200,
                Value::Array(self.artists.values().map(|a| self.artist_json(a)).collect()),
            ),
            ("GET", ["artist", "lookup"]) => {
                let term = q("term").unwrap_or_default();
                let wanted = term.strip_prefix("lidarr:").unwrap_or(term);
                let rows: Vec<Value> = self
                    .catalog_artists
                    .iter()
                    .filter(|(id, ..)| dotnet::eq_ignore_case(id, wanted))
                    .map(|(id, name, _)| {
                        let mut row = match self.artist_by_foreign_id(id) {
                            Some(artist) => self.artist_json(artist),
                            None => json!({ "id": 0, "artistName": name, "foreignArtistId": id }),
                        };
                        row["folder"] = Value::from(name.clone());
                        row
                    })
                    .collect();
                answer(200, Value::Array(rows))
            }
            ("GET", ["artist", id]) => match id.parse().ok().and_then(|id: i32| self.artists.get(&id)) {
                Some(artist) => answer(200, self.artist_json(artist)),
                None => missing("Artist"),
            },
            ("PUT", ["artist", id]) => {
                let Some(artist) = id.parse().ok().and_then(|id: i32| self.artists.get_mut(&id)) else {
                    return missing("Artist");
                };
                if let Some(monitored) = body["monitored"].as_bool() {
                    artist.monitored = monitored;
                }
                if let Some(items) = body["monitorNewItems"].as_str() {
                    artist.monitor_new_items = items.to_string();
                }
                let id = artist.id;
                answer(202, self.artist_json(&self.artists[&id]))
            }
            ("DELETE", ["artist", id]) => {
                let Some(id) = id.parse::<i32>().ok().filter(|id| self.artists.contains_key(id)) else {
                    return missing("Artist");
                };
                let delete_files = q("deleteFiles") == Some("true");
                let albums: Vec<i32> = self
                    .albums
                    .values()
                    .filter(|a| a.artist_id == id)
                    .map(|a| a.id)
                    .collect();
                for album in albums {
                    self.delete_album(album, false);
                }
                if let Some(artist) = self.artists.remove(&id)
                    && delete_files
                    && let Some(folder) = self.local(&artist.path)
                {
                    let _ = std::fs::remove_dir_all(folder);
                }
                answer(200, json!({}))
            }
            ("GET", ["track"]) => {
                let album = q_id("albumId");
                let rows: Vec<Value> = self
                    .tracks
                    .values()
                    .filter(|t| album.is_none_or(|id| id == t.album_id))
                    .filter(|t| self.albums.contains_key(&t.album_id))
                    .map(|t| self.track_json(t))
                    .collect();
                answer(200, Value::Array(rows))
            }
            ("GET", ["trackfile"]) => {
                let album = q_all("albumId");
                let artist = q_id("artistId");
                let rows: Vec<Value> = self
                    .files
                    .values()
                    .filter(|f| f.album_id != 0)
                    .filter(|f| album.is_empty() || album.contains(&f.album_id))
                    .filter(|f| artist.is_none_or(|id| self.artist_of_album(f.album_id).is_some_and(|a| a.id == id)))
                    .map(|f| self.file_json(f))
                    .collect();
                answer(200, Value::Array(rows))
            }
            ("DELETE", ["trackfile", id]) => match id.parse::<i32>() {
                Ok(id) => self.delete_track_file(id),
                Err(_) => missing("Track file"),
            },
            ("GET", ["command"]) => answer(
                200,
                Value::Array(
                    self.commands
                        .values()
                        .filter(|c| c.listed)
                        .map(|c| self.command_json(c))
                        .collect(),
                ),
            ),
            ("GET", ["command", id]) => match id.parse().ok().and_then(|id: i32| self.commands.get(&id)) {
                Some(command) => answer(200, self.command_json(command)),
                None => missing("Command"),
            },
            ("POST", ["command"]) => {
                let mut fields = body.as_object().cloned().unwrap_or_default();
                let name = fields
                    .remove("name")
                    .and_then(|n| n.as_str().map(str::to_string))
                    .unwrap_or_default();
                let id = self.queue(&name, fields, "manual", false);
                if self.mode == CommandMode::Instant {
                    self.drain();
                }
                answer(201, self.command_json(&self.commands[&id]))
            }
            ("GET", ["queue", "details"]) => {
                let artist = q_id("artistId");
                let albums = q_all("albumIds");
                let wanted = |artist_id: Option<i64>, album_id: Option<i64>| {
                    artist.is_none_or(|id| artist_id == Some(i64::from(id)))
                        && (albums.is_empty() || album_id.is_some_and(|a| albums.iter().any(|w| i64::from(*w) == a)))
                };
                let mut rows: Vec<Value> = self
                    .snapshot
                    .clone()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|item| wanted(item["artistId"].as_i64(), item["albumId"].as_i64()))
                    .collect();
                rows.extend(
                    self.pending
                        .iter()
                        .filter(|p| wanted(Some(i64::from(p.artist_id)), Some(i64::from(p.album_id))))
                        .map(|p| {
                            json!({
                                "id": p.queue_id, "artistId": p.artist_id, "albumId": p.album_id,
                                "title": p.title, "status": p.reason, "trackedDownloadState": "downloading",
                                "trackedDownloadStatus": "ok", "protocol": "torrent", "size": 1.0, "sizeleft": 1.0,
                            })
                        }),
                );
                answer(200, Value::Array(rows))
            }
            ("DELETE", ["queue", id]) => match id.parse::<i32>() {
                Ok(id) => self.remove_queue_item(
                    id,
                    q("removeFromClient") != Some("false"),
                    q("blocklist") == Some("true"),
                    q("skipRedownload") == Some("true"),
                ),
                Err(_) => missing("Queue item"),
            },
            ("GET", ["history"]) => {
                let page = q_id("page").unwrap_or(1).max(1);
                let size = q_id("pageSize").unwrap_or(10).max(1);
                let events = q_all("eventType");
                let album = q_id("albumId");
                let download = q("downloadId");
                let artists = q_all("artistIds");
                let rows: Vec<&Row> = self
                    .rows_newest_first()
                    .into_iter()
                    .filter(|r| events.is_empty() || events.contains(&r.event.code()))
                    .filter(|r| album.is_none_or(|id| id == r.album_id))
                    .filter(|r| download.is_none_or(|id| r.download_id.as_deref() == Some(id)))
                    .filter(|r| artists.is_empty() || artists.contains(&r.artist_id))
                    .collect();
                let records: Vec<Value> = rows
                    .iter()
                    .skip(((page - 1) * size) as usize)
                    .take(size as usize)
                    .map(|r| Self::row_json(r))
                    .collect();
                answer(
                    200,
                    json!({
                        "page": page, "pageSize": size, "sortKey": "date", "sortDirection": "descending",
                        "totalRecords": rows.len(), "records": records,
                    }),
                )
            }
            ("GET", ["history", "artist"]) => {
                let artist = q_id("artistId").unwrap_or(0);
                let album = q_id("albumId");
                let event = q_id("eventType");
                let rows: Vec<Value> = self
                    .rows_newest_first()
                    .into_iter()
                    .filter(|r| r.artist_id == artist)
                    .filter(|r| album.is_none_or(|id| id == r.album_id))
                    .filter(|r| event.is_none_or(|code| code == r.event.code()))
                    .map(Self::row_json)
                    .collect();
                answer(200, Value::Array(rows))
            }
            ("GET", ["config", "downloadclient"]) => answer(
                200,
                json!({
                    "id": 1, "downloadClientWorkingFolders": "_UNPACK_|_FAILED_",
                    "enableCompletedDownloadHandling": self.completed_download_handling,
                    "autoRedownloadFailed": true, "autoRedownloadFailedFromInteractiveSearch": true,
                }),
            ),
            ("GET", ["config", "metadataprovider"]) => answer(
                200,
                json!({
                    "id": 1, "metadataSource": "", "writeAudioTags": self.write_audio_tags,
                    "scrubAudioTags": false, "embedCoverArt": true,
                }),
            ),
            ("GET", ["config", "mediamanagement"]) => answer(
                200,
                json!({
                    "id": 1, "recycleBin": if self.recycle_bin { LIDARR_RECYCLE_BIN } else { "" },
                    "recycleBinCleanupDays": 7,
                }),
            ),
            ("GET", ["system", "status"]) => answer(
                200,
                json!({ "appName": "Lidarr", "version": "2.13.3.4711", "startTime": at(self.start_time) }),
            ),
            ("GET", ["rootfolder"]) => answer(
                200,
                Value::Array(
                    self.roots
                        .iter()
                        .enumerate()
                        .map(|(i, path)| json!({ "id": i + 1, "path": path, "accessible": true }))
                        .collect(),
                ),
            ),
            ("GET", ["health"]) => answer(200, self.health.clone()),
            ("GET", ["manualimport"]) => {
                let Some(download) = q("downloadId").and_then(|id| {
                    self.downloads
                        .iter()
                        .find(|d| d.grab.download_id == id && !d.removed && d.state == Fetch::Completed)
                }) else {
                    return answer(200, json!([]));
                };
                let rows: Vec<Value> = download
                    .grab
                    .files
                    .iter()
                    .map(|f| {
                        let name = file_name(f.track_id, &f.title, &f.quality);
                        let release = download.grab.release_id.or_else(|| {
                            self.albums
                                .get(&f.album_id)
                                .and_then(|a| a.releases.iter().find(|r| r.monitored).map(|r| r.id))
                        });
                        json!({
                            "path": format!("{LIDARR_DOWNLOADS}/{}/{name}", download.grab.download_id),
                            "name": name, "size": 1000,
                            "artist": { "id": self.artist_of_album(f.album_id).map_or(0, |a| a.id) },
                            "album": { "id": f.album_id }, "albumReleaseId": release.unwrap_or(0),
                            "tracks": [{ "id": f.track_id }], "quality": { "quality": { "name": f.quality } },
                            "downloadId": download.grab.download_id, "indexerFlags": 0,
                            "rejections": f.rejection.iter().map(|r| json!({ "type": "permanent", "reason": r })).collect::<Vec<_>>(),
                        })
                    })
                    .collect();
                answer(200, Value::Array(rows))
            }
            ("GET", ["release"]) => answer(
                200,
                Value::Array(
                    self.offered
                        .iter()
                        .map(|(guid, indexer_id, title, size)| {
                            json!({ "guid": guid, "indexerId": indexer_id, "title": title, "size": size })
                        })
                        .collect(),
                ),
            ),
            ("POST", ["release"]) => {
                let guid = body["guid"].as_str().unwrap_or_default().to_string();
                if !self.offered.iter().any(|(offered, ..)| *offered == guid) {
                    return answer(
                        404,
                        json!({ "message": "Couldn't find requested release in cache, cache timeout probably expired." }),
                    );
                }
                self.grabs.push(guid.clone());
                let album = self.plans.first().map(|(album, _)| *album).unwrap_or(7);
                if let Some(position) = self.plans.iter().position(|(a, _)| *a == album) {
                    let (_, mut grab) = self.plans.remove(position);
                    grab.guid = guid.clone();
                    self.plans.insert(position, (album, grab));
                }
                self.search(&[album], "InteractiveSearch");
                answer(200, json!({ "guid": guid, "indexerId": body["indexerId"] }))
            }
            _ => answer(404, json!({ "message": format!("no {method} /api/v1/{}", parts.join("/")) })),
        }
    }

    fn post_album(&mut self, body: &Value) -> ResponseTemplate {
        let foreign = body["foreignAlbumId"].as_str().unwrap_or_default().to_string();
        if self
            .albums
            .values()
            .any(|a| dotnet::eq_ignore_case(&a.foreign_id, &foreign))
        {
            return answer(
                400,
                json!([{ "errorMessage": "This album has already been added." }]),
            );
        }
        let Some(catalog) = self.catalog_album(&foreign).cloned() else {
            return answer(400, json!([{ "errorMessage": "Album not found." }]));
        };
        let given = body["artistId"]
            .as_i64()
            .and_then(|id| i32::try_from(id).ok())
            .filter(|id| self.artists.contains_key(id));
        let by_foreign = self.artist_by_foreign_id(&catalog.artist).map(|a| a.id);
        let (artist_id, created) = match given.or(by_foreign) {
            Some(id) => (id, false),
            None => {
                let artist = &body["artist"];
                let options = &artist["addOptions"];
                let first_refresh = Some((
                    options["monitor"].as_str().unwrap_or("all").to_string(),
                    options["albumsToMonitor"]
                        .as_array()
                        .map(|ids| ids.iter().filter_map(Value::as_str).map(str::to_string).collect())
                        .unwrap_or_default(),
                ));
                let id = self.ensure_artist(
                    &catalog.artist,
                    artist["monitored"].as_bool().unwrap_or(true),
                    artist["monitorNewItems"].as_str().unwrap_or("all"),
                    artist["rootFolderPath"].as_str().unwrap_or(LIDARR_ROOT),
                    (
                        artist["qualityProfileId"].as_i64().unwrap_or(1) as i32,
                        artist["metadataProfileId"].as_i64().unwrap_or(1) as i32,
                    ),
                    first_refresh,
                );
                (id, true)
            }
        };
        let monitored = body["monitored"].as_bool().unwrap_or(true);
        let album_id = self
            .insert_album(&foreign, artist_id, monitored)
            .expect("an album the metadata knows");
        if created {
            self.queue(
                "RefreshArtist",
                object(json!({ "artistIds": [artist_id], "isNewArtist": true })),
                "unspecified",
                false,
            );
        } else {
            self.queue(
                "RefreshAlbum",
                object(json!({ "albumId": album_id, "isNewAlbum": true })),
                "unspecified",
                false,
            );
        }
        answer(201, self.album_json(&self.albums[&album_id]))
    }

    /// An album changed in place. When its monitored releases change, or it starts taking any
    /// release, Lidarr's AlbumEditedService unlinks its track files and rescans the artist's
    /// folder.
    fn put_album(&mut self, id: i32, body: &Value) -> ResponseTemplate {
        let Some(album) = self.albums.get_mut(&id) else {
            return missing("Album");
        };
        let monitored_releases = |album: &Album| -> Vec<i32> {
            album
                .releases
                .iter()
                .filter(|r| r.monitored)
                .map(|r| r.id)
                .collect()
        };
        let before = (monitored_releases(album), album.any_release_ok);
        let mut monitor_change = None;
        if let Some(monitored) = body["monitored"].as_bool()
            && monitored != album.monitored
        {
            album.monitored = monitored;
            monitor_change = Some(monitored);
        }
        if let Some(any) = body["anyReleaseOk"].as_bool() {
            album.any_release_ok = any;
        }
        for release in body["releases"].as_array().into_iter().flatten() {
            let wanted = release["id"].as_i64();
            if let (Some(wanted), Some(monitored)) = (wanted, release["monitored"].as_bool())
                && let Some(own) = album.releases.iter_mut().find(|r| i64::from(r.id) == wanted)
            {
                own.monitored = monitored;
            }
        }
        let edited = monitored_releases(album) != before.0 || (!before.1 && album.any_release_ok);
        let artist_id = album.artist_id;
        if let Some(monitored) = monitor_change {
            self.monitor_changes.push(monitored);
        }
        if edited {
            let linked: Vec<i32> = self
                .files
                .values()
                .filter(|f| f.album_id == id)
                .map(|f| f.id)
                .collect();
            for file_id in linked {
                if let Some(file) = self.files.get_mut(&file_id) {
                    file.album_id = 0;
                    file.track_id = 0;
                }
            }
            for track in self.tracks.values_mut().filter(|t| t.album_id == id) {
                track.file_id = 0;
            }
            self.queue_artist_rescan(artist_id, true);
        }
        answer(202, self.album_json(&self.albums[&id]))
    }

    /// Deletes an album's records; its files only with `delete_files`.
    fn delete_album(&mut self, id: i32, delete_files: bool) {
        let files: Vec<i32> = self
            .files
            .values()
            .filter(|f| f.album_id == id)
            .map(|f| f.id)
            .collect();
        for file_id in files {
            if delete_files {
                self.remove_file(file_id, "Manual");
            } else {
                self.files.remove(&file_id);
            }
        }
        self.tracks.retain(|_, t| t.album_id != id);
        self.albums.remove(&id);
    }

    /// `DELETE /api/v1/trackfile/{id}`: 404 for no record, 409 while the artist's root folder is
    /// missing or empty, else the file goes only when it lies at its recorded path.
    fn delete_track_file(&mut self, id: i32) -> ResponseTemplate {
        let Some(file) = self.files.get(&id) else {
            return missing("Track file");
        };
        // An unlinked file's artist folder is the folder above its album's.
        let artist_path = self
            .artist_of_album(file.album_id)
            .map(|a| a.path.clone())
            .unwrap_or_else(|| {
                file.path
                    .rsplit_once('/')
                    .and_then(|(album, _)| album.rsplit_once('/'))
                    .map_or(String::new(), |(artist, _)| artist.to_string())
            });
        let root = artist_path
            .rsplit_once('/')
            .map_or(String::new(), |(root, _)| root.to_string());
        let root_is_there = self.local(&root).is_some_and(|local| {
            std::fs::read_dir(local).is_ok_and(|entries| {
                entries
                    .flatten()
                    .any(|e| e.path().is_dir() && e.file_name() != FAKE_FOLDER)
            })
        });
        if !root_is_there {
            return answer(
                409,
                json!({ "message": format!("Artist's root folder ({root}) doesn't exist or is empty.") }),
            );
        }
        self.remove_file(id, "Manual");
        self.deleted.push(id);
        answer(200, json!({}))
    }

    fn remove_queue_item(
        &mut self,
        id: i32,
        from_client: bool,
        blocklist: bool,
        skip_redownload: bool,
    ) -> ResponseTemplate {
        if let Some(position) = self.pending.iter().position(|p| p.queue_id == id) {
            self.pending.remove(position);
            return answer(200, json!({}));
        }
        let listed = self.snapshot.as_ref().is_some_and(|items| {
            items
                .iter()
                .any(|item| item["id"].as_i64() == Some(i64::from(id)))
        });
        let Some(index) = self
            .downloads
            .iter()
            .position(|d| d.queue_id == id && !d.removed)
            .filter(|_| listed)
        else {
            return missing("Queue item");
        };
        if let Some(items) = self.snapshot.as_mut() {
            items.retain(|item| item["id"].as_i64() != Some(i64::from(id)));
        }
        self.downloads[index].removed = true;
        let download = self.downloads[index].clone();
        if from_client {
            let folder = self
                .local(&format!("{LIDARR_DOWNLOADS}/{}", download.grab.download_id))
                .expect("the downloads folder");
            let _ = std::fs::remove_dir_all(folder);
        }
        if blocklist {
            self.row_for_download(LidarrHistoryEvent::DownloadFailed, &download, None);
            if !skip_redownload {
                self.queue(
                    "AlbumSearch",
                    object(json!({ "albumIds": [download.album_id] })),
                    "unspecified",
                    false,
                );
            }
        }
        answer(200, json!({}))
    }
}

/// Every audio file under a folder, skipping dot folders as Lidarr's scans do.
fn collect_audio(folder: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            if !name.starts_with('.') && !name.eq_ignore_ascii_case("@eadir") {
                collect_audio(&path, found);
            }
        } else if path
            .extension()
            .is_some_and(|e| AUDIO.iter().any(|a| e.eq_ignore_ascii_case(a)))
        {
            found.push(path);
        }
    }
}

impl Respond for FakeLidarr {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Option<Value> = serde_json::from_slice(&request.body).ok();
        let path = match request.url.query() {
            Some(query) => format!("{}?{query}", request.url.path()),
            None => request.url.path().to_string(),
        };
        let query: Vec<(String, String)> = request
            .url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        let mut state = self.state.lock();
        state.requests.push(Recorded {
            method: request.method.as_str().to_string(),
            path,
            body: body.clone(),
        });
        state.handle(request.method.as_str(), request.url.path(), &query, body.as_ref())
    }
}
