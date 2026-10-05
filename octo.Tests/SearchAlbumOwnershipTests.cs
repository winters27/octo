using System.Net;
using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;
using System.Xml.Linq;
using Microsoft.AspNetCore.Hosting;
using Microsoft.AspNetCore.Mvc.Testing;
using Microsoft.Extensions.Configuration;
using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.DependencyInjection.Extensions;
using Microsoft.Extensions.Hosting;
using Microsoft.Extensions.Logging.Abstractions;
using Octo.Services.Library;

namespace Octo.Tests;

/// <summary>
/// Brandon's "drake" search through the real controller: search3 listed HABIBTI, HABIBTI
/// (FOMO) and "$ome $exy $ongs 4 U" as not in the library while the library held them, and
/// What A Time To Be Alive as if none of it were owned. See <see cref="DrakeSearch"/>.
/// </summary>
public sealed class SearchAlbumOwnershipTests
{
    private static readonly XNamespace Ns = "http://subsonic.org/restapi";
    private const string Auth = "u=alice&t=good&s=salt&v=1.16.1&c=test";

    private static readonly (long Id, string Title, string Artist, IReadOnlyList<AlbumOwnership.CatalogTrack> Tracks)[] Catalog =
    [
        (983217461, "HABIBTI", "Drake", DrakeSearch.Habibti),
        (1110918492, "HABIBTI (FOMO)", "Drake", DrakeSearch.HabibtiFomo),
        (712442261, "$ome $exy $ongs 4 U", "PARTYNEXTDOOR", DrakeSearch.SomeSexySongs),
        (11250516, "What A Time To Be Alive", "Drake", DrakeSearch.WhatATime),
        (983225601, "MAID OF HONOUR", "Drake", DrakeSearch.MaidOfHonour),
    ];

    private sealed class FakeServers : HttpMessageHandler
    {
        public readonly System.Collections.Concurrent.ConcurrentQueue<string> Calls = new();

