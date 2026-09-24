using System.Collections.Concurrent;
using System.Text.Json;
using Microsoft.Extensions.Options;
using Octo.Models.Domain;
using Octo.Models.Radio;
using Octo.Models.Settings;
using Octo.Services.Fingerprint;
using Octo.Services.LastFm;
using Octo.Services.Soulseek;

namespace Octo.Services.Subsonic;

public enum SyncCatalogKind { Artist, Album, Song }

/// <summary>
/// One user's discovery catalog: the placeholder songs a syncing client is handed after the
/// last library row, and the albums and artists those songs need to exist on the device.
/// Immutable, so a walk can hold on to the one it started with while a rebuild swaps in.
/// </summary>
public sealed class SyncCatalog
{
    public static readonly SyncCatalog Empty = new([], [], [], new Dictionary<string, DateTime>(), "", DateTime.MinValue);

    public IReadOnlyList<Song> Songs { get; }
    public IReadOnlyList<Album> Albums { get; }
    public IReadOnlyList<Artist> Artists { get; }

    /// <summary>When each song and album id first entered this user's catalog. Rows go out
    /// with this as their <c>created</c> date rather than "now", or every sync would present
    /// the whole catalog as the newest additions to the library and bury real downloads.</summary>
    public IReadOnlyDictionary<string, DateTime> Added { get; }

    public string Fingerprint { get; }
    public DateTime BuiltUtc { get; }

    private readonly Dictionary<string, Song> _songsById;

    public SyncCatalog(IReadOnlyList<Song> songs, IReadOnlyList<Album> albums,
        IReadOnlyList<Artist> artists, IReadOnlyDictionary<string, DateTime> added,
        string fingerprint, DateTime builtUtc)
    {
        Songs = songs; Albums = albums; Artists = artists; Added = added;
        Fingerprint = fingerprint; BuiltUtc = builtUtc;
        _songsById = new Dictionary<string, Song>(StringComparer.Ordinal);
        foreach (var song in songs) _songsById.TryAdd(song.Id, song);
    }

    public bool TryGetSong(string id, out Song song) => _songsById.TryGetValue(id, out song!);

    public int Count(SyncCatalogKind kind) => kind switch
    {
        SyncCatalogKind.Artist => Artists.Count,
        SyncCatalogKind.Album => Albums.Count,
        _ => Songs.Count,
    };
}

/// <summary>
/// Discovery for clients that never send a search to the server.
///
/// Symfonium copies the whole library to the device by paging search3 with an empty query
/// (<c>query=""&amp;songCount=1000&amp;songOffset=N</c>, one kind per walk) and searches that
/// copy offline, so a typed query never reaches Octo and search-time discovery has nothing
/// to add to. The walk itself does reach Octo. This extends it past the last library row
/// with the tracks of the user's radio stations that the library does not own, as ordinary
/// placeholder songs. On the device they are then searchable, browsable and playable like
/// anything else, the station playlists resolve against rows the device already has, and a
/// heart downloads the track the same way it does from any other client.
///
/// The walk is paged by offset with no total, so the catalog is addressed as rows that sit
/// after the library: <see cref="Window"/> turns a page request into a slice of it. A walk
/// pins the catalog it started on, so a rebuild finishing half way through does not shift
/// rows under it.
/// </summary>
public sealed class SyncCatalogService
{
    /// <summary>How long a built catalog is reused while its stations are unchanged. Long
    /// enough that a sync does not rebuild per page, short enough that a track hearted and
    /// downloaded drops out of the catalog on a later sync the same day.</summary>
    internal static readonly TimeSpan CatalogTtl = TimeSpan.FromHours(1);

    /// <summary>How long a walk's pinned catalog and library size are trusted. A sync of a
    /// large library over a slow link is minutes, not hours.</summary>
    internal static readonly TimeSpan WalkTtl = TimeSpan.FromMinutes(20);

    private const int LookupConcurrency = 4;

    private readonly IServiceScopeFactory _scopes;
    private readonly IMusicMetadataService _metadata;
    private readonly ExternalIdRegistry _registry;
    private readonly IOptionsMonitor<SubsonicSettings> _settings;
    private readonly ILogger<SyncCatalogService> _logger;

    private readonly ConcurrentDictionary<string, SyncCatalog> _catalogs = new(StringComparer.OrdinalIgnoreCase);
    private readonly Dictionary<string, (string Fingerprint, Task<SyncCatalog> Task)> _builds =
        new(StringComparer.OrdinalIgnoreCase);
    private readonly ConcurrentDictionary<string, WalkMemo> _walks = new(StringComparer.OrdinalIgnoreCase);

