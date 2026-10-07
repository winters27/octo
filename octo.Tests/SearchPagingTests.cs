using System.Collections.Concurrent;
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
using Moq;
using Octo.Models.Domain;
using Octo.Models.Search;
using Octo.Services;

namespace Octo.Tests;

/// <summary>
/// A client scrolling a search's songs page by page, through the real controller. Page one
/// is the library's best matches, then outside songs; each later page has to carry on from
/// there, where it used to go straight to Navidrome and lose every outside song past page one.
/// </summary>
public sealed class SearchPagingTests
{
    // The library has 30 matches and Last.fm 25 outside songs, one of which (the third) is a
    // song the library already listed on page one, so the search leaves it out. The library
    // comes first and whole: with a 20-row page the search is l0-l19, the other 24 outside
    // songs, then l20-l29.
    private static readonly List<string> WholeSearch =
        SearchPagingWebFactory.Library.Take(20)
            .Concat(SearchPagingWebFactory.Outside.Where((_, index) => index != 2).Select(title => "ph-" + title))
            .Concat(SearchPagingWebFactory.Library.Skip(20))
            .ToList();

    private static async Task<List<string>> PageAsync(HttpClient client, int offset, int count,
        string endpoint = "search3", string format = "json", string user = "alice", string extra = "",
        string app = "Test")
    {
        var body = await client.GetStringAsync(
            $"/rest/{endpoint}.view?query=paging&songCount={count}&songOffset={offset}&albumCount=0&artistCount=0" +
            $"&u={user}&t=token&s=salt&v=1.16.1&c={app}&f={format}{extra}");
        if (format == "xml")
            return XDocument.Parse(body).Descendants().Where(e => e.Name.LocalName == "song")
                .Select(e => e.Attribute("id")!.Value).ToList();
        using var document = JsonDocument.Parse(body);
        var envelope = endpoint == "search2" ? "searchResult2" : "searchResult3";
        return document.RootElement.GetProperty("subsonic-response").GetProperty(envelope)
            .TryGetProperty("song", out var songs)
            ? songs.EnumerateArray().Select(song => song.GetProperty("id").GetString()!).ToList()
            : [];
    }

    [Theory]
    [InlineData("search3", "json")]
    [InlineData("search3", "xml")]
    [InlineData("search2", "json")]
    [InlineData("search2", "xml")]
    public async Task ThreePages_ShowTheWholeSearchOnce_InPageOnesOrder(string endpoint, string format)
    {
        await using var fixture = new SearchPagingWebFactory();
        using var client = fixture.CreateClient();

        var first = await PageAsync(client, 0, 20, endpoint, format);
        var second = await PageAsync(client, 20, 20, endpoint, format);
        var third = await PageAsync(client, 40, 20, endpoint, format);
        var past = await PageAsync(client, 60, 20, endpoint, format);

        // Page one is the library's own 20 best matches: no outside song takes a place while
        // the library has rows for it. The outside songs follow on the next pages, less the
        // one the library already has.
        Assert.Equal(WholeSearch.Take(20), first);
        Assert.Equal(20 + 20 + 14, first.Count + second.Count + third.Count);
        var all = first.Concat(second).Concat(third).ToList();
        Assert.Equal(all.Count, all.Distinct().Count());
        Assert.Equal(WholeSearch, all);
        Assert.Empty(past);

        // The library rows after the prefix came from Navidrome at the right places.
        var asked = fixture.Upstream.SongPages(endpoint);
        Assert.Contains((0, 20), asked);
        Assert.Contains((20, 16), asked);
    }

    [Fact]
    public async Task LaterPages_UsePageOnesOutsideSongs_EvenIfABuildNowWouldDiffer()
    {
        await using var fixture = new SearchPagingWebFactory();
        using var client = fixture.CreateClient();

        var first = await PageAsync(client, 0, 20);
        var calls = fixture.Upstream.LastFmCalls;
        fixture.Upstream.Reshuffle = true;
        var second = await PageAsync(client, 20, 20);

        Assert.Equal(calls, fixture.Upstream.LastFmCalls);
        Assert.Equal(WholeSearch.Skip(20).Take(20), second);
        Assert.Empty(first.Intersect(second));
    }

