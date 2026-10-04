using Microsoft.Extensions.Logging.Abstractions;
using Octo.Models.Settings;
using Octo.Services.Common;
using Octo.Services.Imports;

namespace Octo.Tests;

/// <summary>
/// The trickle: missing songs fetched a few an hour through the heart chain, never ahead of a
/// person's own downloads, waiting out a Soulseek outage, pausable, and carried over a restart.
/// </summary>
public sealed class TrickleTests : IDisposable
{
    private readonly string _dir = Path.Combine(Path.GetTempPath(), "octo-trickle-" + Guid.NewGuid().ToString("N"));

    public TrickleTests() => Directory.CreateDirectory(_dir);

    public void Dispose()
    {
        try { Directory.Delete(_dir, recursive: true); } catch { /* best effort */ }
    }

    private static ImportTrack Song(string key, string title = "Song") =>
        new() { Key = key, Title = $"{title} {key}", Artist = "Artist", Seconds = 200 };

    // ---- The queue -----------------------------------------------------------------------------

    [Fact]
    public void ASongIsQueuedOncePerPerson_HoweverManyListsHoldIt()
    {
        var queue = new TrickleQueue();
        Assert.Equal(2, queue.Add("alice", [Song("a"), Song("b"), Song("a")]));
        Assert.Equal(0, queue.Add("alice", [Song("a")]));
        Assert.Equal(1, queue.Add("bob", [Song("a")]));
        Assert.Equal(3, queue.Snapshot().Count);
    }

    [Fact]
    public void ANotFoundSongIsTriedAgainAfterTwoWeeks_ASkippedOneIsNot()
    {
        var now = new DateTime(2026, 10, 4, 12, 0, 0, DateTimeKind.Utc);
        var queue = new TrickleQueue { Clock = () => now };
        queue.Add("alice", [Song("a"), Song("b")]);
        queue.Update("alice", "a", job => job.State = ImportTrackStates.NotFound);
        Assert.Equal(1, queue.Skip("alice", ["b"]));
        now += TimeSpan.FromDays(13);
        Assert.Equal(0, queue.Add("alice", [Song("a"), Song("b")]));
        now += TimeSpan.FromDays(2);
        Assert.Equal(1, queue.Add("alice", [Song("a"), Song("b")]));
        Assert.Equal(ImportTrackStates.Queued, queue.Get("alice", "a")!.State);
        Assert.Equal(ImportTrackStates.Skipped, queue.Get("alice", "b")!.State);
        Assert.Equal(1, queue.Retry("alice", ["b"]));
    }

    [Fact]
    public void PeopleTakeTurns_AndAPausedPersonIsPassedOver()
    {
        var now = new DateTime(2026, 10, 4, 12, 0, 0, DateTimeKind.Utc);
        var queue = new TrickleQueue { Clock = () => now };
        queue.Add("alice", [Song("a1"), Song("a2"), Song("a3")]);
        now += TimeSpan.FromSeconds(1);
        queue.Add("bob", [Song("b1")]);

        string Next()
        {
            now += TimeSpan.FromMinutes(1);
            var job = queue.TakeNext()!;
            queue.Update(job.Owner, job.Key, j => j.State = ImportTrackStates.Done);
            return job.Key;
        }
        Assert.Equal("a1", Next());
        Assert.Equal("b1", Next());
        Assert.Equal("a2", Next());

        queue.SetPaused("alice", true);
        Assert.Null(queue.TakeNext());
        queue.SetPaused("alice", false);
        Assert.Equal("a3", Next());
    }

    [Fact]
    public void ARestartKeepsTheQueueAndThePause_AndARunningSongGoesAgain()
    {
        var path = Path.Combine(_dir, "imports-trickle.json");
        var queue = new TrickleQueue(path);
        queue.Add("alice", [Song("a"), Song("b")]);
        queue.SetPaused("bob", true);
        Assert.Equal(ImportTrackStates.Downloading, queue.TakeNext()!.State);

        var again = new TrickleQueue(path);
        Assert.All(again.Snapshot(), job => Assert.Equal(ImportTrackStates.Queued, job.State));
        Assert.True(again.IsPaused("bob"));
        Assert.NotNull(again.LastStartUtc);
    }

