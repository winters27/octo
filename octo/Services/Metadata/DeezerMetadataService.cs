using System.Collections.Concurrent;
using System.Text.Json;
using Microsoft.Extensions.Caching.Memory;
using Microsoft.Extensions.Options;
using Octo.Models.Settings;

namespace Octo.Services.Metadata;

/// <summary>
/// Enriches external (YouTube-resolved) tracks with real album/artist metadata
/// from Deezer's public API. Keyless and no ARL — the ARL that expires on the
/// music bot is only for Deezer AUDIO; metadata endpoints are open.
///
/// Everything here is best-effort and cached: a Deezer outage, throttle, or miss
/// returns null, and callers fall back to a synthetic entity. Nothing on this
/// path ever blocks or fails playback.
/// </summary>
public class DeezerMetadataService : IDisposable
{
    public record TrackMeta(string? AlbumTitle, string? AlbumCoverUrl, int? Year, int? Duration,
        string? ArtistName, string? ArtistImageUrl);
    public record ArtistMeta(string? Name, string? ImageUrl);

    /// <summary>Everything Deezer knows about a track, for writing rich file tags.</summary>
    public record FullTrackMeta(
        string? AlbumTitle, string? AlbumCoverUrl, int? Year, int? Duration, string? ArtistName,
        int? TrackNumber, int? DiscNumber, string? Isrc, int? TotalTracks, string? Genre,
        string? Label, string? ReleaseDate);

    /// <summary>One album from a catalog search. Year is not on the search payload;
    /// the detail call fills it.</summary>
    public record AlbumHit(string DeezerId, string Title, string Artist,
        string? CoverUrl, int? Year, int TrackCount, string? RecordType);

    /// <summary>One track of an album, with the real length and position.</summary>
    public record AlbumTrack(string Title, string Artist, int? Duration,
        int? TrackPosition, int? DiscNumber, string? Isrc);

    /// <summary>An album plus its full tracklist.</summary>
    public record AlbumDetail(string DeezerId, string Title, string Artist,
        string? CoverUrl, int? Year, string? Genre, string? Label, List<AlbumTrack> Tracks);

    private const string Base = "https://api.deezer.com";
    private const int MaxCache = 4096;

    /// <summary>
    /// The only Deezer error code meaning "this genuinely does not exist". Everything
    /// else, including quota (code 4) and any code we do not recognise, is treated as
    /// transient. Caching an error we do not understand is exactly how one throttled
    /// call turned into an album that reported zero tracks for the life of the process.
    /// </summary>
    private const int DefinitiveErrorCode = 800;

    /// <summary>
    /// Result of one Deezer call. Deezer answers HTTP 200 even when it is refusing the
    /// request, so "we parsed a document" is not the same as "the call succeeded", and
    /// callers must never cache anything derived from a transient failure.
    /// </summary>
    private sealed class DeezerResponse : IDisposable
    {
        public JsonDocument? Doc { get; init; }

        /// <summary>Failed in a way that may succeed later. Nothing about this call
        /// may be written to a cache.</summary>
        public bool Transient { get; init; }

        public void Dispose() => Doc?.Dispose();
    }

    /// <summary>Good answers are stable, so this only needs to be short enough that a
    /// long-lived container eventually picks up catalog corrections.</summary>
    private static readonly TimeSpan PositiveTtl = TimeSpan.FromHours(12);

    /// <summary>"Deezer answered and this does not exist." Still expires, because the
    /// catalog gains releases.</summary>
    private static readonly TimeSpan NegativeTtl = TimeSpan.FromMinutes(5);

    /// <summary>A tracklist we know is incomplete. Usable now, refetched soon.</summary>
    private static readonly TimeSpan PartialTtl = TimeSpan.FromMinutes(10);

    private readonly IHttpClientFactory _httpFactory;
    private readonly IOptionsMonitor<MetadataSettings> _metadataOptions;
    private readonly ILogger<DeezerMetadataService> _logger;

    // Owned rather than injected from DI: metadata records are tens of bytes and
    // cover-art blobs are hundreds of kilobytes, so a single shared SizeLimit cannot
    // be right for both. Every entry counts as 1, so the limit is an entry count.
    private readonly MemoryCache _cache = new(new MemoryCacheOptions { SizeLimit = MaxCache });

