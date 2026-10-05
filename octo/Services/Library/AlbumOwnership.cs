using System.Diagnostics;
using System.Text.RegularExpressions;
using Octo.Models.Domain;
using Octo.Services.Common;
using Octo.Services.Metadata;
using Octo.Services.Soulseek;

namespace Octo.Services.Library;

/// <summary>
/// The whole library as album ownership reads it: every song by its MatchKey, its title and its
/// ISRC, and every album by its name. Built once per read of the library, so a search weighs a
/// page of outside albums against it without asking Navidrome anything.
/// </summary>
public sealed class LibraryAlbumIndex
{
    /// <summary>One library album, with its songs as the index read them.</summary>
    public sealed record LibraryAlbum(string Id, string Name, string? AlbumArtist, IReadOnlyList<LibrarySongRow> Songs);

    private static readonly SongMatchOptions Lenient = new() { LengthToleranceSeconds = LibraryOwnership.DurationToleranceSeconds };

    private readonly Dictionary<string, List<LibrarySongRow>> _byKey = new(StringComparer.Ordinal);
    private readonly Dictionary<string, List<LibrarySongRow>> _byTitle = new(StringComparer.Ordinal);
    private readonly Dictionary<string, List<LibrarySongRow>> _byIsrc = new(StringComparer.Ordinal);
    private readonly Dictionary<string, LibraryAlbum> _albums = new(StringComparer.Ordinal);
    private readonly Dictionary<string, List<LibraryAlbum>> _albumsByName = new(StringComparer.Ordinal);
    private readonly Dictionary<string, List<LibraryAlbum>> _albumsByBase = new(StringComparer.Ordinal);
    private readonly HashSet<string> _artists = new(StringComparer.Ordinal);

    public LibraryAlbumIndex(IEnumerable<LibrarySongRow> rows)
    {
        Rows = rows as IReadOnlyList<LibrarySongRow> ?? rows.ToList();
        var byAlbum = new Dictionary<string, List<LibrarySongRow>>(StringComparer.Ordinal);
        foreach (var row in Rows)
        {
            if (string.IsNullOrWhiteSpace(row.Title)) continue;
            Add(_byKey, SongIdentity.MatchKey(row.Artist, row.Title), row);
            Add(_byTitle, SongIdentity.ParseTitle(row.Title, row.Artist).Key, row);
            foreach (var isrc in SongIdentity.Isrcs(row.Isrcs ?? [])) Add(_byIsrc, isrc, row);
            foreach (var key in ArtistKeys(row.Artist).Concat(ArtistKeys(row.AlbumArtist))) _artists.Add(key);
            if (!string.IsNullOrEmpty(row.AlbumId)) Add(byAlbum, row.AlbumId, row);
            SongCount++;
        }
        foreach (var (id, songs) in byAlbum)
        {
            var album = new LibraryAlbum(id, songs[0].Album ?? "", songs.Select(s => s.AlbumArtist).FirstOrDefault(a => !string.IsNullOrWhiteSpace(a)), songs);
            _albums[id] = album;
            Add(_albumsByName, SongIdentity.Key(album.Name), album);
            Add(_albumsByBase, AlbumOwnership.BaseKey(album.Name), album);
        }
    }

    /// <summary>How many songs the library has.</summary>
    public int SongCount { get; }

    /// <summary>Every row as it was read, for the checks that look at the whole library (Library health).</summary>
    public IReadOnlyList<LibrarySongRow> Rows { get; }

    public LibraryAlbum? Album(string id) => _albums.GetValueOrDefault(id);

    /// <summary>
    /// The library's copy of a catalog album's song, or null. The same ISRC; the same MatchKey
    /// at a length within 8 seconds; the same title with the credit written another way; or,
    /// last, the same title on an album of the same name at that length, which is how a song a
    /// library credits to the guest ("Drake") is still found for the catalog's "PARTYNEXTDOOR".
    /// A version ("Live", "Remix", "Sped Up") is always its own song.
    /// </summary>
    public LibrarySongRow? Find(string title, string? artist, int? seconds, IEnumerable<string?> isrcs, string? albumTitle)
    {
        foreach (var isrc in SongIdentity.Isrcs(isrcs))
            if (_byIsrc.TryGetValue(isrc, out var coded) && Closest(coded, seconds) is { } byIsrc) return byIsrc;

        if (_byKey.TryGetValue(SongIdentity.MatchKey(artist, title), out var same)
            && Closest(same.Where(row => LengthFits(seconds, row.Duration)), seconds) is { } exact)
            return exact;

        var titleKey = SongIdentity.ParseTitle(title, artist).Key;
        if (titleKey.Length == 0 || !_byTitle.TryGetValue(titleKey, out var titled)) return null;
        var wanted = new SongRef(title, artist, seconds);
        if (Closest(titled.Where(row => SongIdentity.Same(wanted, new SongRef(row.Title, row.Artist, row.Duration), Lenient).IsSame), seconds) is { } credited)
            return credited;

        var album = AlbumOwnership.BaseKey(albumTitle);
        if (album.Length == 0) return null;
        var versioned = SongIdentity.TitleKey(title);
        return Closest(titled.Where(row =>
            SongIdentity.TitleKey(row.Title) == versioned
            && AlbumOwnership.BaseKey(row.Album) == album
            && LengthFits(seconds, row.Duration)), seconds);
    }

