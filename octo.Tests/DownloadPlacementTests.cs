using Microsoft.Extensions.Configuration;
using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.Logging.Abstractions;
using Moq;
using Octo.Models.Domain;
using Octo.Models.Settings;
using Octo.Services;
using Octo.Services.Common;
using Octo.Services.Fingerprint;
using Octo.Services.Library;
using Octo.Services.Local;
using Octo.Services.Notifications;
using Octo.Services.Soulseek;
using Octo.Services.Subsonic;

namespace Octo.Tests;

/// <summary>
/// A download is placed after it is tagged and from the Song (#48), a list of artists never
/// names a folder (#49), and a path that is already taken is only ever replaced by the same song.
/// Each of these used to be decided from the search request before anything knew what the file was.
/// </summary>
public sealed class DownloadPlacementTests : IDisposable
{
    private readonly string _root = Path.Combine(Path.GetTempPath(), "octo-place-" + Guid.NewGuid().ToString("N"));

    public void Dispose()
    {
        try { if (Directory.Exists(_root)) Directory.Delete(_root, recursive: true); } catch { /* best effort */ }
    }

    private static BaseDownloadService.RequestedIdentity Requested(string artist, string title, string album = "", int? track = null)
        => new(artist, title, album, track);

    private static VerificationResult Confirmed(AcoustIdRecording match) => new()
    {
        Verdict = VerificationVerdict.Confirmed,
        Match = match,
        RecordingId = match.RecordingId,
    };

    // ---- PrimaryCredit: split only on proof ---------------------------------------------

    [Theory]
    [InlineData("Earth, Wind & Fire", "Earth, Wind & Fire")]
    [InlineData("Tyler, The Creator", "Tyler, The Creator")]
    [InlineData("Simon & Garfunkel", "Simon & Garfunkel")]
    public void PrimaryCredit_WholeStringNamedByASource_IsKeptWhole(string requested, string structured)
        => Assert.Equal(requested, BaseDownloadService.PrimaryCredit(requested, structured));

    [Theory]
    [InlineData("Bizarrap, Rauw Alejandro", "Bizarrap", "Bizarrap")]
    [InlineData("Kavinsky & Lovefoxxx", "Kavinsky", "Kavinsky")]
    [InlineData("Bizarrap x Rauw Alejandro", "Bizarrap", "Bizarrap")]
    [InlineData("Drake feat. Rihanna", "Drake", "Drake")]
    public void PrimaryCredit_SourceNamesTheFirstCredit_SplitsThere(string requested, string structured, string expected)
        => Assert.Equal(expected, BaseDownloadService.PrimaryCredit(requested, structured));

    /// <summary>"Tyler, The Creator" would become "Tyler" under any rule that splits on a comma.</summary>
    [Theory]
    [InlineData("Tyler, The Creator")]
    [InlineData("Bizarrap, Rauw Alejandro")]
    public void PrimaryCredit_NoStructuredSource_NeverSplits(string requested)
        => Assert.Equal(requested, BaseDownloadService.PrimaryCredit(requested, null, null));

    [Fact]
    public void PrimaryCredit_SourceNamingALaterCredit_DoesNotSplit()
        => Assert.Equal("Bizarrap, Rauw Alejandro",
            BaseDownloadService.PrimaryCredit("Bizarrap, Rauw Alejandro", "Rauw Alejandro"));

    /// <summary>A source that names the whole credit wins over one that names only its first artist.</summary>
    [Fact]
    public void PrimaryCredit_AnySourceNamingTheWholeCredit_KeepsItWhole()
        => Assert.Equal("Tyler, The Creator",
            BaseDownloadService.PrimaryCredit("Tyler, The Creator", "Tyler", "Tyler, The Creator"));

    // ---- ChooseLayout -------------------------------------------------------------------

    [Fact]
    public void ChooseLayout_NameFromMatchOff_UsesTheRequestButThePrimaryFolder()
    {
        var song = new Song { Artist = "Bizarrap, Rauw Alejandro", Title = "Tagged Title", PrimaryArtist = "Bizarrap" };
        var choice = BaseDownloadService.ChooseLayout(song,
            Requested("Bizarrap, Rauw Alejandro", "Requested Title", "Requested Album", 4), nameFromMatch: false);

        Assert.Equal("Bizarrap", choice.FolderArtist);
        Assert.Equal("Bizarrap, Rauw Alejandro", choice.FileArtist);
        Assert.Equal("Requested Title", choice.Title);
        Assert.Equal("Requested Album", choice.Album);
        Assert.Equal(4, choice.Track);
    }

