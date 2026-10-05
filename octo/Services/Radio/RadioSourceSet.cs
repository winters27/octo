using Microsoft.Extensions.Options;
using Octo.Models.Settings;

namespace Octo.Services.Radio;

/// <summary>Every radio source, asked together. One slow or failing source costs its own answer, never the radio.</summary>
public sealed class RadioSourceSet(IEnumerable<IRadioSource> sources,
    IOptionsMonitor<RadioSourceSettings> settings, ILogger<RadioSourceSet> logger)
{
    internal static TimeSpan SourceTimeout { get; set; } = TimeSpan.FromSeconds(8);
    /// <summary>Failures or timeouts in a row before a source is rested, and for how long. A
    /// source that hangs would otherwise cost every radio its full timeout, and a station build
    /// one timeout per seed.</summary>
    internal const int FailuresBeforeRest = 3;
    internal static TimeSpan Rest { get; set; } = TimeSpan.FromMinutes(5);
    private readonly System.Collections.Concurrent.ConcurrentDictionary<string, (int Failures, DateTime RestUntil)> _health = new();
    internal const double LowestWeight = 0.3;
    internal const double HighestWeight = 1.5;
    private readonly IReadOnlyList<IRadioSource> _sources = sources.ToList();

    public IReadOnlyList<IRadioSource> Available => _sources.Where(source => source.Available
        && !(_health.TryGetValue(source.Provider, out var health) && health.RestUntil > DateTime.UtcNow)).ToList();

    /// <summary>
    /// What each source counts for this radio: its base weight, raised for Sounds alike when no
    /// catalog knows the song, times what this listener's plays have taught (1 with no data),
    /// kept within <see cref="LowestWeight"/> and <see cref="HighestWeight"/>. A base of 0 means
    /// the admin left the source out, and stays 0.
    /// </summary>
    public IReadOnlyDictionary<string, double> Weights(string? listener = null, bool libraryLed = false)
    {
        var current = settings.CurrentValue;
        var learned = new Dictionary<string, double>();
        var sound = RadioSourceSettings.EffectiveWeight(current.SoundsAlikeWeight);
        if (libraryLed && sound > 0) sound = Math.Max(sound, RadioBlend.SoundsAlikeWhenUnknown);
        // The bounds never undo the admin: a base set below 0.3 or above 1.5 is itself the bound.
        double Weigh(string provider, double baseWeight) => baseWeight <= 0 ? 0
            : Math.Clamp(baseWeight * learned.GetValueOrDefault(provider, 1.0),
                Math.Min(LowestWeight, baseWeight), Math.Max(HighestWeight, baseWeight));
        return new Dictionary<string, double>
        {
            [RadioProvider.LastFm] = Weigh(RadioProvider.LastFm, RadioSourceSettings.EffectiveWeight(current.LastFmWeight)),
            [RadioProvider.YouTubeMusic] = Weigh(RadioProvider.YouTubeMusic, RadioSourceSettings.EffectiveWeight(current.YouTubeMusicWeight)),
            [RadioProvider.ListenBrainz] = Weigh(RadioProvider.ListenBrainz, RadioSourceSettings.EffectiveWeight(current.ListenBrainzWeight)),
            [RadioProvider.SoundsAlike] = Weigh(RadioProvider.SoundsAlike, sound),
            [RadioProvider.Library] = 1.0,
        };
    }

    public Task<RadioAnswer[]> AskAllAsync(RadioSeed seed, int count,
        IReadOnlyDictionary<string, string> auth, CancellationToken ct) =>
        Task.WhenAll(Available.Select(source => AskAsync(source, ct,
            token => source.SongsLikeAsync(seed, count, auth, token))));

    public Task<RadioAnswer[]> TagAllAsync(string tag, int count, CancellationToken ct) =>
        Task.WhenAll(Available.Select(source => AskAsync(source, ct, token => source.TagAsync(tag, count, token))));

    private async Task<RadioAnswer> AskAsync(IRadioSource source, CancellationToken ct,
        Func<CancellationToken, Task<RadioAnswer>> ask)
    {
        using var timeout = CancellationTokenSource.CreateLinkedTokenSource(ct);
        timeout.CancelAfter(SourceTimeout);
        try
        {
            var answer = await ask(timeout.Token);
            _health.TryRemove(source.Provider, out _);
            return answer;
        }
        catch (OperationCanceledException) when (!ct.IsCancellationRequested)
        {
            logger.LogInformation("Radio source {Provider} took longer than {Seconds} s; left out", source.Provider, SourceTimeout.TotalSeconds);
            Failed(source.Provider);
            return RadioAnswer.Nothing(source.Provider);
        }
        catch (Exception ex) when (ex is not OperationCanceledException)
        {
            logger.LogWarning(ex, "Radio source {Provider} failed; left out", source.Provider);
            Failed(source.Provider);
            return RadioAnswer.Nothing(source.Provider);
        }
    }

    private void Failed(string provider)
    {
        var health = _health.AddOrUpdate(provider, _ => (1, DateTime.MinValue),
            (_, old) => (old.Failures + 1, old.RestUntil));
        if (health.Failures < FailuresBeforeRest) return;
        _health[provider] = (0, DateTime.UtcNow + Rest);
        logger.LogWarning("Radio source {Provider} failed {Count} times in a row; resting it for {Minutes} minutes",
            provider, FailuresBeforeRest, Rest.TotalMinutes);
    }
}
