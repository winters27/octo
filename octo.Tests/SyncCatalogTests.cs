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
using Microsoft.Extensions.Logging;
using Microsoft.Extensions.Options;
using Moq;
using Octo.Models.Domain;
using Octo.Models.Radio;
using Octo.Models.Settings;
using Octo.Services;
using Octo.Services.LastFm;
using Octo.Services.Soulseek;
using Octo.Services.Subsonic;

namespace Octo.Tests;

public sealed class SyncCatalogTests
{
    // -------------------------------------------------------------------------------
    // Who gets it
    // -------------------------------------------------------------------------------

    [Theory]
    [InlineData("Symfonium", true)]
    [InlineData("symfonium", true)]
    [InlineData("Symfonium (Android)", true)]
    [InlineData("Feishin", false)]
    [InlineData("", false)]
    [InlineData(null, false)]
    public void IsSyncClient_MatchesTheConfiguredNames(string? client, bool expected)
    {
        var settings = new SubsonicSettings();
        Assert.Equal(expected, SyncCatalogService.IsSyncClient(settings, client));
    }

    [Fact]
    public void IsSyncClient_IsOffWhenTheFeatureIs_AndTakesAList()
    {
        Assert.False(SyncCatalogService.IsSyncClient(new SubsonicSettings { EnableSyncCatalog = false }, "Symfonium"));
        var listed = new SubsonicSettings { SyncCatalogClients = " Symfonium , Substreamer " };
        Assert.True(SyncCatalogService.IsSyncClient(listed, "Substreamer"));
        Assert.False(SyncCatalogService.IsSyncClient(listed, "Feishin"));
    }

    // -------------------------------------------------------------------------------
    // Paging
    // -------------------------------------------------------------------------------

    [Theory]
    [InlineData(0, 0, 0)]       // empty library: the first page proves it
    [InlineData(2000, 500, 2500)]
    [InlineData(0, 300, 300)]
    public void LocalTotalFromPage_IsKnownFromAPageWithLibraryRows(int offset, int returned, int expected) =>
        Assert.Equal(expected, SyncCatalogService.LocalTotalFromPage(offset, returned));

    [Fact]
    public void LocalTotalFromPage_IsUnknownFromAnEmptyLaterPage() =>
        Assert.Null(SyncCatalogService.LocalTotalFromPage(3000, 0));

    [Theory]
    [InlineData(2000, 1000, 500, 2500, 0, 500)]    // the page the library ends on
    [InlineData(3000, 1000, 0, 2500, 500, 1000)]   // the page after it
    [InlineData(2000, 1000, 0, 2000, 0, 1000)]     // library a multiple of the page size
    public void Window_AddressesTheCatalogAfterTheLibrary(int offset, int count, int returned, int total,
        int start, int take) =>
        Assert.Equal((start, take), SyncCatalogService.Window(offset, count, returned, total));

    [Theory]
    [InlineData(2500, 3000)]
    [InlineData(2000, 2000)]
    [InlineData(0, 1000)]
    [InlineData(1, 4000)]
    public async Task ResolveLocalTotal_FindsTheLibrarySizeWithoutAHint(int total, int emptyOffset)
    {
        var probes = 0;
        var found = await SyncCatalogService.ResolveLocalTotalAsync(emptyOffset, null,
            index => { probes++; return Task.FromResult<bool?>(index < total); });
        Assert.Equal(total, found);
        Assert.True(probes <= 13, $"{probes} probes");
    }

    [Fact]
    public async Task ResolveLocalTotal_TrustsARememberedSizeOnlyWhenTheProbesConfirmIt()
    {
        var probes = 0;
        Task<bool?> Library(int index, int total) { probes++; return Task.FromResult<bool?>(index < total); }

        Assert.Equal(2500, await SyncCatalogService.ResolveLocalTotalAsync(3000, 2500, index => Library(index, 2500)));
        Assert.Equal(2, probes);

        // The library grew since the walk remembered it: bisect instead of trusting it.
        Assert.Equal(2600, await SyncCatalogService.ResolveLocalTotalAsync(3000, 2500, index => Library(index, 2600)));
        Assert.Equal(1800, await SyncCatalogService.ResolveLocalTotalAsync(3000, 2500, index => Library(index, 1800)));
    }

    [Fact]
    public async Task ResolveLocalTotal_GivesUpWhenAProbeFails() =>
        Assert.Null(await SyncCatalogService.ResolveLocalTotalAsync(3000, null,
            _ => Task.FromResult<bool?>(null)));

