using System.Net;
using Microsoft.Extensions.Logging.Abstractions;
using Moq;
using Moq.Protected;
using Octo.Models.Domain;
using Octo.Models.Settings;
using Octo.Services.CoverArt;
using Octo.Services.Soulseek;
using SixLabors.ImageSharp;
using SixLabors.ImageSharp.Drawing.Processing;
using SixLabors.ImageSharp.PixelFormats;
using SixLabors.ImageSharp.Processing;

namespace Octo.Tests;

/// <summary>
/// The download-time cover chain (#51): the right release's cover when a fingerprint named it,
/// the catalog and the aggregator next, and a cover that is not square never passes for one.
/// </summary>
public class CoverChainTests
{
    // Apple's pacing is for the real service; these talk to a fake.
    public CoverChainTests() => ITunesCoverArtLookup.AppleInterval = TimeSpan.Zero;

    private static byte[] Jpeg(int width, int height, Rgba32? centre = null)
    {
        using var image = new Image<Rgba32>(width, height, new Rgba32(0, 0, 0));
        if (centre is { } colour)
        {
            var side = Math.Min(width, height);
            image.Mutate(ctx => ctx.Fill(Color.FromPixel(colour),
                new RectangleF((width - side) / 2f, (height - side) / 2f, side, side)));
        }
        using var stream = new MemoryStream();
        image.SaveAsJpeg(stream);
        return stream.ToArray();
    }

    private static readonly byte[] Square = Jpeg(600, 600, new Rgba32(200, 40, 40));
    private static readonly byte[] Sharp = Jpeg(1200, 1200, new Rgba32(40, 40, 200));
    private static readonly byte[] Catalog = Jpeg(1000, 1000, new Rgba32(200, 200, 40));
    private static readonly byte[] Thumbnail = Jpeg(200, 200, new Rgba32(90, 90, 90));
    private static readonly byte[] VideoFrame = Jpeg(1280, 720, new Rgba32(40, 200, 40));

    // ---- CoverImage -----------------------------------------------------------------------

    [Fact]
    public void IsUsable_SixteenByNine_IsNotASquareCover()
    {
        Assert.False(CoverImage.IsUsable(VideoFrame, requireSquare: true));
        Assert.True(CoverImage.IsUsable(VideoFrame, requireSquare: false));
    }

    [Fact]
    public void IsUsable_NearlySquare_Passes() =>
        Assert.True(CoverImage.IsUsable(Jpeg(600, 590), requireSquare: true));

    [Fact]
    public void IsUsable_TooSmallOrNotAnImage_Fails()
    {
        Assert.False(CoverImage.IsUsable(Jpeg(100, 100), requireSquare: true));
        Assert.False(CoverImage.IsUsable("not an image"u8.ToArray(), requireSquare: false));
        Assert.False(CoverImage.IsUsable(null, requireSquare: false));
    }

    /// <summary>A YouTube "Topic" frame letterboxes the real cover; its centre square is that cover.</summary>
    [Fact]
    public void CropToSquare_Letterbox_ReturnsTheCentre()
    {
        var cropped = CoverImage.CropToSquare(VideoFrame);

        Assert.NotNull(cropped);
        using var image = Image.Load<Rgba32>(cropped);
        Assert.Equal(720, image.Width);
        Assert.Equal(720, image.Height);
        var middle = image[360, 360];
        Assert.True(middle.G > 150 && middle.R < 100, $"centre pixel was {middle}");
    }

    // ---- DownloadCoverResolver ------------------------------------------------------------

    private sealed class FixedSource(byte[]? bytes) : ICoverArtSource
    {
        public string Name => "fixed";
        public int Calls { get; private set; }

        public Task<byte[]?> TryFetchAsync(SoulseekRouting routing, bool background = false, CancellationToken ct = default)
        {
            Calls++;
            return Task.FromResult(bytes);
        }
    }

    private static IHttpClientFactory Http(Func<HttpRequestMessage, HttpResponseMessage> answer, List<string>? calls = null)
    {
        var handler = new Mock<HttpMessageHandler>();
        handler.Protected()
            .Setup<Task<HttpResponseMessage>>("SendAsync", ItExpr.IsAny<HttpRequestMessage>(), ItExpr.IsAny<CancellationToken>())
            .ReturnsAsync((HttpRequestMessage request, CancellationToken _) =>
            {
                calls?.Add(request.RequestUri!.ToString());
                return answer(request);
            });
        var factory = new Mock<IHttpClientFactory>();
        factory.Setup(f => f.CreateClient(It.IsAny<string>())).Returns(() =>
            new HttpClient(handler.Object) { BaseAddress = new Uri("https://coverartarchive.org/") });
        return factory.Object;
    }

