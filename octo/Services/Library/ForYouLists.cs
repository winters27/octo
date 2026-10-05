using System.Collections.Concurrent;
using System.Globalization;
using System.Text.Json.Nodes;
using Octo.Models.Domain;
using Octo.Models.Settings;
using Octo.Services.Common;
using Octo.Services.Metadata;
using Octo.Services.Soulseek;

namespace Octo.Services.Library;

/// <summary>
/// Made for you: the picking behind New Releases, Rediscover and Deep Cuts. Everything here
/// reads one walk of a listener's library, as that listener sees it, so each song carries their
/// own playCount, played (last played), starred and userRating. Pure, so it is tested without a
/// server; the walk and the catalog calls live in <see cref="GeneratedPlaylistService"/> and
/// <see cref="NewReleasesBuilder"/>.
/// </summary>
public static class ForYouLists
{
    public const string NewReleasesKind = "newReleases";
    public const string RediscoverKind = "rediscover";
    public const string DeepCutsKind = "deepCuts";

    /// <summary>The three, in the order they are listed: newest first, then what is old to you.</summary>
    public static readonly string[] Kinds = [NewReleasesKind, RediscoverKind, DeepCutsKind];

    public const int TrackCount = 50;
    public const int RediscoverPerArtist = 3;
    public const int DeepCutsPerArtist = 4;
    public const int DeepCutArtists = 15;
    public const int SongsPerRelease = 3;
    private const int RediscoverPool = 150;
    private const int LovedPlays = 3;
    private const int LovedRating = 4;

    public static string Name(string kind) => kind switch
    {
        NewReleasesKind => "New Releases",
        RediscoverKind => "Rediscover",
        DeepCutsKind => "Deep Cuts",
        _ => kind,
    };

    public static bool IsForYou(string kind) => Kinds.Contains(kind, StringComparer.Ordinal);

    /// <summary>An artist of the library by how much the listener plays them.</summary>
    public sealed record TopArtist(string Key, string Name, int Plays, int Hearts, int Songs);

    /// <summary>
    /// The listener's most-played artists: plays summed per artist. Someone with no plays yet is
    /// read by their hearts, then by how much of each artist they keep. "Various Artists" is
    /// nobody's favorite artist.
    /// </summary>
    public static IReadOnlyList<TopArtist> TopArtists(IReadOnlyList<JsonObject> songs, int count)
    {
        var various = SongIdentity.Key("Various Artists");
        var artists = songs
            .GroupBy(ArtistKey, StringComparer.Ordinal)
            .Select(group => new TopArtist(
                group.Key,
                group.Select(song => Str(song, "artist") ?? "")
                    .Where(name => name.Length > 0)
                    .GroupBy(name => name, StringComparer.Ordinal)
                    .OrderByDescending(names => names.Count()).ThenBy(names => names.Key, StringComparer.Ordinal)
                    .Select(names => names.Key).FirstOrDefault() ?? "",
                group.Sum(song => Int(song, "playCount")),
                group.Count(IsStarred),
                group.Count()))
            .Where(artist => artist.Name.Length > 0 && SongIdentity.Key(artist.Name) != various)
            .ToList();
        // Only artists the listener played or hearted are theirs; an artist they never touched is
        // never one of "your artists", however few they have. Someone with no plays or hearts at
        // all gets the library's biggest artists, so a new listener still has lists.
        var theirs = artists.Where(artist => artist.Plays > 0 || artist.Hearts > 0).ToList();
        return (theirs.Count > 0
                ? theirs.OrderByDescending(a => a.Plays).ThenByDescending(a => a.Hearts).ThenByDescending(a => a.Songs)
                : artists.OrderByDescending(a => a.Songs))
            .ThenBy(a => a.Name, StringComparer.Ordinal)
            .Take(count)
            .ToList();
    }

    /// <summary>Played often, hearted, or rated 4 or more.</summary>
    public static bool Loved(JsonObject song) =>
        Int(song, "playCount") >= LovedPlays || IsStarred(song) || Int(song, "userRating") >= LovedRating;

