using System.Net;
using System.Net.Http.Json;
using System.Text.Json;
using Microsoft.AspNetCore.Hosting;
using Microsoft.AspNetCore.Mvc.Testing;
using Microsoft.Extensions.Configuration;
using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.DependencyInjection.Extensions;
using Microsoft.Extensions.Hosting;
using Microsoft.Extensions.Logging.Abstractions;
using Octo.Controllers;
using Octo.Models.Download;
using Octo.Models.Settings;
using Octo.Services.Admin;
using Octo.Services.Common;
using Octo.Services.Library;
using Octo.Services.Local;

namespace Octo.Tests;

/// <summary>
/// The dashboard's side of three things the apps already do: a download's full log on Fetched
/// songs (kept after the live list forgets it), Find songs as the signed-in admin, and Recently
/// removed with Put back. Every route needs a Navidrome admin's own sign-in; the recovery code
/// and no sign-in are both refused, whatever the dashboard gate let through.
/// </summary>
public sealed class AdminDownloadsTests : IDisposable
{
    private readonly string _directory = Path.Combine(Path.GetTempPath(), "octo-dash-downloads-" + Guid.NewGuid());

    public AdminDownloadsTests() => Directory.CreateDirectory(_directory);

    public void Dispose()
    {
        try { Directory.Delete(_directory, true); } catch { }
    }

    private DownloadHistoryService History(string name = "downloads-history.json") =>
        new(Path.Combine(_directory, name), NullLogger<DownloadHistoryService>.Instance);

    private static DownloadHistoryEntry Entry(string title, string? key, DateTime at) => new()
    {
        Artist = "Daft Punk", Title = title, Album = "Discovery", Path = $"/music/{title}.flac", Format = "FLAC",
        Source = "Soulseek", DownloadedAt = at.ToString("o"), Key = key,
    };

    private static AcquisitionEvent Line(DateTime at, string kind, string text, int copies = 0) =>
        new(at, kind, text, null, copies == 0 ? null
            : Enumerable.Range(1, copies).Select(i => new AcquisitionCandidate("Soulseek", Peer: $"peer{i}", File: $"/share/{i}.flac",
                Format: "flac", BitDepth: 16, SampleRate: 44100, Size: 30_000_000, Rank: i)).ToList());

    // ---- the saved log --------------------------------------------------------------------------

    [Fact]
    public void ALogLandsOnTheEntryThisRunWrote_NeverAnOlderFetchOfTheSameSong()
    {
        var history = History();
        var start = DateTime.UtcNow;
        history.Record(Entry("Digital Love", "soulseek:a", start.AddDays(-2)));
        history.Record(Entry("Old song", null, start.AddDays(-1)));
        history.Record(Entry("Digital Love", "soulseek:a", start.AddSeconds(30)));

        Assert.True(history.AttachLog("soulseek:a", start, [Line(start, AcquisitionEventKinds.Queued, "Asked for")]));
        // A run that began after every entry for it has no entry yet.
        Assert.False(history.AttachLog("soulseek:a", start.AddMinutes(5), [Line(start, AcquisitionEventKinds.Queued, "Asked for")]));

        var entries = History().GetRecent();
        Assert.Single(entries[0].Log!);
        Assert.Null(entries[1].Log);
        Assert.Null(entries[2].Log);
        Assert.Equal("Asked for", History().Find("soulseek:a", entries[0].DownloadedAt)!.Log![0].Text);
        Assert.Null(History().Find("soulseek:a", entries[2].DownloadedAt)!.Log);
    }

    [Fact]
    public void ASavedLogKeepsItsBeginningAndNewestLines_AndTheBestCopiesOfEachLine()
    {
        var at = DateTime.UtcNow;
        var lines = new List<AcquisitionEvent> { Line(at, AcquisitionEventKinds.Found, "Found 30 copies", copies: 30) };
        lines.AddRange(Enumerable.Range(1, 199).Select(i => Line(at.AddSeconds(i), AcquisitionEventKinds.Note, $"line {i}")));
        lines.Add(new AcquisitionEvent(at.AddSeconds(500), AcquisitionEventKinds.Note, new string('x', 2000)));

        var saved = DownloadHistoryService.Trimmed(lines);

        Assert.Equal(DownloadHistoryService.MaxLogLines, saved.Count);
        Assert.Equal("Found 30 copies", saved[0].Text);
        Assert.Equal(DownloadHistoryService.MaxSavedCandidates, saved[0].Candidates!.Count);
        Assert.Equal("peer1", saved[0].Candidates![0].Peer);
        Assert.Contains("left out", saved[1].Text);
        Assert.Equal(DownloadHistoryService.MaxSavedText, saved[^1].Text.Length);
        Assert.Equal("line 199", saved[^2].Text);
    }

