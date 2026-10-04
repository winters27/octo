using System.Globalization;
using Octo.Services.Tagging;

namespace Octo.Services.Common;

/// <summary>One field as each container names it, in the names the common taggers write and
/// the library server reads. A null ID3 frame means the field lives in a TXXX frame with the
/// description.</summary>
internal sealed record TagField(string? Id3Frame, string? Id3Description, string Vorbis, string Mp4);

/// <summary>The fields Octo writes beyond what TagLib's generic tag exposes, named per container.</summary>
internal static class TagFields
{
    public static readonly TagField Isrc = new("TSRC", null, "ISRC", "ISRC");
    public static readonly TagField Label = new("TPUB", null, "LABEL", "LABEL");
    public static readonly TagField CatalogNumber = new(null, "CATALOGNUMBER", "CATALOGNUMBER", "CATALOGNUMBER");
    public static readonly TagField Barcode = new(null, "BARCODE", "BARCODE", "BARCODE");
    public static readonly TagField ReleaseType = new(null, "MusicBrainz Album Type", "RELEASETYPE", "MusicBrainz Album Type");
    public static readonly TagField ReleaseStatus = new(null, "MusicBrainz Album Status", "RELEASESTATUS", "MusicBrainz Album Status");
    public static readonly TagField ReleaseCountry = new(null, "MusicBrainz Album Release Country", "RELEASECOUNTRY", "MusicBrainz Album Release Country");
    public static readonly TagField ReleaseTrackId = new(null, "MusicBrainz Release Track Id", "MUSICBRAINZ_RELEASETRACKID", "MusicBrainz Release Track Id");
    public static readonly TagField AlbumArtistId = new(null, "MusicBrainz Album Artist Id", "MUSICBRAINZ_ALBUMARTISTID", "MusicBrainz Album Artist Id");
    public static readonly TagField ArtistId = new(null, "MusicBrainz Artist Id", "MUSICBRAINZ_ARTISTID", "MusicBrainz Artist Id");
    public static readonly TagField FingerprintId = new(null, "Acoustid Id", "ACOUSTID_ID", "Acoustid Id");
    public static readonly TagField TrackGain = new(null, "REPLAYGAIN_TRACK_GAIN", "REPLAYGAIN_TRACK_GAIN", "REPLAYGAIN_TRACK_GAIN");
    public static readonly TagField TrackPeak = new(null, "REPLAYGAIN_TRACK_PEAK", "REPLAYGAIN_TRACK_PEAK", "REPLAYGAIN_TRACK_PEAK");
    public static readonly TagField AlbumGain = new(null, "REPLAYGAIN_ALBUM_GAIN", "REPLAYGAIN_ALBUM_GAIN", "REPLAYGAIN_ALBUM_GAIN");
    public static readonly TagField AlbumPeak = new(null, "REPLAYGAIN_ALBUM_PEAK", "REPLAYGAIN_ALBUM_PEAK", "REPLAYGAIN_ALBUM_PEAK");
    public static readonly TagField AlbumId = new(null, "MusicBrainz Album Id", "MUSICBRAINZ_ALBUMID", "MusicBrainz Album Id");
    public static readonly TagField AlbumArtists = new(null, "ALBUMARTISTS", "ALBUMARTISTS", "ALBUMARTISTS");
    public static readonly TagField AlbumVersion = new(null, "ALBUMVERSION", "ALBUMVERSION", "ALBUMVERSION");
    public static readonly TagField ReleaseDate = new(null, "RELEASEDATE", "RELEASEDATE", "RELEASEDATE");
    public static readonly TagField Advisory = new(null, "ITUNESADVISORY", "ITUNESADVISORY", "ITUNESADVISORY");
}

/// <summary>
/// The tag frames TagLib's generic Tag does not expose, written in the frame each container's
/// readers look for. Navidrome's mappings.yaml maps TXXX:ARTISTS (ID3v2), ARTISTS (Vorbis) and
/// ----:com.apple.iTunes:ARTISTS (MP4) to its artists tag, and reads the recording id from
/// UFID:http://musicbrainz.org, MUSICBRAINZ_TRACKID and "MusicBrainz Track Id".
/// </summary>
internal static class TagWriterExtras
{
    /// <summary>Picard's and Navidrome's owner string for the recording id on ID3.</summary>
    internal const string MusicBrainzUfidOwner = "http://musicbrainz.org";

