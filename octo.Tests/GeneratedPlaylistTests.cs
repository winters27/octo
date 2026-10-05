using System.Net;
using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;
using Microsoft.AspNetCore.Http;
using Microsoft.AspNetCore.Mvc;
using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.Logging.Abstractions;
using Microsoft.Extensions.Options;
using Octo.Models.Domain;
using Octo.Models.Radio;
using Octo.Models.Settings;
using Octo.Services.Library;
using Octo.Services.Soulseek;
using Octo.Services.Subsonic;

namespace Octo.Tests;

/// <summary>
/// Genre and decade mixes (#54). The draw is what a listener hears, so its rules are tested
/// directly: it holds still for a period, never breaks the artist cap, and keeps its share for
/// new tracks only when asked to.
/// </summary>
public class GeneratedPlaylistSelectTests
{
    private static readonly DateTime Now = new(2026, 9, 25, 12, 0, 0, DateTimeKind.Utc);

    private static JsonObject Song(int id, string artist, int? playCount = 5, DateTime? created = null) => new()
    {
        ["id"] = $"s{id}", ["title"] = $"Song {id}", ["artist"] = artist, ["artistId"] = "ar-" + artist,
        ["playCount"] = playCount, ["created"] = (created ?? Now.AddYears(-2)).ToString("O"), ["duration"] = 200,
    };

    private static List<JsonObject> Pool(int count, int artists) =>
        Enumerable.Range(0, count).Select(i => Song(i, $"Artist {i % artists}")).ToList();

    private static string Ids(IEnumerable<JsonObject> songs) => string.Join(",", songs.Select(song => song["id"]!.GetValue<string>()));

    [Fact]
    public void Select_IsDeterministicForASeed()
    {
        var pool = Pool(200, 50);

        var first = GeneratedPlaylistService.Select(pool, 50, 3, 0, 30, Now, seed: 42);
        var again = GeneratedPlaylistService.Select(pool, 50, 3, 0, 30, Now, seed: 42);
        var other = GeneratedPlaylistService.Select(pool, 50, 3, 0, 30, Now, seed: 43);

        Assert.Equal(50, first.Count);
        Assert.Equal(Ids(first), Ids(again));
        Assert.NotEqual(Ids(first), Ids(other));
    }

    [Fact]
    public void Select_NeverExceedsTheArtistCap_EvenWhenThatLeavesTheMixShort()
    {
        var pool = Pool(100, 4);

        var drawn = GeneratedPlaylistService.Select(pool, 50, 3, 0, 30, Now, seed: 7);

        Assert.Equal(12, drawn.Count);
        Assert.All(drawn.GroupBy(song => song["artist"]!.GetValue<string>()), group => Assert.Equal(3, group.Count()));
    }

    [Fact]
    public void Select_NewShareFillsFromNewTracksFirst()
    {
        var pool = Pool(100, 100);
        for (var i = 0; i < 10; i++) pool[i * 10]["playCount"] = 0;

        var drawn = GeneratedPlaylistService.Select(pool, 20, 3, 50, 30, Now, seed: 1);

        Assert.Equal(20, drawn.Count);
        Assert.Equal(10, drawn.Count(song => GeneratedPlaylistService.IsNew(song, Now, 30)));
    }

    [Fact]
    public void Select_NewShareZero_IgnoresNewness()
    {
        var old = Pool(100, 100);
        var fresh = Pool(100, 100);
        foreach (var song in fresh.Take(50)) song["playCount"] = 0;

        Assert.Equal(Ids(GeneratedPlaylistService.Select(old, 20, 3, 0, 30, Now, seed: 9)),
            Ids(GeneratedPlaylistService.Select(fresh, 20, 3, 0, 30, Now, seed: 9)));
    }

    [Fact]
    public void IsNew_NeverPlayedOrRecentlyAdded()
    {
        Assert.True(GeneratedPlaylistService.IsNew(Song(1, "A", playCount: null), Now, 30));
        Assert.True(GeneratedPlaylistService.IsNew(Song(1, "A", playCount: 0), Now, 30));
        Assert.True(GeneratedPlaylistService.IsNew(Song(1, "A", playCount: 4, created: Now.AddDays(-3)), Now, 30));
        Assert.False(GeneratedPlaylistService.IsNew(Song(1, "A", playCount: 4, created: Now.AddDays(-45)), Now, 30));
    }

