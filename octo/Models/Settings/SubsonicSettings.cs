namespace Octo.Models.Settings;

/// <summary>
/// Download mode for tracks
/// </summary>
public enum DownloadMode
{
    /// <summary>
    /// Download only the requested track (default behavior)
    /// </summary>
    Track,
    
    /// <summary>
    /// When a track is played, download the entire album in background
    /// The requested track is downloaded first, then remaining tracks are queued
    /// </summary>
    Album
}

/// <summary>
/// Explicit content filter mode for Deezer tracks
/// </summary>
public enum ExplicitFilter
{
    /// <summary>
    /// Show all tracks (no filtering)
    /// </summary>
    All,
    
    /// <summary>
    /// Exclude clean/edited versions (explicit_content_lyrics == 3)
    /// Shows original explicit content and naturally clean content
    /// </summary>
    ExplicitOnly,
    
    /// <summary>
    /// Only show clean content (explicit_content_lyrics == 0 or 3)
    /// Excludes tracks with explicit_content_lyrics == 1
    /// </summary>
    CleanOnly
}

/// <summary>
/// Storage mode for downloaded tracks
/// </summary>
public enum StorageMode
{
    /// <summary>
    /// Files are permanently stored in the library and registered in the database
    /// </summary>
    Permanent,
    
    /// <summary>
    /// Files are stored in a temporary cache and automatically cleaned up
    /// Not registered in the database, no Navidrome scan triggered
    /// </summary>
    Cache,
    
    /// <summary>
    /// True streaming mode - audio is proxied directly without saving to disk
    /// Lowest latency, no disk I/O, but re-fetches on each play
    /// </summary>
    Stream
}

/// <summary>
/// Folder structure for downloaded tracks
/// </summary>
public enum FolderStructure
{
    /// <summary>
    /// Organized folder structure: Artist/Album/XX - Track.flac
    /// Better for large libraries with album-based organization
    /// </summary>
    Organized,
    
    /// <summary>
    /// Flat file structure: Artist - Title.flac (all files in root)
    /// Better for simple libraries without nested folders
    /// </summary>
    Flat
}

/// <summary>
/// Where a starred track's permanent copy comes from.
/// </summary>
public enum DownloadSource
{
    /// <summary>Lossless FLAC via Soulseek/slskd (default).</summary>
    Soulseek,

    /// <summary>Lossy MP3 via the yt-dlp shim.</summary>
    YouTube,

    /// <summary>Try Soulseek FLAC first; fall back to YouTube MP3 if it fails.</summary>
    SoulseekThenYouTube,

    /// <summary>
    /// Submit external track/album hearts to an existing Lidarr instance. Lidarr is
    /// album-oriented, so a track heart acquires the track's full album. Non-heart
    /// permanent downloads continue to use Soulseek.
    /// </summary>
    Lidarr
}

/// <summary>A source that can participate in the ordered heart-acquisition chain.</summary>
public enum HeartDownloadSource
{
    Soulseek,
    YouTube,
    Lidarr,
}

public sealed class HeartDownloadStep
{
    public HeartDownloadSource Source { get; set; }
    /// <summary>Legacy single switch; used only when the per-heart switches are absent.</summary>
    public bool? Enabled { get; set; }
    public bool? SongEnabled { get; set; }
    public bool? AlbumEnabled { get; set; }
}

public class SubsonicSettings
{
    public string? Url { get; set; }

    /// <summary>
    /// Optional Navidrome admin username. When set (with AdminPassword), Octo can
    /// authenticate to Navidrome for background work it does as a proxy: detecting
    /// the music folder and triggering an authenticated rescan. If left empty, Octo
    /// falls back to an admin token captured from a client's relayed native login.
    /// Environment variable: SUBSONIC__ADMINUSERNAME
    /// </summary>
    public string? AdminUsername { get; set; }

    /// <summary>
    /// Optional Navidrome admin password paired with AdminUsername. See AdminUsername.
    /// Environment variable: SUBSONIC__ADMINPASSWORD
    /// </summary>
    public string? AdminPassword { get; set; }

