using System.Globalization;
using System.Text.RegularExpressions;
using Octo.Models.Settings;
using Octo.Services.Common;

namespace Octo.Services.Soulseek;

/// <summary>
/// Soulseek files in words, for the downloads log and Find songs: what each one is, why Octo tried
/// it, and why it would pass one over. Reads only; the choosing itself stays in the download.
/// </summary>
internal static class SoulseekCandidates
{
    private static readonly HashSet<string> Lossless = new(StringComparer.OrdinalIgnoreCase)
        { "flac", "wav", "alac", "ape", "aiff", "aif", "wv" };

    // "01 - ", "01. ", "1-03 " in front of a title.
    private static readonly Regex TrackNumber = new(@"^\s*(\d{1,2}[-.])?\d{1,3}\s*[-._)]?\s+", RegexOptions.Compiled);

    /// <summary>A file as the log shows it: its own name, the folder it sits in, and what the peer
    /// said about it.</summary>
    public static AcquisitionCandidate Of(SoulseekFileHit hit, int? rank = null, string? note = null)
    {
        var parts = hit.Filename.Replace('\\', '/').Split('/', StringSplitOptions.RemoveEmptyEntries);
        var leaf = parts.Length > 0 ? parts[^1] : hit.Filename;
        var folders = parts.Length > 1 ? parts[..^1] : [];
        return new AcquisitionCandidate(
            Source: SongFinder.SoulseekSource,
            Peer: hit.Username,
            File: leaf,
            Folder: folders.Length == 0 ? null : string.Join('/', folders.TakeLast(2)),
            Format: Extension(hit),
            BitRate: hit.BitRate,
            BitDepth: hit.BitDepth,
            SampleRate: hit.SampleRate,
            Size: hit.Size > 0 ? hit.Size : null,
            Length: hit.Length,
            QueueLength: hit.QueueLength,
            FreeSlot: hit.HasFreeUploadSlot,
            Speed: hit.UploadSpeed,
            Rank: rank,
            Note: note,
            Title: TitleOf(leaf),
            Album: AlbumOf(folders));
    }

    /// <summary>The file's kind, from the peer's word or its name: "flac".</summary>
    public static string Extension(SoulseekFileHit hit) => SoulseekClient.NormalizeExtension(hit.Extension, hit.Filename);

    public static bool IsLossless(SoulseekFileHit hit) => Lossless.Contains(Extension(hit));

    /// <summary>"Trying Octo's first choice", "Trying the album folder's copy", ...</summary>
    public static string TryingText(int attempt, int total, bool fromAlbum, bool picked) =>
        picked ? "Trying the copy you picked"
        : fromAlbum ? "Trying the copy from the album's folder"
        : total <= 1 ? "Trying the only copy that fits"
        : $"Trying choice {attempt} of {total}";

    /// <summary>Why a peer's copy is worth trying, in a few words: its quality and how soon the
    /// peer can send it.</summary>
    public static string WhyChosen(SoulseekFileHit hit)
    {
        var parts = new List<string>();
        var quality = AcquisitionCandidate.QualityText(Extension(hit), hit.BitRate, hit.BitDepth, hit.SampleRate);
        if (quality is not null) parts.Add(IsLossless(hit) ? $"lossless ({quality})" : quality);
        if (hit.HasFreeUploadSlot == true) parts.Add("the peer can send it now");
        else if (hit.QueueLength is > 0 and var queue) parts.Add($"{queue} ahead in the peer's queue");
        if (hit.UploadSpeed is > 0 and var speed) parts.Add($"{SpeedText(speed)}");
        if (hit.Size > 0) parts.Add(AcquisitionTracker.SizeText(hit.Size));
        return parts.Count == 0 ? "the best of what was found" : string.Join(", ", parts);
    }

