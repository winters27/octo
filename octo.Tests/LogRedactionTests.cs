using System.Collections.Concurrent;
using System.Net;
using System.Text;
using Microsoft.AspNetCore.Hosting;
using Microsoft.AspNetCore.Mvc.Testing;
using Microsoft.Extensions.Configuration;
using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.DependencyInjection.Extensions;
using Microsoft.Extensions.Hosting;
using Microsoft.Extensions.Logging;
using Octo.Services.Common;

namespace Octo.Tests;

/// <summary>
/// Subsonic credentials never reach the log. A client's t and s replay as that user, and p and
/// apiKey are worse, so each is sent on its own and the whole captured log is searched for it:
/// ASP.NET's request lines, HttpClient's lines and a line of Octo's own.
/// </summary>
public sealed class LogRedactionTests
{
    public static TheoryData<string> SecretNames => new() { "t", "s", "p", "apiKey", "token", "api_key", "client", "sk", "api_sig", "T", "APIKEY", "Token", "API_KEY", "Client", "user", "User" };

    /// <summary>Everything a sink would be handed: the message, every structured value and every
    /// scope, with the scope's own values too, since the JSON formatter writes all of them.</summary>
    private sealed record Entry(string Category, EventId EventId, string Message, IReadOnlyList<string> Written);

    private sealed class CapturingLoggerProvider : ILoggerProvider, ISupportExternalScope
    {
        private IExternalScopeProvider _scopes = new LoggerExternalScopeProvider();
        public ConcurrentQueue<Entry> Entries { get; } = new();

        public ILogger CreateLogger(string categoryName) => new CapturingLogger(this, categoryName);
        public void SetScopeProvider(IExternalScopeProvider scopeProvider) => _scopes = scopeProvider;
        public void Dispose() { }

        /// <summary>Every piece of text in the log, for searching.</summary>
        public string AllText() => string.Join("\n", Entries.SelectMany(e => e.Written.Prepend(e.Message)));

        private sealed class CapturingLogger(CapturingLoggerProvider owner, string category) : ILogger
        {
            public IDisposable? BeginScope<TState>(TState state) where TState : notnull => owner._scopes.Push(state);
            public bool IsEnabled(LogLevel logLevel) => true;

            public void Log<TState>(LogLevel logLevel, EventId eventId, TState state, Exception? exception,
                Func<TState, Exception?, string> formatter)
            {
                var written = new List<string>();
                Collect(state, written);
                owner._scopes.ForEachScope((scope, into) =>
                {
                    into.Add(scope?.ToString() ?? "");
                    Collect(scope, into);
                }, written);
                owner.Entries.Enqueue(new Entry(category, eventId, formatter(state, exception), written));
            }

            private static void Collect(object? state, List<string> into)
            {
                if (state is IEnumerable<KeyValuePair<string, object?>> pairs)
                    into.AddRange(pairs.Select(p => $"{p.Key}={p.Value}"));
            }
        }
    }

    /// <summary>Navidrome answering ok to everything, tokenInfo included (as winters).</summary>
    private sealed class PingOk : HttpMessageHandler
    {
        public ConcurrentQueue<string> Paths { get; } = new();

