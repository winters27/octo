using System.Security.Cryptography;
using System.Text;
using System.Text.Json;
using Microsoft.Extensions.Options;
using Octo.Models.Radio;
using Octo.Models.Settings;
using Octo.Services.Soulseek;

namespace Octo.Services.LastFm;

/// <summary>
/// Bounded local state for Last.fm radio. This follows DownloadHistoryService's
/// locked cache + temporary-file rename pattern and is deliberately single-writer.
/// </summary>
public sealed class LastFmRadioStateStore
{
    public const int CurrentVersion = 1;
    private const int MaxPlaysPerUser = 2_000;
    private const int MaxUsers = 100;
    private const int MaxUnavailableTracksPerUser = 500;
    private static readonly TimeSpan UnavailableTrackCooldown = TimeSpan.FromHours(24);
    private static readonly TimeSpan DuplicateWindow = TimeSpan.FromMinutes(5);

    private readonly string _path;
    private readonly IOptionsMonitor<LastFmSettings> _settings;
    private readonly ExternalIdRegistry _registry;
    private readonly ILogger<LastFmRadioStateStore> _logger;
    private readonly object _lock = new();
    private LastFmRadioStateDocument? _state;

    public LastFmRadioStateStore(string path, IOptionsMonitor<LastFmSettings> settings,
        ExternalIdRegistry registry, ILogger<LastFmRadioStateStore> logger)
    {
        _path = path;
        _settings = settings;
        _registry = registry;
        _logger = logger;
    }

    public bool RecordPlay(string username, LastFmRadioPlay play)
    {
        if (string.IsNullOrWhiteSpace(username)
            || string.IsNullOrWhiteSpace(play.Artist)
            || string.IsNullOrWhiteSpace(play.Title))
            return false;

        lock (_lock)
        {
            var state = LoadLocked();
            var user = GetOrCreateLocked(state, username);
            var key = LastFmRadioSeedNormalizer.TrackKey(play.Artist, play.Title);
            var duplicate = user.Plays.Any(existing =>
                LastFmRadioSeedNormalizer.TrackKey(existing.Artist, existing.Title) == key
                && Math.Abs((existing.PlayedAtUtc - play.PlayedAtUtc).TotalMinutes)
                    <= DuplicateWindow.TotalMinutes);
            if (duplicate) return false;

            play.Artist = LastFmRadioSeedNormalizer.Artist(play.Artist) ?? play.Artist.Trim();
            play.Title = LastFmRadioSeedNormalizer.Title(play.Title) ?? play.Title.Trim();
            play.PlayedAtUtc = play.PlayedAtUtc.Kind == DateTimeKind.Utc
                ? play.PlayedAtUtc
                : play.PlayedAtUtc.ToUniversalTime();
            user.Plays.Insert(0, play);
            user.NewPlaysSinceRefresh++;
            user.LastSeenUtc = DateTime.UtcNow;
            PruneLocked(state);
            SaveLocked(state);
            return true;
        }
    }

    public LastFmRadioUserState GetUser(string username)
    {
        lock (_lock)
        {
            var state = LoadLocked();
            return state.Users.TryGetValue(UserKey(username), out var user)
                ? Clone(user)
                : new LastFmRadioUserState { Username = username };
        }
    }

    public bool MarkHeart(string username, string songId, string artist, string title)
    {
        lock (_lock)
        {
            if (!LoadLocked().Users.TryGetValue(UserKey(username), out var user)) return false;
            var key = LastFmRadioSeedNormalizer.TrackKey(artist, title);
            var play = user.Plays.FirstOrDefault(item => item.SongId == songId
                || LastFmRadioSeedNormalizer.TrackKey(item.Artist, item.Title) == key);
            if (play is null || play.Hearted) return false;
            play.Hearted = true;
            SaveLocked(_state!);
            return true;
        }
    }

    public IReadOnlyList<LastFmRadioUserSummary> GetSummaries()
    {
        lock (_lock)
        {
            return LoadLocked().Users.Values
                .OrderByDescending(user => user.LastSeenUtc)
                .Select(user => new LastFmRadioUserSummary
                {
                    Username = user.Username,
                    PlayCount = user.Plays.Count,
                    StationCount = user.Stations.Count,
                    NewPlaysSinceRefresh = user.NewPlaysSinceRefresh,
                    LastRefreshSuccessUtc = user.LastRefreshSuccessUtc,
                    LastRefreshError = user.LastRefreshError,
                    Refreshing = user.Refreshing,
                }).ToList();
        }
    }

    public IReadOnlyList<string> KnownUsers()
    {
        lock (_lock) return LoadLocked().Users.Values.Select(user => user.Username).ToList();
    }

