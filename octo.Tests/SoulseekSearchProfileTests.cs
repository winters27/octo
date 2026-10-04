using System.Net;
using System.Text;
using System.Text.Json;
using Microsoft.Extensions.Logging.Abstractions;
using Microsoft.Extensions.Options;
using Moq;
using Octo.Models.Settings;
using Octo.Services.Soulseek;

namespace Octo.Tests;

/// <summary>
/// slskd hands over a search's answers only when the search ends (#70). These pin how Octo waits
/// for that, cancels a search still running at its ceiling so the answers are kept, and never
/// leaves a search running in slskd behind it.
/// </summary>
public class SoulseekSearchProfileTests
{
    private static readonly DateTime Start = new(2026, 10, 2, 0, 0, 0, DateTimeKind.Utc);
    private static readonly SearchProfile Interactive = SearchProfile.Interactive(new SoulseekSettings());

    [Fact]
    public void ThePayloadCarriesEachProfilesLimits()
    {
        static (int, int, int, bool) Limits(SearchProfile p)
        {
            using var doc = JsonDocument.Parse(SoulseekClient.SearchPayload(Guid.Empty.ToString(), "Artist Song", p));
            var r = doc.RootElement;
            return (r.GetProperty("searchTimeout").GetInt32(), r.GetProperty("responseLimit").GetInt32(),
                r.GetProperty("fileLimit").GetInt32(), r.GetProperty("filterResponses").GetBoolean());
        }
        var s = new SoulseekSettings();
        Assert.Equal((15_000, 500, 2_000, true), Limits(SearchProfile.Interactive(s)));
        Assert.Equal((30_000, 500, 2_000, true), Limits(SearchProfile.Upgrade(s)));
        Assert.Equal((30, 90), (SearchProfile.Interactive(s).CeilingSeconds, SearchProfile.Upgrade(s).CeilingSeconds));
        Assert.Equal(300, SearchProfile.Upgrade(new SoulseekSettings { UpgradeSearchWaitSeconds = 9999 }).CeilingSeconds);
    }

    [Fact]
    public async Task SearchAsync_PostsTheProfileItWasGiven()
    {
        var slskd = new FakeSearchSlskd(endsAfter: 2);
        await Client(slskd).SearchAsync("Artist Song", SearchProfile.Upgrade(new SoulseekSettings()));
        using var posted = JsonDocument.Parse(Assert.Single(slskd.Posted));
        Assert.Equal((2_000, 30_000), (posted.RootElement.GetProperty("fileLimit").GetInt32(),
            posted.RootElement.GetProperty("searchTimeout").GetInt32()));
    }

    [Fact]
    public async Task ASearchThatEndsOnItsOwnReturnsItsResponses()
    {
        // The fourth look already says Completed with nothing saved yet, as slskd really does;
        // the fifth has endedAt and the answers.
        var slskd = new FakeSearchSlskd(endsAfter: 4);
        var client = Client(slskd);
        var hits = await client.SearchAsync("Artist Song", Interactive);
        await client.LastSearchCleanup;
        Assert.Equal("flac", Assert.Single(hits).Extension);
        Assert.True(slskd.Now - Start < TimeSpan.FromSeconds(30), "waited out the ceiling, so it proves nothing");
        Assert.DoesNotContain("PUT", slskd.Calls);
        Assert.Equal("DELETE", slskd.Calls[^1]);
    }

    [Fact]
    public async Task ASearchStillRunningAtTheCeilingIsCancelledAndItsResponsesKept()
    {
        var slskd = new FakeSearchSlskd(endsAfter: null);
        var client = Client(slskd);
        var hits = await client.SearchAsync("Artist Song", Interactive);
        await client.LastSearchCleanup;
        // Octo used to return nothing here while the answers sat in slskd.
        Assert.Single(hits);
        Assert.True(slskd.Now - Start >= TimeSpan.FromSeconds(30));
        Assert.True(slskd.Calls.IndexOf("PUT") < slskd.Calls.IndexOf("GET responses"));
        Assert.Equal(new[] { "PUT", "DELETE" }, slskd.Calls.Where(c => c is "PUT" or "DELETE"));
    }

    [Fact]
    public async Task ASearchSlskdWillNotStopIsCancelledAgainBeforeItIsDeleted()
    {
        var slskd = new FakeSearchSlskd(endsAfter: null, ignoresCancel: true);
        var client = Client(slskd);
        Assert.Empty(await client.SearchAsync("Artist Song", Interactive));
        await client.LastSearchCleanup;
        Assert.Equal(new[] { "PUT", "PUT", "DELETE" }, slskd.Calls.Where(c => c is "PUT" or "DELETE"));
    }

    [Fact]
    public async Task ACallerWhoGivesUpCancelsTheSlskdSearch()
    {
        using var cts = new CancellationTokenSource();
        var slskd = new FakeSearchSlskd(endsAfter: null, onStatusRead: n => { if (n == 3) cts.Cancel(); });
        var client = Client(slskd);
        await Assert.ThrowsAnyAsync<OperationCanceledException>(() => client.SearchAsync("Artist Song", Interactive, cts.Token));
        await client.LastSearchCleanup;
        Assert.Equal(new[] { "PUT", "DELETE" }, slskd.Calls.Where(c => c is "PUT" or "DELETE"));
        Assert.DoesNotContain("GET responses", slskd.Calls);
    }

