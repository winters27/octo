using System.Diagnostics;

namespace Octo.Services.LastFm;

public interface ILastFmRadioAudioTranscoder
{
    Task TranscodeToMp3Async(Stream input, Stream output, int bitrateKbps,
        CancellationToken cancellationToken);
}

/// <summary>Normalizes mixed local FLAC and external M4A sources into one MP3
/// byte stream. A fresh process per song prevents decoder state leaking across
/// track/container boundaries; its stdout is appended to the same client response.</summary>
public sealed class FfmpegLastFmRadioAudioTranscoder : ILastFmRadioAudioTranscoder
{
    public async Task TranscodeToMp3Async(Stream input, Stream output, int bitrateKbps,
        CancellationToken cancellationToken)
    {
        using var process = new Process
        {
            StartInfo = new ProcessStartInfo
            {
                FileName = "ffmpeg",
                RedirectStandardInput = true,
                RedirectStandardOutput = true,
                RedirectStandardError = true,
                UseShellExecute = false,
                CreateNoWindow = true,
            }
        };
        foreach (var argument in new[]
        {
            "-hide_banner", "-loglevel", "error", "-i", "pipe:0", "-vn",
            "-map_metadata", "-1", "-codec:a", "libmp3lame", "-b:a", $"{bitrateKbps}k",
            "-write_xing", "0", "-id3v2_version", "0", "-f", "mp3", "pipe:1"
        }) process.StartInfo.ArgumentList.Add(argument);

        try
        {
            if (!process.Start()) throw new InvalidOperationException("ffmpeg did not start");
        }
        catch (Exception ex) when (ex is not OperationCanceledException)
        {
            throw new InvalidOperationException(
                "Continuous Radio needs ffmpeg in the Octo runtime image", ex);
        }

        try
        {
            var inputTask = Task.Run(async () =>
            {
                try { await input.CopyToAsync(process.StandardInput.BaseStream, cancellationToken); }
                finally { process.StandardInput.Close(); }
            }, cancellationToken);
            var outputTask = process.StandardOutput.BaseStream.CopyToAsync(output, cancellationToken);
            var errorTask = process.StandardError.ReadToEndAsync(cancellationToken);
            await Task.WhenAll(inputTask, outputTask);
            await process.WaitForExitAsync(cancellationToken);
            var error = await errorTask;
            if (process.ExitCode != 0)
                throw new InvalidOperationException($"ffmpeg exited {process.ExitCode}: {error.Trim()}");
        }
        catch
        {
            if (!process.HasExited)
            {
                try { process.Kill(entireProcessTree: true); }
                catch { /* best effort during disconnect/shutdown */ }
            }
            throw;
        }
    }
}
