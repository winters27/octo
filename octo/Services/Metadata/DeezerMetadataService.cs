using System.Collections.Concurrent;
using System.Text.Json;
using Microsoft.Extensions.Caching.Memory;
using Microsoft.Extensions.Options;
using Octo.Models.Settings;
using Octo.Services.Common;

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
public partial class DeezerMetadataService : IDisposable
{
    public record TrackMeta(string? AlbumTitle, string? AlbumCoverUrl, int? Year, int? Duration,
        string? ArtistName, string? ArtistImageUrl)
    {
        /// <summary>Whether the song's words are explicit (1), the clean edit (3) or neither (0),
        /// null when the catalog does not say. See <see cref="Octo.Models.Domain.ExplicitStatus"/>.</summary>
        public int? ExplicitContent { get; init; }
    }
    public record ArtistMeta(string? Name, string? ImageUrl);

    /// <summary>
    /// Everything Deezer knows about a track, for writing rich file tags. Contributors is every
    /// Main and Featured artist, in order; the search hit only names the main one, so it comes
    /// from the track's own record. AlbumArtistName and RecordType come from the album, and are
    /// what tell a compilation apart: Deezer usually reports one as record_type "album", so the
    /// album artist "Various Artists" is the signal that holds.
    /// </summary>
    public record FullTrackMeta(
        string? AlbumTitle, string? AlbumCoverUrl, int? Year, int? Duration, string? ArtistName,
        int? TrackNumber, int? DiscNumber, string? Isrc, int? TotalTracks, string? Genre,
        string? Label, string? ReleaseDate, IReadOnlyList<string>? Contributors = null,
        string? AlbumArtistName = null, string? RecordType = null)
    {
        /// <summary>The track's own title and ids in the catalog, the album's barcode, and what
        /// the catalog says about the words and the loudness, for the chooser and its report.</summary>
        public string? Title { get; init; }
        public string? TrackId { get; init; }
        public string? AlbumId { get; init; }
        public string? Barcode { get; init; }
        public bool? ExplicitLyrics { get; init; }
        /// <summary>The catalog's word for this version's lyrics: 0 not explicit, 1 explicit,
        /// 3 the clean edit; any other value says it does not know.</summary>
        public int? ExplicitContent { get; init; }
        public double? CatalogGain { get; init; }
    }

    /// <summary>The catalog's ranked answers for one song, or the fact that it did not answer.</summary>
    public sealed record CatalogCandidates(IReadOnlyList<FullTrackMeta> Hits, bool DidNotAnswer);

    /// <summary>One album from a catalog search. Year is not on the search payload;
    /// the detail call fills it.</summary>
    public record AlbumHit(string DeezerId, string Title, string Artist,
        string? CoverUrl, int? Year, int TrackCount, string? RecordType)
    {
        /// <summary>The listing's explicit flag. False is not "clean": a listing does not tell a
        /// clean edit from an album that never had explicit words.</summary>
        public bool? ExplicitLyrics { get; init; }
    }

    /// <summary>One track of an album, with the real length and position.</summary>
    public record AlbumTrack(string Title, string Artist, int? Duration,
        int? TrackPosition, int? DiscNumber, string? Isrc)
    {
        /// <summary>1 explicit, 3 the clean edit, 0 neither, null not said.</summary>
        public int? ExplicitContent { get; init; }
    }

    /// <summary>An album plus its full tracklist. RecordType is the catalog's own word for it:
    /// album, ep, single or compile.</summary>
    public record AlbumDetail(string DeezerId, string Title, string Artist,
        string? CoverUrl, int? Year, string? Genre, string? Label, List<AlbumTrack> Tracks,
        string? RecordType = null)
    {
        /// <summary>The album's own word: 1 explicit (or partly), 3 the clean edit, 0 neither.</summary>
        public int? ExplicitContent { get; init; }
    }

    /// <summary>What the catalog said when asked for an album's detail.</summary>
    public enum AlbumAnswer
    {
        /// <summary>The album and its tracklist.</summary>
        Found,
        /// <summary>Deezer answered, and it has no such album.</summary>
        NoSuchAlbum,
        /// <summary>Deezer knows the album, but its tracklist came back empty.</summary>
        NoTracks,
        /// <summary>Deezer did not answer this time: throttled, unreachable or unreadable.</summary>
        Unavailable,
    }

    /// <summary>An album's detail, when there is one, and what the catalog said.</summary>
    public sealed record AlbumLookup(AlbumDetail? Detail, AlbumAnswer Answer);

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

