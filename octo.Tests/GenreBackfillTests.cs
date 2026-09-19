using System.Net;
using System.Net.Http.Json;
using System.Text.Json;
using Microsoft.AspNetCore.Hosting;
using Microsoft.Extensions.Configuration;
using Microsoft.AspNetCore.Mvc.Testing;
using Microsoft.Extensions.Hosting;
using Octo.Services.Metadata;

namespace Octo.Tests;

/// <summary>
/// The backfill rewrites tags across files Octo may not have created, and /api/admin has no
/// authentication of its own. The state file and the journal are what make that recoverable,
/// so they are what these tests pin.
/// </summary>
public class GenreBackfillStoreTests
{
    private static string TempPath() =>
        Path.Combine(Path.GetTempPath(), "octo-backfill-" + Guid.NewGuid() + ".json");

    [Fact]
    public void Run_SurvivesARestart()
    {
        var path = TempPath();
        try
        {
            using (var first = new GenreBackfillStore(path))
            {
                first.Replace(new GenreBackfillRun
                {
                    RunId = "abc123",
                    Status = GenreBackfillStatus.Completed,
                    Total = 10,
                    Cursor = 10,
                    Changed = 4,
                });
            }

            using var second = new GenreBackfillStore(path);
            Assert.Equal("abc123", second.Current.RunId);
            Assert.Equal(4, second.Current.Changed);
        }
        finally { try { File.Delete(path); } catch { } }
    }

    /// <summary>
    /// A run that was going when the process stopped must NOT auto-resume: the user may have
    /// restarted Octo specifically to stop a mass rewrite.
    /// </summary>
    [Fact]
    public void Load_RunningBecomesInterruptedAndDoesNotRestartItself()
    {
        var path = TempPath();
        try
        {
            using (var first = new GenreBackfillStore(path))
            {
                first.Replace(new GenreBackfillRun
                {
                    RunId = "abc123",
                    Status = GenreBackfillStatus.Running,
                    Total = 100,
                    Cursor = 42,
                    Queue = Enumerable.Range(0, 100).Select(i => $"/music/{i}.flac").ToList(),
                });
            }

            using var second = new GenreBackfillStore(path);
            Assert.Equal(GenreBackfillStatus.Interrupted, second.Current.Status);
            Assert.True(second.Current.CanResume);
            Assert.Equal(42, second.Current.Cursor);
        }
        finally { try { File.Delete(path); } catch { } }
    }

    [Fact]
    public void CanResume_IsFalseOnceTheQueueIsExhausted()
    {
        using var store = new GenreBackfillStore();
        store.Replace(new GenreBackfillRun
        {
            Status = GenreBackfillStatus.Cancelled,
            Cursor = 3,
            Queue = ["a", "b", "c"],
        });

        Assert.False(store.Current.CanResume);
    }

    /// <summary>
    /// A whole-library preview would otherwise grow the state file without limit, and the state
    /// file is flushed repeatedly during a run.
    /// </summary>
    [Fact]
    public void Update_BoundsThePreviewAndTheErrorList()
    {
        using var store = new GenreBackfillStore();
        store.Update(run =>
        {
            for (var i = 0; i < GenreBackfillStore.MaxPreviewRows + 50; i++)
                run.Preview.Add(new GenreBackfillChange($"/music/{i}.flac", ["Music"], ["Pop"], "Write", null));
            for (var i = 0; i < 100; i++) run.Errors.Add($"error {i}");
        });

        Assert.Equal(GenreBackfillStore.MaxPreviewRows, store.Current.Preview.Count);
        Assert.Equal(20, store.Current.Errors.Count);
        Assert.Equal("error 99", store.Current.Errors[^1]);
    }

    /// <summary>A state file that will not parse is a cold start, not a failure to boot.</summary>
    [Fact]
    public void Load_UnreadableFile_StartsIdle()
    {
        var path = TempPath();
        File.WriteAllText(path, "{ not the shape we wrote");
        try
        {
            using var store = new GenreBackfillStore(path);
            Assert.Equal(GenreBackfillStatus.Idle, store.Current.Status);
        }
        finally { try { File.Delete(path); } catch { } }
    }
}

public class GenreBackfillJournalTests
{
    private static string TempPath() =>
        Path.Combine(Path.GetTempPath(), "octo-journal-" + Guid.NewGuid() + ".jsonl");

    [Fact]
    public void ReadAll_ReturnsNewestFirst()
    {
        var path = TempPath();
        try
        {
            var journal = new GenreBackfillJournal(path);
            journal.Append(new GenreJournalEntry("/music/a.flac", ["Music"], ["Pop"], DateTime.UtcNow, "r1"));
            journal.Append(new GenreJournalEntry("/music/b.flac", [], ["Rock"], DateTime.UtcNow, "r1"));

            var entries = journal.ReadAll();
            Assert.Equal(2, entries.Count);
            // Newest first, so an undo replaying in order applies the OLDEST entry last and a
            // file changed twice ends up with the frame it originally had.
            Assert.Equal("/music/b.flac", entries[0].Path);
            Assert.Equal("/music/a.flac", entries[1].Path);
            Assert.Equal(["Music"], entries[1].Before);
        }
        finally { try { File.Delete(path); } catch { } }
    }

