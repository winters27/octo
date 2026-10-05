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
using Octo.Models.Domain;
using Octo.Models.Radio;
using Octo.Models.Settings;
using Octo.Services;
using Octo.Services.CoverArt;
using Octo.Services.LastFm;
using Octo.Services.Metadata;
using Octo.Services.Soulseek;
using Octo.Services.YouTube;

namespace Octo.Tests;

/// <summary>
/// The length a client is shown for a song found outside the library. Half of these went
/// out with the 180s placeholder, which the Octo app shows as no length at all: search rows
/// past the first page, and station rows Last.fm had no length for.
/// </summary>
public class SongLengthTests
{
    // ---- The rules ----------------------------------------------------------------

    [Theory]
    [InlineData(29, null)]
    [InlineData(30, 30)]
    [InlineData(417, 417)]
    [InlineData(1200, 1200)]
    [InlineData(1201, null)]
    [InlineData(3600, null)]
    public void SaneVideoLength_KeepsOnlyWhatCouldBeOneSong(int seconds, int? expected)
    {
        Assert.Equal(expected, SongLength.SaneVideoLength(seconds));
    }

    [Fact]
    public void Remember_StrongerSourceReplacesWeaker_NeverTheOtherWay()
    {
        var routing = new SoulseekRouting { Artist = "Daft Punk", Title = "Emotion" };

        Assert.True(SongLength.Remember(routing, 430, LengthSource.Video));
        Assert.True(SongLength.Remember(routing, 418, LengthSource.LastFm));
        Assert.True(SongLength.Remember(routing, 417, LengthSource.Deezer));
        Assert.Equal((417, LengthSource.Deezer), SongLength.Shown(routing));

        Assert.False(SongLength.Remember(routing, 418, LengthSource.LastFm));
        Assert.False(SongLength.Remember(routing, 430, LengthSource.Video));
        Assert.Equal((417, LengthSource.Deezer), SongLength.Shown(routing));
    }

    [Fact]
    public void Remember_NeverStoresAMissingZeroOrImplausibleLength()
    {
        var routing = new SoulseekRouting { Artist = "A", Title = "T" };

        Assert.False(SongLength.Remember(routing, null, LengthSource.Deezer));
        Assert.False(SongLength.Remember(routing, 0, LengthSource.LastFm));
        Assert.False(SongLength.Remember(routing, 3600, LengthSource.Video));
        Assert.False(SongLength.Remember(routing, 12, LengthSource.Video));

        Assert.Equal((null, LengthSource.None), SongLength.Shown(routing));
    }

    [Fact]
    public void Remember_ALongMetadataLengthIsNotBoundLikeAVideo()
    {
        // The range guards against a video carrying more than the song. A catalog length
        // for a 25-minute track is simply the track.
        var routing = new SoulseekRouting { Artist = "A", Title = "T" };
        Assert.True(SongLength.Remember(routing, 1500, LengthSource.Deezer));
    }

    // ---- The registry keeps it ----------------------------------------------------

    [Fact]
    public void Registry_ReMintingASong_KeepsTheLengthALookupFound()
    {
        var registry = new ExternalIdRegistry();
        var id = registry.Register(new SoulseekRouting { Artist = "Justice", Title = "Genesis" });
        Assert.True(registry.RememberLength(id, 234, LengthSource.Deezer));

        // The next search mints a fresh routing for the same song.
        var again = registry.Register(new SoulseekRouting { Artist = "Justice", Title = "Genesis" });

        Assert.Equal(id, again);
        Assert.Equal((234, LengthSource.Deezer), SongLength.Shown(registry.Lookup(id)!));
    }

    [Fact]
    public void Registry_RememberLength_LeavesTheDownloadExpectationAlone()
    {
        // Duration is what a download ranks and checks peer files against. A length found
        // only so a row can show one must not start rejecting files.
        var registry = new ExternalIdRegistry();
        var id = registry.Register(new SoulseekRouting { Artist = "Kavinsky", Title = "Prelude" });

        registry.RememberLength(id, 95, LengthSource.LastFm);

        var routing = registry.Lookup(id)!;
        Assert.Null(routing.Duration);
        Assert.Null(routing.YouTubeId);
        Assert.Equal(95, routing.ShownDuration);
    }

