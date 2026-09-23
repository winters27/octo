using Microsoft.Extensions.Options;
using Octo.Models.Settings;

namespace Octo.Services.Notifications;

/// <summary>
/// Fans one domain event out to every configured transport, if its toggle is on.
///
/// The one unforgivable failure here would be disturbing a download, so the public
/// entry point is fire-and-forget and the whole dispatch is caught at two levels:
/// once around the body, and once per sink so one transport being down cannot delay
/// or kill the other. Rendering happens exactly once so ntfy and Discord always say
/// the same thing.
/// </summary>
public sealed class NotificationService
{
    /// <summary>Named HttpClient with a short timeout: a slow notification server
    /// must never be felt anywhere near the download path.</summary>
    public const string ClientName = "notifications";

    private readonly IReadOnlyList<INotificationSink> _sinks;
    private readonly IOptionsMonitor<NotificationSettings> _opts;
    private readonly ILogger<NotificationService> _logger;
    private readonly Octo.Services.Metadata.DeezerMetadataService? _deezer;

    // Deezer is optional so tests can construct the service without an HTTP
    // stack; the DI container resolves it in production.
    public NotificationService(
        IEnumerable<INotificationSink> sinks,
        IOptionsMonitor<NotificationSettings> opts,
        ILogger<NotificationService> logger,
        Octo.Services.Metadata.DeezerMetadataService? deezer = null)
    {
        _sinks = sinks.ToList();
        _opts = opts;
        _logger = logger;
        _deezer = deezer;
    }

    /// <summary>THE publish call. Never throws and never blocks the caller.</summary>
    public void Notify(NotificationEvent evt) => _ = NotifyInternalAsync(evt);

    /// <summary>Awaitable core, internal so tests can pin behavior deterministically
    /// instead of racing a discarded task.</summary>
    internal async Task NotifyInternalAsync(NotificationEvent evt)
    {
        try
        {
            var settings = _opts.CurrentValue;
            if (!IsEnabled(settings, evt.Type)) return;

            var configured = _sinks.Where(s => s.IsConfigured).ToList();
            if (configured.Count == 0) return;

            // Events fired from deep in the download path (Started, Fallback,
            // Failed) only carry artist/title — the routing has no artwork. This
            // whole method is already off the caller's path, so a cached Deezer
            // lookup here is free and gives every event art instead of only the
            // ones that happened to have it in scope.
            evt = await EnsureCoverArtAsync(evt);

            var message = Render(evt);
            await Task.WhenAll(configured.Select(async sink =>
            {
                try
                {
                    await sink.SendAsync(message, CancellationToken.None);
                }
                catch (Exception ex)
                {
                    _logger.LogWarning("Notification via {Sink} failed: {Msg}", sink.Name, ex.Message);
                }
            }));
        }
        catch (Exception ex)
        {
            _logger.LogWarning("Notification dispatch failed: {Msg}", ex.Message);
        }
    }

    /// <summary>Admin test button. Unlike Notify this awaits every sink and reports
    /// each outcome, including the transport's real error text on failure.</summary>
    public async Task<IReadOnlyList<NotificationTestResult>> SendTestAsync(CancellationToken ct)
    {
        var message = Render(new NotificationEvent { Type = NotificationEventType.Test });
        var results = new List<NotificationTestResult>();
        foreach (var sink in _sinks)
        {
            if (!sink.IsConfigured)
            {
                results.Add(new NotificationTestResult(sink.Name, Configured: false, Ok: false, "not configured"));
                continue;
            }
            try
            {
                await sink.SendAsync(message, ct);
                results.Add(new NotificationTestResult(sink.Name, Configured: true, Ok: true, "delivered"));
            }
            catch (Exception ex)
            {
                results.Add(new NotificationTestResult(sink.Name, Configured: true, Ok: false, ex.Message));
            }
        }
        return results;
    }

    /// <summary>Best-effort: fills a missing cover (and album) from Deezer's cached
    /// lookup. Any failure returns the event untouched — art is decoration, and
    /// decoration must never delay or break a notification.</summary>
    private async Task<NotificationEvent> EnsureCoverArtAsync(NotificationEvent evt)
    {
        if (!string.IsNullOrEmpty(evt.CoverArtUrl) || _deezer is null) return evt;
        if (string.IsNullOrEmpty(evt.Artist) || string.IsNullOrEmpty(evt.Title)) return evt;
        try
        {
            var meta = await _deezer.EnrichTrackAsync(evt.Artist, evt.Title, includeYear: false);
            if (meta is null) return evt;
            return evt with
            {
                CoverArtUrl = meta.AlbumCoverUrl,
                Album = string.IsNullOrEmpty(evt.Album) ? meta.AlbumTitle : evt.Album,
            };
        }
        catch
        {
            return evt;
        }
    }

