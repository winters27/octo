using System.Text.Json;
using System.Text.Json.Nodes;
using Microsoft.AspNetCore.Http;
using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.Logging.Abstractions;
using Microsoft.Extensions.Options;
using Octo.Models.Settings;
using Octo.Services.Library;
using Octo.Services.Metadata;
using Octo.Services.Soulseek;
using Octo.Services.Subsonic;

namespace Octo.Tests;

/// <summary>
/// Made for you: New Releases, Rediscover and Deep Cuts. The picking is what a listener hears,
/// so it is tested directly; then the catalog side of New Releases against a stand-in Deezer,
/// and the whole refresh against a stand-in Navidrome and Deezer together.
/// </summary>
public class ForYouListsTests
{
    private static readonly DateTime Now = new(2026, 10, 4, 12, 0, 0, DateTimeKind.Utc);

    private static JsonObject Song(string id, string artist, int plays = 0, string? played = null, bool starred = false,
        int rating = 0, string? title = null, string? artistId = null) => new()
    {
        ["id"] = id, ["title"] = title ?? $"Song {id}", ["artist"] = artist, ["artistId"] = artistId ?? "ar-" + artist,
        ["playCount"] = plays, ["played"] = played, ["starred"] = starred ? "2025-01-01T00:00:00Z" : null,
        ["userRating"] = rating, ["duration"] = 200,
    };

    // ---- top artists ------------------------------------------------------------------------

    [Fact]
    public void TopArtists_ArePlaysSummed_AndVariousArtistsIsNobody()
    {
        var songs = new[]
        {
            Song("1", "Big", plays: 10), Song("2", "Big", plays: 10), Song("3", "Small", plays: 5),
            Song("4", "Various Artists", plays: 100), Song("5", "Quiet"),
        };

        var top = ForYouLists.TopArtists(songs, 10);

        Assert.Equal(["Big", "Small", "Quiet"], top.Select(artist => artist.Name));
        Assert.Equal(20, top[0].Plays);
    }

    [Fact]
    public void TopArtists_WithNoPlaysAtAll_AreReadByHeartsThenBySongs()
    {
        var songs = new[]
        {
            Song("1", "Kept"), Song("2", "Kept"), Song("3", "Kept"),
            Song("4", "Loved", starred: true), Song("5", "Neither"),
        };

        Assert.Equal(["Loved", "Kept", "Neither"], ForYouLists.TopArtists(songs, 10).Select(artist => artist.Name));
    }

    // ---- Rediscover -------------------------------------------------------------------------

    [Fact]
    public void Rediscover_IsLovedSongsNotPlayedForMonths_NeverOnesWithNoDate()
    {
        var old = Now.AddMonths(-8).ToString("O");
        var recent = Now.AddMonths(-1).ToString("O");
        var songs = new[]
        {
            Song("often", "A", plays: 5, played: old),
            Song("hearted", "B", plays: 1, played: old, starred: true),
            Song("rated", "C", plays: 1, played: old, rating: 4),
            Song("liked-but-recent", "D", plays: 9, played: recent),
            Song("not-loved", "E", plays: 1, played: old),
            Song("no-date", "F", plays: 9),
        };

        var picked = ForYouLists.Rediscover(songs, Now, 6, seed: 1).Select(song => song["id"]!.GetValue<string>()).ToHashSet();

        Assert.Equal(new HashSet<string> { "often", "hearted", "rated" }, picked);
    }

    [Fact]
    public void Rediscover_HoldsStillForASeed_AndNeverMoreThanThreeByOneArtist()
    {
        var old = Now.AddYears(-1).ToString("O");
        var songs = Enumerable.Range(0, 40).Select(i => Song($"s{i}", i < 20 ? "Same" : $"Artist {i}", plays: 5, played: old)).ToList();

        var first = ForYouLists.Rediscover(songs, Now, 6, seed: 7);
        var again = ForYouLists.Rediscover(songs, Now, 6, seed: 7);

        Assert.Equal(first.Select(song => song.ToJsonString()), again.Select(song => song.ToJsonString()));
        Assert.True(first.Count(song => song["artist"]!.GetValue<string>() == "Same") <= ForYouLists.RediscoverPerArtist);
        Assert.Equal(23, first.Count);
    }

    // ---- Deep Cuts --------------------------------------------------------------------------

