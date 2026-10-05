using System.Net;
using System.Text;
using System.Text.Json;
using Microsoft.Extensions.Logging.Abstractions;
using Octo.Models.Settings;
using Octo.Services.Common;
using Octo.Services.Library;
using Octo.Services.Sonic;

namespace Octo.Tests;

/// <summary>
/// Sounds alike's pass over the library: when it holds, what it sends to octo-sonic, what it skips
/// without touching the disk, how failures and crashes are remembered, and new arrivals first.
/// </summary>
public sealed class SonicAnalysisWorkerTests : IDisposable
{
    private readonly string _directory = Path.Combine(Path.GetTempPath(), "octo-sonic-worker-" + Guid.NewGuid());

    public SonicAnalysisWorkerTests() => Directory.CreateDirectory(_directory);
    public void Dispose() { try { Directory.Delete(_directory, true); } catch { } }

    private sealed class Clock : TimeProvider
    {
        private DateTimeOffset _now = new(2026, 10, 4, 12, 0, 0, TimeSpan.Zero);
        public void Advance(TimeSpan by) => _now += by;
        public override DateTimeOffset GetUtcNow() => _now;
    }

    private sealed class Activity : IAcquisitionActivity { public bool IsBusy { get; set; } }

    /// <summary>octo-sonic: its health, and per file name an answer (features, an error, or a dropped connection).</summary>
    private sealed class Sidecar : HttpMessageHandler
    {
        public int? Version { get; set; } = 2;
        /// <summary>Its music folder is empty or unreadable: health answers 503.</summary>
        public bool Blind { get; set; }
        public List<string> Analysed { get; } = [];
        public Dictionary<string, string> Errors { get; } = new();
        public HashSet<string> Drops { get; } = [];
        /// <summary>Files it cannot find (404), as when it mounts another folder than Octo.</summary>
        public HashSet<string> Missing { get; } = [];
        /// <summary>Files it answers with something that is not its JSON.</summary>
        public HashSet<string> Garbled { get; } = [];

        protected override async Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken ct)
        {
            if (request.RequestUri!.AbsolutePath == "/health")
                return Version is not { } v ? new HttpResponseMessage(HttpStatusCode.BadGateway)
                    : Blind ? Json(HttpStatusCode.ServiceUnavailable, $$"""{"ok":false,"featuresVersion":{{v}},"error":"cannot see the music folder"}""")
                    : Json(HttpStatusCode.OK, $$"""{"ok":true,"featuresVersion":{{v}}}""");
            using var doc = JsonDocument.Parse(await request.Content!.ReadAsStringAsync(ct));
            var name = Path.GetFileName(doc.RootElement.GetProperty("path").GetString()!);
            Analysed.Add(name);
            if (Drops.Contains(name)) throw new HttpRequestException("connection reset");
            if (Missing.Contains(name)) return Json(HttpStatusCode.NotFound, """{"error":"no such file"}""");
            if (Garbled.Contains(name)) return Json(HttpStatusCode.OK, """{"features":"lots"}""");
            if (Errors.TryGetValue(name, out var error)) return Json(HttpStatusCode.UnprocessableEntity, $$"""{"error":"{{error}}"}""");
            var features = string.Join(",", Enumerable.Range(0, 23).Select(i => (i / 100.0).ToString(System.Globalization.CultureInfo.InvariantCulture)));
            return Json(HttpStatusCode.OK, $$"""{"features":[{{features}}],"version":{{Version ?? 2}}}""");
        }

