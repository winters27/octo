using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.Logging.Abstractions;
using Octo.Models.Settings;
using Octo.Services.Common;
using Octo.Services.Library;
using Octo.Services.Lidarr;
using Octo.Services.Lyrics;
using Octo.Services.Soulseek;

namespace Octo.Tests;

/// <summary>
/// The downloads drawer's server half: each row's log (what was searched, what was found, the
/// copy chosen and why, the checks, the tags, the cover and the lyrics), clearing finished rows,
/// Find songs with its reasons, and a picked copy reaching the pipeline exactly once.
/// </summary>
public class DownloadLogTests
{
    private sealed class ManualClock : TimeProvider
    {
        public DateTimeOffset Now { get; set; } = new(2026, 10, 4, 18, 0, 0, TimeSpan.Zero);
        public override DateTimeOffset GetUtcNow() => Now;
    }

    private static AcquisitionTracker NewTracker(TimeProvider? time = null) =>
        new(NullLogger<AcquisitionTracker>.Instance, services: null, time);

    private static IReadOnlyList<AcquisitionEvent> Log(AcquisitionTracker tracker, string user = "alice", string key = "soulseek:abc") =>
        tracker.Detail(key, user)!.Events!;

    // ---------------------------------------------------------------------------------------
    // The log
    // ---------------------------------------------------------------------------------------

    [Fact]
    public void ADownloadLogsEachStepOnceInOrder()
    {
        var tracker = NewTracker();
        tracker.Begin("soulseek", "abc", "abc", "alice", "Daft Punk", "Da Funk", "Homework");
        tracker.Stage("soulseek", "abc", AcquisitionState.Searching, "Soulseek");
        tracker.Stage("soulseek", "abc", AcquisitionState.Searching, "Soulseek");
        tracker.Found("soulseek", "abc", "1 copy fits, best first",
            [new AcquisitionCandidate("Soulseek", "peer1", "Da Funk.flac", Format: "flac", Rank: 1)]);
        tracker.Trying("soulseek", "abc", new AcquisitionCandidate("Soulseek", "peer1", "Da Funk.flac", Format: "flac",
            BitDepth: 16, SampleRate: 44100), "Trying the only copy that fits", "lossless");
        tracker.Transfer("soulseek", "abc", 10, 2_000_000, source: "Soulseek");
        tracker.Transfer("soulseek", "abc", 1_000_000, 2_000_000, source: "Soulseek");
        tracker.Stage("soulseek", "abc", AcquisitionState.Verifying);
        tracker.Stage("soulseek", "abc", AcquisitionState.Importing);
        tracker.Imported("soulseek", "abc", "Daft Punk", "Da Funk", "/music/Da Funk.flac");

        var lines = Log(tracker);
        Assert.Equal(["queued", "search", "found", "try", "transfer", "check", "tags", "library", "done"], lines.Select(l => l.Kind));
        Assert.Equal("Downloading from Soulseek (peer1)", lines[4].Text);
        Assert.Equal("In your library", lines[^1].Text);
        var row = tracker.Detail("soulseek:abc", "alice")!;
        Assert.Equal("FLAC 16-bit 44.1 kHz", row.Quality);
        Assert.Equal("peer1", row.Peer);
    }

    [Fact]
    public void ANoteIsLoggedOnlyWhenItChanges_AndAFailureSaysWhy()
    {
        var tracker = NewTracker();
        tracker.Begin("soulseek", "abc", "abc", "alice");
        tracker.Stage("soulseek", "abc", AcquisitionState.Searching, "YouTube", "Soulseek couldn't get it, trying YouTube");
        tracker.Stage("soulseek", "abc", AcquisitionState.Searching, "YouTube", "Soulseek couldn't get it, trying YouTube");
        tracker.Fail("soulseek", "abc", "No YouTube match for that song. More words for the log at /music/x.");

        var lines = Log(tracker);
        Assert.Equal(["queued", "note", "search", "failed"], lines.Select(l => l.Kind));
        Assert.Equal("No YouTube match for that song", lines[^1].Detail);
    }

