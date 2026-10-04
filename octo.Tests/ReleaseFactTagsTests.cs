using System.Diagnostics;
using Octo.Models.Domain;
using Octo.Services.Common;
using Octo.Services.Library;
using Octo.Services.Tagging;

namespace Octo.Tests;

/// <summary>
/// A peer's file carries the facts of the release it was ripped from. Once Octo files the song,
/// only the facts of the album it is written under may stay, each under one name, and the values
/// the library server groups an album by never come from a peer. Checked on real MP3, FLAC and
/// MP4 files, read back the way the library server reads them.
/// </summary>
public sealed class ReleaseFactTagsTests : IDisposable
{
    private const string PeerAlbumId = "0b1c2d3e-4f50-4617-8899-aabbccddeeff";
    private const string PeerTrackId = "11112222-3333-4444-8555-666677778888";

    private readonly string _dir = Path.Combine(Path.GetTempPath(), "octo-facts-" + Guid.NewGuid().ToString("N"));

    public ReleaseFactTagsTests() => Directory.CreateDirectory(_dir);

    public void Dispose()
    {
        try { Directory.Delete(_dir, recursive: true); } catch { /* best effort */ }
    }

    private string Write(string extension, byte[] bytes)
    {
        var path = Path.Combine(_dir, Guid.NewGuid().ToString("N") + extension);
        File.WriteAllBytes(path, bytes);
        return path;
    }

    private string Mp4()
    {
        var path = Path.Combine(_dir, Guid.NewGuid().ToString("N") + ".m4a");
        using var process = Process.Start(new ProcessStartInfo("ffmpeg",
            $"-y -nostdin -hide_banner -v error -f lavfi -i sine=frequency=440:duration=1 -c:a aac -b:a 128k \"{path}\"")
        {
            RedirectStandardError = true, UseShellExecute = false, CreateNoWindow = true,
        })!;
        var error = process.StandardError.ReadToEnd();
        process.WaitForExit();
        Assert.True(process.ExitCode == 0, error);
        return path;
    }

    /// <summary>A FLAC as a peer tagged it for "The Party Never Ends 2.0", track 8 of 16.</summary>
    private string PeerFlac(bool yearOnly = false)
    {
        var path = Write(".flac", AudioFixtures.Flac());
        using var file = TagLib.File.Create(path);
        var xiph = (TagLib.Ogg.XiphComment)file.GetTag(TagLib.TagTypes.Xiph, true);
        xiph.SetField("TITLE", "Love Letter");
        xiph.SetField("ARTIST", "Juice WRLD");
        xiph.SetField("ALBUM", "The Party Never Ends 2.0");
        xiph.SetField("ALBUMARTIST", "Juice WRLD");
        if (yearOnly) xiph.SetField("YEAR", "2024");
        else
        {
            xiph.SetField("DATE", "2024-11-29");
            xiph.SetField("RELEASEDATE", "2024-11-29");
        }
        xiph.SetField("MUSICBRAINZ_ALBUMID", PeerAlbumId);
        xiph.SetField("MUSICBRAINZ_RELEASETRACKID", PeerTrackId);
        xiph.SetField("MUSICBRAINZ_RELEASEGROUPID", "group-tpne");
        xiph.SetField("UPC", "0602475682233");
        xiph.SetField("LABEL", "Grade A Productions - Interscope Records");
        xiph.SetField("ORGANIZATION", "Interscope");
        xiph.SetField("CATALOGNUMBER", "B0042");
        xiph.SetField("RELEASETYPE", "album");
        xiph.SetField("ORIGINALDATE", "2024-11-29");
        xiph.SetField("TRACKNUMBER", "8");
        xiph.SetField("TRACKTOTAL", "16");
        xiph.SetField("DISCNUMBER", "1");
        xiph.SetField("REPLAYGAIN_ALBUM_GAIN", "-8.10 dB");
        xiph.SetField("REPLAYGAIN_TRACK_GAIN", "-7.20 dB");
        xiph.SetField("ISRC", "USUG12407311");
        xiph.SetField("COPYRIGHT", "2024 Grade A Productions, LLC");
        file.Save();
        return path;
    }

    private static Dictionary<string, List<string>> Navidrome(string path)
    {
        using var file = TagLib.File.Create(path);
        return KeptIdentityTags.NavidromeView(file);
    }

    private static string? Vorbis(string path, string field)
    {
        using var file = TagLib.File.Create(path);
        return (file.GetTag(TagLib.TagTypes.Xiph, false) as TagLib.Ogg.XiphComment)?.GetFirstField(field);
    }

