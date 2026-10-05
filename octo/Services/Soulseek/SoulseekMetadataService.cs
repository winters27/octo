using System.Collections.Concurrent;
using System.Text;
using System.Text.Json;
using Octo.Models.Domain;
using Octo.Models.Search;
using Octo.Models.Subsonic;
using Octo.Services.Common;
using Octo.Services.CoverArt;
using Octo.Services.LastFm;
using Octo.Services.Metadata;
using Octo.Services.YouTube;

namespace Octo.Services.Soulseek;

/// <summary>
/// Music metadata service for the YouTube-first / Soulseek-on-star architecture.
///
/// Radio queue creation is YouTube-only and lightweight: one yt-dlp search per
/// Last.fm similar track. We do NOT query Soulseek here — Soulseek is reserved
/// for the explicit "user wants to keep this" action (star / permanent download)
/// in SoulseekDownloadService.
///
/// External IDs are kept short (~30-80 chars) so Subsonic clients accept them.
/// Format:  yt|{videoId}|{artist_b64}|{title_b64}|{durationSec}
/// </summary>
public class SoulseekMetadataService : IMusicMetadataService
{
    public const string ProviderName = "soulseek";

    private readonly YouTubeResolver _youtube;
    private readonly ExternalIdRegistry _idRegistry;
    private readonly DeezerMetadataService _deezer;
    private readonly CoverArtAggregator _coverArt;
    private readonly LastFmService? _lastFm;
    private readonly ILogger<SoulseekMetadataService> _logger;

    public SoulseekMetadataService(
        YouTubeResolver youtube,
        ExternalIdRegistry idRegistry,
        DeezerMetadataService deezer,
        CoverArtAggregator coverArt,
        ILogger<SoulseekMetadataService> logger,
        LastFmService? lastFm = null)
    {
        _youtube = youtube;
        _idRegistry = idRegistry;
        _deezer = deezer;
        _coverArt = coverArt;
        _logger = logger;
        _lastFm = lastFm;
    }

    public Task<List<Song>> SearchSongsAsync(string query, int limit = 20)
    {
        if (string.IsNullOrWhiteSpace(query)) return Task.FromResult(new List<Song>());

        var (queryArtist, queryTitle) = ParseQuery(query);
        return SearchSongsByArtistTitleAsync(queryArtist, queryTitle ?? query, 1);
    }

    public Task<List<Song>> SearchSongsByArtistTitleAsync(string artist, string title, int limit = 1, int? durationSeconds = null)
    {
        if (string.IsNullOrWhiteSpace(artist) && string.IsNullOrWhiteSpace(title))
            return Task.FromResult(new List<Song>());

        // INSTANT placeholder. We do NOT call YouTube here — at queue-build time we'd
        // rate-limit ourselves into oblivion (Arpeggio fans out 5-10 search3 calls
        // per radio session). YouTube resolution is deferred to /rest/stream where
        // it happens once per actual playback, sequentially as the user advances.
        var externalId = _idRegistry.Register(new SoulseekRouting
        {
            // YouTubeId intentionally null — resolved lazily on play.
            Artist = artist,
            Title = title,
            Duration = durationSeconds
        });

        _logger.LogDebug("Placeholder song registered for '{Artist} - {Title}' (dur={Dur}) -> id {Id}",
            artist, title, durationSeconds, externalId);

        // Every caller that hands in a length got it from Last.fm, or from a play's own
        // record. Stored by rank, so a Deezer length an earlier lookup found for this id
        // still wins, and that is the length this row goes out with.
        _idRegistry.RememberLength(externalId, durationSeconds, LengthSource.LastFm);
        var remembered = _idRegistry.Lookup(externalId) is { } routing
            ? SongLength.Shown(routing).Seconds
            : null;

        // 180 is the fallback when we don't know the real duration — most songs
        // are 3-5 min so it's a less-bad guess than 0 (which would prevent
        // clients from rendering a scrub bar at all). The Octo app knows this
        // value and shows no length for it rather than a wrong one.
        var effectiveDuration = remembered ?? durationSeconds ?? 180;

        return Task.FromResult(new List<Song>
        {
            new Song
            {
                Id = externalId,
                Title = title,
                Artist = artist,
                Album = "",
                Duration = effectiveDuration,
                ExplicitContentLyrics = _idRegistry.Lookup(externalId)?.ExplicitContent,
                IsLocal = false,
                ExternalProvider = ProviderName,
                ExternalId = externalId
            }
        });
    }

    // Deezer's real ceiling is ~50 requests per 5 seconds, and this runs on search3's
    // critical path, so the set that blocks a response stays small. 60 rows at 8-way
    // concurrency was roughly 65 requests/second on its own, which is what exhausted the
    // quota and poisoned the metadata caches (issue #8). DeezerRateLimiter now holds that
    // budget centrally, so this figure is about how long a user waits, not about safety.
    //
    // 12 is the same "first page" figure PrewarmYouTubeIdsAsync already uses. It must
    // stay above TopDurationResolveLimit, or the rows that get a YouTube length hint
    // would be reading a duration nobody resolved.
    private const int SearchEnrichLimit = 12;

    // The whole slice a search can return. Everything between the first page and this is
    // filled from cache and warmed for next time, because a row with no enrichment falls
    // back to a flat 180 and a page of identical 3:00 rows is worse than a page of
    // approximate ones. Deezer's length is the approximation - it is not always the
    // recording that plays - and the exact value is resolved when a row is opened or
    // played, where getSong and the native detail endpoint both call
    // ResolveTopDurationsAsync.
    private const int BackgroundEnrichLimit = 60;


    public async Task EnrichExternalSongsAsync(List<Song> songs, CancellationToken ct = default)
    {
        var external = songs.Where(s => !s.IsLocal).ToList();

        // First-page rows Deezer gave no length for. They join the background lookup below
        // rather than going out as 3:00 for good.
        var missed = new ConcurrentDictionary<string, byte>();
        var sem = new SemaphoreSlim(8);
        var tasks = external.Take(SearchEnrichLimit).Select(async song =>
        {
            await sem.WaitAsync(ct);
            try
            {
                var meta = await _deezer.EnrichTrackAsync(song.Artist, song.Title, includeYear: true, ct: ct);
                if (meta?.Duration is not > 0) missed.TryAdd(song.Id, 0);
                if (meta is null) return;
                if (meta.Duration is int d && d > 0) song.Duration = d;
                if (!string.IsNullOrWhiteSpace(meta.AlbumTitle)) song.Album = meta.AlbumTitle;
                if (meta.Year is int y) song.Year = y;
                if (meta.ExplicitContent is int words) song.ExplicitContentLyrics = words;

                // Reflect onto the shared routing so getSong stays consistent.
                var routing = _idRegistry.Lookup(song.Id);
                if (routing != null)
                {
                    if (meta.Duration is int rd && rd > 0) routing.Duration = rd;
                    if (!string.IsNullOrWhiteSpace(meta.AlbumTitle)) routing.Album = meta.AlbumTitle;
                    if (meta.ExplicitContent is int said) routing.ExplicitContent = said;
                }
                _idRegistry.RememberLength(song.Id, meta.Duration, LengthSource.Deezer);
            }
            catch { /* best-effort; a miss just leaves the 180s fallback */ }
            finally { sem.Release(); }
        });
        await Task.WhenAll(tasks);

        var cold = EnrichRemaining(external.Skip(SearchEnrichLimit).Take(BackgroundEnrichLimit - SearchEnrichLimit).ToList());
        WarmLengths(external.Take(SearchEnrichLimit).Where(s => missed.ContainsKey(s.Id)).Concat(cold));
    }