    /// <summary>
    /// Auto-detect the download destination from Navidrome's own music folder
    /// (default: true). Octo is a proxy in front of Navidrome, so downloads should
    /// land where Navidrome scans. When on, the effective download path is the
    /// folder reported by Navidrome's /api/library; the Library:DownloadPath value
    /// becomes a fallback used only until detection succeeds (or if it never does).
    /// Turn off to always use Library:DownloadPath verbatim.
    /// Environment variable: SUBSONIC__AUTODETECTDOWNLOADPATH
    /// </summary>
    public bool AutoDetectDownloadPath { get; set; } = true;

    /// <summary>
    /// Which Navidrome library to download into, given as its folder path, for
    /// servers that serve more than one. Empty (the default) keeps the historical
    /// behaviour of taking the first library Navidrome reports. A value that no
    /// longer matches any reported library is ignored with a warning rather than
    /// leaving downloads with nowhere to go.
    /// Environment variable: SUBSONIC__LIBRARYPATH
    /// </summary>
    public string LibraryPath { get; set; } = "";

    /// <summary>
    /// Explicit content filter mode (default: All)
    /// Environment variable: EXPLICIT_FILTER
    /// Values: "All", "ExplicitOnly", "CleanOnly"
    /// Note: Only works with Deezer
    /// </summary>
    public ExplicitFilter ExplicitFilter { get; set; } = ExplicitFilter.All;
    
    /// <summary>
    /// Legacy direct-download mode (default: Track), retained for playlist jobs.
    /// Environment variable: DOWNLOAD_MODE
    /// Values: "Track" or "Album"
    /// </summary>
    public DownloadMode DownloadMode { get; set; } = DownloadMode.Track;
    
    /// <summary>
    /// Legacy storage mode for direct-download jobs (default: Permanent).
    /// Environment variable: STORAGE_MODE
    /// Ordinary external playback always streams from YouTube unless lossless waiting is enabled.
    /// </summary>
    public StorageMode StorageMode { get; set; } = StorageMode.Permanent;
    
    /// <summary>
    /// Cache duration in hours for Cache storage mode (default: 1)
    /// Environment variable: CACHE_DURATION_HOURS
    /// Files older than this duration will be automatically deleted
    /// Only applies when StorageMode is Cache
    /// </summary>
    public int CacheDurationHours { get; set; } = 1;
    
    /// <summary>
    /// Enable external playlist search and streaming (default: true)
    /// Environment variable: ENABLE_EXTERNAL_PLAYLISTS
    /// When enabled, users can search for playlists from the configured music provider
    /// Playlists appear as "albums" in search results with genre "Playlist"
    /// </summary>
    public bool EnableExternalPlaylists { get; set; } = true;
    
    /// <summary>
    /// Directory name for storing playlist .m3u files (default: "playlists")
    /// Environment variable: PLAYLISTS_DIRECTORY
    /// Relative to the music library root directory
    /// Playlist files will be stored in {MusicDirectory}/{PlaylistsDirectory}/
    /// </summary>
    public string PlaylistsDirectory { get; set; } = "playlists";
    
    /// <summary>
    /// Auto-download tracks when starred (default: true)
    /// Environment variable: DOWNLOAD_ON_STAR
    /// When enabled in Stream/Cache mode, starring a track triggers permanent download
    /// </summary>
    public bool DownloadOnStar { get; set; } = true;

    /// <summary>
    /// Auto-download every track when a whole album is starred (default: true)
    /// Environment variable: DOWNLOAD_ALBUM_ON_STAR
    /// Works in every storage mode. Downloads run one at a time, so a full album is a
    /// long job; turn this off to keep song-starring without the larger commitment.
    /// </summary>
    public bool DownloadAlbumOnStar { get; set; } = true;

    /// <summary>
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
    /// waits that long — turning it on means the first play of each track appears to fail
    /// while the file lands in the background.
    /// </summary>
    public bool WaitForLosslessOnPlay { get; set; } = false;

