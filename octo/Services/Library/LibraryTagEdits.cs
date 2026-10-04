using System.Globalization;
using System.Text.Json;
using Octo.Services.Common;

namespace Octo.Services.Library;

/// <summary>The tags an app can read and change on one library file, by the names the apps send.</summary>
public static class SongTagFields
{
    public const string Title = "title";
    public const string Artist = "artist";
    public const string Album = "album";
    public const string AlbumArtist = "albumArtist";
    public const string Year = "year";
    public const string Genre = "genre";
    public const string Track = "track";
    public const string Disc = "disc";
    public const string Isrc = "isrc";

    public static readonly string[] All = [Title, Artist, Album, AlbumArtist, Year, Genre, Track, Disc, Isrc];

    /// <summary>Never cleared: without them a song is filed under its file name and an unknown artist.</summary>
    public static readonly string[] Required = [Title, Artist, Album];

    /// <summary>Several values in one field are sent and read joined by this.</summary>
    public const string Separator = "; ";

    public static string? Canonical(string name) =>
        All.FirstOrDefault(field => field.Equals(name, StringComparison.OrdinalIgnoreCase));
}

/// <summary>What one edit did to one file, for the answer and for its undo.</summary>
public sealed record TagEditResult(bool Changed, string? Error,
    IReadOnlyDictionary<string, string?> Before, IReadOnlyDictionary<string, string?> After)
{
    public static TagEditResult Failed(string error) => new(false, error, Empty, Empty);
    private static readonly IReadOnlyDictionary<string, string?> Empty = new Dictionary<string, string?>();
}

/// <summary>
/// Changes the tags of one library file in place, for the apps' Library health. In place on
/// purpose: Navidrome keeps a song's id, and so its plays, favorites and playlist places, for a
/// file that stays at the same path whatever its tags say. Only TagLib touches the file; nothing
/// here moves, renames or deletes one.
/// </summary>
public static class LibraryTagEdits
{
    private const int MaxLength = 500;

    /// <summary>The fields as the file holds them; a missing one is null.</summary>
    public static Dictionary<string, string?> Read(string path)
    {
        using var file = TagLib.File.Create(path);
        return Read(file);
    }

    internal static Dictionary<string, string?> Read(TagLib.File file)
    {
        var tag = file.Tag;
        // As written, spaces and all: a stray space is what splits an album, so it has to show.
        static string? Text(string? value) => string.IsNullOrWhiteSpace(value) ? null : value;
        static string? Joined(string[]? values) =>
            values is { Length: > 0 } ? Text(string.Join(SongTagFields.Separator, values.Where(v => !string.IsNullOrWhiteSpace(v)))) : null;
        static string? Number(uint value) => value > 0 ? value.ToString(CultureInfo.InvariantCulture) : null;
        return new Dictionary<string, string?>
        {
            [SongTagFields.Title] = Text(tag.Title),
            [SongTagFields.Artist] = Joined(tag.Performers),
            [SongTagFields.Album] = Text(tag.Album),
            [SongTagFields.AlbumArtist] = Joined(tag.AlbumArtists),
            [SongTagFields.Year] = Number(tag.Year),
            [SongTagFields.Genre] = Joined(tag.Genres),
            [SongTagFields.Track] = Number(tag.Track),
            [SongTagFields.Disc] = Number(tag.Disc),
            [SongTagFields.Isrc] = Text(TagWriterExtras.ReadText(file, TagFields.Isrc) ?? tag.ISRC),
        };
    }