    [Fact]
    public void WhenTheLogsFillTheirRoom_TheOldestLetGoAndTheirEntriesStay()
    {
        var history = History();
        history.LogRoom = 1200;
        var start = DateTime.UtcNow.AddMinutes(-10);
        for (var i = 0; i < 4; i++)
        {
            history.Record(Entry($"Song {i}", $"soulseek:{i}", start.AddMinutes(i + 1)));
            Assert.True(history.AttachLog($"soulseek:{i}", start.AddMinutes(i),
                Enumerable.Range(0, 5).Select(n => Line(start, AcquisitionEventKinds.Note, $"step {n} of song {i}")).ToList()));
        }

        var entries = History().GetRecent();
        Assert.Equal(4, entries.Count);
        Assert.NotNull(entries[0].Log);
        Assert.Null(entries[^1].Log);
        Assert.True(new FileInfo(Path.Combine(_directory, "downloads-history.json")).Length < 4000);
    }

    [Fact]
    public void AFinishedDownloadsLogIsSaved_AndALineAfterTheEndSavesItAgain_AndAFailureSavesNothing()
    {
        var history = History();
        var tracker = new AcquisitionTracker(NullLogger<AcquisitionTracker>.Instance);
        DownloadLogKeeper.Attach(tracker, history, NullLogger.Instance);

        tracker.Begin("soulseek", "song-1", "song-1", "alice", "Daft Punk", "Digital Love");
        tracker.Found("soulseek", "song-1", "Found 2 copies", [new AcquisitionCandidate("Soulseek", Peer: "peer1", Rank: 1), new("Soulseek", Peer: "peer2", Note: "Too short")]);
        tracker.Trying("soulseek", "song-1", new AcquisitionCandidate("Soulseek", Peer: "peer1", Format: "flac"), "Trying peer1", "The best copy");
        history.Record(Entry("Digital Love", "soulseek:song-1", DateTime.UtcNow));
        tracker.Complete("soulseek", "song-1", "nd-1");

        var log = history.GetRecent()[0].Log!;
        Assert.Equal(AcquisitionEventKinds.Done, log[^1].Kind);
        Assert.Contains(log, line => line.Kind == AcquisitionEventKinds.Found && line.Candidates!.Count == 2);

        tracker.Log("soulseek", "song-1", AcquisitionEventKinds.Lyrics, "Found word-timed lyrics");
        Assert.Equal("Found word-timed lyrics", History().GetRecent()[0].Log![^1].Text);

        tracker.Begin("soulseek", "song-2", "song-2", "alice", "Daft Punk", "Aerodynamic");
        tracker.Fail("soulseek", "song-2", "No copy that fits was found.");
        Assert.Single(History().GetRecent());
    }

    // ---- the routes -------------------------------------------------------------------------------

    private sealed class Dashboard : IAsyncDisposable
    {
        private readonly AdminWebFactory _factory = new();
        public WebApplicationFactory<Program> Built { get; }
        public DownloadHistoryService History { get; }
        public LibraryActionJournal Journal { get; } = new();