    private static HttpResponseMessage Picture(byte[] bytes) => new(HttpStatusCode.OK) { Content = new ByteArrayContent(bytes) };

    private static DownloadCoverResolver Resolver(IHttpClientFactory http, ICoverArtSource aggregated, MetadataSettings? settings = null) =>
        new(new CoverArtArchiveLookup(http, NullLogger<CoverArtArchiveLookup>.Instance),
            new CoverArtAggregator([aggregated], NullLogger<CoverArtAggregator>.Instance),
            http, TestOptions.Monitor(settings ?? new MetadataSettings()), NullLogger<DownloadCoverResolver>.Instance);

    [Fact]
    public async Task Resolve_ArchiveFirst_WhenTheAlbumIsTheMatchedRelease()
    {
        var calls = new List<string>();
        var http = Http(request => request.RequestUri!.ToString().Contains("coverartarchive")
            ? Picture(Sharp) : new HttpResponseMessage(HttpStatusCode.NotFound), calls);
        var song = new Song
        {
            Artist = "M83", Title = "Lower Your Eyelids", Album = "Before the Dawn Heals Us",
            MusicBrainzReleaseId = "rel-1", MusicBrainzAlbumTitle = "Before the Dawn Heals Us",
            CoverArtUrlLarge = "https://deezer.example/cover.jpg",
        };

        var choice = await Resolver(http, new FixedSource(null)).ResolveAsync(song, null, CancellationToken.None);

        Assert.Equal("Cover Art Archive", choice!.Source);
        Assert.Contains(calls, url => url.Contains("release/rel-1/front-1200"));
        // Asked as well: a larger copy of the same art may come after a sharp one.
        Assert.Contains(calls, url => url.Contains("deezer.example"));
    }

    private static Song MatchedRelease() => new()
    {
        Artist = "M83", Title = "Lower Your Eyelids", Album = "Before the Dawn Heals Us",
        MusicBrainzReleaseId = "rel-1", MusicBrainzAlbumTitle = "Before the Dawn Heals Us",
        CoverArtUrlLarge = "https://deezer.example/cover.jpg",
    };

    /// <summary>The soft cover Brandon got: the archive's 500 px scan beat the catalog's 1000.</summary>
    [Fact]
    public async Task Resolve_ASmallArchiveScanLosesToTheCatalogsLargerCover()
    {
        var http = Http(request => request.RequestUri!.ToString() switch
        {
            var url when url.Contains("front-1200") => new HttpResponseMessage(HttpStatusCode.NotFound),
            var url when url.Contains("front-500") => Picture(Jpeg(500, 500)),
            var url when url.Contains("deezer.example") => Picture(Catalog),
            _ => new HttpResponseMessage(HttpStatusCode.NotFound),
        });

        var choice = await Resolver(http, new FixedSource(null)).ResolveAsync(MatchedRelease(), null, CancellationToken.None);

        Assert.Equal("the catalog", choice!.Source);
        Assert.Equal((1000, 1000), CoverImage.Measure(choice.Bytes));
    }

    [Fact]
    public async Task Resolve_TheArchivesSmallerThumbnailIsUsedWhenItHasNoLargeOne()
    {
        var calls = new List<string>();
        var http = Http(request => request.RequestUri!.ToString().Contains("front-500")
            ? Picture(Square) : new HttpResponseMessage(HttpStatusCode.NotFound), calls);

        var choice = await Resolver(http, new FixedSource(null)).ResolveAsync(MatchedRelease(), null, CancellationToken.None);

        Assert.Equal("Cover Art Archive", choice!.Source);
        Assert.Equal(["release/rel-1/front-1200", "release/rel-1/front-500"],
            calls.Where(url => url.Contains("coverartarchive")).Select(url => new Uri(url).AbsolutePath.TrimStart('/')).ToList());
    }

    [Fact]
    public async Task Resolve_ASmallCatalogCoverKeepsLookingAndTheSearchsLargerOneWins()
    {
        var http = Http(_ => Picture(Square));
        var search = new FixedSource(Catalog);
        var song = new Song { Artist = "A", Title = "T", CoverArtUrlLarge = "https://deezer.example/cover.jpg" };

        var choice = await Resolver(http, search).ResolveAsync(song, Thumbnail, CancellationToken.None);

        Assert.Equal("a cover search", choice!.Source);
        Assert.False(choice.KeepsExisting);
    }