    [Fact]
    public void Registry_RememberLength_IgnoresUnknownIdsAndNonSongs()
    {
        var registry = new ExternalIdRegistry();
        var album = registry.Register(new SoulseekRouting { Kind = RoutingKind.Album, Artist = "A", Album = "B" });

        Assert.False(registry.RememberLength("nope", 200, LengthSource.Deezer));
        Assert.False(registry.RememberLength(album, 200, LengthSource.Deezer));
    }

    [Fact]
    public void Registry_LengthOutlivesARestart()
    {
        var path = Path.Combine(Path.GetTempPath(), "octo-lengths-" + Guid.NewGuid() + ".json");
        try
        {
            string id;
            using (var first = new ExternalIdRegistry(path))
            {
                id = first.Register(new SoulseekRouting { Artist = "Daft Punk", Title = "Emotion" });
                first.RememberLength(id, 417, LengthSource.Video);
            }

            using var second = new ExternalIdRegistry(path);
            Assert.Equal((417, LengthSource.Video), SongLength.Shown(second.Lookup(id)!));
        }
        finally { File.Delete(path); }
    }

    // ---- Filled from each source, in order ----------------------------------------

    [Fact]
    public async Task Placeholder_CarriesTheRememberedLength_InsteadOfThePlaceholder()
    {
        var fixture = new LengthFixture();
        var svc = fixture.Service();
        var first = (await svc.SearchSongsByArtistTitleAsync("Justice", "Genesis")).Single();
        Assert.Equal(180, first.Duration);

        fixture.Registry.RememberLength(first.Id, 234, LengthSource.Deezer);

        var next = (await svc.SearchSongsByArtistTitleAsync("Justice", "Genesis")).Single();
        Assert.Equal(first.Id, next.Id);
        Assert.Equal(234, next.Duration);
    }

    [Fact]
    public async Task Placeholder_HandedInLength_LosesToARememberedDeezerLength()
    {
        var fixture = new LengthFixture();
        var svc = fixture.Service();
        var first = (await svc.SearchSongsByArtistTitleAsync("Mr. Oizo", "Positif", 1, 200)).Single();
        Assert.Equal(200, first.Duration);

        fixture.Registry.RememberLength(first.Id, 207, LengthSource.Deezer);

        var next = (await svc.SearchSongsByArtistTitleAsync("Mr. Oizo", "Positif", 1, 200)).Single();
        Assert.Equal(207, next.Duration);
    }

    [Fact]
    public async Task CompleteSongLengths_Deezer_IsTriedFirst()
    {
        var fixture = new LengthFixture { Deezer = { ["Justice Genesis"] = 234 }, LastFm = { ["Justice|Genesis"] = 240 } };
        var song = await fixture.StationRowAsync("Justice", "Genesis");

        Assert.Equal((234, LengthSource.Deezer), fixture.Shown(song));
        Assert.DoesNotContain(fixture.Requests, url => url.Contains("track.getInfo"));
    }

    [Fact]
    public async Task CompleteSongLengths_LastFm_WhenDeezerHasNoMatch()
    {
        var fixture = new LengthFixture { LastFm = { ["Kavinsky|Prelude"] = 95 }, Video = { ["Kavinsky Prelude"] = 120 } };
        var song = await fixture.StationRowAsync("Kavinsky", "Prelude");

        Assert.Equal((95, LengthSource.LastFm), fixture.Shown(song));
        Assert.DoesNotContain(fixture.Requests, url => url.Contains("/meta"));
    }