    // Single-flight the album-year fetch: many tracks in one search share an album
    // (a whole album's tracks), so concurrent lookups collapse onto one HTTP call.
    private readonly ConcurrentDictionary<long, Lazy<Task<(int? Year, bool Transient)>>> _albumYearTasks = new();

    /// <summary>Wrapper so a cached null is distinguishable from a cache miss.</summary>
    private sealed record Entry<T>(T Value);

    private bool TryGetCached<T>(string key, out T? value)
    {
        if (_cache.TryGetValue(key, out Entry<T>? e)) { value = e!.Value; return true; }
        value = default;
        return false;
    }

    private void Put<T>(string key, T value, TimeSpan ttl) =>
        _cache.Set(key, new Entry<T>(value), new MemoryCacheEntryOptions
        {
            Size = 1,
            AbsoluteExpirationRelativeToNow = ttl,
        });

    /// <summary>Drop every cached answer. Exposed so a poisoned cache can be cleared
    /// without restarting the container.</summary>
    public void ClearCaches()
    {
        _cache.Clear();
        _albumYearTasks.Clear();
        _logger.LogInformation("deezer metadata caches cleared");
    }

    public void Dispose() => _cache.Dispose();

    public DeezerMetadataService(IHttpClientFactory httpFactory,
        IOptionsMonitor<MetadataSettings> metadataOptions,
        ILogger<DeezerMetadataService> logger)
    {
        _httpFactory = httpFactory;
        _metadataOptions = metadataOptions;
        _logger = logger;
    }

    private HttpClient Client()
    {
        // Named so the rate-limiting handler is in the chain. Resolving the default
        // client here would silently bypass Deezer's budget.
        var c = _httpFactory.CreateClient(DeezerRateLimiter.ClientName);
        c.Timeout = TimeSpan.FromSeconds(8);
        // Genre names in album payloads localize to the caller's IP country
        // unless this header pins them. Applied per creation, so a settings
        // change reaches the next lookup without a restart.
        AcceptLanguageHeader.Apply(c, _metadataOptions.CurrentValue);
        return c;
    }

    private static string TrackKey(string? artist, string? title) =>
        $"t|{artist}|{title}".ToLowerInvariant();

    /// <summary>
    /// What is already known about a track, or null when nothing is. Never makes a
    /// request, so a caller on a latency budget can complete the rows it knows without
    /// paying for the ones it does not. Shares <see cref="TrackKey"/> with
    /// <see cref="EnrichTrackAsync"/>, because a lookup keyed differently from the write
    /// would silently never hit.
    /// </summary>
    public TrackMeta? CachedTrack(string? artist, string? title)
    {
        if (string.IsNullOrWhiteSpace(artist) && string.IsNullOrWhiteSpace(title)) return null;
        return TryGetCached<TrackMeta?>(TrackKey(artist, title), out var cached) ? cached : null;
    }

    /// <summary>Resolve "artist + title" to the real album + artist (name, art, year).
    /// Pass includeYear=false to skip the extra album-detail call (bulk enrichment
    /// wants duration + album fast; the year is fetched lazily by the album view).</summary>
    public async Task<TrackMeta?> EnrichTrackAsync(string? artist, string? title, bool includeYear = true,
        bool background = false, CancellationToken ct = default)
    {
        if (string.IsNullOrWhiteSpace(artist) && string.IsNullOrWhiteSpace(title)) return null;
        var key = TrackKey(artist, title);
        if (TryGetCached<TrackMeta?>(key, out var cached)) return cached;

        TrackMeta? meta = null;
        // Set when the year alone could not be resolved. The rest of the record is still
        // good and is returned; it just must not be remembered, or a throttle blip would
        // be cached as "this track has no year" for the life of the entry.
        var yearUnresolved = false;
        try
        {
            var q = Uri.EscapeDataString(PlainQuery(artist, title));
            using var r = await GetJsonAsync($"{Base}/search?q={q}&limit={MatchCandidates}", ct, background);
            if (r.Transient) return null;
            if (BestMatch(r.Doc, artist, title) is JsonElement t)
            {
                string? albTitle = null, cover = null, artName = null, artImg = null;
                long albId = 0;
                if (t.TryGetProperty("album", out var alb))
                {
                    albTitle = Str(alb, "title");
                    cover = Str(alb, "cover_xl") ?? Str(alb, "cover_medium");
                    if (alb.TryGetProperty("id", out var aid) && aid.ValueKind == JsonValueKind.Number)
                        albId = aid.GetInt64();
                }
                if (t.TryGetProperty("artist", out var art))
                {
                    artName = Str(art, "name");
                    artImg = Str(art, "picture_xl") ?? Str(art, "picture_medium");
                }
                int? duration = t.TryGetProperty("duration", out var du) && du.ValueKind == JsonValueKind.Number
                    ? du.GetInt32() : null;
                int? year = null;
                if (includeYear && albId > 0)
                {
                    var (y, yearTransient) = await AlbumYearAsync(albId, ct);
                    // Degrade to "no year", never to "no track". Returning null here threw
                    // away an album title and duration the search had already fetched, so a
                    // throttled year left the song with no length at all.
                    year = y;
                    yearUnresolved = yearTransient;
                }
                meta = new TrackMeta(albTitle, cover, year, duration, artName, artImg);
            }
        }
        catch (Exception ex)
        {
            _logger.LogDebug("deezer enrich track '{A} - {T}' failed: {M}", artist, title, ex.Message);
        }

        // Usable now, refetched next time, so the year gets another chance.
        if (yearUnresolved) return meta;

        Put(key, meta, meta is null ? NegativeTtl : PositiveTtl);
        return meta;
    }