    [Fact]
    public void ChooseLayout_NameFromMatchOn_Confirmed_UsesTheRecording()
    {
        var match = new AcoustIdRecording("rec", "Teardrop", ["Massive Attack"], "Mezzanine", 1998);
        var song = new Song
        {
            Artist = "Massive Attack", Title = "Teardrop", Album = "Mezzanine", Track = 3,
            Verification = Confirmed(match),
        };
        var choice = BaseDownloadService.ChooseLayout(song,
            Requested("massive attack", "Teardrop (Official Video)"), nameFromMatch: true);

        Assert.Equal("Massive Attack", choice.FolderArtist);
        Assert.Equal("Teardrop", choice.Title);
        Assert.Equal("Mezzanine", choice.Album);
        Assert.Equal(3, choice.Track);
    }

    [Fact]
    public void ChooseLayout_NameFromMatchOn_Inconclusive_UsesTheRequest()
    {
        var song = new Song
        {
            Artist = "X", Title = "Tagged", Album = "Tagged Album",
            Verification = new VerificationResult { Reason = InconclusiveReason.NoEntry },
        };
        var choice = BaseDownloadService.ChooseLayout(song, Requested("Req Artist", "Req Title", "Req Album"), nameFromMatch: true);

        Assert.Equal("Req Artist", choice.FolderArtist);
        Assert.Equal("Req Title", choice.Title);
        Assert.Equal("Req Album", choice.Album);
    }

    /// <summary>A request with no album takes the album it was tagged with, and that album's track (#50).</summary>
    [Fact]
    public void ChooseLayout_RequestWithoutAlbum_TakesTheTaggedAlbumAndItsTrack()
    {
        var song = new Song { Artist = "A", Title = "T", Album = "Deezer Album", Track = 7 };
        var choice = BaseDownloadService.ChooseLayout(song, Requested("A", "T", "", null), nameFromMatch: false);

        Assert.Equal("Deezer Album", choice.Album);
        Assert.Equal(7, choice.Track);
    }

    // ---- PlaceInLibraryAsync ------------------------------------------------------------

    private PlacementService Service(FolderStructure layout, IReadOnlyList<LocalSongMapping>? mappings = null)
    {
        Directory.CreateDirectory(_root);
        var library = new Mock<ILocalLibraryService>();
        library.Setup(l => l.GetMappingsAsync()).ReturnsAsync(mappings ?? []);
        return new PlacementService(_root, layout, library.Object);
    }

    private string Landed(string name, byte[]? bytes = null)
    {
        var dir = Path.Combine(_root, "peer share", "some folder");
        Directory.CreateDirectory(dir);
        var path = Path.Combine(dir, name);
        File.WriteAllBytes(path, bytes ?? AudioFixtures.Mp3());
        return path;
    }

    [Fact]
    public async Task PlaceInLibrary_Organized_FilesUnderThePrimaryArtistAndKeepsTheVersion()
    {
        var service = Service(FolderStructure.Organized);
        var landed = Landed("x.mp3");
        var song = new Song { Artist = "Bizarrap, Rauw Alejandro", Title = "Session (Live)", PrimaryArtist = "Bizarrap" };

        var placement = await service.Place(song, Requested("Bizarrap, Rauw Alejandro", "Session (Live)", "An Album", 2), landed);

        Assert.Equal(Path.Combine(_root, "Bizarrap", "An Album", "02 - Session (Live).mp3"), placement.Path);
        Assert.True(File.Exists(placement.Path));
        Assert.True(placement.CreatedFolder);
        // The peer's own folders are gone once empty; the music root is never touched.
        Assert.False(Directory.Exists(Path.Combine(_root, "peer share")));
        Assert.True(Directory.Exists(_root));
    }

    /// <summary>Flat has no folder to scatter, so the whole credit stays in the file name (#49).</summary>
    [Fact]
    public async Task PlaceInLibrary_Flat_KeepsTheWholeCreditInTheFileName()
    {
        var service = Service(FolderStructure.Flat);
        var song = new Song { Artist = "Bizarrap, Rauw Alejandro", Title = "T", PrimaryArtist = "Bizarrap" };

        var placement = await service.Place(song, Requested("Bizarrap, Rauw Alejandro", "T"), Landed("x.mp3"));

        Assert.Equal(Path.Combine(_root, "Bizarrap, Rauw Alejandro - T.mp3"), placement.Path);
    }

    /// <summary>The bug this replaces: "Song (Live)" used to be named "Song" and delete it.</summary>
    [Fact]
    public async Task PlaceInLibrary_ForeignFileAtTheTarget_KeepsBoth()
    {
        var service = Service(FolderStructure.Flat);
        var existing = Path.Combine(_root, "A - Song.mp3");
        File.WriteAllBytes(existing, AudioFixtures.Mp3());
        var before = File.ReadAllBytes(existing);

        var placement = await service.Place(new Song { Artist = "A", Title = "Song" }, Requested("A", "Song"), Landed("new.mp3"));

        Assert.Equal(Path.Combine(_root, "A - Song (1).mp3"), placement.Path);
        Assert.Equal(before, File.ReadAllBytes(existing));
    }