    [Fact]
    public void ApplyHysteresis_CreatesAt20_KeepsDownTo10_DropsBelow()
    {
        var counts = new Dictionary<string, int>
        {
            ["genre:New"] = 20, ["genre:Almost"] = 19, ["genre:Kept"] = 10, ["genre:Gone"] = 9,
        };

        var active = GeneratedPlaylistService.ApplyHysteresis(counts, ["genre:Kept", "genre:Gone"], 20, 10, 20);

        Assert.Equal(["genre:New", "genre:Kept"], active);
    }

    [Fact]
    public void ApplyHysteresis_ShowsTheLargestFirstUpToTheLimit()
    {
        var counts = new Dictionary<string, int> { ["genre:B"] = 50, ["genre:A"] = 50, ["decade:1990"] = 400, ["genre:C"] = 30 };

        Assert.Equal(["decade:1990", "genre:A", "genre:B"], GeneratedPlaylistService.ApplyHysteresis(counts, [], 20, 10, 3));
    }

    [Fact]
    public void ParseGenres_OneEntryPerGenreWhateverTheCase_AndNeverAYearOrABlockedValue()
    {
        var rows = JsonNode.Parse("""
            [{"value":"Rock","songCount":120},{"value":"rock","songCount":4},{"value":"1990s","songCount":80},
             {"value":"Music","songCount":300},{"value":" ","songCount":10},{"value":"Trip-Hop","songCount":25}]
            """)!.AsArray();

        var counts = GeneratedPlaylistService.ParseGenres(rows, new GenreSettings().EffectiveBlocklist());

        Assert.Equal(new Dictionary<string, int> { ["genre:Rock"] = 120, ["genre:Trip-Hop"] = 25 }, counts);
    }

    [Fact]
    public void Ids_AreOctoShapedAndPerListener()
    {
        var alice = GeneratedPlaylistService.PlaylistId("alice", "genre:Rock");

        Assert.Equal(22, alice.Length);
        Assert.StartsWith("og", alice);
        Assert.Equal(alice, GeneratedPlaylistService.PlaylistId(" ALICE ", "genre:Rock"));
        Assert.NotEqual(alice, GeneratedPlaylistService.PlaylistId("bob", "genre:Rock"));
        Assert.NotEqual(alice, GeneratedPlaylistService.PlaylistId("alice", "genre:Pop"));
    }

    [Fact]
    public void PeriodIndex_ChangesOncePerPeriod()
    {
        var start = new DateTime(2026, 9, 25, 0, 0, 0, DateTimeKind.Utc);

        Assert.Equal(GeneratedPlaylistService.PeriodIndex(start, 24), GeneratedPlaylistService.PeriodIndex(start.AddHours(23.9), 24));
        Assert.Equal(GeneratedPlaylistService.PeriodIndex(start, 24) + 1, GeneratedPlaylistService.PeriodIndex(start.AddHours(24), 24));
        Assert.NotEqual(GeneratedPlaylistService.Seed("alice", "genre:Rock", 1), GeneratedPlaylistService.Seed("alice", "genre:Rock", 2));
    }

    [Theory]
    [InlineData("{0} Mix", "Rock Mix")]
    [InlineData("Mix: {0}", "Mix: Rock")]
    [InlineData("", "Rock Mix")]
    [InlineData("No placeholder", "Rock Mix")]
    [InlineData("{0} {1} Mix", "Rock Mix")]
    public void Name_FollowsTheFormat_AndABrokenFormatFallsBack(string format, string expected)
        => Assert.Equal(expected, new GeneratedPlaylistSettings { NameFormat = format }.Name("Rock"));

    private static Song Station(int i, bool local = false) =>
        new() { Id = $"st{i}", Artist = $"Radio Artist {i}", Title = $"Radio Song {i}", IsLocal = local };

