//! The Lidarr HTTP client. Its records and the JSON readers are
//! `octo_core::lidarr::lidarr_client`.
//!
//! The calls take no cancellation token: a caller that must stop early drops the future, and the
//! track fetcher races its token against each call.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use octo_core::common::dotnet;
use octo_core::lidarr::lidarr_client::{
    LidarrAdded, LidarrAlbum, LidarrArtist, LidarrCommand, LidarrDeletion, LidarrHealthCheck,
    LidarrHistoryEvent, LidarrHistoryRecord, LidarrManualImportItem, LidarrQueueItem, LidarrReleaseChoice,
    LidarrRootFolder, LidarrTrackFile, PICK_GONE_TEXT, PICK_REFUSED_TEXT, child, int_or_zero, map_options,
    nullable_bool, nullable_date, nullable_int, nullable_long, nullable_str, object, parse_album,
    parse_artist, parse_artist_lookup, parse_command, parse_health_check, parse_history, parse_history_page,
    parse_library_album, parse_manual_import, parse_queue_item, parse_release, parse_root_folder,
    parse_track_file, parse_track_number, refresh_covers, same_release, select_best_album, str_or_empty,
    value_str,
};
use octo_core::lidarr::{
    LidarrAlbumCandidate, LidarrAlbumImportState, LidarrAlbumState, LidarrError, LidarrImportedTrack,
    LidarrOptions, LidarrRelease, LidarrReleasePick,
};
use octo_core::settings::{LidarrSettings, SettingsStore};
use reqwest::Method;
use serde_json::{Map, Value, json};
use tracing::{info, warn};

use crate::services::http_client_factory;

type JsonObject = Map<String, Value>;

/// Told the path of each request Lidarr answered with a success.
pub type LidarrAnswered = Arc<dyn Fn(&str) + Send + Sync>;

/// Small, purpose-built client for the Lidarr v1 endpoints Octo needs.
pub struct LidarrClient {
    http: reqwest::Client,
    /// The live settings: the address and key are read at every request.
    settings: Arc<SettingsStore>,
    /// Milliseconds between looks while waiting for an album's track list.
    metadata_poll: AtomicU64,
    /// Told of every request Lidarr answered: Lidarr answering is news of its health.
    answered: parking_lot::Mutex<Option<LidarrAnswered>>,
}

/// What a look at an album's track list found.
enum TrackListWait {
    /// Lidarr lists the album's tracks.
    Ready,
    /// No tracks yet, and a refresh that brings them is queued or running.
    Refreshing,
    /// No tracks, and nothing is refreshing them.
    Idle,
}

impl LidarrClient {
    /// The client's timeout: 20 seconds.
    pub const TIMEOUT: Duration = Duration::from_secs(20);

    pub fn new(settings: Arc<SettingsStore>) -> Self {
        // A plain client, with its timeout set to 20 s.
        let http = reqwest::Client::builder()
            .no_gzip()
            .no_deflate()
            .timeout(Self::TIMEOUT)
            .build()
            .expect("the Lidarr HTTP client builds");
        LidarrClient {
            http,
            settings,
            metadata_poll: AtomicU64::new(2000),
            answered: parking_lot::Mutex::new(None),
        }
    }

    /// Tells `hook` the path of every request Lidarr answers with a success from now on.
    pub fn on_answer(&self, hook: LidarrAnswered) {
        *self.answered.lock() = Some(hook);
    }

    pub async fn is_reachable(&self) -> bool {
        match self.request(Method::GET, "/ping", None) {
            Ok(request) => self.send(request).await.is_ok(),
            Err(_) => false,
        }
    }

    /// Checks an address and key that are not saved yet, and answers the choices that server
    /// offers.
    pub async fn test_connection(&self, base_url: &str, api_key: &str) -> Result<LidarrOptions, LidarrError> {
        if dotnet::is_blank(base_url) || dotnet::is_blank(api_key) {
            return Err(LidarrError::InvalidOperation(
                "Lidarr URL and API key are required.".into(),
            ));
        }
        self.send(Self::request_to(
            Method::GET,
            "/api/v1/system/status",
            base_url,
            api_key,
            None,
        ))
        .await?;

        let (roots, quality, metadata) = tokio::join!(
            self.get_array_from("/api/v1/rootfolder", base_url, api_key),
            self.get_array_from("/api/v1/qualityprofile", base_url, api_key),
            self.get_array_from("/api/v1/metadataprofile", base_url, api_key),
        );
        map_options(&roots?, &quality?, &metadata?)
    }

    pub async fn get_options(&self) -> Result<LidarrOptions, LidarrError> {
        let (roots, quality, metadata) = tokio::join!(
            self.get_array("/api/v1/rootfolder"),
            self.get_array("/api/v1/qualityprofile"),
            self.get_array("/api/v1/metadataprofile"),
        );
        map_options(&roots?, &quality?, &metadata?)
    }

    pub async fn resolve_album(
        &self,
        artist: &str,
        album: &str,
        year: Option<i32>,
    ) -> Result<LidarrAlbumCandidate, LidarrError> {
        let term = dotnet::escape_data_string(&format!("{artist} {album}"));
        let rows = self
            .get_array(&format!("/api/v1/album/lookup?term={term}"))
            .await?;
        let candidates = rows
            .iter()
            .map(parse_album)
            .filter(|x| x.as_ref().map_or(true, |c| !c.foreign_album_id.is_empty()))
            .collect::<Result<Vec<_>, _>>()?;
        select_best_album(&candidates, artist, album, year)
            .cloned()
            .ok_or_else(|| {
                LidarrError::InvalidOperation(format!(
                    "Lidarr could not unambiguously match '{artist} - {album}'."
                ))
            })
    }

