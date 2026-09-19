using Octo.Services.Common;

namespace Octo.Tests;

/// <summary>
/// IsExternalPlaylist decides whether Octo answers a request itself or relays it to
/// Navidrome, so getting it wrong is not a missing feature, it is Octo overwriting a
/// perfectly good upstream response with its own.
/// </summary>
public class PlaylistIdHelperTests
{
    [Theory]
    [InlineData("pl-deezer-123456")]
    [InlineData("pl-qobuz-789")]
    [InlineData("PL-DEEZER-123456")]
    [InlineData("pl-Deezer-abc-def")]
    public void IsExternalPlaylist_KnownProviderIds_AreExternal(string id)
        => Assert.True(PlaylistIdHelper.IsExternalPlaylist(id));

    /// <summary>
    /// Navidrome names its playlist cover art "pl-{id}_{unixhex}", which collides with the
    /// prefix Octo uses for its own playlist IDs. Claiming those served the Octo placeholder
    /// instead of the user's own uploaded cover, in every client that goes through Octo.
    /// </summary>
    [Theory]
    [InlineData("pl-upVzEcScsvwZRAVYjePvu5_6aa92874")]
    [InlineData("pl-pAbMYHyXyD92FjOkSNiJBn_4fa08e4a")]
    public void IsExternalPlaylist_NavidromeCoverArtIds_AreNotExternal(string id)
        => Assert.False(PlaylistIdHelper.IsExternalPlaylist(id));

    /// <summary>
    /// The case a "require a second dash" fix misses. Navidrome's IDs are base62 today, so
    /// a dash cannot appear in one, but that is a property of someone else's ID generator
    /// and not something Octo gets to depend on. Checking the PROVIDER holds either way.
    /// </summary>
    [Theory]
    [InlineData("pl-upVz-EcScsvwZRAVYjePvu5_6aa92874")]
    [InlineData("pl-1a2b3c4d-5e6f-7890-abcd-ef1234567890")]
    public void IsExternalPlaylist_NavidromeShapedIdContainingADash_IsStillNotExternal(string id)
        => Assert.False(PlaylistIdHelper.IsExternalPlaylist(id));

    [Theory]
    [InlineData("pl-")]
    [InlineData("pl--123")]
    [InlineData("pl-deezer-")]
    [InlineData("pl-deezer")]
    [InlineData("pl-spotify-123")]
    [InlineData("al-123")]
    [InlineData("")]
    [InlineData(null)]
    public void IsExternalPlaylist_MalformedOrUnknownProvider_IsNotExternal(string? id)
        => Assert.False(PlaylistIdHelper.IsExternalPlaylist(id));

    [Fact]
    public void ParsePlaylistId_SplitsOnTheFirstDashAfterTheProvider()
    {
        var (provider, externalId) = PlaylistIdHelper.ParsePlaylistId("pl-deezer-abc-def");

        Assert.Equal("deezer", provider);
        Assert.Equal("abc-def", externalId);
    }

    [Fact]
    public void ParsePlaylistId_NavidromeCoverArtId_Throws()
        => Assert.Throws<ArgumentException>(
            () => PlaylistIdHelper.ParsePlaylistId("pl-upVzEcScsvwZRAVYjePvu5_6aa92874"));

    [Fact]
    public void CreatePlaylistId_RoundTripsThroughParse()
    {
        var id = PlaylistIdHelper.CreatePlaylistId("Deezer", "123456");

        Assert.Equal("pl-deezer-123456", id);
        Assert.True(PlaylistIdHelper.IsExternalPlaylist(id));
        Assert.Equal(("deezer", "123456"), PlaylistIdHelper.ParsePlaylistId(id));
    }

    [Theory]
    [InlineData("deezer", true)]
    [InlineData("qobuz", true)]
    [InlineData("DEEZER", true)]
    [InlineData("spotify", false)]
    [InlineData("", false)]
    [InlineData(null, false)]
    public void IsKnownProvider_MatchesOnlyProvidersWeHaveAClientFor(string? provider, bool expected)
        => Assert.Equal(expected, PlaylistIdHelper.IsKnownProvider(provider));
}
