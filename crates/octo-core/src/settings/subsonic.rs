//! `Octo.Models.Settings.SubsonicSettings` and its enums (the `Subsonic` section).

use serde::{Deserialize, Serialize};

use crate::common::dotnet::{is_blank, ordinal_ignore_case_key};

/// Download mode for tracks
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Deserialize, Serialize)]
pub enum DownloadMode {
    /// Download only the requested track (default behavior)
    #[default]
    Track,

    /// When a track is played, download the entire album in background
    /// The requested track is downloaded first, then remaining tracks are queued
    Album,
}

/// Explicit content filter mode for Deezer tracks
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Deserialize, Serialize)]
pub enum ExplicitFilter {
    /// Show all tracks (no filtering)
    #[default]
    All,

    /// Exclude clean/edited versions (explicit_content_lyrics == 3)
    /// Shows original explicit content and naturally clean content
    ExplicitOnly,

    /// Only show clean content (explicit_content_lyrics == 0 or 3)
    /// Excludes tracks with explicit_content_lyrics == 1
    CleanOnly,
}

/// Storage mode for downloaded tracks
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Deserialize, Serialize)]
pub enum StorageMode {
    /// Files are permanently stored in the library and registered in the database
    #[default]
    Permanent,

    /// Files are stored in a temporary cache and automatically cleaned up
    /// Not registered in the database, no Navidrome scan triggered
    Cache,

    /// True streaming mode - audio is proxied directly without saving to disk
    /// Lowest latency, no disk I/O, but re-fetches on each play
    Stream,
}

/// Folder structure for downloaded tracks
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Deserialize, Serialize)]
pub enum FolderStructure {
    /// Organized folder structure: Artist/Album/XX - Track.flac
    /// Better for large libraries with album-based organization
    #[default]
    Organized,

    /// Flat file structure: Artist - Title.flac (all files in root)
    /// Better for simple libraries without nested folders
    Flat,

    /// One folder per artist, no album folder: Artist/Title.flac
    /// For a library built a track at a time, where Organized makes a folder per
    /// single and Flat makes one directory of thousands of files. The album is
    /// still in the tags, so a server that reads tags shows it either way.
    ByArtist,
}

/// Where a starred track's permanent copy comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Deserialize, Serialize)]
pub enum DownloadSource {
    /// Lossless FLAC via Soulseek/slskd (default).
    #[default]
    Soulseek,

    /// Lossy MP3 via the yt-dlp shim.
    YouTube,

    /// Try Soulseek FLAC first; fall back to YouTube MP3 if it fails.
    SoulseekThenYouTube,

    /// Submit external track/album hearts to an existing Lidarr instance. Lidarr is
    /// album-oriented, so a track heart acquires the track's full album. Non-heart
    /// permanent downloads continue to use Soulseek.
    Lidarr,
}

/// A source that can participate in the ordered heart-acquisition chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Deserialize, Serialize)]
pub enum HeartDownloadSource {
    #[default]
    Soulseek,
    YouTube,
    Lidarr,
}