        public Dashboard(string directory, bool allowed = true, bool dryRun = false)
        {
            History = new DownloadHistoryService(Path.Combine(directory, "history.json"), NullLogger<DownloadHistoryService>.Instance);
            Built = _factory.WithWebHostBuilder(b =>
            {
                b.ConfigureAppConfiguration((_, c) => c.AddInMemoryCollection(new Dictionary<string, string?>
                {
                    ["LibraryActions:Enabled"] = "true",
                    ["LibraryActions:DryRun"] = dryRun ? "true" : "false",
                    ["LibraryActions:AllowedUsers:0"] = allowed ? "admin" : "someone-else",
                    ["LibraryActions:QuarantineRetentionDays"] = "30",
                }));
                b.ConfigureServices(services =>
                {
                    services.RemoveAll<IHostedService>();
                    services.RemoveAll<DownloadHistoryService>();
                    services.AddSingleton(History);
                    services.RemoveAll<LibraryActionJournal>();
                    services.AddSingleton(Journal);
                    services.RemoveAll<BrowseSessionStore>();
                    services.AddSingleton(new BrowseSessionStore());
                });
            });
        }

        public T Get<T>() where T : notnull => Built.Services.GetRequiredService<T>();

        public HttpClient Client(string? who)
        {
            var client = Built.CreateClient();
            client.DefaultRequestHeaders.Add("X-Octo-Admin", "1");
            var sessions = Get<BrowseSessionStore>();
            var token = who switch { null => null, "recovery" => sessions.CreateRecovery(), _ => sessions.Create(who) };
            if (token is not null) client.DefaultRequestHeaders.Add("X-Octo-Browse-Token", token);
            return client;
        }

        public async ValueTask DisposeAsync()
        {
            await Built.DisposeAsync();
            await _factory.DisposeAsync();
        }
    }

    private static readonly string[] Reads = ["/api/admin/downloads/log?key=soulseek:a", "/api/admin/find/abc", "/api/admin/find/outside?q=daft",
        "/api/admin/find/cover?id=nd-1", "/api/admin/trash"];
    private static readonly (string Path, object Body)[] Writes = [("/api/admin/find", new { id = "nd-1" }),
        ("/api/admin/find/abc/pick", new { copy = "c1" }), ("/api/admin/trash/restore", new { id = "nd-1" })];

    [Fact]
    public async Task EveryRouteRefusesNoSignInAndTheRecoveryCode()
    {
        await using var dash = new Dashboard(_directory);
        foreach (var who in new string?[] { null, "recovery" })
        {
            using var client = dash.Client(who);
            foreach (var path in Reads)
                Assert.True((await client.GetAsync(path)).StatusCode == HttpStatusCode.Unauthorized, $"GET {path} as {who ?? "nobody"}");
            foreach (var (path, body) in Writes)
                Assert.True((await client.PostAsJsonAsync(path, body)).StatusCode == HttpStatusCode.Unauthorized, $"POST {path} as {who ?? "nobody"}");
        }
    }

    [Fact]
    public async Task TheListLeavesLogsOut_AndTheLogRouteReadsTheSavedOneOrTheLiveOne()
    {
        await using var dash = new Dashboard(_directory);
        var now = DateTime.UtcNow;
        dash.History.Record(Entry("Old song", null, now.AddDays(-3)));
        dash.History.Record(Entry("Digital Love", "soulseek:saved", now.AddMinutes(-1)));
        dash.History.AttachLog("soulseek:saved", now.AddMinutes(-2), [Line(now, AcquisitionEventKinds.Found, "Found 3 copies", copies: 3)]);
        var tracker = dash.Get<AcquisitionTracker>();
        tracker.Begin("soulseek", "live", "live", "alice", "Daft Punk", "One More Time");
        tracker.Stage("soulseek", "live", AcquisitionState.Searching, "Soulseek");
        using var client = dash.Client("admin");

        using (var list = JsonDocument.Parse(await client.GetStringAsync("/api/admin/downloads")))
        {
            var rows = list.RootElement.GetProperty("downloads").EnumerateArray().ToList();
            Assert.False(rows[0].TryGetProperty("log", out _));
            Assert.True(rows[0].GetProperty("hasLog").GetBoolean());
            Assert.Equal("soulseek:saved", rows[0].GetProperty("key").GetString());
            Assert.False(rows[1].GetProperty("hasLog").GetBoolean());
        }

        var savedAt = dash.History.GetRecent()[0].DownloadedAt;
        using (var saved = JsonDocument.Parse(await client.GetStringAsync($"/api/admin/downloads/log?key=soulseek:saved&at={Uri.EscapeDataString(savedAt)}")))
        {
            Assert.Equal("saved", saved.RootElement.GetProperty("kept").GetString());
            var line = Assert.Single(saved.RootElement.GetProperty("event").EnumerateArray());
            Assert.Equal(3, line.GetProperty("candidate").GetArrayLength());
            Assert.Equal("peer1", line.GetProperty("candidate")[0].GetProperty("peer").GetString());
        }
        using (var live = JsonDocument.Parse(await client.GetStringAsync("/api/admin/downloads/log?key=soulseek:live")))
        {
            Assert.Equal("live", live.RootElement.GetProperty("kept").GetString());
            Assert.Equal("searching", live.RootElement.GetProperty("row").GetProperty("state").GetString());
            Assert.Contains(live.RootElement.GetProperty("event").EnumerateArray(), e => e.GetProperty("text").GetString() == "Looking on Soulseek");
        }
        using (var none = JsonDocument.Parse(await client.GetStringAsync("/api/admin/downloads/log?key=soulseek:gone&at=2020-01-01T00:00:00Z")))
            Assert.Equal("none", none.RootElement.GetProperty("kept").GetString());
    }