    /// The candidate for a MusicBrainz release group id, or None when Lidarr does not know it.
    pub async fn resolve_album_by_foreign_id(
        &self,
        foreign_album_id: &str,
    ) -> Result<Option<LidarrAlbumCandidate>, LidarrError> {
        let term = dotnet::escape_data_string(&format!("lidarr:{foreign_album_id}"));
        let rows = self
            .get_array(&format!("/api/v1/album/lookup?term={term}"))
            .await?;
        // FirstOrDefault: rows are read only until the first match.
        for row in &rows {
            let candidate = parse_album(row)?;
            if dotnet::eq_ignore_case(&candidate.foreign_album_id, foreign_album_id) {
                return Ok(Some(candidate));
            }
        }
        Ok(None)
    }

    /// The album as Lidarr has it now, or None when Lidarr does not have it yet.
    pub async fn find_album(&self, foreign_album_id: &str) -> Result<Option<LidarrAlbumState>, LidarrError> {
        let existing = self
            .get_array(&format!(
                "/api/v1/album?foreignAlbumId={}",
                dotnet::escape_data_string(foreign_album_id)
            ))
            .await?;
        match existing.first() {
            None => Ok(None),
            Some(first) => Ok(Some(LidarrAlbumState {
                id: int_or_zero(first, "id")?,
                monitored: nullable_bool(first, "monitored")?.unwrap_or(false),
            })),
        }
    }

    /// Delete one track file, from Lidarr and from disk (into Lidarr's recycle bin when it has one).
    pub async fn delete_track_file(&self, track_file_id: i32) -> Result<(), LidarrError> {
        let request = self.request(
            Method::DELETE,
            &format!("/api/v1/trackfile/{track_file_id}"),
            None,
        )?;
        self.send(request).await?;
        Ok(())
    }

    /// How long an interactive search may take: Lidarr asks every indexer before it answers.
    pub const RELEASE_SEARCH_TIMEOUT: Duration = Duration::from_secs(120);

    /// Every release the indexers offer for an album Lidarr has, as its interactive search
    /// lists them, best first by Lidarr's own order. Nothing is grabbed.
    pub async fn list_releases(&self, album_id: i32) -> Result<Vec<LidarrRelease>, LidarrError> {
        let mut request = self.request(Method::GET, &format!("/api/v1/release?albumId={album_id}"), None)?;
        request.timeout = Some(Self::RELEASE_SEARCH_TIMEOUT);
        let rows = match self.read_json(request).await? {
            Value::Array(items) => items
                .into_iter()
                .filter_map(|item| match item {
                    Value::Object(o) => Some(o),
                    _ => None,
                })
                .collect(),
            // `as JsonArray` on anything else: no rows.
            _ => Vec::new(),
        };
        let mut releases = Vec::new();
        for row in &rows {
            let release = parse_release(row)?;
            if !release.guid.is_empty() {
                releases.push(release);
            }
        }
        Ok(releases)
    }

    /// Asks Lidarr to download one release an interactive search listed. Lidarr grabs it within
    /// the call: no command is queued, so the grab's history row carries the release's guid.
    pub async fn grab_release(&self, release: &LidarrReleasePick) -> Result<(), LidarrError> {
        self.send_json(
            Method::POST,
            "/api/v1/release",
            json!({ "guid": release.guid, "indexerId": release.indexer_id })
                .as_object()
                .expect("an object literal"),
        )
        .await?;
        Ok(())
    }

    /// Grabs a picked release and answers the guid it was grabbed under. Lidarr keeps an
    /// interactive search's releases only for a while, and a pick made before that ran out is
    /// refused by its guid; the album's releases are then listed once more and the same
    /// release, by its indexer, title and size, is grabbed under its new guid.
    pub async fn grab_picked(&self, album_id: i32, pick: &LidarrReleasePick) -> Result<String, LidarrError> {
        self.grab_picked_saving(album_id, pick, |_| {}).await
    }

    /// The same, telling `before` each guid right before the grab under it is sent, so the caller
    /// can save it first.
    pub async fn grab_picked_saving(
        &self,
        album_id: i32,
        pick: &LidarrReleasePick,
        before: impl Fn(&str),
    ) -> Result<String, LidarrError> {
        before(&pick.guid);
        match self.grab_release(pick).await {
            Ok(()) => return Ok(pick.guid.clone()),
            // Most likely forgotten; found again below, or refused in plain words.
            Err(LidarrError::Http(_)) => {}
            Err(e) => return Err(e),
        }
        let again = self
            .list_releases(album_id)
            .await?
            .into_iter()
            .find(|release| same_release(release, pick))
            .ok_or_else(|| LidarrError::InvalidOperation(PICK_GONE_TEXT.into()))?;
        let fresh = LidarrReleasePick {
            guid: again.guid,
            indexer_id: again.indexer_id,
            title: Some(again.title),
            size: again.size,
        };
        before(&fresh.guid);
        match self.grab_release(&fresh).await {
            Ok(()) => Ok(fresh.guid),
            Err(LidarrError::Http(_)) => Err(LidarrError::InvalidOperation(PICK_REFUSED_TEXT.into())),
            Err(e) => Err(e),
        }
    }

    /// How often [`LidarrClient::wait_for_tracks`] looks again.
    pub fn set_metadata_poll(&self, poll: Duration) {
        self.metadata_poll.store(
            poll.as_millis().min(u128::from(u64::MAX)) as u64,
            Ordering::Relaxed,
        );
    }

    /// Waits until Lidarr knows the album's tracks, so an AlbumSearch has something to match a
    /// release against, and says whether it does. A just-added artist or album gets its track
    /// list from Lidarr's own refresh: while a RefreshArtist or RefreshAlbum for it is queued or
    /// running, this waits for it. When none is running and the list is still empty, it asks for
    /// one RefreshAlbum and waits for that. False when that refresh ended without tracks, once
    /// [`track_list_wait`] has passed, or when Lidarr could not be asked.
    pub async fn wait_for_tracks(&self, album_id: i32, artist_id: i32) -> Result<bool, LidarrError> {
        let settings = self.require_settings(false)?;
        Ok(self.track_list(album_id, artist_id, &settings).await)
    }