    [Fact]
    public async Task CompleteSongLengths_Video_OnlyWhenNoMetadataLengthExists()
    {
        var fixture = new LengthFixture { Video = { ["Daft Punk Emotion"] = 417 } };
        var song = await fixture.StationRowAsync("Daft Punk", "Emotion");

        Assert.Equal((417, LengthSource.Video), fixture.Shown(song));
        // Length only: the video is not pinned for playback and the download expectation
        // is untouched.
        var routing = fixture.Registry.Lookup(song.Id)!;
        Assert.Null(routing.YouTubeId);
        Assert.Null(routing.Duration);
    }

    [Fact]
    public async Task CompleteSongLengths_ImplausibleVideo_LeavesTheSongWithoutALength()
    {
        var fixture = new LengthFixture { Video = { ["Someone Live Set"] = 3600 } };
        var song = await fixture.StationRowAsync("Someone", "Live Set");

        Assert.Equal((null, LengthSource.None), fixture.Shown(song));
        var next = (await fixture.Service().SearchSongsByArtistTitleAsync("Someone", "Live Set")).Single();
        Assert.Equal(180, next.Duration);
    }

    [Fact]
    public async Task CompleteSongLengths_NothingKnown_InventsNothing()
    {
        var fixture = new LengthFixture();
        var song = await fixture.StationRowAsync("Nobody", "Nothing");

        Assert.Equal((null, LengthSource.None), fixture.Shown(song));
    }

    [Fact]
    public async Task CompleteSongLengths_AnswersFromDeezersCacheInline()
    {
        // A search for the same song already asked Deezer, so this response needs no lookup.
        var fixture = new LengthFixture { Deezer = { ["Justice Genesis"] = 234 } };
        var svc = fixture.Service();
        await fixture.DeezerService.EnrichTrackAsync("Justice", "Genesis");
        var song = (await svc.SearchSongsByArtistTitleAsync("Justice", "Genesis")).Single();

        svc.CompleteSongLengths([song]);

        Assert.Equal(234, song.Duration);
    }

    [Fact]
    public async Task CompleteSongLengths_LooksUpAtMostTwentyRowsPerResponse()
    {
        var fixture = new LengthFixture();
        var svc = fixture.Service();
        var songs = new List<Song>();
        for (var i = 0; i < 30; i++)
            songs.Add((await svc.SearchSongsByArtistTitleAsync("Artist", $"Song {i}")).Single());

        svc.CompleteSongLengths(songs);
        await svc.LastLengthWarm;

        // One row, one lookup: a miss may try the title alone too, but twenty rows are asked about.
        Assert.Equal(20, fixture.Requests.Count(url => url.Contains("api.deezer.com/search?q=Artist")));
    }

    [Fact]
    public async Task ResolveTopDurations_ImplausibleVideo_KeepsTheShownLength()
    {
        // The video is still pinned for playback as before; only what the row shows is held
        // to the sane range.
        var fixture = new LengthFixture { Video = { ["Someone Live Set"] = 3600 } };
        var svc = fixture.Service();
        var song = (await svc.SearchSongsByArtistTitleAsync("Someone", "Live Set")).Single();

        await svc.ResolveTopDurationsAsync([song]);

        Assert.Equal(180, song.Duration);
        Assert.Equal("vid-Someone Live Set", fixture.Registry.Lookup(song.Id)!.YouTubeId);
    }

    [Fact]
    public async Task ResolveTopDurations_InTheBackground_LeavesTheSongAndWritesTheRouting()
    {
        var fixture = new LengthFixture { Video = { ["Daft Punk Emotion"] = 417 } };
        var svc = fixture.Service();
        var song = (await svc.SearchSongsByArtistTitleAsync("Daft Punk", "Emotion")).Single();

        await svc.ResolveTopDurationsAsync([song], background: true);

        Assert.Equal(180, song.Duration);
        var routing = fixture.Registry.Lookup(song.Id)!;
        Assert.Equal(417, routing.Duration);
        Assert.Equal("vid-Daft Punk Emotion", routing.YouTubeId);
        Assert.Contains(fixture.Requests, url => url.Contains("/meta") && url.Contains("bg=1"));
    }