    /// <summary>
    /// Why these changes cannot be written, or null when they can. Names are the apps' field
    /// names; an empty value clears the field, except for the ones a song cannot do without.
    /// </summary>
    public static string? Invalid(IReadOnlyDictionary<string, string?> changes)
    {
        if (changes.Count == 0) return "No tags were sent to change.";
        foreach (var (name, raw) in changes)
        {
            if (SongTagFields.Canonical(name) is not { } field) return $"\"{name}\" is not a tag this server changes.";
            var value = raw?.Trim() ?? "";
            if (value.Length > MaxLength) return $"The {Words(field)} is too long.";
            if (value.Any(char.IsControl)) return $"The {Words(field)} has characters a tag cannot hold.";
            if (value.Length == 0)
            {
                if (SongTagFields.Required.Contains(field)) return $"A song needs its {Words(field)}, so it cannot be left empty.";
                continue;
            }
            switch (field)
            {
                case SongTagFields.Year when !int.TryParse(value, NumberStyles.None, CultureInfo.InvariantCulture, out var year) || year is < 1000 or > 2999:
                    return $"\"{value}\" is not a year.";
                case SongTagFields.Track or SongTagFields.Disc when !int.TryParse(value, NumberStyles.None, CultureInfo.InvariantCulture, out var number) || number is < 1 or > 999:
                    return $"\"{value}\" is not a {Words(field)}.";
                case SongTagFields.Isrc when SongIdentity.NormalizeIsrc(value) is null:
                    return $"\"{value}\" is not an ISRC.";
            }
        }
        return null;
    }

    /// <summary>Writes the changes that differ from what the file holds. Check them with
    /// <see cref="Invalid"/> first.</summary>
    public static TagEditResult Write(string path, IReadOnlyDictionary<string, string?> changes)
    {
        try
        {
            using var file = TagLib.File.Create(path);
            var before = Read(file);
            var changed = new Dictionary<string, string?>();
            foreach (var (name, raw) in changes)
            {
                var field = SongTagFields.Canonical(name)!;
                var value = string.IsNullOrWhiteSpace(raw) ? null : raw.Trim();
                if (field == SongTagFields.Isrc && value is not null) value = SongIdentity.NormalizeIsrc(value);
                if (string.Equals(before[field], value, StringComparison.Ordinal)) continue;
                Set(file, field, value);
                changed[field] = value;
            }
            if (changed.Count == 0) return new(false, null, before, before);
            file.Save();
            var after = new Dictionary<string, string?>(before);
            foreach (var (field, value) in changed) after[field] = value;
            return new(true, null, before, after);
        }
        catch (Exception ex)
        {
            return TagEditResult.Failed(Why(ex));
        }
    }

    private static void Set(TagLib.File file, string field, string? value)
    {
        var tag = file.Tag;
        static string[] Many(string? value) => value is null ? [] :
            value.Split(';', StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries);
        static uint Number(string? value) => value is null ? 0 : uint.Parse(value, CultureInfo.InvariantCulture);
        switch (field)
        {
            case SongTagFields.Title: tag.Title = value; break;
            // One name as written: a credit like "Simon & Garfunkel" is one artist, not two.
            case SongTagFields.Artist: tag.Performers = value is null ? [] : [value]; break;
            case SongTagFields.Album: tag.Album = value; break;
            case SongTagFields.AlbumArtist: tag.AlbumArtists = value is null ? [] : [value]; break;
            case SongTagFields.Year: tag.Year = Number(value); break;
            case SongTagFields.Genre: tag.Genres = Many(value); break;
            case SongTagFields.Track: tag.Track = Number(value); break;
            case SongTagFields.Disc: tag.Disc = Number(value); break;
            case SongTagFields.Isrc: TagWriterExtras.SetText(file, TagFields.Isrc, value); break;
        }
    }