    private static List<string> Tidy(string path, bool filedElsewhere, AlbumGrouping? joins = null)
    {
        using var file = TagLib.File.Create(path);
        var removed = ReleaseFactTags.Tidy(file, filedElsewhere, joins).ToList();
        file.Save();
        return removed;
    }

    // ---- which album the file is filed under ---------------------------------------------

    [Theory]
    [InlineData("The Party Never Ends 2.0", "Love Letter", true)]
    [InlineData("I Want to Die In New Orleans", "I Want to Die in New Orleans", false)]
    [InlineData("Mezzanine", "Mezzanine (Deluxe)", true)]
    [InlineData("good kid, m.A.A.d city", "good kid m.A.A.d city", false)]
    [InlineData(null, "Love Letter", false)]
    [InlineData("  ", "Love Letter", false)]
    [InlineData("Love Letter", "", false)]
    public void FiledElsewhere_IsAnotherAlbumTitle(string? arrived, string? album, bool expected) =>
        Assert.Equal(expected, ReleaseFactTags.FiledElsewhere(arrived, album));

    // ---- the grouping values never stay ----------------------------------------------------

    [Fact]
    public void SameAlbum_LosesOnlyTheGroupingValues()
    {
        var path = PeerFlac();

        var removed = Tidy(path, filedElsewhere: false);

        Assert.Null(Vorbis(path, "MUSICBRAINZ_ALBUMID"));
        Assert.Null(Vorbis(path, "RELEASEDATE"));
        Assert.Contains("MUSICBRAINZ_ALBUMID", removed, StringComparer.OrdinalIgnoreCase);
        // The rest describes this very album and stays.
        Assert.Equal("0602475682233", Vorbis(path, "UPC"));
        Assert.Equal("B0042", Vorbis(path, "CATALOGNUMBER"));
        Assert.Equal(PeerTrackId, Vorbis(path, "MUSICBRAINZ_RELEASETRACKID"));
        Assert.Equal("8", Vorbis(path, "TRACKNUMBER"));
        Assert.Equal("2024-11-29", Vorbis(path, "DATE"));
        // What the library server groups the album by is now the name and the album artist alone.
        var view = Navidrome(path);
        Assert.Null(KeptIdentityTags.Read(path)!.AlbumId);
        Assert.Null(KeptIdentityTags.Read(path)!.ReleaseDate);
        Assert.False(view.ContainsKey("musicbrainz_albumid"));
    }

    [Fact]
    public void AYearOnlyInYear_StaysAsTheDate()
    {
        var path = PeerFlac(yearOnly: true);

        Tidy(path, filedElsewhere: false);

        Assert.Null(Vorbis(path, "YEAR"));
        Assert.Equal("2024", Vorbis(path, "DATE"));
        using var file = TagLib.File.Create(path);
        Assert.Equal(2024u, file.Tag.Year);
    }

    [Fact]
    public void JoiningAnAlbum_CopiesItsGroupingValuesExactly()
    {
        var path = PeerFlac();
        const string theirs = "99999999-8888-4777-8666-555555555555";

        Tidy(path, filedElsewhere: false, new AlbumGrouping(theirs, "2024-11-29", null));

        var identity = KeptIdentityTags.Read(path)!;
        Assert.Equal((theirs, "2024-11-29", (string?)null), (identity.AlbumId, identity.ReleaseDate, identity.AlbumVersion));
    }

    // ---- filed under another album: that album's facts go ----------------------------------

    [Fact]
    public void FiledElsewhere_Flac_KeepsOnlyWhatIsAboutTheRecording()
    {
        var path = PeerFlac();

        var removed = Tidy(path, filedElsewhere: true);

        foreach (var field in new[]
                 {
                     "MUSICBRAINZ_ALBUMID", "RELEASEDATE", "MUSICBRAINZ_RELEASETRACKID", "MUSICBRAINZ_RELEASEGROUPID", "UPC",
                     "LABEL", "ORGANIZATION", "CATALOGNUMBER", "RELEASETYPE", "ORIGINALDATE", "TRACKNUMBER", "TRACKTOTAL",
                     "DISCNUMBER", "REPLAYGAIN_ALBUM_GAIN",
                 })
            Assert.True(Vorbis(path, field) is null, $"{field} should have gone");
        // The recording's own facts stay: its code, its gain, its words and its year.
        Assert.Equal("USUG12407311", Vorbis(path, "ISRC"));
        Assert.Equal("-7.20 dB", Vorbis(path, "REPLAYGAIN_TRACK_GAIN"));
        Assert.Equal("2024 Grade A Productions, LLC", Vorbis(path, "COPYRIGHT"));
        Assert.Equal("Love Letter", Vorbis(path, "TITLE"));
        Assert.Equal("2024-11-29", Vorbis(path, "DATE"));
        Assert.Contains("track and disc numbers", removed);
    }