    [Fact]
    public async Task ResolveTopDurations_ABackgroundPassInFlight_LeavesRoomForAForegroundLookup()
    {
        var held = new TaskCompletionSource();
        var fixture = new LengthFixture { Video = { ["Daft Punk Emotion"] = 417 } };
        fixture.Hold = uri => uri.Query.Contains("bg=1") ? held.Task : Task.CompletedTask;
        var svc = fixture.Service();
        var searched = new List<Song>();
        for (var i = 0; i < 8; i++) searched.Add((await svc.SearchSongsByArtistTitleAsync("Filler", $"Song {i}")).Single());
        var song = (await svc.SearchSongsByArtistTitleAsync("Daft Punk", "Emotion")).Single();

        // The pass after a search, stuck on a slow shim with every permit it can take.
        var background = svc.ResolveTopDurationsAsync(searched, background: true);
        for (var wait = 0; wait < 200 && fixture.Requests.Count(url => url.Contains("bg=1")) < 2; wait++)
            await Task.Delay(10);

        // getSong's own lookup still gets a permit and shows the video's length.
        await svc.ResolveTopDurationsAsync([song]);
        Assert.Equal(417, song.Duration);

        held.SetResult();
        await background;
    }

    [Fact]
    public async Task ResolveTopDurations_InTheBackground_KeepsTheVideoAPlayPinned()
    {
        var fixture = new LengthFixture { Video = { ["Daft Punk Emotion"] = 417 } };
        var svc = fixture.Service();
        var song = (await svc.SearchSongsByArtistTitleAsync("Daft Punk", "Emotion")).Single();
        var routing = fixture.Registry.Lookup(song.Id)!;
        routing.YouTubeId = "playing-now";
        routing.Duration = 400;

        await svc.ResolveTopDurationsAsync([song], background: true);

        Assert.Equal("playing-now", routing.YouTubeId);
        Assert.Equal(400, routing.Duration);
        Assert.DoesNotContain(fixture.Requests, url => url.Contains("/meta"));
    }
}

/// <summary>
/// Deezer, Last.fm and the yt-dlp shim as far as a length lookup needs them. Anything not
/// listed is a miss, answered the way each service answers one.
/// </summary>
internal sealed class LengthFixture
{
    public Dictionary<string, int> Deezer { get; } = new(StringComparer.OrdinalIgnoreCase);
    public Dictionary<string, int> LastFm { get; } = new(StringComparer.OrdinalIgnoreCase);
    public Dictionary<string, int> Video { get; } = new(StringComparer.OrdinalIgnoreCase);
    public System.Collections.Concurrent.ConcurrentQueue<string> Requests { get; } = new();
    /// <summary>Awaited before a request is answered, so a test can keep some of them in flight.</summary>
    public Func<Uri, Task>? Hold { get; set; }
    public ExternalIdRegistry Registry { get; } = new();
    public DeezerMetadataService DeezerService { get; }
    private readonly HttpMessageHandler _handler;
    private SoulseekMetadataService? _service;

    public LengthFixture()
    {
        _handler = new Handler(this);
        DeezerService = new DeezerMetadataService(new Factory(_handler),
            TestOptions.Monitor(new MetadataSettings()), new Mock<ILogger<DeezerMetadataService>>().Object);
    }

    public SoulseekMetadataService Service()
    {
        if (_service is not null) return _service;
        var factory = new Factory(_handler);
        var config = new ConfigurationBuilder().AddInMemoryCollection(new Dictionary<string, string?>()).Build();
        var lastFm = new LastFmService(new HttpClient(_handler),
            TestOptions.Monitor(new LastFmSettings { ApiKey = "key" }),
            Options.Create(new MetadataSettings()), new Mock<ILogger<LastFmService>>().Object);
        return _service = new SoulseekMetadataService(
            new YouTubeResolver(factory, config, new Mock<ILogger<YouTubeResolver>>().Object),
            Registry, DeezerService,
            new CoverArtAggregator(Array.Empty<ICoverArtSource>(), new Mock<ILogger<CoverArtAggregator>>().Object),
            new Mock<ILogger<SoulseekMetadataService>>().Object, lastFm);
    }

