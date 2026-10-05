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
    /// <summary>Longest a call waits for its one-a-second slot. Past that it gives up with nothing,
    /// which is not a failure: a station build's calls queued ahead are not the source going wrong,
    /// and song radio must not wait out its whole timeout behind them.</summary>
    internal static TimeSpan MaxQueueWait { get; set; } = TimeSpan.FromSeconds(3);
    private readonly object _slotLock = new();
    private DateTime _next = DateTime.MinValue;
    private readonly MemoryCache _cache = new(new MemoryCacheOptions { SizeLimit = 4000 });

    public bool HasToken => !string.IsNullOrWhiteSpace(listenBrainz.CurrentValue.Token);

    /// <summary>The recording MusicBrainz knows for an artist and title, by ListenBrainz's near-exact lookup.</summary>
    public async Task<string?> RecordingMbidAsync(string artist, string title, CancellationToken ct)
    {
        var key = $"acr|{artist}|{title}".ToLowerInvariant();
        if (_cache.TryGetValue(key, out string? cached)) return cached;
        var rows = await LabsAsync($"/acr-lookup/json?artist_credit_name={Uri.EscapeDataString(artist)}&recording_name={Uri.EscapeDataString(title)}", ct);
        var mbid = rows?.Select(row => Text(row, "recording_mbid")).FirstOrDefault(value => value is not null);
        // Only a real answer is kept: a failed or skipped call says nothing about the song.
        if (rows is not null)
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
            if (rows is null) return [];
            var parsed = rows.Select(row => (Artist: Text(row, "artist_credit_name"), Title: Text(row, "recording_name"),
                    Score: Number(row, "score") ?? 0))
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
        if (doc is null) return [];
        var tracks = new List<ListenBrainzTrack>();
        if (doc.RootElement.TryGetProperty("payload", out var payload)
            && payload.TryGetProperty("jspf", out var jspf) && jspf.TryGetProperty("playlist", out var playlist)
            && playlist.TryGetProperty("track", out var list) && list.ValueKind == JsonValueKind.Array)
        {
            foreach (var track in list.EnumerateArray())
            {
                var title = Text(track, "title");
                var artist = Text(track, "creator");
                int? seconds = Number(track, "duration") is { } ms && ms > 1000 ? (int)(ms / 1000) : null;
                if (title is not null && artist is not null)
                    tracks.Add(new ListenBrainzTrack(artist, title, 1.0, seconds));
            }
        }
        _cache.Set(key, (IReadOnlyList<ListenBrainzTrack>)tracks,
            new MemoryCacheEntryOptions { Size = 1, AbsoluteExpirationRelativeToNow = TimeSpan.FromHours(6) });
        return tracks.Take(count).ToList();
    }

    /// <summary>The rows of a labs answer; null when there was no answer to read (failed or skipped).</summary>
    private async Task<IReadOnlyList<JsonElement>?> LabsAsync(string pathAndQuery, CancellationToken ct)
    {
        using var request = new HttpRequestMessage(HttpMethod.Get, LabsUrl + pathAndQuery);
        using var doc = await SendAsync(request, ct);
        return doc is null ? null : Rows(doc.RootElement).Select(row => row.Clone()).ToList();
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

    /// <summary>The wait for this call's one-a-second slot, the slot taken; null when it is more
    /// than <see cref="MaxQueueWait"/> away. Only the slot is under the lock, never the call.</summary>
    private TimeSpan? ReserveSlot()
    {
        lock (_slotLock)
        {
            var now = DateTime.UtcNow;
            var slot = _next > now ? _next : now;
            if (slot - now > MaxQueueWait) return null;
            _next = slot + MinimumGap;
            return slot - now;
        }
    }

    /// <summary>An answer, or null when the call failed, was refused or was skipped as too far back in the queue.</summary>
    private async Task<JsonDocument?> SendAsync(HttpRequestMessage request, CancellationToken ct)
    {
        if (ReserveSlot() is not { } wait)
        {
            logger.LogDebug("ListenBrainz is busy; {Path} skipped", request.RequestUri?.AbsolutePath);
            return null;
        }
        if (wait > TimeSpan.Zero) await Task.Delay(wait, ct);
        try
        {
            using var response = await http.CreateClient(ClientName).SendAsync(request, ct);
            if ((int)response.StatusCode == 429)
            {
                logger.LogWarning("ListenBrainz asked Octo to slow down ({Path})", request.RequestUri?.AbsolutePath);
                lock (_slotLock)
                {
                    var later = DateTime.UtcNow + TimeSpan.FromSeconds(10);
                    if (later > _next) _next = later;
                }
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
    }

    /// <summary>A number, or null for a missing, null or text value: one odd row never fails the answer.</summary>
    private static double? Number(JsonElement element, string name) =>
        element.TryGetProperty(name, out var value) && value.ValueKind == JsonValueKind.Number
            && value.TryGetDouble(out var number) ? number : null;

    private static string? Text(JsonElement element, string name) =>
        element.TryGetProperty(name, out var value) && value.ValueKind == JsonValueKind.String
            && value.GetString() is { Length: > 0 } text ? text : null;

    public void Dispose() => _cache.Dispose();
}