    [Fact]
    public void DeepCuts_ArePlayedAtMostOnce_FromTheTopArtists_AndNotInRediscover()
    {
        var songs = new[]
        {
            Song("never", "Top"), Song("once", "Top", plays: 1), Song("twice", "Top", plays: 2),
            Song("other", "Elsewhere"), Song("taken", "Top"),
        };

        var picked = ForYouLists.DeepCuts(songs, ["ar-Top"], new HashSet<string> { "taken" }, seed: 3)
            .Select(song => song["id"]!.GetValue<string>()).ToHashSet();

        Assert.Equal(new HashSet<string> { "never", "once" }, picked);
    }

    // ---- New Releases picking ---------------------------------------------------------------

    private static DeezerMetadataService.AlbumHit Release(string id, string type, string? date, string title = "Title") =>
        new(id, title, "Artist", "https://cdn.test/" + id + ".jpg", null, 10, type, date);

    [Fact]
    public void RecentReleases_AreTheLastWeeks_NewestFirst_NoCompilationsNoFutureNoUndated()
    {
        var today = new DateOnly(2026, 10, 4);
        var releases = new[]
        {
            Release("old", "album", "2026-07-01"),
            Release("album", "album", "2026-09-20"),
            Release("single", "single", "2026-10-02"),
            Release("ep", "ep", "2026-08-15"),
            Release("compile", "compile", "2026-09-30"),
            Release("future", "album", "2026-11-01"),
            Release("undated", "album", null),
        };

        var recent = ForYouLists.RecentReleases(releases, today, weeks: 8);

        Assert.Equal(["single", "album", "ep"], recent.Select(release => release.DeezerId));
    }

    private static DeezerMetadataService.AlbumTrack Track(int position, int? rank, string title = "") =>
        new(title.Length > 0 ? title : $"Track {position}", "Artist", 200, position, 1, null, rank);

    [Fact]
    public void PickTracks_AnAlbumGivesItsBestByRank_InAlbumOrder_ASingleItsFirst()
    {
        var album = new[] { Track(1, 10), Track(2, 900), Track(3, 50), Track(4, 800), Track(5, 700) };

        Assert.Equal([2, 4, 5], ForYouLists.PickTracks("album", album).Select(track => track.TrackPosition!.Value));
        Assert.Equal([1, 2, 3], ForYouLists.PickTracks("single", album).Select(track => track.TrackPosition!.Value));
        Assert.Equal([1, 2, 3], ForYouLists.PickTracks("album",
            album.Select(track => track with { Rank = null }).ToList()).Select(track => track.TrackPosition!.Value));
    }
}

/// <summary>
/// A stand-in Deezer and Navidrome in one: an artist with releases old, new and still to come,
/// and a library whose listener plays that artist most.
/// </summary>
internal sealed class ForYouUpstream : HttpMessageHandler
{
    public static string Day(int daysAgo) => DateTime.UtcNow.AddDays(-daysAgo).ToString("yyyy-MM-dd");

    public List<string> Calls { get; } = [];
    public bool CatalogDown { get; set; }