    /// <summary>
    /// Complete the rows below the first page from what is already known, then fetch the
    /// rest off the critical path so the next search for this query can complete them too.
    ///
    /// Reading the cache is free, so it happens inline and the rows it answers are real in
    /// THIS response. The fetch is not free, and awaiting it was worse than the bug it
    /// fixed: a 4s budget added 4s to every search to fill about ten rows, and a page only
    /// converged after five searches. Off the critical path the same work costs nothing and
    /// the second search answers all of it from cache.
    ///
    /// The warm still writes nothing back to a Song. Those objects are being serialised as
    /// it runs, and Song.Duration is an int? whose non-atomic write can be read back as 0,
    /// which is exactly the value that stops a client drawing a scrub bar. What it finds
    /// goes on the routing instead, where the next response for the song reads it.
    ///
    /// Returns the rows the cache could not answer, for the caller to hand to that warm.
    /// </summary>
    private List<Song> EnrichRemaining(List<Song> songs)
    {
        var cold = new List<Song>();
        foreach (var song in songs)
        {
            var meta = _deezer.CachedTrack(song.Artist, song.Title);
            if (meta is null) { cold.Add(song); continue; }

            if (meta.Duration is int d && d > 0) song.Duration = d;
            if (!string.IsNullOrWhiteSpace(meta.AlbumTitle)) song.Album = meta.AlbumTitle;
            if (meta.ExplicitContent is int words) song.ExplicitContentLyrics = words;

            // Reflect onto the shared routing so getSong stays consistent.
            var routing = _idRegistry.Lookup(song.Id);
            if (routing != null)
            {
                if (meta.Duration is int rd && rd > 0) routing.Duration = rd;
                if (!string.IsNullOrWhiteSpace(meta.AlbumTitle)) routing.Album = meta.AlbumTitle;
                if (meta.ExplicitContent is int said) routing.ExplicitContent = said;
            }
            _idRegistry.RememberLength(song.Id, meta.Duration, LengthSource.Deezer);
        }
        return cold;
    }

    // ---- Lengths for rows that went out without one ---------------------------------
    //
    // About half of all outside songs reached clients with the 180s placeholder: search
    // rows past the first page, and every station row Last.fm gave no length for. The
    // station path never looked a length up at all, and what the search warm fetched only
    // reached Deezer's in-memory cache, so a song got its length back on a repeat search at
    // best and lost it again on a restart.
    //
    // Nothing here holds up a response. The rows a response can complete for free
    // (registry, Deezer's cache) are completed inline; the rest are looked up in the
    // background and stored on the registry, so the NEXT response for the song carries the
    // length. Sources are tried in order of how far their length can be trusted, and the
    // first one to answer ends the chain.

    /// <summary>Most rows one station response queues for a lookup. The next response
    /// queues the next ones, since those done by then are no longer cold.</summary>
    private const int StationLengthWarmLimit = 20;

    /// <summary>Ceiling on one song's lookup chain. Last.fm has no client timeout of its
    /// own, and one hung call would otherwise stall every song queued behind it.</summary>
    private static readonly TimeSpan LengthLookupTimeout = TimeSpan.FromSeconds(10);

    /// <summary>Ids with a lookup queued or running, so several clients loading one station
    /// at once do not each look its songs up again.</summary>
    private readonly ConcurrentDictionary<string, byte> _lengthLookups = new();

    /// <summary>The most recent background lookup, so a test can wait for it.</summary>
    internal Task LastLengthWarm { get; private set; } = Task.CompletedTask;

    public void CompleteSongLengths(IReadOnlyList<Song> songs)
    {
        var cold = new List<Song>();
        foreach (var song in songs)
        {
            if (song.IsLocal || string.IsNullOrEmpty(song.Id)) continue;
            if (_idRegistry.Lookup(song.Id) is not { Kind: RoutingKind.Song } routing
                || !routing.HasArtistTitle) continue;

            // Minting the song already applied whatever the registry remembered.
            if (SongLength.HasMetadataLength(routing)) continue;

            // Free: a search for the same song may have asked Deezer already.
            if (_deezer.CachedTrack(song.Artist, song.Title)?.Duration is int d && d > 0)
            {
                song.Duration = d;
                _idRegistry.RememberLength(song.Id, d, LengthSource.Deezer);
                continue;
            }
            cold.Add(song);
        }
        WarmLengths(cold.Take(StationLengthWarmLimit));
    }

    /// <summary>
    /// Look lengths up off the request, one song at a time, and store what is found on the
    /// registry. Order: Deezer, then Last.fm's track.getInfo, then a YouTube video's length
    /// inside the sane range. Songs that already have a metadata length are skipped.
    /// </summary>
    private void WarmLengths(IEnumerable<Song> songs)
    {
        var queued = new List<(string Id, string Artist, string Title)>();
        foreach (var song in songs)
        {
            if (string.IsNullOrEmpty(song.Id) || string.IsNullOrWhiteSpace(song.Title)) continue;
            if (_idRegistry.Lookup(song.Id) is not { } routing || SongLength.HasMetadataLength(routing)) continue;
            if (!_lengthLookups.TryAdd(song.Id, 0)) continue;
            queued.Add((song.Id, song.Artist ?? "", song.Title));
        }
        if (queued.Count == 0) return;

        LastLengthWarm = Task.Run(async () =>
        {
            // Sequential on purpose: this has no deadline, and fanning out here is what
            // would eat the Deezer quota a live search needs. The year is skipped because
            // it costs a second request per album and no row shows it.
            foreach (var (id, artist, title) in queued)
            {
                try
                {
                    using var cts = new CancellationTokenSource(LengthLookupTimeout);
                    await LookUpLengthAsync(id, artist, title, cts.Token);
                }
                catch { /* best-effort; the song keeps the placeholder until next time */ }
                finally { _lengthLookups.TryRemove(id, out _); }
            }
        });
    }