    public LastFmRadioStation? FindStation(string username, string stationId)
    {
        lock (_lock)
        {
            if (!LoadLocked().Users.TryGetValue(UserKey(username), out var user)) return null;
            var station = user.Stations.FirstOrDefault(item => item.Id == stationId);
            return station is null ? null : Clone(station);
        }
    }

    /// <summary>Removes an unplayable song from every current station and prevents
    /// an immediate deterministic refresh from selecting it again.</summary>
    public int RejectTrack(string username, LastFmRadioTrack track, DateTime? nowUtc = null)
    {
        if (string.IsNullOrWhiteSpace(username) || string.IsNullOrWhiteSpace(track.Artist)
            || string.IsNullOrWhiteSpace(track.Title)) return 0;
        lock (_lock)
        {
            var state = LoadLocked();
            var user = GetOrCreateLocked(state, username);
            var now = nowUtc ?? DateTime.UtcNow;
            var key = LastFmRadioSeedNormalizer.TrackKey(track.Artist, track.Title);
            var removed = 0;
            foreach (var station in user.Stations)
            {
                var before = station.Tracks.Count;
                station.Tracks = station.Tracks.Where(candidate =>
                    LastFmRadioSeedNormalizer.TrackKey(candidate.Artist, candidate.Title) != key)
                    .ToList();
                removed += before - station.Tracks.Count;
                if (before != station.Tracks.Count) station.ChangedUtc = now;
            }
            user.UnavailableTracks = (user.UnavailableTracks ?? [])
                .Where(item => item.RetryAfterUtc > now && item.Key != key)
                .Prepend(new LastFmRadioUnavailableTrack
                {
                    Key = key, Artist = track.Artist, Title = track.Title,
                    FailedAtUtc = now, RetryAfterUtc = now.Add(UnavailableTrackCooldown),
                })
                .OrderByDescending(item => item.FailedAtUtc)
                .Take(MaxUnavailableTracksPerUser).ToList();
            user.LastSeenUtc = now;
            SaveLocked(state);
            return removed;
        }
    }

    public void ReplaceStations(string username, IReadOnlyCollection<LastFmRadioStation> stations)
    {
        lock (_lock)
        {
            var state = LoadLocked();
            var user = GetOrCreateLocked(state, username);
            var existing = user.Stations.GroupBy(station => station.Id, StringComparer.Ordinal)
                .ToDictionary(group => group.Key, group => group.First(), StringComparer.Ordinal);
            user.Stations = stations.Select(source =>
            {
                var station = Clone(source);
                if (!existing.TryGetValue(station.Id, out var prior)) return station;
                station.CreatedUtc = prior.CreatedUtc;
                if (SameSnapshot(prior, station)) station.ChangedUtc = prior.ChangedUtc;
                return station;
            }).ToList();
            user.NewPlaysSinceRefresh = 0;
            user.LastRefreshSuccessUtc = DateTime.UtcNow;
            user.LastRefreshAttemptUtc = user.LastRefreshSuccessUtc;
            user.LastRefreshError = null;
            user.Refreshing = false;
            user.LastSeenUtc = DateTime.UtcNow;
            RehydrateRoutes(user);
            SaveLocked(state);
        }
    }

    public void MarkRefreshing(string username)
    {
        lock (_lock)
        {
            var state = LoadLocked();
            var user = GetOrCreateLocked(state, username);
            user.Refreshing = true;
            user.LastRefreshAttemptUtc = DateTime.UtcNow;
            user.LastRefreshError = null;
            SaveLocked(state);
        }
    }

    public void MarkRefreshFailed(string username, string message)
    {
        lock (_lock)
        {
            var state = LoadLocked();
            var user = GetOrCreateLocked(state, username);
            user.Refreshing = false;
            user.LastRefreshAttemptUtc = DateTime.UtcNow;
            user.LastRefreshError = message.Length > 500 ? message[..500] : message;
            SaveLocked(state);
        }
    }

    public bool Reset(string username)
    {
        lock (_lock)
        {
            var state = LoadLocked();
            var removed = state.Users.Remove(UserKey(username));
            if (removed) SaveLocked(state);
            return removed;
        }
    }

    public static string StationId(string username, string stationKey)
    {
        var hash = SHA256.HashData(Encoding.UTF8.GetBytes(
            $"{UserKey(username)}|{stationKey.Trim().ToLowerInvariant()}"));
        return "or" + ToBase62(hash, 20);
    }

