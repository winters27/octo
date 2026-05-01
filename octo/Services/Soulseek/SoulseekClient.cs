using System.Net.Http.Headers;
using System.Text;
using System.Text.Json;
using Microsoft.Extensions.Options;
using Octo.Models.Settings;

namespace Octo.Services.Soulseek;

/// <summary>
/// Thin HTTP client for slskd's REST API. Handles auth and the small set of
/// endpoints Octo needs: search, browse responses, enqueue download, poll status.
/// </summary>
public class SoulseekClient
{
    private readonly HttpClient _http;
    private readonly SoulseekSettings _settings;
    private readonly ILogger<SoulseekClient> _logger;

    private string? _jwt;
    private DateTime _jwtExpiresUtc = DateTime.MinValue;
    private readonly SemaphoreSlim _authLock = new(1, 1);

    public SoulseekClient(
        IHttpClientFactory httpClientFactory,
        IOptions<SoulseekSettings> settings,
        ILogger<SoulseekClient> logger)
    {
        _http = httpClientFactory.CreateClient();
        _settings = settings.Value;
        _logger = logger;
        _http.Timeout = TimeSpan.FromSeconds(30);
    }

    private string Base => (_settings.BaseUrl ?? "http://localhost:5030").TrimEnd('/');

    /// <summary>
    /// Fetches and caches a JWT from slskd's session endpoint. Re-authenticates
    /// when the cached token is missing or near expiry.
    /// </summary>
    private async Task<string?> GetJwtAsync(CancellationToken ct)
    {
        if (!string.IsNullOrEmpty(_jwt) && DateTime.UtcNow < _jwtExpiresUtc.AddMinutes(-1))
            return _jwt;

        await _authLock.WaitAsync(ct);
        try
        {
            if (!string.IsNullOrEmpty(_jwt) && DateTime.UtcNow < _jwtExpiresUtc.AddMinutes(-1))
                return _jwt;

            if (string.IsNullOrWhiteSpace(_settings.Username) || string.IsNullOrWhiteSpace(_settings.Password))
            {
                _logger.LogWarning("Soulseek__Username/Password not set; cannot authenticate to slskd");
                return null;
            }

            var body = JsonSerializer.Serialize(new { username = _settings.Username, password = _settings.Password });
            using var resp = await _http.PostAsync(
                $"{Base}/api/v0/session",
                new StringContent(body, Encoding.UTF8, "application/json"),
                ct);

            if (!resp.IsSuccessStatusCode)
            {
                _logger.LogWarning("slskd auth failed: HTTP {Code}", (int)resp.StatusCode);
                _jwt = null;
                return null;
            }

            var json = await resp.Content.ReadAsStringAsync(ct);
            using var doc = JsonDocument.Parse(json);
            _jwt = doc.RootElement.GetProperty("token").GetString();
            _jwtExpiresUtc = DateTimeOffset.FromUnixTimeSeconds(
                doc.RootElement.GetProperty("expires").GetInt64()).UtcDateTime;
            return _jwt;
        }
        finally
        {
            _authLock.Release();
        }
    }

    private async Task<HttpRequestMessage> AuthedRequestAsync(HttpMethod method, string url, CancellationToken ct)
    {
        var req = new HttpRequestMessage(method, url);
        var jwt = await GetJwtAsync(ct);
        if (!string.IsNullOrEmpty(jwt))
            req.Headers.Authorization = new AuthenticationHeaderValue("Bearer", jwt);
        return req;
    }

    private async Task<HttpResponseMessage> SendAsync(HttpMethod method, string url, HttpContent? content, CancellationToken ct)
    {
        var req = await AuthedRequestAsync(method, url, ct);
        if (content != null) req.Content = content;
        var resp = await _http.SendAsync(req, ct);

        // If the JWT was rejected (e.g. rotated key), refresh once and retry.
        if (resp.StatusCode == System.Net.HttpStatusCode.Unauthorized)
        {
            _jwt = null;
            using var retryReq = await AuthedRequestAsync(method, url, ct);
            if (content != null) retryReq.Content = content;
            resp.Dispose();
            return await _http.SendAsync(retryReq, ct);
        }
        return resp;
    }