    protected override Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken ct)
    {
        var uri = request.RequestUri!;
        var path = uri.AbsolutePath.Trim('/');
        lock (Calls) Calls.Add(uri.Host + "/" + path);
        if (uri.Host != "navidrome.test" && CatalogDown)
            return Task.FromResult(new HttpResponseMessage(System.Net.HttpStatusCode.TooManyRequests));
        var body = (uri.Host, path) switch
        {
            ("navidrome.test", "rest/search3") => Navidrome(uri.Query),
            ("navidrome.test", "rest/ping" or "rest/ping.view") => Plain(uri.Query, ""),
            ("navidrome.test", "rest/getPlaylists" or "rest/getPlaylists.view") => Plain(uri.Query, "playlists"),
            (_, "search/artist") => """{"data":[{"id":7,"name":"Big Artist","nb_fan":900},{"id":8,"name":"Big Artist Tribute","nb_fan":5000}]}""",
            (_, "artist/7/albums") => "{\"data\":["
                + Album(100, "New Album", "album", Day(10), 4, "https://cdn.test/100.jpg") + ","
                + Album(101, "New Single", "single", Day(3), 1, "https://cdn.test/101.jpg") + ","
                + Album(102, "Old Album", "album", Day(400), 9, null) + ","
                + Album(103, "Coming Soon", "album", Day(-30), 9, null) + "],\"total\":4}",
            (_, "album/100") => """{"id":100,"title":"New Album","record_type":"album","nb_tracks":4,"artist":{"name":"Big Artist"}}""",
            (_, "album/100/tracks") => "{\"data\":["
                + AlbumTrack("Opener", 180, 1, 100) + "," + AlbumTrack("Hit", 200, 2, 900) + ","
                + AlbumTrack("Owned Song", 210, 3, 800) + "," + AlbumTrack("Closer", 240, 4, 700) + "],\"total\":4}",
            (_, "album/101") => """{"id":101,"title":"New Single","record_type":"single","nb_tracks":1,"artist":{"name":"Big Artist"}}""",
            (_, "album/101/tracks") => "{\"data\":[" + AlbumTrack("Single Song", 190, 1, 500) + "],\"total\":1}",
            _ => """{"data":[]}""",
        };
        return Task.FromResult(ReviewFixtures.Json(body));
    }

    // An empty Subsonic answer, in the format asked for, holding the element named (if any).
    private static string Plain(string query, string element)
    {
        var json = System.Web.HttpUtility.ParseQueryString(query)["f"] == "json";
        if (json)
            return "{\"subsonic-response\":{\"status\":\"ok\",\"version\":\"1.16.1\""
                + (element.Length > 0 ? ",\"" + element + "\":{}" : "") + "}}";
        return "<subsonic-response xmlns=\"http://subsonic.org/restapi\" status=\"ok\" version=\"1.16.1\">"
            + (element.Length > 0 ? "<" + element + "/>" : "") + "</subsonic-response>";
    }

    private static string Album(int id, string title, string type, string date, int tracks, string? cover) =>
        $$"""{"id":{{id}},"title":"{{title}}","record_type":"{{type}}","release_date":"{{date}}","nb_tracks":{{tracks}}{{(cover is null ? "" : $",\"cover_xl\":\"{cover}\"")}}}""";

    private static string AlbumTrack(string title, int duration, int position, int rank) =>
        "{\"title\":\"" + title + "\",\"duration\":" + duration + ",\"track_position\":" + position
        + ",\"disk_number\":1,\"rank\":" + rank + ",\"artist\":{\"name\":\"Big Artist\"}}";

    private static string Navidrome(string query)
    {
        var offset = System.Web.HttpUtility.ParseQueryString(query)["songOffset"];
        if (offset != "0") return """{"subsonic-response":{"status":"ok","version":"1.16.1","searchResult3":{"song":[]}}}""";
        var old = DateTime.UtcNow.AddYears(-1).ToString("O");
        var songs = new List<string>();
        for (var i = 0; i < 12; i++)
            songs.Add($$"""{"id":"big{{i}}","title":"Big {{i}}","artist":"Big Artist","artistId":"arBig","duration":200,"playCount":{{(i < 6 ? 8 : 0)}},"played":"{{old}}"}""");
        songs.Add("""{"id":"owned","title":"Owned Song","artist":"Big Artist","artistId":"arBig","duration":210,"playCount":0}""");
        songs.Add("""{"id":"small","title":"Small","artist":"Small Artist","artistId":"arSmall","duration":200,"playCount":1}""");
        return "{\"subsonic-response\":{\"status\":\"ok\",\"version\":\"1.16.1\",\"searchResult3\":{\"song\":[" + string.Join(",", songs) + "]}}}";
    }
}

public class NewReleasesBuilderTests
{
    [Fact]
    public async Task NewReleases_AreTheArtistsLatest_NewestFirst_WithTheListenersOwnCopies()
    {
        var upstream = new ForYouUpstream();
        var deezer = new DeezerMetadataService(new ReviewFixtures.OneClientFactory(upstream),
            TestOptions.Monitor(new MetadataSettings()), NullLogger<DeezerMetadataService>.Instance);
        var registry = new ExternalIdRegistry();
        var builder = new NewReleasesBuilder(deezer, registry, NullLogger<NewReleasesBuilder>.Instance);
        var library = new List<JsonObject>
        {
            JsonNode.Parse("""{"id":"owned","title":"Owned Song","artist":"Big Artist","playCount":0}""")!.AsObject(),
        };

        var result = await builder.BuildAsync([new ForYouLists.TopArtist("arBig", "Big Artist", 50, 0, 13)], library,
            DateOnly.FromDateTime(DateTime.UtcNow), weeks: 8, CancellationToken.None);

        Assert.True(result.Whole);
        // The single (3 days old) first, then the album's best three by rank, in album order.
        Assert.Equal(["Single Song", "Hit", "Owned Song", "Closer"], result.Entries.Select(entry =>
            entry.Outside?.Title ?? entry.Library!["title"]!.GetValue<string>()));
        Assert.Equal("owned", result.Entries[2].Library!["id"]!.GetValue<string>());
        Assert.All(result.Entries.Where(entry => entry.Outside is not null), entry =>
        {
            Assert.False(entry.Outside!.IsLocal);
            Assert.NotNull(registry.Lookup(entry.Outside.Id));
            Assert.Contains(entry.Outside.Album, new[] { "New Album", "New Single" });
        });
        // The tribute act has more fans, but only the artist of that very name is theirs.
        Assert.DoesNotContain(upstream.Calls, call => call.Contains("artist/8/", StringComparison.Ordinal));
        Assert.DoesNotContain(upstream.Calls, call => call.Contains("album/102", StringComparison.Ordinal));
        Assert.DoesNotContain(upstream.Calls, call => call.Contains("album/103", StringComparison.Ordinal));
    }
}