    /// <summary>A second download of a song Octo itself placed replaces it rather than doubling it.</summary>
    [Fact]
    public async Task PlaceInLibrary_OctoOwnedSameSong_Replaces()
    {
        var existing = Path.Combine(_root, "A - Song.mp3");
        Directory.CreateDirectory(_root);
        File.WriteAllBytes(existing, AudioFixtures.Mp3());
        var service = Service(FolderStructure.Flat,
            [new LocalSongMapping { LocalPath = existing, Artist = "A", Title = "Song" }]);

        var placement = await service.Place(new Song { Artist = "A", Title = "Song" }, Requested("A", "Song"), Landed("new.mp3"));

        Assert.Equal(existing, placement.Path);
        Assert.False(File.Exists(Path.Combine(_root, "A - Song (1).mp3")));
    }

    [Fact]
    public async Task PlaceInLibrary_SameRecordingId_Replaces()
    {
        var service = Service(FolderStructure.Flat);
        var existing = Path.Combine(_root, "A - Song.mp3");
        File.WriteAllBytes(existing, AudioFixtures.Mp3());
        using (var tagged = TagLib.File.Create(existing))
        {
            TagWriterExtras.SetRecordingId(tagged, "rec-1");
            tagged.Save();
        }

        var song = new Song { Artist = "A", Title = "Song", MusicBrainzRecordingId = "rec-1" };
        var placement = await service.Place(song, Requested("A", "Song"), Landed("new.mp3"));

        Assert.Equal(existing, placement.Path);
    }

    [Fact]
    public async Task PlaceInLibrary_DifferentRecordingId_KeepsBoth()
    {
        var service = Service(FolderStructure.Flat);
        var existing = Path.Combine(_root, "A - Song.mp3");
        File.WriteAllBytes(existing, AudioFixtures.Mp3());
        using (var tagged = TagLib.File.Create(existing))
        {
            TagWriterExtras.SetRecordingId(tagged, "rec-1");
            tagged.Save();
        }

        var song = new Song { Artist = "A", Title = "Song", MusicBrainzRecordingId = "rec-2" };
        var placement = await service.Place(song, Requested("A", "Song"), Landed("new.mp3"));

        Assert.Equal(Path.Combine(_root, "A - Song (1).mp3"), placement.Path);
    }

    [Fact]
    public async Task PlaceInLibrary_IntoAnAlbumFolderThatAlreadyHasMusic_IsNotANewFolder()
    {
        var service = Service(FolderStructure.Organized);
        var albumDir = Path.Combine(_root, "A", "Album");
        Directory.CreateDirectory(albumDir);
        File.WriteAllBytes(Path.Combine(albumDir, "01 - Other.mp3"), AudioFixtures.Mp3());

        var placement = await service.Place(new Song { Artist = "A", Title = "T" }, Requested("A", "T", "Album", 2), Landed("x.mp3"));

        Assert.False(placement.CreatedFolder);
    }

    [Fact]
    public async Task PlaceInLibrary_MissingFile_ReturnsTheLandedPath()
    {
        var service = Service(FolderStructure.Flat);
        var missing = Path.Combine(_root, "nope.mp3");

        var placement = await service.Place(new Song { Artist = "A", Title = "T" }, Requested("A", "T"), missing);

        Assert.Equal(missing, placement.Path);
    }

    // ---- Tags ---------------------------------------------------------------------------

    /// <summary>
    /// TagLib# after 2.3.0 writes Tag.MusicBrainzTrackId as the release TRACK id on ID3. Writing
    /// the UFID frame directly keeps the recording id where Navidrome reads it whatever the package.
    /// </summary>
    [Fact]
    public void SetRecordingId_Mp3_WritesTheMusicBrainzOrgUfid()
    {
        Directory.CreateDirectory(_root);
        var path = Path.Combine(_root, "t.mp3");
        File.WriteAllBytes(path, AudioFixtures.Mp3());
        using (var file = TagLib.File.Create(path))
        {
            TagWriterExtras.SetRecordingId(file, "rec-1");
            file.Save();
        }

        using var read = TagLib.File.Create(path);
        var id3 = (TagLib.Id3v2.Tag)read.GetTag(TagLib.TagTypes.Id3v2, false);
        var ufid = TagLib.Id3v2.UniqueFileIdentifierFrame.Get(id3, "http://musicbrainz.org", false);
        Assert.NotNull(ufid);
        Assert.Equal("rec-1", ufid.Identifier.ToString());
        Assert.Equal("rec-1", TagWriterExtras.ReadRecordingId(read));
    }