        protected override Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken ct)
        {
            var uri = request.RequestUri!;
            var path = uri.AbsolutePath;
            var query = System.Web.HttpUtility.ParseQueryString(uri.Query);
            Calls.Enqueue(uri.Host + path + (query["id"] is { } id ? "?id=" + id : ""));

            if (uri.Host == "api.deezer.com")
            {
                if (path.StartsWith("/search/album", StringComparison.Ordinal))
                    return Json(new JsonObject
                    {
                        ["data"] = new JsonArray(Catalog.Select(album => (JsonNode)new JsonObject
                        {
                            ["id"] = album.Id, ["title"] = album.Title, ["record_type"] = "album",
                            ["nb_tracks"] = album.Tracks.Count, ["artist"] = new JsonObject { ["name"] = album.Artist },
                        }).ToArray()),
                    }.ToJsonString());
                foreach (var album in Catalog)
                {
                    if (path == $"/album/{album.Id}")
                        return Json(new JsonObject
                        {
                            ["id"] = album.Id, ["title"] = album.Title, ["nb_tracks"] = album.Tracks.Count,
                            ["release_date"] = "2026-01-01", ["artist"] = new JsonObject { ["name"] = album.Artist },
                        }.ToJsonString());
                    if (path == $"/album/{album.Id}/tracks")
                        return Json(new JsonObject
                        {
                            ["total"] = album.Tracks.Count,
                            ["data"] = new JsonArray(album.Tracks.Select((track, i) => (JsonNode)new JsonObject
                            {
                                ["title"] = track.Title, ["duration"] = track.Duration, ["track_position"] = i + 1,
                                ["disk_number"] = 1, ["isrc"] = track.Isrc, ["artist"] = new JsonObject { ["name"] = track.Artist },
                            }).ToArray()),
                        }.ToJsonString());
                }
                if (path.StartsWith("/search/artist", StringComparison.Ordinal))
                    return Json("""{"data":[{"id":246791,"name":"Drake","nb_fan":1000}]}""");
                if (path.StartsWith("/artist/246791/albums", StringComparison.Ordinal))
                    return Json(new JsonObject
                    {
                        ["data"] = new JsonArray(Catalog.Where(album => album.Artist == "Drake").Select(album => (JsonNode)new JsonObject
                        {
                            ["id"] = album.Id, ["title"] = album.Title, ["record_type"] = "album", ["release_date"] = "2026-01-01",
                        }).ToArray()),
                    }.ToJsonString());
                return Task.FromResult(new HttpResponseMessage(HttpStatusCode.NotFound));
            }

            var json = query["f"] == "json";
            if (path.EndsWith("/rest/getArtist", StringComparison.Ordinal))
                return Json("""
                    {"subsonic-response":{"status":"ok","version":"1.16.1","artist":{"id":"ar-drake","name":"Drake","albumCount":2,"album":[
                      {"id":"al-fomo","name":"HABIBTI (FOMO)","artist":"Drake","songCount":15},
                      {"id":"al-takecare","name":"Take Care","artist":"Drake","songCount":2}]}}}
                    """);
            if (path.EndsWith("/rest/search3", StringComparison.Ordinal))
            {
                // Navidrome's own answer for "drake": the Drake-credited part of the split
                // album, not the expanded HABIBTI (credited apart) nor the other part.
                return json
                    ? Json("""
                        {"subsonic-response":{"status":"ok","version":"1.16.1","searchResult3":{
                          "album":[{"id":"al-takecare","name":"Take Care","artist":"Drake","songCount":2},
                                   {"id":"3jmLzEVlwtrlq8XfVjei9k","name":"$ome $exy $ongs 4 U","artist":"Drake","songCount":5}]}}}
                        """)
                    : Xml("""<subsonic-response xmlns="http://subsonic.org/restapi" status="ok" version="1.16.1"><searchResult3><album id="al-takecare" name="Take Care" artist="Drake" songCount="2"/><album id="3jmLzEVlwtrlq8XfVjei9k" name="$ome $exy $ongs 4 U" artist="Drake" songCount="5"/></searchResult3></subsonic-response>""");
            }
            if (path.EndsWith("/rest/getAlbum", StringComparison.Ordinal)
                && DrakeSearch.Library.Where(row => row.AlbumId == query["id"]).ToList() is { Count: > 0 } rows)
            {
                if (!json)
                    return Xml($"""<subsonic-response xmlns="http://subsonic.org/restapi" status="ok" version="1.16.1"><album id="{rows[0].AlbumId}" name="{rows[0].Album}" artist="{rows[0].AlbumArtist}" songCount="{rows.Count}">{string.Concat(rows.Select((row, i) => $"""<song id="{row.Id}" title="{row.Title}" track="{i + 1}"/>"""))}</album></subsonic-response>""");
                return Json(new JsonObject
                {
                    ["subsonic-response"] = new JsonObject
                    {
                        ["status"] = "ok", ["version"] = "1.16.1",
                        ["album"] = new JsonObject
                        {
                            ["id"] = rows[0].AlbumId, ["name"] = rows[0].Album, ["artist"] = rows[0].AlbumArtist,
                            ["songCount"] = rows.Count,
                            ["song"] = new JsonArray(rows.Select((row, i) => (JsonNode)new JsonObject
                            {
                                ["id"] = row.Id, ["title"] = row.Title, ["album"] = row.Album, ["albumId"] = row.AlbumId,
                                ["artist"] = row.Artist, ["track"] = i + 1, ["duration"] = row.Duration, ["suffix"] = "flac",
                                ["path"] = row.Path,
                            }).ToArray()),
                        },
                    },
                }.ToJsonString());
            }
            if (path.EndsWith("/rest/ping", StringComparison.Ordinal))
                return Json("""{"subsonic-response":{"status":"ok","version":"1.16.1"}}""");
            return Task.FromResult(new HttpResponseMessage(HttpStatusCode.NotFound));
        }

        private static Task<HttpResponseMessage> Json(string body) =>
            Task.FromResult(new HttpResponseMessage(HttpStatusCode.OK) { Content = new StringContent(body, Encoding.UTF8, "application/json") });

