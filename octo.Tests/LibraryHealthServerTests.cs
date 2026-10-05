using System.Text.Json;
using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.Logging.Abstractions;
using Octo.Controllers;
using Octo.Models.Settings;
using Octo.Services.Health;
using Octo.Services.Library;

namespace Octo.Tests;

/// <summary>
/// Library health on the server: Navidrome's own song list read into what the checks need, the
/// words and plans the app has, and the dashboard's runs of fixes and lookups.
/// </summary>
public class LibraryHealthServerTests
{
    // ---- reading Navidrome's song list ----------------------------------------------------------

    /// <summary>NOKIA as LXC 111's Navidrome 0.64 lists it in /api/song (2026-10-05), trimmed.</summary>
    private const string Nokia = """
        [{"id":"6KpragWkvfYGesMZAXBmAm","title":"NOKIA","album":"$ome $exy $ongs 4 U","artist":"Drake",
          "albumArtist":"PARTYNEXTDOOR","albumId":"08qrDbXNlx9NTxtgugKes1","hasCoverArt":true,"trackNumber":14,
          "discNumber":1,"year":2025,"size":35876224,"suffix":"flac","duration":241.92,"bitRate":994,"sampleRate":44100,
          "bitDepth":16,"genre":"R&B","genres":[{"id":"1","name":"R&B"},{"id":"2","name":"Hip Hop"}],
          "mbzRecordingID":"f14b5405-2d38-4693-adaf-38d27821943f","mbzReleaseGroupId":"786a089a-9266-4fd3-984a-1df9e0ca34b4",
          "tags":{"barcode":["808391285880"],"isrc":["USLD91772032"],"recordlabel":["OVO Sound"]},
          "participants":{"albumartist":[{"id":"a","name":"PARTYNEXTDOOR"}],"artist":[{"id":"b","name":"Drake"}]},
          "path":"PARTYNEXTDOOR/$ome $exy $ongs 4 U/14 - NOKIA.flac","missing":false,"createdAt":"2026-10-05T02:16:27.952323177Z"},
         {"id":"gone","title":"Gone","artist":"X","path":"x.flac","missing":true},
         {"id":"bare","title":"Nightcall","artist":"Kavinsky","albumId":"u","hasCoverArt":false,"year":0,"trackNumber":0,
          "createdAt":"2026-08-14T19:43:35.76268Z",
          "suffix":"m4a","bitDepth":0,"duration":257.4,"path":"Kavinsky/Nightcall.m4a"}]
        """;

    private static IReadOnlyList<LibrarySongRow> Rows() =>
        QualityUpgradeWorker.ParseSongs(JsonDocument.Parse(Nokia).RootElement)!.Value.Rows;

    [Fact]
    public void ASongRow_CarriesWhatTheChecksRead()
    {
        var rows = Rows();
        Assert.Equal(["6KpragWkvfYGesMZAXBmAm", "bare"], rows.Select(row => row.Id));
        var nokia = rows[0];
        Assert.Equal(241.92, nokia.Seconds);
        Assert.Equal(242, nokia.Duration);
        Assert.Equal(["R&B", "Hip Hop"], nokia.Genres);
        Assert.Equal((2025, 14, 1, 16, 44100), (nokia.Year, nokia.Track, nokia.Disc, nokia.BitDepth, nokia.SampleRate));
        Assert.True(nokia.HasCover);
        Assert.Equal("f14b5405-2d38-4693-adaf-38d27821943f", nokia.RecordingId);
        Assert.Equal("786a089a-9266-4fd3-984a-1df9e0ca34b4", nokia.ReleaseGroupId);
        Assert.Equal("808391285880", nokia.Barcode);
        Assert.Equal(["OVO Sound"], nokia.Labels);
        Assert.Equal(["Drake"], nokia.Artists);
        Assert.Equal(["PARTYNEXTDOOR"], nokia.AlbumArtists);
        Assert.Equal(["USLD91772032"], nokia.Isrcs);
        Assert.False(rows[1].HasCover);
    }