    [Fact]
    public async Task Resolve_APeersTinyThumbnailNeverBeatsARealCover()
    {
        var http = Http(_ => Picture(Square));
        var song = new Song { Artist = "A", Title = "T", CoverArtUrlLarge = "https://deezer.example/cover.jpg" };

        var choice = await Resolver(http, new FixedSource(null)).ResolveAsync(song, Thumbnail, CancellationToken.None);

        Assert.Equal("the catalog", choice!.Source);
        Assert.False(choice.KeepsExisting);
    }

    [Fact]
    public async Task Resolve_TheFilesOwnArtStaysWhenItIsTheLargest()
    {
        var http = Http(_ => Picture(Square));
        var song = new Song { Artist = "A", Title = "T", CoverArtUrlLarge = "https://deezer.example/cover.jpg" };

        var choice = await Resolver(http, new FixedSource(null)).ResolveAsync(song, Sharp, CancellationToken.None);

        Assert.True(choice!.KeepsExisting);
        Assert.Equal("the file itself", choice.Source);
    }

    [Fact]
    public async Task Resolve_ASharpCatalogCoverStopsTheSearch()
    {
        var http = Http(_ => Picture(Catalog));
        var search = new FixedSource(Sharp);
        var song = new Song { Artist = "A", Title = "T", CoverArtUrlLarge = "https://deezer.example/cover.jpg" };

        var choice = await Resolver(http, search).ResolveAsync(song, null, CancellationToken.None);

        Assert.Equal("the catalog", choice!.Source);
        Assert.Equal(0, search.Calls);
    }

    /// <summary>A download tagged with a compilation's name must not get the original album's cover.</summary>
    [Fact]
    public async Task Resolve_ArchiveSkipped_WhenTheAlbumIsAnotherRelease()
    {
        var calls = new List<string>();
        var http = Http(request => Picture(Square), calls);
        var song = new Song
        {
            Artist = "A", Title = "T", Album = "Now 42",
            MusicBrainzReleaseId = "rel-1", MusicBrainzAlbumTitle = "The Real Album",
            CoverArtUrlLarge = "https://deezer.example/cover.jpg",
        };

        var choice = await Resolver(http, new FixedSource(null)).ResolveAsync(song, null, CancellationToken.None);

        Assert.Equal("the catalog", choice!.Source);
        Assert.DoesNotContain(calls, url => url.Contains("coverartarchive"));
    }

    [Fact]
    public async Task Resolve_NonSquareCatalogCover_FallsThroughToTheSearch()
    {
        var http = Http(_ => Picture(VideoFrame));
        var search = new FixedSource(Square);
        var song = new Song { Artist = "A", Title = "T", CoverArtUrlLarge = "https://i.ytimg.example/maxres.jpg" };

        var choice = await Resolver(http, search).ResolveAsync(song, null, CancellationToken.None);

        Assert.Equal("a cover search", choice!.Source);
        Assert.Equal(1, search.Calls);
    }

    [Fact]
    public async Task Resolve_NothingFound_UsesTheCentreOfTheVideoFrame()
    {
        var http = Http(_ => new HttpResponseMessage(HttpStatusCode.NotFound));
        var song = new Song { Artist = "A", Title = "T" };

        var choice = await Resolver(http, new FixedSource(null)).ResolveAsync(song, VideoFrame, CancellationToken.None);

        Assert.NotNull(choice);
        Assert.False(choice.KeepsExisting);
        Assert.True(CoverImage.IsUsable(choice.Bytes, requireSquare: true));
    }

    [Fact]
    public async Task Resolve_ReplaceVideoCoversOff_KeepsTheFrame()
    {
        var http = Http(_ => new HttpResponseMessage(HttpStatusCode.NotFound));
        var song = new Song { Artist = "A", Title = "T" };

        var choice = await Resolver(http, new FixedSource(null), new MetadataSettings { ReplaceVideoCovers = false })
            .ResolveAsync(song, VideoFrame, CancellationToken.None);

        Assert.True(choice!.KeepsExisting);
    }

