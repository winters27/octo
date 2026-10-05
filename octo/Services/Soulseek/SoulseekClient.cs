using System.Globalization;
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

    // slskd runs one search start or one enqueue at a time and answers 429 to a second arriving in
    // the same moment. With downloads side by side Octo now makes those itself, so its own POSTs
    // queue here, and only the POST: never a wait on what it started.
    private readonly SemaphoreSlim _operationGate = new(1, 1);
    private DateTime _lastSearchStartUtc = DateTime.MinValue;

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
    /// slskd's own word on the Soulseek network, from the same /api/v0/application call that
    /// proves slskd is up. During Soulseek's maintenance on 2026-10-03 slskd answered every call
    /// while sitting in "Disconnecting", and every search failed with "must be connected and
    /// logged in". Null when slskd did not answer.
    /// </summary>
    public async Task<SoulseekServerReading?> ReadServerAsync(CancellationToken ct = default)
    {
        try
        {
            using var resp = await SendAsync(HttpMethod.Get, $"{Base}/api/v0/application", null, ct);
            if (!resp.IsSuccessStatusCode) return null;
            return ParseServerReading(await resp.Content.ReadAsStringAsync(ct));
        }
        catch (Exception ex) when (!ct.IsCancellationRequested)
        {
            _logger.LogDebug("slskd state not readable at {Base}: {Msg}", Base, ex.Message);
            return null;
        }
    }

    /// <summary>
    /// Reads server.isConnected and server.isLoggedIn, exactly what slskd checks before it will
    /// start a search, rather than the state words. A shape without them is Unknown, which never
    /// holds anything back. address and ipEndPoint are left out while disconnected, so nothing
    /// here depends on them.
    /// </summary>
    internal static SoulseekServerReading ParseServerReading(string json)
    {
        try
        {
            using var doc = JsonDocument.Parse(json);
            var root = doc.RootElement;
            if (!TryGetPropertyIgnoreCase(root, "server", out var server) || server.ValueKind != JsonValueKind.Object)
                return new(SoulseekLinkState.Unknown, null, null, null);
            var connected = Flag(server, "isConnected");
            var loggedIn = Flag(server, "isLoggedIn");
            var link = connected is null || loggedIn is null ? SoulseekLinkState.Unknown
                : connected.Value && loggedIn.Value ? SoulseekLinkState.LoggedIn
                : SoulseekLinkState.NotLoggedIn;
            var state = TryGetPropertyIgnoreCase(server, "state", out var s) && s.ValueKind == JsonValueKind.String
                ? s.GetString() : null;
            var username = TryGetPropertyIgnoreCase(root, "user", out var user)
                && TryGetPropertyIgnoreCase(user, "username", out var u) && u.ValueKind == JsonValueKind.String
                ? u.GetString() : null;
            DateTime? next = TryGetPropertyIgnoreCase(root, "connectionWatchdog", out var dog)
                && TryGetPropertyIgnoreCase(dog, "nextAttemptAt", out var n) && n.ValueKind == JsonValueKind.String
                && DateTime.TryParse(n.GetString(), CultureInfo.InvariantCulture,
                    DateTimeStyles.AdjustToUniversal | DateTimeStyles.AssumeUniversal, out var at)
                ? at : null;
            return new(link, state, username, next);
        }
        catch (JsonException)
        {
            return new(SoulseekLinkState.Unknown, null, null, null);
        }
    }

    /// <summary>
    /// One of slskd's read-only answers as text, for the Sharing card: application, options,
    /// shares or uploads. Null when slskd did not answer it. Nothing read here changes slskd.
    /// </summary>
    public async Task<string?> ReadAsync(string path, CancellationToken ct = default)
    {
        try
        {
            using var resp = await SendAsync(HttpMethod.Get, $"{Base}/api/v0/{path.TrimStart('/')}", null, ct);
            return resp.IsSuccessStatusCode ? await resp.Content.ReadAsStringAsync(ct) : null;
        }
        catch (Exception ex) when (!ct.IsCancellationRequested)
        {
            _logger.LogDebug("slskd {Path} not readable at {Base}: {Msg}", path, Base, ex.Message);
            return null;
        }
    }

    /// <summary>
    /// Asks slskd to look through its shared folders again, so songs added since its last look are
    /// shared. Null when it started; otherwise why not, in words for the dashboard.
    /// </summary>
    public async Task<string?> RescanSharesAsync(CancellationToken ct = default)
    {
        try
        {
            using var resp = await SendAsync(HttpMethod.Put, $"{Base}/api/v0/shares", null, ct);
            if (resp.IsSuccessStatusCode) return null;
            return resp.StatusCode == System.Net.HttpStatusCode.Conflict
                ? "slskd is already looking through your shared folders."
                : $"slskd refused the rescan (HTTP {(int)resp.StatusCode}).";
        }
        catch (Exception ex) when (!ct.IsCancellationRequested)
        {
            _logger.LogWarning("slskd share rescan failed at {Base}: {Msg}", Base, ex.Message);
            return "Octo cannot reach slskd.";
        }
    }

    /// <summary>
    /// slskd's own settings file (slskd.yml) as text. Status is slskd's answer: 403 means slskd does
    /// not allow remote configuration, so nothing in it can be changed from here.
    /// </summary>
    public async Task<(int Status, string? Yaml)> ReadSettingsFileAsync(CancellationToken ct = default)
    {
        try
        {
            using var resp = await SendAsync(HttpMethod.Get, $"{Base}/api/v0/options/yaml", null, ct);
            if (!resp.IsSuccessStatusCode) return ((int)resp.StatusCode, null);
            var text = await resp.Content.ReadAsStringAsync(ct);
            // slskd answers with the text itself, or with it as a JSON string when asked for JSON.
            if (resp.Content.Headers.ContentType?.MediaType?.Contains("json", StringComparison.OrdinalIgnoreCase) == true)
                text = JsonSerializer.Deserialize<string>(text) ?? "";
            return ((int)resp.StatusCode, text);
        }
        catch (Exception ex) when (!ct.IsCancellationRequested)
        {
            _logger.LogDebug("slskd settings file not readable at {Base}: {Msg}", Base, ex.Message);
            return (0, null);
        }
    }

    /// <summary>
    /// Replaces slskd.yml. slskd checks it first and keeps the old file as slskd.yml.bak; with its
    /// file watch on, it applies the new one without a restart. Null when written, otherwise why not.
    /// </summary>
    public async Task<string?> WriteSettingsFileAsync(string yaml, CancellationToken ct = default)
    {
        try
        {
            using var resp = await SendAsync(HttpMethod.Put, $"{Base}/api/v0/options/yaml",
                new StringContent(JsonSerializer.Serialize(yaml), Encoding.UTF8, "application/json"), ct);
            if (resp.IsSuccessStatusCode) return null;
            if (resp.StatusCode == System.Net.HttpStatusCode.Forbidden)
                return "slskd does not let Octo change its settings (remote configuration is off).";
            var body = await resp.Content.ReadAsStringAsync(ct);
            return $"slskd refused the change (HTTP {(int)resp.StatusCode}){(string.IsNullOrWhiteSpace(body) ? "" : $": {body.Trim()}")}.";
        }
        catch (Exception ex) when (!ct.IsCancellationRequested)
        {
            _logger.LogWarning("Could not write slskd settings at {Base}: {Msg}", Base, ex.Message);
            return "Octo cannot reach slskd.";
        }
    }

    /// <summary>Stops a share scan in progress. False when none was running or slskd did not answer.</summary>
    public async Task<bool> CancelShareScanAsync(CancellationToken ct = default)
    {
        try
        {
            using var resp = await SendAsync(HttpMethod.Delete, $"{Base}/api/v0/shares", null, ct);
            return resp.IsSuccessStatusCode;
        }
        catch (Exception ex) when (!ct.IsCancellationRequested)
        {
            _logger.LogDebug("slskd share scan cancel failed at {Base}: {Msg}", Base, ex.Message);
            return false;
        }
    }

    /// <summary>Cancels one upload to someone. False when slskd would not.</summary>
    public async Task<bool> CancelUploadAsync(string username, string id, CancellationToken ct = default)
    {
        try
        {
            using var resp = await SendAsync(HttpMethod.Delete,
                $"{Base}/api/v0/transfers/uploads/{Uri.EscapeDataString(username)}/{Uri.EscapeDataString(id)}", null, ct);
            return resp.IsSuccessStatusCode;
        }
        catch (Exception ex) when (!ct.IsCancellationRequested)
        {
            _logger.LogDebug("slskd upload cancel failed at {Base}: {Msg}", Base, ex.Message);
            return false;
        }
    }

    private static bool? Flag(JsonElement element, string name) =>
        TryGetPropertyIgnoreCase(element, name, out var v) && v.ValueKind is JsonValueKind.True or JsonValueKind.False
            ? v.GetBoolean() : null;

    /// <summary>
    /// Reads slskd's resolved downloads directory from /api/v0/options. Purely a
    /// diagnostic: a null (endpoint missing, redacted, or unexpected shape) must
    /// never gate anything.
    /// </summary>
    public Task<string?> GetDownloadsDirectoryAsync(CancellationToken ct = default) =>
        GetDirectoryOptionAsync("downloads", ct);

    /// <summary>
    /// Where slskd writes a transfer before moving it to the downloads directory (#69). Only
    /// its last folder name is any use: the full path is slskd's view of its own container.
    /// Null is normal and leaves slskd's default name in force.
    /// </summary>
    public Task<string?> GetIncompleteDirectoryAsync(CancellationToken ct = default) =>
        GetDirectoryOptionAsync("incomplete", ct);

    private async Task<string?> GetDirectoryOptionAsync(string name, CancellationToken ct)
    {
        try
        {
            using var resp = await SendAsync(HttpMethod.Get, $"{Base}/api/v0/options", null, ct);
            if (!resp.IsSuccessStatusCode) return null;

            var json = await resp.Content.ReadAsStringAsync(ct);
            using var doc = JsonDocument.Parse(json);
            if (doc.RootElement.ValueKind != JsonValueKind.Object) return null;
            if (!TryGetPropertyIgnoreCase(doc.RootElement, "directories", out var dirs)) return null;
            if (!TryGetPropertyIgnoreCase(dirs, name, out var value)) return null;
            return value.ValueKind == JsonValueKind.String ? value.GetString() : null;
        }
        catch (Exception ex)
        {
            _logger.LogDebug("Could not read slskd options: {Msg}", ex.Message);
            return null;
        }
    }

    private static bool TryGetPropertyIgnoreCase(JsonElement element, string name, out JsonElement value)
    {
        value = default;
        if (element.ValueKind != JsonValueKind.Object) return false;
        if (element.TryGetProperty(name, out value)) return true;
        foreach (var prop in element.EnumerateObject())
        {
            if (string.Equals(prop.Name, name, StringComparison.OrdinalIgnoreCase))
            {
                value = prop.Value;
                return true;
            }
        }
        return false;
    }

    /// <summary>
    /// Runs one Soulseek search through slskd and returns every file it found.
    ///
    /// slskd keeps a running search's responses in memory and saves them only when the search
    /// ends (SearchService.cs, slskd 0.26.0), so /responses is empty until then and there is
    /// nothing to act on early. Octo reads the search's state until slskd ends it, then reads
    /// the responses once. A search still running at the profile's ceiling is cancelled, not
    /// abandoned: a cancelled search still saves what it gathered. Octo used to read nothing at
    /// the ceiling and delete the search, which returned zero hits and left the search running
    /// in slskd, because DELETE removes only the record.
    ///
    /// A caller who gives up still gets an OperationCanceledException, as before, so a cancelled
    /// acquisition is not mistaken for "not on Soulseek"; the slskd search is cancelled behind it.
    /// </summary>
    public async Task<List<SoulseekFileHit>> SearchAsync(string query, SearchProfile profile, CancellationToken ct = default) =>
        (await SearchWithEndAsync(query, profile, ct)).Hits;

    /// <summary>
    /// <see cref="SearchAsync"/>, and whether slskd stopped it at the profile's response or file
    /// limit rather than when the answers ran out: such a search saw only the fastest peers.
    /// </summary>
    public async Task<SoulseekSearchOutcome> SearchWithEndAsync(string query, SearchProfile profile, CancellationToken ct = default)
    {
        var searchId = Guid.NewGuid().ToString();
        var began = Clock();
        var ended = false;
        // Set before the start goes out: a caller who gives up while it is on its way leaves a
        // search slskd may already have taken, and cancelling one it never made does no harm.
        var started = true;
        try
        {
            if (!await StartSearchAsync(searchId, query, profile, ct))
            {
                started = false;
                return new SoulseekSearchOutcome([], false);
            }
            var status = await WaitForEndAsync(searchId, began.AddSeconds(profile.CeilingSeconds), ct);
            string reason;
            if (status is { Ended: true })
            {
                reason = "finished";
            }
            else
            {
                await CancelSearchAsync(searchId);
                status = await WaitForEndAsync(searchId, Clock() + CancelGrace, ct) ?? status;
                reason = status is { Ended: true } ? "ceiling, cancelled" : "ceiling, cancel not confirmed";
            }
            ended = status is { Ended: true };

            // Read even when the cancel was not confirmed: slskd may have finished since the last
            // look, and one request is cheap next to the wait already spent.
            var hits = await ReadResponsesAsync(searchId, ct);
            _logger.LogInformation(
                "Soulseek search '{Query}' ({Profile}): {Count} hits after {Elapsed:F1}s ({Reason}; slskd {State}, {Responses} responses)",
                query, profile.Name, hits.Count, (Clock() - began).TotalSeconds, reason,
                status?.State ?? "unknown", status?.ResponseCount ?? 0);
            return new SoulseekSearchOutcome(hits, HitLimit(status?.State));
        }
        catch (OperationCanceledException) when (ct.IsCancellationRequested)
        {
            _logger.LogInformation("Soulseek search '{Query}' ({Profile}): given up after {Elapsed:F1}s; cancelling it in slskd",
                query, profile.Name, (Clock() - began).TotalSeconds);
            throw;
        }
        finally
        {
            // In the background, so nobody waits on housekeeping.
            if (started) LastSearchCleanup = Task.Run(() => CleanUpSearchAsync(searchId, ended));
        }
    }

    internal static string SearchPayload(string searchId, string query, SearchProfile profile) =>
        JsonSerializer.Serialize(new
        {
            id = searchId,
            searchText = query,
            // Milliseconds, whatever slskd's own API doc says: it is passed to Soulseek.NET unchanged.
            searchTimeout = profile.SearchTimeoutMs,
            responseLimit = profile.ResponseLimit,
            fileLimit = profile.FileLimit,
            filterResponses = true,
        });

    private async Task<bool> StartSearchAsync(string searchId, string query, SearchProfile profile, CancellationToken ct)
    {
        try
        {
            using var resp = await SendOperationAsync($"{Base}/api/v0/searches",
                SearchPayload(searchId, query, profile), search: true, ct);
            resp.EnsureSuccessStatusCode();
            return true;
        }
        catch (Exception ex) when (!ct.IsCancellationRequested)
        {
            _logger.LogWarning("Soulseek search start failed: {Msg}", ex.Message);
            return false;
        }
    }

    /// <summary>How many times a POST slskd refused with 429 is sent again, waiting
    /// SearchStartRetryDelay longer each time.</summary>
    internal const int OperationRetries = 3;

    /// <summary>The least time between two search starts, so searches made side by side reach the
    /// Soulseek network spaced out rather than in a burst. Only tests shorten it.</summary>
    internal TimeSpan MinSearchSpacing { get; set; } = TimeSpan.FromSeconds(1);

    /// <summary>
    /// One POST to slskd's one-at-a-time endpoints (search start, enqueue), through Octo's own gate
    /// and sent again on 429. A 429 that survives every retry is handed back for the caller to read.
    /// </summary>
    private async Task<HttpResponseMessage> SendOperationAsync(string url, string body, bool search, CancellationToken ct)
    {
        for (var attempt = 1; ; attempt++)
        {
            HttpResponseMessage resp;
            await _operationGate.WaitAsync(ct);
            try
            {
                if (search)
                {
                    var wait = _lastSearchStartUtc + MinSearchSpacing - DateTime.UtcNow;
                    if (wait > TimeSpan.Zero) await Task.Delay(wait, ct);
                }
                resp = await SendAsync(HttpMethod.Post, url, new StringContent(body, Encoding.UTF8, "application/json"), ct);
                if (search) _lastSearchStartUtc = DateTime.UtcNow;
            }
            finally
            {
                _operationGate.Release();
            }
            if (resp.StatusCode != System.Net.HttpStatusCode.TooManyRequests || attempt > OperationRetries) return resp;
            resp.Dispose();
            await Task.Delay(SearchStartRetryDelay * attempt, ct);
        }
    }

    /// <summary>Reads the search's state until it has ended or <paramref name="until"/> passes.
    /// Returns the last state read, or null when none could be read.</summary>
    private async Task<SearchStatus?> WaitForEndAsync(string searchId, DateTime until, CancellationToken ct)
    {
        SearchStatus? last = null;
        while (Clock() < until)
        {
            await Task.Delay(SearchPollInterval, ct);
            last = await ReadSearchStatusAsync(searchId, ct) ?? last;
            if (last is { Ended: true }) break;
        }
        return last;
    }

    internal sealed record SearchStatus(string State, bool Ended, int ResponseCount);

    /// <summary>Whether slskd's state says a search ended at its limit: "Completed,
    /// FileLimitReached" or "Completed, ResponseLimitReached".</summary>
    internal static bool HitLimit(string? state) =>
        state is not null && state.Contains("LimitReached", StringComparison.OrdinalIgnoreCase);

    private async Task<SearchStatus?> ReadSearchStatusAsync(string searchId, CancellationToken ct)
    {
        try
        {
            using var resp = await SendAsync(HttpMethod.Get, $"{Base}/api/v0/searches/{searchId}", null, ct);
            return resp.IsSuccessStatusCode ? ParseSearchStatus(await resp.Content.ReadAsStringAsync(ct)) : null;
        }
        catch (Exception ex) when (!ct.IsCancellationRequested)
        {
            _logger.LogDebug("Soulseek search state read failed (transient): {Msg}", ex.Message);
            return null;
        }
    }

    /// <summary>
    /// One read of slskd's search record. Ended means endedAt is set, not that the state says
    /// Completed: slskd saves Completed the moment the network search stops, and the responses a
    /// moment later in the same save that sets endedAt. Reading on Completed alone can find the
    /// empty list from in between.
    /// </summary>
    internal static SearchStatus? ParseSearchStatus(string json)
    {
        using var doc = JsonDocument.Parse(json);
        var root = doc.RootElement;
        if (root.ValueKind != JsonValueKind.Object) return null;
        var state = root.TryGetProperty("state", out var s) && s.ValueKind == JsonValueKind.String ? s.GetString() ?? "" : "";
        var ended = root.TryGetProperty("endedAt", out var e) && e.ValueKind == JsonValueKind.String;
        var responses = root.TryGetProperty("responseCount", out var r) && r.ValueKind == JsonValueKind.Number
            && r.TryGetInt32(out var n) ? n : 0;
        return new SearchStatus(state, ended, responses);
    }

    private async Task CancelSearchAsync(string searchId)
    {
        try { using var _ = await SendAsync(HttpMethod.Put, $"{Base}/api/v0/searches/{searchId}", null, CancellationToken.None); }
        catch (Exception ex) { _logger.LogDebug("Soulseek search cancel failed: {Msg}", ex.Message); }
    }

    private async Task<List<SoulseekFileHit>> ReadResponsesAsync(string searchId, CancellationToken ct)
    {
        try
        {
            using var resp = await SendAsync(HttpMethod.Get, $"{Base}/api/v0/searches/{searchId}/responses", null, ct);
            return resp.IsSuccessStatusCode ? ParseResponses(await resp.Content.ReadAsStringAsync(ct)) : [];
        }
        catch (Exception ex) when (!ct.IsCancellationRequested)
        {
            _logger.LogWarning("Soulseek search responses could not be read: {Msg}", ex.Message);
            return [];
        }
    }

    /// <summary>
    /// Removes the search from slskd. One that has not ended is cancelled first: DELETE removes
    /// only the record, and the search would carry on asking the network for nobody. The short
    /// wait after the cancel lets slskd save the ended search before its record goes, so that
    /// save does not fail in slskd's log.
    /// </summary>
    private async Task CleanUpSearchAsync(string searchId, bool ended)
    {
        try
        {
            if (!ended)
            {
                await CancelSearchAsync(searchId);
                await WaitForEndAsync(searchId, Clock() + CancelGrace, CancellationToken.None);
            }
            using var _ = await SendAsync(HttpMethod.Delete, $"{Base}/api/v0/searches/{searchId}", null, CancellationToken.None);
        }
        catch (Exception ex) { _logger.LogDebug("Soulseek search cleanup failed: {Msg}", ex.Message); }
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
                bool? freeSlot = resp.TryGetProperty("hasFreeUploadSlot", out var fsEl)
                    && fsEl.ValueKind is JsonValueKind.True or JsonValueKind.False
                    ? fsEl.GetBoolean()
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
                        Extension = NormalizeExtension(ext, filename),
                        UploadSpeed = uploadSpeed,
                        QueueLength = queueLength,
                        HasFreeUploadSlot = freeSlot,
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
    /// The files in one folder of a peer's share (slskd asks the peer for that folder alone, not
    /// its whole share). Each comes back with its full remote path, ready to enqueue, and the
    /// queue and speed of <paramref name="from"/>, the search hit that pointed at the folder.
    /// Empty when the peer does not answer in time or will not list it.
    /// </summary>
    public async Task<List<SoulseekFileHit>> BrowseFolderAsync(SoulseekFileHit from, string directory, TimeSpan timeout,
        CancellationToken ct = default)
    {
        using var limit = CancellationTokenSource.CreateLinkedTokenSource(ct);
        limit.CancelAfter(timeout);
        try
        {
            using var resp = await SendOperationAsync(
                $"{Base}/api/v0/users/{Uri.EscapeDataString(from.Username)}/directory",
                JsonSerializer.Serialize(new { directory }), search: false, limit.Token);
            if (!resp.IsSuccessStatusCode)
            {
                _logger.LogInformation("slskd could not list {User}'s folder {Folder}: HTTP {Code}",
                    from.Username, directory, (int)resp.StatusCode);
                return [];
            }
            return ParseDirectory(await resp.Content.ReadAsStringAsync(limit.Token), from, directory);
        }
        catch (OperationCanceledException) when (!ct.IsCancellationRequested)
        {
            _logger.LogInformation("{User} did not list the folder {Folder} within {Seconds}s", from.Username, directory, timeout.TotalSeconds);
            return [];
        }
        catch (Exception ex) when (ex is HttpRequestException or JsonException)
        {
            _logger.LogInformation("Could not list {User}'s folder {Folder}: {Message}", from.Username, directory, ex.Message);
            return [];
        }
    }

    /// <summary>
    /// slskd's answer for one folder: a folder object, or a list of them, each with its files. A
    /// file's name is either its full remote path or its name alone, which is then put under the
    /// folder it was listed in.
    /// </summary>
    internal static List<SoulseekFileHit> ParseDirectory(string json, SoulseekFileHit from, string directory)
    {
        var hits = new List<SoulseekFileHit>();
        using var doc = JsonDocument.Parse(json);
        IEnumerable<JsonElement> folders = doc.RootElement.ValueKind switch
        {
            JsonValueKind.Array => doc.RootElement.EnumerateArray().ToList(),
            JsonValueKind.Object => [doc.RootElement],
            _ => [],
        };
        foreach (var folder in folders)
        {
            var name = folder.TryGetProperty("name", out var nameEl) && nameEl.ValueKind == JsonValueKind.String
                ? nameEl.GetString() : null;
            var where = string.IsNullOrWhiteSpace(name) ? directory : name!;
            if (!folder.TryGetProperty("files", out var files) || files.ValueKind != JsonValueKind.Array) continue;
            foreach (var file in files.EnumerateArray())
            {
                var filename = file.TryGetProperty("filename", out var fnEl) ? fnEl.GetString() : null;
                if (string.IsNullOrWhiteSpace(filename)) continue;
                if (filename.IndexOfAny(['\\', '/']) < 0) filename = where.TrimEnd('\\', '/') + "\\" + filename;
                int? Int(string key) => file.TryGetProperty(key, out var el) && el.ValueKind == JsonValueKind.Number ? el.GetInt32() : null;
                hits.Add(new SoulseekFileHit
                {
                    Username = from.Username,
                    Filename = filename,
                    Size = file.TryGetProperty("size", out var sizeEl) && sizeEl.ValueKind == JsonValueKind.Number ? sizeEl.GetInt64() : 0,
                    BitRate = Int("bitRate"),
                    SampleRate = Int("sampleRate"),
                    BitDepth = Int("bitDepth"),
                    Length = Int("length"),
                    Extension = NormalizeExtension(file.TryGetProperty("extension", out var exEl) ? exEl.GetString() : null, filename),
                    UploadSpeed = from.UploadSpeed,
                    QueueLength = from.QueueLength,
                    HasFreeUploadSlot = from.HasFreeUploadSlot,
                });
            }
        }
        return hits;
    }

    /// <summary>
    /// Reduce a file extension to the bare lowercase form ("flac").
    ///
    /// Candidate ranking accepts a hit by comparing this against the configured
    /// PreferredExtension, so the two have to agree on shape. slskd does not
    /// guarantee one: some builds report "flac", some report ".flac", and some
    /// omit the field entirely and leave only the filename to go on. An
    /// unnormalized leading dot compared against a bare "flac" matches nothing,
    /// which reads downstream as "this track is not on Soulseek" rather than as
    /// a parsing mismatch. Both sides of the comparison run through here.
    /// </summary>
    internal static string NormalizeExtension(string? extension, string filename)
    {
        var raw = string.IsNullOrWhiteSpace(extension) ? Path.GetExtension(filename) : extension;
        return (raw ?? "").Trim().TrimStart('.').ToLowerInvariant();
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

        using var resp = await SendOperationAsync(
            $"{Base}/api/v0/transfers/downloads/{Uri.EscapeDataString(username)}", body, search: false, ct);

        if (!resp.IsSuccessStatusCode)
        {
            var err = await resp.Content.ReadAsStringAsync(ct);
            throw new Exception($"slskd download enqueue failed: HTTP {(int)resp.StatusCode} {err}");
        }
    }

    /// <summary>
    /// Whether this slskd takes batch downloads, which is what lets each download land in a folder
    /// of Octo's choosing. Null until the first batch has been tried. True once one was accepted,
    /// and from then on an error is an error. False once slskd answered the batch route as if it
    /// did not know it, and from then on Octo enqueues the old way without asking again.
    /// </summary>
    internal bool? BatchesSupported { get; set; }

    /// <summary>
    /// Queues files from one peer as one slskd batch, all landing in <paramref name="destination"/>,
    /// a folder relative to slskd's downloads directory. The answer carries each file's transfer id,
    /// so the wait can follow that transfer rather than any transfer of the same file name.
    ///
    /// An slskd older than batches answers this route through its per-user enqueue (the user named
    /// "batches"), which rejects the body with 400, or with 404 or 405. Before any batch has worked,
    /// those mean "no batches here"; after one has, they are real errors.
    /// </summary>
    public async Task<BatchEnqueue> EnqueueBatchAsync(string username, IReadOnlyList<(string Filename, long Size)> files,
        string destination, CancellationToken ct = default)
    {
        if (BatchesSupported == false) return BatchEnqueue.NotSupported;
        using var resp = await SendOperationAsync($"{Base}/api/v0/transfers/downloads/batches",
            BatchPayload(username, files, destination), search: false, ct);
        var body = await resp.Content.ReadAsStringAsync(ct);
        if (resp.IsSuccessStatusCode)
        {
            BatchesSupported = true;
            return ParseBatch(body);
        }
        if (BatchesSupported != true && resp.StatusCode is System.Net.HttpStatusCode.BadRequest
                or System.Net.HttpStatusCode.NotFound or System.Net.HttpStatusCode.MethodNotAllowed)
        {
            BatchesSupported = false;
            _logger.LogInformation("slskd does not take batch downloads (HTTP {Code}); downloads go one at a time, the old way",
                (int)resp.StatusCode);
            return BatchEnqueue.NotSupported;
        }
        throw new Exception($"slskd batch enqueue failed: HTTP {(int)resp.StatusCode} {body}");
    }

    internal static string BatchPayload(string username, IReadOnlyList<(string Filename, long Size)> files, string destination) =>
        JsonSerializer.Serialize(new
        {
            id = Guid.NewGuid().ToString(),
            username,
            files = files.Select(file => new { filename = file.Filename, size = file.Size }),
            options = new { destination },
        });

    /// <summary>Reads a batch answer: batch.transfers carries what slskd queued, failures what it
    /// would not. Anything unreadable reads as nothing queued.</summary>
    internal static BatchEnqueue ParseBatch(string json)
    {
        var ids = new Dictionary<string, string>(StringComparer.Ordinal);
        var failures = new List<(string File, string Message)>();
        try
        {
            using var doc = JsonDocument.Parse(json);
            var root = doc.RootElement;
            if (TryGetPropertyIgnoreCase(root, "batch", out var batch)
                && TryGetPropertyIgnoreCase(batch, "transfers", out var transfers) && transfers.ValueKind == JsonValueKind.Array)
                foreach (var transfer in transfers.EnumerateArray())
                    if (transfer.TryGetProperty("filename", out var name) && name.ValueKind == JsonValueKind.String
                        && TransferId(transfer) is { } id)
                        ids[name.GetString()!] = id;
            if (TryGetPropertyIgnoreCase(root, "failures", out var failed) && failed.ValueKind == JsonValueKind.Array)
                foreach (var failure in failed.EnumerateArray())
                    failures.Add((
                        failure.TryGetProperty("filename", out var f) ? f.GetString() ?? "" : "",
                        failure.TryGetProperty("message", out var m) ? m.GetString() ?? "" : ""));
        }
        catch (JsonException) { }
        return new BatchEnqueue(true, ids, failures);
    }

    /// <summary>
    /// Polls a download to completion. Returns Succeeded on success or Errored
    /// on any kind of slskd-side failure (peer rejected, timed out, cancelled,
    /// or the transfer silently disappeared from slskd's active list — which
    /// happens after rejection on some slskd versions and would otherwise hang
    /// us forever). Caller decides whether to retry or escalate.
    ///
    /// <paramref name="onProgress"/> hears the transfer's byte counts on every poll that finds
    /// it. It only listens: the cadence, the deadline and the outcome are the same without it,
    /// and anything it throws is swallowed here.
    /// </summary>
    public async Task<SoulseekTransferState> WaitForCompletionAsync(string username, string filename, int? perAttemptTimeoutSeconds = null, CancellationToken ct = default,
        Action<SoulseekTransferProgress>? onProgress = null, string? transferId = null)
    {
        var timeoutSec = perAttemptTimeoutSeconds ?? _settings.DownloadTimeoutSeconds;
        var watch = new TransferWatch(Clock(), TimeSpan.FromSeconds(timeoutSec), MaxTransferTime);
        var seenAtLeastOnce = false;
        var consecutiveMisses = 0;
        // After we've seen the transfer at least once, missing it for this many
        // consecutive polls means slskd dropped it and we should give up. Some
        // slskd versions remove rejected transfers from the active-list endpoint
        // immediately, so without this we'd poll forever.
        const int MaxConsecutiveMissesAfterSeen = 6;  // ~9s at 1500ms cadence

        while (!watch.Expired(Clock()) && !ct.IsCancellationRequested)
        {
            await Task.Delay(PollInterval, ct);

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
                var transfer = FindTransfer(doc.RootElement, filename, transferId);
                var state = transfer is { } found ? StateOf(found) : null;
                bool foundThisPoll = state is not null;
                if (foundThisPoll)
                {
                    seenAtLeastOnce = true;
                    consecutiveMisses = 0;

                    var progress = ReadTransferProgress(transfer!.Value);
                    watch.Saw(progress.BytesTransferred, Clock());
                    if (onProgress is not null)
                    {
                        try { onProgress(progress); }
                        catch (Exception ex) { _logger.LogDebug("Transfer progress listener failed: {Msg}", ex.Message); }
                    }

                    if (state!.Contains("Completed", StringComparison.OrdinalIgnoreCase) &&
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

        ct.ThrowIfCancellationRequested();

        // Giving up on this peer. Without a cancel slskd keeps the transfer going, and
        // a file that lands after the next peer's copy is a second copy in the library.
        if (await CancelTransferAsync(username, filename, transferId) == SoulseekTransferState.Succeeded)
        {
            _logger.LogInformation("slskd transfer finished just as it was given up: {File}", filename);
            return SoulseekTransferState.Succeeded;
        }
        if (watch.HitCeiling(Clock()))
            _logger.LogWarning("slskd transfer still not done after {Min} minutes; cancelled: {File}",
                (int)MaxTransferTime.TotalMinutes, filename);
        else
            _logger.LogWarning("slskd transfer timed out: nothing new for {Sec}s; cancelled: {File}", timeoutSec, filename);
        return SoulseekTransferState.Errored;
    }

    /// <summary>
    /// The longest a transfer that keeps moving is waited for. A slow peer with the right
    /// file is worth waiting on; one that trickles for an hour is not.
    /// </summary>
    internal static readonly TimeSpan MaxTransferTime = TimeSpan.FromMinutes(60);

    /// <summary>How often a transfer is polled. Only tests shorten it.</summary>
    internal TimeSpan PollInterval { get; set; } = TimeSpan.FromMilliseconds(1500);

    /// <summary>The time a transfer's or a search's wait goes by. Only tests replace it.</summary>
    internal Func<DateTime> Clock { get; set; } = () => DateTime.UtcNow;

    /// <summary>How often a running search's state is read. Only tests shorten it.</summary>
    internal TimeSpan SearchPollInterval { get; set; } = TimeSpan.FromSeconds(1);

    /// <summary>How long a cancelled search is given to end and save its responses. slskd takes
    /// well under a second; the rest is slack for a busy disk.</summary>
    internal static readonly TimeSpan CancelGrace = TimeSpan.FromSeconds(5);

    /// <summary>The wait before the one retry of a start slskd refused with 429. Only tests shorten it.</summary>
    internal TimeSpan SearchStartRetryDelay { get; set; } = TimeSpan.FromSeconds(1);

    /// <summary>The latest search's background cleanup. Only tests await it.</summary>
    internal Task LastSearchCleanup { get; private set; } = Task.CompletedTask;

    /// <summary>
    /// Cancels a download in slskd and removes it from its list, so it can never land.
    /// Answers Succeeded instead when the transfer turns out to have just finished, and
    /// Errored otherwise, including when slskd cannot be asked.
    /// </summary>
    public async Task<SoulseekTransferState> CancelTransferAsync(string username, string filename, string? transferId = null)
    {
        var user = Uri.EscapeDataString(username);
        try
        {
            using var resp = await SendAsync(HttpMethod.Get, $"{Base}/api/v0/transfers/downloads/{user}", null, CancellationToken.None);
            if (!resp.IsSuccessStatusCode) return SoulseekTransferState.Errored;
            using var doc = JsonDocument.Parse(await resp.Content.ReadAsStringAsync());
            if (FindTransfer(doc.RootElement, filename, transferId) is not { } file) return SoulseekTransferState.Errored;
            var state = StateOf(file);
            if (state.Contains("Completed", StringComparison.OrdinalIgnoreCase) &&
                state.Contains("Succeeded", StringComparison.OrdinalIgnoreCase))
                return SoulseekTransferState.Succeeded;
            if (TransferId(file) is not { } id) return SoulseekTransferState.Errored;
            using var cancel = await SendAsync(HttpMethod.Delete,
                $"{Base}/api/v0/transfers/downloads/{user}/{Uri.EscapeDataString(id)}?remove=true", null, CancellationToken.None);
            if (!cancel.IsSuccessStatusCode)
                _logger.LogWarning("slskd refused to cancel {File}: HTTP {Code}", filename, (int)cancel.StatusCode);
        }
        catch (Exception ex)
        {
            _logger.LogWarning("Could not cancel slskd transfer {File}: {Msg}", filename, ex.Message);
        }
        return SoulseekTransferState.Errored;
    }

    /// <summary>The id slskd gives a transfer, for cancelling it.</summary>
    internal static string? TransferId(JsonElement file) =>
        file.TryGetProperty("id", out var id) && id.ValueKind == JsonValueKind.String ? id.GetString() : null;

    /// <summary>
    /// Finds a transfer's state in an slskd downloads response, or null when the
    /// file is not present. The per-user endpoint returns a single
    /// {username, directories} object, while the all-users endpoint returns an
    /// array of them; both shapes are accepted. Reading the wrong shape is what
    /// made every completed transfer look like a timeout: the poll loop rejected
    /// the object response wholesale and rode the per-attempt timer to the end.
    /// </summary>
    internal static string? FindTransferState(JsonElement root, string filename) =>
        FindTransfer(root, filename) is { } file ? StateOf(file) : null;

    private static string StateOf(JsonElement file) =>
        file.TryGetProperty("state", out var stEl) ? stEl.GetString() ?? "" : "";

    /// <summary>
    /// What slskd says a transfer has moved so far. Any field it leaves out, or sends as
    /// something other than a number, is null rather than zero, so a missing size never reads
    /// as a finished file.
    /// </summary>
    internal static SoulseekTransferProgress ReadTransferProgress(JsonElement file)
    {
        static long? Long(JsonElement el, string name) =>
            el.TryGetProperty(name, out var v) && v.ValueKind == JsonValueKind.Number && v.TryGetInt64(out var n)
                ? n : null;
        static double? Double(JsonElement el, string name) =>
            el.TryGetProperty(name, out var v) && v.ValueKind == JsonValueKind.Number && v.TryGetDouble(out var n)
                ? n : null;
        return new SoulseekTransferProgress(StateOf(file),
            Long(file, "bytesTransferred"), Long(file, "size"), Double(file, "percentComplete"));
    }

    /// <summary>The file object for a transfer, in either response shape, or null. With an id, only
    /// that transfer: an older transfer of the same file from the same peer may still be listed.</summary>
    internal static JsonElement? FindTransfer(JsonElement root, string filename, string? transferId = null)
    {
        IEnumerable<JsonElement> userGroups = root.ValueKind switch
        {
            JsonValueKind.Array => root.EnumerateArray(),
            JsonValueKind.Object => new[] { root },
            _ => Array.Empty<JsonElement>(),
        };

        foreach (var userGroup in userGroups)
        {
            if (userGroup.ValueKind != JsonValueKind.Object) continue;
            if (!userGroup.TryGetProperty("directories", out var dirs)) continue;
            if (dirs.ValueKind != JsonValueKind.Array) continue;
            foreach (var dir in dirs.EnumerateArray())
            {
                if (!dir.TryGetProperty("files", out var files)) continue;
                if (files.ValueKind != JsonValueKind.Array) continue;
                foreach (var file in files.EnumerateArray())
                {
                    if (transferId is not null)
                    {
                        if (TransferId(file) == transferId) return file;
                        continue;
                    }
                    var fn = file.TryGetProperty("filename", out var fnEl) ? fnEl.GetString() : null;
                    if (fn != filename) continue;
                    return file;
                }
            }
        }
        return null;
    }
}

/// <summary>What a batch enqueue came to. TransferIds maps each queued file to its transfer id;
/// Failures are the files slskd would not queue.</summary>
public sealed record BatchEnqueue(bool Supported, IReadOnlyDictionary<string, string> TransferIds,
    IReadOnlyList<(string File, string Message)> Failures)
{
    public static readonly BatchEnqueue NotSupported = new(false, new Dictionary<string, string>(), []);
}

/// <summary>A search's files, and whether slskd stopped it at its response or file limit.</summary>
public sealed record SoulseekSearchOutcome(List<SoulseekFileHit> Hits, bool HitLimit);

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

    /// <summary>The peer can start sending now rather than queueing us. Per response, like
    /// QueueLength, so every file one peer offers carries the same value.</summary>
    public bool? HasFreeUploadSlot { get; set; }
}

/// <summary>
/// How long to keep waiting on one transfer. Each time more bytes have arrived, the
/// quiet window starts again, so a slow peer that keeps sending is waited for. A
/// transfer with nothing new for the whole window, or still unfinished at the ceiling,
/// is given up on.
/// </summary>
internal sealed class TransferWatch(DateTime started, TimeSpan quiet, TimeSpan ceiling)
{
    private DateTime _quietSince = started;
    private long _bytes;

    public void Saw(long? bytes, DateTime now)
    {
        if (bytes is not { } b || b <= _bytes) return;
        _bytes = b;
        _quietSince = now;
    }

    public bool HitCeiling(DateTime now) => now - started >= ceiling;

    public bool Expired(DateTime now) => now - _quietSince >= quiet || HitCeiling(now);
}

/// <summary>One poll's view of a transfer. PercentComplete is slskd's own, from 0 to 100.</summary>
public sealed record SoulseekTransferProgress(
    string State, long? BytesTransferred, long? Size, double? PercentComplete)
{
    /// <summary>
    /// Bytes are flowing, or have. A transfer waiting in the peer's queue reads "Queued,
    /// Remotely" with nothing moved, and that is still a wait, not a download.
    /// </summary>
    public bool IsMoving =>
        BytesTransferred is > 0
        || State.Contains("InProgress", StringComparison.OrdinalIgnoreCase)
        || State.Contains("Succeeded", StringComparison.OrdinalIgnoreCase);
}

public enum SoulseekTransferState
{
    Succeeded,
    Errored
}