    /// <summary>Library albums with this very name (case, accents and punctuation aside) that
    /// share an artist with <paramref name="artist"/>.</summary>
    public IReadOnlyList<LibraryAlbum> SameAlbum(string? name, string? artist)
    {
        var key = SongIdentity.Key(name);
        if (key.Length == 0 || !_albumsByName.TryGetValue(key, out var named)) return [];
        return named.Where(album => SharesArtist(album, artist)).ToList();
    }

    /// <summary>
    /// Whether the library may well hold songs of this catalog album, judged on names alone:
    /// an album of the same name once edition tags are set aside ("HABIBTI" and "HABIBTI
    /// (FOMO)"), or a song named as the album (a single), by an artist the two share. Only
    /// ever decides whether the album's songs are looked at now; never who owns what.
    /// </summary>
    public bool Likely(string? albumTitle, string? artist)
    {
        var key = AlbumOwnership.BaseKey(albumTitle);
        if (key.Length == 0) return false;
        if (_albumsByBase.TryGetValue(key, out var albums) && albums.Any(album => SharesArtist(album, artist))) return true;
        var song = SongIdentity.ParseTitle(albumTitle, artist).Key;
        return _byTitle.TryGetValue(song.Length > 0 ? song : key, out var songs)
            && songs.Any(row => ShareAnArtist(row.Artist, artist));
    }

    /// <summary>Whether any library song or album credits one of this credit's artists.</summary>
    public bool KnowsArtist(string? artist) => ArtistKeys(artist).Any(_artists.Contains);

    private static bool SharesArtist(LibraryAlbum album, string? artist) =>
        ShareAnArtist(album.AlbumArtist, artist) || album.Songs.Any(song => ShareAnArtist(song.Artist, artist));

    /// <summary>The two credits name one artist in common, whatever else each names.</summary>
    internal static bool ShareAnArtist(string? a, string? b) =>
        SongIdentity.CompareArtists(a, b) is ArtistAgreement.Agree or ArtistAgreement.Loose or ArtistAgreement.Conflict;

    private static IEnumerable<string> ArtistKeys(string? credit)
    {
        if (string.IsNullOrWhiteSpace(credit)) yield break;
        var parsed = SongIdentity.ParseArtists(credit);
        foreach (var name in parsed.Names.Append(parsed.Display))
        {
            var key = SongIdentity.Key(name);
            if (key.Length > 0) yield return key;
        }
    }

    private static bool LengthFits(int? wanted, int? found) =>
        wanted is not > 0 || found is not > 0 || Math.Abs(wanted.Value - found.Value) <= LibraryOwnership.DurationToleranceSeconds;

    private static LibrarySongRow? Closest(IEnumerable<LibrarySongRow> rows, int? seconds) =>
        rows.OrderBy(row => seconds is > 0 && row.Duration is > 0 ? Math.Abs(seconds.Value - row.Duration.Value) : 0)
            .ThenByDescending(row => DuplicateScanWorker.IsLosslessFile(row.Suffix, row.BitRate))
            .FirstOrDefault();

    private static void Add<T>(Dictionary<string, List<T>> map, string key, T value)
    {
        if (key.Length == 0) return;
        if (!map.TryGetValue(key, out var list)) map[key] = list = [];
        list.Add(value);
    }
}

/// <summary>
/// How much of an outside album the library already holds, from its songs. A name is not an
/// identity: "HABIBTI" is held whole inside the library's "HABIBTI (FOMO)", while a deluxe
/// edition is never held just because the standard one is. So the catalog album's tracklist
/// is read (cached, as album fill-in reads it) and each song looked for in the whole library,
/// whatever album, edition or album artist the library filed it under.
///
/// Search runs as people type, so not every album earns a tracklist then: an album whose name
/// the library has, or that is named as a library song (a single), is read at once; one by an
/// artist the library knows is read in the background, ready for the next search; the rest
/// are left alone. Whatever is not known in time goes out as before, without a count.
/// </summary>
public sealed class AlbumOwnership
{
    /// <summary>One catalog song, as the tracklist names it.</summary>
    public sealed record CatalogTrack(string Title, string? Artist, int? Duration, string? Isrc);

