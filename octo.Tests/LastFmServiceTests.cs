using Microsoft.Extensions.Logging;
using Microsoft.Extensions.Options;
using Moq;
using Octo.Models.Settings;
using Octo.Services.LastFm;

namespace Octo.Tests;

/// <summary>
/// "Can Last.fm answer at all" and "is the radio feature switched on" are different
/// questions. They used to be one property, so turning radio off also emptied the search
/// bar of discovery results — a setting doing something its name does not say.
/// </summary>
public class LastFmServiceTests
{
    private static LastFmService With(string apiKey, bool enableRadio, string language = "en") =>
        new(new HttpClient(),
            TestOptions.Monitor(new LastFmSettings { ApiKey = apiKey, EnableRadio = enableRadio }),
            Options.Create(new MetadataSettings { Language = language }),
            new Mock<ILogger<LastFmService>>().Object);

    [Theory]
    [InlineData("", true, false)]
    [InlineData("", false, false)]
    [InlineData("abc123", true, true)]
    [InlineData("abc123", false, true)]
    public void HasApiKey_DependsOnlyOnTheKey(string key, bool radio, bool expected)
    {
        // Search discovery gates on this, so EnableRadio must not appear in it.
        Assert.Equal(expected, With(key, radio).HasApiKey);
    }

    [Theory]
    [InlineData("abc123", true, true)]
    [InlineData("abc123", false, false)]
    [InlineData("", true, false)]
    public void IsRadioEnabled_NeedsBothTheKeyAndTheSwitch(string key, bool radio, bool expected)
    {
        Assert.Equal(expected, With(key, radio).IsRadioEnabled);
    }

    [Fact]
    public void RadioOffStillLeavesSearchDiscoveryAvailable()
    {
        // The regression this pair exists to prevent.
        var svc = With("abc123", enableRadio: false);

        Assert.True(svc.HasApiKey);
        Assert.False(svc.IsRadioEnabled);
    }

    [Fact]
    public void Construction_AppliesMetadataLanguageToTheClient()
    {
        var client = new HttpClient();
        _ = new LastFmService(client,
            TestOptions.Monitor(new LastFmSettings { ApiKey = "abc123" }),
            Options.Create(new MetadataSettings { Language = "en" }),
            new Mock<ILogger<LastFmService>>().Object);

        Assert.Contains(client.DefaultRequestHeaders.AcceptLanguage, v => v.Value == "en");
    }

    [Fact]
    public async Task RadioMethods_ParseProviderShapesAndCacheIdenticalLookups()
    {
        var handler = new FixtureHandler(request =>
        {
            var method = System.Web.HttpUtility.ParseQueryString(request.RequestUri!.Query)["method"];
            return method switch
            {
                "artist.getsimilar" => "{\"similarartists\":{\"artist\":[{\"name\":\"Muse\",\"match\":\"0.9\"}]}}",
                "artist.gettoptags" or "track.gettoptags" => "{\"toptags\":{\"tag\":[{\"name\":\"alternative rock\"}]}}",
                "tag.gettoptracks" => "{\"tracks\":{\"track\":[{\"name\":\"Song\",\"duration\":\"180000\",\"artist\":{\"name\":\"Artist\"}}]}}",
                "track.getInfo" => "{\"track\":{\"name\":\"Song\",\"duration\":\"180000\",\"artist\":{\"name\":\"Artist\"},\"album\":{\"title\":\"Album\"},\"toptags\":{\"tag\":[{\"name\":\"rock\"}]}}}",
                _ => "{}"
            };
        });
        var service = Service(handler);
        Assert.Equal("Muse", Assert.Single(await service.GetSimilarArtistsAsync("Radiohead")).Name);
        Assert.Equal("alternative rock", Assert.Single(await service.GetArtistTopTagsAsync("Radiohead")));
        Assert.Equal("alternative rock", Assert.Single(await service.GetTrackTopTagsAsync("A", "T")));
        var top = Assert.Single(await service.GetTagTopTracksAsync("rock"));
        Assert.Equal(180, top.Duration);
        var info = await service.GetTrackInfoAsync("Artist", "Song");
        Assert.Equal("Album", info!.Album);
        await service.GetSimilarArtistsAsync("Radiohead");
        Assert.Equal(5, handler.Count);
    }

    [Fact]
    public async Task RadioMethods_TolerateMalformedEmptyAndRateLimitedResponses()
    {
        var malformed = Service(new FixtureHandler(_ => "not json"));
        Assert.Empty(await malformed.GetSimilarArtistsAsync("A"));
        var limited = Service(new FixtureHandler(_ => "{}", System.Net.HttpStatusCode.TooManyRequests));
        Assert.Empty(await limited.GetTagTopTracksAsync("rock"));
    }

    [Fact]
    public async Task RadioMethods_PropagateCallerCancellation()
    {
        var service = Service(new FixtureHandler(async (_, ct) =>
        {
            await Task.Delay(TimeSpan.FromSeconds(5), ct); return "{}";
        }));
        using var cancellation = new CancellationTokenSource(TimeSpan.FromMilliseconds(20));
        await Assert.ThrowsAnyAsync<OperationCanceledException>(() =>
            service.GetSimilarArtistsAsync("A", cancellationToken: cancellation.Token));
    }

