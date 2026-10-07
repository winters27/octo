using Octo.Services.LastFm;

namespace Octo.Tests;

public class LastFmSearchRankingTests
{
    private static LastFmService.SimilarTrack T(string artist, string title, long? listeners) =>
        new(artist, title, 1.0, Listeners: listeners);

    private static List<(string Artist, string Title)> Rank(string query, params LastFmService.SimilarTrack[] tracks) =>
        LastFmSearchRanking.Rank(query, tracks).Select(t => (t.Artist, t.Title)).ToList();

    [Fact]
    public void Adele_PutsTheBiggestHitsFirst_NotLastFmsFuzzyOrder()
    {
        // Last.fm's own order for "adele", with the listener counts measured on 2026-10-06.
        var rows = Rank("adele",
            T("Adele", "Skyfall", 1_430_000),
            T("<Unknown>", "Adele - Hello", 346),
            T("Adele", "Chasing Pavements", 1_650_000),
            T("Adele", "Someone Like You", 2_200_000),
            T("Adele", "Rolling in the Deep", 2_680_000));

        Assert.Equal(
            [("Adele", "Rolling in the Deep"), ("Adele", "Someone Like You"), ("Adele", "Chasing Pavements"),
             ("Adele", "Skyfall"), ("<Unknown>", "Adele - Hello")],
            rows);
    }

    [Fact]
    public void Hello_PutsAdeleFirst()
    {
        var rows = Rank("hello",
            T("Lionel Richie", "Hello", 900_000),
            T("Adele", "Hello", 1_070_000),
            T("Martin Solveig", "Hello", 200_000));

        Assert.Equal(("Adele", "Hello"), rows[0]);
    }

    [Fact]
    public void SameListeners_KeepLastFmsOrder_AndRowsWithoutACountGoLast()
    {
        var rows = Rank("song",
            T("A", "No count", null),
            T("B", "First", 100),
            T("C", "Second", 100),
            T("D", "Bigger", 500));

        Assert.Equal([("D", "Bigger"), ("B", "First"), ("C", "Second"), ("A", "No count")], rows);
    }

    [Fact]
    public void Ranking_IsStable_OverManyEqualRows()
    {
        var input = Enumerable.Range(0, 40).Select(i => T("Artist", $"Song {i}", 10)).ToArray();

        Assert.Equal(input.Select(t => t.Title), LastFmSearchRanking.Rank("artist", input).Select(t => t.Title));
    }

    [Fact]
    public void ACyrillicRow_IsDropped_ForALatinQuery()
    {
        var rows = Rank("adele hello",
            T("Adele", "Hello", 1_070_000),
            T("Адель", "Привет", 3_000_000));

        Assert.Equal([("Adele", "Hello")], rows);
    }

    [Theory]
    [InlineData("田中", "こんにちは")]       // CJK and kana
    [InlineData("العربي", "مرحبا")] // Arabic
    [InlineData("안녕", "하세요")]                  // Hangul
    [InlineData("שלום", "הי")]            // Hebrew
    [InlineData("สวัสดี", "ครับ")] // Thai
    [InlineData("Αλέξης", "Τραγούδι")] // Greek
    public void OtherScripts_AreDropped_ForALatinQuery(string artist, string title)
    {
        var rows = Rank("hello", T("Adele", "Hello", 10), T(artist, title, 5_000_000));

        Assert.Equal([("Adele", "Hello")], rows);
    }

    [Fact]
    public void ALatinTitleWithAGreekLetter_IsKept()
    {
        // Damso's Ipseite is titled with Greek letters in front of French words.
        var rows = Rank("damso",
            T("Damso", "Θ. Macarena", 90_000),
            T("Damso", "Ε. Signaler", 80_000),
            T("Damso", "Γ. Mosaïque solitaire", 70_000));

        Assert.Equal(3, rows.Count);
        Assert.Equal("Θ. Macarena", rows[0].Title);
    }

    [Fact]
    public void ACyrillicQuery_KeepsCyrillicRows()
    {
        var rows = Rank("привет",
            T("Адель", "Привет", 3_000_000),
            T("Adele", "Hello", 1_070_000));

        Assert.Equal(2, rows.Count);
        Assert.Equal("Привет", rows[0].Title);
    }

    [Theory]
    [InlineData("Hello - Live")]
    [InlineData("Hello (Live at the Royal Albert Hall)")]
    [InlineData("Hello (2011 Remaster)")]
    [InlineData("Hello - Remastered 2011")]
    [InlineData("Hello [Mono Version]")]
    [InlineData("Hello (Radio Edit)")]
    [InlineData("Hello (Stereo)")]
    [InlineData("Hello (Demo)")]
    [InlineData("Hello (Acoustic)")]
    [InlineData("Hello (Live) (Remastered)")]
    public void VersionsOfOneSong_AreOneRow_TheMostListened(string variant)
    {
        var rows = LastFmSearchRanking.Rank("hello", [T("Adele", variant, 50_000), T("Adele", "Hello", 1_000_000)]);

        var only = Assert.Single(rows);
        Assert.Equal("Hello", only.Title);

        // The variant wins when it is the one people listen to.
        var reversed = LastFmSearchRanking.Rank("hello", [T("Adele", variant, 3_000_000), T("Adele", "Hello", 1_000_000)]);
        Assert.Equal(variant, Assert.Single(reversed).Title);
    }

    [Theory]
    [InlineData("Hello (Skrillex Remix)")]
    [InlineData("Hello - Skrillex Remix")]
    [InlineData("Hello (Extended Mix)")]
    [InlineData("Hello (feat. Someone)")]
    [InlineData("Hello (Live Remix)")]
    [InlineData("Hello (Sped Up)")]
    [InlineData("Hello (Instrumental)")]
    public void RemixesAndFeatures_AreOtherSongs_AndStay(string other)
    {
        var rows = LastFmSearchRanking.Rank("hello", [T("Adele", "Hello", 1_000_000), T("Adele", other, 50_000)]);

        Assert.Equal(2, rows.Count);
    }

    [Fact]
    public void TheSameTitleByTwoArtists_IsTwoRows()
    {
        var rows = Rank("hello",
            T("Adele", "Hello", 1_070_000), T("Adele", "Hello (Live)", 90_000), T("Lionel Richie", "Hello (Live)", 80_000));

        Assert.Equal([("Adele", "Hello"), ("Lionel Richie", "Hello (Live)")], rows);
    }

    [Fact]
    public void Empty_StaysEmpty() => Assert.Empty(LastFmSearchRanking.Rank("x", []));
}