    /// <summary>
    /// Full track metadata for tagging a downloaded file: one track search (album,
    /// cover_xl, artist, duration, track_position, disk_number, isrc) plus one album
    /// detail call (release year, genre, total tracks, label). Cached; best-effort.
    /// </summary>
    public async Task<FullTrackMeta?> EnrichTrackFullAsync(string? artist, string? title, CancellationToken ct = default)
    {
        if (string.IsNullOrWhiteSpace(artist) && string.IsNullOrWhiteSpace(title)) return null;
        var key = $"full|{artist}|{title}".ToLowerInvariant();
        if (TryGetCached<FullTrackMeta?>(key, out var cached)) return cached;

        FullTrackMeta? meta = null;
        // The album detail carries the year, genre and label. If only that call fails the
        // track itself is still worth having, so it is returned uncached rather than lost:
        // a tagger with an album title and cover art beats one with nothing.
        var detailUnresolved = false;
        try
        {
            var q = Uri.EscapeDataString(PlainQuery(artist, title));
            using var r = await GetJsonAsync($"{Base}/search?q={q}&limit={MatchCandidates}", ct);
            if (r.Transient) return null;
            if (BestMatch(r.Doc, artist, title) is JsonElement t)
            {
                string? albTitle = null, cover = null, artName = null;
                var isrc = Str(t, "isrc");
                long albId = 0;
                if (t.TryGetProperty("album", out var alb))
                {
                    albTitle = Str(alb, "title");
                    cover = Str(alb, "cover_xl") ?? Str(alb, "cover_big") ?? Str(alb, "cover_medium");
                    if (alb.TryGetProperty("id", out var aid) && aid.ValueKind == JsonValueKind.Number)
                        albId = aid.GetInt64();
                }
                if (t.TryGetProperty("artist", out var art)) artName = Str(art, "name");

                int? year = null, totalTracks = null;
                string? genre = null, label = null, releaseDate = null;
                if (albId > 0)
                {
                    using var ar = await GetJsonAsync($"{Base}/album/{albId}", ct);
                    detailUnresolved = ar.Transient;
                    if (ar.Doc != null)
                    {
                        var root = ar.Doc.RootElement;
                        releaseDate = Str(root, "release_date");
                        if (!string.IsNullOrEmpty(releaseDate) && releaseDate.Length >= 4 && int.TryParse(releaseDate[..4], out var yr))
                            year = yr;
                        totalTracks = Int(root, "nb_tracks");
                        label = Str(root, "label");
                        if (root.TryGetProperty("genres", out var g) && g.TryGetProperty("data", out var gd)
                            && gd.ValueKind == JsonValueKind.Array && gd.GetArrayLength() > 0)
                            genre = Str(gd[0], "name");
                    }
                }

                meta = new FullTrackMeta(albTitle, cover, year, Int(t, "duration"), artName,
                    Int(t, "track_position"), Int(t, "disk_number"), isrc, totalTracks, genre, label, releaseDate);
            }
        }
        catch (Exception ex)
        {
            _logger.LogDebug("deezer full enrich '{A} - {T}' failed: {M}", artist, title, ex.Message);
        }

        // Usable now, refetched next time, so the album detail gets another chance.
        if (detailUnresolved) return meta;

        Put(key, meta, meta is null ? NegativeTtl : PositiveTtl);
        return meta;
    }

