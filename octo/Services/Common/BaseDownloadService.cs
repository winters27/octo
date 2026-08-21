using Microsoft.Extensions.Options;
using System.Collections.Concurrent;
using Octo.Models.Domain;
using Octo.Models.Settings;
using Octo.Models.Download;
using Octo.Models.Search;
using Octo.Models.Subsonic;
using Octo.Services.Local;
using Octo.Services.Subsonic;
using TagLib;
using IOFile = System.IO.File;

namespace Octo.Services.Common;

/// <summary>
/// Abstract base class for download services.
/// Implements common download logic, tracking, and metadata writing.
/// Subclasses implement provider-specific download and authentication logic.
/// </summary>
public abstract class BaseDownloadService : IDownloadService
{
    protected readonly IConfiguration Configuration;
    protected readonly ILocalLibraryService LocalLibraryService;
    protected readonly IMusicMetadataService MetadataService;
    // IOptionsMonitor, not a captured copy: this is a singleton, so the admin UI's
    // Download source / storage mode / folder structure changes would otherwise not
    // reach the download path until octorr restarted, while the admin UI itself
    // (which already reads through IOptionsMonitor) showed them as applied.
    private readonly IOptionsMonitor<SubsonicSettings> _subsonicOptions;
    protected SubsonicSettings SubsonicSettings => _subsonicOptions.CurrentValue;
    protected readonly ILogger Logger;
    private readonly IServiceProvider _serviceProvider;
    private readonly NavidromeIdentityService _navIdentity;
    private readonly DownloadHistoryService _history;
    protected readonly Octo.Services.Notifications.NotificationService Notifications;

    // The configured Library:DownloadPath. With auto-detect on this is only a
    // fallback used until Navidrome's real music folder is detected.
    private readonly string _configuredDownloadPath;

    /// <summary>
    /// Effective download destination. Resolves fresh each access so that once
    /// Navidrome's music folder is detected, downloads follow it without a restart.
    /// </summary>
    protected string DownloadPath => _navIdentity.EffectiveDownloadPath(_configuredDownloadPath);
    protected readonly string CachePath;
    
    // Concurrent because the album walk reads this WITHOUT holding DownloadLock while a
    // request thread can be writing to it under the lock. Every write used to be lock
    // guarded and the only unguarded reader was dead code; enabling album downloads makes
    // that pattern live, and concurrent read+write on Dictionary is undefined.
    protected readonly ConcurrentDictionary<string, DownloadInfo> ActiveDownloads = new();
    protected readonly SemaphoreSlim DownloadLock = new(1, 1);
    
    /// <summary>
    /// Lazy-loaded PlaylistSyncService to avoid circular dependency
    /// </summary>
    private PlaylistSyncService? _playlistSyncService;
    protected PlaylistSyncService? PlaylistSyncService
    {
        get
        {
            if (_playlistSyncService == null)
            {
                _playlistSyncService = _serviceProvider.GetService<PlaylistSyncService>();
            }
            return _playlistSyncService;
        }
    }
    
    /// <summary>
    /// Provider name (e.g., "deezer", "qobuz")
    /// </summary>
    protected abstract string ProviderName { get; }
    
    protected BaseDownloadService(
        IConfiguration configuration,
        ILocalLibraryService localLibraryService,
        IMusicMetadataService metadataService,
        IOptionsMonitor<SubsonicSettings> subsonicSettings,
        NavidromeIdentityService navIdentity,
        DownloadHistoryService history,
        Octo.Services.Notifications.NotificationService notifications,
        IServiceProvider serviceProvider,
        ILogger logger)
    {
        Configuration = configuration;
        LocalLibraryService = localLibraryService;
        MetadataService = metadataService;
        _subsonicOptions = subsonicSettings;
        _navIdentity = navIdentity;
        _history = history;
        Notifications = notifications;
        _serviceProvider = serviceProvider;
        Logger = logger;

        _configuredDownloadPath = configuration["Library:DownloadPath"] ?? "./downloads";
        CachePath = PathHelper.GetCachePath();

        // A drive-letter path inside a Linux container is a config mistake the
        // filesystem hides: CreateDirectory below happily makes a literal
        // directory named "E:\Media\Music" and downloads vanish into it.
        if (!OperatingSystem.IsWindows() && PathHelper.LooksLikeWindowsDrivePath(_configuredDownloadPath))
        {
            Logger.LogWarning(
                "Library:DownloadPath is the Windows path '{Path}' but this host is not Windows. "
                + "It will be treated as a literal directory name. Use the container path instead "
                + "(normally /music) and move the library via the DOWNLOAD_PATH bind mount.",
                _configuredDownloadPath);
        }

        if (!Directory.Exists(DownloadPath))
        {
            Directory.CreateDirectory(DownloadPath);
        }
        
        if (!Directory.Exists(CachePath))
        {
            Directory.CreateDirectory(CachePath);
        }
    }
    
