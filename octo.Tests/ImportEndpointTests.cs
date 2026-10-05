using System.Net;
using System.Net.Http.Json;
using System.Text;
using System.Text.Json;
using Microsoft.AspNetCore.Hosting;
using Microsoft.AspNetCore.Mvc.Testing;
using Microsoft.Extensions.Configuration;
using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.DependencyInjection.Extensions;
using Microsoft.Extensions.Hosting;
using Octo.Services.Admin;
using Octo.Services.Imports;

namespace Octo.Tests;

/// <summary>
/// The import page's endpoints: the dashboard's need its sign-in, the apps' check the caller with
/// Navidrome, both answer in the shapes the page and the apps read, and Spotify's return to Octo's
/// own address finishes a sign-in without one.
/// </summary>
public sealed class ImportEndpointTests
{
    private sealed class FakeNavidrome : HttpMessageHandler
    {
        protected override Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken ct)
        {
            var uri = request.RequestUri!;
            var query = System.Web.HttpUtility.ParseQueryString(uri.Query);
            if (uri.AbsolutePath.EndsWith("/rest/ping", StringComparison.Ordinal))
                return Task.FromResult(Json(query["t"] == "good"
                    ? """{"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome"}}"""
                    : """{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":40,"message":"Wrong username or password"}}}"""));
            if (uri.AbsolutePath.EndsWith("/rest/getOpenSubsonicExtensions", StringComparison.Ordinal))
                return Task.FromResult(Json("""{"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome","openSubsonic":true,"openSubsonicExtensions":[{"name":"formPost","versions":[1]}]}}"""));
            if (uri.AbsolutePath == "/auth/login")
                return Task.FromResult(Json("""{"token":"jwt","isAdmin":true,"username":"admin","subsonicToken":"t","subsonicSalt":"s"}"""));
            if (uri.AbsolutePath.StartsWith("/api/song", StringComparison.Ordinal))
                return Task.FromResult(Json("""[{"id":"nd-1","path":"/m/a.flac","suffix":"flac","bitRate":900,"title":"Teardrop","artist":"Massive Attack","duration":330.2}]"""));
            return Task.FromResult(new HttpResponseMessage(HttpStatusCode.NotFound));
        }