    async fn track_list(&self, album_id: i32, artist_id: i32, settings: &LidarrSettings) -> bool {
        let deadline = Instant::now() + track_list_wait(settings);
        let poll = Duration::from_millis(self.metadata_poll.load(Ordering::Relaxed));
        let mut refresh_asked = false;
        loop {
            match self.waiting_on_refresh(album_id, artist_id).await {
                Ok(TrackListWait::Ready) => return true,
                Ok(TrackListWait::Refreshing) => {}
                Ok(TrackListWait::Idle) if !refresh_asked => {
                    refresh_asked = true;
                    let command =
                        json!({ "name": "RefreshAlbum", "albumId": album_id, "albumIds": [album_id] });
                    if let Err(e) = self
                        .send_json(
                            Method::POST,
                            "/api/v1/command",
                            command.as_object().expect("an object literal"),
                        )
                        .await
                    {
                        warn!("Could not ask Lidarr to refresh album {album_id}: {e}");
                        return false;
                    }
                    info!("Lidarr has no tracks for album {album_id} yet; asked it to refresh the album");
                }
                Ok(TrackListWait::Idle) => {
                    warn!("Lidarr refreshed album {album_id} and still lists no tracks");
                    return false;
                }
                Err(e) => {
                    warn!("Could not check Lidarr's tracks for album {album_id}: {e}");
                    return false;
                }
            }
            if Instant::now() + poll > deadline {
                warn!("Lidarr listed no tracks for album {album_id} in time");
                return false;
            }
            tokio::time::sleep(poll).await;
        }
    }

    /// Whether the album has its track list, and if not, whether a refresh that brings it is
    /// queued or running.
    async fn waiting_on_refresh(&self, album_id: i32, artist_id: i32) -> Result<TrackListWait, LidarrError> {
        if !self
            .get_array(&format!("/api/v1/track?albumId={album_id}"))
            .await?
            .is_empty()
        {
            return Ok(TrackListWait::Ready);
        }
        for command in self.get_array("/api/v1/command").await? {
            let name = nullable_str(&command, "name")?;
            let status = nullable_str(&command, "status")?;
            if !matches!(status.as_deref(), Some("queued" | "started")) {
                continue;
            }
            // A refresh of every artist has no id and covers this one too.
            if refresh_covers(name.as_deref(), object(&command, "body"), album_id, artist_id)? {
                return Ok(TrackListWait::Refreshing);
            }
        }
        // A refresh may have finished between the two reads.
        if !self
            .get_array(&format!("/api/v1/track?albumId={album_id}"))
            .await?
            .is_empty()
        {
            return Ok(TrackListWait::Ready);
        }
        Ok(TrackListWait::Idle)
    }

    pub async fn get_album_tracks(&self, album_id: i32) -> Result<Vec<LidarrImportedTrack>, LidarrError> {
        // Lidarr deliberately omits the nested trackFile resource from album track
        // list responses, so load the album's files separately and join by id.
        let tracks_path = format!("/api/v1/track?albumId={album_id}");
        let files_path = format!("/api/v1/trackFile?albumId={album_id}");
        let (tracks, files) = tokio::join!(self.get_array(&tracks_path), self.get_array(&files_path));
        let tracks = tracks?;
        let files = files?;
        let mut by_id: std::collections::HashMap<i32, &JsonObject> = std::collections::HashMap::new();
        for file in &files {
            let id = int_or_zero(file, "id")?;
            if by_id.insert(id, file).is_some() {
                return Err(LidarrError::Argument(format!(
                    "An item with the same key has already been added. Key: {id}"
                )));
            }
        }

        tracks
            .iter()
            .map(|row| {
                let track_file_id = nullable_int(row, "trackFileId")?.unwrap_or(0);
                let file = by_id.get(&track_file_id).copied();
                let artist = object(row, "artist");
                let quality = match file.and_then(|f| f.get("quality")).filter(|v| !v.is_null()) {
                    None => None,
                    Some(q) => match child(q, "quality")? {
                        None => None,
                        Some(inner) => child(inner, "name")?
                            .map(value_str)
                            .transpose()?
                            .map(str::to_string),
                    },
                };
                Ok(LidarrImportedTrack {
                    id: int_or_zero(row, "id")?,
                    title: str_or_empty(row, "title")?,
                    track_number: parse_track_number(&str_or_empty(row, "trackNumber")?),
                    duration_seconds: nullable_int(row, "duration")?.map(|ms| ms / 1000),
                    has_file: nullable_bool(row, "hasFile")?.unwrap_or(false),
                    path: match file {
                        Some(f) => nullable_str(f, "path")?,
                        None => None,
                    },
                    size_bytes: match file {
                        Some(f) => nullable_long(f, "size")?.unwrap_or(0),
                        None => 0,
                    },
                    artist: match artist {
                        Some(a) => nullable_str(a, "artistName")?,
                        None => None,
                    },
                    track_file_id: if file.is_some() { track_file_id } else { 0 },
                    quality,
                    foreign_recording_id: nullable_str(row, "foreignRecordingId")?,
                })
            })
            .collect()
    }

    pub async fn get_album_import_state(&self, album_id: i32) -> Result<LidarrAlbumImportState, LidarrError> {
        let album_path = format!("/api/v1/album/{album_id}");
        let (album, tracks) = tokio::join!(self.get_object(&album_path), self.get_album_tracks(album_id));
        let album = album?;
        let tracks = tracks?;
        let statistics = object(&album, "statistics");
        let (track_count, track_file_count) = match statistics {
            None => (
                tracks.len() as i32,
                tracks.iter().filter(|t| t.has_file).count() as i32,
            ),
            Some(s) => (int_or_zero(s, "trackCount")?, int_or_zero(s, "trackFileCount")?),
        };
        Ok(LidarrAlbumImportState {
            tracks,
            track_count,
            track_file_count,
        })
    }

