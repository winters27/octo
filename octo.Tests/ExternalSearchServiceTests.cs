using System.Net;
using Microsoft.Extensions.Logging.Abstractions;
using Microsoft.Extensions.Options;
using Moq;
using Moq.Protected;
using Octo.Models.Domain;
using Octo.Models.Settings;
using Octo.Services;
using Octo.Services.Common;
using Octo.Services.LastFm;

namespace Octo.Tests;

public class ExternalSearchServiceTests
{
    [Theory]
    [InlineData(50, 0)]
    [InlineData(5, 1)]
    public async Task PadsWithTopTracksOnlyWhenTrackSearchIsThin(int searchHits, int expectedTopTrackCalls)
    {
        var calls = new List<string>();
        var handler = new Mock<HttpMessageHandler>();
        handler.Protected()
            .Setup<Task<HttpResponseMessage>>("SendAsync", ItExpr.IsAny<HttpRequestMessage>(), ItExpr.IsAny<CancellationToken>())
            .ReturnsAsync((HttpRequestMessage request, CancellationToken _) =>
            {
                var url = request.RequestUri!.ToString();
                calls.Add(url);
                var tracks = string.Join(",", Enumerable.Range(1, searchHits)
                    .Select(i => $$"""{"name":"Song {{i}}","artist":"Artist"}"""));
                var body = url.Contains("method=track.search")
                    ? """{"results":{"trackmatches":{"track":[""" + tracks + "]}}}"
                    : """{"toptracks":{"track":[]}}""";
                return new HttpResponseMessage(HttpStatusCode.OK) { Content = new StringContent(body) };
            });
        var lastFm = new LastFmService(new HttpClient(handler.Object),
            TestOptions.Monitor(new LastFmSettings { ApiKey = "key" }),
            Options.Create(new MetadataSettings()),
            NullLogger<LastFmService>.Instance);
        var search = new ExternalSearchService(new Mock<IMusicMetadataService>().Object,
            NullLogger<ExternalSearchService>.Instance, lastFm);

        await search.GetAsync("artist");

        Assert.Equal(expectedTopTrackCalls, calls.Count(url => url.Contains("method=artist.gettoptracks")));
    }

    [Theory]
    [InlineData(true)]
    [InlineData(false)]
    public async Task SearchWaitsForYouTubeDurationsOnlyWhenConfigured(bool waitForDurations)
    {
        var durations = new TaskCompletionSource();
        var started = new TaskCompletionSource<bool>(TaskCreationOptions.RunContinuationsAsynchronously);
        var metadata = new Mock<IMusicMetadataService>();
        metadata.Setup(m => m.SearchSongsByArtistTitleAsync("Artist", "Song", 1, null))
            .ReturnsAsync([new Song { Artist = "Artist", Title = "Song" }]);
        metadata.Setup(m => m.ResolveTopDurationsAsync(It.IsAny<List<Song>>(), It.IsAny<CancellationToken>(), It.IsAny<bool>()))
            .Callback((List<Song> _, CancellationToken _, bool background) => started.TrySetResult(background))
            .Returns(durations.Task);
        var search = new ExternalSearchService(metadata.Object, NullLogger<ExternalSearchService>.Instance,
            OneHitLastFm(), TestOptions.Monitor(new SubsonicSettings { WaitForSearchDurations = waitForDurations }));

        var result = search.GetAsync("song");
        var background = await started.Task.WaitAsync(TimeSpan.FromSeconds(5));

        // The durations pass is still running, so only a search that does not wait can be done.
        Assert.Equal(!waitForDurations, background);
        if (waitForDurations) Assert.False(result.IsCompleted);
        else Assert.Single(await result.WaitAsync(TimeSpan.FromSeconds(5)));
        durations.SetResult();
        Assert.Single(await result.WaitAsync(TimeSpan.FromSeconds(5)));
    }