    [Fact]
    public void LinesMayFollowTheEnd_ButARestartStartsAFreshLog()
    {
        var tracker = NewTracker();
        tracker.Begin("soulseek", "abc", "abc", "alice");
        tracker.Complete("soulseek", "abc", "nd-1");
        tracker.Log("soulseek", "abc", AcquisitionEventKinds.Lyrics, "Synced lyrics from LRCLIB", "Embedded in the song");
        Assert.Equal(["queued", "done", "lyrics"], Log(tracker).Select(l => l.Kind));

        tracker.Begin("soulseek", "abc", "abc", "alice", kind: AcquisitionKinds.Pick);
        var again = tracker.Detail("soulseek:abc", "alice")!;
        Assert.Equal(AcquisitionKinds.Pick, again.Kind);
        Assert.Equal("Asked for the copy you picked", Assert.Single(again.Events!).Text);
    }

    [Fact]
    public void AHeartJoiningAPickKeepsItAPick()
    {
        var tracker = NewTracker();
        tracker.Begin("soulseek", "abc", "abc", "alice", kind: AcquisitionKinds.Pick);
        tracker.Begin("soulseek", "abc", "abc", "bob");
        Assert.Equal(AcquisitionKinds.Pick, tracker.Detail("soulseek:abc", null)!.Kind);
        Assert.Single(Log(tracker));
    }

    [Fact]
    public void ALongLogKeepsItsFirstLine()
    {
        var tracker = NewTracker();
        tracker.Begin("soulseek", "abc", "abc", "alice");
        for (var i = 0; i < AcquisitionTracker.MaxEvents + 20; i++)
            tracker.Log("soulseek", "abc", AcquisitionEventKinds.Note, $"line {i}");

        var lines = Log(tracker);
        Assert.Equal(AcquisitionTracker.MaxEvents, lines.Count);
        Assert.Equal("queued", lines[0].Kind);
        Assert.Equal($"line {AcquisitionTracker.MaxEvents + 19}", lines[^1].Text);
    }

    [Fact]
    public void TheLogIsOnlyForTheRowsOwners()
    {
        var tracker = NewTracker();
        tracker.Begin("soulseek", "abc", "abc", "alice");
        Assert.NotNull(tracker.Detail("soulseek:abc", "ALICE"));
        Assert.Null(tracker.Detail("soulseek:abc", "bob"));
        Assert.Null(tracker.Detail("soulseek:nope", "alice"));
    }

    [Fact]
    public void ClearingTakesFinishedRowsOffOnePersonsList()
    {
        var tracker = NewTracker();
        tracker.Begin("soulseek", "shared", "shared", "alice");
        tracker.Begin("soulseek", "shared", "shared", "bob");
        tracker.Complete("soulseek", "shared");
        tracker.Begin("soulseek", "mine", "mine", "alice");
        tracker.Fail("soulseek", "mine", "nope");
        tracker.Begin("soulseek", "running", "running", "alice");

        Assert.Equal(1, tracker.Clear("alice", "soulseek:mine"));
        Assert.Equal(["running", "shared"], tracker.ForUser("alice").Select(r => r.Id).Order());
        Assert.Equal(1, tracker.Clear("alice"));
        Assert.Equal(["running"], tracker.ForUser("alice").Select(r => r.Id));
        // Still bob's.
        Assert.Equal(["shared"], tracker.ForUser("bob").Select(r => r.Id));
    }

    [Theory]
    [InlineData("flac", null, 16, 44100, "FLAC 16-bit 44.1 kHz")]
    [InlineData("flac", null, 24, 96000, "FLAC 24-bit 96 kHz")]
    [InlineData(".mp3", 320, null, null, "MP3 320 kbps")]
    [InlineData("ogg", null, null, null, "OGG")]
    [InlineData(null, 320, null, null, null)]
    public void QualityReadsLikeAPerson(string? format, int? bitRate, int? bitDepth, int? sampleRate, string? expected) =>
        Assert.Equal(expected, AcquisitionCandidate.QualityText(format, bitRate, bitDepth, sampleRate));

    // ---------------------------------------------------------------------------------------
    // Soulseek files in words
    // ---------------------------------------------------------------------------------------

