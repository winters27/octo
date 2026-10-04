using Octo.Services.Common;

namespace Octo.Services.Tagging;

/// <summary>The values an album already in the library groups by, as the library server reads
/// them: its release id, its release date and its version. Copied exactly onto a track that joins it.</summary>
public sealed record AlbumGrouping(string? AlbumId, string? ReleaseDate, string? AlbumVersion);

/// <summary>
/// The release facts a file arrived with, and which of them may stay. A peer tags a file for the
/// release it ripped; when Octo files the song under another album, or keeps none of that
/// release's grouping values, what the peer wrote would describe the wrong album, and the library
/// server reads it all the same. Names are lowercase and matched against ID3 TXXX descriptions,
/// Vorbis field names and MP4 freeform names alike, covering every alias the library server reads
/// for the field.
/// </summary>
internal static class ReleaseFactTags
{
    /// <summary>
    /// What the library server groups an album by before its name: the release id, then the
    /// album's version and release date. Octo writes none of them for a new download (one track
    /// carrying them beside another without them splits the album), so a peer's never stay. A
    /// Vorbis YEAR is a release date to the library server too; DATE holds the year.
    /// </summary>
    internal static readonly string[] Grouping =
    [
        "musicbrainz album id", "musicbrainz_albumid",
        "releasedate", "year",
        "albumversion", "musicbrainz_albumcomment", "musicbrainz album comment",
    ];

    private static readonly string[] GroupingFrames = ["TDRL"];

    /// <summary>The rest of what one release is. They go when the song is filed under another album.</summary>
    internal static readonly string[] OtherRelease =
    [
        "musicbrainz release group id", "musicbrainz_releasegroupid",
        "musicbrainz release track id", "musicbrainz_releasetrackid",
        "musicbrainz album artist id", "musicbrainz_albumartistid",
        "musicbrainz disc id", "musicbrainz_discid",
        "musicbrainz album type", "musicbrainz_albumtype", "releasetype",
        "musicbrainz album status", "musicbrainz_albumstatus", "releasestatus",
        "musicbrainz album release country", "musicbrainz_albumreleasecountry", "releasecountry",
        "barcode", "upc", "ean",
        "catalognumber",
        "label", "organization", "publisher",
        "originaldate", "originalyear",
        "albumartists", "album artists",
        "asin", "media", "discsubtitle", "setsubtitle",
        "tracktotal", "totaltracks", "disctotal", "totaldiscs",
        "replaygain_album_gain", "replaygain_album_peak",
    ];

    private static readonly string[] OtherReleaseFrames = ["TPUB", "TDOR", "TORY", "TSST", "TMED"];

    /// <summary>
    /// Every name a field goes by, for the fields Octo writes under one of them. Once Octo has
    /// written a value, the other names are a second value the library server reads beside it:
    /// "G59 Records" in LABEL and "G*59 Records" in ORGANIZATION are two labels.
    /// </summary>
    internal static readonly (TagField Field, string[] Names)[] Aliases =
    [
        (TagFields.Barcode, ["barcode", "upc", "ean"]),
        (TagFields.Label, ["label", "organization", "publisher"]),
        (TagFields.ReleaseType, ["releasetype", "musicbrainz_albumtype", "musicbrainz album type"]),
        (TagFields.ReleaseStatus, ["releasestatus", "musicbrainz_albumstatus", "musicbrainz album status"]),
        (TagFields.ReleaseCountry, ["releasecountry", "musicbrainz_albumreleasecountry", "musicbrainz album release country"]),
    ];

    private const string AppleMean = "com.apple.iTunes";

    /// <summary>
    /// Whether the album the song is written under is another album than the one the file arrived
    /// with. Only the title counts: a credit written another way ("JAY-Z &amp; Kanye West" for
    /// "JAY-Z") is still the release the peer described. A file that named no album was filed
    /// nowhere, so nothing it carries is about another album.
    /// </summary>
    public static bool FiledElsewhere(string? arrivedAlbum, string? album) =>
        !string.IsNullOrWhiteSpace(arrivedAlbum) && !string.IsNullOrWhiteSpace(album)
        && SongIdentity.Key(arrivedAlbum) != SongIdentity.Key(album);

    /// <summary>
    /// Take off what the file arrived with that does not describe the album it is written under,
    /// before Octo writes its own values. The grouping values always go, unless the song joins an
    /// album that carries them, which they are then copied from; when the song is filed elsewhere
    /// the rest of the other release goes too, track and disc numbers with it. Returns the names
    /// that went, for the report.
    /// </summary>
    public static IReadOnlyList<string> Tidy(TagLib.File file, bool filedElsewhere, AlbumGrouping? joins)
    {
        var removed = new List<string>();
        KeepTheYear(file);
        removed.AddRange(Remove(file, Grouping, GroupingFrames));
        if (filedElsewhere)
        {
            removed.AddRange(Remove(file, OtherRelease, OtherReleaseFrames));
            var tag = file.Tag;
            if (tag.Track > 0 || tag.TrackCount > 0 || tag.Disc > 0 || tag.DiscCount > 0) removed.Add("track and disc numbers");
            tag.Track = 0;
            tag.TrackCount = 0;
            tag.Disc = 0;
            tag.DiscCount = 0;
        }
        if (joins is not null)
        {
            TagWriterExtras.SetText(file, TagFields.AlbumId, joins.AlbumId);
            TagWriterExtras.SetText(file, TagFields.ReleaseDate, joins.ReleaseDate);
            TagWriterExtras.SetText(file, TagFields.AlbumVersion, joins.AlbumVersion);
        }
        return removed.Distinct(StringComparer.OrdinalIgnoreCase).ToList();
    }

