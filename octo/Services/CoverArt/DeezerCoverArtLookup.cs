using System.Text.Json;
using Octo.Services.Soulseek;

namespace Octo.Services.CoverArt;

/// <summary>
/// Cover art via Deezer's public search API. No key, no auth, very broad
/// catalog including international/non-Western releases — fills the gaps
/// where iTunes' US-skewed catalog whiffs.
///
/// Endpoints used:
///   GET https://api.deezer.com/search?q=track:"Title" artist:"Artist"&limit=5
///   GET https://api.deezer.com/search/album?q=...                       (album mode)
///   GET https://api.deezer.com/search/artist?q=...                      (artist mode)
///
/// Hit shapes contain a nested album with cover_xl (1000x1000), cover_big (500),
/// cover_medium (250). We grab cover_xl for quality, falling back if absent.
/// </summary>
public class DeezerCoverArtLookup : ICoverArtSource
{
    private readonly HttpClient _http;
    private readonly ILogger<DeezerCoverArtLookup> _logger;

    public string Name => "deezer";

    public DeezerCoverArtLookup(IHttpClientFactory httpClientFactory, ILogger<DeezerCoverArtLookup> logger)
    {
        _http = httpClientFactory.CreateClient();
        _http.Timeout = TimeSpan.FromSeconds(8);
        _logger = logger;
    }

    public async Task<byte[]?> TryFetchAsync(SoulseekRouting routing, CancellationToken ct = default)
    {
        var artist = (routing.Artist ?? "").Trim();
        try
        {
            string? coverUrl = routing.Kind switch
            {
                RoutingKind.Album   => await ResolveAlbumCoverAsync(artist, (routing.Album ?? routing.Title ?? "").Trim(), ct),
                RoutingKind.Artist  => await ResolveArtistCoverAsync(artist, ct),
                _                   => await ResolveTrackCoverAsync(artist, (routing.Title ?? "").Trim(), ct),
            };
            if (string.IsNullOrEmpty(coverUrl)) return null;

            using var resp = await _http.GetAsync(coverUrl, ct);
            if (!resp.IsSuccessStatusCode) return null;
            return await resp.Content.ReadAsByteArrayAsync(ct);
        }
        catch (Exception ex)
        {
            _logger.LogDebug(ex, "deezer lookup failed for {Kind} {A}/{T}/{Al}",
                routing.Kind, routing.Artist, routing.Title, routing.Album);
            return null;
        }
    }

    private async Task<string?> ResolveTrackCoverAsync(string artist, string title, CancellationToken ct)
    {
        if (string.IsNullOrEmpty(artist) || string.IsNullOrEmpty(title)) return null;
        // Deezer's q= supports field-qualified queries like `artist:"X" track:"Y"` for
        // higher precision than a flat keyword query.
        var q = $"artist:\"{artist}\" track:\"{title}\"";
        var url = $"https://api.deezer.com/search?q={Uri.EscapeDataString(q)}&limit=5";
        var doc = await GetJsonAsync(url, ct);
        if (doc is null) return null;
        if (!doc.RootElement.TryGetProperty("data", out var data) || data.GetArrayLength() == 0)
            return null;
        return PickBestAlbumCover(data, artist);
    }

    private async Task<string?> ResolveAlbumCoverAsync(string artist, string album, CancellationToken ct)
    {
        if (string.IsNullOrEmpty(artist) || string.IsNullOrEmpty(album)) return null;
        var q = $"artist:\"{artist}\" album:\"{album}\"";
        var url = $"https://api.deezer.com/search/album?q={Uri.EscapeDataString(q)}&limit=5";
        var doc = await GetJsonAsync(url, ct);
        if (doc is null) return null;
        if (!doc.RootElement.TryGetProperty("data", out var data) || data.GetArrayLength() == 0)
        {
            // Fallback to a track-based search for "albums" that are really singles.
            return await ResolveTrackCoverAsync(artist, album, ct);
        }
        return PickBestDirectCover(data, artist);
    }

