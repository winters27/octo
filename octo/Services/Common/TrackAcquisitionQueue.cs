using System.Collections.Concurrent;
using System.Threading.Channels;
using Octo.Models.Settings;

namespace Octo.Services.Common;

/// <summary>One queued request to fetch a permanent copy of a track.</summary>
public sealed class AcquisitionRequest
{
    public required string Provider { get; init; }
    public required string ExternalId { get; init; }

    /// <summary>Carried per request rather than inferred from whoever won a race:
    /// heart routing decides song-versus-album scope, while playback never expands to an album.</summary>
    public bool TriggerAlbumDownload { get; init; }
    public bool ForcePermanent { get; init; }
    public bool IsStar { get; init; }
    public DownloadSource? SourceOverride { get; init; }
    public bool NotifyOnFailure { get; init; } = true;

    /// <summary>
    /// RunContinuationsAsynchronously is required. Without it, completing this runs the
    /// waiting request's continuation — response headers, body writes, the client's whole
    /// download — inline on the worker thread, stalling the queue behind the slowest client.
    /// </summary>
    public TaskCompletionSource<string> Completion { get; } =
        new(TaskCreationOptions.RunContinuationsAsynchronously);

    public string Key => $"{Provider}:{ExternalId}";
}

/// <summary>
/// Serialises permanent-copy downloads onto a single background worker.
///
/// The point is that a download must never be tied to the HTTP request that asked for it.
/// Playing a track used to run the whole Soulseek transfer inside the stream request, so a
/// client giving up cancelled the transfer mid-flight while slskd finished the file anyway,
/// and Octo kept no record of it (issue #9).
///
/// Stars get their own channel and are never dropped: a star is explicit user intent,
/// whereas a play is a hint that can be shed under load.
/// </summary>
public sealed class TrackAcquisitionQueue
{
    // A play storm (skipping through a queue) must not be able to enqueue unbounded work,
    // but a star must always land, so the two channels are bounded differently.
    private const int PlayCapacity = 32;
    private const int StarCapacity = 512;

    private readonly Channel<AcquisitionRequest> _plays =
        Channel.CreateBounded<AcquisitionRequest>(new BoundedChannelOptions(PlayCapacity)
        {
            // Wait, NOT DropWrite: DropWrite makes TryWrite return true while discarding,
            // which is the opposite of the logged, observable drop we want.
            FullMode = BoundedChannelFullMode.Wait,
            SingleReader = true,
        });

    private readonly Channel<AcquisitionRequest> _stars =
        Channel.CreateBounded<AcquisitionRequest>(new BoundedChannelOptions(StarCapacity)
        {
            FullMode = BoundedChannelFullMode.Wait,
            SingleReader = true,
        });

    // Producer-side, so a duplicate never consumes a queue slot at all. Consumer-side dedup
    // would be too late: Feishin re-requests /rest/stream on seek, so duplicates are routine.
    private readonly ConcurrentDictionary<string, AcquisitionRequest> _inFlight = new();

    private readonly ILogger<TrackAcquisitionQueue> _logger;

    public TrackAcquisitionQueue(ILogger<TrackAcquisitionQueue> logger) => _logger = logger;

    /// <summary>
    /// Queue an acquisition, or join the one already running for this track. The returned
    /// task completes when the file is on disk and fully registered; callers that do not
    /// care may ignore it entirely.
    /// </summary>
    public Task<string> Enqueue(string provider, string externalId, bool isStar,
        bool triggerAlbumDownload, bool forcePermanent,
        DownloadSource? sourceOverride = null, bool notifyOnFailure = true)
    {
        var request = new AcquisitionRequest
        {
            Provider = provider,
            ExternalId = externalId,
            IsStar = isStar,
            TriggerAlbumDownload = triggerAlbumDownload,
            ForcePermanent = forcePermanent,
            SourceOverride = sourceOverride,
            NotifyOnFailure = notifyOnFailure,
        };

        var existing = _inFlight.GetOrAdd(request.Key, request);
        if (!ReferenceEquals(existing, request))
        {
            // Already queued or running. Join it rather than fetching the same file twice.
            return existing.Completion.Task;
        }

        var channel = isStar ? _stars : _plays;
        if (!channel.Writer.TryWrite(request))
        {
            // Full. Say so out loud: silently shedding work is how "why didn't that
            // download?" becomes unanswerable.
            _logger.LogWarning(
                "Acquisition queue full ({Kind}); dropped {Provider}:{Id}",
                isStar ? "star" : "play", provider, externalId);
            Release(request);
            request.Completion.TrySetException(
                new InvalidOperationException("Acquisition queue is full; try again shortly."));
            return request.Completion.Task;
        }

        _logger.LogInformation("Queued {Kind} acquisition for {Provider}:{Id}",
            isStar ? "star" : "play", provider, externalId);
        return request.Completion.Task;
    }

    /// <summary>
    /// Take the next request, preferring stars. Returns null when both channels complete.
    /// </summary>
    internal async Task<AcquisitionRequest?> DequeueAsync(CancellationToken ct)
    {
        while (!ct.IsCancellationRequested)
        {
            if (_stars.Reader.TryRead(out var star)) return star;
            if (_plays.Reader.TryRead(out var play)) return play;

            var starWait = _stars.Reader.WaitToReadAsync(ct).AsTask();
            var playWait = _plays.Reader.WaitToReadAsync(ct).AsTask();
            var ready = await Task.WhenAny(starWait, playWait);
            if (!await ready) continue;
        }
        return null;
    }

    /// <summary>
    /// Give up the dedup claim. MUST run on every terminal outcome, including a drop and a
    /// failure, not just on success: a leaked claim makes every future request for that
    /// track join a job that will never run.
    /// </summary>
    internal void Release(AcquisitionRequest request) =>
        _inFlight.TryRemove(new KeyValuePair<string, AcquisitionRequest>(request.Key, request));
}