impl HeartDownloadSource {
    /// `Enum.GetValues<HeartDownloadSource>()`, in declaration order.
    pub const ALL: [HeartDownloadSource; 3] = [
        HeartDownloadSource::Soulseek,
        HeartDownloadSource::YouTube,
        HeartDownloadSource::Lidarr,
    ];
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct HeartDownloadStep {
    pub source: HeartDownloadSource,
    /// Legacy single switch; used only when the per-heart switches are absent.
    pub enabled: Option<bool>,
    pub song_enabled: Option<bool>,
    pub album_enabled: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct SubsonicSettings {
    pub url: Option<String>,

    /// Optional Navidrome admin username. When set (with AdminPassword), Octo can
    /// authenticate to Navidrome for background work it does as a proxy: detecting
    /// the music folder and triggering an authenticated rescan. If left empty, Octo
    /// falls back to an admin token captured from a client's relayed native login.
    /// Environment variable: SUBSONIC__ADMINUSERNAME
    pub admin_username: Option<String>,

    /// Optional Navidrome admin password paired with AdminUsername. See AdminUsername.
    /// Environment variable: SUBSONIC__ADMINPASSWORD
    pub admin_password: Option<String>,

    /// Auto-detect the download destination from Navidrome's own music folder
    /// (default: true). Octo is a proxy in front of Navidrome, so downloads should
    /// land where Navidrome scans. When on, the effective download path is the
    /// folder reported by Navidrome's /api/library; the Library:DownloadPath value
    /// becomes a fallback used only until detection succeeds (or if it never does).
    /// Turn off to always use Library:DownloadPath verbatim.
    /// Environment variable: SUBSONIC__AUTODETECTDOWNLOADPATH
    pub auto_detect_download_path: bool,

    /// Which Navidrome library to download into, given as its folder path, for
    /// servers that serve more than one. Empty (the default) keeps the historical
    /// behavior of taking the first library Navidrome reports. A value that no
    /// longer matches any reported library is ignored with a warning rather than
    /// leaving downloads with nowhere to go.
    /// Environment variable: SUBSONIC__LIBRARYPATH
    pub library_path: String,

    /// Explicit content filter mode (default: All)
    /// Environment variable: EXPLICIT_FILTER
    /// Values: "All", "ExplicitOnly", "CleanOnly"
    /// Note: Only works with Deezer
    pub explicit_filter: ExplicitFilter,

    /// Legacy direct-download mode (default: Track), retained for playlist jobs.
    /// Environment variable: DOWNLOAD_MODE
    /// Values: "Track" or "Album"
    pub download_mode: DownloadMode,

    /// Legacy storage mode for direct-download jobs (default: Permanent).
    /// Environment variable: STORAGE_MODE
    /// Ordinary external playback always streams from YouTube unless lossless waiting is enabled.
    pub storage_mode: StorageMode,

    /// Cache duration in hours for Cache storage mode (default: 1)
    /// Environment variable: CACHE_DURATION_HOURS
    /// Files older than this duration will be automatically deleted
    /// Only applies when StorageMode is Cache
    pub cache_duration_hours: i32,

    /// Enable external playlist search and streaming (default: true)
    /// Environment variable: ENABLE_EXTERNAL_PLAYLISTS
    /// When enabled, users can search for playlists from the configured music provider
    /// Playlists appear as "albums" in search results with genre "Playlist"
    pub enable_external_playlists: bool,

    /// Include Last.fm/Deezer discovery songs and albums in search3/search2 results
    /// (default: true).
    /// Environment variable: ENABLE_SEARCH_DISCOVERY
    ///
    /// Off, search returns only local library matches: every result plays straight from
    /// Navidrome instead of resolving through the YouTube shim on first tap, and a search
    /// no longer waits on Deezer/Last.fm at all. This only changes what search3/search2
    /// hands back; radio (getSimilarSongs2) and the Last.fm personalized/discovery
    /// stations are unaffected either way.
    pub enable_search_discovery: bool,

    /// Resolve the YouTube durations of the top discovery rows before search3/search2
    /// answers (default: true).
    /// Environment variable: WAIT_FOR_SEARCH_DURATIONS
    ///
    /// Off, search answers with Deezer's durations, which saves a few seconds per new query,
    /// and finds the YouTube ones in the background. Rows already sent keep Deezer's length.
    /// getSong and the Navidrome API's song lookup report the YouTube length once it is
    /// known, so apps that look the song up again when it starts show the right length;
    /// apps that only use the length from the search results show Deezer's.
    pub wait_for_search_durations: bool,

    /// How long a search waits for those YouTube lengths, in milliseconds (default: 1000,
    /// range 0-10000). 0 never waits.
    /// Environment variable: SEARCH_DURATION_WAIT_MS
    ///
    /// The rows whose length is found within the wait go out with it; the others go out with
    /// Deezer's length and get the YouTube one in the background, where getSong and the next
    /// search for the same words pick it up. WaitForSearchDurations off means 0.
    pub search_duration_wait_ms: i32,

    /// How long a search waits for Last.fm's track search, in milliseconds (default: 1000,
    /// range 0-10000). 0 waits for Last.fm however long it takes.
    /// Environment variable: SEARCH_LASTFM_WAIT_MS
    ///
    /// When Last.fm has not answered in time, the outside songs come from Deezer's own track
    /// search for the words instead, in Deezer's order. Last.fm's answer, when it arrives, is
    /// kept, and the next search for the same words uses it.
    pub search_last_fm_wait_ms: i32,

    /// A search that asks for this many songs or fewer looks only in the library (default: 12,
    /// range 0-100).
    /// Environment variable: SEARCH_LIBRARY_ONLY_UP_TO
    ///
    /// Such a small count is taken for an app searching as the user types, where asking Last.fm,
    /// Deezer and YouTube on every keystroke would be wasted. Some apps send a small count for a
    /// finished search too (the Music Assistant app asks for 10 songs in its overview) and then
    /// never list a song from outside the library; a lower value gives them those songs, and 0
    /// searches outside at any count. For Octo's apps it is also how many places of a small page
    /// the library keeps for itself.
    pub search_library_only_up_to: i32,

    /// Give clients that sync the library to the device a discovery catalog (default: true).
    /// Environment variable: ENABLE_SYNC_CATALOG
    ///
    /// Symfonium copies the whole library by paging search3 with an empty query and then
    /// searches only that copy, so a typed search never reaches Octo. With this on, the
    /// copy continues past the last library song with your radio stations' tracks that the
    /// library does not own. On the device they search, browse and play like any other
    /// song, the station playlists find their tracks, and a heart downloads one as usual.
    /// Applies only to the clients named in SyncCatalogClients.
    pub enable_sync_catalog: bool,

    /// Clients that get the sync catalog, comma-separated, matched against the client name
    /// the app sends (the Subsonic `c` parameter), ignoring case (default: "Symfonium").
    /// Environment variable: SYNC_CATALOG_CLIENTS
    ///
    /// A named client sees catalog songs in its library views, which is the point for an app
    /// that only ever searches its own copy and noise for one that searches the server. So
    /// name only the first kind.
    pub sync_catalog_clients: String,

    /// The most songs one user's sync catalog holds (default: 1000, range 50-5000).
    /// Environment variable: SYNC_CATALOG_MAX_SONGS
    pub sync_catalog_max_songs: i32,

    /// How many outside songs a minute have their stream size learned in the background, so
    /// the next answer listing them carries it (default: 5, range 0-30). Each costs YouTube one
    /// extraction. 0 turns the background lookups off.
    /// Environment variable: STREAM_SIZE_WARM_PER_MINUTE
    pub stream_size_warm_per_minute: i32,

    /// How many megabytes of library covers Octo keeps on disk, so each cover is read from
    /// Navidrome once and every size an app asks for is cut from that copy (default: 1024,
    /// range 0-102400). The least recently shown covers go first when it is full. 0 turns the
    /// cover cache off and every cover is asked of Navidrome again.
    /// Environment variable: COVER_CACHE_MB
    pub cover_cache_mb: i32,

    /// Draw the small Octo octopus on the covers of outside songs, albums and artists (default:
    /// true), so any app shows which songs are not in the library yet and that a heart downloads
    /// them. Library covers never carry it, and it is never written into a file. Octo's own apps
    /// mark outside songs themselves and always get plain covers.
    /// Environment variable: COVER_BADGE
    pub cover_badge: bool,

    /// Apps that read a song's size only to decide whether to show it, comma-separated,
    /// matched against the client name the app sends (the Subsonic `c` parameter) or its
    /// User-Agent, ignoring case (default: "Amperfy").
    /// Environment variable: SIZE_FLAG_CLIENTS
    ///
    /// Such an app hides a song with no size. An outside song has none until Octo learns the
    /// bytes of its stream, so these apps get 1 for it until then, which reads as "this song
    /// is here". A size above 2,147,483,647, which they cannot store, is held to that number.
    /// Every other app gets sizes unchanged. Empty turns this off.
    pub size_flag_clients: String,

    /// Directory name for storing playlist .m3u files (default: "playlists")
    /// Environment variable: PLAYLISTS_DIRECTORY
    /// Relative to the music library root directory
    /// Playlist files will be stored in {MusicDirectory}/{PlaylistsDirectory}/
    pub playlists_directory: String,

    /// Auto-download tracks when starred (default: true)
    /// Environment variable: DOWNLOAD_ON_STAR
    /// When enabled in Stream/Cache mode, starring a track triggers permanent download
    pub download_on_star: bool,

    /// Auto-download every track when a whole album is starred (default: true)
    /// Environment variable: DOWNLOAD_ALBUM_ON_STAR
    /// Works in every storage mode. Downloads run one at a time, so a full album is a
    /// long job; turn this off to keep song-starring without the larger commitment.
    pub download_album_on_star: bool,

    /// Also fetch the whole album when an outside song is starred (default: false).
    /// Environment variable: DOWNLOAD_ALBUM_ON_SONG_STAR
    /// The starred song goes first, down the song-heart chain as always. Once it shows in the
    /// library (or a few minutes have passed), its album goes down the album-heart chain,
    /// exactly as if the album had been starred, so it needs a source with album hearts
    /// enabled. Songs already in the library are skipped as usual. When Lidarr took the song,
    /// it brings the whole album, so nothing more is fetched. A family member's album songs
    /// are not starred for them; only the song they hearted is. The album is found the way a
    /// song row names it: the song's album, or for a single with no album name, the album the
    /// catalog lists for the song.
    pub download_album_on_song_star: bool,

    /// Record which Subsonic user asked for each download (default: true)
    /// Environment variable: RECORD_REQUESTED_BY
    /// The username reaches the fetched-songs log and the download notification, so on a
    /// shared library you can tell one person's acquisitions from another's. Turning it off
    /// stops the name being captured at all rather than hiding it afterwards, so nothing
    /// downstream ever holds it. Entries written while it was on keep their names.
    pub record_requested_by: bool,

    /// Also favorite a hearted outside song or album in Navidrome once it downloads, for the
    /// person who hearted it (default: false). Off, a heart on a song you do not have only
    /// downloads it; heart it again once it is in the library to make it a favorite. A heart on
    /// a song you already have is always a favorite, whatever this says.
    /// Environment variable: STAR_DOWNLOADS_FOR_REQUESTER
    /// Octo's own apps are left out: their star is the Add button. The person's sign-in is held
    /// in memory until the song arrives, and a restart drops it.
    pub star_downloads_for_requester: bool,

    /// Never download a song already in the library (default: true). A heart, an album walk or a
    /// play of a song you have keeps your copy; a lossy copy is queued for a higher quality one
    /// when a lossless source is in the chain and Better quality may run. The same song means the
    /// same artist and title in one version, and a length within 8 seconds or the same album.
    /// Environment variable: SKIP_OWNED_SONGS
    pub skip_owned_songs: bool,

    /// In Permanent mode, block the first play until the lossless copy has been fetched
    /// (default: false).
    /// Environment variable: WAIT_FOR_LOSSLESS_ON_PLAY
    ///
    /// This also decides what search results DECLARE for external tracks, which is why it
    /// is restart-required. A Subsonic client picks its decoder from the declared suffix
    /// and content type, so those have to describe the bytes that will actually arrive:
    /// off, an external id is always the lossy stream and the lossless copy shows up as a
    /// separate library track after the rescan; on, the id is declared lossless and the
    /// request waits for it.
    ///
    /// Off by default because a Soulseek fetch routinely runs for minutes, and no client
    /// waits that long: turning it on means the first play of each track appears to fail
    /// while the file lands in the background.
    pub wait_for_lossless_on_play: bool,

    /// With WaitForLosslessOnPlay on, give up waiting after this many seconds and fall
    /// back to the lossy preview while the fetch finishes in the background (default: 0,
    /// wait as long as the fetch needs).
    /// Environment variable: LOSSLESS_WAIT_TIMEOUT_SECONDS
    ///
    /// A fallback serves lossy bytes under an id this session declared lossless, which
    /// strict clients can refuse to start. That trade is why it is opt-in and 0 keeps
    /// the declared contract exact.
    pub lossless_wait_timeout_seconds: i32,

    /// Keep a copy of every external track that is played, not only of hearted ones
    /// (default: false).
    /// Environment variable: DOWNLOAD_ON_PLAY
    ///
    /// The copy comes from the first source with song hearts enabled in the heart download
    /// priority that Octo downloads from itself (Soulseek, YouTube; Lidarr is skipped, see
    /// LidarrAlbumOnPlay). Playback still starts from the YouTube stream. Only a play from
    /// the first byte counts, not a seek. Hearts always go first: at most one played track
    /// waits to download, and one played while another waits is skipped until it is played
    /// again.
    pub download_on_play: bool,

    /// Hand the album of every played external track to Lidarr (default: false).
    /// Environment variable: LIDARR_ALBUM_ON_PLAY
    ///
    /// Works like a Lidarr song heart, once per track and run. Every hand-off makes Lidarr
    /// search all its indexers, and radio pulls in an album per played track. A hand-off
    /// Lidarr turns down is tried again on the next play.
    pub lidarr_album_on_play: bool,

    /// Folder structure for downloaded tracks (default: Flat)
    /// Environment variable: FOLDER_STRUCTURE
    /// Values: "Organized" (Artist/Album/Track.flac), "Flat" (Artist - Title.flac)
    pub folder_structure: FolderStructure,

    /// Download source (default: Soulseek). Lidarr applies to hearts only.
    /// Environment variable: DOWNLOAD_SOURCE
    /// Values: "Soulseek" (FLAC), "YouTube" (MP3), "SoulseekThenYouTube"
    /// (FLAC with MP3 fallback), or "Lidarr" (heart-only, full album).
    pub download_source: DownloadSource,

    /// Ordered sources for explicit track and album hearts. Empty keeps older
    /// DOWNLOAD_SOURCE configurations working; Lidarr remains last by default.
    pub heart_download_sources: Vec<HeartDownloadStep>,

    /// Use local staging for cloud storage mounts (default: false)
    /// Environment variable: USE_LOCAL_STAGING
    /// When enabled, downloads go to local temp first, metadata is written there,
    /// then the file is moved to the final destination. Required for FUSE/rclone mounts
    /// where TagLib cannot write metadata directly.
    pub use_local_staging: bool,
}

impl Default for SubsonicSettings {
    fn default() -> Self {
        Self {
            url: None,
            admin_username: None,
            admin_password: None,
            auto_detect_download_path: true,
            library_path: String::new(),
            explicit_filter: ExplicitFilter::All,
            download_mode: DownloadMode::Track,
            storage_mode: StorageMode::Permanent,
            cache_duration_hours: 1,
            enable_external_playlists: true,
            enable_search_discovery: true,
            wait_for_search_durations: true,
            search_duration_wait_ms: 1000,
            search_last_fm_wait_ms: 1000,
            search_library_only_up_to: 12,
            enable_sync_catalog: true,
            sync_catalog_clients: "Symfonium".to_string(),
            sync_catalog_max_songs: 1000,
            stream_size_warm_per_minute: 5,
            cover_cache_mb: 1024,
            cover_badge: true,
            size_flag_clients: "Amperfy".to_string(),
            playlists_directory: "playlists".to_string(),
            download_on_star: true,
            download_album_on_star: true,
            download_album_on_song_star: false,
            record_requested_by: true,
            star_downloads_for_requester: false,
            skip_owned_songs: true,
            wait_for_lossless_on_play: false,
            lossless_wait_timeout_seconds: 0,
            download_on_play: false,
            lidarr_album_on_play: false,
            folder_structure: FolderStructure::Flat,
            download_source: DownloadSource::Soulseek,
            heart_download_sources: Vec::new(),
            use_local_staging: false,
        }
    }
}

impl SubsonicSettings {
    pub fn effective_sync_catalog_max_songs(&self) -> i32 {
        self.sync_catalog_max_songs.clamp(50, 5000)
    }

    /// How long a search waits for the YouTube lengths of its top outside rows: none while
    /// WaitForSearchDurations is off, else SearchDurationWaitMs within 0 to 10 seconds.
    pub fn effective_search_duration_wait(&self) -> std::time::Duration {
        if !self.wait_for_search_durations {
            return std::time::Duration::ZERO;
        }
        std::time::Duration::from_millis(self.search_duration_wait_ms.clamp(0, 10_000) as u64)
    }

    /// How long a search waits for Last.fm's track search before using Deezer's: none (wait
    /// for Last.fm) at 0, else SearchLastFmWaitMs within 0 to 10 seconds.
    pub fn effective_search_last_fm_wait(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.search_last_fm_wait_ms.clamp(0, 10_000) as u64)
    }

    /// SearchLibraryOnlyUpTo within 0 to 100.
    pub fn effective_search_library_only_up_to(&self) -> i32 {
        self.search_library_only_up_to.clamp(0, 100)
    }

    pub fn effective_stream_size_warm_per_minute(&self) -> usize {
        self.stream_size_warm_per_minute.clamp(0, 30) as usize
    }

    /// The cover cache's size in bytes; 0 when it is off.
    pub fn effective_cover_cache_bytes(&self) -> u64 {
        self.cover_cache_mb.clamp(0, 102_400) as u64 * 1024 * 1024
    }

    /// Whether the app calling, known by its client name or its User-Agent, is one of
    /// SizeFlagClients: a name in the list contained in either, ignoring case.
    pub fn is_size_flag_client(&self, client: Option<&str>, user_agent: Option<&str>) -> bool {
        let callers: Vec<String> = [client, user_agent]
            .into_iter()
            .flatten()
            .filter(|text| !is_blank(text))
            .map(ordinal_ignore_case_key)
            .collect();
        if callers.is_empty() {
            return false;
        }
        self.size_flag_clients
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(ordinal_ignore_case_key)
            .any(|name| callers.iter().any(|caller| caller.contains(&name)))
    }

    pub fn effective_heart_download_sources(&self) -> Vec<HeartDownloadStep> {
        // The first step per source is kept, in first-seen order.
        let mut configured: Vec<HeartDownloadStep> = Vec::new();
        for step in &self.heart_download_sources {
            if configured.iter().any(|s| s.source == step.source) {
                continue;
            }
            configured.push(HeartDownloadStep {
                source: step.source,
                enabled: None,
                song_enabled: Some(step.song_enabled.or(step.enabled).unwrap_or(false)),
                album_enabled: Some(step.album_enabled.or(step.enabled).unwrap_or(false)),
            });
        }
        if !configured.is_empty() {
            for source in HeartDownloadSource::ALL {
                if configured.iter().all(|step| step.source != source) {
                    configured.push(HeartDownloadStep {
                        source,
                        enabled: None,
                        song_enabled: Some(false),
                        album_enabled: Some(false),
                    });
                }
            }
            return configured;
        }

        match self.download_source {
            DownloadSource::YouTube => self.default_heart_sources(false, true, false),
            DownloadSource::SoulseekThenYouTube => self.default_heart_sources(true, true, false),
            DownloadSource::Lidarr => self.default_heart_sources(false, false, true),
            _ => self.default_heart_sources(true, false, false),
        }
    }

    fn default_heart_sources(&self, soulseek: bool, youtube: bool, lidarr: bool) -> Vec<HeartDownloadStep> {
        let step = |source, on: bool| HeartDownloadStep {
            source,
            enabled: None,
            song_enabled: Some(on && self.download_on_star),
            album_enabled: Some(on && self.download_album_on_star),
        };
        vec![
            step(HeartDownloadSource::Soulseek, soulseek),
            step(HeartDownloadSource::YouTube, youtube),
            step(HeartDownloadSource::Lidarr, lidarr),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_fallback_maps_to_ordered_sources_with_lidarr_last() {
        let settings = SubsonicSettings {
            download_source: DownloadSource::SoulseekThenYouTube,
            ..Default::default()
        };

        let steps = settings.effective_heart_download_sources();

        assert_eq!(steps.len(), 3);
        assert_eq!(steps[0].source, HeartDownloadSource::Soulseek);
        assert_eq!(
            (steps[0].song_enabled, steps[0].album_enabled),
            (Some(true), Some(true))
        );
        assert_eq!(steps[1].source, HeartDownloadSource::YouTube);
        assert_eq!(
            (steps[1].song_enabled, steps[1].album_enabled),
            (Some(true), Some(true))
        );
        assert_eq!(steps[2].source, HeartDownloadSource::Lidarr);
        assert_eq!(
            (steps[2].song_enabled, steps[2].album_enabled),
            (Some(false), Some(false))
        );
    }

    #[test]
    fn configured_order_is_preserved_and_missing_sources_are_appended_disabled() {
        let settings = SubsonicSettings {
            heart_download_sources: vec![
                HeartDownloadStep {
                    source: HeartDownloadSource::Lidarr,
                    enabled: Some(true),
                    ..Default::default()
                },
                HeartDownloadStep {
                    source: HeartDownloadSource::YouTube,
                    enabled: Some(true),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let steps = settings.effective_heart_download_sources();

        assert_eq!(steps.len(), 3);
        assert_eq!(steps[0].source, HeartDownloadSource::Lidarr);
        assert_eq!(steps[1].source, HeartDownloadSource::YouTube);
        assert_eq!(steps[2].source, HeartDownloadSource::Soulseek);
        assert_eq!(
            (steps[2].song_enabled, steps[2].album_enabled),
            (Some(false), Some(false))
        );
    }

    #[test]
    fn per_heart_switches_win_over_the_legacy_one_and_the_first_step_per_source_counts() {
        let settings = SubsonicSettings {
            heart_download_sources: vec![
                HeartDownloadStep {
                    source: HeartDownloadSource::YouTube,
                    enabled: Some(true),
                    album_enabled: Some(false),
                    song_enabled: None,
                },
                HeartDownloadStep {
                    source: HeartDownloadSource::YouTube,
                    enabled: Some(false),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let steps = settings.effective_heart_download_sources();
        assert_eq!(steps.len(), 3);
        assert_eq!(
            (steps[0].song_enabled, steps[0].album_enabled),
            (Some(true), Some(false))
        );
    }

    #[test]
    fn size_flag_clients_match_the_client_name_or_the_user_agent_ignoring_case() {
        let defaults = SubsonicSettings::default();
        let cases: [(Option<&str>, Option<&str>, bool); 7] = [
            (Some("Amperfy"), None, true),
            (Some("amperfy"), Some("CFNetwork/1568"), true),
            (
                None,
                Some("Amperfy/2026.7 CFNetwork/1568.100.1 Darwin/25.0.0"),
                true,
            ),
            (Some("Symfonium"), Some("okhttp/4.12"), false),
            (Some("  "), None, false),
            (None, None, false),
            (Some("Octo"), None, false),
        ];
        for (client, user_agent, expected) in cases {
            assert_eq!(
                defaults.is_size_flag_client(client, user_agent),
                expected,
                "{client:?} {user_agent:?}"
            );
        }
        let listed = SubsonicSettings {
            size_flag_clients: " narjo , ,Amperfy".to_string(),
            ..Default::default()
        };
        assert!(listed.is_size_flag_client(Some("Narjo for iOS"), None));
        let off = SubsonicSettings {
            size_flag_clients: String::new(),
            ..Default::default()
        };
        assert!(!off.is_size_flag_client(Some("Amperfy"), Some("Amperfy/1")));
    }

    #[test]
    fn stream_size_warming_is_five_a_minute_by_default_and_zero_turns_it_off() {
        assert_eq!(
            SubsonicSettings::default().effective_stream_size_warm_per_minute(),
            5
        );
        for (configured, expected) in [(0, 0), (-3, 0), (12, 12), (500, 30)] {
            let s = SubsonicSettings {
                stream_size_warm_per_minute: configured,
                ..Default::default()
            };
            assert_eq!(
                s.effective_stream_size_warm_per_minute(),
                expected,
                "{configured}"
            );
        }
    }

    #[test]
    fn the_cover_cache_is_a_gigabyte_by_default_and_zero_turns_it_off() {
        assert_eq!(
            SubsonicSettings::default().effective_cover_cache_bytes(),
            1024 * 1024 * 1024
        );
        for (configured, expected) in [(0, 0), (-5, 0), (10, 10), (500_000, 102_400)] {
            let s = SubsonicSettings {
                cover_cache_mb: configured,
                ..Default::default()
            };
            assert_eq!(
                s.effective_cover_cache_bytes(),
                expected * 1024 * 1024,
                "{configured}"
            );
        }
    }

    #[test]
    fn search_waits_a_second_for_lengths_by_default_within_ten_seconds() {
        use std::time::Duration;
        assert_eq!(
            SubsonicSettings::default().effective_search_duration_wait(),
            Duration::from_millis(1000)
        );
        for (configured, expected) in [(0, 0), (-5, 0), (250, 250), (10_000, 10_000), (60_000, 10_000)] {
            let s = SubsonicSettings {
                search_duration_wait_ms: configured,
                ..Default::default()
            };
            assert_eq!(
                s.effective_search_duration_wait(),
                Duration::from_millis(expected),
                "{configured}"
            );
        }
        let off = SubsonicSettings {
            wait_for_search_durations: false,
            search_duration_wait_ms: 4000,
            ..Default::default()
        };
        assert_eq!(off.effective_search_duration_wait(), Duration::ZERO);
    }

    #[test]
    fn search_waits_a_second_for_last_fm_by_default_within_ten_seconds() {
        use std::time::Duration;
        assert_eq!(
            SubsonicSettings::default().effective_search_last_fm_wait(),
            Duration::from_millis(1000)
        );
        for (configured, expected) in [(0, 0), (-1, 0), (400, 400), (20_000, 10_000)] {
            let s = SubsonicSettings {
                search_last_fm_wait_ms: configured,
                ..Default::default()
            };
            assert_eq!(
                s.effective_search_last_fm_wait(),
                Duration::from_millis(expected),
                "{configured}"
            );
        }
    }

    #[test]
    fn sync_catalog_max_songs_is_clamped() {
        for (configured, expected) in [(1, 50), (1000, 1000), (99999, 5000)] {
            let s = SubsonicSettings {
                sync_catalog_max_songs: configured,
                ..Default::default()
            };
            assert_eq!(s.effective_sync_catalog_max_songs(), expected, "{configured}");
        }
    }
}
