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
