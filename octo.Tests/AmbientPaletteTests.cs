using Octo.Services.Admin;
using Octo.Services.CoverArt;
using SixLabors.ImageSharp;
using SixLabors.ImageSharp.PixelFormats;

namespace Octo.Tests;

/// <summary>
/// The dashboard's Afterglow light: a fetched song's cover read with the station covers' colour
/// rules, and the OKLCH round trip it is drawn with.
/// </summary>
public class AmbientPaletteTests
{
    private static Swatch S(string hex, float share) => new(CoverColours.Hex(hex), share);

    [Theory]
    [InlineData("#d9552b")]
    [InlineData("#1d3f8c")]
    [InlineData("#7fb35a")]
    [InlineData("#ffffff")]
    [InlineData("#000000")]
    public void FromLch_UndoesToLch(string hex)
    {
        var argb = CoverColours.Hex(hex);
        var back = CoverColours.FromLch(CoverColours.ToLch(argb));
        Assert.InRange(Math.Abs(CoverColours.R(back) - CoverColours.R(argb)), 0, 1);
        Assert.InRange(Math.Abs(CoverColours.G(back) - CoverColours.G(argb)), 0, 1);
        Assert.InRange(Math.Abs(CoverColours.B(back) - CoverColours.B(argb)), 0, 1);
    }

    /// <summary>A colour no screen shows keeps its hue and loses chroma, never wraps to another colour.</summary>
    [Fact]
    public void FromLch_OutOfGamut_KeepsHue()
    {
        var wanted = new Lch(0.7, 0.4, 145);
        var got = CoverColours.ToLch(CoverColours.FromLch(wanted));
        Assert.True(CoverColours.HueDistance(got.H, wanted.H) < 3, $"hue {got.H}");
        Assert.InRange(got.L, 0.68, 0.72);
        Assert.True(got.C < wanted.C);
    }

    /// <summary>The first light and the accent are the colour the station covers call the music's.</summary>
    [Fact]
    public void Palette_LeadsWithTheColourCoverMusicPicks()
    {
        var swatches = new[] { S("#2b1a12", 0.55f), S("#d9552b", 0.30f), S("#1d3f8c", 0.15f) };
        var music = CoverMusic.FromCovers([swatches])!;
        var palette = CoverPalette.FromSwatches(swatches)!;

        Assert.Equal(3, palette.Colors.Count);
        var lead = CoverColours.ToLch(CoverColours.Hex(palette.Colors[0]));
        var accent = CoverColours.ToLch(CoverColours.Hex(palette.Accent));
        Assert.True(CoverColours.HueDistance(lead.H, music.Hue) < 4, $"lead {lead.H} vs music {music.Hue}");
        Assert.True(CoverColours.HueDistance(accent.H, music.Hue) < 4, $"accent {accent.H} vs music {music.Hue}");
    }

    [Fact]
    public void Palette_LightsAreDistinctHues()
    {
        var palette = CoverPalette.FromSwatches([S("#d9552b", 0.4f), S("#e0662f", 0.2f), S("#1d3f8c", 0.2f), S("#7fb35a", 0.2f)])!;
        var hues = palette.Colors.Select(c => CoverColours.ToLch(CoverColours.Hex(c)).H).ToList();
        for (var i = 0; i < hues.Count; i++)
            for (var j = i + 1; j < hues.Count; j++)
                Assert.True(CoverColours.HueDistance(hues[i], hues[j]) >= 30, $"{palette.Colors[i]} vs {palette.Colors[j]}");
    }

    /// <summary>One colour still gives three lights: its neighbours on the hue circle.</summary>
    [Fact]
    public void Palette_OneColourCover_FillsWithNeighbours()
    {
        var palette = CoverPalette.FromSwatches([S("#1d3f8c", 1f)])!;
        Assert.Equal(3, palette.Colors.Distinct().Count());
    }

    /// <summary>A grey, black and white cover has no colour, so the dashboard keeps its own.</summary>
    [Fact]
    public void Palette_GreyCover_IsNull()
    {
        Assert.Null(CoverPalette.FromSwatches([S("#111111", 0.6f), S("#f0f0f0", 0.3f), S("#808080", 0.1f)]));
        Assert.Null(CoverPalette.FromSwatches([]));
        Assert.Null(CoverPalette.FromSwatches(null));
    }