    [Fact]
    public async Task Resolve_SquareCoverAlreadyOnTheFile_IsKeptWithoutARewrite()
    {
        var http = Http(_ => new HttpResponseMessage(HttpStatusCode.NotFound));

        var choice = await Resolver(http, new FixedSource(null))
            .ResolveAsync(new Song { Artist = "A", Title = "T" }, Square, CancellationToken.None);

        Assert.True(choice!.KeepsExisting);
        Assert.Equal("the file itself", choice.Source);
    }

    [Fact]
    public async Task Resolve_NothingAnywhere_IsNull()
    {
        var http = Http(_ => new HttpResponseMessage(HttpStatusCode.NotFound));

        Assert.Null(await Resolver(http, new FixedSource(null))
            .ResolveAsync(new Song { Artist = "A", Title = "T" }, null, CancellationToken.None));
    }

    // ---- Apple's master -------------------------------------------------------------------

    private static string ITunesAnswer(params (string Artist, string Collection, string? Track, string Explicitness, string Art)[] rows) =>
        System.Text.Json.JsonSerializer.Serialize(new
        {
            resultCount = rows.Length,
            results = rows.Select(r => new Dictionary<string, string?>
            {
                // Apple's lookup answers carry it, and the barcode path keeps only collections.
                ["wrapperType"] = "collection",
                ["artistName"] = r.Artist, ["collectionName"] = r.Collection, ["trackName"] = r.Track,
                ["collectionExplicitness"] = r.Explicitness, ["artworkUrl100"] = r.Art,
            }),
        });

    private static HttpResponseMessage Json(string body) => new(HttpStatusCode.OK) { Content = new StringContent(body) };

    private static DownloadCoverResolver ResolverWithITunes(IHttpClientFactory http, ICoverArtSource aggregated) =>
        new(new CoverArtArchiveLookup(http, NullLogger<CoverArtArchiveLookup>.Instance),
            new CoverArtAggregator([aggregated], NullLogger<CoverArtAggregator>.Instance),
            http, TestOptions.Monitor(new MetadataSettings()), NullLogger<DownloadCoverResolver>.Instance,
            new ITunesCoverArtLookup(http, NullLogger<ITunesCoverArtLookup>.Instance));

    [Fact]
    public async Task Resolve_AppleMasterOfTheSameAlbumComesFirstAtFullSize()
    {
        var calls = new List<string>();
        var http = Http(request => request.RequestUri!.ToString() switch
        {
            var url when url.Contains("itunes.apple.com/search") => Json(ITunesAnswer(
                ("Daft Punk", "Homework", null, "notExplicit", "https://is1.example/Music/wrong/100x100bb.jpg"),
                ("Daft Punk", "Discovery", null, "notExplicit", "https://is1.example/Music/right/100x100bb.jpg"))),
            var url when url.Contains("/right/5000x5000bb") => Picture(Jpeg(3000, 3000)),
            _ => new HttpResponseMessage(HttpStatusCode.NotFound),
        }, calls);
        var song = new Song { Artist = "Daft Punk", Title = "One More Time", Album = "Discovery",
            CoverArtUrlLarge = "https://deezer.example/cover.jpg" };

        var choice = await ResolverWithITunes(http, new FixedSource(Catalog)).ResolveAsync(song, null, CancellationToken.None);

        Assert.Equal("iTunes", choice!.Source);
        Assert.Equal((3000, 3000), CoverImage.Measure(choice.Bytes));
    }

    /// <summary>A barcode the chooser found asks Apple by barcode, never by search: one lookup,
    /// then the master from the match it primed.</summary>
    [Fact]
    public async Task Resolve_ABarcodeOnTheSongPrimesApple_AndNoSearchIsMade()
    {
        var calls = new List<string>();
        var http = Http(request => request.RequestUri!.ToString() switch
        {
            var url when url.Contains("itunes.apple.com/lookup") && url.Contains("724384960629") => Json(ITunesAnswer(
                ("Daft Punk", "Discovery", null, "notExplicit", "https://is1.example/Music/disc/100x100bb.jpg"))),
            var url when url.Contains("/disc/5000x5000bb") => Picture(Jpeg(3000, 3000)),
            _ => new HttpResponseMessage(HttpStatusCode.NotFound),
        }, calls);
        var song = new Song { Artist = "Daft Punk", Title = "One More Time", Album = "Discovery", Barcode = "0724384960629",
            CoverArtUrlLarge = "https://deezer.example/cover.jpg" };

        var choice = await ResolverWithITunes(http, new FixedSource(Catalog)).ResolveAsync(song, null, CancellationToken.None);

        Assert.Equal("iTunes", choice!.Source);
        Assert.Equal((3000, 3000), CoverImage.Measure(choice.Bytes));
        Assert.Single(calls, url => url.Contains("itunes.apple.com/lookup"));
        Assert.DoesNotContain(calls, url => url.Contains("itunes.apple.com/search"));
    }

