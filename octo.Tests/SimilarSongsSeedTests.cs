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
using Microsoft.Extensions.Logging;
using Microsoft.Extensions.Options;
using Moq;
using Octo.Controllers;
using Octo.Models.Domain;
using Octo.Models.Settings;
using Octo.Services;
using Octo.Services.LastFm;
using Octo.Services.Soulseek;

namespace Octo.Tests;

/// <summary>
/// A radio from a song Last.fm cannot place (#78), the songs of a library built from YouTube
/// playlists: the artist is the uploader and the album is the playlist. The seed's own album and
/// genre lead, and similar artists to an uploader's name stay out. Also the bare server address
/// a client checks before signing in (#80).
/// </summary>
public sealed class SimilarSongsSeedTests
{
    [Fact]
    public async Task UnknownUpload_LeadsWithItsPlaylistAndGenre_AndLeavesOutTheUploadersSimilarArtists()
    {
        await using var fixture = new Factory();
        using var client = fixture.CreateClient();

        var songs = await RadioIds(client, "spooky");

        Assert.NotEmpty(songs);
        Assert.DoesNotContain("spooky", songs);
        // The playlist's own songs and the library's phonk, then Last.fm's phonk.
        Assert.Contains("pl-2", songs);
        Assert.Contains("pl-3", songs);
        Assert.Contains("genre-1", songs);
        Assert.Contains(songs, id => id.StartsWith("ext-Kordhell", StringComparison.Ordinal));
        Assert.True(songs.IndexOf(songs.First(id => id.StartsWith("ext-", StringComparison.Ordinal)))
            > 0, "a library song plays first");
        // Last.fm never heard of "Missigno - SPOOKY", so artists like that name are a guess.
        Assert.DoesNotContain(songs, id => id.Contains("The Year", StringComparison.Ordinal));
        Assert.Contains(fixture.Handler.Calls, call => call.Contains("tag.gettoptracks", StringComparison.Ordinal)
            && call.Contains("tag=Phonk", StringComparison.Ordinal));
    }

    [Fact]
    public async Task KnownSong_IsLastFmsRadioAsBefore()
    {
        await using var fixture = new Factory();
        using var client = fixture.CreateClient();

        var songs = await RadioIds(client, "teardrop");

        Assert.Equal(["ext-Portishead-Glory Box", "ext-Morcheeba-Trigger Hippie"], songs);
        Assert.DoesNotContain(fixture.Handler.Calls, call => call.Contains("rest/getAlbum", StringComparison.Ordinal));
    }

    [Fact]
    public async Task NightcoreUpload_TriesTheArtistItsTitleNames_AndKeepsItsPlaylistFirst()
    {
        await using var fixture = new Factory();
        using var client = fixture.CreateClient();

        var songs = await RadioIds(client, "faded-nightcore");

        // Last.fm knows "Alan Walker - Faded", the song this upload came from, and its songs are
        // like the original: they play, but behind the playlist the nightcore upload sits in.
        Assert.Contains(fixture.Handler.Calls, call => call.Contains("method=track.getsimilar", StringComparison.Ordinal)
            && call.Contains("artist=Alan%20Walker", StringComparison.Ordinal));
        Assert.Contains("ext-Avicii-Wake Me Up", songs);
        Assert.StartsWith("pl-", songs[0]);
    }

    [Fact]
    public async Task NewSongByAKnownArtist_StillGetsArtistsLikeThem()
    {
        await using var fixture = new Factory();
        using var client = fixture.CreateClient();

        var songs = await RadioIds(client, "brand-new");

        // Not on Last.fm yet, but Drake is, with an audience: artists like him are no guess.
        Assert.Equal(["ext-Future-Mask Off"], songs);
    }

    [Fact]
    public async Task AlbumId_IsARadioFromItsFirstSong()
    {
        await using var fixture = new Factory();
        using var client = fixture.CreateClient();

        var songs = await RadioIds(client, "album-trip");

        Assert.Equal(["ext-Portishead-Glory Box", "ext-Morcheeba-Trigger Hippie"], songs);
    }

    [Fact]
    public async Task ArtistId_IsTheTopSongsOfArtistsLikeThem()
    {
        await using var fixture = new Factory();
        using var client = fixture.CreateClient();

        var songs = await RadioIds(client, "artist-massive");

        Assert.Equal(["ext-Portishead-Roads"], songs);
    }

    [Theory]
    [InlineData("GET")]
    [InlineData("HEAD")]
    public async Task BareAddress_SendsToTheWebAppLikeNavidrome(string method)
    {
        await using var fixture = new Factory();
        using var client = fixture.CreateClient(new WebApplicationFactoryClientOptions { AllowAutoRedirect = false });

        using var response = await client.SendAsync(new HttpRequestMessage(new HttpMethod(method), "/"));

        Assert.Equal(HttpStatusCode.Redirect, response.StatusCode);
        Assert.Equal("app/", response.Headers.Location?.OriginalString);
    }

