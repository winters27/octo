using System.Collections.Concurrent;
using Octo.Services.Soulseek;

namespace Octo.Services.Common;

/// <summary>
/// One copy someone picked in Find songs: a Soulseek peer's file, or a Lidarr release. Kept as
/// plain values, so an upgrade job waiting for its turn can carry it on disk.
/// </summary>
public sealed class PickedCopy
{
    /// <summary>"Soulseek" or "Lidarr".</summary>
    public string Source { get; set; } = SongFinder.SoulseekSource;

    // A Soulseek file, as the search found it.
    public string? Peer { get; set; }
    public string? File { get; set; }
    public long Size { get; set; }
    public string? Format { get; set; }
    public int? BitRate { get; set; }
    public int? BitDepth { get; set; }
    public int? SampleRate { get; set; }
    public int? Length { get; set; }

    // A Lidarr release. Without one, Lidarr chooses by its quality profile.
    public string? ReleaseGuid { get; set; }
    public int? IndexerId { get; set; }
    public string? ReleaseTitle { get; set; }

    [System.Text.Json.Serialization.JsonIgnore]
    public bool IsSoulseek => string.Equals(Source, SongFinder.SoulseekSource, StringComparison.OrdinalIgnoreCase);

    /// <summary>The file as the Soulseek pipeline takes it.</summary>
    public SoulseekFileHit? ToHit() => IsSoulseek && !string.IsNullOrEmpty(Peer) && !string.IsNullOrEmpty(File)
        ? new SoulseekFileHit
        {
            Username = Peer, Filename = File, Size = Size, BitRate = BitRate, BitDepth = BitDepth,
            SampleRate = SampleRate, Length = Length,
            Extension = SoulseekClient.NormalizeExtension(Format, File),
        }
        : null;

    /// <summary>The release Lidarr is to grab, or null to let it choose.</summary>
    [System.Text.Json.Serialization.JsonIgnore]
    public Octo.Services.Lidarr.LidarrReleasePick? Release =>
        !IsSoulseek && !string.IsNullOrEmpty(ReleaseGuid) && IndexerId is { } indexer
            ? new Octo.Services.Lidarr.LidarrReleasePick(ReleaseGuid, indexer, ReleaseTitle)
            : null;

    /// <summary>"FLAC 16-bit 44.1 kHz from someone", for the log and the upgrade queue.</summary>
    public string Describe()
    {
        if (!IsSoulseek) return ReleaseTitle is { Length: > 0 } release ? $"Lidarr release \"{release}\"" : "Lidarr's choice";
        var quality = AcquisitionCandidate.QualityText(Format, BitRate, BitDepth, SampleRate);
        return $"{quality ?? "a file"} from {Peer}";
    }
}

/// <summary>
/// Copies picked in Find songs, waiting for their download to take them. Keyed by the outside id
/// the download runs under; the Soulseek and Lidarr pipelines take the pick instead of searching.
/// A pick nobody takes is forgotten after a while, so a download asked for much later searches
/// as usual.
/// </summary>
public sealed class DownloadPicks
{
    internal static readonly TimeSpan Keep = TimeSpan.FromHours(2);

    private readonly ConcurrentDictionary<string, (PickedCopy Copy, DateTime At)> _picks = new(StringComparer.Ordinal);
    private readonly TimeProvider _time;

    public DownloadPicks(TimeProvider? time = null) => _time = time ?? TimeProvider.System;

    private DateTime Now => _time.GetUtcNow().UtcDateTime;

    public void Pin(string externalId, PickedCopy copy)
    {
        if (string.IsNullOrWhiteSpace(externalId)) return;
        foreach (var (key, value) in _picks)
            if (Now - value.At >= Keep) _picks.TryRemove(key, out _);
        _picks[externalId] = (copy, Now);
    }

    /// <summary>The pick for this song from this source, taken so it is used once. A pick for the
    /// other source stays for that source.</summary>
    public PickedCopy? Take(string externalId, string source)
    {
        if (string.IsNullOrWhiteSpace(externalId) || !_picks.TryGetValue(externalId, out var pick)) return null;
        if (Now - pick.At >= Keep)
        {
            _picks.TryRemove(externalId, out _);
            return null;
        }
        if (!string.Equals(pick.Copy.Source, source, StringComparison.OrdinalIgnoreCase)) return null;
        return _picks.TryRemove(new KeyValuePair<string, (PickedCopy, DateTime)>(externalId, pick)) ? pick.Copy : null;
    }

    /// <summary>Whether a pick is still waiting for this song.</summary>
    public bool Has(string externalId) => _picks.ContainsKey(externalId);
}