    /// <summary>
    /// Songs the listener loved and has not played for <paramref name="months"/>: the best loved
    /// first, then a seeded draw from them so the list changes from day to day. A song with no
    /// last-played date says nothing about when, so it is left out.
    /// </summary>
    public static IReadOnlyList<JsonObject> Rediscover(IReadOnlyList<JsonObject> songs, DateTime nowUtc, int months, int seed)
    {
        var before = nowUtc.AddMonths(-months);
        var pool = songs
            .Where(song => Loved(song) && Played(song) is { } played && played < before)
            .OrderByDescending(Score)
            .ThenBy(song => Str(song, "id"), StringComparer.Ordinal)
            .Take(RediscoverPool)
            .ToList();
        return Draw(pool, TrackCount, RediscoverPerArtist, seed);
    }

    /// <summary>
    /// Songs played at most once, from the listener's most-played artists, none of them in
    /// Rediscover: the corners of artists they already like.
    /// </summary>
    public static IReadOnlyList<JsonObject> DeepCuts(IReadOnlyList<JsonObject> songs, IReadOnlyCollection<string> artistKeys,
        IReadOnlySet<string> excludedIds, int seed)
    {
        var artists = artistKeys.ToHashSet(StringComparer.Ordinal);
        var pool = songs
            .Where(song => artists.Contains(ArtistKey(song)) && Int(song, "playCount") <= 1
                && Str(song, "id") is { } id && !excludedIds.Contains(id))
            .OrderBy(song => Str(song, "id"), StringComparer.Ordinal)
            .ToList();
        return Draw(pool, TrackCount, DeepCutsPerArtist, seed);
    }

    /// <summary>
    /// An artist's releases out in the last <paramref name="weeks"/> weeks, newest first: albums,
    /// EPs and singles, never compilations, never a date still to come, never one with no date.
    /// </summary>
    public static IReadOnlyList<DeezerMetadataService.AlbumHit> RecentReleases(
        IEnumerable<DeezerMetadataService.AlbumHit> releases, DateOnly today, int weeks)
    {
        var since = today.AddDays(-7 * weeks);
        return releases
            .Select(release => (Release: release, Date: ReleaseDate(release)))
            .Where(item => item.Date is { } date && date >= since && date <= today
                && item.Release.RecordType?.ToLowerInvariant() is "album" or "ep" or "single")
            .OrderByDescending(item => item.Date)
            .ThenBy(item => item.Release.Title, StringComparer.Ordinal)
            .Select(item => item.Release)
            .ToList();
    }

    public static DateOnly? ReleaseDate(DeezerMetadataService.AlbumHit release) =>
        release.ReleaseDate is { Length: >= 10 } text
        && DateOnly.TryParseExact(text[..10], "yyyy-MM-dd", CultureInfo.InvariantCulture, DateTimeStyles.None, out var date)
            ? date
            : null;

    /// <summary>
    /// The songs a release brings to New Releases: a single's or an EP's first few, an album's
    /// best by the catalog's popularity, in album order when it gives none.
    /// </summary>
    public static IReadOnlyList<DeezerMetadataService.AlbumTrack> PickTracks(string? recordType,
        IReadOnlyList<DeezerMetadataService.AlbumTrack> tracks)
    {
        var inOrder = tracks
            .OrderBy(track => track.DiscNumber ?? 1)
            .ThenBy(track => track.TrackPosition ?? int.MaxValue)
            .ToList();
        if (recordType?.ToLowerInvariant() != "album" || inOrder.All(track => track.Rank is null))
            return inOrder.Take(SongsPerRelease).ToList();
        return inOrder
            .Select((track, index) => (Track: track, Index: index))
            .OrderByDescending(item => item.Track.Rank ?? -1)
            .ThenBy(item => item.Index)
            .Take(SongsPerRelease)
            .OrderBy(item => item.Index)
            .Select(item => item.Track)
            .ToList();
    }

