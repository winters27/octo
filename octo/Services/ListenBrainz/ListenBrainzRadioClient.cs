using System.Net.Http.Headers;
using System.Text.Json;
using Microsoft.Extensions.Caching.Memory;
using Microsoft.Extensions.Options;
using Octo.Models.Settings;

namespace Octo.Services.ListenBrainz;

/// <summary>A song ListenBrainz suggested.</summary>
public sealed record ListenBrainzTrack(string Artist, string Title, double Score, int? DurationSeconds);

/// <summary>
/// ListenBrainz's listening data for radio: the labs datasets (no token, at most one call a
/// second as its docs ask) and LB Radio (a user token since 2026-08-21).
/// </summary>
public sealed class ListenBrainzRadioClient(IHttpClientFactory http, IOptionsMonitor<ListenBrainzSettings> listenBrainz,
    ILogger<ListenBrainzRadioClient> logger) : IDisposable
{
    public const string ClientName = "listenbrainz-radio";
    internal const string LabsUrl = "https://labs.api.listenbrainz.org";
    internal const string ApiUrl = "https://api.listenbrainz.org";
    internal static TimeSpan MinimumGap { get; set; } = TimeSpan.FromSeconds(1);
    private readonly SemaphoreSlim _gate = new(1, 1);
    private DateTime _last = DateTime.MinValue;
    private readonly MemoryCache _cache = new(new MemoryCacheOptions { SizeLimit = 4000 });

    public bool HasToken => !string.IsNullOrWhiteSpace(listenBrainz.CurrentValue.Token);

    /// <summary>The recording MusicBrainz knows for an artist and title, by ListenBrainz's near-exact lookup.</summary>
    public async Task<string?> RecordingMbidAsync(string artist, string title, CancellationToken ct)
    {
        var key = $"acr|{artist}|{title}".ToLowerInvariant();
        if (_cache.TryGetValue(key, out string? cached)) return cached;
        var rows = await LabsAsync($"/acr-lookup/json?artist_credit_name={Uri.EscapeDataString(artist)}&recording_name={Uri.EscapeDataString(title)}", ct);
        var mbid = rows.Select(row => Text(row, "recording_mbid")).FirstOrDefault(value => value is not null);
        _cache.Set(key, mbid, new MemoryCacheEntryOptions { Size = 1, AbsoluteExpirationRelativeToNow = TimeSpan.FromDays(7) });
        return mbid;
    }

    public async Task<IReadOnlyList<ListenBrainzTrack>> SimilarRecordingsAsync(string recordingMbid,
        string algorithm, int count, CancellationToken ct)
    {
        var key = $"similar|{recordingMbid}|{algorithm}";
        if (!_cache.TryGetValue(key, out IReadOnlyList<ListenBrainzTrack>? tracks) || tracks is null)
        {
            var rows = await LabsAsync($"/similar-recordings/json?recording_mbids={Uri.EscapeDataString(recordingMbid)}&algorithm={Uri.EscapeDataString(algorithm)}", ct);
            var parsed = rows.Select(row => (Artist: Text(row, "artist_credit_name"), Title: Text(row, "recording_name"),
                    Score: row.TryGetProperty("score", out var s) && s.TryGetDouble(out var value) ? value : 0))
                .Where(row => row.Artist is not null && row.Title is not null).ToList();
            // ListenBrainz's score is a count of shared listening sessions (216, 149, ...), so it
            // is scaled to the answer's best, 0 to 1, like every other source's match.
            var best = parsed.Select(row => row.Score).DefaultIfEmpty(0).Max();
            tracks = parsed.Select(row => new ListenBrainzTrack(row.Artist!, row.Title!,
                best > 0 ? row.Score / best : 1.0, null)).ToList();
            // ListenBrainz rebuilds these every Sunday.
            _cache.Set(key, tracks, new MemoryCacheEntryOptions { Size = 1, AbsoluteExpirationRelativeToNow = TimeSpan.FromDays(1) });
        }
        return tracks.Take(count).ToList();
    }

    /// <summary>LB Radio for a prompt ("artist:(Kordhell)", "tag:(phonk)"); nothing without a token.</summary>
    public async Task<IReadOnlyList<ListenBrainzTrack>> LbRadioAsync(string prompt, int count, CancellationToken ct)
    {
        var token = listenBrainz.CurrentValue.Token;
        if (string.IsNullOrWhiteSpace(token)) return [];
        var key = $"lbradio|{prompt}";
        if (_cache.TryGetValue(key, out IReadOnlyList<ListenBrainzTrack>? cached) && cached is not null)
            return cached.Take(count).ToList();
        using var request = new HttpRequestMessage(HttpMethod.Get,
            $"{ApiUrl}/1/explore/lb-radio?prompt={Uri.EscapeDataString(prompt)}&mode=easy");
        request.Headers.Authorization = new AuthenticationHeaderValue("Token", token.Trim());
        using var doc = await SendAsync(request, ct);
        var tracks = new List<ListenBrainzTrack>();
        if (doc is not null && doc.RootElement.TryGetProperty("payload", out var payload)
            && payload.TryGetProperty("jspf", out var jspf) && jspf.TryGetProperty("playlist", out var playlist)
            && playlist.TryGetProperty("track", out var list) && list.ValueKind == JsonValueKind.Array)
        {
            foreach (var track in list.EnumerateArray())
            {
                var title = Text(track, "title");
                var artist = Text(track, "creator");
                int? seconds = track.TryGetProperty("duration", out var d) && d.TryGetInt64(out var ms) && ms > 1000
                    ? (int)(ms / 1000) : null;
                if (title is not null && artist is not null)
                    tracks.Add(new ListenBrainzTrack(artist, title, 1.0, seconds));
            }
        }
        _cache.Set(key, (IReadOnlyList<ListenBrainzTrack>)tracks,
            new MemoryCacheEntryOptions { Size = 1, AbsoluteExpirationRelativeToNow = TimeSpan.FromHours(6) });
        return tracks.Take(count).ToList();
    }

    private async Task<IReadOnlyList<JsonElement>> LabsAsync(string pathAndQuery, CancellationToken ct)
    {
        using var request = new HttpRequestMessage(HttpMethod.Get, LabsUrl + pathAndQuery);
        using var doc = await SendAsync(request, ct);
        return doc is null ? [] : Rows(doc.RootElement).Select(row => row.Clone()).ToList();
    }

    /// <summary>Rows as the labs datasets give them: an array of rows, or blocks holding a "data" array of rows.</summary>
    internal static IEnumerable<JsonElement> Rows(JsonElement element)
    {
        if (element.ValueKind == JsonValueKind.Array)
        {
            foreach (var item in element.EnumerateArray())
                foreach (var row in Rows(item)) yield return row;
        }
        else if (element.ValueKind == JsonValueKind.Object)
        {
            if (element.TryGetProperty("data", out var data)) { foreach (var row in Rows(data)) yield return row; }
            else yield return element;
        }
    }

    private async Task<JsonDocument?> SendAsync(HttpRequestMessage request, CancellationToken ct)
    {
        await _gate.WaitAsync(ct);
        try
        {
            var wait = _last + MinimumGap - DateTime.UtcNow;
            if (wait > TimeSpan.Zero) await Task.Delay(wait, ct);
            _last = DateTime.UtcNow;
            using var response = await http.CreateClient(ClientName).SendAsync(request, ct);
            if ((int)response.StatusCode == 429)
            {
                logger.LogWarning("ListenBrainz asked Octo to slow down ({Path})", request.RequestUri?.AbsolutePath);
                _last = DateTime.UtcNow + TimeSpan.FromSeconds(10);
                return null;
            }
            if (!response.IsSuccessStatusCode) return null;
            return JsonDocument.Parse(await response.Content.ReadAsStringAsync(ct));
        }
        catch (Exception ex) when (ex is not OperationCanceledException)
        {
            logger.LogDebug(ex, "ListenBrainz {Path} failed", request.RequestUri?.AbsolutePath);
            return null;
        }
        finally { _gate.Release(); }
    }

    private static string? Text(JsonElement element, string name) =>
        element.TryGetProperty(name, out var value) && value.ValueKind == JsonValueKind.String
            && value.GetString() is { Length: > 0 } text ? text : null;

    public void Dispose() { _cache.Dispose(); _gate.Dispose(); }
}
