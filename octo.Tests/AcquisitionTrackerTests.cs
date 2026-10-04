using System.Text.Json;
using Microsoft.Extensions.Logging;
using Microsoft.Extensions.Logging.Abstractions;
using Moq;
using Octo.Models.Settings;
using Octo.Services;
using Octo.Services.Common;
using Octo.Services.Lidarr;
using Octo.Services.Soulseek;

namespace Octo.Tests;

/// <summary>
/// The live progress list behind the app's download ring. It watches the pipeline and must
/// tell the truth about it: one row per hearted song however many sources it falls through,
/// failed only when the last one gives up, and gone three hours after it ends.
/// </summary>
public class AcquisitionTrackerTests
{
    private sealed class ManualClock : TimeProvider
    {
        public DateTimeOffset Now { get; set; } = new(2026, 9, 26, 18, 0, 0, TimeSpan.Zero);
        public override DateTimeOffset GetUtcNow() => Now;
    }

    private static AcquisitionTracker NewTracker(TimeProvider? time = null) =>
        new(NullLogger<AcquisitionTracker>.Instance, services: null, time);

    private static AcquisitionSnapshot Only(AcquisitionTracker tracker, string user) =>
        Assert.Single(tracker.ForUser(user));

    [Fact]
    public void AStarWalksThroughEveryStageToDone()
    {
        var tracker = NewTracker();

        tracker.Begin("soulseek", "abc", "abc", "alice", "Daft Punk", "Da Funk", "Homework");
        Assert.Equal(AcquisitionState.Queued, Only(tracker, "alice").State);

        tracker.Stage("soulseek", "abc", AcquisitionState.Searching, "Soulseek");
        Assert.Equal(AcquisitionState.Searching, Only(tracker, "alice").State);
        Assert.Equal("Soulseek", Only(tracker, "alice").Source);

        tracker.Transfer("soulseek", "abc", 7_250_000, 29_000_000);
        var downloading = Only(tracker, "alice");
        Assert.Equal(AcquisitionState.Downloading, downloading.State);
        Assert.Equal(0.25, downloading.Progress);
        Assert.Equal(7_250_000, downloading.BytesDone);
        Assert.Equal(29_000_000, downloading.BytesTotal);

        tracker.Stage("soulseek", "abc", AcquisitionState.Verifying);
        var verifying = Only(tracker, "alice");
        Assert.Equal(AcquisitionState.Verifying, verifying.State);
        Assert.Null(verifying.Progress);
        Assert.Equal(29_000_000, verifying.BytesTotal);

        tracker.Stage("soulseek", "abc", AcquisitionState.Importing);
        Assert.Equal(AcquisitionState.Importing, Only(tracker, "alice").State);

        // No lookup is available here, so an imported song is done at once.
        tracker.Imported("soulseek", "abc", "Daft Punk", "Da Funk", "/music/Daft Punk/Da Funk.flac");
        var done = Only(tracker, "alice");
        Assert.Equal(AcquisitionState.Done, done.State);
        Assert.Null(done.Error);
        Assert.Equal("Daft Punk", done.Artist);
        Assert.Equal("Da Funk", done.Title);
        Assert.Equal("Homework", done.Album);
    }

    [Fact]
    public void AFinishedRowIsNeverReopenedByALateStage()
    {
        var tracker = NewTracker();
        tracker.Begin("soulseek", "abc", "abc", "alice");
        tracker.Fail("soulseek", "abc", "No Soulseek FLAC found for 'A - B'");

        // A play of the same song an hour later reports stages too; they belong to no heart.
        tracker.Stage("soulseek", "abc", AcquisitionState.Downloading);
        tracker.Transfer("soulseek", "abc", 1, 2);
        tracker.Imported("soulseek", "abc", "A", "B", "/music/b.flac");

        var row = Only(tracker, "alice");
        Assert.Equal(AcquisitionState.Failed, row.State);
        Assert.Equal("No Soulseek FLAC found for 'A - B'", row.Error);
    }