    public async Task<bool> IsReachableAsync(CancellationToken ct = default)
    {
        try
        {
            using var resp = await SendAsync(HttpMethod.Get, $"{Base}/api/v0/application", null, ct);
            return resp.IsSuccessStatusCode;
        }
        catch (Exception ex)
        {
            _logger.LogWarning("slskd not reachable at {Base}: {Msg}", Base, ex.Message);
            return false;
        }
    }

    /// <summary>
    /// Initiates a search and waits up to SearchWaitSeconds for peer responses,
    /// then returns the responses regardless of search-completed state.
    /// </summary>
    public async Task<List<SoulseekFileHit>> SearchAsync(string query, int limit, CancellationToken ct = default)
    {
        var searchId = Guid.NewGuid().ToString();
        var payload = JsonSerializer.Serialize(new
        {
            id = searchId,
            searchText = query,
            fileLimit = Math.Max(limit * 5, 50),
            filterResponses = true
        });

        try
        {
            using var startResp = await SendAsync(
                HttpMethod.Post,
                $"{Base}/api/v0/searches",
                new StringContent(payload, Encoding.UTF8, "application/json"),
                ct);
            startResp.EnsureSuccessStatusCode();
        }
        catch (Exception ex)
        {
            _logger.LogWarning("Soulseek search start failed: {Msg}", ex.Message);
            return new List<SoulseekFileHit>();
        }

        // Poll responses for up to SearchWaitSeconds
        var hits = new List<SoulseekFileHit>();
        var deadline = DateTime.UtcNow.AddSeconds(_settings.SearchWaitSeconds);
        var pollIntervalMs = 1000;

        while (DateTime.UtcNow < deadline && !ct.IsCancellationRequested)
        {
            await Task.Delay(pollIntervalMs, ct);
            try
            {
                using var resp = await SendAsync(
                    HttpMethod.Get,
                    $"{Base}/api/v0/searches/{searchId}/responses",
                    null,
                    ct);
                if (!resp.IsSuccessStatusCode) continue;

                var json = await resp.Content.ReadAsStringAsync(ct);
                hits = ParseResponses(json);
                if (hits.Count >= limit) break;
            }
            catch (Exception ex)
            {
                _logger.LogDebug("Poll failed (transient): {Msg}", ex.Message);
            }
        }

        // Fire-and-forget cleanup so we don't accumulate completed searches
        _ = Task.Run(async () =>
        {
            try { using var _ = await SendAsync(HttpMethod.Delete, $"{Base}/api/v0/searches/{searchId}", null, CancellationToken.None); }
            catch { /* best effort */ }
        });

        return hits;
    }