    private static SoulseekFileHit Hit(string file, string ext = "flac", int? length = 245, long size = 30_000_000) => new()
    {
        Username = "peer1", Filename = file, Extension = ext, Length = length, Size = size,
        BitDepth = 16, SampleRate = 44100, QueueLength = 2, UploadSpeed = 1_500_000, HasFreeUploadSlot = false,
    };

    [Fact]
    public void AFileShowsItsTitleAlbumAndFolder()
    {
        var copy = SoulseekCandidates.Of(Hit(@"Music\Daft Punk\Homework\CD1\03 - Da Funk.flac"), rank: 2);
        Assert.Equal("03 - Da Funk.flac", copy.File);
        Assert.Equal("Homework/CD1", copy.Folder);
        Assert.Equal("Da Funk", copy.Title);
        Assert.Equal("Homework", copy.Album);
        Assert.Equal("FLAC 16-bit 44.1 kHz", copy.Quality);
        Assert.Equal(2, copy.Rank);
    }

    [Fact]
    public void WhyACopyWasChosenNamesItsQualityAndHowSoonItComes() =>
        Assert.Equal("lossless (FLAC 16-bit 44.1 kHz), 2 ahead in the peer's queue, 1.4 MB/s, 28.6 MB",
            SoulseekCandidates.WhyChosen(Hit("Da Funk.flac")));

    [Fact]
    public void WhyACopyIsPassedOverNamesTheFirstTestItFails()
    {
        var settings = new SoulseekSettings();
        string? Why(SoulseekFileHit hit) => SoulseekCandidates.WhyNot(hit, "Da Funk", "Homework", 330, settings, null, false);

        Assert.Equal("4:05 long; the song is 5:30", Why(Hit("Daft Punk - Da Funk.flac")));
        Assert.Equal("The file name does not match the title", Why(Hit("Daft Punk - Around the World.flac", length: 330)));
        Assert.Equal("Another version: a remix, an edit or a live take", Why(Hit("Daft Punk - Da Funk (Remix).flac", length: 330)));
        Assert.Equal("From a live album", Why(Hit(@"Alive 1997 (Live)\Daft Punk - Da Funk.flac", length: 330)));
        Assert.Equal("Not FLAC, which Octo looks for first", Why(Hit("Daft Punk - Da Funk.mp3", "mp3", length: 330)));
        Assert.Null(Why(Hit("Daft Punk - Da Funk.flac", length: 330)));
    }

    [Fact]
    public void WhyACopyIsPassedOverFollowsTheDownloadsVersionRules()
    {
        var settings = new SoulseekSettings();
        string? Why(SoulseekFileHit hit, string title, string? album = null, string artist = "Alex Clare") =>
            SoulseekCandidates.WhyNot(hit, title, album, 245, settings, null, false, artist);

        // A guest credit or a version tag is not words the file name has to carry.
        Assert.Null(Why(Hit("04 - Take Care.flac"), "Take Care (feat. Rihanna)", "Take Care", "Drake"));
        // A single's folder that says radio edit makes a plainly named file the edit.
        Assert.Equal("From a record of other versions: remixes, edits or a single's radio edit",
            Why(Hit(@"music\Alex Clare\Too Close (Radio Edit) - Single\01 - Too Close.flac"), "Too Close", "The Lateness of the Hour"));
        // Asking for a version never takes the plain song.
        Assert.Equal("Not the version asked for: the file is the plain song",
            Why(Hit("01 - Too Close.flac"), "Too Close (Acoustic)"));
    }

    [Fact]
    public void SoulseekCopiesPutTheDownloadsChoicesFirstAndSayWhyTheRestWereNot()
    {
        var target = new FindTarget("Daft Punk", "Da Funk", "Homework", 330, "id", ExternalId: "id");
        var best = Hit("Daft Punk - Da Funk.flac", length: 330);
        var mp3 = Hit("Daft Punk - Da Funk.mp3", "mp3", length: 330);
        var wrong = Hit("Daft Punk - Revolution 909.flac", length: 330);

        var copies = SongFinder.SoulseekCopies([wrong, mp3, best], [best], target, new SoulseekSettings(), null, false);

        Assert.Equal(["Daft Punk - Da Funk.flac", "Daft Punk - Da Funk.mp3", "Daft Punk - Revolution 909.flac"], copies.Select(c => c.Shown.File));
        Assert.Equal(1, copies[0].Shown.Rank);
        Assert.Null(copies[0].Shown.Note);
        Assert.Equal("Not FLAC, which Octo looks for first", copies[1].Shown.Note);
        Assert.Equal("The file name does not match the title", copies[2].Shown.Note);
        Assert.Equal("Daft Punk - Da Funk.flac", copies[0].Pick.ToHit()!.Filename);
    }

