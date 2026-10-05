using System.Text.Json;
using Microsoft.Extensions.Options;
using Octo.Models.Settings;

namespace Octo.Services.Radio;

/// <summary>A listener's radio plays from one source, faded by age.</summary>
public sealed class OutcomeCounts
{
    public double Kept { get; set; }
    public double Skipped { get; set; }
    public DateTime UpdatedUtc { get; set; }
}

public sealed class OutcomeState
{
    /// <summary>Listener, then provider key.</summary>
    public Dictionary<string, Dictionary<string, OutcomeCounts>> Listeners { get; set; } = new(StringComparer.Ordinal);
}

/// <summary>One source on the dashboard: plays counted across listeners, how many were kept, and
/// the range of the multipliers learning gives it.</summary>
public sealed record ProviderOutcome(string Provider, string Name, int Plays, double KeepRate,
    double LowestMultiplier, double HighestMultiplier);

/// <summary>
/// What radio learns from listening. A song a source suggested, started within
/// <see cref="StartWindow"/> of being served, is kept when a finished scrobble follows within
/// <see cref="FinishWindow"/> (the Octo apps can send that late, from an offline queue), and
/// skipped otherwise; a one-star rating is <see cref="DislikeWeight"/> skips. Each listener's
/// counts per source fade by half every <see cref="HalfLifeDays"/> days and give that source a
/// multiplier around 1: above it when the listener keeps its songs more than their own average,
/// below when less. <see cref="PriorPlays"/> pretend plays at the average keep a source with few
/// plays near 1. Only the counts are saved; what was served lives in memory for a day.
/// </summary>
public sealed class RadioOutcomeStore
{
    internal static readonly TimeSpan StartWindow = TimeSpan.FromHours(6);
    internal static readonly TimeSpan FinishWindow = TimeSpan.FromHours(24);
    internal const double HalfLifeDays = 60;
    internal const double PriorPlays = 30;
    internal const double LowestMultiplier = 0.5;
    internal const double HighestMultiplier = 1.5;
    internal const double DislikeWeight = 3;
    internal const int MaxServed = 20_000;
    /// <summary>Listeners remembered, like the radio state's own cap; the longest quiet is forgotten first.</summary>
    internal const int MaxListeners = 100;
    private static readonly TimeSpan FlushEvery = TimeSpan.FromSeconds(5);
    private static readonly IReadOnlyDictionary<string, double> None = new Dictionary<string, double>();

    private readonly string _path;
    private readonly IOptionsMonitor<RadioSourceSettings> _settings;
    private readonly ILogger<RadioOutcomeStore> _logger;
    private readonly object _lock = new();
    private readonly Dictionary<(string User, string Song), (string Provider, DateTime ServedUtc)> _served = new();
    private readonly Dictionary<(string User, string Song), (string Provider, DateTime StartedUtc)> _open = new();
    private OutcomeState _state;
    private bool _dirty;
    private DateTime _lastFlush = DateTime.MinValue;

    public RadioOutcomeStore(string path, IOptionsMonitor<RadioSourceSettings> settings, ILogger<RadioOutcomeStore> logger)
    {
        _path = path; _settings = settings; _logger = logger;
        _state = Load();
    }

    private bool On => _settings.CurrentValue.LearnFromListening;

    /// <summary>Usernames as Navidrome compares them, without case.</summary>
    private static string Who(string listener) => listener.Trim().ToLowerInvariant();

    /// <summary>These songs went out on a radio for this listener, each from its source.</summary>
    /// <param name="refresh">False for a station: clients that sync playlists fetch them again and
    /// again, and a refetch is not the listener choosing the radio, so it does not renew the window.</param>
    public void Served(string? listener, IEnumerable<(string SongId, string? Provider)> songs,
        DateTime? now = null, bool refresh = true)
    {
        if (!On || string.IsNullOrWhiteSpace(listener)) return;
        var at = now ?? DateTime.UtcNow;
        var who = Who(listener);
        lock (_lock)
        {
            Sweep(at);
            foreach (var (song, provider) in songs)
            {
                if (song.Length == 0 || provider is null || provider == RadioProvider.History) continue;
                if (!refresh && _served.TryGetValue((who, song), out var earlier) && at - earlier.ServedUtc <= StartWindow) continue;
                _served[(who, song)] = (provider, at);
            }
            if (_served.Count > MaxServed)
                foreach (var old in _served.OrderBy(pair => pair.Value.ServedUtc).Take(_served.Count - MaxServed)
                             .Select(pair => pair.Key).ToList())
                    _served.Remove(old);
        }
    }

