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
/// skipped otherwise; a finish later still, within <see cref="LateFinishWindow"/>, turns that skip
/// back into a keep. A finish within <see cref="ShortestKeep"/> of its start is a skip, and a
/// one-star rating is <see cref="DislikeWeight"/> skips. One serving teaches once. Each listener's
/// counts per source fade by half every <see cref="HalfLifeDays"/> days and give that source a
/// multiplier around 1: above it when the listener keeps its songs more than their own average,
/// below when less. <see cref="PriorPlays"/> pretend plays at the average keep a source with few
/// plays near 1. Only the counts are saved; what was served lives in memory for a day.
/// </summary>
public sealed class RadioOutcomeStore
{
    internal static readonly TimeSpan StartWindow = TimeSpan.FromHours(6);
    internal static readonly TimeSpan FinishWindow = TimeSpan.FromHours(24);
    internal static readonly TimeSpan LateFinishWindow = TimeSpan.FromDays(7);
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
    /// <summary>Held from the snapshot to the file move, so an older snapshot never lands last.</summary>
    private readonly object _fileLock = new();
    private readonly Dictionary<(string User, string Song), (string Provider, DateTime ServedUtc, bool Used)> _served = new();
    private readonly Dictionary<(string User, string Song), (string Provider, DateTime StartedUtc)> _open = new();
    /// <summary>Outcomes counted as skips when the finish window closed, kept a week in case the finish arrives late.</summary>
    private readonly Dictionary<(string User, string Song), (string Provider, DateTime SweptUtc)> _swept = new();
    private OutcomeState _state;
    private bool _dirty;
    private DateTime _lastFlush = DateTime.MinValue;

    public RadioOutcomeStore(string path, IOptionsMonitor<RadioSourceSettings> settings, ILogger<RadioOutcomeStore> logger)
    {
        _path = path; _settings = settings; _logger = logger;
        _state = Load();
    }

    private bool On => _settings.CurrentValue.LearnFromListening;

    /// <summary>A finish this soon after its start is a skip; per store, so a test can play a song through at once.</summary>
    internal TimeSpan ShortestKeep { get; set; } = TimeSpan.FromSeconds(30);

    /// <summary>Usernames as Navidrome compares them, without case.</summary>
    private static string Who(string listener) => listener.Trim().ToLowerInvariant();

    /// <summary>These songs went out on a radio for this listener, each from its source.</summary>
    /// <param name="refresh">False for a station: clients that sync playlists fetch them again and
    /// again, and a refetch is not the listener choosing the radio, so it never renews a serving
    /// still remembered. True for song radio: asking again is a new serving.</param>
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
                // Only a source's suggestions teach: a song from the listener's own plays or
                // Navidrome's library picks says nothing about any source.
                if (song.Length == 0 || provider is null || provider is RadioProvider.History or RadioProvider.Library) continue;
                if (!refresh && _served.ContainsKey((who, song))) continue;
                _served[(who, song)] = (provider, at, false);
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
                    // One serving opens one outcome: playing the song again later, from the queue
                    // or the library, is the listener's choice, not the source's suggestion.
                    if (_served.TryGetValue(key, out var served) && !served.Used && now - served.ServedUtc <= StartWindow)
                    {
                        _served[key] = served with { Used = true };
                        _open[key] = (served.Provider, now);
                    }
                }
                else if (_open.Remove(key, out var open))
                {
                    // A finish this soon after its start is a client scrobbling the skip.
                    if (now - open.StartedUtc < ShortestKeep) Count(listener, open.Provider, kept: 0, skipped: 1, now);
                    else Count(listener, open.Provider, kept: 1, skipped: 0, now);
                }
                else if (_swept.Remove(key, out var swept))
                    Count(listener, swept.Provider, kept: 1, skipped: -1, now);
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
            if (_swept.Remove(key, out var swept))
            {
                // Already counted as one skip when its window closed.
                Count(listener, swept.Provider, kept: 0, skipped: DislikeWeight - 1, now);
                return;
            }
            var provider = _open.Remove(key, out var open) ? open.Provider
                : _served.TryGetValue(key, out var served) && now - served.ServedUtc <= FinishWindow ? served.Provider
                : null;
            if (provider is not null) Count(listener, provider, kept: 0, skipped: DislikeWeight, now);
            // One serving is disliked once.
            _served.Remove(key);
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
        lock (_lock) { _state = new OutcomeState(); _served.Clear(); _open.Clear(); _swept.Clear(); _dirty = true; }
        Flush();
    }

    /// <summary>An outcome still open past the finish window was skipped (and is remembered a
    /// week, in case its finish comes late); a song served a day ago is forgotten.</summary>
    private void Sweep(DateTime now)
    {
        foreach (var (key, open) in _open.Where(pair => now - pair.Value.StartedUtc > FinishWindow).ToList())
        {
            _open.Remove(key);
            Count(key.User, open.Provider, kept: 0, skipped: 1, now);
            _swept[key] = (open.Provider, now);
        }
        foreach (var key in _served.Where(pair => now - pair.Value.ServedUtc > FinishWindow).Select(pair => pair.Key).ToList())
            _served.Remove(key);
        foreach (var key in _swept.Where(pair => now - pair.Value.SweptUtc > LateFinishWindow).Select(pair => pair.Key).ToList())
            _swept.Remove(key);
        if (_swept.Count > MaxServed)
            foreach (var old in _swept.OrderBy(pair => pair.Value.SweptUtc).Take(_swept.Count - MaxServed)
                         .Select(pair => pair.Key).ToList())
                _swept.Remove(old);
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
        // A late finish takes back a skip that has faded since: never below none.
        counts.Kept = Math.Max(0, fadedKept + kept);
        counts.Skipped = Math.Max(0, fadedSkipped + skipped);
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
        lock (_fileLock)
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
    }

    private OutcomeState Load()
    {
        try
        {
            if (File.Exists(_path))
            {
                var state = JsonSerializer.Deserialize<OutcomeState>(File.ReadAllText(_path)) ?? new OutcomeState();
                // A hand-edited or half-written file can hold nulls; they are dropped, not crashed on.
                var listeners = new Dictionary<string, Dictionary<string, OutcomeCounts>>(StringComparer.Ordinal);
                foreach (var (listener, byProvider) in state.Listeners ?? [])
                {
                    if (string.IsNullOrWhiteSpace(listener) || byProvider is null) continue;
                    var kept = byProvider.Where(pair => pair.Value is not null)
                        .ToDictionary(pair => pair.Key, pair => pair.Value, StringComparer.Ordinal);
                    if (kept.Count > 0) listeners[listener] = kept;
                }
                state.Listeners = listeners;
                return state;
            }
        }
        catch (Exception ex) { _logger.LogWarning(ex, "{Path} could not be read; radio starts learning again", _path); }
        return new OutcomeState();
    }
}
