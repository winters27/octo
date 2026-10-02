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
    [InlineData(true, false)]
    [InlineData(false, true)]
    public async Task SearchWaitsForYouTubeDurationsOnlyWhenConfigured(bool waitForDurations, bool answersBeforeDurations)
    {
        var handler = new Mock<HttpMessageHandler>();
        handler.Protected()
            .Setup<Task<HttpResponseMessage>>("SendAsync", ItExpr.IsAny<HttpRequestMessage>(), ItExpr.IsAny<CancellationToken>())
            .ReturnsAsync(() => new HttpResponseMessage(HttpStatusCode.OK)
            {
                Content = new StringContent("""{"results":{"trackmatches":{"track":[{"name":"Song","artist":"Artist"}]}}}"""),
            });
        var lastFm = new LastFmService(new HttpClient(handler.Object),
            TestOptions.Monitor(new LastFmSettings { ApiKey = "key" }),
            Options.Create(new MetadataSettings()),
            NullLogger<LastFmService>.Instance);

        var durations = new TaskCompletionSource();
        var metadata = new Mock<IMusicMetadataService>();
        metadata.Setup(m => m.SearchSongsByArtistTitleAsync("Artist", "Song", 1, null))
            .ReturnsAsync([new Song { Artist = "Artist", Title = "Song" }]);
        metadata.Setup(m => m.ResolveTopDurationsAsync(It.IsAny<List<Song>>(), It.IsAny<CancellationToken>()))
            .Returns(durations.Task);

        var search = new ExternalSearchService(metadata.Object, NullLogger<ExternalSearchService>.Instance, lastFm,
            TestOptions.Monitor(new SubsonicSettings { WaitForSearchDurations = waitForDurations }));

        var result = search.GetAsync("song");
        var answered = await Task.WhenAny(result, Task.Delay(TimeSpan.FromSeconds(2))) == result;
        durations.SetResult();

        Assert.Equal(answersBeforeDurations, answered);
        Assert.Single(await result);
    }
}