    [Fact]
    public void Interleave_TakesFromEveryStationInTurn_WithoutRepeats()
    {
        LastFmRadioStation Station(params (string Artist, string Title)[] tracks) => new()
        {
            Tracks = tracks.Select(track => new LastFmRadioTrack { Artist = track.Artist, Title = track.Title }).ToList(),
        };
        var stations = new[]
        {
            Station(("A", "1"), ("A", "2"), ("A", "3")),
            Station(("B", "1"), ("a", "1"), ("B", "2")),
        };

        var all = SyncCatalogService.Interleave(stations, 100);
        Assert.Equal(["A|1", "B|1", "A|2", "A|3", "B|2"], all.Select(track => track.Artist + "|" + track.Title));

        var capped = SyncCatalogService.Interleave(stations, 3);
        Assert.Equal(["A|1", "B|1", "A|2"], capped.Select(track => track.Artist + "|" + track.Title));
    }

    // -------------------------------------------------------------------------------
    // What the library already holds
    // -------------------------------------------------------------------------------

    [Fact]
    public void ParseLibraryArtist_MatchesTheArtistExactly_AndCollectsItsAlbumsAndSongs()
    {
        var body = Encoding.UTF8.GetBytes("""
            {"subsonic-response":{"status":"ok","searchResult3":{
              "artist":[{"id":"ar-airbourne","name":"Airbourne"},{"id":"ar-air","name":"Air"}],
              "album":[{"id":"al-moon","name":"Moon Safari","artist":"Air","artistId":"ar-air"},
                       {"id":"al-rock","name":"Runnin' Wild","artist":"Airbourne","artistId":"ar-airbourne"}],
              "song":[{"id":"s1","artist":"Air","title":"La Femme d'Argent"}]}}}
            """);

        var air = SyncCatalogService.ParseLibraryArtist(body, "AIR")!;
        Assert.Equal("ar-air", air.ArtistId);
        Assert.Equal("al-moon", Assert.Single(air.AlbumIds).Value);
        Assert.True(air.Owns("Air", "La Femme d'Argent"));
        Assert.False(air.Owns("Air", "Sexy Boy"));

        var stranger = SyncCatalogService.ParseLibraryArtist(body, "Aire")!;
        Assert.Null(stranger.ArtistId);
        Assert.Empty(stranger.AlbumIds);
    }

    [Fact]
    public void ParseLibraryArtist_IsNullForAFailedResponse() =>
        Assert.Null(SyncCatalogService.ParseLibraryArtist(Encoding.UTF8.GetBytes(
            """{"subsonic-response":{"status":"failed","error":{"code":40}}}"""), "Air"));

    // -------------------------------------------------------------------------------
    // Writing the page
    // -------------------------------------------------------------------------------

    private static SubsonicResponseBuilder Builder() =>
        new(new ExternalIdRegistry(), Options.Create(new SubsonicSettings()));

    private static SyncCatalog Catalog(DateTime added) =>
        new([new Song { Id = "cat-song", Title = "New One", Artist = "Stranger", Album = "New One",
                AlbumId = "cat-album", ArtistId = "cat-artist", Duration = 200 }],
            [new Album { Id = "cat-album", Title = "New One", Artist = "Stranger", ArtistId = "cat-artist", SongCount = 1 }],
            [new Artist { Id = "cat-artist", Name = "Stranger", AlbumCount = 1 }],
            new Dictionary<string, DateTime> { ["cat-song"] = added, ["cat-album"] = added, ["cat-artist"] = added },
            "fp", added);

    [Fact]
    public void AppendJson_AddsRowsAfterTheLibrary_WithTheirCatalogDate()
    {
        var added = new DateTime(2026, 9, 1, 12, 0, 0, DateTimeKind.Utc);
        var catalog = Catalog(added);
        var body = Encoding.UTF8.GetBytes(
            """{"subsonic-response":{"status":"ok","version":"1.16.1","searchResult3":{"song":[{"id":"lib-1","title":"Mine"}]}}}""");

        var output = SyncCatalogResponse.Append(body, "application/json", "searchResult3", Builder(), catalog,
            catalog.Artists, catalog.Albums, catalog.Songs);

        var result = JsonNode.Parse(output)!["subsonic-response"]!["searchResult3"]!;
        Assert.Equal(["lib-1", "cat-song"], result["song"]!.AsArray().Select(row => row!["id"]!.GetValue<string>()));
        Assert.Equal("2026-09-01T12:00:00.000Z", result["song"]![1]!["created"]!.GetValue<string>());
        Assert.Equal("cat-album", result["song"]![1]!["albumId"]!.GetValue<string>());
        Assert.Equal("cat-album", result["album"]![0]!["id"]!.GetValue<string>());
        Assert.Equal("cat-artist", result["artist"]![0]!["id"]!.GetValue<string>());
        Assert.Equal((1, 1, 2), SyncCatalogResponse.CountRows(output, "application/json", "searchResult3"));
    }

