using System.Text.Json;
using Octo.Services.Fingerprint;

namespace Octo.Tests;

/// <summary>
/// AcoustID's response decides whether a finished download is kept or deleted, so the parser
/// has to tell three things apart that all look like "no match" from a distance: a refusal, a
/// track AcoustID has never heard of, and a confident identification of something else.
/// </summary>
public class AcoustIdLookupTests
{
    private static AcoustIdLookup Parse(string json)
    {
        using var doc = JsonDocument.Parse(json);
        return AcoustIdClient.ParseLookup(doc.RootElement);
    }

    /// <summary>
    /// The separator is load-bearing. FormUrlEncodedContent encodes a literal '+' as %2B, so a
    /// '+'-joined meta reaches AcoustID as ONE unknown token; it answers 200 with a real score
    /// and no metadata, every result has zero recordings, and the verdict is permanently
    /// Inconclusive. The feature then accepts every file forever while looking healthy, which
    /// is exactly the silent no-op this whole design is meant to avoid.
    /// </summary>
    [Fact]
    public void MetaFields_AreSpaceSeparated()
    {
        Assert.DoesNotContain('+', AcoustIdClient.MetaFields);
        Assert.Equal(["recordings", "releasegroups", "releases", "compress"],
            AcoustIdClient.MetaFields.Split(' '));
    }

    [Fact]
    public void ParseLookup_RealResponse_ReadsScoreTitleArtistAlbumAndYear()
    {
        var lookup = Parse("""
        {
          "status": "ok",
          "results": [{
            "id": "9ff43b6a-4f16-427c-93c2-92307ca505e0",
            "score": 0.97,
            "recordings": [{
              "id": "0a1b2c3d-0000-0000-0000-000000000001",
              "title": "Teardrop",
              "artists": [{ "name": "Massive Attack" }, { "name": "Elizabeth Fraser" }],
              "releasegroups": [{
                "id": "rg-1", "title": "Mezzanine", "type": "Album",
                "releases": [{ "date": { "year": 2011 } }, { "date": { "year": 1998 } }]
              }]
            }]
          }]
        }
        """);

        Assert.True(lookup.IsOk);
        var result = Assert.Single(lookup.Results);
        Assert.Equal(0.97, result.Score, 3);

        var recording = Assert.Single(result.Recordings);
        Assert.Equal("Teardrop", recording.Title);
        Assert.Equal("Massive Attack, Elizabeth Fraser", recording.ArtistCredit);
        Assert.Equal("Mezzanine", recording.AlbumTitle);
        // The earliest release, because a 2011 reissue is not the track's year.
        Assert.Equal(1998, recording.Year);
    }

    /// <summary>
    /// The Deezer trap, reproduced here: refusal arrives as HTTP 200 with an error envelope.
    /// Reading IsOk from the status code would turn every over-budget call into "no match",
    /// which this feature reads as "accept the file".
    /// </summary>
    [Fact]
    public void ParseLookup_ErrorEnvelopeInA200_IsNotOk()
    {
        var lookup = Parse("""
        {"status": "error", "error": {"code": 6, "message": "invalid API key"}}
        """);

        Assert.False(lookup.IsOk);
        Assert.Equal("invalid API key", lookup.Error);
        Assert.Empty(lookup.Results);
    }

    /// <summary>
    /// A compilation or a live album must not supply the album name and year for a studio
    /// track, so a release group carrying secondarytypes loses to a plain one.
    /// </summary>
    [Fact]
    public void ParseLookup_CompilationReleaseGroup_LosesToThePlainAlbum()
    {
        var lookup = Parse("""
        {
          "status": "ok",
          "results": [{
            "score": 0.99,
            "recordings": [{
              "id": "r1", "title": "Song", "artists": [{ "name": "Artist" }],
              "releasegroups": [
                { "title": "Now That's What I Call Music! 42", "type": "Album",
                  "secondarytypes": ["Compilation"], "releases": [{ "date": { "year": 1999 } }] },
                { "title": "The Real Album", "type": "Album",
                  "releases": [{ "date": { "year": 1997 } }] }
              ]
            }]
          }]
        }
        """);

        var recording = lookup.Results[0].Recordings[0];
        Assert.Equal("The Real Album", recording.AlbumTitle);
        Assert.Equal(1997, recording.Year);
    }

    /// <summary>
    /// AcoustID knows the audio but has no MusicBrainz link for it. Nothing can be decided,
    /// so nothing is.
    /// </summary>
    [Fact]
    public void ParseLookup_ResultWithNoRecordings_YieldsNoMetadata()
    {
        var lookup = Parse("""{"status": "ok", "results": [{"id": "x", "score": 0.99}]}""");

        Assert.True(lookup.IsOk);
        Assert.Empty(Assert.Single(lookup.Results).Recordings);
    }

    /// <summary>
    /// The most important case in the whole feature. A legitimately obscure track, which is
    /// the music Soulseek is best at and the reason Octo uses it, has no AcoustID entry at
    /// all. Rejecting on absence would make verification worst exactly where the library is
    /// rarest.
    /// </summary>
    [Fact]
    public void ParseLookup_NoResultsAtAll_IsAnOkLookupWithNothingToSay()
    {
        var lookup = Parse("""{"status": "ok", "results": []}""");

        Assert.True(lookup.IsOk);
        Assert.Empty(lookup.Results);
    }

    [Fact]
    public void ParseLookup_MalformedRecordingFields_AreSkippedNotFatal()
    {
        var lookup = Parse("""
        {"status": "ok", "results": [{"score": 0.9, "recordings": [
          {"id": "r1", "title": "Song", "artists": [{"nope": "x"}], "releasegroups": []}
        ]}]}
        """);

        var recording = lookup.Results[0].Recordings[0];
        Assert.Empty(recording.Artists);
        Assert.Null(recording.AlbumTitle);
        Assert.Null(recording.Year);
    }
}
