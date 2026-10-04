using System.Text.Json;
using Microsoft.Extensions.Caching.Memory;
using Octo.Models.Domain;
using Octo.Services.LastFm;
using Octo.Services.Metadata;
using Octo.Services.Soulseek;

namespace Octo.Services.Common;

/// <summary>
/// The songs people play most: an artist's, for search, and the chart of the moment, for an
/// empty search. Each comes back ranked, as outside songs that play and can be added like any
/// search row; which of them the caller's library already has is settled per request, by
/// <see cref="MatchLibraryAsync"/>, because the library changes and the list does not.
///
/// Sources, and why in this order:
///   - An artist's ranking is Last.fm's play counts when a key is set (Octo's search already
///     needs one), because a count beside each row is what makes it read as a chart. Deezer's
///     own top list dresses each row with its album, cover and length, so a row costs no
///     further lookup.
///   - Without a key, or when Last.fm does not answer, Deezer's top list is the ranking. It is
///     keyless and clean catalog data, with no counts to show.
///   - The chart of the moment is Deezer's: keyless, current, and dressed the same way.
///     Last.fm's chart only stands in when Deezer does not answer.
///   ListenBrainz has play counts too, but only by MusicBrainz id, which costs a rate-limited
///   MusicBrainz lookup per artist first. Apple's RSS charts have no per-artist list.
/// </summary>
public sealed class TopSongsService : IDisposable
{
    /// <summary>The most rows a list is built with. Callers take the prefix they want.</summary>
    public const int MaxRows = 50;

    public const string DeezerSource = "deezer";
    public const string LastFmSource = "lastfm";

    /// <summary>One ranked song. Plays and Listeners are Last.fm's, when it gave them.</summary>
    public sealed record TopSong(int Rank, Song Song, long? Plays, long? Listeners);

    /// <summary>A ranked list and where its order came from. Artist is null for the chart.</summary>
    public sealed record TopList(string? Artist, string Source, IReadOnlyList<TopSong> Songs)
    {
        public static readonly TopList Empty = new(null, "", []);
    }

    /// <summary>A row the merge settled on: the song as the ranking named it, the catalog track
    /// that dresses it when there is one, and its counts.</summary>
    public sealed record PlannedRow(string Artist, string Title,
        DeezerMetadataService.ChartTrack? Catalog, long? Plays, long? Listeners);

    /// <summary>A whole list keeps for this long once every source answered; one built while a
    /// source did not answer, or that came out empty, is asked again soon.</summary>
    private static readonly TimeSpan ArtistTtl = TimeSpan.FromHours(6);
    private static readonly TimeSpan ChartTtl = TimeSpan.FromHours(1);
    private static readonly TimeSpan ShortTtl = TimeSpan.FromMinutes(10);

    /// <summary>Ceiling on one build, shared by everyone waiting on it.</summary>
    private static readonly TimeSpan BuildTimeout = TimeSpan.FromSeconds(15);

    /// <summary>How many catalog artists of a name are weighed, as an outside artist page does.</summary>
    private const int ArtistCandidates = 5;

    /// <summary>Library searches running at once for one list. Navidrome is local and quick,
    /// and a list is at most <see cref="MaxRows"/> rows.</summary>
    private const int LibrarySearchesAtOnce = 4;

    /// <summary>The same song, for telling a list's rows apart and pairing the two sources:
    /// a radio edit is the song, as it is for a station.</summary>
    private static readonly SongMatchOptions SameSong = new()
    {
        LengthToleranceSeconds = null,
        AlsoNeutral = ["radio edit"],
    };

    private readonly DeezerMetadataService _deezer;
    private readonly IMusicMetadataService _metadata;
    private readonly ExternalIdRegistry _registry;
    private readonly LastFmService? _lastFm;
    private readonly ILogger<TopSongsService> _logger;
    private readonly MemoryCache _cache = new(new MemoryCacheOptions { SizeLimit = 512 });
    private readonly SingleFlight<string, (TopList List, bool Whole)> _builds = new();