    #region IDownloadService Implementation
    
    public async Task<string> DownloadSongAsync(string externalProvider, string externalId, CancellationToken cancellationToken = default)
    {
        return await DownloadSongInternalAsync(externalProvider, externalId, triggerAlbumDownload: true, cancellationToken);
    }
    
    public async Task<Stream> DownloadAndStreamAsync(string externalProvider, string externalId, CancellationToken cancellationToken = default)
    {
        var localPath = await DownloadSongInternalAsync(externalProvider, externalId, triggerAlbumDownload: true, cancellationToken);
        return IOFile.OpenRead(localPath);
    }
    
    public Task<string> ExecuteAcquisitionAsync(string externalProvider, string externalId,
        bool triggerAlbumDownload, bool forcePermanent, DownloadSource? sourceOverride,
        CancellationToken cancellationToken) =>
        DownloadSongInternalAsync(externalProvider, externalId, triggerAlbumDownload,
            cancellationToken, forcePermanent, sourceOverride: sourceOverride);

    public Task<bool> DownloadAlbumWithSourceAsync(string externalProvider, string albumExternalId,
        DownloadSource source, bool suppressSummary, CancellationToken cancellationToken = default)
    {
        if (externalProvider != ProviderName)
            return Task.FromResult(false);
        return DownloadRemainingAlbumTracksAsync(albumExternalId, "", source, suppressSummary,
            cancellationToken);
    }

    public DownloadInfo? GetDownloadStatus(string songId)
    {
        ActiveDownloads.TryGetValue(songId, out var info);
        return info;
    }
    
    public async Task<string?> GetLocalPathIfExistsAsync(string externalProvider, string externalId)
    {
        if (externalProvider != ProviderName)
        {
            return null;
        }
        
        // Check local library
        var localPath = await LocalLibraryService.GetLocalPathForExternalSongAsync(externalProvider, externalId);
        if (localPath != null && IOFile.Exists(localPath))
        {
            return localPath;
        }
        
        // Check cache directory
        var cachedPath = GetCachedFilePath(externalProvider, externalId);
        if (cachedPath != null && IOFile.Exists(cachedPath))
        {
            return cachedPath;
        }
        
        return null;
    }
    
    public abstract Task<bool> IsAvailableAsync();
    
    /// <summary>
    /// Gets a direct stream from the provider CDN (true streaming, no disk).
    /// Default implementation returns null (not supported). Override in subclasses.
    /// </summary>
    public virtual Task<DirectStreamInfo?> GetDirectStreamAsync(string externalProvider, string externalId, string? rangeHeader = null, CancellationToken cancellationToken = default)
    {
        return Task.FromResult<DirectStreamInfo?>(null);
    }
    
    public void DownloadRemainingAlbumTracksInBackground(string externalProvider, string albumExternalId, string excludeTrackExternalId)
    {
        if (externalProvider != ProviderName)
        {
            Logger.LogWarning("Provider '{Provider}' is not supported for album download", externalProvider);
            return;
        }

        _ = Task.Run(async () =>
        {
            try
            {
                await DownloadRemainingAlbumTracksAsync(albumExternalId, excludeTrackExternalId);
            }
            catch (Exception ex)
            {
                Logger.LogError(ex, "Failed to download remaining album tracks for album {AlbumId}", albumExternalId);
            }
        });
    }
    
    #endregion
    
    #region Template Methods (to be implemented by subclasses)
    
    /// <summary>
    /// Downloads a track and saves it to disk.
    /// Subclasses implement provider-specific logic (encryption, authentication, etc.)
    /// </summary>
    /// <param name="trackId">External track ID</param>
    /// <param name="song">Song metadata</param>
    /// <param name="suppressNotify">Mute per-track notifications (album walk, cache fills)</param>
    /// <param name="cancellationToken">Cancellation token</param>
    /// <returns>Local file path where the track was saved</returns>
    protected abstract Task<string> DownloadTrackAsync(string trackId, Song song, bool suppressNotify,
        DownloadSource? sourceOverride, CancellationToken cancellationToken);