    // ---- What Lidarr holds -------------------------------------------------------------------

    /// Every artist Lidarr has.
    pub async fn artists(&self) -> Result<Vec<LidarrArtist>, LidarrError> {
        self.get_array("/api/v1/artist")
            .await?
            .iter()
            .map(parse_artist)
            .collect()
    }

    /// The artist Lidarr has under a MusicBrainz artist id (compared ignoring case), or None.
    pub async fn find_artist(&self, foreign_artist_id: &str) -> Result<Option<LidarrArtist>, LidarrError> {
        Ok(self
            .artists()
            .await?
            .into_iter()
            .find(|artist| dotnet::eq_ignore_case(&artist.foreign_artist_id, foreign_artist_id)))
    }

    /// One artist by Lidarr's id, or None when Lidarr no longer has it.
    pub async fn artist(&self, artist_id: i32) -> Result<Option<LidarrArtist>, LidarrError> {
        self.get_object_or_none(&format!("/api/v1/artist/{artist_id}"))
            .await?
            .as_ref()
            .map(parse_artist)
            .transpose()
    }

    /// The folder Lidarr would give an artist it does not have yet, relative to the root
    /// folder: the `folder` of the artist's lookup row. None when the lookup does not find the
    /// artist or names no folder.
    pub async fn artist_folder(&self, foreign_artist_id: &str) -> Result<Option<String>, LidarrError> {
        let term = dotnet::escape_data_string(&format!("lidarr:{foreign_artist_id}"));
        for row in self
            .get_array(&format!("/api/v1/artist/lookup?term={term}"))
            .await?
        {
            let (id, folder) = parse_artist_lookup(&row)?;
            if dotnet::eq_ignore_case(&id, foreign_artist_id) {
                return Ok(folder);
            }
        }
        Ok(None)
    }

    /// One album by Lidarr's id, or None when Lidarr no longer has it.
    pub async fn album(&self, album_id: i32) -> Result<Option<LidarrAlbum>, LidarrError> {
        self.get_object_or_none(&format!("/api/v1/album/{album_id}"))
            .await?
            .as_ref()
            .map(parse_library_album)
            .transpose()
    }

    /// The album Lidarr has under a MusicBrainz release group id, or None.
    pub async fn album_by_foreign_id(
        &self,
        foreign_album_id: &str,
    ) -> Result<Option<LidarrAlbum>, LidarrError> {
        let rows = self
            .get_array(&format!(
                "/api/v1/album?foreignAlbumId={}",
                dotnet::escape_data_string(foreign_album_id)
            ))
            .await?;
        rows.first().map(parse_library_album).transpose()
    }

    /// Every album Lidarr lists for an artist, the ones its metadata profile added included.
    pub async fn artist_albums(&self, artist_id: i32) -> Result<Vec<LidarrAlbum>, LidarrError> {
        self.get_array(&format!("/api/v1/album?artistId={artist_id}"))
            .await?
            .iter()
            .map(parse_library_album)
            .collect()
    }

    /// Every track file of an artist, on any of its albums, as Lidarr's database holds them at
    /// the call.
    pub async fn artist_track_files(&self, artist_id: i32) -> Result<Vec<LidarrTrackFile>, LidarrError> {
        self.get_array(&format!("/api/v1/trackfile?artistId={artist_id}"))
            .await?
            .iter()
            .map(parse_track_file)
            .collect()
    }

    /// One track file record by Lidarr's id, or None when Lidarr no longer has it.
    pub async fn track_file(&self, track_file_id: i32) -> Result<Option<LidarrTrackFile>, LidarrError> {
        self.get_object_or_none(&format!("/api/v1/trackfile/{track_file_id}"))
            .await?
            .as_ref()
            .map(parse_track_file)
            .transpose()
    }

    /// Every track file of one album.
    pub async fn album_track_files(&self, album_id: i32) -> Result<Vec<LidarrTrackFile>, LidarrError> {
        self.get_array(&format!("/api/v1/trackfile?albumId={album_id}"))
            .await?
            .iter()
            .map(parse_track_file)
            .collect()
    }

    /// Lidarr's root folders, in its own paths.
    pub async fn root_folders(&self) -> Result<Vec<LidarrRootFolder>, LidarrError> {
        self.get_array("/api/v1/rootfolder")
            .await?
            .iter()
            .map(parse_root_folder)
            .collect()
    }

    // ---- Lidarr's settings and state ---------------------------------------------------------

    /// Whether Lidarr imports finished downloads itself (Completed Download Handling). Lidarr's
    /// own default, on, when it does not say.
    pub async fn completed_download_handling(&self) -> Result<bool, LidarrError> {
        let config = self.get_object("/api/v1/config/downloadclient").await?;
        Ok(nullable_bool(&config, "enableCompletedDownloadHandling")?.unwrap_or(true))
    }

    /// Whether Lidarr writes its own tags into the files it imports and adopts ("Write Metadata
    /// to Audio Files" anything but `no`). A setting Lidarr does not name counts as writing.
    pub async fn writes_audio_tags(&self) -> Result<bool, LidarrError> {
        let config = self.get_object("/api/v1/config/metadataprovider").await?;
        Ok(!nullable_str(&config, "writeAudioTags")?.is_some_and(|tags| dotnet::eq_ignore_case(&tags, "no")))
    }

    /// Lidarr's recycle bin, in its own paths, when one is set: a track file Lidarr deletes is
    /// moved there rather than removed.
    pub async fn recycle_bin(&self) -> Result<Option<String>, LidarrError> {
        let config = self.get_object("/api/v1/config/mediamanagement").await?;
        Ok(nullable_str(&config, "recycleBin")?.filter(|bin| !dotnet::is_blank(bin)))
    }

    /// When the running Lidarr started.
    pub async fn start_time(&self) -> Result<Option<DateTime<Utc>>, LidarrError> {
        Ok(self.status_times().await?.0)
    }