    private sealed record WalkMemo(int LocalTotal, SyncCatalog Catalog, DateTime AtUtc);

    public SyncCatalogService(IServiceScopeFactory scopes, IMusicMetadataService metadata,
        ExternalIdRegistry registry, IOptionsMonitor<SubsonicSettings> settings,
        ILogger<SyncCatalogService> logger)
    {
        _scopes = scopes;
        _metadata = metadata;
        _registry = registry;
        _settings = settings;
        _logger = logger;
    }

    /// <summary>
    /// Whether a request comes from a client that syncs the library rather than searching
    /// the server. Decided by the <c>c</c> parameter because nothing about the request tells
    /// a sync walk apart from an ordinary "all songs" listing, and handing catalog rows to a
    /// client that lists songs online would put suggestions into its library views.
    /// </summary>
    public static bool IsSyncClient(SubsonicSettings settings, string? client)
    {
        if (!settings.EnableSyncCatalog || string.IsNullOrWhiteSpace(client)) return false;
        return (settings.SyncCatalogClients ?? "")
            .Split(',', StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries)
            .Any(name => client.Contains(name, StringComparison.OrdinalIgnoreCase));
    }

    // ---------------------------------------------------------------------------------
    // Paging. A walk asks for `count` rows at `offset` and gets `localReturned` from the
    // library; the virtual list is the library followed by the catalog.
    // ---------------------------------------------------------------------------------

    /// <summary>The library's size when this page alone proves it: a short page that still
    /// had library rows on it ends exactly where the library does, and so does an empty
    /// first page. An empty later page only says the library ended at or before it.</summary>
    internal static int? LocalTotalFromPage(int offset, int localReturned) =>
        localReturned > 0 || offset == 0 ? offset + localReturned : null;

    /// <summary>The catalog slice that fills the rest of a page, given the library size.</summary>
    internal static (int Start, int Take) Window(int offset, int count, int localReturned, int localTotal) =>
        (Math.Max(0, offset + localReturned - localTotal), Math.Max(0, count - localReturned));

    /// <summary>
    /// The library's size for an empty page past its end, which the page cannot say. The
    /// size remembered from earlier in the walk is used when two probes confirm it (the row
    /// before it exists, the row at it does not); otherwise it is found by bisection, since
    /// clients that fetch pages in parallel can ask for one past the end before the page
    /// that would have told us. Null when a probe fails, so nothing is appended on a guess.
    /// </summary>
    internal static async Task<int?> ResolveLocalTotalAsync(int emptyOffset, int? remembered,
        Func<int, Task<bool?>> exists)
    {
        if (remembered is int candidate && candidate >= 0 && candidate <= emptyOffset)
        {
            var before = candidate == 0 ? true : await exists(candidate - 1);
            var at = candidate == emptyOffset ? false : await exists(candidate);
            if (before is null || at is null) return null;
            if (before == true && at == false) return candidate;
        }

        int low = 0, high = emptyOffset;
        while (low < high)
        {
            var middle = low + (high - low) / 2;
            var present = await exists(middle);
            if (present is null) return null;
            if (present == true) low = middle + 1; else high = middle;
        }
        return low;
    }

    public int? RememberedLocalTotal(string username, SyncCatalogKind kind) =>
        _walks.TryGetValue(WalkKey(username, kind), out var memo) && Fresh(memo) ? memo.LocalTotal : null;

    /// <summary>The catalog this walk started on, for its later pages.</summary>
    public SyncCatalog? PinnedCatalog(string username, SyncCatalogKind kind) =>
        _walks.TryGetValue(WalkKey(username, kind), out var memo) && Fresh(memo) ? memo.Catalog : null;

    public void Remember(string username, SyncCatalogKind kind, int localTotal, SyncCatalog catalog) =>
        _walks[WalkKey(username, kind)] = new WalkMemo(localTotal, catalog, DateTime.UtcNow);

    private static string WalkKey(string username, SyncCatalogKind kind) => username + "\n" + kind;

    private static bool Fresh(WalkMemo memo) => DateTime.UtcNow - memo.AtUtc < WalkTtl;

    public static (IReadOnlyList<Song> Songs, IReadOnlyList<Album> Albums, IReadOnlyList<Artist> Artists)
        Slice(SyncCatalog catalog, SyncCatalogKind kind, int start, int take)
    {
        IReadOnlyList<T> Take<T>(IReadOnlyList<T> rows) =>
            start >= rows.Count || take <= 0 ? [] : rows.Skip(start).Take(take).ToList();
        return kind switch
        {
            SyncCatalogKind.Artist => ([], [], Take(catalog.Artists)),
            SyncCatalogKind.Album => ([], Take(catalog.Albums), []),
            _ => (Take(catalog.Songs), [], []),
        };
    }

