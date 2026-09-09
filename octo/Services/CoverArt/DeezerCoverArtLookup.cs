using System.Text.Json;
using Microsoft.Extensions.Options;
using Octo.Models.Settings;
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

    public DeezerCoverArtLookup(IHttpClientFactory httpClientFactory,
        IOptions<MetadataSettings> metadataSettings, ILogger<DeezerCoverArtLookup> logger)
    {
        // Named so API calls go through the shared Deezer rate limiter. This same client
        // also fetches the image bytes from the CDN, which the handler leaves unmetered.
        _http = httpClientFactory.CreateClient(Octo.Services.Metadata.DeezerRateLimiter.ClientName);
        _http.Timeout = TimeSpan.FromSeconds(8);
        Octo.Services.Metadata.AcceptLanguageHeader.Apply(_http, metadataSettings.Value);
        _logger = logger;
    }

    public async Task<byte[]?> TryFetchAsync(SoulseekRouting routing, bool background = false, CancellationToken ct = default)
    {
        var artist = (routing.Artist ?? "").Trim();
        try
        {
            string? coverUrl = routing.Kind switch
            {
                RoutingKind.Album   => await ResolveAlbumCoverAsync(artist, (routing.Album ?? routing.Title ?? "").Trim(), background, ct),
                RoutingKind.Artist  => await ResolveArtistCoverAsync(artist, background, ct),
                _                   => await ResolveTrackCoverAsync(artist, (routing.Title ?? "").Trim(), background, ct),
            };
            if (string.IsNullOrEmpty(coverUrl)) return null;

            using var resp = await SendAsync(coverUrl, background, ct);
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

    private async Task<string?> ResolveTrackCoverAsync(string artist, string title, bool background, CancellationToken ct)
    {
        if (string.IsNullOrEmpty(artist) || string.IsNullOrEmpty(title)) return null;
        // Deezer's q= supports field-qualified queries like `artist:"X" track:"Y"` for
        // higher precision than a flat keyword query.
        var q = $"artist:\"{artist}\" track:\"{title}\"";
        var url = $"https://api.deezer.com/search?q={Uri.EscapeDataString(q)}&limit=5";
        var doc = await GetJsonAsync(url, background, ct);
        if (doc is null) return null;
        if (!doc.RootElement.TryGetProperty("data", out var data) || data.GetArrayLength() == 0)
            return null;
        return PickBestAlbumCover(data, artist);
    }

    private async Task<string?> ResolveAlbumCoverAsync(string artist, string album, bool background, CancellationToken ct)
    {
        if (string.IsNullOrEmpty(artist) || string.IsNullOrEmpty(album)) return null;
        var q = $"artist:\"{artist}\" album:\"{album}\"";
        var url = $"https://api.deezer.com/search/album?q={Uri.EscapeDataString(q)}&limit=5";
        var doc = await GetJsonAsync(url, background, ct);
        if (doc is null) return null;
        if (!doc.RootElement.TryGetProperty("data", out var data) || data.GetArrayLength() == 0)
        {
            // Fallback to a track-based search for "albums" that are really singles.
            return await ResolveTrackCoverAsync(artist, album, background, ct);
        }
        return PickBestDirectCover(data, artist);
    }

    private async Task<string?> ResolveArtistCoverAsync(string artist, bool background, CancellationToken ct)
    {
        if (string.IsNullOrEmpty(artist)) return null;
        var url = $"https://api.deezer.com/search/artist?q={Uri.EscapeDataString(artist)}&limit=5";
        var doc = await GetJsonAsync(url, background, ct);
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

    private async Task<JsonDocument?> GetJsonAsync(string url, bool background, CancellationToken ct)
    {
        using var resp = await SendAsync(url, background, ct);
        if (!resp.IsSuccessStatusCode) return null;
        var json = await resp.Content.ReadAsStringAsync(ct);
        try { return JsonDocument.Parse(json); }
        catch { return null; }
    }

    /// <summary>
    /// GETs through the shared Deezer client, marking the request for the background
    /// rate-limit lane when this call is a prewarm. Only api.deezer.com is metered by
    /// <see cref="Octo.Services.Metadata.DeezerRateLimitHandler"/>, so setting this option
    /// on a CDN image request is harmless but pointless; done uniformly for simplicity.
    /// </summary>
    private Task<HttpResponseMessage> SendAsync(string url, bool background, CancellationToken ct)
    {
        var req = new HttpRequestMessage(HttpMethod.Get, url);
        req.Options.Set(Octo.Services.Metadata.DeezerRateLimitHandler.BackgroundLane, background);
        return _http.SendAsync(req, ct);
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
