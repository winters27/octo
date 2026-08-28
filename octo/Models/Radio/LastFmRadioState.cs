namespace Octo.Models.Radio;

public sealed class LastFmRadioStateDocument
{
    public int Version { get; set; } = 1;
    public Dictionary<string, LastFmRadioUserState> Users { get; set; } =
        new(StringComparer.OrdinalIgnoreCase);
}

public sealed class LastFmRadioUserState
{
    public string Username { get; set; } = string.Empty;
    public DateTime LastSeenUtc { get; set; } = DateTime.UtcNow;
    public int NewPlaysSinceRefresh { get; set; }
    public List<LastFmRadioPlay> Plays { get; set; } = [];
    public List<LastFmRadioStation> Stations { get; set; } = [];
    public List<LastFmRadioUnavailableTrack> UnavailableTracks { get; set; } = [];
    public DateTime? LastRefreshAttemptUtc { get; set; }
    public DateTime? LastRefreshSuccessUtc { get; set; }
    public string? LastRefreshError { get; set; }
    public bool Refreshing { get; set; }
}

public sealed class LastFmRadioUnavailableTrack
{
    public string Key { get; set; } = string.Empty;
    public string Artist { get; set; } = string.Empty;
    public string Title { get; set; } = string.Empty;
    public DateTime FailedAtUtc { get; set; } = DateTime.UtcNow;
    public DateTime RetryAfterUtc { get; set; } = DateTime.UtcNow.AddHours(24);
}

public sealed class LastFmRadioPlay
{
    public string SongId { get; set; } = string.Empty;
    public string Artist { get; set; } = string.Empty;
    public string Title { get; set; } = string.Empty;
    public string? Album { get; set; }
    public string? Genre { get; set; }
    public int? Duration { get; set; }
    public bool IsLocal { get; set; }
    public bool Hearted { get; set; }
    public bool LearnedSignal { get; set; } = true;
    public string Source { get; set; } = "scrobble";
    public DateTime PlayedAtUtc { get; set; } = DateTime.UtcNow;
}

public enum LastFmRadioStationKind
{
    Starter,
    YourMix,
    Discovery,
    Artist,
    Genre,
    Pinned,
}

public sealed class LastFmRadioStation
{
    public string Id { get; set; } = string.Empty;
    public string Key { get; set; } = string.Empty;
    public string Name { get; set; } = string.Empty;
    public string Owner { get; set; } = string.Empty;
    public LastFmRadioStationKind Kind { get; set; }
    public bool Personalized { get; set; }
    public int DefinitionVersion { get; set; }
    public DateTime CreatedUtc { get; set; } = DateTime.UtcNow;
    public DateTime ChangedUtc { get; set; } = DateTime.UtcNow;
    public DateTime ValidUntilUtc { get; set; } = DateTime.UtcNow;
    public List<string> Seeds { get; set; } = [];
    public List<LastFmRadioTrack> Tracks { get; set; } = [];
}

public sealed class LastFmRadioTrack
{
    public string Artist { get; set; } = string.Empty;
    public string Title { get; set; } = string.Empty;
    public string? Album { get; set; }
    public string? Genre { get; set; }
    public int? Duration { get; set; }
    public int? Year { get; set; }
    public double Score { get; set; }
    public string Source { get; set; } = string.Empty;
    public string? ResolvedId { get; set; }
    public bool IsLocal { get; set; }
    public string? ExternalProvider { get; set; }
    public string? YouTubeId { get; set; }
}

public sealed class LastFmRadioUserSummary
{
    public string Username { get; set; } = string.Empty;
    public int PlayCount { get; set; }
    public int StationCount { get; set; }
    public int NewPlaysSinceRefresh { get; set; }
    public DateTime? LastRefreshSuccessUtc { get; set; }
    public string? LastRefreshError { get; set; }
    public bool Refreshing { get; set; }
}