    [Fact]
    public async Task FindSongsRunsAsTheSignedInAdmin_AndOnlyTheyCanFollowOrPickFromIt()
    {
        await using var dash = new Dashboard(_directory);
        var finder = dash.Get<SongFinder>();
        var pickedBy = new List<(string User, string Peer)>();
        finder.Resolve = (id, _) => Task.FromResult<FindTarget?>(id switch
        {
            "nd-1" => new FindTarget("Daft Punk", "Digital Love", "Discovery", 301, "nd-1", LibraryId: "nd-1", OwnedFormat: "mp3"),
            "soulseek:ext-1" => new FindTarget("Daft Punk", "Digital Love", "Discovery", 301, "ext-1", ExternalId: "ext-1"),
            _ => null,
        });
        finder.SourcesFor = _ => new Dictionary<string, string?> { [SongFinder.SoulseekSource] = null };
        finder.SearchSoulseek = (_, _) => Task.FromResult<(IReadOnlyList<FoundCopy>, IReadOnlyList<string>, string?)>((
            [new FoundCopy(new AcquisitionCandidate("Soulseek", Peer: "peer1", File: "/share/Digital Love.flac", Format: "flac", BitDepth: 16, SampleRate: 44100, Rank: 1),
                new PickedCopy { Peer = "peer1", File = "/share/Digital Love.flac", Format = "flac" })],
            ["Daft Punk Digital Love"], "1 file from 1 peer; 1 fits the song"));
        finder.Fetch = (_, copy, user) =>
        {
            pickedBy.Add((user, copy.Pick.Peer!));
            return Task.FromResult(new PickOutcome(LibraryActionStates.Queued, "Getting the copy you picked."));
        };
        using var admin = dash.Client("admin");
        using var other = dash.Client("other-admin");

        Assert.Equal(HttpStatusCode.NotFound, (await admin.PostAsJsonAsync("/api/admin/find", new { id = "nope" })).StatusCode);
        // A library song's copy would replace its file, which needs Better quality on for this admin.
        using (var library = JsonDocument.Parse(await (await admin.PostAsJsonAsync("/api/admin/find", new { id = "nd-1" })).Content.ReadAsStringAsync()))
        {
            Assert.Equal("failed", library.RootElement.GetProperty("state").GetString());
            Assert.Contains("Better quality", library.RootElement.GetProperty("error").GetString());
        }
        var started = await admin.PostAsJsonAsync("/api/admin/find", new { id = "soulseek:ext-1" });
        Assert.Equal(HttpStatusCode.OK, started.StatusCode);
        var id = JsonDocument.Parse(await started.Content.ReadAsStringAsync()).RootElement.GetProperty("id").GetString()!;
        await finder.WaitAsync(id, TimeSpan.FromSeconds(5));

        using (var found = JsonDocument.Parse(await admin.GetStringAsync($"/api/admin/find/{id}")))
        {
            Assert.Equal("done", found.RootElement.GetProperty("state").GetString());
            Assert.Equal(JsonValueKind.Null, found.RootElement.GetProperty("song").GetProperty("libraryId").ValueKind);
            var copy = Assert.Single(found.RootElement.GetProperty("candidate").EnumerateArray());
            Assert.Equal("c1", copy.GetProperty("id").GetString());
            Assert.Equal("FLAC 16-bit 44.1 kHz", copy.GetProperty("quality").GetString());
        }
        // Another admin's search is not theirs to follow or pick from.
        Assert.Equal(HttpStatusCode.NotFound, (await other.GetAsync($"/api/admin/find/{id}")).StatusCode);
        using (var refused = JsonDocument.Parse(await (await other.PostAsJsonAsync($"/api/admin/find/{id}/pick", new { copy = "c1" })).Content.ReadAsStringAsync()))
            Assert.Equal("skipped", refused.RootElement.GetProperty("state").GetString());

        using (var picked = JsonDocument.Parse(await (await admin.PostAsJsonAsync($"/api/admin/find/{id}/pick", new { copy = "c1" })).Content.ReadAsStringAsync()))
            Assert.Equal("queued", picked.RootElement.GetProperty("state").GetString());
        Assert.Equal([("admin", "peer1")], pickedBy);
        Assert.Equal(HttpStatusCode.BadRequest, (await admin.PostAsJsonAsync($"/api/admin/find/{id}/pick", new { copy = "" })).StatusCode);

        // The admin's own progress card, and nobody else's.
        var item = ActivityController.Find(finder.ForUser("admin"), DateTime.UtcNow)!;
        Assert.Equal("done", item.State);
        Assert.Equal("fetched", item.Page);
        Assert.Contains("1 copy found", item.Detail);
        Assert.Null(ActivityController.Find(finder.ForUser("other-admin"), DateTime.UtcNow));
    }

