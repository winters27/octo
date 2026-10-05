using Octo.Controllers;
using Octo.Services.Common;
using Octo.Services.CoverArt;
using Octo.Services.Library;
using Octo.Services.Lyrics;
using Octo.Services.Metadata;
using Octo.Services.Soulseek;

namespace Octo.Tests;

/// <summary>
/// The dashboard's progress toasts read every job through one shape. Each job's running, just
/// finished and long finished states, and the gaps a page used to miss (a run accepted but not yet
/// Running, a rescan slskd has not started yet).
/// </summary>
public class ActivityTests
{
    private static readonly DateTime Now = new(2026, 10, 5, 12, 0, 0, DateTimeKind.Utc);

    [Fact]
    public void Genre_AcceptedButNotRunning_IsRunning()
    {
        var old = new GenreBackfillRun { RunId = "old", Status = GenreBackfillStatus.Completed, FinishedUtc = Now.AddHours(-3) };
        var item = ActivityController.Genre(old, pending: true, Now)!;
        Assert.Equal("running", item.State);
        Assert.Equal("tags", item.Page);
        Assert.Equal("Reading your library", item.Detail);
    }

    [Fact]
    public void Genre_Running_CountsSongs()
    {
        var run = new GenreBackfillRun { RunId = "r1", Status = GenreBackfillStatus.Running, DryRun = false, Processed = 120, Total = 840, Changed = 7 };
        var item = ActivityController.Genre(run, pending: false, Now)!;
        Assert.Equal(("genre:r1", "running", 120, 840, "songs"), (item.Id, item.State, item.Done, item.Total, item.Unit));
        Assert.Equal("Re-tagging genres", item.Title);
        Assert.Equal("7 changed so far", item.Detail);
    }

    [Fact]
    public void Genre_JustFinished_SaysHowItEnded_LongFinished_IsGone()
    {
        var done = new GenreBackfillRun { RunId = "r1", Status = GenreBackfillStatus.Completed, DryRun = true, Changed = 42, FinishedUtc = Now.AddSeconds(-20) };
        var item = ActivityController.Genre(done, false, Now)!;
        Assert.Equal(("done", "42 songs would change"), (item.State, item.Detail));

        var failed = new GenreBackfillRun { RunId = "r2", Status = GenreBackfillStatus.Failed, Reason = "The music folder is read-only.", FinishedUtc = Now.AddSeconds(-5) };
        Assert.Equal(("failed", "The music folder is read-only."), (ActivityController.Genre(failed, false, Now)!.State, ActivityController.Genre(failed, false, Now)!.Error));

        var cancelled = new GenreBackfillRun { RunId = "r3", Status = GenreBackfillStatus.Cancelled, FinishedUtc = Now.AddSeconds(-5) };
        Assert.Equal("stopped", ActivityController.Genre(cancelled, false, Now)!.State);

        var stale = new GenreBackfillRun { RunId = "r4", Status = GenreBackfillStatus.Completed, FinishedUtc = Now - ActivityController.RecentWindow - TimeSpan.FromSeconds(1) };
        Assert.Null(ActivityController.Genre(stale, false, Now));
        Assert.Null(ActivityController.Genre(new GenreBackfillRun(), false, Now));
    }

    [Fact]
    public void Covers_CountsAlbumsWhenLookingUp_FoldersWhenScanning()
    {
        var scan = new CoverUpgradeRun { RunId = "c1", Status = CoverUpgradeStatus.Running, Mode = CoverUpgradeMode.Scan, Processed = 30, Total = 400 };
        var s = ActivityController.Covers(scan, busy: true, Now)!;
        Assert.Equal(("Looking for soft covers", 30, 400, "folders"), (s.Title, s.Done, s.Total, s.Unit));

        var apply = new CoverUpgradeRun { RunId = "c2", Status = CoverUpgradeStatus.Running, Mode = CoverUpgradeMode.Apply, AlbumsDone = 3, AlbumsTotal = 12 };
        var a = ActivityController.Covers(apply, busy: true, Now)!;
        Assert.Equal(("Replacing covers", 3, 12, "albums"), (a.Title, a.Done, a.Total, a.Unit));

        var finished = new CoverUpgradeRun { RunId = "c3", Status = CoverUpgradeStatus.Completed, Mode = CoverUpgradeMode.Scan, Soft = 0, FinishedUtc = Now.AddSeconds(-3) };
        Assert.Equal("Every cover is sharp", ActivityController.Covers(finished, false, Now)!.Detail);

        var starting = new CoverUpgradeRun { Status = CoverUpgradeStatus.Completed, FinishedUtc = Now.AddDays(-1) };
        Assert.Equal(("covers:pending", "running"), (ActivityController.Covers(starting, busy: true, Now)!.Id, ActivityController.Covers(starting, busy: true, Now)!.State));
    }