        private static Task<HttpResponseMessage> Xml(string body) =>
            Task.FromResult(new HttpResponseMessage(HttpStatusCode.OK) { Content = new StringContent(body, Encoding.UTF8, "application/xml") });
    }

    private sealed class WebFactory : WebApplicationFactory<Program>
    {
        private readonly string _directory = Path.Combine(Path.GetTempPath(), "octo-ownership-web-" + Guid.NewGuid());
        public FakeServers Servers { get; } = new();
        public bool LibraryReadable { get; init; } = true;

        protected override void ConfigureWebHost(IWebHostBuilder builder)
        {
            Directory.CreateDirectory(_directory);
            builder.ConfigureAppConfiguration((_, configuration) => configuration.AddInMemoryCollection(
                new Dictionary<string, string?>
                {
                    ["Subsonic:Url"] = "http://navidrome.test",
                    ["Subsonic:AutoDetectDownloadPath"] = "false",
                    ["Soulseek:BaseUrl"] = "http://127.0.0.1:1",
                    ["YouTube:ShimUrl"] = "http://127.0.0.1:1",
                    ["Library:DownloadPath"] = _directory,
                }));
            builder.ConfigureServices(services =>
            {
                services.RemoveAll<IHostedService>();
                services.RemoveAll<IHttpClientFactory>();
                services.AddSingleton<IHttpClientFactory>(new ReviewFixtures.OneClientFactory(Servers));
                services.RemoveAll<LibrarySnapshot>();
                services.AddSingleton(sp => new LibrarySnapshot(sp.GetRequiredService<NavidromePlaylistApi>(), NullLogger<LibrarySnapshot>.Instance)
                {
                    ReadPage = (start, count, _) => Task.FromResult<(IReadOnlyList<LibrarySongRow>, int)?>(
                        LibraryReadable ? (DrakeSearch.Library.Skip(start).Take(count).ToList(), Math.Min(count, Math.Max(0, DrakeSearch.Library.Count - start))) : null),
                });
            });
        }

        public override async ValueTask DisposeAsync()
        {
            await base.DisposeAsync();
            try { Directory.Delete(_directory, recursive: true); } catch { /* best effort */ }
        }
    }

    private static async Task<List<JsonElement>> SearchAlbumsAsync(HttpClient client)
    {
        using var json = JsonDocument.Parse(await client.GetStringAsync(
            $"/rest/search3.view?{Auth}&f=json&query=drake&songCount=0&artistCount=0&albumCount=20"));
        return json.RootElement.GetProperty("subsonic-response").GetProperty("searchResult3").GetProperty("album")
            .EnumerateArray().Select(album => album.Clone()).ToList();
    }

    [Fact]
    public async Task Search_ListsHeldAlbumsAsTheLibrarys_AndCountsWhatIsHeldOfTheRest()
    {
        await using var factory = new WebFactory();
        using var client = factory.CreateClient();

        var albums = await SearchAlbumsAsync(client);
        var rows = albums.Select(a => (Name: a.GetProperty("name").GetString(), Id: a.GetProperty("id").GetString(),
            Outside: a.GetProperty("isExternal").GetBoolean())).ToList();

        // The library's own two, then the library albums standing in for HABIBTI (both
        // editions) and for "$ome $exy $ongs 4 U" (the part credited to PARTYNEXTDOOR), then
        // the two outside albums the library does not hold whole.
        Assert.Equal(
        [
            ("Take Care", "al-takecare", false),
            ("$ome $exy $ongs 4 U", DrakeSearch.SexyDrakePartId, false),
            ("HABIBTI (FOMO)", DrakeSearch.FomoId, false),
            ("$ome $exy $ongs 4 U", DrakeSearch.SexyPndPartId, false),
        ], rows.Take(4).Select(r => (r.Name, r.Id, r.Outside)));
        Assert.Equal(["What A Time To Be Alive", "MAID OF HONOUR"], rows.Skip(4).Select(r => r.Name));
        Assert.All(rows.Skip(4), r => Assert.True(r.Outside));

        var wattba = albums[4];
        Assert.Equal(2, wattba.GetProperty("ownedCount").GetInt32());
        Assert.Equal(11, wattba.GetProperty("songCount").GetInt32());
        Assert.Equal(0, albums[5].GetProperty("ownedCount").GetInt32());
        // An added library album is listed as Navidrome lists it, without its songs.
        Assert.False(albums[2].TryGetProperty("song", out _));
        Assert.False(albums[2].TryGetProperty("ownedCount", out _));
    }

    [Fact]
    public async Task Search_InXml_SaysTheSame()
    {
        await using var factory = new WebFactory();
        using var client = factory.CreateClient();

        var xml = XDocument.Parse(await client.GetStringAsync(
            $"/rest/search3.view?{Auth}&query=drake&songCount=0&artistCount=0&albumCount=20"));
        var albums = xml.Descendants(Ns + "album").ToList();

        Assert.Equal(["Take Care", "$ome $exy $ongs 4 U", "HABIBTI (FOMO)", "$ome $exy $ongs 4 U", "What A Time To Be Alive", "MAID OF HONOUR"],
            albums.Select(a => (string?)a.Attribute("name")));
        Assert.Empty(albums[2].Elements(Ns + "song"));
        Assert.Equal("2", (string?)albums[4].Attribute("ownedCount"));
        Assert.Equal("11", (string?)albums[4].Attribute("songCount"));
    }

    [Fact]
    public async Task Search_WithoutTheLibrary_IsAsBefore()
    {
        await using var factory = new WebFactory { LibraryReadable = false };
        using var client = factory.CreateClient();

        var albums = await SearchAlbumsAsync(client);

        // Only the exact name and artist the search itself listed is left out, as before.
        Assert.Equal(["Take Care", "$ome $exy $ongs 4 U", "HABIBTI", "HABIBTI (FOMO)", "$ome $exy $ongs 4 U", "What A Time To Be Alive", "MAID OF HONOUR"],
            albums.Select(a => a.GetProperty("name").GetString()));
        Assert.All(albums, a => Assert.False(a.TryGetProperty("ownedCount", out _)));
    }

    [Fact]
    public async Task ArtistPage_LeavesOutWhatItsAlbumsHoldWhole_AndCountsTheRest()
    {
        await using var factory = new WebFactory();
        using var client = factory.CreateClient();

        using var json = JsonDocument.Parse(await client.GetStringAsync($"/rest/getArtist.view?{Auth}&f=json&id=ar-drake"));
        var albums = json.RootElement.GetProperty("subsonic-response").GetProperty("artist").GetProperty("album")
            .EnumerateArray().ToDictionary(a => a.GetProperty("name").GetString()!, a => a.Clone());

        // HABIBTI is all inside the page's own HABIBTI (FOMO); the catalog's HABIBTI (FOMO) is that album.
        Assert.Equal(["HABIBTI (FOMO)", "Take Care", "What A Time To Be Alive", "MAID OF HONOUR"], albums.Keys);
        Assert.Equal(2, albums["What A Time To Be Alive"].GetProperty("ownedCount").GetInt32());
        Assert.Equal(0, albums["MAID OF HONOUR"].GetProperty("ownedCount").GetInt32());
        Assert.False(albums["HABIBTI (FOMO)"].GetProperty("isExternal").GetBoolean());
    }

    [Fact]
    public async Task OutsideAlbum_GivesTheSongsTheLibraryHoldsAsTheLibrarysCopies()
    {
        await using var factory = new WebFactory();
        using var client = factory.CreateClient();

        var wattbaId = (await SearchAlbumsAsync(client)).Single(a => a.GetProperty("name").GetString() == "What A Time To Be Alive")
            .GetProperty("id").GetString();

        using var json = JsonDocument.Parse(await client.GetStringAsync($"/rest/getAlbum.view?{Auth}&f=json&id={wattbaId}"));
        var album = json.RootElement.GetProperty("subsonic-response").GetProperty("album");
        var songs = album.GetProperty("song").EnumerateArray().ToList();

        Assert.Equal(11, songs.Count);
        Assert.Equal(11, album.GetProperty("songCount").GetInt32());
        Assert.Equal(2, album.GetProperty("ownedCount").GetInt32());
        Assert.Equal(DrakeSearch.WhatATime.Select(t => t.Title), songs.Select(s => s.GetProperty("title").GetString()));

        // The two the library holds are its own files, numbered as this album numbers them.
        var owned = songs.Where(s => !s.GetProperty("isExternal").GetBoolean()).ToList();
        Assert.Equal(["Live From The Gutter", "Jumpman"], owned.Select(s => s.GetProperty("title").GetString()));
        Assert.All(owned, s => Assert.StartsWith($"s-{DrakeSearch.WattbaId}-", s.GetProperty("id").GetString()));
        Assert.Equal([3, 9], owned.Select(s => s.GetProperty("track").GetInt32()));
        Assert.All(owned, s => Assert.EndsWith(".flac", s.GetProperty("path").GetString()));
        Assert.All(songs.Except(owned), s => Assert.True(s.GetProperty("isExternal").GetBoolean()));

        // XML says the same.
        var xml = XDocument.Parse(await client.GetStringAsync($"/rest/getAlbum.view?{Auth}&id={wattbaId}"));
        var xmlAlbum = xml.Root!.Element(Ns + "album")!;
        Assert.Equal("2", (string?)xmlAlbum.Attribute("ownedCount"));
        Assert.Equal(["false", "false"], xmlAlbum.Elements(Ns + "song")
            .Where(s => (string?)s.Attribute("title") is "Jumpman" or "Live From The Gutter")
            .Select(s => (string?)s.Attribute("isExternal")));
    }

    [Fact]
    public async Task OutsideAlbum_TheLibraryHoldsNoneOf_IsAsBefore_WithItsCount()
    {
        await using var factory = new WebFactory();
        using var client = factory.CreateClient();

        var maidId = (await SearchAlbumsAsync(client)).Single(a => a.GetProperty("name").GetString() == "MAID OF HONOUR")
            .GetProperty("id").GetString();
        using var json = JsonDocument.Parse(await client.GetStringAsync($"/rest/getAlbum.view?{Auth}&f=json&id={maidId}"));
        var album = json.RootElement.GetProperty("subsonic-response").GetProperty("album");

        Assert.Equal(0, album.GetProperty("ownedCount").GetInt32());
        Assert.All(album.GetProperty("song").EnumerateArray(), s => Assert.True(s.GetProperty("isExternal").GetBoolean()));
    }
}