    /// <summary>Record a completed download in the fetched-songs log. Best-effort:
    /// format + source are derived from the file extension (flac -> Soulseek/lossless,
    /// otherwise -> YouTube/lossy), which matches Octo's two download sources.</summary>
    private async Task RecordHistoryAsync(Song song, string localPath, bool suppressNotify)
    {
        try
        {
            string? cover = song.CoverArtUrlLarge ?? song.CoverArtUrl;
            string? album = string.IsNullOrEmpty(song.Album) ? null : song.Album;

            // The star path rebuilds the song from its id, so it has no artwork or
            // album. Pull the cover + album straight from Deezer for the log entry
            // (cached, so this is cheap). Best-effort — never fails a download.
            if (string.IsNullOrEmpty(cover) || album is null)
            {
                try
                {
                    var deezer = _serviceProvider.GetService<Octo.Services.Metadata.DeezerMetadataService>();
                    if (deezer != null)
                    {
                        var meta = await deezer.EnrichTrackAsync(song.Artist, song.Title, includeYear: false);
                        if (meta != null)
                        {
                            cover ??= meta.AlbumCoverUrl;
                            album ??= meta.AlbumTitle;
                        }
                    }
                }
                catch { /* enrichment is best-effort */ }
            }

            var ext = System.IO.Path.GetExtension(localPath).TrimStart('.').ToUpperInvariant();
            long size = 0;
            try { size = new FileInfo(localPath).Length; } catch { /* best-effort */ }
            _history.Record(new DownloadHistoryEntry
            {
                Artist = song.Artist,
                Title = song.Title,
                Album = album ?? string.Empty,
                Path = localPath,
                Format = string.IsNullOrEmpty(ext) ? "?" : ext,
                Source = ext == "FLAC" ? "Soulseek" : "YouTube",
                CoverArtUrl = cover,
                SizeBytes = size,
                DownloadedAt = DateTime.UtcNow.ToString("o"),
            });

            // Same chokepoint as the fetched-songs log, reusing the locals it just
            // assembled (Deezer-enriched cover and album included) — anything worth
            // logging is worth telling the user about, with the same data.
            if (!suppressNotify)
            {
                Notifications.Notify(new Octo.Services.Notifications.NotificationEvent
                {
                    Type = Octo.Services.Notifications.NotificationEventType.DownloadCompleted,
                    Artist = song.Artist,
                    Title = song.Title,
                    Album = album,
                    Format = string.IsNullOrEmpty(ext) ? "?" : ext,
                    Source = ext == "FLAC" ? "Soulseek" : "YouTube",
                    CoverArtUrl = cover,
                    SizeBytes = size,
                    // EnrichAndTagAsync ran before this hook, so these are the
                    // Deezer-enriched values the file itself was tagged with.
                    DurationSeconds = song.Duration,
                    Year = song.Year,
                });
            }
        }
        catch (Exception ex)
        {
            Logger.LogWarning(ex, "Failed to record download history for {Path}", localPath);
        }
    }

    /// <summary>
    /// Extracts the external album ID from the internal album ID format.
    /// Example: "ext-deezer-album-123456" -> "123456"
    /// </summary>
    protected abstract string? ExtractExternalIdFromAlbumId(string albumId);

    /// <summary>
    /// Re-assert any routing state a long-running batch depends on. An album download can
    /// run for hours while searches keep filling the id registry, so a track's routing can
    /// be evicted before its turn comes. Ids are a pure hash of the routing fields, so
    /// re-registering the SAME fields restores the SAME id and is idempotent.
    /// </summary>
    protected virtual void EnsureRoutingRegistered(Song track) { }
    
    #endregion
    
    #region Common Download Logic
    
