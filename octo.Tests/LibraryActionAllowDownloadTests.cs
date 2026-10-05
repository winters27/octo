using System.Net;
using System.Net.Http.Json;
using System.Text.Json;
using Microsoft.AspNetCore.Hosting;
using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.DependencyInjection.Extensions;
using Microsoft.Extensions.Hosting;
using Octo.Models.Settings;
using Octo.Services.Admin;
using Octo.Services.Library;

namespace Octo.Tests;

/// <summary>
/// A song removed with Delete from disk is not downloaded again. The download's log used to
/// say "clear that entry from the dashboard", which had no control for it; the Library actions
/// history now has Allow downloading again.
/// </summary>
public sealed class LibraryActionAllowDownloadTests
{
    private static LibraryActionEntry Removed(string id, string artist, string title, bool copy = false) =>
        new(LibraryActionJournal.MakeKey(LibraryAction.Delete, id, "1:2"), LibraryAction.Delete, id, "alice",
            title, artist, "Discovery", $"/music/{id}.flac", $"/music/.octo-trash/{id}.flac", PathSource.NativeApi,
            LibraryActionState.Applied, "Removed.", false, DateTime.UtcNow) { Copy = copy };

    [Fact]
    public void AllowingASongLiftsEveryRemovalOfIt_AndOnlyIt()
    {
        var journal = new LibraryActionJournal();
        var first = Removed("nd-1", "Daft Punk", "Digital Love");
        journal.Record(first);
        journal.Record(Removed("nd-2", "Daft Punk", "Digital Love"));
        journal.Record(Removed("nd-3", "Daft Punk", "Aerodynamic"));
        Assert.True(journal.IsNeverRequested("Daft Punk", "Digital Love"));

        Assert.Equal(2, journal.AllowDownloads(first.Key));

        Assert.False(journal.IsNeverRequested("Daft Punk", "Digital Love"));
        Assert.True(journal.IsNeverRequested("Daft Punk", "Aerodynamic"));
        Assert.Equal(0, journal.AllowDownloads(first.Key));
    }

    [Fact]
    public async Task TheHistoryOffersAllowDownloadingAgain_AndItWorksOnlyWhenSignedIn()
    {
        var journal = new LibraryActionJournal();
        var removed = Removed("nd-1", "Daft Punk", "Digital Love");
        journal.Record(removed);
        journal.Record(Removed("nd-2", "Daft Punk", "Aerodynamic", copy: true));
        await using var factory = new AdminWebFactory();
        await using var built = factory.WithWebHostBuilder(builder => builder.ConfigureServices(services =>
        {
            services.RemoveAll<IHostedService>();
            services.RemoveAll<LibraryActionJournal>();
            services.AddSingleton(journal);
        }));
        var token = built.Services.GetRequiredService<BrowseSessionStore>().Create("admin");
        using var anonymous = built.CreateClient();
        anonymous.DefaultRequestHeaders.Add("X-Octo-Admin", "1");
        using var signedIn = built.CreateClient();
        signedIn.DefaultRequestHeaders.Add("X-Octo-Admin", "1");
        signedIn.DefaultRequestHeaders.Add("X-Octo-Browse-Token", token);

        using (var doc = JsonDocument.Parse(await signedIn.GetStringAsync("/api/admin/library-actions")))
        {
            var rows = doc.RootElement.GetProperty("entries").EnumerateArray().ToList();
            var blocking = rows.Single(row => row.GetProperty("navidromeId").GetString() == "nd-1");
            Assert.True(blocking.GetProperty("blocksDownloads").GetBoolean());
            Assert.Equal(removed.Key, blocking.GetProperty("key").GetString());
            // A second copy removed never kept the song from downloading.
            Assert.False(rows.Single(row => row.GetProperty("navidromeId").GetString() == "nd-2").GetProperty("blocksDownloads").GetBoolean());
        }

        Assert.Equal(HttpStatusCode.Unauthorized,
            (await anonymous.PostAsJsonAsync("/api/admin/library-actions/allow-download", new { key = removed.Key })).StatusCode);
        Assert.True(journal.IsNeverRequested("Daft Punk", "Digital Love"));

        var allowed = await signedIn.PostAsJsonAsync("/api/admin/library-actions/allow-download", new { key = removed.Key });

        Assert.Equal(HttpStatusCode.OK, allowed.StatusCode);
        Assert.False(journal.IsNeverRequested("Daft Punk", "Digital Love"));
        Assert.Equal(HttpStatusCode.BadRequest,
            (await signedIn.PostAsJsonAsync("/api/admin/library-actions/allow-download", new { key = removed.Key })).StatusCode);
    }
}