public class ForYouServiceTests : IDisposable
{
    private readonly string _dir = Path.Combine(Path.GetTempPath(), "octo-foryou-" + Guid.NewGuid());

    public void Dispose()
    {
        try { Directory.Delete(_dir, true); } catch { }
        GC.SuppressFinalize(this);
    }

    private static readonly Dictionary<string, string> Auth = new() { ["u"] = "alice", ["t"] = "token", ["s"] = "salt", ["v"] = "1.16.1", ["c"] = "test" };

    private GeneratedPlaylistService Service(ForYouUpstream upstream, GeneratedPlaylistSettings settings)
    {
        var services = new ServiceCollection()
            .AddSingleton<IHttpClientFactory>(new ReviewFixtures.OneClientFactory(upstream))
            .AddSingleton<IOptionsMonitor<SubsonicSettings>>(TestOptions.Monitor(new SubsonicSettings { Url = "http://navidrome.test" }))
            .AddSingleton<IOptionsMonitor<MetadataSettings>>(TestOptions.Monitor(new MetadataSettings()))
            .AddSingleton<IHttpContextAccessor>(new HttpContextAccessor())
            .AddSingleton(new ExternalIdRegistry())
            .AddSingleton(sp => new SubsonicResponseBuilder(sp.GetRequiredService<ExternalIdRegistry>(), Options.Create(new SubsonicSettings())))
            .AddSingleton(sp => new DeezerMetadataService(sp.GetRequiredService<IHttpClientFactory>(),
                sp.GetRequiredService<IOptionsMonitor<MetadataSettings>>(), NullLogger<DeezerMetadataService>.Instance))
            .AddSingleton(sp => new NewReleasesBuilder(sp.GetRequiredService<DeezerMetadataService>(),
                sp.GetRequiredService<ExternalIdRegistry>(), NullLogger<NewReleasesBuilder>.Instance))
            .AddScoped<SubsonicProxyService>()
            .BuildServiceProvider();
        return new GeneratedPlaylistService(Path.Combine(_dir, "generated-playlists.json"),
            services.GetRequiredService<IServiceScopeFactory>(), TestOptions.Monitor(settings),
            TestOptions.Monitor(new GenreSettings()), NullLogger<GeneratedPlaylistService>.Instance);
    }

    private static async Task<IReadOnlyList<GeneratedPlaylist>> ListedAsync(GeneratedPlaylistService service)
    {
        // The first list waits only a moment for a build; ask until the build has landed.
        for (var i = 0; i < 100; i++)
        {
            var lists = await service.ListAsync("alice", Auth);
            if (lists.Count > 0) return lists;
            await Task.Delay(100);
        }
        return await service.ListAsync("alice", Auth);
    }

    [Fact]
    public async Task TheThreeListsAreMadeFromTheListenersPlays_NewestFirst_WithoutTheMixesSwitch()
    {
        var service = Service(new ForYouUpstream(), new GeneratedPlaylistSettings());

        var lists = await ListedAsync(service);

        Assert.Equal(["New Releases", "Rediscover", "Deep Cuts"], lists.Select(list => list.Name));
        Assert.Equal(["newReleases", "rediscover", "deepCuts"], lists.Select(list => list.Kind));
        Assert.All(lists, list => Assert.StartsWith("og", list.Id));
        var rediscover = await service.MaterializeAsync("alice", lists[1], Auth, CancellationToken.None);
        Assert.Equal(3, rediscover.Count); // the six played songs, three per artist at most
        var deepCuts = await service.MaterializeAsync("alice", lists[2], Auth, CancellationToken.None);
        Assert.All(deepCuts, song => Assert.True(song["playCount"]!.GetValue<int>() <= 1));
        var newReleases = await service.MaterializeAsync("alice", lists[0], Auth, CancellationToken.None);
        Assert.Contains(newReleases, song => song["id"]!.GetValue<string>() == "owned");
        Assert.Contains(newReleases, song => song["title"]!.GetValue<string>() == "Single Song"
            && song["isExternal"]?.GetValue<bool>() != false);
        Assert.Equal(lists[0], service.Find("alice", lists[0].Id));
        Assert.Null(service.Find("bob", lists[0].Id));
        Assert.Equal(["https://cdn.test/101.jpg", "https://cdn.test/100.jpg"], service.ForYouCovers("alice"));
    }