    public TopSongsService(DeezerMetadataService deezer, IMusicMetadataService metadata,
        ExternalIdRegistry registry, ILogger<TopSongsService> logger, LastFmService? lastFm = null)
    {
        _deezer = deezer;
        _metadata = metadata;
        _registry = registry;
        _logger = logger;
        _lastFm = lastFm;
    }

    public void Dispose() => _cache.Dispose();

    /// <summary>
    /// An artist's most played songs. <paramref name="artistId"/> may be an outside artist's id
    /// from search, which already names the catalog artist; a library id, or none, finds the
    /// artist by name.
    /// </summary>
    public Task<TopList> ForArtistAsync(string name, string? artistId, CancellationToken ct = default)
    {
        name = name.Trim();
        if (name.Length == 0) return Task.FromResult(TopList.Empty);
        var known = artistId is { Length: > 0 } && _registry.Lookup(artistId) is { Kind: RoutingKind.Artist } routing
            && SongIdentity.SameArtistName(routing.Artist, name)
            ? routing.ExternalArtistId
            : null;
        var key = known is { Length: > 0 } ? $"artist|id|{known}" : $"artist|name|{SongIdentity.Key(name)}";
        return CachedAsync(key, token => BuildArtistAsync(name, known, token), ArtistTtl, ct);
    }

    /// <summary>The chart of the moment.</summary>
    public Task<TopList> ChartAsync(CancellationToken ct = default) =>
        CachedAsync("chart", BuildChartAsync, ChartTtl, ct);

    private async Task<TopList> CachedAsync(string key, Func<CancellationToken, Task<(TopList, bool)>> build,
        TimeSpan ttl, CancellationToken ct)
    {
        if (_cache.TryGetValue(key, out TopList? cached) && cached is not null) return cached;
        try
        {
            var (list, whole) = await _builds.RunAsync(key, build, BuildTimeout).WaitAsync(ct);
            _cache.Set(key, list, new MemoryCacheEntryOptions
            {
                Size = 1,
                AbsoluteExpirationRelativeToNow = whole && list.Songs.Count > 0 ? ttl : ShortTtl,
            });
            return list;
        }
        catch (Exception ex) when (ex is not OperationCanceledException || !ct.IsCancellationRequested)
        {
            // Top songs are an addition to search, never a reason for it to fail.
            _logger.LogDebug("top songs '{Key}' failed: {M}", key, ex.Message);
            return TopList.Empty;
        }
    }

    private async Task<(TopList, bool)> BuildArtistAsync(string name, string? deezerArtistId, CancellationToken ct)
    {
        var lastFmTask = _lastFm is { HasApiKey: true } lastFm
            ? lastFm.GetArtistChartAsync(name, MaxRows, ct)
            : Task.FromResult<List<LastFmService.ChartTrack>?>(null);

        deezerArtistId ??= await FindCatalogArtistAsync(name);
        var catalog = deezerArtistId is null
            ? new List<DeezerMetadataService.ChartTrack>()
            : await _deezer.GetArtistTopAsync(deezerArtistId, DeezerMetadataService.ChartLimit, ct);
        var counts = await lastFmTask;

        var (rows, source) = Merge(catalog, counts, oneArtist: true, MaxRows);
        // Whole: every source that could answer did. A catalog miss on the name is an answer.
        var whole = catalog is not null && (counts is not null || _lastFm is not { HasApiKey: true });
        var shownName = rows.FirstOrDefault(row => row.Catalog is not null)?.Catalog?.Artist ?? name;
        var songs = await BuildSongsAsync(rows, ct);
        _logger.LogInformation("Top songs for '{Artist}': {N} from {Source}", name, songs.Count, source);
        return (new TopList(SongIdentity.SameArtistName(shownName, name) ? shownName : name, source, songs), whole);
    }

    private async Task<(TopList, bool)> BuildChartAsync(CancellationToken ct)
    {
        var catalog = await _deezer.GetChartAsync(DeezerMetadataService.ChartLimit, ct);
        var counts = catalog is null && _lastFm is { HasApiKey: true } lastFm
            ? await lastFm.GetChartAsync(MaxRows, ct)
            : null;
        var (rows, source) = catalog is not null ? Merge(catalog, null, oneArtist: false, MaxRows) : Merge(null, counts, oneArtist: false, MaxRows);
        var songs = await BuildSongsAsync(rows, ct);
        _logger.LogInformation("Top chart: {N} from {Source}", songs.Count, source);
        return (new TopList(null, source, songs), catalog is not null);
    }