    // ---------------------------------------------------------------------------------
    // Building.
    // ---------------------------------------------------------------------------------

    /// <summary>The user's catalog as last built, if any. Lets other responses describe a
    /// catalog song exactly as the sync did.</summary>
    public bool TryGetSong(string username, string id, out Song song)
    {
        song = null!;
        return _catalogs.TryGetValue(username, out var catalog) && catalog.TryGetSong(id, out song);
    }

    /// <summary>Starts a build if the cached catalog is stale, without waiting for it.</summary>
    public void Warm(string username, IReadOnlyList<LastFmRadioStation> stations,
        IReadOnlyDictionary<string, string> authenticatedParameters) =>
        _ = GetAsync(username, stations, authenticatedParameters)
            .ContinueWith(task => _ = task.Exception, TaskContinuationOptions.OnlyOnFaulted);

    /// <summary>
    /// The user's catalog for these stations: the cached one while the stations are
    /// unchanged and it is younger than <see cref="CatalogTtl"/>, else a build. One build per
    /// user at a time, shared by every caller, and never cancelled by the request that
    /// started it, because the next page of the same walk is about to want it.
    /// </summary>
    public Task<SyncCatalog> GetAsync(string username, IReadOnlyList<LastFmRadioStation> stations,
        IReadOnlyDictionary<string, string> authenticatedParameters)
    {
        if (string.IsNullOrEmpty(username) || stations.Count == 0) return Task.FromResult(SyncCatalog.Empty);
        var fingerprint = Fingerprint(stations, _settings.CurrentValue);
        if (_catalogs.TryGetValue(username, out var cached) && cached.Fingerprint == fingerprint
            && DateTime.UtcNow - cached.BuiltUtc < CatalogTtl)
            return Task.FromResult(cached);

        // A lock rather than AddOrUpdate: its factory can run more than once under
        // contention, and here running it means starting a build.
        lock (_builds)
        {
            if (_builds.TryGetValue(username, out var running) && running.Fingerprint == fingerprint
                && !running.Task.IsCompleted)
                return running.Task;
            var auth = authenticatedParameters.ToDictionary(pair => pair.Key, pair => pair.Value);
            var task = StartBuild(username, stations, auth, fingerprint);
            _builds[username] = (fingerprint, task);
            return task;
        }
    }

    private Task<SyncCatalog> StartBuild(string username, IReadOnlyList<LastFmRadioStation> stations,
        Dictionary<string, string> auth, string fingerprint)
    {
        // Detached from the request that asked: the build outlives it, and must not carry
        // that request's HttpContext into the relays it makes.
        using var _ = ExecutionContext.SuppressFlow();
        return Task.Run(async () =>
        {
            try
            {
                var previous = _catalogs.TryGetValue(username, out var old) ? old : null;
                var built = await BuildAsync(stations, auth, fingerprint, previous);
                _catalogs[username] = built;
                _logger.LogInformation(
                    "Sync catalog for {User}: {Songs} songs, {Albums} albums, {Artists} artists from {Stations} stations",
                    username, built.Songs.Count, built.Albums.Count, built.Artists.Count, stations.Count);
                return built;
            }
            catch (Exception ex)
            {
                _logger.LogWarning(ex, "Sync catalog build failed for {User}", username);
                return _catalogs.TryGetValue(username, out var fallback) ? fallback : SyncCatalog.Empty;
            }
        });
    }

    internal static string Fingerprint(IReadOnlyList<LastFmRadioStation> stations, SubsonicSettings settings) =>
        string.Join("|", stations.Select(station => station.Id + ":" + station.ChangedUtc.Ticks))
        + "#" + settings.EffectiveSyncCatalogMaxSongs + "#" + settings.ExplicitFilter;

    /// <summary>
    /// Station tracks in round-robin order, one from each station in turn, de-duplicated. A
    /// cap then trims every station a little rather than dropping the last stations whole.
    /// </summary>
    internal static List<LastFmRadioTrack> Interleave(IReadOnlyList<LastFmRadioStation> stations, int limit)
    {
        var output = new List<LastFmRadioTrack>();
        var seen = new HashSet<string>(StringComparer.Ordinal);
        var depth = stations.Count == 0 ? 0 : stations.Max(station => station.Tracks.Count);
        for (var index = 0; index < depth && output.Count < limit; index++)
            foreach (var station in stations)
            {
                if (index >= station.Tracks.Count || output.Count >= limit) continue;
                var track = station.Tracks[index];
                if (string.IsNullOrWhiteSpace(track.Artist) || string.IsNullOrWhiteSpace(track.Title)) continue;
                if (seen.Add(TrackMatchComparer.Normalize(track.Artist) + "|" + TrackMatchComparer.Normalize(track.Title)))
                    output.Add(track);
            }
        return output;
    }