    // ---------------------------------------------------------------------------------------
    // Find songs
    // ---------------------------------------------------------------------------------------

    private static SongFinder NewFinder(FindTarget? target)
    {
        var services = new ServiceCollection().BuildServiceProvider();
        return new SongFinder(services, NullLogger<SongFinder>.Instance)
        {
            Resolve = (_, _) => Task.FromResult(target),
        };
    }

    private static (IReadOnlyList<FoundCopy>, IReadOnlyList<string>, string?) Copies(params FoundCopy[] copies) =>
        (copies, ["query"], $"{copies.Length} found");

    private static FoundCopy Copy(string source, int? rank, string file) =>
        new(new AcquisitionCandidate(source, File: file, Rank: rank, Format: "flac"), new PickedCopy { Source = source, Peer = "p", File = file });

    [Fact]
    public async Task ALookSearchesEverySourceThatIsOnAndOrdersTheCopies()
    {
        var finder = NewFinder(new FindTarget("Air", "Sexy Boy", "Moon Safari", 298, "id", ExternalId: "id"));
        finder.SourcesFor = _ => new Dictionary<string, string?> { ["Soulseek"] = null, ["Lidarr"] = null };
        finder.SearchSoulseek = (_, _) => Task.FromResult(Copies(Copy("Soulseek", null, "other.flac"), Copy("Soulseek", 1, "best.flac")));
        finder.SearchLidarr = (_, _) => Task.FromResult(Copies(Copy("Lidarr", 1, "Lidarr's choice")));

        var started = (await finder.StartAsync("id", "alice"))!;
        var done = (await finder.WaitAsync(started.Id, TimeSpan.FromSeconds(10)))!;

        Assert.Equal(FindStates.Done, done.State);
        Assert.Equal(["best.flac", "Lidarr's choice", "other.flac"], done.Copies.Select(c => c.Shown.File));
        Assert.All(done.Sources, s => Assert.Equal(FindStates.Done, s.State));
        Assert.Null(finder.Get(started.Id, "bob"));
    }

    [Fact]
    public async Task ASourceThatFailsSaysSo_AndTheOtherStillAnswers()
    {
        var finder = NewFinder(new FindTarget("Air", "Sexy Boy", null, null, "id", ExternalId: "id"));
        finder.SourcesFor = _ => new Dictionary<string, string?> { ["Soulseek"] = null, ["Lidarr"] = null };
        finder.SearchSoulseek = (_, _) => throw new InvalidOperationException("slskd is not logged in to Soulseek. Try later.");
        finder.SearchLidarr = (_, _) => Task.FromResult(Copies(Copy("Lidarr", 1, "Lidarr's choice")));

        var done = (await finder.WaitAsync((await finder.StartAsync("id", "alice"))!.Id, TimeSpan.FromSeconds(10)))!;

        Assert.Equal(FindStates.Done, done.State);
        var soulseek = done.Sources.Single(s => s.Name == "Soulseek");
        Assert.Equal(FindStates.Failed, soulseek.State);
        Assert.Equal("slskd is not logged in to Soulseek", soulseek.Text);
    }

    [Fact]
    public async Task WithEverySourceOffALookFailsAtOnceAndSaysWhy()
    {
        var finder = NewFinder(new FindTarget("Air", "Sexy Boy", null, null, "id", ExternalId: "id"));
        finder.SourcesFor = _ => new Dictionary<string, string?>
            { ["Soulseek"] = "Soulseek is not set up on this server", ["Lidarr"] = "Lidarr is not set up on this server" };

        var look = (await finder.StartAsync("id", "alice"))!;

        Assert.Equal(FindStates.Failed, look.State);
        Assert.Contains("None of your download sources can search", look.Error);
    }