    /// <summary>Navidrome's search3, which the apps read, lists songs as they were added; the
    /// checks take them in that order, so a reason names the same song the app's does.</summary>
    [Fact]
    public void TheSongsAreCheckedInTheOrderTheAppsReadThem()
    {
        var rows = Rows();
        Assert.Equal(new DateTime(2026, 10, 5, 2, 16, 27, 952, DateTimeKind.Utc).AddTicks(3232), rows[0].AddedUtc);
        Assert.Equal(["bare", "6KpragWkvfYGesMZAXBmAm"], LibraryHealthService.InSearchOrder(rows).Select(row => row.Id));
    }

    [Fact]
    public void TheServersReader_ReadsTheSongsAsTheAppDoesThroughSubsonic()
    {
        var rows = Rows();
        var fields = new ServerSongFields(rows);
        // Subsonic cuts the fraction off: 241.92 s is 241, not the rounded 242.
        Assert.Equal(241, fields.Seconds(rows[0]));
        Assert.True(fields.Lossless(rows[0]));
        // An m4a with no bit depth is AAC.
        Assert.False(fields.Lossless(rows[1]));
        Assert.Equal("FLAC, 16-bit, 44.1 kHz", HealthWords.QualityText(rows[0], fields));
        Assert.Null(fields.Cover(rows[1]));
        Assert.Equal("786a089a-9266-4fd3-984a-1df9e0ca34b4", fields.ReleaseGroupId(rows[0]));
        Assert.Equal(["OVO Sound"], fields.Labels(rows[0]));
        var report = LibraryHealth.Check(rows, fields);
        Assert.Equal(["bare"], report.Missing[HealthTag.Cover].Select(song => song.Id));
        Assert.Equal(["bare"], report.Missing[HealthTag.Year].Select(song => song.Id));
    }

    [Fact]
    public void AnAlbumsRelease_IsTheOneMostOfItsSongsCarry()
    {
        LibrarySongRow Row(string id, string? group) => new(id, $"{id}.flac", null, 1, "flac", 900, id, "A", 200, "Album", "al", "A")
        {
            ReleaseGroupId = group,
        };
        var rows = new[] { Row("1", "g2"), Row("2", "g1"), Row("3", "g1"), Row("4", null) };
        var fields = new ServerSongFields(rows);
        Assert.All(rows, row => Assert.Equal("g1", fields.ReleaseGroupId(row)));
    }

    // ---- words and plans ---------------------------------------------------------------------

    [Fact]
    public void CountsAndQualityReadNaturally()
    {
        Assert.Equal("1 song", HealthWords.CountLabel(HealthCheck.NoGenre, 1));
        Assert.Equal("2,256 songs", HealthWords.CountLabel(HealthCheck.NoGenre, 2256));
        Assert.Equal("64 songs with more than one copy", HealthWords.CountLabel(HealthCheck.Duplicates, 64));
        Assert.Equal("FLAC, 16-bit, 44.1 kHz", HealthWords.QualityText("flac", true, 16, 44_100, 900));
        Assert.Equal("MP3, 320 kbps", HealthWords.QualityText("mp3", false, null, 44_100, 320));
        Assert.Equal("unknown quality", HealthWords.QualityText(null, false, null, null, null));
    }

    [Fact]
    public void NoWordsUseADash()
    {
        var lookup = new SongLookup("found", null, new Dictionary<string, string?>(), new Dictionary<string, string?>(),
            "Medium", "Database", "'Mezzanine' 1998");
        var words = Enum.GetValues<HealthCheck>().SelectMany(check => new[]
            {
                HealthWords.Title(check), HealthWords.Meaning(check), HealthWords.Advice(check),
                HealthWords.FixAllLabel(check, 2), HealthWords.FixMeaning(check),
            })
            .Concat(Enum.GetValues<AlbumDifference>().Select(kind => HealthWords.Words(new AlbumDifferenceValues(kind, ["a", "b"]))))
            .Concat(Enum.GetValues<DuplicateBasis>().Select(HealthWords.Words))
            .Concat(Enum.GetValues<BestReason>().Select(reason => HealthWords.Words(reason, "FLAC")))
            .Concat(Enum.GetValues<SplitBasis>().Select(basis => HealthWords.Words(new SplitReason(basis, "A", "B", "S", "V"))))
            .Concat(SongTagFields.All.Select(HealthFixes.TagName))
            .Concat([HealthWords.AllClear, new TagChange("year", "1997", "1998").Words, HealthFixes.Origin(lookup)]);
        foreach (var line in words)
            Assert.False(line.Contains('—') || line.Contains('–') || line.Contains(" - "), line);
    }