    [Theory]
    [InlineData(3)]
    [InlineData(4)]
    public void FiledElsewhere_Mp3_LosesTheOtherReleasesFrames(int version)
    {
        var path = Write(".mp3", AudioFixtures.Mp3());
        using (var file = TagLib.File.Create(path))
        {
            var id3 = (TagLib.Id3v2.Tag)file.GetTag(TagLib.TagTypes.Id3v2, true);
            id3.Version = (byte)version;
            file.Tag.Title = "Love Letter";
            file.Tag.Album = "The Party Never Ends 2.0";
            file.Tag.Track = 8;
            file.Tag.TrackCount = 16;
            id3.SetTextFrame("TPUB", "Interscope");
            id3.SetTextFrame("TDRL", "2024-11-29");
            id3.SetTextFrame("TDOR", "2024-11-29");
            id3.SetTextFrame("TSRC", "USUG12407311");
            TagLib.Id3v2.UserTextInformationFrame.Get(id3, "MusicBrainz Album Id", true).Text = [PeerAlbumId];
            TagLib.Id3v2.UserTextInformationFrame.Get(id3, "MusicBrainz Release Track Id", true).Text = [PeerTrackId];
            TagLib.Id3v2.UserTextInformationFrame.Get(id3, "BARCODE", true).Text = ["0602475682233"];
            TagLib.Id3v2.UserTextInformationFrame.Get(id3, "CATALOGNUMBER", true).Text = ["B0042"];
            file.Save();
        }

        Tidy(path, filedElsewhere: true);

        using var after = TagLib.File.Create(path);
        var tag = (TagLib.Id3v2.Tag)after.GetTag(TagLib.TagTypes.Id3v2, false);
        foreach (var frame in new[] { "TPUB", "TDRL", "TDOR", "TORY", "TRCK" })
            Assert.Empty(tag.GetFrames(frame));
        foreach (var description in new[] { "MusicBrainz Album Id", "MusicBrainz Release Track Id", "BARCODE", "CATALOGNUMBER" })
            Assert.Null(TagLib.Id3v2.UserTextInformationFrame.Get(tag, description, false));
        Assert.Equal("USUG12407311", tag.GetFrames<TagLib.Id3v2.TextInformationFrame>("TSRC").Single().Text.Single());
    }

    [FfmpegFact]
    public void FiledElsewhere_Mp4_LosesTheOtherReleasesFreeformAtoms()
    {
        var path = Mp4();
        using (var file = TagLib.File.Create(path))
        {
            var apple = (TagLib.Mpeg4.AppleTag)file.GetTag(TagLib.TagTypes.Apple, true);
            file.Tag.Album = "The Party Never Ends 2.0";
            apple.SetDashBox("com.apple.iTunes", "MusicBrainz Album Id", PeerAlbumId);
            apple.SetDashBox("com.apple.iTunes", "BARCODE", "0602475682233");
            apple.SetDashBox("com.apple.iTunes", "LABEL", "Interscope");
            apple.SetDashBox("com.apple.iTunes", "ISRC", "USUG12407311");
            file.Save();
        }

        Tidy(path, filedElsewhere: true);

        using var after = TagLib.File.Create(path);
        var tag = (TagLib.Mpeg4.AppleTag)after.GetTag(TagLib.TagTypes.Apple, false);
        Assert.Null(tag.GetDashBox("com.apple.iTunes", "MusicBrainz Album Id"));
        Assert.Null(tag.GetDashBox("com.apple.iTunes", "BARCODE"));
        Assert.Null(tag.GetDashBox("com.apple.iTunes", "LABEL"));
        Assert.Equal("USUG12407311", tag.GetDashBox("com.apple.iTunes", "ISRC"));
    }

    // ---- one value per field ---------------------------------------------------------------

    [Fact]
    public void ALabelOctoWrote_IsTheOnlyLabel()
    {
        var path = PeerFlac();
        using (var file = TagLib.File.Create(path))
        {
            TagWriterExtras.SetText(file, TagFields.Label, "G59 Records");
            TagWriterExtras.SetText(file, TagFields.Barcode, "195497822577");
            ReleaseFactTags.DropOtherNames(file, TagFields.Label);
            ReleaseFactTags.DropOtherNames(file, TagFields.Barcode);
            file.Save();
        }

        var view = Navidrome(path);
        Assert.Equal(["G59 Records"], view["label"]);
        Assert.False(view.ContainsKey("organization"));
        Assert.Equal(["195497822577"], view["barcode"]);
        Assert.False(view.ContainsKey("upc"));
    }