    [Fact]
    public void Blend_PutsNewLibrarySongsFromTheEnd_AndNeverRepeatsOne()
    {
        var songs = Enumerable.Range(0, 10).Select(i => Station(i)).ToList();
        var candidates = new List<JsonObject>
        {
            Song(100, "Radio Artist 3", playCount: 0),
            Song(101, "Library One", playCount: 0),
            Song(102, "Library Two", playCount: 7),
            Song(103, "Library Three", playCount: 0),
        };
        candidates[0]["title"] = "Radio Song 3";
        candidates[1]["suffix"] = "mp3";
        candidates[1]["bitRate"] = 320;
        candidates[1]["isrc"] = new JsonArray("USRC17607839", "us-rc1-76-07840");

        var blended = GeneratedPlaylistService.Blend(songs, candidates, 2, 30, Now);

        Assert.Equal(10, blended.Count);
        Assert.Equal("s101", blended[9].Id);
        Assert.Equal("s103", blended[4].Id);
        Assert.True(blended[9].IsLocal);
        Assert.Equal("mp3", blended[9].Suffix);
        Assert.Equal(320, blended[9].BitRate);
        // The library's own ISRCs go back out exactly as Navidrome sent them.
        Assert.Equal(["USRC17607839", "us-rc1-76-07840"], blended[9].IsrcsForClients());
        Assert.Empty(blended[4].IsrcsForClients());
        Assert.Equal(8, blended.Count(song => song.Id.StartsWith("st", StringComparison.Ordinal)));
    }

    [Fact]
    public void Blend_NothingNew_LeavesTheStationAlone()
    {
        var songs = Enumerable.Range(0, 10).Select(i => Station(i)).ToList();

        Assert.Same(songs, GeneratedPlaylistService.Blend(songs, [Song(1, "Library", playCount: 3)], 2, 30, Now));
    }
}

/// <summary>
/// The service end to end against a stand-in Navidrome: counting, listing, the draw a playlist
/// read returns, and the Discovery blend.
/// </summary>
public class GeneratedPlaylistServiceTests : IDisposable
{
    private readonly string _dir = Path.Combine(Path.GetTempPath(), "octo-mixes-" + Guid.NewGuid());

    public void Dispose()
    {
        try { Directory.Delete(_dir, true); } catch { }
        GC.SuppressFinalize(this);
    }

    internal sealed class LibraryNavidrome : HttpMessageHandler
    {
        public int GenreCalls;
        public List<string> Calls { get; } = [];

        protected override Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken ct)
        {
            var path = request.RequestUri!.AbsolutePath.Trim('/');
            var query = System.Web.HttpUtility.ParseQueryString(request.RequestUri.Query);
            lock (Calls) Calls.Add(path + "?" + request.RequestUri.Query);
            string body = path switch
            {
                "rest/getGenres" => Ok("\"genres\":{\"genre\":[{\"value\":\"Rock\",\"songCount\":40},{\"value\":\"Polka\",\"songCount\":3}]}"),
                "rest/getSongsByGenre" => Ok("\"songsByGenre\":{\"song\":[" + Songs(40, query["offset"] == "0" ? 0 : 1000) + "]}"),
                "rest/getRandomSongs" when query["fromYear"] == "1990" => Ok("\"randomSongs\":{\"song\":[" + Songs(25, 2000) + "]}"),
                "rest/getRandomSongs" when query["fromYear"] is null => Ok("\"randomSongs\":{\"song\":[" + Songs(30, 3000, playCount: 0) + "]}"),
                "rest/getRandomSongs" => Ok("\"randomSongs\":{\"song\":[]}"),
                _ => Ok(""),
            };
            if (path == "rest/getGenres") Interlocked.Increment(ref GenreCalls);
            return Task.FromResult(ReviewFixtures.Json(body));
        }

        private static string Ok(string inner) =>
            "{\"subsonic-response\":{\"status\":\"ok\",\"version\":\"1.16.1\"" + (inner.Length > 0 ? "," + inner : "") + "}}";

