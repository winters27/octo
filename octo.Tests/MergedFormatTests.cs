using System.Net;
using System.Text;
using System.Text.Json;
using System.Xml.Linq;
using Microsoft.AspNetCore.Hosting;
using Microsoft.AspNetCore.Mvc.Testing;
using Microsoft.Extensions.Configuration;
using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.DependencyInjection.Extensions;
using Microsoft.Extensions.Hosting;
using Octo.Services.Subsonic;

namespace Octo.Tests;

/// <summary>
/// getAlbum and getArtist merge the outside songs and albums into Navidrome's answer. The merge
/// reads JSON, and an XML client used to get Navidrome's own XML back, without the outside part:
/// half an album. Both formats must now carry the same songs.
/// </summary>
public sealed class MergedFormatTests
{
    private static readonly XNamespace Ns = "http://subsonic.org/restapi";

    /// <summary>Navidrome holding two of an album's four tracks, and Deezer knowing all four.</summary>
    private sealed class FakeServers : HttpMessageHandler
    {
        public readonly List<string> NavidromeFormats = [];

        /// <summary>Every path the catalog was asked for.</summary>
        public readonly System.Collections.Concurrent.ConcurrentQueue<string> DeezerCalls = new();

        /// <summary>Holds each catalog answer, by path, until the returned task completes.</summary>
        public Func<string, Task>? HoldCatalog { get; set; }

        protected override Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken ct)
        {
            var answer = Answer(request);
            if (request.RequestUri!.Host != "api.deezer.com" || HoldCatalog is not { } hold) return answer;
            return Held(hold(request.RequestUri.AbsolutePath), answer);
        }

        private static async Task<HttpResponseMessage> Held(Task until, Task<HttpResponseMessage> answer)
        {
            // Bounded, so a request that never comes fails the test rather than hanging it.
            await until.WaitAsync(TimeSpan.FromSeconds(30));
            return await answer;
        }