    /// The time on Lidarr's own clock, to the second, from its answer's `Date` header: every time
    /// Octo compares with Lidarr's dates comes from Lidarr.
    pub async fn now(&self) -> Result<DateTime<Utc>, LidarrError> {
        Ok(self.status_times().await?.1)
    }

    /// When the running Lidarr started, and the time on its clock now.
    pub async fn status_times(&self) -> Result<(Option<DateTime<Utc>>, DateTime<Utc>), LidarrError> {
        let request = self.request(Method::GET, "/api/v1/system/status", None)?;
        let timeout = request.timeout.unwrap_or(Self::TIMEOUT);
        let response = self.send(request).await?;
        let now = response
            .headers()
            .get(reqwest::header::DATE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| DateTime::parse_from_rfc2822(value.trim()).ok())
            .map(|date| date.with_timezone(&Utc))
            .ok_or_else(|| LidarrError::InvalidOperation("Lidarr's answer carries no Date header.".into()))?;
        let text = response
            .text()
            .await
            .map_err(|error| transport_error(error, timeout))?;
        let node = serde_json::from_str(&text).map_err(|e| LidarrError::Json(e.to_string()))?;
        let status = Self::object_of(node)?;
        Ok((nullable_date(&status, "startTime")?, now))
    }

    /// Lidarr's health checks as they stand.
    pub async fn health(&self) -> Result<Vec<LidarrHealthCheck>, LidarrError> {
        self.get_array("/api/v1/health")
            .await?
            .iter()
            .map(parse_health_check)
            .collect()
    }

    // ---- Changing Lidarr ---------------------------------------------------------------------

    /// Adds an album Lidarr does not have, and nothing more: the album unmonitored and no search
    /// started. `artist` is the Lidarr artist to add it under; None adds the artist too, with
    /// the chosen root folder and profiles, unmonitored and monitoring no album and no new
    /// release. Lidarr then refreshes the album or the artist and rescans the artist's folder.
    pub async fn add_album(
        &self,
        candidate: &LidarrAlbumCandidate,
        artist: Option<&LidarrArtist>,
    ) -> Result<LidarrAdded, LidarrError> {
        let settings = self.require_settings(true)?;
        let mut resource = candidate.resource.clone();
        resource.shift_remove("id");
        resource.insert("monitored".into(), Value::Bool(false));
        resource.insert("addOptions".into(), json!({ "searchForNewAlbum": false }));
        match artist {
            Some(artist) => {
                resource.insert("artistId".into(), Value::from(artist.id));
                resource.insert("artist".into(), Value::Object(artist.resource.clone()));
            }
            None => {
                let Some(Value::Object(new_artist)) = resource.get_mut("artist") else {
                    return Err(LidarrError::InvalidOperation(
                        "Lidarr album lookup returned no artist resource.".into(),
                    ));
                };
                new_artist.shift_remove("id");
                new_artist.insert(
                    "rootFolderPath".into(),
                    settings
                        .root_folder_path
                        .clone()
                        .map_or(Value::Null, Value::String),
                );
                new_artist.insert(
                    "qualityProfileId".into(),
                    Value::from(settings.quality_profile_id),
                );
                new_artist.insert(
                    "metadataProfileId".into(),
                    Value::from(settings.metadata_profile_id),
                );
                new_artist.insert("monitored".into(), Value::Bool(false));
                new_artist.insert("monitorNewItems".into(), Value::from("none"));
                new_artist.insert("tags".into(), json!([]));
                new_artist.insert(
                    "addOptions".into(),
                    json!({ "monitor": "none", "searchForMissingAlbums": false }),
                );
            }
        }
        let added = self.send_json(Method::POST, "/api/v1/album", &resource).await?;
        Ok(LidarrAdded {
            album_id: int_or_zero(&added, "id")?,
            artist_id: artist_id_of(&added)?,
        })
    }

    /// Sets which releases of an album Lidarr takes, and nothing else about it. When the set of
    /// monitored releases changes, or any release becomes acceptable, Lidarr unlinks the album's
    /// track files and rescans the artist's folder. A release the choice does not name keeps its
    /// state.
    pub async fn set_release_choice(
        &self,
        album_id: i32,
        choice: &LidarrReleaseChoice,
    ) -> Result<LidarrAlbum, LidarrError> {
        self.set_release_choice_if(album_id, |_| true, choice)
            .await?
            .ok_or_else(|| LidarrError::InvalidOperation("Lidarr's album changed meanwhile.".into()))
    }

    /// The same, only when the album as Lidarr sends it right before the change still passes
    /// `still`: a change someone made in Lidarr since Octo last read the album is never written
    /// over. None when it did not pass, and nothing was changed.
    pub async fn set_release_choice_if(
        &self,
        album_id: i32,
        still: impl Fn(&LidarrAlbum) -> bool,
        choice: &LidarrReleaseChoice,
    ) -> Result<Option<LidarrAlbum>, LidarrError> {
        let path = format!("/api/v1/album/{album_id}");
        let mut resource = self.get_object(&path).await?;
        if !still(&parse_library_album(&resource)?) {
            return Ok(None);
        }
        resource.insert("anyReleaseOk".into(), Value::Bool(choice.any_release_ok));
        if let Some(Value::Array(releases)) = resource.get_mut("releases") {
            for release in releases.iter_mut().filter_map(Value::as_object_mut) {
                let id = int_or_zero(release, "id")?;
                if let Some(wanted) = choice.releases.iter().find(|r| r.id == id) {
                    release.insert("monitored".into(), Value::Bool(wanted.monitored));
                }
            }
        }
        let updated = self.send_json(Method::PUT, &path, &resource).await?;
        parse_library_album(&updated).map(Some)
    }

    /// Deletes an album from Lidarr, never its files, and keeps it off no import list.
    pub async fn delete_album(&self, album_id: i32) -> Result<LidarrDeletion, LidarrError> {
        self.delete(&format!(
            "/api/v1/album/{album_id}?deleteFiles=false&addImportListExclusion=false"
        ))
        .await
    }

