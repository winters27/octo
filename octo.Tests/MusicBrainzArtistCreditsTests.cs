using System.Net;
using System.Text.Json;
using Microsoft.Extensions.Logging;
using Microsoft.Extensions.Options;
using Moq;
using Octo.Models.Settings;
using Octo.Services.LastFm;
using Octo.Services.MusicBrainz;

namespace Octo.Tests;

/// <summary>
/// A renamed artist is tagged with its new MusicBrainz name while Last.fm still files the
/// catalogue under the old one. "Ye - Crack Music" has no Last.fm similars; "Kanye West -
/// Crack Music" does. The instant mix used to come back empty for that reason alone.
/// </summary>
public class MusicBrainzArtistCreditsTests
{
    private const string CrackMusicMbid = "15316644-edf0-47c1-a438-3c0f25f6a1e5";
    private const string YeArtistId = "164f0d73-1234-4e2c-8743-d77bf2191051";

    private const string RecordingJson = $$$"""
        {"id":"{{{CrackMusicMbid}}}","title":"Crack Music",
         "artist-credit":[
           {"name":"Kanye West","joinphrase":" feat. ","artist":{"id":"{{{YeArtistId}}}","name":"Ye"}},
           {"name":"The Game","artist":{"id":"8b9d0c5b-1111-4e7c-9f3e-1d2c3b4a5f60","name":"The Game"}}]}
        """;

    private const string ArtistJson = """
        {"name":"Ye","aliases":[
          {"name":"Kanye West","type":"Artist name"},
          {"name":"Kanye Omari West","type":"Legal name"},
          {"name":"Yeezy","type":"Search hint"}]}
        """;

    [Fact]
    public void CollectNames_PutsThePrintedCreditFirstAndSkipsLegalNamesAndHints()
    {
        using var recording = JsonDocument.Parse(RecordingJson);
        using var artist = JsonDocument.Parse(ArtistJson);

        var names = MusicBrainzArtistCredits.CollectNames(recording.RootElement, artist.RootElement);

        Assert.Equal(["Kanye West", "Ye"], names);
    }

    [Fact]
    public void FirstCreditedArtistId_IsThePrimaryArtistNotAFeature()
    {
        using var recording = JsonDocument.Parse(RecordingJson);
        Assert.Equal(YeArtistId, MusicBrainzArtistCredits.FirstCreditedArtistId(recording.RootElement));
    }

    [Fact]
    public async Task GetAlternateArtistNames_ExcludesTheNameAlreadyTried()
    {
        var credits = Credits(MusicBrainzFixture(), out _);

        var names = await credits.GetAlternateArtistNamesAsync(CrackMusicMbid, "ye");

        Assert.Equal(["Kanye West"], names);
    }

    [Theory]
    [InlineData(null)]
    [InlineData("")]
    [InlineData("not-a-guid")]
    public async Task GetAlternateArtistNames_WithoutAUsableMbid_MakesNoRequest(string? mbid)
    {
        var credits = Credits(MusicBrainzFixture(), out var handler);

        Assert.Empty(await credits.GetAlternateArtistNamesAsync(mbid, "Ye"));
        Assert.Equal(0, handler.Count);
    }

    [Fact]
    public async Task GetAlternateArtistNames_UnknownRecordingIsEmptyNotAnError()
    {
        var credits = Credits(new Handler(_ => (HttpStatusCode.NotFound, "{}")), out _);
        Assert.Empty(await credits.GetAlternateArtistNamesAsync(CrackMusicMbid, "Ye"));
    }

    [Fact]
    public async Task GetAlternateArtistNames_CachesPerRecording()
    {
        var credits = Credits(MusicBrainzFixture(), out var handler);

        await credits.GetAlternateArtistNamesAsync(CrackMusicMbid, "Ye");
        await credits.GetAlternateArtistNamesAsync(CrackMusicMbid, "Ye");

        Assert.Equal(2, handler.Count); // recording + artist, once
    }

    // What Last.fm actually does for the tag name: no track similars, but a similar-artists
    // lookup that returns a confident, unrelated artist. Observed against production: the
    // mix for "Ye - Crack Music" came back as thirty Jon Anderson songs.
    private static string RenamedArtistLastFm(HttpRequestMessage request, bool creditHasTrackSimilars = true)
    {
        var q = System.Web.HttpUtility.ParseQueryString(request.RequestUri!.Query);
        return (q["method"], q["artist"]) switch
        {
            ("track.getsimilar", "Kanye West") when creditHasTrackSimilars =>
                """{"similartracks":{"track":[{"name":"Touch the Sky","match":0.9,"artist":{"name":"Kanye West"}}]}}""",
            ("artist.getsimilar", "Ye") => """{"similarartists":{"artist":[{"name":"Jon Anderson","match":"0.9"}]}}""",
            ("artist.gettoptracks", "Jon Anderson") => """{"toptracks":{"track":[{"name":"Ocean Song","artist":{"name":"Jon Anderson"}}]}}""",
            ("artist.getsimilar", "Kanye West") => """{"similarartists":{"artist":[{"name":"Lupe Fiasco","match":"0.8"}]}}""",
            ("artist.gettoptracks", "Lupe Fiasco") => """{"toptracks":{"track":[{"name":"Kick, Push","artist":{"name":"Lupe Fiasco"}}]}}""",
            _ => """{"similartracks":{"track":[]},"similarartists":{"artist":[]},"toptracks":{"track":[]}}"""
        };
    }

