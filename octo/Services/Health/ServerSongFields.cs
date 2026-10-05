using Octo.Services.Library;

namespace Octo.Services.Health;

/// <summary>
/// Library health's view of Navidrome's own song list, read the way the Octo app reads the same
/// songs through Subsonic, so both find the same things: the length in whole seconds (Subsonic
/// cuts the fraction off), an m4a lossless only with a bit depth, and the release a part came out
/// on taken for its whole album (Navidrome keeps one per album, the value most of its songs carry).
/// </summary>
public sealed class ServerSongFields : IHealthFields<LibrarySongRow>
{
    private static readonly HashSet<string> LosslessFormats = new(StringComparer.Ordinal)
    {
        "flac", "alac", "wav", "wave", "aiff", "aif", "aifc", "pcm", "ape", "wv", "dsf", "dff", "tta",
    };

    private sealed record AlbumFacts(string? Release, string? Group, string? Barcode, IReadOnlyList<string> Labels);

    private readonly Dictionary<string, AlbumFacts> _albums = new(StringComparer.Ordinal);

    public ServerSongFields(IEnumerable<LibrarySongRow> rows)
    {
        foreach (var album in rows.Where(row => !string.IsNullOrEmpty(row.AlbumId)).GroupBy(row => row.AlbumId!, StringComparer.Ordinal))
        {
            var labels = new List<string>();
            foreach (var label in album.SelectMany(row => row.Labels))
                if (!labels.Contains(label, StringComparer.Ordinal)) labels.Add(label);
            _albums[album.Key] = new AlbumFacts(MostOf(album, row => row.ReleaseId), MostOf(album, row => row.ReleaseGroupId),
                MostOf(album, row => row.Barcode), labels);
        }
    }

    /// <summary>The value most of an album's songs carry; between equals, the first in plain order.</summary>
    private static string? MostOf(IEnumerable<LibrarySongRow> songs, Func<LibrarySongRow, string?> of) =>
        songs.Select(of).Where(value => !string.IsNullOrWhiteSpace(value)).Select(value => value!)
            .GroupBy(value => value, StringComparer.Ordinal)
            .OrderByDescending(group => group.Count()).ThenBy(group => group.Key, StringComparer.Ordinal)
            .Select(group => group.Key).FirstOrDefault();

    private static string? Text(string? value) => string.IsNullOrWhiteSpace(value) ? null : value;

    public static bool IsLossless(string? format, int? bitDepth)
    {
        var kind = format?.ToLowerInvariant();
        if (kind is null) return false;
        if (kind is "m4a" or "mp4") return (bitDepth ?? 0) > 0;
        return LosslessFormats.Contains(kind);
    }

    public string Id(LibrarySongRow song) => song.Id;
    public string Title(LibrarySongRow song) => song.Title;
    public string? Artist(LibrarySongRow song) => Text(song.Artist);
    public string? Album(LibrarySongRow song) => song.Album;
    public string? AlbumId(LibrarySongRow song) => song.AlbumId;
    public string? AlbumArtist(LibrarySongRow song) => Text(song.AlbumArtist) ?? song.AlbumArtists.FirstOrDefault();
    public IReadOnlyList<string> Genres(LibrarySongRow song) => song.Genres;
    public int? Year(LibrarySongRow song) => song.Year;
    public int? Disc(LibrarySongRow song) => song.Disc;
    public int? Track(LibrarySongRow song) => song.Track;
    public int Seconds(LibrarySongRow song) => (int)Math.Floor(song.Seconds ?? song.Duration ?? 0);
    public string? Format(LibrarySongRow song) => Text(song.Suffix)?.ToLowerInvariant();
    public bool Lossless(LibrarySongRow song) => IsLossless(song.Suffix, song.BitDepth);
    public int? BitRate(LibrarySongRow song) => song.BitRate;
    public int? BitDepth(LibrarySongRow song) => song.BitDepth;
    public int? SampleRate(LibrarySongRow song) => song.SampleRate;
    public IReadOnlyList<string> Isrcs(LibrarySongRow song) => song.Isrcs ?? [];
    public string? RecordingId(LibrarySongRow song) => song.RecordingId;

    /// <summary>A picture inside the file. Navidrome's Subsonic answer always names a cover (the
    /// album's, when the file has none), so the apps cannot tell; the native list can.</summary>
    public string? Cover(LibrarySongRow song) => song.HasCover ? "embedded" : null;

    public IReadOnlyList<string> Artists(LibrarySongRow song)
    {
        var named = song.Artists.Where(name => !string.IsNullOrWhiteSpace(name)).ToList();
        return named.Count > 0 ? named : Artist(song) is { } one ? [one] : [];
    }

    public IReadOnlyList<string> AlbumArtists(LibrarySongRow song)
    {
        var named = song.AlbumArtists.Where(name => !string.IsNullOrWhiteSpace(name)).ToList();
        return named.Count > 0 ? named : AlbumArtist(song) is { } one ? [one] : [];
    }

    public string? ReleaseId(LibrarySongRow song) => FactsOf(song)?.Release;
    public string? ReleaseGroupId(LibrarySongRow song) => FactsOf(song)?.Group;
    public string? Barcode(LibrarySongRow song) => FactsOf(song)?.Barcode;
    public IReadOnlyList<string> Labels(LibrarySongRow song) => FactsOf(song)?.Labels ?? [];

    private AlbumFacts? FactsOf(LibrarySongRow song) =>
        song.AlbumId is { Length: > 0 } id ? _albums.GetValueOrDefault(id) : null;
}