    /// <summary>
    /// A night sky that is nearly black but teal (Drake's NOKIA cover, read live) is grey to the
    /// station covers, yet still lights the page: quietly, in its own teal.
    /// </summary>
    [Fact]
    public void Palette_FaintlyTintedCover_IsMutedInItsOwnHue()
    {
        var swatches = new[]
        {
            S("#0b1f1e", 0.17f), S("#e4efef", 0.17f), S("#0a1515", 0.14f), S("#2d322a", 0.12f), S("#010709", 0.12f),
        };
        Assert.Null(CoverMusic.FromCovers([swatches]));
        var palette = CoverPalette.FromSwatches(swatches)!;
        var lead = CoverColours.ToLch(CoverColours.Hex(palette.Colors[0]));
        Assert.True(CoverColours.HueDistance(lead.H, 191) < 12, $"hue {lead.H}");
        Assert.InRange(lead.C, 0.03, 0.065);
        Assert.Equal(3, palette.Colors.Distinct().Count());
        var accent = CoverColours.Hex(palette.Accent);
        Assert.True(CoverColours.ContrastRatio(accent, CoverColours.Hex("#14080e")) >= 7);
    }

    /// <summary>The accent carries dark words on it and reads as words on the dark page.</summary>
    [Theory]
    [InlineData("#d9552b")]
    [InlineData("#1d3f8c")]
    [InlineData("#f2d100")]
    [InlineData("#5a2a82")]
    public void Accent_IsReadableBothWays(string cover)
    {
        var accent = CoverColours.Hex(CoverPalette.FromSwatches([S(cover, 1f)])!.Accent);
        Assert.True(CoverColours.ContrastRatio(accent, CoverColours.Hex("#14080e")) >= 7, "dark words on the accent");
        Assert.True(CoverColours.ContrastRatio(accent, CoverColours.Hex("#0a0a0f")) >= 7, "the accent on the page");
    }

    /// <summary>An album fetched whole is one cover to choose from, and a song without a cover counts by its album.</summary>
    [Fact]
    public void NewestPerCover_OneEntryPerCover_NewestFirst()
    {
        static Octo.Models.Download.DownloadHistoryEntry E(string title, string? cover, string album = "A") =>
            new() { Artist = "Drake", Title = title, Album = album, CoverArtUrl = cover };
        var log = new[]
        {
            E("NOKIA", "https://x/nokia.jpg"),
            E("Make Them Know", "https://x/iceman.jpg"),
            E("Firm Friends", "https://x/iceman.jpg"),
            E("Digital Love", "https://x/discovery.jpg"),
            E("Don't Worry", "https://x/iceman.jpg"),
            E("No Art 1", null, "Loose"),
            E("No Art 2", null, "Loose"),
            E("Old", "https://x/old.jpg"),
        };
        var picked = Octo.Controllers.AmbientController.NewestPerCover(log, 6).Select(e => e.Title).ToList();
        Assert.Equal(["NOKIA", "Make Them Know", "Digital Love", "No Art 1", "Old"], picked);
        Assert.Equal(["NOKIA", "Make Them Know"], Octo.Controllers.AmbientController.NewestPerCover(log, 2).Select(e => e.Title));
    }

    /// <summary>End to end from a real picture, through the station covers' reading of it.</summary>
    [Fact]
    public void Palette_FromAPicture()
    {
        using var image = new Image<Rgba32>(64, 64);
        image.ProcessPixelRows(rows =>
        {
            for (var y = 0; y < rows.Height; y++)
            {
                var row = rows.GetRowSpan(y);
                for (var x = 0; x < row.Length; x++)
                    row[x] = x < 40 ? new Rgba32(0xd9, 0x55, 0x2b) : new Rgba32(0x1d, 0x3f, 0x8c);
            }
        });
        using var png = new MemoryStream();
        image.SaveAsPng(png);
        var palette = CoverPalette.FromSwatches(CoverArtService.SwatchesOf(png.ToArray()))!;
        var lead = CoverColours.ToLch(CoverColours.Hex(palette.Colors[0]));
        Assert.True(CoverColours.HueDistance(lead.H, CoverColours.ToLch(CoverColours.Hex("#d9552b")).H) < 4);
    }
}
