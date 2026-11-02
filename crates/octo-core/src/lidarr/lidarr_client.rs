//! The pure half of the Lidarr client: its records, the strict readers for Lidarr's JSON (a
//! value of the wrong kind is an error), and the album choice. The HTTP side is
//! `octo::services::lidarr::lidarr_client`.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::common::dotnet;
use crate::common::{ArtistAgreement, SongIdentity};
use crate::json::datetime;

/// What can go wrong talking to Lidarr, by kind, so callers can branch on the kind. Each
/// carries its message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LidarrError {
    /// Not set up, no single album, an answer of the wrong shape, or a JSON value of the wrong
    /// kind.
    #[error("{0}")]
    InvalidOperation(String),
    /// Lidarr answered with an error status, or could not be reached.
    #[error("{0}")]
    Http(String),
    /// The client's timeout, or the caller's cancellation.
    #[error("{0}")]
    Canceled(String),
    /// An answer that is not JSON.
    #[error("{0}")]
    Json(String),
    /// Two track files with one id.
    #[error("{0}")]
    Argument(String),
    /// The track fetcher found no album, or no good enough copy.
    #[error("{0}")]
    FileNotFound(String),
    /// Copying the file out failed.
    #[error("{0}")]
    Io(String),
}

impl LidarrError {
    /// The caller canceled the operation.
    pub fn operation_canceled() -> Self {
        LidarrError::Canceled("The operation was canceled.".to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LidarrChoice {
    pub id: i32,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LidarrRootFolder {
    pub id: i32,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LidarrOptions {
    pub root_folders: Vec<LidarrRootFolder>,
    pub quality_profiles: Vec<LidarrChoice>,
    pub metadata_profiles: Vec<LidarrChoice>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LidarrAlbumCandidate {
    pub id: i32,
    pub foreign_album_id: String,
    pub title: String,
    pub artist: String,
    pub year: Option<i32>,
    /// The lookup row as Lidarr sent it, which is what adding the album posts back.
    pub resource: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LidarrImportedTrack {
    pub id: i32,
    pub title: String,
    pub track_number: Option<i32>,
    pub duration_seconds: Option<i32>,
    pub has_file: bool,
    pub path: Option<String>,
    pub size_bytes: i64,
    pub artist: Option<String>,
    /// 0 when the track has no file.
    pub track_file_id: i32,
    pub quality: Option<String>,
    /// The MusicBrainz recording Lidarr matched the track to. It only orders candidates: it
    /// never proves which recording a file holds.
    pub foreign_recording_id: Option<String>,
}

/// An album Lidarr already has, as it stood before Octo touched it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LidarrAlbumState {
    pub id: i32,
    pub monitored: bool,
}

/// A search Lidarr accepted, and whether Octo added or monitored the album to get it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LidarrSearchStarted {
    pub album_id: i32,
    pub existed: bool,
    pub was_monitored: bool,
}

/// One release an indexer offered for an album, as Lidarr's interactive search lists it.
/// `rejected` says Lidarr would not take it on its own, and `rejections` says why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LidarrRelease {
    pub guid: String,
    pub indexer_id: i32,
    pub title: String,
    pub indexer: Option<String>,
    pub quality: Option<String>,
    pub size: Option<i64>,
    pub seeders: Option<i32>,
    pub leechers: Option<i32>,
    pub protocol: Option<String>,
    pub age_days: Option<i32>,
    pub rejected: bool,
    pub rejections: Vec<String>,
}

/// A release someone picked in Find songs, grabbed instead of Lidarr's own search. Its title
/// and size find it again when Lidarr has forgotten the search that listed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LidarrReleasePick {
    pub guid: String,
    pub indexer_id: i32,
    pub title: Option<String>,
    pub size: Option<i64>,
}

/// What a picked release no indexer offers any more is told as.
pub const PICK_GONE_TEXT: &str =
    "The release you picked is no longer offered by the indexers. Search again and pick another.";

/// What grabbing a picked release says when Lidarr refuses the release found again.
pub const PICK_REFUSED_TEXT: &str =
    "Lidarr would not take the release you picked. Search again and pick another.";

/// Whether a release is the picked one in a newer listing: the same indexer and title, and the
/// same size when both say.
pub fn same_release(release: &LidarrRelease, pick: &LidarrReleasePick) -> bool {
    release.indexer_id == pick.indexer_id
        && pick.title.as_deref().is_some_and(|title| {
            !dotnet::is_blank(title) && dotnet::eq_ignore_case(release.title.trim(), title.trim())
        })
        && (release.size.is_none() || pick.size.is_none() || release.size == pick.size)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LidarrAlbumImportState {
    pub tracks: Vec<LidarrImportedTrack>,
    pub track_count: i32,
    pub track_file_count: i32,
}

impl LidarrAlbumImportState {
    pub fn is_complete(&self) -> bool {
        self.track_count > 0 && self.track_file_count >= self.track_count
    }
}

/// An artist as Lidarr holds it.
#[derive(Debug, Clone, PartialEq)]
pub struct LidarrArtist {
    pub id: i32,
    /// The MusicBrainz artist id.
    pub foreign_artist_id: String,
    pub name: String,
    /// The artist's folder, in Lidarr's own paths.
    pub path: Option<String>,
    pub monitored: bool,
    /// Which new releases Lidarr monitors: `all`, `none` or `new`.
    pub monitor_new_items: Option<String>,
    /// When the artist was added to Lidarr.
    pub added: Option<DateTime<Utc>>,
    /// `statistics.trackFileCount`. Lidarr keeps it apart from the track files, so it is only a
    /// first read: above 0 it proves files, at 0 the track file list has the last word.
    pub track_file_count: Option<i32>,
    /// The artist as Lidarr sent it, which an album added under this artist posts back.
    pub resource: Map<String, Value>,
}

/// One release (edition) of an album Lidarr holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LidarrAlbumRelease {
    pub id: i32,
    /// The MusicBrainz release id.
    pub foreign_release_id: String,
    pub title: String,
    pub monitored: bool,
}

/// An album as Lidarr holds it.
#[derive(Debug, Clone, PartialEq)]
pub struct LidarrAlbum {
    pub id: i32,
    /// The MusicBrainz release group id.
    pub foreign_album_id: String,
    pub title: String,
    pub artist_id: i32,
    pub monitored: bool,
    /// Whether Lidarr takes any release of the album, not only its monitored ones.
    pub any_release_ok: bool,
    pub releases: Vec<LidarrAlbumRelease>,
    /// `statistics.trackFileCount`, when Lidarr sent the album's statistics.
    pub track_file_count: Option<i32>,
    /// The album as Lidarr sent it, which changing it puts back.
    pub resource: Map<String, Value>,
}

/// Whether one release of an album is monitored.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct LidarrReleaseMonitored {
    pub id: i32,
    pub monitored: bool,
}

/// Which releases of an album Lidarr takes: its monitored ones, or any release. Saved before
/// Octo pins an album to one release, and put back exactly afterwards.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct LidarrReleaseChoice {
    pub any_release_ok: bool,
    /// Every release of the album, by id.
    pub releases: Vec<LidarrReleaseMonitored>,
}

impl LidarrAlbum {
    /// The album's release choice as it stands.
    pub fn release_choice(&self) -> LidarrReleaseChoice {
        let mut releases: Vec<LidarrReleaseMonitored> = self
            .releases
            .iter()
            .map(|r| LidarrReleaseMonitored {
                id: r.id,
                monitored: r.monitored,
            })
            .collect();
        releases.sort_by_key(|r| r.id);
        LidarrReleaseChoice {
            any_release_ok: self.any_release_ok,
            releases,
        }
    }

    /// The choice that makes one release the only one Lidarr takes, or None when the album
    /// lists no release with that id.
    pub fn pinned_to(&self, release_id: i32) -> Option<LidarrReleaseChoice> {
        if !self.releases.iter().any(|r| r.id == release_id) {
            return None;
        }
        let mut choice = self.release_choice();
        choice.any_release_ok = false;
        for release in &mut choice.releases {
            release.monitored = release.id == release_id;
        }
        Some(choice)
    }

    /// The release with this MusicBrainz release id, compared ignoring case.
    pub fn release(&self, foreign_release_id: &str) -> Option<&LidarrAlbumRelease> {
        self.releases
            .iter()
            .find(|r| dotnet::eq_ignore_case(&r.foreign_release_id, foreign_release_id))
    }
}

/// What adding an album came to: its id, and the id of the artist it was added under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LidarrAdded {
    pub album_id: i32,
    pub artist_id: i32,
}