    /// <summary>After Octo wrote a field, its other names go, so the library server reads one value.</summary>
    public static void DropOtherNames(TagLib.File file, TagField field)
    {
        foreach (var (aliased, names) in Aliases)
            if (aliased == field) RemoveExcept(file, names, field);
    }

    /// <summary>
    /// Totals under the second name a Vorbis comment can give them. TagLib writes TRACKTOTAL and
    /// DISCTOTAL, so once those hold Octo's totals a peer's TOTALTRACKS or TOTALDISCS is a second value.
    /// </summary>
    public static void DropOtherTotals(TagLib.File file)
    {
        if (file.GetTag(TagLib.TagTypes.Xiph, false) is not TagLib.Ogg.XiphComment xiph) return;
        if (!string.IsNullOrEmpty(xiph.GetFirstField("TRACKTOTAL"))) xiph.RemoveField("TOTALTRACKS");
        if (!string.IsNullOrEmpty(xiph.GetFirstField("DISCTOTAL"))) xiph.RemoveField("TOTALDISCS");
    }

    /// <summary>A Vorbis comment whose only year is in YEAR keeps it as DATE, the recording date,
    /// before YEAR goes as a release date.</summary>
    private static void KeepTheYear(TagLib.File file)
    {
        if (file.GetTag(TagLib.TagTypes.Xiph, false) is not TagLib.Ogg.XiphComment xiph) return;
        if (!string.IsNullOrWhiteSpace(xiph.GetFirstField("DATE"))) return;
        if (xiph.GetFirstField("YEAR") is { Length: > 0 } year && !string.IsNullOrWhiteSpace(year)) xiph.SetField("DATE", year.Trim());
    }

    private static List<string> Remove(TagLib.File file, IReadOnlyCollection<string> names, IReadOnlyCollection<string> frames)
    {
        var wanted = new HashSet<string>(names, StringComparer.OrdinalIgnoreCase);
        var removed = new List<string>();

        if (file.GetTag(TagLib.TagTypes.Id3v2, false) is TagLib.Id3v2.Tag id3)
        {
            foreach (var frame in id3.GetFrames<TagLib.Id3v2.UserTextInformationFrame>()
                         .Where(f => f.Description is { } d && wanted.Contains(d)).ToList())
            {
                removed.Add(frame.Description);
                id3.RemoveFrame(frame);
            }
            foreach (var id in frames)
                if (id3.GetFrames(TagLib.ByteVector.FromString(id, TagLib.StringType.Latin1)).Any())
                {
                    removed.Add(id);
                    id3.RemoveFrames(TagLib.ByteVector.FromString(id, TagLib.StringType.Latin1));
                }
        }
        if (file.GetTag(TagLib.TagTypes.Xiph, false) is TagLib.Ogg.XiphComment xiph)
            foreach (var name in xiph.Where(wanted.Contains).ToList())
            {
                removed.Add(name);
                xiph.RemoveField(name);
            }
        if (file.GetTag(TagLib.TagTypes.Apple, false) is TagLib.Mpeg4.AppleTag apple)
            foreach (var name in FreeformNames(apple).Where(wanted.Contains).ToList())
            {
                removed.Add(name);
                apple.SetDashBox(AppleMean, name, null);
            }
        return removed;
    }

    /// <summary>Every name in <paramref name="names"/> goes from each container but the one Octo
    /// writes the field under there. On ID3 a field with a frame of its own keeps no TXXX at all.</summary>
    private static void RemoveExcept(TagLib.File file, IReadOnlyCollection<string> names, TagField keep)
    {
        if (file.GetTag(TagLib.TagTypes.Id3v2, false) is TagLib.Id3v2.Tag id3)
            foreach (var frame in id3.GetFrames<TagLib.Id3v2.UserTextInformationFrame>()
                         .Where(f => f.Description is { } d && names.Contains(d, StringComparer.OrdinalIgnoreCase)
                             && (keep.Id3Frame is not null || !string.Equals(d, keep.Id3Description, StringComparison.OrdinalIgnoreCase)))
                         .ToList())
                id3.RemoveFrame(frame);
        if (file.GetTag(TagLib.TagTypes.Xiph, false) is TagLib.Ogg.XiphComment xiph)
            foreach (var name in xiph.Where(n => names.Contains(n, StringComparer.OrdinalIgnoreCase)
                         && !string.Equals(n, keep.Vorbis, StringComparison.OrdinalIgnoreCase)).ToList())
                xiph.RemoveField(name);
        if (file.GetTag(TagLib.TagTypes.Apple, false) is TagLib.Mpeg4.AppleTag apple)
            foreach (var name in FreeformNames(apple).Where(n => names.Contains(n, StringComparer.OrdinalIgnoreCase)
                         && !string.Equals(n, keep.Mp4, StringComparison.OrdinalIgnoreCase)).ToList())
                apple.SetDashBox(AppleMean, name, null);
    }

    /// <summary>The names of the iTunes freeform atoms a file carries.</summary>
    private static IEnumerable<string> FreeformNames(TagLib.Mpeg4.AppleTag apple) =>
        apple.Where(box => box.BoxType == "----")
            .Select(box =>
            {
                string? Part(string type) => box.Children.OfType<TagLib.Mpeg4.AppleAdditionalInfoBox>()
                    .FirstOrDefault(child => child.BoxType == type)?.Text;
                return Part("mean") == AppleMean ? Part("name") : null;
            })
            .OfType<string>()
            .Where(name => name.Length > 0)
            .Distinct(StringComparer.Ordinal)
            .ToList();
}
