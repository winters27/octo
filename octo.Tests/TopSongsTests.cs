using System.Collections.Concurrent;
using System.Net;
using System.Text;
using System.Text.Json;
using Microsoft.AspNetCore.Hosting;
using Microsoft.AspNetCore.Mvc.Testing;
using Microsoft.Extensions.Configuration;
using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.DependencyInjection.Extensions;
using Microsoft.Extensions.Hosting;
using Octo.Models.Domain;
using Octo.Services.Common;
using Octo.Services.LastFm;
using Octo.Services.Metadata;

namespace Octo.Tests;

/// <summary>
/// Top songs for the apps' search: how the two sources are merged into one ranked list, how a
/// row is found in the library, and getArtistTopSongs and getTopChart as the apps read them.
/// </summary>
public sealed class TopSongsTests
{
    private static DeezerMetadataService.ChartTrack Catalog(string title, string artist = "Daft Punk", string album = "Discovery") =>
        new($"{artist}|{title}".GetHashCode().ToString(), title, artist, "27", album, "302127",
            $"https://cdn/{album}.jpg", 240, false);

    private static LastFmService.ChartTrack Counted(string title, long plays, string artist = "Daft Punk") =>
        new(artist, title, plays, plays / 10);

    [Fact]
    public void Merge_WithPlayCounts_FollowsLastFmAndDressesEachRowFromTheCatalog()
    {
        var catalog = new[] { Catalog("Instant Crush (feat. Julian Casablancas)", album: "Random Access Memories"), Catalog("One More Time"), Catalog("Harder, Better, Faster, Stronger") };
        var counts = new[] { Counted("Harder, Better, Faster, Stronger", 2_500_000), Counted("One More Time", 1_900_000), Counted("Aerodynamic", 1_500_000), Counted("Instant Crush", 900_000) };

        var (rows, source) = TopSongsService.Merge(catalog, counts, oneArtist: true, max: 10);

        Assert.Equal(TopSongsService.LastFmSource, source);
        Assert.Equal(["Harder, Better, Faster, Stronger", "One More Time", "Aerodynamic", "Instant Crush"], rows.Select(row => row.Title));
        Assert.Equal([2_500_000L, 1_900_000, 1_500_000, 900_000], rows.Select(row => row.Plays!.Value));
        // A guest credit is still the song, so the catalog's copy dresses it.
        Assert.Equal("Random Access Memories", rows[3].Catalog?.Album);
        // One the catalog's top list lacks is left for a lookup.
        Assert.Null(rows[2].Catalog);
    }

    [Fact]
    public void Merge_WithoutCounts_KeepsTheCatalogOrderAndOneRowPerSong()
    {
        var catalog = new[]
        {
            Catalog("Instant Crush (feat. Julian Casablancas)"),
            Catalog("Get Lucky (Radio Edit - feat. Pharrell Williams and Nile Rodgers)", album: "Get Lucky"),
            Catalog("One More Time"),
            Catalog("Get Lucky (feat. Pharrell Williams and Nile Rodgers)", album: "Random Access Memories"),
        };

        var (rows, source) = TopSongsService.Merge(catalog, null, oneArtist: true, max: 10);

        Assert.Equal(TopSongsService.DeezerSource, source);
        Assert.Equal(3, rows.Count);
        Assert.StartsWith("Get Lucky (Radio Edit", rows[1].Title);
        Assert.All(rows, row => Assert.Null(row.Plays));
    }

    [Fact]
    public void Merge_OnAChart_KeepsTwoArtistsSongsOfOneTitle()
    {
        var catalog = new[] { Catalog("Hello", "Adele", "25"), Catalog("Hello", "Lionel Richie", "Can't Slow Down"), Catalog("Hello", "Adele", "Hello") };

        var (rows, _) = TopSongsService.Merge(catalog, null, oneArtist: false, max: 10);

        Assert.Equal(["Adele", "Lionel Richie"], rows.Select(row => row.Artist));
    }