/// One track file record, as Lidarr's database holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LidarrTrackFile {
    pub id: i32,
    pub artist_id: i32,
    pub album_id: i32,
    /// The file, in Lidarr's own paths.
    pub path: Option<String>,
    pub size_bytes: i64,
    pub quality: Option<String>,
}

/// One item of Lidarr's queue: a download it tracks, or a release it holds back (pending).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LidarrQueueItem {
    /// What removing the item takes. A pending release's id is Lidarr's own hash.
    pub id: i32,
    pub artist_id: Option<i32>,
    pub album_id: Option<i32>,
    /// The download client's id for the download; a pending release has none.
    pub download_id: Option<String>,
    /// The name the download client has in Lidarr; a pending release has none.
    pub download_client: Option<String>,
    pub title: String,
    /// The client's own state (`queued`, `paused`, `downloading`, `completed`, `failed`,
    /// `warning`), or why a pending release waits (`delay`, `downloadClientUnavailable`,
    /// `fallback`).
    pub status: Option<String>,
    /// Lidarr's state for the download: `downloading`, `importPending`, `importBlocked`,
    /// `importFailed`, `imported`, `downloadFailed`, `downloadFailedPending`, `ignored`. It is
    /// copied at each refresh of the downloads, so it never says an import is running.
    pub tracked_download_state: Option<String>,
    /// `ok`, `warning` (the client's own warning, such as a stalled torrent) or `error`.
    pub tracked_download_status: Option<String>,
    pub size_bytes: Option<i64>,
    pub size_left_bytes: Option<i64>,
    /// Where the client puts the download, in Lidarr's paths.
    pub output_path: Option<String>,
    pub protocol: Option<String>,
    /// When Lidarr took the item, on Lidarr's clock: a download's newest grab row, a held-back
    /// release's own time.
    pub added: Option<DateTime<Utc>>,
    /// The indexer the release came from.
    pub indexer: Option<String>,
}

/// What a history row records, as Lidarr numbers and names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LidarrHistoryEvent {
    Unknown,
    Grabbed,
    ArtistFolderImported,
    TrackFileImported,
    DownloadFailed,
    TrackFileDeleted,
    TrackFileRenamed,
    AlbumImportIncomplete,
    DownloadImported,
    TrackFileRetagged,
    DownloadIgnored,
}

impl LidarrHistoryEvent {
    /// Every event, in Lidarr's number order.
    pub const ALL: [LidarrHistoryEvent; 11] = [
        LidarrHistoryEvent::Unknown,
        LidarrHistoryEvent::Grabbed,
        LidarrHistoryEvent::ArtistFolderImported,
        LidarrHistoryEvent::TrackFileImported,
        LidarrHistoryEvent::DownloadFailed,
        LidarrHistoryEvent::TrackFileDeleted,
        LidarrHistoryEvent::TrackFileRenamed,
        LidarrHistoryEvent::AlbumImportIncomplete,
        LidarrHistoryEvent::DownloadImported,
        LidarrHistoryEvent::TrackFileRetagged,
        LidarrHistoryEvent::DownloadIgnored,
    ];

    /// Lidarr's number for the event, which a history query filters by.
    pub fn code(self) -> i32 {
        match self {
            LidarrHistoryEvent::Unknown => 0,
            LidarrHistoryEvent::Grabbed => 1,
            LidarrHistoryEvent::ArtistFolderImported => 2,
            LidarrHistoryEvent::TrackFileImported => 3,
            LidarrHistoryEvent::DownloadFailed => 4,
            LidarrHistoryEvent::TrackFileDeleted => 5,
            LidarrHistoryEvent::TrackFileRenamed => 6,
            LidarrHistoryEvent::AlbumImportIncomplete => 7,
            LidarrHistoryEvent::DownloadImported => 8,
            LidarrHistoryEvent::TrackFileRetagged => 9,
            LidarrHistoryEvent::DownloadIgnored => 10,
        }
    }

    /// Lidarr's name for the event, as its answers write it.
    pub fn name(self) -> &'static str {
        match self {
            LidarrHistoryEvent::Unknown => "unknown",
            LidarrHistoryEvent::Grabbed => "grabbed",
            LidarrHistoryEvent::ArtistFolderImported => "artistFolderImported",
            LidarrHistoryEvent::TrackFileImported => "trackFileImported",
            LidarrHistoryEvent::DownloadFailed => "downloadFailed",
            LidarrHistoryEvent::TrackFileDeleted => "trackFileDeleted",
            LidarrHistoryEvent::TrackFileRenamed => "trackFileRenamed",
            LidarrHistoryEvent::AlbumImportIncomplete => "albumImportIncomplete",
            LidarrHistoryEvent::DownloadImported => "downloadImported",
            LidarrHistoryEvent::TrackFileRetagged => "trackFileRetagged",
            LidarrHistoryEvent::DownloadIgnored => "downloadIgnored",
        }
    }

    /// The event a row's `eventType` names, by name (any case) or by number. An event Lidarr
    /// adds later reads as `Unknown`; a value of another kind is an error.
    pub fn read(value: &Value) -> Result<LidarrHistoryEvent, LidarrError> {
        let found = match value {
            Value::Null => None,
            Value::String(name) => Self::ALL
                .into_iter()
                .find(|event| dotnet::eq_ignore_case(event.name(), name)),
            Value::Number(number) => number
                .as_i64()
                .and_then(|n| Self::ALL.into_iter().find(|event| i64::from(event.code()) == n)),
            other => return Err(cannot_convert(other, "EntityHistoryEventType")),
        };
        Ok(found.unwrap_or(LidarrHistoryEvent::Unknown))
    }

    /// Whether the row ends a download: Lidarr writes one only after every file's own row.
    pub fn ends_download(self) -> bool {
        matches!(
            self,
            LidarrHistoryEvent::DownloadImported
                | LidarrHistoryEvent::AlbumImportIncomplete
                | LidarrHistoryEvent::DownloadFailed
                | LidarrHistoryEvent::DownloadIgnored
        )
    }
}

/// One row of Lidarr's history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LidarrHistoryRecord {
    pub id: i32,
    pub event: LidarrHistoryEvent,
    pub date: DateTime<Utc>,
    pub album_id: i32,
    pub artist_id: i32,
    pub track_id: i32,
    /// The download client's id for the download the row is about.
    pub download_id: Option<String>,
    /// The release's title for a grab; the file's path for a deleted file.
    pub source_title: String,
    /// The row's details by Lidarr's key; null values are left out.
    pub data: BTreeMap<String, String>,
}

impl LidarrHistoryRecord {
    /// One of the row's details by its key, compared ignoring case; None when it is missing or
    /// blank.
    pub fn data(&self, key: &str) -> Option<&str> {
        self.data
            .iter()
            .find(|(k, _)| dotnet::eq_ignore_case(k, key))
            .map(|(_, v)| v.as_str())
            .filter(|v| !dotnet::is_blank(v))
    }

    /// The track file a `trackFileImported` row is about (`data.fileId`).
    pub fn file_id(&self) -> Option<i32> {
        self.data("fileId").and_then(|id| id.trim().parse().ok())
    }

    /// The download client that delivered the files (`data.downloadClient`). Every import of a
    /// tracked download names it; a folder import never does.
    pub fn download_client(&self) -> Option<&str> {
        self.data("downloadClient")
    }

    /// What made a grab (`data.releaseSource`): `Search`, `UserInvokedSearch`,
    /// `InteractiveSearch`, `ReleasePush`, `Rss` and the like.
    pub fn release_source(&self) -> Option<&str> {
        self.data("releaseSource")
    }

    /// The grabbed release's guid (`data.guid`).
    pub fn guid(&self) -> Option<&str> {
        self.data("guid")
    }

    /// The file as the download delivered it, before Lidarr imported it (`data.droppedPath`).
    pub fn dropped_path(&self) -> Option<&str> {
        self.data("droppedPath")
    }

    /// Where Lidarr's import put the file (`data.importedPath`).
    pub fn imported_path(&self) -> Option<&str> {
        self.data("importedPath")
    }