    [Theory]
    [InlineData("Phonk", "Phonk")]
    [InlineData("Phonk; Drift Phonk", "Phonk")]
    [InlineData("Electronic/Dance", "Electronic")]
    [InlineData("Unknown", null)]
    [InlineData("", null)]
    [InlineData(null, null)]
    public void PrimaryGenre_IsTheFirstRealOne(string? genre, string? expected) =>
        Assert.Equal(expected, SubsonicController.PrimaryGenre(genre));

    [Fact]
    public void Interleave_TakesOneFromEachInTurn() =>
        Assert.Equal([1, 10, 2, 20, 3], LastFmRadioTrackResolver.Interleave<int>([[1, 2, 3], [10, 20]]));

    private static async Task<List<string>> RadioIds(HttpClient client, string id)
    {
        var body = await client.GetStringAsync($"/rest/getSimilarSongs2?id={id}&u=alice&t=token&s=salt&f=json&count=20");
        using var doc = JsonDocument.Parse(body);
        var response = doc.RootElement.GetProperty("subsonic-response");
        if (!response.TryGetProperty("similarSongs2", out var similar)
            || !similar.TryGetProperty("song", out var songs)) return [];
        return songs.EnumerateArray().Select(song => song.GetProperty("id").GetString()!).ToList();
    }

    private sealed class Handler : HttpMessageHandler
    {
        public ConcurrentQueue<string> Calls { get; } = new();

        private static string Song(string id, string title, string artist, string album, string albumId, string genre) =>
            $$"""{"id":"{{id}}","title":"{{title}}","artist":"{{artist}}","album":"{{album}}","albumId":"{{albumId}}","genre":"{{genre}}","duration":180}""";

        private static string Album(string id, string name, string artist, params string[] songs) =>
            $$"""{"album":{"id":"{{id}}","name":"{{name}}","artist":"{{artist}}","song":[""" + string.Join(",", songs) + "]}}";

        private static readonly Dictionary<string, string> LibrarySongs = new()
        {
            ["spooky"] = Song("spooky", "SPOOKY", "Missigno", "PHONK", "al-phonk", "Phonk"),
            ["teardrop"] = Song("teardrop", "Teardrop", "Massive Attack", "Mezzanine", "al-mezz", "Trip-Hop"),
            ["brand-new"] = Song("brand-new", "Brand New", "Drake", "Brand New", "al-brand-new", "Hip-Hop"),
            ["faded-nightcore"] = Song("faded-nightcore", "Alan Walker - Faded (Nightcore)", "Nightcore Galaxy",
                "Nightcore", "al-nightcore", ""),
        };

        protected override async Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken ct)
        {
            var uri = request.RequestUri!;
            Calls.Enqueue(uri.AbsolutePath + uri.Query);
            var query = System.Web.HttpUtility.ParseQueryString(uri.Query);
            if (uri.Host == "ws.audioscrobbler.com") return Json(LastFm(query));

            var path = uri.AbsolutePath;
            var id = query["id"] ?? "";
            string? answer = path switch
            {
                _ when path.EndsWith("/rest/getSong") => LibrarySongs.TryGetValue(id, out var song)
                    ? $$"""{"song":{{song}}}""" : null,
                _ when path.EndsWith("/rest/getAlbum") => id switch
                {
                    "al-phonk" => Album("al-phonk", "PHONK", "Missigno", LibrarySongs["spooky"], Song("pl-2", "DRIFT", "Missigno", "PHONK", "al-phonk", "Phonk"), Song("pl-3", "NIGHT RIDE", "Missigno", "PHONK", "al-phonk", "Phonk")),
                    "al-nightcore" => Album("al-nightcore", "Nightcore", "Nightcore Galaxy", LibrarySongs["faded-nightcore"], Song("pl-9", "Alone (Nightcore)", "Nightcore Galaxy", "Nightcore", "al-nightcore", "")),
                    "album-trip" => Album("album-trip", "Mezzanine", "Massive Attack", LibrarySongs["teardrop"]),
                    _ => null,
                },
                _ when path.EndsWith("/rest/getArtist") => id == "artist-massive"
                    ? """{"artist":{"id":"artist-massive","name":"Massive Attack"}}""" : null,
                _ when path.EndsWith("/rest/getRandomSongs") => query["genre"] == "Phonk"
                    ? """{"randomSongs":{"song":[""" + Song("genre-1", "GHOST", "Kaito", "Phonk Mix", "al-mix", "Phonk") + "," + LibrarySongs["spooky"] + "]}}"
                    : """{"randomSongs":{"song":[]}}""",
                _ when path.EndsWith("/rest/search3") => """{"searchResult3":{}}""",
                _ when path.EndsWith("/rest/ping") => "",
                _ => null,
            };
            await Task.Yield();
            return answer is null
                ? Json("""{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":70,"message":"not found"}}}""")
                : Json(Ok(answer));
        }

