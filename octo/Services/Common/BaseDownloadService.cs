using Microsoft.Extensions.Options;
using System.Collections.Concurrent;
using Octo.Models.Domain;
using Octo.Models.Settings;
using Octo.Models.Download;
using Octo.Models.Search;
using Octo.Models.Subsonic;
using Octo.Services.Audio;
using Octo.Services.Local;
using Octo.Services.Metadata;
using Octo.Services.Subsonic;
using Octo.Services.Tagging;
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
    // reach the download path until Octo restarted, while the admin UI itself
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

    private readonly IOptionsMonitor<GenreSettings> _genreOptions;

    /// <summary>Read through the monitor, never captured: a settings change has to reach a
    /// singleton without a restart.</summary>
    protected GenreSettings GenreSettings => _genreOptions.CurrentValue;

    /// <summary>Resolved per read for the same reason; through the provider so the constructor
    /// every subclass calls stays as it is.</summary>
    private SoulseekSettings SoulseekSettingsValue =>
        _serviceProvider.GetService<IOptionsMonitor<SoulseekSettings>>()?.CurrentValue ?? new SoulseekSettings();
    private MetadataSettings MetadataSettingsValue =>
        _serviceProvider.GetService<IOptionsMonitor<MetadataSettings>>()?.CurrentValue ?? new MetadataSettings();

    /// <summary>How many transfers may run at once, and the gate they share. Null in tests that
    /// build a service without one, which is one at a time, as before.</summary>
    protected DownloadConcurrency? Concurrency => _serviceProvider.GetService<DownloadConcurrency>();

    /// <summary>The Soulseek settings as they are now, for switches a subclass reads live.</summary>
    protected SoulseekSettings CurrentSoulseekSettings => SoulseekSettingsValue;

    /// <summary>For a subclass that needs an optional service without changing this constructor.</summary>
    protected T? OptionalService<T>() where T : class => _serviceProvider.GetService<T>();

    // The library file the running replacement replaces. Flows with the call into
    // DownloadTrackAsync, so a backend that can use it (Lidarr reads the album from its tags)
    // sees it without a new parameter on every download.
    private static readonly AsyncLocal<string?> ReplacingPathLocal = new();

    /// <summary>The library file this download replaces, or null for an ordinary download.</summary>
    protected static string? ReplacingPath => ReplacingPathLocal.Value;

    /// <summary>
    /// The source a backend fetched a song from, when the format alone does not say: a FLAC
    /// can be Lidarr's as well as Soulseek's. Weak, so a finished song takes its entry with it.
    /// </summary>
    protected static readonly System.Runtime.CompilerServices.ConditionalWeakTable<Song, string> FetchedFrom = new();

    /// <summary>Songs a backend asked to land without a notice of their own: a Lidarr album's
    /// songs that nobody hearted, and the songs of an album heart, which gets one for the album.</summary>
    protected static readonly System.Runtime.CompilerServices.ConditionalWeakTable<Song, object> Muted = new();

    private static string SourceLabel(Song song, string ext) =>
        FetchedFrom.TryGetValue(song, out var source) ? source : ext == "FLAC" ? "Soulseek" : "YouTube";

    /// <summary>
    /// Tell the live progress list where a download has got to. It only watches, so it is
    /// resolved per use through the provider like the settings above, and whatever it throws
    /// stays here: a bookkeeping error must never fail or slow a download.
    /// </summary>
    protected void Track(Action<AcquisitionTracker> step)
    {
        try
        {
            if (_serviceProvider.GetService<AcquisitionTracker>() is { } tracker) step(tracker);
        }
        catch (Exception ex)
        {
            Logger.LogDebug("Acquisition tracker skipped a step: {Message}", ex.Message);
        }
    }

    /// <summary>The name a download was asked for under, captured before anything corrects it.</summary>
    protected internal sealed record RequestedIdentity(string Artist, string Title, string Album, int? Track);

    /// <summary>What a download's path is built from.</summary>
    internal sealed record LayoutChoice(string FolderArtist, string FileArtist, string Title, string Album, int? Track);

    /// <summary>Where a download was placed, and whether its folder is new, which is the only
    /// place a cover.jpg may go without changing an album that was already there.</summary>
    protected internal sealed record Placement(string Path, bool CreatedFolder);
    
    protected BaseDownloadService(
        IConfiguration configuration,
        ILocalLibraryService localLibraryService,
        IMusicMetadataService metadataService,
        IOptionsMonitor<SubsonicSettings> subsonicSettings,
        IOptionsMonitor<GenreSettings> genreSettings,
        NavidromeIdentityService navIdentity,
        DownloadHistoryService history,
        Octo.Services.Notifications.NotificationService notifications,
        IServiceProvider serviceProvider,
        ILogger logger)
    {
        Configuration = configuration;
        _genreOptions = genreSettings;
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
        CancellationToken cancellationToken, IReadOnlyList<string>? requestedBy = null,
        bool upgradeSearch = false, Octo.Services.Library.ReplacementHandoff? replacement = null) =>
        DownloadSongInternalAsync(externalProvider, externalId, triggerAlbumDownload,
            cancellationToken, forcePermanent, sourceOverride: sourceOverride,
            requestedBy: requestedBy, upgradeSearch: upgradeSearch, replacement: replacement);

    public Task<bool> DownloadAlbumWithSourceAsync(string externalProvider, string albumExternalId,
        DownloadSource source, bool suppressSummary, CancellationToken cancellationToken = default,
        IReadOnlyList<string>? requestedBy = null)
    {
        if (externalProvider != ProviderName)
            return Task.FromResult(false);
        return DownloadRemainingAlbumTracksAsync(albumExternalId, "", source, suppressSummary,
            cancellationToken, requestedBy);
    }

    public DownloadInfo? GetDownloadStatus(string songId)
    {
        ActiveDownloads.TryGetValue(songId, out var info);
        return info;
    }

    public bool HasActiveDownloads => ActiveDownloads.Values.Any(info => info.Status == DownloadStatus.InProgress);
    
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
        DownloadSource? sourceOverride, bool upgradeSearch, CancellationToken cancellationToken);

    /// <summary>Record a completed download in the fetched-songs log. Best-effort:
    /// format + source are derived from the file extension (flac -> Soulseek/lossless,
    /// otherwise -> YouTube/lossy), which matches Octo's two download sources.</summary>
    private async Task RecordHistoryAsync(Song song, string localPath, bool suppressNotify,
        IReadOnlyList<string>? requestedBy = null)
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
                Source = SourceLabel(song, ext),
                CoverArtUrl = cover,
                SizeBytes = size,
                TranscodedFrom = song.TranscodedFrom,
                Tagging = song.TagPlan?.ToReport(),
                DownloadedAt = DateTime.UtcNow.ToString("o"),
                RequestedBy = requestedBy is { Count: > 0 } ? [.. requestedBy] : null,
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
                    Source = SourceLabel(song, ext),
                    CoverArtUrl = cover,
                    SizeBytes = size,
                    // EnrichAsync and WriteMetadataAsync ran before this hook, so these are the
                    // Deezer-enriched values the file itself was tagged with.
                    DurationSeconds = song.Duration,
                    Year = song.Year,
                    RequestedBy = requestedBy is { Count: > 0 } ? requestedBy : null,
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
        DownloadSource? sourceOverride = null, IReadOnlyList<string>? requestedBy = null,
        AlbumTagContext? albumContext = null, bool upgradeSearch = false,
        Octo.Services.Library.ReplacementHandoff? replacement = null)
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
            // A replacement is always a fresh file: an existing one is what is being replaced.
            if (!isCache && replacement is null)
            {
                var existingPath = await LocalLibraryService.GetLocalPathForExternalSongAsync(externalProvider, externalId);
                if (existingPath != null && IOFile.Exists(existingPath))
                {
                    Logger.LogInformation("Song already downloaded: {Path}", existingPath);
                    Track(t => t.Imported(externalProvider, externalId, null, null, existingPath));
                    return existingPath;
                }
            }
            else if (isCache)
            {
                // For cache mode, check if file exists in cache directory
                var cachedPath = GetCachedFilePath(externalProvider, externalId);
                if (cachedPath != null && IOFile.Exists(cachedPath))
                {
                    Logger.LogInformation("Song found in cache: {Path}", cachedPath);
                    // Update file access time for cache cleanup logic
                    IOFile.SetLastAccessTime(cachedPath, DateTime.UtcNow);
                    Track(t => t.Complete(externalProvider, externalId));
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

            // The worker has it now. Looking the song up is the first part of the search.
            Track(t => t.Stage(externalProvider, externalId, AcquisitionState.Searching));

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
            Track(t => t.Describe(externalProvider, externalId, song.Artist, song.Title, song.Album));

            // Never a second copy of a song already in the library; a lossy one is queued for a
            // higher quality copy instead. Not for a replacement or a Better quality search, which
            // exist to download a song that is already there.
            if (!isCache && replacement is null && !upgradeSearch && SubsonicSettings.SkipOwnedSongs
                && OptionalService<Octo.Services.Library.LibraryOwnership>() is { } ownership
                && await ownership.FindAsync(song.Artist, song.Title, song.Duration, song.Album, cancellationToken) is { } owned)
            {
                var decision = Octo.Services.Library.LibraryOwnership.Decide(owned,
                    SourceCanBeLossless(sourceOverride), UpgradeAllowed(requestedBy));
                if (decision == Octo.Services.Library.OwnedDecision.KeepAndUpgrade) QueueUpgradeFor(owned, song, requestedBy!);
                Logger.LogInformation("'{Artist} - {Title}' is already in your library ({Suffix}) at {Path}; not downloading another copy{Upgrade}",
                    song.Artist, song.Title, owned.Suffix, owned.AbsolutePath,
                    decision == Octo.Services.Library.OwnedDecision.KeepAndUpgrade ? ", looking for a higher quality one instead" : "");
                Track(t => t.Imported(externalProvider, externalId, song.Artist, song.Title, owned.AbsolutePath));
                return owned.AbsolutePath;
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
            // A user who asked for this track to be removed meant it. Resolved through the
            // service provider rather than the constructor, because the journal's owner depends
            // on the acquisition queue this class sits underneath.
            var actionJournal = _serviceProvider
                .GetService<Octo.Services.Library.LibraryActionJournal>();
            if (actionJournal?.IsNeverRequested(song.Artist, song.Title) == true)
            {
                Logger.LogInformation(
                    "Skipping '{Artist} - {Title}': it was removed with a library action, so it is "
                    + "not requested again. Clear that entry from the dashboard to allow it.",
                    song.Artist, song.Title);
                throw new InvalidOperationException(
                    $"'{song.Artist} - {song.Title}' was deleted with a library action");
            }

            // Snapshot before anything can correct it: with NameFromMatch off this is still what
            // names the file, which is how every existing library was built.
            var requested = new RequestedIdentity(song.Artist, song.Title, song.Album ?? "", song.Track);

            // Parallel only once slskd has proven it files each download in its own folder; until
            // then the lock is held through the transfer exactly as before. The in-progress marker
            // above keeps a second request for this song waiting either way, and the limiter counts
            // every transfer, album walks and hearts outside the queue included.
            var concurrency = Concurrency;
            IDisposable? slot = null;
            if (concurrency?.Current > 1)
            {
                DownloadLock.Release();
                lockHeld = false;
                slot = await concurrency.Transfers.EnterAsync(CancellationToken.None);
            }
            string landedPath;
            ReplacingPathLocal.Value = replacement?.OriginalPath;
            try
            {
                landedPath = await DownloadTrackAsync(
                    externalId, song, silence, sourceOverride, upgradeSearch, cancellationToken);
            }
            finally
            {
                slot?.Dispose();
            }
            EnsureOnDisk(landedPath);
            // Placing, tagging and registering touch the library and the mapping file, and those
            // stay one at a time.
            if (!lockHeld)
            {
                await DownloadLock.WaitAsync(CancellationToken.None);
                lockHeld = true;
            }
            song.LocalPath = landedPath;
            Track(t => t.Stage(externalProvider, externalId, AcquisitionState.Importing));
            var finalize = System.Diagnostics.Stopwatch.StartNew();

            // The loudness is measured while the file is identified: ffmpeg works the disk and
            // the lookups work the network, so the two overlap. Both finish before the file is
            // placed, since nothing may read a file while it moves.
            var loudness = StartLoudness(landedPath);

            // Identify before the file is placed: the album the chooser settles on names the
            // folder (#50) and its main artist names the artist folder (#49). Reads only; nothing
            // is written to the file until it sits where it will stay.
            await IdentifyAsync(song, requested, landedPath, albumContext, CancellationToken.None);
            await ApplyLoudnessAsync(song, loudness, landedPath);

            // Placed from the Song, so the path and the tags come from one decision (#48). The
            // file used to be moved inside DownloadTrackAsync, before any of this was known.
            // A library action's replacement is staged where no scan looks, and moved in only once
            // it carries the original's identity and has passed (W8).
            var placement = replacement is null
                ? await PlaceInLibraryAsync(song, requested, landedPath)
                : StageReplacement(landedPath);
            var localPath = placement.Path;
            song.LocalPath = localPath;
            // Again after placement: identification takes seconds, and placement hands back the
            // old path when the file is missing rather than failing.
            EnsureOnDisk(localPath);
            if (albumContext is not null && song.TagPlan is not null)
                albumContext.Loudness[localPath] = song.TagPlan.IntegratedLufs is { } lufs
                    ? new Loudness(lufs, 0, song.TagPlan.TruePeakDbfs ?? 0) : null;

            // Rich tags and real album art, written where the file will stay. Downloads
            // otherwise arrive bare (YouTube: artist/title and a video thumbnail; Soulseek:
            // whatever the peer tagged), so this is what makes every fetched song a
            // properly-tagged library citizen.
            var writing = System.Diagnostics.Stopwatch.StartNew();
            var cover = await WriteMetadataAsync(localPath, song, CancellationToken.None);
            if (replacement is not null)
            {
                placement = await RevealReplacementAsync(song, requested, localPath, replacement);
                localPath = placement.Path;
                song.LocalPath = localPath;
            }
            if (!isCache) await WriteSidecarsAsync(song, placement, cover, CancellationToken.None);
            if (song.TagPlan is { } tagPlan)
            {
                tagPlan.StageSeconds["write"] = writing.Elapsed.TotalSeconds;
                tagPlan.StageSeconds["total"] = finalize.Elapsed.TotalSeconds;
                Logger.LogInformation("{Summary}", tagPlan.Describe(song));
                if (finalize.Elapsed > FinalizeBudget)
                    Logger.LogWarning("finalizing '{Artist} - {Title}' took {Seconds:0.0}s; stages: {Stages}",
                        song.Artist, song.Title, finalize.Elapsed.TotalSeconds,
                        string.Join(", ", tagPlan.StageSeconds.Select(s => $"{s.Key} {s.Value:0.0}s")));
            }

            downloadInfo.Status = DownloadStatus.Completed;
            downloadInfo.LocalPath = localPath;
            downloadInfo.CompletedAt = DateTime.UtcNow;

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
                await RecordHistoryAsync(song, localPath, silence || Muted.TryGetValue(song, out _), requestedBy);
                AskForReview(song, localPath, requestedBy);
                Track(t => t.Imported(externalProvider, externalId, song.Artist, song.Title, localPath));

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
                                    albumExternalId, externalId, sourceOverride,
                                    // The album walk is still this user's star, so every
                                    // track it pulls in is attributed to them too.
                                    requestedBy: requestedBy);
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
                Track(t => t.Complete(externalProvider, externalId));
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
            if (ex is Octo.Services.Library.ReplacementRejectedException)
                Logger.LogInformation("Replacement download {SongId} refused: {Problem}", songId, ex.Message);
            else
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
        CancellationToken cancellationToken = default,
        IReadOnlyList<string>? requestedBy = null)
    {
        Logger.LogInformation("Starting background download for album {AlbumId} (excluding track {TrackId})", 
            albumExternalId, excludeTrackExternalId);

        var album = await MetadataService.GetAlbumAsync(ProviderName, albumExternalId);
        if (AlbumWalkRefusal(album) is { } refusal)
        {
            Logger.LogWarning("Album {AlbumId}: {Refusal}", albumExternalId, refusal);
            // Only a heart on the album itself reports this. A walk started by a track star has
            // already reported that track's own outcome, and a mid-chain source stays quiet so
            // the next source can try.
            if (!suppressSummary && string.IsNullOrEmpty(excludeTrackExternalId))
                Notifications.Notify(new Octo.Services.Notifications.NotificationEvent
                {
                    Type = Octo.Services.Notifications.NotificationEventType.DownloadFailed,
                    Artist = album?.Artist,
                    Title = album?.Title ?? "album",
                    CoverArtUrl = album?.CoverArtUrl,
                    Detail = refusal,
                });
            return false;
        }

        var tracksToDownload = album!.Songs
            .Where(s => s.ExternalId != excludeTrackExternalId && !string.IsNullOrEmpty(s.ExternalId))
            .ToList();

        Logger.LogInformation("Found {Count} additional tracks to download for album '{AlbumTitle}'",
            tracksToDownload.Count, album.Title);

        // The whole list is known now, so a hearted album shows every track it will fetch.
        Track(t => t.Announce(ProviderName, albumExternalId, excludeTrackExternalId,
            tracksToDownload.Select(s => (s.ExternalId!, (string?)s.Artist, (string?)s.Title, (string?)album.Title))));

        // Per-track notifications are muted below; these feed one summary instead.
        int succeeded = 0, lossless = 0, failed = 0, kept = 0, upgrading = 0;

        // Songs already in the library stay as they are, and a lossy one is queued for a higher
        // quality copy instead of downloaded again. Only what is missing is looked for, by folder
        // or song by song.
        if (SubsonicSettings.SkipOwnedSongs && OptionalService<Octo.Services.Library.LibraryOwnership>() is { } ownership)
        {
            var owned = new ConcurrentDictionary<string, Octo.Services.Library.OwnedCopy>(StringComparer.Ordinal);
            await Parallel.ForEachAsync(tracksToDownload, new ParallelOptions { MaxDegreeOfParallelism = 4 }, async (track, ct) =>
            {
                if (await ownership.FindAsync(track.Artist, track.Title, track.Duration, track.Album ?? album.Title, ct) is { } copy)
                    owned[track.ExternalId!] = copy;
            });
            var canBeLossless = SourceCanBeLossless(sourceOverride);
            var mayUpgrade = UpgradeAllowed(requestedBy);
            foreach (var track in tracksToDownload.Where(t => owned.ContainsKey(t.ExternalId!)))
            {
                var copy = owned[track.ExternalId!];
                if (Octo.Services.Library.LibraryOwnership.Decide(copy, canBeLossless, mayUpgrade)
                    == Octo.Services.Library.OwnedDecision.KeepAndUpgrade)
                {
                    QueueUpgradeFor(copy, track, requestedBy!);
                    upgrading++;
                }
                else kept++;
                Track(t => t.Imported(ProviderName, track.ExternalId!, track.Artist, track.Title, copy.AbsolutePath));
            }
            tracksToDownload = tracksToDownload.Where(t => !owned.ContainsKey(t.ExternalId!)).ToList();
            if (owned.Count > 0)
                Logger.LogInformation("Album '{Album}': {Kept} songs already yours, {Upgrading} queued for a higher quality copy, {Left} to download",
                    album.Title, kept, upgrading, tracksToDownload.Count);
        }

        // Every track of the walk shares the release the first one settled on, and the walk
        // measures each track so the album gain can be written once it ends.
        var albumContext = new AlbumTagContext(albumExternalId, album.Title, album.Artist);

        // One track of the walk. Counters are shared by both lanes below, so they move atomically.
        async Task OneTrackAsync(Song track)
        {
            try
            {
                EnsureRoutingRegistered(track);

                var existingPath = await LocalLibraryService.GetLocalPathForExternalSongAsync(ProviderName, track.ExternalId!);
                if (existingPath != null && IOFile.Exists(existingPath))
                {
                    Logger.LogDebug("Track {TrackId} already downloaded, skipping", track.ExternalId);
                    Track(t => t.Imported(ProviderName, track.ExternalId!, track.Artist, track.Title, existingPath));
                    return;
                }

                // Check if download is already in progress or recently completed
                var songId = $"ext-{ProviderName}-{track.ExternalId}";
                if (ActiveDownloads.TryGetValue(songId, out var activeDownload))
                {
                    if (activeDownload.Status == DownloadStatus.InProgress)
                    {
                        Logger.LogDebug("Track {TrackId} download already in progress, skipping", track.ExternalId);
                        return;
                    }

                    if (activeDownload.Status == DownloadStatus.Completed)
                    {
                        Logger.LogDebug("Track {TrackId} already downloaded in this session, skipping", track.ExternalId);
                        Track(t => t.Imported(ProviderName, track.ExternalId!, track.Artist, track.Title,
                            activeDownload.LocalPath));
                        return;
                    }
                }

                Logger.LogInformation("Downloading track '{Title}' from album '{Album}'", track.Title, album.Title);
                var path = await DownloadSongInternalAsync(
                    ProviderName, track.ExternalId!, triggerAlbumDownload: false,
                    cancellationToken, forcePermanent: true, suppressNotify: true,
                    sourceOverride: sourceOverride, requestedBy: requestedBy,
                    albumContext: albumContext);
                Interlocked.Increment(ref succeeded);
                if (path.EndsWith(".flac", StringComparison.OrdinalIgnoreCase)) Interlocked.Increment(ref lossless);

                // Force a rescan per track so the album fills in progressively in the
                // client instead of appearing all at once at the end. The per-download
                // scan inside DownloadSongInternalAsync is debounced, which during a
                // batch swallows most triggers and can strand the final tracks entirely.
                await LocalLibraryService.TriggerLibraryScanAsync(force: true);
            }
            catch (Exception ex)
            {
                Logger.LogWarning(ex, "Failed to download track {TrackId} '{Title}'", track.ExternalId, track.Title);
                Interlocked.Increment(ref failed);
                // Same rule as the summary: a source with another after it stays quiet, and
                // the next walk picks the track up again.
                if (!suppressSummary) Track(t => t.Fail(ProviderName, track.ExternalId ?? "", ex.Message));
            }
        }

        // Tracks whose file came from one peer's folder of the album, already queued in one batch,
        // go one at a time in album order: that peer sends them back to back, so waiting on several
        // at once would only sit in its queue and run out each wait's quiet window. The rest are
        // searched song by song, side by side when downloads may run in parallel; the transfer gate
        // keeps the total to the setting. With nothing prepared and one at a time, this is the walk
        // as it always was.
        var prepared = await PrepareAlbumAsync(album, tracksToDownload, sourceOverride, cancellationToken);
        try
        {
            var fromFolder = tracksToDownload.Where(t => prepared.Contains(t.ExternalId!)).ToList();
            var bySearch = tracksToDownload.Where(t => !prepared.Contains(t.ExternalId!)).ToList();
            var folderLane = Task.Run(async () =>
            {
                foreach (var track in fromFolder) await OneTrackAsync(track);
            });
            var searchLane = Parallel.ForEachAsync(bySearch,
                new ParallelOptions { MaxDegreeOfParallelism = Math.Max(1, Concurrency?.Current ?? 1) },
                async (track, _) => await OneTrackAsync(track));
            await Task.WhenAll(folderLane, searchLane);
        }
        finally
        {
            await FinishAlbumAsync(prepared);
        }

        Logger.LogInformation("Completed background download for album '{AlbumTitle}'", album.Title);

        if (MetadataSettingsValue.ReplayGain) WriteAlbumGain(albumContext, album.Title);

        var summary = BuildAlbumSummary(album, succeeded, lossless, failed, kept, upgrading);
        // Hide an intermediate failure while another source remains, but still report
        // success when an earlier priority step completes the album acquisition.
        if ((!suppressSummary || failed == 0) && summary is not null) Notifications.Notify(summary);
        return failed == 0;
    }

    /// <summary>
    /// Queue what an album walk can take from one place in one go, before the walk starts, and
    /// say which tracks that covers. Each of those tracks still goes through its own download,
    /// which takes the queued file first. The base takes nothing, so the walk is song by song.
    /// </summary>
    protected virtual Task<IReadOnlyCollection<string>> PrepareAlbumAsync(Album album, IReadOnlyList<Song> tracks,
        DownloadSource? source, CancellationToken cancellationToken) =>
        Task.FromResult<IReadOnlyCollection<string>>(Array.Empty<string>());

    /// <summary>Called when the walk ends, however it ends: let go of whatever was prepared and
    /// not used.</summary>
    protected virtual Task FinishAlbumAsync(IReadOnlyCollection<string> prepared) => Task.CompletedTask;

    /// <summary>
    /// The album gain and peak, written into every file the walk measured, once the walk ends.
    /// Only when every track was measured: an album gain for half an album is worse than none.
    /// A rewrite in place, so the library server keeps each file's id.
    /// </summary>
    internal void WriteAlbumGain(AlbumTagContext context, string albumTitle)
    {
        if (context.Loudness.IsEmpty) return;
        var album = ReplayGainTags.ForAlbum(context.Loudness.Values.ToList());
        if (album is null)
        {
            Logger.LogInformation("No album gain for '{Album}': not every track could be measured", albumTitle);
            return;
        }
        foreach (var path in context.Loudness.Keys)
        {
            try
            {
                if (!IOFile.Exists(path)) continue;
                using var tagFile = TagLib.File.Create(path);
                TagWriterExtras.SetReplayGain(tagFile, null, null, album.GainDb, album.Peak);
                tagFile.Save();
            }
            catch (Exception ex)
            {
                Logger.LogWarning("Could not write the album gain to {Path}: {M}", path, ex.Message);
            }
        }
        Logger.LogInformation("Album gain {Gain} (peak {Peak}) written to {Count} tracks of '{Album}'",
            album.GainText, album.PeakText, context.Loudness.Count, albumTitle);
    }

    /// <summary>
    /// Why an album walk cannot start, or null when it can. A walk with nothing to walk used to
    /// return success, so a hearted album whose track list never loaded (the metadata provider
    /// was down or rate-limited) did nothing, said nothing, and stopped the source chain there.
    /// </summary>
    internal static string? AlbumWalkRefusal(Album? album) =>
        album is null
            ? "Octo could no longer look this album up, so nothing was downloaded. Heart it again from a fresh search."
            : !album.Songs.Any(song => !string.IsNullOrEmpty(song.ExternalId))
                ? $"No track list came back for \"{album.Title}\", so nothing was downloaded. The metadata provider may be down; try again later."
                : null;

    /// <summary>
    /// Null when the walk did no work — a re-star whose tracks are all already
    /// present must not ping the phone. Counts cover the walked tracks only; the
    /// track whose star triggered the walk got its own DownloadCompleted.
    /// </summary>
    internal static Octo.Services.Notifications.NotificationEvent? BuildAlbumSummary(
        Album album, int succeeded, int lossless, int failed, int kept = 0, int upgrading = 0)
        => succeeded + failed + kept + upgrading == 0 ? null : new Octo.Services.Notifications.NotificationEvent
        {
            Type = Octo.Services.Notifications.NotificationEventType.AlbumCompleted,
            Artist = album.Artist,
            Title = album.Title,
            CoverArtUrl = album.CoverArtUrl,
            TrackCount = succeeded,
            LosslessCount = lossless,
            FailedCount = failed,
            KeptCount = kept,
            UpgradingCount = upgrading,
        };

    /// <summary>
    /// Whether a lossless copy can be looked for: an owned lossy copy is then queued for Better
    /// quality rather than kept as it is. That runs through the upgrade sources (Soulseek, Lidarr),
    /// whichever source this download uses. Without them, as before: this download's own source.
    /// </summary>
    private bool SourceCanBeLossless(DownloadSource? sourceOverride) =>
        OptionalService<Octo.Services.Library.UpgradeSources>()?.Ready
        ?? (sourceOverride ?? SubsonicSettings.DownloadSource) is DownloadSource.Soulseek or DownloadSource.SoulseekThenYouTube;

    /// <summary>Whether Better quality may run for the person who asked: every gate of the action.</summary>
    private bool UpgradeAllowed(IReadOnlyList<string>? requestedBy) =>
        requestedBy is { Count: > 0 } askers
        && OptionalService<Octo.Services.Library.UpgradeQueue>() is not null
        && OptionalService<IOptionsMonitor<LibraryActionSettings>>()?.CurrentValue is { } actions
        && actions.Enabled && !actions.DryRun && actions.IsAllowed(askers[0])
        && actions.EffectiveActions().Any(a => a.Action == LibraryAction.BetterQuality && a.Enabled);

    private void QueueUpgradeFor(Octo.Services.Library.OwnedCopy owned, Song song, IReadOnlyList<string> requestedBy) =>
        OptionalService<Octo.Services.Library.UpgradeQueue>()?.Add(
            [new Octo.Services.Library.UpgradeAsk(owned.NavidromeId!, song.Title, song.Artist, song.Album, owned.Suffix)],
            requestedBy[0], "heart");
    
    #endregion
    
    #region Common Metadata Writing
    
    /// <summary>The finalize phase runs under the download lock, so past this it is logged with
    /// its stage timings. Nothing is cut short beyond the per-stage caps.</summary>
    private static readonly TimeSpan FinalizeBudget = TimeSpan.FromSeconds(20);

    private ReleaseIdentifier? _identifier;

    /// <summary>Resolved per use like the other services; built on the spot where a host did not
    /// register one, so placement tests need nothing but the provider they already have.</summary>
    private ReleaseIdentifier Identifier => _identifier ??= _serviceProvider.GetService<ReleaseIdentifier>()
        ?? new ReleaseIdentifier(_serviceProvider, Microsoft.Extensions.Logging.Abstractions.NullLogger<ReleaseIdentifier>.Instance);

    /// <summary>Whether a landed file came from the video site's staging folder, whose tags are
    /// an uploader's and not evidence of anything.</summary>
    internal static bool IsStagedUpload(string path) =>
        path.Split(Path.DirectorySeparatorChar, Path.AltDirectorySeparatorChar)
            .Any(segment => string.Equals(segment, Octo.Services.Soulseek.SoulseekDownloadService.IncomingFolderName, StringComparison.OrdinalIgnoreCase));

    /// <summary>
    /// Works out what the file is and sets the Song from it. Every candidate release the
    /// fingerprint service, the music database and the catalog offer is weighed against what was
    /// asked for and what landed; a sure match sets the album-level tags, a doubtful one only
    /// fills blanks, the way the catalog always did. Reads only: the tags are written by
    /// WriteMetadataAsync once the file has been placed. Best-effort; a miss never breaks the
    /// download.
    /// </summary>
    protected async Task IdentifyAsync(Song song, RequestedIdentity requested, string filePath,
        AlbumTagContext? album, CancellationToken cancellationToken)
    {
        // Last.fm/YouTube titles often carry a redundant "Artist - " prefix (e.g.
        // "Radiohead - No Surprises") which mislabels the file, so it goes from the written
        // title. The lookup gets the title whole: Deezer tries it without "(Official Video)"
        // and guests on its own, and a "(Live)" left in is what stops a live download being
        // tagged with the studio album's cover, track number and year.
        song.Title = StripArtistPrefix(song.Artist, song.Title);
        var queryTitle = song.Title;
        var settings = MetadataSettingsValue;

        TagPlan? plan = null;
        try
        {
            var request = ReleaseIdentifier.RequestFor(song, song.Artist, queryTitle, requested.Album, requested.Track);
            plan = await Identifier.IdentifyAsync(song, request, filePath, !IsStagedUpload(filePath), album, cancellationToken);
            song.TagPlan = plan;

            if (settings.TagRehearsal) plan.ApplyRehearsalTo(song);
            else
            {
                plan.ApplyTo(song);
                if (plan.AlbumFromCandidate && plan.Fields.TryGetValue("album", out var chosenAlbum)
                    && plan.Evidence?.File.Album is { Length: > 0 } fileAlbum
                    && SongIdentity.Key(fileAlbum) != SongIdentity.Key(chosenAlbum.Value))
                    Logger.LogInformation("'{Album}' replaces the file's own '{FileAlbum}' ({Confidence})",
                        chosenAlbum.Value, fileAlbum, plan.Confidence);
            }
            FillBlanksFromCatalog(song, plan.CatalogBest);
        }
        catch (Exception ex)
        {
            Logger.LogWarning(ex, "Identification failed for '{Artist} - {Title}'; filling blanks the old way", song.Artist, song.Title);
            await FillBlanksFromCatalogAsync(song, queryTitle, cancellationToken);
        }

        FillAlbumFromFile(song, filePath);
        ApplySingleFallback(song, settings.AlbumFromTitle);

        // Explicit or clean, from what landed; the library server shows it on every song.
        var advisory = ExplicitAdvisory.Decide(song, plan, song.SourceFile);
        song.Advisory = advisory?.Value;
        if (plan is not null && advisory is not null) plan.Fields["advisory"] = ExplicitAdvisory.Field(advisory);

        var sibling = SubsonicSettings.FolderStructure == FolderStructure.Organized ? AlbumSibling(song, requested, filePath) : null;
        if (sibling is not null) JoinAlbum(song, sibling);

        if (plan is not null && !settings.TagRehearsal)
        {
            if (album is not null)
            {
                album.Pin(song);
                album.Capture(plan, song);
            }
            else if (sibling is not null)
                PinToSibling(song, sibling);
        }
    }

    /// <summary>The old catalog enrichment, asked for on its own when identification itself failed.</summary>
    private async Task FillBlanksFromCatalogAsync(Song song, string queryTitle, CancellationToken cancellationToken)
    {
        try
        {
            var deezer = _serviceProvider.GetService<Octo.Services.Metadata.DeezerMetadataService>();
            if (deezer != null) FillBlanksFromCatalog(song, await deezer.EnrichTrackFullAsync(song.Artist, queryTitle, cancellationToken));
        }
        catch (Exception ex)
        {
            Logger.LogWarning(ex, "Deezer enrichment for tagging failed for '{Artist} - {Title}'", song.Artist, song.Title);
        }
    }

    /// <summary>
    /// Fills any missing metadata on <paramref name="song"/> from the catalog's best hit.
    /// Existing values win (a well-tagged Soulseek FLAC is enriched, not overwritten); the
    /// catalog fills the gaps and supplies the cover. The album's own facts (its cover, its
    /// compilation flag) are taken only when the hit is the album the song is filed under,
    /// since the chooser may have put the song on another release than the catalog's first hit.
    /// </summary>
    internal static void FillBlanksFromCatalog(Song song, DeezerMetadataService.FullTrackMeta? m)
    {
        if (m is null) return;
        var sameAlbum = string.IsNullOrEmpty(song.Album) || string.IsNullOrEmpty(m.AlbumTitle)
            || SongIdentity.Key(song.Album) == SongIdentity.Key(m.AlbumTitle);

        // Deezer's main artist names the folder when the request carried a list of
        // credits (#49); its contributors give every credited artist a value of
        // their own, so Navidrome files a collaboration under each of them.
        if (string.IsNullOrEmpty(song.PrimaryArtist) && !string.IsNullOrEmpty(m.ArtistName)) song.PrimaryArtist = m.ArtistName;
        if (song.Artists.Count == 0 && m.Contributors is { Count: > 1 } contributors) song.Artists = contributors.ToList();
        if (string.IsNullOrEmpty(song.Album) && !string.IsNullOrEmpty(m.AlbumTitle)) song.Album = m.AlbumTitle;
        if (sameAlbum)
        {
            // The album's own artist, not the track's: the two differ on every feature and
            // every compilation.
            if (string.IsNullOrEmpty(song.AlbumArtist) && (m.AlbumArtistName ?? m.ArtistName) is { Length: > 0 } albumArtist)
                song.AlbumArtist = albumArtist;
            if (IsVariousArtists(m.AlbumArtistName)
                || string.Equals(m.RecordType, "compile", StringComparison.OrdinalIgnoreCase))
                song.IsCompilation = true;
            if (string.IsNullOrEmpty(song.CoverArtUrlLarge)) song.CoverArtUrlLarge = m.AlbumCoverUrl;
            if (!song.Year.HasValue) song.Year = m.Year;
            if (!song.Track.HasValue) song.Track = m.TrackNumber;
            if (!song.DiscNumber.HasValue) song.DiscNumber = m.DiscNumber;
            if (!song.TotalTracks.HasValue) song.TotalTracks = m.TotalTracks;
            if (string.IsNullOrEmpty(song.Label)) song.Label = m.Label;
            if (string.IsNullOrEmpty(song.Barcode)) song.Barcode = m.Barcode;
            if (string.IsNullOrEmpty(song.ReleaseDate)) song.ReleaseDate = m.ReleaseDate;
        }
        if (!song.Duration.HasValue) song.Duration = m.Duration;
        if (string.IsNullOrEmpty(song.Genre)) song.Genre = m.Genre;
        if (string.IsNullOrEmpty(song.Isrc)) song.Isrc = m.Isrc;
    }

    /// <summary>One file of the album folder this song will join, when the folder is already there
    /// and the file agrees on the album and its artist; its facts as read, with its path.</summary>
    private sealed record AlbumSiblingFile(string Path, FileFacts Facts);

    private AlbumSiblingFile? AlbumSibling(Song song, RequestedIdentity requested, string currentPath)
    {
        try
        {
            if (string.IsNullOrWhiteSpace(song.Album) || string.IsNullOrEmpty(DownloadPath)) return null;
            var choice = ChooseLayout(song, requested, SoulseekSettingsValue.NameFromMatch);
            var target = PathHelper.BuildLayoutPath(FolderStructure.Organized, DownloadPath,
                string.IsNullOrWhiteSpace(choice.FolderArtist) ? "Unknown Artist" : choice.FolderArtist,
                choice.Album, PathHelper.FileTitle(choice.Title, choice.FileArtist), choice.Track, Path.GetExtension(currentPath));
            var dir = Path.GetDirectoryName(target);
            if (string.IsNullOrEmpty(dir) || !Directory.Exists(dir)) return null;
            var sibling = Directory.EnumerateFiles(dir)
                .FirstOrDefault(file => AudioExtensions.Contains(Path.GetExtension(file))
                    && !string.Equals(Path.GetFullPath(file), Path.GetFullPath(currentPath), StringComparison.OrdinalIgnoreCase));
            if (sibling is null) return null;

            var facts = TagWriterExtras.ReadFacts(sibling, tagsAreEvidence: true);
            if (SongIdentity.Key(facts.Album) != SongIdentity.Key(song.Album)) return null;
            var albumArtist = song.AlbumArtist ?? song.PrimaryArtist ?? song.Artist;
            if (!string.IsNullOrEmpty(facts.AlbumArtist) && !string.IsNullOrEmpty(albumArtist)
                && !SongIdentity.SameArtistName(facts.AlbumArtist, albumArtist)) return null;
            return new AlbumSiblingFile(sibling, facts);
        }
        catch (Exception ex)
        {
            Logger.LogDebug("could not read the album folder's sibling for {Path}: {M}", currentPath, ex.Message);
            return null;
        }
    }

    /// <summary>
    /// A single joining an album folder that is already there takes that album's release facts
    /// from one of its files, so the album the library server shows keeps one label, one
    /// catalog number and one year.
    /// </summary>
    private static void PinToSibling(Song song, AlbumSiblingFile sibling)
    {
        var facts = sibling.Facts;
        if (facts.Year is > 0) song.Year = facts.Year;
        if (!string.IsNullOrEmpty(facts.Label)) song.Label = facts.Label;
        if (!string.IsNullOrEmpty(facts.CatalogNumber)) song.CatalogNumber = facts.CatalogNumber;
        if (!string.IsNullOrEmpty(facts.Barcode)) song.Barcode = facts.Barcode;
        song.TagPlan?.Notes.Add($"album facts taken from the album folder's own '{Path.GetFileName(sibling.Path)}'");
    }

    /// <summary>
    /// The values the library server groups the folder's album by, read the way it reads them, so
    /// the new track carries exactly the same ones and lands in that album rather than one of its
    /// own. Not a matching decision, so a rehearsal keeps it too.
    /// </summary>
    private void JoinAlbum(Song song, AlbumSiblingFile sibling)
    {
        if (Octo.Services.Library.KeptIdentityTags.Read(sibling.Path) is not { } kept) return;
        if (kept.AlbumId is null && kept.ReleaseDate is null && kept.AlbumVersion is null) return;
        song.JoinsAlbum = new AlbumGrouping(kept.AlbumId, kept.ReleaseDate, kept.AlbumVersion);
        song.TagPlan?.Notes.Add($"album grouping copied from the album folder's own '{Path.GetFileName(sibling.Path)}'");
        Logger.LogDebug("{Title} joins the album of {Sibling}", song.Title, sibling.Path);
    }

    /// <summary>Start measuring a landed file, beside identification, when ReplayGain is on.</summary>
    private Task<Loudness?>? StartLoudness(string path)
    {
        var settings = MetadataSettingsValue;
        if (!settings.ReplayGain) return null;
        var meter = _serviceProvider.GetService<ILoudnessMeter>();
        if (meter is null) return null;
        return Task.Run(() => meter.MeasureAsync(path, settings.EffectiveReplayGainTimeoutSeconds, CancellationToken.None));
    }

    /// <summary>Wait for the measurement, which has its own cap, and set the song's ReplayGain.</summary>
    private async Task ApplyLoudnessAsync(Song song, Task<Loudness?>? measurement, string path)
    {
        if (measurement is null) return;
        var clock = System.Diagnostics.Stopwatch.StartNew();
        Loudness? loudness = null;
        try
        {
            loudness = await measurement;
        }
        catch (Exception ex)
        {
            Logger.LogDebug("loudness measurement failed for {Path}: {M}", path, ex.Message);
        }
        if (song.TagPlan is { } plan)
        {
            plan.StageSeconds["loudness"] = clock.Elapsed.TotalSeconds;
            plan.IntegratedLufs = loudness?.IntegratedLufs;
            plan.TruePeakDbfs = loudness?.TruePeakDbfs;
        }
        var tags = ReplayGainTags.ForTrack(loudness);
        if (tags is null)
        {
            song.TagPlan?.Notes.Add("the loudness could not be measured, so the file has no ReplayGain");
            return;
        }
        song.ReplayGainTrackGainDb = tags.GainDb;
        song.ReplayGainTrackPeak = tags.Peak;
    }

    /// <summary>A source's own album tag beats nothing, and beats filing the track under its title.
    /// Its compilation flag counts only for that album: a song the chooser filed elsewhere is not
    /// a compilation because the file it came from was ripped from one.</summary>
    private static void FillAlbumFromFile(Song song, string filePath)
    {
        var (album, albumArtist, compilation) = TagWriterExtras.ReadAlbum(filePath);
        var sameAlbum = string.IsNullOrWhiteSpace(song.Album) || string.IsNullOrWhiteSpace(album)
            || SongIdentity.Key(song.Album) == SongIdentity.Key(album);
        if (sameAlbum && (compilation || IsVariousArtists(albumArtist))) song.IsCompilation = true;
        if (!string.IsNullOrWhiteSpace(song.Album) || string.IsNullOrWhiteSpace(album)) return;
        song.Album = album.Trim();
        if (string.IsNullOrEmpty(song.AlbumArtist) && !string.IsNullOrWhiteSpace(albumArtist))
            song.AlbumArtist = albumArtist.Trim();
    }

    /// <summary>
    /// A track that still has no album is filed as a single under its own title (#50). That is
    /// a real, correct release, and it gives Navidrome something to group; the alternative is
    /// one "[Unknown Album]" collecting every unrelated album-less track. Skipped for a
    /// compilation, where a hundred one-track albums would be worse than the one untidy bucket.
    /// </summary>
    internal static void ApplySingleFallback(Song song, bool enabled)
    {
        if (!enabled || !string.IsNullOrWhiteSpace(song.Album) || string.IsNullOrWhiteSpace(song.Title)) return;
        if (song.IsCompilation || IsVariousArtists(song.AlbumArtist)) return;
        song.Album = song.Title.Trim();
        if (string.IsNullOrEmpty(song.AlbumArtist)) song.AlbumArtist = song.PrimaryArtist ?? song.Artist;
    }

    internal static bool IsVariousArtists(string? name) =>
        name?.Trim() is { Length: > 0 } value
        && (value.Equals("Various Artists", StringComparison.OrdinalIgnoreCase)
            || value.Equals("Various", StringComparison.OrdinalIgnoreCase)
            || value.Equals("VA", StringComparison.OrdinalIgnoreCase));

    /// <summary>Drop a redundant leading "Artist - " from a track title.</summary>
    private static string StripArtistPrefix(string? artist, string? title)
    {
        var t = (title ?? string.Empty).Trim();
        var a = (artist ?? string.Empty).Trim();
        if (a.Length > 0 && t.StartsWith(a + " - ", StringComparison.OrdinalIgnoreCase))
            t = t[(a.Length + 3)..].Trim();
        return t;
    }

    /// <summary>
    /// Last resort for a file with no usable genre: the listener-supplied tags Last.fm already
    /// caches for radio.
    ///
    /// Gated on HasApiKey, NOT on IsRadioEnabled. A user who turned radio off still configured
    /// a key, and tagging is not radio - the same split LastFmService documents on those two
    /// properties. Last.fm tags are also the rawest input the normaliser ever sees, so they go
    /// through radio's own vocabulary first: that list filters Last.fm data, which is exactly
    /// what it was written for, without the genre settings having to own it.
    /// </summary>
    private async Task<IReadOnlyList<string>> ResolveGenreFallbackAsync(
        Song song, GenreSettings settings, CancellationToken cancellationToken)
    {
        if (settings.Fallback == GenreFallbackSource.MusicBrainz)
        {
            // The release lookup already carries the genres people voted on for the chosen
            // release. Two votes or more count, so one person's tag cannot name a genre. With
            // no votes it behaves as Last.fm and says so, because a setting that appears to work
            // and silently does nothing is worse than one that is missing.
            var voted = song.TagPlan?.Details?.TopGenres() ?? [];
            if (voted.Count > 0) return voted;
            Logger.LogInformation(
                "MusicBrainz lists no voted genres for {Artist} - {Title}; using Last.fm top tags",
                song.Artist, song.Title);
        }

        try
        {
            var lastFm = _serviceProvider.GetService<Octo.Services.LastFm.LastFmService>();
            if (lastFm is null || !lastFm.HasApiKey) return [];

            var tags = await lastFm.GetTrackTopTagsAsync(song.Artist ?? "", song.Title ?? "", 8, cancellationToken);
            if (tags.Count == 0 && !string.IsNullOrEmpty(song.Artist))
                tags = await lastFm.GetArtistTopTagsAsync(song.Artist, 8, cancellationToken);

            return tags
                .Select(Octo.Services.LastFm.LastFmRadioRecommendationService.CanonicalTag)
                .Where(tag => tag.Length > 0)
                .ToList();
        }
        catch (Exception ex)
        {
            Logger.LogDebug("genre fallback lookup failed for {Artist} - {Title}: {M}",
                song.Artist, song.Title, ex.Message);
            return [];
        }
    }

    /// <summary>
    /// Write the Song's tags and the chosen cover onto the file. Returns the cover it settled on,
    /// embedded or already there, so the same picture can go beside the file as cover.jpg; null
    /// when there was none or the write failed.
    /// </summary>
    protected async Task<byte[]?> WriteMetadataAsync(string filePath, Song song, CancellationToken cancellationToken)
    {
        byte[]? chosenCover = null;
        try
        {
            Logger.LogInformation("Writing metadata to: {Path}", filePath);
            
            using var tagFile = TagLib.File.Create(filePath);

            // A peer tags a file for the release it ripped. What it wrote about that release goes
            // before Octo writes its own, so the file never names two albums at once (a barcode,
            // a release id or a track number of the album it was filed away from).
            var arrivedAlbum = tagFile.Tag.Album;
            var filedElsewhere = ReleaseFactTags.FiledElsewhere(arrivedAlbum, song.Album);
            var dropped = ReleaseFactTags.Tidy(tagFile, filedElsewhere, song.JoinsAlbum);
            if (dropped.Count > 0)
            {
                var why = filedElsewhere ? $"it arrived filed under '{arrivedAlbum}'" : "a new download keeps no album grouping values";
                song.TagPlan?.Notes.Add($"took off the file's own {string.Join(", ", dropped)}: {why}");
                Logger.LogInformation("Took off {Fields} from {Path}: {Why}", string.Join(", ", dropped), filePath, why);
            }

            // Basic metadata. Title/artist we always have; only overwrite album +
            // album-artist when we actually resolved them, so a well-tagged Soulseek
            // FLAC keeps its own album if Deezer had no match.
            if (!string.IsNullOrEmpty(song.Title)) tagFile.Tag.Title = song.Title;
            if (!string.IsNullOrEmpty(song.Artist)) tagFile.Tag.Performers = new[] { song.Artist };
            // The full credit stays in the artist tag; each artist also gets a value of their own,
            // so Navidrome files a collaboration under every one of them (#49).
            if (song.Artists.Count > 1) TagWriterExtras.SetMultiValue(tagFile, "ARTISTS", song.Artists);
            if (!string.IsNullOrEmpty(song.Album)) tagFile.Tag.Album = song.Album;
            if (!string.IsNullOrEmpty(song.AlbumArtist))
                tagFile.Tag.AlbumArtists = new[] { song.AlbumArtist };
            else if (!string.IsNullOrEmpty(song.Artist))
                // A list of credits as album artist scatters the album view the way it scattered
                // folders, so the first credit stands in when a source named one.
                tagFile.Tag.AlbumArtists = new[] { song.PrimaryArtist ?? song.Artist };
            
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
            {
                tagFile.Tag.Disc = (uint)song.DiscNumber.Value;
                if (song.TotalDiscs is > 0 && song.TotalDiscs >= song.DiscNumber)
                    tagFile.Tag.DiscCount = (uint)song.TotalDiscs.Value;
            }
            ReleaseFactTags.DropOtherTotals(tagFile);

            if (song.Year.HasValue)
                tagFile.Tag.Year = (uint)song.Year.Value;
            
            // Genre is the one tag that must be able to write NOTHING.
            //
            // Before this, genre was written ONLY when song.Genre was non-empty, and was never
            // cleared. So when Deezer missed and the source had no genre, whatever multi-value
            // frame the Soulseek peer's file or the yt-dlp output already carried survived
            // untouched: "People & Blogs" and seven-genres-at-once reached the library through
            // the ABSENCE of a write, not a bad one. The fix has to read the existing frame,
            // normalise it, and write the result back.
            var genreSettings = GenreSettings;
            if (genreSettings.Enabled)
            {
                var existing = tagFile.Tag.Genres ?? [];
                var plan = GenreNormalizer.Plan(existing, song.Genre, genreSettings);

                // Only pay for the lookup when nothing usable survived. Last.fm's top tags are
                // weaker evidence than a real genre frame and earn a turn only when there is
                // no frame left.
                if (plan.Action != GenreTagAction.Write
                    && genreSettings.Fallback != GenreFallbackSource.None)
                {
                    var fallback = await ResolveGenreFallbackAsync(song, genreSettings, cancellationToken);
                    if (fallback.Count > 0)
                        plan = GenreNormalizer.Plan(existing, song.Genre, genreSettings, fallback);
                }

                switch (plan.Action)
                {
                    case GenreTagAction.Write:
                        tagFile.Tag.Genres = plan.Genres.ToArray();
                        song.Genre = plan.Primary;
                        break;
                    case GenreTagAction.Clear:
                        // Destructive and not undoable per file, so it is logged below at
                        // Information: a user who regrets their mapping table can at least see
                        // what left.
                        tagFile.Tag.Genres = [];
                        song.Genre = null;
                        break;
                }

                if (plan.Action != GenreTagAction.None)
                    Logger.LogInformation("Genre normalised for {Path}: [{Before}] -> [{After}]{Rule}",
                        filePath, string.Join(", ", existing), string.Join(", ", plan.Genres),
                        plan.MatchedRule is null ? "" : $" via {plan.MatchedRule}");
            }
            else if (!string.IsNullOrEmpty(song.Genre))
            {
                // Feature off: byte-for-byte the behaviour that shipped before this.
                tagFile.Tag.Genres = new[] { song.Genre };
            }
            
            if (song.Bpm.HasValue)
                tagFile.Tag.BeatsPerMinute = (uint)song.Bpm.Value;
            
            if (song.Contributors.Count > 0)
                tagFile.Tag.Composers = song.Contributors.ToArray();
            
            if (!string.IsNullOrEmpty(song.Copyright))
                tagFile.Tag.Copyright = song.Copyright;
            
            // What the fingerprint proved (#48), so no later pass has to identify this file
            // again. No album id: Navidrome groups albums by MUSICBRAINZ_ALBUMID before the
            // album name, so one track carrying it beside another without it splits an album.
            // The group id is written only when the album really is that release.
            if (!string.IsNullOrEmpty(song.MusicBrainzRecordingId))
                TagWriterExtras.SetRecordingId(tagFile, song.MusicBrainzRecordingId);
            var albumIsRelease = !string.IsNullOrEmpty(song.MusicBrainzReleaseGroupId)
                && Octo.Services.Fingerprint.VerificationResult.AlbumIsFromRelease(song);
            if (albumIsRelease) tagFile.Tag.MusicBrainzReleaseGroupId = song.MusicBrainzReleaseGroupId;
            if (song.MusicBrainzArtistIds.Count > 0) TagWriterExtras.SetMulti(tagFile, TagFields.ArtistId, song.MusicBrainzArtistIds);
            // The flag is album-level: when the chooser set the album, a peer's stale flag from
            // the compilation the file was ripped from would file the studio album as one.
            if (song.IsCompilation) TagWriterExtras.SetCompilation(tagFile, true);
            else if (filedElsewhere || song.TagPlan is { AlbumFromCandidate: true, Rehearsed: false }) TagWriterExtras.SetCompilation(tagFile, false);

            // The rest of what a release is: its code, its label and catalogue number, its barcode,
            // its kind and status, where and when it came out, and the ids that name it. The code
            // has its own field now; the "ISRC: x" comment is no longer written, and a comment the
            // file arrived with is left alone. Every setter skips an empty value.
            TagWriterExtras.SetText(tagFile, TagFields.Isrc, SongIdentity.NormalizeIsrc(song.Isrc));
            TagWriterExtras.SetText(tagFile, TagFields.Label, song.Label);
            TagWriterExtras.SetText(tagFile, TagFields.CatalogNumber, song.CatalogNumber);
            TagWriterExtras.SetText(tagFile, TagFields.Barcode, song.Barcode);
            if (song.ReleaseType is { Length: > 0 } releaseType)
                TagWriterExtras.SetMulti(tagFile, TagFields.ReleaseType, releaseType.Split("; ", StringSplitOptions.RemoveEmptyEntries));
            TagWriterExtras.SetText(tagFile, TagFields.ReleaseStatus, song.ReleaseStatus);
            TagWriterExtras.SetText(tagFile, TagFields.ReleaseCountry, song.ReleaseCountry);
            TagWriterExtras.SetOriginalDate(tagFile, song.OriginalDate);
            if (albumIsRelease) TagWriterExtras.SetReleaseTrackId(tagFile, song.MusicBrainzReleaseTrackId);
            if (albumIsRelease) TagWriterExtras.SetMulti(tagFile, TagFields.AlbumArtistId, song.MusicBrainzAlbumArtistIds);
            TagWriterExtras.SetText(tagFile, TagFields.FingerprintId, song.AcoustId);
            TagWriterExtras.SetReplayGain(tagFile, song.ReplayGainTrackGainDb, song.ReplayGainTrackPeak,
                song.ReplayGainAlbumGainDb, song.ReplayGainAlbumPeak);
            // A value Octo wrote under one name is the only one: a peer's copy under another name
            // the library server also reads would be a second label or a second barcode.
            foreach (var (field, value) in new (TagField, string?)[]
                     {
                         (TagFields.Barcode, song.Barcode), (TagFields.Label, song.Label), (TagFields.ReleaseType, song.ReleaseType),
                         (TagFields.ReleaseStatus, song.ReleaseStatus), (TagFields.ReleaseCountry, song.ReleaseCountry),
                     })
                if (!string.IsNullOrWhiteSpace(value)) ReleaseFactTags.DropOtherNames(tagFile, field);
            TagWriterExtras.SetAdvisory(tagFile, song.Advisory);

            // One chain (#51) instead of one Deezer URL: Apple's master of the album, the Cover
            // Art Archive when a fingerprint named the release, the catalog's own cover, then
            // Deezer, iTunes and Last.fm by name, then the file's own art; the largest wins. A
            // cover that is not square counts as missing, and a letterboxed video frame gives up
            // its centre. The file gets it at 1500 px unless full size is asked for; cover.jpg
            // gets it whole.
            try
            {
                // The art of the album the file was filed away from is not this album's, so it never
                // outranks the chain; it stays on the file only when the chain finds nothing.
                var embedded = filedElsewhere ? null
                    : tagFile.Tag.Pictures.FirstOrDefault(picture => picture.Type == TagLib.PictureType.FrontCover)
                        ?? tagFile.Tag.Pictures.FirstOrDefault();
                var resolver = _serviceProvider.GetService<Octo.Services.CoverArt.DownloadCoverResolver>();
                var cover = resolver is null ? null
                    : await resolver.ResolveAsync(song, embedded?.Data?.Data, cancellationToken);
                if (cover is not null)
                {
                    chosenCover = cover.Bytes;
                    if (!cover.KeepsExisting)
                    {
                        var embed = MetadataSettingsValue.EmbedFullSizeCovers ? cover.Bytes
                            : Octo.Services.CoverArt.CoverImage.FitWithin(cover.Bytes, MetadataSettings.EmbeddedCoverSide);
                        tagFile.Tag.Pictures = new TagLib.IPicture[]
                        {
                            new TagLib.Picture
                            {
                                Type = TagLib.PictureType.FrontCover,
                                MimeType = Octo.Services.CoverArt.CoverImage.MimeType(embed),
                                Description = "Cover",
                                Data = new TagLib.ByteVector(embed),
                            },
                        };
                        Logger.LogInformation("Cover art embedded from {Source}: {Size} bytes", cover.Source, embed.Length);
                    }
                }
            }
            catch (Exception ex)
            {
                Logger.LogWarning(ex, "Could not choose a cover for {Path}", filePath);
            }
            
            tagFile.Save();
            Logger.LogInformation("Metadata written successfully to: {Path}", filePath);
            return chosenCover;
        }
        catch (Exception ex)
        {
            Logger.LogError(ex, "Failed to write metadata to: {Path}", filePath);
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
    /// Files beside the audio file. Best-effort; never fails a download.
    ///
    /// cover.jpg only in the Organized layout and only in a folder this download created.
    /// Navidrome ranks cover.* above embedded art, so in Flat every download shares one folder,
    /// in ByArtist one folder holds all of an artist's albums, and in an album folder that was
    /// already there one new track would change the whole album's cover. The one exception is
    /// a cover.jpg Octo wrote itself, in the Organized layout: a later track of the same album
    /// that found a larger cover replaces it, so a soft first track does not set the album's
    /// cover for good.
    /// </summary>
    protected Task WriteSidecarsAsync(Song song, Placement placement, byte[]? cover, CancellationToken ct)
    {
        try
        {
            if (MetadataSettingsValue.WriteCoverFile && cover is { Length: > 0 }
                && SubsonicSettings.FolderStructure == FolderStructure.Organized
                && Path.GetDirectoryName(placement.Path) is { Length: > 0 } dir
                && Octo.Services.CoverArt.CoverFiles.ShouldWrite(dir, cover, placement.CreatedFolder))
            {
                Octo.Services.CoverArt.CoverFiles.Write(dir, cover);
                Logger.LogInformation("Wrote cover.jpg beside {Path}", placement.Path);
            }
        }
        catch (Exception ex)
        {
            Logger.LogWarning("Could not write cover.jpg beside {Path}: {M}", placement.Path, ex.Message);
        }

        // Lyrics are fetched in the background: this runs under the download lock, and a lyrics
        // service that is slow or shedding load must not hold up the next download (#52).
        if (MetadataSettingsValue.FetchLyrics
            && _serviceProvider.GetService<Octo.Services.Lyrics.LyricsSidecarWriter>() is { } lyrics)
            lyrics.TryEnqueue(new Octo.Services.Lyrics.LyricsJob(placement.Path,
                song.PrimaryArtist ?? song.Artist,
                Octo.Services.Lyrics.LyricsText.QueryTitle(song.Title, song.Artist),
                song.Album, song.Duration));
        return Task.CompletedTask;
    }

    /// <summary>
    /// A download a person can settle by listening goes to their Review playlist (#47): the ones
    /// who asked for it, or, when nobody on the allowlist did, whoever keeps the library.
    /// </summary>
    private void AskForReview(Song song, string localPath, IReadOnlyList<string>? requestedBy)
    {
        try
        {
            if (song.Verification is not { NeedsReview: true } verdict) return;
            if (_serviceProvider.GetService<IOptionsMonitor<LibraryActionSettings>>()?.CurrentValue
                is not { Enabled: true, ReviewEnabled: true } actions) return;
            if (_serviceProvider.GetService<Octo.Services.Library.NoticeQueue>() is not { } notices) return;

            var owners = (requestedBy ?? []).Where(actions.IsAllowed).ToList();
            if (owners.Count == 0) owners = (actions.AllowedUsers ?? []).Where(user => !string.IsNullOrWhiteSpace(user)).ToList();
            foreach (var owner in owners)
                if (notices.AddReview(owner, localPath, song, verdict))
                    Logger.LogInformation("Asking {User} about '{Artist} - {Title}': {Reason}",
                        owner, song.Artist, song.Title, verdict.Reason);
        }
        catch (Exception ex)
        {
            Logger.LogWarning("Could not queue '{Artist} - {Title}' for review: {M}", song.Artist, song.Title, ex.Message);
        }
    }

    private static readonly HashSet<string> AudioExtensions = new(StringComparer.OrdinalIgnoreCase)
    {
        ".flac", ".mp3", ".m4a", ".aac", ".alac", ".ogg", ".opus", ".wav", ".aiff", ".aif",
        ".ape", ".wv", ".wma", ".dsf",
    };

    /// <summary>
    /// Fail a download whose file is not on disk. Everything after the transfer carries on past a
    /// missing file (placement keeps the path, tagging logs and moves on), which is how a song was
    /// recorded as downloaded with 0 bytes and never placed (#69). Throwing marks the request
    /// Failed, writes no history, and lets the failure notice and any fallback source run.
    /// </summary>
    internal static void EnsureOnDisk(string? path)
    {
        long length = 0;
        try { if (!string.IsNullOrEmpty(path) && IOFile.Exists(path)) length = new FileInfo(path).Length; }
        catch { /* a file that cannot be read is no more use than a missing one */ }
        if (length > 0) return;
        throw new FileNotFoundException(string.IsNullOrEmpty(path)
            ? "The download returned no file"
            : $"The download returned {path}, but there is no audio there", path);
    }

    /// <summary>
    /// Move a finished download into the configured layout, named by ChooseLayout.
    ///
    /// Never overwrites a different file. A path that is already taken is replaced only when it
    /// provably holds this same song (see IsSameSongAsync); anything else keeps both. The move
    /// used to delete whatever sat at the target, which is how "Song (Live)" replaced "Song".
    /// Returns the landed path when the move fails, so the song is still registered.
    /// </summary>
    protected async Task<Placement> PlaceInLibraryAsync(Song song, RequestedIdentity requested, string currentPath)
    {
        try
        {
            if (string.IsNullOrEmpty(DownloadPath) || !IOFile.Exists(currentPath)) return new(currentPath, false);

            var structure = SubsonicSettings.FolderStructure;
            var target = LayoutTarget(song, requested, Path.GetExtension(currentPath));

            if (string.Equals(Path.GetFullPath(target), Path.GetFullPath(currentPath), StringComparison.OrdinalIgnoreCase))
                return new(currentPath, false);

            var targetDir = Path.GetDirectoryName(target);
            var folderHadAudio = !string.IsNullOrEmpty(targetDir) && Directory.Exists(targetDir)
                && Directory.EnumerateFiles(targetDir).Any(file => AudioExtensions.Contains(Path.GetExtension(file)));

            if (IOFile.Exists(target))
            {
                if (await IsSameSongAsync(target, song))
                {
                    Logger.LogInformation("{Target} already holds this song; replacing it with the new download", target);
                    IOFile.Delete(target);
                }
                else
                {
                    var unique = PathHelper.ResolveUniquePath(target);
                    Logger.LogInformation("{Target} already holds a different file; keeping both as {Unique}", target, unique);
                    target = unique;
                }
            }

            if (!string.IsNullOrEmpty(targetDir)) Directory.CreateDirectory(targetDir);
            IOFile.Move(currentPath, target);
            Logger.LogInformation("Placed download in {Layout}: {From} -> {To}", structure, currentPath, target);

            // Clean up any now-empty folder the file came from (a Soulseek peer's own layout, or
            // the YouTube staging folder), never walking above the music root.
            TryRemoveEmptyParents(Path.GetDirectoryName(currentPath), DownloadPath);
            return new(target, !folderHadAudio);
        }
        catch (Exception ex)
        {
            Logger.LogWarning(ex, "Could not place {Path} in the configured layout; leaving it where it landed", currentPath);
            return new(currentPath, false);
        }
    }

    /// <summary>Where the configured layout files this song, before any clash is looked at.</summary>
    private string LayoutTarget(Song song, RequestedIdentity requested, string extension)
    {
        var choice = ChooseLayout(song, requested, SoulseekSettingsValue.NameFromMatch);
        var structure = SubsonicSettings.FolderStructure;
        // Flat has no folder to scatter, so its file name keeps the whole credit (#49).
        var artist = structure == FolderStructure.Flat ? choice.FileArtist : choice.FolderArtist;
        return PathHelper.BuildLayoutPath(structure, DownloadPath,
            string.IsNullOrWhiteSpace(artist) ? "Unknown Artist" : artist,
            choice.Album, PathHelper.FileTitle(choice.Title, choice.FileArtist), choice.Track, extension);
    }

    /// <summary>Move a replacement into the incoming dot folder, which Navidrome never scans.</summary>
    protected Placement StageReplacement(string landedPath)
    {
        var incoming = Path.Combine(DownloadPath, Octo.Services.Soulseek.SoulseekDownloadService.IncomingFolderName);
        Directory.CreateDirectory(incoming);
        var staged = Path.Combine(incoming, $"replacement-{Guid.NewGuid():N}{Path.GetExtension(landedPath)}");
        IOFile.Move(landedPath, staged);
        TryRemoveEmptyParents(Path.GetDirectoryName(landedPath), DownloadPath);
        return new(staged, false);
    }

    /// <summary>
    /// Give the staged replacement the original's identity, let the library action judge it,
    /// then move it to the original's folder and name in one rename (W8). Nothing may scan it
    /// before its tags are final: Navidrome would file it as a new song for good. A refused one
    /// is deleted here, where no scan ever saw it.
    /// </summary>
    protected async Task<Placement> RevealReplacementAsync(Song song, RequestedIdentity requested,
        string staged, Octo.Services.Library.ReplacementHandoff handoff)
    {
        try
        {
            Octo.Services.Library.KeptIdentityTags.Apply(staged, handoff.Identity);
            if (await handoff.BeforeReveal(staged) is { } problem)
                throw new Octo.Services.Library.ReplacementRejectedException(problem);

            var extension = Path.GetExtension(staged);
            var target = handoff.TargetFor(extension)
                ?? PathHelper.ResolveUniquePath(LayoutTarget(song, requested, extension));
            Directory.CreateDirectory(Path.GetDirectoryName(target)!);
            IOFile.Move(staged, target);
            handoff.RevealedPath = target;
            handoff.OnRevealed?.Invoke(target);
            Logger.LogInformation("Placed the replacement where the original was: {Path}", target);
            return new(target, false);
        }
        finally
        {
            if (handoff.RevealedPath is null)
                try { if (IOFile.Exists(staged)) IOFile.Delete(staged); } catch { /* swept after a day */ }
        }
    }

    /// <summary>
    /// Whether an occupied path holds this same song: both carry the same MusicBrainz recording,
    /// or, lacking an id on either side, it is a file Octo itself downloaded for the same artist
    /// and title. Anything short of that is somebody's other file and is never replaced.
    /// </summary>
    private async Task<bool> IsSameSongAsync(string target, Song song)
    {
        var existingId = TagWriterExtras.ReadIdentity(target).RecordingId;
        if (!string.IsNullOrEmpty(existingId) && !string.IsNullOrEmpty(song.MusicBrainzRecordingId))
            return string.Equals(existingId, song.MusicBrainzRecordingId, StringComparison.OrdinalIgnoreCase);

        var full = Path.GetFullPath(target);
        var mappings = await LocalLibraryService.GetMappingsAsync();
        return mappings.Any(mapping =>
            !string.IsNullOrEmpty(mapping.LocalPath)
            && string.Equals(Path.GetFullPath(mapping.LocalPath), full, StringComparison.OrdinalIgnoreCase)
            && SongIdentity.MatchKey(mapping.Artist, mapping.Title) == SongIdentity.MatchKey(song.Artist, song.Title));
    }

    /// <summary>
    /// Which name a download is filed under. With NameFromMatch on and a confirmed match, the
    /// recording's, already applied to the Song. Otherwise the request's, except that a list of
    /// credits never names a folder and a request with no album takes the album it was tagged
    /// with, together with that album's track number.
    /// </summary>
    internal static LayoutChoice ChooseLayout(Song song, RequestedIdentity requested, bool nameFromMatch)
    {
        var match = song.Verification is { Verdict: Octo.Services.Fingerprint.VerificationVerdict.Confirmed, Match: { } m }
            ? m : null;

        if (nameFromMatch && match is not null)
            return new LayoutChoice(
                match.PrimaryArtist ?? song.PrimaryArtist ?? song.Artist, song.Artist,
                song.Title, song.Album ?? "", song.Track);

        var fromRequest = requested.Album.Length > 0;
        return new LayoutChoice(
            PrimaryCredit(requested.Artist, match?.PrimaryArtist, song.PrimaryArtist),
            requested.Artist,
            requested.Title,
            fromRequest ? requested.Album : song.Album ?? "",
            fromRequest ? requested.Track : song.Track);
    }

    private static readonly System.Text.RegularExpressions.Regex CreditSeparator = new(
        @"^\s*(,|&|\+|/|;|\bx\b|\bvs\.?|\bfeat\.?|\bft\.?|\bfeaturing\b|\bwith\b|\band\b)",
        System.Text.RegularExpressions.RegexOptions.IgnoreCase | System.Text.RegularExpressions.RegexOptions.Compiled);

    /// <summary>
    /// The artist a folder is named after (#49). Splits only on proof: a structured source (the
    /// MusicBrainz credit, Deezer's main artist) that names either the whole requested string,
    /// which keeps it whole, or its FIRST credit followed by a separator, which splits there.
    /// "Earth, Wind &amp; Fire" and "Tyler, The Creator" survive because every structured source
    /// names them whole. A separator on its own never splits anything.
    /// </summary>
    internal static string PrimaryCredit(string requested, params string?[] structured)
    {
        var whole = (requested ?? "").Trim();
        var candidates = structured.Where(value => !string.IsNullOrWhiteSpace(value))
            .Select(value => value!.Trim()).ToList();

        if (whole.Length == 0) return candidates.FirstOrDefault() ?? "";
        if (candidates.Any(candidate => candidate.Equals(whole, StringComparison.OrdinalIgnoreCase))) return whole;

        foreach (var candidate in candidates)
            if (whole.Length > candidate.Length
                && whole.StartsWith(candidate, StringComparison.OrdinalIgnoreCase)
                && CreditSeparator.IsMatch(whole[candidate.Length..]))
                return candidate;

        return whole;
    }

    /// <summary>Walk up from <paramref name="startDir"/> removing empty folders, never past
    /// <paramref name="stopAt"/>.</summary>
    protected static void TryRemoveEmptyParents(string? startDir, string stopAt)
    {
        if (string.IsNullOrEmpty(startDir) || string.IsNullOrEmpty(stopAt)) return;
        var stop = Path.GetFullPath(stopAt).TrimEnd(Path.DirectorySeparatorChar, Path.AltDirectorySeparatorChar);
        var current = Path.GetFullPath(startDir).TrimEnd(Path.DirectorySeparatorChar, Path.AltDirectorySeparatorChar);
        // Walk up while we're inside DownloadPath and the directory is empty.
        while (!string.IsNullOrEmpty(current)
            && current.Length > stop.Length
            && current.StartsWith(stop, StringComparison.OrdinalIgnoreCase)
            && Directory.Exists(current))
        {
            try
            {
                if (Directory.EnumerateFileSystemEntries(current).Any()) break;
                Directory.Delete(current);
            }
            catch { break; }
            current = Path.GetDirectoryName(current) ?? "";
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
