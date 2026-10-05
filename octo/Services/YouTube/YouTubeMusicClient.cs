using System.Text.Json;

namespace Octo.Services.YouTube;

/// <summary>One row of YouTube Music, as the shim reads it through ytmusicapi.</summary>
/// <param name="VideoType">MUSIC_VIDEO_TYPE_ATV (a release), OMV (official video), UGC (an
/// upload) or PODCAST_EPISODE; null when YouTube Music did not say.</param>
public sealed record YtmRow(string VideoId, string Title, IReadOnlyList<string> Artists,
    string? Album, int? DurationSeconds, string? VideoType);

/// <summary>YouTube Music search and radio, through the yt-dlp shim's /ytm endpoints.</summary>
public sealed class YouTubeMusicClient
{
    private readonly IHttpClientFactory _http;
    private readonly string? _shimUrl;
    private readonly ILogger<YouTubeMusicClient> _logger;

    public YouTubeMusicClient(IHttpClientFactory http, IConfiguration config, ILogger<YouTubeMusicClient> logger)
    {
        _http = http;
        // Read once, like YouTubeResolver: the shim's address changes only with a restart.
        _shimUrl = config["YouTube:ShimUrl"]?.TrimEnd('/');
        _logger = logger;
    }

    public bool Configured => !string.IsNullOrEmpty(_shimUrl);

    /// <param name="filter">"songs" for releases, "videos" for uploads.</param>
    public Task<IReadOnlyList<YtmRow>> SearchAsync(string query, string filter, CancellationToken ct) =>
        GetRowsAsync($"/ytm/search?q={Uri.EscapeDataString(query)}&filter={filter}&limit=10", ct);

    public Task<IReadOnlyList<YtmRow>> RadioAsync(string videoId, int limit, CancellationToken ct) =>
        GetRowsAsync($"/ytm/radio?videoId={Uri.EscapeDataString(videoId)}&limit={limit}", ct);

    /// <summary>The radio YouTube Music offers for an artist, found by name, and the name it found.</summary>
    public async Task<(string? Artist, IReadOnlyList<YtmRow> Rows)> ArtistRadioAsync(string artist, int limit, CancellationToken ct)
    {
        string? found = null;
        var rows = await GetRowsAsync($"/ytm/artist-radio?q={Uri.EscapeDataString(artist)}&limit={limit}", ct,
            root => found = root.TryGetProperty("artist", out var a) && a.ValueKind == JsonValueKind.String ? a.GetString() : null);
        return (found, rows);
    }

    private async Task<IReadOnlyList<YtmRow>> GetRowsAsync(string pathAndQuery, CancellationToken ct,
        Action<JsonElement>? read = null)
    {
        if (!Configured) return [];
        var client = _http.CreateClient("yt-dlp-shim-search");
        using var response = await client.GetAsync(_shimUrl + pathAndQuery, ct);
        if (!response.IsSuccessStatusCode)
        {
            _logger.LogDebug("YouTube Music {Path} answered {Status}", pathAndQuery.Split('?')[0], (int)response.StatusCode);
            return [];
        }
        using var doc = JsonDocument.Parse(await response.Content.ReadAsStringAsync(ct));
        read?.Invoke(doc.RootElement);
        return Rows(doc.RootElement);
    }

    internal static IReadOnlyList<YtmRow> Rows(JsonElement root)
    {
        if (!root.TryGetProperty("tracks", out var tracks) || tracks.ValueKind != JsonValueKind.Array) return [];
        var rows = new List<YtmRow>();
        foreach (var row in tracks.EnumerateArray())
        {
            var id = Text(row, "videoId");
            var title = Text(row, "title");
            if (id is null || title is null) continue;
            var artists = row.TryGetProperty("artists", out var list) && list.ValueKind == JsonValueKind.Array
                ? list.EnumerateArray().Where(item => item.ValueKind == JsonValueKind.String)
                    .Select(item => item.GetString()!).Where(name => name.Length > 0).ToList()
                : [];
            int? seconds = row.TryGetProperty("durationSeconds", out var d) && d.ValueKind == JsonValueKind.Number
                ? d.GetInt32() : null;
            rows.Add(new YtmRow(id, title, artists, Text(row, "album"), seconds, Text(row, "videoType")));
        }
        return rows;
    }

    private static string? Text(JsonElement element, string name) =>
        element.TryGetProperty(name, out var value) && value.ValueKind == JsonValueKind.String
            && value.GetString() is { Length: > 0 } text ? text : null;
}