    [Fact]
    public void SetRecordingId_Flac_WritesMusicbrainzTrackid()
    {
        Directory.CreateDirectory(_root);
        var path = Path.Combine(_root, "t.flac");
        File.WriteAllBytes(path, AudioFixtures.Flac());
        using (var file = TagLib.File.Create(path))
        {
            TagWriterExtras.SetRecordingId(file, "rec-1");
            file.Save();
        }

        using var read = TagLib.File.Create(path);
        var xiph = (TagLib.Ogg.XiphComment)read.GetTag(TagLib.TagTypes.Xiph, false);
        Assert.Equal("rec-1", xiph.GetFirstField("MUSICBRAINZ_TRACKID"));
    }

    [Theory]
    [InlineData("t.mp3")]
    [InlineData("t.flac")]
    public void SetMultiValue_WritesOneValuePerArtist(string name)
    {
        Directory.CreateDirectory(_root);
        var path = Path.Combine(_root, name);
        File.WriteAllBytes(path, name.EndsWith(".mp3") ? AudioFixtures.Mp3() : AudioFixtures.Flac());
        using (var file = TagLib.File.Create(path))
        {
            TagWriterExtras.SetMultiValue(file, "ARTISTS", ["Bizarrap", "Rauw Alejandro"]);
            file.Save();
        }

        using var read = TagLib.File.Create(path);
        string[] values = name.EndsWith(".mp3")
            ? TagLib.Id3v2.UserTextInformationFrame.Get((TagLib.Id3v2.Tag)read.GetTag(TagLib.TagTypes.Id3v2, false), "ARTISTS", false)!.Text
            : ((TagLib.Ogg.XiphComment)read.GetTag(TagLib.TagTypes.Xiph, false)).GetField("ARTISTS");
        Assert.Equal(["Bizarrap", "Rauw Alejandro"], values);
    }

    [Fact]
    public async Task WriteMetadata_ConfirmedMatch_WritesIdsArtistsAndNoAlbumId()
    {
        var service = Service(FolderStructure.Flat);
        var path = Path.Combine(_root, "t.flac");
        File.WriteAllBytes(path, AudioFixtures.Flac());
        var song = new Song
        {
            Title = "Session", Artist = "Bizarrap, Rauw Alejandro", Album = "Session",
            PrimaryArtist = "Bizarrap", Artists = ["Bizarrap", "Rauw Alejandro"],
            MusicBrainzRecordingId = "rec-1", MusicBrainzReleaseId = "rel-1",
            MusicBrainzReleaseGroupId = "rg-1", MusicBrainzAlbumTitle = "Session",
        };

        await service.Write(path, song);

        using var read = TagLib.File.Create(path);
        var xiph = (TagLib.Ogg.XiphComment)read.GetTag(TagLib.TagTypes.Xiph, false);
        Assert.Equal("rec-1", xiph.GetFirstField("MUSICBRAINZ_TRACKID"));
        Assert.Equal(["Bizarrap", "Rauw Alejandro"], xiph.GetField("ARTISTS"));
        Assert.Equal("rg-1", read.Tag.MusicBrainzReleaseGroupId);
        // Navidrome groups albums by MUSICBRAINZ_ALBUMID before the album name.
        Assert.True(string.IsNullOrEmpty(read.Tag.MusicBrainzReleaseId));
        // With no album artist of its own, the first credit stands in, not the list.
        Assert.Equal(["Bizarrap"], read.Tag.AlbumArtists);
    }

    /// <summary>A group id beside an album name from somewhere else would describe another album.</summary>
    [Fact]
    public async Task WriteMetadata_AlbumFromAnotherSource_GetsNoGroupId()
    {
        var service = Service(FolderStructure.Flat);
        var path = Path.Combine(_root, "t.flac");
        File.WriteAllBytes(path, AudioFixtures.Flac());
        var song = new Song
        {
            Title = "Song", Artist = "A", Album = "Now That's What I Call Music! 42",
            MusicBrainzRecordingId = "rec-1", MusicBrainzReleaseGroupId = "rg-1", MusicBrainzAlbumTitle = "The Real Album",
        };

        await service.Write(path, song);

        using var read = TagLib.File.Create(path);
        Assert.True(string.IsNullOrEmpty(read.Tag.MusicBrainzReleaseGroupId));
    }

    // ---- A track that arrives without an album (#50) ------------------------------------

    [Fact]
    public void ApplySingleFallback_NoAlbum_FilesUnderTheTitle()
    {
        var song = new Song { Artist = "Bizarrap, Rauw Alejandro", PrimaryArtist = "Bizarrap", Title = "Session 56" };
        BaseDownloadService.ApplySingleFallback(song, enabled: true);

        Assert.Equal("Session 56", song.Album);
        Assert.Equal("Bizarrap", song.AlbumArtist);
    }