    [Fact]
    public void ALookupPicksBlanks_AndOverwritesOnlyWhenSure_NeverTheSongsName()
    {
        var lookup = new SongLookup("found", null,
            new Dictionary<string, string?> { ["title"] = "teardrop", ["year"] = null, ["genre"] = "Electronic", ["track"] = "3" },
            new Dictionary<string, string?> { ["title"] = "Teardrop", ["year"] = "1998", ["genre"] = "Trip Hop", ["track"] = "3" },
            "Strong", "Fingerprint", null);

        Assert.Equal(new Dictionary<string, bool> { ["title"] = false, ["year"] = true, ["genre"] = true },
            HealthFixes.Changes(lookup).ToDictionary(pair => pair.Change.Tag, pair => pair.Pick));
        Assert.Equal(new Dictionary<string, bool> { ["title"] = false, ["year"] = true, ["genre"] = false },
            HealthFixes.Changes(lookup with { Confidence = "Low" }).ToDictionary(pair => pair.Change.Tag, pair => pair.Pick));
        Assert.Equal("A sure match from the song's fingerprint.", HealthFixes.Origin(lookup));
    }

    [Fact]
    public void ARunSaysWhatWasDone_WhatWasNot_AndHowToPutItBack()
    {
        var retag = FixStep.RetagWith("flac", "Teardrop", [new TagChange("year", null, "1998")]);
        var remove = new FixStep(FixActions.Remove, "mp3", "Teardrop");
        var outcome = new FixOutcome([retag, remove], [new FixStep(FixActions.Cover, "c", "Angel")],
            [(new FixStep(FixActions.Remove, "ogg", "Teardrop"), "Could not work out which file this is, so nothing was touched.")], false, false);

        Assert.Equal(["mp3"], outcome.Removed);
        Assert.Equal([new FixStep(FixActions.Restore, "mp3", "Teardrop"), new FixStep(FixActions.Undo, "flac", "Teardrop")], outcome.UndoSteps);
        Assert.Equal("Removed 1 song, changed the tags of 1 song. Teardrop: Could not work out which file this is, so nothing was touched.",
            outcome.Summary());
        Assert.Contains("dry run", new FixOutcome([], [], [], true, false).Summary());
        Assert.Equal("Nothing needed changing.", new FixOutcome([], [remove], [], false, false).Summary());
    }

    [Fact]
    public void TheDuplicatePlan_RemovesCopiesAsCopies_AfterFillingTheKeptOne()
    {
        var rows = new[]
        {
            new LibrarySongRow("mp3", "a.mp3", null, 1, "mp3", 320, "Teardrop", "Massive Attack", 330, "Mezzanine", "al", "Massive Attack")
                { Year = 1998, Seconds = 330 },
            new LibrarySongRow("flac", "a.flac", null, 1, "flac", 900, "Teardrop", "Massive Attack", 330, "Mezzanine", "al", "Massive Attack")
                { BitDepth = 16, SampleRate = 44100, Seconds = 330.4 },
        };
        var fields = new ServerSongFields(rows);
        var set = Assert.Single(LibraryHealth.FindDuplicates(rows, fields));
        var steps = HealthFixes.Steps(HealthFixes.DuplicateFix(set, fields), fields);

        Assert.Equal(new FixStep(FixActions.Retag, "flac", "Teardrop", new Dictionary<string, string> { ["year"] = "1998" }), steps[0]);
        Assert.Equal(FixStep.RemoveCopy("mp3", "Teardrop"), steps[1]);
        Assert.Equal("true", steps[1].Params["copy"]);
    }

