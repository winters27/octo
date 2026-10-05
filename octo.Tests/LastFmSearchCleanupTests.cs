using Octo.Services.LastFm;

namespace Octo.Tests;

public class LastFmSearchCleanupTests
{
    private static LastFmService.SimilarTrack T(string artist, string title, long listeners) =>
        new(artist, title, 1.0, Listeners: listeners);

    private static List<(string Artist, string Title)> Clean(params LastFmService.SimilarTrack[] tracks) =>
        LastFmSearchCleanup.Clean(tracks).Select(t => (t.Artist, t.Title)).ToList();

    [Fact]
    public void KavinskyNightcall_AsLastFmAnswersIt_IsTheRealSongOnce()
    {
        // Last.fm's answer for "Kavinsky Nightcall" on 2026-09-30, in its order.
        var rows = Clean(
            T("Kavinsky", "Kavinsky - Nightcall", 1324),
            T("Kavinsky Nightcall", "Nightcall", 44),
            T("Kavinsky", "Nightcall", 1257699),
            T("Record Makers", "Kavinsky - Nightcall", 252),
            T("[unknown]", "Kavinsky - Nightcall", 131),
            T("<Unknown>", "Kavinsky - Nightcall", 129),
            T("Kavinsky & Lovefoxxx", "Nightcall", 114928),
            T("Kavinsky \u2013 Nightcall", "Nightcall", 73));

        Assert.Equal(("Kavinsky", "Nightcall"), rows[0]);
        Assert.Equal(1, rows.Count(r => r == ("Kavinsky", "Nightcall")));
        Assert.DoesNotContain(rows, r => r.Artist.Contains("Nightcall") || r.Title.StartsWith("Kavinsky"));
        Assert.DoesNotContain(rows, r => r.Artist is "[unknown]" or "<Unknown>" or "Record Makers");
    }

    [Fact]
    public void KanyeWestStronger_TheArtistInTheTitle_IsTakenOut_AndTheMostListenedSpellingKept()
    {
        var rows = Clean(
            T("Kanye West", "Kanye West - Stronger", 2342),
            T("Kanye West", "Kanye west -stronger", 1143),
            T("Kanye West", "Kanye West-Stronger", 370),
            T("Kanye", "Kanye West - Stronger", 56),
            T("Kanye West", "Stronger", 2977765));

        Assert.Equal([("Kanye West", "Stronger")], rows);
    }

    [Fact]
    public void KanyeWestStronger_TheRowsThatGotThroughAtFirst_AreGoneToo()
    {
        // More of Last.fm's answer for "Kanye West Stronger" on 2026-09-30.
        var rows = Clean(
            T("Kanye West", "Stronger", 2977765),
            T("Jeanne29570", "Kanye West Stronger", 25),
            T("Kanye West \u2013 Stronger", "!", 79),
            T("Kanye West \u2013 Stronger", "Stronger", 56),
            T("eleonoraalili", "Kanye West   Stronger", 47),
            T("Kanye West", "Kanye West   Stronger", 48),
            T("[unknown]", "Kanye West   Stronger", 30),
            T("<Unknown>", "Kanye West \u2013 Stronger", 29));

        Assert.Equal([("Kanye West", "Stronger")], rows);
    }

    [Fact]
    public void AnArtistFieldNamingAnotherArtistAndADash_IsDropped_WhenFarLessListened()
    {
        var rows = Clean(
            T("Kavinsky", "Nightcall", 1257699),
            T("Kavinsky - Nightcall", "Drive", 202),
            T("Kavinsky-Nightcall", "Instrumental-Drive", 32));

        Assert.Equal([("Kavinsky", "Nightcall")], rows);
    }

    [Fact]
    public void ANameWithAHyphen_IsNotDropped_WhenItIsTheBiggerArtist()
    {
        var rows = Clean(
            T("Jay", "Some Song", 900),
            T("Jay-Z", "99 Problems", 1500000));

        Assert.Contains(("Jay-Z", "99 Problems"), rows);
    }

    [Fact]
    public void ATitleStartingWithAnArtistsName_IsOnlySplitAtSpacesOnEvidence()
    {
        // "Queen Bitch" is David Bowie's even though Queen answers the same search.
        var rows = Clean(
            T("Queen", "Bohemian Rhapsody", 3000000),
            T("David Bowie", "Queen Bitch", 400000));

        Assert.Contains(("David Bowie", "Queen Bitch"), rows);
    }

    [Fact]
    public void AHyphenInTheArtist_IsReadWhole()
    {
        Assert.Equal([("Jay-Z", "99 Problems")], Clean(T("Jay-Z", "Jay-Z - 99 Problems", 10)));
    }

    [Fact]
    public void TitlesAndArtistsThatOnlyLookAlike_AreLeftAsTheyAre()
    {
        var rows = Clean(
            T("Earth, Wind & Fire", "Fire", 500000),
            T("Radiohead", "Creep - Acoustic", 90000),
            T("The Weeknd", "Blinding Lights", 3000000));

        Assert.Equal([("Earth, Wind & Fire", "Fire"), ("Radiohead", "Creep - Acoustic"), ("The Weeknd", "Blinding Lights")], rows);
    }

    [Fact]
    public void AnotherArtistAtTheFrontOfATitle_NeedsTheirOwnRowOfThatSong()
    {
        // A search for "creep" answers with a band called Creep too; Radiohead keeps its song.
        var rows = Clean(
            T("Radiohead", "Creep", 4234650),
            T("Creep", "You", 9000),
            T("Radiohead", "Creep - Acoustic", 90000));

        Assert.Contains(("Radiohead", "Creep - Acoustic"), rows);
        Assert.DoesNotContain(("Creep", "Acoustic"), rows);
    }

    [Fact]
    public void AnArtistWithTheTitleOnTheEnd_IsOnlyTrimmedWhenTheAnswerNamesTheShorterArtist()
    {
        Assert.Equal([("Kavinsky Nightcall", "Nightcall")], Clean(T("Kavinsky Nightcall", "Nightcall", 44)));
    }

    [Fact]
    public void ARowWithNoArtist_TakesItFromTheTitle_OrIsDropped()
    {
        Assert.Equal([("Daft Punk", "One More Time")], Clean(T("[unknown]", "Daft Punk - One More Time", 20)));
        Assert.Empty(Clean(T("<Unknown>", "Track 01", 20)));
    }
}
