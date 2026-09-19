using System.Diagnostics;
using System.Text.Json;

namespace Octo.Services.Fingerprint;

public enum FingerprintOutcome
{
    /// <summary>fpcalc produced a fingerprint.</summary>
    Ok,

    /// <summary>
    /// fpcalc is not in the image, timed out, or failed in a way that says nothing about
    /// the FILE. Verification must treat this as "no opinion" and keep the download.
    /// </summary>
    Unavailable,

    /// <summary>
    /// fpcalc started, read the file, and could not decode audio out of it. That is a fact
    /// about the file: a zero-filled or truncated FLAC that TagLib still reports a duration
    /// for decodes to nothing here.
    /// </summary>
    Undecodable,
}

public sealed record FingerprintResult(FingerprintOutcome Outcome, string? Fingerprint, int DecodedSeconds);

/// <summary>
/// Chromaprint fingerprints, via the fpcalc binary.
///
/// Modelled on LastFmRadioAudioTranscoder's ffmpeg shell-out, with one deliberate
/// difference: that one translates a missing binary into a thrown exception, because radio
/// cannot work without ffmpeg. Here a missing binary must NEVER fail a download that has
/// already succeeded. Every failure path is "no opinion", and the caller keeps the file.
/// </summary>
public sealed class AudioFingerprinter
{
    private static readonly FingerprintResult Missing =
        new(FingerprintOutcome.Unavailable, null, 0);

    private readonly ILogger<AudioFingerprinter> _logger;

    /// <summary>Latched so a misbuilt image costs one log line, not a spawned process per download.</summary>
    private volatile bool _binaryMissing;

    public AudioFingerprinter(ILogger<AudioFingerprinter> logger) => _logger = logger;

    /// <summary>
    /// How much audio to fingerprint and how long fpcalc may take both come from settings:
    /// the first trades decode time against nothing much, and the second depends entirely on
    /// how slow the user's storage is.
    /// </summary>
    public async Task<FingerprintResult> FingerprintAsync(string path, int lengthSeconds, int timeoutSeconds)
    {
        if (_binaryMissing) return Missing;

        using var process = new Process
        {
            StartInfo = new ProcessStartInfo
            {
                FileName = "fpcalc",
                WorkingDirectory = Path.GetTempPath(),
                RedirectStandardInput = false,
                RedirectStandardOutput = true,
                RedirectStandardError = true,
                UseShellExecute = false,
                CreateNoWindow = true,
            }
        };
        process.StartInfo.ArgumentList.Add("-json");
        process.StartInfo.ArgumentList.Add("-length");
        process.StartInfo.ArgumentList.Add(lengthSeconds.ToString());
        process.StartInfo.ArgumentList.Add(path);

        try
        {
            if (!process.Start()) throw new InvalidOperationException("fpcalc did not start");
        }
        catch (Exception ex)
        {
            _binaryMissing = true;
            _logger.LogWarning(
                "fpcalc is not in this image, so download verification can only ever accept: {M}. "
                + "Rebuild with libchromaprint-tools installed.", ex.Message);
            return Missing;
        }

        // Deliberately not linked to any caller token. Verification runs between a finished
        // transfer and the file joining the library, and a caller who has already left must
        // not be able to wave an unchecked file through.
        using var cts = new CancellationTokenSource(TimeSpan.FromSeconds(timeoutSeconds));
        string stdout;
        string stderr;
        try
        {
            var outputTask = process.StandardOutput.ReadToEndAsync(cts.Token);
            var errorTask = process.StandardError.ReadToEndAsync(cts.Token);
            stdout = await outputTask;
            stderr = await errorTask;
            await process.WaitForExitAsync(cts.Token);
        }
        catch (Exception ex)
        {
            if (!process.HasExited)
            {
                try { process.Kill(entireProcessTree: true); }
                catch { /* best effort */ }
            }
            _logger.LogWarning("fpcalc did not finish for {Path}: {M}", path, ex.Message);
            return Missing;
        }

        var parsed = ParseFpcalcJson(stdout);
        if (parsed is not null) return parsed;

        // Exit 0 with nothing parseable is a shape we do not understand: no opinion.
        if (process.ExitCode == 0)
        {
            _logger.LogWarning("fpcalc produced no usable JSON for {Path}", path);
            return Missing;
        }

        // Started, read the file, refused it. The file is the problem. This is the branch a
        // zero-filled FLAC lands in when its header still claims a runtime.
        _logger.LogWarning("fpcalc could not decode {Path} (exit {Code}): {Err}",
            path, process.ExitCode, stderr.Trim());
        return new FingerprintResult(FingerprintOutcome.Undecodable, null, 0);
    }

    /// <summary>
    /// fpcalc -json writes one object to stdout: {"duration": 253.14, "fingerprint": "AQADt..."}.
    /// Simpler than the transcoder's stderr scraping, which is why -json is used.
    ///
    /// Returns null when the output is not fpcalc's shape at all, so the caller can tell
    /// "I do not understand this" apart from "this file has no audio".
    /// </summary>
    internal static FingerprintResult? ParseFpcalcJson(string? stdout)
    {
        if (string.IsNullOrWhiteSpace(stdout)) return null;
        try
        {
            using var doc = JsonDocument.Parse(stdout);
            var root = doc.RootElement;
            if (root.ValueKind != JsonValueKind.Object) return null;

            var fingerprint = root.TryGetProperty("fingerprint", out var fp) ? fp.GetString() : null;
            if (string.IsNullOrWhiteSpace(fingerprint)) return null;

            var seconds = root.TryGetProperty("duration", out var d) && d.ValueKind == JsonValueKind.Number
                ? (int)Math.Round(d.GetDouble())
                : 0;

            // A fingerprint over zero seconds of audio is not a fingerprint of anything.
            if (seconds <= 0) return new FingerprintResult(FingerprintOutcome.Undecodable, null, 0);

            return new FingerprintResult(FingerprintOutcome.Ok, fingerprint, seconds);
        }
        catch (JsonException) { return null; }
    }
}