    /// <summary>
    /// Internal method for downloading a song with control over album download triggering
    /// </summary>
    /// <param name="forcePermanent">
    /// Treat this as a permanent download regardless of Cache storage mode. Cache mode
    /// otherwise skips library registration, the fetched-songs log and the rescan, which
    /// is wrong for a deliberate "keep this" gesture like hearting an album.
    /// </param>
    /// <param name="suppressNotify">
    /// Mute per-track notifications. Set by the album walk, which fires one summary
    /// at the end instead of a ping per track.
    /// </param>
    protected async Task<string> DownloadSongInternalAsync(string externalProvider, string externalId,
        bool triggerAlbumDownload, CancellationToken cancellationToken = default,
        bool forcePermanent = false, bool suppressNotify = false,
        DownloadSource? sourceOverride = null)
    {
        if (externalProvider != ProviderName)
        {
            throw new NotSupportedException($"Provider '{externalProvider}' is not supported");
        }

        var songId = $"ext-{externalProvider}-{externalId}";
        var isCache = !forcePermanent && SubsonicSettings.StorageMode == StorageMode.Cache;
        // Cache-mode fills are background plumbing, not a user gesture: they skip the
        // fetched-songs log, so they skip notifications for the same reason.
        var silence = suppressNotify || isCache;
        
        // Acquire lock BEFORE checking existence to prevent race conditions with concurrent requests
        await DownloadLock.WaitAsync(cancellationToken);
        // The in-progress branch below releases early so it can wait without holding the
        // lock, and the finally would then release a second time. On a SemaphoreSlim(1,1)
        // that either throws from inside a finally, discarding a successful return, or
        // worse succeeds and lets two callers hold a mutex that permits one.
        var lockHeld = true;

        try
        {
            // Check if already downloaded (skip for cache mode as we want to check cache folder)
            if (!isCache)
            {
                var existingPath = await LocalLibraryService.GetLocalPathForExternalSongAsync(externalProvider, externalId);
                if (existingPath != null && IOFile.Exists(existingPath))
                {
                    Logger.LogInformation("Song already downloaded: {Path}", existingPath);
                    return existingPath;
                }
            }
            else
            {
                // For cache mode, check if file exists in cache directory
                var cachedPath = GetCachedFilePath(externalProvider, externalId);
                if (cachedPath != null && IOFile.Exists(cachedPath))
                {
                    Logger.LogInformation("Song found in cache: {Path}", cachedPath);
                    // Update file access time for cache cleanup logic
                    IOFile.SetLastAccessTime(cachedPath, DateTime.UtcNow);
                    return cachedPath;
                }
            }

            // Check if download in progress
            if (ActiveDownloads.TryGetValue(songId, out var activeDownload) && activeDownload.Status == DownloadStatus.InProgress)
            {
                Logger.LogInformation("Download already in progress for {SongId}, waiting...", songId);
                // Release lock while waiting
                DownloadLock.Release();
                lockHeld = false;

                while (ActiveDownloads.TryGetValue(songId, out activeDownload) && activeDownload.Status == DownloadStatus.InProgress)
                {
                    await Task.Delay(500, cancellationToken);
                }
                
                if (activeDownload?.Status == DownloadStatus.Completed && activeDownload.LocalPath != null)
                {
                    return activeDownload.LocalPath;
                }
                
                throw new Exception(activeDownload?.ErrorMessage ?? "Download failed");
            }

            // Get metadata
            // In Album mode, fetch the full album first to ensure AlbumArtist is correctly set
            Song? song = null;
            
            if (SubsonicSettings.DownloadMode == DownloadMode.Album)
            {
                // First try to get the song to extract album ID
                var tempSong = await MetadataService.GetSongAsync(externalProvider, externalId);
                if (tempSong != null && !string.IsNullOrEmpty(tempSong.AlbumId))
                {
                    var albumExternalId = ExtractExternalIdFromAlbumId(tempSong.AlbumId);
                    if (!string.IsNullOrEmpty(albumExternalId))
                    {
                        // Get full album with correct AlbumArtist
                        var album = await MetadataService.GetAlbumAsync(externalProvider, albumExternalId);
                        if (album != null)
                        {
                            // Find the track in the album
                            song = album.Songs.FirstOrDefault(s => s.ExternalId == externalId);
                        }
                    }
                }
            }
            
            // Fallback to individual song fetch if not in Album mode or album fetch failed
            if (song == null)
            {
                song = await MetadataService.GetSongAsync(externalProvider, externalId);
            }
            
            if (song == null)
            {
                throw new Exception("Song not found");
            }

            var downloadInfo = new DownloadInfo
            {
                SongId = songId,
                ExternalId = externalId,
                ExternalProvider = externalProvider,
                Status = DownloadStatus.InProgress,
                StartedAt = DateTime.UtcNow
            };
            ActiveDownloads[songId] = downloadInfo;

            // The TRANSFER is cancellable. Everything below it is the FINALIZE
            // phase and deliberately is not: once bytes exist on disk, tagging,
            // registration and the rescan must all run or the file becomes an
            // orphan that Octo has no record of. A client giving up on a slow
            // download used to abort exactly here, which is why a completed
            // download could never be played.
            var localPath = await DownloadTrackAsync(
                externalId, song, silence, sourceOverride, cancellationToken);

            downloadInfo.Status = DownloadStatus.Completed;
            downloadInfo.LocalPath = localPath;
            downloadInfo.CompletedAt = DateTime.UtcNow;

            song.LocalPath = localPath;

            // Enrich from Deezer and write rich tags + real album art onto the file.
            // Downloads otherwise arrive bare (YouTube: artist/title + a video
            // thumbnail; Soulseek: whatever the peer tagged), so this is what makes
            // every fetched song a properly-tagged library citizen.
            await EnrichAndTagAsync(song, localPath, CancellationToken.None);

            // Check if this track belongs to a playlist and update M3U
            if (PlaylistSyncService != null)
            {
                try
                {
                    var playlistId = PlaylistSyncService.GetPlaylistIdForTrack(songId);
                    if (playlistId != null)
                    {
                        Logger.LogInformation("Track {SongId} belongs to playlist {PlaylistId}, adding to M3U", songId, playlistId);
                        await PlaylistSyncService.AddTrackToM3UAsync(playlistId, song, localPath, isFullPlaylistDownload: false);
                    }
                }
                catch (Exception ex)
                {
                    Logger.LogWarning(ex, "Failed to update playlist M3U for track {SongId}", songId);
                }
            }
            
            // Only register and scan if NOT in cache mode
            if (!isCache)
            {
                await LocalLibraryService.RegisterDownloadedSongAsync(song, localPath);
                await RecordHistoryAsync(song, localPath, silence);

                // Trigger a Subsonic library rescan (with debounce)
                _ = Task.Run(async () =>
                {
                    try
                    {
                        await LocalLibraryService.TriggerLibraryScanAsync();
                    }
                    catch (Exception ex)
                    {
                        Logger.LogWarning(ex, "Failed to trigger library scan after download");
                    }
                });
                
                // If download mode is Album and triggering is enabled, start background download of remaining tracks
                if (triggerAlbumDownload && SubsonicSettings.DownloadMode == DownloadMode.Album && !string.IsNullOrEmpty(song.AlbumId))
                {
                    var albumExternalId = ExtractExternalIdFromAlbumId(song.AlbumId);
                    if (!string.IsNullOrEmpty(albumExternalId))
                    {
                        Logger.LogInformation("Download mode is Album, triggering background download for album {AlbumId}", albumExternalId);
                        _ = Task.Run(async () =>
                        {
                            try
                            {
                                await DownloadRemainingAlbumTracksAsync(
                                    albumExternalId, externalId, sourceOverride);
                            }
                            catch (Exception ex)
                            {
                                Logger.LogError(ex,
                                    "Failed to download remaining album tracks for album {AlbumId}",
                                    albumExternalId);
                            }
                        });
                    }
                }
            }
            else
            {
                Logger.LogInformation("Cache mode: skipping library registration and scan");
            }
            
            Logger.LogInformation("Download completed: {Path}", localPath);
            return localPath;
        }
        catch (Exception ex)
        {
            if (ActiveDownloads.TryGetValue(songId, out var downloadInfo))
            {
                downloadInfo.Status = DownloadStatus.Failed;
                downloadInfo.ErrorMessage = ex.Message;
            }
            Logger.LogError(ex, "Download failed for {SongId}", songId);
            throw;
        }
        finally
        {
            if (lockHeld) DownloadLock.Release();
        }
    }

