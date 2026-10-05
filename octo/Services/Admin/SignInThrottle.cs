using System.Collections.Concurrent;

namespace Octo.Services.Admin;

/// <summary>
/// Slows down guessing at the dashboard's sign-in: after ten wrong tries from one address in
/// fifteen minutes, that address waits until the oldest try is fifteen minutes old. In memory only.
/// Behind a reverse proxy every request comes from the proxy, so this becomes one shared limit;
/// that can hold up a real sign-in for fifteen minutes, never let a guess through.
/// </summary>
public sealed class SignInThrottle
{
    public const int MaxFailures = 10;
    public static readonly TimeSpan Window = TimeSpan.FromMinutes(15);

    private readonly ConcurrentDictionary<string, Queue<DateTime>> _failures = new(StringComparer.Ordinal);

    internal Func<DateTime> Clock { get; set; } = () => DateTime.UtcNow;

    /// <summary>How long this address must wait, or null when it may try now.</summary>
    public TimeSpan? RetryAfter(string key)
    {
        if (!_failures.TryGetValue(key, out var tries)) return null;
        lock (tries)
        {
            var now = Clock();
            while (tries.Count > 0 && now - tries.Peek() >= Window) tries.Dequeue();
            return tries.Count >= MaxFailures ? tries.Peek() + Window - now : null;
        }
    }

    public void Failed(string key)
    {
        var tries = _failures.GetOrAdd(key, _ => new Queue<DateTime>());
        var now = Clock();
        lock (tries)
        {
            while (tries.Count > 0 && now - tries.Peek() >= Window) tries.Dequeue();
            tries.Enqueue(now);
        }
        // A flood from many addresses must not grow this forever: drop the ones with nothing recent.
        if (_failures.Count > 10_000)
            foreach (var kv in _failures)
                lock (kv.Value)
                    if (kv.Value.Count == 0 || now - kv.Value.Last() >= Window)
                        _failures.TryRemove(kv.Key, out _);
    }

    public void Succeeded(string key) => _failures.TryRemove(key, out _);

    /// <summary>The key for a request: its remote address.</summary>
    public static string KeyOf(HttpContext context) => context.Connection.RemoteIpAddress?.ToString() ?? "unknown";

    /// <summary>A wait in words: "1 minute", "12 minutes".</summary>
    public static string Minutes(TimeSpan wait)
    {
        var n = Math.Max(1, (int)Math.Ceiling(wait.TotalMinutes));
        return n == 1 ? "1 minute" : $"{n} minutes";
    }
}
