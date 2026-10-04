using Octo.Services.Common;
using Octo.Services.Library;

namespace Octo.Services.Imports;

/// <summary>
/// Whether the library already has each imported song, the way Octo decides it everywhere else:
/// one MatchKey (the primary artist and the title, one version, so a live take or a remix is its
/// own song) and a length within LibraryOwnership's 8 seconds when both lengths are known. A song
/// the key misses gets a second look through SongIdentity.Same on its title alone, which forgives
/// a credit written another way ("A, B" against "B feat. A", an accent, a renamed artist).
///
/// The whole library is read once, through Navidrome's own song list, and kept a few minutes, so a
/// list of thousands is matched in one pass rather than a search per song.
/// </summary>
public sealed class ImportMatcher
{
    internal static readonly TimeSpan KeepIndexFor = TimeSpan.FromMinutes(10);
    private const int PageSize = 1000;

    private readonly ILogger<ImportMatcher> _logger;
    private readonly SemaphoreSlim _gate = new(1, 1);
    private LibraryIndex? _index;
    private DateTime _indexedAt;

    public ImportMatcher(NavidromePlaylistApi navidrome, ILogger<ImportMatcher> logger)
    {
        _logger = logger;
        ReadPage = (start, count, ct) => navidrome.ListSongsAsync(start, count, ct);
    }

    /// <summary>One page of the library; null when Navidrome could not be read. Tests answer from a list.</summary>
    internal Func<int, int, CancellationToken, Task<(IReadOnlyList<LibrarySongRow> Rows, int Count)?>> ReadPage { get; set; }
    internal Func<DateTime> Clock { get; set; } = () => DateTime.UtcNow;

    /// <summary>Why the last match could not read the library, in words, or null when it could.</summary>
    public string? Problem { get; private set; }

    internal const string CannotReadLibrary =
        "Octo could not read your Navidrome library, so it cannot tell which songs you have. Octo needs a Navidrome admin sign-in, set under Music server.";

    /// <summary>Read the library again on the next match: something was just added to it.</summary>
    public void Forget() => _index = null;

    /// <summary>
    /// Sets each track's LibraryId to the library's song for it, or null. False when the library
    /// could not be read, in which case the tracks keep what an earlier match found.
    /// </summary>
    public async Task<bool> MatchAsync(IReadOnlyList<ImportTrack> tracks, CancellationToken ct)
    {
        if (await IndexAsync(ct) is not { } index) return false;
        foreach (var track in tracks) track.LibraryId = index.Find(track)?.Id;
        return true;
    }

    private async Task<LibraryIndex?> IndexAsync(CancellationToken ct)
    {
        await _gate.WaitAsync(ct);
        try
        {
            if (_index is { } fresh && Clock() - _indexedAt < KeepIndexFor) return fresh;
            var rows = new List<LibrarySongRow>();
            for (var start = 0; ; start += PageSize)
            {
                if (await ReadPage(start, PageSize, ct) is not var (page, count))
                {
                    _logger.LogWarning("Could not read the library to match imported songs");
                    Problem = CannotReadLibrary;
                    return null;
                }
                rows.AddRange(page);
                if (count < PageSize) break;
            }
            _index = new LibraryIndex(rows);
            _indexedAt = Clock();
            Problem = null;
            _logger.LogDebug("Read {Count} library songs to match imported songs against", rows.Count);
            return _index;
        }
        finally { _gate.Release(); }
    }
}

/// <summary>The library by MatchKey and by title, for matching many songs at once.</summary>
public sealed class LibraryIndex
{
    private static readonly SongMatchOptions Lenient = new() { LengthToleranceSeconds = LibraryOwnership.DurationToleranceSeconds };

    private readonly Dictionary<string, List<LibrarySongRow>> _byKey = new(StringComparer.Ordinal);
    private readonly Dictionary<string, List<LibrarySongRow>> _byTitle = new(StringComparer.Ordinal);

    public LibraryIndex(IEnumerable<LibrarySongRow> rows)
    {
        foreach (var row in rows)
        {
            if (string.IsNullOrWhiteSpace(row.Title)) continue;
            Add(_byKey, SongIdentity.MatchKey(row.Artist, row.Title), row);
            Add(_byTitle, SongIdentity.ParseTitle(row.Title, row.Artist).Key, row);
        }
    }

    public int Count => _byKey.Values.Sum(rows => rows.Count);

    private static void Add(Dictionary<string, List<LibrarySongRow>> map, string key, LibrarySongRow row)
    {
        if (key.Length == 0) return;
        if (!map.TryGetValue(key, out var rows)) map[key] = rows = [];
        rows.Add(row);
    }

    /// <summary>The library's best copy of this song, or null.</summary>
    public LibrarySongRow? Find(ImportTrack track)
    {
        if (_byKey.TryGetValue(SongIdentity.MatchKey(track.Artist, track.Title), out var same)
            && Best(same.Where(row => LengthFits(track.Seconds, row.Duration)), track) is { } exact)
            return exact;

        var title = SongIdentity.ParseTitle(track.Title, track.Artist).Key;
        if (title.Length == 0 || !_byTitle.TryGetValue(title, out var titled)) return null;
        var wanted = new SongRef(track.Title, track.Artist, track.Seconds);
        return Best(titled.Where(row => SongIdentity.Same(wanted, new SongRef(row.Title, row.Artist, row.Duration), Lenient).IsSame), track);
    }

    private static bool LengthFits(int? wanted, int? found) =>
        wanted is not > 0 || found is not > 0 || Math.Abs(wanted.Value - found.Value) <= LibraryOwnership.DurationToleranceSeconds;

    /// <summary>The closest in length, then the same album, then lossless, then the highest bitrate.</summary>
    private static LibrarySongRow? Best(IEnumerable<LibrarySongRow> rows, ImportTrack track) =>
        rows.OrderBy(row => track.Seconds is > 0 && row.Duration is > 0 ? Math.Abs(track.Seconds.Value - row.Duration.Value) : 0)
            .ThenByDescending(row => track.Album is { } album && SongIdentity.Key(row.Album) == SongIdentity.Key(album))
            .ThenByDescending(row => DuplicateScanWorker.IsLosslessFile(row.Suffix, row.BitRate))
            .ThenByDescending(row => row.BitRate)
            .FirstOrDefault();
}