    /// Deletes an artist from Lidarr, never its files (its folder may hold the person's own
    /// library), and keeps it off no import list.
    pub async fn delete_artist(&self, artist_id: i32) -> Result<LidarrDeletion, LidarrError> {
        self.delete(&format!(
            "/api/v1/artist/{artist_id}?deleteFiles=false&addImportListExclusion=false"
        ))
        .await
    }

    /// Deletes a track file's record. Lidarr deletes the file only while it lies at the recorded
    /// path (into its recycle bin when one is set); a file Octo moved away stays where Octo put
    /// it. `NotFound`: the record is gone already. `Conflict`: Lidarr keeps it while the
    /// artist's root folder is missing or empty.
    pub async fn delete_track_file_record(&self, track_file_id: i32) -> Result<LidarrDeletion, LidarrError> {
        self.delete(&format!("/api/v1/trackfile/{track_file_id}")).await
    }

    // ---- Commands ------------------------------------------------------------------------------

    /// Starts Lidarr's search for one album. Nothing about the album or its artist changes, so
    /// an unmonitored album stays unmonitored.
    pub async fn album_search(&self, album_id: i32) -> Result<LidarrCommand, LidarrError> {
        self.post_command(json!({ "name": "AlbumSearch", "albumIds": [album_id] }))
            .await
    }

    /// Asks Lidarr to run its health checks now.
    pub async fn check_health(&self) -> Result<LidarrCommand, LidarrError> {
        self.post_command(json!({ "name": "CheckHealth" })).await
    }

    /// Imports files of tracked downloads as those downloads ("auto": moved or copied as
    /// Lidarr's own imports are), replacing no file Lidarr has. Each file keeps its download id,
    /// so its import row names the download and its client.
    pub async fn manual_import(
        &self,
        items: &[LidarrManualImportItem],
    ) -> Result<LidarrCommand, LidarrError> {
        let files: Vec<Value> = items
            .iter()
            .map(|item| {
                json!({
                    "path": item.path,
                    "artistId": item.artist_id,
                    "albumId": item.album_id,
                    "albumReleaseId": item.album_release_id,
                    "trackIds": item.track_ids,
                    "quality": item.quality,
                    "indexerFlags": item.indexer_flags,
                    "downloadId": item.download_id,
                    "disableReleaseSwitching": false,
                })
            })
            .collect();
        self.post_command(json!({
            "name": "ManualImport",
            "files": files,
            "importMode": "auto",
            "replaceExistingFiles": false,
        }))
        .await
    }

    /// Every command Lidarr lists: queued, running, and lately ended.
    pub async fn commands(&self) -> Result<Vec<LidarrCommand>, LidarrError> {
        self.get_array("/api/v1/command")
            .await?
            .iter()
            .map(parse_command)
            .collect()
    }

    /// One command by id, or None when Lidarr no longer has it.
    pub async fn command(&self, command_id: i32) -> Result<Option<LidarrCommand>, LidarrError> {
        self.get_object_or_none(&format!("/api/v1/command/{command_id}"))
            .await?
            .as_ref()
            .map(parse_command)
            .transpose()
    }

    async fn post_command(&self, body: Value) -> Result<LidarrCommand, LidarrError> {
        let body = body.as_object().cloned().unwrap_or_default();
        parse_command(&self.send_json(Method::POST, "/api/v1/command", &body).await?)
    }

    // ---- The queue -----------------------------------------------------------------------------

    /// The queue items of one album, pending releases included.
    pub async fn album_queue(&self, album_id: i32) -> Result<Vec<LidarrQueueItem>, LidarrError> {
        self.get_array(&format!("/api/v1/queue/details?albumIds={album_id}"))
            .await?
            .iter()
            .map(parse_queue_item)
            .collect()
    }

    /// Every queue item Lidarr has, of every artist, pending releases included.
    pub async fn queue(&self) -> Result<Vec<LidarrQueueItem>, LidarrError> {
        self.get_array("/api/v1/queue/details")
            .await?
            .iter()
            .map(parse_queue_item)
            .collect()
    }

    /// The queue items of every album of one artist, pending releases included.
    pub async fn artist_queue(&self, artist_id: i32) -> Result<Vec<LidarrQueueItem>, LidarrError> {
        self.get_array(&format!("/api/v1/queue/details?artistId={artist_id}"))
            .await?
            .iter()
            .map(parse_queue_item)
            .collect()
    }

    /// Removes a queue item and its download from the client, starting no new search.
    /// `blocklist` also marks the download failed, so its release is never grabbed again.
    pub async fn remove_queue_item(
        &self,
        queue_id: i32,
        blocklist: bool,
    ) -> Result<LidarrDeletion, LidarrError> {
        self.delete(&format!(
            "/api/v1/queue/{queue_id}?removeFromClient=true&blocklist={blocklist}&skipRedownload=true"
        ))
        .await
    }

    // ---- History -------------------------------------------------------------------------------

    /// How many rows of one event the album's history holds.
    pub async fn album_history_count(
        &self,
        album_id: i32,
        event: LidarrHistoryEvent,
    ) -> Result<i32, LidarrError> {
        let page = self
            .get_object(&format!(
                "/api/v1/history?albumId={album_id}&eventType={}&pageSize=1",
                event.code()
            ))
            .await?;
        int_or_zero(&page, "totalRecords")
    }

    /// The album's rows of one event, newest first, back to `since` (every row when None).
    pub async fn album_history(
        &self,
        album_id: i32,
        event: LidarrHistoryEvent,
        since: Option<DateTime<Utc>>,
    ) -> Result<Vec<LidarrHistoryRecord>, LidarrError> {
        self.history_pages(&format!("albumId={album_id}&eventType={}", event.code()), since)
            .await
    }