    private const string AppleMean = "com.apple.iTunes";

    /// <summary>
    /// The file's ID3v2 tag, created when it has none. A tag Octo creates is version 4, so the
    /// original date has its own frame; a tag the file arrived with keeps whatever version it
    /// has. Never a global switch: that would change every other writer in Octo unseen.
    /// </summary>
    public static TagLib.Id3v2.Tag? Id3For(TagLib.File file)
    {
        if (file.GetTag(TagLib.TagTypes.Id3v2, true) is not TagLib.Id3v2.Tag id3) return null;
        if (!file.TagTypesOnDisk.HasFlag(TagLib.TagTypes.Id3v2)) id3.Version = 4;
        return id3;
    }

    /// <summary>
    /// The container's own tag, created when the format has an obvious one. For anything else
    /// only a tag that already exists is used, so a WAV never grows a Vorbis comment it cannot hold.
    /// </summary>
    private static (TagLib.Id3v2.Tag? Id3, TagLib.Ogg.XiphComment? Xiph, TagLib.Mpeg4.AppleTag? Apple) NativeTags(
        TagLib.File file) => file switch
    {
        TagLib.Mpeg.AudioFile => (Id3For(file), null, null),
        TagLib.Mpeg4.File => (null, null, file.GetTag(TagLib.TagTypes.Apple, true) as TagLib.Mpeg4.AppleTag),
        TagLib.Flac.File or TagLib.Ogg.File =>
            (null, file.GetTag(TagLib.TagTypes.Xiph, true) as TagLib.Ogg.XiphComment, null),
        _ => (file.GetTag(TagLib.TagTypes.Id3v2, false) as TagLib.Id3v2.Tag,
            file.GetTag(TagLib.TagTypes.Xiph, false) as TagLib.Ogg.XiphComment,
            file.GetTag(TagLib.TagTypes.Apple, false) as TagLib.Mpeg4.AppleTag),
    };

    /// <summary>
    /// One field holding several values, each its own value rather than one joined string.
    /// TagLib writes a multi-value TXXX null-separated even in ID3v2.3, which is what keeps a
    /// credit of two artists from being read back as one artist named after both.
    /// </summary>
    public static void SetMultiValue(TagLib.File file, string field, IReadOnlyList<string> values)
    {
        var array = values.Where(value => !string.IsNullOrWhiteSpace(value)).Select(value => value.Trim()).ToArray();
        if (array.Length == 0) return;

        var (id3, xiph, apple) = NativeTags(file);
        if (id3 is not null) TagLib.Id3v2.UserTextInformationFrame.Get(id3, field, true).Text = array;
        xiph?.SetField(field, array);
        apple?.SetDashBoxes(AppleMean, field, array);
    }

    /// <summary>
    /// One value in one field, in the frame each container's readers look for. An empty value
    /// leaves the file as it is: a release fact that could not be found is not a reason to strip
    /// one a peer wrote.
    /// </summary>
    public static void SetText(TagLib.File file, TagField field, string? value)
    {
        var text = value?.Trim();
        if (string.IsNullOrEmpty(text)) return;

        var (id3, xiph, apple) = NativeTags(file);
        if (id3 is not null)
        {
            if (field.Id3Frame is { } frame) id3.SetTextFrame(frame, text);
            else if (field.Id3Description is { } description)
                TagLib.Id3v2.UserTextInformationFrame.Get(id3, description, true).Text = [text];
        }
        xiph?.SetField(field.Vorbis, text);
        apple?.SetDashBox(AppleMean, field.Mp4, text);
    }

    /// <summary>Several values in one field, by the same names as <see cref="SetText"/>.</summary>
    public static void SetMulti(TagLib.File file, TagField field, IReadOnlyList<string> values)
    {
        var array = values.Where(value => !string.IsNullOrWhiteSpace(value)).Select(value => value.Trim()).Distinct(StringComparer.Ordinal).ToArray();
        if (array.Length == 0) return;

        var (id3, xiph, apple) = NativeTags(file);
        if (id3 is not null)
        {
            if (field.Id3Frame is { } frame) id3.SetTextFrame(frame, array);
            else if (field.Id3Description is { } description)
                TagLib.Id3v2.UserTextInformationFrame.Get(id3, description, true).Text = array;
        }
        xiph?.SetField(field.Vorbis, array);
        apple?.SetDashBoxes(AppleMean, field.Mp4, array);
    }

