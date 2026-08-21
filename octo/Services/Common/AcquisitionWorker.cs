using Octo.Services.Notifications;
using Octo.Services.Soulseek;

namespace Octo.Services.Common;

/// <summary>
/// Drains <see cref="TrackAcquisitionQueue"/>.
///
/// Exactly ONE worker, deliberately, and this is not a knob. Two would reinstate the race
/// between the existence check and the in-progress marker in DownloadSongInternalAsync, and
/// ResolveLocalPath matches on leaf filename with a 64KB size tolerance across the whole
/// music directory, so two concurrent transfers can claim and move each other's files.
/// Concurrency here is unsafe until that resolution is made deterministic.
/// </summary>
public sealed class AcquisitionWorker : BackgroundService
{
    private readonly TrackAcquisitionQueue _queue;
    private readonly IDownloadService _downloads;
    private readonly ExternalIdRegistry _idRegistry;
    private readonly NotificationService _notifications;
    private readonly ILogger<AcquisitionWorker> _logger;

    public AcquisitionWorker(TrackAcquisitionQueue queue, IDownloadService downloads,
        ExternalIdRegistry idRegistry, NotificationService notifications,
        ILogger<AcquisitionWorker> logger)
    {
        _queue = queue;
        _downloads = downloads;
        _idRegistry = idRegistry;
        _notifications = notifications;
        _logger = logger;
    }

    protected override async Task ExecuteAsync(CancellationToken stoppingToken)
    {
        _logger.LogInformation("Acquisition worker started");

        while (!stoppingToken.IsCancellationRequested)
        {
            AcquisitionRequest? request;
            try
            {
                request = await _queue.DequeueAsync(stoppingToken);
            }
            catch (OperationCanceledException)
            {
                break;
            }
            if (request is null) break;

            // Per-item catch is mandatory: BackgroundServiceExceptionBehavior defaults to
            // StopHost, so a single unhandled exception here would take Octo down.
            try
            {
                // CancellationToken.None, not stoppingToken. The transfer must not be
                // cancellable by anything other than the process ending: that is the
                // difference between "the client left" and "the download is lost".
                var path = await _downloads.ExecuteAcquisitionAsync(
                    request.Provider, request.ExternalId,
                    request.TriggerAlbumDownload, request.ForcePermanent,
                    request.SourceOverride,
                    CancellationToken.None);

                request.Completion.TrySetResult(path);
                _logger.LogInformation("Acquisition finished for {Provider}:{Id} -> {Path}",
                    request.Provider, request.ExternalId, path);
            }
            catch (Exception ex)
            {
                _logger.LogError(ex, "Acquisition failed for {Provider}:{Id}",
                    request.Provider, request.ExternalId);
                // Stars only: a shed play-triggered acquisition is a hint, a failed
                // star is the user's explicit ask going unmet. This is the one place
                // a terminal failure surfaces exactly once per gesture (album-walk
                // per-track failures are caught inside the walk and aggregate into
                // its summary instead).
                if (request.IsStar && request.NotifyOnFailure) NotifyFailed(request, ex);
                // Release before completing the task: an ordered heart fallback may
                // immediately enqueue the same track for its next source.
                _queue.Release(request);
                request.Completion.TrySetException(ex);
            }
            finally
            {
                // Every terminal outcome, or the next request for this track joins a job
                // that has already finished and will never complete again.
                _queue.Release(request);
            }
        }

        _logger.LogInformation("Acquisition worker stopped");
    }

    /// <summary>
    /// Own try/catch on purpose: a notification error must never disturb the
    /// TrySetException/queue-release bookkeeping around it. Metadata comes from the
    /// same routing pair the download path itself uses, so the names match what the
    /// user starred.
    /// </summary>
    private void NotifyFailed(AcquisitionRequest request, Exception ex)
    {
        try
        {
            var routing = _idRegistry.Lookup(request.ExternalId)
                ?? SoulseekMetadataService.TryDecodeExternalId(request.ExternalId);
            _notifications.Notify(new NotificationEvent
            {
                Type = NotificationEventType.DownloadFailed,
                Artist = routing?.Artist,
                Title = routing?.Title,
                Album = routing?.Album,
                Detail = ex.Message,
            });
        }
        catch (Exception nex)
        {
            _logger.LogWarning("Failure notification skipped: {Msg}", nex.Message);
        }
    }
}