    /// <summary>
    /// Gives the target file the album of the lead file: the title, album artists, version,
    /// release date, MusicBrainz album id, compilation flag and year, which are what Navidrome
    /// tells albums apart by. The target keeps its own title, track and disc.
    /// <paramref name="leadAlbumArtist"/> is the album artist Navidrome shows for the lead, used
    /// only when its file names none, so the target hashes to the same album.
    /// </summary>
    public static TagEditResult JoinAlbum(string targetPath, string leadPath, string? leadAlbumArtist, string? targetAlbumArtist)
    {
        if (KeptIdentityTags.Read(leadPath, leadAlbumArtist) is not { } lead) return TagEditResult.Failed("could not read the album's tags");
        if (KeptIdentityTags.Read(targetPath, targetAlbumArtist) is not { } mine) return TagEditResult.Failed("could not read the song's tags");
        uint leadYear;
        try
        {
            using var leadFile = TagLib.File.Create(leadPath);
            leadYear = leadFile.Tag.Year;
        }
        catch (Exception ex)
        {
            return TagEditResult.Failed(Why(ex));
        }

        var joined = mine with
        {
            Album = lead.Album,
            AlbumArtist = lead.AlbumArtist,
            AlbumArtists = lead.AlbumArtists,
            AlbumVersion = lead.AlbumVersion,
            ReleaseDate = lead.ReleaseDate,
            AlbumId = lead.AlbumId,
            Compilation = lead.Compilation,
            TrackCount = lead.TrackCount > 0 ? lead.TrackCount : mine.TrackCount,
            DiscCount = lead.DiscCount > 0 ? lead.DiscCount : mine.DiscCount,
        };
        try
        {
            var before = Read(targetPath);
            var sameAlbum = KeptIdentityTags.PidInputs(joined) == KeptIdentityTags.PidInputs(mine);
            var sameYear = leadYear == 0 || before[SongTagFields.Year] == leadYear.ToString(CultureInfo.InvariantCulture);
            if (sameAlbum && sameYear) return new(false, null, before, before);
            if (!sameAlbum) KeptIdentityTags.Apply(targetPath, joined);
            if (!sameYear)
            {
                using var file = TagLib.File.Create(targetPath);
                file.Tag.Year = leadYear;
                file.Save();
            }
            return new(true, null, before, Read(targetPath));
        }
        catch (Exception ex)
        {
            return TagEditResult.Failed(Why(ex));
        }
    }

    /// <summary>Puts back an album identity read before a <see cref="JoinAlbum"/>, and its year.</summary>
    public static TagEditResult RestoreAlbum(string path, KeptIdentity identity, string? year)
    {
        try
        {
            var before = Read(path);
            KeptIdentityTags.Apply(path, identity);
            using (var file = TagLib.File.Create(path))
            {
                file.Tag.Year = year is null ? 0 : uint.Parse(year, CultureInfo.InvariantCulture);
                file.Save();
            }
            return new(true, null, before, Read(path));
        }
        catch (Exception ex)
        {
            return TagEditResult.Failed(Why(ex));
        }
    }

    /// <summary>Whether the file carries a picture of its own.</summary>
    public static bool HasPicture(string path)
    {
        try
        {
            using var file = TagLib.File.Create(path);
            return file.Tag.Pictures is { Length: > 0 };
        }
        catch (Exception) { return false; }
    }

    /// <summary>Puts a front cover inside a file that has no picture. A file that has one is left
    /// alone: this fills a gap, it never replaces what someone chose.</summary>
    public static TagEditResult AddCover(string path, byte[] cover)
    {
        try
        {
            using var file = TagLib.File.Create(path);
            var before = Read(file);
            if (file.Tag.Pictures is { Length: > 0 }) return new(false, null, before, before);
            file.Tag.Pictures =
            [
                new TagLib.Picture
                {
                    Type = TagLib.PictureType.FrontCover,
                    MimeType = Octo.Services.CoverArt.CoverImage.MimeType(cover),
                    Description = "Cover",
                    Data = new TagLib.ByteVector(cover),
                },
            ];
            file.Save();
            return new(true, null, before, before);
        }
        catch (Exception ex)
        {
            return TagEditResult.Failed(Why(ex));
        }
    }

    /// <summary>Takes every picture out of a file, to undo <see cref="AddCover"/>.</summary>
    public static TagEditResult RemoveCover(string path)
    {
        try
        {
            using var file = TagLib.File.Create(path);
            var before = Read(file);
            if (file.Tag.Pictures is not { Length: > 0 }) return new(false, null, before, before);
            file.Tag.Pictures = [];
            file.Save();
            return new(true, null, before, before);
        }
        catch (Exception ex)
        {
            return TagEditResult.Failed(Why(ex));
        }
    }