    private async Task LookUpLengthAsync(string id, string artist, string title, CancellationToken ct)
    {
        var meta = await _deezer.EnrichTrackAsync(artist, title, includeYear: false, background: true, ct: ct);
        if (_idRegistry.RememberLength(id, meta?.Duration, LengthSource.Deezer)) return;

        if (_lastFm is { HasApiKey: true })
        {
            var info = await _lastFm.GetTrackInfoAsync(artist, title, ct);
            if (_idRegistry.RememberLength(id, info?.Duration, LengthSource.LastFm)) return;
        }

        // Last, and only while the shim has room for background work: a video's length is
        // the weakest guess there is, and not worth making a play wait for.
        if (_idRegistry.Lookup(id) is { } routing && SongLength.Shown(routing).Source >= LengthSource.Video) return;
        if (!await _prewarmGate.WaitAsync(PrewarmQueueWait, ct)) return;
        try
        {
            // Length only. The video is not pinned for playback, so which video plays and
            // what a download is checked against both stay as they were.
            var hit = await _youtube.MetaAsync($"{artist} {title}", background: true, ct: ct);
            _idRegistry.RememberLength(id, hit?.Duration, LengthSource.Video);
        }
        finally { _prewarmGate.Release(); }
    }

    // Resolve the ACTUAL YouTube video for the top of the list at search time and
    // use its duration. Deezer's duration is a different recording (e.g. "Fade"
    // is 3:13 on Deezer but the YouTube upload that plays is 3:45), so the scrub
    // bar overran and the client's advance logic broke. Storing the videoId also
    // means playback reuses this exact video (durations match) and it is prewarmed.
    private const int TopDurationResolveLimit = 8;

    // Shared across ALL invocations, not created per call. The shim runs 5
    // yt-dlp processes at a time; per-invocation semaphores let the three
    // prewarm triggers (radio, scrobble, external search) stack to 12
    // concurrent /search against it, and the old value of 6 here exceeded the
    // whole gate on its own. Sized to the shim's background capacity
    // (MAX_CONCURRENT_YTDLP - GATE_RESERVE_INTERACTIVE). This service is
    // registered as a singleton, so an instance field is already process-wide
    // without being static (which would make parallel test runs hostile).
    private readonly SemaphoreSlim _prewarmGate = new(3);
    private static readonly TimeSpan PrewarmQueueWait = TimeSpan.FromSeconds(2);

    // Cover art never touches the shim: it hits Deezer/iTunes/Last.fm over HTTP, and
    // Deezer's own background lane (DeezerRateLimiter.BackgroundPermits) already bounds
    // that traffic. It needs its own gate, not _prewarmGate above: sharing that one meant
    // 24 cover-art tasks and 12 YouTube tasks fought over 3 permits with a 2s bounded
    // wait, so most cover fetches timed out and the ones that won starved YouTube prewarm.
    private readonly SemaphoreSlim _coverArtPrewarmGate = new(6);

    // The length pass that runs after a search has its own, smaller gate. On _prewarmGate its
    // eight lookups took every permit, and a getSong right after the search waited out its two
    // seconds and showed Deezer's length instead of the video's.
    private readonly SemaphoreSlim _backgroundDurationGate = new(2);

    public async Task ResolveTopDurationsAsync(List<Song> songs, CancellationToken ct = default, bool background = false)
    {
        var tasks = songs.Where(s => !s.IsLocal).Take(TopDurationResolveLimit).Select(async song =>
        {
            // The background pass runs after the client has the results, so a play may already
            // have pinned a video for this song. It keeps it, and the shim is spared the lookup.
            if (background && _idRegistry.Lookup(song.Id) is { YouTubeId.Length: > 0 }) return;
            var gate = background ? _backgroundDurationGate : _prewarmGate;
            if (!await gate.WaitAsync(PrewarmQueueWait, ct)) return;
            try
            {
                // Fast metadata-only lookup (flat search, no URL solve). Pass the
                // Deezer duration as a hint so it picks the closest-length canonical
                // video (not a long-form/compilation upload); playback reuses the
                // stored videoId, so the shown length matches the audio.
                var hit = await _youtube.MetaAsync($"{song.Artist} {song.Title}", song.Duration,
                    background: background, ct: ct);
                if (hit is { VideoId.Length: > 0 } && hit.Duration is int d && d > 0)
                {
                    // Shown only inside the sane range. An hour-long upload is a mix or a
                    // live set, and its length is no better than the one the row has.
                    // Only the foreground pass may touch the Song. In the background the list is
                    // already with the client and cached for the next page, being serialised as this
                    // runs, and Song.Duration is an int? whose torn write can read back as 0.
                    // getSong builds its answer from routing.Duration, so it still gets this length.
                    if (!background && SongLength.SaneVideoLength(d) is int shown) song.Duration = shown;
                    var routing = _idRegistry.Lookup(song.Id);
                    // A play can pin a different video while this lookup runs. The next Range
                    // request has to get the same video, so the pinned one wins.
                    if (background && routing is { YouTubeId.Length: > 0 } && routing.YouTubeId != hit.VideoId) return;
                    if (routing != null)
                    {
                        routing.YouTubeId = hit.VideoId; // playback reuses this exact video
                        routing.Duration = d;
                    }
                    _idRegistry.RememberLength(song.Id, d, LengthSource.Video);
                }
            }
            catch { /* best-effort; keeps the existing duration on a miss */ }
            finally { gate.Release(); }
        });
        await Task.WhenAll(tasks);
    }

    /// <summary>
    /// Fire-and-forget background prewarm: resolve the YouTube videoId (and via
    /// shim's automatic prefetch, the stream URL) for the first <paramref name="topN"/>
    /// placeholder songs from a search. Without this, Arpeggi's ~10s HTTP timeout
    /// fires while the cold yt-dlp ytsearch1: + yt-dlp -g chain is still running,
    /// the client cancels, and external songs never play.
    ///
    /// Only the top hits matter: search clients render in order and users almost
    /// never click past the first screen of results. Resolving 150 placeholders
    /// would saturate the shim's yt-dlp gate and waste work.
    /// </summary>
    public Task PrewarmYouTubeIdsAsync(IEnumerable<Song> songs, int topN, CancellationToken ct = default)
    {
        var ids = songs
            .Where(s => !string.IsNullOrEmpty(s.Id))
            .Select(s => s.Id);
        return PrewarmYouTubeIdsForSongIdsAsync(ids, topN, ct);
    }

