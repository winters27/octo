using Octo.Services.Fingerprint;

namespace Octo.Tests;

/// <summary>
/// Exact string equality would reject constantly on real data: the requested side comes from
/// Last.fm and Deezer, the matched side is MusicBrainz's canonical credit. This comparer is
/// biased toward "match" on purpose, because a false NO discards a good file AND writes a
/// deny-list entry that stands for thirty days.
/// </summary>
public class TrackMatchComparerTests
{
    [Theory]
    [InlineData("Björk", "Bjork")]
    [InlineData("Simon & Garfunkel", "Simon and Garfunkel")]
    [InlineData("Don't Stop Me Now", "Dont Stop Me Now")]
    [InlineData("Blue (Da Ba Dee)", "Blue [Da Ba Dee]")]
    public void Normalize_FoldsDiacriticsAmpersandsAndPunctuation(string a, string b)
        => Assert.Equal(TrackMatchComparer.Normalize(a), TrackMatchComparer.Normalize(b));

    /// <summary>Last.fm appends remaster and video tails that MusicBrainz does not carry.</summary>
    [Theory]
    [InlineData("Teardrop - Remastered 2011", "Teardrop")]
    [InlineData("Karma Police (Official Video)", "Karma Police")]
    [InlineData("Paranoid Android", "Paranoid Android")]
    public void TitleMatches_RemasterAndVideoTails_AreTheSameRecording(string requested, string matched)
        => Assert.True(TrackMatchComparer.TitleMatches(requested, matched));

    /// <summary>
    /// The case the length check cannot catch. "Group Four" and "Group Four (Security Forces
    /// dub)" are two seconds apart, so only the variant marker separates them.
    /// </summary>
    [Theory]
    [InlineData("Group Four", "Group Four (Security Forces dub)")]
    [InlineData("Teardrop", "Teardrop (Mad Professor mix)")]
    [InlineData("Creep", "Creep (Live at Glastonbury)")]
    public void TitleMatches_UnrequestedVariantMarker_IsADifferentRecording(string requested, string matched)
        => Assert.False(TrackMatchComparer.TitleMatches(requested, matched));

    /// <summary>Asking for the remix and getting the remix is a match, not a mismatch.</summary>
    [Fact]
    public void TitleMatches_RequestedVariant_MatchesTheVariantRecording()
        => Assert.True(TrackMatchComparer.TitleMatches(
            "Teardrop (Mad Professor mix)", "Teardrop (Mad Professor mix)"));

    [Theory]
    [InlineData("Group Four", "Four Seasons")]
    [InlineData("Hotline Bling", "Marvins Room")]
    public void TitleMatches_DifferentSong_IsAMismatch(string requested, string matched)
        => Assert.False(TrackMatchComparer.TitleMatches(requested, matched));

    /// <summary>
    /// Without a length floor on the prefix rule, "Go" matches "Gold" and a correct file is
    /// discarded for being a different song.
    /// </summary>
    [Fact]
    public void TitleMatches_ShortTitleIsNotExtendedIntoAnotherWord()
        => Assert.False(TrackMatchComparer.TitleMatches("Go", "Gold"));

    /// <summary>Absence of evidence is not a mismatch; there is nothing to judge on.</summary>
    [Theory]
    [InlineData(null, "Teardrop")]
    [InlineData("Teardrop", "")]
    public void TitleMatches_EitherSideEmpty_Passes(string? requested, string? matched)
        => Assert.True(TrackMatchComparer.TitleMatches(requested, matched));

    /// <summary>MusicBrainz credits features that Last.fm leaves off, and sometimes the reverse.</summary>
    [Theory]
    [InlineData("Massive Attack", "Massive Attack feat. Elizabeth Fraser")]
    [InlineData("Massive Attack feat. Elizabeth Fraser", "Massive Attack")]
    [InlineData("The Beatles", "Beatles")]
    public void ArtistMatches_NestedAndArticleCredits_AreTheSameArtist(string requested, string credited)
        => Assert.True(TrackMatchComparer.ArtistMatches(requested, credited, null));

    [Fact]
    public void ArtistMatches_OneOfSeveralCredits_IsEnough()
        => Assert.True(TrackMatchComparer.ArtistMatches(
            "Elizabeth Fraser", "Massive Attack, Elizabeth Fraser",
            ["Massive Attack", "Elizabeth Fraser"]));

    [Fact]
    public void ArtistMatches_DifferentArtist_IsAMismatch()
        => Assert.False(TrackMatchComparer.ArtistMatches("Drake", "Kendrick Lamar", null));
}