    /// <summary>
    /// What the library holds of one outside album. <see cref="Owned"/> is null when its songs
    /// were not looked at. <see cref="Holders"/> are the library albums holding the owned songs,
    /// most first; <see cref="SameName"/> the library albums with its very name and artist.
    /// </summary>
    public sealed record Standing(Album Album, int? Owned, int Total,
        IReadOnlyList<LibraryAlbumIndex.LibraryAlbum> Holders, IReadOnlyList<LibraryAlbumIndex.LibraryAlbum> SameName)
    {
        public bool FullyOwned => Owned is int owned && Total > 0 && owned >= Total;

        /// <summary>The library albums that stand for this one when it is left out: its
        /// namesake, or the albums that hold all of its songs.</summary>
        public IReadOnlyList<LibraryAlbumIndex.LibraryAlbum> StandIns => SameName.Count > 0 ? SameName : FullyOwned ? Holders : [];
    }

    private static readonly Regex TrailingBracket = new(@"\s*[\(\[][^\(\)\[\]]*[\)\]]\s*$", RegexOptions.Compiled);
    private static readonly Regex TrailingEdition = new(
        @"\s+-\s+(?:single|ep|(?:(?:super\s+)?deluxe|expanded|explicit|clean|remaster(?:ed)?|special|anniversary)(?:\s+(?:edition|version))?)\s*$",
        RegexOptions.IgnoreCase | RegexOptions.Compiled);

    /// <summary>
    /// An album title without its edition: every trailing bracket and a trailing " - Single",
    /// " - EP" or " - Deluxe Edition" go, so "HABIBTI (FOMO)", "What A Time To Be Alive
    /// (Explicit Version)" and "Take Care (Deluxe)" read as their albums. Only ever used to
    /// pair names, never to decide that two editions hold the same songs.
    /// </summary>
    public static string BaseKey(string? title)
    {
        var text = SongIdentity.Fold(title);
        while (true)
        {
            var trimmed = TrailingEdition.Replace(TrailingBracket.Replace(text, ""), "");
            if (trimmed == text || SongIdentity.Key(trimmed).Length == 0) break;
            text = trimmed;
        }
        return SongIdentity.Key(text);
    }

    private readonly ILogger<AlbumOwnership> _logger;

    public AlbumOwnership(LibrarySnapshot library, DeezerMetadataService deezer, ExternalIdRegistry ids,
        ILogger<AlbumOwnership> logger)
    {
        _logger = logger;
        Library = library.CurrentAsync;
        Tracklist = async (album, background, ct) =>
        {
            if (ids.Lookup(album.Id) is not { ExternalAlbumId: { Length: > 0 } deezerId }) return null;
            return Tracks((await deezer.SharedAlbumDetailAsync(deezerId, background, ct)).Detail);
        };
        Cached = album => ids.Lookup(album.Id) is { ExternalAlbumId: { Length: > 0 } deezerId }
            ? Tracks(deezer.CachedAlbumDetail(deezerId)?.Detail)
            : null;
    }

    private static IReadOnlyList<CatalogTrack>? Tracks(DeezerMetadataService.AlbumDetail? detail) =>
        detail?.Tracks.Select(t => new CatalogTrack(t.Title, t.Artist, t.Duration, t.Isrc)).ToList();

    /// <summary>For tests, which answer through <see cref="Library"/> and <see cref="Tracklist"/>.</summary>
    internal AlbumOwnership(ILogger<AlbumOwnership> logger)
    {
        _logger = logger;
        Library = _ => Task.FromResult<LibraryAlbumIndex?>(null);
        Tracklist = (_, _, _) => Task.FromResult<IReadOnlyList<CatalogTrack>?>(null);
        Cached = _ => null;
    }

    /// <summary>
    /// Tracklists one request may ask Deezer for, at once and in the background. Deezer's budget
    /// is shared with everything else a search does (40 requests in 5 seconds, two per album), and
    /// an artist page can name fifty albums. Albums already cached cost nothing and are always
    /// counted, so each visit counts more of a long list than the one before.
    /// </summary>
    internal const int MaxReads = 8;
    internal const int MaxBackgroundReads = 4;

    // Seams: tests answer from lists instead of Navidrome and Deezer.
    internal Func<CancellationToken, Task<LibraryAlbumIndex?>> Library { get; set; }
    internal Func<Album, bool, CancellationToken, Task<IReadOnlyList<CatalogTrack>?>> Tracklist { get; set; }
    internal Func<Album, IReadOnlyList<CatalogTrack>?> Cached { get; set; }

