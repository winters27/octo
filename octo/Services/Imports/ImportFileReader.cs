using System.IO.Compression;
using System.Text;
using System.Text.Json;
using Octo.Services.Common;

namespace Octo.Services.Imports;

/// <summary>A list a file held: its name and songs.</summary>
public sealed record FileList(string Name, IReadOnlyList<ImportTrack> Tracks);

/// <summary>
/// Lists from files other tools write, for when there is no Spotify sign-in, or a playlist the
/// sign-in cannot read in full:
///
/// - CSV from Exportify, TuneMyMusic, Soundiiz and the like. Columns are found by their header,
///   so any CSV with a title and an artist column works; album, length and ISRC are read when
///   there.
/// - Spotify's own "Download your data" export (Account, Privacy): YourLibrary.json holds the
///   liked songs and Playlist1.json (and on) every playlist, whole. The zip Spotify sends works
///   as it is.
/// </summary>
public static class ImportFileReader
{
    /// <summary>Bigger than any real export, small enough that a wrong file cannot fill memory.</summary>
    public const long MaxBytes = 64L * 1024 * 1024;

    public static IReadOnlyList<FileList> Read(string fileName, Stream content)
    {
        var name = Path.GetFileName(fileName ?? "");
        var extension = Path.GetExtension(name).ToLowerInvariant();
        if (extension == ".zip") return ReadZip(content);
        using var reader = new StreamReader(content, Encoding.UTF8, detectEncodingFromByteOrderMarks: true);
        var text = reader.ReadToEnd();
        if (extension == ".json" || text.TrimStart().StartsWith('{') || text.TrimStart().StartsWith('['))
            return ReadJson(Path.GetFileNameWithoutExtension(name), text);
        return ReadCsv(Path.GetFileNameWithoutExtension(name), text);
    }

    private static IReadOnlyList<FileList> ReadZip(Stream content)
    {
        var lists = new List<FileList>();
        using var zip = new ZipArchive(content, ZipArchiveMode.Read);
        foreach (var entry in zip.Entries.OrderBy(entry => entry.FullName, StringComparer.Ordinal))
        {
            var file = Path.GetFileName(entry.FullName);
            var wanted = file.Equals("YourLibrary.json", StringComparison.OrdinalIgnoreCase)
                || (file.StartsWith("Playlist", StringComparison.OrdinalIgnoreCase) && file.EndsWith(".json", StringComparison.OrdinalIgnoreCase))
                || file.EndsWith(".csv", StringComparison.OrdinalIgnoreCase);
            if (!wanted || entry.Length > MaxBytes) continue;
            using var stream = entry.Open();
            lists.AddRange(Read(file, stream));
        }
        return lists;
    }

    // ---- Spotify's data export ----------------------------------------------------------------

    internal static IReadOnlyList<FileList> ReadJson(string name, string text)
    {
        using var doc = JsonDocument.Parse(text);
        var root = doc.RootElement;
        var lists = new List<FileList>();
        if (root.ValueKind != JsonValueKind.Object) return lists;

        // YourLibrary.json: { "tracks": [ { "artist", "album", "track", "uri" } ], ... }
        if (root.TryGetProperty("tracks", out var liked) && liked.ValueKind == JsonValueKind.Array)
        {
            var tracks = liked.EnumerateArray()
                .Select(row => Track(Str(row, "track"), Str(row, "artist"), Str(row, "album"), null, null, Str(row, "uri")))
                .OfType<ImportTrack>().ToList();
            if (tracks.Count > 0) lists.Add(new FileList("Liked Songs", tracks));
        }

        // Playlist1.json: { "playlists": [ { "name", "items": [ { "track": { "trackName", "artistName", "albumName", "trackUri" } } ] } ] }
        if (root.TryGetProperty("playlists", out var playlists) && playlists.ValueKind == JsonValueKind.Array)
            foreach (var playlist in playlists.EnumerateArray())
            {
                var tracks = new List<ImportTrack>();
                if (playlist.TryGetProperty("items", out var items) && items.ValueKind == JsonValueKind.Array)
                    foreach (var item in items.EnumerateArray())
                    {
                        if (!item.TryGetProperty("track", out var t) || t.ValueKind != JsonValueKind.Object) continue;
                        if (Track(Str(t, "trackName"), Str(t, "artistName"), Str(t, "albumName"), null, null, Str(t, "trackUri")) is { } track)
                            tracks.Add(track);
                    }
                if (tracks.Count > 0) lists.Add(new FileList(Str(playlist, "name") ?? name, tracks));
            }
        return lists;
    }

    // ---- CSV ----------------------------------------------------------------------------------

    private static readonly string[] TitleColumns = ["track name", "title", "track", "song", "song name", "name", "track title"];
    private static readonly string[] ArtistColumns = ["artist name(s)", "artist names", "artist name", "artists", "artist", "artist(s)"];
    private static readonly string[] AlbumColumns = ["album name", "album", "album title", "release"];
    private static readonly string[] LengthColumns = ["track duration (ms)", "duration (ms)", "duration_ms", "duration ms", "duration", "length", "time"];
    private static readonly string[] IsrcColumns = ["isrc"];
    private static readonly string[] SpotifyColumns = ["track uri", "spotify - id", "spotify uri", "spotify id", "uri", "spotify track id"];
    private static readonly string[] PlaylistColumns = ["playlist name", "playlist"];