    [Fact]
    public async Task SimilarTracks_TheCreditedNameBeatsTheSimilarArtistsGuessForTheTagName()
    {
        var lastFm = LastFm(r => RenamedArtistLastFm(r));
        var credits = Credits(MusicBrainzFixture(), out _);

        var tracks = await lastFm.GetSimilarTracksWithCreditFallbackAsync("Ye", "Crack Music", CrackMusicMbid, credits, 10);

        Assert.Equal("Touch the Sky", Assert.Single(tracks).Title);
    }

    [Fact]
    public async Task SimilarTracks_WhenNoNameMatchesTheTrack_SimilarArtistsUseTheCreditedName()
    {
        var lastFm = LastFm(r => RenamedArtistLastFm(r, creditHasTrackSimilars: false));
        var credits = Credits(MusicBrainzFixture(), out _);

        var tracks = await lastFm.GetSimilarTracksWithCreditFallbackAsync("Ye", "Crack Music", CrackMusicMbid, credits, 10);

        Assert.Equal("Lupe Fiasco", Assert.Single(tracks).Artist);
    }

    [Fact]
    public async Task SimilarTracks_WithoutAnMbid_BehaveExactlyAsBefore()
    {
        var lastFm = LastFm(r => RenamedArtistLastFm(r));

        var tracks = await lastFm.GetSimilarTracksWithCreditFallbackAsync("Ye", "Crack Music", null, null, 10);

        Assert.Equal("Jon Anderson", Assert.Single(tracks).Artist);
    }

    [Fact]
    public async Task SimilarTracks_SecondCallGivesTheSameAnswerAsTheFirst()
    {
        // The exact-match result is cached; the fallback must not be skipped because of it.
        var lastFm = LastFm(r => RenamedArtistLastFm(r));
        var credits = Credits(MusicBrainzFixture(), out _);

        var first = await lastFm.GetSimilarTracksWithCreditFallbackAsync("Ye", "Crack Music", CrackMusicMbid, credits, 10);
        var second = await lastFm.GetSimilarTracksWithCreditFallbackAsync("Ye", "Crack Music", CrackMusicMbid, credits, 10);

        Assert.Equal(first.Select(t => t.Title), second.Select(t => t.Title));
    }

    [Fact]
    public async Task SimilarTracks_DoNotConsultMusicBrainzWhenLastFmAlreadyAnswered()
    {
        var lastFm = LastFm(_ =>
            """{"similartracks":{"track":[{"name":"Drip Too Hard","match":1,"artist":{"name":"Lil Baby"}}]}}""");
        var credits = Credits(MusicBrainzFixture(), out var mb);

        var tracks = await lastFm.GetSimilarTracksWithCreditFallbackAsync("Lil Baby", "Pure Cocaine", CrackMusicMbid, credits, 10);

        Assert.Single(tracks);
        Assert.Equal(0, mb.Count);
    }

    private static Handler MusicBrainzFixture() => new(request =>
        request.RequestUri!.AbsolutePath.Contains("/recording/")
            ? (HttpStatusCode.OK, RecordingJson)
            : (HttpStatusCode.OK, ArtistJson));

    private static MusicBrainzArtistCredits Credits(Handler handler, out Handler used)
    {
        used = handler;
        var factory = new Mock<IHttpClientFactory>();
        factory.Setup(f => f.CreateClient(MusicBrainzArtistCredits.ClientName)).Returns(() => new HttpClient(handler, false));
        return new MusicBrainzArtistCredits(factory.Object, new Mock<ILogger<MusicBrainzArtistCredits>>().Object)
        {
            MinRequestInterval = TimeSpan.Zero
        };
    }

    private static LastFmService LastFm(Func<HttpRequestMessage, string> fixture) =>
        new(new HttpClient(new Handler(r => (HttpStatusCode.OK, fixture(r)))),
            TestOptions.Monitor(new LastFmSettings { ApiKey = "key" }),
            Options.Create(new MetadataSettings { Language = "en" }),
            new Mock<ILogger<LastFmService>>().Object);

    private sealed class Handler(Func<HttpRequestMessage, (HttpStatusCode Status, string Body)> fixture) : HttpMessageHandler
    {
        public int Count { get; private set; }

        protected override Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken cancellationToken)
        {
            Count++;
            var (status, body) = fixture(request);
            return Task.FromResult(new HttpResponseMessage(status) { Content = new StringContent(body) });
        }
    }
}