    [Fact]
    public void Lyrics_RunningAndFinished()
    {
        var run = new LyricsLibraryRun { RunId = "l1", Status = LyricsLibraryStatus.Running, Mode = LyricsLibraryMode.Save, Processed = 5, Total = 50, Written = 4 };
        var item = ActivityController.Lyrics(run, true, Now)!;
        Assert.Equal(("Saving lyrics", "running", "4 written so far"), (item.Title, item.State, item.Detail));

        var done = new LyricsLibraryRun { RunId = "l1", Status = LyricsLibraryStatus.Completed, Mode = LyricsLibraryMode.Save, Written = 41, FinishedUtc = Now.AddSeconds(-1) };
        Assert.Equal("41 songs got lyrics", ActivityController.Lyrics(done, false, Now)!.Detail);
    }

    [Fact]
    public void Upgrades_OneItemForTheQueue()
    {
        var jobs = new[]
        {
            new UpgradeJob { Title = "505", Artist = "Arctic Monkeys", State = UpgradeStates.Working, QueuedUtc = Now.AddMinutes(-2), StartedUtc = Now.AddMinutes(-1), UpdatedUtc = Now },
            new UpgradeJob { Title = "Fluorescent Adolescent", Artist = "Arctic Monkeys", State = UpgradeStates.Queued, QueuedUtc = Now.AddMinutes(-2), UpdatedUtc = Now },
        };
        var item = ActivityController.Upgrades(jobs, Now)!;
        Assert.Equal(("upgrades", "running"), (item.Id, item.State));
        Assert.Equal("Arctic Monkeys · 505, 1 more waiting", item.Detail);

        var ended = new[]
        {
            new UpgradeJob { State = UpgradeStates.Upgraded, UpdatedUtc = Now.AddSeconds(-10) },
            new UpgradeJob { State = UpgradeStates.Failed, UpdatedUtc = Now.AddSeconds(-5) },
        };
        var finished = ActivityController.Upgrades(ended, Now)!;
        Assert.Equal(("done", "1 upgraded, 1 failed"), (finished.State, finished.Detail));
        Assert.Null(ActivityController.Upgrades([], Now));
    }

    private static AcquisitionSnapshot A(string title, AcquisitionState state, double? progress = null, DateTime? updated = null, string? error = null) =>
        new($"id-{title}", "deezer", title, "Frank Ocean", title, "Blonde", [], "Soulseek", state, progress, null, null,
            Now.AddMinutes(-1), updated ?? Now, error, null);

    [Fact]
    public void Downloads_OneItemWhileMoving_ThenWhatLanded()
    {
        var moving = ActivityController.Downloads([A("Nights", AcquisitionState.Queued), A("Pink + White", AcquisitionState.Downloading, 0.64)], Now)!;
        Assert.Equal(("downloads", "Fetching 2 songs", 0.64), (moving.Id, moving.Title, moving.Fraction));
        Assert.Equal("Frank Ocean · Pink + White · downloading", moving.Detail);

        var one = ActivityController.Downloads([A("Pink + White", AcquisitionState.Done, updated: Now.AddSeconds(-4))], Now)!;
        Assert.Equal(("done", "Added Pink + White", "Frank Ocean"), (one.State, one.Title, one.Detail));

        var failed = ActivityController.Downloads([A("Nights", AcquisitionState.Failed, updated: Now.AddSeconds(-4), error: "No copy that fits was found.")], Now)!;
        Assert.Equal(("failed", "No copy that fits was found."), (failed.State, failed.Error));

        Assert.Null(ActivityController.Downloads([A("Old", AcquisitionState.Done, updated: Now.AddHours(-1))], Now));
    }

    private static SharingReport Report(bool scanning, bool failed, double? progress = null, int? files = null) =>
        new(true, "ok", [], null, files, scanning, progress, failed, null, null, null, null, null,
            new UploadActivity(0, 0, 0, 0, 0, 0, null), null, new PortCheckResult(PortState.Off, null, null, null),
            new ShareSwitchState(true, default, null), []);

    [Fact]
    public void Rescan_StartingThenScanningThenDone()
    {
        var asked = Now.AddSeconds(-3);
        Assert.Equal(("running", "Starting"), (ActivityController.Rescan(Report(false, false), asked, Now).State, ActivityController.Rescan(Report(false, false), asked, Now).Detail));
        var scanning = ActivityController.Rescan(Report(true, false, 0.4), Now.AddMinutes(-1), Now);
        Assert.Equal(("running", 0.4), (scanning.State, scanning.Fraction));
        var done = ActivityController.Rescan(Report(false, false, files: 12000), Now.AddMinutes(-2), Now);
        Assert.Equal(("done", "12,000 files shared"), (done.State, done.Detail));
        Assert.Equal("failed", ActivityController.Rescan(Report(false, true), Now.AddMinutes(-2), Now).State);
    }
}