    /// <summary>Values exactly as given, untrimmed, replacing the field; none removes it. For
    /// copying another file's values, where one changed character changes what they hash to.</summary>
    public static void SetExact(TagLib.File file, TagField field, IReadOnlyList<string> values)
    {
        var array = values.Where(value => !string.IsNullOrEmpty(value)).ToArray();
        var (id3, xiph, apple) = NativeTags(file);
        if (id3 is not null && field.Id3Description is { } description)
        {
            if (array.Length > 0) TagLib.Id3v2.UserTextInformationFrame.Get(id3, description, true).Text = array;
            else if (TagLib.Id3v2.UserTextInformationFrame.Get(id3, description, false) is { } frame) id3.RemoveFrame(frame);
        }
        if (xiph is not null) { if (array.Length > 0) xiph.SetField(field.Vorbis, array); else xiph.RemoveField(field.Vorbis); }
        // TagLib's SetDashBoxes reads the first value before anything else, so none goes through SetDashBox.
        if (array.Length > 0) apple?.SetDashBoxes(AppleMean, field.Mp4, array);
        else if (apple?.GetDashBox(AppleMean, field.Mp4) is not null) apple.SetDashBox(AppleMean, field.Mp4, null);
    }

    /// <summary>The original release date: a TDOR frame on a version 4 tag, TORY (the year) on
    /// version 3, ORIGINALDATE and ORIGINALYEAR on a Vorbis comment, originaldate on MP4.
    /// TagLib keeps every frame under its version 4 id and renders TDOR as TORY on a version 3
    /// tag, so the frame is set as TDOR either way, with the year alone where TORY holds no more.</summary>
    public static void SetOriginalDate(TagLib.File file, string? date)
    {
        var text = date?.Trim();
        if (string.IsNullOrEmpty(text)) return;
        var year = text.Length >= 4 ? text[..4] : text;

        var (id3, xiph, apple) = NativeTags(file);
        id3?.SetTextFrame("TDOR", id3.Version >= 4 ? text : year);
        if (xiph is not null)
        {
            xiph.SetField("ORIGINALDATE", text);
            xiph.SetField("ORIGINALYEAR", year);
        }
        apple?.SetDashBox(AppleMean, "originaldate", text);
    }

    /// <summary>The release track id, in the frame taggers and the library server read it from.</summary>
    public static void SetReleaseTrackId(TagLib.File file, string? id) => SetText(file, TagFields.ReleaseTrackId, id);

    /// <summary>ReplayGain as the players read it: the gain with two decimals and " dB", the peak
    /// with six, always with a dot, whatever the server's own culture.</summary>
    public static void SetReplayGain(TagLib.File file, double? trackGainDb, double? trackPeak, double? albumGainDb, double? albumPeak)
    {
        if (trackGainDb is { } tg) SetText(file, TagFields.TrackGain, GainText(tg));
        if (trackPeak is { } tp) SetText(file, TagFields.TrackPeak, PeakText(tp));
        if (albumGainDb is { } ag) SetText(file, TagFields.AlbumGain, GainText(ag));
        if (albumPeak is { } ap) SetText(file, TagFields.AlbumPeak, PeakText(ap));
    }

    /// <summary>The MP4 rating atom, which holds the advisory there.</summary>
    private static readonly TagLib.ByteVector Rating = TagLib.ByteVector.FromString("rtng", TagLib.StringType.Latin1);