    [Fact]
    public async Task TheListsSurviveARestart_WithoutBeingMadeAgain()
    {
        var upstream = new ForYouUpstream();
        await ListedAsync(Service(upstream, new GeneratedPlaylistSettings()));
        var walked = upstream.Calls.Count(call => call.EndsWith("rest/search3", StringComparison.Ordinal));

        var restarted = Service(upstream, new GeneratedPlaylistSettings());
        var lists = await restarted.ListAsync("alice", Auth);

        Assert.Equal(3, lists.Count);
        Assert.Equal(walked, upstream.Calls.Count(call => call.EndsWith("rest/search3", StringComparison.Ordinal)));
        Assert.NotEmpty(await restarted.MaterializeAsync("alice", lists[1], Auth, CancellationToken.None));
    }

    [Fact]
    public async Task AListSwitchedOff_IsGoneAtOnce()
    {
        var upstream = new ForYouUpstream();
        await ListedAsync(Service(upstream, new GeneratedPlaylistSettings()));

        var lists = await Service(upstream, new GeneratedPlaylistSettings { Rediscover = false }).ListAsync("alice", Auth);

        Assert.DoesNotContain(lists, list => list.Kind == ForYouLists.RediscoverKind);
    }

    [Fact]
    public async Task ACatalogThatDoesNotAnswer_LeavesYesterdaysNewReleases()
    {
        var upstream = new ForYouUpstream();
        var settings = new GeneratedPlaylistSettings { RefreshHours = 1 };
        var first = await ListedAsync(Service(upstream, settings));
        var before = (await Service(upstream, settings).MaterializeAsync("alice", first[0], Auth, CancellationToken.None)).Count;

        upstream.CatalogDown = true;
        // Made stale by a new switch value, so the next list builds again with the catalog down.
        var again = Service(upstream, new GeneratedPlaylistSettings { RefreshHours = 1, NewReleaseWeeks = 9 });
        await again.ListAsync("alice", Auth);
        await Task.Delay(500);
        var lists = await again.ListAsync("alice", Auth);

        var newReleases = lists.Single(list => list.Kind == ForYouLists.NewReleasesKind);
        Assert.Equal(before, (await again.MaterializeAsync("alice", newReleases, Auth, CancellationToken.None)).Count);
    }

    [Fact]
    public async Task AllThreeOff_AndMixesOff_IsNothing()
        => Assert.Empty(await Service(new ForYouUpstream(),
            new GeneratedPlaylistSettings { NewReleases = false, Rediscover = false, DeepCuts = false }).ListAsync("alice", Auth));
}

public class ForYouResponseTests
{
    private static SubsonicResponseBuilder Builder() => new(new ExternalIdRegistry(), Options.Create(new SubsonicSettings()));

    private static GeneratedPlaylist List(string kind) =>
        new("og" + new string('a', 20), "for:" + kind, kind, kind, kind, "alice", 10, DateTime.UtcNow, DateTime.UtcNow.AddDays(1));

    [Fact]
    public void AMadeForYouList_SaysWhichOne_AndAMixDoesNot()
    {
        var builder = Builder();

        var newReleases = builder.GeneratedPlaylistFields(List(ForYouLists.NewReleasesKind), new GeneratedPlaylistSettings());
        var genre = builder.GeneratedPlaylistFields(List("genre"), new GeneratedPlaylistSettings());

        Assert.Equal("newReleases", newReleases["octoList"]);
        Assert.Contains("new releases", (string)newReleases["comment"]);
        Assert.False(genre.ContainsKey("octoList"));
    }

