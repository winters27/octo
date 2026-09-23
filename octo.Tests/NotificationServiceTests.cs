using Microsoft.Extensions.Logging.Abstractions;
using Microsoft.Extensions.Options;
using Octo.Models.Settings;
using Octo.Services.Notifications;

namespace Octo.Tests;

/// <summary>
/// The orchestrator's one unforgivable failure would be disturbing a download, so
/// the properties pinned here are the quiet ones: a disabled event must be dropped,
/// an unconfigured sink must never be called, one transport being down must not
/// take the other with it, and nothing may ever escape as an exception. Rendering
/// is pinned too, because both transports carry the same text and the whole point
/// of the Started event is naming the format up front.
/// </summary>
public class NotificationServiceTests
{
    private sealed class RecordingSink : INotificationSink
    {
        public List<NotificationMessage> Sent { get; } = new();
        public bool Configured { get; set; } = true;
        public bool Throw { get; set; }

        public string Name => "recording";
        public bool IsConfigured => Configured;

        public Task SendAsync(NotificationMessage message, CancellationToken ct)
        {
            if (Throw) throw new InvalidOperationException("sink down");
            Sent.Add(message);
            return Task.CompletedTask;
        }
    }

    private sealed class StaticMonitor : IOptionsMonitor<NotificationSettings>
    {
        public StaticMonitor(NotificationSettings value) => CurrentValue = value;
        public NotificationSettings CurrentValue { get; }
        public NotificationSettings Get(string? name) => CurrentValue;
        public IDisposable? OnChange(Action<NotificationSettings, string?> listener) => null;
    }

    private static NotificationService Build(NotificationSettings settings, params INotificationSink[] sinks) =>
        new(sinks, new StaticMonitor(settings), NullLogger<NotificationService>.Instance);

    private static NotificationEvent Completed() => new()
    {
        Type = NotificationEventType.DownloadCompleted,
        Artist = "Randy Rogers Band",
        Title = "In My Arms Instead",
        Format = "FLAC",
        Source = "Soulseek",
        SizeBytes = 34_684_600,
    };

    [Fact]
    public async Task DisabledEventTypesAreDropped()
    {
        var sink = new RecordingSink();
        var svc = Build(new NotificationSettings { NotifyDownloadCompleted = false }, sink);

        await svc.NotifyInternalAsync(Completed());

        Assert.Empty(sink.Sent);
    }

    [Fact]
    public async Task EventsFanOutOnlyToConfiguredSinks()
    {
        var on = new RecordingSink();
        var off = new RecordingSink { Configured = false };
        var svc = Build(new NotificationSettings(), on, off);

        await svc.NotifyInternalAsync(Completed());

        Assert.Single(on.Sent);
        Assert.Empty(off.Sent);
    }

    [Fact]
    public async Task OneSinkFailingDoesNotStopTheOther()
    {
        var broken = new RecordingSink { Throw = true };
        var healthy = new RecordingSink();
        var svc = Build(new NotificationSettings(), broken, healthy);

        await svc.NotifyInternalAsync(Completed());

        Assert.Single(healthy.Sent);
    }

    [Fact]
    public async Task NotifyNeverThrows()
    {
        var broken = new RecordingSink { Throw = true };
        var svc = Build(new NotificationSettings(), broken);

        // A throwing sink plus an event with every optional field missing: the
        // combination most likely to surface a swallowed NullReferenceException.
        await svc.NotifyInternalAsync(new NotificationEvent { Type = NotificationEventType.DownloadFailed });
        svc.Notify(new NotificationEvent { Type = NotificationEventType.DownloadCompleted });
    }

    [Fact]
    public void StartedMessageNamesTheFormatUpFront()
    {
        var msg = NotificationService.Render(new NotificationEvent
        {
            Type = NotificationEventType.DownloadStarted,
            Artist = "Randy Rogers Band",
            Title = "In My Arms Instead",
            Format = "FLAC",
            Source = "Soulseek",
            SizeBytes = 34_684_600,
        });

        // "Did it find lossless or is it settling?" is the event's entire reason
        // to exist, so the answer leads the body.
        Assert.Contains("FLAC via Soulseek", msg.Body);
        Assert.Contains("33.1 MB", msg.Body);
        Assert.StartsWith("Downloading:", msg.Title);
    }