    public Task PrewarmYouTubeIdsForSongIdsAsync(IEnumerable<string> songIds, int topN, CancellationToken ct = default)
    {
        // Skip ids whose YouTube resolution is already cached on the routing —
        // those are already warm and don't need a yt-dlp roundtrip. This is the
        // path used by the scrobble-driven sliding window: as the user advances
        // through a queue most upcoming items will still be cold, but if they
        // jump back to one we resolved earlier we don't burn shim cycles re-doing it.
        var targets = songIds
            .Where(id => !string.IsNullOrEmpty(id))
            .Select(id => (id, routing: _idRegistry.Lookup(id)))
            .Where(t => t.routing != null
                        && string.IsNullOrEmpty(t.routing!.YouTubeId)
                        && t.routing.HasArtistTitle)
            .Take(topN)
            .ToList();
        if (targets.Count == 0) return Task.CompletedTask;

        var tasks = targets.Select(async t =>
        {
            // Bounded wait, then drop. With a shared limiter an unbounded wait
            // lets a skip-happy user pile up prewarm tasks for songs they left
            // behind five tracks ago. Prewarm is best-effort by design, so its
            // queueing is best-effort too.
            if (!await _prewarmGate.WaitAsync(PrewarmQueueWait, ct)) return;
            try
            {
                var routing = t.routing!;
                if (!string.IsNullOrEmpty(routing.YouTubeId)) return;
                var hit = await _youtube.SearchAsync($"{routing.Artist} {routing.Title}",
                    routing.Duration, background: true, ct: ct);
                if (hit is { VideoId: { Length: > 0 } })
                {
                    routing.YouTubeId = hit.VideoId;
                    if (hit.Duration is int d) routing.Duration = d;
                    _idRegistry.RememberLength(t.id, hit.Duration, LengthSource.Video);
                }
            }
            catch { /* best-effort warm; never throw out of fire-and-forget */ }
            finally { _prewarmGate.Release(); }
        });
        return Task.WhenAll(tasks);
    }

    /// <summary>
    /// Fire-and-forget background prewarm of cover art for the first <paramref name="topN"/>
    /// songs of a search, so a client that renders them a moment later finds the image
    /// already in <see cref="CoverArtAggregator"/>'s cache. Uses its own
    /// <see cref="_coverArtPrewarmGate"/>, separate from the shim-bound YouTube prewarm
    /// gate, and passes background: true through to the cover sources so this can never
    /// queue behind a live search or getCoverArt request.
    /// </summary>
    public Task PrewarmCoverArtAsync(IEnumerable<Song> songs, int topN, CancellationToken ct = default)
    {
        var targets = songs.Where(s => !s.IsLocal).Take(topN).ToList();
        if (targets.Count == 0) return Task.CompletedTask;

        var tasks = targets.Select(async song =>
        {
            if (!await _coverArtPrewarmGate.WaitAsync(PrewarmQueueWait, ct)) return;
            try
            {
                var routing = _idRegistry.Lookup(song.Id) ?? new SoulseekRouting
                {
                    Kind = RoutingKind.Song,
                    Artist = song.Artist,
                    Title = song.Title,
                };
                await _coverArt.GetCoverAsync(routing, background: true, ct);
            }
            catch { /* best-effort warm; never throw out of fire-and-forget */ }
            finally { _coverArtPrewarmGate.Release(); }
        });
        return Task.WhenAll(tasks);
    }

    public async Task<List<Album>> SearchAlbumsAsync(string query, int limit = 20, CancellationToken ct = default)
    {
        if (string.IsNullOrWhiteSpace(query) || limit <= 0) return new List<Album>();

        var hits = await _deezer.SearchAlbumsAsync(query, limit, ct);
        var albums = new List<Album>(hits.Count);
        var cleanCopies = CleanCopies(hits);

        foreach (var hit in hits)
        {
            // The registry id is the external id everywhere: getAlbum, getCoverArt and
            // star all round-trip through it. The Deezer id rides along on the routing
            // so album detail can fetch the exact tracklist without a name lookup. A clean copy
            // listed beside its explicit original gets an id of its own, or the two rows would
            // open the same album.
            var words = cleanCopies.Contains(hit.DeezerId) ? ExplicitStatus.Clean
                : hit.ExplicitLyrics == true ? ExplicitStatus.Explicit : (int?)null;
            var albumId = _idRegistry.Register(new SoulseekRouting
            {
                Kind = RoutingKind.Album,
                Artist = hit.Artist,
                Album = hit.Title,
                ExternalAlbumId = hit.DeezerId,
                ExplicitContent = words,
                Version = ExplicitStatus.VersionOf(words),
            });
            var artistId = _idRegistry.Register(new SoulseekRouting
            {
                Kind = RoutingKind.Artist,
                Artist = hit.Artist,
            });

            albums.Add(new Album
            {
                Id = albumId,
                Title = hit.Title,
                Artist = hit.Artist,
                ArtistId = artistId,
                Year = hit.Year,
                SongCount = hit.TrackCount,
                CoverArtUrl = hit.CoverUrl,
                ReleaseTypes = ReleaseTypes(hit.RecordType),
                ExplicitContentLyrics = words,
                IsLocal = false,
                ExternalProvider = ProviderName,
                ExternalId = albumId,
            });
        }

        return albums;
    }

    /// <summary>
    /// The catalog ids of the clean copies in an album listing: a release that is not marked
    /// explicit beside one of the same artist and title that is. The listing alone does not tell
    /// a clean edit from an album that never had explicit words, but its explicit twin does.
    /// </summary>
    internal static IReadOnlySet<string> CleanCopies(IEnumerable<DeezerMetadataService.AlbumHit> hits) =>
        hits.GroupBy(hit => (SongIdentity.Key(hit.Artist), SongIdentity.Key(hit.Title)))
            .Where(group => group.Any(hit => hit.ExplicitLyrics == true))
            .SelectMany(group => group.Where(hit => hit.ExplicitLyrics == false).Select(hit => hit.DeezerId))
            .ToHashSet(StringComparer.Ordinal);

    public async Task<List<Artist>> SearchArtistsAsync(string query, int limit = 20)
    {
        if (string.IsNullOrWhiteSpace(query) || limit <= 0) return new List<Artist>();

        var hits = await _deezer.SearchArtistsAsync(query, limit);
        var artists = new List<Artist>(hits.Count);

        // Two catalog artists of one name get one id, so they are one row: two rows opening
        // the same page would only confuse. The row is the artist Octo already settled on for
        // that name, else the one more people follow.
        foreach (var sameName in hits.GroupBy(hit => hit.Name, StringComparer.Ordinal))
        {
            // Same registry id an album row mints for its artist, because the seed is the
            // artist name alone. So an artist found here and the same artist reached from
            // an album are one entity, and getArtist answers for both.
            var id = _idRegistry.Register(new SoulseekRouting
            {
                Kind = RoutingKind.Artist,
                Artist = sameName.Key,
            });
            var routing = _idRegistry.Lookup(id);
            var hit = sameName.FirstOrDefault(h => h.DeezerId == routing?.ExternalArtistId)
                ?? MostFollowed(sameName);
            // Remembered only when nothing is yet: an earlier search or visit to the artist's
            // page already settled which artist of the name this is. An album never settles
            // it: the name's entry is everyone's, and the first album to show a little-known
            // namesake would have decided the name for all of them.
            if (routing is not null && routing.ExternalArtistId is null)
            {
                routing.ExternalArtistId = hit.DeezerId;
                _idRegistry.Register(routing);
            }

            artists.Add(new Artist
            {
                Id = id,
                Name = hit.Name,
                ImageUrl = hit.PictureUrl,
                AlbumCount = hit.AlbumCount,
                IsLocal = false,
                ExternalProvider = ProviderName,
                ExternalId = id,
            });
        }

        return artists;
    }