    /// <summary>Another album by the same artist is a wrong tag, not a soft picture.</summary>
    [Fact]
    public async Task Resolve_AppleIsSkippedWhenNoReleaseHasTheAlbumsName()
    {
        var http = Http(request => request.RequestUri!.ToString() switch
        {
            var url when url.Contains("itunes.apple.com/search") => Json(ITunesAnswer(
                ("Daft Punk", "Homework", null, "notExplicit", "https://is1.example/Music/wrong/100x100bb.jpg"))),
            var url when url.Contains("deezer.example") => Picture(Catalog),
            _ => Picture(Sharp),
        });
        var song = new Song { Artist = "Daft Punk", Title = "One More Time", Album = "Discovery",
            CoverArtUrlLarge = "https://deezer.example/cover.jpg" };

        var choice = await ResolverWithITunes(http, new FixedSource(null)).ResolveAsync(song, null, CancellationToken.None);

        Assert.Equal("the catalog", choice!.Source);
    }

    [Fact]
    public async Task Resolve_ASingleIsMatchedByItsSongAndAppleSingleSuffix()
    {
        var http = Http(request => request.RequestUri!.ToString() switch
        {
            var url when url.Contains("itunes.apple.com/search") && url.Contains("entity=song") => Json(ITunesAnswer(
                ("Tame Impala", "Currents", "Let It Happen", "notExplicit", "https://is1.example/Music/album/100x100bb.jpg"),
                ("Tame Impala", "Let It Happen - Single", "Let It Happen", "notExplicit", "https://is1.example/Music/single/100x100bb.jpg"))),
            var url when url.Contains("/single/5000x5000bb") => Picture(Jpeg(1400, 1400)),
            _ => new HttpResponseMessage(HttpStatusCode.NotFound),
        });
        var song = new Song { Artist = "Tame Impala", Title = "Let It Happen", Album = "Let It Happen" };

        var choice = await ResolverWithITunes(http, new FixedSource(null)).ResolveAsync(song, null, CancellationToken.None);

        Assert.Equal("iTunes", choice!.Source);
        Assert.Equal((1400, 1400), CoverImage.Measure(choice.Bytes));
    }

    [Fact]
    public async Task Resolve_ACompilationNeverAsksApple()
    {
        var calls = new List<string>();
        var http = Http(_ => Picture(Catalog), calls);
        var song = new Song { Artist = "Various Artists", Title = "T", Album = "Now 42", IsCompilation = true,
            CoverArtUrlLarge = "https://deezer.example/cover.jpg" };

        await ResolverWithITunes(http, new FixedSource(null)).ResolveAsync(song, null, CancellationToken.None);

        Assert.DoesNotContain(calls, url => url.Contains("itunes"));
    }

    // ---- Embedding and cover.jpg ----------------------------------------------------------

    [Fact]
    public void FitWithin_ShrinksAMasterAndLeavesASmallerCoverAlone()
    {
        var master = Jpeg(3000, 3000);
        Assert.Equal((1500, 1500), CoverImage.Measure(CoverImage.FitWithin(master, 1500)));
        Assert.Same(Catalog, CoverImage.FitWithin(Catalog, 1500));
    }

    [Fact]
    public void MarkAsOcto_StaysAReadableJpegAndIsRecognised()
    {
        Assert.False(CoverImage.IsOctoCover(Square));

        var marked = CoverImage.MarkAsOcto(Square);

        Assert.True(CoverImage.IsOctoCover(marked));
        Assert.Equal((600, 600), CoverImage.Measure(marked));
        Assert.Same(marked, CoverImage.MarkAsOcto(marked));
        var notJpeg = "not a jpeg"u8.ToArray();
        Assert.Same(notJpeg, CoverImage.MarkAsOcto(notJpeg));
    }

    private static string TempFolder()
    {
        var dir = Path.Combine(Path.GetTempPath(), "octo-covers-" + Guid.NewGuid().ToString("N"));
        Directory.CreateDirectory(dir);
        return dir;
    }

