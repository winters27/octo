using Microsoft.Extensions.Options;
using Microsoft.AspNetCore.Mvc;
using Octo.Models.Settings;

namespace Octo.Services.Subsonic;

/// <summary>
/// Handles proxying requests to the underlying Subsonic server.
/// </summary>
public class SubsonicProxyService
{
    private readonly HttpClient _httpClient;
    // IOptionsMonitor, not IOptions: the admin UI writes settings.json and the
    // config provider reloads it, but IOptions.Value is resolved once and this is a
    // singleton, so a captured copy would serve startup values until a restart. The
    // admin UI read through IOptionsMonitor and therefore SHOWED the new value while
    // nothing acted on it.
    private readonly IOptionsMonitor<SubsonicSettings> subsonicSettingsOptions;
    private SubsonicSettings _subsonicSettings => subsonicSettingsOptions.CurrentValue;
    private readonly IHttpContextAccessor _httpContextAccessor;

    public SubsonicProxyService(
        IHttpClientFactory httpClientFactory,
        IOptionsMonitor<SubsonicSettings> subsonicSettings,
        IHttpContextAccessor httpContextAccessor)
    {
        _httpClient = httpClientFactory.CreateClient();
        subsonicSettingsOptions = subsonicSettings;
        _httpContextAccessor = httpContextAccessor;
    }

    /// <summary>
    /// Relays a request to the Subsonic server and returns the response.
    /// </summary>
    public async Task<(byte[] Body, string? ContentType)> RelayAsync(
        string endpoint, 
        Dictionary<string, string> parameters)
        => await RelayAsync(endpoint, parameters.AsEnumerable());

    /// <summary>Relay overload for batch endpoints whose repeated keys cannot be
    /// represented by the legacy request dictionary.</summary>
    public async Task<(byte[] Body, string? ContentType)> RelayAsync(
        string endpoint, IEnumerable<KeyValuePair<string, string>> parameters)
    {
        if (string.IsNullOrWhiteSpace(_subsonicSettings.Url)
            || !Uri.TryCreate(_subsonicSettings.Url, UriKind.Absolute, out _))
        {
            throw new OctoNotConfiguredException(
                "Octo has no valid Navidrome URL. Set SUBSONIC_URL (Subsonic__Url) to your " +
                "Navidrome server, e.g. http://192.168.1.10:4533 — an absolute URL reachable " +
                "from the Octo container, not localhost.");
        }

        var query = string.Join("&", parameters.Select(kv =>
            $"{Uri.EscapeDataString(kv.Key)}={Uri.EscapeDataString(kv.Value)}"));
        var url = $"{_subsonicSettings.Url.TrimEnd('/')}/{endpoint}?{query}";

        HttpResponseMessage response = await _httpClient.GetAsync(url);
        response.EnsureSuccessStatusCode();
        
        var body = await response.Content.ReadAsByteArrayAsync();
        var contentType = response.Content.Headers.ContentType?.ToString();
        
        return (body, contentType);
    }

    /// <summary>
    /// Faithful relay: forwards the caller's HTTP method + body + content-type and
    /// returns the upstream status verbatim (no EnsureSuccessStatusCode). This is
    /// what makes non-GET / body-carrying endpoints work through Octo — e.g.
    /// Navidrome's native POST /auth/login that some clients use to sign in.
    /// Requires request buffering (enabled in Program.cs) so the body is re-readable.
    /// </summary>
    // Headers forwarded to Navidrome so its native /api/* endpoints (used by
    // Navidrome-mode clients like Feishin) authenticate and behave correctly.
    private static readonly string[] ForwardRequestHeaders =
    {
        "Authorization", "X-Nd-Client-Unique-Id", "X-Nd-Authorization",
        "Accept", "User-Agent", "If-None-Match", "If-Modified-Since", "Range",
    };
    // Response headers passed back to the client (notably the rotated ND token).
    private static readonly string[] ForwardResponseHeaders =
    {
        "X-Nd-Authorization", "ETag", "Last-Modified", "Cache-Control",
        "Content-Range", "Accept-Ranges", "Vary",
    };

