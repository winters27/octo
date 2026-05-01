using System.Collections.Concurrent;
using Octo.Services.Soulseek;

namespace Octo.Services.CoverArt;

/// <summary>
/// Cover art chain: queries each registered <see cref="ICoverArtSource"/> in
/// order and returns the first hit. Caches the result per (kind, artist,
/// album|title) so a queue scroll doesn't trigger N external API calls per
/// visible song.
///
/// Order matters: put broad-catalog sources (Deezer) first so we don't pay
/// the iTunes round-trip for international tracks where iTunes whiffs anyway.
/// Last.fm last because its track image often points to the same iTunes
/// asset we'd have gotten one source earlier.
/// </summary>
public class CoverArtAggregator
{
    private readonly IReadOnlyList<ICoverArtSource> _sources;
    private readonly ILogger<CoverArtAggregator> _logger;

    private readonly ConcurrentDictionary<string, byte[]?> _cache = new();
    private const int MaxCacheEntries = 4000;

    public CoverArtAggregator(IEnumerable<ICoverArtSource> sources, ILogger<CoverArtAggregator> logger)
    {
        _sources = sources.ToList();
        _logger = logger;
        _logger.LogInformation("CoverArtAggregator: {N} sources in order: {Names}",
            _sources.Count, string.Join(", ", _sources.Select(s => s.Name)));
    }

    public async Task<byte[]?> GetCoverAsync(SoulseekRouting routing, CancellationToken ct = default)
    {
        var cacheKey = MakeCacheKey(routing);
        if (_cache.TryGetValue(cacheKey, out var cached)) return cached;

        // Bounded LRU-ish trim: when we cross the cap, drop the first half by
        // dictionary order. ConcurrentDictionary doesn't preserve insertion
        // order strictly but it's close enough — cache misses just re-fetch.
        if (_cache.Count > MaxCacheEntries)
        {
            foreach (var key in _cache.Keys.Take(_cache.Count / 2))
                _cache.TryRemove(key, out _);
        }

        foreach (var source in _sources)
        {
            byte[]? bytes;
            try
            {
                bytes = await source.TryFetchAsync(routing, ct);
            }
            catch (Exception ex)
            {
                _logger.LogDebug(ex, "cover source {Source} threw for {Key}", source.Name, cacheKey);
                continue;
            }
            if (bytes is { Length: > 0 })
            {
                _logger.LogDebug("cover {Source} hit for {Key} ({Bytes} bytes)", source.Name, cacheKey, bytes.Length);
                _cache[cacheKey] = bytes;
                return bytes;
            }
        }

        _logger.LogDebug("cover all-miss for {Key}", cacheKey);
        _cache[cacheKey] = null;
        return null;
    }

    private static string MakeCacheKey(SoulseekRouting r)
    {
        var artist = (r.Artist ?? "").Trim().ToLowerInvariant();
        var albumOrTitle = (r.Kind == RoutingKind.Album
                ? (r.Album ?? r.Title ?? "")
                : (r.Title ?? r.Album ?? ""))
            .Trim().ToLowerInvariant();
        return $"{r.Kind}|{artist}|{albumOrTitle}";
    }
}