    /// Every row of one download, on any album, newest first.
    pub async fn download_history(&self, download_id: &str) -> Result<Vec<LidarrHistoryRecord>, LidarrError> {
        self.history_pages(
            &format!("downloadId={}", dotnet::escape_data_string(download_id)),
            None,
        )
        .await
    }

    /// The artist's rows of one event, or of every event, newest first.
    pub async fn artist_history(
        &self,
        artist_id: i32,
        event: Option<LidarrHistoryEvent>,
    ) -> Result<Vec<LidarrHistoryRecord>, LidarrError> {
        let path = match event {
            Some(event) => format!(
                "/api/v1/history/artist?artistId={artist_id}&eventType={}",
                event.code()
            ),
            None => format!("/api/v1/history/artist?artistId={artist_id}"),
        };
        let mut rows = self
            .get_array(&path)
            .await?
            .iter()
            .map(parse_history)
            .collect::<Result<Vec<_>, _>>()?;
        rows.sort_by_key(|row| std::cmp::Reverse(row.date));
        Ok(rows)
    }

    /// Every page of a history query, newest first, stopping at the first row older than
    /// `since`. A row that moved to the next page while paging is kept once.
    async fn history_pages(
        &self,
        filter: &str,
        since: Option<DateTime<Utc>>,
    ) -> Result<Vec<LidarrHistoryRecord>, LidarrError> {
        let mut rows: Vec<LidarrHistoryRecord> = Vec::new();
        let mut page = 1;
        loop {
            let answer = self
                .get_object(&format!(
                    "/api/v1/history?{filter}&page={page}&pageSize={HISTORY_PAGE_SIZE}&sortKey=date&sortDirection=descending"
                ))
                .await?;
            let found = parse_history_page(&answer)?;
            let count = found.records.len();
            for row in found.records {
                if since.is_some_and(|since| row.date < since) {
                    return Ok(rows);
                }
                if !rows.iter().any(|kept| kept.id == row.id) {
                    rows.push(row);
                }
            }
            if count == 0 || i64::from(page) * i64::from(HISTORY_PAGE_SIZE) >= i64::from(found.total_records)
            {
                return Ok(rows);
            }
            page += 1;
        }
    }

    // ---- Manual import -------------------------------------------------------------------------

    /// What Lidarr would import from one tracked download, leaving every file it has in place.
    /// Lidarr asks the download's client where its files are.
    pub async fn manual_import_candidates(
        &self,
        download_id: &str,
    ) -> Result<Vec<LidarrManualImportItem>, LidarrError> {
        self.get_array(&format!(
            "/api/v1/manualimport?downloadId={}&replaceExistingFiles=false",
            dotnet::escape_data_string(download_id)
        ))
        .await?
        .iter()
        .map(parse_manual_import)
        .collect()
    }

    fn require_settings(&self, require_profiles: bool) -> Result<LidarrSettings, LidarrError> {
        let value = self.settings.current().lidarr.clone();
        if dotnet::is_null_or_white_space(value.base_url.as_deref())
            || dotnet::is_null_or_white_space(value.api_key.as_deref())
        {
            return Err(LidarrError::InvalidOperation(
                "Lidarr URL and API key are required.".into(),
            ));
        }
        if require_profiles
            && (dotnet::is_null_or_white_space(value.root_folder_path.as_deref())
                || value.quality_profile_id <= 0
                || value.metadata_profile_id <= 0)
        {
            return Err(LidarrError::InvalidOperation(
                "Choose a Lidarr root folder, quality profile, and metadata profile.".into(),
            ));
        }
        Ok(value)
    }

    /// `CreateRequest(method, path)`: against the saved address and key.
    fn request(&self, method: Method, path: &str, body: Option<&Value>) -> Result<Request, LidarrError> {
        let settings = self.require_settings(false)?;
        Ok(Self::request_to(
            method,
            path,
            settings.base_url.as_deref().unwrap_or_default(),
            settings.api_key.as_deref().unwrap_or_default(),
            body,
        ))
    }

    fn request_to(
        method: Method,
        path: &str,
        base_url: &str,
        api_key: &str,
        body: Option<&Value>,
    ) -> Request {
        Request {
            method,
            url: format!("{}{path}", base_url.trim_end_matches('/')),
            api_key: api_key.to_string(),
            // JsonContent.Create: the web serializer's escaping.
            body: body.map(octo_core::json::to_string),
            timeout: None,
        }
    }

    async fn send(&self, request: Request) -> Result<reqwest::Response, LidarrError> {
        let timeout = request.timeout.unwrap_or(Self::TIMEOUT);
        let response = self.answer(request).await?;
        if !response.status().is_success() {
            return Err(Self::status_error(response, timeout).await);
        }
        Ok(response)
    }

    /// What an error status is told as: the status and the start of the body.
    async fn status_error(response: reqwest::Response, timeout: Duration) -> LidarrError {
        let status = response.status().as_u16();
        match response.text().await {
            Ok(body) => LidarrError::Http(format!("Lidarr returned HTTP {status}: {}", truncate(&body, 300))),
            Err(error) => transport_error(error, timeout),
        }
    }

    /// The answer whatever its status; only a request that got no answer fails.
    async fn answer(&self, request: Request) -> Result<reqwest::Response, LidarrError> {
        let timeout = request.timeout.unwrap_or(Self::TIMEOUT);
        let failed = |error: reqwest::Error| transport_error(error, timeout);
        let url = reqwest::Url::parse(&request.url).map_err(|_| {
            LidarrError::InvalidOperation(
                "An invalid request URI was provided. Either the request URI must be an absolute URI or BaseAddress must be set."
                    .into(),
            )
        })?;
        let path = url.path().to_string();
        let mut builder = self.http.request(request.method, url);
        if let Some(timeout) = request.timeout {
            builder = builder.timeout(timeout);
        }
        // TryAddWithoutValidation: a value a header cannot hold is left out.
        if let Ok(value) = reqwest::header::HeaderValue::from_str(&request.api_key) {
            builder = builder.header("X-Api-Key", value);
        }
        if let Some(body) = request.body {
            builder = builder
                .header(reqwest::header::CONTENT_TYPE, "application/json; charset=utf-8")
                .body(body);
        }
        let response = builder.send().await.map_err(failed)?;
        if response.status().is_success() {
            let hook = self.answered.lock().clone();
            if let Some(hook) = hook {
                hook(&path);
            }
        }
        Ok(response)
    }

