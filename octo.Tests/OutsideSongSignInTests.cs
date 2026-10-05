using System.Net;
using System.Text;
using System.Text.Json;
using System.Xml.Linq;
using Microsoft.AspNetCore.Hosting;
using Microsoft.AspNetCore.Mvc.Testing;
using Microsoft.Extensions.Configuration;
using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.DependencyInjection.Extensions;
using Microsoft.Extensions.Hosting;
using Moq;
using Octo.Models.Settings;
using Octo.Services;
using Octo.Services.Common;
using Octo.Services.Local;
using Octo.Services.Soulseek;

namespace Octo.Tests;

/// <summary>
/// A star or a stream of a song from outside the library never reaches Navidrome, so Octo
/// checks the sign-in itself before it fetches or plays anything for it.
/// </summary>
public sealed class OutsideSongSignInTests
{
    /// <summary>Navidrome as these calls need it: a ping that accepts the token "good" or the
    /// API key "bob-key", tokenInfo for that key, and a library stream. Down makes every call
    /// fail as if Navidrome were off.</summary>
    private sealed class FakeNavidrome : HttpMessageHandler
    {
        public int Pings;
        public int Streams;
        public volatile bool Down;

        protected override Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken ct)
        {
            if (Down) throw new HttpRequestException("connection refused");
            var uri = request.RequestUri!;
            var query = System.Web.HttpUtility.ParseQueryString(uri.Query);
            if (uri.AbsolutePath.EndsWith("/rest/ping", StringComparison.Ordinal))
            {
                Interlocked.Increment(ref Pings);
                return Task.FromResult(Json(query["t"] == "good" || query["apiKey"] == "bob-key"
                    ? """{"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome"}}"""
                    : """{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":40,"message":"Wrong username or password"}}}"""));
            }
            if (uri.AbsolutePath.EndsWith("/rest/tokenInfo", StringComparison.Ordinal))
            {
                return Task.FromResult(Json(query["apiKey"] == "bob-key"
                    ? """{"subsonic-response":{"status":"ok","version":"1.16.1","tokenInfo":{"username":"bob"}}}"""
                    : """{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":44,"message":"Invalid API key"}}}"""));
            }
            if (uri.AbsolutePath.EndsWith("/rest/stream", StringComparison.Ordinal))
            {
                Interlocked.Increment(ref Streams);
                var response = new HttpResponseMessage(HttpStatusCode.OK) { Content = new ByteArrayContent([7, 8, 9]) };
                response.Content.Headers.ContentType = new("audio/flac");
                return Task.FromResult(response);
            }
            return Task.FromResult(new HttpResponseMessage(HttpStatusCode.NotFound));
        }

