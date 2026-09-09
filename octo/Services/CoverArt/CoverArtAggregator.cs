using Microsoft.Extensions.Caching.Memory;
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
public class CoverArtAggregator : IDisposable
{
    private readonly IReadOnlyList<ICoverArtSource> _sources;
    private readonly ILogger<CoverArtAggregator> _logger;

    // Bounded in BYTES, and on its own instance rather than shared with the metadata
    // caches: these entries are 1000x1000 JPEGs at 150-400KB each, so an entry count
    // that suits small metadata records would be a meaningless bound here.
    private const long MaxCacheBytes = 256L * 1024 * 1024;
    private readonly MemoryCache _cache = new(new MemoryCacheOptions { SizeLimit = MaxCacheBytes });

    private static readonly TimeSpan HitTtl = TimeSpan.FromHours(12);

    /// <summary>A miss can be caused by a source being throttled, so it must not be
    /// remembered for long enough to blank a cover for the life of the process.</summary>
    private static readonly TimeSpan MissTtl = TimeSpan.FromMinutes(5);

    /// <summary>Wrapper so a cached miss is distinguishable from a cache miss.</summary>
    private sealed record Entry(byte[]? Bytes);

    public CoverArtAggregator(IEnumerable<ICoverArtSource> sources, ILogger<CoverArtAggregator> logger)
    {
        _sources = sources.ToList();
        _logger = logger;
        _logger.LogInformation("CoverArtAggregator: {N} sources in order: {Names}",
            _sources.Count, string.Join(", ", _sources.Select(s => s.Name)));
    }

    public async Task<byte[]?> GetCoverAsync(SoulseekRouting routing, bool background = false, CancellationToken ct = default)
    {
        var cacheKey = MakeCacheKey(routing);
        if (_cache.TryGetValue(cacheKey, out Entry? cached)) return cached!.Bytes;

        foreach (var source in _sources)
        {
            byte[]? bytes;
            try
            {
                bytes = await source.TryFetchAsync(routing, background, ct);
            }
            catch (Exception ex)
            {
                _logger.LogDebug(ex, "cover source {Source} threw for {Key}", source.Name, cacheKey);
                continue;
            }
            if (bytes is { Length: > 0 })
            {
                _logger.LogDebug("cover {Source} hit for {Key} ({Bytes} bytes)", source.Name, cacheKey, bytes.Length);
                Put(cacheKey, bytes, bytes.Length, HitTtl);
                return bytes;
            }
        }

        _logger.LogDebug("cover all-miss for {Key}", cacheKey);
        Put(cacheKey, null, 1, MissTtl);
        return null;
    }

    private void Put(string key, byte[]? bytes, long size, TimeSpan ttl) =>
        _cache.Set(key, new Entry(bytes), new MemoryCacheEntryOptions
        {
            Size = size,
            AbsoluteExpirationRelativeToNow = ttl,
        });

    /// <summary>Drop every cached cover. Exposed so a run of throttled lookups can be
    /// cleared without restarting the container.</summary>
    public void ClearCache()
    {
        _cache.Clear();
        _logger.LogInformation("cover art cache cleared");
    }

    public void Dispose() => _cache.Dispose();

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