    /// <summary>Last.fm as it files a renamed artist: the catalogue under "Kanye West" only.
    /// Records every request's method and artist.</summary>
    private static FixtureHandler RenamedArtistLastFm(List<(string Method, string Artist)> asked, bool knowsSong = true) =>
        new(request =>
        {
            var query = System.Web.HttpUtility.ParseQueryString(request.RequestUri!.Query);
            var (method, artist) = (query["method"] ?? "", query["artist"] ?? "");
            lock (asked) asked.Add((method, artist));
            return method switch
            {
                "track.getsimilar" when knowsSong && artist == "Kanye West" =>
                    "{\"similartracks\":{\"track\":[{\"name\":\"Gold Digger\",\"match\":1,\"artist\":{\"name\":\"Kanye West\"}}]}}",
                "track.getsimilar" => "{\"similartracks\":{\"track\":[]}}",
                "artist.getsimilar" when artist == "Kanye West" =>
                    "{\"similarartists\":{\"artist\":[{\"name\":\"Jay-Z\"}]}}",
                "artist.getsimilar" => "{\"similarartists\":{\"artist\":[{\"name\":\"Jon Anderson\"}]}}",
                "artist.gettoptracks" =>
                    $"{{\"toptracks\":{{\"track\":[{{\"name\":\"Top of {artist}\"}}]}}}}",
                _ => "{}"
            };
        });

    [Fact]
    public async Task SimilarTracks_ARenamedArtistIsAlsoAskedUnderTheNameLastFmKnows()
    {
        // Last.fm has nothing under "Ye" for this song, and the similar tracks under "Kanye West".
        var asked = new List<(string Method, string Artist)>();
        var service = Service(RenamedArtistLastFm(asked));

        var tracks = await service.GetSimilarTracksAsync("Ye", "Crack Music");

        Assert.Equal("Gold Digger", Assert.Single(tracks).Title);
        Assert.Contains(("track.getsimilar", "Ye"), asked);
        Assert.Contains(("track.getsimilar", "Kanye West"), asked);
        Assert.DoesNotContain(asked, request => request.Method == "artist.getsimilar");
    }

    [Fact]
    public async Task SimilarTracks_TheSimilarArtistGuessRunsUnderTheNameLastFmKnows()
    {
        // Asked for artists like "Ye", Last.fm answers for someone else: a mix of Jon Anderson.
        var asked = new List<(string Method, string Artist)>();
        var service = Service(RenamedArtistLastFm(asked, knowsSong: false));

        var tracks = await service.GetSimilarTracksAsync("Ye", "Crack Music");

        Assert.Equal(["Jay-Z"], tracks.Select(track => track.Artist).Distinct());
        Assert.Contains(("artist.getsimilar", "Kanye West"), asked);
        Assert.DoesNotContain(("artist.getsimilar", "Ye"), asked);
    }

    [Fact]
    public async Task SimilarTracks_AnArtistWithOneNameIsAskedOnlyUnderIt()
    {
        var asked = new List<(string Method, string Artist)>();
        var service = Service(RenamedArtistLastFm(asked, knowsSong: false));

        await service.GetSimilarTracksAsync("Radiohead", "Creep");

        Assert.All(asked.Where(request => request.Method != "artist.gettoptracks"),
            request => Assert.Equal("Radiohead", request.Artist));
        Assert.Contains(("artist.getsimilar", "Radiohead"), asked);
    }

    [Fact]
    public async Task SimilarTracks_ASmallCachedAnswerDoesNotShortChangeABiggerAsk()
    {
        // A station refresh asks for a few; a player's radio then asks for many.
        var handler = new FixtureHandler(request =>
        {
            var query = System.Web.HttpUtility.ParseQueryString(request.RequestUri!.Query);
            var limit = int.Parse(query["limit"] ?? "50");
            var tracks = string.Join(",", Enumerable.Range(0, limit).Select(index =>
                $"{{\"name\":\"song-{index}\",\"match\":1,\"artist\":{{\"name\":\"Artist {index}\"}}}}"));
            return query["method"] == "track.getsimilar" ? $"{{\"similartracks\":{{\"track\":[{tracks}]}}}}" : "{}";
        });
        var service = Service(handler);

        Assert.Equal(5, (await service.GetSimilarTracksAsync("Radiohead", "Creep", 5)).Count);
        Assert.Equal(20, (await service.GetSimilarTracksAsync("Radiohead", "Creep", 20)).Count);
        Assert.Equal(2, handler.Count);

        // A smaller ask after that is served from the bigger answer.
        Assert.Equal(10, (await service.GetSimilarTracksAsync("Radiohead", "Creep", 10)).Count);
        Assert.Equal(2, handler.Count);
    }

    private static LastFmService Service(HttpMessageHandler handler) => new(new HttpClient(handler),
        TestOptions.Monitor(new LastFmSettings { ApiKey = "key", RadioCacheDurationHours = 2 }),
        Options.Create(new MetadataSettings { Language = "en" }),
        new Mock<ILogger<LastFmService>>().Object);

    private sealed class FixtureHandler : HttpMessageHandler
    {
        private readonly Func<HttpRequestMessage, CancellationToken, Task<string>> _fixture;
        private readonly System.Net.HttpStatusCode _status;
        public int Count { get; private set; }
        public FixtureHandler(Func<HttpRequestMessage, string> fixture,
            System.Net.HttpStatusCode status = System.Net.HttpStatusCode.OK)
            : this((request, _) => Task.FromResult(fixture(request)), status) { }
        public FixtureHandler(Func<HttpRequestMessage, CancellationToken, Task<string>> fixture,
            System.Net.HttpStatusCode status = System.Net.HttpStatusCode.OK)
        { _fixture = fixture; _status = status; }
        protected override async Task<HttpResponseMessage> SendAsync(HttpRequestMessage request,
            CancellationToken cancellationToken)
        {
            Count++;
            return new HttpResponseMessage(_status)
            { Content = new StringContent(await _fixture(request, cancellationToken)) };
        }
    }
}