    [Fact]
    public async Task AFindStartedWhileTwoRunAnswersWhy()
    {
        await using var dash = new Dashboard(_directory);
        var finder = dash.Get<SongFinder>();
        var hold = new TaskCompletionSource();
        finder.Resolve = (id, _) => Task.FromResult<FindTarget?>(new FindTarget("Daft Punk", id, null, null, id, ExternalId: id));
        finder.SourcesFor = _ => new Dictionary<string, string?> { [SongFinder.SoulseekSource] = null };
        finder.SearchSoulseek = async (_, _) =>
        {
            await hold.Task;
            return ([], [], "Nothing found");
        };
        using var admin = dash.Client("admin");
        try
        {
            await admin.PostAsJsonAsync("/api/admin/find", new { id = "one" });
            await admin.PostAsJsonAsync("/api/admin/find", new { id = "two" });
            using var third = JsonDocument.Parse(await (await admin.PostAsJsonAsync("/api/admin/find", new { id = "three" })).Content.ReadAsStringAsync());
            Assert.Equal("failed", third.RootElement.GetProperty("state").GetString());
            Assert.Contains("already have", third.RootElement.GetProperty("error").GetString());
            Assert.Equal("running", ActivityController.Find(finder.ForUser("admin"), DateTime.UtcNow)!.State);
        }
        finally { hold.SetResult(); }
    }

    private (LibraryActionEntry Entry, string Original, string Trashed) Trashed(string musicRoot, string id, bool withLyrics)
    {
        var original = Path.Combine(musicRoot, "Daft Punk", "Discovery", $"{id}.flac");
        var trashDir = Path.Combine(musicRoot, ".octo-trash", "2026-10-04");
        Directory.CreateDirectory(trashDir);
        var trashed = Path.Combine(trashDir, $"{id}.flac");
        File.WriteAllText(trashed, "audio");
        var sidecars = new List<QuarantinedSidecar>();
        if (withLyrics)
        {
            var lrc = Path.Combine(trashDir, $"{id}.lrc");
            File.WriteAllText(lrc, "[00:01.00]words");
            sidecars.Add(new QuarantinedSidecar(Path.ChangeExtension(original, ".lrc"), lrc));
        }
        File.WriteAllText(trashed + ".octo-action.json", JsonSerializer.Serialize(
            new QuarantineManifest(original, id, "Delete", "alice", DateTime.UtcNow, sidecars)));
        var entry = new LibraryActionEntry(LibraryActionJournal.MakeKey(LibraryAction.Delete, id, "1:2"), LibraryAction.Delete, id, "alice",
            "Digital Love", "Daft Punk", "Discovery", original, trashed, PathSource.NativeApi,
            LibraryActionState.Applied, "Removed.", false, DateTime.UtcNow.AddDays(-2));
        return (entry, original, trashed);
    }

