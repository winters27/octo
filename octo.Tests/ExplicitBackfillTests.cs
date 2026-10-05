using System.Net;
using System.Text;
using System.Text.Json;
using Microsoft.AspNetCore.Mvc.Testing;
using Microsoft.Extensions.Configuration;
using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.DependencyInjection.Extensions;
using Microsoft.Extensions.Logging.Abstractions;
using Octo.Services.Common;
using Octo.Services.Library;
using Octo.Services.Metadata;
using Octo.Services.Tagging;

namespace Octo.Tests;

/// <summary>
/// Marking the library explicit or clean: only a confident catalog match counts, the preview
/// writes nothing, Apply writes only explicit and clean in place and journals each write, a
/// file changed since the preview is left alone, and Undo takes every mark out again.
/// </summary>
public sealed class ExplicitBackfillTests : IDisposable
{
    private readonly string _root = Path.Combine(Path.GetTempPath(), "octo-explicit-" + Guid.NewGuid().ToString("N"));

    public ExplicitBackfillTests()
    {
        Directory.CreateDirectory(_root);
        ExplicitBackfill.Pace = TimeSpan.Zero;
        ExplicitBackfill.BusyWait = TimeSpan.Zero;
    }

    public void Dispose()
    {
        try { Directory.Delete(_root, recursive: true); } catch { /* best effort */ }
    }

    private static CatalogTrackFacts Track(string id, string title, string artist, int? content, int? seconds = 200,
        string? album = null, string? isrc = null) => new(id, title, artist, album, seconds, isrc, content);

    private static LibrarySongFacts Song(string artist, string title, string? album = null, double? seconds = 200,
        params string[] isrcs) => new(artist, title, album, seconds, isrcs);

    /// <summary>A catalog that answers from fixed lists.</summary>
    private sealed class FakeCatalog : IExplicitCatalog
    {
        public Dictionary<string, CatalogTrackAnswer> ByIsrc { get; } = new(StringComparer.Ordinal);
        public Dictionary<string, CatalogTrackAnswer> ByName { get; } = new(StringComparer.OrdinalIgnoreCase);
        public int Searches { get; private set; }

        public Task<CatalogTrackAnswer> ByIsrcAsync(string isrc, CancellationToken ct) =>
            Task.FromResult(ByIsrc.GetValueOrDefault(isrc, new CatalogTrackAnswer([], false)));

        public Task<CatalogTrackAnswer> SearchAsync(string? artist, string? title, CancellationToken ct)
        {
            Searches++;
            return Task.FromResult(ByName.GetValueOrDefault($"{artist}|{title}", new CatalogTrackAnswer([], false)));
        }
    }

    // ---- The lookup ------------------------------------------------------------------------

    [Fact]
    public void Decide_EveryCopyThatFitsSaysExplicit_IsExplicit()
    {
        var finding = ExplicitLookup.Decide(Song("Logic", "America", "Everybody"),
            [Track("1", "America", "Logic", 1, album: "Everybody"), Track("2", "America", "Logic", 1, album: "Everybody (Deluxe)")]);

        Assert.Equal(ExplicitOutcome.Explicit, finding.Outcome);
        Assert.Equal(1, finding.Advisory);
        Assert.Equal("artist, title, length and album, 2 copies agree", finding.How);
    }

    /// <summary>The catalog lists a clean edit beside the original under the same name and
    /// length; with nothing to tell them apart, nothing is written.</summary>
    [Fact]
    public void Decide_AnExplicitAndACleanCopyBothFit_IsUnsure()
    {
        var finding = ExplicitLookup.Decide(Song("Kendrick Lamar", "squabble up", "GNX", 157),
            [Track("1", "squabble up", "Kendrick Lamar", 1, 157, "GNX"), Track("2", "squabble up", "Kendrick Lamar", 3, 157, "GNX")]);

        Assert.Equal(ExplicitOutcome.Unsure, finding.Outcome);
        Assert.Null(finding.Advisory);
        Assert.Equal("an explicit and a clean copy both fit", finding.How);
    }