    [Fact]
    public void AppendJson_CreatesTheEnvelopeAnEmptyPageLeftOut()
    {
        var catalog = Catalog(DateTime.UtcNow);
        var body = Encoding.UTF8.GetBytes("""{"subsonic-response":{"status":"ok","version":"1.16.1"}}""");
        var output = SyncCatalogResponse.Append(body, "application/json", "searchResult3", Builder(), catalog,
            [], [], catalog.Songs);
        Assert.Equal((0, 0, 1), SyncCatalogResponse.CountRows(output, "application/json", "searchResult3"));
    }

    [Fact]
    public void AppendXml_KeepsSchemaOrder()
    {
        var catalog = Catalog(DateTime.UtcNow);
        var body = Encoding.UTF8.GetBytes("""
            <subsonic-response xmlns="http://subsonic.org/restapi" status="ok" version="1.16.1">
              <searchResult3><artist id="lib-ar" name="Mine"/><song id="lib-1" title="Mine"/></searchResult3>
            </subsonic-response>
            """);

        var output = SyncCatalogResponse.Append(body, "text/xml", "searchResult3", Builder(), catalog,
            catalog.Artists, catalog.Albums, catalog.Songs);

        var rows = XDocument.Parse(Encoding.UTF8.GetString(output)).Root!.Elements().Single().Elements()
            .Select(row => row.Name.LocalName + ":" + row.Attribute("id")!.Value).ToList();
        Assert.Equal(["artist:lib-ar", "artist:cat-artist", "album:cat-album", "song:lib-1", "song:cat-song"], rows);
    }

    // -------------------------------------------------------------------------------
    // A whole sync, the way Symfonium walks it
    // -------------------------------------------------------------------------------

    private static async Task<List<JsonElement>> WalkAsync(HttpClient client, string kind, int pageSize,
        string clientName = "Symfonium", string extra = "")
    {
        var rows = new List<JsonElement>();
        for (var offset = 0; ; offset += pageSize)
        {
            var counts = string.Join("&", new[] { "song", "album", "artist" }.Select(name =>
                $"{name}Count={(name == kind ? pageSize : 0)}&{name}Offset={(name == kind ? offset : 0)}"));
            var body = await client.GetStringAsync(
                $"/rest/search3.view?query=%22%22&{counts}&u=alice&t=token&s=salt&v=1.13.0&c={clientName}&f=json{extra}");
            using var document = JsonDocument.Parse(body);
            var page = document.RootElement.GetProperty("subsonic-response").TryGetProperty("searchResult3", out var result)
                && result.TryGetProperty(kind, out var array) ? array.EnumerateArray().Select(row => row.Clone()).ToList() : [];
            rows.AddRange(page);
            if (page.Count < pageSize) return rows;
            Assert.True(offset < 100_000, "walk never ended");
        }
    }

    private static List<string> Ids(IEnumerable<JsonElement> rows) =>
        rows.Select(row => row.GetProperty("id").GetString()!).ToList();

    [Theory]
    [InlineData(2500, 1000)]   // the library ends part way through a page
    [InlineData(2000, 1000)]   // the library ends exactly on a page boundary
    [InlineData(12, 5)]        // the catalog spans several pages of its own
    [InlineData(0, 100)]       // nothing in the library at all
    public async Task SongWalk_ReturnsTheWholeLibraryThenTheCatalog_OnceEach(int librarySize, int pageSize)
    {
        await using var fixture = new SyncWebFactory(librarySize);
        fixture.InstallStations();
        using var client = fixture.CreateClient();

        var ids = Ids(await WalkAsync(client, "song", pageSize));

        Assert.Equal(ids.Count, ids.Distinct().Count());
        Assert.Equal(fixture.Upstream.LibrarySongIds, ids.Take(librarySize));
        var catalog = ids.Skip(librarySize).ToList();
        Assert.Equal(SyncWebFactory.ExpectedCatalogTitles.Count, catalog.Count);
        Assert.All(catalog, id => Assert.StartsWith("ph-", id));
    }