    [Fact]
    public async Task RecentlyRemovedListsTheTrash_AndPutBackRestoresTheSongWithItsLyrics()
    {
        await using var dash = new Dashboard(_directory);
        var root = dash.Get<NavidromeSongPathResolver>().MusicRoot();
        var (entry, original, trashed) = Trashed(root, "nd-1", withLyrics: true);
        dash.Journal.Record(entry);
        using var admin = dash.Client("admin");

        using (var list = JsonDocument.Parse(await admin.GetStringAsync("/api/admin/trash")))
        {
            Assert.Equal(30, list.RootElement.GetProperty("keepDays").GetInt32());
            var song = Assert.Single(list.RootElement.GetProperty("songs").EnumerateArray());
            Assert.Equal("nd-1", song.GetProperty("id").GetString());
            Assert.Equal("alice", song.GetProperty("removedBy").GetString());
            Assert.Equal(["nd-1.lrc"], song.GetProperty("sidecars").EnumerateArray().Select(s => s.GetString()));
            Assert.True(song.GetProperty("blocksDownloads").GetBoolean());
            Assert.Equal(entry.Key, song.GetProperty("key").GetString());
            Assert.True(song.TryGetProperty("goneAt", out var gone) && gone.ValueKind == JsonValueKind.String);
        }

        using (var restored = JsonDocument.Parse(await (await admin.PostAsJsonAsync("/api/admin/trash/restore", new { id = "nd-1" })).Content.ReadAsStringAsync()))
            Assert.Equal("applied", restored.RootElement.GetProperty("state").GetString());
        Assert.True(File.Exists(original));
        Assert.True(File.Exists(Path.ChangeExtension(original, ".lrc")));
        Assert.False(File.Exists(trashed));
        using (var empty = JsonDocument.Parse(await admin.GetStringAsync("/api/admin/trash")))
            Assert.Empty(empty.RootElement.GetProperty("songs").EnumerateArray());
        Assert.Contains("admin", dash.Journal.Recent(10).Single(e => e.Key == entry.Key).Detail);
    }

    [Fact]
    public async Task AnAdminOffTheAllowedListSeesWhyAndChangesNothing()
    {
        await using var dash = new Dashboard(_directory, allowed: false);
        var root = dash.Get<NavidromeSongPathResolver>().MusicRoot();
        var (entry, original, trashed) = Trashed(root, "nd-2", withLyrics: false);
        dash.Journal.Record(entry);
        using var admin = dash.Client("admin");

        var list = await admin.GetAsync("/api/admin/trash");
        Assert.Equal(HttpStatusCode.Forbidden, list.StatusCode);
        Assert.Contains("allowed list", await list.Content.ReadAsStringAsync());
        Assert.Equal(HttpStatusCode.Forbidden, (await admin.PostAsJsonAsync("/api/admin/trash/restore", new { id = "nd-2" })).StatusCode);
        Assert.True(File.Exists(trashed));
        Assert.False(File.Exists(original));
    }

    [Fact]
    public async Task WhileDryRunIsOnPutBackOnlySaysWhatItWouldDo()
    {
        await using var dash = new Dashboard(_directory, dryRun: true);
        var root = dash.Get<NavidromeSongPathResolver>().MusicRoot();
        var (entry, original, trashed) = Trashed(root, "nd-3", withLyrics: false);
        dash.Journal.Record(entry);
        using var admin = dash.Client("admin");

        using var restored = JsonDocument.Parse(await (await admin.PostAsJsonAsync("/api/admin/trash/restore", new { id = "nd-3" })).Content.ReadAsStringAsync());
        Assert.Equal("rehearsed", restored.RootElement.GetProperty("state").GetString());
        Assert.True(File.Exists(trashed));
        Assert.False(File.Exists(original));
    }
}