    [Fact]
    public async Task ACallerWhoGivesUpWhileTheStartIsOnItsWayStillCancelsTheSlskdSearch()
    {
        using var cts = new CancellationTokenSource();
        var slskd = new FakeSearchSlskd(endsAfter: null, onStartSent: cts.Cancel);
        var client = Client(slskd);
        await Assert.ThrowsAnyAsync<OperationCanceledException>(() => client.SearchAsync("Artist Song", Interactive, cts.Token));
        await client.LastSearchCleanup;
        Assert.Single(slskd.Posted);
        Assert.Equal(new[] { "PUT", "DELETE" }, slskd.Calls.Where(c => c is "PUT" or "DELETE"));
    }

    [Fact]
    public async Task AStartRefusedWithTooManyRequestsIsRetriedOnce()
    {
        var slskd = new FakeSearchSlskd(endsAfter: 2, refuseFirstStart: true);
        var hits = await Client(slskd).SearchAsync("Artist Song", Interactive);
        Assert.Equal(2, slskd.Posted.Count);
        Assert.Single(hits);
    }

    private static SoulseekClient Client(FakeSearchSlskd slskd)
    {
        var factory = new Mock<IHttpClientFactory>();
        factory.Setup(f => f.CreateClient(It.IsAny<string>())).Returns(() => new HttpClient(slskd));
        var settings = Options.Create(new SoulseekSettings { BaseUrl = "http://slskd.test:5030", Username = "octo", Password = "secret" });
        return new SoulseekClient(factory.Object, settings, NullLogger<SoulseekClient>.Instance)
        {
            SearchPollInterval = TimeSpan.FromMilliseconds(1),
            SearchStartRetryDelay = TimeSpan.FromMilliseconds(1),
            Clock = () => slskd.Now,
        };
    }

    // slskd's search API as 0.26.0 behaves: answers are saved only when a search ends, the record
    // says Completed one look before they are, PUT cancels, DELETE only removes the record. The
    // clock moves half a second with every look at the state.
    private sealed class FakeSearchSlskd(int? endsAfter, bool ignoresCancel = false, bool refuseFirstStart = false,
        Action<int>? onStatusRead = null, Action? onStartSent = null) : HttpMessageHandler
    {
        private const string Answers = """
            [{"username":"peer","uploadSpeed":1000000,"queueLength":0,"files":[
              {"filename":"Music\\Artist\\01 - Song.flac","size":30000000,"extension":"flac","length":200}]}]
            """;
        private readonly object _lock = new();
        private int _reads;
        private bool _cancelAsked, _saved;
        private string _state = "InProgress";

        public DateTime Now { get; private set; } = Start;
        public List<string> Posted { get; } = [];
        public List<string> Calls { get; } = [];

        protected override async Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken ct)
        {
            var path = request.RequestUri!.AbsolutePath;
            var body = request.Content is null ? null : await request.Content.ReadAsStringAsync(ct);
            if (onStartSent is not null && request.Method == HttpMethod.Post && path == "/api/v0/searches")
            {
                // slskd takes the search, and the answer is held until the caller gives up.
                lock (_lock) Posted.Add(body!);
                onStartSent();
                await Task.Delay(Timeout.Infinite, ct);
            }
            lock (_lock)
            {
                if (path == "/api/v0/session")
                    return Json($$"""{"token":"jwt","expires":{{DateTimeOffset.UtcNow.AddHours(1).ToUnixTimeSeconds()}}}""");
                if (request.Method == HttpMethod.Post)
                {
                    Posted.Add(body!);
                    return refuseFirstStart && Posted.Count == 1 ? new HttpResponseMessage(HttpStatusCode.TooManyRequests) : Json("{}");
                }
                if (request.Method == HttpMethod.Put) { Calls.Add("PUT"); _cancelAsked = true; return new HttpResponseMessage(HttpStatusCode.OK); }
                if (request.Method == HttpMethod.Delete) { Calls.Add("DELETE"); return new HttpResponseMessage(HttpStatusCode.NoContent); }
                if (path.EndsWith("/responses")) { Calls.Add("GET responses"); return Json(_saved ? Answers : "[]"); }

                Now += TimeSpan.FromMilliseconds(500);
                // Counted outside the ?. call: a null callback would skip the increment too.
                _reads++;
                onStatusRead?.Invoke(_reads);
                if (_state.StartsWith("Completed")) _saved = true;
                else if (_reads >= endsAfter || (_cancelAsked && !ignoresCancel))
                    _state = _cancelAsked ? "Completed, Cancelled" : "Completed, TimedOut";
                var endedAt = _saved ? $"\"{Now:O}\"" : "null";
                return Json($$"""{"state":"{{_state}}","responseCount":1,"fileCount":1,"endedAt":{{endedAt}}}""");
            }
        }

        private static HttpResponseMessage Json(string body) =>
            new(HttpStatusCode.OK) { Content = new StringContent(body, Encoding.UTF8, "application/json") };
    }
}
