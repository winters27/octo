using System.Text;
using System.Text.Json;
using Octo.Models.Domain;
using Octo.Models.Search;
using Octo.Models.Subsonic;
using Octo.Services.CoverArt;
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
    private readonly ILogger<SoulseekMetadataService> _logger;

    public SoulseekMetadataService(
        YouTubeResolver youtube,
        ExternalIdRegistry idRegistry,
        DeezerMetadataService deezer,
        CoverArtAggregator coverArt,
        ILogger<SoulseekMetadataService> logger)
    {
        _youtube = youtube;
        _idRegistry = idRegistry;
        _deezer = deezer;
        _coverArt = coverArt;
        _logger = logger;
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

        // 180 is the fallback when we don't know the real duration — most songs
        // are 3-5 min so it's a less-bad guess than 0 (which would prevent
        // clients from rendering a scrub bar at all).
        var effectiveDuration = durationSeconds ?? 180;

        return Task.FromResult(new List<Song>
        {
            new Song
            {
                Id = externalId,
                Title = title,
                Artist = artist,
                Album = "",
                Duration = effectiveDuration,
                IsLocal = false,
                ExternalProvider = ProviderName,
                ExternalId = externalId
            }
        });
    }

    // Deezer's real ceiling is ~50 requests per 5 seconds, and this runs on search3's
    // critical path, so the AWAITED set has to stay small. 60 rows at 8-way concurrency
    // was roughly 65 requests/second on its own, which is what exhausted the quota and
    // poisoned the metadata caches (issue #8).
    //
    // 12 is the same "first page" figure PrewarmYouTubeIdsAsync already uses. It must
    // stay above TopDurationResolveLimit, or the rows that get a YouTube length hint
    // would be reading a duration nobody resolved.
    private const int SearchEnrichLimit = 12;

    // Rows past the first page are warmed off the critical path, so per-row detail
    // calls (getSong, the native song endpoint) hit a populated cache instead of
    // paying for the lookup while a user waits.
    private const int BackgroundEnrichLimit = 60;

    public async Task EnrichExternalSongsAsync(List<Song> songs, CancellationToken ct = default)
    {
        var external = songs.Where(s => !s.IsLocal).ToList();

        var sem = new SemaphoreSlim(8);
        var tasks = external.Take(SearchEnrichLimit).Select(async song =>
        {
            await sem.WaitAsync(ct);
            try
            {
                var meta = await _deezer.EnrichTrackAsync(song.Artist, song.Title, includeYear: true, ct: ct);
                if (meta is null) return;
                if (meta.Duration is int d && d > 0) song.Duration = d;
                if (!string.IsNullOrWhiteSpace(meta.AlbumTitle)) song.Album = meta.AlbumTitle;
                if (meta.Year is int y) song.Year = y;

                // Reflect onto the shared routing so getSong stays consistent.
                var routing = _idRegistry.Lookup(song.Id);
                if (routing != null)
                {
                    if (meta.Duration is int rd && rd > 0) routing.Duration = rd;
                    if (!string.IsNullOrWhiteSpace(meta.AlbumTitle)) routing.Album = meta.AlbumTitle;
                }
            }
            catch { /* best-effort; a miss just leaves the 180s fallback */ }
            finally { sem.Release(); }
        });
        await Task.WhenAll(tasks);

        WarmRemainingInBackground(external.Skip(SearchEnrichLimit).Take(BackgroundEnrichLimit - SearchEnrichLimit).ToList());
    }

    /// <summary>
    /// Populate the Deezer cache for rows below the first page.
    ///
    /// These deliberately do NOT write back to the Song or its routing. Those objects
    /// are being serialised into the response as this runs, and Song.Duration is an
    /// int? whose non-atomic write can be read back as 0 — which is precisely the value
    /// that stops a client drawing a scrub bar. Writing Album mid-loop would also split
    /// one album across two synthetic ids. Cache only.
    /// </summary>
    private void WarmRemainingInBackground(List<Song> songs)
    {
        if (songs.Count == 0) return;

        _ = Task.Run(async () =>
        {
            foreach (var song in songs)
            {
                // Sequential on purpose: this has no deadline, and fanning out here is
                // what would eat the quota the awaited set needs.
                try { await _deezer.EnrichTrackAsync(song.Artist, song.Title, includeYear: false, background: true); }
                catch { /* best-effort */ }
            }
        });
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

    public async Task ResolveTopDurationsAsync(List<Song> songs, CancellationToken ct = default)
    {
        var tasks = songs.Where(s => !s.IsLocal).Take(TopDurationResolveLimit).Select(async song =>
        {
            if (!await _prewarmGate.WaitAsync(PrewarmQueueWait, ct)) return;
            try
            {
                // Fast metadata-only lookup (flat search, no URL solve). Pass the
                // Deezer duration as a hint so it picks the closest-length canonical
                // video (not a long-form/compilation upload); playback reuses the
                // stored videoId, so the shown length matches the audio.
                var hit = await _youtube.MetaAsync($"{song.Artist} {song.Title}", song.Duration, ct: ct);
                if (hit is { VideoId.Length: > 0 } && hit.Duration is int d && d > 0)
                {
                    song.Duration = d;
                    var routing = _idRegistry.Lookup(song.Id);
                    if (routing != null)
                    {
                        routing.YouTubeId = hit.VideoId; // playback reuses this exact video
                        routing.Duration = d;
                    }
                }
            }
            catch { /* best-effort; keeps the existing duration on a miss */ }
            finally { _prewarmGate.Release(); }
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

        foreach (var hit in hits)
        {
            // The registry id is the external id everywhere: getAlbum, getCoverArt and
            // star all round-trip through it. The Deezer id rides along on the routing
            // so album detail can fetch the exact tracklist without a name lookup.
            var albumId = _idRegistry.Register(new SoulseekRouting
            {
                Kind = RoutingKind.Album,
                Artist = hit.Artist,
                Album = hit.Title,
                ExternalAlbumId = hit.DeezerId,
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
                IsLocal = false,
                ExternalProvider = ProviderName,
                ExternalId = albumId,
            });
        }

        return albums;
    }

    public Task<List<Artist>> SearchArtistsAsync(string query, int limit = 20)
        => Task.FromResult(new List<Artist>());

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
            _logger.LogWarning(
                "getAlbum '{Artist} - {Album}' ({Id}): no Deezer album id resolved; returning album without a tracklist",
                routing.Artist, placeholder, externalId);
            return album;
        }

        var detail = await _deezer.GetAlbumDetailAsync(deezerAlbumId);
        // An album with no resolvable tracklist must still render, so fall through with
        // whatever we already have rather than failing the request.
        if (detail is null)
        {
            _logger.LogWarning(
                "getAlbum '{Artist} - {Album}' ({Id}): Deezer album {DeezerId} returned no usable detail "
                + "(see the deezer warning above for why); returning album without a tracklist",
                routing.Artist, placeholder, externalId, deezerAlbumId);
            return album;
        }

        album.Title = detail.Title;
        album.Year = detail.Year ?? album.Year;
        album.Genre = detail.Genre;
        album.CoverArtUrl = detail.CoverUrl ?? album.CoverArtUrl;
        // Defence in depth: the Deezer layer no longer returns a tracklist-less album,
        // but if one ever gets through, reporting zero is worse than saying nothing.
        if (detail.Tracks.Count > 0) album.SongCount = detail.Tracks.Count;
        if (!string.IsNullOrWhiteSpace(detail.Artist)) album.Artist = detail.Artist;

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

    public async Task<Artist?> GetArtistAsync(string externalProvider, string externalId)
    {
        if (!string.Equals(externalProvider, ProviderName, StringComparison.OrdinalIgnoreCase)) return null;
        var routing = _idRegistry.Lookup(externalId);
        if (routing is null) return null;

        var meta = await _deezer.EnrichArtistAsync(routing.Artist);
        return new Artist
        {
            Id = externalId,
            Name = meta?.Name ?? routing.Artist ?? "",
            ImageUrl = meta?.ImageUrl,
            IsLocal = false,
            ExternalProvider = ProviderName,
            ExternalId = externalId,
        };
    }

    public Task<List<Album>> GetArtistAlbumsAsync(string externalProvider, string externalId)
        => Task.FromResult(new List<Album>());

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

    public bool HasYouTube => !string.IsNullOrEmpty(YouTubeId);
    public bool HasArtistTitle => !string.IsNullOrEmpty(Artist) && !string.IsNullOrEmpty(Title);
}