    /// <summary>The catalog artist of this exact name that more people follow, as an outside
    /// artist's page settles it when it holds no id.</summary>
    private async Task<string?> FindCatalogArtistAsync(string name)
    {
        var hits = await _deezer.SearchArtistsAsync(name, ArtistCandidates);
        return hits.Where(hit => SongIdentity.SameArtistName(hit.Name, name))
            .OrderByDescending(hit => hit.Fans)
            .FirstOrDefault()?.DeezerId;
    }

    /// <summary>
    /// The rows of a list, in rank order. With Last.fm's counts, Last.fm's order, each row
    /// dressed by the catalog track of the same title when the catalog lists it. Without them,
    /// the catalog's order. One row per song: a radio edit listed beside the song is the song,
    /// and only its first, higher ranked, listing is kept. On one artist's list titles alone
    /// say which song is which; on a chart the artist must agree too.
    /// </summary>
    public static (List<PlannedRow> Rows, string Source) Merge(
        IReadOnlyList<DeezerMetadataService.ChartTrack>? catalog,
        IReadOnlyList<LastFmService.ChartTrack>? counts,
        bool oneArtist, int max)
    {
        var rows = new List<PlannedRow>();
        bool Listed(string artist, string title) => rows.Any(row => Same(row.Artist, row.Title, artist, title, oneArtist));

        if (counts is { Count: > 0 })
        {
            var unused = (catalog ?? []).ToList();
            foreach (var track in counts)
            {
                if (rows.Count >= max) break;
                if (Listed(track.Artist, track.Title)) continue;
                var dressed = unused.FirstOrDefault(c => Same(c.Artist, c.Title, track.Artist, track.Title, oneArtist));
                if (dressed is not null) unused.Remove(dressed);
                rows.Add(new PlannedRow(track.Artist, track.Title, dressed, track.Plays, track.Listeners));
            }
            return (rows, LastFmSource);
        }

        foreach (var track in catalog ?? [])
        {
            if (rows.Count >= max) break;
            if (Listed(track.Artist, track.Title)) continue;
            rows.Add(new PlannedRow(track.Artist, track.Title, track, null, null));
        }
        return (rows, rows.Count > 0 ? DeezerSource : "");
    }

    private static bool Same(string aArtist, string aTitle, string bArtist, string bTitle, bool oneArtist) =>
        oneArtist
            ? SongIdentity.SameTitle(aTitle, bTitle, SameSong).IsSame
            : SongIdentity.Same(aTitle, aArtist, bTitle, bArtist, SameSong).IsSame;

    /// <summary>
    /// Outside songs for the rows, registered so they play, open their album and are added
    /// like any other outside song. A row the catalog dressed carries its album, cover and
    /// length; the rest are looked up the way search looks its rows up.
    /// </summary>
    private async Task<List<TopSong>> BuildSongsAsync(List<PlannedRow> rows, CancellationToken ct)
    {
        var songs = new List<TopSong>(rows.Count);
        var bare = new List<Song>();
        foreach (var row in rows)
        {
            Song? song;
            if (row.Catalog is { } track)
            {
                song = CatalogSong(track);
            }
            else
            {
                var hits = await _metadata.SearchSongsByArtistTitleAsync(row.Artist, row.Title, 1);
                song = hits.FirstOrDefault();
                if (song is not null) bare.Add(song);
            }
            if (song is not null) songs.Add(new TopSong(songs.Count + 1, song, row.Plays, row.Listeners));
        }
        if (bare.Count > 0) await _metadata.EnrichExternalSongsAsync(bare, ct);

        // A list is opened to be played: warm the covers it shows and the first few streams,
        // as search does for its own first screen. Never awaited.
        var all = songs.Select(top => top.Song).ToList();
        _ = _metadata.PrewarmCoverArtAsync(all, topN: 20);
        _ = _metadata.PrewarmYouTubeIdsAsync(all, topN: 5);
        return songs;
    }