    private List<SoulseekFileHit> ParseResponses(string json)
    {
        var hits = new List<SoulseekFileHit>();
        try
        {
            using var doc = JsonDocument.Parse(json);
            if (doc.RootElement.ValueKind != JsonValueKind.Array) return hits;

            foreach (var resp in doc.RootElement.EnumerateArray())
            {
                if (!resp.TryGetProperty("username", out var unameEl)) continue;
                var username = unameEl.GetString() ?? "";
                if (string.IsNullOrWhiteSpace(username)) continue;

                if (!resp.TryGetProperty("files", out var filesEl) ||
                    filesEl.ValueKind != JsonValueKind.Array) continue;

                int? uploadSpeed = resp.TryGetProperty("uploadSpeed", out var spEl) && spEl.ValueKind == JsonValueKind.Number
                    ? spEl.GetInt32()
                    : null;
                int? queueLength = resp.TryGetProperty("queueLength", out var qlEl) && qlEl.ValueKind == JsonValueKind.Number
                    ? qlEl.GetInt32()
                    : null;

                foreach (var file in filesEl.EnumerateArray())
                {
                    var filename = file.TryGetProperty("filename", out var fnEl) ? fnEl.GetString() : null;
                    if (string.IsNullOrWhiteSpace(filename)) continue;

                    long size = file.TryGetProperty("size", out var szEl) && szEl.ValueKind == JsonValueKind.Number
                        ? szEl.GetInt64()
                        : 0;
                    int? bitRate = file.TryGetProperty("bitRate", out var brEl) && brEl.ValueKind == JsonValueKind.Number
                        ? brEl.GetInt32()
                        : null;
                    int? sampleRate = file.TryGetProperty("sampleRate", out var srEl) && srEl.ValueKind == JsonValueKind.Number
                        ? srEl.GetInt32()
                        : null;
                    int? bitDepth = file.TryGetProperty("bitDepth", out var bdEl) && bdEl.ValueKind == JsonValueKind.Number
                        ? bdEl.GetInt32()
                        : null;
                    int? length = file.TryGetProperty("length", out var lenEl) && lenEl.ValueKind == JsonValueKind.Number
                        ? lenEl.GetInt32()
                        : null;
                    var ext = file.TryGetProperty("extension", out var exEl) ? exEl.GetString() : null;

                    hits.Add(new SoulseekFileHit
                    {
                        Username = username,
                        Filename = filename,
                        Size = size,
                        BitRate = bitRate,
                        SampleRate = sampleRate,
                        BitDepth = bitDepth,
                        Length = length,
                        Extension = (ext ?? Path.GetExtension(filename).TrimStart('.')).ToLowerInvariant(),
                        UploadSpeed = uploadSpeed,
                        QueueLength = queueLength
                    });
                }
            }
        }
        catch (Exception ex)
        {
            _logger.LogWarning("Failed to parse Soulseek responses: {Msg}", ex.Message);
        }
        return hits;
    }

    /// <summary>
    /// Enqueues a download from a specific peer. Returns when the request is accepted by slskd
    /// (not when the file is fully transferred — caller polls for that).
    /// </summary>
    public async Task EnqueueDownloadAsync(string username, string filename, long size, CancellationToken ct = default)
    {
        var body = JsonSerializer.Serialize(new[]
        {
            new { filename, size }
        });

        using var resp = await SendAsync(
            HttpMethod.Post,
            $"{Base}/api/v0/transfers/downloads/{Uri.EscapeDataString(username)}",
            new StringContent(body, Encoding.UTF8, "application/json"),
            ct);

        if (!resp.IsSuccessStatusCode)
        {
            var err = await resp.Content.ReadAsStringAsync(ct);
            throw new Exception($"slskd download enqueue failed: HTTP {(int)resp.StatusCode} {err}");
        }
    }

