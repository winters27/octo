using System.Text.Json;
using Microsoft.Extensions.Logging;
using Moq;
using Octo.Models.Download;
using Octo.Services.Common;

namespace Octo.Tests;

/// <summary>
/// Attribution has to survive the one thing that makes it interesting: two people wanting the
/// same track. The queue deduplicates by track, so the second star joins a transfer that is
/// already running rather than starting another, and anything that reads the requester at
/// enqueue time would credit whoever happened to win that race and drop the rest.
/// </summary>
public class DownloadAttributionTests
{
    private static TrackAcquisitionQueue NewQueue() =>
        new(new Mock<ILogger<TrackAcquisitionQueue>>().Object);

    /// <summary>Takes the request the worker would take next. Never blocks: every test here
    /// queues before it reads.</summary>
    private static AcquisitionRequest Dequeue(TrackAcquisitionQueue queue)
    {
        var task = queue.DequeueAsync(CancellationToken.None);
        Assert.True(task.IsCompleted, "nothing was queued");
        return task.Result!;
    }

    [Fact]
    public void Enqueue_RecordsTheUserWhoAsked()
    {
        var queue = NewQueue();
        _ = queue.Enqueue("deezer", "1", isStar: true, triggerAlbumDownload: false,
            forcePermanent: true, requestedBy: "alice");

        Assert.Equal(["alice"], Dequeue(queue).RequestedBy);
    }

    /// <summary>
    /// The case the original proposal did not cover. Both names, one transfer.
    /// </summary>
    [Fact]
    public void Enqueue_SecondUserJoiningAnInFlightRequest_IsAlsoRecorded()
    {
        var queue = NewQueue();
        var first = queue.Enqueue("deezer", "1", isStar: true, triggerAlbumDownload: false,
            forcePermanent: true, requestedBy: "alice");
        var second = queue.Enqueue("deezer", "1", isStar: true, triggerAlbumDownload: false,
            forcePermanent: true, requestedBy: "bob");

        // Joined, not queued twice.
        Assert.Same(first, second);

        Assert.Equal(["alice", "bob"], Dequeue(queue).RequestedBy);
    }

    /// <summary>
    /// A user can join after the worker has taken the request off the queue but before the
    /// transfer finishes, which is most of the window. Reading the set when the file is
    /// recorded rather than when it was queued is what makes that work.
    /// </summary>
    [Fact]
    public void Enqueue_AJoinAfterDequeue_StillLandsOnTheSameRequest()
    {
        var queue = NewQueue();
        _ = queue.Enqueue("deezer", "1", isStar: true, triggerAlbumDownload: false,
            forcePermanent: true, requestedBy: "alice");
        var request = Dequeue(queue);

        _ = queue.Enqueue("deezer", "1", isStar: true, triggerAlbumDownload: false,
            forcePermanent: true, requestedBy: "bob");

        Assert.Equal(["alice", "bob"], request.RequestedBy);
    }

    [Fact]
    public void Enqueue_TheSameUserTwice_IsRecordedOnce()
    {
        var queue = NewQueue();
        _ = queue.Enqueue("deezer", "1", isStar: true, triggerAlbumDownload: false,
            forcePermanent: true, requestedBy: "Alice");
        _ = queue.Enqueue("deezer", "1", isStar: true, triggerAlbumDownload: false,
            forcePermanent: true, requestedBy: "alice");

        Assert.Equal(["Alice"], Dequeue(queue).RequestedBy);
    }

    /// <summary>
    /// Octo starts acquisitions of its own, and the setting can be off. Either way nothing is
    /// captured, and the history entry has to leave the field out rather than write an empty
    /// list that reads as "requested by nobody in particular".
    /// </summary>
    [Theory]
    [InlineData(null)]
    [InlineData("")]
    [InlineData("   ")]
    public void Enqueue_WithNoUser_RecordsNothing(string? username)
    {
        var queue = NewQueue();
        _ = queue.Enqueue("deezer", "1", isStar: false, triggerAlbumDownload: false,
            forcePermanent: true, requestedBy: username);

        Assert.Empty(Dequeue(queue).RequestedBy);
    }

    /// <summary>
    /// A separate track keeps its own requesters; the dedup key is the track, not the user.
    /// </summary>
    [Fact]
    public void Enqueue_DifferentTracks_DoNotShareRequesters()
    {
        var queue = NewQueue();
        _ = queue.Enqueue("deezer", "1", isStar: true, triggerAlbumDownload: false,
            forcePermanent: true, requestedBy: "alice");
        _ = queue.Enqueue("deezer", "2", isStar: true, triggerAlbumDownload: false,
            forcePermanent: true, requestedBy: "bob");

        var requests = new[] { Dequeue(queue), Dequeue(queue) }
            .ToDictionary(r => r.ExternalId, r => r.RequestedBy);

        Assert.Equal(["alice"], requests["1"]);
        Assert.Equal(["bob"], requests["2"]);
    }

    /// <summary>
    /// Every entry written before this field existed has to keep loading, which is the whole
    /// reason it is nullable rather than an empty list.
    /// </summary>
    [Fact]
    public void HistoryEntry_WrittenBeforeAttributionExisted_StillLoads()
    {
        var entry = JsonSerializer.Deserialize<DownloadHistoryEntry>("""
        {"Artist":"Portishead","Title":"Glory Box","Path":"/music/a.flac",
         "Format":"FLAC","Source":"Soulseek","SizeBytes":1,"DownloadedAt":"2026-09-01T00:00:00Z"}
        """);

        Assert.NotNull(entry);
        Assert.Equal("Portishead", entry.Artist);
        Assert.Null(entry.RequestedBy);
    }

    [Fact]
    public void HistoryEntry_RoundTripsItsRequesters()
    {
        var json = JsonSerializer.Serialize(new DownloadHistoryEntry
        {
            Artist = "Portishead",
            Title = "Glory Box",
            RequestedBy = ["alice", "bob"],
        });

        Assert.Equal(["alice", "bob"],
            JsonSerializer.Deserialize<DownloadHistoryEntry>(json)!.RequestedBy);
    }
}
