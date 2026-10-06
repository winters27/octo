using System.Collections.Concurrent;
using System.Net;
using System.Net.Sockets;
using System.Text;
using Microsoft.AspNetCore.Hosting;
using Microsoft.AspNetCore.Mvc.Testing;
using Microsoft.Extensions.Configuration;
using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.DependencyInjection.Extensions;
using Microsoft.Extensions.Hosting;
using Microsoft.Extensions.Logging;

namespace Octo.Tests;

/// <summary>
/// A live Navidrome transcode announces an estimated Content-Length (estimateContentLength=true,
/// which the Octo app sends on mobile data) and then sends a few percent less. Octo used to
/// promise the client that estimate, so the copy failed after the last real byte and the
/// connection was cut (Verified 2026-10-05 on LXC 111: 6,101,975 announced, 5,960,653 sent).
/// Navidrome here is a real socket, so HttpClient fails exactly as it does against the real one.
/// </summary>
public sealed class StreamLengthTests
{
    /// <summary>Navidrome as these requests see it: a 50,000 byte song. A transcode (any format
    /// but raw) is made live, so it says Accept-Ranges: none and, when asked for an estimate,
    /// announces 3% more than it sends. A raw request is served from the file, ranges and all.
    /// Every answer closes the connection, as Go does after a short body.</summary>
    private sealed class SocketNavidrome : IDisposable
    {
        public const int Size = 50_000;
        public const int Estimate = 51_500;
        public static readonly byte[] Song = Enumerable.Range(0, Size).Select(i => (byte)(i % 251)).ToArray();

        private readonly TcpListener _listener = new(IPAddress.Loopback, 0);
        private readonly CancellationTokenSource _stop = new();

        public SocketNavidrome()
        {
            _listener.Start();
            _ = AcceptAsync();
        }

        public string Url => $"http://127.0.0.1:{((IPEndPoint)_listener.LocalEndpoint).Port}";

        private async Task AcceptAsync()
        {
            while (!_stop.IsCancellationRequested)
            {
                TcpClient client;
                try { client = await _listener.AcceptTcpClientAsync(_stop.Token); }
                catch { return; }
                _ = ServeAsync(client);
            }
        }

        private static async Task ServeAsync(TcpClient client)
        {
            using (client)
            {
                var stream = client.GetStream();
                var head = await ReadHeadAsync(stream);
                var lines = head.Split("\r\n");
                var target = new Uri("http://navidrome" + lines[0].Split(' ')[1]);
                var query = System.Web.HttpUtility.ParseQueryString(target.Query);
                var range = lines.Skip(1).FirstOrDefault(l => l.StartsWith("Range:", StringComparison.OrdinalIgnoreCase))
                    ?[6..].Trim();

                var path = target.AbsolutePath;
                if (!path.EndsWith("/rest/stream", StringComparison.Ordinal)
                    && !path.EndsWith("/rest/download", StringComparison.Ordinal))
                {
                    await WriteAsync(stream, "404 Not Found", ["Content-Length: 0"], []);
                    return;
                }

                var format = query["format"];
                if (!string.IsNullOrEmpty(format) && format != "raw")
                {
                    var headers = new List<string> { "Content-Type: audio/mpeg", "Accept-Ranges: none" };
                    if (query["estimateContentLength"] == "true") headers.Add($"Content-Length: {Estimate}");
                    await WriteAsync(stream, "200 OK", headers, Song);
                    return;
                }

                if (range is not null && range.StartsWith("bytes=", StringComparison.Ordinal))
                {
                    var bounds = range[6..].Split('-');
                    var start = long.Parse(bounds[0]);
                    if (start >= Size)
                    {
                        await WriteAsync(stream, "416 Requested Range Not Satisfiable",
                            [$"Content-Range: bytes */{Size}", "Content-Length: 0"], []);
                        return;
                    }
                    var end = bounds[1].Length == 0 ? Size - 1 : Math.Min(long.Parse(bounds[1]), Size - 1);
                    var slice = Song[(int)start..(int)(end + 1)];
                    await WriteAsync(stream, "206 Partial Content",
                        ["Content-Type: audio/flac", "Accept-Ranges: bytes",
                         $"Content-Range: bytes {start}-{end}/{Size}", $"Content-Length: {slice.Length}"], slice);
                    return;
                }

                await WriteAsync(stream, "200 OK",
                    ["Content-Type: audio/flac", "Accept-Ranges: bytes", $"Content-Length: {Size}"], Song);
            }
        }