    /// <summary>
    /// A scrobble Navidrome accepted for this listener. A start of a served song opens an outcome;
    /// a finish closes it as kept. A finish with no start seen teaches nothing: a client that never
    /// sends starts would otherwise only ever count keeps.
    /// </summary>
    public void Observe(string listener, IReadOnlyList<string> ids, IReadOnlyList<string> submissions, DateTime now)
    {
        if (!On || string.IsNullOrWhiteSpace(listener)) return;
        listener = Who(listener);
        lock (_lock)
        {
            Sweep(now);
            for (var index = 0; index < ids.Count; index++)
            {
                // Read as LearnFromScrobblesAsync reads it: one submission for several ids is for
                // all of them, and none at all means a completed play.
                var completed = submissions.Count == 1
                    ? IsTrue(submissions[0])
                    : index >= submissions.Count || IsTrue(submissions[index]);
                var key = (listener, ids[index]);
                if (!completed)
                {
                    if (_served.TryGetValue(key, out var served) && now - served.ServedUtc <= StartWindow)
                        _open[key] = (served.Provider, now);
                }
                else if (_open.Remove(key, out var open))
                    Count(listener, open.Provider, kept: 1, skipped: 0, now);
            }
        }
        FlushIfDue();
    }

    /// <summary>One star for a song radio just played counts as several skips for its source.</summary>
    public void Disliked(string listener, string songId, DateTime now)
    {
        if (!On || string.IsNullOrWhiteSpace(listener)) return;
        listener = Who(listener);
        lock (_lock)
        {
            var key = (listener, songId);
            var provider = _open.Remove(key, out var open) ? open.Provider
                : _served.TryGetValue(key, out var served) && now - served.ServedUtc <= FinishWindow ? served.Provider
                : null;
            if (provider is not null) Count(listener, provider, kept: 0, skipped: DislikeWeight, now);
        }
        FlushIfDue();
    }

    /// <summary>Each source's multiplier for this listener; empty with no data or learning off.</summary>
    public IReadOnlyDictionary<string, double> Multipliers(string? listener, DateTime? now = null)
    {
        if (!On || string.IsNullOrWhiteSpace(listener)) return None;
        var at = now ?? DateTime.UtcNow;
        lock (_lock)
        {
            Sweep(at);
            return _state.Listeners.TryGetValue(Who(listener), out var byProvider)
                ? MultipliersOf(byProvider.ToDictionary(pair => pair.Key, pair => Faded(pair.Value, at)))
                : None;
        }
    }

    internal static Dictionary<string, double> MultipliersOf(IReadOnlyDictionary<string, (double Kept, double Skipped)> counts)
    {
        var kept = counts.Values.Sum(count => count.Kept);
        var all = counts.Values.Sum(count => count.Kept + count.Skipped);
        var average = (kept + 1) / (all + 2);
        return counts.ToDictionary(pair => pair.Key, pair =>
        {
            var rate = (pair.Value.Kept + PriorPlays * average) / (pair.Value.Kept + pair.Value.Skipped + PriorPlays);
            return Math.Clamp(rate / average, LowestMultiplier, HighestMultiplier);
        });
    }

