using System.Text.Json;
using Octo.Services.Soulseek;

namespace Octo.Services.CoverArt;

/// <summary>
/// Cover art via Apple's free iTunes Search API. No key, ~600x600 JPEGs after
/// CDN substitution, very high coverage for mainstream Western releases —
/// weaker for international, indie, and underground.
///
/// Improvements over the original implementation:
/// - <c>country</c> filter dropped: US-only filtering kept missing releases that
///   are non-US-exclusive (lots of UK/EU/JP/KR catalog and re-releases).
/// - <c>limit</c> raised to 5: we then pick the result whose artist string
///   matches the routing best, instead of trusting iTunes' first hit blindly.
///   The first hit is often a "Karaoke Version" or different-artist cover that
///   shares the title.
/// - For routings whose Album is just the song title (the "single" convention
///   we use for placeholder songs), fall back from entity=album to entity=song
///   when the album-style query whiffs.
/// </summary>
public class ITunesCoverArtLookup : ICoverArtSource
{
    private readonly HttpClient _http;
    private readonly ILogger<ITunesCoverArtLookup> _logger;

    public string Name => "itunes";

    public ITunesCoverArtLookup(IHttpClientFactory httpClientFactory, ILogger<ITunesCoverArtLookup> logger)
    {
        _http = httpClientFactory.CreateClient();
        _http.Timeout = TimeSpan.FromSeconds(8);
        _logger = logger;
    }

    public async Task<byte[]?> TryFetchAsync(SoulseekRouting routing, CancellationToken ct = default)
    {
        var artist = (routing.Artist ?? "").Trim();
        if (routing.Kind == RoutingKind.Album)
        {
            var album = (routing.Album ?? routing.Title ?? "").Trim();
            if (string.IsNullOrEmpty(artist) || string.IsNullOrEmpty(album)) return null;

            // Album-style lookup. For our placeholder "singles" the album is the
            // song title — iTunes may return a song-level hit shaped like an
            // album anyway, or fall through to song-entity fallback below.
            var hit = await SearchAndScoreAsync($"{artist} {album}", "album", artist, ct);
            if (hit != null) return await DownloadHiResAsync(hit, ct);

            return await SearchSongFallbackAsync(artist, album, ct);
        }
        if (routing.Kind == RoutingKind.Artist)
        {
            if (string.IsNullOrEmpty(artist)) return null;
            var hit = await SearchAndScoreAsync(artist, "musicArtist", artist, ct);
            return hit is null ? null : await DownloadHiResAsync(hit, ct);
        }
        // Song
        var title = (routing.Title ?? "").Trim();
        if (string.IsNullOrEmpty(artist) || string.IsNullOrEmpty(title)) return null;
        var songHit = await SearchAndScoreAsync($"{artist} {title}", "song", artist, ct);
        return songHit is null ? null : await DownloadHiResAsync(songHit, ct);
    }

    /// <summary>
    /// Issue a search and rank the up-to-5 results by closeness of the artist
    /// match, returning the artworkUrl100 of the best one. Without scoring,
    /// "Drake — Hold On, We're Going Home" would frequently come back as the
    /// karaoke version's cover when it appeared first in the index.
    /// </summary>
    private async Task<string?> SearchAndScoreAsync(string term, string entity, string expectedArtist, CancellationToken ct)
    {
        try
        {
            var url = $"https://itunes.apple.com/search?term={Uri.EscapeDataString(term)}&entity={entity}&limit=5";
            using var resp = await _http.GetAsync(url, ct);
            if (!resp.IsSuccessStatusCode) return null;

            var json = await resp.Content.ReadAsStringAsync(ct);
            using var doc = JsonDocument.Parse(json);
            if (!doc.RootElement.TryGetProperty("results", out var results) || results.GetArrayLength() == 0)
                return null;

            string? bestUrl = null;
            int bestScore = int.MinValue;
            foreach (var item in results.EnumerateArray())
            {
                var artist = item.TryGetProperty("artistName", out var a) ? a.GetString() ?? "" : "";
                var artwork = item.TryGetProperty("artworkUrl100", out var aw) ? aw.GetString() : null;
                if (string.IsNullOrEmpty(artwork)) continue;

                var score = ScoreArtistMatch(expectedArtist, artist);
                if (score > bestScore)
                {
                    bestScore = score;
                    bestUrl = artwork;
                }
            }
            return bestUrl;
        }
        catch (Exception ex)
        {
            _logger.LogDebug(ex, "iTunes search failed for '{Term}'", term);
            return null;
        }
    }

    /// <summary>
    /// Singles often have no proper "album" entry on iTunes — the song exists
    /// but only as a track. Re-query with entity=song so we still get cover art.
    /// </summary>
    private async Task<byte[]?> SearchSongFallbackAsync(string artist, string albumOrTitle, CancellationToken ct)
    {
        var hit = await SearchAndScoreAsync($"{artist} {albumOrTitle}", "song", artist, ct);
        return hit is null ? null : await DownloadHiResAsync(hit, ct);
    }

    private async Task<byte[]?> DownloadHiResAsync(string artworkUrl100, CancellationToken ct)
    {
        // iTunes CDN serves arbitrary sizes by URL substring substitution. 600x600
        // is the sweet spot — most clients thumbnail to <=300, going higher would
        // just waste bandwidth.
        var hiRes = artworkUrl100.Replace("100x100bb", "600x600bb");
        try
        {
            using var resp = await _http.GetAsync(hiRes, ct);
            if (!resp.IsSuccessStatusCode) return null;
            return await resp.Content.ReadAsByteArrayAsync(ct);
        }
        catch (Exception ex)
        {
            _logger.LogDebug(ex, "iTunes artwork download failed for {Url}", hiRes);
            return null;
        }
    }

    /// <summary>
    /// Cheap case-insensitive substring score. Exact equality is best, then
    /// containment in either direction, then any token overlap. We don't need
    /// a real edit distance — we just need to push wrong-artist hits to the
    /// bottom and let a correct-artist hit win.
    /// </summary>
    private static int ScoreArtistMatch(string expected, string actual)
    {
        if (string.IsNullOrEmpty(actual)) return 0;
        var e = expected.Trim().ToLowerInvariant();
        var a = actual.Trim().ToLowerInvariant();
        if (a == e) return 100;
        if (a.Contains(e) || e.Contains(a)) return 60;
        var eTokens = e.Split(' ', StringSplitOptions.RemoveEmptyEntries);
        var aTokens = a.Split(' ', StringSplitOptions.RemoveEmptyEntries);
        int overlap = eTokens.Count(t => aTokens.Contains(t));
        return overlap * 10;
    }
}
