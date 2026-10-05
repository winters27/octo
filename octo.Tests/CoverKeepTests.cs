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
/// The cover a song arrives with is judged before the chain replaces it: the same art at 1000 px
/// or more stays, a small, blown-up, wrong or off-square one goes, and a smaller cover of the
/// same art stays unless the chain finds one much larger.
/// </summary>
public class CoverKeepTests
{
    public CoverKeepTests() => ITunesCoverArtLookup.AppleInterval = TimeSpan.Zero;

    /// <summary>Artwork with real edges, drawn natively at <paramref name="side"/>: the same seed
    /// is the same picture at any size, another seed another picture.</summary>
    internal static byte[] Art(int seed, int side, int? height = null)
    {
        var random = new Random(seed);
        using var image = new Image<Rgba32>(side, height ?? side, new Rgba32(20, 20, 20));
        image.Mutate(ctx =>
        {
            for (var i = 0; i < 60; i++)
            {
                var x = (float)random.NextDouble() * side;
                var y = (float)random.NextDouble() * (height ?? side);
                var w = (float)(random.NextDouble() * side / 3) + 4;
                var h = (float)(random.NextDouble() * (height ?? side) / 3) + 4;
                ctx.Fill(Color.FromRgb((byte)random.Next(256), (byte)random.Next(256), (byte)random.Next(256)),
                    new RectangleF(x, y, w, h));
            }
            // Fine lines, the lettering a real cover has and a blown-up copy loses.
            for (var i = 0; i < 400; i++)
            {
                var x = (float)random.NextDouble() * side;
                var y = (float)random.NextDouble() * (height ?? side);
                ctx.Fill(Color.White, new RectangleF(x, y, side / 300f + 1, side / 60f));
            }
        });
        using var stream = new MemoryStream();
        image.SaveAsJpeg(stream);
        return stream.ToArray();
    }

    /// <summary>The picture drawn small, then stretched: what a peer's upscaled thumbnail is.</summary>
    private static byte[] BlownUp(int seed, int from, int to)
    {
        using var image = Image.Load(Art(seed, from));
        image.Mutate(ctx => ctx.Resize(to, to, KnownResamplers.Bicubic));
        using var stream = new MemoryStream();
        image.SaveAsJpeg(stream);
        return stream.ToArray();
    }

    private static readonly byte[] Catalog = Art(1, 1000);

    // ---- The judgement ----------------------------------------------------------------------

    [Fact]
    public void Judge_TheSameArtAt1400_IsKept()
    {
        var verdict = CoverKeep.Judge(Art(1, 1400), Catalog, requireSquare: true);

        Assert.Equal(OwnCoverVerdict.Keep, verdict.Verdict);
        Assert.True(verdict.SameArt);
        Assert.Equal(1400, verdict.Side);
    }

    [Fact]
    public void Judge_TheSameArtUnder1000_IsWeighed()
    {
        var verdict = CoverKeep.Judge(Art(1, 600), Catalog, requireSquare: true);

        Assert.Equal(OwnCoverVerdict.Weigh, verdict.Verdict);
        Assert.True(verdict.SameArt);
    }

    [Fact]
    public void Judge_ADifferentPicture_IsReplacedHoweverLarge()
    {
        var verdict = CoverKeep.Judge(Art(2, 1400), Catalog, requireSquare: true);

        Assert.Equal(OwnCoverVerdict.Replace, verdict.Verdict);
        Assert.False(verdict.SameArt);
        Assert.Contains("different picture", verdict.Why);
    }

    [Fact]
    public void Judge_ASmallPictureBlownUp_IsReplaced()
    {
        var verdict = CoverKeep.Judge(BlownUp(1, 200, 1400), Catalog, requireSquare: true);

        Assert.Equal(OwnCoverVerdict.Replace, verdict.Verdict);
        Assert.Contains("blown up", verdict.Why);
    }

