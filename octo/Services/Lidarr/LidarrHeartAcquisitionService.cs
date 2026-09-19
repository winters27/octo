using System.Collections.Concurrent;
using Microsoft.Extensions.Options;
using Octo.Models.Domain;
using Octo.Models.Download;
using Octo.Models.Settings;
using Octo.Services.Common;
using Octo.Services.Local;
using Octo.Services.Metadata;
using Octo.Services.Notifications;
using Octo.Services.Subsonic;

namespace Octo.Services.Lidarr;

public interface ILidarrHeartAcquisitionService
{
    Task<bool> TryAcquireTrackAsync(
        string provider, string externalId, bool notifyFailure = true,
        string? requestedBy = null);
    Task<bool> TryAcquireAlbumAsync(
        string provider, string externalId, bool notifyFailure = true,
        string? requestedBy = null);
}

/// <summary>
/// Submits Lidarr album searches without occupying Octo's serialized direct-download
/// worker, then reconciles imported files independently.
/// </summary>
public sealed class LidarrHeartAcquisitionService : ILidarrHeartAcquisitionService
{
    private readonly LidarrClient _client;
    private readonly IMusicMetadataService _metadata;
    private readonly DeezerMetadataService _deezer;
    private readonly IOptionsMonitor<LidarrSettings> _settings;
    private readonly IOptionsMonitor<SubsonicSettings> _subsonicSettings;
    private readonly IConfiguration _configuration;
    private readonly NavidromeIdentityService _navIdentity;
    private readonly ILocalLibraryService _library;
    private readonly DownloadHistoryService _history;
    private readonly NotificationService _notifications;
    private readonly ILogger<LidarrHeartAcquisitionService> _logger;
    private readonly ConcurrentDictionary<string, Lazy<Task>> _albumJobs = new();
    private readonly ConcurrentDictionary<string, byte> _recordedPaths = new(StringComparer.OrdinalIgnoreCase);

    // Who asked for each album, by Lidarr foreign id. Separate from _albumJobs because the
    // same dedup applies: a second user starring an album Lidarr is already working on joins
    // that job, and the import can land minutes later, so the set is read when the file is
    // recorded rather than captured when the job started.
    private readonly ConcurrentDictionary<string, ConcurrentDictionary<string, byte>> _albumRequesters = new();

    public LidarrHeartAcquisitionService(
        LidarrClient client,
        IMusicMetadataService metadata,
        DeezerMetadataService deezer,
        IOptionsMonitor<LidarrSettings> settings,
        IOptionsMonitor<SubsonicSettings> subsonicSettings,
        IConfiguration configuration,
        NavidromeIdentityService navIdentity,
        ILocalLibraryService library,
        DownloadHistoryService history,
        NotificationService notifications,
        ILogger<LidarrHeartAcquisitionService> logger)
    {
        _client = client;
        _metadata = metadata;
        _deezer = deezer;
        _settings = settings;
        _subsonicSettings = subsonicSettings;
        _configuration = configuration;
        _navIdentity = navIdentity;
        _library = library;
        _history = history;
        _notifications = notifications;
        _logger = logger;
        foreach (var entry in history.GetRecent(int.MaxValue))
            if (!string.IsNullOrWhiteSpace(entry.Path)) _recordedPaths.TryAdd(entry.Path, 0);
    }

    public Task<bool> TryAcquireTrackAsync(
        string provider, string externalId, bool notifyFailure = true,
        string? requestedBy = null) =>
        TryAcquireAsync(async () =>
        {
            var song = await _metadata.GetSongAsync(provider, externalId)
                ?? throw new InvalidOperationException("The starred external track is no longer available.");
            var enriched = await _deezer.EnrichTrackAsync(song.Artist, song.Title, includeYear: true);
            var albumTitle = enriched?.AlbumTitle;
            if (string.IsNullOrWhiteSpace(albumTitle)) albumTitle = song.Album;
            if (string.IsNullOrWhiteSpace(albumTitle))
                throw new InvalidOperationException($"Could not resolve an album for '{song.Artist} - {song.Title}'.");

            song.Album = albumTitle;
            song.CoverArtUrl ??= enriched?.AlbumCoverUrl;
            song.Year ??= enriched?.Year;
            var album = new Album
            {
                Title = albumTitle,
                Artist = enriched?.ArtistName ?? song.Artist,
                Year = enriched?.Year,
                CoverArtUrl = enriched?.AlbumCoverUrl,
                Songs = new List<Song> { song },
            };
            await QueueResolvedAlbumAsync(album, requestedBy);
        }, "track", externalId, notifyFailure);

