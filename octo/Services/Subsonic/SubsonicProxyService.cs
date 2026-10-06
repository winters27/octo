using Microsoft.Extensions.Options;
using Microsoft.Extensions.Primitives;
using Microsoft.AspNetCore.Mvc;
using Microsoft.AspNetCore.WebUtilities;
using Octo.Models.Settings;
using System.Net;
using System.Text;

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
    private readonly ILogger<SubsonicProxyService>? _logger;

    public SubsonicProxyService(
        IHttpClientFactory httpClientFactory,
        IOptionsMonitor<SubsonicSettings> subsonicSettings,
        IHttpContextAccessor httpContextAccessor,
        ILogger<SubsonicProxyService>? logger = null)
    {
        _httpClient = httpClientFactory.CreateClient();
        subsonicSettingsOptions = subsonicSettings;
        _httpContextAccessor = httpContextAccessor;
        _logger = logger;
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

        var query = await BuildQueryAsync(parameters, bodyForwarded: false);
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
    //
    // This is an allowlist, so anything missing from it is silently dropped, and
    // X-Total-Count was. Navidrome's native list endpoints report their length only in
    // that header, and Navidrome-mode clients size their virtualised lists from it: with
    // no header, Feishin's Albums, Artists and Tracks pages have nothing to size against
    // and render empty, while Home and Search, which do not paginate, look perfectly fine
    // (issue #34). The body was always correct, which is why it read as a client bug.
    private static readonly string[] ForwardResponseHeaders =
    {
        "X-Nd-Authorization", "ETag", "Last-Modified", "Cache-Control",
        "Content-Range", "Accept-Ranges", "Vary",
        "X-Total-Count", "Access-Control-Expose-Headers",
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

        var ctx = _httpContextAccessor.HttpContext;
        var incoming = ctx?.Request;
        var method = incoming?.Method ?? "GET";
        var rawBody = ctx?.Items.TryGetValue("Octo.RawBody", out var rb) == true
            && rb is byte[] bytes && bytes.Length > 0 ? bytes : null;

        var query = await BuildQueryAsync(parameters, bodyForwarded: rawBody != null);
        var url = $"{_subsonicSettings.Url.TrimEnd('/')}/{endpoint}?{query}";
        using var req = new HttpRequestMessage(new HttpMethod(method), url);

        // Forward the raw request body captured by the middleware (the live body
        // stream is already closed by parameter extraction at this point).
        if (rawBody != null)
        {
            req.Content = new ByteArrayContent(rawBody);
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

        // Headers first, so the body can be read knowing whether its length is Navidrome's
        // estimate (rest/download with a format and estimateContentLength). The timeout
        // still covers the whole body, as it did when SendAsync read it.
        using var timeout = new CancellationTokenSource(_httpClient.Timeout);
        using var response = await _httpClient.SendAsync(req,
            HttpCompletionOption.ResponseHeadersRead, timeout.Token);
        byte[] body;
        if (LengthMayBeEstimate(response, parameters))
        {
            await using var upstream = TolerateEarlyEnd(
                await response.Content.ReadAsStreamAsync(timeout.Token), response, endpoint);
            using var buffer = new MemoryStream();
            await upstream.CopyToAsync(buffer, timeout.Token);
            body = buffer.ToArray();
        }
        else
        {
            body = await response.Content.ReadAsByteArrayAsync(timeout.Token);
        }

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

    /// <summary>Builds the upstream query string from the lookup dictionary and the
    /// client's own request.</summary>
    private async Task<string> BuildQueryAsync(
        IEnumerable<KeyValuePair<string, string>> parameters, bool bodyForwarded)
    {
        var incoming = _httpContextAccessor.HttpContext?.Request;
        var form = incoming is null ? null : await ReadFormAsync(incoming);
        // Navidrome reads a url-encoded body, not a multipart one, so only the first can
        // carry the fields for the query.
        var urlEncoded = incoming?.ContentType?.StartsWith("application/x-www-form-urlencoded",
            StringComparison.OrdinalIgnoreCase) == true;
        var pairs = RestoreRepeatedParameters(parameters, incoming?.Query, form,
            formInBody: bodyForwarded && urlEncoded && form is not null);
        return string.Join("&", pairs.Select(kv =>
            $"{Uri.EscapeDataString(kv.Key)}={Uri.EscapeDataString(kv.Value)}"));
    }

    /// <summary>
    /// The parameter dictionary holds a repeated key as one comma-joined string
    /// (<c>id=A&amp;id=B</c> reads "A,B"), and Navidrome takes that as ONE id. A value
    /// that is still exactly what the client sent goes out as the client's separate
    /// values again, in order; a value a handler changed or added goes out as it is.
    /// With <paramref name="formInBody"/>, an unchanged form field is left out of the
    /// query, because the forwarded body already carries it and Navidrome reads both.
    /// </summary>
    internal static List<KeyValuePair<string, string>> RestoreRepeatedParameters(
        IEnumerable<KeyValuePair<string, string>> parameters,
        IEnumerable<KeyValuePair<string, StringValues>>? query,
        IEnumerable<KeyValuePair<string, StringValues>>? form,
        bool formInBody)
    {
        var queryValues = ToLookup(query);
        var formValues = ToLookup(form);
        var result = new List<KeyValuePair<string, string>>();
        foreach (var (key, value) in parameters)
        {
            if (formValues.TryGetValue(key, out var sent) && Unchanged(value, sent))
            {
                if (!formInBody) result.AddRange(sent.Select(v => new KeyValuePair<string, string>(key, v ?? "")));
                continue;
            }
            if (queryValues.TryGetValue(key, out sent) && Unchanged(value, sent))
            {
                result.AddRange(sent.Select(v => new KeyValuePair<string, string>(key, v ?? "")));
                continue;
            }
            result.Add(new(key, value));
        }
        return result;

        static bool Unchanged(string value, StringValues sent) =>
            sent.Count > 0 && string.Equals(value, sent.ToString(), StringComparison.Ordinal);

        static Dictionary<string, StringValues> ToLookup(
            IEnumerable<KeyValuePair<string, StringValues>>? source)
        {
            var lookup = new Dictionary<string, StringValues>();
            if (source is null) return lookup;
            foreach (var (key, values) in source) lookup[key] = values;
            return lookup;
        }
    }

    /// <summary>The client's form fields, or null when the request carries none.</summary>
    private static async Task<IEnumerable<KeyValuePair<string, StringValues>>?> ReadFormAsync(
        HttpRequest request)
    {
        if (!request.HasFormContentType) return null;
        try
        {
            // Parameter extraction already read the form, so this returns the cached copy.
            return await request.ReadFormAsync();
        }
        catch
        {
            // Same fallback as the request parser: read the captured body by hand.
            if (request.HttpContext.Items.TryGetValue("Octo.RawBody", out var rb)
                && rb is byte[] bytes && bytes.Length > 0)
                return QueryHelpers.ParseQuery(Encoding.UTF8.GetString(bytes));
            return null;
        }
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
    /// True when the upstream Content-Length may be Navidrome's estimate of a transcode
    /// rather than the size of the body it will send. Navidrome serves a file, or a
    /// transcode it has finished and cached, with Accept-Ranges: bytes and an exact
    /// length, and a 206 carries an exact Content-Range. A transcode still being made
    /// says Accept-Ranges: none, and then has a length only because the client asked
    /// for an estimate (Verified 2026-10-05: announced 4,067,983, sent 3,974,093).
    /// With neither mark, the request decides: an estimate or a transcode was asked for.
    /// </summary>
    internal static bool LengthMayBeEstimate(HttpResponseMessage response,
        IEnumerable<KeyValuePair<string, string>> parameters)
    {
        if (response.StatusCode == HttpStatusCode.PartialContent) return false;
        var acceptRanges = response.Headers.AcceptRanges;
        if (acceptRanges.Contains("bytes", StringComparer.OrdinalIgnoreCase)) return false;
        if (acceptRanges.Contains("none", StringComparer.OrdinalIgnoreCase)) return true;

        string? Value(string key) => parameters
            .FirstOrDefault(pair => string.Equals(pair.Key, key, StringComparison.OrdinalIgnoreCase)).Value;
        if (string.Equals(Value("estimateContentLength"), "true", StringComparison.OrdinalIgnoreCase))
            return true;
        var format = Value("format");
        if (!string.IsNullOrEmpty(format) && !string.Equals(format, "raw", StringComparison.OrdinalIgnoreCase))
            return true;
        return int.TryParse(Value("maxBitRate"), out var maxBitRate) && maxBitRate > 0;
    }

    private Stream TolerateEarlyEnd(Stream upstream, HttpResponseMessage response, string endpoint)
    {
        var announced = response.Content.Headers.ContentLength;
        return new EstimatedLengthStream(upstream, sent => _logger?.LogDebug(
            "{Endpoint}: the transcode ended at {Sent} bytes, short of the estimated {Announced}",
            endpoint, sent, announced));
    }

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

            var query = await BuildQueryAsync(parameters, bodyForwarded: false);
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
                // A 416 names the real size ("bytes */N"). A player that asked past the end
                // of a transcode, usually because it believed an estimate, learns where the
                // end is; ExoPlayer reads that as the end of the song.
                if (response.StatusCode == HttpStatusCode.RequestedRangeNotSatisfiable
                    && response.Content.Headers.TryGetValues("Content-Range", out var size))
                {
                    outgoingResponse.Headers["Content-Range"] = size.ToArray();
                }
                var status = (int)response.StatusCode;
                response.Dispose();
                return new StatusCodeResult(status);
            }

            // Forward HTTP status code (e.g., 206 Partial Content for range requests)
            outgoingResponse.StatusCode = (int)response.StatusCode;

            // A live transcode's Content-Length is Navidrome's guess, and the real output
            // usually comes out 2 to 3% smaller. Promising the guess to the client meant
            // the copy failed after the last real byte and the connection was cut, so
            // that length is left out and the body goes chunked.
            var estimate = LengthMayBeEstimate(response, parameters);

            // Forward streaming-required headers from upstream response
            foreach (var header in StreamingRequiredHeaders)
            {
                if (estimate && header == "Content-Length") continue;
                if (response.Headers.TryGetValues(header, out var values) ||
                    response.Content.Headers.TryGetValues(header, out values))
                {
                    outgoingResponse.Headers[header] = values.ToArray();
                }
            }

            var stream = await response.Content.ReadAsStreamAsync(cancellationToken);
            if (estimate) stream = TolerateEarlyEnd(stream, response, "rest/stream");
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

    /// <summary>
    /// A transcode whose Content-Length was an estimate. HttpClient holds the body to the
    /// length announced and throws when it ends sooner, which here only means the real
    /// output was smaller than the guess, so that early end reads as the end of the song.
    /// Any other failure still throws.
    /// </summary>
    private sealed class EstimatedLengthStream(Stream inner, Action<long> endedShort) : Stream
    {
        private long _read;
        private bool _ended;

        public override bool CanRead => true;
        public override bool CanSeek => false;
        public override bool CanWrite => false;
        public override long Length => throw new NotSupportedException();
        public override long Position
        {
            get => _read;
            set => throw new NotSupportedException();
        }
        public override void Flush() { }

        public override int Read(byte[] buffer, int offset, int count)
        {
            if (_ended) return 0;
            try { return Counted(inner.Read(buffer, offset, count)); }
            catch (HttpIOException ex) when (ex.HttpRequestError == HttpRequestError.ResponseEnded)
            {
                return EndedShort();
            }
        }

        public override Task<int> ReadAsync(byte[] buffer, int offset, int count,
            CancellationToken cancellationToken) =>
            ReadAsync(buffer.AsMemory(offset, count), cancellationToken).AsTask();

        public override async ValueTask<int> ReadAsync(Memory<byte> buffer,
            CancellationToken cancellationToken = default)
        {
            if (_ended) return 0;
            try { return Counted(await inner.ReadAsync(buffer, cancellationToken)); }
            catch (HttpIOException ex) when (ex.HttpRequestError == HttpRequestError.ResponseEnded)
            {
                return EndedShort();
            }
        }

        private int Counted(int read)
        {
            _read += read;
            return read;
        }

        private int EndedShort()
        {
            _ended = true;
            endedShort(_read);
            return 0;
        }

        public override long Seek(long offset, SeekOrigin origin) => throw new NotSupportedException();
        public override void SetLength(long value) => throw new NotSupportedException();
        public override void Write(byte[] buffer, int offset, int count) => throw new NotSupportedException();
        protected override void Dispose(bool disposing)
        {
            if (disposing) inner.Dispose();
            base.Dispose(disposing);
        }
        public override async ValueTask DisposeAsync()
        {
            await inner.DisposeAsync();
            GC.SuppressFinalize(this);
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