    /// <summary>What the library holds for one artist: its id, its albums by name, and its
    /// recordings. Null when the lookup failed, which leaves that artist's tracks out rather
    /// than risk handing the device a copy of a song it already has.</summary>
    internal sealed record LibraryArtist(string? ArtistId, IReadOnlyDictionary<string, string> AlbumIds,
        IReadOnlyList<(string Artist, string Title)> Songs)
    {
        public bool Owns(string artist, string title) =>
            Songs.Any(song => LastFmRadioTrackResolver.IsSameRecording(artist, title, song.Artist, song.Title));
    }

    private async Task<SyncCatalog> BuildAsync(IReadOnlyList<LastFmRadioStation> stations,
        Dictionary<string, string> auth, string fingerprint, SyncCatalog? previous)
    {
        var settings = _settings.CurrentValue;
        var tracks = Interleave(stations, settings.EffectiveSyncCatalogMaxSongs);
        if (tracks.Count == 0) return new SyncCatalog([], [], [], new Dictionary<string, DateTime>(), fingerprint, DateTime.UtcNow);

        // One library lookup per artist answers everything the catalog needs to know: which
        // of the artist's tracks are owned (they are already on the device), and the ids to
        // file the rest under so an artist or album the user owns does not appear twice.
        var artists = tracks.GroupBy(track => TrackMatchComparer.Normalize(track.Artist))
            .Select(group => group.First().Artist).ToList();
        var library = new ConcurrentDictionary<string, LibraryArtist?>(StringComparer.Ordinal);
        using (var scope = _scopes.CreateScope())
        {
            var proxy = scope.ServiceProvider.GetRequiredService<SubsonicProxyService>();
            await Parallel.ForEachAsync(artists,
                new ParallelOptions { MaxDegreeOfParallelism = LookupConcurrency },
                async (artist, _) =>
                    library[TrackMatchComparer.Normalize(artist)] = await LookupArtistAsync(proxy, auth, artist));
        }

        var songs = new List<Song>();
        var seenIds = new HashSet<string>(StringComparer.Ordinal);
        var localIds = new HashSet<string>(StringComparer.Ordinal);
        foreach (var track in tracks)
        {
            if (!library.TryGetValue(TrackMatchComparer.Normalize(track.Artist), out var owned) || owned is null)
                continue;
            if (owned.Owns(track.Artist, track.Title)) continue;

            // The same call, with the same duration, that the station playlist makes, so the
            // placeholder id here is the id the playlist lists.
            var hit = (await _metadata.SearchSongsByArtistTitleAsync(track.Artist, track.Title, 1, track.Duration))
                .FirstOrDefault();
            if (hit is null || hit.Id.Length == 0 || !seenIds.Add(hit.Id)) continue;
            if (settings.ExplicitFilter == ExplicitFilter.CleanOnly && hit.ExplicitContentLyrics is 1) continue;
            if (settings.ExplicitFilter == ExplicitFilter.ExplicitOnly && hit.ExplicitContentLyrics is 3) continue;

            // Same fallback the response builder applies, made here so the album rows and the
            // songs agree on it: a single is its own album.
            var album = !string.IsNullOrWhiteSpace(hit.Album) ? hit.Album
                : !string.IsNullOrWhiteSpace(track.Album) ? track.Album!
                : hit.Title;
            hit.Album = album;
            hit.Genre ??= track.Genre;
            hit.Year ??= track.Year;
            hit.Duration ??= track.Duration;

            if (owned.ArtistId is { Length: > 0 } artistId) { hit.ArtistId = artistId; localIds.Add(artistId); }
            else hit.ArtistId = _registry.Register(new SoulseekRouting { Kind = RoutingKind.Artist, Artist = hit.Artist });

            if (owned.ArtistId is not null
                && owned.AlbumIds.TryGetValue(TrackMatchComparer.Normalize(album), out var albumId))
            { hit.AlbumId = albumId; localIds.Add(albumId); }
            else hit.AlbumId = _registry.Register(new SoulseekRouting
                { Kind = RoutingKind.Album, Artist = hit.Artist, Album = album });

            songs.Add(hit);
        }

        var albums = songs.Where(song => !localIds.Contains(song.AlbumId!))
            .GroupBy(song => song.AlbumId!, StringComparer.Ordinal)
            .Select(group =>
            {
                var first = group.First();
                return new Album
                {
                    Id = group.Key, Title = first.Album, Artist = first.Artist, ArtistId = first.ArtistId,
                    SongCount = group.Count(), Year = group.Select(song => song.Year).FirstOrDefault(year => year is not null),
                    Genre = group.Select(song => song.Genre).FirstOrDefault(genre => !string.IsNullOrWhiteSpace(genre)),
                    IsLocal = false, ExternalProvider = first.ExternalProvider,
                };
            }).ToList();

        var catalogArtists = songs.Where(song => !localIds.Contains(song.ArtistId!))
            .GroupBy(song => song.ArtistId!, StringComparer.Ordinal)
            .Select(group => new Artist
            {
                Id = group.Key, Name = group.First().Artist,
                AlbumCount = group.Select(song => song.AlbumId).Distinct(StringComparer.Ordinal).Count(),
                IsLocal = false, ExternalProvider = group.First().ExternalProvider,
            }).ToList();

        var now = DateTime.UtcNow;
        var added = new Dictionary<string, DateTime>(StringComparer.Ordinal);
        foreach (var id in songs.Select(song => song.Id).Concat(albums.Select(album => album.Id))
                     .Concat(catalogArtists.Select(artist => artist.Id)))
            added[id] = previous?.Added.TryGetValue(id, out var first) == true ? first : now;

        return new SyncCatalog(songs, albums, catalogArtists, added, fingerprint, now);
    }

