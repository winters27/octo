namespace Octo.Services.Notifications;

public enum NotificationEventType
{
    DownloadStarted,
    DownloadCompleted,
    LosslessFallback,
    DownloadFailed,
    AlbumCompleted,

    /// <summary>The admin "Send test" button. Always allowed regardless of the
    /// per-event toggles; never fired by the download pipeline.</summary>
    Test,
}

/// <summary>
/// One thing that happened, in domain terms. Rendering to title/body text happens
/// once in <see cref="NotificationService.Render"/> so every transport says the
/// same thing. All fields except Type are optional on purpose: events fire from
/// paths where metadata may be partial, and a missing field must degrade the text,
/// never the send.
/// </summary>
public sealed record NotificationEvent
{
    public required NotificationEventType Type { get; init; }

    public string? Artist { get; init; }
    public string? Title { get; init; }
    public string? Album { get; init; }

    /// <summary>"FLAC" / "MP3" / "M4A".</summary>
    public string? Format { get; init; }

    /// <summary>"Soulseek" / "YouTube".</summary>
    public string? Source { get; init; }

    public string? CoverArtUrl { get; init; }

    /// <summary>For DownloadStarted this is the chosen candidate's advertised size;
    /// DownloadCompleted carries the real file's.</summary>
    public long? SizeBytes { get; init; }

    /// <summary>Track length in seconds. On the completed path this is the enriched
    /// song's duration (EnrichAndTagAsync runs before the hook); on started it is
    /// the routing's expected duration.</summary>
    public int? DurationSeconds { get; init; }

    public int? Year { get; init; }

    /// <summary>Fallback reason or failure message.</summary>
    public string? Detail { get; init; }

    // AlbumCompleted only. Counts cover the walked tracks; the track whose star
    // triggered the walk got its own DownloadCompleted.
    public int? TrackCount { get; init; }
    public int? LosslessCount { get; init; }
    public int? FailedCount { get; init; }

    /// <summary>
    /// Who asked for this, when Octo could tell. Empty for an acquisition Octo started
    /// itself, and for every event when the setting is off.
    /// </summary>
    public IReadOnlyList<string>? RequestedBy { get; init; }
}