    /// <summary>
    /// Polls a download to completion. Returns Succeeded on success or Errored
    /// on any kind of slskd-side failure (peer rejected, timed out, cancelled,
    /// or the transfer silently disappeared from slskd's active list — which
    /// happens after rejection on some slskd versions and would otherwise hang
    /// us forever). Caller decides whether to retry or escalate.
    /// </summary>
    public async Task<SoulseekTransferState> WaitForCompletionAsync(string username, string filename, int? perAttemptTimeoutSeconds = null, CancellationToken ct = default)
    {
        var timeoutSec = perAttemptTimeoutSeconds ?? _settings.DownloadTimeoutSeconds;
        var deadline = DateTime.UtcNow.AddSeconds(timeoutSec);
        var seenAtLeastOnce = false;
        var consecutiveMisses = 0;
        // After we've seen the transfer at least once, missing it for this many
        // consecutive polls means slskd dropped it and we should give up. Some
        // slskd versions remove rejected transfers from the active-list endpoint
        // immediately, so without this we'd poll forever.
        const int MaxConsecutiveMissesAfterSeen = 6;  // ~9s at 1500ms cadence

        while (DateTime.UtcNow < deadline && !ct.IsCancellationRequested)
        {
            await Task.Delay(1500, ct);

            try
            {
                using var resp = await SendAsync(
                    HttpMethod.Get,
                    $"{Base}/api/v0/transfers/downloads/{Uri.EscapeDataString(username)}",
                    null,
                    ct);
                if (!resp.IsSuccessStatusCode)
                {
                    if (seenAtLeastOnce) consecutiveMisses++;
                    if (consecutiveMisses >= MaxConsecutiveMissesAfterSeen)
                    {
                        _logger.LogWarning("slskd transfer disappeared after rejection (no longer queryable): {File}", filename);
                        return SoulseekTransferState.Errored;
                    }
                    continue;
                }

                var json = await resp.Content.ReadAsStringAsync(ct);
                using var doc = JsonDocument.Parse(json);
                if (doc.RootElement.ValueKind != JsonValueKind.Array)
                {
                    if (seenAtLeastOnce) consecutiveMisses++;
                    if (consecutiveMisses >= MaxConsecutiveMissesAfterSeen) return SoulseekTransferState.Errored;
                    continue;
                }

                // Walk the user/directory/file tree looking for our file.
                bool foundThisPoll = false;
                foreach (var userGroup in doc.RootElement.EnumerateArray())
                {
                    if (!userGroup.TryGetProperty("directories", out var dirs)) continue;
                    foreach (var dir in dirs.EnumerateArray())
                    {
                        if (!dir.TryGetProperty("files", out var files)) continue;
                        foreach (var file in files.EnumerateArray())
                        {
                            var fn = file.TryGetProperty("filename", out var fnEl) ? fnEl.GetString() : null;
                            if (fn != filename) continue;
                            foundThisPoll = true;
                            seenAtLeastOnce = true;
                            consecutiveMisses = 0;

                            var state = file.TryGetProperty("state", out var stEl) ? stEl.GetString() ?? "" : "";

                            if (state.Contains("Completed", StringComparison.OrdinalIgnoreCase) &&
                                state.Contains("Succeeded", StringComparison.OrdinalIgnoreCase))
                            {
                                return SoulseekTransferState.Succeeded;
                            }
                            if (state.Contains("Errored", StringComparison.OrdinalIgnoreCase) ||
                                state.Contains("Cancelled", StringComparison.OrdinalIgnoreCase) ||
                                state.Contains("Rejected", StringComparison.OrdinalIgnoreCase) ||
                                state.Contains("TimedOut", StringComparison.OrdinalIgnoreCase))
                            {
                                _logger.LogDebug("slskd transfer ended in state: {State}", state);
                                return SoulseekTransferState.Errored;
                            }
                        }
                    }
                }

                if (!foundThisPoll && seenAtLeastOnce)
                {
                    consecutiveMisses++;
                    if (consecutiveMisses >= MaxConsecutiveMissesAfterSeen)
                    {
                        _logger.LogWarning("slskd transfer disappeared from active list (rejection or cleanup): {File}", filename);
                        return SoulseekTransferState.Errored;
                    }
                }
            }
            catch (Exception ex) when (ex is not TaskCanceledException)
            {
                _logger.LogDebug("Transfer poll transient: {Msg}", ex.Message);
            }
        }

        _logger.LogWarning("slskd transfer timed out after {Sec}s: {File}", timeoutSec, filename);
        return SoulseekTransferState.Errored;
    }
}

public class SoulseekFileHit
{
    public string Username { get; set; } = "";
    public string Filename { get; set; } = "";
    public long Size { get; set; }
    public int? BitRate { get; set; }
    public int? SampleRate { get; set; }
    public int? BitDepth { get; set; }
    public int? Length { get; set; }
    public string Extension { get; set; } = "";
    public int? UploadSpeed { get; set; }
    public int? QueueLength { get; set; }
}

public enum SoulseekTransferState
{
    Succeeded,
    Errored
}