    [Theory]
    [InlineData("duplicates", HealthCheck.Duplicates)]
    [InlineData("splitAlbums", HealthCheck.SplitAlbums)]
    [InlineData("noTrackNumber", HealthCheck.NoTrackNumber)]
    [InlineData("NOGENRE", HealthCheck.NoGenre)]
    public void CheckNamesGoBothWays(string name, HealthCheck check)
    {
        Assert.Equal(check, LibraryHealthController.ParseCheck(name));
        Assert.Equal(name, LibraryHealthController.CheckName(check), StringComparer.OrdinalIgnoreCase);
    }

    [Fact]
    public void OnlyKnownChangesAreSent()
    {
        Assert.Null(LibraryHealthService.Invalid([FixStep.RemoveCopy("a", "A"), FixStep.Join("b", "B", "a")]));
        Assert.NotNull(LibraryHealthService.Invalid([]));
        Assert.NotNull(LibraryHealthService.Invalid([new FixStep("delete-everything", "a", "A")]));
        Assert.NotNull(LibraryHealthService.Invalid([new FixStep(FixActions.Retag, "a", "A", new Dictionary<string, string> { ["path"] = "/etc" })]));
        Assert.NotNull(LibraryHealthService.Invalid([new FixStep(FixActions.JoinAlbum, "a", "A")]));
        Assert.NotNull(LibraryHealthService.Invalid(Enumerable.Range(0, LibraryHealthService.MaxSteps + 1)
            .Select(i => FixStep.RemoveCopy($"{i}", "x")).ToList()));
    }

    // ---- the scan after a run ------------------------------------------------------------------

    [Fact]
    public void AHeldRescan_AsksOnceAtTheEnd()
    {
        var rescan = new LibraryRescan();
        using (rescan.Hold())
        {
            rescan.Soon();
            rescan.Soon();
            rescan.Soon();
            Assert.Equal(0, rescan.Scheduled);
        }
        Assert.Equal(3, rescan.Asked);
        Assert.Equal(1, rescan.Scheduled);

        using (rescan.Hold()) { }
        Assert.Equal(1, rescan.Scheduled);
        rescan.Soon();
        Assert.Equal(2, rescan.Scheduled);
    }

    // ---- the service ---------------------------------------------------------------------------

    private static LibrarySongRow Song(string id, string title, string album, string albumId, int? year = 2020, string genre = "Rock",
        string suffix = "flac", int seconds = 200) =>
        new(id, $"{id}.{suffix}", null, 1, suffix, suffix == "flac" ? 900 : 320, title, "Artist", seconds, album, albumId, "Artist")
        {
            Seconds = seconds, Year = year, Genres = [genre], Track = 1, BitDepth = suffix == "flac" ? 16 : 0, SampleRate = 44100,
        };

    private sealed class Library
    {
        public List<LibrarySongRow> Rows { get; set; } = [];
        public int Reads;
        public DateTime Now = new(2026, 10, 5, 12, 0, 0, DateTimeKind.Utc);
    }

    private static (LibraryHealthService Health, LibraryRescan Rescan, Library Library) Service(LibraryActionSettings? settings = null)
    {
        var library = new Library();
        var snapshot = new LibrarySnapshot(null!, NullLogger<LibrarySnapshot>.Instance)
        {
            ReadPage = (start, count, _) =>
            {
                if (start == 0) Interlocked.Increment(ref library.Reads);
                return Task.FromResult<(IReadOnlyList<LibrarySongRow>, int)?>((library.Rows.Skip(start).Take(count).ToList(),
                    Math.Max(0, Math.Min(count, library.Rows.Count - start))));
            },
            Clock = () => library.Now,
        };
        var rescan = new LibraryRescan();
        var services = new ServiceCollection().BuildServiceProvider();
        var health = new LibraryHealthService(snapshot, rescan, new TestOptionsMonitor<LibraryActionSettings>(settings ?? new LibraryActionSettings()),
            services, NullLogger<LibraryHealthService>.Instance)
        {
            Clock = () => library.Now,
        };
        return (health, rescan, library);
    }

