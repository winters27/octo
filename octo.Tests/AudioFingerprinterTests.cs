using Octo.Services.Fingerprint;

namespace Octo.Tests;

/// <summary>
/// fpcalc's output is the only thing standing between a finished download and an AcoustID
/// lookup, and every failure here has to mean "no opinion" rather than "reject", because a
/// rejection deletes a file and writes a deny-list entry.
/// </summary>
public class AudioFingerprinterTests
{
    [Fact]
    public void ParseFpcalcJson_RealOutput_ReadsFingerprintAndDuration()
    {
        var result = AudioFingerprinter.ParseFpcalcJson(
            """{"duration": 253.14, "fingerprint": "AQADtEmkRUkSHR2CH1-OHT-Ko8dxHT2OH0eP4-jxHT2O"}""");

        Assert.NotNull(result);
        Assert.Equal(FingerprintOutcome.Ok, result!.Outcome);
        Assert.Equal(253, result.DecodedSeconds);
        Assert.StartsWith("AQADtEmkRUkSHR2CH1", result.Fingerprint);
    }

    /// <summary>
    /// A fingerprint over zero seconds of audio is not a fingerprint of anything. This is the
    /// shape a zero-filled FLAC produces when its header still claims a runtime.
    /// </summary>
    [Fact]
    public void ParseFpcalcJson_ZeroDuration_IsUndecodableNotOk()
    {
        var result = AudioFingerprinter.ParseFpcalcJson("""{"duration": 0, "fingerprint": "AQADtEmk"}""");

        Assert.NotNull(result);
        Assert.Equal(FingerprintOutcome.Undecodable, result!.Outcome);
    }

    /// <summary>
    /// Null means "this is not fpcalc's shape at all", which the caller must tell apart from
    /// "this file has no audio": one keeps the file, the other deletes it.
    /// </summary>
    [Theory]
    [InlineData("")]
    [InlineData("   ")]
    [InlineData("not json at all")]
    [InlineData("[1,2,3]")]
    [InlineData("""{"duration": 253.14}""")]
    [InlineData("""{"duration": 253.14, "fingerprint": ""}""")]
    public void ParseFpcalcJson_UnrecognisedOutput_ReturnsNullRatherThanThrowing(string stdout)
        => Assert.Null(AudioFingerprinter.ParseFpcalcJson(stdout));

    /// <summary>
    /// The contract under test is "verification never fails a download", not which flavour of
    /// failure a given machine produces: there is no fpcalc on a Windows dev box and no audio
    /// file at this path on any of them.
    /// </summary>
    [Fact]
    public async Task FingerprintAsync_MissingFileOrBinary_NeverThrowsIntoTheDownloadLoop()
    {
        var fingerprinter = new AudioFingerprinter(
            new Microsoft.Extensions.Logging.Abstractions.NullLogger<AudioFingerprinter>());

        var result = await fingerprinter.FingerprintAsync(
            Path.Combine(Path.GetTempPath(), "octo-no-such-file-" + Guid.NewGuid() + ".flac"),
            lengthSeconds: 120, timeoutSeconds: 5);

        Assert.NotEqual(FingerprintOutcome.Ok, result.Outcome);
    }
}