    public async Task<SearchResult> SearchAllAsync(string query, int songLimit = 20, int albumLimit = 20, int artistLimit = 20)
    {
        var songs = await SearchSongsAsync(query, songLimit);
        return new SearchResult { Songs = songs, Albums = new List<Album>(), Artists = new List<Artist>() };
    }

    public Task<Song?> GetSongAsync(string externalProvider, string externalId)
    {
        if (!string.Equals(externalProvider, ProviderName, StringComparison.OrdinalIgnoreCase))
            return Task.FromResult<Song?>(null);

        var routing = _idRegistry.Lookup(externalId) ?? TryDecodeExternalId(externalId);
        if (routing is null) return Task.FromResult<Song?>(null);

        return Task.FromResult<Song?>(new Song
        {
            Id = externalId,
            Title = routing.Title ?? "",
            Artist = routing.Artist ?? "",
            // Carried from the routing so an album download tags the album the user
            // actually hearted rather than whatever Deezer guesses from artist+title,
            // and keeps its position so the album stays in order.
            Album = routing.Album ?? "",
            Track = routing.Track,
            DiscNumber = routing.DiscNumber,
            TotalTracks = routing.TotalTracks,
            Duration = routing.Duration,
            Isrc = routing.Isrc,
            ExplicitContentLyrics = routing.ExplicitContent,
            IsLocal = false,
            ExternalProvider = ProviderName,
            ExternalId = externalId
        });
    }

    public async Task<Album?> GetAlbumAsync(string externalProvider, string externalId)
    {
        if (!string.Equals(externalProvider, ProviderName, StringComparison.OrdinalIgnoreCase)) return null;
        var routing = _idRegistry.Lookup(externalId);
        if (routing is null) return null;

        var placeholder = routing.Album ?? routing.Title ?? "";
        // Enrich by the track title (the placeholder "album" is the song title) so
        // Deezer returns the REAL album (e.g. "Creep" -> "Pablo Honey"). Degrades
        // to the placeholder name if Deezer misses or is unreachable.
        var artistId = _idRegistry.Register(new SoulseekRouting { Kind = RoutingKind.Artist, Artist = routing.Artist });

        // Resolve the Deezer album two ways. A search-derived routing already knows the
        // exact id. One minted from a song row does not, so recover the REAL album name
        // first (the placeholder is the song title, e.g. "Creep" -> "Pablo Honey") and
        // look the id up by name.
        DeezerMetadataService.TrackMeta? meta = null;
        var deezerAlbumId = routing.ExternalAlbumId;
        if (string.IsNullOrEmpty(deezerAlbumId))
        {
            meta = await _deezer.EnrichTrackAsync(routing.Artist, routing.Album ?? routing.Title);
            deezerAlbumId = await _deezer.FindAlbumIdAsync(routing.Artist, meta?.AlbumTitle ?? placeholder);
        }

        var album = new Album
        {
            Id = externalId,
            Title = meta?.AlbumTitle ?? placeholder,
            Artist = routing.Artist ?? "",
            ArtistId = artistId,
            Year = meta?.Year,
            CoverArtUrl = meta?.AlbumCoverUrl,
            IsLocal = false,
            ExternalProvider = ProviderName,
            ExternalId = externalId,
        };

        if (string.IsNullOrEmpty(deezerAlbumId))
        {
            // Logged rather than silent: this is what a user sees as an album that opens
            // with no tracks, and without a line here there is nothing to diagnose from.
            ListSongsFiledUnder(album, routing, placeholder, artistId);
            _logger.LogWarning(
                "getAlbum '{Artist} - {Album}' ({Id}): no Deezer album id resolved; listing the {Count} song(s) filed under it",
                routing.Artist, placeholder, externalId, album.Songs.Count);
            return album;
        }

        var (detail, answer) = await _deezer.LookUpAlbumDetailAsync(deezerAlbumId);
        // An album with no resolvable tracklist must still render, so fall through with
        // whatever we already have rather than failing the request.
        if (detail is null)
        {
            // Deezer only failed to answer this time: no songs are filed in, because a
            // partial list would be taken for the whole album by a client that caches what
            // it syncs. When Deezer answered that it has no such album, or no tracks for it,
            // the songs filed under it are all there is to list.
            var standIn = answer is not DeezerMetadataService.AlbumAnswer.Unavailable;
            if (standIn) ListSongsFiledUnder(album, routing, placeholder, artistId);
            _logger.LogWarning(
                "getAlbum '{Artist} - {Album}' ({Id}): Deezer album {DeezerId} returned no usable detail ({Answer}; "
                + "see the deezer warning above for why); {Outcome}",
                routing.Artist, placeholder, externalId, deezerAlbumId, answer,
                standIn ? $"listing the {album.Songs.Count} song(s) filed under it" : "returning album without a tracklist");
            return album;
        }

        album.Title = detail.Title;
        album.Year = detail.Year ?? album.Year;
        album.Genre = detail.Genre;
        album.CoverArtUrl = detail.CoverUrl ?? album.CoverArtUrl;
        album.ReleaseTypes = ReleaseTypes(detail.RecordType);
        album.ExplicitContentLyrics = detail.ExplicitContent ?? routing.ExplicitContent;
        // Defence in depth: the Deezer layer no longer returns a tracklist-less album,
        // but if one ever gets through, reporting zero is worse than saying nothing.
        if (detail.Tracks.Count > 0) album.SongCount = detail.Tracks.Count;
        if (!string.IsNullOrWhiteSpace(detail.Artist)) album.Artist = detail.Artist;
        // Deezer says the album has no tracks at all: the songs filed under it stand in.
        if (detail.Tracks.Count == 0) ListSongsFiledUnder(album, routing, placeholder, artistId);

        foreach (var track in detail.Tracks)
        {
            // Album is carried on the ROUTING as well as the Song. The download path
            // re-resolves each track by id through GetSongAsync, and without this the
            // tagger re-derives the album from artist+title alone, which for a well
            // known single often lands on a greatest-hits record instead of this one.
            var trackId = _idRegistry.Register(new SoulseekRouting
            {
                Kind = RoutingKind.Song,
                Artist = track.Artist,
                Title = track.Title,
                Album = detail.Title,
                Duration = track.Duration,
                Track = track.TrackPosition,
                DiscNumber = track.DiscNumber,
                TotalTracks = detail.Tracks.Count,
                Isrc = track.Isrc,
                // A clean edit's track has an id of its own, apart from the explicit album's.
                ExplicitContent = track.ExplicitContent,
                Version = ExplicitStatus.VersionOf(track.ExplicitContent),
            });

            album.Songs.Add(new Song
            {
                Id = trackId,
                Title = track.Title,
                Artist = track.Artist,
                ArtistId = artistId,
                Album = detail.Title,
                AlbumId = externalId,
                Duration = track.Duration,
                Track = track.TrackPosition,
                DiscNumber = track.DiscNumber,
                Isrc = track.Isrc,
                ExplicitContentLyrics = track.ExplicitContent,
                Year = detail.Year,
                Genre = detail.Genre,
                CoverArtUrl = detail.CoverUrl,
                CoverArtUrlLarge = detail.CoverUrl,
                AlbumArtist = detail.Artist,
                Label = detail.Label,
                IsLocal = false,
                ExternalProvider = ProviderName,
                ExternalId = trackId,
            });
        }

        return album;
    }