    /// <summary>Resolve an artist name to its Deezer name + image.</summary>
    public async Task<ArtistMeta?> EnrichArtistAsync(string? artist, CancellationToken ct = default)
    {
        if (string.IsNullOrWhiteSpace(artist)) return null;
        var key = $"a|{artist}".ToLowerInvariant();
        if (TryGetCached<ArtistMeta?>(key, out var cached)) return cached;

        ArtistMeta? meta = null;
        try
        {
            var q = Uri.EscapeDataString(artist);
            using var r = await GetJsonAsync($"{Base}/search/artist?q={q}&limit=1", ct);
            if (r.Transient) return null;
            if (FirstData(r.Doc) is JsonElement a)
                meta = new ArtistMeta(Str(a, "name"), Str(a, "picture_xl") ?? Str(a, "picture_medium"));
        }
        catch (Exception ex)
        {
            _logger.LogDebug("deezer enrich artist '{A}' failed: {M}", artist, ex.Message);
        }

        Put(key, meta, meta is null ? NegativeTtl : PositiveTtl);
        return meta;
    }

    /// <summary>One artist from a catalog search.</summary>
    public record ArtistHit(string DeezerId, string Name, string? PictureUrl, int AlbumCount);

    /// <summary>
    /// Search the catalog for artists. Plain query: the artist endpoint takes a bare name
    /// and the qualified form is dead everywhere now.
    /// </summary>
    public async Task<List<ArtistHit>> SearchArtistsAsync(string query, int limit, CancellationToken ct = default)
    {
        if (string.IsNullOrWhiteSpace(query) || limit <= 0) return new List<ArtistHit>();
        var key = $"ars|{query}|{limit}".ToLowerInvariant();
        if (TryGetCached<List<ArtistHit>>(key, out var cached)) return cached!;

        var hits = new List<ArtistHit>();
        try
        {
            var q = Uri.EscapeDataString(query);
            using var r = await GetJsonAsync($"{Base}/search/artist?q={q}&limit={limit}", ct);
            // Caching an empty list on a refusal is what would make external artists
            // silently vanish from search3 for the rest of the process.
            if (r.Transient) return new List<ArtistHit>();
            if (r.Doc is not null
                && r.Doc.RootElement.TryGetProperty("data", out var data)
                && data.ValueKind == JsonValueKind.Array)
            {
                // Materialize everything before the JsonDocument is disposed.
                foreach (var a in data.EnumerateArray())
                {
                    var id = a.TryGetProperty("id", out var aid) && aid.ValueKind == JsonValueKind.Number
                        ? aid.GetInt64().ToString() : null;
                    var name = Str(a, "name");
                    if (id is null || string.IsNullOrWhiteSpace(name)) continue;

                    hits.Add(new ArtistHit(id, name,
                        Str(a, "picture_xl") ?? Str(a, "picture_medium"),
                        Int(a, "nb_album") ?? 0));
                }
            }
        }
        catch (Exception ex)
        {
            _logger.LogDebug("deezer artist search '{Q}' failed: {M}", query, ex.Message);
        }

        Put(key, hits, hits.Count == 0 ? NegativeTtl : PositiveTtl);
        return hits;
    }