        private static async Task<string> ReadHeadAsync(NetworkStream stream)
        {
            var head = new List<byte>();
            var one = new byte[1];
            while (await stream.ReadAsync(one) == 1)
            {
                head.Add(one[0]);
                if (head.Count >= 4 && head[^4] == '\r' && head[^3] == '\n' && head[^2] == '\r' && head[^1] == '\n')
                    break;
            }
            return Encoding.ASCII.GetString(head.ToArray());
        }

        private static async Task WriteAsync(NetworkStream stream, string status, IEnumerable<string> headers, byte[] body)
        {
            var head = $"HTTP/1.1 {status}\r\n{string.Join("", headers.Select(h => h + "\r\n"))}Connection: close\r\n\r\n";
            await stream.WriteAsync(Encoding.ASCII.GetBytes(head));
            await stream.WriteAsync(body);
            await stream.FlushAsync();
        }

        public void Dispose()
        {
            _stop.Cancel();
            _listener.Stop();
        }
    }

    /// <summary>Every log line with an exception or at Error and above.</summary>
    private sealed class FailureLog : ILoggerProvider
    {
        public ConcurrentQueue<string> Failures { get; } = new();
        public ILogger CreateLogger(string categoryName) => new Logger(this, categoryName);
        public void Dispose() { }

        private sealed class Logger(FailureLog owner, string category) : ILogger
        {
            public IDisposable? BeginScope<TState>(TState state) where TState : notnull => null;
            public bool IsEnabled(LogLevel logLevel) => true;
            public void Log<TState>(LogLevel logLevel, EventId eventId, TState state, Exception? exception,
                Func<TState, Exception?, string> formatter)
            {
                if (exception is not null || logLevel >= LogLevel.Error)
                    owner.Failures.Enqueue($"{category}: {formatter(state, exception)} {exception}");
            }
        }
    }

    private sealed class Factory : WebApplicationFactory<Program>
    {
        private readonly string _directory = Path.Combine(Path.GetTempPath(), "octo-stream-length-" + Guid.NewGuid());
        private readonly SocketsHttpHandler _sockets = new();
        public SocketNavidrome Navidrome { get; } = new();
        public FailureLog Log { get; } = new();

        protected override void ConfigureWebHost(IWebHostBuilder builder)
        {
            Directory.CreateDirectory(_directory);
            builder.ConfigureAppConfiguration((_, configuration) => configuration.AddInMemoryCollection(
                new Dictionary<string, string?>
                {
                    ["Subsonic:Url"] = Navidrome.Url,
                    ["Subsonic:AutoDetectDownloadPath"] = "false",
                    ["Soulseek:BaseUrl"] = "http://127.0.0.1:1",
                    ["YouTube:ShimUrl"] = "http://127.0.0.1:1",
                    ["Library:DownloadPath"] = _directory,
                    ["Octo:StateDirectory"] = _directory,
                }));
            builder.ConfigureLogging(logging => logging.AddProvider(Log));
            builder.ConfigureServices(services =>
            {
                services.RemoveAll<IHostedService>();
                services.RemoveAll<IHttpClientFactory>();
                services.AddSingleton<IHttpClientFactory>(new ReviewFixtures.OneClientFactory(_sockets));
            });
        }

        public override async ValueTask DisposeAsync()
        {
            await base.DisposeAsync();
            Navidrome.Dispose();
            _sockets.Dispose();
            try { Directory.Delete(_directory, recursive: true); } catch { /* best effort */ }
        }
    }

    private const string Auth = "u=alice&t=token&s=salt&v=1.16.1&c=test";
    private const string Transcode = "format=mp3&maxBitRate=192&estimateContentLength=true";

    private static async Task<HttpResponseMessage> GetAsync(HttpClient client, string url, string? range = null)
    {
        using var request = new HttpRequestMessage(HttpMethod.Get, url);
        if (range is not null) request.Headers.TryAddWithoutValidation("Range", range);
        return await client.SendAsync(request, HttpCompletionOption.ResponseHeadersRead);
    }