    private static async Task<HealthRun> Finished(LibraryHealthService health)
    {
        for (var i = 0; i < 500 && health.Run is { State: "running" }; i++) await Task.Delay(10);
        return health.Run!;
    }

    [Fact]
    public async Task TheReport_IsTheAppsChecksOverTheLibrary()
    {
        var (health, _, library) = Service();
        library.Rows = [Song("a", "Song", "Album", "x"), Song("b", "Song", "Album", "x", suffix: "mp3"), Song("c", "Other", "Album", "x", year: null)];

        var found = await health.ReportAsync(fresh: false);

        Assert.NotNull(found);
        Assert.Equal(3, found.Report.Checked);
        Assert.Equal([HealthCheck.Duplicates, HealthCheck.NoYear], found.Report.Findings);
        Assert.Equal(["a", "b"], found.Report.Duplicates[0].Copies.Select(song => song.Id));
    }

    [Fact]
    public async Task ARun_SendsEachStepAsTheAdmin_AsksForOneScan_AndLeavesTheFixedSongsOut()
    {
        var (health, rescan, library) = Service();
        library.Rows = [Song("a", "One", "Album", "x"), Song("b", "Two", "Album", "x", year: null), Song("c", "Three", "Album", "x", year: null)];
        var sent = new List<(FixStep Step, string User)>();
        health.Send = (step, user) =>
        {
            sent.Add((step, user));
            rescan.Soon();
            return Task.FromResult(step.Id == "c" ? new FixAnswer("failed", "The file has changed since.") : new FixAnswer("applied", "Changed the year."));
        };
        Assert.Equal(2, (await health.ReportAsync(false))!.Report.Count(HealthCheck.NoYear));

        var steps = new[] { "b", "c" }.Select(id => FixStep.RetagWith(id, id, [new TagChange("year", null, "2020")])).ToList();
        Assert.Null(health.StartFix("Filling in tags", HealthCheck.NoYear, steps, "winters"));
        var run = await Finished(health);

        Assert.Equal("done", run.State);
        Assert.All(sent, pair => Assert.Equal("winters", pair.User));
        Assert.Equal(2, run.Done);
        Assert.Equal("Changed the tags of 1 song. c: The file has changed since.", run.Outcome!.Summary());
        Assert.Equal(1, rescan.Scheduled);
        Assert.Equal(1, health.UndoCount);
        // The read is from before the fix, so b is left out until a read from after Navidrome's scan.
        Assert.Equal(["c"], (await health.ReportAsync(false))!.Report.Songs(HealthCheck.NoYear).Select(song => song.Id));
        library.Now += LibraryHealthService.SettleFor + TimeSpan.FromSeconds(1);
        // By title within the album: Three, then Two.
        Assert.Equal(["c", "b"], (await health.ReportAsync(fresh: true))!.Report.Songs(HealthCheck.NoYear).Select(song => song.Id));
    }

    [Fact]
    public async Task OneRunAtATime_AndStopEndsItBetweenSongs()
    {
        var (health, _, library) = Service();
        var gate = new TaskCompletionSource();
        var sent = 0;
        health.Send = async (_, _) =>
        {
            Interlocked.Increment(ref sent);
            await gate.Task;
            return new FixAnswer("applied", null);
        };
        var steps = Enumerable.Range(0, 5).Select(i => FixStep.RemoveCopy($"{i}", $"Song {i}")).ToList();

        Assert.Null(health.StartFix("Fixing copies", HealthCheck.Duplicates, steps, "winters"));
        var busy = health.StartFix("Joining albums", HealthCheck.SplitAlbums, steps, "winters");
        Assert.NotNull(busy);
        Assert.Equal("Fixing copies", busy.Label);
        Assert.NotNull(health.StartUndo("winters"));

        Assert.True(health.Stop());
        gate.SetResult();
        var run = await Finished(health);
        Assert.Equal("stopped", run.State);
        Assert.Equal(1, sent);
        Assert.Contains("Stopped before the end.", run.Outcome!.Summary());
        Assert.Equal(1, health.UndoCount);
    }