    [Fact]
    public void TheListsAreAdvertised()
        => Assert.Contains(SubsonicResponseBuilder.OwnExtensions, extension => extension.Name == "octoLists" && extension.Versions.Contains(1));
}

public sealed class ForYouControllerTests
{
    private sealed class ForYouWebFactory : Microsoft.AspNetCore.Mvc.Testing.WebApplicationFactory<Program>
    {
        private readonly string _directory = Path.Combine(Path.GetTempPath(), "octo-foryou-web-" + Guid.NewGuid());
        public ForYouUpstream Upstream { get; } = new();

        protected override void ConfigureWebHost(Microsoft.AspNetCore.Hosting.IWebHostBuilder builder)
        {
            Directory.CreateDirectory(_directory);
            builder.ConfigureAppConfiguration((_, configuration) =>
                Microsoft.Extensions.Configuration.MemoryConfigurationBuilderExtensions.AddInMemoryCollection(configuration,
                    new Dictionary<string, string?>
                    {
                        ["Subsonic:Url"] = "http://navidrome.test",
                        ["Subsonic:AutoDetectDownloadPath"] = "false",
                        ["Library:DownloadPath"] = _directory,
                    }));
            builder.ConfigureServices(services =>
            {
                Microsoft.Extensions.DependencyInjection.Extensions.ServiceCollectionDescriptorExtensions
                    .RemoveAll<Microsoft.Extensions.Hosting.IHostedService>(services);
                Microsoft.Extensions.DependencyInjection.Extensions.ServiceCollectionDescriptorExtensions
                    .RemoveAll<IHttpClientFactory>(services);
                services.AddSingleton<IHttpClientFactory>(new ReviewFixtures.OneClientFactory(Upstream));
                Microsoft.Extensions.DependencyInjection.Extensions.ServiceCollectionDescriptorExtensions
                    .RemoveAll<GeneratedPlaylistService>(services);
                services.AddSingleton(sp => new GeneratedPlaylistService(Path.Combine(_directory, "generated-playlists.json"),
                    sp.GetRequiredService<IServiceScopeFactory>(),
                    sp.GetRequiredService<IOptionsMonitor<GeneratedPlaylistSettings>>(),
                    sp.GetRequiredService<IOptionsMonitor<GenreSettings>>(),
                    NullLogger<GeneratedPlaylistService>.Instance));
            });
        }

        public override async ValueTask DisposeAsync()
        {
            await base.DisposeAsync();
            try { Directory.Delete(_directory, recursive: true); } catch { /* best effort */ }
        }
    }

    private const string Auth = "u=alice&t=token&s=salt&v=1.16.1&c=test";

    [Fact]
    public async Task GetPlaylists_MarksEachMadeForYouList_InJsonAndXml()
    {
        await using var factory = new ForYouWebFactory();
        using var client = factory.CreateClient();

        List<JsonElement> rows = [];
        for (var i = 0; i < 100 && rows.Count < 3; i++)
        {
            using var list = JsonDocument.Parse(await client.GetStringAsync($"/rest/getPlaylists.view?{Auth}&f=json"));
            rows = list.RootElement.GetProperty("subsonic-response").GetProperty("playlists").TryGetProperty("playlist", out var found)
                ? found.EnumerateArray().Where(row => row.TryGetProperty("octoList", out _)).Select(row => row.Clone()).ToList()
                : [];
            if (rows.Count < 3) await Task.Delay(100);
        }

        Assert.Equal(["newReleases", "rediscover", "deepCuts"], rows.Select(row => row.GetProperty("octoList").GetString()));
        Assert.All(rows, row => Assert.True(row.GetProperty("readonly").GetBoolean()));
        var xml = await client.GetStringAsync($"/rest/getPlaylists.view?{Auth}");
        Assert.Contains("octoList=\"newReleases\"", xml);

        var id = rows[0].GetProperty("id").GetString();
        using var detail = JsonDocument.Parse(await client.GetStringAsync($"/rest/getPlaylist.view?{Auth}&f=json&id={id}"));
        var playlist = detail.RootElement.GetProperty("subsonic-response").GetProperty("playlist");
        Assert.Equal("newReleases", playlist.GetProperty("octoList").GetString());
        Assert.NotEmpty(playlist.GetProperty("entry").EnumerateArray());

        using var cover = await client.GetAsync($"/rest/getCoverArt.view?{Auth}&f=json&id={id}");
        cover.EnsureSuccessStatusCode();
        Assert.Equal("image/jpeg", cover.Content.Headers.ContentType?.MediaType);
    }
}