    /// <summary>A station row with no length of its own, completed and looked up.</summary>
    public async Task<Song> StationRowAsync(string artist, string title)
    {
        var svc = Service();
        var song = (await svc.SearchSongsByArtistTitleAsync(artist, title)).Single();
        svc.CompleteSongLengths([song]);
        await svc.LastLengthWarm;
        return song;
    }

    public (int? Seconds, LengthSource Source) Shown(Song song) => SongLength.Shown(Registry.Lookup(song.Id)!);

    internal static string Answer(LengthFixture fixture, Uri uri, out HttpStatusCode status)
    {
        status = HttpStatusCode.OK;
        var query = System.Web.HttpUtility.ParseQueryString(uri.Query);
        if (uri.Host == "api.deezer.com")
        {
            var q = query["q"] ?? "";
            var hit = fixture.Deezer.FirstOrDefault(pair => pair.Key.Equals(q, StringComparison.OrdinalIgnoreCase));
            if (hit.Key is null || uri.AbsolutePath != "/search") return "{\"data\":[]}";
            var split = hit.Key.LastIndexOf(' ');
            return JsonSerializer.Serialize(new
            {
                data = new[] { new { title = hit.Key[(split + 1)..], duration = hit.Value,
                    artist = new { name = hit.Key[..split] } } }
            });
        }
        if (uri.Host == "ws.audioscrobbler.com")
        {
            var key = $"{query["artist"]}|{query["track"]}";
            if (query["method"] == "track.getInfo" && fixture.LastFm.TryGetValue(key, out var seconds))
                return $"{{\"track\":{{\"name\":\"{query["track"]}\",\"duration\":\"{seconds * 1000}\",\"artist\":{{\"name\":\"{query["artist"]}\"}}}}}}";
            return "{\"error\":6,\"message\":\"Track not found\"}";
        }
        if (uri.Host == "yt-dlp-shim" && uri.AbsolutePath == "/meta"
            && fixture.Video.TryGetValue(query["q"] ?? "", out var length))
            return $"{{\"video_id\":\"vid-{query["q"]}\",\"duration\":{length}}}";
        status = HttpStatusCode.NotFound;
        return "";
    }

    private sealed class Handler(LengthFixture fixture) : HttpMessageHandler
    {
        protected override async Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken ct)
        {
            fixture.Requests.Enqueue(request.RequestUri!.ToString());
            if (fixture.Hold is { } hold) await hold(request.RequestUri!);
            var body = Answer(fixture, request.RequestUri!, out var status);
            return new HttpResponseMessage(status)
            { Content = new StringContent(body, Encoding.UTF8, "application/json") };
        }
    }

    private sealed class Factory(HttpMessageHandler handler) : IHttpClientFactory
    {
        public HttpClient CreateClient(string name) => new(handler, disposeHandler: false);
    }
}