    /// <summary>When the listener last played the song, from Navidrome's <c>played</c>.</summary>
    public static DateTime? Played(JsonObject song) =>
        Str(song, "played") is { } text
        && DateTime.TryParse(text, CultureInfo.InvariantCulture, DateTimeStyles.RoundtripKind, out var at)
            ? at.ToUniversalTime()
            : null;

    private static double Score(JsonObject song) =>
        Int(song, "playCount") + (IsStarred(song) ? 5 : 0) + 2 * Int(song, "userRating");

    /// <summary>A seeded shuffle, never more than <paramref name="maxPerArtist"/> by one artist.</summary>
    internal static IReadOnlyList<JsonObject> Draw(IReadOnlyList<JsonObject> pool, int count, int maxPerArtist, int seed)
    {
        var shuffled = pool.ToList();
        var random = new Random(seed);
        for (var i = shuffled.Count - 1; i > 0; i--)
        {
            var j = random.Next(i + 1);
            (shuffled[i], shuffled[j]) = (shuffled[j], shuffled[i]);
        }
        var perArtist = new Dictionary<string, int>(StringComparer.Ordinal);
        var taken = new List<JsonObject>();
        foreach (var song in shuffled)
        {
            if (taken.Count >= count) break;
            var artist = ArtistKey(song);
            var already = perArtist.GetValueOrDefault(artist);
            if (already >= maxPerArtist) continue;
            perArtist[artist] = already + 1;
            taken.Add(song);
        }
        return taken;
    }

    internal static string ArtistKey(JsonObject song) =>
        Str(song, "artistId") is { Length: > 0 } id ? id : SongIdentity.Key(Str(song, "artist"));

    private static bool IsStarred(JsonObject song) => Str(song, "starred") is { Length: > 0 };

    internal static string? Str(JsonObject node, string name) =>
        node[name] is JsonValue value && value.TryGetValue<string>(out var text) ? text : null;

    internal static int Int(JsonObject node, string name) =>
        node[name] is JsonValue value
            ? value.TryGetValue<int>(out var number) ? number
            : value.TryGetValue<long>(out var wide) ? (int)Math.Clamp(wide, int.MinValue, int.MaxValue)
            : value.TryGetValue<double>(out var real) ? (int)real
            : 0
            : 0;
}

/// <summary>
/// New Releases for one listener: their most-played artists found in the catalog (Deezer,
/// keyless, on its background lane so a refresh never slows search), each artist's releases of
/// the last weeks, and a few songs from each. A song the listener already has is their own copy;
/// the rest are outside songs, registered exactly as the artist "Top songs" ones are, so they
/// play, open their album and are added with "+".
/// </summary>
public sealed class NewReleasesBuilder(DeezerMetadataService deezer, ExternalIdRegistry registry, ILogger<NewReleasesBuilder> logger)
{
    private static readonly TimeSpan Found = TimeSpan.FromDays(7);
    private static readonly TimeSpan Missed = TimeSpan.FromDays(1);
    private const int ArtistCandidates = 5;
    private const int ReleasesAtOnce = 4;
    private const int ArtistAlbumPages = 3;

    // One listener's catalog walk at a time. The catalog's background lane takes two calls a
    // second and every call, its wait included, gives up after 8 seconds: listeners refreshing
    // together would time each other out, and a timed-out artist looks like one with no releases.
    private readonly SemaphoreSlim _oneAtATime = new(1, 1);

    // A catalog artist per library artist name, or none, for a while: names do not move.
    private readonly ConcurrentDictionary<string, (string? Id, DateTime Until)> _artists = new(StringComparer.Ordinal);

    /// <summary>
    /// What an outside song of New Releases is made from, kept with the list so the song can be
    /// made again, with the same id, whenever the list is served: Octo remembers outside songs
    /// only so long (<see cref="ExternalIdRegistry"/> keeps the most recent 10,000), and the list
    /// outlives a busy day of searches.
    /// </summary>
    public sealed record CatalogSeed(string Title, string Artist, string? ArtistId, string? Album, string? AlbumId,
        string? CoverUrl, int? Duration, int? Year, int? Track, int? ExplicitContentLyrics);