        private static string Songs(int count, int first, int playCount = 5) => string.Join(",", Enumerable.Range(first, count).Select(i =>
            $"{{\"id\":\"lib{i}\",\"title\":\"Track {i}\",\"artist\":\"Artist {i % 20}\",\"artistId\":\"ar{i % 20}\",\"duration\":200,\"playCount\":{playCount},\"suffix\":\"flac\",\"bitRate\":900}}"));
    }

    private GeneratedPlaylistService Service(LibraryNavidrome navidrome, GeneratedPlaylistSettings settings)
    {
        var services = new ServiceCollection()
            .AddSingleton<IHttpClientFactory>(new ReviewFixtures.OneClientFactory(navidrome))
            .AddSingleton<IOptionsMonitor<SubsonicSettings>>(TestOptions.Monitor(new SubsonicSettings { Url = "http://navidrome.test" }))
            .AddSingleton<IHttpContextAccessor>(new HttpContextAccessor())
            .AddScoped<SubsonicProxyService>()
            .BuildServiceProvider();
        return new GeneratedPlaylistService(Path.Combine(_dir, "generated-playlists.json"),
            services.GetRequiredService<IServiceScopeFactory>(), TestOptions.Monitor(settings),
            TestOptions.Monitor(new GenreSettings()), NullLogger<GeneratedPlaylistService>.Instance);
    }

    private static readonly Dictionary<string, string> Auth = new() { ["u"] = "alice", ["t"] = "token", ["s"] = "salt", ["v"] = "1.16.1", ["c"] = "test", ["id"] = "not-for-navidrome" };

    [Fact]
    public async Task List_CountsTheLibraryAndOffersWhatClearsTheThreshold()
    {
        var navidrome = new LibraryNavidrome();
        var service = Service(navidrome, new GeneratedPlaylistSettings { Enabled = true });

        var mixes = await service.ListAsync("alice", Auth);

        Assert.Equal(["Rock Mix", "1990s Mix"], mixes.Select(mix => mix.Name));
        Assert.All(mixes, mix => Assert.Equal("alice", mix.Owner));
        Assert.DoesNotContain(navidrome.Calls, call => call.Contains("id=not-for-navidrome", StringComparison.Ordinal));
        Assert.Equal(mixes[0], service.Find("alice", mixes[0].Id));
        Assert.Null(service.Find("bob", mixes[0].Id));
    }

    [Fact]
    public async Task List_WhileFresh_DoesNotCountAgain_AndSurvivesARestart()
    {
        var navidrome = new LibraryNavidrome();
        var settings = new GeneratedPlaylistSettings { Enabled = true };
        await Service(navidrome, settings).ListAsync("alice", Auth);
        await Service(navidrome, settings).ListAsync("alice", Auth);

        var restarted = Service(navidrome, settings);
        var mixes = await restarted.ListAsync("alice", Auth);

        Assert.Equal(1, navidrome.GenreCalls);
        Assert.Equal(2, mixes.Count);
    }

    [Fact]
    public async Task List_Off_IsNothing()
        => Assert.Empty(await Service(new LibraryNavidrome(), new GeneratedPlaylistSettings()).ListAsync("alice", Auth));

    [Fact]
    public async Task Materialize_DrawsFromTheGenre_AndHoldsStillForThePeriod()
    {
        var navidrome = new LibraryNavidrome();
        var service = Service(navidrome, new GeneratedPlaylistSettings { Enabled = true, TrackCount = 10, MaxPerArtist = 1 });
        var rock = (await service.ListAsync("alice", Auth)).Single(mix => mix.Kind == "genre");

        var first = await service.MaterializeAsync("alice", rock, Auth, CancellationToken.None);
        var second = await service.MaterializeAsync("alice", rock, Auth, CancellationToken.None);

        Assert.Equal(10, first.Count);
        Assert.Equal(10, first.Select(song => song["artist"]!.GetValue<string>()).Distinct().Count());
        Assert.Equal(first.Select(song => song.ToJsonString()), second.Select(song => song.ToJsonString()));
        Assert.Single(navidrome.Calls, call => call.StartsWith("rest/getSongsByGenre", StringComparison.Ordinal));
    }

    [Fact]
    public async Task Blend_OnlyDiscovery_AndOnlyWithAShare()
    {
        var songs = Enumerable.Range(0, 10).Select(i => new Song { Id = $"st{i}", Artist = $"A{i}", Title = $"T{i}" }).ToList();
        var discovery = new LastFmRadioStation { Id = "orStation", Kind = LastFmRadioStationKind.Discovery, ChangedUtc = DateTime.UtcNow, ValidUntilUtc = DateTime.UtcNow.AddHours(6) };
        var yourMix = new LastFmRadioStation { Id = "orMix", Kind = LastFmRadioStationKind.YourMix, ChangedUtc = DateTime.UtcNow };

        var none = Service(new LibraryNavidrome(), new GeneratedPlaylistSettings());
        Assert.Same(songs, await none.BlendIntoDiscoveryAsync("alice", discovery, songs, Auth, CancellationToken.None));

        var shared = Service(new LibraryNavidrome(), new GeneratedPlaylistSettings { NewShare = 20 });
        Assert.Same(songs, await shared.BlendIntoDiscoveryAsync("alice", yourMix, songs, Auth, CancellationToken.None));
        var blended = await shared.BlendIntoDiscoveryAsync("alice", discovery, songs, Auth, CancellationToken.None);
        Assert.Equal(2, blended.Count(song => song.IsLocal));
    }
}