    [Fact]
    public void ALabelOctoWroteOnId3_LeavesNoTxxxLabelBesideIt()
    {
        var path = Write(".mp3", AudioFixtures.Mp3());
        using (var file = TagLib.File.Create(path))
        {
            var id3 = TagWriterExtras.Id3For(file)!;
            TagLib.Id3v2.UserTextInformationFrame.Get(id3, "LABEL", true).Text = ["G*59 Records"];
            TagLib.Id3v2.UserTextInformationFrame.Get(id3, "RELEASETYPE", true).Text = ["compilation"];
            TagWriterExtras.SetText(file, TagFields.Label, "G59 Records");
            TagWriterExtras.SetMulti(file, TagFields.ReleaseType, ["album"]);
            ReleaseFactTags.DropOtherNames(file, TagFields.Label);
            ReleaseFactTags.DropOtherNames(file, TagFields.ReleaseType);
            file.Save();
        }

        using var after = TagLib.File.Create(path);
        var tag = (TagLib.Id3v2.Tag)after.GetTag(TagLib.TagTypes.Id3v2, false);
        Assert.Equal("G59 Records", tag.GetFrames<TagLib.Id3v2.TextInformationFrame>("TPUB").Single().Text.Single());
        Assert.Null(TagLib.Id3v2.UserTextInformationFrame.Get(tag, "LABEL", false));
        Assert.Null(TagLib.Id3v2.UserTextInformationFrame.Get(tag, "RELEASETYPE", false));
        Assert.Equal("album", TagLib.Id3v2.UserTextInformationFrame.Get(tag, "MusicBrainz Album Type", false)!.Text.Single());
    }

    [Fact]
    public void TotalsUnderTheirSecondName_GoOnceOctoWroteThem()
    {
        var path = PeerFlac();
        using (var file = TagLib.File.Create(path))
        {
            var xiph = (TagLib.Ogg.XiphComment)file.GetTag(TagLib.TagTypes.Xiph, false);
            xiph.SetField("TOTALTRACKS", "40");
            xiph.SetField("TOTALDISCS", "2");
            file.Tag.TrackCount = 11;
            file.Tag.DiscCount = 1;
            ReleaseFactTags.DropOtherTotals(file);
            file.Save();
        }

        Assert.Equal("11", Vorbis(path, "TRACKTOTAL"));
        Assert.Equal("1", Vorbis(path, "DISCTOTAL"));
        Assert.Null(Vorbis(path, "TOTALTRACKS"));
        Assert.Null(Vorbis(path, "TOTALDISCS"));
    }

    // ---- the advisory ----------------------------------------------------------------------

    [Theory]
    [InlineData(".mp3", 1)]
    [InlineData(".flac", 2)]
    [InlineData(".mp3", 0)]
    public void Advisory_LandsWhereTheLibraryServerReadsIt(string extension, int value)
    {
        var path = Write(extension, extension == ".mp3" ? AudioFixtures.Mp3() : AudioFixtures.Flac());
        using (var file = TagLib.File.Create(path))
        {
            TagWriterExtras.SetAdvisory(file, value);
            file.Save();
        }

        using var after = TagLib.File.Create(path);
        Assert.Equal(value, TagWriterExtras.ReadAdvisory(after));
        Assert.Equal([value.ToString()], KeptIdentityTags.NavidromeView(after)["itunesadvisory"]);
    }

    [Fact]
    public void Advisory_OutOfRangeOrUnknown_LeavesTheFileAlone()
    {
        var path = Write(".flac", AudioFixtures.Flac());
        using (var file = TagLib.File.Create(path))
        {
            TagWriterExtras.SetAdvisory(file, 1);
            TagWriterExtras.SetAdvisory(file, null);
            TagWriterExtras.SetAdvisory(file, 7);
            file.Save();
        }
        Assert.Equal("1", Vorbis(path, "ITUNESADVISORY"));
    }