    private async Task<string?> ResolveArtistCoverAsync(string artist, CancellationToken ct)
    {
        if (string.IsNullOrEmpty(artist)) return null;
        var url = $"https://api.deezer.com/search/artist?q={Uri.EscapeDataString(artist)}&limit=5";
        var doc = await GetJsonAsync(url, ct);
        if (doc is null) return null;
        if (!doc.RootElement.TryGetProperty("data", out var data) || data.GetArrayLength() == 0)
            return null;
        // Artist endpoint returns picture_xl directly on each item.
        string? best = null;
        int bestScore = int.MinValue;
        foreach (var item in data.EnumerateArray())
        {
            var name = item.TryGetProperty("name", out var n) ? n.GetString() ?? "" : "";
            var pic = ReadString(item, "picture_xl") ?? ReadString(item, "picture_big") ?? ReadString(item, "picture");
            if (string.IsNullOrEmpty(pic)) continue;
            var score = ScoreNameMatch(artist, name);
            if (score > bestScore)
            {
                bestScore = score;
                best = pic;
            }
        }
        return best;
    }

    private async Task<JsonDocument?> GetJsonAsync(string url, CancellationToken ct)
    {
        using var resp = await _http.GetAsync(url, ct);
        if (!resp.IsSuccessStatusCode) return null;
        var json = await resp.Content.ReadAsStringAsync(ct);
        try { return JsonDocument.Parse(json); }
        catch { return null; }
    }

    /// <summary>Pick best track-result cover by artist scoring; reads from <c>album.cover_xl</c>.</summary>
    private static string? PickBestAlbumCover(JsonElement data, string expectedArtist)
    {
        string? best = null;
        int bestScore = int.MinValue;
        foreach (var item in data.EnumerateArray())
        {
            var artist = "";
            if (item.TryGetProperty("artist", out var artistEl)
                && artistEl.TryGetProperty("name", out var artistName))
                artist = artistName.GetString() ?? "";
            string? cover = null;
            if (item.TryGetProperty("album", out var albumEl))
            {
                cover = ReadString(albumEl, "cover_xl")
                     ?? ReadString(albumEl, "cover_big")
                     ?? ReadString(albumEl, "cover_medium");
            }
            if (string.IsNullOrEmpty(cover)) continue;
            var score = ScoreNameMatch(expectedArtist, artist);
            if (score > bestScore)
            {
                bestScore = score;
                best = cover;
            }
        }
        return best;
    }

    /// <summary>Pick best album-result cover by artist scoring; reads from <c>cover_xl</c> directly on the result.</summary>
    private static string? PickBestDirectCover(JsonElement data, string expectedArtist)
    {
        string? best = null;
        int bestScore = int.MinValue;
        foreach (var item in data.EnumerateArray())
        {
            var artist = "";
            if (item.TryGetProperty("artist", out var artistEl)
                && artistEl.TryGetProperty("name", out var artistName))
                artist = artistName.GetString() ?? "";
            var cover = ReadString(item, "cover_xl")
                     ?? ReadString(item, "cover_big")
                     ?? ReadString(item, "cover_medium");
            if (string.IsNullOrEmpty(cover)) continue;
            var score = ScoreNameMatch(expectedArtist, artist);
            if (score > bestScore)
            {
                bestScore = score;
                best = cover;
            }
        }
        return best;
    }

    private static string? ReadString(JsonElement obj, string prop)
        => obj.TryGetProperty(prop, out var p) && p.ValueKind == JsonValueKind.String
            ? p.GetString()
            : null;

    private static int ScoreNameMatch(string expected, string actual)
    {
        if (string.IsNullOrEmpty(actual)) return 0;
        var e = expected.Trim().ToLowerInvariant();
        var a = actual.Trim().ToLowerInvariant();
        if (a == e) return 100;
        if (a.Contains(e) || e.Contains(a)) return 60;
        var eTokens = e.Split(' ', StringSplitOptions.RemoveEmptyEntries);
        var aTokens = a.Split(' ', StringSplitOptions.RemoveEmptyEntries);
        return eTokens.Count(t => aTokens.Contains(t)) * 10;
    }
}