    /// Why a track file was deleted (`data.reason`): `Upgrade`, `Manual`, `MissingFromDisk` and
    /// the like.
    pub fn reason(&self) -> Option<&str> {
        self.data("reason")
    }
}

/// One page of `GET /api/v1/history`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LidarrHistoryPage {
    /// Every row the query matches, on every page.
    pub total_records: i32,
    pub records: Vec<LidarrHistoryRecord>,
}

/// One of Lidarr's commands.
#[derive(Debug, Clone, PartialEq)]
pub struct LidarrCommand {
    pub id: i32,
    /// The command's name, such as `AlbumSearch` or `RescanFolders`.
    pub name: String,
    /// `queued`, `started`, `completed`, `failed`, `aborted`, `cancelled` or `orphaned`.
    pub status: String,
    pub queued: Option<DateTime<Utc>>,
    pub started: Option<DateTime<Utc>>,
    pub ended: Option<DateTime<Utc>>,
    /// `manual` for a command asked through the API or the UI, `scheduled` for one on Lidarr's
    /// own timer, `unspecified` for one Lidarr pushed itself.
    pub trigger: Option<String>,
    /// What the command was asked to do, as Lidarr sent it.
    pub body: Map<String, Value>,
}

impl LidarrCommand {
    /// Queued or running.
    pub fn is_active(&self) -> bool {
        matches!(self.status.as_str(), "queued" | "started")
    }

    /// The albums the command names (`albumId` and `albumIds`).
    pub fn album_ids(&self) -> Vec<i32> {
        ids_in(&self.body, "albumId", "albumIds")
    }

    /// The artists the command names (`artistId` and `artistIds`).
    pub fn artist_ids(&self) -> Vec<i32> {
        ids_in(&self.body, "artistId", "artistIds")
    }

    /// Whether this is a RefreshArtist or RefreshAlbum that refreshes the album or its artist,
    /// matched as [`refresh_covers`] matches.
    pub fn refreshes(&self, album_id: i32, artist_id: i32) -> Result<bool, LidarrError> {
        refresh_covers(Some(&self.name), Some(&self.body), album_id, artist_id)
    }

    /// Whether this is a RescanFolders that can link files to the artist: one with no folders
    /// (every root folder, as Lidarr's daily scan is), one over the artist's folder or a folder
    /// above it, or one naming the artist.
    pub fn rescans_artist(&self, artist_id: i32, artist_path: Option<&str>) -> bool {
        if self.name != "RescanFolders" {
            return false;
        }
        if self.artist_ids().contains(&artist_id) {
            return true;
        }
        let folders: Vec<&str> = match self.body.get("folders") {
            Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        folders.is_empty()
            || artist_path.is_some_and(|path| folders.iter().any(|folder| folder_holds(folder, path)))
    }
}

/// One of Lidarr's health checks as it stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LidarrHealthCheck {
    /// The check that reported it, such as `IndexerStatusCheck`.
    pub source: String,
    /// `ok`, `notice`, `warning` or `error`.
    pub kind: String,
    pub message: String,
    /// The wiki page about it; its fragment names the exact problem.
    pub wiki_url: Option<String>,
}

/// One file Lidarr's manual import offers from a download.
#[derive(Debug, Clone, PartialEq)]
pub struct LidarrManualImportItem {
    /// The file, in Lidarr's paths.
    pub path: String,
    pub artist_id: i32,
    pub album_id: i32,
    pub album_release_id: i32,
    pub track_ids: Vec<i32>,
    /// The quality as Lidarr sent it, which importing the file posts back.
    pub quality: Value,
    pub indexer_flags: i32,
    pub download_id: Option<String>,
    /// Why Lidarr would not import the file; empty when it would.
    pub rejections: Vec<String>,
}

/// What asking Lidarr to delete something came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LidarrDeletion {
    /// Lidarr deleted it.
    Deleted,
    /// Lidarr has nothing by that id (404): it is gone already.
    NotFound,
    /// Lidarr would not delete it now (409). For a track file: the artist's root folder is
    /// missing or empty.
    Conflict,
}

// --- Reading Lidarr's JSON strictly. A missing property and a JSON null both read as absent;
// a value of another kind is an error, never read as absent. ---

fn kind_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "Null",
        Value::Bool(true) => "True",
        Value::Bool(false) => "False",
        Value::Number(_) => "Number",
        Value::String(_) => "String",
        Value::Array(_) => "Array",
        Value::Object(_) => "Object",
    }
}

fn cannot_convert(value: &Value, type_name: &str) -> LidarrError {
    match value {
        Value::Array(_) | Value::Object(_) => {
            LidarrError::InvalidOperation("The node must be of type 'JsonValue'.".to_string())
        }
        _ => LidarrError::InvalidOperation(format!(
            "An element of type '{}' cannot be converted to a '{type_name}'.",
            kind_name(value)
        )),
    }
}

/// The property, or None when it is missing or null.
pub fn property<'a>(o: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    o.get(key).filter(|v| !v.is_null())
}

/// The property of a node that may not be an object: anything but an object is an error.
pub fn child<'a>(node: &'a Value, key: &str) -> Result<Option<&'a Value>, LidarrError> {
    match node {
        Value::Object(o) => Ok(property(o, key)),
        _ => Err(LidarrError::InvalidOperation(
            "The node must be of type 'JsonObject'.".to_string(),
        )),
    }
}

/// The property when it is an object, else None.
pub fn object<'a>(o: &'a Map<String, Value>, key: &str) -> Option<&'a Map<String, Value>> {
    o.get(key).and_then(Value::as_object)
}

/// The value as a string; any other kind is an error.
pub fn value_str(value: &Value) -> Result<&str, LidarrError> {
    value
        .as_str()
        .ok_or_else(|| cannot_convert(value, "System.String"))
}

/// The property as a string, None when it is missing or null.
pub fn nullable_str(o: &Map<String, Value>, key: &str) -> Result<Option<String>, LidarrError> {
    property(o, key)
        .map(|v| value_str(v).map(str::to_string))
        .transpose()
}

/// The property as a string, empty when it is missing or null.
pub fn str_or_empty(o: &Map<String, Value>, key: &str) -> Result<String, LidarrError> {
    Ok(nullable_str(o, key)?.unwrap_or_default())
}

/// The property as an `i32`, None when it is missing or null. A number with a fraction or out
/// of range is an error.
pub fn nullable_int(o: &Map<String, Value>, key: &str) -> Result<Option<i32>, LidarrError> {
    property(o, key)
        .map(|v| {
            v.as_i64()
                .and_then(|n| i32::try_from(n).ok())
                .ok_or_else(|| cannot_convert(v, "System.Int32"))
        })
        .transpose()
}

/// The property as an `i32`, 0 when it is missing or null.
pub fn int_or_zero(o: &Map<String, Value>, key: &str) -> Result<i32, LidarrError> {
    Ok(nullable_int(o, key)?.unwrap_or(0))
}

/// The property as an `i64`, None when it is missing or null.
pub fn nullable_long(o: &Map<String, Value>, key: &str) -> Result<Option<i64>, LidarrError> {
    property(o, key)
        .map(|v| v.as_i64().ok_or_else(|| cannot_convert(v, "System.Int64")))
        .transpose()
}

/// The property as a bool, None when it is missing or null.
pub fn nullable_bool(o: &Map<String, Value>, key: &str) -> Result<Option<bool>, LidarrError> {
    property(o, key)
        .map(|v| v.as_bool().ok_or_else(|| cannot_convert(v, "System.Boolean")))
        .transpose()
}

/// The property as a UTC time, None when it is missing or null. A string that is no date is an
/// error, as is a value of another kind.
pub fn nullable_date(o: &Map<String, Value>, key: &str) -> Result<Option<DateTime<Utc>>, LidarrError> {
    match nullable_str(o, key)? {
        None => Ok(None),
        Some(text) => datetime::parse_utc(&text)
            .map(Some)
            .ok_or_else(|| LidarrError::InvalidOperation(format!("'{text}' is not a date."))),
    }
}

/// The property as a whole number of bytes however Lidarr wrote it (a decimal may carry a
/// fraction), None when it is missing or null. Anything but a number is an error.
pub fn nullable_bytes(o: &Map<String, Value>, key: &str) -> Result<Option<i64>, LidarrError> {
    property(o, key)
        .map(|v| {
            v.as_f64()
                .map(|n| n.round() as i64)
                .ok_or_else(|| cannot_convert(v, "System.Decimal"))
        })
        .transpose()
}