    public async Task<RawRelayResult> RelayRawAsync(
        string endpoint,
        Dictionary<string, string> parameters)
    {
        if (string.IsNullOrWhiteSpace(_subsonicSettings.Url)
            || !Uri.TryCreate(_subsonicSettings.Url, UriKind.Absolute, out _))
        {
            throw new OctoNotConfiguredException(
                "Octo has no valid Navidrome URL. Set SUBSONIC_URL (Subsonic__Url) to your " +
                "Navidrome server, e.g. http://192.168.1.10:4533 — an absolute URL reachable " +
                "from the Octo container, not localhost.");
        }

        var query = string.Join("&", parameters.Select(kv =>
            $"{Uri.EscapeDataString(kv.Key)}={Uri.EscapeDataString(kv.Value)}"));
        var url = $"{_subsonicSettings.Url.TrimEnd('/')}/{endpoint}?{query}";

        var ctx = _httpContextAccessor.HttpContext;
        var incoming = ctx?.Request;
        var method = incoming?.Method ?? "GET";
        using var req = new HttpRequestMessage(new HttpMethod(method), url);

        // Forward the raw request body captured by the middleware (the live body
        // stream is already closed by parameter extraction at this point).
        if (ctx?.Items.TryGetValue("Octo.RawBody", out var rb) == true
            && rb is byte[] bytes && bytes.Length > 0)
        {
            req.Content = new ByteArrayContent(bytes);
            if (!string.IsNullOrEmpty(incoming?.ContentType))
                req.Content.Headers.TryAddWithoutValidation("Content-Type", incoming.ContentType);
        }

        // Forward auth + conditional headers so native Navidrome endpoints work.
        if (incoming != null)
        {
            foreach (var h in ForwardRequestHeaders)
                if (incoming.Headers.TryGetValue(h, out var vals))
                    req.Headers.TryAddWithoutValidation(h, vals.ToArray());
        }

        using var response = await _httpClient.SendAsync(req);
        var body = await response.Content.ReadAsByteArrayAsync();

        var respHeaders = new List<KeyValuePair<string, string>>();
        foreach (var h in ForwardResponseHeaders)
        {
            if (response.Headers.TryGetValues(h, out var v) ||
                response.Content.Headers.TryGetValues(h, out v))
                foreach (var val in v) respHeaders.Add(new(h, val));
        }

        return new RawRelayResult((int)response.StatusCode, body,
            response.Content.Headers.ContentType?.ToString(), respHeaders);
    }

    /// <summary>
    /// Safely relays a request to the Subsonic server, returning null on failure.
    /// </summary>
    public async Task<(byte[]? Body, string? ContentType, bool Success)> RelaySafeAsync(
        string endpoint, 
        Dictionary<string, string> parameters)
    {
        try
        {
            var result = await RelayAsync(endpoint, parameters);
            return (result.Body, result.ContentType, true);
        }
        catch
        {
            return (null, null, false);
        }
    }

    private static readonly string[] StreamingRequiredHeaders =
    {
        "Accept-Ranges",
        "Content-Range",
        "Content-Length",
        "ETag",
        "Last-Modified"
    };

    /// <summary>
    /// Relays a stream request to the Subsonic server with range processing support.
    /// </summary>
    public async Task<IActionResult> RelayStreamAsync(
        Dictionary<string, string> parameters,
        CancellationToken cancellationToken)
    {
        try
        {
            // Get HTTP context for request/response forwarding
            var httpContext = _httpContextAccessor.HttpContext;
            if (httpContext == null)
            {
                return new ObjectResult(new { error = "HTTP context not available" })
                {
                    StatusCode = 500
                };
            }
            
            var incomingRequest = httpContext.Request;
            var outgoingResponse = httpContext.Response;

            var query = string.Join("&", parameters.Select(kv => 
                $"{Uri.EscapeDataString(kv.Key)}={Uri.EscapeDataString(kv.Value)}"));
            var url = $"{_subsonicSettings.Url}/rest/stream?{query}";
            
            using var request = new HttpRequestMessage(HttpMethod.Get, url);

            // Forward Range headers for progressive streaming support (iOS clients)
            if (incomingRequest.Headers.TryGetValue("Range", out var range))
            {
                request.Headers.TryAddWithoutValidation("Range", range.ToArray());
            }
            
            if (incomingRequest.Headers.TryGetValue("If-Range", out var ifRange))
            {
                request.Headers.TryAddWithoutValidation("If-Range", ifRange.ToArray());
            }
            
            var response = await _httpClient.SendAsync(
                request, 
                HttpCompletionOption.ResponseHeadersRead, 
                cancellationToken);
            
            if (!response.IsSuccessStatusCode)
            {
                return new StatusCodeResult((int)response.StatusCode);
            }

            // Forward HTTP status code (e.g., 206 Partial Content for range requests)
            outgoingResponse.StatusCode = (int)response.StatusCode;

            // Forward streaming-required headers from upstream response
            foreach (var header in StreamingRequiredHeaders)
            {
                if (response.Headers.TryGetValues(header, out var values) ||
                    response.Content.Headers.TryGetValues(header, out values))
                {
                    outgoingResponse.Headers[header] = values.ToArray();
                }
            }

            var stream = await response.Content.ReadAsStreamAsync(cancellationToken);
            var contentType = response.Content.Headers.ContentType?.ToString() ?? "audio/mpeg";
            
            return new FileStreamResult(stream, contentType)
            {
                EnableRangeProcessing = true
            };
        }
        catch (Exception ex)
        {
            return new ObjectResult(new { error = $"Error streaming from Subsonic: {ex.Message}" })
            {
                StatusCode = 500
            };
        }
    }