    /// <summary>
    /// A run killed mid-append leaves a partial last line. Skipping it costs the undo for one
    /// file; refusing to parse the file would cost the undo for all of them.
    /// </summary>
    [Fact]
    public void ReadAll_TornFinalLine_IsSkippedNotFatal()
    {
        var path = TempPath();
        try
        {
            var journal = new GenreBackfillJournal(path);
            journal.Append(new GenreJournalEntry("/music/a.flac", ["Music"], ["Pop"], DateTime.UtcNow, "r1"));
            File.AppendAllText(path, "{\"p\":\"/music/b.flac\",\"b\":[\"Ro");

            var entries = journal.ReadAll();
            Assert.Equal("/music/a.flac", Assert.Single(entries).Path);
        }
        finally { try { File.Delete(path); } catch { } }
    }

    [Fact]
    public void Clear_RemovesTheJournalAndWithItTheUndo()
    {
        var path = TempPath();
        try
        {
            var journal = new GenreBackfillJournal(path);
            journal.Append(new GenreJournalEntry("/music/a.flac", ["Music"], ["Pop"], DateTime.UtcNow, "r1"));
            Assert.True(journal.Exists);

            journal.Clear();
            Assert.False(journal.Exists);
            Assert.Empty(journal.ReadAll());
        }
        finally { try { File.Delete(path); } catch { } }
    }

    [Fact]
    public void ReadAll_NoJournal_IsEmptyRatherThanThrowing()
        => Assert.Empty(new GenreBackfillJournal(TempPath()).ReadAll());

    /// <summary>An empty Before means the file had no genre frame, so undo clears it.</summary>
    [Fact]
    public void Entry_EmptyBefore_RoundTrips()
    {
        var path = TempPath();
        try
        {
            var journal = new GenreBackfillJournal(path);
            journal.Append(new GenreJournalEntry("/music/a.flac", [], ["Pop"], DateTime.UtcNow, "r1"));

            Assert.Empty(Assert.Single(journal.ReadAll()).Before);
        }
        finally { try { File.Delete(path); } catch { } }
    }
}

/// <summary>
/// /api/admin has no authentication at all, so a button that rewrites every tag in a music
/// library cannot be the second unauthenticated destructive surface. Every backfill endpoint
/// is gated on a verified Navidrome admin session.
/// </summary>
public class GenreBackfillEndpointTests
{
    public static TheoryData<string, string> Endpoints => new()
    {
        { "POST", "/api/admin/genre/backfill" },
        { "GET", "/api/admin/genre/backfill" },
        { "POST", "/api/admin/genre/backfill/cancel" },
        { "POST", "/api/admin/genre/backfill/resume" },
        { "POST", "/api/admin/genre/backfill/undo" },
        // Library actions list filenames and usernames, and the resolver answers with a real
        // path, so both are gated the same way.
    };

    [Theory]
    [MemberData(nameof(Endpoints))]
    public async Task EveryBackfillEndpoint_RequiresABrowseSession(string method, string url)
    {
        using var factory = new AdminWebFactory();
        using var client = factory.CreateClient();
        using var request = new HttpRequestMessage(new HttpMethod(method), url);
        if (method == "POST")
            request.Content = JsonContent.Create(new { scope = "OctoDownloads", dryRun = true });

        using var response = await client.SendAsync(request);
        Assert.Equal(HttpStatusCode.Unauthorized, response.StatusCode);
    }

    /// <summary>The preset is read-only and has no destructive surface, so it stays open the
    /// way the rest of the settings API is.</summary>
    [Fact]
    public async Task GenrePresets_IsReadableWithoutASession()
    {
        using var factory = new AdminWebFactory();
        using var client = factory.CreateClient();
        using var response = await client.GetAsync("/api/admin/genre/presets");

        Assert.Equal(HttpStatusCode.OK, response.StatusCode);
        Assert.Contains("Hip-Hop", await response.Content.ReadAsStringAsync());
    }

    /// <summary>
    /// The preset is fetched and posted straight back to the settings API, so it has to come out
    /// in the shape that API reads: exact PascalCase keys and the match mode as its NAME. The
    /// default serializer camelCases and writes the enum as a number, which made the shipped
    /// preset impossible to save.
    /// </summary>
    [Fact]
    public async Task GenrePresets_ComeBackInTheShapeTheSettingsApiAccepts()
    {
        using var factory = new AdminWebFactory();
        using var client = factory.CreateClient();

        var body = await client.GetFromJsonAsync<JsonElement>("/api/admin/genre/presets");
        var first = body.GetProperty("broad").EnumerateArray().First();

        Assert.True(first.TryGetProperty("Pattern", out _), "Pattern must be PascalCase");
        Assert.True(first.TryGetProperty("Genre", out _), "Genre must be PascalCase");
        Assert.True(first.TryGetProperty("Enabled", out _), "Enabled must be PascalCase");
        Assert.Equal(JsonValueKind.String, first.GetProperty("Match").ValueKind);
        Assert.Equal("Contains", first.GetProperty("Match").GetString());
    }
}

internal sealed class AdminWebFactory : WebApplicationFactory<Program>
{
    private readonly string _directory =
        Path.Combine(Path.GetTempPath(), "octo-admin-web-" + Guid.NewGuid());

    protected override IHost CreateHost(IHostBuilder builder)
    {
        Directory.CreateDirectory(_directory);
        builder.UseEnvironment("Development");
        builder.ConfigureHostConfiguration(config => config.AddInMemoryCollection(new Dictionary<string, string?>
        {
            ["Library:DownloadPath"] = _directory,
        }));
        return base.CreateHost(builder);
    }

    protected override void Dispose(bool disposing)
    {
        base.Dispose(disposing);
        try { Directory.Delete(_directory, true); } catch { }
    }
}