    [Fact]
    public async Task TranscodeWithAnEstimatedLength_ArrivesWhole_AndEndsCleanly()
    {
        await using var factory = new Factory();
        using var client = factory.CreateClient();

        using var response = await GetAsync(client, $"/rest/stream?id=library-track&{Auth}&{Transcode}");
        // Read before the body: once it is buffered, HttpClient reports the buffer's length.
        var announced = response.Content.Headers.ContentLength;
        var body = await response.Content.ReadAsByteArrayAsync();

        Assert.Equal(HttpStatusCode.OK, response.StatusCode);
        // No promise Navidrome cannot keep: the client reads to the end of a chunked body.
        Assert.Null(announced);
        Assert.Equal(["none"], response.Headers.AcceptRanges);
        Assert.Equal("audio/mpeg", response.Content.Headers.ContentType?.MediaType);
        Assert.Equal(SocketNavidrome.Song, body);
        Assert.Empty(factory.Log.Failures);
    }

    [Fact]
    public async Task TranscodeWithAnEstimatedLength_AndARange_StillArrivesWhole()
    {
        // Navidrome cannot seek in a transcode it is still making, so it ignores the Range and
        // answers 200 with the whole song; Octo passes that on as it is.
        await using var factory = new Factory();
        using var client = factory.CreateClient();

        using var response = await GetAsync(client, $"/rest/stream?id=library-track&{Auth}&{Transcode}", "bytes=0-");
        var announced = response.Content.Headers.ContentLength;
        var body = await response.Content.ReadAsByteArrayAsync();

        Assert.Equal(HttpStatusCode.OK, response.StatusCode);
        Assert.Null(announced);
        Assert.Equal(SocketNavidrome.Song, body);
        Assert.Empty(factory.Log.Failures);
    }

    [Fact]
    public async Task DownloadOfATranscodeWithAnEstimatedLength_ArrivesWhole()
    {
        // rest/download goes through the general relay, which used to answer an error envelope
        // here ("Error while copying content to a stream", Verified 2026-10-05 on LXC 111).
        await using var factory = new Factory();
        using var client = factory.CreateClient();

        using var response = await GetAsync(client, $"/rest/download?id=library-track&{Auth}&{Transcode}");
        var body = await response.Content.ReadAsByteArrayAsync();

        Assert.Equal(HttpStatusCode.OK, response.StatusCode);
        Assert.Equal("audio/mpeg", response.Content.Headers.ContentType?.MediaType);
        Assert.Equal(SocketNavidrome.Song, body);
        Assert.Empty(factory.Log.Failures);
    }

    [Fact]
    public async Task RawStream_KeepsItsExactLength()
    {
        await using var factory = new Factory();
        using var client = factory.CreateClient();

        using var response = await GetAsync(client, $"/rest/stream?id=library-track&{Auth}&estimateContentLength=true");
        var announced = response.Content.Headers.ContentLength;
        var body = await response.Content.ReadAsByteArrayAsync();

        Assert.Equal(HttpStatusCode.OK, response.StatusCode);
        Assert.Equal(SocketNavidrome.Size, announced);
        Assert.Equal(["bytes"], response.Headers.AcceptRanges);
        Assert.Equal(SocketNavidrome.Song, body);
        Assert.Empty(factory.Log.Failures);
    }

    [Fact]
    public async Task RawStreamWithARange_Answers206WithTheExactRange()
    {
        // The iOS clients (Arpeggi, Narjo, Amperfy) probe with a Range and need a real 206.
        await using var factory = new Factory();
        using var client = factory.CreateClient();

        using var response = await GetAsync(client, $"/rest/stream?id=library-track&{Auth}", "bytes=100-199");
        var announced = response.Content.Headers.ContentLength;
        var body = await response.Content.ReadAsByteArrayAsync();

        Assert.Equal(HttpStatusCode.PartialContent, response.StatusCode);
        Assert.Equal(100, announced);
        Assert.Equal($"bytes 100-199/{SocketNavidrome.Size}", response.Content.Headers.ContentRange?.ToString());
        Assert.Equal(SocketNavidrome.Song[100..200], body);
        Assert.Empty(factory.Log.Failures);
    }

    [Fact]
    public async Task RangePastTheEnd_Answers416WithTheRealSize()
    {
        // What a player asks after believing a too-large estimate. The real size lets it end
        // the song (ExoPlayer treats a 416 at the stated size as the end).
        await using var factory = new Factory();
        using var client = factory.CreateClient();

        using var response = await GetAsync(client, $"/rest/stream?id=library-track&{Auth}",
            $"bytes={SocketNavidrome.Size}-");

        Assert.Equal(HttpStatusCode.RequestedRangeNotSatisfiable, response.StatusCode);
        Assert.Equal($"bytes */{SocketNavidrome.Size}", response.Content.Headers.ContentRange?.ToString());
    }
}