    /// <summary>
    /// An album the catalog cannot list still has the songs Octo showed under it: at least the
    /// one whose row named it. Listing those instead of nothing is what keeps a client that
    /// opens the album of the song it is playing (Tempo, #59) from finding it empty.
    /// </summary>
    private void ListSongsFiledUnder(Album album, SoulseekRouting routing, string placeholder, string artistId)
    {
        foreach (var (id, song) in _idRegistry.SongsFiledUnder(routing.Artist, placeholder)
                     .OrderBy(pair => pair.Routing.DiscNumber ?? 1).ThenBy(pair => pair.Routing.Track ?? int.MaxValue))
        {
            album.Songs.Add(new Song
            {
                Id = id,
                Title = song.Title ?? "",
                Artist = song.Artist ?? "",
                ArtistId = artistId,
                Album = album.Title,
                AlbumId = album.Id,
                Duration = SongLength.Shown(song).Seconds ?? song.Duration,
                Track = song.Track,
                DiscNumber = song.DiscNumber,
                Isrc = song.Isrc,
                Year = album.Year,
                CoverArtUrl = album.CoverArtUrl,
                IsLocal = false,
                ExternalProvider = ProviderName,
                ExternalId = id,
            });
        }
        if (album.Songs.Count > 0) album.SongCount = album.Songs.Count;
    }

    public async Task<Artist?> GetArtistAsync(string externalProvider, string externalId)
    {
        if (!string.Equals(externalProvider, ProviderName, StringComparison.OrdinalIgnoreCase)) return null;
        var routing = _idRegistry.Lookup(externalId);
        if (routing is null) return null;

        // The name and picture of the catalog artist this page lists, as the album listing
        // settles it: the id already held, else the one of this exact name more people
        // follow. The first search hit can be a bigger act whose name contains this one.
        var hit = await FindCatalogArtistAsync(routing);
        var meta = hit is null ? await _deezer.EnrichArtistAsync(routing.Artist) : null;
        return new Artist
        {
            Id = externalId,
            Name = hit?.Name ?? meta?.Name ?? routing.Artist ?? "",
            ImageUrl = hit?.PictureUrl ?? meta?.ImageUrl,
            IsLocal = false,
            ExternalProvider = ProviderName,
            ExternalId = externalId,
        };
    }

    /// <summary>How long an artist's page waits for its albums' track counts.</summary>
    private static readonly TimeSpan TrackCountWait = TimeSpan.FromSeconds(2);

    /// <summary>How many unknown track counts one visit to an artist's page asks for, and how
    /// many at once: gentle on the catalog's quota, which search and playback share.</summary>
    private const int TrackCountsPerVisit = 20;
    private const int TrackCountsAtOnce = 4;

    /// <summary>How many catalog artists a name search weighs. The first hit is not reliably
    /// the artist asked for: a name another artist shares, or a bigger act that contains it,
    /// can come first.</summary>
    private const int ArtistCandidates = 5;

    /// <summary>How many artists of one name a library artist's page compares against the
    /// library. Each is one listing call, cached, and only made when a name is shared.</summary>
    private const int ArtistsCompared = 3;

    /// <summary>
    /// The releases of the catalog artist an outside artist's name stands for, or none when no
    /// catalog artist has that name. The catalog id Octo already holds wins over a name search:
    /// it is the artist the user tapped in search, or the one an earlier visit settled on. On a
    /// library artist's page it must also share an album with the library, because two artists
    /// can share a name and the library says which one is meant. Without an id, only artists
    /// with this exact name count; of several, the one sharing the most albums with the
    /// library, else the one more people follow.
    ///
    /// The choice is kept for the next visit, and where depends on the page. The artist's
    /// routing is shared by everyone who reaches that name, from search, an album or a song,
    /// so an outside page keeps its choice there. A library artist's page keeps its own apart,
    /// under <paramref name="pageKey"/>: written onto the routing, the library's namesake
    /// became every listener's search row and outside page for the name.
    /// </summary>
    private async Task<List<DeezerMetadataService.AlbumHit>> FindArtistReleasesAsync(
        SoulseekRouting routing, string name, IReadOnlySet<string> owned, string pageKey)
    {
        var libraryPage = owned.Count > 0;
        var knownId = libraryPage && _libraryPicks.TryGetValue(pageKey, out var picked)
            ? picked
            : routing.ExternalArtistId;

        List<DeezerMetadataService.AlbumHit>? knownReleases = null;
        if (knownId is { Length: > 0 } known)
        {
            knownReleases = await _deezer.GetArtistAlbumsAsync(known, name);
            if (!libraryPage) return knownReleases;
            if (Shared(knownReleases, owned) > 0)
            {
                KeepLibraryPick(pageKey, known);
                return knownReleases;
            }
        }

        var candidates = (await _deezer.SearchArtistsAsync(name, ArtistCandidates))
            .Where(hit => SongIdentity.SameArtistName(hit.Name, name))
            .ToList();
        if (candidates.Count == 0) return knownReleases ?? new List<DeezerMetadataService.AlbumHit>();

        var pick = MostFollowed(candidates);
        var releases = (List<DeezerMetadataService.AlbumHit>?)null;
        if (candidates.Count > 1 && owned.Count > 0)
        {
            var best = 0;
            foreach (var candidate in candidates.OrderByDescending(hit => hit.Fans).Take(ArtistsCompared))
            {
                var theirs = await _deezer.GetArtistAlbumsAsync(candidate.DeezerId, name);
                var shared = Shared(theirs, owned);
                if (shared <= best) continue;
                (best, pick, releases) = (shared, candidate, theirs);
            }
        }
        releases ??= await _deezer.GetArtistAlbumsAsync(pick.DeezerId, name);

        if (libraryPage)
        {
            KeepLibraryPick(pageKey, pick.DeezerId);
        }
        else if (pick.DeezerId != routing.ExternalArtistId)
        {
            routing.ExternalArtistId = pick.DeezerId;
            _idRegistry.Register(routing);
        }
        return releases;
    }

    /// <summary>The catalog artist each library artist's page settled on, by the page's artist
    /// and library titles. Never on the shared routing: see FindArtistReleasesAsync.</summary>
    private readonly ConcurrentDictionary<string, string> _libraryPicks = new();

    /// <summary>How many library pages' choices are kept. Past it they start over: a choice
    /// lost costs one comparison against listings the catalog cache still holds.</summary>
    private const int LibraryPicksKept = 2048;

    private void KeepLibraryPick(string pageKey, string deezerArtistId)
    {
        if (_libraryPicks.Count >= LibraryPicksKept && !_libraryPicks.ContainsKey(pageKey)) _libraryPicks.Clear();
        _libraryPicks[pageKey] = deezerArtistId;
    }