    [Fact]
    public async Task APickGoesToFetchWithTheCopyAtThatPlace()
    {
        var finder = NewFinder(new FindTarget("Air", "Sexy Boy", null, null, "id", ExternalId: "id"));
        finder.SourcesFor = _ => new Dictionary<string, string?> { ["Soulseek"] = null };
        finder.SearchSoulseek = (_, _) => Task.FromResult(Copies(Copy("Soulseek", 1, "one.flac"), Copy("Soulseek", 2, "two.flac")));
        FoundCopy? fetched = null;
        finder.Fetch = (_, copy, _) =>
        {
            fetched = copy;
            return Task.FromResult(new PickOutcome("queued", "Getting it."));
        };
        var look = (await finder.WaitAsync((await finder.StartAsync("id", "alice"))!.Id, TimeSpan.FromSeconds(10)))!;

        Assert.Equal("skipped", (await finder.PickAsync(look.Id, 5, "alice")).State);
        Assert.Equal("skipped", (await finder.PickAsync(look.Id, 1, "bob")).State);
        Assert.Equal("queued", (await finder.PickAsync(look.Id, 1, "alice")).State);
        Assert.Equal("two.flac", fetched!.Shown.File);
    }

    [Fact]
    public async Task APickWaitsForTheSearchToEnd_AndACopyKeepsItsIdAndOrderWhenAnotherSourceAnswers()
    {
        var finder = NewFinder(new FindTarget("Air", "Sexy Boy", null, null, "id", ExternalId: "id"));
        finder.SourcesFor = _ => new Dictionary<string, string?> { ["Soulseek"] = null, ["Lidarr"] = null };
        var lidarr = new TaskCompletionSource<(IReadOnlyList<FoundCopy>, IReadOnlyList<string>, string?)>();
        finder.SearchSoulseek = (_, _) => Task.FromResult(Copies(Copy("Soulseek", 2, "two.flac"), Copy("Soulseek", null, "other.flac")));
        finder.SearchLidarr = (_, _) => lidarr.Task;
        FoundCopy? fetched = null;
        finder.Fetch = (_, copy, _) =>
        {
            fetched = copy;
            return Task.FromResult(new PickOutcome("queued", "Getting it."));
        };
        var id = (await finder.StartAsync("id", "alice"))!.Id;
        FindSnapshot early;
        do
        {
            await Task.Delay(10);
            early = finder.Get(id, "alice")!;
        } while (early.Copies.Count < 2);

        // Soulseek has answered and Lidarr has not: a pick now, by place or by id, waits.
        Assert.Equal(FindStates.Searching, early.State);
        Assert.Equal(["two.flac", "other.flac"], early.Copies.Select(c => c.Shown.File));
        Assert.Equal(2, early.Copies.Select(c => c.Id).Distinct().Count());
        Assert.Equal(SongFinder.StillSearchingText, (await finder.PickAsync(id, 0, "alice")).Detail);
        Assert.Equal(SongFinder.StillSearchingText, (await finder.PickAsync(id, early.Copies[1].Id, "alice")).Detail);
        Assert.Null(fetched);

        lidarr.SetResult(Copies(Copy("Lidarr", 1, "Lidarr's choice")));
        var done = (await finder.WaitAsync(id, TimeSpan.FromSeconds(10)))!;

        // Lidarr's first choice goes in between; the copies already shown keep their ids and order.
        Assert.Equal(["Lidarr's choice", "two.flac", "other.flac"], done.Copies.Select(c => c.Shown.File));
        Assert.Equal(early.Copies.Select(c => c.Id), done.Copies.Skip(1).Select(c => c.Id));
        Assert.Equal("queued", (await finder.PickAsync(id, early.Copies[1].Id, "alice")).State);
        Assert.Equal("other.flac", fetched!.Shown.File);
        Assert.Equal("That copy is not on the list.", (await finder.PickAsync(id, "c99", "alice")).Detail);
    }

