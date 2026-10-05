using System.Diagnostics;

namespace Octo.Services.Library;

/// <summary>
/// The whole library, read through Navidrome's own song list and kept, for the questions a
/// search asks of every row at once (which outside albums the library already holds). Reading
/// it is a few large requests, so it is never done on a search's own time once there is a copy:
/// a copy older than <see cref="FreshFor"/> is still answered with while a new one is read, and
/// only one read runs at a time. Needs the Navidrome admin sign-in, as Spotify import does; with
/// none the answer is null and searches go out as before.
/// </summary>
public sealed class LibrarySnapshot
{
    /// <summary>A copy this young is answered with as it is.</summary>
    internal static readonly TimeSpan FreshFor = TimeSpan.FromMinutes(5);

    /// <summary>A copy older than this is not answered with at all: the next caller waits for a read.</summary>
    internal static readonly TimeSpan UsableFor = TimeSpan.FromHours(1);

    /// <summary>After a read that failed, how long callers are answered "unknown" before another is tried.</summary>
    internal static readonly TimeSpan RetryAfter = TimeSpan.FromMinutes(1);

    private const int PageSize = 1000;

    private readonly ILogger<LibrarySnapshot> _logger;
    private readonly object _gate = new();
    private (LibraryAlbumIndex Index, DateTime At)? _current;
    private Task<LibraryAlbumIndex?>? _reading;
    private DateTime? _failedAt;

    public LibrarySnapshot(NavidromePlaylistApi navidrome, ILogger<LibrarySnapshot> logger)
    {
        _logger = logger;
        ReadPage = (start, count, ct) => navidrome.ListSongsAsync(start, count, ct);
    }

    /// <summary>One page of the library; null when Navidrome could not be read. Tests answer from a list.</summary>
    internal Func<int, int, CancellationToken, Task<(IReadOnlyList<LibrarySongRow> Rows, int Count)?>> ReadPage { get; set; }
    internal Func<DateTime> Clock { get; set; } = () => DateTime.UtcNow;

    /// <summary>The library as last read, reading it first when there is no usable copy.</summary>
    public Task<LibraryAlbumIndex?> CurrentAsync(CancellationToken ct = default)
    {
        Task<LibraryAlbumIndex?> reading;
        lock (_gate)
        {
            if (_current is { } current)
            {
                var age = Clock() - current.At;
                if (age < FreshFor) return Task.FromResult<LibraryAlbumIndex?>(current.Index);
                if (age < UsableFor)
                {
                    _reading ??= ReadAsync();
                    return Task.FromResult<LibraryAlbumIndex?>(current.Index);
                }
            }
            if (_reading is null && _failedAt is { } failed && Clock() - failed < RetryAfter)
                return Task.FromResult<LibraryAlbumIndex?>(null);
            reading = _reading ??= ReadAsync();
        }
        return reading.WaitAsync(ct);
    }

    private async Task<LibraryAlbumIndex?> ReadAsync()
    {
        // Off the caller's thread and without its token: one caller giving up does not stop a
        // read every later caller will use.
        await Task.Yield();
        try
        {
            var clock = Stopwatch.StartNew();
            var rows = new List<LibrarySongRow>();
            for (var start = 0; ; start += PageSize)
            {
                if (await ReadPage(start, PageSize, CancellationToken.None) is not var (page, count))
                {
                    _logger.LogDebug("Could not read the library for album ownership");
                    lock (_gate) _failedAt = Clock();
                    return null;
                }
                rows.AddRange(page);
                if (count < PageSize) break;
            }
            var index = new LibraryAlbumIndex(rows);
            lock (_gate)
            {
                _current = (index, Clock());
                _failedAt = null;
            }
            _logger.LogDebug("Read {Count} library songs for album ownership in {Ms} ms", rows.Count, clock.ElapsedMilliseconds);
            return index;
        }
        catch (Exception ex)
        {
            _logger.LogDebug("Could not read the library for album ownership: {M}", ex.Message);
            lock (_gate) _failedAt = Clock();
            return null;
        }
        finally
        {
            lock (_gate) _reading = null;
        }
    }
}