    [Fact]
    public void AStageForASongNobodyHeartedCreatesNothing()
    {
        var tracker = NewTracker();

        tracker.Stage("soulseek", "played", AcquisitionState.Searching, "Soulseek");
        tracker.Transfer("soulseek", "played", 5, 10);

        Assert.Empty(tracker.All());
    }

    [Fact]
    public void ASecondHeartJoinsTheRunningRowAndAHeartAfterTheEndRestartsIt()
    {
        var tracker = NewTracker();
        tracker.Begin("soulseek", "abc", "abc", "alice");
        tracker.Transfer("soulseek", "abc", 5, 10);

        tracker.Begin("soulseek", "abc", "abc", "bob");
        var joined = Assert.Single(tracker.All());
        Assert.Equal(["alice", "bob"], joined.RequestedBy);
        Assert.Equal(AcquisitionState.Downloading, joined.State);

        tracker.Fail("soulseek", "abc", "gone");
        tracker.Begin("soulseek", "abc", "abc", "carol");
        var restarted = Assert.Single(tracker.All());
        Assert.Equal(AcquisitionState.Queued, restarted.State);
        Assert.Equal(["carol"], restarted.RequestedBy);
        Assert.Null(restarted.Error);
        Assert.Empty(tracker.ForUser("alice"));
    }

    [Fact]
    public void EachUserSeesOnlyTheirOwnRows()
    {
        var tracker = NewTracker();
        tracker.Begin("soulseek", "a1", "a1", "alice");
        tracker.Begin("soulseek", "b1", "b1", "bob");

        Assert.Equal("a1", Only(tracker, "alice").Id);
        Assert.Equal("b1", Only(tracker, "bob").Id);
        Assert.Equal("a1", Only(tracker, "ALICE").Id);
        Assert.Empty(tracker.ForUser("mallory"));
        Assert.Empty(tracker.ForUser(""));
        Assert.Equal(2, tracker.All().Count);
    }

    [Fact]
    public void TheRowCarriesTheIdTheClientStarred()
    {
        var tracker = NewTracker();
        tracker.Begin("soulseek", "12345", "ext-soulseek-song-12345", "alice");

        var row = Only(tracker, "alice");
        Assert.Equal("ext-soulseek-song-12345", row.Id);
        Assert.Equal("12345", row.ExternalId);
    }

    // --- The source chain -----------------------------------------------------------------

    private static (HeartAcquisitionCoordinator Coordinator, TrackAcquisitionQueue Queue) Chain(
        AcquisitionTracker tracker, params HeartDownloadSource[] order)
    {
        var queue = new TrackAcquisitionQueue(new Mock<ILogger<TrackAcquisitionQueue>>().Object);
        var settings = TestOptions.Monitor(new SubsonicSettings
        {
            HeartDownloadSources = order.Select(source =>
                new HeartDownloadStep { Source = source, Enabled = true }).ToList(),
        });
        var lidarr = new Mock<ILidarrHeartAcquisitionService>();
        var coordinator = new HeartAcquisitionCoordinator(settings, queue,
            new Mock<IDownloadService>().Object, lidarr.Object,
            new Mock<ILogger<HeartAcquisitionCoordinator>>().Object, tracker);
        return (coordinator, queue);
    }