    [Fact]
    public void Decide_TheCopyOnTheSongsOwnAlbumSpeaksForIt()
    {
        var finding = ExplicitLookup.Decide(Song("A", "Song", "Real Album"),
            [Track("1", "Song", "A", 3, album: "Radio Hits 2020"), Track("2", "Song", "A", 1, album: "Real Album")]);

        Assert.Equal(ExplicitOutcome.Explicit, finding.Outcome);
        Assert.Equal("2", finding.CatalogTrackId);
    }

    [Fact]
    public void Decide_AnotherLengthOrAnotherVersionIsNotTheSong()
    {
        Assert.Equal(ExplicitOutcome.Unsure, ExplicitLookup.Decide(Song("A", "Song", seconds: 200),
            [Track("1", "Song", "A", 1, seconds: 260)]).Outcome);
        Assert.Equal(ExplicitOutcome.Unsure, ExplicitLookup.Decide(Song("A", "Song"),
            [Track("1", "Song (Live)", "A", 1)]).Outcome);
    }

    [Fact]
    public void Decide_NotExplicitAndUnsaid()
    {
        Assert.Equal(ExplicitOutcome.NotExplicit, ExplicitLookup.Decide(Song("John Mayer", "Gravity"),
            [Track("1", "Gravity", "John Mayer", 0)]).Outcome);
        Assert.Equal("the catalog does not say", ExplicitLookup.Decide(Song("A", "B"),
            [Track("1", "B", "A", null)]).How);
    }

    [Fact]
    public async Task Find_TheIsrcSpeaksFirst_AndANameSayingCleanBeforeThat()
    {
        var catalog = new FakeCatalog();
        catalog.ByIsrc["USUM71702791"] = new([Track("9", "AfricAryaN", "Logic", 1, isrc: "USUM71702791")], false);
        catalog.ByName["Logic|AfricAryaN"] = new([Track("8", "AfricAryaN", "Logic", 3)], false);

        var byCode = await ExplicitLookup.FindAsync(Song("Logic", "AfricAryaN", isrcs: "usum71702791"), catalog, default);
        var byName = await ExplicitLookup.FindAsync(Song("Logic", "AfricAryaN (Clean)"), catalog, default);

        Assert.Equal(ExplicitOutcome.Explicit, byCode.Outcome);
        Assert.Equal("same ISRC (USUM71702791)", byCode.How);
        Assert.Equal(0, catalog.Searches);
        Assert.Equal(ExplicitOutcome.Clean, byName.Outcome);
    }

    [Fact]
    public async Task Find_ABusyCatalogIsSaidSo_NotTakenForAnAnswer()
    {
        var catalog = new FakeCatalog();
        catalog.ByName["A|B"] = CatalogTrackAnswer.Busy;

        var finding = await ExplicitLookup.FindAsync(Song("A", "B"), catalog, default);

        Assert.True(finding.CatalogBusy);
        Assert.Null(finding.Advisory);
    }

    // ---- The run on real files ----------------------------------------------------------

    private string Flac(string name, string artist, string title, int? advisory = null)
    {
        var path = Path.Combine(_root, name);
        File.WriteAllBytes(path, AudioFixtures.Flac());
        using var file = TagLib.File.Create(path);
        file.Tag.Performers = [artist];
        file.Tag.Title = title;
        if (advisory is not null) TagWriterExtras.SetAdvisory(file, advisory);
        file.Save();
        return path;
    }

    private string Mp3(string name, string artist, string title)
    {
        var path = Path.Combine(_root, name);
        File.WriteAllBytes(path, AudioFixtures.Mp3());
        using var file = TagLib.File.Create(path);
        file.Tag.Performers = [artist];
        file.Tag.Title = title;
        file.Save();
        return path;
    }

    private static int? AdvisoryOf(string path)
    {
        using var file = TagLib.File.Create(path);
        return TagWriterExtras.ReadAdvisory(file);
    }