    [Fact]
    public void Merge_StopsAtTheLimitAndSaysNothingForNothing()
    {
        var catalog = Enumerable.Range(1, 30).Select(i => Catalog($"Song {i}")).ToArray();
        Assert.Equal(5, TopSongsService.Merge(catalog, null, true, 5).Rows.Count);
        Assert.Equal("", TopSongsService.Merge(null, null, true, 5).Source);
        // Counts that came back empty leave the catalog's order.
        Assert.Equal(TopSongsService.DeezerSource, TopSongsService.Merge(catalog, [], true, 5).Source);
    }

    [Theory]
    [InlineData("Daft Punk", "Get Lucky (feat. Pharrell Williams and Nile Rodgers)", "Daft Punk Get Lucky")]
    [InlineData("Daft Punk", "Get Lucky (Radio Edit - feat. Pharrell Williams and Nile Rodgers)", "Daft Punk Get Lucky")]
    [InlineData("Drake feat. Rihanna", "Too Good", "Drake Too Good")]
    [InlineData("Air", "Sexy Boy", "Air Sexy Boy")]
    public void LibraryQuery_IsTheMainArtistAndTheBareTitle(string artist, string title, string query)
    {
        var song = new Song { Artist = artist, Title = title };
        Assert.Equal(query, TopSongsService.LibraryQuery(song), ignoreCase: true);
    }

    private static TopSongsService.TopSong Top(int rank, string artist, string title) =>
        new(rank, new Song { Id = $"out-{rank}", Artist = artist, Title = title }, null, null);

    private static JsonElement Songs(params (string Id, string Artist, string Title)[] songs) =>
        JsonDocument.Parse(JsonSerializer.Serialize(songs.Select(s => new { id = s.Id, artist = s.Artist, title = s.Title }))).RootElement.Clone();

    [Fact]
    public async Task MatchLibrary_FindsTheSameRecordingOnly_AndGivesALibrarySongToOneRow()
    {
        var rows = new[]
        {
            Top(1, "Air", "Sexy Boy"),
            Top(2, "Daft Punk", "Get Lucky (feat. Pharrell Williams and Nile Rodgers)"),
            Top(3, "Daft Punk", "Get Lucky (Radio Edit)"),
            Top(4, "Daft Punk", "Around the World"),
        };
        var asked = new ConcurrentBag<string>();
        var library = await TopSongsService.MatchLibraryAsync(rows, (query, _) =>
        {
            asked.Add(query);
            JsonElement? answer = query switch
            {
                "Air Sexy Boy" => Songs(("nd-airbourne", "Airbourne", "Sexy Boy")),
                "Daft Punk Get Lucky" => Songs(("nd-lucky", "Daft Punk", "Get Lucky")),
                "Daft Punk Around the World" => null,
                _ => Songs(),
            };
            return Task.FromResult(answer);
        });

        Assert.Null(library[0]);
        Assert.Equal("nd-lucky", library[1]?.GetProperty("id").GetString());
        // The radio edit is the same song, but the library copy already stands for row 2.
        Assert.Null(library[2]);
        Assert.Null(library[3]);
        Assert.Equal(4, asked.Count);
    }

    // ---- the endpoints, as the apps read them ------------------------------------------

    /// <summary>Navidrome, Deezer and Last.fm as these calls need them. Navidrome accepts the
    /// token "good" and its library holds "One More Time" and "Get Lucky" by Daft Punk.</summary>
    private sealed class FakeUpstream : HttpMessageHandler
    {
        public ConcurrentQueue<string> Calls { get; } = new();

        public string? LastFmTop { get; set; }