    [Fact]
    public async Task SongWalk_LeavesOutWhatTheLibraryOwns_AndFilesUnderTheLibrarysArtistAndAlbum()
    {
        await using var fixture = new SyncWebFactory(30);
        fixture.InstallStations();
        using var client = fixture.CreateClient();

        var catalog = (await WalkAsync(client, "song", 1000)).Skip(30).ToList();
        var titles = catalog.Select(row => row.GetProperty("title").GetString()).ToList();
        Assert.DoesNotContain("Owned Hit", titles);
        Assert.Equal(SyncWebFactory.ExpectedCatalogTitles.Order(), titles.Order());

        var missing = catalog.Single(row => row.GetProperty("title").GetString() == "Missing Hit");
        Assert.Equal("ar-owned", missing.GetProperty("artistId").GetString());
        Assert.Equal("al-owned", missing.GetProperty("albumId").GetString());

        var stranger = catalog.First(row => row.GetProperty("artist").GetString() == "Stranger");
        Assert.NotEqual("ar-owned", stranger.GetProperty("artistId").GetString());
    }

    [Fact]
    public async Task AlbumAndArtistWalks_AddOnlyWhatTheLibraryDoesNotHave_AndMatchTheSongs()
    {
        await using var fixture = new SyncWebFactory(30);
        fixture.InstallStations();
        using var client = fixture.CreateClient();

        var songs = (await WalkAsync(client, "song", 1000)).Skip(30).ToList();
        var albums = (await WalkAsync(client, "album", 500)).Skip(fixture.Upstream.LibraryAlbumCount).ToList();
        var artists = (await WalkAsync(client, "artist", 500)).Skip(fixture.Upstream.LibraryArtistCount).ToList();

        Assert.DoesNotContain("al-owned", Ids(albums));
        Assert.DoesNotContain("ar-owned", Ids(artists));
        Assert.Equal(["Stranger"], artists.Select(row => row.GetProperty("name").GetString()));

        var songAlbums = songs.Select(row => row.GetProperty("albumId").GetString()).Where(id => id != "al-owned");
        Assert.Equal(songAlbums.Distinct().Order(), Ids(albums).Order());
    }

    [Theory]
    [InlineData("Feishin", "")]
    [InlineData("Symfonium", "&musicFolderId=1")]
    public async Task OtherClientsAndFolderWalks_GetTheLibraryUnchanged(string clientName, string extra)
    {
        await using var fixture = new SyncWebFactory(120);
        fixture.InstallStations();
        using var client = fixture.CreateClient();

        var ids = Ids(await WalkAsync(client, "song", 50, clientName, extra));

        Assert.Equal(fixture.Upstream.LibrarySongIds, ids);
    }

    [Fact]
    public async Task AShortPageFromAServerCap_IsNotMistakenForTheEndOfTheLibrary()
    {
        await using var fixture = new SyncWebFactory(100);
        fixture.Upstream.PageCap = 30;
        fixture.InstallStations();
        using var client = fixture.CreateClient();

        var ids = Ids(await WalkAsync(client, "song", 50));

        Assert.Equal(fixture.Upstream.LibrarySongIds.Take(30), ids);
    }

    [Fact]
    public async Task StationPlaylist_DescribesItsSongsAsTheSyncDid()
    {
        await using var fixture = new SyncWebFactory(10);
        fixture.InstallStations();
        using var client = fixture.CreateClient();
        var synced = (await WalkAsync(client, "song", 1000)).Skip(10)
            .Single(row => row.GetProperty("title").GetString() == "Missing Hit");

        var body = await client.GetStringAsync(
            $"/rest/getPlaylist?id={fixture.StationId}&u=alice&t=token&s=salt&f=json");
        using var document = JsonDocument.Parse(body);
        var entry = document.RootElement.GetProperty("subsonic-response").GetProperty("playlist")
            .GetProperty("entry").EnumerateArray()
            .Single(row => row.GetProperty("title").GetString() == "Missing Hit");

        Assert.Equal(synced.GetProperty("id").GetString(), entry.GetProperty("id").GetString());
        Assert.Equal("al-owned", entry.GetProperty("albumId").GetString());
        Assert.Equal("ar-owned", entry.GetProperty("artistId").GetString());
    }
}