    [Fact]
    public async Task ChangingThePageSize_StillShowsEachRowOnce()
    {
        await using var fixture = new SearchPagingWebFactory();
        using var client = fixture.CreateClient();

        var all = await PageAsync(client, 0, 20);
        var offset = 20;
        foreach (var size in new[] { 15, 30, 7, 50 })
        {
            all.AddRange(await PageAsync(client, offset, size));
            offset += size;
        }

        Assert.Equal(WholeSearch, all);
    }

    /// <summary>
    /// Feishin's song search in Subsonic mode: the list asks for page one at its page size
    /// while getSongListCount walks the same query 500 rows at a time from offset 0, advancing
    /// by the rows it got. Both are page ones of one search; the list's next page must carry on
    /// from the list's own page one, not the count's.
    /// </summary>
    [Fact]
    public async Task AListAndItsCountWalk_KeepTheirOwnOrders()
    {
        await using var fixture = new SearchPagingWebFactory();
        using var client = fixture.CreateClient();

        async Task CountWalk()
        {
            var total = 0;
            while (await PageAsync(client, total, 500) is { Count: > 0 } rows) total += rows.Count;
        }
        // In the order that lost the list's page one: the count's page one lands after it.
        var first = await PageAsync(client, 0, 50);
        await CountWalk();
        var second = await PageAsync(client, 50, 50);
        var third = await PageAsync(client, 100, 50);

        var all = first.Concat(second).Concat(third).ToList();
        Assert.Equal(all.Count, all.Distinct().Count());
        Assert.Equal(WholeSearch.OrderBy(id => id), all.OrderBy(id => id));
        // The walk itself is not checked: a client that advances by the rows it got, after a
        // page one shorter than it asked for, lands inside page one's places. That is the
        // planner's to settle, not which order a page reads.
    }

    /// <summary>One person on two devices with different page sizes scrolls two lists.</summary>
    [Fact]
    public async Task TwoClientsOfOneUser_KeepTheirOwnOrders()
    {
        await using var fixture = new SearchPagingWebFactory();
        using var client = fixture.CreateClient();

        var phone = await PageAsync(client, 0, 20, app: "Phone");
        var desktop = await PageAsync(client, 0, 15, app: "Desktop");
        var phoneNext = await PageAsync(client, 20, 15, app: "Phone");
        var desktopNext = await PageAsync(client, 15, 15, app: "Desktop");

        Assert.Equal(WholeSearch.Take(20), phone);
        Assert.Equal(WholeSearch.Skip(20).Take(15), phoneNext);
        var desktopAll = desktop.Concat(desktopNext).ToList();
        Assert.Equal(desktopAll.Count, desktopAll.Distinct().Count());
        var cache = fixture.Services.GetRequiredService<Octo.Services.Subsonic.SearchSongOrderCache>();
        Assert.Equal(2, cache.Count);
    }

    [Fact]
    public async Task ALaterPageWithNothingRemembered_BuildsTheSameOrderAgain()
    {
        await using var fixture = new SearchPagingWebFactory();
        using var client = fixture.CreateClient();

        // Bob never asked for page one (as after a restart): his page two is rebuilt from a
        // fresh build and the library's prefix, and lands where Alice's did.
        await PageAsync(client, 0, 20, user: "alice");
        var alice = await PageAsync(client, 20, 20, user: "alice");
        var bob = await PageAsync(client, 20, 20, user: "bob");

        Assert.Equal(alice, bob);
    }

    [Fact]
    public async Task ATypeAheadSizedLaterPage_WithNothingRemembered_GoesToNavidromeUnchanged()
    {
        await using var fixture = new SearchPagingWebFactory();
        using var client = fixture.CreateClient();

        var page = await PageAsync(client, 10, 10);

        Assert.Equal(SearchPagingWebFactory.Library.Skip(10).Take(10), page);
        Assert.Equal(0, fixture.Upstream.LastFmCalls);
    }

    [Fact]
    public async Task WithDiscoveryOff_LaterPagesGoToNavidromeUnchanged()
    {
        await using var fixture = new SearchPagingWebFactory(discovery: false);
        using var client = fixture.CreateClient();

        await PageAsync(client, 0, 20);
        var second = await PageAsync(client, 20, 20);

        Assert.Equal(SearchPagingWebFactory.Library.Skip(20).Take(20), second);
    }

