using System.Text.Json;
using Microsoft.Extensions.Options;
using Octo.Models.Settings;
using Octo.Services.Soulseek;

namespace Octo.Services.CoverArt;

/// <summary>
/// Cover art via Last.fm's <c>track.getInfo</c> / <c>album.getInfo</c>. Uses
/// the API key Octo already has configured for radio. Coverage overlaps a lot
/// with iTunes/Deezer — included as the third fallback so we still get
/// something for tracks the other two whiffed (often live recordings, remixes,
/// or very recent releases that haven't been indexed yet by streaming APIs).
///
/// Notable wart: Last.fm <em>artist</em> images were officially deprecated in
/// 2019 — the API now returns a star-shaped placeholder for `artist.getInfo`
/// images. We don't bother calling that endpoint for artist routings.
/// </summary>
public class LastFmCoverArtLookup : ICoverArtSource
{
    private readonly HttpClient _http;
    private readonly LastFmSettings _settings;
    private readonly ILogger<LastFmCoverArtLookup> _logger;

    public string Name => "lastfm";

    private const string BaseUrl = "https://ws.audioscrobbler.com/2.0/";

    public LastFmCoverArtLookup(IHttpClientFactory httpClientFactory, IOptions<LastFmSettings> settings, ILogger<LastFmCoverArtLookup> logger)
    {
        _http = httpClientFactory.CreateClient();
        _http.Timeout = TimeSpan.FromSeconds(8);
        _settings = settings.Value;
        _logger = logger;
    }

    public async Task<byte[]?> TryFetchAsync(SoulseekRouting routing, CancellationToken ct = default)
    {
        if (string.IsNullOrEmpty(_settings.ApiKey)) return null;
        var artist = (routing.Artist ?? "").Trim();
        if (string.IsNullOrEmpty(artist)) return null;

        try
        {
            string? imgUrl = routing.Kind switch
            {
                RoutingKind.Album   => await GetAlbumImageUrlAsync(artist, (routing.Album ?? routing.Title ?? "").Trim(), ct),
                RoutingKind.Artist  => null,  // Last.fm artist.getInfo returns the placeholder star, never a real image
                _                   => await GetTrackImageUrlAsync(artist, (routing.Title ?? "").Trim(), ct),
            };
            if (string.IsNullOrEmpty(imgUrl)) return null;

            using var resp = await _http.GetAsync(imgUrl, ct);
            if (!resp.IsSuccessStatusCode) return null;
            return await resp.Content.ReadAsByteArrayAsync(ct);
        }
        catch (Exception ex)
        {
            _logger.LogDebug(ex, "lastfm lookup failed for {A}/{T}/{Al}", routing.Artist, routing.Title, routing.Album);
            return null;
        }
    }

    private async Task<string?> GetTrackImageUrlAsync(string artist, string title, CancellationToken ct)
    {
        if (string.IsNullOrEmpty(title)) return null;
        var url = $"{BaseUrl}?method=track.getInfo&artist={Uri.EscapeDataString(artist)}&track={Uri.EscapeDataString(title)}&api_key={_settings.ApiKey}&format=json&autocorrect=1";
        return await PickImageFromResponseAsync(url, "track", "album", ct);
    }

    private async Task<string?> GetAlbumImageUrlAsync(string artist, string album, CancellationToken ct)
    {
        if (string.IsNullOrEmpty(album)) return null;
        var url = $"{BaseUrl}?method=album.getInfo&artist={Uri.EscapeDataString(artist)}&album={Uri.EscapeDataString(album)}&api_key={_settings.ApiKey}&format=json&autocorrect=1";
        // The album response has the image array directly under <album>.
        var direct = await PickImageFromResponseAsync(url, "album", null, ct);
        if (!string.IsNullOrEmpty(direct)) return direct;
        // Singles often don't have an album entry — fall back to track-level lookup
        // using the album name as if it were the title.
        return await GetTrackImageUrlAsync(artist, album, ct);
    }

    /// <summary>
    /// Walks the Last.fm response to find the best image URL. Path differs
    /// depending on which endpoint we hit:
    ///   track.getInfo  → response.track.album.image[]
    ///   album.getInfo  → response.album.image[]
    /// The image[] array has entries with sizes "small"/"medium"/"large"/
    /// "extralarge"/"mega"; we pick the largest non-empty one.
    /// </summary>
    private async Task<string?> PickImageFromResponseAsync(string url, string outerKey, string? innerKey, CancellationToken ct)
    {
        using var resp = await _http.GetAsync(url, ct);
        if (!resp.IsSuccessStatusCode) return null;
        var json = await resp.Content.ReadAsStringAsync(ct);
        using var doc = JsonDocument.Parse(json);

        if (!doc.RootElement.TryGetProperty(outerKey, out var outer)) return null;
        var imageHost = outer;
        if (innerKey != null)
        {
            if (!outer.TryGetProperty(innerKey, out var inner)) return null;
            imageHost = inner;
        }
        if (!imageHost.TryGetProperty("image", out var images) || images.ValueKind != JsonValueKind.Array)
            return null;

        var sizeRanking = new Dictionary<string, int>
        {
            ["mega"] = 5, ["extralarge"] = 4, ["large"] = 3, ["medium"] = 2, ["small"] = 1
        };
        string? best = null;
        int bestScore = -1;
        foreach (var img in images.EnumerateArray())
        {
            var size = img.TryGetProperty("size", out var s) ? s.GetString() ?? "" : "";
            var text = img.TryGetProperty("#text", out var t) ? t.GetString() ?? "" : "";
            if (string.IsNullOrEmpty(text)) continue;
            // Last.fm's "deprecated artist image" placeholder is hosted at
            // /i/u/2a96cbd8b46e442fc41c2b86b821562f.png — explicitly skip it
            // so we don't return a star icon as cover art.
            if (text.Contains("2a96cbd8b46e442fc41c2b86b821562f", StringComparison.Ordinal)) continue;
            var rank = sizeRanking.TryGetValue(size, out var r) ? r : 0;
            if (rank > bestScore)
            {
                bestScore = rank;
                best = text;
            }
        }
        return best;
    }
}
