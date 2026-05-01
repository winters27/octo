using System.Text.Json;

namespace Octo.Services.YouTube;

/// <summary>
/// Pure HTTP client for the yt-dlp-shim sidecar service. Octo never spawns
/// yt-dlp itself — the shim wraps it behind /search and /stream endpoints
/// in its own container, so any process-management quirks stay isolated.
/// </summary>
public class YouTubeResolver
{
    // Named clients registered in Program.cs. The "stream" client uses an infinite
    // timeout because /stream from the shim is open for the duration of playback;
    // the default HttpClient timeout would kill the read mid-song.
    public const string SearchClientName = "yt-dlp-shim-search";
    public const string StreamClientName = "yt-dlp-shim-stream";

    private readonly IHttpClientFactory _httpClientFactory;
    private readonly ILogger<YouTubeResolver> _logger;
    private readonly string _baseUrl;

    public YouTubeResolver(IHttpClientFactory httpClientFactory, IConfiguration configuration, ILogger<YouTubeResolver> logger)
    {
        _httpClientFactory = httpClientFactory;
        _logger = logger;
        _baseUrl = (configuration.GetValue<string>("YouTube:ShimUrl") ?? "http://yt-dlp-shim:8080").TrimEnd('/');
    }

    /// <summary>
    /// Resolves "Artist - Title" to a single best YouTube hit via the shim.
    /// </summary>
    public async Task<YouTubeHit?> SearchAsync(string query, CancellationToken ct = default)
    {
        if (string.IsNullOrWhiteSpace(query)) return null;
        var url = $"{_baseUrl}/search?q={Uri.EscapeDataString(query)}";
        try
        {
            var http = _httpClientFactory.CreateClient(SearchClientName);
            using var resp = await http.GetAsync(url, ct);
            if (!resp.IsSuccessStatusCode)
            {
                _logger.LogDebug("shim /search HTTP {Code} for '{Q}'", (int)resp.StatusCode, query);
                return null;
            }
            var json = await resp.Content.ReadAsStringAsync(ct);
            using var doc = JsonDocument.Parse(json);
            var root = doc.RootElement;
            var videoId = root.TryGetProperty("video_id", out var v) ? v.GetString() : null;
            if (string.IsNullOrEmpty(videoId)) return null;
            return new YouTubeHit
            {
                VideoId = videoId,
                Title = root.TryGetProperty("title", out var t) ? t.GetString() : null,
                Duration = root.TryGetProperty("duration", out var d) && d.ValueKind == JsonValueKind.Number ? d.GetInt32() : null,
                Channel = root.TryGetProperty("channel", out var c) ? c.GetString() : null
            };
        }
        catch (Exception ex)
        {
            _logger.LogWarning("shim /search failed for '{Q}': {Msg}", query, ex.Message);
            return null;
        }
    }

    /// <summary>
    /// Opens a streaming connection to the shim's /stream endpoint. The shim
    /// proxies bytes from YouTube's CDN to us; we hand the resulting stream
    /// off to ASP.NET which forwards it to the Subsonic client.
    ///
    /// <paramref name="rangeHeader"/> is forwarded verbatim to the shim, which
    /// passes it on to googlevideo. iOS Subsonic clients (Arpeggi, Narjo) probe
    /// with `Range: bytes=0-1` and won't play audio/mp4 unless the server
    /// returns 206 with a valid Content-Range, so this passthrough is required
    /// for them to even attempt playback.
    ///
    /// Returns (stream, contentType, contentLength, statusCode, contentRange,
    /// owner) or null on failure. Caller owns the stream + response. The
    /// HttpClient itself is owned by IHttpClientFactory and must NOT be
    /// disposed — doing so tears down the handler/connection backing the
    /// returned stream and reads die mid-song.
    /// </summary>
    public async Task<(Stream stream, string contentType, long? contentLength, int statusCode, string? contentRange, HttpResponseMessage owner)?> OpenStreamAsync(string videoId, string? rangeHeader = null, CancellationToken ct = default)
    {
        if (string.IsNullOrWhiteSpace(videoId)) return null;
        var url = $"{_baseUrl}/stream?id={Uri.EscapeDataString(videoId)}";

        var http = _httpClientFactory.CreateClient(StreamClientName);

        HttpResponseMessage? resp = null;
        try
        {
            using var req = new HttpRequestMessage(HttpMethod.Get, url);
            if (!string.IsNullOrEmpty(rangeHeader))
            {
                req.Headers.TryAddWithoutValidation("Range", rangeHeader);
            }
            resp = await http.SendAsync(req, HttpCompletionOption.ResponseHeadersRead, ct);
            // Accept 200 (full body) and 206 (partial content). Anything else
            // means the upstream/shim couldn't satisfy the request.
            if ((int)resp.StatusCode != 200 && (int)resp.StatusCode != 206)
            {
                _logger.LogWarning("shim /stream HTTP {Code} for {Vid}", (int)resp.StatusCode, videoId);
                resp.Dispose();
                return null;
            }
            var stream = await resp.Content.ReadAsStreamAsync(ct);
            var contentType = resp.Content.Headers.ContentType?.ToString() ?? "audio/mp4";
            var contentLength = resp.Content.Headers.ContentLength;
            var contentRange = resp.Content.Headers.ContentRange?.ToString();
            var statusCode = (int)resp.StatusCode;
            var taken = resp; resp = null; // ownership transferred to caller via OwningStream wrapper
            return (stream, contentType, contentLength, statusCode, contentRange, taken);
        }
        catch (Exception ex)
        {
            _logger.LogWarning("shim /stream failed for {Vid}: {Msg}", videoId, ex.Message);
            resp?.Dispose();
            return null;
        }
    }
}

public class YouTubeHit
{
    public string VideoId { get; set; } = "";
    public string? Title { get; set; }
    public int? Duration { get; set; }
    public string? Channel { get; set; }
}
