using System.Text.Json;
using Microsoft.Extensions.Options;
using Octo.Models.Settings;

namespace Octo.Services.Library;

public sealed record QuarantineResult(bool Moved, string? QuarantinePath, string? Error);

/// <summary>What is written next to a quarantined file so a restore works without the journal.</summary>
public sealed record QuarantineManifest(
    string OriginalPath, string NavidromeId, string Action, string Username, DateTime AtUtc);

/// <summary>
/// Moves a verified file out of the library instead of deleting it.
///
/// There is no File.Delete anywhere in library actions except the retention sweep. These
/// actions delete files Octo did NOT create, on one tap in a music client with no confirmation
/// dialog, and the whole point of the feature is that the user is correcting a mistake, which
/// means they can make one.
///
/// This is where DiscardRejectedDownload's philosophy is reconciled rather than contradicted.
/// That guard refuses to delete anything created before the attempt started, because its input
/// is a GUESS: ResolveLocalPath matches on leaf name and approximate size. Here the user has
/// pointed at a specific track, so a creation-time guard would break the feature's purpose. The
/// invariant underneath still holds: never act on a path you inferred rather than proved. That
/// is discharged by the resolver's byte-size check, which is strictly stronger than
/// DiscardRejectedDownload's own approximate-size matching, plus quarantine instead of delete,
/// which makes getting it wrong recoverable rather than terminal.
/// </summary>
public sealed class LibraryActionQuarantine
{
    private const string ManifestSuffix = ".octo-action.json";

    private readonly IOptionsMonitor<LibraryActionSettings> _settings;
    private readonly ILogger<LibraryActionQuarantine> _logger;

    public LibraryActionQuarantine(IOptionsMonitor<LibraryActionSettings> settings,
        ILogger<LibraryActionQuarantine> logger)
    {
        _settings = settings;
        _logger = logger;
    }

    public string RootFor(string musicRoot) =>
        Path.Combine(musicRoot, _settings.CurrentValue.EffectiveQuarantineDirectory);

    /// <summary>
    /// Move a verified file into quarantine, preserving its layout underneath so a restore is a
    /// straight copy back.
    /// </summary>
    public QuarantineResult Move(ResolvedSongFile file, string musicRoot, LibraryAction action, string username)
    {
        try
        {
            var relative = Path.GetRelativePath(musicRoot, file.AbsolutePath);
            if (relative.StartsWith("..", StringComparison.Ordinal))
                return new QuarantineResult(false, null, "the file is not under the music root");

            var destination = Path.Combine(RootFor(musicRoot),
                DateTime.UtcNow.ToString("yyyy-MM-dd"), relative);
            Directory.CreateDirectory(Path.GetDirectoryName(destination)!);
            destination = Unique(destination);

            try
            {
                File.Move(file.AbsolutePath, destination);
            }
            catch (IOException)
            {
                // Across devices Move fails, so copy, confirm the length, then remove. The
                // length check is what stops a truncated copy from turning into a delete.
                File.Copy(file.AbsolutePath, destination, overwrite: false);
                if (new FileInfo(destination).Length != file.SizeBytes)
                {
                    TryDelete(destination);
                    return new QuarantineResult(false, null, "the copy did not match the original's size");
                }
                File.Delete(file.AbsolutePath);
            }

            WriteManifest(destination, new QuarantineManifest(
                file.AbsolutePath, file.NavidromeId, action.ToString(), username, DateTime.UtcNow));

            _logger.LogInformation("Library action {Action} by {User}: quarantined {From} -> {To}",
                action, username, file.AbsolutePath, destination);
            return new QuarantineResult(true, destination, null);
        }
        catch (Exception ex)
        {
            _logger.LogWarning("Library action could not quarantine {Path}: {M}", file.AbsolutePath, ex.Message);
            return new QuarantineResult(false, null, ex.Message);
        }
    }

