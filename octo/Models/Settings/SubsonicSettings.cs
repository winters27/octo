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

public class SubsonicSettings
{
    public string? Url { get; set; }
    
    /// <summary>
    /// Explicit content filter mode (default: All)
    /// Environment variable: EXPLICIT_FILTER
    /// Values: "All", "ExplicitOnly", "CleanOnly"
    /// Note: Only works with Deezer
    /// </summary>
    public ExplicitFilter ExplicitFilter { get; set; } = ExplicitFilter.All;
    
    /// <summary>
    /// Download mode for tracks (default: Track)
    /// Environment variable: DOWNLOAD_MODE
    /// Values: "Track" (download only played track), "Album" (download full album when playing a track)
    /// </summary>
    public DownloadMode DownloadMode { get; set; } = DownloadMode.Track;
    
    /// <summary>
    /// Storage mode for downloaded files (default: Permanent)
    /// Environment variable: STORAGE_MODE
    /// Values: "Permanent" (files saved to library), "Cache" (temporary files, auto-cleanup)
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
    /// Folder structure for downloaded tracks (default: Flat)
    /// Environment variable: FOLDER_STRUCTURE
    /// Values: "Organized" (Artist/Album/Track.flac), "Flat" (Artist - Title.flac)
    /// </summary>
    public FolderStructure FolderStructure { get; set; } = FolderStructure.Flat;
    
    /// <summary>
    /// Use local staging for cloud storage mounts (default: false)
    /// Environment variable: USE_LOCAL_STAGING
    /// When enabled, downloads go to local temp first, metadata is written there,
    /// then the file is moved to the final destination. Required for FUSE/rclone mounts
    /// where TagLib cannot write metadata directly.
    /// </summary>
    public bool UseLocalStaging { get; set; } = false;

}