    /// <summary>A page that starts inside page one's rows is not that list's next page, even at
    /// page one's size: it used to be placed as if it were, and repeated rows the list had.</summary>
    [Fact]
    public void APageInsidePageOne_DoesNotBorrowItsOrder()
    {
        var cache = new Octo.Services.Subsonic.SearchSongOrderCache();
        var key = Octo.Services.Subsonic.SearchSongOrderCache.Key("alice", "Test", "rest/search3", null, "paging");
        Octo.Services.Subsonic.SearchSongOrder PageOne(int count) =>
            Octo.Services.Subsonic.SearchSongOrder.From([], count, count, 0, []);
        cache.Set(key, PageOne(20));
        cache.Set(key, PageOne(50));

        Assert.Equal(50, cache.Get(key, 50, 50)!.PageOneCount);
        Assert.Equal(20, cache.Get(key, 50, 30)!.PageOneCount);
        Assert.Null(cache.Get(key, 50, 10));
    }

    /// <summary>
    /// Navidrome failing on a later page's library rows used to leave a page of outside songs
    /// only, and the client never saw the rows it skipped. Now the page goes to Navidrome as
    /// it used to, and the next try, once Navidrome answers, is the page it should be.
    /// </summary>
    [Fact]
    public async Task ALaterPage_WhenNavidromeFails_IsNotMadeFromTheOrderAlone()
    {
        await using var fixture = new SearchPagingWebFactory();
        using var client = fixture.CreateClient();

        await PageAsync(client, 0, 20);
        fixture.Upstream.FailLaterSongPages = true;
        var failed = await client.GetStringAsync(
            "/rest/search3.view?query=paging&songCount=20&songOffset=40&albumCount=0&artistCount=0" +
            "&u=alice&t=token&s=salt&v=1.16.1&c=Test&f=json");
        fixture.Upstream.FailLaterSongPages = false;
        var second = await PageAsync(client, 40, 20);

        Assert.DoesNotContain("ph-", failed);
        Assert.Equal(WholeSearch.Skip(40).Take(20), second);
    }

    /// <summary>
    /// A song you own is listed once, as the library's copy, before any outside song, and the
    /// library is not cut short to make room for outside results. A page with room to spare
    /// fills the rest with outside songs, less the one already owned.
    /// </summary>
    [Theory]
    [InlineData(20, 20, 0)]
    [InlineData(40, 30, 9)]
    public async Task PageOne_ListsTheLibraryFirst_AndAnOwnedSongOnlyOnce(int count, int locals, int outsiders)
    {
        await using var fixture = new SearchPagingWebFactory();
        using var client = fixture.CreateClient();

        var first = await PageAsync(client, 0, count);

        Assert.Equal(SearchPagingWebFactory.Library.Take(locals), first.Take(locals));
        Assert.Equal(outsiders, first.Skip(locals).Count());
        Assert.All(first.Skip(locals), id => Assert.StartsWith("ph-", id));
        Assert.DoesNotContain("ph-Library Song 3", first);
        Assert.Equal(first.Count, first.Distinct().Count());
    }

    [Fact]
    public async Task ALaterAlbumPage_DoesNotRepeatTheOutsideAlbums()
    {
        await using var fixture = new SearchPagingWebFactory();
        using var client = fixture.CreateClient();

        await client.GetStringAsync("/rest/search3.view?query=paging&songCount=0&albumCount=20&albumOffset=0" +
            "&artistCount=0&u=alice&t=token&s=salt&v=1.16.1&c=Test&f=json");
        await client.GetStringAsync("/rest/search3.view?query=paging&songCount=0&albumCount=20&albumOffset=20" +
            "&artistCount=0&u=alice&t=token&s=salt&v=1.16.1&c=Test&f=json");

        fixture.Metadata.Verify(service => service.SearchAlbumsAsync("paging", It.IsAny<int>(),
            It.IsAny<CancellationToken>()), Times.Once);
    }
}

/// <summary>An Octo in front of a fake Navidrome and a fake Last.fm, for search paging.</summary>
internal sealed class SearchPagingWebFactory : WebApplicationFactory<Program>
{
    public static readonly IReadOnlyList<string> Library =
        Enumerable.Range(0, 30).Select(index => $"lib-{index}").ToList();

    /// <summary>Last.fm's answer. The third is a song the library has (lib-3).</summary>
    public static readonly IReadOnlyList<string> Outside =
        Enumerable.Range(0, 25).Select(index => index == 2 ? "Library Song 3" : $"Outside Song {index}").ToList();

    private readonly string _directory = Path.Combine(Path.GetTempPath(), "octo-search-paging-" + Guid.NewGuid());
    private readonly bool _discovery;
    public SearchPagingUpstream Upstream { get; } = new();
    public Mock<IMusicMetadataService> Metadata { get; } = new();

