using Microsoft.Extensions.Options;
using Octo.Models.Settings;
using Octo.Services.Lidarr;

namespace Octo.Services.Common;

/// <summary>Routes explicit heart gestures without changing playback acquisition.</summary>
public sealed class HeartAcquisitionCoordinator
{
    private readonly IOptionsMonitor<SubsonicSettings> _settings;
    private readonly TrackAcquisitionQueue _directQueue;
    private readonly IDownloadService _directDownloads;
    private readonly ILidarrHeartAcquisitionService _lidarr;
    private readonly ILogger<HeartAcquisitionCoordinator> _logger;

    public HeartAcquisitionCoordinator(IOptionsMonitor<SubsonicSettings> settings,
        TrackAcquisitionQueue directQueue, IDownloadService directDownloads,
        ILidarrHeartAcquisitionService lidarr, ILogger<HeartAcquisitionCoordinator> logger)
    {
        _settings = settings;
        _directQueue = directQueue;
        _directDownloads = directDownloads;
        _lidarr = lidarr;
        _logger = logger;
    }

    public void QueueTrack(string provider, string externalId, string? requestedBy = null)
    {
        _ = AcquireTrackAsync(provider, externalId, requestedBy);
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
                if (isLast) return;
                // The next enabled source owns the fallback.
            }
        }
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
                if (isLast) return;
                // Continue down the configured priority list.
            }
        }
    }

    private List<HeartDownloadSource> EnabledSteps(bool albumHeart) =>
        _settings.CurrentValue.EffectiveHeartDownloadSources()
            .Where(step => albumHeart ? step.AlbumEnabled == true : step.SongEnabled == true)
            .Select(step => step.Source)
            .ToList();

    private static DownloadSource ToDirectSource(HeartDownloadSource source) => source switch
    {
        HeartDownloadSource.YouTube => DownloadSource.YouTube,
        _ => DownloadSource.Soulseek,
    };
}
