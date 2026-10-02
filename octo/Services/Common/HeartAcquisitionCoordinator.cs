using System.Collections.Concurrent;
using Microsoft.Extensions.Options;
using Octo.Models.Settings;
using Octo.Services.Lidarr;

namespace Octo.Services.Common;

/// <summary>
/// Routes explicit heart gestures, and plays when DownloadOnPlay or LidarrAlbumOnPlay ask
/// for it.
/// </summary>
public sealed class HeartAcquisitionCoordinator
{
    private readonly IOptionsMonitor<SubsonicSettings> _settings;
    private readonly TrackAcquisitionQueue _directQueue;
    private readonly IDownloadService _directDownloads;
    private readonly ILidarrHeartAcquisitionService _lidarr;
    private readonly ILogger<HeartAcquisitionCoordinator> _logger;

    /// <summary>Optional so a coordinator built without one still routes. The chain is the only
    /// place that knows which failure is the last, so it is the one that reports it.</summary>
    private readonly AcquisitionTracker? _tracker;

    public HeartAcquisitionCoordinator(IOptionsMonitor<SubsonicSettings> settings,
        TrackAcquisitionQueue directQueue, IDownloadService directDownloads,
        ILidarrHeartAcquisitionService lidarr, ILogger<HeartAcquisitionCoordinator> logger,
        AcquisitionTracker? tracker = null)
    {
        _settings = settings;
        _directQueue = directQueue;
        _directDownloads = directDownloads;
        _lidarr = lidarr;
        _logger = logger;
        _tracker = tracker;
    }

    public void QueueTrack(string provider, string externalId, string? requestedBy = null)
    {
        _ = AcquireTrackAsync(provider, externalId, requestedBy);
    }

    // Track ids already handed to Lidarr on play. Each hand-off is an AlbumSearch against
    // every indexer, and clients request /rest/stream again on every seek.
    private readonly ConcurrentDictionary<string, byte> _lidarrPlays = new();

    public void QueuePlay(string provider, string externalId, string? requestedBy = null)
    {
        var settings = _settings.CurrentValue;
        // WaitForLosslessOnPlay acquires the track itself, from its own source.
        if (settings.DownloadOnPlay && !settings.WaitForLosslessOnPlay && PlaySource() is DownloadSource source)
            _ = _directQueue.Enqueue(provider, externalId, isStar: false, triggerAlbumDownload: false,
                forcePermanent: true, sourceOverride: source, notifyOnFailure: false, requestedBy: requestedBy);
        if (settings.LidarrAlbumOnPlay && _lidarrPlays.TryAdd($"{provider}:{externalId}", 0))
            _ = _lidarr.TryAcquireTrackAsync(provider, externalId, notifyFailure: false, requestedBy);
    }

    public void QueueAlbum(string provider, string albumExternalId, string? requestedBy = null)
    {
        _ = AcquireAlbumAsync(provider, albumExternalId, requestedBy);
    }

    internal async Task AcquireTrackAsync(string provider, string externalId,
        string? requestedBy = null)
    {
        var steps = EnabledSteps(albumHeart: false);
        for (var index = 0; index < steps.Count; index++)
        {
            var isLast = index == steps.Count - 1;
            // One entry for the whole chain. The first direct source is still waiting for the
            // worker; every later source, and Lidarr, starts over by looking.
            _tracker?.Stage(provider, externalId,
                index == 0 && steps[index] != HeartDownloadSource.Lidarr
                    ? AcquisitionState.Queued : AcquisitionState.Searching,
                SourceName(steps[index]));
            if (steps[index] == HeartDownloadSource.Lidarr)
            {
                if (await _lidarr.TryAcquireTrackAsync(provider, externalId, isLast, requestedBy)) return;
                continue;
            }

            try
            {
                await _directQueue.Enqueue(provider, externalId, isStar: true,
                    triggerAlbumDownload: false, forcePermanent: true,
                    sourceOverride: ToDirectSource(steps[index]), notifyOnFailure: isLast,
                    // Passed on every step, not only the first. A track that fails its way
                    // down the source chain is still the same person's star.
                    requestedBy: requestedBy);
                return;
            }
            catch (Exception ex)
            {
                // A muted mid-chain failure must still leave a trace, or a track that
                // silently fell through every source is undiagnosable from the logs.
                _logger.LogWarning("Heart source {Source} failed for track {Provider}:{Id}: {Message}",
                    steps[index], provider, externalId, ex.Message);
                if (isLast)
                {
                    _tracker?.Fail(provider, externalId, ex.Message);
                    return;
                }
                // The next enabled source owns the fallback.
            }
        }
        // Only reached when the last source was Lidarr and it said no. It records its own
        // reason first; this is the fallback when it could not.
        _tracker?.Fail(provider, externalId, "No download source could get this song.");
    }

    internal async Task AcquireAlbumAsync(string provider, string albumExternalId,
        string? requestedBy = null)
    {
        var steps = EnabledSteps(albumHeart: true);
        for (var index = 0; index < steps.Count; index++)
        {
            var isLast = index == steps.Count - 1;
            if (steps[index] == HeartDownloadSource.Lidarr)
            {
                if (await _lidarr.TryAcquireAlbumAsync(provider, albumExternalId, isLast, requestedBy)) return;
                continue;
            }

            try
            {
                if (await _directDownloads.DownloadAlbumWithSourceAsync(
                        provider, albumExternalId, ToDirectSource(steps[index]),
                        suppressSummary: !isLast,
                        requestedBy: requestedBy is null ? null : [requestedBy]))
                    return;
            }
            catch (Exception ex)
            {
                _logger.LogWarning("Heart source {Source} failed for album {Provider}:{Id}: {Message}",
                    steps[index], provider, albumExternalId, ex.Message);
                if (isLast)
                {
                    _tracker?.FailAlbum(provider, albumExternalId, ex.Message);
                    return;
                }
                // Continue down the configured priority list.
            }
        }
        // Every source has had its go. Tracks the last walk already settled keep what it said;
        // this only closes the ones nothing finished.
        _tracker?.FailAlbum(provider, albumExternalId, "No download source could get this track.");
    }

    private List<HeartDownloadSource> EnabledSteps(bool albumHeart) =>
        _settings.CurrentValue.EffectiveHeartDownloadSources()
            .Where(step => albumHeart ? step.AlbumEnabled == true : step.SongEnabled == true)
            .Select(step => step.Source)
            .ToList();

    private DownloadSource? PlaySource()
    {
        var sources = EnabledSteps(albumHeart: false).Where(s => s != HeartDownloadSource.Lidarr).ToList();
        if (sources.Count == 0) return null;
        return sources[0] == HeartDownloadSource.YouTube ? DownloadSource.YouTube
            : sources.Contains(HeartDownloadSource.YouTube) ? DownloadSource.SoulseekThenYouTube
            : DownloadSource.Soulseek;
    }

    private static string SourceName(HeartDownloadSource source) => source switch
    {
        HeartDownloadSource.Lidarr => "Lidarr",
        HeartDownloadSource.YouTube => "YouTube",
        _ => "Soulseek",
    };

    private static DownloadSource ToDirectSource(HeartDownloadSource source) => source switch
    {
        HeartDownloadSource.YouTube => DownloadSource.YouTube,
        _ => DownloadSource.Soulseek,
    };
}
