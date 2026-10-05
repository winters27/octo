using System.Net;
using System.Text;
using Microsoft.AspNetCore.Hosting;
using Microsoft.AspNetCore.Mvc.Testing;
using Microsoft.AspNetCore.WebUtilities;
using Microsoft.Extensions.Configuration;
using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.DependencyInjection.Extensions;
using Microsoft.Extensions.Hosting;
using Microsoft.Extensions.Primitives;
using Octo.Services.Subsonic;

namespace Octo.Tests;

/// <summary>
/// Subsonic sends a list as a repeated parameter (id=A&amp;id=B&amp;id=C). Octo's parameter
/// dictionary joins those into one "A,B,C", and the relay used to send that joined string on,
/// so Navidrome saw ONE id and refused every savePlayQueueByIndex. These go through the whole
/// app to a fake Navidrome and check what it received.
/// </summary>
public sealed class RelayedRepeatsTests
{
    /// <summary>One request as Navidrome saw it: its query, and its form body if any.</summary>
    private sealed record Seen(string Method, string Path, Dictionary<string, StringValues> Query,
        Dictionary<string, StringValues> Body)
    {
        public List<string> All(string key) =>
            [.. Query.GetValueOrDefault(key), .. Body.GetValueOrDefault(key)];
    }

    private sealed class FakeNavidrome : HttpMessageHandler
    {
        public readonly List<Seen> Requests = [];

        protected override async Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken ct)
        {
            var uri = request.RequestUri!;
            var body = request.Content is null ? "" : await request.Content.ReadAsStringAsync(ct);
            var isForm = request.Content?.Headers.ContentType?.MediaType == "application/x-www-form-urlencoded";
            lock (Requests)
                Requests.Add(new Seen(request.Method.Method, uri.AbsolutePath, QueryHelpers.ParseQuery(uri.Query),
                    isForm ? QueryHelpers.ParseQuery(body) : []));
            return new HttpResponseMessage(HttpStatusCode.OK)
            {
                Content = new StringContent("""{"subsonic-response":{"status":"ok","version":"1.16.1"}}""",
                    Encoding.UTF8, "application/json"),
            };
        }