    /// <summary>The catalog artist of this exact name an outside artist stands for: the one
    /// Octo already settled on, else the one more people follow. Null when none has the name.
    /// The search is the one the album listing makes, so it costs nothing more.</summary>
    private async Task<DeezerMetadataService.ArtistHit?> FindCatalogArtistAsync(SoulseekRouting routing)
    {
        if (routing.Artist is not { Length: > 0 } name) return null;
        var candidates = (await _deezer.SearchArtistsAsync(name, ArtistCandidates))
            .Where(hit => SongIdentity.SameArtistName(hit.Name, name))
            .ToList();
        if (candidates.Count == 0) return null;
        return candidates.FirstOrDefault(hit => hit.DeezerId == routing.ExternalArtistId)
            ?? MostFollowed(candidates);
    }

    /// <summary>A catalog record type as release types, a fresh list for each album.</summary>
    private static List<string> ReleaseTypes(string? recordType) =>
        [.. DeezerMetadataService.ReleaseTypes(recordType)];

    /// <summary>How many of an artist's releases the library holds, by the matcher's key.</summary>
    private static int Shared(IEnumerable<DeezerMetadataService.AlbumHit> releases, IReadOnlySet<string> owned) =>
        releases.Count(release => owned.Contains(SongIdentity.Key(release.Title)));

    /// <summary>The artist more people follow; the catalog's own order on a tie.</summary>
    private static DeezerMetadataService.ArtistHit MostFollowed(IEnumerable<DeezerMetadataService.ArtistHit> hits) =>
        hits.OrderByDescending(hit => hit.Fans).First();

    /// <summary>
    /// An outside artist's releases, for their page and for filling out a library artist's
    /// page. Each album is registered the way album search registers one, so it opens, plays
    /// and stars like any other outside album. The artist name and id are left for the caller:
    /// a library artist's page links its albums back to the library artist.
    /// </summary>
    public Task<List<Album>> GetArtistAlbumsAsync(string externalProvider, string externalId)
        => GetArtistAlbumsAsync(externalProvider, externalId, null);

    /// <summary>
    /// The same, for a library artist's page: the library's album titles say which of two
    /// artists of one name the page is about.
    /// </summary>
    public Task<List<Album>> GetArtistAlbumsAsync(string externalProvider, string externalId,
        IReadOnlyCollection<string>? libraryAlbumTitles)
        => ArtistAlbumsAsync(externalProvider, externalId, libraryAlbumTitles, fillCounts: true);

    /// <summary>
    /// The same list with only the track counts already known, for the counts an artist's own
    /// record shows. The page asks for its album list at the same moment, and that request
    /// asks the catalog for the missing counts; asking again here doubled the traffic.
    /// </summary>
    public Task<List<Album>> GetArtistAlbumsKnownCountsAsync(string externalProvider, string externalId)
        => ArtistAlbumsAsync(externalProvider, externalId, null, fillCounts: false);

    /// <summary>An artist's releases and the track counts shown beside them.</summary>
    private sealed record ArtistWalk(List<DeezerMetadataService.AlbumHit> Releases, int?[] Counts);

    /// <summary>
    /// Walks of an artist's catalog in progress: the releases alone, and the releases with
    /// their counts filled. Keyed by the artist's id and the library titles, since those decide
    /// which artist of a name is meant. A client opens an artist's page with two requests at
    /// once, and each used to walk the catalog on its own: twice the calls against a quota
    /// search and playback share. Everything a walk asks is cached once it is back, so a
    /// finished walk leaves this and the next visit reads the cache.
    /// </summary>
    private readonly ConcurrentDictionary<string, Lazy<Task<List<DeezerMetadataService.AlbumHit>>>> _releaseWalks = new();
    private readonly ConcurrentDictionary<string, Lazy<Task<ArtistWalk>>> _countWalks = new();

    /// <summary>One run of <paramref name="start"/> for every caller asking under the same key
    /// while it runs.</summary>
    private static Task<T> SharedWalk<T>(ConcurrentDictionary<string, Lazy<Task<T>>> walks, string key,
        Func<Task<T>> start) =>
        walks.GetOrAdd(key, k => new Lazy<Task<T>>(async () =>
        {
            try { return await start(); }
            finally { walks.TryRemove(k, out _); }
        })).Value;

    private async Task<List<Album>> ArtistAlbumsAsync(string externalProvider, string externalId,
        IReadOnlyCollection<string>? libraryAlbumTitles, bool fillCounts)
    {
        if (!string.Equals(externalProvider, ProviderName, StringComparison.OrdinalIgnoreCase)) return new List<Album>();
        var routing = _idRegistry.Lookup(externalId);
        if (routing?.Artist is not { Length: > 0 } name) return new List<Album>();

        var owned = new HashSet<string>(
            (libraryAlbumTitles ?? []).Select(SongIdentity.Key).Where(key => key.Length > 0),
            StringComparer.Ordinal);
        var key = externalId + "|" + string.Join("\u001f", owned.Order(StringComparer.Ordinal));
        Task<List<DeezerMetadataService.AlbumHit>> Releases() =>
            SharedWalk(_releaseWalks, key, () => FindArtistReleasesAsync(routing, name, owned, key));

        ArtistWalk walk;
        if (fillCounts)
        {
            walk = await SharedWalk(_countWalks, key, async () =>
            {
                var found = await Releases();
                return new ArtistWalk(found, await FillTrackCountsAsync(found, fill: true));
            });
        }
        else
        {
            var found = await Releases();
            walk = new ArtistWalk(found, await FillTrackCountsAsync(found, fill: false));
        }

        // Each caller gets albums of its own: a library artist's page relinks them to itself.
        return ToAlbums(name, walk);
    }

    /// <summary>
    /// The listing carries no track counts. Each album's own record has one. Counts already
    /// known cost nothing; of the rest, the first few on the page are asked a few at a time, and
    /// the page waits a moment for them. What arrives in time is shown and the rest are kept for
    /// the next visit, so a long career fills in over a visit or two without flooding the
    /// catalog's quota. Without <paramref name="fill"/>, only the counts already known.
    /// </summary>
    private async Task<int?[]> FillTrackCountsAsync(List<DeezerMetadataService.AlbumHit> releases, bool fill)
    {
        var counts = new int?[releases.Count];
        var lookups = new List<Task>();
        // Not disposed: lookups still waiting when the page answers keep using it.
        var gate = new SemaphoreSlim(TrackCountsAtOnce);
        for (var i = 0; i < releases.Count; i++)
        {
            var release = releases[i];
            if (release.TrackCount > 0) counts[i] = release.TrackCount;
            else if (_deezer.TryKnownTrackCount(release.DeezerId, out var known)) counts[i] = known;
            else if (fill && lookups.Count < TrackCountsPerVisit) lookups.Add(FillCount(i, release.DeezerId));
        }
        if (lookups.Count > 0) await Task.WhenAny(Task.WhenAll(lookups), Task.Delay(TrackCountWait));
        return (int?[])counts.Clone();

        async Task FillCount(int index, string deezerId)
        {
            await gate.WaitAsync();
            try { counts[index] = await _deezer.AlbumTrackCountAsync(deezerId); }
            finally { gate.Release(); }
        }
    }