    /// <summary>Search the album catalog. Single-track "albums" are dropped: a plain
    /// artist query returns a lot of them and they crowd out real records.</summary>
    public async Task<List<AlbumHit>> SearchAlbumsAsync(string query, int limit, CancellationToken ct = default)
    {
        if (string.IsNullOrWhiteSpace(query) || limit <= 0) return new List<AlbumHit>();
        var key = $"as|{query}|{limit}".ToLowerInvariant();
        if (TryGetCached<List<AlbumHit>>(key, out var cached)) return cached!;

        var hits = new List<AlbumHit>();
        try
        {
            var q = Uri.EscapeDataString(query);
            using var r = await GetJsonAsync($"{Base}/search/album?q={q}&limit={limit}", ct);
            // Caching an empty list here is what would make external albums silently
            // vanish from search3 for the rest of the process.
            if (r.Transient) return new List<AlbumHit>();
            if (r.Doc is not null
                && r.Doc.RootElement.TryGetProperty("data", out var data)
                && data.ValueKind == JsonValueKind.Array)
            {
                // Materialize everything before the JsonDocument is disposed.
                foreach (var a in data.EnumerateArray())
                {
                    var id = a.TryGetProperty("id", out var aid) && aid.ValueKind == JsonValueKind.Number
                        ? aid.GetInt64().ToString() : null;
                    var title = Str(a, "title");
                    if (id is null || string.IsNullOrWhiteSpace(title)) continue;

                    var recordType = Str(a, "record_type");
                    var trackCount = Int(a, "nb_tracks") ?? 0;
                    if (string.Equals(recordType, "single", StringComparison.OrdinalIgnoreCase) && trackCount <= 2)
                        continue;

                    var artist = a.TryGetProperty("artist", out var art) ? Str(art, "name") : null;
                    hits.Add(new AlbumHit(
                        id, title, artist ?? "",
                        Str(a, "cover_xl") ?? Str(a, "cover_medium"),
                        null, trackCount, recordType));
                }
            }
        }
        catch (Exception ex)
        {
            _logger.LogDebug("deezer album search '{Q}' failed: {M}", query, ex.Message);
        }

        Put(key, hits, hits.Count == 0 ? NegativeTtl : PositiveTtl);
        return hits;
    }

    /// <summary>Same ceiling the album tracklist call uses. A discography longer than this
    /// is logged as truncated rather than silently presented as complete.</summary>
    private const int ArtistAlbumsLimit = 300;

    /// <summary>
    /// An artist's releases (albums, EPs, singles and compilations), for the artist page.
    /// This payload names neither the artist nor the track count, so Artist is empty and
    /// TrackCount is 0; the caller already knows the artist, and the real track count
    /// arrives with the album detail when the album is opened.
    /// </summary>
    public async Task<List<AlbumHit>> GetArtistAlbumsAsync(string deezerArtistId, CancellationToken ct = default)
    {
        if (string.IsNullOrWhiteSpace(deezerArtistId)) return new List<AlbumHit>();
        var key = $"aa|{deezerArtistId}";
        if (TryGetCached<List<AlbumHit>>(key, out var cached)) return cached!;

        var hits = new List<AlbumHit>();
        try
        {
            using var r = await GetJsonAsync($"{Base}/artist/{deezerArtistId}/albums?limit={ArtistAlbumsLimit}", ct);
            // Same rule as the searches: caching an empty list on a refusal would leave the
            // artist page empty for the life of the entry.
            if (r.Transient) return new List<AlbumHit>();
            if (r.Doc is not null
                && r.Doc.RootElement.TryGetProperty("data", out var data)
                && data.ValueKind == JsonValueKind.Array)
            {
                // Materialize everything before the JsonDocument is disposed.
                foreach (var a in data.EnumerateArray())
                {
                    var id = a.TryGetProperty("id", out var aid) && aid.ValueKind == JsonValueKind.Number
                        ? aid.GetInt64().ToString() : null;
                    var title = Str(a, "title");
                    if (id is null || string.IsNullOrWhiteSpace(title)) continue;

                    // Unlike the search payload, this one carries the release date.
                    int? year = null;
                    var rd = Str(a, "release_date");
                    if (!string.IsNullOrEmpty(rd) && rd.Length >= 4 && int.TryParse(rd[..4], out var yr))
                        year = yr;

                    hits.Add(new AlbumHit(
                        id, title, "",
                        Str(a, "cover_xl") ?? Str(a, "cover_medium"),
                        year, 0, Str(a, "record_type")));
                }

                // Compared against what Deezer sent, not what was kept, so a skipped row
                // with no title is not mistaken for truncation.
                var total = Int(r.Doc.RootElement, "total");
                var returned = data.GetArrayLength();
                if (total is int n && n > returned)
                {
                    _logger.LogWarning(
                        "deezer artist {Id} has {Total} releases but {Got} were returned; discography is truncated",
                        deezerArtistId, n, returned);
                }
            }
        }
        catch (Exception ex)
        {
            _logger.LogDebug("deezer artist albums {Id} failed: {M}", deezerArtistId, ex.Message);
        }

        Put(key, hits, hits.Count == 0 ? NegativeTtl : PositiveTtl);
        return hits;
    }