    protected async Task<bool> DownloadRemainingAlbumTracksAsync(
        string albumExternalId, string excludeTrackExternalId,
        DownloadSource? sourceOverride = null, bool suppressSummary = false,
        CancellationToken cancellationToken = default)
    {
        Logger.LogInformation("Starting background download for album {AlbumId} (excluding track {TrackId})", 
            albumExternalId, excludeTrackExternalId);

        var album = await MetadataService.GetAlbumAsync(ProviderName, albumExternalId);
        if (album == null)
        {
            Logger.LogWarning("Album {AlbumId} not found, cannot download remaining tracks", albumExternalId);
            return false;
        }

        var tracksToDownload = album.Songs
            .Where(s => s.ExternalId != excludeTrackExternalId && !string.IsNullOrEmpty(s.ExternalId))
            .ToList();

        Logger.LogInformation("Found {Count} additional tracks to download for album '{AlbumTitle}'",
            tracksToDownload.Count, album.Title);

        // Per-track notifications are muted below; these feed one summary instead.
        int succeeded = 0, lossless = 0, failed = 0;

        foreach (var track in tracksToDownload)
        {
            try
            {
                EnsureRoutingRegistered(track);

                var existingPath = await LocalLibraryService.GetLocalPathForExternalSongAsync(ProviderName, track.ExternalId!);
                if (existingPath != null && IOFile.Exists(existingPath))
                {
                    Logger.LogDebug("Track {TrackId} already downloaded, skipping", track.ExternalId);
                    continue;
                }

                // Check if download is already in progress or recently completed
                var songId = $"ext-{ProviderName}-{track.ExternalId}";
                if (ActiveDownloads.TryGetValue(songId, out var activeDownload))
                {
                    if (activeDownload.Status == DownloadStatus.InProgress)
                    {
                        Logger.LogDebug("Track {TrackId} download already in progress, skipping", track.ExternalId);
                        continue;
                    }
                    
                    if (activeDownload.Status == DownloadStatus.Completed)
                    {
                        Logger.LogDebug("Track {TrackId} already downloaded in this session, skipping", track.ExternalId);
                        continue;
                    }
                }

                Logger.LogInformation("Downloading track '{Title}' from album '{Album}'", track.Title, album.Title);
                var path = await DownloadSongInternalAsync(
                    ProviderName, track.ExternalId!, triggerAlbumDownload: false,
                    cancellationToken, forcePermanent: true, suppressNotify: true,
                    sourceOverride: sourceOverride);
                succeeded++;
                if (path.EndsWith(".flac", StringComparison.OrdinalIgnoreCase)) lossless++;

                // Force a rescan per track so the album fills in progressively in the
                // client instead of appearing all at once at the end. The per-download
                // scan inside DownloadSongInternalAsync is debounced, which during a
                // batch swallows most triggers and can strand the final tracks entirely.
                await LocalLibraryService.TriggerLibraryScanAsync(force: true);
            }
            catch (Exception ex)
            {
                Logger.LogWarning(ex, "Failed to download track {TrackId} '{Title}'", track.ExternalId, track.Title);
                failed++;
            }
        }

        Logger.LogInformation("Completed background download for album '{AlbumTitle}'", album.Title);

        var summary = BuildAlbumSummary(album, succeeded, lossless, failed);
        // Hide an intermediate failure while another source remains, but still report
        // success when an earlier priority step completes the album acquisition.
        if ((!suppressSummary || failed == 0) && summary is not null) Notifications.Notify(summary);
        return failed == 0;
    }