    /// <summary>Requests on their way to the catalog, by cache key. The cache only answers
    /// once a request is back, so two callers asking the same thing at once each asked.</summary>
    private readonly ConcurrentDictionary<string, Lazy<Task<object?>>> _inFlight = new();

    /// <summary>
    /// One request for any number of callers asking the same thing at once. The fetch writes
    /// the cache before it finishes, so a caller arriving after it has left finds the answer
    /// there. It runs without any one caller's token: a caller giving up stops waiting, and
    /// the others still get their answer.
    /// </summary>
    private async Task<T> SharedAsync<T>(string key, Func<Task<T>> fetch, CancellationToken ct)
    {
        var flight = _inFlight.GetOrAdd(key, k => new Lazy<Task<object?>>(async () =>
        {
            try { return await fetch(); }
            finally { _inFlight.TryRemove(k, out _); }
        }));
        return (T)(await flight.Value.WaitAsync(ct))!;
    }

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
            var (r, found) = await FindTrackAsync(artist, title, ct, background);
            using var response = r;
            if (r?.Transient == true) return null;
            if (found is JsonElement t)
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
                meta = new TrackMeta(albTitle, cover, year, duration, artName, artImg)
                {
                    ExplicitContent = SongExplicitness(r?.Doc, artist, title, t),
                };
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
            var (r, found) = await FindTrackAsync(artist, title, ct);
            using var response = r;
            if (r?.Transient == true) return null;
            if (found is JsonElement t)
                (meta, detailUnresolved) = await BuildFullMetaAsync(t, ct);
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

    /// <summary>
    /// The ranked hits for one song, not only the first: every hit of the first search that
    /// finds any, in the catalog's order, with the detail calls made for the best
    /// <paramref name="max"/>. The chooser weighs them against the other sources. A throttled
    /// catalog answers nothing and says so, and nothing from that call is remembered.
    /// </summary>
    public async Task<CatalogCandidates> EnrichTrackCandidatesAsync(string? artist, string? title, int max = 2,
        CancellationToken ct = default)
    {
        if (string.IsNullOrWhiteSpace(artist) && string.IsNullOrWhiteSpace(title)) return new([], false);
        max = Math.Clamp(max, 1, MatchCandidates);
        var key = $"cands|{artist}|{title}|{max}".ToLowerInvariant();
        if (TryGetCached<CatalogCandidates?>(key, out var cached) && cached is not null) return cached;

        var hits = new List<FullTrackMeta>();
        var detailUnresolved = false;
        try
        {
            var (r, found) = await FindTrackHitsAsync(artist, title, ct);
            using var response = r;
            if (r?.Transient == true) return new([], true);
            foreach (var hit in found.Take(max))
            {
                var (meta, unresolved) = await BuildFullMetaAsync(hit, ct);
                detailUnresolved |= unresolved;
                if (meta is not null) hits.Add(meta);
            }
        }
        catch (Exception ex)
        {
            _logger.LogDebug("deezer candidates '{A} - {T}' failed: {M}", artist, title, ex.Message);
        }

        var answer = new CatalogCandidates(hits, false);
        if (!detailUnresolved) Put(key, answer, hits.Count == 0 ? NegativeTtl : PositiveTtl);
        return answer;
    }