    [Fact]
    public void ApplySingleFallback_Compilation_LeavesItEmpty()
    {
        var song = new Song { Artist = "A", Title = "T", IsCompilation = true };
        BaseDownloadService.ApplySingleFallback(song, enabled: true);
        Assert.Equal("", song.Album);
    }

    [Theory]
    [InlineData("Various Artists")]
    [InlineData("various")]
    [InlineData("VA")]
    public void ApplySingleFallback_VariousArtistsAlbumArtist_LeavesItEmpty(string albumArtist)
    {
        var song = new Song { Artist = "A", Title = "T", AlbumArtist = albumArtist };
        BaseDownloadService.ApplySingleFallback(song, enabled: true);
        Assert.Equal("", song.Album);
    }

    [Fact]
    public void ApplySingleFallback_Off_ChangesNothing()
    {
        var song = new Song { Artist = "A", Title = "T" };
        BaseDownloadService.ApplySingleFallback(song, enabled: false);
        Assert.Equal("", song.Album);
    }

    [Fact]
    public void ApplySingleFallback_AlbumAlreadyKnown_IsLeftAlone()
    {
        var song = new Song { Artist = "A", Title = "T", Album = "Real Album" };
        BaseDownloadService.ApplySingleFallback(song, enabled: true);
        Assert.Equal("Real Album", song.Album);
    }

    /// <summary>A source's own album tag beats filing the track under its title.</summary>
    [Fact]
    public async Task Enrich_FilesOwnAlbum_BeatsTheTitle()
    {
        var service = Service(FolderStructure.Flat);
        var path = Path.Combine(_root, "t.flac");
        File.WriteAllBytes(path, AudioFixtures.Flac());
        using (var file = TagLib.File.Create(path))
        {
            file.Tag.Album = "Peer Album";
            file.Tag.AlbumArtists = ["Peer Artist"];
            file.Save();
        }

        var song = new Song { Artist = "A", Title = "T" };
        await service.Enrich(path, song);

        Assert.Equal("Peer Album", song.Album);
        Assert.Equal("Peer Artist", song.AlbumArtist);
    }

    [Fact]
    public async Task Enrich_CompilationFlagOnTheFile_KeepsTheTitleOutOfTheAlbum()
    {
        var service = Service(FolderStructure.Flat);
        var path = Path.Combine(_root, "t.mp3");
        File.WriteAllBytes(path, AudioFixtures.Mp3());
        using (var file = TagLib.File.Create(path))
        {
            TagWriterExtras.SetCompilation(file, true);
            file.Save();
        }

        var song = new Song { Artist = "A", Title = "T" };
        await service.Enrich(path, song);

        Assert.True(song.IsCompilation);
        Assert.Equal("", song.Album);
    }

    [Fact]
    public async Task Enrich_NothingKnown_FilesTheTrackAsASingle()
    {
        var service = Service(FolderStructure.Flat);
        var path = Path.Combine(_root, "t.mp3");
        File.WriteAllBytes(path, AudioFixtures.Mp3());

        var song = new Song { Artist = "A", Title = "T" };
        await service.Enrich(path, song);

        Assert.Equal("T", song.Album);
    }

    // ---- a library action's replacement (W8) --------------------------------------------

    private (PlacementService Service, string Original, string Staged, KeptIdentity Identity) Replacement()
    {
        var service = Service(FolderStructure.Organized);
        var original = Path.Combine(_root, "Odd Folder", "03 teardrop old.mp3");
        Directory.CreateDirectory(Path.GetDirectoryName(original)!);
        File.WriteAllBytes(original, AudioFixtures.Mp3());
        using (var f = TagLib.File.Create(original)) { f.Tag.Title = "Teardrop"; f.Save(); }
        return (service, original, service.Stage(Landed("peer upload.flac", AudioFixtures.Flac())).Path, KeptIdentityTags.Read(original)!);
    }

    [Fact]
    public async Task AReplacementMovesInUnderTheOriginalsFolderAndName()
    {
        var (service, original, staged, identity) = Replacement();
        Assert.Contains(SoulseekDownloadService.IncomingFolderName, staged);
        string? announced = null;
        var handoff = new ReplacementHandoff { OriginalPath = original, Identity = identity,
            BeforeReveal = _ => { File.Delete(original); return Task.FromResult<string?>(null); },
            OnRevealed = path => announced = path };
        var placed = await service.Reveal(new Song { Artist = "Massive Attack", Title = "Teardrop" }, Requested("Massive Attack", "Teardrop"), staged, handoff);
        Assert.Equal(Path.Combine(_root, "Odd Folder", "03 teardrop old.flac"), placed.Path);
        Assert.Equal(placed.Path, handoff.RevealedPath);
        // Told at once, so the library action can record the swap before anything else runs.
        Assert.Equal(placed.Path, announced);
        Assert.False(File.Exists(staged));
        using var revealed = TagLib.File.Create(placed.Path);
        Assert.Equal("Teardrop", revealed.Tag.Title);
    }