    [Fact]
    public async Task Undo_PutsBackWhatTheLastRunDid_TheLastFirst()
    {
        var (health, _, _) = Service();
        var sent = new List<FixStep>();
        health.Send = (step, _) =>
        {
            sent.Add(step);
            return Task.FromResult(new FixAnswer("applied", null));
        };
        Assert.Null(health.StartFix("Fixing copies", HealthCheck.Duplicates,
            [FixStep.RetagWith("keep", "Keep", [new TagChange("year", null, "1998")]), FixStep.RemoveCopy("copy", "Copy")], "winters"));
        await Finished(health);
        sent.Clear();

        Assert.Null(health.StartUndo("winters"));
        var run = await Finished(health);

        Assert.Equal("undo", run.Kind);
        Assert.Equal([new FixStep(FixActions.Restore, "copy", "Copy"), new FixStep(FixActions.Undo, "keep", "Keep")], sent);
        Assert.Equal("Put back 1 song, undid the changes to 1 song.", run.Outcome!.Summary());
        Assert.Equal(0, health.UndoCount);
        Assert.Equal("There is nothing to undo.", health.StartUndo("winters"));
    }

    [Fact]
    public async Task ALookup_SaysWhatItFound_AndWhatToPick()
    {
        var (health, _, _) = Service();
        health.Look = id => Task.FromResult(id == "miss"
            ? new SongLookup("failed", "Nothing matched.", new Dictionary<string, string?>(), new Dictionary<string, string?>(), null, null, null)
            : new SongLookup("found", null, new Dictionary<string, string?> { ["track"] = null },
                new Dictionary<string, string?> { ["track"] = "3", ["year"] = "1998" }, "Strong", "Fingerprint", "Mezzanine"));

        Assert.Null(health.StartLookup([("hit", "Teardrop"), ("miss", "Angel")], HealthCheck.NoTrackNumber, "winters"));
        var run = await Finished(health);

        Assert.Equal("lookup", run.Kind);
        var lookups = run.LookupsNow();
        Assert.Equal("A sure match from the song's fingerprint, on Mezzanine.", lookups[0].Origin);
        Assert.Equal([("year", "1998", true), ("track", "3", true)], lookups[0].Changes.Select(pair => (pair.Change.Tag, pair.Change.Value, pair.Pick)));
        Assert.Equal("Nothing matched.", lookups[1].Detail);
        var item = Assert.Single(ActivityController.Health(run, null, run.FinishedUtc!.Value));
        Assert.Equal(("health", "done", "Found tags for 1 of 2 songs"), (item.Page, item.State, item.Detail));
    }

    [Fact]
    public void TheActivityFeed_ShowsACheckAndARunGoing()
    {
        var now = DateTime.UtcNow;
        var run = new HealthRun { Id = "r", Kind = "fix", Label = "Joining albums", User = "winters", Total = 20, Done = 5, Current = "NOKIA" };
        var items = ActivityController.Health(run, now, now).ToList();

        Assert.Equal(["Checking your library", "Joining albums"], items.Select(item => item.Title));
        Assert.Equal((5, 20, "changes", "NOKIA"), (items[1].Done, items[1].Total, items[1].Unit, items[1].Detail));
        Assert.Empty(ActivityController.Health(null, null, now));
    }

    [Fact]
    public void WhatThePageMayDo_FollowsTheLibraryActionSettings()
    {
        var off = Service().Health.Abilities("winters");
        Assert.False(off.Edit);
        Assert.Contains("Library actions are off", off.Why);

        var rehearsing = Service(new LibraryActionSettings { Enabled = true, AllowedUsers = ["winters"] }).Health.Abilities("winters");
        Assert.True(rehearsing.DryRun);
        Assert.False(rehearsing.Edit);
        Assert.Contains("rehearsal", rehearsing.Why);

        var stranger = Service(new LibraryActionSettings { Enabled = true, DryRun = false, AllowedUsers = ["winters"] }).Health.Abilities("bob");
        Assert.Contains("bob is not on the library actions allowed list", stranger.Why);
    }
}
