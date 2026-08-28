using System.Security.Cryptography;

namespace Octo.Services.LastFm;

public sealed record LastFmRadioStreamSession(
    string Token,
    string Username,
    string StationId,
    IReadOnlyDictionary<string, string> Authentication,
    DateTime ExpiresUtc,
    IReadOnlyList<PreparedRadioTrack>? ReadyPool = null);

/// <summary>
/// Bounded in-memory authorization bridge between an authenticated Subsonic
/// station-list request and the credential-free streamUrl a radio client opens.
/// Tokens disappear on restart and never expose usernames or Navidrome secrets.
/// </summary>
public sealed class LastFmRadioStreamSessionStore
{
    internal const int MaximumSessions = 1024;
    private static readonly TimeSpan Lifetime = TimeSpan.FromHours(12);
    private static readonly HashSet<string> AuthenticationKeys =
        new(["u", "p", "t", "s", "v", "c"], StringComparer.OrdinalIgnoreCase);
    private readonly object _lock = new();
    private readonly Dictionary<string, LastFmRadioStreamSession> _sessions =
        new(StringComparer.Ordinal);

    public string Issue(string username, string stationId,
        IReadOnlyDictionary<string, string> requestParameters, DateTime? nowUtc = null)
    {
        var now = nowUtc ?? DateTime.UtcNow;
        var auth = requestParameters
            .Where(pair => AuthenticationKeys.Contains(pair.Key))
            .ToDictionary(pair => pair.Key, pair => pair.Value, StringComparer.OrdinalIgnoreCase);
        var token = Convert.ToHexString(RandomNumberGenerator.GetBytes(24)).ToLowerInvariant();
        var session = new LastFmRadioStreamSession(token, username, stationId, auth,
            now.Add(Lifetime));

        lock (_lock)
        {
            PruneLocked(now);
            while (_sessions.Count >= MaximumSessions)
            {
                var oldest = _sessions.Values.MinBy(item => item.ExpiresUtc);
                if (oldest is null) break;
                _sessions.Remove(oldest.Token);
            }
            _sessions[token] = session;
        }
        return token;
    }

    public LastFmRadioStreamSession? Get(string token, DateTime? nowUtc = null)
    {
        var now = nowUtc ?? DateTime.UtcNow;
        lock (_lock)
        {
            PruneLocked(now);
            return _sessions.TryGetValue(token, out var session) ? session : null;
        }
    }

    public bool AttachReadyPool(string token, IReadOnlyList<PreparedRadioTrack> readyPool,
        DateTime? nowUtc = null)
    {
        var now = nowUtc ?? DateTime.UtcNow;
        lock (_lock)
        {
            PruneLocked(now);
            if (!_sessions.TryGetValue(token, out var session)) return false;
            _sessions[token] = session with { ReadyPool = readyPool.ToList() };
            return true;
        }
    }

    public void ConsumeReadyTrack(string token, string cacheKey)
    {
        lock (_lock)
        {
            if (!_sessions.TryGetValue(token, out var session)) return;
            var pool = session.ReadyPool?.ToList() ?? [];
            var index = pool.FindIndex(item => item.CacheKey == cacheKey);
            if (index >= 0) pool.RemoveAt(index);
            _sessions[token] = session with { ReadyPool = pool };
        }
    }

    public void AppendReadyTrack(string token, PreparedRadioTrack track, int maximum)
    {
        lock (_lock)
        {
            if (!_sessions.TryGetValue(token, out var session)) return;
            var pool = session.ReadyPool?.ToList() ?? [];
            if (pool.All(item => item.CacheKey != track.CacheKey)) pool.Add(track);
            if (pool.Count > maximum) pool.RemoveRange(maximum, pool.Count - maximum);
            _sessions[token] = session with { ReadyPool = pool };
        }
    }

    public void Remove(string token)
    {
        lock (_lock) _sessions.Remove(token);
    }

    internal int Count { get { lock (_lock) return _sessions.Count; } }

    private void PruneLocked(DateTime nowUtc)
    {
        foreach (var token in _sessions.Where(pair => pair.Value.ExpiresUtc <= nowUtc)
                     .Select(pair => pair.Key).ToList())
            _sessions.Remove(token);
    }
}