    [Fact]
    public async Task ARefusedReplacementIsDeletedBeforeAnyScanCouldSeeIt()
    {
        var (service, original, staged, identity) = Replacement();
        var handoff = new ReplacementHandoff { OriginalPath = original, Identity = identity, BeforeReveal = _ => Task.FromResult<string?>("is not lossless") };
        var refused = await Assert.ThrowsAsync<ReplacementRejectedException>(() => service.Reveal(new Song(), Requested("A", "T"), staged, handoff));
        Assert.Equal("is not lossless", refused.Problem);
        Assert.False(File.Exists(staged));
        Assert.True(File.Exists(original));
        Assert.Empty(Directory.EnumerateFiles(_root, "*.flac", SearchOption.AllDirectories));
    }

    // ---- cover.jpg (#51) ----------------------------------------------------------------

    private static readonly byte[] CoverBytes = [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];

    [Fact]
    public async Task WriteSidecars_NewAlbumFolderInOrganized_GetsACoverFile()
    {
        var service = Service(FolderStructure.Organized);
        var placement = await service.Place(new Song { Artist = "A", Title = "T" }, Requested("A", "T", "Album", 1), Landed("x.mp3"));

        await service.Sidecars(new Song(), placement, CoverBytes);

        Assert.True(File.Exists(Path.Combine(_root, "A", "Album", "cover.jpg")));
    }

    /// <summary>Navidrome ranks cover.* above embedded art, so an album that was already there must not change cover.</summary>
    [Fact]
    public async Task WriteSidecars_ExistingAlbumFolder_GetsNoCoverFile()
    {
        var service = Service(FolderStructure.Organized);
        var albumDir = Path.Combine(_root, "A", "Album");
        Directory.CreateDirectory(albumDir);
        File.WriteAllBytes(Path.Combine(albumDir, "01 - Other.mp3"), AudioFixtures.Mp3());
        var placement = await service.Place(new Song { Artist = "A", Title = "T" }, Requested("A", "T", "Album", 2), Landed("x.mp3"));

        await service.Sidecars(new Song(), placement, CoverBytes);

        Assert.False(File.Exists(Path.Combine(albumDir, "cover.jpg")));
    }

    /// <summary>In Flat every download shares one folder: one cover.jpg would cover every album.</summary>
    [Fact]
    public async Task WriteSidecars_Flat_NeverGetsACoverFile()
    {
        var service = Service(FolderStructure.Flat);
        var placement = new BaseDownloadService.Placement(Path.Combine(_root, "A - T.mp3"), CreatedFolder: true);
        File.WriteAllBytes(placement.Path, AudioFixtures.Mp3());

        await service.Sidecars(new Song(), placement, CoverBytes);

        Assert.False(File.Exists(Path.Combine(_root, "cover.jpg")));
    }

    [Fact]
    public async Task WriteSidecars_NeverReplacesAnExistingCover()
    {
        var service = Service(FolderStructure.Organized);
        var placement = await service.Place(new Song { Artist = "A", Title = "T" }, Requested("A", "T", "Album", 1), Landed("x.mp3"));
        var existing = Path.Combine(_root, "A", "Album", "folder.png");
        File.WriteAllBytes(existing, [1, 2, 3]);

        await service.Sidecars(new Song(), placement, CoverBytes);

        Assert.False(File.Exists(Path.Combine(_root, "A", "Album", "cover.jpg")));
        Assert.Equal([1, 2, 3], File.ReadAllBytes(existing));
    }

    // ---- The cover a download keeps ------------------------------------------------------

