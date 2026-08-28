using System.Collections.Concurrent;
using System.Threading.Channels;

namespace Octo.Services.LastFm;

/// <summary>
/// Keeps persisted Radio snapshots ready independently of client traffic. Work stays
/// inside core Octo and uses the temporary Radio cache; it never records a play,
/// scrobbles, learns, or acquires a permanent library copy.
/// </summary>
public sealed class LastFmRadioWarmupService : BackgroundService
{
    private static readonly TimeSpan ReadinessScanInterval = TimeSpan.FromMinutes(1);
    private readonly Channel<string> _users = Channel.CreateBounded<string>(
        new BoundedChannelOptions(100)
        {
            FullMode = BoundedChannelFullMode.DropWrite,
            SingleReader = true,
            SingleWriter = false,
        });
    private readonly ConcurrentDictionary<string, byte> _queued =
        new(StringComparer.OrdinalIgnoreCase);
    private readonly IServiceScopeFactory _scopes;
    private readonly LastFmRadioStateStore _state;
    private readonly ILogger<LastFmRadioWarmupService> _logger;

    public LastFmRadioWarmupService(IServiceScopeFactory scopes,
        LastFmRadioStateStore state, ILogger<LastFmRadioWarmupService> logger)
    {
        _scopes = scopes;
        _state = state;
        _logger = logger;
    }

    public bool QueueUser(string username)
    {
        username = username.Trim();
        if (username.Length == 0 || !_queued.TryAdd(username, 0)) return false;
        if (_users.Writer.TryWrite(username)) return true;
        _queued.TryRemove(username, out _);
        return false;
    }

    public async Task<RadioWarmupResult> ProcessAsync(string username,
        CancellationToken cancellationToken = default)
    {
        using var scope = _scopes.CreateScope();
        var streams = scope.ServiceProvider.GetRequiredService<LastFmRadioStreamService>();
        return await streams.WarmStoredStationsAsync(username, cancellationToken);
    }

    protected override async Task ExecuteAsync(CancellationToken stoppingToken)
    {
        var knownUsers = _state.KnownUsers();
        _logger.LogInformation(
            "Radio startup warm found {ProfileCount} persisted Radio profiles",
            knownUsers.Count);
        foreach (var username in knownUsers) QueueUser(username);
        var scan = ScanReadinessAsync(stoppingToken);
        try
        {
            while (!stoppingToken.IsCancellationRequested)
            {
                string username;
                try { username = await _users.Reader.ReadAsync(stoppingToken); }
                catch (OperationCanceledException) { break; }
                try
                {
                    var result = await ProcessAsync(username, stoppingToken);
                    if (result.ReadyStationCount == result.StationCount)
                        _logger.LogInformation(
                            "Radio cache warm for {User}: {ReadyStations}/{Stations} stations, " +
                            "{ReadyTracks} ready tracks",
                            username, result.ReadyStationCount, result.StationCount,
                            result.ReadyTrackCount);
                    else
                        _logger.LogWarning(
                            "Radio cache warm incomplete for {User}: {ReadyStations}/{Stations} " +
                            "stations; will retry",
                            username, result.ReadyStationCount, result.StationCount);
                }
                catch (OperationCanceledException) when (stoppingToken.IsCancellationRequested)
                {
                    break;
                }
                catch (Exception ex)
                {
                    // Startup dependencies such as the yt-dlp shim may still be coming
                    // online. The minute scan retries without poisoning station state.
                    _logger.LogWarning(ex, "Radio cache warm failed for {User}; will retry", username);
                }
                finally { _queued.TryRemove(username, out _); }
            }
        }
        finally
        {
            try { await scan; }
            catch (OperationCanceledException) when (stoppingToken.IsCancellationRequested) { }
        }
    }

    private async Task ScanReadinessAsync(CancellationToken stoppingToken)
    {
        using var timer = new PeriodicTimer(ReadinessScanInterval);
        while (await timer.WaitForNextTickAsync(stoppingToken))
            foreach (var username in _state.KnownUsers()) QueueUser(username);
    }
}