    public SearchPagingWebFactory(bool discovery = true)
    {
        _discovery = discovery;
        Directory.CreateDirectory(_directory);
        Metadata.Setup(service => service.SearchSongsByArtistTitleAsync(
                It.IsAny<string>(), It.IsAny<string>(), It.IsAny<int>(), It.IsAny<int?>()))
            .ReturnsAsync((string artist, string title, int _, int? duration) => new List<Song>
            {
                new() { Id = "ph-" + title, Artist = artist, Title = title, Album = "",
                    Duration = duration ?? 180, IsLocal = false, ExternalProvider = "soulseek" },
            });
        Metadata.Setup(service => service.EnrichExternalSongsAsync(It.IsAny<List<Song>>(), It.IsAny<CancellationToken>()))
            .Returns(Task.CompletedTask);
        Metadata.Setup(service => service.ResolveTopDurationsAsync(It.IsAny<List<Song>>(), It.IsAny<CancellationToken>(), It.IsAny<bool>()))
            .Returns(Task.CompletedTask);
        Metadata.Setup(service => service.PrewarmYouTubeIdsAsync(
                It.IsAny<IEnumerable<Song>>(), It.IsAny<int>(), It.IsAny<CancellationToken>()))
            .Returns(Task.CompletedTask);
        Metadata.Setup(service => service.PrewarmCoverArtAsync(
                It.IsAny<IEnumerable<Song>>(), It.IsAny<int>(), It.IsAny<CancellationToken>()))
            .Returns(Task.CompletedTask);
        Metadata.Setup(service => service.SearchAlbumsAsync(It.IsAny<string>(), It.IsAny<int>(), It.IsAny<CancellationToken>()))
            .ReturnsAsync([]);
        Metadata.Setup(service => service.SearchArtistsAsync(It.IsAny<string>(), It.IsAny<int>()))
            .ReturnsAsync([]);
        Metadata.Setup(service => service.SearchPlaylistsAsync(It.IsAny<string>(), It.IsAny<int>()))
            .ReturnsAsync([]);
    }

    protected override void ConfigureWebHost(IWebHostBuilder builder)
    {
        builder.ConfigureAppConfiguration((_, configuration) => configuration.AddInMemoryCollection(
            new Dictionary<string, string?>
            {
                ["Subsonic:Url"] = "http://navidrome.test",
                ["Subsonic:AutoDetectDownloadPath"] = "false",
                ["Subsonic:EnableSearchDiscovery"] = _discovery ? "true" : "false",
                ["Library:DownloadPath"] = _directory,
                ["Octo:StateDirectory"] = _directory,
                ["LastFm:ApiKey"] = "test-key",
            }));
        builder.ConfigureServices(services =>
        {
            services.RemoveAll<IHostedService>();
            services.RemoveAll<IHttpClientFactory>();
            services.AddSingleton<IHttpClientFactory>(new Factory(Upstream));
            services.RemoveAll<IMusicMetadataService>();
            services.AddSingleton(Metadata.Object);
        });
    }

    public override async ValueTask DisposeAsync()
    {
        await base.DisposeAsync();
        try { Directory.Delete(_directory, true); } catch { }
    }

    private sealed class Factory(SearchPagingUpstream handler) : IHttpClientFactory
    {
        public HttpClient CreateClient(string name) => new(handler, disposeHandler: false);
    }
}

/// <summary>
/// Navidrome with <see cref="SearchPagingWebFactory.Library"/> as the matches for any search,
/// paged by songOffset/songCount in either format and envelope, and Last.fm answering
/// track.search with <see cref="SearchPagingWebFactory.Outside"/>.
/// </summary>
internal sealed class SearchPagingUpstream : HttpMessageHandler
{
    private int _lastFmCalls;
    private readonly ConcurrentQueue<(string Endpoint, int Offset, int Count)> _songPages = new();

    public int LastFmCalls => _lastFmCalls;

    /// <summary>API keys this Navidrome knows, and whose each is.</summary>
    public Dictionary<string, string> ApiKeys { get; } = new(StringComparer.Ordinal)
    {
        ["alice-key"] = "alice", ["bob-key"] = "bob",
    };

    public int TokenInfoCalls => Volatile.Read(ref _tokenInfoCalls);
    private int _tokenInfoCalls;