    /// <summary>
    /// Null when the walk did no work — a re-star whose tracks are all already
    /// present must not ping the phone. Counts cover the walked tracks only; the
    /// track whose star triggered the walk got its own DownloadCompleted.
    /// </summary>
    internal static Octo.Services.Notifications.NotificationEvent? BuildAlbumSummary(
        Album album, int succeeded, int lossless, int failed)
        => succeeded + failed == 0 ? null : new Octo.Services.Notifications.NotificationEvent
        {
            Type = Octo.Services.Notifications.NotificationEventType.AlbumCompleted,
            Artist = album.Artist,
            Title = album.Title,
            CoverArtUrl = album.CoverArtUrl,
            TrackCount = succeeded,
            LosslessCount = lossless,
            FailedCount = failed,
        };
    
    #endregion
    
    #region Common Metadata Writing
    
    /// <summary>
    /// Writes ID3/Vorbis metadata and cover art to the audio file
    /// </summary>
    /// <summary>
    /// Fills any missing metadata on <paramref name="song"/> from Deezer, then writes
    /// full tags + real album art onto the downloaded file. Existing values win (a
    /// well-tagged Soulseek FLAC is enriched, not overwritten); Deezer fills the gaps
    /// and supplies the cover. Best-effort — a miss or write failure never breaks the
    /// download, the file just keeps whatever tags it already had.
    /// </summary>
    protected async Task EnrichAndTagAsync(Song song, string filePath, CancellationToken cancellationToken)
    {
        // Last.fm/YouTube titles often carry a redundant "Artist - " prefix (e.g.
        // "Radiohead - No Surprises") which both mislabels the file and breaks the
        // Deezer lookup. Strip it for the written title; strip bracketed junk too for
        // the lookup query so the match lands and we get real album art + tags.
        song.Title = StripArtistPrefix(song.Artist, song.Title);
        var queryTitle = StripBracketedJunk(song.Title);

        try
        {
            var deezer = _serviceProvider.GetService<Octo.Services.Metadata.DeezerMetadataService>();
            if (deezer != null)
            {
                var m = await deezer.EnrichTrackFullAsync(song.Artist, queryTitle, cancellationToken);
                if (m != null)
                {
                    if (string.IsNullOrEmpty(song.Album) && !string.IsNullOrEmpty(m.AlbumTitle)) song.Album = m.AlbumTitle;
                    if (string.IsNullOrEmpty(song.AlbumArtist) && !string.IsNullOrEmpty(m.ArtistName)) song.AlbumArtist = m.ArtistName;
                    if (string.IsNullOrEmpty(song.CoverArtUrlLarge)) song.CoverArtUrlLarge = m.AlbumCoverUrl;
                    if (!song.Year.HasValue) song.Year = m.Year;
                    if (!song.Track.HasValue) song.Track = m.TrackNumber;
                    if (!song.DiscNumber.HasValue) song.DiscNumber = m.DiscNumber;
                    if (!song.TotalTracks.HasValue) song.TotalTracks = m.TotalTracks;
                    if (!song.Duration.HasValue) song.Duration = m.Duration;
                    if (string.IsNullOrEmpty(song.Genre)) song.Genre = m.Genre;
                    if (string.IsNullOrEmpty(song.Isrc)) song.Isrc = m.Isrc;
                    if (string.IsNullOrEmpty(song.Label)) song.Label = m.Label;
                    if (string.IsNullOrEmpty(song.ReleaseDate)) song.ReleaseDate = m.ReleaseDate;
                }
            }
        }
        catch (Exception ex)
        {
            Logger.LogWarning(ex, "Deezer enrichment for tagging failed for '{Artist} - {Title}'", song.Artist, song.Title);
        }

        await WriteMetadataAsync(filePath, song, cancellationToken);
    }

