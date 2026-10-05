using Microsoft.AspNetCore.Hosting;
using Microsoft.AspNetCore.Mvc.Testing;
using Microsoft.Extensions.Configuration;
using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.DependencyInjection.Extensions;
using Microsoft.Extensions.Hosting;
using Microsoft.Extensions.Logging.Abstractions;
using Microsoft.Extensions.Options;
using Octo.Models.Settings;
using Octo.Services.LastFm;
using Octo.Services.Soulseek;

namespace Octo.Tests;

/// <summary>
/// A one-star rating keeps a song off the listener's radio, for a library song and for one
/// Octo found online, and only for a caller Navidrome accepts.
/// </summary>
public class RadioBanEndpointTests
{
    private static string Rate(string id, int rating) =>
        $"/rest/setRating.view?u=alice&t=token&s=salt&v=1.16.1&c=test&f=json&id={id}&rating={rating}";

    [Fact]
    public async Task OneStarOnALibrarySong_KeepsItOffTheRadioAndAnotherRatingLiftsIt()
    {
        await using var factory = new BanWebFactory(pingOk: true);
        using var client = factory.CreateClient();
        var key = LastFmRadioSeedNormalizer.TrackKey("Library Artist", "Library Song");

        (await client.GetAsync(Rate("nd-1", 1))).EnsureSuccessStatusCode();
        Assert.Contains(key, factory.State.RadioBanKeys("alice"));

        (await client.GetAsync(Rate("nd-1", 3))).EnsureSuccessStatusCode();
        Assert.DoesNotContain(key, factory.State.RadioBanKeys("alice"));
    }

    [Fact]
    public async Task OneStarOnASongFoundOnline_CountsOnceNavidromeAcceptsTheCaller()
    {
        await using var factory = new BanWebFactory(pingOk: true);
        using var client = factory.CreateClient();
        var id = factory.Registry.Register(new SoulseekRouting
            { Kind = RoutingKind.Song, Artist = "Online Artist", Title = "Online Song" });

        (await client.GetAsync(Rate(id, 1))).EnsureSuccessStatusCode();

        Assert.Contains(LastFmRadioSeedNormalizer.TrackKey("Online Artist", "Online Song"),
            factory.State.RadioBanKeys("alice"));
    }

    [Fact]
    public async Task OneStarOnASongFoundOnline_FromACallerNavidromeRefuses_ChangesNothing()
    {
        await using var factory = new BanWebFactory(pingOk: false);
        using var client = factory.CreateClient();
        var id = factory.Registry.Register(new SoulseekRouting
            { Kind = RoutingKind.Song, Artist = "Online Artist", Title = "Online Song" });

        (await client.GetAsync(Rate(id, 1))).EnsureSuccessStatusCode();

        Assert.Empty(factory.State.RadioBanKeys("alice"));
    }

    /// <summary>Navidrome as the rating path sees it: a ping that passes or fails, one library
    /// song, and setRating answering ok.</summary>
    private sealed class FakeNavidrome(bool pingOk) : HttpMessageHandler
    {
        protected override Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken ct)
        {
            var path = request.RequestUri!.AbsolutePath;
            if (path.Contains("/rest/ping", StringComparison.Ordinal) && !pingOk)
                return Task.FromResult(ReviewFixtures.Json(
                    "{\"subsonic-response\":{\"status\":\"failed\",\"error\":{\"code\":40,\"message\":\"Wrong username or password\"}}}"));
            if (path.Contains("/rest/getSong", StringComparison.Ordinal))
                return Task.FromResult(ReviewFixtures.Json(
                    "{\"subsonic-response\":{\"status\":\"ok\",\"song\":{\"id\":\"nd-1\",\"artist\":\"Library Artist\",\"title\":\"Library Song\",\"duration\":200}}}"));
            return Task.FromResult(ReviewFixtures.Json("{\"subsonic-response\":{\"status\":\"ok\",\"version\":\"1.16.1\"}}"));
        }
    }

    private sealed class BanWebFactory(bool pingOk) : WebApplicationFactory<Program>
    {
        private readonly string _directory = Path.Combine(Path.GetTempPath(), "octo-radio-ban-web-" + Guid.NewGuid());

        public LastFmRadioStateStore State => Services.GetRequiredService<LastFmRadioStateStore>();
        public ExternalIdRegistry Registry => Services.GetRequiredService<ExternalIdRegistry>();

        protected override void ConfigureWebHost(IWebHostBuilder builder)
        {
            Directory.CreateDirectory(_directory);
            builder.ConfigureAppConfiguration((_, configuration) => configuration.AddInMemoryCollection(
                new Dictionary<string, string?>
                {
                    ["Subsonic:Url"] = "http://navidrome.test",
                    ["Subsonic:AutoDetectDownloadPath"] = "false",
                    ["Library:DownloadPath"] = _directory,
                }));
            builder.ConfigureServices(services =>
            {
                services.RemoveAll<IHostedService>();
                services.RemoveAll<IHttpClientFactory>();
                services.AddSingleton<IHttpClientFactory>(new ReviewFixtures.OneClientFactory(new FakeNavidrome(pingOk)));
                // The radio's state in this test's own folder, never the real config.
                services.RemoveAll<ExternalIdRegistry>();
                services.AddSingleton(new ExternalIdRegistry());
                services.RemoveAll<LastFmRadioStateStore>();
                services.AddSingleton(sp => new LastFmRadioStateStore(
                    Path.Combine(_directory, "lastfm-radio-state.json"),
                    sp.GetRequiredService<IOptionsMonitor<LastFmSettings>>(),
                    sp.GetRequiredService<ExternalIdRegistry>(),
                    NullLogger<LastFmRadioStateStore>.Instance));
            });
        }

        public override async ValueTask DisposeAsync()
        {
            await base.DisposeAsync();
            try { Directory.Delete(_directory, recursive: true); } catch { /* best effort */ }
        }
    }
}