    /// <summary>One search hit made whole: the album's detail (year, genre, label, barcode,
    /// album artist, kind) and the track's own record (position, contributors, code, words,
    /// loudness). The second value says a detail call did not answer, so nothing is cached.</summary>
    private async Task<(FullTrackMeta? Meta, bool DetailUnresolved)> BuildFullMetaAsync(JsonElement t, CancellationToken ct)
    {
        var detailUnresolved = false;
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
        string? genre = null, label = null, releaseDate = null, albumArtist = null, recordType = null, barcode = null;
        if (albId > 0)
        {
            using var ar = await GetJsonAsync($"{Base}/album/{albId}", ct);
            detailUnresolved = ar.Transient;
            if (ar.Doc != null)
            {
                var root = ar.Doc.RootElement;
                recordType = Str(root, "record_type");
                if (root.TryGetProperty("artist", out var albumArt) && albumArt.ValueKind == JsonValueKind.Object)
                    albumArtist = Str(albumArt, "name");
                releaseDate = Str(root, "release_date");
                if (!string.IsNullOrEmpty(releaseDate) && releaseDate.Length >= 4 && int.TryParse(releaseDate[..4], out var yr))
                    year = yr;
                totalTracks = Int(root, "nb_tracks");
                label = Str(root, "label");
                barcode = Str(root, "upc");
                if (root.TryGetProperty("genres", out var g) && g.TryGetProperty("data", out var gd)
                    && gd.ValueKind == JsonValueKind.Array && gd.GetArrayLength() > 0)
                    genre = Str(gd[0], "name");
            }
        }

        // The search hit carries neither the track's position nor anyone but the main
        // artist, so the track number was never written (#48) and a collaboration was one
        // artist (#49). The track's own record has both.
        int? trackNumber = Int(t, "track_position"), discNumber = Int(t, "disk_number");
        List<string>? contributors = null;
        bool? explicitLyrics = t.TryGetProperty("explicit_lyrics", out var ex0) && ex0.ValueKind is JsonValueKind.True or JsonValueKind.False
            ? ex0.GetBoolean() : null;
        var explicitContent = Int(t, "explicit_content_lyrics");
        double? gain = null;
        string? trackId = null;
        if (t.TryGetProperty("id", out var tid) && tid.ValueKind == JsonValueKind.Number)
        {
            trackId = tid.GetInt64().ToString(System.Globalization.CultureInfo.InvariantCulture);
            using var tr = await GetJsonAsync($"{Base}/track/{tid.GetInt64()}", ct);
            detailUnresolved |= tr.Transient;
            if (tr.Doc != null)
            {
                var track = tr.Doc.RootElement;
                trackNumber ??= Int(track, "track_position");
                discNumber ??= Int(track, "disk_number");
                isrc ??= Str(track, "isrc");
                if (track.TryGetProperty("explicit_lyrics", out var ex1) && ex1.ValueKind is JsonValueKind.True or JsonValueKind.False)
                    explicitLyrics = ex1.GetBoolean();
                explicitContent = Int(track, "explicit_content_lyrics") ?? explicitContent;
                if (track.TryGetProperty("gain", out var gn) && gn.ValueKind == JsonValueKind.Number) gain = gn.GetDouble();
                if (track.TryGetProperty("contributors", out var people) && people.ValueKind == JsonValueKind.Array)
                    contributors = people.EnumerateArray()
                        .Where(person => Str(person, "role") is null or "Main" or "Featured")
                        .Select(person => Str(person, "name"))
                        .Where(name => !string.IsNullOrWhiteSpace(name))
                        .Select(name => name!)
                        .Distinct(StringComparer.OrdinalIgnoreCase)
                        .ToList();
            }
        }

        var meta = new FullTrackMeta(albTitle, cover, year, Int(t, "duration"), artName,
            trackNumber, discNumber, isrc, totalTracks, genre, label, releaseDate, contributors,
            albumArtist, recordType)
        {
            Title = Str(t, "title"),
            TrackId = trackId,
            AlbumId = albId > 0 ? albId.ToString(System.Globalization.CultureInfo.InvariantCulture) : null,
            Barcode = string.IsNullOrWhiteSpace(barcode) ? null : barcode,
            ExplicitLyrics = explicitLyrics,
            ExplicitContent = explicitContent,
            CatalogGain = gain,
        };
        return (meta, detailUnresolved);
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

    /// <summary>One artist from a catalog search. Fans is how many follow them, which is what
    /// tells two artists of one name apart when nothing better is known.</summary>
    public record ArtistHit(string DeezerId, string Name, string? PictureUrl, int AlbumCount, int Fans = 0);

    /// <summary>
    /// Search the catalog for artists. Plain query: the artist endpoint takes a bare name
    /// and the qualified form is dead everywhere now.
    /// </summary>
    public Task<List<ArtistHit>> SearchArtistsAsync(string query, int limit, CancellationToken ct = default)
    {
        if (string.IsNullOrWhiteSpace(query) || limit <= 0) return Task.FromResult(new List<ArtistHit>());
        var key = $"ars|{query}|{limit}".ToLowerInvariant();
        if (TryGetCached<List<ArtistHit>>(key, out var cached)) return Task.FromResult(cached!);
        // An artist page names its artist and lists its albums in two requests at once, and
        // both search for the name. They share one search.
        return SharedAsync(key, () => FetchArtistSearchAsync(query, limit, key), ct);
    }

    private async Task<List<ArtistHit>> FetchArtistSearchAsync(string query, int limit, string key)
    {
        var hits = new List<ArtistHit>();
        try
        {
            var q = Uri.EscapeDataString(query);
            using var r = await GetJsonAsync($"{Base}/search/artist?q={q}&limit={limit}", CancellationToken.None);
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
                        Int(a, "nb_album") ?? 0, Int(a, "nb_fan") ?? 0));
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
    public async Task<List<AlbumHit>> SearchAlbumsAsync(string query, int limit, CancellationToken ct = default,
        bool keepSingles = false)
    {
        if (string.IsNullOrWhiteSpace(query) || limit <= 0) return new List<AlbumHit>();
        var key = $"as|{query}|{limit}|{keepSingles}".ToLowerInvariant();
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
                    // Search lists albums; a one- or two-track single is a song there. The cover
                    // upgrade keeps them: a library of singles has their covers to replace.
                    if (!keepSingles && string.Equals(recordType, "single", StringComparison.OrdinalIgnoreCase) && trackCount <= 2)
                        continue;

                    var artist = a.TryGetProperty("artist", out var art) ? Str(art, "name") : null;
                    hits.Add(new AlbumHit(
                        id, title, artist ?? "",
                        Str(a, "cover_xl") ?? Str(a, "cover_medium"),
                        null, trackCount, recordType) { ExplicitLyrics = Bool(a, "explicit_lyrics") });
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

    /// <summary>
    /// An artist's own releases for their page: albums, then EPs, then singles, then their own
    /// compilations, newest first within each, the way the apps group a discography. One copy
    /// of each title, whatever its type: the catalog lists a clean and an explicit copy of many,
    /// and an album can share its title with its own single or EP. The album is the one kept.
    /// Two releases of one title would also open as one, since an outside album's id is made
    /// from the artist and title, and the second would take over the first's. Singles used to
    /// be left out unless there was nothing else, which hid half of a career that is mostly
    /// singles; grouped after the records, they no longer bury them. The artist's name is not
    /// on this listing, so every hit carries the one given.
    /// </summary>
    public async Task<List<AlbumHit>> GetArtistAlbumsAsync(string deezerArtistId, string artistName, CancellationToken ct = default)
    {
        if (string.IsNullOrWhiteSpace(deezerArtistId)) return new List<AlbumHit>();
        var key = $"ara|{deezerArtistId}".ToLowerInvariant();
        if (TryGetCached<List<AlbumHit>>(key, out var cached)) return cached!;

        var releases = new List<AlbumHit>();
        // Where each title sits in the list, so a better copy found later takes its place.
        var byTitle = new Dictionary<string, int>(StringComparer.Ordinal);
        int? total = null;
        var listed = 0;
        try
        {
            var id = Uri.EscapeDataString(deezerArtistId);
            using var r = await GetJsonAsync($"{Base}/artist/{id}/albums?limit={ArtistAlbumsLimit}", ct);
            // Caching an empty list on a refusal would leave the artist's page empty for hours.
            if (r.Transient) return new List<AlbumHit>();
            if (r.Doc is not null
                && r.Doc.RootElement.TryGetProperty("data", out var data)
                && data.ValueKind == JsonValueKind.Array)
            {
                total = Int(r.Doc.RootElement, "total");
                listed = data.GetArrayLength();
                // Materialize everything before the JsonDocument is disposed.
                foreach (var a in data.EnumerateArray())
                {
                    var albumId = a.TryGetProperty("id", out var aid) && aid.ValueKind == JsonValueKind.Number
                        ? aid.GetInt64().ToString() : null;
                    var title = Str(a, "title");
                    if (albumId is null || string.IsNullOrWhiteSpace(title)) continue;

                    var recordType = Str(a, "record_type");
                    var released = Str(a, "release_date");
                    int? year = released is { Length: >= 4 } && int.TryParse(released[..4], out var y) && y > 0 ? y : null;
                    var hit = new AlbumHit(albumId, title, artistName,
                        Str(a, "cover_xl") ?? Str(a, "cover_medium"), year, Int(a, "nb_tracks") ?? 0, recordType)
                    { ExplicitLyrics = Bool(a, "explicit_lyrics") };

                    var titleKey = Octo.Services.Common.SongIdentity.Key(title);
                    if (!byTitle.TryGetValue(titleKey, out var at))
                    {
                        byTitle[titleKey] = releases.Count;
                        releases.Add(hit);
                    }
                    else if (ReleaseRank(recordType) < ReleaseRank(releases[at].RecordType)
                             // Of a clean and an explicit copy of one release, the explicit one: the
                             // original, and the one a search row of its songs stands for.
                             || (ReleaseRank(recordType) == ReleaseRank(releases[at].RecordType)
                                 && hit.ExplicitLyrics == true && releases[at].ExplicitLyrics != true))
                    {
                        releases[at] = hit;
                    }
                }
            }
        }
        catch (Exception ex)
        {
            _logger.LogDebug("deezer artist albums '{Id}' failed: {M}", deezerArtistId, ex.Message);
        }

        // Said out loud, because a page that stops short otherwise looks like a whole career.
        if (total is int all && all > listed)
            _logger.LogWarning(
                "deezer artist {Id} ('{Artist}') has {Total} releases; the page lists the first {Listed}",
                deezerArtistId, artistName, all, listed);

        var hits = releases
            .OrderBy(h => ReleaseRank(h.RecordType))
            .ThenByDescending(h => h.Year ?? 0)
            .ToList();
        Put(key, hits, hits.Count == 0 ? NegativeTtl : PositiveTtl);
        return hits;
    }

    /// <summary>
    /// A catalog record type as release types, in the lowercase words Navidrome relays from the
    /// tags (MusicBrainz's): "album", "ep", "single". An outside album sits beside library
    /// albums on one artist's page, and the two used to disagree on case. The catalog says
    /// "compile" for a compilation, which MusicBrainz files as an album that is a compilation.
    /// None for a type with no such word.
    /// </summary>
    public static IReadOnlyList<string> ReleaseTypes(string? recordType) => RecordType(recordType) switch
    {
        "album" => ["album"],
        "ep" => ["ep"],
        "single" => ["single"],
        "compile" => ["album", "compilation"],
        _ => [],
    };

    /// <summary>The catalog's record type in one spelling: lowercase, "compile" for either
    /// word for a compilation.</summary>
    private static string? RecordType(string? recordType) => recordType?.Trim().ToLowerInvariant() switch
    {
        "compilation" => "compile",
        var type => type,
    };

    /// <summary>Where a record type sits on an artist's page: albums, EPs, singles, compilations,
    /// then anything the catalog did not name.</summary>
    private static int ReleaseRank(string? recordType) => RecordType(recordType) switch
    {
        "album" => 0,
        "ep" => 1,
        "single" => 2,
        "compile" => 3,
        _ => 4,
    };

    /// <summary>A track count already known for an album, without asking the catalog.</summary>
    public bool TryKnownTrackCount(string deezerId, out int? count) => TryGetCached($"tc|{deezerId}", out count);

    /// <summary>How many releases an artist's page asks for: the catalog's page size, enough for
    /// all but the longest careers.</summary>
    private const int ArtistAlbumsLimit = 100;

    /// <summary>
    /// How many tracks a catalog album has, from the album's own record, or null when it cannot
    /// be told. An artist's listing leaves the count out, and a page showing "0 songs" on every
    /// album reads as empty albums (some clients hide them). Asked in the interactive lane:
    /// someone is looking at the page, and the background lane is kept full by cache warming,
    /// which turned every one of these away. The caller keeps the number asked small.
    /// </summary>
    public Task<int?> AlbumTrackCountAsync(string deezerId, CancellationToken ct = default)
    {
        var key = $"tc|{deezerId}";
        if (TryGetCached<int?>(key, out var cached)) return Task.FromResult(cached);
        // Two visits to one page at once ask for the same albums; each album is asked once.
        return SharedAsync(key, () => FetchAlbumTrackCountAsync(deezerId, key), ct);
    }

    private async Task<int?> FetchAlbumTrackCountAsync(string deezerId, string key)
    {
        try
        {
            using var r = await GetJsonAsync($"{Base}/album/{Uri.EscapeDataString(deezerId)}", CancellationToken.None);
            if (r.Transient) return null;
            var count = r.Doc is null ? null : Int(r.Doc.RootElement, "nb_tracks");
            Put(key, count, count is null ? NegativeTtl : PositiveTtl);
            return count;
        }
        catch (Exception ex)
        {
            _logger.LogDebug("deezer album {Id} track count failed: {M}", deezerId, ex.Message);
            return null;
        }
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
    public async Task<AlbumDetail?> GetAlbumDetailAsync(string deezerId, CancellationToken ct = default) =>
        (await LookUpAlbumDetailAsync(deezerId, ct)).Detail;

    /// <summary>
    /// <see cref="GetAlbumDetailAsync"/>, saying also why there is no detail when there is none:
    /// Deezer has no such album, it has the album but no tracks for it, or it did not answer
    /// this time. Only the last may come right on its own a moment later.
    /// </summary>
    public async Task<AlbumLookup> LookUpAlbumDetailAsync(string deezerId, CancellationToken ct = default,
        bool background = false)
    {
        if (string.IsNullOrWhiteSpace(deezerId)) return new AlbumLookup(null, AlbumAnswer.NoSuchAlbum);
        var cacheKey = $"ad|{deezerId}";
        if (TryGetCached<AlbumLookup>(cacheKey, out var cached)) return cached!;
        var unavailable = new AlbumLookup(null, AlbumAnswer.Unavailable);

        AlbumDetail? detail = null;
        // A tracklist we know is short gets a shorter life than a complete one, so a
        // truncated or partly-skipped album repairs itself instead of sticking.
        var partial = false;
        try
        {
            string title = "", artist = "", genre = "", label = "", cover = "";
            string? recordType = null;
            int? albumExplicit = null;
            int? year = null;
            // Declared out here on purpose: the document below is disposed before the
            // tracklist call, and this is what tells an empty tracklist apart from an
            // album that genuinely has no tracks.
            int? nbTracks = null;

            // Both calls at once: the tracklist does not depend on the album's answer, and an
            // album opened or counted from a search waits on the slower of the two rather than
            // on both in turn. When the album call says there is nothing, the tracklist's
            // answer is dropped unread.
            var tracksCall = GetJsonAsync($"{Base}/album/{deezerId}/tracks?limit=300", ct, background);
            async Task<AlbumLookup> Without(AlbumLookup answer)
            {
                (await tracksCall).Dispose();
                return answer;
            }

            using (var r = await GetJsonAsync($"{Base}/album/{deezerId}", ct, background))
            {
                if (r.Transient) return await Without(unavailable);
                if (r.Doc is not null)
                {
                    var root = r.Doc.RootElement;
                    nbTracks = Int(root, "nb_tracks");
                    title = Str(root, "title") ?? "";
                    cover = Str(root, "cover_xl") ?? Str(root, "cover_medium") ?? "";
                    label = Str(root, "label") ?? "";
                    recordType = Str(root, "record_type");
                    albumExplicit = Octo.Models.Domain.ExplicitStatus.FromCatalog(
                        Int(root, "explicit_content_lyrics"), Bool(root, "explicit_lyrics"));
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
            if (string.IsNullOrWhiteSpace(title))
            {
                var none = new AlbumLookup(null, AlbumAnswer.NoSuchAlbum);
                Put(cacheKey, none, NegativeTtl);
                return await Without(none);
            }

            var tracks = new List<AlbumTrack>();
            using (var tr = await tracksCall)
            {
                // The album call can succeed while the tracklist call is throttled. That
                // built a perfectly valid AlbumDetail carrying title, year and genre with
                // an empty tracklist, cached it permanently, and is why getAlbum reported
                // songCount 0 forever while still showing real metadata.
                if (tr.Transient) return unavailable;
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
                            Int(t, "track_position"), Int(t, "disk_number"), Str(t, "isrc"))
                        {
                            ExplicitContent = Octo.Models.Domain.ExplicitStatus.FromCatalog(
                                Int(t, "explicit_content_lyrics"), Bool(t, "explicit_lyrics")),
                        });
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
                return new AlbumLookup(null, AlbumAnswer.NoTracks);
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
                tracks, recordType) { ExplicitContent = albumExplicit };
        }
        catch (Exception ex)
        {
            _logger.LogDebug("deezer album detail {Id} failed: {M}", deezerId, ex.Message);
        }

        var lookup = detail is null ? unavailable : new AlbumLookup(detail, AlbumAnswer.Found);
        Put(cacheKey, lookup, detail is null ? NegativeTtl : partial ? PartialTtl : PositiveTtl);
        return lookup;
    }

    /// <summary>
    /// <see cref="LookUpAlbumDetailAsync"/> from the cache, or from one request shared by every
    /// caller asking at once. The request runs without the caller's token, so a search that
    /// stops waiting still leaves the answer cached for the next one. <paramref name="background"/>
    /// puts it behind anything a listener is waiting on.
    /// </summary>
    /// <summary>The album's detail when it is already cached, without asking Deezer.</summary>
    public AlbumLookup? CachedAlbumDetail(string deezerId) =>
        !string.IsNullOrWhiteSpace(deezerId) && TryGetCached<AlbumLookup>($"ad|{deezerId}", out var cached) ? cached : null;

    public Task<AlbumLookup> SharedAlbumDetailAsync(string deezerId, bool background, CancellationToken ct)
    {
        if (string.IsNullOrWhiteSpace(deezerId)) return Task.FromResult(new AlbumLookup(null, AlbumAnswer.NoSuchAlbum));
        var cacheKey = $"ad|{deezerId}";
        if (TryGetCached<AlbumLookup>(cacheKey, out var cached)) return Task.FromResult(cached!);
        return SharedAsync(cacheKey, () => LookUpAlbumDetailAsync(deezerId, CancellationToken.None, background), ct);
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

    /// <summary>
    /// An album's barcode (UPC), which names one exact release in every store, so the cover
    /// upgrade can find the same release at Apple in a batch instead of one search an album.
    /// </summary>
    public async Task<string?> GetAlbumUpcAsync(string deezerId, CancellationToken ct = default)
    {
        if (string.IsNullOrWhiteSpace(deezerId)) return null;
        var key = $"upc|{deezerId}";
        if (TryGetCached<string>(key, out var cached)) return string.IsNullOrEmpty(cached) ? null : cached;
        try
        {
            using var r = await GetJsonAsync($"{Base}/album/{Uri.EscapeDataString(deezerId)}", ct);
            if (r.Transient) return null;
            var upc = r.Doc is not null ? Str(r.Doc.RootElement, "upc") : null;
            Put(key, upc ?? "", string.IsNullOrEmpty(upc) ? NegativeTtl : PositiveTtl);
            return string.IsNullOrEmpty(upc) ? null : upc;
        }
        catch (Exception ex) when (!ct.IsCancellationRequested)
        {
            _logger.LogDebug("deezer album {Id} barcode failed: {M}", deezerId, ex.Message);
            return null;
        }
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
    /// Whether a search row's song is explicit. The row stands for the song, not one copy of it,
    /// and the catalog lists the explicit original and its clean edit side by side under the same
    /// title: when any hit of this very song (the same version as the one matched) is explicit,
    /// the song is; otherwise the matched hit says.
    /// </summary>
    internal static int? SongExplicitness(JsonDocument? doc, string? artist, string? title, JsonElement matched)
    {
        var mine = Octo.Models.Domain.ExplicitStatus.FromCatalog(Int(matched, "explicit_content_lyrics"), Bool(matched, "explicit_lyrics"));
        var version = SongIdentity.DistinctVersions(SongIdentity.ParseTitle(Str(matched, "title")));
        foreach (var hit in AllMatches(doc, artist, title))
        {
            if (!SongIdentity.DistinctVersions(SongIdentity.ParseTitle(Str(hit, "title"))).SetEquals(version)) continue;
            if (Octo.Models.Domain.ExplicitStatus.FromCatalog(Int(hit, "explicit_content_lyrics"), Bool(hit, "explicit_lyrics"))
                == Octo.Models.Domain.ExplicitStatus.Explicit)
                return Octo.Models.Domain.ExplicitStatus.Explicit;
        }
        return mine;
    }

    /// <summary>How many searches one track lookup may make: the song as asked, then written
    /// another way, then the title alone. Each is one request against Deezer's shared budget,
    /// and only a miss makes the next.</summary>
    private const int TrackSearches = 3;

    /// <summary>
    /// The first track search hit that is this song, trying the song as asked and then the
    /// other ways <see cref="SongIdentity.QueryVariants"/> writes it ("suicideboys SUICIDE" for
    /// "$uicideboy$ $UICIDE", the title without its guests, the primary artist, the title
    /// alone). Every hit is judged against the song as asked, so a looser query never means a
    /// looser match. The response holding the hit is the caller's to dispose; a transient one
    /// comes back with no hit and must not be cached.
    /// </summary>
    private async Task<(DeezerResponse? Response, JsonElement? Hit)> FindTrackAsync(string? artist, string? title,
        CancellationToken ct, bool background = false)
    {
        foreach (var variant in SongIdentity.QueryVariants(title, artist).Take(TrackSearches))
        {
            var r = await GetJsonAsync($"{Base}/search?q={Uri.EscapeDataString(variant.Text)}&limit={MatchCandidates}", ct, background);
            if (r.Transient) return (r, null);
            if (BestMatch(r.Doc, artist, title) is JsonElement hit) return (r, hit);
            r.Dispose();
        }
        return (null, null);
    }

    /// <summary>Like <see cref="FindTrackAsync"/>, but every hit that is this song from the first
    /// search that finds any, in the catalog's order.</summary>
    private async Task<(DeezerResponse? Response, List<JsonElement> Hits)> FindTrackHitsAsync(string? artist, string? title,
        CancellationToken ct)
    {
        foreach (var variant in SongIdentity.QueryVariants(title, artist).Take(TrackSearches))
        {
            var r = await GetJsonAsync($"{Base}/search?q={Uri.EscapeDataString(variant.Text)}&limit={MatchCandidates}", ct);
            if (r.Transient) return (r, []);
            var hits = AllMatches(r.Doc, artist, title);
            if (hits.Count > 0) return (r, hits);
            r.Dispose();
        }
        return (null, []);
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

    /// <summary>What one field of a hit says about the request.</summary>
    private enum FieldVerdict { Match, Absent, Mismatch }

    /// <summary>
    /// Compare the titles by <see cref="SongIdentity"/>: the same key, or the same key once
    /// stylized characters are read as letters, or one key containing the other, because
    /// Deezer decorates titles in ways no list names. Never another version: a live take or a
    /// remix carries its own album and length, and attaching those to the original is wrong.
    ///
    /// A field either side left empty is <see cref="FieldVerdict.Absent"/>, never a
    /// mismatch. Absent evidence is not counter-evidence, and treating a field Deezer
    /// simply did not send as a contradiction would throw away good hits the moment the
    /// payload shape changes.
    /// </summary>
    private static FieldVerdict CompareTitles(string? want, string? got)
    {
        var a = SongIdentity.ParseTitle(want);
        var b = SongIdentity.ParseTitle(got);
        if (a.Key.Length == 0 || b.Key.Length == 0) return FieldVerdict.Absent;
        if (!SongIdentity.DistinctVersions(a).SetEquals(SongIdentity.DistinctVersions(b))) return FieldVerdict.Mismatch;
        return a.Key == b.Key || a.LooseKey == b.LooseKey || a.Key.Contains(b.Key) || b.Key.Contains(a.Key)
            ? FieldVerdict.Match : FieldVerdict.Mismatch;
    }

    /// <summary>The artists by <see cref="SongIdentity"/>, and one key containing the other, since
    /// Deezer credits guests in the artist field.</summary>
    private static FieldVerdict CompareArtists(string? want, string? got)
    {
        var a = SongIdentity.Key(want);
        var b = SongIdentity.Key(got);
        if (a.Length == 0 || b.Length == 0) return FieldVerdict.Absent;
        return SongIdentity.ArtistsAgree(want, got) || a.Contains(b) || b.Contains(a)
            ? FieldVerdict.Match : FieldVerdict.Mismatch;
    }

    /// <summary>
    /// The first hit that positively matches on artist or title and contradicts on
    /// neither. This is the guard that makes a plain query safe to use in place of the
    /// qualified one: without it a near-miss at position 0 would be attached to the song
    /// as fact. Requiring at least one positive match is what stops a hit that states
    /// nothing at all from matching everything.
    /// </summary>
    private static JsonElement? BestMatch(JsonDocument? doc, string? artist, string? title) =>
        AllMatches(doc, artist, title) is { Count: > 0 } hits ? hits[0] : null;

    /// <summary>Every hit that positively matches on artist or title and contradicts on neither,
    /// in the catalog's order. The first is what <see cref="BestMatch"/> returns.</summary>
    private static List<JsonElement> AllMatches(JsonDocument? doc, string? artist, string? title)
    {
        var hits = new List<JsonElement>();
        if (doc is null) return hits;
        if (!doc.RootElement.TryGetProperty("data", out var data)
            || data.ValueKind != JsonValueKind.Array) return hits;

        foreach (var hit in data.EnumerateArray())
        {
            var titleVerdict = CompareTitles(title, Str(hit, "title"));
            var artistVerdict = CompareArtists(artist,
                hit.TryGetProperty("artist", out var a) ? Str(a, "name") : null);

            if (titleVerdict == FieldVerdict.Mismatch || artistVerdict == FieldVerdict.Mismatch) continue;
            if (titleVerdict == FieldVerdict.Match || artistVerdict == FieldVerdict.Match) hits.Add(hit);
        }
        return hits;
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

    private static bool? Bool(JsonElement e, string name)
        => e.TryGetProperty(name, out var v) && v.ValueKind is JsonValueKind.True or JsonValueKind.False ? v.GetBoolean() : null;

    private static int? Int(JsonElement e, string name)
        => e.TryGetProperty(name, out var v) && v.ValueKind == JsonValueKind.Number ? v.GetInt32() : (int?)null;

}