        private Task<HttpResponseMessage> Answer(HttpRequestMessage request)
        {
            var uri = request.RequestUri!;
            var path = uri.AbsolutePath;
            var query = System.Web.HttpUtility.ParseQueryString(uri.Query);

            if (uri.Host == "api.deezer.com")
            {
                DeezerCalls.Enqueue(path);
                // The catalog writes this title with a curly apostrophe; the library's tags do not.
                if (path.StartsWith("/search/album", StringComparison.Ordinal) && query["q"]?.Contains("Look Back") == true)
                    return Json("""{"data":[{"id":20,"title":"Don’t Look Back","record_type":"album","nb_tracks":10,"artist":{"name":"Test Artist"}},{"id":21,"title":"Look Back Again","record_type":"album","nb_tracks":10,"artist":{"name":"Test Artist"}}]}""");
                // octo-player#1: a catalog album with the library album's very name, and none of its songs.
                if (path.StartsWith("/search/album", StringComparison.Ordinal) && query["q"]?.Contains("Nightcore") == true)
                    return Json("""{"data":[{"id":30,"title":"Nightcore","record_type":"album","nb_tracks":3,"artist":{"name":"Nightcore"}}]}""");
                if (path == "/album/30/tracks")
                    return Json("""
                        {"total":3,"data":[
                          {"title":"Love Tonight (Nightcore Remix)","duration":142,"track_position":1,"disk_number":1,"artist":{"name":"Nightcore"}},
                          {"title":"Angel (Nightcore Remix)","duration":159,"track_position":2,"disk_number":1,"artist":{"name":"Nightcore"}},
                          {"title":"Sad Songs & Depression (Nightcore Remix)","duration":280,"track_position":3,"disk_number":1,"artist":{"name":"Nightcore"}}
                        ]}
                        """);
                if (path == "/album/30")
                    return Json("""{"id":30,"title":"Nightcore","release_date":"2014-01-01","artist":{"name":"Nightcore"}}""");
                if (path.StartsWith("/search/album", StringComparison.Ordinal))
                    return Json("""{"data":[{"id":1,"title":"Test Album","record_type":"album","nb_tracks":4,"artist":{"name":"Test Artist"}}]}""");
                if (path == "/album/1/tracks")
                    return Json("""
                        {"total":4,"data":[
                          {"title":"One","duration":100,"track_position":1,"disk_number":1,"isrc":"GBAAA0000001","artist":{"name":"Test Artist"}},
                          {"title":"Two","duration":200,"track_position":2,"disk_number":1,"isrc":"GBAAA0000002","artist":{"name":"Test Artist"}},
                          {"title":"Three","duration":300,"track_position":3,"disk_number":1,"isrc":"GBAAA0000003","artist":{"name":"Test Artist"}},
                          {"title":"Four","duration":400,"track_position":4,"disk_number":1,"isrc":"GBAAA0000004","artist":{"name":"Test Artist"}}
                        ]}
                        """);
                if (path == "/album/2")
                    return Json("""{"id":2,"title":"Other Album","nb_tracks":9,"release_date":"2005-05-05","artist":{"name":"Test Artist"}}""");
                if (path == "/album/1")
                    return Json("""{"id":1,"title":"Test Album","release_date":"2001-01-01","artist":{"name":"Test Artist"}}""");
                // A bigger act whose name contains this one comes first, and a better known
                // artist of the very same name before the one the library holds.
                if (path.StartsWith("/search/artist", StringComparison.Ordinal))
                    return Json("""{"data":[{"id":8,"name":"Test Artist Orchestra","nb_fan":90000,"picture_xl":"https://cdn/orchestra.jpg"},{"id":9,"name":"Test Artist","nb_fan":5000,"picture_xl":"https://cdn/somebody-else.jpg"},{"id":7,"name":"Test Artist","nb_fan":10,"picture_xl":"https://cdn/test-artist.jpg"}]}""");
                if (path.StartsWith("/artist/9/albums", StringComparison.Ordinal))
                    return Json("""{"data":[{"id":90,"title":"Somebody Else's Record","record_type":"album","release_date":"2010-01-01"}]}""");
                // The catalog's own shape: no artist and no track counts on this listing. An EP
                // shares the album's title, and would open as the album (or the album as it).
                if (path.StartsWith("/artist/7/albums", StringComparison.Ordinal))
                    return Json("""{"data":[{"id":1,"title":"Test Album","record_type":"album","release_date":"2001-01-01"},{"id":2,"title":"Other Album","record_type":"album","release_date":"2005-05-05"},{"id":3,"title":"A Single","record_type":"single","release_date":"2006-01-01"},{"id":4,"title":"Other Album","record_type":"ep","release_date":"2004-04-04"}]}""");
                return Task.FromResult(new HttpResponseMessage(HttpStatusCode.NotFound));
            }

            var json = query["f"] == "json";
            if (path.EndsWith("/rest/getAlbum", StringComparison.Ordinal) || path.EndsWith("/rest/getArtist", StringComparison.Ordinal))
                lock (NavidromeFormats) NavidromeFormats.Add(query["f"] ?? "xml");

            if (path.EndsWith("/rest/getAlbum", StringComparison.Ordinal) && query["id"] == "al-nc")
                return Json("""
                    {"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome","album":{
                      "id":"al-nc","name":"Nightcore","artist":"Nightcore","songCount":3,"duration":518,
                      "song":[
                        {"id":"nc-1","title":"Believer (Rock Version)","album":"Nightcore","artist":"Missigno","track":1,"duration":216},
                        {"id":"nc-2","title":"Mi Mi Mi (Rock Version)","album":"Nightcore","artist":"Missigno","track":2,"duration":168},
                        {"id":"nc-3","title":"MAGIC","album":"Nightcore","artist":"Missigno","track":3,"duration":134}
                      ]}}}
                    """);
            if (path.EndsWith("/rest/getAlbum", StringComparison.Ordinal))
            {
                return json
                    ? Json("""
                        {"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome","album":{
                          "id":"al-1","name":"Test Album","artist":"Test Artist","songCount":2,"duration":400,
                          "genres":[{"name":"Rock"}],
                          "song":[
                            {"id":"s-1","title":"One","album":"Test Album","artist":"Test Artist","track":1,"duration":100,"isrc":["GBAAA0000001"],"replayGain":{"trackGain":-6.5},"path":"Test Artist/Test Album/01 One.flac","size":34684600,"created":"2026-08-14T23:11:28Z","suffix":"flac","bitRate":867},
                            {"id":"s-3","title":"Three","album":"Test Album","artist":"Test Artist","track":3,"duration":300,"isrc":[]}
                          ]}}}
                        """)
                    : Xml("""<subsonic-response xmlns="http://subsonic.org/restapi" status="ok" version="1.16.1"><album id="al-1" name="Test Album" artist="Test Artist" songCount="2"><song id="s-1" title="One" track="1"/><song id="s-3" title="Three" track="3"/></album></subsonic-response>""");
            }
            if (path.EndsWith("/rest/getArtist", StringComparison.Ordinal))
            {
                return json
                    ? Json("""{"subsonic-response":{"status":"ok","version":"1.16.1","artist":{"id":"ar-1","name":"Test Artist","albumCount":1,"album":[{"id":"al-1","name":"Test Album","artist":"Test Artist","songCount":2,"releaseTypes":["album","compilation"]}]}}}""")
                    : Xml("""<subsonic-response xmlns="http://subsonic.org/restapi" status="ok" version="1.16.1"><artist id="ar-1" name="Test Artist" albumCount="1"><album id="al-1" name="Test Album"><releaseTypes>album</releaseTypes><releaseTypes>compilation</releaseTypes></album></artist></subsonic-response>""");
            }
            // Navidrome's native API, for a library artist only.
            if (path == "/api/artist/ar-1")
                return Json("""{"id":"ar-1","name":"Test Artist","albumCount":1,"songCount":2,"size":1}""");
            if (path == "/api/album" && query["name"] == "Look Back")
                return Json("""[{"id":"al-9","name":"Don't Look Back","albumArtist":"Test Artist","libraryId":1}]""");
            if (path == "/api/album" && query["artist_id"] == "ar-1")
            {
                var answer = new HttpResponseMessage(HttpStatusCode.OK)
                {
                    Content = new StringContent("""[{"id":"al-1","name":"Test Album","albumArtistId":"ar-1"}]""", Encoding.UTF8, "application/json"),
                };
                answer.Headers.Add("X-Total-Count", "1");
                return Task.FromResult(answer);
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
        private readonly string _directory = Path.Combine(Path.GetTempPath(), "octo-merge-web-" + Guid.NewGuid());
        public FakeServers Servers { get; } = new();

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
                    ["Octo:StateDirectory"] = _directory,
                }));
            builder.ConfigureServices(services =>
            {
                services.RemoveAll<IHostedService>();
                services.RemoveAll<IHttpClientFactory>();
                services.AddSingleton<IHttpClientFactory>(new ReviewFixtures.OneClientFactory(Servers));
            });
        }

        public override async ValueTask DisposeAsync()
        {
            await base.DisposeAsync();
            try { Directory.Delete(_directory, recursive: true); } catch { /* best effort */ }
        }
    }

    private const string Auth = "u=alice&t=good&s=salt&v=1.16.1&c=test";

    [Fact]
    public async Task GetAlbum_XmlCarriesTheSameMergedSongsAsJson()
    {
        await using var factory = new WebFactory();
        using var client = factory.CreateClient();

        using var json = JsonDocument.Parse(await client.GetStringAsync($"/rest/getAlbum.view?{Auth}&f=json&id=al-1"));
        var jsonSongs = json.RootElement.GetProperty("subsonic-response").GetProperty("album").GetProperty("song")
            .EnumerateArray().Select(s => s.GetProperty("title").GetString()).ToList();

        var response = await client.GetAsync($"/rest/getAlbum.view?{Auth}&id=al-1");
        Assert.Equal("application/xml", response.Content.Headers.ContentType?.MediaType);
        var xml = XDocument.Parse(await response.Content.ReadAsStringAsync());
        var album = xml.Root!.Element(Ns + "album")!;
        var xmlSongs = album.Elements(Ns + "song").Select(s => (string?)s.Attribute("title")).ToList();

        Assert.Equal(["One", "Two", "Three", "Four"], jsonSongs);
        Assert.Equal(jsonSongs, xmlSongs);
        Assert.Equal("4", (string?)album.Attribute("songCount"));
        Assert.Equal("ok", (string?)xml.Root.Attribute("status"));
        // Navidrome was asked for JSON both times: the merge reads JSON.
        Assert.All(factory.Servers.NavidromeFormats, f => Assert.Equal("json", f));
    }

    [Fact]
    public async Task GetAlbum_LeavesAnAlbumAloneWhenTheCatalogAlbumOnlySharesItsName()
    {
        await using var factory = new WebFactory();
        using var client = factory.CreateClient();

        using var json = JsonDocument.Parse(await client.GetStringAsync($"/rest/getAlbum.view?{Auth}&f=json&id=al-nc"));
        var album = json.RootElement.GetProperty("subsonic-response").GetProperty("album");
        var titles = album.GetProperty("song").EnumerateArray().Select(s => s.GetProperty("title").GetString()).ToList();

        Assert.Equal(["Believer (Rock Version)", "Mi Mi Mi (Rock Version)", "MAGIC"], titles);
        Assert.Equal(3, album.GetProperty("songCount").GetInt32());
        // The catalog was asked, and its album turned down for having none of these songs.
        Assert.Contains("/album/30/tracks", factory.Servers.DeezerCalls);
    }

    [Fact]
    public async Task GetAlbum_MarksTheSongsTheLibraryDoesNotHold()
    {
        await using var factory = new WebFactory();
        using var client = factory.CreateClient();

        using var json = JsonDocument.Parse(await client.GetStringAsync($"/rest/getAlbum.view?{Auth}&f=json&id=al-1"));
        var songs = json.RootElement.GetProperty("subsonic-response").GetProperty("album").GetProperty("song")
            .EnumerateArray().ToDictionary(s => s.GetProperty("title").GetString()!, s => s.Clone());

        // The library's songs go out as Navidrome described them.
        var one = songs["One"];
        Assert.False(one.GetProperty("isExternal").GetBoolean());
        Assert.Equal("s-1", one.GetProperty("id").GetString());
        Assert.Equal("Test Artist/Test Album/01 One.flac", one.GetProperty("path").GetString());
        Assert.Equal(34684600, one.GetProperty("size").GetInt64());
        Assert.Equal("2026-08-14T23:11:28Z", one.GetProperty("created").GetString());
        Assert.Equal(867, one.GetProperty("bitRate").GetInt32());
        Assert.False(songs["Three"].GetProperty("isExternal").GetBoolean());

        // The outside ones say so, and claim no file.
        foreach (var title in new[] { "Two", "Four" })
        {
            var song = songs[title];
            Assert.True(song.GetProperty("isExternal").GetBoolean(), title);
            foreach (var key in new[] { "path", "size", "created", "bitDepth", "samplingRate", "channelCount" })
                Assert.False(song.TryGetProperty(key, out _), $"{title} has {key}");
            Assert.Equal("m4a", song.GetProperty("suffix").GetString());
        }

        // XML says the same.
        var xml = XDocument.Parse(await client.GetStringAsync($"/rest/getAlbum.view?{Auth}&id=al-1"));
        var rows = xml.Root!.Element(Ns + "album")!.Elements(Ns + "song").ToDictionary(s => (string)s.Attribute("title")!);
        Assert.Equal("false", (string?)rows["One"].Attribute("isExternal"));
        Assert.Equal("Test Artist/Test Album/01 One.flac", (string?)rows["One"].Attribute("path"));
        Assert.Equal("true", (string?)rows["Two"].Attribute("isExternal"));
        Assert.Null(rows["Two"].Attribute("path"));
        Assert.Null(rows["Two"].Attribute("created"));

        // The outside song opens by its id, marked the same way, without asking Navidrome.
        var twoId = songs["Two"].GetProperty("id").GetString();
        using var opened = JsonDocument.Parse(await client.GetStringAsync($"/rest/getSong.view?{Auth}&f=json&id={twoId}"));
        var song2 = opened.RootElement.GetProperty("subsonic-response").GetProperty("song");
        Assert.Equal("Two", song2.GetProperty("title").GetString());
        Assert.True(song2.GetProperty("isExternal").GetBoolean());
        Assert.False(song2.TryGetProperty("path", out _));
    }

    [Fact]
    public async Task GetAlbum_XmlWritesListsAndObjectsTheOpenSubsonicWay()
    {
        await using var factory = new WebFactory();
        using var client = factory.CreateClient();

        var xml = XDocument.Parse(await client.GetStringAsync($"/rest/getAlbum.view?{Auth}&id=al-1"));
        var album = xml.Root!.Element(Ns + "album")!;
        var one = album.Elements(Ns + "song").First(s => (string?)s.Attribute("title") == "One");
        var two = album.Elements(Ns + "song").First(s => (string?)s.Attribute("title") == "Two");

        // A list of plain values: one text element each. An object: one child element.
        Assert.Equal(["GBAAA0000001"], one.Elements(Ns + "isrc").Select(e => e.Value));
        Assert.Equal("-6.5", (string?)one.Element(Ns + "replayGain")!.Attribute("trackGain"));
        // An outside song carries Deezer's code the same way.
        Assert.Equal(["GBAAA0000002"], two.Elements(Ns + "isrc").Select(e => e.Value));
        // A list of objects: one element each, named for the list.
        Assert.Equal("Rock", (string?)album.Element(Ns + "genres")!.Attribute("name"));
        // Plain values are attributes, never child elements.
        Assert.Equal("1", (string?)one.Attribute("track"));
        Assert.Null(one.Element(Ns + "track"));
    }

    [Fact]
    public async Task GetArtist_ALibraryArtistGainsTheAlbumsTheLibraryLacks()
    {
        await using var factory = new WebFactory();
        using var client = factory.CreateClient();

        using var json = JsonDocument.Parse(await client.GetStringAsync($"/rest/getArtist.view?{Auth}&f=json&id=ar-1"));
        var artist = json.RootElement.GetProperty("subsonic-response").GetProperty("artist");
        var albums = artist.GetProperty("album").EnumerateArray().ToList();

        // The owned album once, then the outside ones, the album before the single.
        Assert.Equal(["Test Album", "Other Album", "A Single"], albums.Select(a => a.GetProperty("name").GetString()));
        Assert.Equal(3, artist.GetProperty("albumCount").GetInt32());
        var outside = albums[1];
        Assert.Equal("Test Artist", outside.GetProperty("artist").GetString());
        // It links back to the library artist, not to an outside copy of them.
        Assert.Equal("ar-1", outside.GetProperty("artistId").GetString());
        Assert.Equal(2005, outside.GetProperty("year").GetInt32());
        // The listing has no track count; the album's own record fills it in.
        Assert.Equal(9, outside.GetProperty("songCount").GetInt32());

        // And the outside album opens, as the album rather than the EP of its name.
        using var opened = JsonDocument.Parse(await client.GetStringAsync(
            $"/rest/getAlbum.view?{Auth}&f=json&id={outside.GetProperty("id").GetString()}"));
        Assert.Equal("Other Album", opened.RootElement.GetProperty("subsonic-response").GetProperty("album").GetProperty("name").GetString());
        var registry = factory.Services.GetRequiredService<Octo.Services.Soulseek.ExternalIdRegistry>();
        Assert.Equal("2", registry.Lookup(outside.GetProperty("id").GetString()!)!.ExternalAlbumId);
    }

    [Fact]
    public async Task GetArtist_AnOutsideArtistsPageListsTheirAlbums()
    {
        await using var factory = new WebFactory();
        using var client = factory.CreateClient();
        var registry = factory.Services.GetRequiredService<Octo.Services.Soulseek.ExternalIdRegistry>();
        var id = registry.Register(new Octo.Services.Soulseek.SoulseekRouting
        {
            Kind = Octo.Services.Soulseek.RoutingKind.Artist,
            Artist = "Test Artist",
            // Tapped in search: this artist, not the better known one of the name.
            ExternalArtistId = "7",
        });

        using var json = JsonDocument.Parse(await client.GetStringAsync($"/rest/getArtist.view?{Auth}&f=json&id={id}"));
        var artist = json.RootElement.GetProperty("subsonic-response").GetProperty("artist");

        Assert.Equal(["Other Album", "Test Album", "A Single"],
            artist.GetProperty("album").EnumerateArray().Select(a => a.GetProperty("name").GetString()));
        Assert.All(artist.GetProperty("album").EnumerateArray(), a => Assert.Equal(id, a.GetProperty("artistId").GetString()));
    }

    [Fact]
    public async Task GetArtist_XmlCarriesTheSameAlbumsAsJson()
    {
        await using var factory = new WebFactory();
        using var client = factory.CreateClient();

        using var json = JsonDocument.Parse(await client.GetStringAsync($"/rest/getArtist.view?{Auth}&f=json&id=ar-1"));
        var jsonAlbums = json.RootElement.GetProperty("subsonic-response").GetProperty("artist").GetProperty("album")
            .EnumerateArray().Select(a => a.GetProperty("name").GetString()).ToList();

        var response = await client.GetAsync($"/rest/getArtist.view?{Auth}&id=ar-1");
        Assert.Equal("application/xml", response.Content.Headers.ContentType?.MediaType);
        var xml = XDocument.Parse(await response.Content.ReadAsStringAsync());
        var artist = xml.Root!.Element(Ns + "artist")!;
        var xmlAlbums = artist.Elements(Ns + "album").Select(a => (string?)a.Attribute("name")).ToList();

        Assert.NotEmpty(jsonAlbums);
        Assert.Equal(jsonAlbums, xmlAlbums);
        Assert.Equal("Test Artist", (string?)artist.Attribute("name"));
    }

    private static string RegisterOutsideArtist(WebFactory factory) =>
        factory.Services.GetRequiredService<Octo.Services.Soulseek.ExternalIdRegistry>().Register(
            new Octo.Services.Soulseek.SoulseekRouting
            {
                Kind = Octo.Services.Soulseek.RoutingKind.Artist,
                Artist = "Test Artist",
                // Tapped in search: this artist, not the better known one of the name.
                ExternalArtistId = "7",
            });

    [Fact]
    public async Task NativeArtist_AnOutsideArtistOpensWithTheirAlbums()
    {
        // Feishin in Navidrome mode opens an artist page with /api/artist/{id} and asks for
        // the albums with /api/album?artist_id=. Relayed, an outside id reached Navidrome,
        // which has no such artist, and the page never loaded.
        await using var factory = new WebFactory();
        using var client = factory.CreateClient();
        var id = RegisterOutsideArtist(factory);

        using var detailResponse = await client.GetAsync($"/api/artist/{id}");
        detailResponse.EnsureSuccessStatusCode();
        using var detail = JsonDocument.Parse(await detailResponse.Content.ReadAsStringAsync());
        var artist = detail.RootElement;
        Assert.Equal(id, artist.GetProperty("id").GetString());
        Assert.Equal("Test Artist", artist.GetProperty("name").GetString());
        Assert.Equal(3, artist.GetProperty("albumCount").GetInt32());
        Assert.Equal(3, artist.GetProperty("stats").GetProperty("albumartist").GetProperty("albumCount").GetInt32());
        Assert.Equal(0, artist.GetProperty("size").GetInt32());
        Assert.Equal("https://cdn/test-artist.jpg", artist.GetProperty("largeImageUrl").GetString());

        // Feishin's own request for the page: the whole discography, _end=-1.
        using var listResponse = await client.GetAsync(
            $"/api/album?_end=-1&_order=DESC&_sort=max_year&_start=0&artist_id={id}&missing=false");
        listResponse.EnsureSuccessStatusCode();
        Assert.Equal("3", listResponse.Headers.GetValues("X-Total-Count").Single());
        using var list = JsonDocument.Parse(await listResponse.Content.ReadAsStringAsync());
        var albums = list.RootElement.EnumerateArray().ToList();
        // The albums, then the single, as getArtist lists them.
        Assert.Equal(["Other Album", "Test Album", "A Single"], albums.Select(a => a.GetProperty("name").GetString()));
        Assert.All(albums, a =>
        {
            Assert.Equal(id, a.GetProperty("albumArtistId").GetString());
            Assert.Equal("Test Artist", a.GetProperty("albumArtist").GetString());
        });
        Assert.Equal(9, albums[0].GetProperty("songCount").GetInt32());

        // Each album opens natively too.
        using var opened = await client.GetAsync($"/api/album/{albums[0].GetProperty("id").GetString()}");
        opened.EnsureSuccessStatusCode();
    }

    [Fact]
    public async Task NativeArtist_ThePageAndItsAlbumsAtOnceAskTheCatalogOnce()
    {
        // Feishin asks for the artist and the album list together. Each walked the catalog,
        // album counts and all, so one page cost twice its calls.
        await using var factory = new WebFactory();
        using var client = factory.CreateClient();
        var id = RegisterOutsideArtist(factory);
        // The artist names itself with a search and the album list starts with the listing.
        // Neither hears back until both have asked, so the two always meet. The listing comes
        // back last, a moment after the artist has its name: a walk of the artist's own would
        // ask for it again in that moment, and one asking twice is let through at once.
        var bothAsking = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        var listedTwice = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        var listings = 0;
        factory.Servers.HoldCatalog = path =>
        {
            var asked = factory.Servers.DeezerCalls;
            if (asked.Any(p => p.StartsWith("/search/artist", StringComparison.Ordinal)) && asked.Contains("/artist/7/albums"))
                bothAsking.TrySetResult();
            if (path != "/artist/7/albums") return bothAsking.Task;
            if (Interlocked.Increment(ref listings) > 1) listedTwice.TrySetResult();
            return Task.WhenAny(listedTwice.Task, bothAsking.Task.ContinueWith(_ => Task.Delay(500)).Unwrap());
        };

        var answers = await Task.WhenAll(
            client.GetAsync($"/api/artist/{id}"),
            client.GetAsync($"/api/album?_end=-1&_order=DESC&_sort=max_year&_start=0&artist_id={id}&missing=false"));
        Assert.All(answers, answer => answer.EnsureSuccessStatusCode());

        var calls = factory.Servers.DeezerCalls.ToList();
        // The search that names the artist, the listing, and each album's own record, once.
        Assert.Equal(1, calls.Count(path => path.StartsWith("/search/artist", StringComparison.Ordinal)));
        Assert.Equal(1, calls.Count(path => path == "/artist/7/albums"));
        Assert.Equal(["/album/1", "/album/2", "/album/3"], calls.Where(path => path.StartsWith("/album/", StringComparison.Ordinal)).Order());
        Assert.Equal(5, calls.Count);
    }

    [Fact]
    public async Task NativeArtist_AlbumsHonourThePageAsked()
    {
        await using var factory = new WebFactory();
        using var client = factory.CreateClient();
        var id = RegisterOutsideArtist(factory);

        using var response = await client.GetAsync($"/api/album?_start=1&_end=2&artist_id={id}");
        response.EnsureSuccessStatusCode();
        using var page = JsonDocument.Parse(await response.Content.ReadAsStringAsync());

        Assert.Equal(["Test Album"], page.RootElement.EnumerateArray().Select(a => a.GetProperty("name").GetString()));
        Assert.Equal("3", response.Headers.GetValues("X-Total-Count").Single());
    }

    [Fact]
    public async Task NativeArtist_AlbumsSayWhatKindOfReleaseTheyAre()
    {
        // Navidrome's native album keeps its release types among its tags, lowercase, and
        // Feishin groups an artist's page by them.
        await using var factory = new WebFactory();
        using var client = factory.CreateClient();
        var id = RegisterOutsideArtist(factory);

        using var list = JsonDocument.Parse(await client.GetStringAsync($"/api/album?_start=0&_end=-1&artist_id={id}"));
        var types = list.RootElement.EnumerateArray().ToDictionary(a => a.GetProperty("name").GetString()!,
            a => a.GetProperty("tags").GetProperty("releasetype").EnumerateArray().Select(t => t.GetString()!).ToList());

        Assert.Equal(["album"], types["Other Album"]);
        Assert.Equal(["single"], types["A Single"]);
    }

    [Fact]
    public async Task NativeArtist_ALibraryArtistStillComesFromNavidrome()
    {
        await using var factory = new WebFactory();
        using var client = factory.CreateClient();

        using var detail = JsonDocument.Parse(await client.GetStringAsync("/api/artist/ar-1"));
        Assert.Equal(1, detail.RootElement.GetProperty("size").GetInt32());

        using var response = await client.GetAsync("/api/album?_start=0&_end=-1&artist_id=ar-1");
        using var list = JsonDocument.Parse(await response.Content.ReadAsStringAsync());
        Assert.Equal(["al-1"], list.RootElement.EnumerateArray().Select(a => a.GetProperty("id").GetString()));
    }

    [Fact]
    public async Task NativeAlbumSearch_AnOwnedAlbumIsNotAddedAgainOverAnApostrophe()
    {
        // The library has "Don't Look Back"; the catalog calls it "Don’t Look Back". The
        // native search matched the two by exact text, so the album showed up twice.
        await using var factory = new WebFactory();
        using var client = factory.CreateClient();

        using var response = await client.GetAsync("/api/album?_start=0&_end=20&name=Look%20Back");
        response.EnsureSuccessStatusCode();
        using var list = JsonDocument.Parse(await response.Content.ReadAsStringAsync());

        Assert.Equal(["Don't Look Back", "Look Back Again"],
            list.RootElement.EnumerateArray().Select(a => a.GetProperty("name").GetString()));
        Assert.Equal("al-9", list.RootElement[0].GetProperty("id").GetString());
        Assert.Equal("2", response.Headers.GetValues("X-Total-Count").Single());
    }

    [Fact]
    public async Task GetArtist_EveryAlbumSaysWhatKindOfReleaseItIs()
    {
        await using var factory = new WebFactory();
        using var client = factory.CreateClient();

        using var json = JsonDocument.Parse(await client.GetStringAsync($"/rest/getArtist.view?{Auth}&f=json&id=ar-1"));
        var types = json.RootElement.GetProperty("subsonic-response").GetProperty("artist").GetProperty("album")
            .EnumerateArray().ToDictionary(a => a.GetProperty("name").GetString()!,
                a => a.GetProperty("releaseTypes").EnumerateArray().Select(t => t.GetString()!).ToList());

        // The library's album keeps what Navidrome said, word for word.
        Assert.Equal(["album", "compilation"], types["Test Album"]);
        // The outside ones say what the catalog calls them, in the same lowercase words, so
        // one artist's list never mixes "album" and "Album".
        Assert.Equal(["album"], types["Other Album"]);
        Assert.Equal(["single"], types["A Single"]);
        Assert.All(types.Values.SelectMany(t => t), type => Assert.Equal(type.ToLowerInvariant(), type));

        var xml = XDocument.Parse(await client.GetStringAsync($"/rest/getArtist.view?{Auth}&id=ar-1"));
        var xmlTypes = xml.Root!.Element(Ns + "artist")!.Elements(Ns + "album").ToDictionary(
            a => (string)a.Attribute("name")!, a => a.Elements(Ns + "releaseTypes").Select(e => e.Value).ToList());
        Assert.Equal(types, xmlTypes);
    }

    [Fact]
    public void JsonShapeToXml_LeavesOutMissingValuesAndKeepsNumbersInvariant()
    {
        var element = SubsonicResponseBuilder.JsonShapeToXml(Ns, "song", new Dictionary<string, object>
        {
            ["title"] = "A",
            ["comment"] = null!,
            ["bitRate"] = 320,
            ["averageRating"] = 4.5,
            ["starred"] = true,
            ["moods"] = new List<object>(),
        });

        Assert.Equal("A", (string?)element.Attribute("title"));
        Assert.Null(element.Attribute("comment"));
        Assert.Equal("4.5", (string?)element.Attribute("averageRating"));
        Assert.Equal("true", (string?)element.Attribute("starred"));
        Assert.Empty(element.Elements());
    }
}
