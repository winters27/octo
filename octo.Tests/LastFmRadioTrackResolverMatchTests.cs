using Octo.Services.LastFm;

namespace Octo.Tests;

/// <summary>
/// Radio prefers a copy you already own, so matching a Last.fm recommendation to the library
/// decides what actually plays. A false match plays a different song with nothing to say so;
/// a missed match only plays the external copy. These pin the cases that went the wrong way
/// under substring matching, and the owned tags that must keep matching.
/// </summary>
public class LastFmRadioTrackResolverMatchTests
{
    [Theory]
    [InlineData("Air", "Sexy Boy", "Airbourne", "Sexy Boy")]            // artist is a substring
    [InlineData("Massive Attack", "Intro", "Massive Attack", "Intro (Live)")]   // a different take
    [InlineData("Radiohead", "Creep", "Radiohead", "Creep (Acoustic)")]
    [InlineData("Radiohead", "", "Radiohead", "Creep")]                 // nothing to compare
    [InlineData("", "Creep", "Radiohead", "Creep")]
    [InlineData("Daft Punk", "One More Time", "Daft Punk", "One")]       // title is a substring
    [InlineData("Them", "Gloria", "M", "Gloria")]                       // "the" is a word, not letters
    public void DifferentRecordings_DoNotMatch(string wantArtist, string wantTitle, string hitArtist, string hitTitle) =>
        Assert.False(LastFmRadioTrackResolver.IsSameRecording(wantArtist, wantTitle, hitArtist, hitTitle));

    [Theory]
    [InlineData("Massive Attack", "Teardrop", "Massive Attack feat. Elizabeth Fraser", "Teardrop")]
    [InlineData("Björk", "Hyperballad", "Bjork", "Hyperballad")]
    [InlineData("The Cure", "Lullaby", "Cure", "Lullaby")]
    [InlineData("Massive Attack", "Teardrop - Remastered 2011", "Massive Attack", "Teardrop")]
    [InlineData("deadmau5", "Strobe", "deadmau5", "Strobe (Original Mix)")]
    [InlineData("Oasis", "Wonderwall", "Oasis", "Wonderwall (Remastered)")]
    [InlineData("Oasis", "Wonderwall", "Oasis", "Wonderwall (2014 Remastered Version)")]
    [InlineData("Ftown Band", "Hello", "Ftown Band", "Hello")]          // "ft" inside a word is not a separator
    [InlineData("AC/DC", "Thunderstruck", "AC/DC", "Thunderstruck")]
    public void TheSameRecording_Matches(string wantArtist, string wantTitle, string hitArtist, string hitTitle) =>
        Assert.True(LastFmRadioTrackResolver.IsSameRecording(wantArtist, wantTitle, hitArtist, hitTitle));

    [Fact]
    public void AnOriginalMix_IsNotARemix()
    {
        Assert.False(LastFmRadioTrackResolver.IsSameRecording("deadmau5", "Strobe", "deadmau5", "Strobe (Remix)"));
    }
}