/// <summary>
/// What a client actually receives: search3 and a station's getPlaylist, through the real
/// metadata service, with Navidrome owning none of the songs.
/// </summary>
public sealed class SongLengthEndpointTests
{
    [Fact]
    public async Task Search3_RowsWithoutAMetadataLength_CarryOneInTheNextResponse()
    {
        await using var web = new LengthWebFactory();
        // Nine ordinary rows with Deezer lengths, so the ones under test sit past the rows
        // whose YouTube length the search resolves itself.
        for (var i = 1; i <= 9; i++) web.Fixture.Deezer[$"Filler Song{i}"] = 200 + i;
        web.SearchTracks.AddRange(Enumerable.Range(1, 9).Select(i => ("Filler", $"Song{i}")));
        web.SearchTracks.AddRange([("Daft Punk", "Emotion"), ("Kavinsky", "Prelude"),
            ("Nobody", "Nothing"), ("Justice", "Genesis")]);
        web.Fixture.Video["Daft Punk Emotion"] = 417;
        web.Fixture.LastFm["Kavinsky|Prelude"] = 95;
        web.Fixture.Video["Nobody Nothing"] = 3600;
        web.Fixture.Deezer["Justice Genesis"] = 234;
        using var client = web.CreateClient();

        var first = await web.Search3LengthsAsync(client);
        Assert.Equal(201, first["Filler|Song1"]);
        Assert.Equal(180, first["Daft Punk|Emotion"]);
        await web.Metadata.LastLengthWarm;

        var next = await web.Search3LengthsAsync(client);
        Assert.Equal(201, next["Filler|Song1"]);
        Assert.Equal(417, next["Daft Punk|Emotion"]);
        Assert.Equal(95, next["Kavinsky|Prelude"]);
        Assert.Equal(234, next["Justice|Genesis"]);
        Assert.Equal(180, next["Nobody|Nothing"]); // still the placeholder: nothing plausible
    }

    [Fact]
    public async Task GetPlaylist_StationRowsWithoutALength_CarryOneInTheNextResponse()
    {
        await using var web = new LengthWebFactory();
        web.Fixture.Deezer["Justice Genesis"] = 234;
        web.Fixture.LastFm["Kavinsky|Prelude"] = 95;
        web.Fixture.Video["Daft Punk Emotion"] = 417;
        web.InstallStation(
            new() { Artist = "Justice", Title = "Genesis" },
            new() { Artist = "Kavinsky", Title = "Prelude" },
            new() { Artist = "Daft Punk", Title = "Emotion" },
            new() { Artist = "Mr. Oizo", Title = "Positif", Duration = 207 },
            new() { Artist = "Nobody", Title = "Nothing" });
        using var client = web.CreateClient();

        var first = await web.PlaylistLengthsAsync(client);
        Assert.Equal(207, first["Mr. Oizo|Positif"]);
        Assert.Equal(180, first["Justice|Genesis"]);
        await web.Metadata.LastLengthWarm;

        var next = await web.PlaylistLengthsAsync(client);
        Assert.Equal(234, next["Justice|Genesis"]);
        Assert.Equal(95, next["Kavinsky|Prelude"]);
        Assert.Equal(417, next["Daft Punk|Emotion"]);
        Assert.Equal(207, next["Mr. Oizo|Positif"]);
        Assert.Equal(180, next["Nobody|Nothing"]);
    }
}

internal sealed class LengthWebFactory : WebApplicationFactory<Program>
{
    private readonly string _directory = Path.Combine(Path.GetTempPath(), "octo-length-web-" + Guid.NewGuid());
    public LengthFixture Fixture { get; } = new();
    public List<(string Artist, string Title)> SearchTracks { get; } = [];
    public SoulseekMetadataService Metadata => (SoulseekMetadataService)Services.GetRequiredService<IMusicMetadataService>();
    public string StationId => LastFmRadioStateStore.StationId("alice", "your-mix");

    public LengthWebFactory() => Directory.CreateDirectory(_directory);

    public void InstallStation(params LastFmRadioTrack[] tracks)
    {
        Services.GetRequiredService<LastFmRadioStateStore>().ReplaceStations("alice", [new LastFmRadioStation
        {
            Id = StationId, Key = "your-mix", Name = "Your Mix", Owner = "alice",
            Kind = LastFmRadioStationKind.YourMix, Personalized = true,
            CreatedUtc = DateTime.UtcNow.AddHours(-1), ChangedUtc = DateTime.UtcNow.AddHours(-1),
            ValidUntilUtc = DateTime.UtcNow.AddDays(1), Tracks = [.. tracks]
        }]);
    }