    /// <summary>The outside song a seed makes, registered so it plays, opens its album and is added with "+".</summary>
    public static Song Mint(ExternalIdRegistry registry, CatalogSeed seed)
    {
        var song = TopSongsService.CatalogSong(registry, new DeezerMetadataService.ChartTrack(
            "", seed.Title, seed.Artist, seed.ArtistId, seed.Album, seed.AlbumId, seed.CoverUrl, seed.Duration, null));
        song.Year = seed.Year;
        song.Track = seed.Track;
        song.ExplicitContentLyrics = seed.ExplicitContentLyrics;
        return song;
    }

    /// <summary>Whether the server's explicit filter lets an outside song in, as search and radio apply it.</summary>
    public static bool ExplicitAllowed(int? explicitContentLyrics, ExplicitFilter filter) => filter switch
    {
        ExplicitFilter.CleanOnly => explicitContentLyrics is not 1,
        ExplicitFilter.ExplicitOnly => explicitContentLyrics is not 3,
        _ => true,
    };

    /// <summary>One entry of New Releases: a library song as Navidrome sent it, or an outside song and its seed.</summary>
    public sealed record Entry(JsonObject? Library, Song? Outside, string? CoverUrl, CatalogSeed? Seed = null);

    /// <summary>What a build came to, and whether the catalog answered for every artist asked.</summary>
    public sealed record Result(IReadOnlyList<Entry> Entries, bool Whole);

    public async Task<Result> BuildAsync(IReadOnlyList<ForYouLists.TopArtist> artists, IReadOnlyList<JsonObject> library,
        DateOnly today, int weeks, CancellationToken ct, ExplicitFilter explicitFilter = ExplicitFilter.All)
    {
        await _oneAtATime.WaitAsync(ct);
        try
        {
            return await BuildOneAsync(artists, library, today, weeks, explicitFilter, ct);
        }
        finally
        {
            _oneAtATime.Release();
        }
    }