        protected override Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken ct)
        {
            Paths.Enqueue(request.RequestUri!.AbsolutePath);
            return Task.FromResult(new HttpResponseMessage(HttpStatusCode.OK)
            {
                Content = new StringContent(
                    """{"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome","tokenInfo":{"username":"winters"}}}""",
                    Encoding.UTF8, "application/json"),
            });
        }
    }

    /// <summary>Octo with the request log and HttpClient's lines turned up to Information, as a
    /// deployment that wants to see its traffic runs it; the shipped appsettings keep
    /// Microsoft.AspNetCore and System.Net.Http.HttpClient at Warning.</summary>
    private sealed class LoggingWebFactory(bool everything = false) : WebApplicationFactory<Program>
    {
        private readonly string _directory = Path.Combine(Path.GetTempPath(), "octo-log-web-" + Guid.NewGuid());
        public CapturingLoggerProvider Log { get; } = new();
        public PingOk Upstream { get; } = new();

        protected override void ConfigureWebHost(IWebHostBuilder builder)
        {
            Directory.CreateDirectory(_directory);
            var settings = new Dictionary<string, string?>
            {
                ["Logging:LogLevel:Microsoft.AspNetCore"] = "Information",
                ["Logging:LogLevel:System.Net.Http.HttpClient"] = "Information",
                ["Subsonic:Url"] = "http://navidrome.test",
                ["Subsonic:AutoDetectDownloadPath"] = "false",
                ["Soulseek:BaseUrl"] = "http://127.0.0.1:1",
                ["YouTube:ShimUrl"] = "http://127.0.0.1:1",
                ["Library:DownloadPath"] = _directory,
                ["Octo:StateDirectory"] = _directory,
            };
            if (everything)
            {
                // Every line Octo and HttpClient can write, for the tests that ask.
                settings["Logging:LogLevel:Default"] = "Trace";
                settings["Logging:LogLevel:Octo"] = "Trace";
                settings["Logging:LogLevel:System.Net.Http"] = "Trace";
                settings["Logging:LogLevel:System.Net.Http.HttpClient"] = "Trace";
            }
            builder.ConfigureAppConfiguration((_, configuration) => configuration.AddInMemoryCollection(settings));
            builder.ConfigureLogging(logging => logging.AddProvider(Log));
            builder.ConfigureServices(services =>
            {
                services.RemoveAll<IHostedService>();
                services.RemoveAll<IHttpClientFactory>();
                services.AddSingleton<IHttpClientFactory>(new ReviewFixtures.OneClientFactory(Upstream));
            });
        }

        public override async ValueTask DisposeAsync()
        {
            await base.DisposeAsync();
            try { Directory.Delete(_directory, recursive: true); } catch { /* best effort */ }
        }
    }

    private static string Secret() => "Secret" + Guid.NewGuid().ToString("N");

    private static async Task<IReadOnlyList<Entry>> RequestLines(CapturingLoggerProvider log)
    {
        // "Request finished" is written as the server tears the request down, which can be just
        // after the client already has its response.
        for (var i = 0; i < 100; i++)
        {
            var lines = log.Entries.Where(e => e.Category == "Microsoft.AspNetCore.Hosting.Diagnostics").ToList();
            if (lines.Any(e => e.Message.StartsWith("Request finished", StringComparison.Ordinal))) return lines;
            await Task.Delay(50);
        }
        return log.Entries.Where(e => e.Category == "Microsoft.AspNetCore.Hosting.Diagnostics").ToList();
    }

    [Theory]
    [MemberData(nameof(SecretNames))]
    public async Task RequestLines_MaskEachSecretParameter(string name)
    {
        await using var factory = new LoggingWebFactory();
        using var client = factory.CreateClient();
        var secret = Secret();

        var response = await client.GetAsync($"/rest/ping.view?u=winters&{name}={secret}&v=1.16.1&c=Octo&f=json");
        Assert.Equal(HttpStatusCode.OK, response.StatusCode);

        var lines = await RequestLines(factory.Log);
        var starting = Assert.Single(lines, e => e.Message.StartsWith("Request starting", StringComparison.Ordinal));
        var finished = Assert.Single(lines, e => e.Message.StartsWith("Request finished", StringComparison.Ordinal));
        foreach (var line in new[] { starting, finished })
        {
            Assert.Contains($"/rest/ping.view?u=winters&{name}=***&v=1.16.1&c=Octo&f=json", line.Message);
            Assert.Contains(line.Written, w => w.Contains($"{name}=***", StringComparison.Ordinal));
        }
        Assert.DoesNotContain(secret, factory.Log.AllText());
    }

    [Fact]
    public async Task RequestLines_MaskEverySecretAtOnceAndKeepTheRest()
    {
        await using var factory = new LoggingWebFactory();
        using var client = factory.CreateClient();
        var (t, s, p, apiKey, token) = (Secret(), Secret(), "enc:" + Secret(), Secret(), Secret());

        await client.GetAsync(
            $"/rest/star.view?u=winters&t={t}&s={s}&p={p}&apiKey={apiKey}&token={token}&v=1.16.1&c=Octo&f=json&id=42");

        var finished = Assert.Single(await RequestLines(factory.Log),
            e => e.Message.StartsWith("Request finished", StringComparison.Ordinal));
        Assert.Contains("/rest/star.view?u=winters&t=***&s=***&p=***&apiKey=***&token=***&v=1.16.1&c=Octo&f=json&id=42",
            finished.Message);
        var all = factory.Log.AllText();
        foreach (var secret in new[] { t, s, p, apiKey, token })
            Assert.DoesNotContain(secret, all);
    }

    [Theory]
    [MemberData(nameof(SecretNames))]
    public async Task HttpClientLines_CarryNoSecret(string name)
    {
        var log = new CapturingLoggerProvider();
        var services = new ServiceCollection();
        services.AddLogging(logging => logging.SetMinimumLevel(LogLevel.Trace).AddProvider(log).AddCredentialRedaction());
        services.AddHttpClient("navidrome").ConfigurePrimaryHttpMessageHandler(() => new PingOk());
        await using var provider = services.BuildServiceProvider();
        var secret = Secret();

        using var client = provider.GetRequiredService<IHttpClientFactory>().CreateClient("navidrome");
        await client.GetAsync($"http://navidrome:4533/rest/ping?u=winters&{name}={secret}&v=1.16.1&c=Octo");

        Assert.Contains(log.Entries, e => e.Category.StartsWith("System.Net.Http.HttpClient.navidrome", StringComparison.Ordinal)
            && e.Message.Contains("http://navidrome:4533/rest/ping", StringComparison.Ordinal));
        Assert.DoesNotContain(secret, log.AllText());
    }

    [Theory]
    [MemberData(nameof(SecretNames))]
    public async Task OctosOwnLines_MaskEachSecretParameter(string name)
    {
        await using var factory = new LoggingWebFactory();
        var logger = factory.Services.GetRequiredService<ILogger<LogRedactionTests>>();
        var secret = Secret();
        var url = $"http://navidrome:4533/rest/getSong?u=winters&{name}={secret}&id=7";

        // A URL as a structured value, as a Uri, and pasted straight into the template.
        logger.LogWarning("relay to {Url} failed", url);
        logger.LogWarning("relay to {Uri} failed", new Uri(url));
#pragma warning disable CA2254
        logger.LogWarning($"relay to {url} failed");
#pragma warning restore CA2254
        // The unmasked form of HttpClient's own line, as it reads with .NET's query masking off.
        factory.Services.GetRequiredService<ILoggerFactory>()
            .CreateLogger("System.Net.Http.HttpClient.Default.ClientHandler")
            .LogInformation("Sending HTTP request {HttpMethod} {Uri}", "GET", url);

        var own = factory.Log.Entries.Where(e => e.Message.Contains("rest/getSong", StringComparison.Ordinal)).ToList();
        Assert.Equal(4, own.Count);
        Assert.All(own, e => Assert.Contains($"u=winters&{name}=***&id=7", e.Message));
        Assert.DoesNotContain(secret, factory.Log.AllText());
    }

    /// <summary>An API key sign-in carries no username, so Octo asks Navidrome whose key it is.
    /// That call carries the key too, and neither it nor Octo's own lines about it may log it.</summary>
    [Fact]
    public async Task AnApiKeySignIn_IsNamed_WithoutTheKeyReachingTheLog()
    {
        await using var factory = new LoggingWebFactory(everything: true);
        using var client = factory.CreateClient();
        var key = Secret();

        await client.GetAsync($"/rest/scrobble.view?apiKey={key}&v=1.16.1&c=Octo&f=json&id=42&submission=true");
        await client.GetAsync(
            $"/rest/search3.view?query=anything&songCount=40&songOffset=40&apiKey={key}&v=1.16.1&c=Octo&f=json");
        await RequestLines(factory.Log);

        // Upstream is reached through a stand-in client factory, so HttpClient writes no lines of
        // its own here; HttpClientLines_CarryNoSecret covers those.
        Assert.Contains(factory.Upstream.Paths, path => path.EndsWith("/rest/tokenInfo", StringComparison.Ordinal));
        Assert.Contains(factory.Log.Entries, e => e.Message.Contains("apiKey=***", StringComparison.Ordinal));
        Assert.DoesNotContain(key, factory.Log.AllText());
    }

    [Fact]
    public async Task Scopes_AreMaskedToo()
    {
        var log = new CapturingLoggerProvider();
        var services = new ServiceCollection();
        services.AddLogging(logging => logging.AddProvider(log).AddCredentialRedaction());
        await using var provider = services.BuildServiceProvider();
        var logger = provider.GetRequiredService<ILogger<LogRedactionTests>>();
        var secret = Secret();

        using (logger.BeginScope("HTTP GET {Uri}", $"http://navidrome:4533/rest/ping?u=winters&t={secret}"))
            logger.LogInformation("inside");

        Assert.Contains(log.Entries.Single().Written, w => w.Contains("t=***", StringComparison.Ordinal));
        Assert.DoesNotContain(secret, log.AllText());
    }

    [Theory]
    [InlineData("/rest/ping?u=a&t=abc&s=def", "/rest/ping?u=a&t=***&s=***")]
    [InlineData("?p=enc:6162&u=a", "?p=***&u=a")]
    [InlineData("?apikey=K1&Token=K2#top", "?apikey=***&Token=***#top")]
    [InlineData("/2.0/?method=track.search&api_key=K1&format=json", "/2.0/?method=track.search&api_key=***&format=json")]
    [InlineData("v2/lookup?client=K1&meta=recordings", "v2/lookup?client=***&meta=recordings")]
    [InlineData("GET /rest/x?t=abc - 200", "GET /rest/x?t=*** - 200")]
    [InlineData("<a href='/rest/x?u=a&amp;t=abc'>", "<a href='/rest/x?u=a&amp;t=***'>")]
    // Names that only start or end like a secret, and empty values, are left alone.
    [InlineData("?ts=1&st=2&sort=3&apiKeyId=4&tokens=5&clientId=6&api_keys=7&c=Octo", "?ts=1&st=2&sort=3&apiKeyId=4&tokens=5&clientId=6&api_keys=7&c=Octo")]
    [InlineData("?t=&s=", "?t=&s=")]
    [InlineData("no query here, t=abc", "no query here, t=abc")]
    public void Redact_MasksOnlySecretValues(string text, string expected) =>
        Assert.Equal(expected, LogRedaction.Redact(text));

    [Fact]
    public void Redact_ReturnsTheSameStringWhenNothingIsMasked()
    {
        var text = "/rest/ping?u=winters&v=1.16.1";
        Assert.Same(text, LogRedaction.Redact(text));
    }
}