    private LastFmRadioStateDocument LoadLocked()
    {
        if (_state is not null) return _state;
        try
        {
            if (!File.Exists(_path)) return _state = new LastFmRadioStateDocument();
            var json = File.ReadAllText(_path);
            _state = JsonSerializer.Deserialize<LastFmRadioStateDocument>(json)
                ?? new LastFmRadioStateDocument();
            if (_state.Version != CurrentVersion)
            {
                _logger.LogWarning("Unsupported Last.fm radio state version {Version}; starting clean",
                    _state.Version);
                _state = new LastFmRadioStateDocument();
            }
            _state.Users = new Dictionary<string, LastFmRadioUserState>(
                _state.Users ?? [], StringComparer.OrdinalIgnoreCase);
            PruneLocked(_state);
            foreach (var user in _state.Users.Values) RehydrateRoutes(user);
        }
        catch (Exception ex)
        {
            _logger.LogWarning("Last.fm radio state load failed ({Message}); starting clean", ex.Message);
            _state = new LastFmRadioStateDocument();
        }
        return _state;
    }

    private LastFmRadioUserState GetOrCreateLocked(LastFmRadioStateDocument state, string username)
    {
        var key = UserKey(username);
        if (state.Users.TryGetValue(key, out var existing)) return existing;
        var user = new LastFmRadioUserState { Username = username.Trim() };
        state.Users[key] = user;
        return user;
    }

    private void PruneLocked(LastFmRadioStateDocument state)
    {
        var cutoff = DateTime.UtcNow.AddDays(-_settings.CurrentValue.EffectiveHistoryRetentionDays);
        foreach (var user in state.Users.Values)
        {
            user.Plays = (user.Plays ?? [])
                .Where(play => play.PlayedAtUtc >= cutoff)
                .OrderByDescending(play => play.PlayedAtUtc)
                .Take(MaxPlaysPerUser)
                .ToList();
            user.Stations ??= [];
            user.UnavailableTracks = (user.UnavailableTracks ?? [])
                .Where(track => track.RetryAfterUtc > DateTime.UtcNow)
                .OrderByDescending(track => track.FailedAtUtc)
                .Take(MaxUnavailableTracksPerUser).ToList();
        }

        foreach (var key in state.Users.OrderByDescending(pair => pair.Value.LastSeenUtc)
                     .Skip(MaxUsers).Select(pair => pair.Key).ToList())
            state.Users.Remove(key);
    }

    private void RehydrateRoutes(LastFmRadioUserState user)
    {
        foreach (var track in user.Stations.SelectMany(station => station.Tracks)
                     .Where(track => !track.IsLocal))
        {
            track.ResolvedId = _registry.Register(new SoulseekRouting
            {
                Kind = RoutingKind.Song,
                Artist = track.Artist,
                Title = track.Title,
                Album = track.Album,
                Duration = track.Duration,
                YouTubeId = track.YouTubeId,
            });
            track.ExternalProvider ??= SoulseekMetadataService.ProviderName;
        }
    }

    private void SaveLocked(LastFmRadioStateDocument state)
    {
        try
        {
            Directory.CreateDirectory(Path.GetDirectoryName(_path) ?? ".");
            var temp = _path + ".tmp";
            File.WriteAllText(temp, JsonSerializer.Serialize(state,
                new JsonSerializerOptions { WriteIndented = true }));
            File.Move(temp, _path, overwrite: true);
        }
        catch (Exception ex)
        {
            _logger.LogWarning("Last.fm radio state save failed: {Message}", ex.Message);
        }
    }

    private static string UserKey(string username) => username.Trim().ToLowerInvariant();

    private static bool SameSnapshot(LastFmRadioStation left, LastFmRadioStation right) =>
        left.DefinitionVersion == right.DefinitionVersion
        && left.Name == right.Name
        && left.Tracks.Select(track => LastFmRadioSeedNormalizer.TrackKey(track.Artist, track.Title))
            .SequenceEqual(right.Tracks.Select(track =>
                LastFmRadioSeedNormalizer.TrackKey(track.Artist, track.Title)));

    private static T Clone<T>(T value) => JsonSerializer.Deserialize<T>(
        JsonSerializer.Serialize(value))!;

    private const string Alphabet = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    private static string ToBase62(ReadOnlySpan<byte> bytes, int length)
    {
        var value = new System.Numerics.BigInteger(bytes[..16], isUnsigned: true, isBigEndian: true);
        var builder = new StringBuilder(length);
        while (builder.Length < length)
        {
            value = System.Numerics.BigInteger.DivRem(value, 62, out var remainder);
            builder.Append(Alphabet[(int)remainder]);
            if (value.IsZero) break;
        }
        while (builder.Length < length) builder.Append('0');
        return builder.ToString();
    }
}
