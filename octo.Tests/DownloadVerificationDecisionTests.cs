using Octo.Models.Domain;
using Octo.Services.Fingerprint;

namespace Octo.Tests;

/// <summary>
/// Every branch here either keeps a downloaded file or deletes it and blacklists its peer for
/// thirty days, so the bias is deliberate: only a confident, disagreeing match is a mismatch.
/// Everything else keeps the file and remembers nothing.
/// </summary>
public class DownloadVerificationDecisionTests
{
    private const double Threshold = 0.85;

    private static AcoustIdLookup Ok(params AcoustIdResult[] results) => new(true, null, results);

    private static AcoustIdResult Result(double score, params AcoustIdRecording[] recordings) =>
        new(score, recordings);

    private static AcoustIdRecording Recording(string title, string artist,
        string? album = null, int? year = null) =>
        new("mbid-" + title, title, [artist], album, year);

    [Fact]
    public void Decide_ConfidentAgreeingMatch_IsConfirmed()
    {
        var verdict = DownloadVerificationService.Decide(
            Ok(Result(0.97, Recording("Teardrop", "Massive Attack", "Mezzanine", 1998))),
            "Massive Attack", "Teardrop", Threshold, tagsAuthoritative: true);

        Assert.Equal(VerificationVerdict.Confirmed, verdict.Verdict);
        Assert.Equal("Mezzanine", verdict.MatchedAlbum);
        Assert.Equal(1998, verdict.MatchedYear);
    }

    /// <summary>
    /// The most important test in the feature. A weak match must never delete a file, which is
    /// also why raising MinMatchScore makes Octo more permissive rather than stricter.
    /// </summary>
    [Fact]
    public void Decide_ScoreBelowThreshold_IsInconclusiveNotAMismatch()
    {
        var verdict = DownloadVerificationService.Decide(
            Ok(Result(0.60, Recording("Something Else Entirely", "Another Artist"))),
            "Massive Attack", "Teardrop", Threshold, tagsAuthoritative: false);

        Assert.Equal(VerificationVerdict.Inconclusive, verdict.Verdict);
        Assert.Equal("", verdict.DenyReason);
    }

    /// <summary>
    /// An obscure track has no AcoustID entry, and that is the music Soulseek exists to find.
    /// Rejecting on absence would make verification worst where the library is rarest.
    /// </summary>
    [Fact]
    public void Decide_NoResultsAtAll_IsInconclusive()
    {
        var verdict = DownloadVerificationService.Decide(
            Ok(), "Some Obscure Band", "A Song", Threshold, tagsAuthoritative: false);

        Assert.Equal(VerificationVerdict.Inconclusive, verdict.Verdict);
    }

    /// <summary>AcoustID knows the audio but has no MusicBrainz link, so nothing can be decided.</summary>
    [Fact]
    public void Decide_QualifyingResultWithNoRecordings_IsInconclusive()
    {
        var verdict = DownloadVerificationService.Decide(
            Ok(Result(0.99)), "Massive Attack", "Teardrop", Threshold, tagsAuthoritative: false);

        Assert.Equal(VerificationVerdict.Inconclusive, verdict.Verdict);
    }

    [Fact]
    public void Decide_ConfidentDifferentRecording_IsAMismatchAndNamesWhatItGot()
    {
        var verdict = DownloadVerificationService.Decide(
            Ok(Result(0.96, Recording("Picture to Burn", "Taylor Swift"))),
            "Taylor Swift", "Cruel Summer", Threshold, tagsAuthoritative: false);

        Assert.Equal(VerificationVerdict.Mismatch, verdict.Verdict);
        Assert.Contains("Picture to Burn", verdict.DenyReason);
        Assert.Equal("'Taylor Swift - Picture to Burn'", verdict.Describe());
    }

    /// <summary>
    /// One AcoustID id maps to several MusicBrainz recordings when the same audio ships on an
    /// album and a compilation. Demanding the first would reject correct files.
    /// </summary>
    [Fact]
    public void Decide_SecondRecordingAgrees_IsConfirmed()
    {
        var verdict = DownloadVerificationService.Decide(
            Ok(Result(0.98,
                Recording("Teardrop", "Massive Attack", "Now That's What I Call Music! 42"),
                Recording("Teardrop", "Massive Attack", "Mezzanine", 1998))),
            "Massive Attack", "Teardrop", Threshold, tagsAuthoritative: false);

        Assert.Equal(VerificationVerdict.Confirmed, verdict.Verdict);
    }

    /// <summary>The highest-scoring qualifying result wins, not the first one listed.</summary>
    [Fact]
    public void Decide_PrefersTheHighestScoringQualifyingResult()
    {
        var verdict = DownloadVerificationService.Decide(
            Ok(Result(0.86, Recording("Wrong Song", "Wrong Artist")),
               Result(0.99, Recording("Teardrop", "Massive Attack"))),
            "Massive Attack", "Teardrop", Threshold, tagsAuthoritative: false);

        Assert.Equal(VerificationVerdict.Confirmed, verdict.Verdict);
        Assert.Equal(0.99, verdict.Score, 3);
    }

    [Fact]
    public void ApplyTagsTo_WithTaggingOff_LeavesTheSongAlone()
    {
        var song = new Song { Title = "peer title", Artist = "peer artist", Album = "peer album" };
        DownloadVerificationService
            .Decide(Ok(Result(0.99, Recording("Teardrop", "Massive Attack", "Mezzanine", 1998))),
                "Massive Attack", "Teardrop", Threshold, tagsAuthoritative: false)
            .ApplyTagsTo(song);

        Assert.Equal("peer title", song.Title);
        Assert.Equal("peer artist", song.Artist);
        Assert.Null(song.Year);
    }

    [Fact]
    public void ApplyTagsTo_ConfirmedAndTaggingOn_OverwritesThePeersTags()
    {
        var song = new Song { Title = "peer title", Artist = "peer artist", Album = "peer album" };
        DownloadVerificationService
            .Decide(Ok(Result(0.99, Recording("Teardrop", "Massive Attack", "Mezzanine", 1998))),
                "Massive Attack", "Teardrop", Threshold, tagsAuthoritative: true)
            .ApplyTagsTo(song);

        Assert.Equal("Teardrop", song.Title);
        Assert.Equal("Massive Attack", song.Artist);
        Assert.Equal("Mezzanine", song.Album);
        Assert.Equal(1998, song.Year);
    }

    /// <summary>A mismatch must never retag; the file is about to be deleted.</summary>
    [Fact]
    public void ApplyTagsTo_Mismatch_LeavesTheSongAlone()
    {
        var song = new Song { Title = "peer title", Artist = "peer artist" };
        DownloadVerificationService
            .Decide(Ok(Result(0.96, Recording("Picture to Burn", "Taylor Swift"))),
                "Taylor Swift", "Cruel Summer", Threshold, tagsAuthoritative: true)
            .ApplyTagsTo(song);

        Assert.Equal("peer title", song.Title);
    }
}