    /// <summary>Opens a full upstream track body without binding it to the current
    /// HTTP response. Continuous Radio feeds this stream into its in-process
    /// transcoder, so it must own the upstream response until the track ends.</summary>
    public async Task<DirectStreamInfo?> OpenAudioStreamAsync(
        IReadOnlyDictionary<string, string> parameters,
        CancellationToken cancellationToken)
    {
        if (string.IsNullOrWhiteSpace(_subsonicSettings.Url)
            || !Uri.TryCreate(_subsonicSettings.Url, UriKind.Absolute, out _)) return null;
        var query = string.Join("&", parameters.Select(pair =>
            $"{Uri.EscapeDataString(pair.Key)}={Uri.EscapeDataString(pair.Value)}"));
        var url = $"{_subsonicSettings.Url.TrimEnd('/')}/rest/stream?{query}";
        var response = await _httpClient.SendAsync(new HttpRequestMessage(HttpMethod.Get, url),
            HttpCompletionOption.ResponseHeadersRead, cancellationToken);
        if (!response.IsSuccessStatusCode)
        {
            response.Dispose();
            return null;
        }
        var stream = await response.Content.ReadAsStreamAsync(cancellationToken);
        return new DirectStreamInfo
        {
            AudioStream = new ResponseOwnedStream(stream, response),
            ContentType = response.Content.Headers.ContentType?.ToString() ?? "application/octet-stream",
            ContentLength = response.Content.Headers.ContentLength,
            Quality = "navidrome-raw",
        };
    }

    private sealed class ResponseOwnedStream(Stream inner, HttpResponseMessage owner) : Stream
    {
        public override bool CanRead => inner.CanRead;
        public override bool CanSeek => inner.CanSeek;
        public override bool CanWrite => false;
        public override long Length => inner.Length;
        public override long Position { get => inner.Position; set => inner.Position = value; }
        public override void Flush() => inner.Flush();
        public override int Read(byte[] buffer, int offset, int count) => inner.Read(buffer, offset, count);
        public override ValueTask<int> ReadAsync(Memory<byte> buffer,
            CancellationToken cancellationToken = default) => inner.ReadAsync(buffer, cancellationToken);
        public override long Seek(long offset, SeekOrigin origin) => inner.Seek(offset, origin);
        public override void SetLength(long value) => throw new NotSupportedException();
        public override void Write(byte[] buffer, int offset, int count) => throw new NotSupportedException();
        protected override void Dispose(bool disposing)
        {
            if (disposing) { inner.Dispose(); owner.Dispose(); }
            base.Dispose(disposing);
        }
        public override async ValueTask DisposeAsync()
        {
            await inner.DisposeAsync(); owner.Dispose(); GC.SuppressFinalize(this);
        }
    }
}

/// <summary>
/// Thrown when Octo's upstream Navidrome URL is missing or not an absolute URL.
/// The global handler surfaces its message to the client as an actionable
/// Subsonic error instead of an opaque "Invalid request".
/// </summary>
public class OctoNotConfiguredException : Exception
{
    public OctoNotConfiguredException(string message) : base(message) { }
}

/// <summary>Result of a faithful (method/body/status/header-preserving) relay.</summary>
public record RawRelayResult(
    int Status, byte[] Body, string? ContentType, List<KeyValuePair<string, string>> ResponseHeaders);