    /// <summary>
    /// With WaitForLosslessOnPlay on, give up waiting after this many seconds and fall
    /// back to the lossy preview while the fetch finishes in the background (default: 0,
    /// wait as long as the fetch needs).
    /// Environment variable: LOSSLESS_WAIT_TIMEOUT_SECONDS
    ///
    /// A fallback serves lossy bytes under an id this session declared lossless, which
    /// strict clients can refuse to start. That trade is why it is opt-in and 0 keeps
    /// the declared contract exact.
    /// </summary>
    public int LosslessWaitTimeoutSeconds { get; set; } = 0;

    /// <summary>
    /// Folder structure for downloaded tracks (default: Flat)
    /// Environment variable: FOLDER_STRUCTURE
    /// Values: "Organized" (Artist/Album/Track.flac), "Flat" (Artist - Title.flac)
    /// </summary>
    public FolderStructure FolderStructure { get; set; } = FolderStructure.Flat;

    /// <summary>
    /// Download source (default: Soulseek). Lidarr applies to hearts only.
    /// Environment variable: DOWNLOAD_SOURCE
    /// Values: "Soulseek" (FLAC), "YouTube" (MP3), "SoulseekThenYouTube"
    /// (FLAC with MP3 fallback), or "Lidarr" (heart-only, full album).
    /// </summary>
    public DownloadSource DownloadSource { get; set; } = DownloadSource.Soulseek;

    /// <summary>
    /// Ordered sources for explicit track and album hearts. Empty keeps older
    /// DOWNLOAD_SOURCE configurations working; Lidarr remains last by default.
    /// </summary>
    public List<HeartDownloadStep> HeartDownloadSources { get; set; } = [];

    public IReadOnlyList<HeartDownloadStep> EffectiveHeartDownloadSources()
    {
        var configured = HeartDownloadSources
            .Where(step => Enum.IsDefined(step.Source))
            .GroupBy(step => step.Source)
            .Select(group =>
            {
                var step = group.First();
                return new HeartDownloadStep
                {
                    Source = step.Source,
                    SongEnabled = step.SongEnabled ?? step.Enabled ?? false,
                    AlbumEnabled = step.AlbumEnabled ?? step.Enabled ?? false,
                };
            })
            .ToList();
        if (configured.Count > 0)
        {
            foreach (var source in Enum.GetValues<HeartDownloadSource>())
                if (configured.All(step => step.Source != source))
                    configured.Add(new HeartDownloadStep
                    {
                        Source = source,
                        SongEnabled = false,
                        AlbumEnabled = false,
                    });
            return configured;
        }

        return DownloadSource switch
        {
            DownloadSource.YouTube => DefaultHeartSources(false, true, false),
            DownloadSource.SoulseekThenYouTube => DefaultHeartSources(true, true, false),
            DownloadSource.Lidarr => DefaultHeartSources(false, false, true),
            _ => DefaultHeartSources(true, false, false),
        };
    }

    private IReadOnlyList<HeartDownloadStep> DefaultHeartSources(
        bool soulseek, bool youtube, bool lidarr) =>
        [
            new() { Source = HeartDownloadSource.Soulseek, SongEnabled = soulseek && DownloadOnStar, AlbumEnabled = soulseek && DownloadAlbumOnStar },
            new() { Source = HeartDownloadSource.YouTube, SongEnabled = youtube && DownloadOnStar, AlbumEnabled = youtube && DownloadAlbumOnStar },
            new() { Source = HeartDownloadSource.Lidarr, SongEnabled = lidarr && DownloadOnStar, AlbumEnabled = lidarr && DownloadAlbumOnStar },
        ];
    
    /// <summary>
    /// Use local staging for cloud storage mounts (default: false)
    /// Environment variable: USE_LOCAL_STAGING
    /// When enabled, downloads go to local temp first, metadata is written there,
    /// then the file is moved to the final destination. Required for FUSE/rclone mounts
    /// where TagLib cannot write metadata directly.
    /// </summary>
    public bool UseLocalStaging { get; set; } = false;

}