/// <summary>An Octo in front of a fake Navidrome with a library of <c>librarySize</c> songs.</summary>
internal sealed class SyncWebFactory : WebApplicationFactory<Program>
{
    private readonly string _directory = Path.Combine(Path.GetTempPath(), "octo-sync-web-" + Guid.NewGuid());
    public SyncUpstreamHandler Upstream { get; }
    public Mock<IMusicMetadataService> Metadata { get; } = new();
    public LastFmRadioStateStore State => Services.GetRequiredService<LastFmRadioStateStore>();
    public string StationId => LastFmRadioStateStore.StationId("alice", "your-mix");

    /// <summary>The station tracks the library does not own.</summary>
    public static readonly IReadOnlyList<string> ExpectedCatalogTitles =
        ["Missing Hit", .. Enumerable.Range(1, 12).Select(index => $"Stranger Song {index}"), "Loner"];

    public SyncWebFactory(int librarySize)
    {
        Upstream = new SyncUpstreamHandler(librarySize);
        Directory.CreateDirectory(_directory);
        Metadata.Setup(service => service.SearchSongsByArtistTitleAsync(
                It.IsAny<string>(), It.IsAny<string>(), It.IsAny<int>(), It.IsAny<int?>()))
            .ReturnsAsync((string artist, string title, int _, int? duration) => new List<Song>
            {
                new() { Id = "ph-" + (artist + "-" + title).Replace(' ', '-'), Artist = artist, Title = title,
                    Album = "", Duration = duration ?? 180, IsLocal = false, ExternalProvider = "soulseek" },
            });
        Metadata.Setup(service => service.PrewarmYouTubeIdsAsync(
                It.IsAny<IEnumerable<Song>>(), It.IsAny<int>(), It.IsAny<CancellationToken>()))
            .Returns(Task.CompletedTask);
    }

    public void InstallStations()
    {
        // Artists alternate at the front because a station playlist drops a track by the
        // artist it has just played.
        var tracks = Enumerable.Range(1, 12).Select(index => new LastFmRadioTrack
            { Artist = "Stranger", Title = $"Stranger Song {index}", Album = index <= 6 ? "First Record" : "Second Record", Duration = 200 })
            .ToList();
        tracks.Insert(0, new LastFmRadioTrack { Artist = "Owned Artist", Title = "Owned Hit", Duration = 180 });
        tracks.Insert(2, new LastFmRadioTrack { Artist = "Owned Artist", Title = "Missing Hit", Album = "Owned Album", Duration = 190 });
        tracks.Add(new LastFmRadioTrack { Artist = "Stranger", Title = "Loner", Duration = 150 });

        State.ReplaceStations("alice", [new LastFmRadioStation
        {
            Id = StationId, Key = "your-mix", Name = "Your Mix", Owner = "alice",
            Kind = LastFmRadioStationKind.YourMix, Personalized = true,
            CreatedUtc = new DateTime(2026, 9, 1, 1, 0, 0, DateTimeKind.Utc),
            ChangedUtc = new DateTime(2026, 9, 1, 2, 0, 0, DateTimeKind.Utc),
            ValidUntilUtc = DateTime.UtcNow.AddDays(1),
            Tracks = tracks,
        }]);
    }

    protected override void ConfigureWebHost(IWebHostBuilder builder)
    {
        builder.ConfigureAppConfiguration((_, configuration) => configuration.AddInMemoryCollection(
            new Dictionary<string, string?>
            {
                ["Subsonic:Url"] = "http://navidrome.test",
                ["Subsonic:AutoDetectDownloadPath"] = "false",
                ["Library:DownloadPath"] = _directory,
                ["LastFm:EnableRadio"] = "true",
                ["LastFm:EnablePersonalizedStations"] = "true",
                ["LastFm:ExposeRadioAsPlaylists"] = "true",
            }));
        builder.ConfigureServices(services =>
        {
            services.RemoveAll<IHostedService>();
            services.RemoveAll<IHttpClientFactory>();
            services.AddSingleton<IHttpClientFactory>(new Factory(Upstream));
            services.RemoveAll<IMusicMetadataService>();
            services.AddSingleton(Metadata.Object);
            services.RemoveAll<LastFmRadioStateStore>();
            services.AddSingleton(provider => new LastFmRadioStateStore(
                Path.Combine(_directory, "radio-state.json"),
                provider.GetRequiredService<IOptionsMonitor<LastFmSettings>>(),
                provider.GetRequiredService<ExternalIdRegistry>(),
                provider.GetRequiredService<ILogger<LastFmRadioStateStore>>()));
        });
    }

