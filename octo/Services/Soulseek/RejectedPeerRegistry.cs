using System.Collections.Concurrent;
using System.Text.Json;

namespace Octo.Services.Soulseek;

/// <summary>
/// Remembers the peer-and-file pairs a completed download proved were the wrong recording,
/// so RankCandidates never offers them again.
///
/// Before this, a rejection deleted the bytes and threw the fact away. The next star re-ran
/// the same search, ranked the same peer first for the same reasons, and paid for the same
/// wrong file again. That is issue #40's actual complaint: not that Octo picks badly, but
/// that it cannot learn.
///
/// Shaped like <see cref="ExternalIdRegistry"/>: LRU-bounded, coalesced flush, atomic
/// temp-and-rename, and a file that will not parse is a cold start rather than a failure to
/// boot. The one addition is a TTL, because this list can be WRONG about a file and a
/// permanent deny-list turns one false positive into a track that can never be fetched.
/// </summary>
public class RejectedPeerRegistry : IDisposable
{
    private const int MaxEntries = 10_000;

    /// <summary>
    /// Fallback when no settings are supplied, which is only the case in tests. The real value
    /// is Soulseek:RejectedPeerTtlDays, because how long a denial stands is a judgement about
    /// how much a user trusts the verdict, not an invariant.
    /// </summary>
    internal const int DefaultTtlDays = 30;

    /// <summary>A search registers many candidates, so flushing per write would turn one
    /// download into a burst of file writes. Same reasoning as the id registry.</summary>
    private static readonly TimeSpan FlushInterval = TimeSpan.FromSeconds(10);

    public sealed record Entry(
        string Username, string Filename, string Reason, string Track, DateTime RejectedUtc);

    // Case-insensitive on the whole composite: slskd echoes the peer's own path, and the
    // casing differs between a search response and a transfer record. Two distinct files on
    // one peer never differ by case alone, so nothing is over-denied by that.
    private readonly ConcurrentDictionary<string, Entry> _byKey = new(StringComparer.OrdinalIgnoreCase);
    private readonly LinkedList<string> _lru = new();
    private readonly object _lruLock = new();

    private readonly string? _path;
    private readonly Func<int> _ttlDays;
    private readonly ILogger<RejectedPeerRegistry>? _logger;
    private readonly Timer? _flushTimer;
    private int _dirty;

    public RejectedPeerRegistry(string? path = null, ILogger<RejectedPeerRegistry>? logger = null,
        Func<int>? ttlDays = null)
    {
        _path = string.IsNullOrWhiteSpace(path) ? null : path;
        _ttlDays = ttlDays ?? (() => DefaultTtlDays);
        _logger = logger;
        if (_path is null) return;

        Load();
        _flushTimer = new Timer(_ => Flush(), null, FlushInterval, FlushInterval);
    }

    /// <summary>
    /// Identity of a candidate: the same pair SoulseekFileHit carries and the same pair
    /// EnqueueDownloadAsync and WaitForCompletionAsync already use. Deliberately NOT the
    /// username alone, which would blacklist a whole well-stocked library over one bad rip.
    /// </summary>
    public static string MakeKey(string? username, string? filename) => $"{username}|{filename}";

    /// <summary>Pure so the lapse rule can be tested without waiting a month. 0 days never
    /// lapses, which is a real choice for anyone who would rather clear the list by hand.</summary>
    internal static bool IsExpired(DateTime rejectedUtc, DateTime nowUtc, int ttlDays) =>
        ttlDays > 0 && nowUtc - rejectedUtc >= TimeSpan.FromDays(ttlDays);

    public int Count => _byKey.Count;

    public void Deny(string? username, string? filename, string reason, string track)
    {
        if (string.IsNullOrEmpty(username) || string.IsNullOrEmpty(filename)) return;

        var key = MakeKey(username, filename);
        _byKey[key] = new Entry(username, filename, reason, track, DateTime.UtcNow);
        Touch(key);
        Trim();
        Interlocked.Exchange(ref _dirty, 1);
        _logger?.LogInformation("Remembering rejected candidate {User} -> {File} ({Reason})",
            username, filename, reason);
    }