    [Fact]
    public void CoverFile_IsWrittenIntoANewFolderButNotAnOldOneWithoutACover()
    {
        var dir = TempFolder();
        try
        {
            Assert.True(CoverFiles.ShouldWrite(dir, Square, folderIsNew: true));
            Assert.False(CoverFiles.ShouldWrite(dir, Square, folderIsNew: false));
        }
        finally { Directory.Delete(dir, true); }
    }

    [Fact]
    public void CoverFile_OctosOwnGivesWayToALargerOneOnly()
    {
        var dir = TempFolder();
        try
        {
            CoverFiles.Write(dir, Square);
            Assert.True(CoverImage.IsOctoCover(File.ReadAllBytes(Path.Combine(dir, "cover.jpg"))));

            Assert.False(CoverFiles.ShouldWrite(dir, Jpeg(500, 500), folderIsNew: false));
            Assert.True(CoverFiles.ShouldWrite(dir, Catalog, folderIsNew: false));

            CoverFiles.Write(dir, Catalog);
            Assert.Equal((1000, 1000), CoverImage.Measure(File.ReadAllBytes(Path.Combine(dir, "cover.jpg"))));
            Assert.Single(Directory.GetFiles(dir));
        }
        finally { Directory.Delete(dir, true); }
    }

    [Fact]
    public void CoverFile_TheOwnersCoverIsNeverReplaced()
    {
        var dir = TempFolder();
        try
        {
            File.WriteAllBytes(Path.Combine(dir, "cover.jpg"), Thumbnail);
            Assert.False(CoverFiles.ShouldWrite(dir, Sharp, folderIsNew: true));

            File.Delete(Path.Combine(dir, "cover.jpg"));
            File.WriteAllBytes(Path.Combine(dir, "folder.jpg"), Thumbnail);
            Assert.False(CoverFiles.ShouldWrite(dir, Sharp, folderIsNew: true));
        }
        finally { Directory.Delete(dir, true); }
    }

    /// <summary>Apple answers about 20 searches a minute: a thousand albums were an hour. One
    /// lookup by barcode answers 40, and an album matched there is never searched.</summary>
    [Fact]
    public async Task ABarcodeLookupMatchesManyAlbumsAtOnceAndTheyAreNotSearchedAfter()
    {
        var calls = new List<string>();
        var http = Http(request => request.RequestUri!.ToString() switch
        {
            var url when url.Contains("itunes.apple.com/lookup") => Json(System.Text.Json.JsonSerializer.Serialize(new
            {
                results = new object[]
                {
                    new { wrapperType = "collection", collectionName = "Discovery", artistName = "Daft Punk", collectionExplicitness = "notExplicit", artworkUrl100 = "https://is1.example/Music/disc/100x100bb.jpg" },
                    new { wrapperType = "collection", collectionName = "Currents", artistName = "Tame Impala", collectionExplicitness = "notExplicit", artworkUrl100 = "https://is1.example/Music/curr/100x100bb.jpg" },
                    new { wrapperType = "collection", collectionName = "Some Stray Single - Single", artistName = "Someone Else", collectionExplicitness = "notExplicit", artworkUrl100 = "https://is1.example/Music/stray/100x100bb.jpg" },
                },
            })),
            var url when url.Contains("/disc/5000x5000bb") => Picture(Jpeg(3000, 3000)),
            _ => new HttpResponseMessage(HttpStatusCode.NotFound),
        }, calls);
        var itunes = new ITunesCoverArtLookup(http, NullLogger<ITunesCoverArtLookup>.Instance);

        var matched = await itunes.PrimeByBarcodeAsync(
            [("Daft Punk", "Discovery", "724384960650"), ("Tame Impala", "Currents", "602547306807"), ("Lorde", "Melodrama", "602557000000")],
            null, CancellationToken.None);
        var cover = await itunes.TryFetchAlbumMasterAsync("Daft Punk", "Discovery", "One More Time", CancellationToken.None);

        Assert.Equal(2, matched);
        Assert.Single(calls, url => url.Contains("itunes.apple.com/lookup") && url.Contains("724384960650,602547306807,602557000000"));
        Assert.DoesNotContain(calls, url => url.Contains("itunes.apple.com/search"));
        Assert.Equal((3000, 3000), CoverImage.Measure(cover!));
    }