        protected override Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken ct)
        {
            var uri = request.RequestUri!;
            var query = System.Web.HttpUtility.ParseQueryString(uri.Query);
            Calls.Enqueue(uri.Host + uri.AbsolutePath + "?" + query["method"] + query["query"]);
            if (uri.Host == "navidrome.test")
            {
                if (uri.AbsolutePath.EndsWith("/rest/ping", StringComparison.Ordinal))
                    return Json(query["t"] == "good"
                        ? """{"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome"}}"""
                        : """{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":40,"message":"Wrong username or password"}}}""");
                if (uri.AbsolutePath.EndsWith("/rest/getOpenSubsonicExtensions", StringComparison.Ordinal))
                    return Json("""{"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome","openSubsonic":true,"openSubsonicExtensions":[{"name":"formPost","versions":[1]}]}}""");
                if (uri.AbsolutePath.EndsWith("/rest/search3", StringComparison.Ordinal))
                {
                    var song = query["query"] switch
                    {
                        "Daft Punk One More Time" => """{"id":"nd-omt","title":"One More Time","artist":"Daft Punk","album":"Discovery","coverArt":"al-1","starred":"2026-01-01T00:00:00Z","duration":320}""",
                        "Daft Punk Get Lucky" => """{"id":"nd-lucky","title":"Get Lucky","artist":"Daft Punk","album":"Random Access Memories","duration":369}""",
                        _ => null,
                    };
                    return Json("""{"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome","searchResult3":{"song":[""" + song + "]}}}");
                }
            }
            if (uri.Host == "api.deezer.com")
            {
                if (uri.AbsolutePath == "/search/artist")
                    return Json("""{"data":[{"id":27,"name":"Daft Punk","nb_album":40,"nb_fan":5000000,"picture_xl":"https://cdn/dp.jpg"},{"id":99,"name":"Daft Punk Tribute","nb_fan":10}]}""");
                if (uri.AbsolutePath == "/artist/27/top")
                    return Json("""
                        {"data":[
                        {"id":1,"title":"Instant Crush (feat. Julian Casablancas)","duration":337,"explicit_lyrics":false,"artist":{"id":27,"name":"Daft Punk"},"album":{"id":6575789,"title":"Random Access Memories","cover_xl":"https://cdn/ram.jpg"}},
                        {"id":2,"title":"One More Time","duration":320,"artist":{"id":27,"name":"Daft Punk"},"album":{"id":302127,"title":"Discovery","cover_xl":"https://cdn/discovery.jpg"}},
                        {"id":3,"title":"Get Lucky (Radio Edit - feat. Pharrell Williams and Nile Rodgers)","duration":248,"artist":{"id":27,"name":"Daft Punk"},"album":{"id":1,"title":"Get Lucky","cover_xl":"https://cdn/gl.jpg"}},
                        {"id":4,"title":"Get Lucky (feat. Pharrell Williams and Nile Rodgers)","duration":369,"artist":{"id":27,"name":"Daft Punk"},"album":{"id":6575789,"title":"Random Access Memories","cover_xl":"https://cdn/ram.jpg"}},
                        {"id":5,"title":"Gone","readable":false,"duration":200,"artist":{"id":27,"name":"Daft Punk"},"album":{"id":7,"title":"Gone"}}
                        ]}
                        """);
                if (uri.AbsolutePath == "/chart/0/tracks")
                    return Json("""
                        {"data":[
                        {"id":10,"title":"Dracula","duration":209,"explicit_lyrics":true,"artist":{"id":134790,"name":"Tame Impala"},"album":{"id":11,"title":"Deadbeat","cover_xl":"https://cdn/deadbeat.jpg"}},
                        {"id":2,"title":"One More Time","duration":320,"artist":{"id":27,"name":"Daft Punk"},"album":{"id":302127,"title":"Discovery","cover_xl":"https://cdn/discovery.jpg"}}
                        ]}
                        """);
            }
            if (uri.Host == "ws.audioscrobbler.com" && query["method"] == "artist.gettoptracks" && LastFmTop is { } top)
                return Json(top);
            return Task.FromResult(new HttpResponseMessage(HttpStatusCode.NotFound));
        }