    /// <summary>
    /// Whether the words are explicit (1), the clean edit (2) or neither (0), where the library
    /// server reads a song's explicit status from: ITUNESADVISORY on ID3 and Vorbis, the rtng atom
    /// on MP4, which holds it as a one-byte number. Anything else leaves the file as it is.
    /// </summary>
    public static void SetAdvisory(TagLib.File file, int? value)
    {
        if (value is not (0 or 1 or 2)) return;
        var text = value.Value.ToString(CultureInfo.InvariantCulture);
        var (id3, xiph, apple) = NativeTags(file);
        if (id3 is not null) TagLib.Id3v2.UserTextInformationFrame.Get(id3, TagFields.Advisory.Id3Description!, true).Text = [text];
        xiph?.SetField(TagFields.Advisory.Vorbis, text);
        if (apple is not null)
        {
            apple.SetData(Rating, new TagLib.ByteVector(new[] { (byte)value.Value }), (uint)TagLib.Mpeg4.AppleDataBox.FlagType.ForTempo);
            // A freeform copy beside the atom would be a second value.
            if (apple.GetDashBox(AppleMean, TagFields.Advisory.Mp4) is not null) apple.SetDashBox(AppleMean, TagFields.Advisory.Mp4, null);
        }
    }

    /// <summary>The advisory a file carries, from whichever frame holds it; null when none does.</summary>
    public static int? ReadAdvisory(TagLib.File file)
    {
        if (file.GetTag(TagLib.TagTypes.Apple, false) is TagLib.Mpeg4.AppleTag apple
            && apple.DataBoxes(Rating).FirstOrDefault()?.Data is { Count: > 0 } data)
            return data[data.Count - 1];
        return int.TryParse(ReadText(file, TagFields.Advisory), NumberStyles.Integer, CultureInfo.InvariantCulture, out var value)
            ? value : null;
    }

    internal static string GainText(double gainDb) => gainDb.ToString("+0.00;-0.00", CultureInfo.InvariantCulture) + " dB";
    internal static string PeakText(double peak) => peak.ToString("0.000000", CultureInfo.InvariantCulture);

    /// <summary>One value as the file holds it, from whichever container's frame has it.</summary>
    public static string? ReadText(TagLib.File file, TagField field)
    {
        var id3 = file.GetTag(TagLib.TagTypes.Id3v2, false) as TagLib.Id3v2.Tag;
        var xiph = file.GetTag(TagLib.TagTypes.Xiph, false) as TagLib.Ogg.XiphComment;
        var apple = file.GetTag(TagLib.TagTypes.Apple, false) as TagLib.Mpeg4.AppleTag;

        string? fromId3 = null;
        if (id3 is not null)
        {
            if (field.Id3Frame is { } frame)
                fromId3 = id3.GetFrames<TagLib.Id3v2.TextInformationFrame>(frame).FirstOrDefault()?.Text?.FirstOrDefault();
            else if (field.Id3Description is { } description)
                fromId3 = TagLib.Id3v2.UserTextInformationFrame.Get(id3, description, false)?.Text?.FirstOrDefault();
        }
        var value = fromId3 ?? xiph?.GetFirstField(field.Vorbis) ?? apple?.GetDashBox(AppleMean, field.Mp4);
        return string.IsNullOrWhiteSpace(value) ? null : value.Trim();
    }

    /// <summary>
    /// The RECORDING id, in the frame Picard and Navidrome both read it from. Written frame by
    /// frame rather than through Tag.MusicBrainzTrackId: TagLib# after 2.3.0 repurposes that
    /// property for the release TRACK id on ID3 (UFID owner "MusicBrainz Release Track Id"),
    /// which Navidrome would not read as a recording, and a package bump must not change what
    /// lands on disk.
    /// </summary>
    public static void SetRecordingId(TagLib.File file, string recordingId)
    {
        if (string.IsNullOrWhiteSpace(recordingId)) return;

        var (id3, xiph, apple) = NativeTags(file);
        if (id3 is not null)
            TagLib.Id3v2.UniqueFileIdentifierFrame.Get(id3, MusicBrainzUfidOwner, true).Identifier =
                TagLib.ByteVector.FromString(recordingId, TagLib.StringType.UTF8);
        xiph?.SetField("MUSICBRAINZ_TRACKID", recordingId);
        apple?.SetDashBox(AppleMean, "MusicBrainz Track Id", recordingId);
    }

    /// <summary>The recording id a file already carries, from whichever frame holds it.</summary>
    public static string? ReadRecordingId(TagLib.File file)
    {
        var (id3, xiph, apple) = NativeTags(file);
        var fromId3 = id3 is null ? null
            : TagLib.Id3v2.UniqueFileIdentifierFrame.Get(id3, MusicBrainzUfidOwner, false)?.Identifier?.ToString();
        var value = fromId3
            ?? xiph?.GetFirstField("MUSICBRAINZ_TRACKID")
            ?? apple?.GetDashBox(AppleMean, "MusicBrainz Track Id");
        return string.IsNullOrWhiteSpace(value) ? null : value.Trim();
    }