    [FfmpegFact]
    public void Advisory_OnMp4_IsTheOneByteRatingAtom()
    {
        var path = Mp4();
        using (var file = TagLib.File.Create(path))
        {
            ((TagLib.Mpeg4.AppleTag)file.GetTag(TagLib.TagTypes.Apple, true)).SetDashBox("com.apple.iTunes", "ITUNESADVISORY", "2");
            TagWriterExtras.SetAdvisory(file, 1);
            file.Save();
        }

        using var after = TagLib.File.Create(path);
        var apple = (TagLib.Mpeg4.AppleTag)after.GetTag(TagLib.TagTypes.Apple, false);
        var box = apple.DataBoxes(TagLib.ByteVector.FromString("rtng", TagLib.StringType.Latin1)).Single();
        Assert.Equal(new byte[] { 1 }, box.Data.Data);
        Assert.Equal((uint)TagLib.Mpeg4.AppleDataBox.FlagType.ForTempo, box.Flags);
        Assert.Null(apple.GetDashBox("com.apple.iTunes", "ITUNESADVISORY"));
        Assert.Equal(1, TagWriterExtras.ReadAdvisory(after));
    }

    // ---- which version landed --------------------------------------------------------------

    [Theory]
    [InlineData(1, null, ExplicitAdvisory.Explicit)]
    [InlineData(3, false, ExplicitAdvisory.Clean)]
    [InlineData(0, false, ExplicitAdvisory.None)]
    [InlineData(2, true, ExplicitAdvisory.Explicit)]
    [InlineData(6, false, null)]
    [InlineData(null, null, null)]
    public void FromCatalog_MapsTheCatalogsNumbers(int? content, bool? explicitLyrics, int? expected) =>
        Assert.Equal(expected, ExplicitAdvisory.FromCatalog(content, explicitLyrics));

    private static ReleaseCandidate Hit(string isrc, int content) =>
        new(TagSource.Catalog, "Money Trees", "Kendrick Lamar") { Isrcs = [isrc], ExplicitContent = content, ReleaseTitle = "good kid, m.A.A.d city" };

    private static TagPlan Plan(IReadOnlyList<string> fileIsrcs, TagConfidence confidence, params ReleaseCandidate[] hits)
    {
        var request = new TagRequest("Kendrick Lamar", "Money Trees", null, null, null, 386, null, null, null, new HashSet<string>());
        var file = FileFacts.Unknown(".flac") with { Isrcs = fileIsrcs, TagsAreEvidence = fileIsrcs.Count > 0 };
        var ranked = hits.Select(hit => new ScoredCandidate(hit, 0.1, [])).ToList();
        return new TagPlan
        {
            Confidence = confidence, Ranked = ranked, Chosen = ranked.FirstOrDefault(),
            Evidence = new TagEvidence(request, file, 0.8, new HashSet<string>()),
        };
    }

    [Fact]
    public void TheHitWithTheLandedCode_Decides_EvenWhenItIsNotTheFirst()
    {
        var plan = Plan(["USUM71210787"], TagConfidence.Medium, Hit("USUM71210782", 1), Hit("USUM71210787", 3));

        var decision = ExplicitAdvisory.Decide(new Song { Title = "Money Trees", Isrc = "USUM71210782" }, plan);

        Assert.Equal(ExplicitAdvisory.Clean, decision!.Value);
    }

    [Fact]
    public void NoCodeToCompare_OnlyAStrongCatalogMatchDecides()
    {
        var song = new Song { Title = "Money Trees" };
        Assert.Null(ExplicitAdvisory.Decide(song, Plan([], TagConfidence.Medium, Hit("USUM71210782", 1))));
        Assert.Equal(ExplicitAdvisory.Explicit, ExplicitAdvisory.Decide(song, Plan([], TagConfidence.Strong, Hit("USUM71210782", 1)))!.Value);

        var rehearsed = Plan([], TagConfidence.Strong, Hit("USUM71210782", 1));
        rehearsed.Rehearsed = true;
        Assert.Null(ExplicitAdvisory.Decide(song, rehearsed));
    }

    [Fact]
    public void ACodeNoHitCarries_SaysNothing()
    {
        var plan = Plan(["GBAAA9800001"], TagConfidence.Strong, Hit("USUM71210782", 1));
        Assert.Null(ExplicitAdvisory.Decide(new Song { Title = "Money Trees" }, plan));
    }

    [Theory]
    [InlineData("Money Trees (Clean)", null)]
    [InlineData("Money Trees", @"@@peer\Music\Kendrick Lamar - Money Trees (Clean Version).flac")]
    public void ANameThatSaysClean_IsTheCleanEdit(string title, string? sourceFile)
    {
        var plan = Plan([], TagConfidence.Strong, Hit("USUM71210782", 1));
        Assert.Equal(new AdvisoryDecision(ExplicitAdvisory.Clean, "the name"),
            ExplicitAdvisory.Decide(new Song { Title = title }, plan, sourceFile));
    }
}
