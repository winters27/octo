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
    public void TopArtists_ArePlaysSummed_OnlyOnesPlayedOrHearted_AndVariousArtistsIsNobody()
    {
        // Found live: with room for 50, a listener who played two artists got every artist in
        // the library as "theirs". An artist never played nor hearted is never one.
        var songs = new[]
        {
            Song("1", "Big", plays: 10), Song("2", "Big", plays: 10), Song("3", "Small", plays: 5),
            Song("4", "Various Artists", plays: 100), Song("5", "Quiet"), Song("6", "Hearted", starred: true),
        };

        var top = ForYouLists.TopArtists(songs, 50);

        Assert.Equal(["Big", "Small", "Hearted"], top.Select(artist => artist.Name));
        Assert.Equal(20, top[0].Plays);
    }

    [Fact]
    public void TopArtists_WithNoPlays_AreTheHearted_AndWithNothingAtAll_TheBiggest()
    {
        var hearted = new[]
        {
            Song("1", "Kept"), Song("2", "Kept"), Song("3", "Kept"),
            Song("4", "Loved", starred: true), Song("5", "Neither"),
        };
        Assert.Equal(["Loved"], ForYouLists.TopArtists(hearted, 10).Select(artist => artist.Name));

        // A new listener, with no plays or hearts yet, still gets lists from the library's biggest.
        var fresh = new[] { Song("1", "Kept"), Song("2", "Kept"), Song("3", "Neither") };
        Assert.Equal(["Kept", "Neither"], ForYouLists.TopArtists(fresh, 10).Select(artist => artist.Name));
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
    /// <summary>While set, every catalog call waits for it.</summary>
    public TaskCompletionSource? HoldCatalog { get; set; }
    /// <summary>The artist has more than a page of releases, the newest single on the second.</summary>
    public bool LongCareer { get; set; }
    /// <summary>A library of more than a page whose second page Navidrome fails to send.</summary>
    public bool WalkBreaks { get; set; }

    protected override async Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken ct)
    {
        var asked = request.RequestUri!;
        lock (Calls) Calls.Add(asked.Host + "/" + asked.AbsolutePath.Trim('/'));
        if (HoldCatalog is { } hold && asked.Host != "navidrome.test") await hold.Task;
        return await Answer(request);
    }

    private Task<HttpResponseMessage> Answer(HttpRequestMessage request)
    {
        var uri = request.RequestUri!;
        var path = uri.AbsolutePath.Trim('/');
        if (uri.Host != "navidrome.test" && CatalogDown)
            return Task.FromResult(new HttpResponseMessage(System.Net.HttpStatusCode.TooManyRequests));
        if (WalkBreaks && path == "rest/search3"
            && System.Web.HttpUtility.ParseQueryString(uri.Query)["songOffset"] != "0")
            return Task.FromResult(new HttpResponseMessage(System.Net.HttpStatusCode.InternalServerError));
        var body = (uri.Host, path) switch
        {
            ("navidrome.test", "rest/search3") => Navidrome(uri.Query),
            ("navidrome.test", "rest/ping" or "rest/ping.view") => Plain(uri.Query, ""),
            ("navidrome.test", "rest/getPlaylists" or "rest/getPlaylists.view") => Plain(uri.Query, "playlists"),
            (_, "search/artist") => """{"data":[{"id":7,"name":"Big Artist","nb_fan":900},{"id":8,"name":"Big Artist Tribute","nb_fan":5000}]}""",
            (_, "artist/7/albums") when LongCareer && uri.Query.Contains("index=100", StringComparison.Ordinal) =>
                "{\"data\":[" + Album(104, "Page Two Single", "single", Day(1), 1, "https://cdn.test/104.jpg") + "],\"total\":105}",
            (_, "artist/7/albums") => "{\"data\":["
                + Album(100, "New Album", "album", Day(10), 4, "https://cdn.test/100.jpg") + ","
                + Album(101, "New Single", "single", Day(3), 1, "https://cdn.test/101.jpg") + ","
                + Album(102, "Old Album", "album", Day(400), 9, null) + ","
                + Album(103, "Coming Soon", "album", Day(-30), 9, null) + "],\"total\":" + (LongCareer ? 105 : 4) + "}",
            (_, "album/104") => """{"id":104,"title":"Page Two Single","record_type":"single","nb_tracks":1,"artist":{"name":"Big Artist"}}""",
            (_, "album/104/tracks") => "{\"data\":[" + AlbumTrack("Page Two Song", 180, 1, 300) + "],\"total\":1}",
            (_, "album/100") => """{"id":100,"title":"New Album","record_type":"album","nb_tracks":4,"artist":{"name":"Big Artist"}}""",
            (_, "album/100/tracks") => "{\"data\":["
                + AlbumTrack("Opener", 180, 1, 100) + "," + AlbumTrack("Hit", 200, 2, 900, explicitLyrics: 1) + ","
                + AlbumTrack("Owned Song", 210, 3, 800, explicitLyrics: 1) + "," + AlbumTrack("Closer", 240, 4, 700) + "],\"total\":4}",
            (_, "album/101") => """{"id":101,"title":"New Single","record_type":"single","nb_tracks":1,"artist":{"name":"Big Artist"}}""",
            // The single is credited to a guest, as the catalog does with shared songs.
            (_, "album/101/tracks") => "{\"data\":[" + AlbumTrack("Single Song", 190, 1, 500, "Guest Star") + "],\"total\":1}",
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

    private static string AlbumTrack(string title, int duration, int position, int rank, string artist = "Big Artist",
        int explicitLyrics = 0) =>
        "{\"title\":\"" + title + "\",\"duration\":" + duration + ",\"track_position\":" + position
        + ",\"disk_number\":1,\"rank\":" + rank + ",\"explicit_content_lyrics\":" + explicitLyrics
        + ",\"artist\":{\"name\":\"" + artist + "\"}}";

    private string Navidrome(string query)
    {
        var offset = System.Web.HttpUtility.ParseQueryString(query)["songOffset"];
        if (offset != "0") return """{"subsonic-response":{"status":"ok","version":"1.16.1","searchResult3":{"song":[]}}}""";
        var old = DateTime.UtcNow.AddYears(-1).ToString("O");
        var songs = new List<string>();
        // A full first page, so the walk asks for a second.
        if (WalkBreaks)
            for (var i = 0; i < 500; i++)
                songs.Add($$"""{"id":"filler{{i}}","title":"Filler {{i}}","artist":"Filler","artistId":"arFiller","duration":200,"playCount":9}""");
        for (var i = 0; i < 12; i++)
            songs.Add($$"""{"id":"big{{i}}","title":"Big {{i}}","artist":"Big Artist","artistId":"arBig","duration":200,"playCount":{{(i < 6 ? 8 : 0)}},"played":"{{old}}"}""");
        songs.Add("""{"id":"owned","title":"Owned Song","artist":"Big Artist","artistId":"arBig","duration":210,"playCount":0}""");
        songs.Add("""{"id":"small","title":"Small","artist":"Small Artist","artistId":"arSmall","duration":200,"playCount":1}""");
        songs.Add("""{"id":"never","title":"Never","artist":"Never Played","artistId":"arNever","duration":200,"playCount":0}""");
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
            // Credited to a guest in the catalog, filed under the listener's own artist here.
            JsonNode.Parse("""{"id":"single","title":"Single Song","artist":"Big Artist","playCount":0}""")!.AsObject(),
        };

        var result = await builder.BuildAsync([new ForYouLists.TopArtist("arBig", "Big Artist", 50, 0, 13)], library,
            DateOnly.FromDateTime(DateTime.UtcNow), weeks: 8, CancellationToken.None);

        Assert.True(result.Whole);
        // The single (3 days old) first, then the album's best three by rank, in album order.
        Assert.Equal(["Single Song", "Hit", "Owned Song", "Closer"], result.Entries.Select(entry =>
            entry.Outside?.Title ?? entry.Library!["title"]!.GetValue<string>()));
        Assert.Equal("owned", result.Entries[2].Library!["id"]!.GetValue<string>());
        Assert.Equal("single", result.Entries[0].Library!["id"]!.GetValue<string>());
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
        // The catalog's artist rides on a song credited to them; a guest's song names no catalog artist.
        var hit = result.Entries.Single(entry => entry.Outside?.Title == "Hit");
        Assert.Equal("7", registry.Lookup(hit.Outside!.ArtistId!)!.ExternalArtistId);
        Assert.Equal(1, hit.Outside.ExplicitContentLyrics);
    }

    private static (NewReleasesBuilder Builder, ForYouUpstream Upstream) Make(Action<ForYouUpstream>? setUp = null)
    {
        var upstream = new ForYouUpstream();
        setUp?.Invoke(upstream);
        var deezer = new DeezerMetadataService(new ReviewFixtures.OneClientFactory(upstream),
            TestOptions.Monitor(new MetadataSettings()), NullLogger<DeezerMetadataService>.Instance);
        return (new NewReleasesBuilder(deezer, new ExternalIdRegistry(), NullLogger<NewReleasesBuilder>.Instance), upstream);
    }

    private static readonly ForYouLists.TopArtist BigArtist = new("arBig", "Big Artist", 50, 0, 13);

    private static IReadOnlyList<string> Titles(NewReleasesBuilder.Result result) =>
        result.Entries.Select(entry => entry.Outside?.Title ?? entry.Library!["title"]!.GetValue<string>()).ToList();

    [Fact]
    public async Task ACleanOnlyServer_LeavesOutExplicitOutsideSongs_ButNeverTheListenersOwn()
    {
        var (builder, _) = Make();
        var library = new List<JsonObject>
        {
            JsonNode.Parse("""{"id":"owned","title":"Owned Song","artist":"Big Artist","playCount":0}""")!.AsObject(),
        };

        var clean = await builder.BuildAsync([BigArtist], library, DateOnly.FromDateTime(DateTime.UtcNow), 8,
            CancellationToken.None, ExplicitFilter.CleanOnly);

        Assert.Equal(["Single Song", "Owned Song", "Closer"], Titles(clean));
    }

    [Fact]
    public async Task ALongCareer_IsReadPastItsFirstPage_SoTheNewestSingleIsFound()
    {
        var (builder, upstream) = Make(fake => fake.LongCareer = true);

        var result = await builder.BuildAsync([BigArtist], [], DateOnly.FromDateTime(DateTime.UtcNow), 8, CancellationToken.None);

        Assert.Equal("Page Two Song", Titles(result)[0]);
        Assert.Contains(upstream.Calls, call => call.EndsWith("artist/7/albums", StringComparison.Ordinal));
    }

    [Fact]
    public async Task ListenersBuildingTogether_TakeTheCatalogInTurn()
    {
        var (builder, upstream) = Make(fake => fake.HoldCatalog = new TaskCompletionSource());
        var today = DateOnly.FromDateTime(DateTime.UtcNow);

        var first = builder.BuildAsync([BigArtist], [], today, 8, CancellationToken.None);
        // Another listener, another artist: a search of its own, were it allowed to ask.
        var second = builder.BuildAsync([new ForYouLists.TopArtist("arOther", "Other Artist", 9, 0, 3)], [], today, 8,
            CancellationToken.None);
        await Task.Delay(200);
        // Only the first listener's walk is asking the catalog; the second waits its turn.
        lock (upstream.Calls) Assert.Single(upstream.Calls, call => call.EndsWith("search/artist", StringComparison.Ordinal));
        upstream.HoldCatalog!.SetResult();

        Assert.NotEmpty((await first).Entries);
        Assert.Empty((await second).Entries);
        lock (upstream.Calls) Assert.Equal(2, upstream.Calls.Count(call => call.EndsWith("search/artist", StringComparison.Ordinal)));
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

    private ServiceProvider? _services;

    private GeneratedPlaylistService Service(ForYouUpstream upstream, GeneratedPlaylistSettings settings,
        ExplicitFilter explicitFilter = ExplicitFilter.All)
    {
        var services = new ServiceCollection()
            .AddSingleton<IHttpClientFactory>(new ReviewFixtures.OneClientFactory(upstream))
            .AddSingleton<IOptionsMonitor<SubsonicSettings>>(TestOptions.Monitor(
                new SubsonicSettings { Url = "http://navidrome.test", ExplicitFilter = explicitFilter }))
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
        _services = services;
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
        Assert.DoesNotContain(deepCuts, song => song["id"]!.GetValue<string>() == "never");
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
    public async Task AnOutsideSongPushedOutOfOctosMemory_IsMadeAgain_WithTheSameId()
    {
        var service = Service(new ForYouUpstream(), new GeneratedPlaylistSettings());
        var lists = await ListedAsync(service);
        var registry = _services!.GetRequiredService<ExternalIdRegistry>();
        var outside = (await service.MaterializeAsync("alice", lists[0], Auth, CancellationToken.None))
            .Where(song => song["isExternal"]?.GetValue<bool>() == true)
            .Select(song => song["id"]!.GetValue<string>())
            .ToList();
        Assert.NotEmpty(outside);

        // A busy day of searches: more outside songs than Octo remembers.
        for (var i = 0; i < 10_050; i++)
            registry.Register(new SoulseekRouting { Kind = RoutingKind.Song, Artist = "Flood", Title = "Song " + i });
        Assert.All(outside, id => Assert.Null(registry.Lookup(id)));

        var served = await service.MaterializeAsync("alice", lists[0], Auth, CancellationToken.None);

        Assert.All(outside, id => Assert.NotNull(registry.Lookup(id)));
        Assert.All(served, song => Assert.Null(song["octoCatalog"]));
    }

    [Fact]
    public async Task AWalkThatBreaksPartWay_LeavesTheListsAsTheyWere()
    {
        var upstream = new ForYouUpstream();
        var first = await ListedAsync(Service(upstream, new GeneratedPlaylistSettings { RefreshHours = 1 }));
        var deepCutsBefore = (await Service(upstream, new GeneratedPlaylistSettings { RefreshHours = 1 })
            .MaterializeAsync("alice", first[2], Auth, CancellationToken.None)).Select(song => song["id"]!.GetValue<string>()).ToList();

        upstream.WalkBreaks = true;
        var again = Service(upstream, new GeneratedPlaylistSettings { RefreshHours = 1, NewReleaseWeeks = 9 });
        await again.ListAsync("alice", Auth);
        await Task.Delay(500);
        var lists = await again.ListAsync("alice", Auth);

        var deepCuts = lists.Single(list => list.Kind == ForYouLists.DeepCutsKind);
        Assert.Equal(deepCutsBefore,
            (await again.MaterializeAsync("alice", deepCuts, Auth, CancellationToken.None)).Select(song => song["id"]!.GetValue<string>()));
    }

    [Fact]
    public async Task RediscoverAndDeepCuts_ShowWhileNewReleasesWaitsOnTheCatalog()
    {
        var upstream = new ForYouUpstream { HoldCatalog = new TaskCompletionSource() };
        var service = Service(upstream, new GeneratedPlaylistSettings());

        var early = await ListedAsync(service);
        Assert.Equal(["rediscover", "deepCuts"], early.Select(list => list.Kind));

        upstream.HoldCatalog.SetResult();
        IReadOnlyList<GeneratedPlaylist> lists = early;
        for (var i = 0; i < 100 && lists.Count < 3; i++)
        {
            await Task.Delay(100);
            lists = await service.ListAsync("alice", Auth);
        }
        Assert.Equal(["newReleases", "rediscover", "deepCuts"], lists.Select(list => list.Kind));
    }

    [Fact]
    public async Task TheExplicitFilter_IsObeyed_AndChangingItMakesNewReleasesAgain()
    {
        var upstream = new ForYouUpstream();
        var all = await ListedAsync(Service(upstream, new GeneratedPlaylistSettings()));
        var allService = Service(upstream, new GeneratedPlaylistSettings());
        Assert.Contains(await allService.MaterializeAsync("alice", all[0], Auth, CancellationToken.None),
            song => song["title"]!.GetValue<string>() == "Hit");

        var clean = Service(upstream, new GeneratedPlaylistSettings(), ExplicitFilter.CleanOnly);
        await clean.ListAsync("alice", Auth);
        IReadOnlyList<JsonObject> songs = [];
        for (var i = 0; i < 50; i++)
        {
            await Task.Delay(100);
            var lists = await clean.ListAsync("alice", Auth);
            songs = await clean.MaterializeAsync("alice", lists[0], Auth, CancellationToken.None);
            if (songs.All(song => song["title"]!.GetValue<string>() != "Hit")) break;
        }

        Assert.DoesNotContain(songs, song => song["title"]!.GetValue<string>() == "Hit");
        Assert.Contains(songs, song => song["title"]!.GetValue<string>() == "Owned Song");
    }

    [Fact]
    public async Task AllThreeOff_AndMixesOff_IsNothing()
        => Assert.Empty(await Service(new ForYouUpstream(),
            new GeneratedPlaylistSettings { NewReleases = false, Rediscover = false, DeepCuts = false }).ListAsync("alice", Auth));
}

public class ForYouResponseTests
{
    private static SubsonicResponseBuilder Builder() => new(new ExternalIdRegistry(), Options.Create(new SubsonicSettings()));

    private static GeneratedPlaylist List(string kind, int songs = 10) =>
        new("og" + new string('a', 20), "for:" + kind, kind, kind, kind, "alice", songs, DateTime.UtcNow, DateTime.UtcNow.AddDays(1));

    [Fact]
    public void AMadeForYouList_CountsWhatItHolds_NotTheMixesTrackCount()
    {
        var settings = new GeneratedPlaylistSettings { TrackCount = 10 };

        Assert.Equal(24, Builder().GeneratedPlaylistFields(List(ForYouLists.DeepCutsKind, 24), settings)["songCount"]);
        Assert.Equal(10, Builder().GeneratedPlaylistFields(List("genre", 24), settings)["songCount"]);
    }

    [Theory]
    [InlineData("https://cdn-images.dzcdn.net/images/cover/abc/1000x1000-000000-80-0-0.jpg", true)]
    [InlineData("https://e-cdns-images.dzcdn.net/images/cover/abc/500x500.jpg", true)]
    [InlineData("http://cdn-images.dzcdn.net/images/cover/abc.jpg", false)]
    [InlineData("https://dzcdn.net.example.com/cover.jpg", false)]
    [InlineData("https://192.168.1.10/cover.jpg", false)]
    [InlineData("not a url", false)]
    public void NewReleasesCovers_AreFetchedOnlyFromTheCatalogsImageHost(string url, bool fetched)
        => Assert.Equal(fetched, Octo.Controllers.SubsonicController.IsCatalogImage(url));

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