/// <summary>What a client is told about a mix and its songs, and that a library MP3 is an MP3.</summary>
public class GeneratedPlaylistResponseTests
{
    private static SubsonicResponseBuilder Builder() =>
        new(new ExternalIdRegistry(), Options.Create(new SubsonicSettings()));

    private static readonly GeneratedPlaylist Mix = new("og" + new string('a', 20), "genre:Rock", "genre", "Rock", "Rock Mix",
        "alice", 40, new DateTime(2026, 9, 25, 0, 0, 0, DateTimeKind.Utc), new DateTime(2026, 9, 26, 0, 0, 0, DateTimeKind.Utc));

    private static readonly JsonObject Entry = JsonNode.Parse(
        """{"id":"lib1","title":"Track","artist":"Artist","duration":200,"playCount":3,"genres":[{"name":"Rock"}],"replayGain":{"trackGain":-6.1}}""")!.AsObject();

    [Fact]
    public void Row_IsReadOnlyAndOwnedByTheListener()
    {
        var fields = Builder().GeneratedPlaylistFields(Mix, new GeneratedPlaylistSettings { TrackCount = 100 });

        Assert.Equal("Rock Mix", fields["name"]);
        Assert.Equal("alice", fields["owner"]);
        Assert.Equal(true, fields["readonly"]);
        Assert.Equal(40, fields["songCount"]);
        Assert.Equal(Mix.Id, fields["coverArt"]);
    }

    [Fact]
    public void Playlist_Json_PassesNavidromesSongsThrough()
    {
        var json = Assert.IsType<JsonResult>(Builder().CreateGeneratedPlaylistResponse("json", Mix, new GeneratedPlaylistSettings(), [Entry]));
        using var document = JsonDocument.Parse(JsonSerializer.Serialize(json.Value));
        var playlist = document.RootElement.GetProperty("subsonic-response").GetProperty("playlist");

        Assert.Equal(1, playlist.GetProperty("songCount").GetInt32());
        Assert.Equal(200, playlist.GetProperty("duration").GetInt32());
        var song = playlist.GetProperty("entry")[0];
        Assert.Equal("lib1", song.GetProperty("id").GetString());
        Assert.Equal(-6.1, song.GetProperty("replayGain").GetProperty("trackGain").GetDouble());
    }

    [Fact]
    public void Playlist_Xml_CarriesEveryScalarAsAnAttribute()
    {
        var xml = Assert.IsType<ContentResult>(Builder().CreateGeneratedPlaylistResponse("xml", Mix, new GeneratedPlaylistSettings(), [Entry]));
        var playlist = System.Xml.Linq.XDocument.Parse(xml.Content!).Root!.Elements().Single();
        var entry = playlist.Elements().Single();

        Assert.Equal("true", playlist.Attribute("readonly")!.Value);
        Assert.Equal("lib1", entry.Attribute("id")!.Value);
        Assert.Equal("200", entry.Attribute("duration")!.Value);
        Assert.Null(entry.Attribute("genres"));
    }

    /// <summary>A strict client prepares its decoder from these; an MP3 declared as FLAC fails.</summary>
    [Fact]
    public void ConvertSong_LocalMp3_IsDeclaredAsMp3()
    {
        var mp3 = Builder().ConvertSongToJson(new Song { Id = "l1", Artist = "A", Title = "T", IsLocal = true, Suffix = "MP3", BitRate = 320, Duration = 100 });
        var unknown = Builder().ConvertSongToJson(new Song { Id = "l2", Artist = "A", Title = "T", IsLocal = true, Duration = 100 });

        Assert.Equal("mp3", mp3["suffix"]);
        Assert.Equal("audio/mpeg", mp3["contentType"]);
        Assert.Equal(320, mp3["bitRate"]);
        Assert.Equal("flac", unknown["suffix"]);
        Assert.Equal(1411, unknown["bitRate"]);
    }
}

