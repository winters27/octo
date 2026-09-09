using Octo.Models.Domain;
using Octo.Services.LastFm;

namespace Octo.Services.Common;

/// <summary>
/// Builds the external (discovery) half of a search, once per query.
///
/// Every caller for the same query joins one execution and receives the same list. That
/// is not an optimisation, it is what keeps the search3 fix from re-creating issue #8.
/// Clients routinely fire several search calls for one typed query, and registry ids are
/// deterministic, so those calls resolve to the *same* SoulseekRouting objects and would
/// each run the enrichment pipeline over them concurrently — three writers to a shared
/// int? duration, and three times the Deezer fan-out against a budget that is already the
/// tightest thing in the system.
///
/// The returned list is FROZEN. Nothing may mutate a Song after the build completes;
/// callers slice it and serialise it, concurrently, without copying. Both Subsonic
/// serialisers and the native one only read, and the star/download paths rebuild a Song
/// from the registry rather than from a search result. Any future "top up the enrichment
/// because this caller wanted more rows" belongs inside the build, not after it.
/// </summary>
public sealed class ExternalSearchService
{
    /// <summary>
    /// Rows built per query, regardless of how many the caller wants.
    ///
    /// It has to be a constant rather than the caller's target, or single-flight is
    /// unsound: a client asking for 8 rows could win the race and hand 8 rows to a caller
    /// that asked for 150. 60 is the number because that is where enrichment stops
    /// (BackgroundEnrichLimit), so rows past it would ship as bare placeholders carrying a
    /// fallback duration, and because the Navidrome-native search path already caps here.
    /// </summary>
    public const int BuildSize = 60;

    /// <summary>
    /// Deadline for one build. Last.fm has no configured HTTP timeout of its own, so
    /// without this a single hung call would pin the query for every joined caller.
    /// </summary>
    private static readonly TimeSpan BuildTimeout = TimeSpan.FromSeconds(10);

    /// <summary>Deadline for one album build. Deezer's own client timeout already bounds
    /// the call inside it; this is the ceiling on the whole build.</summary>
    private static readonly TimeSpan AlbumBuildTimeout = TimeSpan.FromSeconds(10);

    // Amperfy (and most Subsonic clients' type-ahead) fires one search3 call per
    // keystroke, uncancelled. Plain SingleFlight only collapses two callers asking for
    // the SAME query; it does nothing for the sequence "cage", "cage t", "cage the",
    // where each keystroke is a distinct key racing the same rate-limited lane as the
    // query the user actually meant. SupersedableBuildCoordinator adds prefix-based
    // cancellation on top of that collapsing.
    private readonly SupersedableBuildCoordinator<List<Song>> _songBuilds = new();
    private readonly SupersedableBuildCoordinator<List<Album>> _albumBuilds = new();
    private readonly IMusicMetadataService _metadata;
    private readonly LastFmService? _lastFm;
    private readonly ILogger<ExternalSearchService> _logger;

    public ExternalSearchService(
        IMusicMetadataService metadata,
        ILogger<ExternalSearchService> logger,
        LastFmService? lastFm = null)
    {
        _metadata = metadata;
        _logger = logger;
        _lastFm = lastFm;
    }

    /// <summary>
    /// Up to <see cref="BuildSize"/> enriched external songs for this query. Callers take
    /// the prefix they need; the list is shared and must not be mutated.
    /// </summary>
    public async Task<IReadOnlyList<Song>> GetAsync(string query, CancellationToken ct = default)
    {
        if (string.IsNullOrWhiteSpace(query)) return Array.Empty<Song>();
        // Only the key, not the radio switch. Discovery in the search bar is a different
        // feature from radio, and gating it on EnableRadio made turning radio off empty
        // the search results too.
        if (_lastFm is null || !_lastFm.HasApiKey) return Array.Empty<Song>();

        return await _songBuilds.RunAsync(
            query.Trim(),
            token => BuildAsync(query, token),
            BuildTimeout,
            fallback: new List<Song>(),
            onFailure: (q, ex) =>
                // Discovery is an addition to search, never a precondition for it. The
                // steps inside the build are individually best-effort, but the deadline is
                // not: the enrichment fan-out waits on its semaphore outside its own try, so
                // a timeout there would otherwise escape and take local results down with it.
                _logger.LogDebug("external search '{Q}' failed: {M}", q, ex.Message));
    }

