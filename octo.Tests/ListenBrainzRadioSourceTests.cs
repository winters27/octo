using System.Collections.Concurrent;
using System.Net;
using System.Text;
using System.Text.Json;
using Microsoft.Extensions.Logging.Abstractions;
using Octo.Models.Domain;
using Octo.Models.Settings;
using Octo.Services.ListenBrainz;
using Octo.Services.Radio;

namespace Octo.Tests;

/// <summary>
/// ListenBrainz as a radio source, against answers captured from the live labs service
/// (Fixtures/listenbrainz, 2026-10-04): a recording found by name or by its tag, its similar
/// recordings scaled to 0 to 1, and LB Radio only with a token.
/// </summary>
public sealed class ListenBrainzRadioSourceTests
{
    public ListenBrainzRadioSourceTests() => ListenBrainzRadioClient.MinimumGap = TimeSpan.Zero;

    private static string Fixture(string name) =>
        File.ReadAllText(Path.Combine(AppContext.BaseDirectory, "Fixtures", "listenbrainz", name));

    private sealed class Labs(Func<HttpRequestMessage, (HttpStatusCode, string)> answer) : HttpMessageHandler
    {
        public ConcurrentQueue<HttpRequestMessage> Requests { get; } = new();

        protected override Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken ct)
        {
            Requests.Enqueue(request);
            var (status, body) = answer(request);
            return Task.FromResult(new HttpResponseMessage(status) { Content = new StringContent(body, Encoding.UTF8, "application/json") });
        }
    }

    private static (HttpStatusCode, string) Live(HttpRequestMessage request) => request.RequestUri!.AbsolutePath switch
    {
        "/acr-lookup/json" => (HttpStatusCode.OK, request.RequestUri.Query.Contains("Kordhell") ? Fixture("acr-lookup.json") : Fixture("acr-lookup-none.json")),
        "/similar-recordings/json" => (HttpStatusCode.OK, Fixture("similar-recordings.json")),
        "/1/explore/lb-radio" => (HttpStatusCode.OK, """{"payload":{"jspf":{"playlist":{"track":[{"title":"Close Eyes","creator":"DVRST","duration":132000},{"title":"RAVE","creator":"Dxrk"}]}}}}"""),
        _ => (HttpStatusCode.NotFound, "{}"),
    };

    private static (ListenBrainzRadioSource Source, Labs Labs) Source(string token = "", Func<HttpRequestMessage, (HttpStatusCode, string)>? answer = null)
    {
        var labs = new Labs(answer ?? Live);
        var client = new ListenBrainzRadioClient(new ReviewFixtures.OneClientFactory(labs),
            TestOptions.Monitor(new ListenBrainzSettings { Token = token }), NullLogger<ListenBrainzRadioClient>.Instance);
        return (new ListenBrainzRadioSource(client, TestOptions.Monitor(new RadioSourceSettings())), labs);
    }

    private static readonly IReadOnlyDictionary<string, string> NoAuth = new Dictionary<string, string>();

    [Fact]
    public async Task ASongFoundByName_IsTheSimilarRecordings_ScaledToItsBest()
    {
        var (source, labs) = Source();

        var answer = await source.SongsLikeAsync(new RadioSeed("Kordhell", "Murder In My Mind", 145, null), 50, NoAuth, default);

        Assert.Equal(RadioMatch.Song, answer.Match);
        Assert.True(answer.Tracks.Count > 20);
        Assert.Equal(("Dxrk ダーク", "RAVE", 1.0), (answer.Tracks[0].Artist, answer.Tracks[0].Title, answer.Tracks[0].Match));
        Assert.Equal(149.0 / 216, answer.Tracks[1].Match, 6);
        Assert.All(answer.Tracks, track => Assert.InRange(track.Match, 0, 1));
        Assert.All(answer.Tracks, track => Assert.Equal(RadioProvider.ListenBrainz, track.Provider));
        Assert.Contains(labs.Requests, request => request.RequestUri!.Query.Contains("recording_mbids=eb27c076-7686-465a-8737-8722611486c5"));
        Assert.Contains(labs.Requests, request => request.RequestUri!.Query.Contains("session_based_days_9000"));
    }

    [Fact]
    public async Task ASongWithItsRecordingTagged_NeedsNoLookup()
    {
        var (source, labs) = Source();
        var tagged = new Song { Id = "lib-1", MusicBrainzRecordingId = "eb27c076-7686-465a-8737-8722611486c5" };

        await source.SongsLikeAsync(new RadioSeed("Kordhell", "Murder In My Mind", 145, tagged), 50, NoAuth, default);

        Assert.DoesNotContain(labs.Requests, request => request.RequestUri!.AbsolutePath == "/acr-lookup/json");
    }

    [Fact]
    public async Task ASongListenBrainzDoesNotKnow_IsNothing()
    {
        var (source, labs) = Source();

        var answer = await source.SongsLikeAsync(new RadioSeed("Nobody Zzq", "Nothing Qqz", 100, null), 50, NoAuth, default);

        Assert.Equal(RadioMatch.None, answer.Match);
        Assert.DoesNotContain(labs.Requests, request => request.RequestUri!.AbsolutePath == "/similar-recordings/json");
    }

    [Fact]
    public async Task LbRadio_NeedsAToken_AndSendsIt()
    {
        var (without, quiet) = Source();
        Assert.Equal(RadioMatch.None, (await without.SongsLikeAsync(new RadioSeed("Kordhell", "", null, null), 20, NoAuth, default)).Match);
        Assert.Equal(RadioMatch.None, (await without.TagAsync("phonk", 20, default)).Match);
        Assert.Empty(quiet.Requests);

        var (with, labs) = Source(token: "lb-token");
        var artist = await with.SongsLikeAsync(new RadioSeed("Kordhell", "", null, null), 20, NoAuth, default);
        var tag = await with.TagAsync("phonk", 20, default);

        Assert.Equal(RadioMatch.ArtistsTrusted, artist.Match);
        Assert.Equal(RadioMatch.Genre, tag.Match);
        Assert.Equal(132, artist.Tracks[0].Duration);
        Assert.All(labs.Requests, request => Assert.Equal("Token lb-token", request.Headers.Authorization?.ToString()));
        Assert.Contains(labs.Requests, request => Uri.UnescapeDataString(request.RequestUri!.Query).Contains("prompt=artist:(Kordhell)"));
        Assert.Contains(labs.Requests, request => Uri.UnescapeDataString(request.RequestUri!.Query).Contains("prompt=tag:(phonk)"));
    }

    [Fact]
    public async Task SlowDown_IsNothing_NotAnError()
    {
        var (source, _) = Source(answer: _ => ((HttpStatusCode)429, "{}"));
        var answer = await source.SongsLikeAsync(new RadioSeed("Kordhell", "Murder In My Mind", 145, null), 50, NoAuth, default);
        Assert.Equal(RadioMatch.None, answer.Match);
    }

    [Fact]
    public void Rows_ReadBothShapesTheLabsDatasetsUse()
    {
        using var flat = JsonDocument.Parse("""[{"recording_name":"A"},{"recording_name":"B"}]""");
        using var blocks = JsonDocument.Parse("""[{"type":"markup","data":"<p/>"},{"type":"dataset","data":[{"recording_name":"C"}]}]""");
        Assert.Equal(2, ListenBrainzRadioClient.Rows(flat.RootElement).Count());
        Assert.Equal("C", ListenBrainzRadioClient.Rows(blocks.RootElement).Last().GetProperty("recording_name").GetString());
    }
}