    public Task<bool> TryAcquireAlbumAsync(
        string provider, string externalId, bool notifyFailure = true,
        string? requestedBy = null) =>
        TryAcquireAsync(async () =>
        {
            var album = await _metadata.GetAlbumAsync(provider, externalId)
                ?? throw new InvalidOperationException("The starred external album is no longer available.");
            await QueueResolvedAlbumAsync(album, requestedBy);
        }, "album", externalId, notifyFailure);

    private void AddRequester(string albumKey, string? username)
    {
        if (string.IsNullOrWhiteSpace(username)) return;
        _albumRequesters
            .GetOrAdd(albumKey, _ => new ConcurrentDictionary<string, byte>(StringComparer.OrdinalIgnoreCase))
            .TryAdd(username.Trim(), 0);
    }

    private IReadOnlyList<string>? RequestersFor(string albumKey) =>
        _albumRequesters.TryGetValue(albumKey, out var set) && !set.IsEmpty
            ? set.Keys.OrderBy(name => name, StringComparer.OrdinalIgnoreCase).ToList()
            : null;

    private async Task QueueResolvedAlbumAsync(Album album, string? requestedBy = null)
    {
        if (string.IsNullOrWhiteSpace(album.Artist) || string.IsNullOrWhiteSpace(album.Title))
            throw new InvalidOperationException("Lidarr requires an album artist and title.");

        var candidate = await _client.ResolveAlbumAsync(album.Artist, album.Title, album.Year);
        // Before GetOrAdd, so a caller that joins an existing job is still recorded.
        AddRequester(candidate.ForeignAlbumId, requestedBy);
        var lazy = _albumJobs.GetOrAdd(candidate.ForeignAlbumId,
            _ => new Lazy<Task>(() => SubmitAndStartReconciliationAsync(candidate, album),
                LazyThreadSafetyMode.ExecutionAndPublication));
        try
        {
            await lazy.Value;
        }
        catch
        {
            _albumJobs.TryRemove(new KeyValuePair<string, Lazy<Task>>(candidate.ForeignAlbumId, lazy));
            throw;
        }
    }

    private async Task SubmitAndStartReconciliationAsync(
        LidarrAlbumCandidate candidate, Album album)
    {
        var snapshot = _settings.CurrentValue;
        var albumId = await _client.EnsureAlbumAndSearchAsync(candidate);
        _logger.LogInformation("Lidarr accepted AlbumSearch for '{Artist} - {Album}' ({ForeignId}, local id {Id})",
            album.Artist, album.Title, candidate.ForeignAlbumId, albumId);
        _notifications.Notify(new NotificationEvent
        {
            Type = NotificationEventType.DownloadStarted,
            Artist = album.Artist,
            Title = album.Title,
            Album = album.Title,
            Source = "Lidarr",
            CoverArtUrl = album.CoverArtUrl,
            Detail = "Album search accepted",
        });

        _ = Task.Run(async () =>
        {
            try
            {
                await ReconcileImportsAsync(albumId, album, snapshot, candidate.ForeignAlbumId);
            }
            catch (Exception ex)
            {
                _logger.LogError(ex, "Lidarr import reconciliation failed for '{Artist} - {Album}'",
                    album.Artist, album.Title);
                if (snapshot.CompletionMode == LidarrCompletionMode.Imported)
                {
                    _notifications.Notify(new NotificationEvent
                    {
                        Type = NotificationEventType.DownloadFailed,
                        Artist = album.Artist,
                        Title = album.Title,
                        Album = album.Title,
                        Source = "Lidarr",
                        CoverArtUrl = album.CoverArtUrl,
                        Detail = ex.Message,
                    });
                }
            }
            finally
            {
                _albumJobs.TryRemove(candidate.ForeignAlbumId, out _);
            }
        });
    }