        public Seen Only(string path) => Requests.Single(r => r.Path == path);
    }

    private sealed class WebFactory : WebApplicationFactory<Program>
    {
        private readonly string _directory = Path.Combine(Path.GetTempPath(), "octo-repeats-web-" + Guid.NewGuid());
        public FakeNavidrome Navidrome { get; } = new();

        protected override void ConfigureWebHost(IWebHostBuilder builder)
        {
            Directory.CreateDirectory(_directory);
            builder.ConfigureAppConfiguration((_, configuration) => configuration.AddInMemoryCollection(
                new Dictionary<string, string?>
                {
                    ["Subsonic:Url"] = "http://navidrome.test",
                    ["Subsonic:AutoDetectDownloadPath"] = "false",
                    ["Soulseek:BaseUrl"] = "http://127.0.0.1:1",
                    ["YouTube:ShimUrl"] = "http://127.0.0.1:1",
                    ["Library:DownloadPath"] = _directory,
                    ["Octo:StateDirectory"] = _directory,
                }));
            builder.ConfigureServices(services =>
            {
                services.RemoveAll<IHostedService>();
                services.RemoveAll<IHttpClientFactory>();
                services.AddSingleton<IHttpClientFactory>(new ReviewFixtures.OneClientFactory(Navidrome));
            });
        }

        public override async ValueTask DisposeAsync()
        {
            await base.DisposeAsync();
            try { Directory.Delete(_directory, recursive: true); } catch { /* best effort */ }
        }
    }

    private const string Auth = "u=alice&t=good&s=salt&v=1.16.1&c=test&f=json";
    private const string A = "3vXkQ9mTz2LbW8rYcN1pDf";
    private const string B = "7HqRs4uVw0XyZaBcDeFgHi";
    private const string C = "1JkLmNoPqRsTuVwXyZ0a2b";

    [Fact]
    public async Task SavePlayQueueByIndex_Get_ReachesNavidromeWithEveryId()
    {
        await using var factory = new WebFactory();
        using var client = factory.CreateClient();

        var response = await client.GetAsync(
            $"/rest/savePlayQueueByIndex?{Auth}&id={A}&id={B}&id={C}&currentIndex=2&position=61000");

        Assert.Equal(HttpStatusCode.OK, response.StatusCode);
        var seen = factory.Navidrome.Only("/rest/savePlayQueueByIndex");
        Assert.Equal([A, B, C], seen.All("id"));
        Assert.Equal(["2"], seen.All("currentIndex"));
        Assert.Equal(["61000"], seen.All("position"));
        Assert.Equal(["alice"], seen.All("u"));
    }

    [Fact]
    public async Task SavePlayQueueByIndex_FormPost_ReachesNavidromeWithEveryIdOnce()
    {
        await using var factory = new WebFactory();
        using var client = factory.CreateClient();

        // OpenSubsonic formPost: everything in the body, nothing in the query.
        var form = new FormUrlEncodedContent(
        [
            new("u", "alice"), new("t", "good"), new("s", "salt"), new("v", "1.16.1"), new("c", "test"),
            new("f", "json"), new("id", A), new("id", B), new("id", C), new("currentIndex", "2"),
            new("position", "61000"),
        ]);
        var response = await client.PostAsync("/rest/savePlayQueueByIndex", form);

        Assert.Equal(HttpStatusCode.OK, response.StatusCode);
        var seen = factory.Navidrome.Only("/rest/savePlayQueueByIndex");
        Assert.Equal("POST", seen.Method);
        // Navidrome reads the query and the body together, so each value must be in one of them only.
        Assert.Equal([A, B, C], seen.All("id"));
        Assert.Equal(["2"], seen.All("currentIndex"));
        Assert.Equal(["alice"], seen.All("u"));
    }

    [Fact]
    public async Task SavePlayQueueByIndex_FormPostWithAuthInTheQuery_KeepsBothHalves()
    {
        await using var factory = new WebFactory();
        using var client = factory.CreateClient();

        var form = new FormUrlEncodedContent(
            [new("id", A), new("id", B), new("id", C), new("currentIndex", "1")]);
        var response = await client.PostAsync($"/rest/savePlayQueueByIndex.view?{Auth}", form);

        Assert.Equal(HttpStatusCode.OK, response.StatusCode);
        var seen = factory.Navidrome.Only("/rest/savePlayQueueByIndex.view");
        Assert.Equal([A, B, C], seen.All("id"));
        Assert.Equal(["1"], seen.All("currentIndex"));
        Assert.Equal(["alice"], seen.All("u"));
        Assert.Equal(["salt"], seen.All("s"));
    }

    [Fact]
    public async Task UpdatePlaylist_Get_AddsEverySong()
    {
        await using var factory = new WebFactory();
        using var client = factory.CreateClient();

        var response = await client.GetAsync(
            $"/rest/updatePlaylist?{Auth}&playlistId=pl1&songIdToAdd={A}&songIdToAdd={B}&songIdToAdd={C}" +
            "&songIndexToRemove=0&songIndexToRemove=4");

        Assert.Equal(HttpStatusCode.OK, response.StatusCode);
        var seen = factory.Navidrome.Only("/rest/updatePlaylist");
        Assert.Equal([A, B, C], seen.All("songIdToAdd"));
        Assert.Equal(["0", "4"], seen.All("songIndexToRemove"));
        Assert.Equal(["pl1"], seen.All("playlistId"));
    }

    [Fact]
    public async Task UpdatePlaylist_FormPost_AddsEverySong()
    {
        await using var factory = new WebFactory();
        using var client = factory.CreateClient();

        var form = new FormUrlEncodedContent(
        [
            new("playlistId", "pl1"), new("songIdToAdd", A), new("songIdToAdd", B), new("songIdToAdd", C),
        ]);
        var response = await client.PostAsync($"/rest/updatePlaylist.view?{Auth}", form);

        Assert.Equal(HttpStatusCode.OK, response.StatusCode);
        var seen = factory.Navidrome.Only("/rest/updatePlaylist");
        Assert.Equal([A, B, C], seen.All("songIdToAdd"));
        Assert.Equal(["pl1"], seen.All("playlistId"));
    }

    [Fact]
    public void Restore_AValueAHandlerChangedGoesOutAsItIs()
    {
        var query = new Dictionary<string, StringValues>
        {
            ["id"] = new([A, B]),
            ["size"] = new("50"),
        };
        var parameters = new Dictionary<string, string> { ["id"] = "rewritten", ["size"] = "50", ["extra"] = "1" };

        var sent = SubsonicProxyService.RestoreRepeatedParameters(parameters, query, form: null, formInBody: false);

        Assert.Equal(
            [new("id", "rewritten"), new("size", "50"), new("extra", "1")],
            sent);
    }

    [Fact]
    public void Restore_ASingleValueWithACommaStaysOneValue()
    {
        var query = new Dictionary<string, StringValues> { ["query"] = new("daft punk, justice") };
        var parameters = new Dictionary<string, string> { ["query"] = "daft punk, justice" };

        var sent = SubsonicProxyService.RestoreRepeatedParameters(parameters, query, form: null, formInBody: false);

        Assert.Equal([new("query", "daft punk, justice")], sent);
    }

    [Fact]
    public void Restore_AChangedFormFieldStillReachesTheQueryWhenTheBodyIsForwarded()
    {
        var form = new Dictionary<string, StringValues> { ["id"] = new([A, B]), ["u"] = new("alice") };
        var parameters = new Dictionary<string, string> { ["id"] = $"{A},{B}", ["u"] = "octo" };

        var sent = SubsonicProxyService.RestoreRepeatedParameters(parameters, query: null, form, formInBody: true);

        // The unchanged ids ride in the body only; a changed value is never dropped.
        Assert.Equal([new("u", "octo")], sent);
    }
}