    /// <summary>Resolve an artist + album name to a Deezer album id. Needed because album
    /// ids minted from a song row carry no Deezer id, so the name is all we have.</summary>
    public async Task<string?> FindAlbumIdAsync(string? artist, string? album, CancellationToken ct = default)
    {
        if (string.IsNullOrWhiteSpace(album)) return null;
        var key = $"ai|{artist}|{album}".ToLowerInvariant();
        if (TryGetCached<string?>(key, out var cached)) return cached;

        string? id = null;
        try
        {
            var q = Uri.EscapeDataString(PlainQuery(artist, album));
            using var r = await GetJsonAsync($"{Base}/search/album?q={q}&limit={MatchCandidates}", ct);
            if (r.Transient) return null;
            if (BestMatch(r.Doc, artist, album) is JsonElement a
                && a.TryGetProperty("id", out var aid) && aid.ValueKind == JsonValueKind.Number)
            {
                id = aid.GetInt64().ToString();
            }
        }
        catch (Exception ex)
        {
            _logger.LogDebug("deezer album id lookup '{A} - {Al}' failed: {M}", artist, album, ex.Message);
        }

        Put(key, id, id is null ? NegativeTtl : PositiveTtl);
        return id;
    }

    /// <summary>Album detail plus its full tracklist, ordered by disc then track position.
    /// One bounded request per resource; a release larger than the cap is reported as
    /// truncated rather than silently presented as complete.</summary>
    public async Task<AlbumDetail?> GetAlbumDetailAsync(string deezerId, CancellationToken ct = default)
    {
        if (string.IsNullOrWhiteSpace(deezerId)) return null;
        var cacheKey = $"ad|{deezerId}";
        if (TryGetCached<AlbumDetail?>(cacheKey, out var cached)) return cached;

        AlbumDetail? detail = null;
        // A tracklist we know is short gets a shorter life than a complete one, so a
        // truncated or partly-skipped album repairs itself instead of sticking.
        var partial = false;
        try
        {
            string title = "", artist = "", genre = "", label = "", cover = "";
            int? year = null;
            // Declared out here on purpose: the document below is disposed before the
            // tracklist call, and this is what tells an empty tracklist apart from an
            // album that genuinely has no tracks.
            int? nbTracks = null;

            using (var r = await GetJsonAsync($"{Base}/album/{deezerId}", ct))
            {
                if (r.Transient) return null;
                if (r.Doc is not null)
                {
                    var root = r.Doc.RootElement;
                    nbTracks = Int(root, "nb_tracks");
                    title = Str(root, "title") ?? "";
                    cover = Str(root, "cover_xl") ?? Str(root, "cover_medium") ?? "";
                    label = Str(root, "label") ?? "";
                    var rd = Str(root, "release_date");
                    if (!string.IsNullOrEmpty(rd) && rd.Length >= 4 && int.TryParse(rd[..4], out var yr))
                        year = yr;
                    if (root.TryGetProperty("artist", out var art))
                        artist = Str(art, "name") ?? "";
                    if (root.TryGetProperty("genres", out var genres)
                        && genres.TryGetProperty("data", out var gd)
                        && gd.ValueKind == JsonValueKind.Array && gd.GetArrayLength() > 0)
                        genre = Str(gd[0], "name") ?? "";
                }
            }

            // Deezer answered and there is no such album. Cacheable, but not forever.
            if (string.IsNullOrWhiteSpace(title)) { Put(cacheKey, (AlbumDetail?)null, NegativeTtl); return null; }

            var tracks = new List<AlbumTrack>();
            using (var tr = await GetJsonAsync($"{Base}/album/{deezerId}/tracks?limit=300", ct))
            {
                // The album call can succeed while the tracklist call is throttled. That
                // built a perfectly valid AlbumDetail carrying title, year and genre with
                // an empty tracklist, cached it permanently, and is why getAlbum reported
                // songCount 0 forever while still showing real metadata.
                if (tr.Transient) return null;
                if (tr.Doc is not null
                    && tr.Doc.RootElement.TryGetProperty("data", out var data)
                    && data.ValueKind == JsonValueKind.Array)
                {
                    foreach (var t in data.EnumerateArray())
                    {
                        var tTitle = Str(t, "title");
                        if (string.IsNullOrWhiteSpace(tTitle)) continue;
                        var tArtist = t.TryGetProperty("artist", out var ta) ? Str(ta, "name") : null;
                        tracks.Add(new AlbumTrack(
                            tTitle, tArtist ?? artist, Int(t, "duration"),
                            Int(t, "track_position"), Int(t, "disk_number"), Str(t, "isrc")));
                    }

                    var total = Int(tr.Doc.RootElement, "total");
                    if (total is int n && n > tracks.Count)
                    {
                        partial = true;
                        _logger.LogWarning(
                            "deezer album '{Title}' ({Id}) returned {Got} of {Total} tracks; tracklist is truncated",
                            title, deezerId, tracks.Count, n);
                    }
                }
            }

            // An empty tracklist on an album Deezer says HAS tracks is a failure, not an
            // answer. Note the null check is load-bearing: Int() returns int?, and a lifted
            // `nbTracks > 0` is FALSE when nb_tracks is absent, so testing that alone would
            // let the empty result through and cache it exactly as before.
            if (tracks.Count == 0 && (nbTracks is null || nbTracks > 0))
            {
                _logger.LogWarning(
                    "deezer album '{Title}' ({Id}) reports {Expected} track(s) but returned none; not caching",
                    title, deezerId, nbTracks?.ToString() ?? "an unknown number of");
                return null;
            }

            // Fewer tracks than the album claims, e.g. entries skipped for a blank title.
            if (nbTracks is int expected && tracks.Count < expected) partial = true;

            tracks = tracks
                .OrderBy(t => t.DiscNumber ?? 1)
                .ThenBy(t => t.TrackPosition ?? int.MaxValue)
                .ToList();

            detail = new AlbumDetail(deezerId, title, artist,
                string.IsNullOrEmpty(cover) ? null : cover, year,
                string.IsNullOrEmpty(genre) ? null : genre,
                string.IsNullOrEmpty(label) ? null : label,
                tracks);
        }
        catch (Exception ex)
        {
            _logger.LogDebug("deezer album detail {Id} failed: {M}", deezerId, ex.Message);
        }

        Put(cacheKey, detail, detail is null ? NegativeTtl : partial ? PartialTtl : PositiveTtl);
        return detail;
    }