    /// <summary>
    /// Barcodes, Apple's answers and each album's covers move together (Brandon, 2026-10-01):
    /// an album waits for its own batch, a barcode in the file's own tag needs no catalog
    /// lookup, and Apple is asked by barcode, never searched, for albums it matched.
    /// </summary>
    [Fact]
    public async Task APrimedAlbumWaitsForItsBatchAndATaggedBarcodeSkipsTheCatalog()
    {
        AlbumCoverFinder.AppleBatchIdle = TimeSpan.FromMilliseconds(50);
        var http = Http(request => request.RequestUri!.ToString() switch
        {
            var url when url.Contains("api.deezer.com/search/album") && url.Contains("Currents") =>
                Json("""{"data":[{"id":10709540,"title":"Currents","artist":{"name":"Tame Impala"},"cover_xl":"https://deezer.example/currents.jpg","nb_tracks":13,"record_type":"album"}]}"""),
            var url when url.Contains("api.deezer.com/search/album") => Json("""{"data":[]}"""),
            var url when url.Contains("api.deezer.com/album/10709540") => Json("""{"id":10709540,"upc":"602547306807"}"""),
            var url when url.Contains("itunes.apple.com/lookup") => Json(ITunesAnswer(
                ("Daft Punk", "Discovery", null, "notExplicit", "https://is1.example/Music/disc/100x100bb.jpg"),
                ("Tame Impala", "Currents", null, "notExplicit", "https://is1.example/Music/curr/100x100bb.jpg"))),
            var url when url.Contains("/5000x5000bb") => Picture(Jpeg(3000, 3000)),
            var url when url.Contains("deezer.example") => Picture(Catalog),
            // A search at Apple answers nothing here, so a cover from iTunes proves the barcode path.
            _ => new HttpResponseMessage(HttpStatusCode.NotFound),
        });
        var deezer = new Octo.Services.Metadata.DeezerMetadataService(http, TestOptions.Monitor(new MetadataSettings()),
            NullLogger<Octo.Services.Metadata.DeezerMetadataService>.Instance);
        var itunes = new ITunesCoverArtLookup(http, NullLogger<ITunesCoverArtLookup>.Instance);
        var finder = new AlbumCoverFinder(itunes, new CoverArtArchiveLookup(http, NullLogger<CoverArtArchiveLookup>.Instance),
            deezer, http, NullLogger<AlbumCoverFinder>.Instance);
        var discovery = new AlbumCoverQuery("Daft Punk", "Discovery", null, Barcode: "0724384960650");
        var currents = new AlbumCoverQuery("Tame Impala", "Currents", null);

        var priming = finder.PrimeAsync([discovery, currents], null, CancellationToken.None);
        var found = await Task.WhenAll(finder.FindAsync(discovery, CancellationToken.None), finder.FindAsync(currents, CancellationToken.None));
        await priming;

        Assert.All(found, cover => Assert.Equal(("iTunes", 3000), (cover!.Source, cover.Side)));
        Assert.Equal(["0724384960650", "724384960650", "602547306807"],
            ITunesCoverArtLookup.BarcodeForms("0724384960650").Concat(ITunesCoverArtLookup.BarcodeForms("602547306807")).ToArray());
    }

    private static byte[] Pattern(int side, bool flipped, int quality = 90)
    {
        using var image = new Image<Rgba32>(side, side);
        image.ProcessPixelRows(rows =>
        {
            for (var y = 0; y < rows.Height; y++)
            {
                var row = rows.GetRowSpan(y);
                for (var x = 0; x < row.Length; x++)
                {
                    var v = (byte)(flipped ? 255 * (side - 1 - x) / side : 255 * ((x / (side / 8) + y / (side / 8)) % 2));
                    row[x] = new Rgba32(v, v, v);
                }
            }
        });
        using var stream = new MemoryStream();
        image.SaveAsJpeg(stream, new SixLabors.ImageSharp.Formats.Jpeg.JpegEncoder { Quality = quality });
        return stream.ToArray();
    }

    [Fact]
    public void TheSameArtworkLooksAlikeAtAnySizeAndAnotherDoesNot()
    {
        var sharp = CoverImage.LooksHash(Pattern(3000, flipped: false))!.Value;
        var soft = CoverImage.LooksHash(Pattern(200, flipped: false, quality: 40))!.Value;
        var other = CoverImage.LooksHash(Pattern(3000, flipped: true))!.Value;

        Assert.True(CoverImage.LookAlike(sharp, soft));
        Assert.False(CoverImage.LookAlike(sharp, other));
        Assert.Null(CoverImage.LooksHash("not an image"u8.ToArray()));
    }

