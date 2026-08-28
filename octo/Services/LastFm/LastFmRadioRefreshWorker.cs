using Microsoft.Extensions.Options;
using Octo.Models.Settings;
using Octo.Services.Common;

namespace Octo.Services.LastFm;

/// <summary>Runs canonical recommendation refreshes inside the existing Octo host.</summary>
public sealed class LastFmRadioRefreshWorker : BackgroundService
{
    private static readonly TimeSpan StaleScanInterval = TimeSpan.FromMinutes(1);
    private readonly LastFmRadioRefreshQueue _queue;
    private readonly IServiceScopeFactory _scopes;
    private readonly LastFmRadioStateStore _state;
    private readonly IOptionsMonitor<LastFmSettings> _settings;
    private readonly SingleFlight<string, bool> _singleFlight = new();
    private readonly ILogger<LastFmRadioRefreshWorker> _logger;
    private readonly LastFmRadioWarmupService? _warmup;
    private Dictionary<string, string> _definitions;
    private bool _radioEnabled;
    private bool _personalizedEnabled;
    public int InFlightCount => _singleFlight.InFlightCount;

    public LastFmRadioRefreshWorker(LastFmRadioRefreshQueue queue, IServiceScopeFactory scopes,
        LastFmRadioStateStore state, IOptionsMonitor<LastFmSettings> settings,
        ILogger<LastFmRadioRefreshWorker> logger, LastFmRadioWarmupService? warmup = null)
    {
        _queue = queue; _scopes = scopes; _state = state; _settings = settings; _logger = logger;
        _warmup = warmup;
        _definitions = Fingerprints(settings.CurrentValue);
        _radioEnabled = settings.CurrentValue.EnableRadio;
        _personalizedEnabled = settings.CurrentValue.EnablePersonalizedStations;
        settings.OnChange(changed =>
        {
            var next = Fingerprints(changed);
            var removed = _definitions.Keys.Except(next.Keys, StringComparer.OrdinalIgnoreCase).Any();
            var rebuildAll = removed
                || (!_radioEnabled && changed.EnableRadio)
                || (_personalizedEnabled != changed.EnablePersonalizedStations);
            foreach (var user in _state.KnownUsers())
            {
                if (rebuildAll) _queue.Enqueue(user);
                else foreach (var pair in next.Where(pair => !_definitions.TryGetValue(pair.Key, out var old)
                                  || old != pair.Value))
                        _queue.Enqueue(user, pair.Key);
            }
            _definitions = next;
            _radioEnabled = changed.EnableRadio;
            _personalizedEnabled = changed.EnablePersonalizedStations;
        });
    }

    private static Dictionary<string, string> Fingerprints(LastFmSettings settings) =>
        settings.EffectiveDiscoveryStations().Where(definition => definition.Enabled).ToDictionary(definition => definition.Id,
            definition => definition.Name + "|" + definition.Enabled + "|" + string.Join('|', definition.Tags),
            StringComparer.OrdinalIgnoreCase);

    protected override async Task ExecuteAsync(CancellationToken stoppingToken)
    {
        // Jitter startup work deterministically so restarts do not fan out all profiles at once.
        foreach (var username in _state.KnownUsers())
        {
            if (stoppingToken.IsCancellationRequested) break;
            var user = _state.GetUser(username);
            var stale = LastFmRadioRefreshPolicy.IsStale(user, _settings.CurrentValue);
            if (stale) _queue.Enqueue(username);
            try { await Task.Delay(LastFmRadioRefreshPolicy.StartupJitter(username), stoppingToken); }
            catch (OperationCanceledException) { return; }
        }

        var staleScan = ScanForStaleUsersAsync(stoppingToken);
        try
        {
            while (!stoppingToken.IsCancellationRequested)
            {
                LastFmRadioRefreshJob job;
                try { job = await _queue.DequeueAsync(stoppingToken); }
                catch (OperationCanceledException) { break; }
                try
                {
                    await ProcessAsync(job);
                }
                catch { /* ProcessAsync recorded the failure; continue draining. */ }
            }
        }
        finally
        {
            try { await staleScan; }
            catch (OperationCanceledException) when (stoppingToken.IsCancellationRequested) { }
        }
    }

    private async Task ScanForStaleUsersAsync(CancellationToken stoppingToken)
    {
        using var timer = new PeriodicTimer(StaleScanInterval);
        while (await timer.WaitForNextTickAsync(stoppingToken))
        {
            var settings = _settings.CurrentValue;
            if (!settings.EnableRadio) continue;
            foreach (var username in _state.KnownUsers())
                if (LastFmRadioRefreshPolicy.ShouldSchedulePeriodicRefresh(
                        _state.GetUser(username), settings))
                    _queue.Enqueue(username);
        }
    }

    public async Task<bool> ProcessAsync(LastFmRadioRefreshJob job)
    {
        try
        {
            return await _singleFlight.RunAsync(job.Username, ct => RefreshAsync(job, ct),
                TimeSpan.FromSeconds(35));
        }
        catch (Exception ex)
        {
            _state.MarkRefreshFailed(job.Username, ex.Message);
            _logger.LogWarning(ex, "Last.fm radio refresh failed for {User}; retaining snapshot", job.Username);
            throw;
        }
    }

    private async Task<bool> RefreshAsync(LastFmRadioRefreshJob job, CancellationToken cancellationToken)
    {
        var settings = _settings.CurrentValue;
        if (!settings.EnableRadio
            || (!settings.EnablePersonalizedStations && !settings.EnableDiscoveryStations)) return false;
        _state.MarkRefreshing(job.Username);
        using var scope = _scopes.CreateScope();
        var service = scope.ServiceProvider.GetRequiredService<LastFmRadioRecommendationService>();
        var built = await service.BuildAsync(job.Username, cancellationToken);
        IReadOnlyCollection<Octo.Models.Radio.LastFmRadioStation> stations = built;
        if (!string.IsNullOrEmpty(job.StationDefinitionId))
        {
            var replacement = built.FirstOrDefault(station =>
                station.Key == "pinned-" + job.StationDefinitionId);
            if (replacement is null) throw new InvalidOperationException("Pinned station returned no usable tracks");
            var merged = _state.GetUser(job.Username).Stations
                .Where(station => station.Key != replacement.Key).ToList();
            merged.Add(replacement);
            stations = merged;
        }
        if (stations.Count == 0 && _state.GetUser(job.Username).Stations.Count > 0)
            throw new InvalidOperationException("Provider returned no usable replacement stations");
        _state.ReplaceStations(job.Username, stations);
        _warmup?.QueueUser(job.Username);
        return true;
    }
}