    private async Task<LibraryArtist?> LookupArtistAsync(SubsonicProxyService proxy,
        IReadOnlyDictionary<string, string> auth, string artist)
    {
        var parameters = auth.ToDictionary(pair => pair.Key, pair => pair.Value);
        parameters["query"] = artist;
        parameters["artistCount"] = "10"; parameters["artistOffset"] = "0";
        parameters["albumCount"] = "200"; parameters["albumOffset"] = "0";
        parameters["songCount"] = "500"; parameters["songOffset"] = "0";
        parameters["f"] = "json";
        try
        {
            var result = await proxy.RelaySafeAsync("rest/search3", parameters);
            if (!result.Success || result.Body is not { Length: > 0 }) return null;
            return ParseLibraryArtist(result.Body, artist);
        }
        catch (Exception ex)
        {
            _logger.LogDebug(ex, "sync catalog library lookup failed for {Artist}", artist);
            return null;
        }
    }

    /// <summary>Reads a Navidrome search3 answer for an artist name. Only an exact (normalized)
    /// name counts as the same artist: a search for "Air" also returns "Airbourne".</summary>
    internal static LibraryArtist? ParseLibraryArtist(byte[] body, string artist)
    {
        using var document = JsonDocument.Parse(body);
        if (!document.RootElement.TryGetProperty("subsonic-response", out var response)) return null;
        if (response.TryGetProperty("status", out var status) && status.GetString() != "ok") return null;
        var want = TrackMatchComparer.Normalize(artist);
        if (!response.TryGetProperty("searchResult3", out var result))
            return new LibraryArtist(null, new Dictionary<string, string>(), []);

        static string Text(JsonElement element, string name) =>
            element.TryGetProperty(name, out var value) && value.ValueKind == JsonValueKind.String
                ? value.GetString() ?? "" : "";
        static IEnumerable<JsonElement> Rows(JsonElement parent, string name) =>
            parent.TryGetProperty(name, out var rows) && rows.ValueKind == JsonValueKind.Array
                ? rows.EnumerateArray() : [];

        var artistId = Rows(result, "artist")
            .Where(row => TrackMatchComparer.Normalize(Text(row, "name")) == want)
            .Select(row => Text(row, "id")).FirstOrDefault(id => id.Length > 0);

        var albums = new Dictionary<string, string>(StringComparer.Ordinal);
        if (artistId is not null)
            foreach (var row in Rows(result, "album"))
            {
                var byArtist = Text(row, "artistId") == artistId
                    || TrackMatchComparer.Normalize(Text(row, "artist")) == want;
                var name = TrackMatchComparer.Normalize(Text(row, "name"));
                var id = Text(row, "id");
                if (byArtist && name.Length > 0 && id.Length > 0) albums.TryAdd(name, id);
            }

        var songs = Rows(result, "song").Select(row => (Text(row, "artist"), Text(row, "title"))).ToList();
        return new LibraryArtist(artistId, albums, songs);
    }
}