    /// <summary>Put a quarantined file back where it came from.</summary>
    public QuarantineResult Restore(string quarantinePath)
    {
        try
        {
            if (!File.Exists(quarantinePath))
                return new QuarantineResult(false, null, "the quarantined file is gone");

            var manifest = ReadManifest(quarantinePath);
            if (manifest is null)
                return new QuarantineResult(false, null, "no manifest, so the original path is unknown");

            Directory.CreateDirectory(Path.GetDirectoryName(manifest.OriginalPath)!);
            if (File.Exists(manifest.OriginalPath))
                return new QuarantineResult(false, null, "something already exists at the original path");

            File.Move(quarantinePath, manifest.OriginalPath);
            TryDelete(quarantinePath + ManifestSuffix);

            _logger.LogInformation("Library action restored {Path}", manifest.OriginalPath);
            return new QuarantineResult(true, manifest.OriginalPath, null);
        }
        catch (Exception ex)
        {
            _logger.LogWarning("Library action could not restore {Path}: {M}", quarantinePath, ex.Message);
            return new QuarantineResult(false, null, ex.Message);
        }
    }

    /// <summary>
    /// The only code in this feature that really deletes, and it goes on age rather than on
    /// which action produced the file. Retention 0 means never sweep.
    /// </summary>
    public int Sweep(string musicRoot)
    {
        var days = _settings.CurrentValue.EffectiveQuarantineRetentionDays;
        if (days <= 0) return 0;

        var root = RootFor(musicRoot);
        if (!Directory.Exists(root)) return 0;

        var cutoff = DateTime.UtcNow.AddDays(-days);
        var removed = 0;
        try
        {
            foreach (var dated in Directory.EnumerateDirectories(root))
            {
                var name = Path.GetFileName(dated);
                if (!DateTime.TryParseExact(name, "yyyy-MM-dd", null,
                        System.Globalization.DateTimeStyles.AssumeUniversal
                        | System.Globalization.DateTimeStyles.AdjustToUniversal, out var day)) continue;
                if (day >= cutoff) continue;

                var count = Directory.EnumerateFiles(dated, "*", SearchOption.AllDirectories)
                    .Count(path => !path.EndsWith(ManifestSuffix, StringComparison.Ordinal));
                Directory.Delete(dated, recursive: true);
                removed += count;
                _logger.LogInformation("Library action quarantine swept {Day} ({Count} file(s))", name, count);
            }
        }
        catch (Exception ex)
        {
            _logger.LogWarning("Library action quarantine sweep failed: {M}", ex.Message);
        }
        return removed;
    }

    private void WriteManifest(string quarantinePath, QuarantineManifest manifest)
    {
        try
        {
            File.WriteAllText(quarantinePath + ManifestSuffix, JsonSerializer.Serialize(manifest));
        }
        catch (Exception ex)
        {
            // The journal still records this, so a missing manifest costs the standalone
            // restore rather than the recovery entirely.
            _logger.LogWarning("Library action could not write a quarantine manifest: {M}", ex.Message);
        }
    }

    private QuarantineManifest? ReadManifest(string quarantinePath)
    {
        try
        {
            var path = quarantinePath + ManifestSuffix;
            if (!File.Exists(path)) return null;
            return JsonSerializer.Deserialize<QuarantineManifest>(File.ReadAllText(path));
        }
        catch { return null; }
    }

    private static string Unique(string path)
    {
        if (!File.Exists(path)) return path;
        var directory = Path.GetDirectoryName(path)!;
        var stem = Path.GetFileNameWithoutExtension(path);
        var extension = Path.GetExtension(path);
        for (var suffix = 2; suffix < 1000; suffix++)
        {
            var candidate = Path.Combine(directory, $"{stem} ({suffix}){extension}");
            if (!File.Exists(candidate)) return candidate;
        }
        return Path.Combine(directory, $"{stem} ({Guid.NewGuid():N}){extension}");
    }

    private static void TryDelete(string path)
    {
        try { if (File.Exists(path)) File.Delete(path); } catch { /* best effort */ }
    }
}