/// The ids a command's body names: `single` when it is a whole number, and every whole number
/// in `many`.
fn ids_in(body: &Map<String, Value>, single: &str, many: &str) -> Vec<i32> {
    let mut ids: Vec<i32> = Vec::new();
    let mut add = |value: &Value| {
        if let Some(id) = value.as_i64().and_then(|n| i32::try_from(n).ok())
            && !ids.contains(&id)
        {
            ids.push(id);
        }
    };
    if let Some(value) = body.get(single) {
        add(value);
    }
    if let Some(Value::Array(values)) = body.get(many) {
        values.iter().for_each(&mut add);
    }
    ids
}

/// One row of `GET /api/v1/rootfolder`.
pub fn parse_root_folder(row: &Map<String, Value>) -> Result<LidarrRootFolder, LidarrError> {
    Ok(LidarrRootFolder {
        id: int_or_zero(row, "id")?,
        path: str_or_empty(row, "path")?,
    })
}

/// The server's root folders and profiles.
pub fn map_options(
    roots: &[Map<String, Value>],
    quality: &[Map<String, Value>],
    metadata: &[Map<String, Value>],
) -> Result<LidarrOptions, LidarrError> {
    let choice = |x: &Map<String, Value>| -> Result<LidarrChoice, LidarrError> {
        Ok(LidarrChoice {
            id: int_or_zero(x, "id")?,
            name: str_or_empty(x, "name")?,
        })
    };
    Ok(LidarrOptions {
        root_folders: roots
            .iter()
            .map(parse_root_folder)
            .collect::<Result<_, LidarrError>>()?,
        quality_profiles: quality.iter().map(choice).collect::<Result<_, _>>()?,
        metadata_profiles: metadata.iter().map(choice).collect::<Result<_, _>>()?,
    })
}

/// One row of an album lookup, its year from the first four characters of its release date.
pub fn parse_album(row: &Map<String, Value>) -> Result<LidarrAlbumCandidate, LidarrError> {
    let artist = object(row, "artist");
    let date = nullable_str(row, "releaseDate")?;
    let year = date.as_deref().and_then(|d| {
        // The first four UTF-16 units; a cut surrogate pair is no number either.
        if dotnet::utf16_len(d) < 4 {
            return None;
        }
        let units: Vec<u16> = d.encode_utf16().take(4).collect();
        String::from_utf16(&units)
            .ok()
            .and_then(|head| int_try_parse(&head))
    });
    Ok(LidarrAlbumCandidate {
        id: nullable_int(row, "id")?.unwrap_or(0),
        foreign_album_id: nullable_str(row, "foreignAlbumId")?.unwrap_or_default(),
        title: nullable_str(row, "title")?.unwrap_or_default(),
        artist: match artist {
            Some(a) => nullable_str(a, "artistName")?.unwrap_or_default(),
            None => String::new(),
        },
        year,
        resource: row.clone(),
    })
}

/// A whole number however Lidarr wrote it: read as a double, then rounded half to even.
/// Anything but a number is None.
fn nullable_number(o: &Map<String, Value>, key: &str) -> Option<i32> {
    o.get(key)
        .and_then(Value::as_f64)
        .map(|n| n.round_ties_even() as i32)
}

/// One row of `GET /api/v1/release`.
pub fn parse_release(row: &Map<String, Value>) -> Result<LidarrRelease, LidarrError> {
    let quality = object(row, "quality").and_then(|q| object(q, "quality"));
    let age_days = match row.get("ageHours").and_then(Value::as_f64) {
        Some(hours) => Some((hours / 24.0).round_ties_even() as i32),
        None => nullable_number(row, "age"),
    };
    let rejections = match row.get("rejections") {
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| match item {
                Value::Null => Ok(String::new()),
                other => value_str(other).map(str::to_string),
            })
            .filter(|r| r.as_ref().map_or(true, |r| !r.is_empty()))
            .collect::<Result<Vec<_>, _>>()?,
        _ => Vec::new(),
    };
    Ok(LidarrRelease {
        guid: nullable_str(row, "guid")?.unwrap_or_default(),
        indexer_id: nullable_number(row, "indexerId").unwrap_or(0),
        title: nullable_str(row, "title")?.unwrap_or_default(),
        indexer: nullable_str(row, "indexer")?,
        quality: match quality {
            Some(q) => nullable_str(q, "name")?,
            None => None,
        },
        size: row.get("size").and_then(Value::as_f64).map(|bytes| bytes as i64),
        seeders: nullable_number(row, "seeders"),
        leechers: nullable_number(row, "leechers"),
        protocol: nullable_str(row, "protocol")?,
        age_days,
        rejected: row.get("rejected").and_then(Value::as_bool).unwrap_or(false),
        rejections,
    })
}

/// The objects of an array property: none when it is missing or null, an error when it is
/// anything but an array. Items that are not objects are skipped.
fn objects_in<'a>(o: &'a Map<String, Value>, key: &str) -> Result<Vec<&'a Map<String, Value>>, LidarrError> {
    match property(o, key) {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => Ok(items.iter().filter_map(Value::as_object).collect()),
        Some(_) => Err(LidarrError::InvalidOperation(
            "Lidarr returned an invalid array response.".to_string(),
        )),
    }
}

/// The MusicBrainz artist id of an artist row: `foreignArtistId`, else `mbId`.
fn foreign_artist_id_of(row: &Map<String, Value>) -> Result<Option<String>, LidarrError> {
    match nullable_str(row, "foreignArtistId")? {
        Some(id) => Ok(Some(id)),
        None => nullable_str(row, "mbId"),
    }
}

/// `statistics.trackFileCount`, when the row carries its statistics.
fn track_file_count_of(row: &Map<String, Value>) -> Result<Option<i32>, LidarrError> {
    match object(row, "statistics") {
        Some(statistics) => nullable_int(statistics, "trackFileCount"),
        None => Ok(None),
    }
}

/// One row of `GET /api/v1/artist`.
pub fn parse_artist(row: &Map<String, Value>) -> Result<LidarrArtist, LidarrError> {
    Ok(LidarrArtist {
        id: int_or_zero(row, "id")?,
        foreign_artist_id: foreign_artist_id_of(row)?.unwrap_or_default(),
        name: str_or_empty(row, "artistName")?,
        path: nullable_str(row, "path")?,
        monitored: nullable_bool(row, "monitored")?.unwrap_or(false),
        monitor_new_items: nullable_str(row, "monitorNewItems")?,
        added: nullable_date(row, "added")?,
        track_file_count: track_file_count_of(row)?,
        resource: row.clone(),
    })
}

/// The MusicBrainz artist id and the folder of one row of `GET /api/v1/artist/lookup`. The
/// folder is the one Lidarr would give the artist under a root folder.
pub fn parse_artist_lookup(row: &Map<String, Value>) -> Result<(String, Option<String>), LidarrError> {
    Ok((
        foreign_artist_id_of(row)?.unwrap_or_default(),
        nullable_str(row, "folder")?.filter(|folder| !dotnet::is_blank(folder)),
    ))
}

/// One album as `GET /api/v1/album` lists it.
pub fn parse_library_album(row: &Map<String, Value>) -> Result<LidarrAlbum, LidarrError> {
    let artist_id = match nullable_int(row, "artistId")?.filter(|id| *id > 0) {
        Some(id) => id,
        None => match object(row, "artist") {
            Some(artist) => int_or_zero(artist, "id")?,
            None => 0,
        },
    };
    let releases = objects_in(row, "releases")?
        .into_iter()
        .map(|release| {
            Ok(LidarrAlbumRelease {
                id: int_or_zero(release, "id")?,
                foreign_release_id: str_or_empty(release, "foreignReleaseId")?,
                title: str_or_empty(release, "title")?,
                monitored: nullable_bool(release, "monitored")?.unwrap_or(false),
            })
        })
        .collect::<Result<Vec<_>, LidarrError>>()?;
    Ok(LidarrAlbum {
        id: int_or_zero(row, "id")?,
        foreign_album_id: str_or_empty(row, "foreignAlbumId")?,
        title: str_or_empty(row, "title")?,
        artist_id,
        monitored: nullable_bool(row, "monitored")?.unwrap_or(false),
        // Lidarr's own default for an album.
        any_release_ok: nullable_bool(row, "anyReleaseOk")?.unwrap_or(true),
        releases,
        track_file_count: track_file_count_of(row)?,
        resource: row.clone(),
    })
}