    [Fact]
    public void OnlyALosslessCopyMayReplaceALossyLibrarySong()
    {
        var mp3Owned = new FindTarget("Air", "Sexy Boy", null, null, "nd-1", LibraryId: "nd-1", OwnedFormat: "mp3");
        var flacOwned = mp3Owned with { OwnedFormat = "flac" };
        var flac = Copy("Soulseek", 1, "Air - Sexy Boy.flac");
        flac.Pick.Size = 30_000_000;
        var mp3 = new FoundCopy(new AcquisitionCandidate("Soulseek", Format: "mp3"), new PickedCopy { Source = "Soulseek" });
        var lidarrMp3 = new FoundCopy(new AcquisitionCandidate("Lidarr", Format: "MP3-320"),
            new PickedCopy { Source = "Lidarr", ReleaseGuid = "g", IndexerId = 1 });
        var lidarrFlac = new FoundCopy(new AcquisitionCandidate("Lidarr", Format: "FLAC 24bit"),
            new PickedCopy { Source = "Lidarr", ReleaseGuid = "g", IndexerId = 1 });
        var lidarrChoice = new FoundCopy(new AcquisitionCandidate("Lidarr"), new PickedCopy { Source = "Lidarr" });

        Assert.Null(SongFinder.ReplaceRefusal(mp3Owned, flac));
        Assert.Null(SongFinder.ReplaceRefusal(mp3Owned, lidarrFlac));
        Assert.Null(SongFinder.ReplaceRefusal(mp3Owned, lidarrChoice));
        Assert.NotNull(SongFinder.ReplaceRefusal(mp3Owned, mp3));
        Assert.NotNull(SongFinder.ReplaceRefusal(mp3Owned, lidarrMp3));
        Assert.Equal("Your copy is already lossless, so nothing would be better.", SongFinder.ReplaceRefusal(flacOwned, flac));
    }

    [Fact]
    public void LidarrReleasesKeepLidarrsOrderAndSayWhyOneIsRejected()
    {
        var target = new FindTarget("Air", "Sexy Boy", "Moon Safari", 298, "id", ExternalId: "id");
        var copies = SongFinder.LidarrCopies(
        [
            new LidarrRelease("a", 1, "Air - Moon Safari [FLAC]", "Redacted", "FLAC", 300_000_000, 12, 0, "torrent", 3, false, []),
            new LidarrRelease("b", 2, "Air - Moon Safari MP3", "NZBgeek", "MP3-320", 90_000_000, null, null, "usenet", 1, true, ["Not wanted in profile"]),
        ], target, "Moon Safari");

        Assert.Equal(2, copies[0].Shown.Rank);
        Assert.Equal("torrent, 12 seeders, 3 days old", copies[0].Shown.Note);
        Assert.Null(copies[1].Shown.Rank);
        Assert.Equal("Not wanted in profile", copies[1].Shown.Note);
        Assert.Equal(new LidarrReleasePick("a", 1, "Air - Moon Safari [FLAC]"), copies[0].Pick.Release);
    }

    // ---------------------------------------------------------------------------------------
    // Picks reach the pipeline once
    // ---------------------------------------------------------------------------------------

    [Fact]
    public void APickIsTakenOnceAndOnlyByItsOwnSource()
    {
        var clock = new ManualClock();
        var picks = new DownloadPicks(clock);
        picks.Pin("abc", new PickedCopy { Source = "Lidarr", ReleaseGuid = "g", IndexerId = 4 });

        Assert.Null(picks.Take("abc", "Soulseek"));
        Assert.Equal("g", picks.Take("abc", "Lidarr")!.ReleaseGuid);
        Assert.Null(picks.Take("abc", "Lidarr"));

        picks.Pin("old", new PickedCopy { Source = "Soulseek", Peer = "p", File = "f.flac" });
        clock.Now += DownloadPicks.Keep;
        Assert.Null(picks.Take("old", "Soulseek"));
    }