    internal static bool IsEnabled(NotificationSettings s, NotificationEventType type) => type switch
    {
        NotificationEventType.DownloadStarted => s.NotifyDownloadStarted,
        NotificationEventType.DownloadCompleted => s.NotifyDownloadCompleted,
        NotificationEventType.LosslessFallback => s.NotifyLosslessFallback,
        NotificationEventType.DownloadFailed => s.NotifyDownloadFailed,
        NotificationEventType.AlbumCompleted => s.NotifyAlbumCompleted,
        // The test button bypasses toggles by design: it verifies transports.
        NotificationEventType.Test => true,
        _ => false,
    };

    /// <summary>Single source of truth for the text both transports carry. Track
    /// events additionally get structured stat fields so layout-capable transports
    /// can render a song card instead of prose.</summary>
    internal static NotificationMessage Render(NotificationEvent evt)
    {
        var track = string.IsNullOrEmpty(evt.Artist) ? evt.Title ?? "unknown track"
            : $"{evt.Artist} – {evt.Title}";

        return evt.Type switch
        {
            NotificationEventType.DownloadStarted => new NotificationMessage(
                evt.Type,
                $"Downloading: {track}",
                $"{evt.Format} via {evt.Source}" + (evt.SizeBytes is long sb and > 0 ? $" ({FormatSize(sb)})" : ""),
                evt.CoverArtUrl,
                Description: evt.Album,
                Fields: TrackFields(evt)),

            NotificationEventType.DownloadCompleted => new NotificationMessage(
                evt.Type,
                $"Downloaded: {track}",
                (string.IsNullOrEmpty(evt.Album) ? "" : evt.Album + "\n")
                    + $"{evt.Format} via {evt.Source}"
                    + (evt.SizeBytes is long cb and > 0 ? $", {FormatSize(cb)}" : "")
                    + (evt.DurationSeconds is int ds and > 0 ? $" · {FormatDuration(ds)}" : ""),
                evt.CoverArtUrl,
                Description: evt.Album,
                Fields: TrackFields(evt)),

            NotificationEventType.LosslessFallback => new NotificationMessage(
                evt.Type,
                $"Lossless miss: {track}",
                $"Soulseek failed ({evt.Detail ?? "no usable result"}); settling for YouTube MP3.",
                evt.CoverArtUrl),

            NotificationEventType.DownloadFailed => new NotificationMessage(
                evt.Type,
                $"Download failed: {track}",
                evt.Detail ?? "Every enabled source failed.",
                evt.CoverArtUrl),

            NotificationEventType.AlbumCompleted => new NotificationMessage(
                evt.Type,
                // Sent after every album walk, so it has to say when nothing arrived at all.
                evt.TrackCount is 0 && evt.FailedCount is > 0 ? $"Album failed: {track}" : $"Album complete: {track}",
                $"{evt.TrackCount} tracks fetched, {evt.LosslessCount} lossless"
                    + (evt.FailedCount is int f and > 0 ? $", {f} failed" : ""),
                evt.CoverArtUrl,
                Description: null,
                Fields: AlbumFields(evt)),

            _ => new NotificationMessage(
                evt.Type,
                "Octo test notification",
                "Transports are wired up correctly.",
                null),
        };
    }

    /// <summary>The stats a song card shows, in display order, skipping unknowns.
    /// Bitrate is derived from size and duration — it describes the actual file,
    /// which is the number a lossless-vs-lossy glance wants.</summary>
    private static List<KeyValuePair<string, string>> TrackFields(NotificationEvent evt)
    {
        var fields = new List<KeyValuePair<string, string>>();
        void Add(string name, string? value)
        {
            if (!string.IsNullOrEmpty(value))
                fields.Add(new KeyValuePair<string, string>(name, value));
        }

        Add("Format", evt.Format);
        Add("Source", evt.Source);
        if (evt.SizeBytes is long size and > 0) Add("Size", FormatSize(size));
        if (evt.DurationSeconds is int dur and > 0)
        {
            Add("Length", FormatDuration(dur));
            if (evt.SizeBytes is long b and > 0)
                Add("Bitrate", $"≈{b * 8 / dur / 1000:N0} kbps");
        }
        if (evt.Year is int year and > 0) Add("Year", year.ToString());
        // Last, because on a single-user library it is always the same name and the
        // fields above are what anyone is actually reading.
        if (evt.RequestedBy is { Count: > 0 } askers)
            Add("Requested by", string.Join(", ", askers));
        return fields;
    }

    private static List<KeyValuePair<string, string>> AlbumFields(NotificationEvent evt) =>
        new()
        {
            new("Tracks", (evt.TrackCount ?? 0).ToString()),
            new("Lossless", (evt.LosslessCount ?? 0).ToString()),
            new("Failed", (evt.FailedCount ?? 0).ToString()),
        };

    internal static string FormatSize(long bytes) =>
        bytes >= 1024L * 1024 * 1024 ? $"{bytes / (1024.0 * 1024 * 1024):F1} GB"
        : bytes >= 1024L * 1024 ? $"{bytes / (1024.0 * 1024):F1} MB"
        : $"{bytes / 1024.0:F0} KB";

    internal static string FormatDuration(int seconds) =>
        seconds >= 3600
            ? $"{seconds / 3600}:{seconds % 3600 / 60:D2}:{seconds % 60:D2}"
            : $"{seconds / 60}:{seconds % 60:D2}";
}