    /// <summary>
    /// What the library holds of each outside album, in the order given, waiting at most
    /// <paramref name="wait"/> for the library and the tracklists. Library albums pass
    /// through unjudged. Empty when the library cannot be read in time.
    /// </summary>
    public async Task<IReadOnlyList<Standing>> JudgeAsync(IReadOnlyList<Album> albums, TimeSpan wait, CancellationToken ct = default)
    {
        var outside = albums.Where(album => !album.IsLocal).ToList();
        if (outside.Count == 0) return [];
        var clock = Stopwatch.StartNew();

        LibraryAlbumIndex? index;
        try { index = await Library(ct).WaitAsync(wait, ct); }
        catch (TimeoutException)
        {
            _logger.LogDebug("album ownership: the library was not read within {Wait} ms; albums go out uncounted", wait.TotalMilliseconds);
            return [];
        }
        if (index is null) return [];

        var reads = new Dictionary<Album, Task<IReadOnlyList<CatalogTrack>?>>();
        int asked = 0, warming = 0;
        foreach (var album in outside)
        {
            if (index.SameAlbum(album.Title, album.Artist).Count > 0) continue;
            var likely = index.Likely(album.Title, album.Artist);
            if (!likely && !index.KnowsArtist(album.Artist)) continue;
            if (Cached(album) is { } cached) reads[album] = Task.FromResult<IReadOnlyList<CatalogTrack>?>(cached);
            else if (likely && asked < MaxReads) { asked++; reads[album] = Read(album, background: false, ct); }
            else if (warming < MaxBackgroundReads) { warming++; reads[album] = Read(album, background: true, ct); }
        }

        var left = wait - clock.Elapsed;
        if (reads.Count > 0 && left > TimeSpan.Zero)
            await Task.WhenAny(Task.WhenAll(reads.Values), Task.Delay(left, ct));

        var standings = outside.Select(album =>
        {
            var sameName = index.SameAlbum(album.Title, album.Artist);
            if (sameName.Count > 0) return new Standing(album, null, album.SongCount ?? 0, [], sameName);
            return reads.TryGetValue(album, out var read) && read.IsCompletedSuccessfully && read.Result is { Count: > 0 } tracks
                ? Judge(album, tracks, index)
                : new Standing(album, null, album.SongCount ?? 0, [], []);
        }).ToList();

        _logger.LogDebug("album ownership: {Albums} outside album(s), {Read} tracklist(s) asked, {Counted} counted in {Ms} ms",
            outside.Count, reads.Count, standings.Count(s => s.Owned is not null), clock.ElapsedMilliseconds);
        return standings;
    }

    /// <summary>What the library holds of a catalog album, song by song.</summary>
    public static Standing Judge(Album album, IReadOnlyList<CatalogTrack> tracks, LibraryAlbumIndex index)
    {
        var owned = 0;
        var holding = new Dictionary<string, int>(StringComparer.Ordinal);
        foreach (var track in tracks)
        {
            if (Find(index, album, track) is not { } row) continue;
            owned++;
            if (!string.IsNullOrEmpty(row.AlbumId)) holding[row.AlbumId] = holding.GetValueOrDefault(row.AlbumId) + 1;
        }
        var holders = holding.OrderByDescending(pair => pair.Value)
            .Select(pair => index.Album(pair.Key))
            .OfType<LibraryAlbumIndex.LibraryAlbum>()
            .ToList();
        return new Standing(album, owned, tracks.Count, holders, []);
    }

    /// <summary>The library's copy of one catalog song of <paramref name="album"/>, or null.</summary>
    public static LibrarySongRow? Find(LibraryAlbumIndex index, Album album, CatalogTrack track) =>
        index.Find(track.Title, string.IsNullOrWhiteSpace(track.Artist) ? album.Artist : track.Artist,
            track.Duration, [track.Isrc], album.Title);

    /// <summary>The library as it stands, waiting at most <paramref name="wait"/>; null when it
    /// cannot be read in that time.</summary>
    public async Task<LibraryAlbumIndex?> LibraryAsync(TimeSpan wait, CancellationToken ct = default)
    {
        try { return await Library(ct).WaitAsync(wait, ct); }
        catch (TimeoutException) { return null; }
    }

    private async Task<IReadOnlyList<CatalogTrack>?> Read(Album album, bool background, CancellationToken ct)
    {
        try { return await Tracklist(album, background, ct); }
        catch (Exception ex) when (ex is not OperationCanceledException)
        {
            _logger.LogDebug("album ownership: no tracklist for '{Artist} - {Album}': {M}", album.Artist, album.Title, ex.Message);
            return null;
        }
    }
}