    /// <summary>
    /// Expiry is enforced here rather than by a sweep timer: a container that runs for
    /// months would otherwise honour a lapsed denial forever, which is the exact trap the
    /// TTL exists to avoid. Called from inside the candidate ranking, so it is a dictionary
    /// hit and nothing more.
    /// </summary>
    public bool IsDenied(string? username, string? filename)
    {
        if (string.IsNullOrEmpty(username) || string.IsNullOrEmpty(filename)) return false;

        var key = MakeKey(username, filename);
        if (!_byKey.TryGetValue(key, out var entry)) return false;
        if (!IsExpired(entry.RejectedUtc, DateTime.UtcNow, _ttlDays())) return true;

        Forget(key);
        Interlocked.Exchange(ref _dirty, 1);
        return false;
    }

    /// <summary>
    /// The recovery lever for a wrong denial. Flushes synchronously rather than waiting for
    /// the timer: a user who clears the list and immediately restarts the container must not
    /// get every entry back, which is what "cleared" would otherwise mean for ten seconds.
    /// </summary>
    public int Clear()
    {
        var removed = _byKey.Count;
        _byKey.Clear();
        lock (_lruLock) _lru.Clear();
        Interlocked.Exchange(ref _dirty, 1);
        Flush();
        return removed;
    }

    private void Forget(string key)
    {
        _byKey.TryRemove(key, out _);
        lock (_lruLock) _lru.Remove(key);
    }

    private void Touch(string key)
    {
        lock (_lruLock)
        {
            _lru.Remove(key);
            _lru.AddFirst(key);
        }
    }

    private void Trim()
    {
        if (_byKey.Count <= MaxEntries) return;
        lock (_lruLock)
        {
            while (_byKey.Count > MaxEntries && _lru.Last is not null)
            {
                var oldest = _lru.Last.Value;
                _lru.RemoveLast();
                _byKey.TryRemove(oldest, out _);
            }
        }
    }

    private void Load()
    {
        try
        {
            if (!File.Exists(_path)) return;
            var entries = JsonSerializer.Deserialize<List<Entry>>(File.ReadAllText(_path!));
            if (entries is null) return;

            var now = DateTime.UtcNow;
            // Stored most-recently-used first, so replaying in order rebuilds the same
            // eviction order. A restart is also when lapsed entries get collected.
            foreach (var entry in entries)
            {
                if (string.IsNullOrEmpty(entry.Username) || string.IsNullOrEmpty(entry.Filename)) continue;
                if (IsExpired(entry.RejectedUtc, now, _ttlDays())) continue;
                var key = MakeKey(entry.Username, entry.Filename);
                _byKey[key] = entry;
                lock (_lruLock) _lru.AddLast(key);
            }
            Trim();
            if (_byKey.Count > 0)
                _logger?.LogInformation("rejected peer registry restored {Count} entries", _byKey.Count);
        }
        catch (Exception ex)
        {
            // A registry that will not load is a cold start, not a failure to boot.
            _logger?.LogWarning("rejected peer registry could not be read: {M}", ex.Message);
        }
    }

    private void Flush()
    {
        if (_path is null) return;
        if (Interlocked.Exchange(ref _dirty, 0) == 0) return;
        try
        {
            List<string> order;
            lock (_lruLock) order = _lru.ToList();

            var entries = new List<Entry>(order.Count);
            foreach (var key in order)
                if (_byKey.TryGetValue(key, out var entry)) entries.Add(entry);

            Directory.CreateDirectory(Path.GetDirectoryName(_path)!);
            var tmp = _path + ".tmp";
            File.WriteAllText(tmp, JsonSerializer.Serialize(entries));
            File.Move(tmp, _path, overwrite: true);
        }
        catch (Exception ex)
        {
            // Best-effort: losing a flush costs the denials since the last one, which is the
            // behaviour we had before this existed. It must never take a download down.
            Interlocked.Exchange(ref _dirty, 1);
            _logger?.LogWarning("rejected peer registry could not be written: {M}", ex.Message);
        }
    }

    public void Dispose()
    {
        _flushTimer?.Dispose();
        Flush();
    }
}