        private static HttpResponseMessage Json(string body) =>
            new(HttpStatusCode.OK) { Content = new StringContent(body, Encoding.UTF8, "application/json") };
    }

    private sealed class Factory(Dictionary<string, string?>? settings = null) : WebApplicationFactory<Program>
    {
        private readonly string _directory = Path.Combine(Path.GetTempPath(), "octo-import-web-" + Guid.NewGuid());

        protected override void ConfigureWebHost(IWebHostBuilder builder)
        {
            Directory.CreateDirectory(_directory);
            builder.ConfigureAppConfiguration((_, configuration) => configuration.AddInMemoryCollection(
                new Dictionary<string, string?>
                {
                    ["Subsonic:Url"] = "http://navidrome.test",
                    ["Subsonic:AutoDetectDownloadPath"] = "false",
                    ["Subsonic:AdminUsername"] = "admin",
                    ["Subsonic:AdminPassword"] = "synthetic",
                    ["Soulseek:BaseUrl"] = "http://127.0.0.1:1",
                    ["YouTube:ShimUrl"] = "http://127.0.0.1:1",
                    ["Library:DownloadPath"] = _directory,
                    ["Octo:StateDirectory"] = _directory,
                }).AddInMemoryCollection(settings ?? []));
            builder.ConfigureServices(services =>
            {
                services.RemoveAll<IHostedService>();
                services.RemoveAll<IHttpClientFactory>();
                services.AddSingleton<IHttpClientFactory>(new ReviewFixtures.OneClientFactory(new FakeNavidrome()));
                // In memory, so nothing here writes beside a real settings file.
                services.RemoveAll<BrowseSessionStore>();
                services.AddSingleton(new BrowseSessionStore());
                services.RemoveAll<ImportStore>();
                services.AddSingleton(new ImportStore());
                services.RemoveAll<TrickleQueue>();
                services.AddSingleton(new TrickleQueue());
                services.RemoveAll<SpotifyAccountStore>();
                services.AddSingleton(new SpotifyAccountStore());
            });
        }

        public string SignIn(string user = "alice") => Services.GetRequiredService<BrowseSessionStore>().Create(user);

        public HttpClient Client(string? token)
        {
            var client = CreateClient();
            client.DefaultRequestHeaders.Add("X-Octo-Admin", "1");
            if (token is not null) client.DefaultRequestHeaders.Add("X-Octo-Browse-Token", token);
            return client;
        }

        public override async ValueTask DisposeAsync()
        {
            await base.DisposeAsync();
            try { Directory.Delete(_directory, recursive: true); } catch { /* best effort */ }
        }
    }

    [Theory]
    [InlineData("GET", "/api/admin/imports")]
    [InlineData("GET", "/api/admin/imports/lists/spotify-liked")]
    [InlineData("POST", "/api/admin/imports/spotify/connect")]
    [InlineData("POST", "/api/admin/imports/spotify/disconnect")]
    [InlineData("POST", "/api/admin/imports/link")]
    [InlineData("POST", "/api/admin/imports/lists/x/fetch")]
    [InlineData("POST", "/api/admin/imports/trickle/pause")]
    [InlineData("DELETE", "/api/admin/imports/lists/x")]
    public async Task EveryDashboardEndpointNeedsTheSignIn(string method, string path)
    {
        await using var factory = new Factory();
        using var client = factory.Client(null);
        var request = new HttpRequestMessage(new HttpMethod(method), path);
        if (method != "GET" && method != "DELETE") request.Content = JsonContent.Create(new { on = true });
        using var response = await client.SendAsync(request);
        Assert.Equal(HttpStatusCode.Unauthorized, response.StatusCode);
    }

    [Fact]
    public async Task TheDashboardShowsTheSignedInPersonsLists_AndAddsAFile()
    {
        await using var factory = new Factory(new() { ["Imports:SpotifyClientId"] = "client-1" });
        using var client = factory.Client(factory.SignIn());

        var form = new MultipartFormDataContent
        {
            { new ByteArrayContent(Encoding.UTF8.GetBytes("Track Name,Artist Name(s)\nTeardrop,Massive Attack\nAngel,Massive Attack\n")), "file", "Gym.csv" },
        };
        using var upload = await client.PostAsync("/api/admin/imports/file", form);
        Assert.Equal(HttpStatusCode.OK, upload.StatusCode);
        var added = await upload.Content.ReadFromJsonAsync<JsonElement>();
        Assert.Equal("Added Gym, 2 songs.", added.GetProperty("message").GetString());
        var listId = added.GetProperty("listId").GetString();

        var page = await client.GetFromJsonAsync<JsonElement>("/api/admin/imports");
        Assert.Equal("alice", page.GetProperty("user").GetString());
        var overview = page.GetProperty("overview");
        Assert.True(overview.GetProperty("spotify").GetProperty("configured").GetBoolean());
        Assert.False(overview.GetProperty("spotify").GetProperty("connected").GetBoolean());
        Assert.Equal("http://127.0.0.1/callback", overview.GetProperty("spotify").GetProperty("redirectUri").GetString());
        var gym = overview.GetProperty("lists").EnumerateArray().Single();
        Assert.Equal(1, gym.GetProperty("have").GetInt32());
        Assert.Equal(1, gym.GetProperty("missing").GetInt32());
        Assert.Equal("idle", overview.GetProperty("trickle").GetProperty("state").GetString());

        using var fetch = await client.PostAsJsonAsync($"/api/admin/imports/lists/{listId}/fetch", new { on = true });
        Assert.Equal(HttpStatusCode.OK, fetch.StatusCode);
        var detail = await client.GetFromJsonAsync<JsonElement>($"/api/admin/imports/lists/{listId}");
        var states = detail.GetProperty("tracks").EnumerateArray().Select(t => t.GetProperty("state").GetString()).ToList();
        Assert.Equal(["have", "queued"], states);

        using var connect = await client.PostAsync("/api/admin/imports/spotify/connect", null);
        var url = (await connect.Content.ReadFromJsonAsync<JsonElement>()).GetProperty("url").GetString();
        Assert.StartsWith(SpotifyAuth.AuthorizeUrl, url);
    }

    [Fact]
    public async Task AWriteWithoutTheAdminHeaderIsRefused()
    {
        await using var factory = new Factory();
        var token = factory.SignIn();
        using var client = factory.CreateClient();
        client.DefaultRequestHeaders.Add("X-Octo-Browse-Token", token);
        using var response = await client.PostAsJsonAsync("/api/admin/imports/link", new { url = "https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M" });
        Assert.Equal(HttpStatusCode.Forbidden, response.StatusCode);
    }

    [Fact]
    public async Task SpotifysReturnToOctoWithAnUnknownStateSaysSo()
    {
        await using var factory = new Factory();
        using var client = factory.CreateClient();
        var html = await client.GetStringAsync("/imports/spotify/callback?code=abc&state=nope");
        Assert.Contains("Spotify sign-in did not finish", html);
        Assert.Contains("too old or was already used", html);
        var denied = await client.GetStringAsync("/imports/spotify/callback?error=access_denied&state=nope");
        Assert.Contains("access was not allowed", denied);
    }

    private const string Auth = "u=alice&t=good&s=salt&v=1.16.1&c=Octo&f=json";

    [Fact]
    public async Task TheAppsSeeTheSameLists_AsWhoeverTheySignedInAs()
    {
        await using var factory = new Factory();
        using var dashboard = factory.Client(factory.SignIn());
        var form = new MultipartFormDataContent
        {
            { new ByteArrayContent(Encoding.UTF8.GetBytes("Track Name,Artist Name(s)\nTeardrop,Massive Attack\n")), "file", "Gym.csv" },
        };
        (await dashboard.PostAsync("/api/admin/imports/file", form)).EnsureSuccessStatusCode();

        using var app = factory.CreateClient();
        using var doc = JsonDocument.Parse(await app.GetStringAsync($"/rest/getImports.view?{Auth}"));
        var envelope = doc.RootElement.GetProperty("subsonic-response");
        Assert.Equal("ok", envelope.GetProperty("status").GetString());
        var list = envelope.GetProperty("imports").GetProperty("lists").EnumerateArray().Single();
        Assert.Equal("Gym", list.GetProperty("name").GetString());

        using var one = JsonDocument.Parse(await app.GetStringAsync($"/rest/getImport?{Auth}&id={list.GetProperty("id").GetString()}"));
        Assert.Equal("have", one.RootElement.GetProperty("subsonic-response").GetProperty("import").GetProperty("tracks")[0].GetProperty("state").GetString());

        using var bob = JsonDocument.Parse(await app.GetStringAsync("/rest/getImports?u=bob&t=good&s=salt&v=1.16.1&c=Octo&f=json"));
        Assert.Empty(bob.RootElement.GetProperty("subsonic-response").GetProperty("imports").GetProperty("lists").EnumerateArray());
    }

    [Fact]
    public async Task AnAppActionAnswersInWords_AndAWrongPasswordIsRefused()
    {
        await using var factory = new Factory();
        using var app = factory.CreateClient();
        using var pause = JsonDocument.Parse(await app.GetStringAsync($"/rest/importAction?{Auth}&action=pause"));
        var answer = pause.RootElement.GetProperty("subsonic-response").GetProperty("importAction");
        Assert.True(answer.GetProperty("ok").GetBoolean());
        Assert.StartsWith("Paused", answer.GetProperty("message").GetString());

        using var connect = JsonDocument.Parse(await app.GetStringAsync($"/rest/importAction?{Auth}&action=connect&redirect=http://127.0.0.1:5555/callback"));
        Assert.Contains("Client ID", connect.RootElement.GetProperty("subsonic-response").GetProperty("importAction").GetProperty("message").GetString());

        using var wrong = JsonDocument.Parse(await app.GetStringAsync("/rest/getImports?u=alice&t=bad&s=salt&v=1.16.1&c=Octo&f=json"));
        Assert.Equal(40, wrong.RootElement.GetProperty("subsonic-response").GetProperty("error").GetProperty("code").GetInt32());
    }

    [Fact]
    public async Task TheAppsAreToldImportsAreHere()
    {
        await using var factory = new Factory();
        using var client = factory.CreateClient();
        using var doc = JsonDocument.Parse(await client.GetStringAsync("/rest/getOpenSubsonicExtensions.view?f=json&v=1.16.1&c=Octo"));
        var names = doc.RootElement.GetProperty("subsonic-response").GetProperty("openSubsonicExtensions").EnumerateArray()
            .Select(e => e.GetProperty("name").GetString()).ToList();
        Assert.Contains("octoImports", names);
    }
}