    /// A request whose 404 and 409 are answers: what a delete came to.
    async fn delete(&self, path: &str) -> Result<LidarrDeletion, LidarrError> {
        let request = self.request(Method::DELETE, path, None)?;
        let timeout = request.timeout.unwrap_or(Self::TIMEOUT);
        let response = self.answer(request).await?;
        match response.status().as_u16() {
            404 => Ok(LidarrDeletion::NotFound),
            409 => Ok(LidarrDeletion::Conflict),
            _ if response.status().is_success() => Ok(LidarrDeletion::Deleted),
            _ => Err(Self::status_error(response, timeout).await),
        }
    }

    /// The object at `path`, or None when Lidarr answers 404.
    async fn get_object_or_none(&self, path: &str) -> Result<Option<JsonObject>, LidarrError> {
        let request = self.request(Method::GET, path, None)?;
        let timeout = request.timeout.unwrap_or(Self::TIMEOUT);
        let response = self.answer(request).await?;
        if response.status().as_u16() == 404 {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(Self::status_error(response, timeout).await);
        }
        let text = response
            .text()
            .await
            .map_err(|error| transport_error(error, timeout))?;
        let node = serde_json::from_str(&text).map_err(|e| LidarrError::Json(e.to_string()))?;
        Self::object_of(node).map(Some)
    }

    async fn read_json(&self, request: Request) -> Result<Value, LidarrError> {
        let timeout = request.timeout.unwrap_or(Self::TIMEOUT);
        let response = self.send(request).await?;
        let text = response
            .text()
            .await
            .map_err(|error| transport_error(error, timeout))?;
        serde_json::from_str(&text).map_err(|e| LidarrError::Json(e.to_string()))
    }

    async fn get_array(&self, path: &str) -> Result<Vec<JsonObject>, LidarrError> {
        let request = self.request(Method::GET, path, None)?;
        Self::objects_of(self.read_json(request).await?)
    }

    async fn get_array_from(
        &self,
        path: &str,
        base_url: &str,
        api_key: &str,
    ) -> Result<Vec<JsonObject>, LidarrError> {
        let request = Self::request_to(Method::GET, path, base_url, api_key, None);
        Self::objects_of(self.read_json(request).await?)
    }

    /// `node.OfType<JsonObject>()`: the array's objects, anything else in it skipped.
    fn objects_of(node: Value) -> Result<Vec<JsonObject>, LidarrError> {
        match node {
            Value::Array(items) => Ok(items
                .into_iter()
                .filter_map(|item| match item {
                    Value::Object(o) => Some(o),
                    _ => None,
                })
                .collect()),
            _ => Err(LidarrError::InvalidOperation(
                "Lidarr returned an invalid array response.".into(),
            )),
        }
    }

    fn object_of(node: Value) -> Result<JsonObject, LidarrError> {
        match node {
            Value::Object(o) => Ok(o),
            _ => Err(LidarrError::InvalidOperation(
                "Lidarr returned an invalid object response.".into(),
            )),
        }
    }

    async fn get_object(&self, path: &str) -> Result<JsonObject, LidarrError> {
        let request = self.request(Method::GET, path, None)?;
        Self::object_of(self.read_json(request).await?)
    }

    async fn send_json(
        &self,
        method: Method,
        path: &str,
        body: &JsonObject,
    ) -> Result<JsonObject, LidarrError> {
        let body = Value::Object(body.clone());
        let request = self.request(method, path, Some(&body))?;
        Self::object_of(self.read_json(request).await?)
    }
}

/// One request, built before it is sent.
struct Request {
    method: Method,
    url: String,
    api_key: String,
    body: Option<String>,
    /// The timeout for this request, when it is not the usual 20 s.
    timeout: Option<Duration>,
}

/// The artist id an album resource names: its `artistId`, else its artist's `id`; 0 when
/// neither is there.
fn artist_id_of(album: &JsonObject) -> Result<i32, LidarrError> {
    if let Some(id) = nullable_int(album, "artistId")?.filter(|id| *id > 0) {
        return Ok(id);
    }
    match object(album, "artist") {
        Some(artist) => int_or_zero(artist, "id"),
        None => Ok(0),
    }
}

/// What a request that got no answer fails with: its timeout, or the connection.
fn transport_error(error: reqwest::Error, timeout: Duration) -> LidarrError {
    if error.is_timeout() {
        LidarrError::Canceled(http_client_factory::timeout_message(timeout))
    } else {
        LidarrError::Http(http_client_factory::connect_failure_message(&error))
    }
}

/// `value[..max]` in UTF-16 code units, without cutting a character in two.
fn truncate(value: &str, max: usize) -> &str {
    let mut units = 0;
    for (i, c) in value.char_indices() {
        units += c.len_utf16();
        if units > max {
            return &value[..i];
        }
    }
    value
}

/// The longest wait for Lidarr to list an album's tracks before its search starts.
const TRACK_LIST_WAIT_MAX: Duration = Duration::from_secs(5 * 60);

/// How many history rows one page asks for.
const HISTORY_PAGE_SIZE: i32 = 100;

/// How long to wait for an album's track list: the import timeout, at most
/// [`TRACK_LIST_WAIT_MAX`], so a long import timeout never holds the search back for as long.
fn track_list_wait(settings: &LidarrSettings) -> Duration {
    Duration::from_secs(settings.import_timeout_seconds.max(1) as u64).min(TRACK_LIST_WAIT_MAX)
}

#[cfg(test)]
#[path = "lidarr_client_tests.rs"]
mod tests;