    private Task<(int? Year, bool Transient)> AlbumYearAsync(long albumId, CancellationToken ct)
    {
        if (TryGetCached<int?>($"y|{albumId}", out var y)) return Task.FromResult<(int?, bool)>((y, false));
        // Shared across concurrent callers for the same album id (single-flight).
        return _albumYearTasks.GetOrAdd(albumId,
            id => new Lazy<Task<(int? Year, bool Transient)>>(() => FetchAlbumYearAsync(id))).Value;
    }

    private async Task<(int? Year, bool Transient)> FetchAlbumYearAsync(long albumId)
    {
        int? year = null;
        using var r = await GetJsonAsync($"{Base}/album/{albumId}", CancellationToken.None);
        _albumYearTasks.TryRemove(albumId, out _);

        // This used to be a raw indexer write after a bare catch, so it bypassed the
        // cache helper entirely and a throttled year lookup stuck permanently.
        if (r.Transient) return (null, true);

        var rd = r.Doc is null ? null : Str(r.Doc.RootElement, "release_date");
        if (!string.IsNullOrEmpty(rd) && rd.Length >= 4 && int.TryParse(rd[..4], out var yr))
            year = yr;
        Put($"y|{albumId}", year, year is null ? NegativeTtl : PositiveTtl);
        return (year, false);
    }

    private async Task<DeezerResponse> GetJsonAsync(string url, CancellationToken ct, bool background = false)
    {
        JsonDocument? doc = null;
        try
        {
            using var req = new HttpRequestMessage(HttpMethod.Get, url);
            if (background) req.Options.Set(DeezerRateLimitHandler.BackgroundLane, true);

            using var resp = await Client().SendAsync(req, ct);
            // A 429 here is usually our OWN limiter shedding load rather than Deezer's,
            // and either way it is transient, so nothing derived from it is cached.
            if (!resp.IsSuccessStatusCode) return new DeezerResponse { Transient = true };

            var s = await resp.Content.ReadAsStringAsync(ct);
            doc = JsonDocument.Parse(s);

            // Deezer reports throttling as 200 + {"error":{"code":4,...}}, which parses
            // perfectly and then reads as "the album has no tracks". Catching it here is
            // what stops a quota blip becoming permanent cached state.
            if (doc.RootElement.ValueKind == JsonValueKind.Object
                && doc.RootElement.TryGetProperty("error", out var err)
                && err.ValueKind == JsonValueKind.Object)
            {
                var code = Int(err, "code");
                var definitive = code == DefinitiveErrorCode;
                _logger.LogWarning("deezer refused {Url}: {Type} \"{Msg}\" (code {Code}, treated as {Kind})",
                    url, Str(err, "type"), Str(err, "message"), code, definitive ? "definitive" : "transient");
                doc.Dispose();
                return new DeezerResponse { Transient = !definitive };
            }

            var ok = new DeezerResponse { Doc = doc };
            doc = null;
            return ok;
        }
        catch (Exception ex)
        {
            doc?.Dispose();
            _logger.LogDebug("deezer request {Url} failed: {M}", url, ex.Message);
            return new DeezerResponse { Transient = true };
        }
    }