    private async Task<Result> BuildOneAsync(IReadOnlyList<ForYouLists.TopArtist> artists, IReadOnlyList<JsonObject> library,
        DateOnly today, int weeks, ExplicitFilter explicitFilter, CancellationToken ct)
    {
        var owned = new Dictionary<string, JsonObject>(StringComparer.Ordinal);
        foreach (var song in library)
            owned.TryAdd(SongIdentity.MatchKey(ForYouLists.Str(song, "artist"), ForYouLists.Str(song, "title")), song);

        var whole = true;
        var releases = new List<(DeezerMetadataService.AlbumHit Release, string ArtistName, string ArtistId)>();
        // The catalog answers a throttled or failed call with nothing, just as it answers a name it
        // does not know. Nothing at all, for every artist asked, is read as no answer, so a bad
        // moment never replaces yesterday's list with an empty one.
        var resolved = 0;
        var listedAny = false;
        foreach (var artist in artists)
        {
            ct.ThrowIfCancellationRequested();
            var id = await ResolveAsync(artist.Name, ct);
            if (id is null) continue;
            resolved++;
            var albums = await deezer.GetArtistAlbumsAsync(id, artist.Name, ct, background: true, pages: ArtistAlbumPages);
            if (albums.Count > 0) listedAny = true;
            foreach (var release in ForYouLists.RecentReleases(albums, today, weeks))
                if (releases.All(known => known.Release.DeezerId != release.DeezerId))
                    releases.Add((release, artist.Name, id));
        }
        if (artists.Count > 0 && (resolved == 0 || !listedAny)) whole = false;

        var picked = new (DeezerMetadataService.AlbumHit Release, IReadOnlyList<DeezerMetadataService.AlbumTrack> Tracks,
            string ArtistName, string ArtistId)?[releases.Count];
        using var gate = new SemaphoreSlim(ReleasesAtOnce);
        await Task.WhenAll(releases.Select(async (item, index) =>
        {
            await gate.WaitAsync(ct);
            try
            {
                var lookup = await deezer.LookUpAlbumDetailAsync(item.Release.DeezerId, ct, background: true);
                if (lookup.Detail is { } detail)
                    picked[index] = (item.Release, ForYouLists.PickTracks(detail.RecordType ?? item.Release.RecordType, detail.Tracks),
                        item.ArtistName, item.ArtistId);
                else if (lookup.Answer == DeezerMetadataService.AlbumAnswer.Unavailable)
                    whole = false;
            }
            finally
            {
                gate.Release();
            }
        }));

        var entries = new List<Entry>();
        var listed = new HashSet<string>(StringComparer.Ordinal);
        foreach (var (release, tracks, artistName, artistId) in picked
                     .OfType<(DeezerMetadataService.AlbumHit, IReadOnlyList<DeezerMetadataService.AlbumTrack>, string, string)>()
                     .OrderByDescending(item => ForYouLists.ReleaseDate(item.Item1)))
        {
            foreach (var track in tracks)
            {
                if (entries.Count >= ForYouLists.TrackCount) break;
                var key = SongIdentity.MatchKey(track.Artist, track.Title);
                // The catalog may credit a shared song to the other artist (a release on Ella
                // Langley's page credited to Miranda Lambert), while the library files it under the
                // listener's artist who led here: the same title under that artist is theirs too.
                var byTheirArtist = SongIdentity.MatchKey(artistName, track.Title);
                if (listed.Contains(key) || listed.Contains(byTheirArtist)) continue;
                var isTheirs = owned.TryGetValue(key, out var mine) || owned.TryGetValue(byTheirArtist, out mine);
                // The listener's own copy is theirs whatever the filter; an outside song obeys it.
                if (!isTheirs && !ExplicitAllowed(track.ExplicitContentLyrics, explicitFilter)) continue;
                listed.Add(key);
                listed.Add(byTheirArtist);
                if (isTheirs)
                {
                    entries.Add(new Entry(mine!.DeepClone().AsObject(), null, release.CoverUrl));
                    continue;
                }
                // The catalog artist found for the listener's artist is this song's when it is
                // credited to them, so its artist link opens the right page.
                var seed = new CatalogSeed(track.Title, track.Artist,
                    SongIdentity.Key(track.Artist) == SongIdentity.Key(artistName) ? artistId : null,
                    release.Title, release.DeezerId, release.CoverUrl, track.Duration,
                    ForYouLists.ReleaseDate(release)?.Year, track.TrackPosition, track.ExplicitContentLyrics);
                entries.Add(new Entry(null, Mint(registry, seed), release.CoverUrl, seed));
            }
        }
        logger.LogInformation("New Releases: {Releases} releases from {Artists} artists, {Songs} songs",
            releases.Count, artists.Count, entries.Count);
        return new Result(entries, whole);
    }

    /// <summary>
    /// The catalog's artist for a library artist's name: the one whose name is the same name,
    /// most followed first, since two acts can share a name and the bigger one is the likelier.
    /// </summary>
    private async Task<string?> ResolveAsync(string name, CancellationToken ct)
    {
        var key = SongIdentity.Key(name);
        if (key.Length == 0) return null;
        if (_artists.TryGetValue(key, out var known) && known.Until > DateTime.UtcNow) return known.Id;
        var hits = await deezer.SearchArtistsAsync(name, ArtistCandidates, ct, background: true);
        // No candidates at all may be a throttled answer: not remembered, asked again next time.
        if (hits.Count == 0) return null;
        var id = hits
            .Where(hit => SongIdentity.Key(hit.Name) == key)
            .OrderByDescending(hit => hit.Fans)
            .Select(hit => hit.DeezerId)
            .FirstOrDefault();
        // Candidates but none of that very name: the catalog does not know them, for a day.
        _artists[key] = (id, DateTime.UtcNow + (id is null ? Missed : Found));
        return id;
    }
}