/// One row of `GET /api/v1/trackfile`.
pub fn parse_track_file(row: &Map<String, Value>) -> Result<LidarrTrackFile, LidarrError> {
    let quality = object(row, "quality").and_then(|q| object(q, "quality"));
    Ok(LidarrTrackFile {
        id: int_or_zero(row, "id")?,
        artist_id: int_or_zero(row, "artistId")?,
        album_id: int_or_zero(row, "albumId")?,
        path: nullable_str(row, "path")?,
        size_bytes: nullable_long(row, "size")?.unwrap_or(0),
        quality: match quality {
            Some(q) => nullable_str(q, "name")?,
            None => None,
        },
    })
}

/// One row of `GET /api/v1/queue/details`.
pub fn parse_queue_item(row: &Map<String, Value>) -> Result<LidarrQueueItem, LidarrError> {
    let present = |value: Option<String>| value.filter(|v| !dotnet::is_blank(v));
    Ok(LidarrQueueItem {
        id: int_or_zero(row, "id")?,
        artist_id: nullable_int(row, "artistId")?,
        album_id: nullable_int(row, "albumId")?,
        download_id: present(nullable_str(row, "downloadId")?),
        download_client: present(nullable_str(row, "downloadClient")?),
        title: str_or_empty(row, "title")?,
        status: nullable_str(row, "status")?,
        tracked_download_state: nullable_str(row, "trackedDownloadState")?,
        tracked_download_status: nullable_str(row, "trackedDownloadStatus")?,
        size_bytes: nullable_bytes(row, "size")?,
        size_left_bytes: nullable_bytes(row, "sizeleft")?,
        output_path: nullable_str(row, "outputPath")?,
        protocol: nullable_str(row, "protocol")?,
        added: nullable_date(row, "added")?,
        indexer: present(nullable_str(row, "indexer")?),
    })
}

/// One history row. A row has a date; one without is an error.
pub fn parse_history(row: &Map<String, Value>) -> Result<LidarrHistoryRecord, LidarrError> {
    let date = nullable_date(row, "date")?
        .ok_or_else(|| LidarrError::InvalidOperation("A Lidarr history row has no date.".to_string()))?;
    let mut data = BTreeMap::new();
    if let Some(details) = object(row, "data") {
        for (key, value) in details {
            if !value.is_null() {
                data.insert(key.clone(), value_str(value)?.to_string());
            }
        }
    }
    Ok(LidarrHistoryRecord {
        id: int_or_zero(row, "id")?,
        event: LidarrHistoryEvent::read(row.get("eventType").unwrap_or(&Value::Null))?,
        date,
        album_id: int_or_zero(row, "albumId")?,
        artist_id: int_or_zero(row, "artistId")?,
        track_id: int_or_zero(row, "trackId")?,
        download_id: nullable_str(row, "downloadId")?.filter(|id| !dotnet::is_blank(id)),
        source_title: str_or_empty(row, "sourceTitle")?,
        data,
    })
}

/// One page of `GET /api/v1/history`.
pub fn parse_history_page(page: &Map<String, Value>) -> Result<LidarrHistoryPage, LidarrError> {
    Ok(LidarrHistoryPage {
        total_records: int_or_zero(page, "totalRecords")?,
        records: objects_in(page, "records")?
            .into_iter()
            .map(parse_history)
            .collect::<Result<_, _>>()?,
    })
}

/// One row of `GET /api/v1/command`, or the answer to posting one.
pub fn parse_command(row: &Map<String, Value>) -> Result<LidarrCommand, LidarrError> {
    Ok(LidarrCommand {
        id: int_or_zero(row, "id")?,
        name: str_or_empty(row, "name")?,
        status: str_or_empty(row, "status")?,
        queued: nullable_date(row, "queued")?,
        started: nullable_date(row, "started")?,
        ended: nullable_date(row, "ended")?,
        trigger: nullable_str(row, "trigger")?,
        body: object(row, "body").cloned().unwrap_or_default(),
    })
}

/// One row of `GET /api/v1/health`.
pub fn parse_health_check(row: &Map<String, Value>) -> Result<LidarrHealthCheck, LidarrError> {
    Ok(LidarrHealthCheck {
        source: str_or_empty(row, "source")?,
        kind: str_or_empty(row, "type")?,
        message: str_or_empty(row, "message")?,
        wiki_url: nullable_str(row, "wikiUrl")?,
    })
}

/// One row of `GET /api/v1/manualimport`. The artist, the album and the tracks are read from
/// the nested resources, else from their ids. A rejection with no reason still counts.
pub fn parse_manual_import(row: &Map<String, Value>) -> Result<LidarrManualImportItem, LidarrError> {
    let id_of = |resource: &str, id: &str| -> Result<i32, LidarrError> {
        match object(row, resource) {
            Some(nested) => int_or_zero(nested, "id"),
            None => int_or_zero(row, id),
        }
    };
    let track_ids = match property(row, "tracks") {
        Some(_) => objects_in(row, "tracks")?
            .into_iter()
            .map(|track| int_or_zero(track, "id"))
            .collect::<Result<Vec<_>, _>>()?,
        None => ids_in(row, "", "trackIds"),
    };
    let mut rejections = Vec::new();
    if let Some(Value::Array(items)) = row.get("rejections") {
        for item in items {
            match item {
                Value::Null => {}
                Value::Object(rejection) => {
                    let reason = str_or_empty(rejection, "reason")?;
                    rejections.push(if dotnet::is_blank(&reason) {
                        str_or_empty(rejection, "type")?
                    } else {
                        reason
                    });
                }
                other => {
                    let reason = value_str(other)?;
                    if !reason.is_empty() {
                        rejections.push(reason.to_string());
                    }
                }
            }
        }
    }
    Ok(LidarrManualImportItem {
        path: str_or_empty(row, "path")?,
        artist_id: id_of("artist", "artistId")?,
        album_id: id_of("album", "albumId")?,
        album_release_id: int_or_zero(row, "albumReleaseId")?,
        track_ids,
        quality: row.get("quality").cloned().unwrap_or(Value::Null),
        indexer_flags: int_or_zero(row, "indexerFlags")?,
        download_id: nullable_str(row, "downloadId")?.filter(|id| !dotnet::is_blank(id)),
        rejections,
    })
}

/// The album whose title reads the same as asked, by [`SongIdentity::key`], credited to the
/// artist asked for by [`SongIdentity::compare_artists`] (a shared artist and no clash of guests:
/// "Jay-Z & Kanye West" for "Jay-Z", "Kanye West" for "Ye", never "Airbourne" for "Air");
/// several releases of it are told apart by the year. None when there is no single one.
pub fn select_best_album<'a>(
    candidates: &'a [LidarrAlbumCandidate],
    artist: &str,
    album: &str,
    year: Option<i32>,
) -> Option<&'a LidarrAlbumCandidate> {
    let wanted_album = SongIdentity::key(album);
    let mut exact: Vec<&LidarrAlbumCandidate> = Vec::new();
    for c in candidates {
        if matches!(
            SongIdentity::compare_artists(&c.artist, artist),
            ArtistAgreement::Agree | ArtistAgreement::Loose
        ) && SongIdentity::key(&c.title) == wanted_album
            && !exact
                .iter()
                .any(|e| dotnet::eq_ignore_case(&e.foreign_album_id, &c.foreign_album_id))
        {
            exact.push(c);
        }
    }
    if exact.len() == 1 {
        return Some(exact[0]);
    }
    if exact.is_empty() {
        return None;
    }

    if let Some(y) = year {
        let same_year: Vec<_> = exact.iter().filter(|c| c.year == Some(y)).collect();
        if same_year.len() == 1 {
            return Some(same_year[0]);
        }
    }

    // Multiple releases with the same display identity are unsafe to choose
    // automatically. A wrong album is much worse than an actionable failure.
    None
}

/// A track number from "3", "1-3", "A/2" and the like, by the part before the first `-`, `/`
/// or `.`.
pub fn parse_track_number(value: &str) -> Option<i32> {
    let head = value.split(['-', '/', '.']).next().unwrap_or("");
    int_try_parse(head)
}