    /// <summary>Drop a redundant leading "Artist - " from a track title.</summary>
    private static string StripArtistPrefix(string? artist, string? title)
    {
        var t = (title ?? string.Empty).Trim();
        var a = (artist ?? string.Empty).Trim();
        if (a.Length > 0 && t.StartsWith(a + " - ", StringComparison.OrdinalIgnoreCase))
            t = t[(a.Length + 3)..].Trim();
        return t;
    }

    /// <summary>Strip [bracketed] / (parenthesized) annotations for a cleaner Deezer
    /// query (e.g. "No Surprises (Official Video)" -> "No Surprises"). Only used for
    /// the lookup, not the written title, so real "(feat. …)" tags are preserved.</summary>
    private static string StripBracketedJunk(string title)
    {
        var stripped = System.Text.RegularExpressions.Regex
            .Replace(title ?? string.Empty, @"\s*[\[\(][^\]\)]*[\]\)]", "").Trim();
        // A title that is entirely an annotation ("(Exchange)") strips to nothing, which
        // would send an empty query to Deezer. Fall back to the original.
        return stripped.Length == 0 ? (title ?? string.Empty).Trim() : stripped;
    }

    protected async Task WriteMetadataAsync(string filePath, Song song, CancellationToken cancellationToken)
    {
        try
        {
            Logger.LogInformation("Writing metadata to: {Path}", filePath);
            
            using var tagFile = TagLib.File.Create(filePath);
            
            // Basic metadata. Title/artist we always have; only overwrite album +
            // album-artist when we actually resolved them, so a well-tagged Soulseek
            // FLAC keeps its own album if Deezer had no match.
            if (!string.IsNullOrEmpty(song.Title)) tagFile.Tag.Title = song.Title;
            if (!string.IsNullOrEmpty(song.Artist)) tagFile.Tag.Performers = new[] { song.Artist };
            if (!string.IsNullOrEmpty(song.Album)) tagFile.Tag.Album = song.Album;
            if (!string.IsNullOrEmpty(song.AlbumArtist))
                tagFile.Tag.AlbumArtists = new[] { song.AlbumArtist };
            else if (!string.IsNullOrEmpty(song.Artist))
                tagFile.Tag.AlbumArtists = new[] { song.Artist };
            
            // Only write the track number when we actually have one, and only pair
            // the total with it — avoids a bogus "0/11" when Deezer's search result
            // carried the album total but not this track's position.
            if (song.Track is > 0)
            {
                tagFile.Tag.Track = (uint)song.Track.Value;
                if (song.TotalTracks.HasValue)
                    tagFile.Tag.TrackCount = (uint)song.TotalTracks.Value;
            }
            
            if (song.DiscNumber.HasValue)
                tagFile.Tag.Disc = (uint)song.DiscNumber.Value;
            
            if (song.Year.HasValue)
                tagFile.Tag.Year = (uint)song.Year.Value;
            
            if (!string.IsNullOrEmpty(song.Genre))
                tagFile.Tag.Genres = new[] { song.Genre };
            
            if (song.Bpm.HasValue)
                tagFile.Tag.BeatsPerMinute = (uint)song.Bpm.Value;
            
            if (song.Contributors.Count > 0)
                tagFile.Tag.Composers = song.Contributors.ToArray();
            
            if (!string.IsNullOrEmpty(song.Copyright))
                tagFile.Tag.Copyright = song.Copyright;
            
            var comments = new List<string>();
            if (!string.IsNullOrEmpty(song.Isrc))
                comments.Add($"ISRC: {song.Isrc}");
            
            if (comments.Count > 0)
                tagFile.Tag.Comment = string.Join(" | ", comments);
            
            // Download and embed cover art
            var coverUrl = song.CoverArtUrlLarge ?? song.CoverArtUrl;
            if (!string.IsNullOrEmpty(coverUrl))
            {
                try
                {
                    var coverData = await DownloadCoverArtAsync(coverUrl, cancellationToken);
                    if (coverData != null && coverData.Length > 0)
                    {
                        var mimeType = coverUrl.Contains(".png") ? "image/png" : "image/jpeg";
                        var picture = new TagLib.Picture
                        {
                            Type = TagLib.PictureType.FrontCover,
                            MimeType = mimeType,
                            Description = "Cover",
                            Data = new TagLib.ByteVector(coverData)
                        };
                        tagFile.Tag.Pictures = new TagLib.IPicture[] { picture };
                        Logger.LogInformation("Cover art embedded: {Size} bytes", coverData.Length);
                    }
                }
                catch (Exception ex)
                {
                    Logger.LogWarning(ex, "Failed to download cover art from {Url}", coverUrl);
                }
            }
            
            tagFile.Save();
            Logger.LogInformation("Metadata written successfully to: {Path}", filePath);
        }
        catch (Exception ex)
        {
            Logger.LogError(ex, "Failed to write metadata to: {Path}", filePath);
        }
    }
    