    internal static string Words(string field) => field switch
    {
        SongTagFields.AlbumArtist => "album artist",
        SongTagFields.Track => "track number",
        SongTagFields.Disc => "disc number",
        SongTagFields.Isrc => "ISRC",
        _ => field,
    };

    private static string Why(Exception ex) => ex switch
    {
        TagLib.UnsupportedFormatException => "this kind of file cannot hold tags the server can write",
        TagLib.CorruptFileException => "the file looks damaged, so its tags were left alone",
        UnauthorizedAccessException => "the server is not allowed to write to this file",
        IOException io => $"the file could not be written ({io.Message})",
        _ => ex.Message,
    };
}

/// <summary>One edit the apps made to one file, kept so it can be undone and so a person can see
/// later which files were changed, by whom and how.</summary>
public sealed record TagEditEntry(
    string Id, string NavidromeId, string Path, string Kind, string Username,
    Dictionary<string, string?> Before, Dictionary<string, string?> After,
    long SizeAfter, long WriteTicksAfter, DateTime AtUtc)
{
    /// <summary>For an album join: the album as it was, so the undo can put it back exactly.</summary>
    public KeptIdentity? AlbumBefore { get; init; }

    public bool Undone { get; init; }
}

public static class TagEditKinds
{
    public const string Retag = "retag";
    public const string JoinAlbum = "joinAlbum";
    public const string Cover = "cover";
}

/// <summary>
/// The tag edits made from the apps, newest last, kept beside the settings. Bounded, rewritten
/// whole through a temporary file on every change (edits come one at a time from a person, so
/// that is cheap), and read back at start.
/// </summary>
public sealed class TagEditJournal
{
    private const int MaxEntries = 2000;
    private readonly string? _path;
    private readonly ILogger<TagEditJournal>? _logger;
    private readonly object _lock = new();
    private readonly List<TagEditEntry> _entries = [];

    public TagEditJournal(string? path = null, ILogger<TagEditJournal>? logger = null)
    {
        _path = string.IsNullOrWhiteSpace(path) ? null : path;
        _logger = logger;
        if (_path is null || !File.Exists(_path)) return;
        try
        {
            _entries.AddRange(JsonSerializer.Deserialize<List<TagEditEntry>>(File.ReadAllText(_path)) ?? []);
        }
        catch (Exception ex)
        {
            _logger?.LogWarning("tag edit journal could not be read: {M}", ex.Message);
        }
    }

    /// <summary>False when it could not be kept on disk.</summary>
    public bool Record(TagEditEntry entry)
    {
        lock (_lock)
        {
            _entries.Add(entry);
            if (_entries.Count > MaxEntries) _entries.RemoveRange(0, _entries.Count - MaxEntries);
            return Save();
        }
    }

    /// <summary>The newest edit of this song not undone yet.</summary>
    public TagEditEntry? Last(string navidromeId)
    {
        lock (_lock)
            return _entries.LastOrDefault(entry => entry.NavidromeId == navidromeId && !entry.Undone);
    }

    public IReadOnlyList<TagEditEntry> Recent(int limit = 200)
    {
        lock (_lock)
            return _entries.AsEnumerable().Reverse().Take(limit).ToList();
    }

    public bool MarkUndone(string entryId)
    {
        lock (_lock)
        {
            var at = _entries.FindIndex(entry => entry.Id == entryId);
            if (at < 0) return false;
            _entries[at] = _entries[at] with { Undone = true };
            return Save();
        }
    }

    private bool Save()
    {
        if (_path is null) return true;
        try
        {
            Directory.CreateDirectory(Path.GetDirectoryName(_path)!);
            var tmp = _path + ".tmp";
            File.WriteAllText(tmp, JsonSerializer.Serialize(_entries));
            File.Move(tmp, _path, overwrite: true);
            return true;
        }
        catch (Exception ex)
        {
            _logger?.LogWarning("tag edit journal could not be written: {M}", ex.Message);
            return false;
        }
    }
}