    private List<Album> ToAlbums(string name, ArtistWalk walk)
    {
        var (releases, shown) = (walk.Releases, walk.Counts);
        var albums = new List<Album>(releases.Count);
        for (var i = 0; i < releases.Count; i++)
        {
            var release = releases[i];
            var count = shown[i];
            var albumId = _idRegistry.Register(new SoulseekRouting
            {
                Kind = RoutingKind.Album,
                Artist = name,
                Album = release.Title,
                ExternalAlbumId = release.DeezerId,
                ExplicitContent = release.ExplicitLyrics == true ? ExplicitStatus.Explicit : null,
            });
            albums.Add(new Album
            {
                Id = albumId,
                Title = release.Title,
                Year = release.Year,
                SongCount = count ?? release.TrackCount,
                CoverArtUrl = release.CoverUrl,
                ReleaseTypes = ReleaseTypes(release.RecordType),
                ExplicitContentLyrics = release.ExplicitLyrics == true ? ExplicitStatus.Explicit : null,
                IsLocal = false,
                ExternalProvider = ProviderName,
                ExternalId = albumId,
            });
        }
        return albums;
    }

    public Task<List<ExternalPlaylist>> SearchPlaylistsAsync(string query, int limit = 20)
        => Task.FromResult(new List<ExternalPlaylist>());

    public Task<ExternalPlaylist?> GetPlaylistAsync(string externalProvider, string externalId)
        => Task.FromResult<ExternalPlaylist?>(null);

    public Task<List<Song>> GetPlaylistTracksAsync(string externalProvider, string externalId)
        => Task.FromResult(new List<Song>());

    // ====== Short opaque ID format ======
    // Pipe-delimited fields, base64url where needed.
    //   yt|{videoId}|{artistB64}|{titleB64}|{durationSec}
    // Total length ~30-80 chars depending on artist/title length.

    public static string EncodeExternalId(SoulseekRouting r)
    {
        var artist = r.Artist ?? "";
        var title = r.Title ?? "";
        var dur = r.Duration?.ToString() ?? "";
        return $"yt|{r.YouTubeId ?? ""}|{B64UrlEncode(artist)}|{B64UrlEncode(title)}|{dur}";
    }

    public static SoulseekRouting? TryDecodeExternalId(string? externalId)
    {
        if (string.IsNullOrWhiteSpace(externalId)) return null;
        var parts = externalId.Split('|');
        if (parts.Length < 4 || parts[0] != "yt") return null;
        try
        {
            int? duration = null;
            if (parts.Length >= 5 && int.TryParse(parts[4], out var d)) duration = d;
            return new SoulseekRouting
            {
                YouTubeId = parts[1],
                Artist = B64UrlDecode(parts[2]),
                Title = B64UrlDecode(parts[3]),
                Duration = duration
            };
        }
        catch
        {
            return null;
        }
    }

    private static string B64UrlEncode(string s)
        => Convert.ToBase64String(Encoding.UTF8.GetBytes(s))
            .TrimEnd('=').Replace('+', '-').Replace('/', '_');

    private static string B64UrlDecode(string s)
    {
        s = s.Replace('-', '+').Replace('_', '/');
        switch (s.Length % 4)
        {
            case 2: s += "=="; break;
            case 3: s += "="; break;
        }
        return Encoding.UTF8.GetString(Convert.FromBase64String(s));
    }

    private static (string artist, string? title) ParseQuery(string query)
    {
        var trimmed = query.Trim();
        var idx = trimmed.IndexOf(' ');
        if (idx > 0) return (trimmed[..idx], trimmed[(idx + 1)..].Trim());
        return (trimmed, null);
    }
}

public enum RoutingKind
{
    Song = 0,
    Album = 1,
    Artist = 2,
}

public class SoulseekRouting
{
    public RoutingKind Kind { get; set; } = RoutingKind.Song;
    public string? YouTubeId { get; set; }
    public string? Artist { get; set; }
    public string? Title { get; set; }
    public string? Album { get; set; }
    public int? Duration { get; set; }

    /// <summary>Deezer album id, when an album search resolved one. Absent on album
    /// routings minted from a song row, which fall back to a name lookup.</summary>
    public string? ExternalAlbumId { get; set; }

    /// <summary>Deezer artist id behind an artist routing, once an artist search or an artist
    /// page settled on one. The id itself is minted from the name alone, and two artists can
    /// share a name, so this is what says which of them the page lists. Not part of the id.</summary>
    public string? ExternalArtistId { get; set; }

    /// <summary>Position within its album. Carried so a track downloaded as part of an
    /// album keeps its ordering: the download path rebuilds the song from its id alone,
    /// and without this every track lands untracked and sorts alphabetically.</summary>
    public int? Track { get; set; }

    /// <summary>Disc within a multi-disc release. Same reasoning as <see cref="Track"/>.</summary>
    public int? DiscNumber { get; set; }

    /// <summary>Track count of the album this came from. Without it the tagger fills the
    /// "x of y" denominator from a per-track Deezer search that can match a different
    /// release, producing nonsense like 5/10 on an 8-track album.</summary>
    public int? TotalTracks { get; set; }

    /// <summary>The track's ISRC, when the album listing that minted this routing named one.
    /// Carried for the same reason as <see cref="Track"/>, and because it is the strongest
    /// evidence download verification can be given about which recording was asked for.
    /// Not part of the id, so routings minted before it existed keep their ids.</summary>
    public string? Isrc { get; set; }

    /// <summary>The length shown for this song once a lookup found one, kept here so every
    /// later response carries it, across restarts too. Display only: see
    /// <see cref="SongLength"/> for why this is not <see cref="Duration"/>.</summary>
    public int? ShownDuration { get; set; }

    /// <summary>Where <see cref="ShownDuration"/> came from, so a weaker source never
    /// replaces a stronger one.</summary>
    public LengthSource ShownDurationSource { get; set; }

    /// <summary>Whether the words are explicit (1), the clean edit (3) or neither (0), as the
    /// catalog said, so every later response for this id carries it. Display only, not part of
    /// the id: see <see cref="Octo.Models.Domain.ExplicitStatus"/>.</summary>
    public int? ExplicitContent { get; set; }

    /// <summary>"clean" for a clean edit, which gets an id of its own so it can be listed beside
    /// the explicit original. Null for every other song or album, so every id minted before
    /// this existed (stars, playlists, pins) stays the same.</summary>
    public string? Version { get; set; }

    public bool HasYouTube => !string.IsNullOrEmpty(YouTubeId);
    public bool HasArtistTitle => !string.IsNullOrEmpty(Artist) && !string.IsNullOrEmpty(Title);
}