    public override async ValueTask DisposeAsync()
    {
        await base.DisposeAsync();
        try { Directory.Delete(_directory, true); } catch { }
    }

    private sealed class Factory(SyncUpstreamHandler handler) : IHttpClientFactory
    {
        public HttpClient CreateClient(string name) => new(handler, disposeHandler: false);
    }
}

/// <summary>
/// Navidrome as far as a sync needs it: empty-query search3 pages over a fixed library, and
/// search3 by artist name for the catalog's library lookups.
/// </summary>
internal sealed class SyncUpstreamHandler(int librarySize) : HttpMessageHandler
{
    public IReadOnlyList<string> LibrarySongIds { get; } =
        Enumerable.Range(0, librarySize).Select(index => $"lib-{index}").ToList();
    public int LibraryAlbumCount => 3;

    /// <summary>The most rows one page returns, whatever was asked for.</summary>
    public int PageCap { get; set; } = int.MaxValue;
    public int LibraryArtistCount => 2;

    protected override Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken cancellationToken)
    {
        var path = request.RequestUri!.AbsolutePath.Trim('/');
        var query = System.Web.HttpUtility.ParseQueryString(request.RequestUri.Query);
        if (!path.StartsWith("rest/search3", StringComparison.Ordinal)) return Ok("");

        int Number(string name) => int.TryParse(query[name], out var value)
            ? name.EndsWith("Count", StringComparison.Ordinal) ? Math.Min(value, PageCap) : value
            : 20;
        var term = (query["query"] ?? "").Trim().Trim('"');
        if (term.Length == 0)
        {
            var songs = LibrarySongIds.Skip(Number("songOffset")).Take(Number("songCount"))
                .Select(id => new JsonObject { ["id"] = id, ["title"] = "Song " + id, ["artist"] = "Owned Artist" });
            var albums = Enumerable.Range(0, LibraryAlbumCount).Skip(Number("albumOffset")).Take(Number("albumCount"))
                .Select(index => new JsonObject { ["id"] = index == 0 ? "al-owned" : $"al-{index}", ["name"] = index == 0 ? "Owned Album" : $"Album {index}" });
            var artists = Enumerable.Range(0, LibraryArtistCount).Skip(Number("artistOffset")).Take(Number("artistCount"))
                .Select(index => new JsonObject { ["id"] = index == 0 ? "ar-owned" : $"ar-{index}", ["name"] = index == 0 ? "Owned Artist" : $"Artist {index}" });
            return Ok(Result(songs, albums, artists));
        }

        if (term == "Owned Artist")
            return Ok(Result(
                [new JsonObject { ["id"] = "lib-owned-hit", ["title"] = "Owned Hit", ["artist"] = "Owned Artist" }],
                [new JsonObject { ["id"] = "al-owned", ["name"] = "Owned Album", ["artist"] = "Owned Artist", ["artistId"] = "ar-owned" }],
                [new JsonObject { ["id"] = "ar-owned", ["name"] = "Owned Artist" }]));
        return Ok(Result([], [], []));
    }

    private static string Result(IEnumerable<JsonObject> songs, IEnumerable<JsonObject> albums, IEnumerable<JsonObject> artists)
    {
        var result = new JsonObject();
        JsonArray Array(IEnumerable<JsonObject> rows) => new(rows.Select(row => (JsonNode)row).ToArray());
        var songArray = Array(songs); var albumArray = Array(albums); var artistArray = Array(artists);
        if (artistArray.Count > 0) result["artist"] = artistArray;
        if (albumArray.Count > 0) result["album"] = albumArray;
        if (songArray.Count > 0) result["song"] = songArray;
        return new JsonObject
        {
            ["subsonic-response"] = new JsonObject
            {
                ["status"] = "ok", ["version"] = "1.16.1", ["searchResult3"] = result,
            },
        }.ToJsonString();
    }

    private static Task<HttpResponseMessage> Ok(string body) =>
        Task.FromResult(new HttpResponseMessage(HttpStatusCode.OK)
        {
            Content = new StringContent(body.Length == 0
                ? """{"subsonic-response":{"status":"ok","version":"1.16.1"}}""" : body,
                Encoding.UTF8, "application/json"),
        });
}
