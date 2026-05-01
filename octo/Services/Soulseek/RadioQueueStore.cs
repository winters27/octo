using System.Collections.Concurrent;

namespace Octo.Services.Soulseek;

/// <summary>
/// Tracks the most recent search/radio responses as ordered queues so we can
/// implement a sliding-window prewarm: when the client scrobbles song N, we
/// look up which queue N belongs to and prewarm songs N+1..N+8 from there.
///
/// Stateless across restarts (in-memory only) and bounded by entry count.
/// Subsonic has no notion of a per-client session so we just keep the last
/// few queues globally; if a user is in two clients at once the most recent
/// queue still wins. That's good enough for "skip-fast" prewarming.
/// </summary>
public class RadioQueueStore
{
    private const int MaxQueues = 32;

    private readonly LinkedList<RadioQueue> _queues = new();
    private readonly object _lock = new();

    public void Register(IEnumerable<string> songIds)
    {
        var ids = songIds.Where(id => !string.IsNullOrEmpty(id)).ToList();
        if (ids.Count == 0) return;
        var queue = new RadioQueue(ids, DateTime.UtcNow);
        lock (_lock)
        {
            _queues.AddFirst(queue);
            while (_queues.Count > MaxQueues) _queues.RemoveLast();
        }
    }

    /// <summary>
    /// Find the most-recently-registered queue containing <paramref name="songId"/>
    /// and return up to <paramref name="count"/> ids that come after it.
    /// Returns an empty list when the song isn't tracked — caller should treat
    /// that as "nothing to prewarm" rather than an error.
    /// </summary>
    public List<string> GetUpcomingFrom(string songId, int count)
    {
        if (string.IsNullOrEmpty(songId) || count <= 0) return new List<string>();
        lock (_lock)
        {
            foreach (var q in _queues)
            {
                var idx = q.Songs.IndexOf(songId);
                if (idx < 0) continue;
                // Move this queue to front so subsequent scrobbles in it stay fast.
                _queues.Remove(q);
                _queues.AddFirst(q);
                return q.Songs.Skip(idx + 1).Take(count).ToList();
            }
        }
        return new List<string>();
    }

    private sealed record RadioQueue(List<string> Songs, DateTime CreatedAt);
}
