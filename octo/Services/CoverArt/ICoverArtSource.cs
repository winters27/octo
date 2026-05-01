using Octo.Services.Soulseek;

namespace Octo.Services.CoverArt;

/// <summary>
/// One backend that can supply cover art bytes for a routing (song / album /
/// artist). Multiple sources stack behind <see cref="CoverArtAggregator"/>;
/// the aggregator queries them in order and returns the first hit.
///
/// Implementations should be self-contained (own HTTP client, own caching if
/// relevant) and never throw out of <see cref="TryFetchAsync"/> — return null
/// for any kind of miss or error so the chain can move on.
/// </summary>
public interface ICoverArtSource
{
    /// <summary>Short tag for log lines, e.g. "deezer", "itunes", "lastfm".</summary>
    string Name { get; }

    /// <summary>
    /// Try to fetch cover art bytes for the routing. Return null on any miss
    /// or transport error. Bytes returned should be a decodable image (caller
    /// will composite a watermark, so don't pre-encode).
    /// </summary>
    Task<byte[]?> TryFetchAsync(SoulseekRouting routing, CancellationToken ct = default);
}