    private Song CatalogSong(DeezerMetadataService.ChartTrack track)
    {
        var artistId = _registry.Register(new SoulseekRouting
        {
            Kind = RoutingKind.Artist,
            Artist = track.Artist,
            ExternalArtistId = track.ArtistId,
        });
        string? albumId = null;
        if (!string.IsNullOrWhiteSpace(track.Album))
        {
            albumId = _registry.Register(new SoulseekRouting
            {
                Kind = RoutingKind.Album,
                Artist = track.Artist,
                Album = track.Album,
                ExternalAlbumId = track.AlbumId,
            });
        }
        // The album rides on the song's routing too, so a download tags it with this album
        // rather than one found again from the artist and title.
        var id = _registry.Register(new SoulseekRouting
        {
            Kind = RoutingKind.Song,
            Artist = track.Artist,
            Title = track.Title,
            Album = track.Album,
            Duration = track.Duration,
        });
        _registry.RememberLength(id, track.Duration, LengthSource.Deezer);
        return new Song
        {
            Id = id,
            Title = track.Title,
            Artist = track.Artist,
            ArtistId = artistId,
            Album = track.Album ?? "",
            AlbumId = albumId,
            Duration = track.Duration,
            CoverArtUrl = track.CoverUrl,
            CoverArtUrlLarge = track.CoverUrl,
            ExplicitContentLyrics = track.Explicit switch { true => 1, false => 0, null => null },
            IsLocal = false,
            ExternalProvider = SoulseekMetadataService.ProviderName,
            ExternalId = id,
        };
    }

    /// <summary>
    /// The library song each row already is, or null, in row order. <paramref name="searchSongs"/>
    /// searches the caller's library and answers its song list (Navidrome's searchResult3
    /// songs), or null when it could not. A row is the library's only when it is the same
    /// recording, by the rule radio uses to play a library copy in place of an outside one.
    /// </summary>
    public static async Task<IReadOnlyList<JsonElement?>> MatchLibraryAsync(IReadOnlyList<TopSong> rows,
        Func<string, CancellationToken, Task<JsonElement?>> searchSongs, CancellationToken ct = default)
    {
        var found = new JsonElement?[rows.Count];
        using var gate = new SemaphoreSlim(LibrarySearchesAtOnce);
        await Task.WhenAll(rows.Select(async (row, index) =>
        {
            await gate.WaitAsync(ct);
            try
            {
                var songs = await searchSongs(LibraryQuery(row.Song), ct);
                if (songs is not { ValueKind: JsonValueKind.Array } list) return;
                foreach (var hit in list.EnumerateArray())
                {
                    var hitArtist = Text(hit, "artist");
                    var hitTitle = Text(hit, "title");
                    if (Text(hit, "id").Length == 0
                        || !LastFmRadioTrackResolver.IsSameRecording(row.Song.Artist, row.Song.Title, hitArtist, hitTitle))
                        continue;
                    found[index] = hit;
                    return;
                }
            }
            finally { gate.Release(); }
        }));

        // One library song stands for one row: the higher ranked one keeps it.
        var taken = new HashSet<string>(StringComparer.Ordinal);
        for (var i = 0; i < found.Length; i++)
        {
            if (found[i] is { } hit && !taken.Add(Text(hit, "id"))) found[i] = null;
        }
        return found;
    }

    /// <summary>What the library is searched with for a row: its main artist and the title
    /// without brackets or guests. Navidrome wants every word it is given, so "(feat. X)" or
    /// "(Radio Edit)" would hide a library copy tagged without them.</summary>
    public static string LibraryQuery(Song song)
    {
        var artist = SongIdentity.PrimaryArtist(song.Artist);
        var title = SongIdentity.ParseTitle(song.Title, song.Artist).Core;
        if (title.Length == 0) title = song.Title ?? "";
        return $"{artist} {title}".Trim();
    }

    private static string Text(JsonElement element, string name) =>
        element.TryGetProperty(name, out var value) && value.ValueKind == JsonValueKind.String ? value.GetString() ?? "" : "";
}