/// <summary>Mixes through the Subsonic API, as a client sees them.</summary>
public sealed class GeneratedPlaylistControllerTests
{
    private sealed class MixWebFactory : Microsoft.AspNetCore.Mvc.Testing.WebApplicationFactory<Program>
    {
        private readonly string _directory = Path.Combine(Path.GetTempPath(), "octo-mix-web-" + Guid.NewGuid());
        public GeneratedPlaylistServiceTests.LibraryNavidrome Navidrome { get; } = new();

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
                        ["Octo:StateDirectory"] = _directory,
                        ["GeneratedPlaylists:Enabled"] = "true",
                    }));
            builder.ConfigureServices(services =>
            {
                Microsoft.Extensions.DependencyInjection.Extensions.ServiceCollectionDescriptorExtensions
                    .RemoveAll<Microsoft.Extensions.Hosting.IHostedService>(services);
                Microsoft.Extensions.DependencyInjection.Extensions.ServiceCollectionDescriptorExtensions
                    .RemoveAll<IHttpClientFactory>(services);
                services.AddSingleton<IHttpClientFactory>(new ReviewFixtures.OneClientFactory(Navidrome));
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

    private const string Auth = "u=alice&t=token&s=salt&v=1.16.1&c=test&f=json";

    private static async Task<JsonElement> MixAsync(HttpClient client)
    {
        using var list = JsonDocument.Parse(await client.GetStringAsync($"/rest/getPlaylists.view?{Auth}"));
        return list.RootElement.GetProperty("subsonic-response").GetProperty("playlists").GetProperty("playlist")
            .EnumerateArray().Single(row => row.GetProperty("name").GetString() == "Rock Mix").Clone();
    }

    [Fact]
    public async Task GetPlaylists_ListsTheListenersMixesReadOnly()
    {
        await using var factory = new MixWebFactory();
        using var client = factory.CreateClient();

        var mix = await MixAsync(client);

        Assert.StartsWith("og", mix.GetProperty("id").GetString());
        Assert.Equal("alice", mix.GetProperty("owner").GetString());
        Assert.True(mix.GetProperty("readonly").GetBoolean());
    }

    [Fact]
    public async Task GetPlaylist_AMix_IsItsDraw()
    {
        await using var factory = new MixWebFactory();
        using var client = factory.CreateClient();
        var id = (await MixAsync(client)).GetProperty("id").GetString();

        using var detail = JsonDocument.Parse(await client.GetStringAsync($"/rest/getPlaylist.view?{Auth}&id={id}"));
        var playlist = detail.RootElement.GetProperty("subsonic-response").GetProperty("playlist");

        Assert.Equal("Rock Mix", playlist.GetProperty("name").GetString());
        var entries = playlist.GetProperty("entry").EnumerateArray().ToList();
        Assert.Equal(40, entries.Count);
        Assert.All(entries, entry => Assert.StartsWith("lib", entry.GetProperty("id").GetString()));
    }

    [Fact]
    public async Task UpdatePlaylist_OnAMix_IsRefused()
    {
        await using var factory = new MixWebFactory();
        using var client = factory.CreateClient();
        var id = (await MixAsync(client)).GetProperty("id").GetString();

        using var answer = JsonDocument.Parse(await client.GetStringAsync($"/rest/updatePlaylist.view?{Auth}&playlistId={id}&name=Mine"));

        Assert.Equal(70, answer.RootElement.GetProperty("subsonic-response").GetProperty("error").GetProperty("code").GetInt32());
        Assert.DoesNotContain(factory.Navidrome.Calls, call => call.StartsWith("rest/updatePlaylist", StringComparison.Ordinal));
    }

    [Fact]
    public async Task GetCoverArt_ForAMix_IsAnImage()
    {
        await using var factory = new MixWebFactory();
        using var client = factory.CreateClient();
        var id = (await MixAsync(client)).GetProperty("id").GetString();

        using var cover = await client.GetAsync($"/rest/getCoverArt.view?{Auth}&id={id}");

        cover.EnsureSuccessStatusCode();
        Assert.Equal("image/jpeg", cover.Content.Headers.ContentType?.MediaType);
    }
}