    /// <summary>When set, Last.fm answers in reverse, as a later build might.</summary>
    public bool Reshuffle { get; set; }

    /// <summary>When set, a search page past the first fails with a 503.</summary>
    public bool FailLaterSongPages { get; set; }

    public List<(int Offset, int Count)> SongPages(string endpoint) =>
        _songPages.Where(page => page.Endpoint == endpoint).Select(page => (page.Offset, page.Count)).ToList();

    protected override Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken cancellationToken)
    {
        var query = System.Web.HttpUtility.ParseQueryString(request.RequestUri!.Query);
        if (request.RequestUri.Host == "ws.audioscrobbler.com")
        {
            Interlocked.Increment(ref _lastFmCalls);
            if (query["method"] != "track.search")
                return Ok("""{"toptracks":{"track":[]}}""", "application/json");
            var titles = Reshuffle ? SearchPagingWebFactory.Outside.Reverse() : SearchPagingWebFactory.Outside;
            var tracks = new JsonArray(titles.Select(title => (JsonNode)new JsonObject
            {
                ["name"] = title,
                ["artist"] = title.StartsWith("Library", StringComparison.Ordinal) ? "Owned Artist" : "Outside Artist",
            }).ToArray());
            return Ok(new JsonObject { ["results"] = new JsonObject { ["trackmatches"] = new JsonObject { ["track"] = tracks } } }
                .ToJsonString(), "application/json");
        }

        var path = request.RequestUri.AbsolutePath.Trim('/');
        var endpoint = path.StartsWith("rest/search2", StringComparison.Ordinal) ? "search2"
            : path.StartsWith("rest/search3", StringComparison.Ordinal) ? "search3" : null;
        var xml = query["f"] == "xml";
        if (path.StartsWith("rest/tokenInfo", StringComparison.Ordinal))
        {
            Interlocked.Increment(ref _tokenInfoCalls);
            return Ok(query["apiKey"] is { } key && ApiKeys.TryGetValue(key, out var owner)
                ? """{"subsonic-response":{"status":"ok","version":"1.16.1","tokenInfo":{"username":""" + JsonSerializer.Serialize(owner) + "}}}"
                : """{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":44,"message":"Invalid API key"}}}""",
                "application/json");
        }
        if (endpoint is null)
            return Ok(xml ? """<subsonic-response xmlns="http://subsonic.org/restapi" status="ok" version="1.16.1"/>"""
                : """{"subsonic-response":{"status":"ok","version":"1.16.1"}}""", xml ? "text/xml" : "application/json");

        int Number(string name, int fallback) => int.TryParse(query[name], out var value) ? value : fallback;
        var offset = Number("songOffset", 0);
        var count = Number("songCount", 20);
        _songPages.Enqueue((endpoint, offset, count));
        if (FailLaterSongPages && offset > 0)
            return Task.FromResult(new HttpResponseMessage(HttpStatusCode.ServiceUnavailable));
        var ids = SearchPagingWebFactory.Library.Skip(Math.Max(0, offset)).Take(Math.Max(0, count)).ToList();
        string Title(string id) => "Library Song " + id["lib-".Length..];
        var envelope = endpoint == "search2" ? "searchResult2" : "searchResult3";

        if (xml)
        {
            XNamespace ns = "http://subsonic.org/restapi";
            var document = new XElement(ns + "subsonic-response", new XAttribute("status", "ok"),
                new XAttribute("version", "1.16.1"),
                new XElement(ns + envelope, ids.Select(id => new XElement(ns + "song", new XAttribute("id", id),
                    new XAttribute("title", Title(id)), new XAttribute("artist", "Owned Artist")))));
            return Ok(document.ToString(), "text/xml");
        }

        var result = new JsonObject();
        if (ids.Count > 0)
            result["song"] = new JsonArray(ids.Select(id => (JsonNode)new JsonObject
                { ["id"] = id, ["title"] = Title(id), ["artist"] = "Owned Artist" }).ToArray());
        return Ok(new JsonObject
        {
            ["subsonic-response"] = new JsonObject { ["status"] = "ok", ["version"] = "1.16.1", [envelope] = result },
        }.ToJsonString(), "application/json");
    }

    private static Task<HttpResponseMessage> Ok(string body, string contentType) =>
        Task.FromResult(new HttpResponseMessage(HttpStatusCode.OK)
        {
            Content = new StringContent(body, Encoding.UTF8, contentType),
        });
}