    private async Task ReconcileImportsAsync(int albumId, Album album, LidarrSettings settings,
        string albumKey)
    {
        var timeout = TimeSpan.FromSeconds(Math.Max(1, settings.ImportTimeoutSeconds));
        var poll = TimeSpan.FromSeconds(Math.Clamp(settings.ImportTimeoutSeconds / 30, 1, 10));
        var deadline = DateTime.UtcNow + timeout;
        var imported = 0;
        var expected = 0;
        var visibleToOcto = 0;

        var octoRoot = _navIdentity.EffectiveDownloadPath(_configuration["Library:DownloadPath"] ?? "/music");

        while (DateTime.UtcNow < deadline)
        {
            var state = await _client.GetAlbumImportStateAsync(albumId);
            var tracks = state.Tracks;
            expected = state.TrackCount;
            visibleToOcto = 0;
            var visible = tracks
                .Where(t => t.HasFile && !string.IsNullOrWhiteSpace(t.Path))
                .GroupBy(t => t.Path!, StringComparer.OrdinalIgnoreCase)
                .Select(group => group.First())
                .ToList();
            foreach (var track in visible)
            {
                var importedPath = TranslateImportedPath(track.Path!, settings.RootFolderPath, octoRoot);
                if (!File.Exists(importedPath)) continue;
                var localPath = NormalizeImportedLayout(importedPath, album, track, octoRoot);
                visibleToOcto++;
                if (!_recordedPaths.TryAdd(localPath, 0)) continue;
                try
                {
                    await RecordImportAsync(album, track, localPath, RequestersFor(albumKey));
                    imported++;
                }
                catch
                {
                    _recordedPaths.TryRemove(localPath, out _);
                    throw;
                }
            }

            if (state.IsComplete && visible.Count > 0 && visibleToOcto == visible.Count)
            {
                if (imported > 0) await _library.TriggerLibraryScanAsync(force: true);
                if (settings.CompletionMode == LidarrCompletionMode.Imported && imported > 0)
                {
                    _notifications.Notify(new NotificationEvent
                    {
                        Type = NotificationEventType.AlbumCompleted,
                        Artist = album.Artist,
                        Title = album.Title,
                        CoverArtUrl = album.CoverArtUrl,
                        TrackCount = state.TrackCount,
                        LosslessCount = visible.Count(t =>
                            string.Equals(Path.GetExtension(t.Path), ".flac", StringComparison.OrdinalIgnoreCase)),
                        FailedCount = 0,
                    });
                }
                return;
            }

            await Task.Delay(poll);
        }

        if (imported > 0) await _library.TriggerLibraryScanAsync(force: true);
        var detail = $"Lidarr import timed out after {(int)timeout.TotalMinutes} minute(s)"
                     + (expected > 0 ? $" ({visibleToOcto}/{expected} files visible to Octo)" : "");
        _logger.LogWarning("{Detail} for '{Artist} - {Album}'", detail, album.Artist, album.Title);
        if (settings.CompletionMode == LidarrCompletionMode.Imported)
        {
            _notifications.Notify(new NotificationEvent
            {
                Type = NotificationEventType.DownloadFailed,
                Artist = album.Artist,
                Title = album.Title,
                Album = album.Title,
                Source = "Lidarr",
                CoverArtUrl = album.CoverArtUrl,
                Detail = detail,
            });
        }
    }

    private async Task RecordImportAsync(Album album, LidarrImportedTrack imported, string localPath,
        IReadOnlyList<string>? requestedBy = null)
    {
        var song = MatchSong(album, imported) ?? new Song
        {
            Artist = imported.Artist ?? album.Artist,
            Title = imported.Title,
            Album = album.Title,
            Track = imported.TrackNumber,
            Duration = imported.DurationSeconds,
            CoverArtUrl = album.CoverArtUrl,
            IsLocal = false,
        };

        if (!string.IsNullOrWhiteSpace(song.ExternalProvider) && !string.IsNullOrWhiteSpace(song.ExternalId))
            await _library.RegisterDownloadedSongAsync(song, localPath);

        var ext = Path.GetExtension(localPath).TrimStart('.').ToUpperInvariant();
        long size = imported.SizeBytes;
        if (size <= 0) try { size = new FileInfo(localPath).Length; } catch { /* best effort */ }
        _history.Record(new DownloadHistoryEntry
        {
            Artist = song.Artist,
            Title = song.Title,
            Album = album.Title,
            Path = localPath,
            Format = string.IsNullOrEmpty(ext) ? "?" : ext,
            Source = "Lidarr",
            CoverArtUrl = song.CoverArtUrlLarge ?? song.CoverArtUrl ?? album.CoverArtUrl,
            SizeBytes = size,
            DownloadedAt = DateTime.UtcNow.ToString("o"),
            RequestedBy = requestedBy is { Count: > 0 } ? [.. requestedBy] : null,
        });
    }