    /// <summary>
    /// Up to <paramref name="limit"/> external albums for this query, via the same
    /// prefix-supersession as <see cref="GetAsync"/>. Needs no Last.fm key: Deezer's
    /// album catalog is keyless.
    /// </summary>
    public async Task<IReadOnlyList<Album>> GetAlbumsAsync(string query, int limit, CancellationToken ct = default)
    {
        if (string.IsNullOrWhiteSpace(query) || limit <= 0) return Array.Empty<Album>();

        return await _albumBuilds.RunAsync(
            query,
            token => _metadata.SearchAlbumsAsync(query, limit, token),
            AlbumBuildTimeout,
            fallback: new List<Album>(),
            onFailure: (q, ex) => _logger.LogDebug("external album search '{Q}' failed: {M}", q, ex.Message));
    }

    /// <summary>
    /// Fans out to Last.fm, then fills in the metadata a client needs to render and play
    /// the rows. Order:
    ///   1. track.search hits (best fuzzy matches for the query as typed)
    ///   2. canonical artist's top tracks (in case (1) was thin — common for
    ///      single-word artist queries)
    /// Deduped by artist+title so the same track cannot appear twice.
    /// </summary>
    private async Task<List<Song>> BuildAsync(string query, CancellationToken ct)
    {
        var seen = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
        var collected = new List<(string Artist, string Title)>();

        var tracks = await _lastFm!.SearchTracksAsync(query, Math.Min(50, BuildSize * 2));
        foreach (var t in tracks)
        {
            var key = $"{t.Artist}|{t.Title}".ToLowerInvariant();
            if (seen.Add(key)) collected.Add((t.Artist, t.Title));
            if (collected.Count >= BuildSize) break;
        }

        if (collected.Count < BuildSize)
        {
            // Use the first track-search hit's artist as the canonical anchor
            // for top-tracks padding. Falls back to the raw query string when
            // track.search came back empty.
            var anchor = tracks.Count > 0 ? tracks[0].Artist : query;
            var topTracks = await _lastFm.GetArtistTopTracksAsync(anchor, BuildSize * 2);
            foreach (var t in topTracks)
            {
                var key = $"{t.Artist}|{t.Title}".ToLowerInvariant();
                if (seen.Add(key)) collected.Add((t.Artist, t.Title));
                if (collected.Count >= BuildSize) break;
            }
        }

        var songs = new List<Song>(collected.Count);
        foreach (var (artist, title) in collected)
        {
            var hits = await _metadata.SearchSongsByArtistTitleAsync(artist, title, 1);
            if (hits.Count > 0) songs.Add(hits[0]);
        }
        _logger.LogInformation("External search '{Q}' -> {N} placeholder songs", query, songs.Count);

        // Album/art/year from Deezer (fast), then the ACCURATE duration for the top of the
        // list from the real YouTube video (so the scrub bar matches the audio and the
        // client advances correctly). Bounded + cached.
        await _metadata.EnrichExternalSongsAsync(songs, ct);
        await _metadata.ResolveTopDurationsAsync(songs, ct);

        // Fire-and-forget: pre-resolve YouTube videoIds for the top hits so the first
        // /rest/stream click doesn't pay the cold yt-dlp double-call cost (ytsearch1: + -g,
        // 6-16s combined). Arpeggi cancels at ~10s and falls back to a local song; without
        // this, external playback is unreachable from that client. 12 is about what fits on
        // the first page of search results.
        //
        // It runs LAST on purpose. It used to run before enrichment, where it wrote a
        // videoId chosen with no duration hint while ResolveTopDurationsAsync was choosing
        // a different one using the Deezer duration — so for the top rows the two raced and
        // the loser could leave a song advertising the length of a video that would not be
        // the one played.
        _ = _metadata.PrewarmYouTubeIdsAsync(songs, topN: 12);

        // Same reasoning, for cover art: a client renders the first screen of results a
        // moment after this returns, and without a prewarm each row's getCoverArt call
        // pays for the Deezer/iTunes/Last.fm chain itself, one row at a time. 24 covers
        // more than a screenful so scrolling a little still finds a warm cache.
        _ = _metadata.PrewarmCoverArtAsync(songs, topN: 24);

        return songs;
    }
}