    private static async Task<AcquisitionRequest> NextAsync(TrackAcquisitionQueue queue)
    {
        using var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(10));
        var request = await queue.DequeueAsync(timeout.Token);
        Assert.NotNull(request);
        return request;
    }

    [Fact]
    public async Task AFallbackChainStaysOneRowAndFailsOnlyAtTheEnd()
    {
        var tracker = NewTracker();
        var (coordinator, queue) = Chain(tracker, HeartDownloadSource.Soulseek, HeartDownloadSource.YouTube);
        tracker.Begin("soulseek", "abc", "abc", "alice", "Radiohead", "Creep", null);

        var acquisition = coordinator.AcquireTrackAsync("soulseek", "abc", "alice");

        var first = await NextAsync(queue);
        var queued = Only(tracker, "alice");
        Assert.Equal(AcquisitionState.Queued, queued.State);
        Assert.Equal("Soulseek", queued.Source);

        // What the Soulseek path reports before its last peer gives up.
        tracker.Stage("soulseek", "abc", AcquisitionState.Searching, "Soulseek");
        tracker.Transfer("soulseek", "abc", 400, 1000);
        queue.Release(first);
        first.Completion.TrySetException(new InvalidOperationException("All 5 Soulseek peer attempts failed"));

        var second = await NextAsync(queue);
        var fellBack = Only(tracker, "alice");
        Assert.Equal(AcquisitionState.Searching, fellBack.State);
        Assert.Equal("YouTube", fellBack.Source);
        Assert.Equal("Soulseek couldn't get it, trying YouTube", fellBack.Note);
        Assert.Null(fellBack.Error);
        Assert.Null(fellBack.Progress);
        Assert.Null(fellBack.BytesDone);

        queue.Release(second);
        second.Completion.TrySetException(new FileNotFoundException("No YouTube match for 'Radiohead - Creep'"));
        await acquisition;

        var failed = Assert.Single(tracker.All());
        Assert.Equal(AcquisitionState.Failed, failed.State);
        Assert.Equal("No YouTube match for 'Radiohead - Creep'", failed.Error);
        Assert.Equal("YouTube", failed.Source);
    }

    [Fact]
    public async Task AFallbackThatSucceedsNeverShowsTheEarlierFailure()
    {
        var tracker = NewTracker();
        var (coordinator, queue) = Chain(tracker, HeartDownloadSource.Soulseek, HeartDownloadSource.YouTube);
        tracker.Begin("soulseek", "abc", "abc", "alice");

        var acquisition = coordinator.AcquireTrackAsync("soulseek", "abc", "alice");
        var first = await NextAsync(queue);
        queue.Release(first);
        first.Completion.TrySetException(new InvalidOperationException("no peer"));

        var second = await NextAsync(queue);
        tracker.Imported("soulseek", "abc", "A", "B", "/music/b.mp3");
        queue.Release(second);
        second.Completion.TrySetResult("/music/b.mp3");
        await acquisition;

        var row = Assert.Single(tracker.All());
        Assert.Equal(AcquisitionState.Done, row.State);
        Assert.Null(row.Error);
    }

    // --- Albums --------------------------------------------------------------------------

    [Fact]
    public void AnAlbumHeartListsEveryTrackForWhoeverHeartedIt()
    {
        var tracker = NewTracker();
        tracker.BeginAlbum("soulseek", "alb", "bob");

        tracker.Announce("soulseek", "alb", null,
            [("t1", "Air", "La Femme d'Argent", "Moon Safari"), ("t2", "Air", "Sexy Boy", "Moon Safari")]);

        var rows = tracker.ForUser("bob");
        Assert.Equal(2, rows.Count);
        Assert.All(rows, row => Assert.Equal(AcquisitionState.Queued, row.State));
        Assert.All(rows, row => Assert.Equal("Moon Safari", row.Album));
    }

    [Fact]
    public void AWalkNobodyHeartedListsNothing()
    {
        var tracker = NewTracker();

        tracker.Announce("soulseek", "alb", null, [("t1", "A", "B", "C")]);

        Assert.Empty(tracker.All());
    }

    [Fact]
    public void AWalkStartedByAHeartedSongBelongsToItsOwner()
    {
        var tracker = NewTracker();
        tracker.Begin("soulseek", "t0", "t0", "carol");

        tracker.Announce("soulseek", "alb", "t0", [("t1", "A", "B", "C")]);

        Assert.Equal(2, tracker.ForUser("carol").Count);
    }

    [Fact]
    public void FailAlbumClosesOnlyTracksStillRunning()
    {
        var tracker = NewTracker();
        tracker.BeginAlbum("soulseek", "alb", "bob");
        tracker.Announce("soulseek", "alb", null, [("t1", "A", "One", "C"), ("t2", "A", "Two", "C")]);
        tracker.Imported("soulseek", "t1", "A", "One", "/music/one.flac");

        tracker.FailAlbum("soulseek", "alb", "Lidarr import timed out after 30 minute(s).");

        var rows = tracker.ForUser("bob").ToDictionary(row => row.Id);
        Assert.Equal(AcquisitionState.Done, rows["t1"].State);
        Assert.Equal(AcquisitionState.Failed, rows["t2"].State);
        Assert.Equal("Lidarr import timed out after 30 minute(s)", rows["t2"].Error);
    }

    [Fact]
    public void AnnounceRequeuesAFailedTrackButLeavesADoneOneAlone()
    {
        var tracker = NewTracker();
        tracker.BeginAlbum("soulseek", "alb", "bob");
        tracker.Announce("soulseek", "alb", null, [("t1", "A", "One", "C"), ("t2", "A", "Two", "C")]);
        tracker.Complete("soulseek", "t1");
        tracker.Fail("soulseek", "t2", "no peer");

        tracker.BeginAlbum("soulseek", "alb", "bob");
        tracker.Announce("soulseek", "alb", null, [("t1", "A", "One", "C"), ("t2", "A", "Two", "C")]);

        var rows = tracker.ForUser("bob").ToDictionary(row => row.Id);
        Assert.Equal(AcquisitionState.Done, rows["t1"].State);
        Assert.Equal(AcquisitionState.Queued, rows["t2"].State);
    }

    // --- Expiry and the cap --------------------------------------------------------------

    [Fact]
    public void AFinishedRowStaysThreeHoursThenGoes()
    {
        var clock = new ManualClock();
        var tracker = NewTracker(clock);
        tracker.Begin("soulseek", "abc", "abc", "alice");
        tracker.Complete("soulseek", "abc");

        clock.Now += TimeSpan.FromHours(3) - TimeSpan.FromMinutes(1);
        Assert.Single(tracker.ForUser("alice"));

        clock.Now += TimeSpan.FromMinutes(1);
        Assert.Empty(tracker.ForUser("alice"));
    }

    [Fact]
    public void ARunningRowOutlivesThirtyMinutesButNotADayOfSilence()
    {
        var clock = new ManualClock();
        var tracker = NewTracker(clock);
        tracker.Begin("soulseek", "abc", "abc", "alice");

        clock.Now += TimeSpan.FromHours(2);
        Assert.Equal(AcquisitionState.Queued, Only(tracker, "alice").State);

        clock.Now += TimeSpan.FromHours(22);
        Assert.Empty(tracker.All());
    }

    [Fact]
    public void TheCapDropsFinishedRowsFirstThenTheOldest()
    {
        var clock = new ManualClock();
        var tracker = NewTracker(clock);
        tracker.Begin("soulseek", "finished", "finished", "alice");
        tracker.Complete("soulseek", "finished");

        for (var i = 0; i < AcquisitionTracker.Capacity; i++)
        {
            clock.Now += TimeSpan.FromSeconds(1);
            tracker.Begin("soulseek", $"run{i}", $"run{i}", "alice");
        }

        var ids = tracker.All().Select(row => row.Id).ToHashSet();
        Assert.Equal(AcquisitionTracker.Capacity, ids.Count);
        Assert.DoesNotContain("finished", ids);
        Assert.Contains("run0", ids);

        clock.Now += TimeSpan.FromSeconds(1);
        tracker.Begin("soulseek", "newest", "newest", "alice");

        ids = tracker.All().Select(row => row.Id).ToHashSet();
        Assert.Equal(AcquisitionTracker.Capacity, ids.Count);
        Assert.DoesNotContain("run0", ids);
        Assert.Contains("newest", ids);
    }

    // --- Seeing it in Navidrome ------------------------------------------------------------

    [Fact]
    public async Task ImportedWaitsForNavidromeAndRecordsTheLibraryId()
    {
        var tracker = NewTracker();
        var calls = 0;
        tracker.VisibilityPoll = TimeSpan.FromMilliseconds(10);
        tracker.LibraryLookup = (_, _, _, _) => Task.FromResult(++calls < 3 ? null : "nd-song-1");
        tracker.Begin("soulseek", "abc", "abc", "alice");

        tracker.Imported("soulseek", "abc", "A", "B", "/music/b.flac");
        Assert.Equal(AcquisitionState.Importing, Only(tracker, "alice").State);

        var row = await WaitForAsync(tracker, "alice", AcquisitionState.Done);
        Assert.Equal("nd-song-1", row.LibraryId);
    }

    [Fact]
    public async Task ImportedIsDoneWithoutAnIdWhenNavidromeNeverShowsIt()
    {
        var tracker = NewTracker();
        tracker.VisibilityPoll = TimeSpan.FromMilliseconds(5);
        tracker.VisibilityAttempts = 3;
        tracker.SlowVisibilityAttempts = 0;
        tracker.LibraryLookup = (_, _, _, _) => throw new HttpRequestException("navidrome down");
        tracker.Begin("soulseek", "abc", "abc", "alice");

        tracker.Imported("soulseek", "abc", "A", "B", "/music/b.flac");

        var row = await WaitForAsync(tracker, "alice", AcquisitionState.Done);
        Assert.Null(row.LibraryId);
    }

    [Fact]
    public async Task ASongStillMissingAfterAMinuteAsksNavidromeToScanOnceAndIsFoundAfterIt()
    {
        var tracker = NewTracker();
        var scans = 0;
        var calls = 0;
        tracker.VisibilityPoll = TimeSpan.FromMilliseconds(2);
        tracker.SlowVisibilityPoll = TimeSpan.FromMilliseconds(2);
        tracker.VisibilityAttempts = 4;
        tracker.SlowVisibilityAttempts = 10;
        tracker.RescanAfterAttempts = 6;
        tracker.Rescan = () => { Interlocked.Increment(ref scans); return Task.CompletedTask; };
        // Navidrome shows the song only once the forced scan has run.
        tracker.LibraryLookup = (_, _, _, _) =>
        {
            Interlocked.Increment(ref calls);
            return Task.FromResult(Volatile.Read(ref scans) > 0 ? "nd-song-9" : null);
        };
        tracker.Begin("soulseek", "abc", "abc", "alice");

        tracker.Imported("soulseek", "abc", "A", "B", "/music/b.flac");

        var row = await WaitForAsync(tracker, "alice", AcquisitionState.Done);
        Assert.Equal("nd-song-9", row.LibraryId);
        Assert.Equal(1, scans);
        Assert.Equal(7, calls);
    }

    [Fact]
    public async Task TheWatchOutlastsTheFastPollsBeforeGivingUp()
    {
        var tracker = NewTracker();
        var calls = 0;
        tracker.VisibilityPoll = TimeSpan.FromMilliseconds(2);
        tracker.SlowVisibilityPoll = TimeSpan.FromMilliseconds(2);
        tracker.VisibilityAttempts = 3;
        tracker.SlowVisibilityAttempts = 5;
        tracker.LibraryLookup = (_, _, _, _) => { Interlocked.Increment(ref calls); return Task.FromResult<string?>(null); };
        tracker.Begin("soulseek", "abc", "abc", "alice");

        tracker.Imported("soulseek", "abc", "A", "B", "/music/b.flac");

        var row = await WaitForAsync(tracker, "alice", AcquisitionState.Done);
        Assert.Null(row.LibraryId);
        Assert.Equal(8, calls);
    }

    // --- Words beside the ring -------------------------------------------------------------

    [Fact]
    public void ATransferThatHasNotMovedAByteHasNoProgressRatherThanZero()
    {
        var tracker = NewTracker();
        tracker.Begin("soulseek", "abc", "abc", "alice");

        tracker.Transfer("soulseek", "abc", 0, 29_000_000, 0, "Soulseek");
        var connecting = Only(tracker, "alice");
        Assert.Equal(AcquisitionState.Downloading, connecting.State);
        Assert.Null(connecting.Progress);
        Assert.Equal(29_000_000, connecting.BytesTotal);

        tracker.Transfer("soulseek", "abc", 2_900_000, 29_000_000, 10, "Soulseek");
        Assert.Equal(0.1, Only(tracker, "alice").Progress);
    }

    [Fact]
    public void AQueuedSongCountsTheDownloadsAheadOfItWhoeverAskedForThem()
    {
        var tracker = NewTracker();
        tracker.Begin("soulseek", "first", "first", "bob");
        tracker.Announce("soulseek", null, "first",
            [("t1", "A", "One", "Album"), ("t2", "A", "Two", "Album")]);
        tracker.Begin("soulseek", "mine", "mine", "alice");

        var mine = Only(tracker, "alice");
        Assert.Equal(3, mine.Ahead);

        // Running and finished rows have no place in the queue, and finished ones free a slot.
        tracker.Transfer("soulseek", "first", 1, 2);
        tracker.Fail("soulseek", "t1", "gone");
        Assert.Null(Assert.Single(tracker.ForUser("bob"), row => row.Id == "first").Ahead);
        Assert.Equal(2, Only(tracker, "alice").Ahead);
        Assert.Equal(1, Assert.Single(tracker.ForUser("bob"), row => row.Id == "t2").Ahead);
    }

    [Fact]
    public void ANoteStaysUntilAnotherReplacesItAndGoesWhenTheSongArrives()
    {
        var tracker = NewTracker();
        tracker.Begin("soulseek", "abc", "abc", "alice");

        tracker.Stage("soulseek", "abc", AcquisitionState.Searching, "YouTube", "Soulseek couldn't get it, trying YouTube");
        tracker.Stage("soulseek", "abc", AcquisitionState.Searching);
        Assert.Equal("Soulseek couldn't get it, trying YouTube", Only(tracker, "alice").Note);

        tracker.Complete("soulseek", "abc", "nd-1");
        Assert.Null(Only(tracker, "alice").Note);
    }

    private static async Task<AcquisitionSnapshot> WaitForAsync(AcquisitionTracker tracker, string user,
        AcquisitionState state)
    {
        var deadline = DateTime.UtcNow.AddSeconds(10);
        while (DateTime.UtcNow < deadline)
        {
            var row = tracker.ForUser(user).SingleOrDefault();
            if (row?.State == state) return row;
            await Task.Delay(10);
        }
        throw new TimeoutException($"never reached {state}");
    }

    // --- Progress from slskd ------------------------------------------------------------

    [Theory]
    [InlineData(7_250_000L, 29_000_000L, null, 0.25)]
    [InlineData(12_180_000L, 29_000_000L, 99.0, 0.42)]
    [InlineData(null, null, 42.0, 0.42)]
    [InlineData(null, 29_000_000L, 42.0, 0.42)]
    [InlineData(30_000_000L, 29_000_000L, null, 1.0)]
    [InlineData(0L, 29_000_000L, null, 0.0)]
    public void ProgressComesFromTheBytesThenThePercentage(long? done, long? total, double? percent,
        double expected)
    {
        Assert.Equal(expected, AcquisitionTracker.FractionOf(done, total, percent));
    }

    [Fact]
    public void NoFiguresMeansNoProgress()
    {
        Assert.Null(AcquisitionTracker.FractionOf(null, null, null));
        Assert.Null(AcquisitionTracker.FractionOf(100, 0, null));
    }

    // The per-user transfer shape slskd 0.26 returns, with a transfer part way through.
    private const string InProgressTransfer = """
        {
          "username": "blixquoy",
          "directories": [
            {
              "directory": "music\\Daft Punk\\1997 - Homework [CD]",
              "files": [
                {
                  "filename": "music\\Daft Punk\\1997 - Homework [CD]\\01 - Daftendirekt.flac",
                  "state": "InProgress",
                  "size": 29000000,
                  "bytesTransferred": 12180000,
                  "percentComplete": 42.0
                }
              ]
            }
          ]
        }
        """;

    private const string Filename = @"music\Daft Punk\1997 - Homework [CD]\01 - Daftendirekt.flac";

    [Fact]
    public void ATransferPollBecomesTheRowsProgress()
    {
        using var doc = JsonDocument.Parse(InProgressTransfer);
        var file = SoulseekClient.FindTransfer(doc.RootElement, Filename);
        Assert.NotNull(file);
        var progress = SoulseekClient.ReadTransferProgress(file.Value);
        Assert.Equal(12_180_000, progress.BytesTransferred);
        Assert.Equal(29_000_000, progress.Size);
        Assert.Equal(42.0, progress.PercentComplete);
        Assert.True(progress.IsMoving);

        var tracker = NewTracker();
        tracker.Begin("soulseek", "abc", "abc", "alice");
        tracker.Transfer("soulseek", "abc", progress.BytesTransferred, progress.Size, progress.PercentComplete, "Soulseek");

        var row = Only(tracker, "alice");
        Assert.Equal(AcquisitionState.Downloading, row.State);
        Assert.Equal(0.42, row.Progress);
        Assert.Equal(12_180_000, row.BytesDone);
        Assert.Equal(29_000_000, row.BytesTotal);
    }

    [Fact]
    public void ATransferWaitingInThePeersQueueIsNotMoving()
    {
        var json = InProgressTransfer
            .Replace("\"InProgress\"", "\"Queued, Remotely\"")
            .Replace("12180000", "0")
            .Replace("42.0", "0");
        using var doc = JsonDocument.Parse(json);

        var progress = SoulseekClient.ReadTransferProgress(SoulseekClient.FindTransfer(doc.RootElement, Filename)!.Value);

        Assert.False(progress.IsMoving);
        Assert.Equal("Queued, Remotely", SoulseekClient.FindTransferState(doc.RootElement, Filename));
    }

    [Fact]
    public void MissingFiguresReadAsUnknownNotZero()
    {
        const string bare = """{"username":"u","directories":[{"files":[{"filename":"f.flac","state":"InProgress","size":"big"}]}]}""";
        using var doc = JsonDocument.Parse(bare);

        var progress = SoulseekClient.ReadTransferProgress(SoulseekClient.FindTransfer(doc.RootElement, "f.flac")!.Value);

        Assert.Null(progress.BytesTransferred);
        Assert.Null(progress.Size);
        Assert.Null(progress.PercentComplete);
    }

    // --- What a phone is told when it fails -------------------------------------------------

    [Theory]
    [InlineData(
        "All 5 Soulseek peer attempts failed for 'The Killers - Mr. Brightside'. Last error: timed out. If slskd shows these transfers as Completed, slskd's downloads directory is not the directory Octo watches (/music); set SLSKD_DOWNLOADS_DIR=/music.",
        "All 5 Soulseek peer attempts failed for 'The Killers - Mr. Brightside'")]
    [InlineData("Could not open /music/.octo-incoming/abc.mp3 for reading", "Could not open a path for reading")]
    [InlineData(@"Access to C:\Music\x.flac is denied", "Access to a path is denied")]
    [InlineData("No Soulseek FLAC found for 'AC/DC - Thunderstruck'", "No Soulseek FLAC found for 'AC/DC - Thunderstruck'")]
    [InlineData("Song not found", "Song not found")]
    public void ErrorsAreShortAndKeepServerPathsHome(string message, string expected)
    {
        Assert.Equal(expected, AcquisitionTracker.UserSafe(message));
    }

    [Fact]
    public void AnOverlongErrorIsCut()
    {
        var safe = AcquisitionTracker.UserSafe(new string('x', 400));
        Assert.NotNull(safe);
        Assert.True(safe.Length <= 160);
        Assert.EndsWith("...", safe);
    }
}
