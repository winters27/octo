namespace Octo.Models.Settings;

public class LastFmSettings
{
    /// <summary>
    /// Last.fm API key for fetching similar tracks
    /// </summary>
    public string ApiKey { get; set; } = string.Empty;
    
    /// <summary>
    /// Enable/disable the radio feature
    /// </summary>
    public bool EnableRadio { get; set; } = true;
    
    /// <summary>
    /// Maximum tracks in song-seeded queues and reusable station snapshots.
    /// </summary>
    public int RadioTrackCount { get; set; } = 50;
    
    /// <summary>
    /// Cache duration for Last.fm lookups in hours
    /// </summary>
    public int RadioCacheDurationHours { get; set; } = 24;

    /// <summary>Automatically learn per-user stations from completed plays.</summary>
    public bool EnablePersonalizedStations { get; set; } = true;

    /// <summary>Expose administrator-pinned Last.fm tag stations.</summary>
    public bool EnableDiscoveryStations { get; set; } = true;

    /// <summary>Publish Octo stations as normal read-only playlists.</summary>
    public bool ExposeRadioAsPlaylists { get; set; } = true;

    /// <summary>Publish Octo stations through Subsonic internet radio.</summary>
    public bool ExposeRadioAsStreams { get; set; } = true;

    /// <summary>Continuous internet-radio MP3 bitrate.</summary>
    public int RadioStreamBitrateKbps { get; set; } = 192;

    /// <summary>Embed the current station track as opt-in ICY stream metadata.</summary>
    public bool EnableIcyMetadata { get; set; } = true;

    public int HistoryRetentionDays { get; set; } = 90;
    public int DiscoveryPercent { get; set; } = 35;
    public int RefreshIntervalHours { get; set; } = 12;
    public int MinimumPlays { get; set; } = 10;
    public List<DiscoveryStationSettings> DiscoveryStations { get; set; } = [];

    public int EffectiveHistoryRetentionDays => Math.Clamp(HistoryRetentionDays, 7, 365);
    public int EffectiveRadioTrackCount => Math.Clamp(RadioTrackCount, 10, 100);
    public int EffectiveRadioCacheDurationHours => Math.Clamp(RadioCacheDurationHours, 1, 168);
    public int EffectiveDiscoveryPercent => Math.Clamp(DiscoveryPercent, 0, 100);
    public int EffectiveRefreshIntervalHours => Math.Clamp(RefreshIntervalHours, 1, 168);
    public int EffectiveMinimumPlays => Math.Clamp(MinimumPlays, 3, 100);
    public int EffectiveRadioStreamBitrateKbps => RadioStreamBitrateKbps switch
    {
        <= 96 => 96,
        <= 128 => 128,
        <= 192 => 192,
        <= 256 => 256,
        _ => 320,
    };

    public IReadOnlyList<DiscoveryStationSettings> EffectiveDiscoveryStations()
    {
        var seenIds = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
        var seenNames = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
        var result = new List<DiscoveryStationSettings>();

        foreach (var source in DiscoveryStations.Take(12))
        {
            var name = (source.Name ?? "").Trim();
            var tags = (source.Tags ?? [])
                .Select(DiscoveryStationSettings.NormalizeTag)
                .Where(tag => tag.Length is > 0 and <= 80)
                .Distinct(StringComparer.OrdinalIgnoreCase)
                .Take(5)
                .ToList();
            if (name.Length is 0 or > 100 || tags.Count == 0 || !seenNames.Add(name)) continue;

            var id = DiscoveryStationSettings.NormalizeId(source.Id);
            if (string.IsNullOrEmpty(id)) id = DiscoveryStationSettings.DeterministicId(name, tags);
            if (!seenIds.Add(id)) continue;

            result.Add(new DiscoveryStationSettings
            {
                Id = id,
                Name = name,
                Enabled = source.Enabled,
                Tags = tags,
            });
        }
        return result;
    }
}

public sealed class DiscoveryStationSettings
{
    public string Id { get; set; } = string.Empty;
    public string Name { get; set; } = string.Empty;
    public bool Enabled { get; set; } = true;
    public List<string> Tags { get; set; } = [];

    public static string NormalizeTag(string? value) =>
        string.Join(' ', (value ?? "").Trim().ToLowerInvariant()
            .Split(' ', StringSplitOptions.RemoveEmptyEntries));

    public static string NormalizeId(string? value)
    {
        if (string.IsNullOrWhiteSpace(value)) return string.Empty;
        var chars = value.Trim().Where(char.IsAsciiLetterOrDigit).Take(32).ToArray();
        return new string(chars).ToLowerInvariant();
    }

    public static string DeterministicId(string name, IEnumerable<string> tags)
    {
        var seed = $"{name.Trim().ToLowerInvariant()}|{string.Join('|', tags)}";
        var hash = System.Security.Cryptography.SHA256.HashData(
            System.Text.Encoding.UTF8.GetBytes(seed));
        return Convert.ToHexString(hash[..12]).ToLowerInvariant();
    }
}
