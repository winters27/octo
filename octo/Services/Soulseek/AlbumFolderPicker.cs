using System.Text.RegularExpressions;
using Octo.Services.Common;

namespace Octo.Services.Soulseek;

/// <summary>One track of an album, as the picker matches files to it.</summary>
public sealed record AlbumTrack(string ExternalId, string Title, int? Duration, int? Number);

/// <summary>The folder an album walk takes: whose it is, where, and the file each track gets, in
/// album order.</summary>
public sealed record AlbumFolderChoice(string Username, string Folder,
    IReadOnlyList<(AlbumTrack Track, SoulseekFileHit File)> Files);

/// <summary>
/// Picks the one peer folder that covers most of an album, so the album comes from one rip in one
/// batch instead of a search, a peer and a queue per song.
///
/// Every check a song's own search makes applies to each file here too: the title as a phrase in
/// the file name, a length within the window, no version the track did not ask for and none it
/// asked for missing. On top of that
/// a file's leading track number counts, and matching is global rather than in track order, so
/// "Hold On" never takes "Hold On, We're Going Home" when both are on the record.
/// </summary>
public static class AlbumFolderPicker
{
    /// <summary>Album mode needs a length for most tracks: the per-file length check is what keeps a
    /// wrong file out, and a track without one goes to the song by song search instead.</summary>
    internal const double MinKnownLengths = 0.8;

    /// <summary>A folder must cover at least this share of the tracks still wanted.</summary>
    internal const double MinCoverage = 0.5;

    /// <summary>A peer with a free upload slot starts sending now. Its folder wins when it covers at
    /// least this share of what the fullest folder covers.</summary>
    internal const double FreeSlotShare = 0.75;

    private static readonly Regex DiscFolder = new(@"^(cd|dis[ck])\s*\d+$",
        RegexOptions.IgnoreCase | RegexOptions.CultureInvariant);

    // "03 - Song", "03. Song", "1-03 Song": the track number, without a disc prefix.
    private static readonly Regex LeadingNumber = new(@"^\s*(?:\d{1,2}\s*[-.]\s*(?=\d))?0*(\d{1,3})(?!\d)",
        RegexOptions.CultureInvariant);

    public static AlbumFolderChoice? Choose(IReadOnlyList<SoulseekFileHit> hits, IReadOnlyList<AlbumTrack> tracks,
        Func<SoulseekFileHit, bool> usable)
    {
        if (tracks.Count == 0) return null;
        var known = tracks.Where(t => t.Duration is > 0).ToList();
        if (known.Count < MinKnownLengths * tracks.Count) return null;

        var folders = hits.Where(usable)
            .GroupBy(hit => (hit.Username, Folder: FolderOf(hit.Filename)))
            .Select(group => (group.Key.Username, group.Key.Folder, Files: Match(known, group.ToList())))
            .Where(folder => folder.Files.Count > 0)
            .ToList();
        if (folders.Count == 0) return null;

        var fullest = folders.Max(folder => folder.Files.Count);
        var pick = folders
            .OrderByDescending(folder => FreeNow(folder.Files) && folder.Files.Count >= FreeSlotShare * fullest)
            .ThenByDescending(folder => folder.Files.Count)
            .ThenBy(folder => folder.Files.Average(pair => SoulseekDownloadService.QualityPenalty(pair.File)))
            .ThenBy(folder => folder.Files[0].File.QueueLength ?? int.MaxValue)
            .ThenByDescending(folder => folder.Files[0].File.UploadSpeed ?? 0)
            .First();

        if (pick.Files.Count < Math.Max(2, Math.Ceiling(MinCoverage * tracks.Count))) return null;
        var order = tracks.Select((track, index) => (track.ExternalId, index)).ToDictionary(x => x.ExternalId, x => x.index);
        return new AlbumFolderChoice(pick.Username, pick.Folder,
            pick.Files.OrderBy(pair => order[pair.Track.ExternalId]).ToList());
    }

    private static bool FreeNow(IReadOnlyList<(AlbumTrack Track, SoulseekFileHit File)> files) =>
        files[0].File.HasFreeUploadSlot == true;

    /// <summary>The folder a file is in, with a CD1 or Disc 2 folder counted as its parent, so a
    /// two-disc rip is one candidate.</summary>
    internal static string FolderOf(string remoteFilename)
    {
        var parts = remoteFilename.Replace('\\', '/').Split('/', StringSplitOptions.RemoveEmptyEntries).ToList();
        if (parts.Count > 0) parts.RemoveAt(parts.Count - 1);
        if (parts.Count > 1 && DiscFolder.IsMatch(parts[^1].Trim())) parts.RemoveAt(parts.Count - 1);
        return string.Join('/', parts);
    }

    /// <summary>
    /// Every track and file pair that passes the filters, scored, then taken lowest score first,
    /// each track and each file once. Score: the title exactly (0) or only as a phrase (3); the
    /// file's leading number equal to the track's (0), missing (1) or different (4); plus the length
    /// difference in seconds.
    /// </summary>
    internal static List<(AlbumTrack Track, SoulseekFileHit File)> Match(IReadOnlyList<AlbumTrack> tracks,
        IReadOnlyList<SoulseekFileHit> files)
    {
        var pairs = new List<(AlbumTrack Track, SoulseekFileHit File, int Score)>();
        foreach (var track in tracks)
        foreach (var file in files)
        {
            if (!SoulseekDownloadService.FilenamePlausiblyMatchesTitle(file.Filename, track.Title, requirePhrase: true)) continue;
            if (!SoulseekDownloadService.DurationPlausible(file.Length, track.Duration)) continue;
            if (SoulseekDownloadService.AddsVersion(file.Filename, track.Title)) continue;
            if (VersionVariant.LacksRequested(file.Filename, track.Title)) continue;
            var leaf = Path.GetFileNameWithoutExtension(file.Filename.Replace('\\', '/').Split('/')[^1]);
            var number = LeadingNumber.Match(leaf) is { Success: true } m && int.TryParse(m.Groups[1].Value, out var n) ? n : (int?)null;
            var score = (ExactTitle(leaf, track.Title) ? 0 : 3)
                + (number is null || track.Number is null ? 1 : number == track.Number ? 0 : 4)
                + (file.Length is int length && track.Duration is int duration ? Math.Abs(length - duration) : 4);
            pairs.Add((track, file, score));
        }

        var takenTracks = new HashSet<string>(StringComparer.Ordinal);
        var takenFiles = new HashSet<SoulseekFileHit>(ReferenceEqualityComparer.Instance);
        var matched = new List<(AlbumTrack Track, SoulseekFileHit File)>();
        foreach (var pair in pairs.OrderBy(p => p.Score).ThenBy(p => p.File.Filename, StringComparer.Ordinal))
        {
            if (takenTracks.Contains(pair.Track.ExternalId) || takenFiles.Contains(pair.File)) continue;
            takenTracks.Add(pair.Track.ExternalId);
            takenFiles.Add(pair.File);
            matched.Add((pair.Track, pair.File));
        }
        return matched;
    }

    /// <summary>The file name, once a leading number and an "Artist - " are taken off, is the title.</summary>
    private static bool ExactTitle(string leaf, string title)
    {
        var wanted = SongIdentity.Key(title);
        if (wanted.Length == 0) return false;
        var rest = LeadingNumber.Replace(leaf, "", 1).TrimStart(' ', '-', '.', '_');
        if (SongIdentity.Key(rest) == wanted) return true;
        var dash = rest.LastIndexOf(" - ", StringComparison.Ordinal);
        return dash >= 0 && SongIdentity.Key(rest[(dash + 3)..]) == wanted;
    }
}