    public IReadOnlyList<ProviderOutcome> Stats(DateTime? now = null)
    {
        var at = now ?? DateTime.UtcNow;
        lock (_lock)
        {
            Sweep(at);
            var perListener = _state.Listeners.ToDictionary(listener => listener.Key,
                listener => listener.Value.ToDictionary(pair => pair.Key, pair => Faded(pair.Value, at)));
            var multipliers = perListener.Values.Select(MultipliersOf).ToList();
            return perListener.Values.SelectMany(byProvider => byProvider.Keys).Distinct().Order()
                .Select(provider =>
                {
                    var counts = perListener.Values.Where(v => v.ContainsKey(provider)).Select(v => v[provider]).ToList();
                    var kept = counts.Sum(count => count.Kept);
                    var total = counts.Sum(count => count.Kept + count.Skipped);
                    var range = multipliers.Where(m => m.ContainsKey(provider)).Select(m => m[provider]).DefaultIfEmpty(1).ToList();
                    return new ProviderOutcome(provider, RadioProvider.DisplayName(provider) ?? provider,
                        (int)Math.Round(total), total > 0 ? kept / total : 0, range.Min(), range.Max());
                }).ToList();
        }
    }

    /// <summary>Forget everything learned, from the dashboard.</summary>
    public void Forget()
    {
        lock (_lock) { _state = new OutcomeState(); _served.Clear(); _open.Clear(); _dirty = true; }
        Flush();
    }

    /// <summary>An outcome still open past the finish window was skipped; a song served a day ago is forgotten.</summary>
    private void Sweep(DateTime now)
    {
        foreach (var (key, open) in _open.Where(pair => now - pair.Value.StartedUtc > FinishWindow).ToList())
        {
            _open.Remove(key);
            Count(key.User, open.Provider, kept: 0, skipped: 1, now);
        }
        foreach (var key in _served.Where(pair => now - pair.Value.ServedUtc > FinishWindow).Select(pair => pair.Key).ToList())
            _served.Remove(key);
    }

    private void Count(string listener, string provider, double kept, double skipped, DateTime now)
    {
        if (!_state.Listeners.TryGetValue(listener, out var byProvider))
        {
            if (_state.Listeners.Count >= MaxListeners)
                _state.Listeners.Remove(_state.Listeners
                    .OrderBy(pair => pair.Value.Values.Select(c => c.UpdatedUtc).DefaultIfEmpty(DateTime.MinValue).Max()).First().Key);
            _state.Listeners[listener] = byProvider = new Dictionary<string, OutcomeCounts>(StringComparer.Ordinal);
        }
        if (!byProvider.TryGetValue(provider, out var counts))
            byProvider[provider] = counts = new OutcomeCounts { UpdatedUtc = now };
        var (fadedKept, fadedSkipped) = Faded(counts, now);
        counts.Kept = fadedKept + kept;
        counts.Skipped = fadedSkipped + skipped;
        counts.UpdatedUtc = now;
        _dirty = true;
    }

    private static (double Kept, double Skipped) Faded(OutcomeCounts counts, DateTime now)
    {
        var days = Math.Max(0, (now - counts.UpdatedUtc).TotalDays);
        var keep = Math.Pow(0.5, days / HalfLifeDays);
        return (counts.Kept * keep, counts.Skipped * keep);
    }

    private static bool IsTrue(string value) =>
        value.Equals("true", StringComparison.OrdinalIgnoreCase) || value == "1";

    private void FlushIfDue()
    {
        if (DateTime.UtcNow - _lastFlush >= FlushEvery) Flush();
    }

    public void Flush()
    {
        string json;
        lock (_lock)
        {
            if (!_dirty) return;
            json = JsonSerializer.Serialize(_state);
            _dirty = false;
            _lastFlush = DateTime.UtcNow;
        }
        try
        {
            Directory.CreateDirectory(Path.GetDirectoryName(_path)!);
            var temp = _path + ".tmp";
            File.WriteAllText(temp, json);
            File.Move(temp, _path, overwrite: true);
        }
        catch (Exception ex) { _logger.LogWarning(ex, "Could not save {Path}", _path); lock (_lock) _dirty = true; }
    }

    private OutcomeState Load()
    {
        try
        {
            if (File.Exists(_path))
                return JsonSerializer.Deserialize<OutcomeState>(File.ReadAllText(_path)) ?? new OutcomeState();
        }
        catch (Exception ex) { _logger.LogWarning(ex, "{Path} could not be read; radio starts learning again", _path); }
        return new OutcomeState();
    }
}
