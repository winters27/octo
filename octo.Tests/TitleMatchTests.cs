using Octo.Services.Common;

namespace Octo.Tests;

public class TitleMatchTests
{
    // The case that produced the bug: three of thirteen tracks on King Von's
    // "Grandson, Vol. 1" are tagged with U+2019, every provider writes U+0027, and
    // each of the three was appended to the album a second time as a preview.
    [Theory]
    [InlineData("What It’s Like", "What It's Like")]
    [InlineData("Hoes Ain’t Shit", "Hoes Ain't Shit")]
    [InlineData("Mama’s Boy", "Mama's Boy")]
    [InlineData("Don’t Miss", "Dont Miss")]
    public void Same_TreatsApostropheFormsAsOneTitle(string tagged, string provider)
    {
        Assert.True(TitleMatch.Same(tagged, provider));
    }

    [Theory]
    [InlineData("Beyoncé", "Beyonce")]
    [InlineData("Sigur Rós", "Sigur Ros")]
    [InlineData("BLACK SKINHEAD", "Black Skinhead")]
    [InlineData("Niño", "Nino")]
    public void Same_FoldsAccentsAndCase(string left, string right)
    {
        Assert.True(TitleMatch.Same(left, right));
    }

    [Theory]
    [InlineData("Song – Remix", "Song - Remix")]      // en dash
    [InlineData("Song — Remix", "Song - Remix")]      // em dash
    [InlineData("“Heartless”", "\"Heartless\"")] // curly quotes
    [InlineData("Crazy  Story", "Crazy Story")]            // doubled space
    [InlineData("  Jimmy  ", "Jimmy")]                     // padding
    public void Same_FoldsDashesQuotesAndWhitespace(string left, string right)
    {
        Assert.True(TitleMatch.Same(left, right));
    }

    // The fold must stay narrow. These are different recordings and a listener who
    // owns one still wants to be offered the other.
    [Theory]
    [InlineData("Crazy Story", "Crazy Story, Pt. 3")]
    [InlineData("Crazy Story", "Crazy Story (remix)")]
    [InlineData("Jet", "Jetsetter")]
    [InlineData("Twin Nem", "Twin Nem (Live)")]
    public void Same_KeepsDifferentTitlesApart(string left, string right)
    {
        Assert.False(TitleMatch.Same(left, right));
    }

    [Theory]
    [InlineData(null)]
    [InlineData("")]
    [InlineData("   ")]
    public void Same_IsFalseWhenThereIsNothingToCompare(string? value)
    {
        Assert.False(TitleMatch.Same(value, value));
        Assert.Equal(string.Empty, TitleMatch.Key(value));
    }

    [Fact]
    public void ContainsEither_MatchesASuffixedTitleAgainstThePlainOne()
    {
        Assert.True(TitleMatch.ContainsEither("Jimmy (feat. Lil Durk)", "Jimmy"));
        Assert.True(TitleMatch.ContainsEither("Jimmy", "Jimmy (feat. Lil Durk)"));
        // And still across typography, which is the whole point.
        Assert.True(TitleMatch.ContainsEither("What It’s Like (feat. OMB Peezy)", "What It's Like"));
    }

    [Fact]
    public void ContainsEither_RejectsUnrelatedTitlesAndEmptyInput()
    {
        Assert.False(TitleMatch.ContainsEither("Pressure", "Heartless"));
        Assert.False(TitleMatch.ContainsEither("", "Heartless"));
        Assert.False(TitleMatch.ContainsEither("Pressure", null));
    }

    [Fact]
    public void AlbumKey_PairsArtistWithAlbumAndFoldsBoth()
    {
        Assert.Equal(TitleMatch.AlbumKey("King Von", "Grandson, Vol. 1"),
            TitleMatch.AlbumKey("king von", "Grandson, Vol. 1"));
        Assert.NotEqual(TitleMatch.AlbumKey("King Von", "Grandson"),
            TitleMatch.AlbumKey("King Von", "Grandson, Vol. 1"));
    }

    [Fact]
    public void AlbumKey_IsNullWithoutAnAlbumName()
    {
        Assert.Null(TitleMatch.AlbumKey("King Von", null));
        Assert.Null(TitleMatch.AlbumKey("King Von", "   "));
        // An album with no artist is still a key, so it can be compared at all.
        Assert.NotNull(TitleMatch.AlbumKey(null, "Grandson"));
    }

    [Fact]
    public void Key_DropsInvisibleCharactersThatArriveInsideTags()
    {
        Assert.Equal(TitleMatch.Key("Heartless"), TitleMatch.Key("Heart​less"));
        Assert.Equal(TitleMatch.Key("Heartless"), TitleMatch.Key("﻿Heartless"));
    }
}
