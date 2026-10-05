using Octo.Services.Local;

namespace Octo.Services.Common;

/// <summary>
/// Saves each finished download's log with its entry in the fetched-songs log, so the dashboard
/// can show how an older download went after the live list has forgotten it (three hours). A
/// line that arrives after the end (lyrics, an upgrade's verdict) saves the log again. Only
/// watches: a save that fails is logged and never reaches the download.
/// </summary>
public sealed class DownloadLogKeeper
{
    private readonly AcquisitionTracker _tracker;
    private readonly DownloadHistoryService _history;
    private readonly ILogger _logger;

    private DownloadLogKeeper(AcquisitionTracker tracker, DownloadHistoryService history, ILogger logger)
    {
        _tracker = tracker;
        _history = history;
        _logger = logger;
    }

    /// <summary>Starts saving the logs of this tracker's finished downloads into this history.</summary>
    public static DownloadLogKeeper Attach(AcquisitionTracker tracker, DownloadHistoryService history, ILogger logger)
    {
        var keeper = new DownloadLogKeeper(tracker, history, logger);
        tracker.Ended += end =>
        {
            if (end.Done) keeper.Save(end.Key);
        };
        tracker.LoggedAfterEnd += keeper.Save;
        return keeper;
    }

    /// <summary>Saves the log of the row with this key, when it ended in the library.</summary>
    internal void Save(string key)
    {
        try
        {
            if (_tracker.Detail(key, username: null) is not { State: AcquisitionState.Done, Events: { Count: > 0 } events } row) return;
            _history.AttachLog(key, row.StartedAt, events);
        }
        catch (Exception ex)
        {
            _logger.LogDebug("Saving the log of {Key} failed: {Message}", key, ex.Message);
        }
    }
}