    [Fact]
    public void APickWaitingInTheUpgradeQueueOutlivesARestart()
    {
        var path = Path.Combine(Path.GetTempPath(), $"octo-upgrades-{Guid.NewGuid():N}.json");
        try
        {
            var pick = new PickedCopy { Source = "Lidarr", ReleaseGuid = "g1", IndexerId = 7, ReleaseTitle = "Air - Moon Safari [FLAC]" };
            new UpgradeQueue(path).Add([new UpgradeAsk("nd-1", Pick: pick)], "alice", "find");
            Assert.DoesNotContain("IsSoulseek", File.ReadAllText(path));

            var job = Assert.Single(new UpgradeQueue(path).Snapshot());
            Assert.Equal(new LidarrReleasePick("g1", 7, "Air - Moon Safari [FLAC]"), job.Pick!.Release);
            Assert.False(job.Pick.IsSoulseek);
        }
        finally
        {
            File.Delete(path);
        }
    }

    [Fact]
    public async Task AnUpgradeWithAPickFetchesOnlyThatCopyAndLogsTheVerdict()
    {
        var queue = new UpgradeQueue();
        var picks = new DownloadPicks();
        var tracker = NewTracker();
        var pick = new PickedCopy { Source = "Soulseek", Peer = "peer9", File = "x.flac", Format = "flac", BitDepth = 16, SampleRate = 44100 };
        queue.Add([new UpgradeAsk("nd-1", "Sexy Boy", "Air", "Moon Safari", "mp3", Pick: pick)], "alice", "find");
        LibraryActionRequest? asked = null;
        var worker = new UpgradeWorker(queue, null!, null!, NullLogger<UpgradeWorker>.Instance, tracker: tracker, picks: picks)
        {
            Apply = (request, _) =>
            {
                asked = request;
                request.OnReplacementQueued!("soulseek", "ext1");
                tracker.Complete("soulseek", "ext1");
                return Task.FromResult(new LibraryActionOutcome(LibraryActionState.Failed, "nothing", LibraryActionCodes.NoReplacement));
            },
            Describe = (_, _) => Task.FromResult<ResolvedSongFile?>(null),
            Width = () => 1,
            SoulseekOffline = _ => Task.FromResult(false),
        };

        await worker.TickAsync(CancellationToken.None);
        await worker.DrainAsync();

        Assert.Equal(DownloadSource.Soulseek, asked!.OnlySource);
        Assert.Equal("peer9", picks.Take("ext1", "Soulseek")!.Peer);
        var row = tracker.Detail("soulseek:ext1", "alice")!;
        Assert.Equal(AcquisitionKinds.Upgrade, row.Kind);
        Assert.Equal("nd-1", row.Id);
        Assert.Contains(row.Events!, line => line.Text == "Getting FLAC 16-bit 44.1 kHz from peer9");
        Assert.Equal("No lossless copy found; your copy is unchanged", row.Events![^1].Text);
    }

    // ---------------------------------------------------------------------------------------
    // Lyrics in the log
    // ---------------------------------------------------------------------------------------

    private static readonly LyricsJob Job = new("/music/a.flac", "Air", "Sexy Boy", null, 298, Acquisition: "soulseek:abc");

    [Fact]
    public void LyricsSayWhatWasFoundFromWhereAndWhereTheyWent()
    {
        var found = new LyricsResult("LRCLIB", "[00:01.00]Hi", null, false);
        Assert.Equal(("Synced lyrics from LRCLIB", "Embedded in the song"),
            LyricsSidecarWriter.ReportText(Job, new LyricsWrite(LyricsWriteOutcome.Written, found), SongLyricsPlace.Inside));
        Assert.Equal(("Synced lyrics from LRCLIB", "Saved beside the song as a .lrc file"),
            LyricsSidecarWriter.ReportText(Job, new LyricsWrite(LyricsWriteOutcome.Written, found), SongLyricsPlace.Beside));
        Assert.Equal(("No lyrics found", "None of the lyrics sources has this song"),
            LyricsSidecarWriter.ReportText(Job, new LyricsWrite(LyricsWriteOutcome.NotFound, null), SongLyricsPlace.None));
        Assert.Equal("No lyrics service answered",
            LyricsSidecarWriter.ReportText(Job, new LyricsWrite(LyricsWriteOutcome.Retrying, null), SongLyricsPlace.None).Text);
        Assert.Null(LyricsSidecarWriter.ReportText(Job, new LyricsWrite(LyricsWriteOutcome.Gone, null), SongLyricsPlace.None).Text);
    }
}
