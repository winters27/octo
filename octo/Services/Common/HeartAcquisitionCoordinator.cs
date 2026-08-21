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

    public HeartAcquisitionCoordinator(IOptionsMonitor<SubsonicSettings> settings,
        TrackAcquisitionQueue directQueue, IDownloadService directDownloads,
        ILidarrHeartAcquisitionService lidarr)
    {
        _settings = settings;
        _directQueue = directQueue;
        _directDownloads = directDownloads;
        _lidarr = lidarr;
    }

    public void QueueTrack(string provider, string externalId)
    {
        _ = AcquireTrackAsync(provider, externalId);
    }

    public void QueueAlbum(string provider, string albumExternalId)
    {
        _ = AcquireAlbumAsync(provider, albumExternalId);
    }

    internal async Task AcquireTrackAsync(string provider, string externalId)
    {
        var steps = EnabledSteps(albumHeart: false);
        for (var index = 0; index < steps.Count; index++)
        {
            var isLast = index == steps.Count - 1;
            if (steps[index] == HeartDownloadSource.Lidarr)
            {
                if (await _lidarr.TryAcquireTrackAsync(provider, externalId, isLast)) return;
                continue;
            }

            try
            {
                await _directQueue.Enqueue(provider, externalId, isStar: true,
                    triggerAlbumDownload: false, forcePermanent: true,
                    sourceOverride: ToDirectSource(steps[index]), notifyOnFailure: isLast);
                return;
            }
            catch
            {
                if (isLast) return;
                // The next enabled source owns the fallback.
            }
        }
    }

    internal async Task AcquireAlbumAsync(string provider, string albumExternalId)
    {
        var steps = EnabledSteps(albumHeart: true);
        for (var index = 0; index < steps.Count; index++)
        {
            var isLast = index == steps.Count - 1;
            if (steps[index] == HeartDownloadSource.Lidarr)
            {
                if (await _lidarr.TryAcquireAlbumAsync(provider, albumExternalId, isLast)) return;
                continue;
            }

            try
            {
                if (await _directDownloads.DownloadAlbumWithSourceAsync(
                        provider, albumExternalId, ToDirectSource(steps[index]),
                        suppressSummary: !isLast))
                    return;
            }
            catch
            {
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