    /// <summary>A service that judges covers against a catalog cover and logs to a tracker.</summary>
    private (PlacementService Service, AcquisitionTracker Tracker) CoverService(byte[] catalog)
    {
        var handler = new Mock<HttpMessageHandler>();
        Moq.Protected.ProtectedExtension.Protected(handler)
            .Setup<Task<HttpResponseMessage>>("SendAsync", Moq.Protected.ItExpr.IsAny<HttpRequestMessage>(), Moq.Protected.ItExpr.IsAny<CancellationToken>())
            .ReturnsAsync((HttpRequestMessage request, CancellationToken _) =>
                request.RequestUri!.Host == "deezer.example"
                    ? new HttpResponseMessage(System.Net.HttpStatusCode.OK) { Content = new ByteArrayContent(catalog) }
                    : new HttpResponseMessage(System.Net.HttpStatusCode.NotFound));
        var http = new Mock<IHttpClientFactory>();
        http.Setup(f => f.CreateClient(It.IsAny<string>())).Returns(() => new HttpClient(handler.Object));
        var metadata = TestOptions.Monitor(new MetadataSettings());
        var tracker = new AcquisitionTracker(NullLogger<AcquisitionTracker>.Instance);
        var services = new ServiceCollection()
            .AddSingleton<Microsoft.Extensions.Options.IOptionsMonitor<SoulseekSettings>>(TestOptions.Monitor(new SoulseekSettings()))
            .AddSingleton<Microsoft.Extensions.Options.IOptionsMonitor<MetadataSettings>>(metadata)
            .AddSingleton(tracker)
            .AddSingleton(new Octo.Services.CoverArt.DownloadCoverResolver(
                new Octo.Services.CoverArt.CoverArtArchiveLookup(http.Object, NullLogger<Octo.Services.CoverArt.CoverArtArchiveLookup>.Instance),
                new Octo.Services.CoverArt.CoverArtAggregator([], NullLogger<Octo.Services.CoverArt.CoverArtAggregator>.Instance),
                http.Object, metadata, NullLogger<Octo.Services.CoverArt.DownloadCoverResolver>.Instance))
            .BuildServiceProvider();
        var library = new Mock<ILocalLibraryService>();
        return (new PlacementService(_root, FolderStructure.Organized, library.Object, services), tracker);
    }

    private string FlacWithCover(string name, byte[]? cover)
    {
        Directory.CreateDirectory(_root);
        var path = Path.Combine(_root, name);
        File.WriteAllBytes(path, AudioFixtures.Flac());
        if (cover is not null)
        {
            using var file = TagLib.File.Create(path);
            file.Tag.Pictures = [new TagLib.Picture(new TagLib.ByteVector(cover)) { Type = TagLib.PictureType.FrontCover }];
            file.Save();
        }
        return path;
    }

    private static byte[]? CoverOf(string path)
    {
        using var file = TagLib.File.Create(path);
        return file.Tag.Pictures.FirstOrDefault()?.Data?.Data;
    }

    private static Song CoverSong() => new()
    {
        Artist = "Ella Langley", Title = "Choosin' Texas", Album = "Dandelion",
        CoverArtUrlLarge = "https://deezer.example/cover.jpg", ExternalProvider = "test", ExternalId = "x",
    };

    [Fact]
    public async Task WriteMetadata_AGoodCoverOfTheSameArtStaysByteForByte_AndTheTimelineSaysSo()
    {
        var (service, tracker) = CoverService(CoverKeepTests.Art(1, 1000));
        var own = CoverKeepTests.Art(1, 1400);
        var path = FlacWithCover("a.flac", own);
        tracker.Begin("test", "x", "x", "alice");

        await service.Write(path, CoverSong());

        Assert.Equal(own, CoverOf(path));
        var line = tracker.Detail("test:x", "alice")!.Events!.Single(e => e.Kind == AcquisitionEventKinds.Cover);
        Assert.Equal("Kept the cover it came with", line.Text);
        Assert.Matches(@"^1400 x 1400 px, \d+ KB, the same art$", line.Detail);
    }

    [Fact]
    public async Task WriteMetadata_AnotherPictureIsReplacedByTheCatalogsCover_AndTheTimelineSaysWhy()
    {
        var catalog = CoverKeepTests.Art(1, 1000);
        var (service, tracker) = CoverService(catalog);
        var path = FlacWithCover("b.flac", CoverKeepTests.Art(2, 1400));
        tracker.Begin("test", "x", "x", "alice");

        await service.Write(path, CoverSong());

        Assert.Equal(catalog, CoverOf(path));
        var line = tracker.Detail("test:x", "alice")!.Events!.Single(e => e.Kind == AcquisitionEventKinds.Cover);
        Assert.Equal("Replaced its 1400 px cover with a 1000 x 1000 px one from the catalog", line.Text);
        Assert.Equal("It was a different picture from the album's cover. Now embedded in the song.", line.Detail);
    }

    /// <summary>Better quality: the new copy gets the old copy's cover, the picture the album
    /// already shows, instead of the peer's.</summary>
    [Fact]
    public async Task WriteMetadata_AReplacementTakesTheOldCopysGoodCover()
    {
        var (service, tracker) = CoverService(CoverKeepTests.Art(1, 1000));
        var oldCover = CoverKeepTests.Art(1, 1200);
        var original = FlacWithCover("old.flac", oldCover);
        var staged = FlacWithCover("new.flac", CoverKeepTests.Art(5, 300));
        tracker.Begin("test", "x", "x", "alice");

        BaseDownloadService.ActAsReplacementOf(original);
        try { await service.Write(staged, CoverSong()); }
        finally { BaseDownloadService.ActAsReplacementOf(null); }

        Assert.Equal(oldCover, CoverOf(staged));
        Assert.Equal("Kept your old copy's cover",
            tracker.Detail("test:x", "alice")!.Events!.Single(e => e.Kind == AcquisitionEventKinds.Cover).Text);
    }