    [Fact]
    public void FallbackMessageSaysWhatWasLost()
    {
        var msg = NotificationService.Render(new NotificationEvent
        {
            Type = NotificationEventType.LosslessFallback,
            Artist = "A",
            Title = "B",
            Detail = "No Soulseek FLAC found",
        });

        Assert.Contains("MP3", msg.Body);
        Assert.Contains("No Soulseek FLAC found", msg.Body);
    }

    [Fact]
    public void TestEventBypassesEveryToggle()
    {
        var everythingOff = new NotificationSettings
        {
            NotifyDownloadStarted = false,
            NotifyDownloadCompleted = false,
            NotifyLosslessFallback = false,
            NotifyDownloadFailed = false,
            NotifyAlbumCompleted = false,
        };

        // The test button exists to verify transports; toggles must not mute it.
        Assert.True(NotificationService.IsEnabled(everythingOff, NotificationEventType.Test));
    }

    [Fact]
    public async Task TestSendReportsEachSinkSeparately()
    {
        var healthy = new RecordingSink();
        var broken = new RecordingSink { Throw = true };
        var svc = Build(new NotificationSettings(), healthy, broken);

        var results = await svc.SendTestAsync(CancellationToken.None);

        Assert.Equal(2, results.Count);
        Assert.Contains(results, r => r.Ok);
        Assert.Contains(results, r => !r.Ok && r.Detail.Contains("sink down"));
    }

    [Fact]
    public void AlbumSummaryCountsTracksAndLosslessness()
    {
        var msg = NotificationService.Render(new NotificationEvent
        {
            Type = NotificationEventType.AlbumCompleted,
            Artist = "Tame Impala",
            Title = "Currents",
            TrackCount = 15,
            LosslessCount = 12,
            FailedCount = 1,
        });

        Assert.Contains("15 tracks fetched, 12 lossless, 1 failed", msg.Body);
    }

    /// <summary>Every track failing used to arrive titled "Album complete".</summary>
    [Fact]
    public void AlbumSummaryWithNothingFetched_SaysFailed()
    {
        var msg = NotificationService.Render(new NotificationEvent
        {
            Type = NotificationEventType.AlbumCompleted,
            Artist = "Tame Impala",
            Title = "Currents",
            TrackCount = 0,
            LosslessCount = 0,
            FailedCount = 12,
        });

        Assert.StartsWith("Album failed:", msg.Title);
    }

    /// <summary>The heart chain has one to three sources, so "Both" was wrong for most setups.</summary>
    [Fact]
    public void FailureWithoutDetail_DoesNotAssumeTwoSources()
    {
        var msg = NotificationService.Render(new NotificationEvent
        {
            Type = NotificationEventType.DownloadFailed,
            Artist = "A",
            Title = "B",
        });

        Assert.DoesNotContain("Both", msg.Body);
    }

    /// <summary>
    /// A hearted album whose track list never loaded used to count as a successful walk of
    /// nothing: no download, no notification, and the source chain stopped there.
    /// </summary>
    [Fact]
    public void AlbumWalkRefusal_NamesWhyNothingCanBeDownloaded()
    {
        var empty = new Octo.Models.Domain.Album { Artist = "A", Title = "B" };
        var unkeyed = new Octo.Models.Domain.Album { Artist = "A", Title = "B", Songs = [new() { Title = "t" }] };
        var walkable = new Octo.Models.Domain.Album { Artist = "A", Title = "B", Songs = [new() { Title = "t", ExternalId = "x" }] };

        Assert.NotNull(Octo.Services.Common.BaseDownloadService.AlbumWalkRefusal(null));
        Assert.Contains("No track list", Octo.Services.Common.BaseDownloadService.AlbumWalkRefusal(empty));
        Assert.NotNull(Octo.Services.Common.BaseDownloadService.AlbumWalkRefusal(unkeyed));
        Assert.Null(Octo.Services.Common.BaseDownloadService.AlbumWalkRefusal(walkable));
    }

    [Fact]
    public void AlbumSummaryIsSkippedWhenTheWalkDidNothing()
    {
        // A re-star whose tracks are all already present must not ping the phone.
        var album = new Octo.Models.Domain.Album { Artist = "A", Title = "B" };

        Assert.Null(Octo.Services.Common.BaseDownloadService.BuildAlbumSummary(album, 0, 0, 0));
        Assert.NotNull(Octo.Services.Common.BaseDownloadService.BuildAlbumSummary(album, 1, 1, 0));
    }
}
