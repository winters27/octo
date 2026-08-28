using System.Collections.Concurrent;
using System.Threading.Channels;

namespace Octo.Services.LastFm;

/// <summary>Bounded in-process refresh queue. Duplicate user jobs collapse while queued.</summary>
public sealed record LastFmRadioRefreshJob(string Username, string? StationDefinitionId = null);

public sealed class LastFmRadioRefreshQueue
{
    private readonly Channel<LastFmRadioRefreshJob> _jobs = Channel.CreateBounded<LastFmRadioRefreshJob>(new BoundedChannelOptions(100)
    {
        FullMode = BoundedChannelFullMode.DropWrite,
        SingleReader = true,
        SingleWriter = false,
    });
    private readonly ConcurrentDictionary<string, byte> _queued = new(StringComparer.OrdinalIgnoreCase);

    public bool Enqueue(string username, string? stationDefinitionId = null)
    {
        username = username.Trim();
        var key = username + "|" + stationDefinitionId;
        if (username.Length == 0 || !_queued.TryAdd(key, 0)) return false;
        if (_jobs.Writer.TryWrite(new LastFmRadioRefreshJob(username, stationDefinitionId))) return true;
        _queued.TryRemove(key, out _);
        return false;
    }

    public async ValueTask<LastFmRadioRefreshJob> DequeueAsync(CancellationToken cancellationToken)
    {
        var job = await _jobs.Reader.ReadAsync(cancellationToken);
        _queued.TryRemove(job.Username + "|" + job.StationDefinitionId, out _);
        return job;
    }
}