    /// <summary>
    /// Downloads cover art from a URL
    /// </summary>
    protected async Task<byte[]?> DownloadCoverArtAsync(string url, CancellationToken cancellationToken)
    {
        try
        {
            using var httpClient = new HttpClient();
            var response = await httpClient.GetAsync(url, cancellationToken);
            response.EnsureSuccessStatusCode();
            return await response.Content.ReadAsByteArrayAsync(cancellationToken);
        }
        catch (Exception ex)
        {
            Logger.LogWarning(ex, "Failed to download cover art from {Url}", url);
            return null;
        }
    }
    
    #endregion
    
    #region Utility Methods
    
    /// <summary>
    /// Ensures a directory exists, creating it and all parent directories if necessary
    /// </summary>
    protected void EnsureDirectoryExists(string path)
    {
        try
        {
            if (!Directory.Exists(path))
            {
                Directory.CreateDirectory(path);
                Logger.LogDebug("Created directory: {Path}", path);
            }
        }
        catch (Exception ex)
        {
            Logger.LogError(ex, "Failed to create directory: {Path}", path);
            throw;
        }
    }
    
    /// <summary>
    /// Gets the cached file path for a given provider and external ID
    /// Returns null if no cached file exists
    /// </summary>
    protected string? GetCachedFilePath(string provider, string externalId)
    {
        try
        {
            // Search for cached files matching the pattern: {provider}_{externalId}.*
            var pattern = $"{provider}_{externalId}.*";
            var files = Directory.GetFiles(CachePath, pattern, SearchOption.AllDirectories);
            
            if (files.Length > 0)
            {
                return files[0]; // Return first match
            }
            
            return null;
        }
        catch (Exception ex)
        {
            Logger.LogWarning(ex, "Failed to search for cached file: {Provider}_{ExternalId}", provider, externalId);
            return null;
        }
    }
    
    #endregion
}