    public async Task<Dictionary<string, int>> Search3LengthsAsync(HttpClient client)
    {
        var body = await client.GetStringAsync(
            "/rest/search3?query=genesis&songCount=50&albumCount=0&artistCount=0&u=alice&t=token&s=salt&f=json");
        using var doc = JsonDocument.Parse(body);
        return Lengths(doc.RootElement.GetProperty("subsonic-response").GetProperty("searchResult3").GetProperty("song"));
    }

    public async Task<Dictionary<string, int>> PlaylistLengthsAsync(HttpClient client)
    {
        var body = await client.GetStringAsync($"/rest/getPlaylist?id={StationId}&u=alice&t=token&s=salt&f=json");
        using var doc = JsonDocument.Parse(body);
        return Lengths(doc.RootElement.GetProperty("subsonic-response").GetProperty("playlist").GetProperty("entry"));
    }

    private static Dictionary<string, int> Lengths(JsonElement songs) =>
        songs.EnumerateArray().ToDictionary(
            song => $"{song.GetProperty("artist").GetString()}|{song.GetProperty("title").GetString()}",
            song => song.GetProperty("duration").GetInt32());

    protected override void ConfigureWebHost(IWebHostBuilder builder)
    {
        builder.ConfigureAppConfiguration((_, configuration) => configuration.AddInMemoryCollection(
            new Dictionary<string, string?>
            {
                ["Subsonic:Url"] = "http://navidrome.test",
                ["Subsonic:AutoDetectDownloadPath"] = "false",
                ["Library:DownloadPath"] = _directory,
                ["Octo:StateDirectory"] = _directory,
                ["LastFm:ApiKey"] = "key",
                ["LastFm:EnableRadio"] = "true",
                ["LastFm:EnablePersonalizedStations"] = "true",
                ["LastFm:ExposeRadioAsPlaylists"] = "true",
            }));
        builder.ConfigureServices(services =>
        {
            services.RemoveAll<IHostedService>();
            services.RemoveAll<IHttpClientFactory>();
            services.AddSingleton<IHttpClientFactory>(new Factory(new Upstream(this)));
            services.RemoveAll<ExternalIdRegistry>();
            services.AddSingleton(Fixture.Registry);
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

    private sealed class Factory(HttpMessageHandler handler) : IHttpClientFactory
    {
        public HttpClient CreateClient(string name) => new(handler, disposeHandler: false);
    }

    /// <summary>Navidrome with an empty library, Last.fm's track.search, and the length
    /// sources from <see cref="LengthFixture"/>.</summary>
    private sealed class Upstream(LengthWebFactory web) : HttpMessageHandler
    {
        protected override Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken ct)
        {
            var uri = request.RequestUri!;
            var query = System.Web.HttpUtility.ParseQueryString(uri.Query);
            if (uri.Host == "navidrome.test")
            {
                var fields = uri.AbsolutePath.Contains("search3")
                    ? ",\"searchResult3\":{\"song\":[],\"album\":[],\"artist\":[]}" : "";
                return Ok("{\"subsonic-response\":{\"status\":\"ok\",\"version\":\"1.16.1\"" + fields + "}}");
            }
            if (uri.Host == "ws.audioscrobbler.com" && query["method"] == "track.search")
                return Ok(JsonSerializer.Serialize(new { results = new { trackmatches = new {
                    track = web.SearchTracks.Select(t => new { name = t.Title, artist = t.Artist }) } } }));
            if (uri.Host == "ws.audioscrobbler.com" && query["method"] == "artist.gettoptracks")
                return Ok("{\"toptracks\":{\"track\":[]}}");

            web.Fixture.Requests.Enqueue(uri.ToString());
            var body = LengthFixture.Answer(web.Fixture, uri, out var status);
            return Task.FromResult(new HttpResponseMessage(status)
            { Content = new StringContent(body, Encoding.UTF8, "application/json") });
        }

        private static Task<HttpResponseMessage> Ok(string body) =>
            Task.FromResult(new HttpResponseMessage(HttpStatusCode.OK)
            { Content = new StringContent(body, Encoding.UTF8, "application/json") });
    }
}