    private (ExplicitBackfill Worker, TagEditJournal Journal, FakeCatalog Catalog) Worker(params (string Path, string? Status)[] songs)
    {
        var catalog = new FakeCatalog();
        // The fixtures are two seconds long, so the catalog's lengths match them.
        catalog.ByName["Logic|America"] = new([Track("1", "America", "Logic", 1, 2)], false);
        catalog.ByName["Edit|Radio"] = new([Track("2", "Radio", "Edit", 3, 2)], false);
        catalog.ByName["John Mayer|Gravity"] = new([Track("3", "Gravity", "John Mayer", 0, 2)], false);
        catalog.ByName["Kendrick Lamar|squabble up"] = new([Track("4", "squabble up", "Kendrick Lamar", 1, 2),
            Track("5", "squabble up", "Kendrick Lamar", 3, 2)], false);
        var journal = new TagEditJournal(Path.Combine(_root, "tag-edits.json"));
        var worker = new ExplicitBackfill(new ExplicitBackfillStore(), journal, catalog,
            new ServiceCollection().BuildServiceProvider(), new ConfigurationBuilder().Build(), NullLogger<ExplicitBackfill>.Instance)
        {
            ListSongs = _ => Task.FromResult<IReadOnlyList<(ExplicitBackfillSong, string?)>>(
                songs.Select(song => (new ExplicitBackfillSong(song.Path, "nd-" + Path.GetFileNameWithoutExtension(song.Path)), song.Status)).ToList()),
        };
        return (worker, journal, catalog);
    }

    [Fact]
    public async Task Preview_LooksUpOnlyUnmarkedSongs_AndWritesNothing()
    {
        var america = Flac("america.flac", "Logic", "America");
        var marked = Flac("marked.flac", "Logic", "America", advisory: 1);
        var bytes = File.ReadAllBytes(america);
        var (worker, _, catalog) = Worker((america, ""), (marked, ""), (Flac("e.flac", "Logic", "America"), "e"));

        await worker.RunAsync(new ExplicitBackfillRequest(ExplicitBackfillMode.Preview));

        var run = worker.Current;
        Assert.Equal(ExplicitBackfillStatus.Completed, run.Status);
        Assert.Equal(1, run.Explicit);
        Assert.Equal(2, run.AlreadyMarked);
        Assert.Equal(1, catalog.Searches);
        Assert.Equal(bytes, File.ReadAllBytes(america));
        Assert.True(run.CanApply);
    }

    [Fact]
    public async Task Apply_WritesExplicitAndCleanInPlace_JournalsEach_AndUndoTakesThemOut()
    {
        var america = Flac("america.flac", "Logic", "America");
        var radio = Mp3("radio.mp3", "Edit", "Radio");
        var gravity = Flac("gravity.flac", "John Mayer", "Gravity");
        var squabble = Flac("squabble.flac", "Kendrick Lamar", "squabble up");
        var changed = Flac("changed.flac", "Logic", "America");
        var (worker, journal, _) = Worker((america, ""), (radio, ""), (gravity, ""), (squabble, ""), (changed, ""));

        await worker.RunAsync(new ExplicitBackfillRequest(ExplicitBackfillMode.Preview));
        var preview = worker.Current;
        Assert.Equal((2, 1, 1, 1), (preview.Explicit, preview.Clean, preview.NotExplicit, preview.Unsure));
        // Changed after the preview: Apply must leave it alone.
        using (var file = TagLib.File.Create(changed)) { file.Tag.Comment = "edited by hand"; file.Save(); }
        File.SetLastWriteTimeUtc(changed, DateTime.UtcNow.AddMinutes(1));

        await worker.RunAsync(new ExplicitBackfillRequest(ExplicitBackfillMode.Apply, "winters"));

        Assert.Equal(1, AdvisoryOf(america));
        Assert.Equal(2, AdvisoryOf(radio));
        Assert.Null(AdvisoryOf(gravity));
        Assert.Null(AdvisoryOf(squabble));
        Assert.Null(AdvisoryOf(changed));
        var applied = worker.Current;
        Assert.Equal((2, 1), (applied.Written, applied.LeftAlone));
        var entries = journal.OfRun(applied.AppliedRunId!);
        Assert.Equal(2, entries.Count);
        Assert.All(entries, entry => Assert.Equal(TagEditKinds.Advisory, entry.Kind));
        Assert.Contains(entries, entry => entry.NavidromeId == "nd-america" && entry.After[TagEditKinds.Advisory] == "1"
            && entry.Before[TagEditKinds.Advisory] is null && entry.Username == "winters");
        Assert.True(File.Exists(Path.Combine(_root, "tag-edits.json")));
        Assert.True(worker.CanUndo);

        await worker.RunAsync(new ExplicitBackfillRequest(ExplicitBackfillMode.Undo));

        Assert.Null(AdvisoryOf(america));
        Assert.Null(AdvisoryOf(radio));
        Assert.Empty(journal.OfRun(applied.AppliedRunId!));
        Assert.False(worker.CanUndo);
        Assert.Equal(2, worker.Current.Written);
    }