    /// <summary>The download service with the transfer stubbed out, so placement and tagging can
    /// be driven against real files in a temp folder.</summary>
    private sealed class PlacementService(string root, FolderStructure layout, ILocalLibraryService library,
        IServiceProvider? services = null)
        : BaseDownloadService(
            new ConfigurationBuilder().AddInMemoryCollection(new Dictionary<string, string?> { ["Library:DownloadPath"] = root }).Build(),
            library,
            Mock.Of<IMusicMetadataService>(),
            TestOptions.Monitor(new SubsonicSettings { FolderStructure = layout, AutoDetectDownloadPath = false }),
            TestOptions.Monitor(new GenreSettings()),
            new NavidromeIdentityService(
                TestOptions.Monitor(new SubsonicSettings { AutoDetectDownloadPath = false }),
                Mock.Of<IHttpClientFactory>(), NullLogger<NavidromeIdentityService>.Instance),
            new DownloadHistoryService(Path.Combine(root, "history.json"), NullLogger<DownloadHistoryService>.Instance),
            new NotificationService([], TestOptions.Monitor(new NotificationSettings()), NullLogger<NotificationService>.Instance),
            services ?? new ServiceCollection()
                .AddSingleton<Microsoft.Extensions.Options.IOptionsMonitor<SoulseekSettings>>(TestOptions.Monitor(new SoulseekSettings()))
                .BuildServiceProvider(),
            NullLogger.Instance)
    {
        protected override string ProviderName => "test";
        public override Task<bool> IsAvailableAsync() => Task.FromResult(true);
        protected override Task<string> DownloadTrackAsync(string trackId, Song song, bool suppressNotify,
            DownloadSource? sourceOverride, bool upgradeSearch, CancellationToken cancellationToken) => throw new NotSupportedException();
        protected override string? ExtractExternalIdFromAlbumId(string albumId) => null;

        public Task<Placement> Place(Song song, RequestedIdentity requested, string path) =>
            PlaceInLibraryAsync(song, requested, path);

        public Task Write(string path, Song song) => WriteMetadataAsync(path, song, CancellationToken.None);

        public Task Enrich(string path, Song song) =>
            IdentifyAsync(song, new RequestedIdentity(song.Artist, song.Title, song.Album ?? "", song.Track), path, null, CancellationToken.None);

        public Task Sidecars(Song song, Placement placement, byte[]? cover) =>
            WriteSidecarsAsync(song, placement, cover, CancellationToken.None);

        public Placement Stage(string landed) => StageReplacement(landed);
        public Task<Placement> Reveal(Song s, RequestedIdentity r, string staged, ReplacementHandoff h) => RevealReplacementAsync(s, r, staged, h);
    }
}

/// <summary>
/// The smallest real audio files TagLib will open as the formats Octo downloads, built in memory
/// so the tests need neither fixtures on disk nor ffmpeg.
/// </summary>
internal static class AudioFixtures
{
    /// <summary>Twenty silent MPEG-1 Layer III frames, 128 kbps, 44.1 kHz.</summary>
    public static byte[] Mp3()
    {
        const int frameLength = 417;
        var bytes = new byte[frameLength * 20];
        for (var frame = 0; frame < 20; frame++)
        {
            var offset = frame * frameLength;
            bytes[offset] = 0xFF;
            bytes[offset + 1] = 0xFB;
            bytes[offset + 2] = 0x90;
            bytes[offset + 3] = 0x64;
        }
        return bytes;
    }

    /// <summary>A FLAC with a STREAMINFO block describing two seconds of 16-bit stereo and no frames.</summary>
    public static byte[] Flac()
    {
        using var stream = new MemoryStream();
        stream.Write("fLaC"u8);
        stream.Write([0x80, 0x00, 0x00, 0x22]);
        stream.Write([0x10, 0x00, 0x10, 0x00]);
        stream.Write([0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        const ulong sampleRate = 44100, channelsMinusOne = 1, bitsMinusOne = 15, totalSamples = 88200;
        var packed = (sampleRate << 44) | (channelsMinusOne << 41) | (bitsMinusOne << 36) | totalSamples;
        for (var shift = 56; shift >= 0; shift -= 8) stream.WriteByte((byte)(packed >> shift));
        stream.Write(new byte[16]);
        return stream.ToArray();
    }
}