    internal static Song? MatchSong(Album album, LidarrImportedTrack track)
    {
        var normalized = LidarrClient.Normalize(track.Title);
        var byTitle = album.Songs.Where(s => LidarrClient.Normalize(s.Title) == normalized).ToList();
        if (byTitle.Count == 1) return byTitle[0];
        if (track.TrackNumber is int number)
        {
            var byNumber = album.Songs.Where(s => s.Track == number).ToList();
            if (byNumber.Count == 1) return byNumber[0];
        }
        return byTitle.FirstOrDefault();
    }

    internal static string TranslateImportedPath(string lidarrPath, string? lidarrRoot, string octoRoot)
    {
        if (string.IsNullOrWhiteSpace(lidarrRoot))
            throw new InvalidOperationException("Lidarr root folder is not configured.");
        var root = Path.GetFullPath(lidarrRoot);
        var source = Path.GetFullPath(lidarrPath);
        var relative = Path.GetRelativePath(root, source);
        if (relative == ".." || relative.StartsWith($"..{Path.DirectorySeparatorChar}", StringComparison.Ordinal))
            throw new InvalidOperationException($"Lidarr imported a path outside its configured root: {lidarrPath}");
        var targetRoot = Path.GetFullPath(octoRoot);
        var target = Path.GetFullPath(Path.Combine(targetRoot, relative));
        if (Path.GetRelativePath(targetRoot, target).StartsWith("..", StringComparison.Ordinal))
            throw new InvalidOperationException("Translated Lidarr path escaped Octo's library root.");
        return target;
    }

    /// <summary>
    /// Lidarr does not always rename an import into an Artist/Album layout: a release with
    /// thin MusicBrainz metadata can land under its raw staging folder name (e.g.
    /// "HELLHOUND {mbid:...} {Album}") with the peer's original filename still attached.
    /// Re-home it into the same layout PathHelper.BuildTrackPath gives direct Soulseek
    /// downloads, so Navidrome never sees Lidarr-sourced tracks organized differently.
    /// </summary>
    private string NormalizeImportedLayout(string importedPath, Album album, LidarrImportedTrack track, string octoRoot)
    {
        try
        {
            var ext = Path.GetExtension(importedPath);
            var title = string.IsNullOrWhiteSpace(track.Title) ? album.Title : track.Title;

            // Follow the SAME setting the Soulseek path follows. Building the Artist/Album
            // layout unconditionally would put Lidarr imports in folders while a Flat
            // library keeps everything in one directory, which is the inconsistency this
            // is here to remove, and Flat is the default.
            var canonicalPath = PathHelper.BuildLayoutPath(
                _subsonicSettings.CurrentValue.FolderStructure, octoRoot,
                album.Artist, album.Title, title, track.TrackNumber, ext);
            if (string.Equals(Path.GetFullPath(canonicalPath), Path.GetFullPath(importedPath), StringComparison.OrdinalIgnoreCase))
                return importedPath;

            var targetDir = Path.GetDirectoryName(canonicalPath);
            if (!string.IsNullOrEmpty(targetDir)) Directory.CreateDirectory(targetDir);
            canonicalPath = PathHelper.ResolveUniquePath(canonicalPath);
            File.Move(importedPath, canonicalPath);

            var oldDir = Path.GetDirectoryName(importedPath);
            if (!string.IsNullOrEmpty(oldDir) && Directory.Exists(oldDir)
                && !Directory.EnumerateFileSystemEntries(oldDir).Any())
                Directory.Delete(oldDir);

            return canonicalPath;
        }
        catch
        {
            // Best effort: keep the file registered where Lidarr put it rather than losing it.
            return importedPath;
        }
    }

    private async Task<bool> TryAcquireAsync(
        Func<Task> work, string kind, string externalId, bool notifyFailure)
    {
        try
        {
            await work();
            return true;
        }
        catch (Exception ex)
        {
            _logger.LogError(ex, "Lidarr {Kind} heart failed for {Id}", kind, externalId);
            if (notifyFailure) _notifications.Notify(new NotificationEvent
            {
                Type = NotificationEventType.DownloadFailed,
                Source = "Lidarr",
                Detail = ex.Message,
            });
            return false;
        }
    }
}