    public static void SetCompilation(TagLib.File file, bool value)
    {
        var (id3, xiph, apple) = NativeTags(file);
        if (id3 is not null) id3.IsCompilation = value;
        if (xiph is not null) xiph.IsCompilation = value;
        if (apple is not null) apple.IsCompilation = value;
    }

    public static bool IsCompilation(TagLib.File file) =>
        (file.GetTag(TagLib.TagTypes.Id3v2, false) as TagLib.Id3v2.Tag)?.IsCompilation == true
        || (file.GetTag(TagLib.TagTypes.Xiph, false) as TagLib.Ogg.XiphComment)?.IsCompilation == true
        || (file.GetTag(TagLib.TagTypes.Apple, false) as TagLib.Mpeg4.AppleTag)?.IsCompilation == true;

    /// <summary>The album a file already names, for a download whose source named none.</summary>
    public static (string? Album, string? AlbumArtist, bool IsCompilation) ReadAlbum(string path)
    {
        try
        {
            using var file = TagLib.File.Create(path);
            return (file.Tag.Album, file.Tag.FirstAlbumArtist, IsCompilation(file));
        }
        catch
        {
            return (null, null, false);
        }
    }

    /// <summary>The recording id and length of a file on disk, for deciding whether two files
    /// are the same recording. Nulls when the file cannot be read.</summary>
    public static (string? RecordingId, int Seconds) ReadIdentity(string path)
    {
        try
        {
            using var file = TagLib.File.Create(path);
            return (ReadRecordingId(file), (int)Math.Round(file.Properties.Duration.TotalSeconds));
        }
        catch
        {
            return (null, 0);
        }
    }

    /// <summary>
    /// What a file already says about itself, for the chooser: its length and format, and the
    /// tags it arrived with. With <paramref name="tagsAreEvidence"/> off only the length and
    /// format are read, since an uploader's name is not a credit.
    /// </summary>
    public static FileFacts ReadFacts(string path, bool tagsAreEvidence)
    {
        var extension = Path.GetExtension(path);
        try
        {
            using var file = TagLib.File.Create(path);
            var seconds = (int)Math.Round(file.Properties.Duration.TotalSeconds);
            var rate = file.Properties.AudioSampleRate;
            if (!tagsAreEvidence)
                return new FileFacts(seconds, extension, rate, null, null, null, null, null, null, null, [], null, null, null,
                    null, null, false, false);

            var tag = file.Tag;
            var isrcs = new List<string?> { tag.ISRC, ReadText(file, TagFields.Isrc) };
            if (file.GetTag(TagLib.TagTypes.Id3v2, false) is TagLib.Id3v2.Tag id3
                && TagLib.Id3v2.UserTextInformationFrame.Get(id3, "ISRC", false) is { } frame)
                isrcs.AddRange(frame.Text);
            var codes = isrcs.Where(value => !string.IsNullOrWhiteSpace(value))
                .SelectMany(value => value!.Split([';', ',', '/', '\0'], StringSplitOptions.RemoveEmptyEntries))
                .Select(SongIdentity.NormalizeIsrc).OfType<string>().Distinct(StringComparer.Ordinal).ToList();

            return new FileFacts(seconds, extension, rate,
                Blank(tag.Title), Blank(tag.FirstPerformer), Blank(tag.Album), Blank(tag.FirstAlbumArtist),
                tag.Year > 0 ? (int)tag.Year : null, tag.Track > 0 ? (int)tag.Track : null, tag.Disc > 0 ? (int)tag.Disc : null,
                codes, ReadText(file, TagFields.Barcode), ReadText(file, TagFields.CatalogNumber),
                Blank(tag.Publisher) ?? ReadText(file, TagFields.Label), ReadRecordingId(file), Blank(tag.MusicBrainzReleaseId),
                IsCompilation(file), true);
        }
        catch
        {
            return FileFacts.Unknown(extension);
        }
    }

    private static string? Blank(string? value) => string.IsNullOrWhiteSpace(value) ? null : value.Trim();
}
