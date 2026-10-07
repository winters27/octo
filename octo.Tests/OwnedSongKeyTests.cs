using Octo.Services.Common;

namespace Octo.Tests;

/// <summary>
/// An outside song already in the library must be recognised as owned, however the outside
/// source writes it. The library side below is read from real tags (MusicBrainz spellings kept
/// by Lidarr); the outside side is how Last.fm / Deezer commonly write the same song.
/// </summary>
public class OwnedSongKeyTests
{
    [Theory]
    [InlineData("Damso", "Θ. Macarena", "Damso", "Θ. Macarena")]
    [InlineData("Damso", "Θ. Macarena", "DAMSO", "Θ. MACARENA")]
    [InlineData("Damso", "Ε. Signaler", "Damso", "Ε. Signaler")]
    [InlineData("Damso", "Qui m’a demandé", "Damso", "Qui m'a demandé")]
    [InlineData("Damso", "T’es mon DEL", "Damso", "T'es mon DEL")]
    [InlineData("Damso", "Frère", "Damso", "Frere")]
    [InlineData("SZA feat. James Fauntleroy", "Wavy (Interlude)", "SZA", "Wavy (Interlude)")]
    [InlineData("SZA feat. James Fauntleroy", "Wavy (Interlude)", "SZA", "Wavy")]
    [InlineData("Dr. Dre • Xzibit • Tray-Dee", "Lolo (Intro)", "Dr. Dre", "Lolo (Intro)")]
    [InlineData("Dr. Dre • Eddie Griffin", "Ed-Ucation (Skit)", "Dr. Dre", "Ed‐ucation (Skit)")]
    [InlineData("Kendrick Lamar", "Savior (Interlude)", "Kendrick Lamar", "Savior (Interlude)")]
    [InlineData("Queen", "Bohemian Rhapsody", "Queen", "Bohemian Rhapsody - Remastered 2011")]
    [InlineData("Adele", "Hello", "Adele", "Hello (feat. Someone)")]
    public void TheSameSong_GetsTheSameKey(string ownedArtist, string ownedTitle, string artist, string title)
    {
        Assert.Equal(SongIdentity.MatchKey(ownedArtist, ownedTitle), SongIdentity.MatchKey(artist, title));
    }
}