    [Fact]
    public void CancellingTakesBackOnlyWhatHasNotRun()
    {
        var queue = new TrickleQueue();
        queue.Add("alice", [Song("a"), Song("b"), Song("c")]);
        queue.TakeNext();
        Assert.Equal(1, queue.CancelQueued("alice", ["b"]));
        Assert.Equal(1, queue.CancelQueued("alice"));
        Assert.Single(queue.Snapshot("alice"));
    }

    // ---- The worker ----------------------------------------------------------------------------

    private sealed class Rig
    {
        public DateTime Now = new(2026, 10, 4, 12, 0, 0, DateTimeKind.Utc);
        public readonly TrickleQueue Queue;
        public readonly TestOptionsMonitor<ImportSettings> Settings = TestOptions.Monitor(new ImportSettings { SongsPerHour = 12 });
        public readonly TrickleWorker Worker;
        public readonly Dictionary<string, AcquisitionSnapshot> Rows = new();
        public readonly List<string> Started = [];
        public readonly List<TrickleJob> Finished = [];
        public bool Busy;
        public bool SoulseekOut;
        public Func<TrickleJob, string?> Owned = _ => null;
        public Func<TrickleJob, AcquisitionState?> Outcome = _ => AcquisitionState.Done;

        public Rig()
        {
            Queue = new TrickleQueue { Clock = () => Now };
            Worker = new TrickleWorker(Queue, Settings, NullLogger<TrickleWorker>.Instance)
            {
                Clock = () => Now,
                PeopleDownloading = () => Busy,
                SoulseekOut = _ => Task.FromResult(SoulseekOut),
                Owned = (job, _) => Task.FromResult(Owned(job) is { } id ? new TrickleWorker.OwnedSong(id) : null),
                Row = key => Rows.GetValueOrDefault(key),
            };
            Worker.Start = job =>
            {
                var key = $"soulseek:{job.Key}";
                Queue.Update(job.Owner, job.Key, j => j.AcquisitionKey = key);
                Started.Add(job.Key);
                if (Outcome(job) is { } state) Rows[key] = Row(key, state);
                return Task.FromResult(key);
            };
            Worker.Finished += job => Finished.Add(job);
        }

        public static AcquisitionSnapshot Row(string key, AcquisitionState state) => new(
            key, "soulseek", key, "Artist", "Song", null, ["alice"], "Soulseek", state, null, null, null,
            DateTime.UtcNow, DateTime.UtcNow, state == AcquisitionState.Failed ? "Nobody on Soulseek had it." : null,
            state == AcquisitionState.Done ? $"nd-{key}" : null);

        public async Task<bool> Tick()
        {
            var started = await Worker.TickAsync(CancellationToken.None);
            await Worker.DrainAsync();
            return started;
        }
    }

    [Fact]
    public async Task SongsStartNoCloserThanTheSettingAllows()
    {
        var rig = new Rig();
        rig.Queue.Add("alice", [Song("a"), Song("b")]);
        Assert.True(await rig.Tick());
        Assert.False(await rig.Tick());
        rig.Now += TimeSpan.FromMinutes(4);
        Assert.False(await rig.Tick());
        Assert.Equal(TrickleStates.Running, rig.Worker.StatusFor("alice").State);
        Assert.Equal(rig.Now + TimeSpan.FromMinutes(1), rig.Worker.StatusFor("alice").NextUtc);
        rig.Now += TimeSpan.FromMinutes(1);
        Assert.True(await rig.Tick());
        Assert.Equal(["a", "b"], rig.Started);
    }

    [Fact]
    public async Task AFetchedSongIsDone_WithItsLibraryId()
    {
        var rig = new Rig();
        rig.Queue.Add("alice", [Song("a")]);
        await rig.Tick();
        var job = rig.Queue.Get("alice", "a")!;
        Assert.Equal(ImportTrackStates.Done, job.State);
        Assert.Equal("nd-soulseek:a", job.LibraryId);
        Assert.Equal("Fetched from Soulseek", job.Detail);
        Assert.Single(rig.Finished);
    }