    [Fact]
    public void Detail_ABlownUpCopyHoldsFarLessThanTheRealArtAtTheSameSize()
    {
        var real = CoverImage.Detail(Art(1, 1400), 800)!.Value;
        var blown = CoverImage.Detail(BlownUp(1, 200, 1400), 800)!.Value;

        Assert.True(blown < real * CoverKeep.BlownUpRatio, $"blown up {blown:0.00}, real {real:0.00}");
        Assert.False(CoverKeep.BlownUp(Art(1, 1400), 1400, Catalog));
    }

    [Fact]
    public void Judge_NotSquare_IsReplacedOnlyWhileVideoCoversAreReplaced()
    {
        var wide = Art(1, 1600, 900);

        Assert.Equal(OwnCoverVerdict.Replace, CoverKeep.Judge(wide, Catalog, requireSquare: true).Verdict);
        Assert.NotEqual(OwnCoverVerdict.Replace, CoverKeep.Judge(wide, null, requireSquare: false).Verdict);
    }

    [Fact]
    public void Judge_TheArtOfTheAlbumItWasFiledAwayFrom_IsReplaced()
    {
        var verdict = CoverKeep.Judge(Art(1, 1400), Catalog, requireSquare: true, otherAlbum: true);

        Assert.Equal(OwnCoverVerdict.Replace, verdict.Verdict);
        Assert.Contains("filed under before", verdict.Why);
    }

    [Fact]
    public void Judge_TheLibraryCopysOwnPicture_IsKeptEvenWhenItIsAnotherPicture()
    {
        var verdict = CoverKeep.Judge(Art(2, 1400), Catalog, requireSquare: true, libraryCopy: true);

        Assert.Equal(OwnCoverVerdict.Keep, verdict.Verdict);
    }

    [Fact]
    public void Judge_WithNothingToCompareWith_IsWeighed()
    {
        var verdict = CoverKeep.Judge(Art(1, 1400), null, requireSquare: true);

        Assert.Equal(OwnCoverVerdict.Weigh, verdict.Verdict);
        Assert.Null(verdict.SameArt);
    }

    [Fact]
    public void Judge_ATinyThumbnail_IsReplaced() =>
        Assert.Equal(OwnCoverVerdict.Replace, CoverKeep.Judge(Art(1, 120), Catalog, requireSquare: true).Verdict);

    [Theory]
    [InlineData(1000, 800, true)]
    [InlineData(1000, 900, false)]
    [InlineData(3000, 600, true)]
    public void IsMuchLarger_NeedsAQuarterMoreSide(int found, int own, bool expected) =>
        Assert.Equal(expected, CoverKeep.IsMuchLarger(found, own));


