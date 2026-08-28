using Microsoft.Extensions.Options;
using Octo.Models.Settings;

namespace Octo.Services.Common;

/// <summary>
/// Background service that periodically cleans up old cached files
/// Only runs when StorageMode is set to Cache
/// </summary>
public class CacheCleanupService : BackgroundService
{
    private readonly IConfiguration _configuration;
    // IOptionsMonitor, not IOptions: the admin UI writes settings.json and the
    // config provider reloads it, but IOptions.Value is resolved once and this is a
    // singleton, so a captured copy would serve startup values until a restart. The
    // admin UI read through IOptionsMonitor and therefore SHOWED the new value while
    // nothing acted on it.
    private readonly IOptionsMonitor<SubsonicSettings> subsonicSettingsOptions;
    private SubsonicSettings _subsonicSettings => subsonicSettingsOptions.CurrentValue;
    private readonly ILogger<CacheCleanupService> _logger;
    private readonly TimeSpan _cleanupInterval = TimeSpan.FromHours(1);

    public CacheCleanupService(
        IConfiguration configuration,
        IOptionsMonitor<SubsonicSettings> subsonicSettings,
        ILogger<CacheCleanupService> logger)
    {
        _configuration = configuration;
        subsonicSettingsOptions = subsonicSettings;
        _logger = logger;
    }

    protected override async Task ExecuteAsync(CancellationToken stoppingToken)
    {
        // Only run if storage mode is Cache
        if (_subsonicSettings.StorageMode != StorageMode.Cache)
        {
            _logger.LogInformation("CacheCleanupService disabled: StorageMode is not Cache");
            return;
        }

        _logger.LogInformation("CacheCleanupService started with cleanup interval of {Interval} and retention of {Hours} hours",
            _cleanupInterval, _subsonicSettings.CacheDurationHours);

        while (!stoppingToken.IsCancellationRequested)
        {
            try
            {
                await CleanupOldCachedFilesAsync(stoppingToken);
                await Task.Delay(_cleanupInterval, stoppingToken);
            }
            catch (OperationCanceledException)
            {
                // Service is stopping, exit gracefully
                break;
            }
            catch (Exception ex)
            {
                _logger.LogError(ex, "Error during cache cleanup");
                // Continue running even if cleanup fails
                await Task.Delay(_cleanupInterval, stoppingToken);
            }
        }

        _logger.LogInformation("CacheCleanupService stopped");
    }

    private async Task CleanupOldCachedFilesAsync(CancellationToken cancellationToken)
    {
        var cachePath = PathHelper.GetCachePath();

        if (!Directory.Exists(cachePath))
        {
            _logger.LogDebug("Cache directory does not exist: {Path}", cachePath);
            return;
        }

        var cutoffTime = DateTime.UtcNow.AddHours(-_subsonicSettings.CacheDurationHours);
        var deletedCount = 0;
        var totalSize = 0L;

        _logger.LogInformation("Starting cache cleanup: deleting files older than {CutoffTime}", cutoffTime);

        try
        {
            // Get all files in cache directory and subdirectories
            var files = Directory.GetFiles(cachePath, "*.*", SearchOption.AllDirectories);

            foreach (var filePath in files)
            {
                if (cancellationToken.IsCancellationRequested)
                    break;

                // Continuous Radio owns this sub-cache. Its prepared starter tracks
                // must outlive the generic one-hour download cache so every issued
                // (12-hour) station URL keeps its immediate-start guarantee.
                var relativePath = Path.GetRelativePath(cachePath, filePath);
                if (relativePath.StartsWith($"radio{Path.DirectorySeparatorChar}",
                        StringComparison.OrdinalIgnoreCase))
                    continue;

                try
                {
                    var fileInfo = new FileInfo(filePath);

                    // Use last access time to determine if file should be deleted
                    // This gets updated when a cached file is streamed
                    if (fileInfo.LastAccessTimeUtc < cutoffTime)
                    {
                        var size = fileInfo.Length;
                        File.Delete(filePath);
                        deletedCount++;
                        totalSize += size;
                        _logger.LogDebug("Deleted cached file: {Path} (last accessed: {LastAccess})",
                            filePath, fileInfo.LastAccessTimeUtc);
                    }
                }
                catch (Exception ex)
                {
                    _logger.LogWarning(ex, "Failed to delete cached file: {Path}", filePath);
                }
            }

            // Clean up empty directories
            await CleanupEmptyDirectoriesAsync(cachePath, cancellationToken);

            if (deletedCount > 0)
            {
                var sizeMB = totalSize / (1024.0 * 1024.0);
                _logger.LogInformation("Cache cleanup completed: deleted {Count} files, freed {Size:F2} MB",
                    deletedCount, sizeMB);
            }
            else
            {
                _logger.LogDebug("Cache cleanup completed: no files to delete");
            }
        }
        catch (Exception ex)
        {
            _logger.LogError(ex, "Error during cache cleanup");
        }
    }

    private async Task CleanupEmptyDirectoriesAsync(string rootPath, CancellationToken cancellationToken)
    {
        try
        {
            var directories = Directory.GetDirectories(rootPath, "*", SearchOption.AllDirectories)
                .OrderByDescending(d => d.Length); // Process deepest directories first

            foreach (var directory in directories)
            {
                if (cancellationToken.IsCancellationRequested)
                    break;

                try
                {
                    if (!Directory.EnumerateFileSystemEntries(directory).Any())
                    {
                        Directory.Delete(directory);
                        _logger.LogDebug("Deleted empty directory: {Path}", directory);
                    }
                }
                catch (Exception ex)
                {
                    _logger.LogWarning(ex, "Failed to delete empty directory: {Path}", directory);
                }
            }
        }
        catch (Exception ex)
        {
            _logger.LogWarning(ex, "Error cleaning up empty directories");
        }

        await Task.CompletedTask;
    }
}