        private static string LastFm(System.Collections.Specialized.NameValueCollection query)
        {
            var artist = query["artist"] ?? "";
            var track = query["track"] ?? "";
            return query["method"] switch
            {
                "track.getsimilar" => (artist, track) switch
                {
                    ("Massive Attack", "Teardrop") => """{"similartracks":{"track":[{"name":"Glory Box","artist":{"name":"Portishead"},"match":0.9},{"name":"Trigger Hippie","artist":{"name":"Morcheeba"},"match":0.8}]}}""",
                    ("Alan Walker", "Faded") => """{"similartracks":{"track":[{"name":"Wake Me Up","artist":{"name":"Avicii"},"match":0.9}]}}""",
                    _ => """{"similartracks":{"track":[]}}""",
                },
                "artist.getsimilar" => artist switch
                {
                    "Massive Attack" => """{"similarartists":{"artist":[{"name":"Portishead"}]}}""",
                    "Drake" => """{"similarartists":{"artist":[{"name":"Future"}]}}""",
                    _ => """{"similarartists":{"artist":[{"name":"The Year"}]}}""",
                },
                "artist.gettoptracks" => artist switch
                {
                    "Portishead" => """{"toptracks":{"track":[{"name":"Roads"}]}}""",
                    "Future" => """{"toptracks":{"track":[{"name":"Mask Off"}]}}""",
                    _ => """{"toptracks":{"track":[{"name":"Full Damage"}]}}""",
                },
                // An uploader's name can be on Last.fm too, scrobbled by a few people.
                "artist.getInfo" => artist == "Drake"
                    ? """{"artist":{"name":"Drake","stats":{"listeners":"5200000"}}}"""
                    : """{"artist":{"name":"Missigno","stats":{"listeners":"40"}}}""",
                "track.getInfo" or "track.getinfo" => """{"error":6,"message":"Track not found"}""",
                "tag.gettoptracks" => query["tag"] == "Phonk"
                    ? """{"tracks":{"track":[{"name":"Murder In My Mind","artist":{"name":"Kordhell"}}]}}"""
                    : """{"tracks":{"track":[]}}""",
                _ => "{}",
            };
        }

        /// <summary>A Subsonic "ok" carrying the answer's fields.</summary>
        private static string Ok(string answer)
        {
            var fields = System.Text.Json.Nodes.JsonNode.Parse(answer.Length > 0 ? answer : "{}")!.AsObject();
            fields["status"] = "ok";
            fields["version"] = "1.16.1";
            return new System.Text.Json.Nodes.JsonObject { ["subsonic-response"] = fields }.ToJsonString();
        }

        private static HttpResponseMessage Json(string body) =>
            new(HttpStatusCode.OK) { Content = new StringContent(body, Encoding.UTF8, "application/json") };
    }

    private sealed class Factory : WebApplicationFactory<Program>
    {
        private readonly string _directory = Path.Combine(Path.GetTempPath(), "octo-similar-seed-" + Guid.NewGuid());
        public Handler Handler { get; } = new();
        private readonly Mock<IMusicMetadataService> _metadata = new();

        public Factory()
        {
            Directory.CreateDirectory(_directory);
            // Every Last.fm pick not in the library comes back as an outside copy named after it.
            _metadata.Setup(service => service.SearchSongsByArtistTitleAsync(
                    It.IsAny<string>(), It.IsAny<string>(), It.IsAny<int>(), It.IsAny<int?>()))
                .ReturnsAsync((string artist, string title, int _, int? _) =>
                    [new Song { Id = $"ext-{artist}-{title}", Artist = artist, Title = title, IsLocal = false }]);
            _metadata.Setup(service => service.PrewarmYouTubeIdsAsync(
                    It.IsAny<IEnumerable<Song>>(), It.IsAny<int>(), It.IsAny<CancellationToken>()))
                .Returns(Task.CompletedTask);
        }

        protected override void ConfigureWebHost(IWebHostBuilder builder)
        {
            builder.ConfigureAppConfiguration((_, configuration) => configuration.AddInMemoryCollection(
                new Dictionary<string, string?>
                {
                    ["Subsonic:Url"] = "http://navidrome.test",
                    ["Subsonic:AutoDetectDownloadPath"] = "false",
                    ["Library:DownloadPath"] = _directory,
                    ["Soulseek:BaseUrl"] = "http://127.0.0.1:1",
                    ["YouTube:ShimUrl"] = "http://127.0.0.1:1",
                    ["LastFm:ApiKey"] = "test-key",
                    ["LastFm:EnableRadio"] = "true",
                }));
            builder.ConfigureServices(services =>
            {
                services.RemoveAll<IHostedService>();
                services.RemoveAll<IHttpClientFactory>();
                services.AddSingleton<IHttpClientFactory>(new ReviewFixtures.OneClientFactory(Handler));
                services.RemoveAll<IMusicMetadataService>();
                services.AddSingleton(_metadata.Object);
                services.RemoveAll<Octo.Services.Admin.SettingsFileWriter>();
                services.AddSingleton(new Octo.Services.Admin.SettingsFileWriter(Path.Combine(_directory, "settings.json")));
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
    }
}
