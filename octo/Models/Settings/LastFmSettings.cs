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

    /// <summary>
    /// Build the listener's own mix. When they have not been learned from yet this is
    /// Starter Radio, which is the only station a new user gets, so turning it off can
    /// leave them with nothing at all until plays accumulate.
    /// Environment variable: LASTFM_ENABLE_YOUR_MIX
    /// </summary>
    public bool EnableYourMix { get; set; } = true;

    /// <summary>
    /// Build the Discovery Mix, seeded from the listener's top tags.
    /// Environment variable: LASTFM_ENABLE_DISCOVERY_MIX
    /// </summary>
    public bool EnableDiscoveryMix { get; set; } = true;

    /// <summary>
    /// How many per-artist radios to build, most-played first. 0 disables them.
    /// Environment variable: LASTFM_ARTIST_STATION_COUNT
    /// </summary>
    public int ArtistStationCount { get; set; } = 2;

    /// <summary>
    /// How many per-genre radios to build, from the listener's top tags. 0 disables them.
    /// Environment variable: LASTFM_GENRE_STATION_COUNT
    /// </summary>
    public int GenreStationCount { get; set; } = 3;

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

    /// <summary>
    /// How long a station-list request waits for a cold station's first track before
    /// answering without it. Production keeps going in the background and the station
    /// appears on the client's next refresh. 0 waits for the starter however long it
    /// takes, which is the original behaviour.
    /// </summary>
    public int StarterPublishTimeoutSeconds { get; set; } = 8;

    /// <summary>
    /// EBU R128 integrated loudness every radio track is brought to before it joins the
    /// stream, in LUFS. Tracks arrive from local FLAC and from YouTube previews at
    /// levels many LU apart; a static gain per track (with a true-peak limiter at
    /// -1 dBTP) makes the station one level. 0 disables normalisation.
    /// </summary>
    public int RadioLoudnessTargetLufs { get; set; } = -16;

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
    public int EffectiveArtistStationCount => Math.Clamp(ArtistStationCount, 0, 5);
    public int EffectiveGenreStationCount => Math.Clamp(GenreStationCount, 0, 5);

    /// <summary>
    /// Whether the settings ask for nothing to be built: dynamic stations are on, every
    /// individual kind is off, and no pinned station is contributing either.
    ///
    /// Narrow on purpose. The refresh worker treats an empty build as a provider failure
    /// and keeps the last good snapshot rather than replacing it with nothing, which is
    /// the behaviour that stops a bad Last.fm response from wiping a listener's stations.
    /// Switching every kind off is the one empty build that is a choice rather than a
    /// failure, so only that case is excused; every configuration that predates the
    /// per-type settings keeps the old guard exactly.
    /// </summary>
    public bool StationsExplicitlyEmpty => EnablePersonalizedStations
        && !EnableYourMix && !EnableDiscoveryMix
        && EffectiveArtistStationCount == 0 && EffectiveGenreStationCount == 0
        && !(EnableDiscoveryStations && EffectiveDiscoveryStations().Any(station => station.Enabled));

    public double? EffectiveRadioLoudnessTarget => RadioLoudnessTargetLufs == 0
        ? null
        : Math.Clamp(RadioLoudnessTargetLufs, -23, -9);
    public TimeSpan? EffectiveStarterPublishTimeout => StarterPublishTimeoutSeconds <= 0
        ? null
        : TimeSpan.FromSeconds(Math.Clamp(StarterPublishTimeoutSeconds, 1, 300));
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