    // ---- The resolver -----------------------------------------------------------------------

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
                if (calls is not null) lock (calls) calls.Add(request.RequestUri!.ToString());
                return answer(request);
            });
        var factory = new Mock<IHttpClientFactory>();
        factory.Setup(f => f.CreateClient(It.IsAny<string>())).Returns(() =>
            new HttpClient(handler.Object) { BaseAddress = new Uri("https://coverartarchive.org/") });
        return factory.Object;
    }

    private static HttpResponseMessage Picture(byte[] bytes) => new(HttpStatusCode.OK) { Content = new ByteArrayContent(bytes) };
    private static HttpResponseMessage Json(string body) => new(HttpStatusCode.OK) { Content = new StringContent(body) };
    private static HttpResponseMessage NotFound() => new(HttpStatusCode.NotFound);

    private static string Apple(string artist, string album, string? track = null) =>
        System.Text.Json.JsonSerializer.Serialize(new
        {
            resultCount = 1,
            results = new[]
            {
                new Dictionary<string, string?>
                {
                    ["wrapperType"] = track is null ? "collection" : "track", ["artistName"] = artist,
                    ["collectionName"] = album, ["trackName"] = track,
                    ["collectionExplicitness"] = "notExplicit", ["artworkUrl100"] = "https://is1.example/Music/m/100x100bb.jpg",
                },
            },
        });

    /// <summary>The catalog answers with <paramref name="catalog"/>, the archive with
    /// <paramref name="archive"/>, Apple with <paramref name="master"/> for the album asked
    /// (or, when <paramref name="appleAlbum"/> is another album, lists the song only there).</summary>
    private static IHttpClientFactory Sources(byte[]? catalog, byte[]? master, List<string> calls,
        byte[]? archive = null, string appleAlbum = "Dandelion") =>
        Http(request => request.RequestUri!.ToString() switch
        {
            var url when url.Contains("deezer.example") => catalog is null ? NotFound() : Picture(catalog),
            var url when url.Contains("coverartarchive") => archive is null ? NotFound() : Picture(archive),
            var url when url.Contains("itunes.apple.com/search") =>
                master is null ? Json("{\"resultCount\":0,\"results\":[]}")
                    : Json(Apple("Ella Langley", appleAlbum, url.Contains("entity=song") ? "Be Her" : null)),
            var url when url.Contains("is1.example") && master is not null => Picture(master),
            _ => NotFound(),
        }, calls);

    private static DownloadCoverResolver Resolver(IHttpClientFactory http, ICoverArtSource? search = null, MetadataSettings? settings = null) =>
        new(new CoverArtArchiveLookup(http, NullLogger<CoverArtArchiveLookup>.Instance),
            new CoverArtAggregator([search ?? new FixedSource(null)], NullLogger<CoverArtAggregator>.Instance),
            http, TestOptions.Monitor(settings ?? new MetadataSettings()), NullLogger<DownloadCoverResolver>.Instance,
            new ITunesCoverArtLookup(http, NullLogger<ITunesCoverArtLookup>.Instance));

    private static Song Song(bool catalog = true) => new()
    {
        Artist = "Ella Langley", Title = "Choosin' Texas", Album = "Dandelion",
        CoverArtUrlLarge = catalog ? "https://deezer.example/cover.jpg" : null,
    };

    /// <summary>The live case (canary, "Be Her"): Apple lists the song only on the album
    /// "Dandelion", the archive has the single at 1200 px and stopped the chain, and the file's
    /// own 1400 px cover of the same art was replaced. Now it stays, and the timeline names
    /// everything that was looked at.</summary>
    private static Song BeHer() => new()
    {
        Artist = "Ella Langley", Title = "Be Her", Album = "Be Her",
        MusicBrainzReleaseId = "rel-be-her", MusicBrainzAlbumTitle = "Be Her",
        CoverArtUrlLarge = "https://deezer.example/cover.jpg",
    };

    [Fact]
    public async Task Choose_BeHer_ItsOwn1400PxCoverStays_AndTheTimelineSaysWhatElseWasSeen()
    {
        var calls = new List<string>();
        var choice = await Resolver(Sources(Catalog, Art(9, 3000), calls, archive: Art(1, 1200)))
            .ChooseAsync(BeHer(), new CoversOnHand(Art(1, 1400)), CancellationToken.None);

        Assert.True(choice!.KeepsExisting);
        Assert.Equal("Kept the cover it came with", choice.Headline);
        Assert.Matches(@"^1400 x 1400 px, \d+ KB, the same art\. Also looked at: iTunes had none, "
            + @"Cover Art Archive 1200 px, the catalog 1000 px\.$", choice.Note);
        Assert.Contains(calls, url => url.Contains("coverartarchive"));
    }

    /// <summary>A sharper picture of another album never stands in for the release's art.</summary>
    [Fact]
    public async Task Choose_BeHer_ASmallOwnCoverGoesToTheLargestCopyOfTheSameArt_NeverToAnotherPicture()
    {
        var choice = await Resolver(Sources(Catalog, Art(9, 3000), [], archive: Art(1, 1200), appleAlbum: "Be Her"))
            .ChooseAsync(BeHer(), new CoversOnHand(Art(1, 300)), CancellationToken.None);

        Assert.Equal("Cover Art Archive", choice!.Source);
        Assert.Equal("Replaced its 300 px cover with a 1200 x 1200 px one from Cover Art Archive", choice.Headline);
        Assert.Contains("iTunes 3000 px of a different picture", choice.Note);
    }

    [Fact]
    public async Task Choose_TheSameArtAt1400_IsKeptOverSmallerCopies()
    {
        var choice = await Resolver(Sources(Catalog, Art(1, 1200), []))
            .ChooseAsync(Song(), new CoversOnHand(Art(1, 1400)), CancellationToken.None);

        Assert.True(choice!.KeepsExisting);
        Assert.StartsWith("1400 x 1400 px, ", choice.Note);
        Assert.Contains("the same art. Also looked at: iTunes 1200 px, the catalog 1000 px.", choice.Note);
    }

    /// <summary>A much larger copy of the same art is still worth having: Apple's master over a
    /// 1400 px scan.</summary>
    [Fact]
    public async Task Choose_AMuchLargerCopyOfTheSameArt_ReplacesItsOwn()
    {
        var choice = await Resolver(Sources(Catalog, Art(1, 3000), []))
            .ChooseAsync(Song(), new CoversOnHand(Art(1, 1400)), CancellationToken.None);

        Assert.Equal("iTunes", choice!.Source);
        Assert.Equal("Replaced its 1400 px cover with a 3000 x 3000 px one from iTunes", choice.Headline);
        Assert.StartsWith("iTunes has the same art much larger.", choice.Note);
    }

    [Fact]
    public async Task Choose_ASmallCoverIsReplacedByAppleAndSaysSo()
    {
        var choice = await Resolver(Sources(Catalog, Art(1, 3000), []))
            .ChooseAsync(Song(), new CoversOnHand(Art(1, 300)), CancellationToken.None);

        Assert.False(choice!.KeepsExisting);
        Assert.Equal("Replaced its 300 px cover with a 3000 x 3000 px one from iTunes", choice.Headline);
    }

    [Fact]
    public async Task Choose_ASomewhatSmallerCoverOfTheSameArtStaysWhenNothingIsMuchLarger()
    {
        var choice = await Resolver(Sources(Catalog, master: null, []))
            .ChooseAsync(Song(), new CoversOnHand(Art(1, 900)), CancellationToken.None);

        Assert.True(choice!.KeepsExisting);
        Assert.Contains("the same art; nothing found was much larger. Also looked at: iTunes had none, the catalog 1000 px.", choice.Note);
    }

    [Fact]
    public async Task Choose_ADifferentPictureIsReplacedEvenWhenLarger()
    {
        var choice = await Resolver(Sources(Catalog, master: null, []))
            .ChooseAsync(Song(), new CoversOnHand(Art(2, 1400)), CancellationToken.None);

        Assert.False(choice!.KeepsExisting);
        Assert.Equal("the catalog", choice.Source);
        Assert.Equal("Replaced its 1400 px cover with a 1000 x 1000 px one from the catalog", choice.Headline);
        Assert.StartsWith("It was a different picture from the album's cover.", choice.Note);
    }

    [Fact]
    public async Task Choose_ABlownUpCoverIsReplaced()
    {
        var choice = await Resolver(Sources(Catalog, Art(1, 3000), []))
            .ChooseAsync(Song(), new CoversOnHand(BlownUp(1, 200, 1400)), CancellationToken.None);

        Assert.Equal("iTunes", choice!.Source);
        Assert.StartsWith("It was a smaller picture blown up.", choice.Note);
    }

    /// <summary>With no strict source, a cover search by name is weak evidence: it may add a
    /// larger copy of the same art, never swap the picture.</summary>
    [Fact]
    public async Task Choose_WithoutAStrictSource_ACoverSearchMayNotSwapThePicture()
    {
        var other = await Resolver(Sources(null, master: null, []), new FixedSource(Art(3, 1200)))
            .ChooseAsync(Song(catalog: false), new CoversOnHand(Art(1, 900)), CancellationToken.None);
        var larger = await Resolver(Sources(null, master: null, []), new FixedSource(Art(1, 1200)))
            .ChooseAsync(Song(catalog: false), new CoversOnHand(Art(1, 1000)), CancellationToken.None);

        Assert.True(other!.KeepsExisting);
        Assert.Contains("a cover search 1200 px of a different picture", other.Note);
        Assert.True(larger!.KeepsExisting);
        var muchLarger = await Resolver(Sources(null, master: null, []), new FixedSource(Art(1, 1200)))
            .ChooseAsync(Song(catalog: false), new CoversOnHand(Art(1, 600)), CancellationToken.None);
        Assert.Equal("a cover search", muchLarger!.Source);
    }

    [Fact]
    public async Task Choose_NothingFoundAnywhere_KeepsWhatItHas()
    {
        var choice = await Resolver(Sources(null, master: null, []))
            .ChooseAsync(Song(catalog: false), new CoversOnHand(Art(2, 700)), CancellationToken.None);

        Assert.True(choice!.KeepsExisting);
        Assert.Contains("nothing else was found", choice.Note);
    }

    /// <summary>The tagging round's rule stands: filed under another album, its own art never
    /// outranks the chain, and stays only when the chain finds nothing.</summary>
    [Fact]
    public async Task Choose_ArtOfTheAlbumItWasFiledAwayFrom_LosesToTheChainButStaysAsALastResort()
    {
        var replaced = await Resolver(Sources(Catalog, master: null, []))
            .ChooseAsync(Song(), new CoversOnHand(Art(1, 1400), ArrivedFromOtherAlbum: true), CancellationToken.None);
        var alone = await Resolver(Sources(null, master: null, []))
            .ChooseAsync(Song(catalog: false), new CoversOnHand(Art(1, 1400), ArrivedFromOtherAlbum: true), CancellationToken.None);

        Assert.Equal("the catalog", replaced!.Source);
        Assert.Contains("filed under before", replaced.Note);
        Assert.True(alone!.KeepsExisting);
    }

    /// <summary>Better quality and the other replacements: the old copy's cover is what the
    /// album shows, so a good one goes onto the new file instead of the peer's.</summary>
    [Fact]
    public async Task Choose_AReplacementKeepsTheOldCopysGoodCover()
    {
        var oldCover = Art(4, 1500);
        var calls = new List<string>();
        var choice = await Resolver(Sources(Catalog, Art(1, 3000), calls))
            .ChooseAsync(Song(), new CoversOnHand(Art(1, 1400), LibraryCopy: oldCover), CancellationToken.None);

        Assert.False(choice!.KeepsExisting);
        Assert.Same(oldCover, choice.Bytes);
        Assert.Equal("Kept your old copy's cover", choice.Headline);
        Assert.DoesNotContain(calls, url => url.Contains("itunes"));
    }

    [Fact]
    public async Task Choose_AReplacementWhoseOldCoverIsSmall_FallsBackToTheNewFilesGoodOne()
    {
        var choice = await Resolver(Sources(Catalog, master: null, []))
            .ChooseAsync(Song(), new CoversOnHand(Art(1, 1400), LibraryCopy: Art(1, 250)), CancellationToken.None);

        Assert.True(choice!.KeepsExisting);
        Assert.Equal("Kept the cover it came with", choice.Headline);
    }

    [Fact]
    public async Task Choose_NoCoverOfItsOwn_SaysSo()
    {
        var choice = await Resolver(Sources(Catalog, Art(1, 3000), []))
            .ChooseAsync(Song(), new CoversOnHand(null), CancellationToken.None);

        Assert.Equal("iTunes", choice!.Source);
        Assert.Equal("Cover from iTunes, 3000 x 3000 px", choice.Headline);
        Assert.Equal("It came with no cover of its own. Also looked at: iTunes 3000 px, the catalog 1000 px.", choice.Note);
    }
}