    [Fact]
    public async Task BackgroundDurationsRunBeforeTheVideoPrewarm()
    {
        var durations = new TaskCompletionSource();
        var started = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        var prewarmed = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        var metadata = new Mock<IMusicMetadataService>();
        metadata.Setup(m => m.SearchSongsByArtistTitleAsync("Artist", "Song", 1, null))
            .ReturnsAsync([new Song { Artist = "Artist", Title = "Song" }]);
        metadata.Setup(m => m.ResolveTopDurationsAsync(It.IsAny<List<Song>>(), It.IsAny<CancellationToken>(), true))
            .Callback(() => started.TrySetResult()).Returns(durations.Task);
        metadata.Setup(m => m.PrewarmYouTubeIdsAsync(It.IsAny<IEnumerable<Song>>(), 12, It.IsAny<CancellationToken>()))
            .Callback(() => prewarmed.TrySetResult()).Returns(Task.CompletedTask);
        var search = new ExternalSearchService(metadata.Object, NullLogger<ExternalSearchService>.Instance,
            OneHitLastFm(), TestOptions.Monitor(new SubsonicSettings { WaitForSearchDurations = false }));

        await search.GetAsync("song").WaitAsync(TimeSpan.FromSeconds(5));
        await started.Task.WaitAsync(TimeSpan.FromSeconds(5));
        Assert.False(prewarmed.Task.IsCompleted);

        durations.SetResult();
        await prewarmed.Task.WaitAsync(TimeSpan.FromSeconds(5));
    }

    [Fact]
    public async Task SearchRowsAreBuiltMostListenedFirst()
    {
        var handler = new Mock<HttpMessageHandler>();
        handler.Protected()
            .Setup<Task<HttpResponseMessage>>("SendAsync", ItExpr.IsAny<HttpRequestMessage>(), ItExpr.IsAny<CancellationToken>())
            .ReturnsAsync((HttpRequestMessage request, CancellationToken _) =>
            {
                var rows = new[]
                {
                    Row("Skyfall", "Adele", 1430000),
                    Row("Rolling in the Deep", "Adele", 2680000),
                    Row("Rolling in the Deep (Live)", "Adele", 9000),
                    Row("\u041f\u0440\u0438\u0432\u0435\u0442", "\u0410\u0434\u0435\u043b\u044c", 5000000),
                    Row("Someone Like You", "Adele", 2200000),
                };
                var body = request.RequestUri!.ToString().Contains("method=track.search")
                    ? """{"results":{"trackmatches":{"track":[""" + string.Join(",", rows) + "]}}}"
                    : """{"toptracks":{"track":[]}}""";
                return new HttpResponseMessage(HttpStatusCode.OK) { Content = new StringContent(body) };
            });
        var lastFm = new LastFmService(new HttpClient(handler.Object),
            TestOptions.Monitor(new LastFmSettings { ApiKey = "key" }),
            Options.Create(new MetadataSettings()),
            NullLogger<LastFmService>.Instance);
        var metadata = new Mock<IMusicMetadataService>();
        metadata.Setup(m => m.SearchSongsByArtistTitleAsync(It.IsAny<string>(), It.IsAny<string>(), 1, null))
            .ReturnsAsync((string artist, string title, int _, string? _) => [new Song { Artist = artist, Title = title }]);
        var search = new ExternalSearchService(metadata.Object, NullLogger<ExternalSearchService>.Instance, lastFm,
            TestOptions.Monitor(new SubsonicSettings { WaitForSearchDurations = false }));

        var songs = await search.GetAsync("adele");

        Assert.Equal(["Rolling in the Deep", "Someone Like You", "Skyfall"], songs.Select(s => s.Title));
    }

    // One track.search row; the \u escapes are JSON's, so the names stay readable here.
    private static string Row(string name, string artist, long listeners) =>
        $$"""{"name":"{{name}}","artist":"{{artist}}","listeners":"{{listeners}}"}""";

    private static LastFmService OneHitLastFm()
    {
        var handler = new Mock<HttpMessageHandler>();
        handler.Protected()
            .Setup<Task<HttpResponseMessage>>("SendAsync", ItExpr.IsAny<HttpRequestMessage>(), ItExpr.IsAny<CancellationToken>())
            .ReturnsAsync(() => new HttpResponseMessage(HttpStatusCode.OK)
            {
                Content = new StringContent("""{"results":{"trackmatches":{"track":[{"name":"Song","artist":"Artist"}]}}}"""),
            });
        return new LastFmService(new HttpClient(handler.Object),
            TestOptions.Monitor(new LastFmSettings { ApiKey = "key" }),
            Options.Create(new MetadataSettings()),
            NullLogger<LastFmService>.Instance);
    }
}