        private static Task<HttpResponseMessage> Json(string body) =>
            Task.FromResult(new HttpResponseMessage(HttpStatusCode.OK) { Content = new StringContent(body, Encoding.UTF8, "application/json") });
    }

    private sealed class TopSongsWebFactory(bool discovery = true, string? lastFmKey = null) : WebApplicationFactory<Program>
    {
        private readonly string _directory = Path.Combine(Path.GetTempPath(), "octo-top-" + Guid.NewGuid());
        public FakeUpstream Upstream { get; } = new();

        protected override void ConfigureWebHost(IWebHostBuilder builder)
        {
            Directory.CreateDirectory(_directory);
            builder.ConfigureAppConfiguration((_, configuration) => configuration.AddInMemoryCollection(
                new Dictionary<string, string?>
                {
                    ["Subsonic:Url"] = "http://navidrome.test",
                    ["Subsonic:AutoDetectDownloadPath"] = "false",
                    ["Subsonic:EnableSearchDiscovery"] = discovery ? "true" : "false",
                    ["Soulseek:BaseUrl"] = "http://127.0.0.1:1",
                    ["YouTube:ShimUrl"] = "http://127.0.0.1:1",
                    ["Library:DownloadPath"] = _directory,
                    ["LastFm:ApiKey"] = lastFmKey ?? "",
                }));
            builder.ConfigureServices(services =>
            {
                services.RemoveAll<IHostedService>();
                services.RemoveAll<IHttpClientFactory>();
                services.AddSingleton<IHttpClientFactory>(new ReviewFixtures.OneClientFactory(Upstream));
            });
        }

        public override async ValueTask DisposeAsync()
        {
            await base.DisposeAsync();
            try { Directory.Delete(_directory, recursive: true); } catch { /* best effort */ }
        }
    }

    private const string Auth = "u=alice&t=good&s=salt&v=1.16.1&c=octo-desktop";

    private static async Task<JsonElement> GetJsonAsync(HttpClient client, string url)
    {
        var response = await client.GetAsync(url);
        Assert.Equal("application/json", response.Content.Headers.ContentType?.MediaType);
        using var doc = JsonDocument.Parse(await response.Content.ReadAsStringAsync());
        return doc.RootElement.GetProperty("subsonic-response").Clone();
    }

    [Fact]
    public async Task ArtistTopSongs_WithoutAKey_IsTheCatalogsList_MarkedAgainstTheLibrary()
    {
        await using var factory = new TopSongsWebFactory();
        using var client = factory.CreateClient();

        var envelope = await GetJsonAsync(client, $"/rest/getArtistTopSongs.view?{Auth}&artist=daft%20punk");

        Assert.Equal("ok", envelope.GetProperty("status").GetString());
        var top = envelope.GetProperty("topSongs");
        Assert.Equal("Daft Punk", top.GetProperty("artist").GetString());
        Assert.Equal("deezer", top.GetProperty("source").GetString());
        var entries = top.GetProperty("entry").EnumerateArray().ToList();
        // The radio edit and the album cut of Get Lucky are one row; an unplayable track is none.
        Assert.Equal(3, entries.Count);
        Assert.Equal([1, 2, 3], entries.Select(e => e.GetProperty("rank").GetInt32()));
        Assert.All(entries, e => Assert.Equal(JsonValueKind.Null, e.GetProperty("plays").ValueKind));

        var crush = entries[0];
        Assert.False(crush.GetProperty("inLibrary").GetBoolean());
        var outside = crush.GetProperty("song");
        Assert.True(outside.GetProperty("isExternal").GetBoolean());
        Assert.Equal("Instant Crush (feat. Julian Casablancas)", outside.GetProperty("title").GetString());
        Assert.Equal("Random Access Memories", outside.GetProperty("album").GetString());
        Assert.Equal(337, outside.GetProperty("duration").GetInt32());
        // Its cover is asked for by its own id, as every outside song's is.
        Assert.Equal(outside.GetProperty("id").GetString(), outside.GetProperty("coverArt").GetString());

        // A library song comes back as Navidrome described it to the caller, heart and all.
        var omt = entries[1];
        Assert.True(omt.GetProperty("inLibrary").GetBoolean());
        Assert.Equal("nd-omt", omt.GetProperty("song").GetProperty("id").GetString());
        Assert.False(omt.GetProperty("song").GetProperty("isExternal").GetBoolean());
        Assert.True(omt.GetProperty("song").TryGetProperty("starred", out _));

        Assert.Equal("nd-lucky", entries[2].GetProperty("song").GetProperty("id").GetString());
        Assert.Equal(1, factory.Upstream.Calls.Count(call => call.Contains("/artist/27/top")));

        // A second listener asking the same is answered from what was kept.
        await GetJsonAsync(client, $"/rest/getArtistTopSongs.view?{Auth}&artist=Daft%20Punk&count=1");
        Assert.Equal(1, factory.Upstream.Calls.Count(call => call.Contains("/artist/27/top")));
    }

    [Fact]
    public async Task ArtistTopSongs_WithAKey_RanksByLastFmPlays()
    {
        await using var factory = new TopSongsWebFactory(lastFmKey: "test-key");
        factory.Upstream.LastFmTop = """
            {"toptracks":{"track":[
            {"name":"Get Lucky","playcount":"9000000","listeners":"900000","artist":{"name":"Daft Punk"}},
            {"name":"Aerodynamic","playcount":"5000000","listeners":"500000","artist":{"name":"Daft Punk"}},
            {"name":"Get Lucky - Radio Edit","playcount":"100000","listeners":"10000","artist":{"name":"Daft Punk"}}
            ]}}
            """;
        using var client = factory.CreateClient();

        var top = (await GetJsonAsync(client, $"/rest/getArtistTopSongs.view?{Auth}&artist=Daft%20Punk")).GetProperty("topSongs");

        Assert.Equal("lastfm", top.GetProperty("source").GetString());
        var entries = top.GetProperty("entry").EnumerateArray().ToList();
        Assert.Equal(2, entries.Count);
        Assert.Equal(9_000_000, entries[0].GetProperty("plays").GetInt64());
        Assert.Equal(900_000, entries[0].GetProperty("listeners").GetInt64());
        Assert.Equal("nd-lucky", entries[0].GetProperty("song").GetProperty("id").GetString());
        Assert.Equal("Aerodynamic", entries[1].GetProperty("song").GetProperty("title").GetString());
        Assert.True(entries[1].GetProperty("song").GetProperty("isExternal").GetBoolean());
    }

    [Fact]
    public async Task TopChart_IsTheCatalogsChart()
    {
        await using var factory = new TopSongsWebFactory();
        using var client = factory.CreateClient();

        var top = (await GetJsonAsync(client, $"/rest/getTopChart.view?{Auth}")).GetProperty("topSongs");

        Assert.Equal(JsonValueKind.Null, top.GetProperty("artist").ValueKind);
        var entries = top.GetProperty("entry").EnumerateArray().ToList();
        Assert.Equal(["Dracula", "One More Time"], entries.Select(e => e.GetProperty("song").GetProperty("title").GetString()));
        Assert.Equal([false, true], entries.Select(e => e.GetProperty("inLibrary").GetBoolean()));
    }

    [Fact]
    public async Task TopSongs_RefuseAWrongPassword_AndAskForAnArtist()
    {
        await using var factory = new TopSongsWebFactory();
        using var client = factory.CreateClient();

        var refused = await GetJsonAsync(client, "/rest/getTopChart.view?u=alice&t=bad&s=salt&v=1.16.1&c=x");
        Assert.Equal(40, refused.GetProperty("error").GetProperty("code").GetInt32());

        var missing = await GetJsonAsync(client, $"/rest/getArtistTopSongs.view?{Auth}");
        Assert.Equal(10, missing.GetProperty("error").GetProperty("code").GetInt32());
        Assert.DoesNotContain(factory.Upstream.Calls, call => call.StartsWith("api.deezer.com", StringComparison.Ordinal));
    }

    [Fact]
    public async Task TheExtension_IsListedOnlyWhileSearchDiscoveryIsOn()
    {
        foreach (var discovery in new[] { true, false })
        {
            await using var factory = new TopSongsWebFactory(discovery);
            using var client = factory.CreateClient();
            var listed = (await GetJsonAsync(client, "/rest/getOpenSubsonicExtensions.view?f=json"))
                .GetProperty("openSubsonicExtensions").EnumerateArray()
                .Any(e => e.GetProperty("name").GetString() == "octoTopSongs");
            Assert.Equal(discovery, listed);

            var entries = (await GetJsonAsync(client, $"/rest/getArtistTopSongs.view?{Auth}&artist=Daft%20Punk"))
                .GetProperty("topSongs").GetProperty("entry").GetArrayLength();
            Assert.Equal(discovery ? 3 : 0, entries);
        }
    }
}