    internal static IReadOnlyList<FileList> ReadCsv(string name, string text)
    {
        var rows = Rows(text).ToList();
        if (rows.Count == 0) return [];
        var header = rows[0].Select(cell => cell.Trim().ToLowerInvariant()).ToList();
        int Column(string[] names) => names.Select(n => header.IndexOf(n)).FirstOrDefault(i => i >= 0, -1);
        var title = Column(TitleColumns);
        var artist = Column(ArtistColumns);
        if (title < 0 || artist < 0)
            throw new FormatException("The file needs a title column and an artist column, named in its first row.");
        var album = Column(AlbumColumns);
        var length = Column(LengthColumns);
        var isrc = Column(IsrcColumns);
        var spotify = Column(SpotifyColumns);
        var playlist = Column(PlaylistColumns);
        var lengthInMs = length >= 0 && header[length].Contains("ms");

        string? Cell(List<string> row, int index) => index >= 0 && index < row.Count && row[index].Trim().Length > 0 ? row[index].Trim() : null;

        // One file can hold several playlists (TuneMyMusic, Soundiiz): a playlist column splits them.
        var byList = new Dictionary<string, List<ImportTrack>>(StringComparer.Ordinal);
        var order = new List<string>();
        foreach (var row in rows.Skip(1))
        {
            var track = Track(Cell(row, title), Cell(row, artist), Cell(row, album),
                Seconds(Cell(row, length), lengthInMs), Cell(row, isrc), Cell(row, spotify));
            if (track is null) continue;
            var list = Cell(row, playlist) ?? name;
            if (!byList.TryGetValue(list, out var tracks))
            {
                byList[list] = tracks = [];
                order.Add(list);
            }
            tracks.Add(track);
        }
        return order.Select(list => new FileList(list, byList[list])).ToList();
    }

    /// <summary>RFC 4180 rows: quoted cells may hold commas, quotes ("") and line breaks. A semicolon file is read too.</summary>
    internal static IEnumerable<List<string>> Rows(string text)
    {
        var firstLine = text.Split('\n', 2)[0];
        var separator = firstLine.Count(c => c == ';') > firstLine.Count(c => c == ',') ? ';' : ',';
        var row = new List<string>();
        var cell = new StringBuilder();
        var quoted = false;
        for (var i = 0; i < text.Length; i++)
        {
            var c = text[i];
            if (quoted)
            {
                if (c == '"' && i + 1 < text.Length && text[i + 1] == '"') { cell.Append('"'); i++; }
                else if (c == '"') quoted = false;
                else cell.Append(c);
                continue;
            }
            if (c == '"') quoted = true;
            else if (c == separator) { row.Add(cell.ToString()); cell.Clear(); }
            else if (c == '\n' || c == '\r')
            {
                if (c == '\r' && i + 1 < text.Length && text[i + 1] == '\n') i++;
                row.Add(cell.ToString());
                cell.Clear();
                if (row.Any(value => value.Length > 0)) yield return row;
                row = [];
            }
            else cell.Append(c);
        }
        row.Add(cell.ToString());
        if (row.Any(value => value.Length > 0)) yield return row;
    }

    /// <summary>A length in milliseconds, seconds, or m:ss.</summary>
    internal static int? Seconds(string? value, bool milliseconds)
    {
        if (string.IsNullOrWhiteSpace(value)) return null;
        if (value.Contains(':'))
        {
            var parts = value.Split(':').Select(part => int.TryParse(part, out var n) ? n : -1).ToList();
            if (parts.Any(n => n < 0)) return null;
            return parts.Aggregate(0, (total, part) => total * 60 + part);
        }
        if (!double.TryParse(value, System.Globalization.NumberStyles.Float, System.Globalization.CultureInfo.InvariantCulture, out var number) || number <= 0)
            return null;
        // A bare number above an hour is surely milliseconds, whatever the column says.
        return (int)Math.Round(milliseconds || number > 3600 ? number / 1000 : number);
    }

    private static ImportTrack? Track(string? title, string? artist, string? album, int? seconds, string? isrc, string? spotify)
    {
        if (string.IsNullOrWhiteSpace(title) || string.IsNullOrWhiteSpace(artist)) return null;
        // Exportify joins artists with commas, Spotify's export gives one, and some tools use semicolons.
        var credit = string.Join(", ", artist.Split(';', StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries));
        return new ImportTrack
        {
            Key = ImportKeys.ForSpotify(spotify) ?? ImportKeys.ForWords(credit, title),
            Title = title.Trim(), Artist = credit, Album = string.IsNullOrWhiteSpace(album) ? null : album.Trim(),
            Seconds = seconds, Isrc = SongIdentity.NormalizeIsrc(isrc),
        };
    }

    private static string? Str(JsonElement e, string name) =>
        e.ValueKind == JsonValueKind.Object && e.TryGetProperty(name, out var v) && v.ValueKind == JsonValueKind.String ? v.GetString() : null;
}
