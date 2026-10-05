using System.Collections.Concurrent;
using System.Net;
using System.Text;
using System.Text.Json;
using Microsoft.Extensions.Configuration;
using Microsoft.Extensions.Logging.Abstractions;
using Octo.Models.Settings;
using Octo.Services.Radio;
using Octo.Services.YouTube;

namespace Octo.Tests;

/// <summary>
/// YouTube Music as a radio source: finding the seed's video, its radio, which rows count as
/// music, artist radio only for the very artist, and the seed search cached.
/// </summary>
public sealed class YouTubeMusicRadioSourceTests
{
    private const string Atv = "MUSIC_VIDEO_TYPE_ATV";
    private const string Ugc = "MUSIC_VIDEO_TYPE_UGC";

    private sealed class Shim : HttpMessageHandler
    {
        public ConcurrentQueue<string> Calls { get; } = new();
        public Dictionary<string, object[]> Search { get; } = new();
        public Dictionary<string, object[]> Radio { get; } = new();
        public (string? Artist, object[] Rows) Artist { get; set; } = (null, []);
        /// <summary>Searches answer 503, as the shim does with its gate full.</summary>
        public bool Busy { get; set; }

        protected override Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken ct)
        {
            var uri = request.RequestUri!;
            Calls.Enqueue(uri.PathAndQuery);
            if (Busy && uri.AbsolutePath == "/ytm/search")
                return Task.FromResult(new HttpResponseMessage(HttpStatusCode.ServiceUnavailable));
            var query = System.Web.HttpUtility.ParseQueryString(uri.Query);
            object body = uri.AbsolutePath switch
            {
                "/ytm/search" => new { tracks = Search.GetValueOrDefault($"{query["filter"]}|{query["q"]}", []) },
                "/ytm/radio" => new { tracks = Radio.GetValueOrDefault(query["videoId"]!, []) },
                "/ytm/artist-radio" => new { artist = Artist.Artist, tracks = Artist.Rows },
                _ => new { },
            };
            return Task.FromResult(new HttpResponseMessage(HttpStatusCode.OK)
            {
                Content = new StringContent(JsonSerializer.Serialize(body), Encoding.UTF8, "application/json"),
            });
        }
    }

    private static object Row(string id, string title, string artist, int seconds, string? type = Atv) =>
        new { videoId = id, title, artists = new[] { artist }, durationSeconds = seconds, videoType = type };

    private static (YouTubeMusicRadioSource Source, Shim Shim) Source()
    {
        var shim = new Shim();
        var config = new ConfigurationBuilder().AddInMemoryCollection(
            new Dictionary<string, string?> { ["YouTube:ShimUrl"] = "http://shim.test" }).Build();
        var client = new YouTubeMusicClient(new ReviewFixtures.OneClientFactory(shim), config,
            NullLogger<YouTubeMusicClient>.Instance);
        return (new YouTubeMusicRadioSource(client, TestOptions.Monitor(new RadioSourceSettings())), shim);
    }

    private static readonly IReadOnlyDictionary<string, string> NoAuth = new Dictionary<string, string>();

    [Fact]
    public async Task ARelease_GetsItsRadio_WithoutUploadsPodcastsOrItself()
    {
        var (source, shim) = Source();
        shim.Search["songs|Massive Attack Teardrop"] = [Row("t", "Teardrop", "Massive Attack", 330)];
        shim.Radio["t"] =
        [
            Row("t", "Teardrop", "Massive Attack", 330),
            Row("g", "Glory Box", "Portishead", 305),
            Row("u", "Teardrop (cover)", "Some Channel", 300, Ugc),
            Row("p", "Episode 4", "A Podcast", 2400, "MUSIC_VIDEO_TYPE_PODCAST_EPISODE"),
        ];

        var answer = await source.SongsLikeAsync(new RadioSeed("Massive Attack", "Teardrop", 331, null), 20, NoAuth, default);

        Assert.Equal(RadioMatch.Song, answer.Match);
        var track = Assert.Single(answer.Tracks);
        Assert.Equal(("Portishead", "Glory Box", "g", RadioProvider.YouTubeMusic), (track.Artist, track.Title, track.YouTubeId, track.Provider));
    }

    [Fact]
    public async Task AnUpload_IsFoundAmongVideos_AndItsRadioKeepsUploads()
    {
        var (source, shim) = Source();
        shim.Search["videos|Missigno SPOOKY"] = [Row("s", "SPOOKY", "Missigno", 180, Ugc)];
        shim.Radio["s"] = [Row("d", "DRIFT KING", "Phonk Lord", 150, Ugc), Row("m", "Murder In My Mind", "Kordhell", 145)];

        var answer = await source.SongsLikeAsync(new RadioSeed("Missigno", "SPOOKY", 180, null), 20, NoAuth, default);

        Assert.Equal(RadioMatch.Song, answer.Match);
        Assert.Equal(["DRIFT KING", "Murder In My Mind"], answer.Tracks.Select(track => track.Title));
    }

    [Fact]
    public async Task OnlyAnotherVersionOnYouTubeMusic_IsTheOriginalsRadio()
    {
        var (source, shim) = Source();
        shim.Search["songs|Nightcore Galaxy Faded (Nightcore)"] = [Row("f", "Faded", "Nightcore Galaxy", 212)];
        shim.Radio["f"] = [Row("w", "Wake Me Up", "Avicii", 247)];

        var answer = await source.SongsLikeAsync(new RadioSeed("Nightcore Galaxy", "Faded (Nightcore)", 160, null), 20, NoAuth, default);

        Assert.Equal(RadioMatch.Original, answer.Match);
        Assert.Single(answer.Tracks);
    }

    [Fact]
    public async Task AKnownVideo_NeedsNoSearch()
    {
        var (source, shim) = Source();
        shim.Radio["known"] = [Row("n", "Next", "Someone", 200)];

        var answer = await source.SongsLikeAsync(new RadioSeed("A", "B", 200, null, YouTubeId: "known"), 20, NoAuth, default);

        Assert.Single(answer.Tracks);
        Assert.DoesNotContain(shim.Calls, call => call.StartsWith("/ytm/search", StringComparison.Ordinal));
    }

    [Fact]
    public async Task TheSeedSearch_IsAskedOnce()
    {
        var (source, shim) = Source();
        shim.Search["songs|Portishead Roads"] = [Row("r", "Roads", "Portishead", 305)];
        var seed = new RadioSeed("Portishead", "Roads", 305, null);

        await source.SongsLikeAsync(seed, 20, NoAuth, default);
        await source.SongsLikeAsync(seed, 20, NoAuth, default);

        Assert.Single(shim.Calls, call => call.StartsWith("/ytm/search", StringComparison.Ordinal));
    }

    [Fact]
    public async Task ABusyShim_IsAskedAgainNextTime_NotRememberedAsNothingFound()
    {
        var (source, shim) = Source();
        shim.Search["songs|Portishead Roads"] = [Row("r", "Roads", "Portishead", 305)];
        shim.Radio["r"] = [Row("g", "Glory Box", "Portishead", 305)];
        var seed = new RadioSeed("Portishead", "Roads", 305, null);

        shim.Busy = true;
        Assert.Empty((await source.SongsLikeAsync(seed, 20, NoAuth, default)).Tracks);
        Assert.Single(shim.Calls, call => call.StartsWith("/ytm/search", StringComparison.Ordinal));

        shim.Busy = false;
        Assert.Single((await source.SongsLikeAsync(seed, 20, NoAuth, default)).Tracks);
    }

    [Fact]
    public async Task NothingFound_IsNothing()
    {
        var (source, _) = Source();
        var answer = await source.SongsLikeAsync(new RadioSeed("Nobody", "Nothing", 100, null), 20, NoAuth, default);
        Assert.Equal(RadioMatch.None, answer.Match);
        Assert.Empty(answer.Tracks);
    }

    [Theory]
    [InlineData("Massive Attack", true)]
    [InlineData("Massive Attack Tribute Band", false)]
    public async Task ArtistRadio_IsOnlyForTheVeryArtist(string found, bool used)
    {
        var (source, shim) = Source();
        shim.Artist = (found, [Row("a", "Angel", found, 379)]);

        var answer = await source.SongsLikeAsync(new RadioSeed("Massive Attack", "", null, null), 20, NoAuth, default);

        Assert.Equal(used ? RadioMatch.ArtistsTrusted : RadioMatch.None, answer.Match);
        Assert.Equal(used ? 1 : 0, answer.Tracks.Count);
    }

    [Fact]
    public void Rows_TolerateMissingFields()
    {
        using var doc = JsonDocument.Parse("""{"tracks":[{"videoId":"a","title":"A"},{"title":"no id"},{"videoId":"b","title":"B","artists":["X",null],"durationSeconds":"soon"}]}""");
        var rows = YouTubeMusicClient.Rows(doc.RootElement);
        Assert.Equal(["a", "b"], rows.Select(row => row.VideoId));
        Assert.Equal(["X"], rows[1].Artists);
        Assert.Null(rows[1].DurationSeconds);
    }
}