    [Fact]
    public void SetAdvisory_RoundTripsAndClears()
    {
        var path = Mp3("x.mp3", "A", "B");

        var written = LibraryTagEdits.SetAdvisory(path, 1);
        var again = LibraryTagEdits.SetAdvisory(path, 1);
        var cleared = LibraryTagEdits.SetAdvisory(path, null);

        Assert.True(written.Changed);
        Assert.False(again.Changed);
        Assert.True(cleared.Changed);
        Assert.Equal("1", cleared.Before[TagEditKinds.Advisory]);
        Assert.Null(AdvisoryOf(path));
    }

    // ---- The dashboard's calls ----------------------------------------------------------

    [Fact]
    public async Task Endpoints_NeedASignIn_AndApplyNeedsTheMusicPathTyped()
    {
        var store = new ExplicitBackfillStore();
        store.Replace(new ExplicitBackfillRun
        {
            RunId = "r1", Status = ExplicitBackfillStatus.Completed, Mode = ExplicitBackfillMode.Preview, Explicit = 1,
            Rows = [new ExplicitBackfillRow { Path = "/music/a.flac", Artist = "Logic", Title = "America", Outcome = "explicit", How = "same ISRC" }],
        });
        await using var factory = new AdminWebFactory().WithWebHostBuilder(builder => builder.ConfigureServices(services =>
        {
            services.RemoveAll<ExplicitBackfillStore>();
            services.AddSingleton(store);
        }));
        var client = factory.CreateClient();
        var token = factory.Services.GetRequiredService<Octo.Services.Admin.BrowseSessionStore>().Create("admin");

        using var anonymous = await client.GetAsync("/api/admin/explicit/backfill");
        using var get = new HttpRequestMessage(HttpMethod.Get, "/api/admin/explicit/backfill");
        get.Headers.Add("X-Octo-Browse-Token", token);
        using var status = await client.SendAsync(get);
        using var apply = new HttpRequestMessage(HttpMethod.Post, "/api/admin/explicit/backfill/apply")
        {
            Content = new StringContent("{\"confirm\":\"wrong\"}", Encoding.UTF8, "application/json"),
        };
        apply.Headers.Add("X-Octo-Browse-Token", token);
        apply.Headers.Add(Octo.Middleware.AdminRequestGuard.HeaderName, "1");
        using var refused = await client.SendAsync(apply);

        Assert.Equal(HttpStatusCode.Unauthorized, anonymous.StatusCode);
        Assert.Equal(HttpStatusCode.OK, status.StatusCode);
        using var body = JsonDocument.Parse(await status.Content.ReadAsStringAsync());
        Assert.Equal(1, body.RootElement.GetProperty("explicit").GetInt32());
        Assert.Equal(1, body.RootElement.GetProperty("toWrite").GetInt32());
        Assert.True(body.RootElement.GetProperty("canApply").GetBoolean());
        Assert.Equal("explicit", body.RootElement.GetProperty("rows")[0].GetProperty("outcome").GetString());
        Assert.Equal(HttpStatusCode.BadRequest, refused.StatusCode);
        Assert.Contains("type the music path", await refused.Content.ReadAsStringAsync());
    }
}
