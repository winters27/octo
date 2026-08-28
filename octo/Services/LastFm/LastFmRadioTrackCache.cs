using System.Security.Cryptography;
using System.Text;
using Octo.Services.Common;

namespace Octo.Services.LastFm;

/// <summary>
/// Bounded temporary storage for fully transcoded Radio starter tracks. This is
/// deliberately the normal Octo cache, not the music library: preparing a
/// station must never turn listening into a permanent acquisition.
/// </summary>
public sealed class LastFmRadioTrackCache
{
    private const long MaximumBytes = 512L * 1024 * 1024;
    private static readonly TimeSpan Retention = TimeSpan.FromHours(24);
    private static readonly TimeSpan PruneInterval = TimeSpan.FromHours(1);
    private readonly string _root;
    private readonly SingleFlight<string, string> _singleFlight = new();
    private readonly SemaphoreSlim _transcodeSlots = new(2, 2);
    private readonly object _pruneLock = new();
    private DateTime _nextPruneUtc = DateTime.MinValue;

    public LastFmRadioTrackCache() : this(Path.Combine(PathHelper.GetCachePath(), "radio")) { }

    internal LastFmRadioTrackCache(string root) => _root = root;

    public string Key(string username, string stationId, string trackIdentity, int bitrateKbps)
    {
        var material = string.Join('\n', username.Trim().ToUpperInvariant(), stationId,
            trackIdentity, bitrateKbps);
        return Convert.ToHexString(SHA256.HashData(Encoding.UTF8.GetBytes(material)))
            .ToLowerInvariant();
    }

    public async Task<string> GetOrCreateAsync(string key,
        Func<Stream, CancellationToken, Task> producer, CancellationToken cancellationToken)
    {
        Directory.CreateDirectory(_root);
        var path = Path.Combine(_root, $"{key}.mp3");
        if (IsReady(path)) return Touch(path);

        var work = _singleFlight.RunAsync(key, async token =>
        {
            if (IsReady(path)) return Touch(path);
            await _transcodeSlots.WaitAsync(token);
            var temporaryPath = Path.Combine(_root, $".{key}.{Guid.NewGuid():N}.tmp");
            try
            {
                await using (var output = new FileStream(temporaryPath, FileMode.CreateNew,
                    FileAccess.Write, FileShare.None, 81920, FileOptions.Asynchronous))
                    await producer(output, token);
                if (!IsReady(temporaryPath))
                    throw new InvalidOperationException("Radio starter transcode produced no audio");
                File.Move(temporaryPath, path, overwrite: true);
                PruneIfDue(path);
                return path;
            }
            finally
            {
                _transcodeSlots.Release();
                try { if (File.Exists(temporaryPath)) File.Delete(temporaryPath); }
                catch { /* cleanup is best effort */ }
            }
        }, TimeSpan.FromMinutes(15));
        return await work.WaitAsync(cancellationToken);
    }

    public string? GetReadyPath(string key)
    {
        var path = Path.Combine(_root, $"{key}.mp3");
        return IsReady(path) ? Touch(path) : null;
    }

    public FileStream OpenRead(string path)
    {
        Touch(path);
        return new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.Read, 81920,
            FileOptions.Asynchronous | FileOptions.SequentialScan);
    }

    public bool IsReadyPath(string path) => IsReady(path);

    private static bool IsReady(string path) => File.Exists(path) && new FileInfo(path).Length > 0;

    private static string Touch(string path)
    {
        try { File.SetLastAccessTimeUtc(path, DateTime.UtcNow); }
        catch { /* access-time support varies by filesystem */ }
        return path;
    }

    private void PruneIfDue(string protectedPath)
    {
        lock (_pruneLock)
        {
            var now = DateTime.UtcNow;
            if (now < _nextPruneUtc) return;
            _nextPruneUtc = now.Add(PruneInterval);
            var files = new DirectoryInfo(_root).EnumerateFiles("*.mp3")
                .Where(file => !file.FullName.Equals(protectedPath, StringComparison.Ordinal))
                .OrderBy(file => file.LastAccessTimeUtc).ToList();
            foreach (var file in files.Where(file => file.LastAccessTimeUtc < now - Retention))
                TryDelete(file);
            var remaining = new DirectoryInfo(_root).EnumerateFiles("*.mp3")
                .OrderBy(file => file.LastAccessTimeUtc).ToList();
            var total = remaining.Sum(file => file.Length);
            foreach (var file in remaining)
            {
                if (total <= MaximumBytes) break;
                if (file.FullName.Equals(protectedPath, StringComparison.Ordinal)) continue;
                var length = file.Length;
                if (TryDelete(file)) total -= length;
            }
        }
    }

    private static bool TryDelete(FileInfo file)
    {
        try { file.Delete(); return true; }
        catch { return false; }
    }
}