    [Fact]
    public async Task AProbeLearnsTheMastersSizeAndTakesApplesSmallCopyWithoutTheMaster()
    {
        var calls = new List<string>();
        var ranged = false;
        var http = Http(request =>
        {
            var url = request.RequestUri!.ToString();
            if (url.Contains("itunes.apple.com/search")) return Json(ITunesAnswer(
                ("Daft Punk", "Discovery", null, "notExplicit", "https://is1.example/Music/disc/100x100bb.jpg")));
            if (url.Contains("/5000x5000bb")) { ranged |= request.Headers.Range is not null; return Picture(Jpeg(3000, 3000)); }
            if (url.Contains("/320x320bb")) return Picture(Jpeg(320, 320));
            return new HttpResponseMessage(HttpStatusCode.NotFound);
        }, calls);
        var itunes = new ITunesCoverArtLookup(http, NullLogger<ITunesCoverArtLookup>.Instance);

        var probe = await itunes.TryProbeAlbumMasterAsync("Daft Punk", "Discovery", "One More Time", CancellationToken.None);

        Assert.Equal(3000, probe!.Value.Side);
        Assert.Equal((320, 320), CoverImage.Measure(probe.Value.Thumb));
        Assert.True(ranged);
    }

    [Fact]
    public async Task AppleMatchesAreRememberedOnDiskAcrossARestart()
    {
        var cache = Path.Combine(Path.GetTempPath(), "octo-itunes-" + Guid.NewGuid().ToString("N") + ".json");
        try
        {
            var calls = new List<string>();
            var http = Http(request => request.RequestUri!.ToString() switch
            {
                var url when url.Contains("itunes.apple.com/search") => Json(ITunesAnswer(
                    ("Daft Punk", "Discovery", null, "notExplicit", "https://is1.example/Music/disc/100x100bb.jpg"))),
                var url when url.Contains("/5000x5000bb") => Picture(Jpeg(3000, 3000)),
                _ => new HttpResponseMessage(HttpStatusCode.NotFound),
            }, calls);
            using (var first = new ITunesCoverArtLookup(http, NullLogger<ITunesCoverArtLookup>.Instance, cache))
                Assert.NotNull(await first.TryFetchAlbumMasterAsync("Daft Punk", "Discovery", null, CancellationToken.None));
            calls.Clear();

            using var second = new ITunesCoverArtLookup(http, NullLogger<ITunesCoverArtLookup>.Instance, cache);
            Assert.NotNull(await second.TryFetchAlbumMasterAsync("Daft Punk", "Discovery", null, CancellationToken.None));
            Assert.DoesNotContain(calls, url => url.Contains("itunes.apple.com/search"));
        }
        finally
        {
            File.Delete(cache);
        }
    }

    [Fact]
    public async Task APreviewReportsTheMastersSizeButCarriesOnlyTheSmallCopy()
    {
        var http = Http(request => request.RequestUri!.ToString() switch
        {
            var url when url.Contains("itunes.apple.com/search") => Json(ITunesAnswer(
                ("Daft Punk", "Discovery", null, "notExplicit", "https://is1.example/Music/disc/100x100bb.jpg"))),
            var url when url.Contains("/5000x5000bb") => Picture(Jpeg(3000, 3000)),
            var url when url.Contains("/320x320bb") => Picture(Jpeg(320, 320)),
            _ => new HttpResponseMessage(HttpStatusCode.NotFound),
        });
        var finder = new AlbumCoverFinder(new ITunesCoverArtLookup(http, NullLogger<ITunesCoverArtLookup>.Instance),
            new CoverArtArchiveLookup(http, NullLogger<CoverArtArchiveLookup>.Instance),
            new Octo.Services.Metadata.DeezerMetadataService(http, TestOptions.Monitor(new MetadataSettings()),
                NullLogger<Octo.Services.Metadata.DeezerMetadataService>.Instance),
            http, NullLogger<AlbumCoverFinder>.Instance);

        var found = await finder.PreviewAsync(new AlbumCoverQuery("Daft Punk", "Discovery", "One More Time"), CancellationToken.None);

        Assert.Equal(("iTunes", 3000), (found!.Source, found.Side));
        Assert.Equal((320, 320), CoverImage.Measure(found.Bytes));
    }
}