    [Fact]
    public async Task ASongAlreadyInTheLibraryIsNeverDownloaded()
    {
        var rig = new Rig { Owned = _ => "nd-owned" };
        rig.Queue.Add("alice", [Song("a")]);
        await rig.Tick();
        Assert.Empty(rig.Started);
        var job = rig.Queue.Get("alice", "a")!;
        Assert.Equal(ImportTrackStates.Done, job.State);
        Assert.Equal("nd-owned", job.LibraryId);
        Assert.Equal("Already in your library", job.Detail);
    }

    [Fact]
    public async Task ASongNoSourceHadIsNotFound_WithTheReason()
    {
        var rig = new Rig { Outcome = _ => AcquisitionState.Failed };
        rig.Queue.Add("alice", [Song("a")]);
        await rig.Tick();
        var job = rig.Queue.Get("alice", "a")!;
        Assert.Equal(ImportTrackStates.NotFound, job.State);
        Assert.Equal("Nobody on Soulseek had it.", job.Detail);
    }

    [Fact]
    public async Task AHandOffToLidarrIsFollowedWithoutHoldingTheNextSongBack()
    {
        var rig = new Rig { Outcome = job => job.Key == "a" ? AcquisitionState.Downloading : AcquisitionState.Done };
        rig.Queue.Add("alice", [Song("a"), Song("b")]);
        await rig.Tick();
        Assert.Equal(ImportTrackStates.Downloading, rig.Queue.Get("alice", "a")!.State);

        rig.Now += TimeSpan.FromMinutes(5);
        Assert.True(await rig.Tick());
        Assert.Equal(ImportTrackStates.Done, rig.Queue.Get("alice", "b")!.State);
        Assert.Equal(ImportTrackStates.Downloading, rig.Queue.Get("alice", "a")!.State);

        rig.Rows["soulseek:a"] = Rig.Row("soulseek:a", AcquisitionState.Done);
        await rig.Tick();
        Assert.Equal(ImportTrackStates.Done, rig.Queue.Get("alice", "a")!.State);
    }

    [Fact]
    public async Task ItWaitsWhilePeoplesOwnDownloadsRun_AndWhileSoulseekIsOut()
    {
        var rig = new Rig { Busy = true };
        rig.Queue.Add("alice", [Song("a")]);
        Assert.False(await rig.Tick());
        Assert.Equal(TrickleStates.Yielding, rig.Worker.StatusFor("alice").State);

        rig.Busy = false;
        rig.SoulseekOut = true;
        Assert.False(await rig.Tick());
        Assert.Equal(TrickleStates.WaitingForSoulseek, rig.Worker.StatusFor("alice").State);

        rig.SoulseekOut = false;
        Assert.True(await rig.Tick());
    }

    [Fact]
    public async Task NoneAnHourOrAPauseStartsNothing()
    {
        var rig = new Rig();
        rig.Settings.Set(new ImportSettings { SongsPerHour = 0 });
        rig.Queue.Add("alice", [Song("a")]);
        Assert.False(await rig.Tick());
        Assert.Equal(TrickleStates.Off, rig.Worker.StatusFor("alice").State);

        rig.Settings.Set(new ImportSettings { SongsPerHour = 12 });
        rig.Queue.SetPaused("alice", true);
        Assert.False(await rig.Tick());
        Assert.Equal(TrickleStates.Paused, rig.Worker.StatusFor("alice").State);
        Assert.Empty(rig.Started);
    }

    [Fact]
    public async Task ADownloadNobodyReportsOnIsCheckedAgain()
    {
        var rig = new Rig { Outcome = _ => null };
        rig.Queue.Add("alice", [Song("a")]);
        await rig.Tick();
        var job = rig.Queue.Get("alice", "a")!;
        Assert.Equal(ImportTrackStates.Queued, job.State);
        Assert.Equal("Checking again", job.Detail);
    }
}