    /// <summary>
    /// Why the download would pass a file over, or null when it would take it. The same tests the
    /// download's ranking applies, read one by one so the first that fails can be named.
    /// </summary>
    public static string? WhyNot(SoulseekFileHit hit, string title, string? album, int? duration,
        SoulseekSettings settings, RejectedPeerRegistry? rejected, bool remembersRejections, string? artist = null) =>
        Judge(hit, title, album, duration, settings, rejected, remembersRejections, artist).Note;

    /// <summary>The reason, and how far the file is from the song: 0 it fits, 1 it is the song in
    /// another format, 2 it is likely not the song asked for. Find songs lists them in that order.</summary>
    public static (string? Note, int Tier) Judge(SoulseekFileHit hit, string title, string? album, int? duration,
        SoulseekSettings settings, RejectedPeerRegistry? rejected, bool remembersRejections, string? artist = null)
    {
        if (!SoulseekDownloadService.CandidateAllowed(hit, rejected, remembersRejections))
            return ("Octo downloaded this file before and it was the wrong recording", 2);
        if (hit.Size > 0 && hit.Size < settings.MinFileSizeBytes && IsLossless(hit))
            return ("Too small to be the whole song", 2);
        if (!SoulseekDownloadService.FilenamePlausiblyMatchesTitle(hit.Filename, title))
            return ("The file name does not match the title", 2);
        if (!SoulseekDownloadService.DurationPlausible(hit.Length, duration, requireKnownLength: false))
            return (hit.Length is { } seconds && duration is { } expected
                ? $"{LengthText(seconds)} long; the song is {LengthText(expected)}"
                : "A different length from the song", 2);
        if (SoulseekDownloadService.AddsVersion(hit.Filename, title, artist))
            return ("Another version: a remix, an edit or a live take", 2);
        if (SoulseekDownloadService.FromLiveFolder(hit.Filename, title, album, artist))
            return ("From a live album", 2);
        if (SoulseekDownloadService.FromVersionFolder(hit.Filename, title, album, artist))
            return ("From a record of other versions: remixes, edits or a single's radio edit", 2);
        if (VersionVariant.LacksRequested(hit.Filename, title, artist))
            return ("Not the version asked for: the file is the plain song", 2);
        var wanted = SoulseekClient.NormalizeExtension(settings.PreferredExtension, "");
        if (!string.Equals(Extension(hit), wanted, StringComparison.OrdinalIgnoreCase))
            return ($"Not {wanted.ToUpperInvariant()}, which Octo looks for first", 1);
        return (null, 0);
    }

    /// <summary>"3:58".</summary>
    public static string LengthText(int seconds) =>
        $"{seconds / 60}:{(seconds % 60).ToString("00", CultureInfo.InvariantCulture)}";

    /// <summary>slskd reports bytes a second: "1.2 MB/s", "340 KB/s".</summary>
    public static string SpeedText(int bytesPerSecond) =>
        bytesPerSecond >= 1024 * 1024
            ? $"{(bytesPerSecond / 1048576.0).ToString("0.0", CultureInfo.InvariantCulture)} MB/s"
            : $"{Math.Max(1, bytesPerSecond / 1024)} KB/s";

    /// <summary>The song's name from a file's name: no extension, no track number.</summary>
    internal static string TitleOf(string leaf)
    {
        var dot = leaf.LastIndexOf('.');
        var name = dot > 0 ? leaf[..dot] : leaf;
        var bare = TrackNumber.Replace(name, "").Trim();
        return bare.Length == 0 ? name.Trim() : bare;
    }

    /// <summary>The record a file sits in: its folder, or the one above a "CD1" or "Disc 2" folder.</summary>
    internal static string? AlbumOf(IReadOnlyList<string> folders)
    {
        if (folders.Count == 0) return null;
        var last = folders[^1];
        if (Regex.IsMatch(last, @"^(cd|disc|disk)\s*\d+$", RegexOptions.IgnoreCase) && folders.Count > 1) last = folders[^2];
        return last.Trim();
    }
}