        private static HttpResponseMessage Json(HttpStatusCode status, string body) =>
            new(status) { Content = new StringContent(body, Encoding.UTF8, "application/json") };
    }

    private sealed class Library(string directory) : ISonicLibrary
    {
        public List<LibrarySongRow> Rows { get; } = [];
        public Dictionary<string, LibrarySongRow> Arrivals { get; } = new();
        public int FileLookups;
        public bool Listable { get; set; } = true;
        /// <summary>A page that fails, by its start.</summary>
        public int? FailingPage { get; set; }
        /// <summary>Songs Navidrome lists as missing: counted in the page, left out of its rows.</summary>
        public HashSet<string> MissingIds { get; } = [];

        /// <summary>A row with no file behind it, for listing tests.</summary>
        public void AddListed(string id) =>
            Rows.Add(new LibrarySongRow(id, Path.Combine(directory, "none", id + ".flac"), null, 1000, "flac", 900, id, "Artist", 200, "Album"));

        public LibrarySongRow Add(string id, string name, int? seconds = 200, List<LibrarySongRow>? into = null)
        {
            var path = Path.Combine(directory, name);
            File.WriteAllBytes(path, new byte[1000 + id.Length]);
            var row = new LibrarySongRow(id, path, null, new FileInfo(path).Length, "flac", 900, name, "Artist", seconds, "Album");
            (into ?? Rows).Add(row);
            return row;
        }

        /// <summary>As Navidrome's native API answers: the count is the page's rows before missing
        /// songs were left out, not the library's.</summary>
        public Task<(IReadOnlyList<LibrarySongRow> Rows, int Count)?> ListAsync(int start, int count, CancellationToken ct)
        {
            if (!Listable || FailingPage == start) return Task.FromResult<(IReadOnlyList<LibrarySongRow> Rows, int Count)?>(null);
            var page = Rows.Skip(start).Take(count).ToList();
            return Task.FromResult<(IReadOnlyList<LibrarySongRow> Rows, int Count)?>(
                (page.Where(row => !MissingIds.Contains(row.Id)).ToList(), page.Count));
        }

        public Task<LibrarySongRow?> ArrivedAsync(string id, CancellationToken ct) => Task.FromResult(Arrivals.GetValueOrDefault(id));

        public FileInfo? LocalFile(LibrarySongRow row)
        {
            FileLookups++;
            return File.Exists(row.Path) ? new FileInfo(row.Path) : null;
        }
    }

    private (SonicAnalysisWorker Worker, SonicStore Store, Sidecar Sidecar, Library Library, Activity Activity, Clock Clock)
        Make(RadioSourceSettings? settings = null)
    {
        var store = new SonicStore(Path.Combine(_directory, "state.json"), NullLogger<SonicStore>.Instance);
        var sidecar = new Sidecar();
        var monitor = TestOptions.Monitor(settings ?? new RadioSourceSettings { SonicUrl = "http://sonic.test", SonicPauseSeconds = 0 });
        var client = new SonicClient(new ReviewFixtures.OneClientFactory(sidecar), monitor, NullLogger<SonicClient>.Instance);
        var library = new Library(_directory);
        var activity = new Activity();
        var clock = new Clock();
        var worker = new SonicAnalysisWorker(store, client, library, activity, monitor,
            NullLogger<SonicAnalysisWorker>.Instance, tracker: null, time: clock);
        return (worker, store, sidecar, library, activity, clock);
    }

    [Fact]
    public async Task ItHolds_WhenOff_Paused_Down_OrBusy()
    {
        var off = Make(new RadioSourceSettings { SoundsAlike = false });
        await off.Worker.TickAsync(default);
        Assert.Equal("Off", off.Worker.Status().State);

        var t = Make();
        t.Library.Add("a", "a.flac");
        t.Worker.SetPaused(true);
        await t.Worker.TickAsync(default);
        Assert.Equal("Paused", t.Worker.Status().State);

        t.Worker.SetPaused(false);
        t.Sidecar.Version = null;
        await t.Worker.TickAsync(default);
        Assert.Equal(("Waiting", "octo-sonic is not answering."), (t.Worker.Status().State, t.Worker.Status().Reason));

        t.Sidecar.Version = 2;
        t.Activity.IsBusy = true;
        await t.Worker.TickAsync(default);
        Assert.Equal("Waiting for a download to finish.", t.Worker.Status().Reason);
        Assert.Empty(t.Sidecar.Analysed);
    }

    [Fact]
    public async Task WithoutANavidromeSignIn_ItSaysSo()
    {
        var t = Make();
        t.Library.Listable = false;
        await t.Worker.TickAsync(default);
        Assert.Contains("Octo needs its own Navidrome sign-in", t.Worker.Status().Reason);
    }

    [Fact]
    public async Task ItAnalysesEachSongOnce_ThenFinishesThePass()
    {
        var t = Make();
        t.Library.Add("a", "a.flac");
        t.Library.Add("b", "b.flac");

        await t.Worker.TickAsync(default);
        await t.Worker.TickAsync(default);
        Assert.Equal(["a.flac", "b.flac"], t.Sidecar.Analysed);
        Assert.Equal(23, t.Store.Read(s => s.Songs["a"].F.Length));
        Assert.Equal(2, t.Store.Read(s => s.Songs["a"].Version));

        var lookups = t.Library.FileLookups;
        await t.Worker.TickAsync(default);
        Assert.Equal("Done", t.Worker.Status().State);
        Assert.Equal(1, t.Store.Read(s => s.Pass));
        // Analysed songs at the same size are passed over without asking for their files.
        Assert.Equal(lookups, t.Library.FileLookups);
        Assert.Equal(2, t.Sidecar.Analysed.Count);
    }

    [Fact]
    public async Task ANewFeatureVersion_IsAnalysedAgain()
    {
        var t = Make();
        t.Library.Add("a", "a.flac");
        await t.Worker.TickAsync(default);
        await t.Worker.TickAsync(default);
        t.Clock.Advance(SonicAnalysisWorker.PassInterval + TimeSpan.FromMinutes(1));

        t.Sidecar.Version = 3;
        await t.Worker.TickAsync(default);
        Assert.Equal(["a.flac", "a.flac"], t.Sidecar.Analysed);
        Assert.Equal(3, t.Store.Read(s => s.Songs["a"].Version));
    }

    [Fact]
    public async Task AnUnreadableFile_IsRemembered_AndTriedAgainAWeekLater()
    {
        var t = Make();
        t.Library.Add("bad", "bad.flac");
        t.Sidecar.Errors["bad.flac"] = "empty or too short song";

        await t.Worker.TickAsync(default);
        Assert.Equal("empty or too short song", t.Store.Read(s => s.Failed["bad"].Error));
        var lookups = t.Library.FileLookups;
        await t.Worker.TickAsync(default);
        Assert.Single(t.Sidecar.Analysed);
        Assert.Equal(lookups, t.Library.FileLookups);

        t.Clock.Advance(SonicAnalysisWorker.RetryFailedAfter + TimeSpan.FromHours(1));
        await t.Worker.TickAsync(default);
        await t.Worker.TickAsync(default);
        Assert.Equal(2, t.Sidecar.Analysed.Count);
    }

    [Fact]
    public async Task AFileThatDropsTheConnectionTwice_IsMarkedUnreadable_AndThePassMovesOn()
    {
        var t = Make();
        t.Library.Add("crash", "crash.flac");
        t.Library.Add("fine", "fine.flac");
        t.Sidecar.Drops.Add("crash.flac");

        await t.Worker.TickAsync(default);
        Assert.Equal("octo-sonic stopped answering; trying again.", t.Worker.Status().Reason);
        Assert.Empty(t.Store.Read(s => s.Failed));

        await t.Worker.TickAsync(default);
        Assert.Equal("octo-sonic stopped while reading it", t.Store.Read(s => s.Failed["crash"].Error));

        await t.Worker.TickAsync(default);
        Assert.Equal(["crash.flac", "crash.flac", "fine.flac"], t.Sidecar.Analysed);
    }

    [Fact]
    public async Task ASongThatJustArrived_GoesFirst_EvenIfThePassHasNotListedIt()
    {
        var t = Make();
        t.Library.Add("a", "a.flac");
        t.Library.Add("b", "b.flac");
        await t.Worker.TickAsync(default);

        var arrival = t.Library.Add("new", "new.flac", into: []);
        t.Library.Arrivals["new"] = arrival;
        t.Worker.ArrivedFirst("new");
        await t.Worker.TickAsync(default);

        Assert.Equal(["a.flac", "new.flac"], t.Sidecar.Analysed);
    }

    [Fact]
    public async Task ALongMix_IsNeverRead()
    {
        var t = Make();
        t.Library.Add("mix", "mix.flac", seconds: 2 * 3600);
        await t.Worker.TickAsync(default);
        Assert.Empty(t.Sidecar.Analysed);
        Assert.Equal("Done", t.Worker.Status().State);
    }

    [Fact]
    public async Task AtThePassEnd_GoneSongsAreForgotten_AndStartOverClearsEverything()
    {
        var t = Make();
        t.Store.Write(s => s.Songs["gone"] = new SonicSong { Stamp = "1:1", Version = 2, F = new float[23] });
        t.Library.Add("a", "a.flac");
        await t.Worker.TickAsync(default);
        await t.Worker.TickAsync(default);
        Assert.False(t.Store.Read(s => s.Songs.ContainsKey("gone")));
        Assert.True(t.Store.Read(s => s.Songs.ContainsKey("a")));

        t.Worker.Reset();
        Assert.Empty(t.Store.Read(s => s.Songs));
        Assert.Equal(0, t.Store.Read(s => s.Pass));
    }

    private async Task<int> TicksUntilDone((SonicAnalysisWorker Worker, SonicStore Store, Sidecar Sidecar, Library Library, Activity Activity, Clock Clock) t)
    {
        for (var tick = 1; tick <= 20; tick++)
        {
            await t.Worker.TickAsync(default);
            if (t.Worker.Status().State == "Done") return tick;
        }
        throw new Xunit.Sdk.XunitException("the pass never finished");
    }

    [Fact]
    public async Task APageThatFails_ForgetsNothing()
    {
        var t = Make();
        for (var i = 0; i < SonicAnalysisWorker.PageSize + 5; i++) t.Library.AddListed($"s{i}");
        t.Store.Write(s => s.Songs[$"s{SonicAnalysisWorker.PageSize + 2}"] = new SonicSong { Stamp = "1:1", Version = 2, F = new float[23] });
        t.Library.FailingPage = SonicAnalysisWorker.PageSize;

        await t.Worker.TickAsync(default);
        Assert.Equal("Waiting", t.Worker.Status().State);
        Assert.Single(t.Store.Read(s => s.Songs));
        Assert.Equal(0, t.Store.Read(s => s.Pass));
    }

    [Fact]
    public async Task APageWithAMissingSong_IsNotTheEndOfTheList()
    {
        var t = Make();
        for (var i = 0; i < SonicAnalysisWorker.PageSize + 5; i++) t.Library.AddListed($"s{i}");
        t.Library.MissingIds.Add("s3");
        var later = $"s{SonicAnalysisWorker.PageSize + 2}";
        t.Store.Write(s => s.Songs[later] = new SonicSong { Stamp = "1:1", Version = 2, F = new float[23] });

        await TicksUntilDone(t);
        Assert.True(t.Store.Read(s => s.Songs.ContainsKey(later)), "a song on the second page is still known");
        // Every listed row has no file: left out, and counted.
        Assert.Equal(SonicAnalysisWorker.PageSize + 4, t.Worker.Status().Skipped);
    }

    [Fact]
    public async Task ASongThatArrivedDuringAPass_IsKeptAtItsEnd()
    {
        var t = Make();
        t.Library.Add("a", "a.flac");
        await t.Worker.TickAsync(default);
        var arrival = t.Library.Add("new", "new.flac", into: []);
        t.Library.Arrivals["new"] = arrival;
        t.Worker.ArrivedFirst("new");

        await TicksUntilDone(t);
        Assert.True(t.Store.Read(s => s.Songs.ContainsKey("new")));
    }

    [Fact]
    public async Task ASongThatArrivesBetweenPasses_IsReadWithoutANewPass()
    {
        var t = Make();
        t.Library.Add("a", "a.flac");
        await TicksUntilDone(t);
        var arrival = t.Library.Add("new", "new.flac", into: []);
        t.Library.Arrivals["new"] = arrival;
        t.Worker.ArrivedFirst("new");

        await t.Worker.TickAsync(default);
        await t.Worker.TickAsync(default);
        Assert.Equal(["a.flac", "new.flac"], t.Sidecar.Analysed);
        Assert.Equal(1, t.Store.Read(s => s.Pass));
        Assert.Equal("Done", t.Worker.Status().State);
    }

    [Fact]
    public async Task WhenOctoSonicCannotSeeTheMusic_ItWaits_AndBlamesNoSong()
    {
        var t = Make();
        t.Library.Add("a", "a.flac");
        t.Sidecar.Blind = true;
        await t.Worker.TickAsync(default);
        Assert.Equal(SonicAnalysisWorker.CannotSeeMusic, t.Worker.Status().Reason);
        Assert.Empty(t.Sidecar.Analysed);

        // It sees a folder, but not Octo's: every file is missing to it.
        t.Sidecar.Blind = false;
        t.Sidecar.Missing.Add("a.flac");
        await t.Worker.TickAsync(default);
        Assert.Equal(SonicAnalysisWorker.CannotSeeMusic, t.Worker.Status().Reason);
        Assert.Empty(t.Store.Read(s => s.Failed));

        t.Sidecar.Missing.Clear();
        await t.Worker.TickAsync(default);
        Assert.True(t.Store.Read(s => s.Songs.ContainsKey("a")));
    }

    [Fact]
    public async Task AnAnswerOctoCannotRead_IsThatSongsFailure()
    {
        var t = Make();
        t.Library.Add("odd", "odd.flac");
        t.Library.Add("fine", "fine.flac");
        t.Sidecar.Garbled.Add("odd.flac");
        await TicksUntilDone(t);
        Assert.Contains("could not read", t.Store.Read(s => s.Failed["odd"].Error));
        Assert.True(t.Store.Read(s => s.Songs.ContainsKey("fine")));
    }

    [Fact]
    public async Task ASongWithNoLength_IsLeftOut_AndCounted()
    {
        var t = Make();
        t.Library.Add("zero", "zero.flac", seconds: 0);
        t.Library.Add("none", "none.flac", seconds: null);
        t.Library.Add("fine", "fine.flac");
        await TicksUntilDone(t);
        Assert.Equal(["fine.flac"], t.Sidecar.Analysed);
    }

    [Fact]
    public async Task PauseAndStartOver_AreSavedAtOnce()
    {
        var t = Make();
        t.Worker.SetPaused(true);
        Assert.Contains("\"Paused\":true", File.ReadAllText(Path.Combine(_directory, "state.json")));
        t.Worker.Reset();
        Assert.Contains("\"Pass\":0", File.ReadAllText(Path.Combine(_directory, "state.json")));
    }
}