        private static HttpResponseMessage Json(string body) =>
            new(HttpStatusCode.OK) { Content = new StringContent(body, Encoding.UTF8, "application/json") };
    }

    private sealed class SignInWebFactory : WebApplicationFactory<Program>
    {
        private readonly string _directory = Path.Combine(Path.GetTempPath(), "octo-signin-web-" + Guid.NewGuid());
        public FakeNavidrome Navidrome { get; } = new();
        public Mock<IDownloadService> Downloads { get; } = new();
        public Mock<ILocalLibraryService> Library { get; } = new();

        public SignInWebFactory()
        {
            Downloads.Setup(service => service.GetDirectStreamAsync(
                    "soulseek", "track-id", It.IsAny<string?>(), It.IsAny<CancellationToken>()))
                .ReturnsAsync(() => new DirectStreamInfo
                {
                    AudioStream = new MemoryStream([1, 2, 3]),
                    ContentType = "audio/mp4",
                    ContentLength = 3,
                    StatusCode = 200,
                });
            Library.Setup(service => service.ParseSongId("external-track"))
                .Returns((true, "soulseek", "track-id"));
            Library.Setup(service => service.ParseSongId("library-track"))
                .Returns((false, null, null));
        }

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
                services.RemoveAll<IDownloadService>();
                services.RemoveAll<ILocalLibraryService>();
                services.AddSingleton(Downloads.Object);
                services.AddSingleton(Library.Object);
            });
        }

        public AcquisitionTracker Tracker => Services.GetRequiredService<AcquisitionTracker>();

        public override async ValueTask DisposeAsync()
        {
            await base.DisposeAsync();
            try { Directory.Delete(_directory, recursive: true); } catch { /* best effort */ }
        }
    }

    private static string Auth(string token) => $"u=alice&t={token}&s=salt&v=1.16.1&c=test";

    private static async Task<int> JsonErrorCode(HttpResponseMessage response)
    {
        using var doc = JsonDocument.Parse(await response.Content.ReadAsStringAsync());
        var envelope = doc.RootElement.GetProperty("subsonic-response");
        Assert.Equal("failed", envelope.GetProperty("status").GetString());
        return envelope.GetProperty("error").GetProperty("code").GetInt32();
    }

    [Fact]
    public async Task StarOnAnOutsideSong_WithAWrongPassword_StartsNothing()
    {
        await using var factory = new SignInWebFactory();
        using var client = factory.CreateClient();

        using var response = await client.GetAsync($"/rest/star.view?id=external-track&{Auth("bad")}&f=json");

        Assert.Equal(40, await JsonErrorCode(response));
        Assert.Empty(factory.Tracker.All());
    }

    [Fact]
    public async Task StarWithNoSignIn_IsRefusedWithoutAPing()
    {
        await using var factory = new SignInWebFactory();
        using var client = factory.CreateClient();

        using var response = await client.GetAsync("/rest/star.view?id=external-track&u=alice&v=1.16.1&c=test&f=json");

        Assert.Equal(40, await JsonErrorCode(response));
        Assert.Equal(0, factory.Navidrome.Pings);
        Assert.Empty(factory.Tracker.All());
    }

    [Fact]
    public async Task StarOnAnOutsideAlbum_WithAWrongPassword_StartsNothing()
    {
        await using var factory = new SignInWebFactory();
        using var client = factory.CreateClient();
        var albumId = factory.Services.GetRequiredService<ExternalIdRegistry>().Register(new SoulseekRouting
        {
            Kind = RoutingKind.Album, Artist = "Massive Attack", Album = "Mezzanine",
        });

        using var response = await client.GetAsync($"/rest/star.view?albumId={albumId}&{Auth("bad")}&f=json");

        Assert.Equal(40, await JsonErrorCode(response));
        Assert.Empty(factory.Tracker.All());
        await Task.Delay(100);
        factory.Downloads.Verify(service => service.DownloadAlbumWithSourceAsync(
            It.IsAny<string>(), It.IsAny<string>(), It.IsAny<DownloadSource>(), It.IsAny<bool>(),
            It.IsAny<CancellationToken>(), It.IsAny<IReadOnlyList<string>?>()), Times.Never);
    }

    [Fact]
    public async Task StreamOfAnOutsideSong_WithAWrongPassword_ServesNothing()
    {
        await using var factory = new SignInWebFactory();
        using var client = factory.CreateClient();

        // No f: the error comes back in XML, as the client asked.
        using var response = await client.GetAsync($"/rest/stream?id=external-track&{Auth("bad")}");

        var xml = XDocument.Parse(await response.Content.ReadAsStringAsync());
        XNamespace ns = "http://subsonic.org/restapi";
        Assert.Equal("failed", (string?)xml.Root!.Attribute("status"));
        Assert.Equal("40", (string?)xml.Root.Element(ns + "error")?.Attribute("code"));
        factory.Downloads.Verify(service => service.GetDirectStreamAsync(
            It.IsAny<string>(), It.IsAny<string>(), It.IsAny<string?>(), It.IsAny<CancellationToken>()), Times.Never);
    }

    [Fact]
    public async Task StreamOfAnOutsideSong_SameAddressTwice_PingsOnce()
    {
        await using var factory = new SignInWebFactory();
        using var client = factory.CreateClient();
        var url = $"/rest/stream?id=external-track&{Auth("good")}&f=json";

        using var first = await client.GetAsync(url);
        using var second = await client.GetAsync(url);

        Assert.Equal([1, 2, 3], await first.Content.ReadAsByteArrayAsync());
        Assert.Equal([1, 2, 3], await second.Content.ReadAsByteArrayAsync());
        Assert.Equal(1, factory.Navidrome.Pings);
    }

    [Fact]
    public async Task StreamWhileNavidromeIsDown_IsRefused_AndRecoversAtOnce()
    {
        await using var factory = new SignInWebFactory();
        using var client = factory.CreateClient();
        var url = $"/rest/stream?id=external-track&{Auth("good")}&f=json";

        factory.Navidrome.Down = true;
        using (var refused = await client.GetAsync(url))
            Assert.Equal(0, await JsonErrorCode(refused));
        factory.Downloads.Verify(service => service.GetDirectStreamAsync(
            It.IsAny<string>(), It.IsAny<string>(), It.IsAny<string?>(), It.IsAny<CancellationToken>()), Times.Never);

        factory.Navidrome.Down = false;
        using var served = await client.GetAsync(url);
        Assert.Equal([1, 2, 3], await served.Content.ReadAsByteArrayAsync());
    }

    [Fact]
    public async Task StreamOfALibrarySong_IsLeftToNavidrome()
    {
        await using var factory = new SignInWebFactory();
        using var client = factory.CreateClient();

        using var response = await client.GetAsync($"/rest/stream?id=library-track&{Auth("good")}");

        Assert.Equal([7, 8, 9], await response.Content.ReadAsByteArrayAsync());
        Assert.Equal(0, factory.Navidrome.Pings);
        Assert.Equal(1, factory.Navidrome.Streams);
    }

    [Fact]
    public async Task StarFromAnApiKey_IsFiledUnderTheKeysOwner()
    {
        await using var factory = new SignInWebFactory();
        using var client = factory.CreateClient();

        using var response = await client.GetAsync("/rest/star.view?id=external-track&apiKey=bob-key&v=1.16.1&c=test&f=json");

        response.EnsureSuccessStatusCode();
        Assert.Single(factory.Tracker.ForUser("bob"));
    }
}