/// An `i32` from ASCII digits, allowing white space around it and a leading sign.
fn int_try_parse(s: &str) -> Option<i32> {
    let is_white = |c: char| matches!(c, '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' ');
    let s = s.trim_matches(is_white);
    let (negative, digits) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut n: i64 = 0;
    for b in digits.bytes() {
        n = n.checked_mul(10)?.checked_add(i64::from(b - b'0'))?;
        if n > i64::from(i32::MAX) + 1 {
            return None;
        }
    }
    i32::try_from(if negative { -n } else { n }).ok()
}

/// Whether a command's body names an id: `single` equal to it, or `many` holding it. A `single`
/// of another kind than a whole number is an error; a `many` that is no array names nothing.
pub fn body_names_id(
    body: Option<&Map<String, Value>>,
    single: &str,
    many: &str,
    id: i32,
) -> Result<bool, LidarrError> {
    let Some(body) = body else {
        return Ok(false);
    };
    if nullable_int(body, single)? == Some(id) {
        return Ok(true);
    }
    Ok(match body.get(many) {
        Some(Value::Array(ids)) => ids.iter().any(|v| v.as_i64() == Some(i64::from(id))),
        _ => false,
    })
}

/// Whether a command, by its name and body, refreshes the album or its artist: a RefreshAlbum
/// naming the album, or a RefreshArtist naming the artist or no artist at all (a refresh of
/// every artist). While the artist is not known yet (`artist_id` 0 or less), every
/// RefreshArtist counts.
pub fn refresh_covers(
    name: Option<&str>,
    body: Option<&Map<String, Value>>,
    album_id: i32,
    artist_id: i32,
) -> Result<bool, LidarrError> {
    Ok(match name {
        Some("RefreshArtist") => {
            artist_id <= 0
                || body_names_id(body, "artistId", "artistIds", artist_id)?
                || body.is_none_or(|b| !b.contains_key("artistId") && !b.contains_key("artistIds"))
        }
        Some("RefreshAlbum") => body_names_id(body, "albumId", "albumIds", album_id)?,
        _ => false,
    })
}

/// Whether `folder` is `path` or a folder above it. `/` and `\` count as one separator,
/// trailing ones are ignored, and so is case, so a folder that may reach the path counts. A
/// blank folder holds nothing.
pub fn folder_holds(folder: &str, path: &str) -> bool {
    if dotnet::is_blank(folder) {
        return false;
    }
    let folder = folder.trim().replace('\\', "/");
    let path = path.trim().replace('\\', "/");
    let folder = folder.trim_end_matches('/');
    let path = path.trim_end_matches('/');
    dotnet::eq_ignore_case(folder, path) || dotnet::starts_with_ignore_case(path, &format!("{folder}/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    pub(crate) fn candidate(
        foreign_id: &str,
        title: &str,
        artist: &str,
        year: Option<i32>,
    ) -> LidarrAlbumCandidate {
        let resource = json!({
            "foreignAlbumId": foreign_id,
            "title": title,
            "releaseDate": year.map(|y| format!("{y}-01-01T00:00:00Z")),
            "artist": { "artistName": artist, "foreignArtistId": "artist-mbid" },
        });
        LidarrAlbumCandidate {
            id: 0,
            foreign_album_id: foreign_id.into(),
            title: title.into(),
            artist: artist.into(),
            year,
            resource: resource.as_object().cloned().unwrap_or_default(),
        }
    }

    #[test]
    fn album_selection_requires_exact_identity_and_uses_year_to_disambiguate() {
        let candidates = [
            candidate("a", "In Rainbows", "Radiohead", Some(2007)),
            candidate("b", "In Rainbows", "Radiohead", Some(2025)),
        ];

        assert_eq!(
            select_best_album(&candidates, "RADIOHEAD", "In-Rainbows", Some(2007))
                .map(|c| c.foreign_album_id.as_str()),
            Some("a")
        );
        assert!(select_best_album(&candidates, "Radiohead", "In Rainbows", None).is_none());
        assert!(select_best_album(&candidates, "Other", "In Rainbows", Some(2007)).is_none());
    }

    #[test]
    fn album_artist_is_compared_as_a_credit() {
        let duo = [candidate(
            "a",
            "Bridge over Troubled Water",
            "Simon & Garfunkel",
            Some(1970),
        )];
        assert!(select_best_album(&duo, "Simon & Garfunkel", "Bridge over Troubled Water", None).is_some());
        // A credit with a guest is the lead artist's album.
        let guest = [candidate(
            "w",
            "Watch the Throne",
            "Jay-Z & Kanye West",
            Some(2011),
        )];
        assert!(select_best_album(&guest, "Jay-Z", "Watch the Throne", None).is_some());
        // An alias is the same artist.
        let alias = [candidate("d", "Donda", "Kanye West", Some(2021))];
        assert!(select_best_album(&alias, "Ye", "Donda", None).is_some());
        // A name inside another name is not.
        let other = [candidate("s", "Sexy Boy", "Airbourne", Some(1998))];
        assert!(select_best_album(&other, "Air", "Sexy Boy", None).is_none());
    }

    /// Two rows of one release group are one album, compared ignoring case.
    #[test]
    fn one_release_group_listed_twice_is_one_album() {
        let candidates = [
            candidate("RG", "In Rainbows", "Radiohead", Some(2007)),
            candidate("rg", "In Rainbows", "Radiohead", Some(2008)),
        ];
        assert_eq!(
            select_best_album(&candidates, "Radiohead", "In Rainbows", None).map(|c| c.year),
            Some(Some(2007))
        );
    }

    #[test]
    fn track_numbers_and_years_parse_as_plain_integers() {
        for (text, expected) in [
            ("3", Some(3)),
            ("1-3", Some(1)),
            ("2/12", Some(2)),
            (" 4 ", Some(4)),
            ("+5", Some(5)),
            ("A1", None),
            ("", None),
            ("-1", None),
            ("2147483648", None),
        ] {
            assert_eq!(parse_track_number(text), expected, "{text:?}");
        }
        for (date, year) in [
            (Some("1998-04-20"), Some(1998)),
            (Some("199"), None),
            (Some(" 199"), Some(199)),
            (Some("19é8"), None),
            (None, None),
        ] {
            let row = json!({ "releaseDate": date, "foreignAlbumId": "x" });
            let parsed = parse_album(row.as_object().expect("an object")).expect("a row");
            assert_eq!(parsed.year, year, "{date:?}");
        }
    }

    /// A value of another kind is an error, rather than reading as missing.
    #[test]
    fn a_value_of_the_wrong_kind_is_an_error() {
        let row = json!({ "id": "7", "title": 5, "size": 1.5, "ok": null });
        let row = row.as_object().expect("an object");
        assert_eq!(
            nullable_int(row, "id"),
            Err(LidarrError::InvalidOperation(
                "An element of type 'String' cannot be converted to a 'System.Int32'.".into()
            ))
        );
        assert!(nullable_str(row, "title").is_err());
        assert!(nullable_long(row, "size").is_err());
        assert_eq!(nullable_int(row, "ok"), Ok(None));
        assert_eq!(int_or_zero(row, "missing"), Ok(0));
    }

    // Completion reads the album's statistics, not alternate release rows (the HTTP half is in
    // octo's client tests).
    #[test]
    fn an_album_is_complete_once_every_track_has_a_file() {
        let state = |count, files| LidarrAlbumImportState {
            tracks: Vec::new(),
            track_count: count,
            track_file_count: files,
        };
        assert!(state(1, 1).is_complete());
        assert!(!state(2, 1).is_complete());
        assert!(!state(0, 0).is_complete());
    }

    fn object_of(value: Value) -> Map<String, Value> {
        value.as_object().cloned().expect("an object")
    }

    fn at(text: &str) -> DateTime<Utc> {
        datetime::parse_utc(text).expect("a date")
    }

    #[test]
    fn an_artist_reads_its_path_monitoring_added_time_and_file_count() {
        let row = object_of(json!({
            "id": 5, "artistName": "Massive Attack", "foreignArtistId": "ma",
            "path": "/data/music/Massive Attack", "monitored": false, "monitorNewItems": "none",
            "added": "2026-10-29T12:00:00.1234567Z", "statistics": { "trackFileCount": 3 },
        }));
        let artist = parse_artist(&row).expect("an artist");
        assert_eq!(artist.id, 5);
        assert_eq!(artist.foreign_artist_id, "ma");
        assert_eq!(artist.path.as_deref(), Some("/data/music/Massive Attack"));
        assert!(!artist.monitored);
        assert_eq!(artist.monitor_new_items.as_deref(), Some("none"));
        assert_eq!(artist.added, Some(at("2026-10-29T12:00:00.1234567Z")));
        assert_eq!(artist.track_file_count, Some(3));
        assert_eq!(artist.resource, row);
        // An older Lidarr names the MusicBrainz id `mbId`.
        let older = parse_artist(&object_of(json!({ "id": 2, "mbId": "old" }))).expect("an artist");
        assert_eq!(older.foreign_artist_id, "old");
        assert_eq!(older.track_file_count, None);
        assert!(parse_artist(&object_of(json!({ "added": "soon" }))).is_err());
        assert_eq!(
            parse_artist_lookup(&object_of(
                json!({ "foreignArtistId": "ma", "folder": "Massive Attack" })
            )),
            Ok(("ma".to_string(), Some("Massive Attack".to_string())))
        );
        assert_eq!(
            parse_artist_lookup(&object_of(json!({ "foreignArtistId": "ma", "folder": " " }))),
            Ok(("ma".to_string(), None))
        );
    }

    #[test]
    fn an_album_reads_its_releases_and_pins_one_of_them() {
        let row = object_of(json!({
            "id": 7, "title": "Mezzanine", "foreignAlbumId": "rg", "artist": { "id": 5 },
            "monitored": false, "anyReleaseOk": true,
            "releases": [
                { "id": 71, "foreignReleaseId": "uk", "title": "Mezzanine", "monitored": false },
                { "id": 70, "foreignReleaseId": "us", "title": "Mezzanine", "monitored": true },
                "not a release",
            ],
            "statistics": { "trackFileCount": 0 },
        }));
        let album = parse_library_album(&row).expect("an album");
        assert_eq!((album.id, album.artist_id, album.monitored), (7, 5, false));
        assert_eq!(album.track_file_count, Some(0));
        assert_eq!(album.releases.len(), 2);
        assert_eq!(album.release("UK").map(|r| r.id), Some(71));

        let saved = album.release_choice();
        assert_eq!(
            saved,
            LidarrReleaseChoice {
                any_release_ok: true,
                releases: vec![
                    LidarrReleaseMonitored {
                        id: 70,
                        monitored: true
                    },
                    LidarrReleaseMonitored {
                        id: 71,
                        monitored: false
                    },
                ],
            }
        );
        let pin = album.pinned_to(71).expect("a listed release");
        assert!(!pin.any_release_ok);
        assert_eq!(
            pin.releases
                .iter()
                .map(|r| (r.id, r.monitored))
                .collect::<Vec<_>>(),
            [(70, false), (71, true)]
        );
        assert_eq!(album.pinned_to(99), None);
        // Saved in a pending entry as Octo's state files are written.
        let text = serde_json::to_string(&saved).expect("written");
        assert_eq!(
            text,
            r#"{"AnyReleaseOk":true,"Releases":[{"Id":70,"Monitored":true},{"Id":71,"Monitored":false}]}"#
        );
        assert_eq!(
            serde_json::from_str::<LidarrReleaseChoice>(&text).expect("read"),
            saved
        );
        // An album row with its artist id at the top level; without `anyReleaseOk`, Lidarr's
        // default.
        let bare = parse_library_album(&object_of(json!({ "id": 8, "artistId": 6 }))).expect("an album");
        assert_eq!((bare.artist_id, bare.any_release_ok), (6, true));
        assert!(parse_library_album(&object_of(json!({ "releases": "none" }))).is_err());
    }

    #[test]
    fn queue_items_and_track_files_read_as_lidarr_writes_them() {
        let item = parse_queue_item(&object_of(json!({
            "id": 12, "artistId": 5, "albumId": 7, "downloadId": "ABC", "downloadClient": "SAB Music",
            "title": "Massive Attack - Mezzanine", "status": "completed",
            "trackedDownloadState": "importPending", "trackedDownloadStatus": "ok",
            "size": 300000000.0, "sizeleft": 0.4, "outputPath": "/data/downloads/ABC", "protocol": "usenet",
        })))
        .expect("an item");
        assert_eq!(item.download_id.as_deref(), Some("ABC"));
        assert_eq!(item.download_client.as_deref(), Some("SAB Music"));
        assert_eq!(item.tracked_download_state.as_deref(), Some("importPending"));
        assert_eq!(
            (item.size_bytes, item.size_left_bytes),
            (Some(300000000), Some(0))
        );
        // A pending release has no download id and no client.
        let pending = parse_queue_item(&object_of(json!({
            "id": 2147000000, "albumId": 7, "downloadId": "", "status": "delay",
        })))
        .expect("an item");
        assert_eq!((pending.download_id, pending.download_client), (None, None));
        assert!(parse_queue_item(&object_of(json!({ "sizeleft": "0" }))).is_err());

        let file = parse_track_file(&object_of(json!({
            "id": 100, "artistId": 5, "albumId": 7, "path": "/data/music/a.flac", "size": 1000,
            "quality": { "quality": { "name": "FLAC" } },
        })))
        .expect("a file");
        assert_eq!(
            file,
            LidarrTrackFile {
                id: 100,
                artist_id: 5,
                album_id: 7,
                path: Some("/data/music/a.flac".into()),
                size_bytes: 1000,
                quality: Some("FLAC".into()),
            }
        );
    }

    #[test]
    fn history_rows_read_events_by_name_or_number_and_details_in_any_case() {
        let row = parse_history(&object_of(json!({
            "id": 3, "albumId": 7, "artistId": 5, "trackId": 1, "eventType": "trackFileImported",
            "date": "2026-10-29T12:00:01Z", "downloadId": "ABC", "sourceTitle": "01 - Angel.flac",
            "data": {
                "fileId": "100", "droppedPath": "/data/downloads/ABC/01 - Angel.flac",
                "importedPath": "/data/music/Massive Attack/Mezzanine/01 - Angel.flac",
                "DownloadClient": "Sabnzbd", "downloadClientName": "SAB Music", "releaseGroup": null,
            },
        })))
        .expect("a row");
        assert_eq!(row.event, LidarrHistoryEvent::TrackFileImported);
        assert_eq!(row.date, at("2026-10-29T12:00:01Z"));
        assert_eq!(row.file_id(), Some(100));
        assert_eq!(row.download_client(), Some("Sabnzbd"));
        assert_eq!(row.dropped_path(), Some("/data/downloads/ABC/01 - Angel.flac"));
        assert_eq!(
            row.imported_path(),
            Some("/data/music/Massive Attack/Mezzanine/01 - Angel.flac")
        );
        assert!(!row.data.contains_key("releaseGroup"));
        assert_eq!(
            (row.guid(), row.release_source(), row.reason()),
            (None, None, None)
        );

        // A folder import names no client.
        let folder = parse_history(&object_of(json!({
            "eventType": 3, "date": "2026-10-29T12:00:02Z", "downloadId": "ABC",
            "data": { "fileId": "101", "downloadClient": "" },
        })))
        .expect("a row");
        assert_eq!(folder.event, LidarrHistoryEvent::TrackFileImported);
        assert_eq!(folder.download_client(), None);

        let grab = parse_history(&object_of(json!({
            "eventType": "grabbed", "date": "2026-10-29T12:00:00Z",
            "data": { "releaseSource": "UserInvokedSearch", "guid": "g-1" },
        })))
        .expect("a row");
        assert_eq!(
            (grab.event, grab.release_source(), grab.guid()),
            (
                LidarrHistoryEvent::Grabbed,
                Some("UserInvokedSearch"),
                Some("g-1")
            )
        );
        let deleted = parse_history(&object_of(json!({
            "eventType": "trackFileDeleted", "date": "2026-10-29T12:00:03Z",
            "sourceTitle": "/data/music/a.flac", "data": { "reason": "Upgrade" },
        })))
        .expect("a row");
        assert_eq!(
            (deleted.reason(), deleted.source_title.as_str()),
            (Some("Upgrade"), "/data/music/a.flac")
        );

        let later = parse_history(&object_of(
            json!({ "eventType": "somethingNew", "date": "2026-10-29T12:00:00Z" }),
        ))
        .expect("a row");
        assert_eq!(later.event, LidarrHistoryEvent::Unknown);
        assert!(parse_history(&object_of(json!({ "eventType": "grabbed" }))).is_err());
        assert!(
            parse_history(&object_of(
                json!({ "eventType": true, "date": "2026-10-29T12:00:00Z" })
            ))
            .is_err()
        );
        assert!(
            parse_history(&object_of(
                json!({ "date": "2026-10-29T12:00:00Z", "data": { "fileId": 100 } })
            ))
            .is_err()
        );

        for event in LidarrHistoryEvent::ALL {
            assert_eq!(LidarrHistoryEvent::read(&json!(event.name())), Ok(event));
            assert_eq!(LidarrHistoryEvent::read(&json!(event.code())), Ok(event));
        }
        let ending: Vec<i32> = LidarrHistoryEvent::ALL
            .into_iter()
            .filter(|e| e.ends_download())
            .map(LidarrHistoryEvent::code)
            .collect();
        assert_eq!(ending, [4, 7, 8, 10]);

        let page = parse_history_page(&object_of(json!({
            "page": 1, "pageSize": 1, "totalRecords": 4,
            "records": [{ "id": 9, "eventType": "grabbed", "date": "2026-10-29T12:00:00Z" }],
        })))
        .expect("a page");
        assert_eq!((page.total_records, page.records.len()), (4, 1));
        assert!(parse_history_page(&object_of(json!({ "records": {} }))).is_err());
    }

    #[test]
    fn commands_read_their_times_and_what_they_name() {
        let command = parse_command(&object_of(json!({
            "id": 12, "name": "RescanFolders", "status": "started", "trigger": "unspecified",
            "queued": "2026-10-29T12:00:00Z", "started": "2026-10-29T12:00:01Z", "ended": null,
            "body": { "folders": ["/data/music/Massive Attack"], "artistIds": [5] },
        })))
        .expect("a command");
        assert!(command.is_active());
        assert_eq!(command.started, Some(at("2026-10-29T12:00:01Z")));
        assert_eq!(command.ended, None);
        assert_eq!(command.artist_ids(), [5]);
        let search = parse_command(&object_of(json!({
            "id": 13, "name": "AlbumSearch", "status": "completed", "body": { "albumIds": [7, "8", 9] },
        })))
        .expect("a command");
        assert!(!search.is_active());
        assert_eq!(search.album_ids(), [7, 9]);
        assert!(parse_command(&object_of(json!({ "started": 5 }))).is_err());
    }

    /// The same matching the wait for an album's track list uses.
    #[test]
    fn a_refresh_covers_its_album_its_artist_or_every_artist() {
        let body = |value: Value| object_of(value);
        let album = body(json!({ "albumId": 12 }));
        let albums = body(json!({ "albumIds": [11, 12] }));
        let artist = body(json!({ "artistIds": [7] }));
        let other = body(json!({ "artistId": 8 }));
        let everyone = body(json!({ "isNewArtist": false }));
        let cases = [
            (Some("RefreshAlbum"), Some(&album), 7, true),
            (Some("RefreshAlbum"), Some(&albums), 7, true),
            (Some("RefreshAlbum"), Some(&other), 7, false),
            (Some("RefreshAlbum"), None, 7, false),
            (Some("RefreshArtist"), Some(&artist), 7, true),
            (Some("RefreshArtist"), Some(&other), 7, false),
            (Some("RefreshArtist"), Some(&other), 0, true),
            (Some("RefreshArtist"), Some(&everyone), 7, true),
            (Some("RefreshArtist"), None, 7, true),
            (Some("RescanFolders"), Some(&album), 7, false),
            (None, Some(&album), 7, false),
        ];
        for (name, body, artist_id, expected) in cases {
            assert_eq!(
                refresh_covers(name, body, 12, artist_id),
                Ok(expected),
                "{name:?} {body:?}"
            );
        }
        assert!(
            refresh_covers(
                Some("RefreshAlbum"),
                Some(&body(json!({ "albumId": "12" }))),
                12,
                7
            )
            .is_err()
        );
        let command = parse_command(&object_of(
            json!({ "name": "RefreshAlbum", "body": { "albumIds": [12] } }),
        ))
        .expect("a command");
        assert_eq!(command.refreshes(12, 7), Ok(true));
        assert_eq!(command.refreshes(13, 7), Ok(false));
    }

    #[test]
    fn a_rescan_reaches_the_artist_by_its_folder_or_its_id() {
        let rescan = |body: Value| {
            parse_command(&object_of(
                json!({ "name": "RescanFolders", "status": "queued", "body": body }),
            ))
            .expect("a command")
        };
        let path = Some("/data/music/Massive Attack");
        assert!(rescan(json!({ "folders": null })).rescans_artist(5, path));
        assert!(rescan(json!({ "folders": [] })).rescans_artist(5, path));
        assert!(rescan(json!({ "folders": ["/data/music"] })).rescans_artist(5, path));
        assert!(rescan(json!({ "folders": ["/data/music/massive attack/"] })).rescans_artist(5, path));
        assert!(rescan(json!({ "folders": ["/data/other"], "artistIds": [5] })).rescans_artist(5, path));
        assert!(!rescan(json!({ "folders": ["/data/music/Massive Attack 2"] })).rescans_artist(5, path));
        assert!(!rescan(json!({ "folders": ["/data/other"], "artistIds": [6] })).rescans_artist(5, path));
        assert!(!rescan(json!({ "folders": ["/data/other"] })).rescans_artist(5, None));
        let search =
            parse_command(&object_of(json!({ "name": "AlbumSearch", "body": {} }))).expect("a command");
        assert!(!search.rescans_artist(5, path));
    }

    #[test]
    fn a_folder_holds_itself_and_what_lies_below_it() {
        assert!(folder_holds("/data/music", "/data/music/Artist/01.flac"));
        assert!(folder_holds("/data/music/", "/data/music"));
        assert!(folder_holds("D:\\Music", "d:/music/Artist"));
        assert!(folder_holds("/", "/data"));
        assert!(!folder_holds("/data/music", "/data/musicals/Artist"));
        assert!(!folder_holds("/data/music/Artist", "/data/music"));
        assert!(!folder_holds(" ", "/data/music"));
    }

    #[test]
    fn health_checks_and_manual_import_items_read_what_octo_acts_on() {
        let check = parse_health_check(&object_of(json!({
            "source": "IndexerSearchCheck", "type": "warning",
            "message": "All search-capable indexers are temporarily unavailable due to recent indexer errors",
            "wikiUrl": "https://wiki.servarr.com/lidarr/system#indexers-are-unavailable-due-to-failures",
        })))
        .expect("a check");
        assert_eq!(
            (check.source.as_str(), check.kind.as_str()),
            ("IndexerSearchCheck", "warning")
        );
        assert!(
            check
                .wiki_url
                .as_deref()
                .is_some_and(|url| url.ends_with("#indexers-are-unavailable-due-to-failures"))
        );

        let item = parse_manual_import(&object_of(json!({
            "path": "/data/downloads/ABC/01 - Angel.flac", "artist": { "id": 5 }, "album": { "id": 7 },
            "albumReleaseId": 70, "tracks": [{ "id": 1 }], "quality": { "quality": { "name": "FLAC" } },
            "indexerFlags": 0, "downloadId": "ABC",
            "rejections": [{ "type": "permanent", "reason": "Not an upgrade" }, { "type": "temporary", "reason": "" }],
        })))
        .expect("an item");
        assert_eq!(
            (
                item.artist_id,
                item.album_id,
                item.album_release_id,
                item.track_ids.clone()
            ),
            (5, 7, 70, vec![1])
        );
        assert_eq!(item.quality, json!({ "quality": { "name": "FLAC" } }));
        assert_eq!(item.download_id.as_deref(), Some("ABC"));
        assert_eq!(item.rejections, ["Not an upgrade", "temporary"]);
        let by_id = parse_manual_import(&object_of(json!({
            "path": "/x.flac", "artistId": 5, "albumId": 8, "trackIds": [2, 3],
        })))
        .expect("an item");
        assert_eq!(
            (by_id.album_id, by_id.track_ids, by_id.rejections.len()),
            (8, vec![2, 3], 0)
        );
        assert_eq!(
            parse_root_folder(&object_of(
                json!({ "id": 1, "path": "/data/music", "freeSpace": 1 })
            )),
            Ok(LidarrRootFolder {
                id: 1,
                path: "/data/music".into(),
            })
        );
    }
}