    /// <summary>
    /// Deezer no longer supports field-qualified search on the track endpoints. A query
    /// like artist:"X" track:"Y" is now read as free text, so the literal words "artist"
    /// and "track" have to appear in the record and nothing ever matches. Plain terms are
    /// the only shape that still works.
    /// </summary>
    private static string PlainQuery(params string?[] parts) =>
        string.Join(' ', parts.Where(p => !string.IsNullOrWhiteSpace(p)).Select(p => p!.Trim()));

    /// <summary>How many hits to weigh before giving up. A plain query is fuzzier than a
    /// qualified one was, so it puts covers, karaoke and live takes next to the real
    /// recording and the first row is not reliably the right one.</summary>
    private const int MatchCandidates = 5;

    /// <summary>Letters and digits only, so punctuation, case and spacing cannot decide
    /// a match.</summary>
    private static string MatchKey(string? value) =>
        new((value ?? string.Empty).Normalize().ToLowerInvariant().Where(char.IsLetterOrDigit).ToArray());

    /// <summary>What one field of a hit says about the request.</summary>
    private enum FieldVerdict { Match, Absent, Mismatch }

    /// <summary>
    /// Compare one field. Containment rather than equality, because Deezer decorates
    /// titles ("Reckoner (Remastered)") and credits features in the artist field; an exact
    /// compare rejects the correct recording far more often than it rejects a wrong one.
    ///
    /// A field either side left empty is <see cref="FieldVerdict.Absent"/>, never a
    /// mismatch. Absent evidence is not counter-evidence, and treating a field Deezer
    /// simply did not send as a contradiction would throw away good hits the moment the
    /// payload shape changes.
    /// </summary>
    private static FieldVerdict Compare(string want, string got)
    {
        if (want.Length == 0 || got.Length == 0) return FieldVerdict.Absent;
        return got.Contains(want) || want.Contains(got) ? FieldVerdict.Match : FieldVerdict.Mismatch;
    }

    /// <summary>
    /// The first hit that positively matches on artist or title and contradicts on
    /// neither. This is the guard that makes a plain query safe to use in place of the
    /// qualified one: without it a near-miss at position 0 would be attached to the song
    /// as fact. Requiring at least one positive match is what stops a hit that states
    /// nothing at all from matching everything.
    /// </summary>
    private static JsonElement? BestMatch(JsonDocument? doc, string? artist, string? title)
    {
        if (doc is null) return null;
        if (!doc.RootElement.TryGetProperty("data", out var data)
            || data.ValueKind != JsonValueKind.Array) return null;

        var wantArtist = MatchKey(artist);
        var wantTitle = MatchKey(title);

        foreach (var hit in data.EnumerateArray())
        {
            var titleVerdict = Compare(wantTitle, MatchKey(Str(hit, "title")));
            var artistVerdict = Compare(wantArtist,
                MatchKey(hit.TryGetProperty("artist", out var a) ? Str(a, "name") : null));

            if (titleVerdict == FieldVerdict.Mismatch || artistVerdict == FieldVerdict.Mismatch) continue;
            if (titleVerdict == FieldVerdict.Match || artistVerdict == FieldVerdict.Match) return hit;
        }
        return null;
    }

    private static JsonElement? FirstData(JsonDocument? doc)
    {
        if (doc is null) return null;
        if (doc.RootElement.TryGetProperty("data", out var d) && d.ValueKind == JsonValueKind.Array && d.GetArrayLength() > 0)
            return d[0];
        return null;
    }

    private static string? Str(JsonElement e, string name)
        => e.TryGetProperty(name, out var v) && v.ValueKind == JsonValueKind.String ? v.GetString() : null;

    private static int? Int(JsonElement e, string name)
        => e.TryGetProperty(name, out var v) && v.ValueKind == JsonValueKind.Number ? v.GetInt32() : (int?)null;

}
