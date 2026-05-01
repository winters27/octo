using Microsoft.Extensions.Options;
using Octo.Models.Domain;
using Octo.Models.Settings;
using Octo.Services.Common;
using Octo.Services.Local;
using Octo.Services.YouTube;
using IOFile = System.IO.File;

namespace Octo.Services.Soulseek;

/// <summary>
/// Hybrid download service:
///   - GetDirectStreamAsync   -> instant lossy preview via YouTube (yt-dlp)
///   - DownloadTrackAsync     -> permanent FLAC fetch via slskd, triggered ONLY
///                              when the user stars a track. Soulseek is searched
///                              here on demand using the encoded artist+title.
/// </summary>
public class SoulseekDownloadService : BaseDownloadService
{
    private readonly SoulseekClient _slskd;
    private readonly SoulseekSettings _settings;
    private readonly YouTubeResolver _youtube;
    private readonly ExternalIdRegistry _idRegistry;
    private readonly HttpClient _httpClient;

    protected override string ProviderName => SoulseekMetadataService.ProviderName;

    public SoulseekDownloadService(
        IConfiguration configuration,
        ILocalLibraryService localLibraryService,
        IMusicMetadataService metadataService,
        IOptions<SubsonicSettings> subsonicSettings,
        IOptions<SoulseekSettings> soulseekSettings,
        SoulseekClient slskd,
        YouTubeResolver youtube,
        ExternalIdRegistry idRegistry,
        IHttpClientFactory httpClientFactory,
        IServiceProvider serviceProvider,
        ILogger<SoulseekDownloadService> logger)
        : base(configuration, localLibraryService, metadataService, subsonicSettings.Value, serviceProvider, logger)
    {
        _slskd = slskd;
        _settings = soulseekSettings.Value;
        _youtube = youtube;
        _idRegistry = idRegistry;
        _httpClient = httpClientFactory.CreateClient();
        _httpClient.Timeout = TimeSpan.FromMinutes(10);
    }

    public override Task<bool> IsAvailableAsync() => _slskd.IsReachableAsync();

    protected override string? ExtractExternalIdFromAlbumId(string albumId) => null;

    // =========================================================================
    // Streaming path (every play of an unowned radio track)
    // =========================================================================
    public override async Task<DirectStreamInfo?> GetDirectStreamAsync(
        string externalProvider, string externalId, string? rangeHeader = null, CancellationToken cancellationToken = default)
    {
        if (!string.Equals(externalProvider, ProviderName, StringComparison.OrdinalIgnoreCase))
            return null;

        var routing = _idRegistry.Lookup(externalId) ?? SoulseekMetadataService.TryDecodeExternalId(externalId);
        if (routing is null) return null;

        var videoId = routing.YouTubeId;
        if (string.IsNullOrEmpty(videoId) && routing.HasArtistTitle)
        {
            var hit = await _youtube.SearchAsync($"{routing.Artist} {routing.Title}", cancellationToken);
            videoId = hit?.VideoId;
            // Cache back on the routing so a second click on the same placeholder
            // skips the yt-dlp ytsearch1: round trip — that 3-8s saving is the
            // difference between Arpeggi (~10s HTTP timeout) playing the song or
            // canceling and falling back to a local one. The routing object is
            // shared via the registry singleton, so this mutation is visible to
            // every subsequent stream request for this id.
            if (!string.IsNullOrEmpty(videoId))
            {
                routing.YouTubeId = videoId;
            }
        }
        if (string.IsNullOrEmpty(videoId)) return null;

        var opened = await _youtube.OpenStreamAsync(videoId, rangeHeader, cancellationToken);
        if (opened is null)
        {
            Logger.LogWarning("yt-dlp shim failed to open stream for vid={Vid}", videoId);
            return null;
        }

        var (stream, contentType, contentLength, statusCode, contentRange, owner) = opened.Value;
        var owned = new OwningStream(stream, owner);

        Logger.LogInformation("YouTube preview '{Artist} - {Title}' (vid={Vid}, status={Status}, {Len} bytes{Range})",
            routing.Artist, routing.Title, videoId, statusCode, contentLength,
            contentRange is null ? "" : $", range={contentRange}");

        return new DirectStreamInfo
        {
            AudioStream = owned,
            ContentType = contentType,
            ContentLength = contentLength,
            Quality = "youtube-m4a",
            StatusCode = statusCode,
            ContentRange = contentRange,
        };
    }

    // =========================================================================
    // Permanent download path (only when the user stars a track)
    // Search Soulseek, walk the top-N peers in quality order, first successful
    // transfer wins. ~30-50% of Soulseek peer requests are rejected (queue
    // full / overwhelmed / banned), so trying just the top hit fails too often.
    // =========================================================================
    private const int MaxPeerAttempts = 5;
    private const int PerAttemptTimeoutSeconds = 60;

    protected override async Task<string> DownloadTrackAsync(string trackId, Song song, CancellationToken cancellationToken)
    {
        var routing = _idRegistry.Lookup(song.ExternalId ?? "") ?? SoulseekMetadataService.TryDecodeExternalId(song.ExternalId ?? "");
        if (routing is null || !routing.HasArtistTitle)
            throw new InvalidOperationException(
                $"Cannot download '{song.Artist} - {song.Title}': missing artist/title in external id");

        // Clean the title before searching Soulseek. Last.fm's track.search
        // sometimes returns `title="Adele - Hello"` with the artist redundantly
        // prefixed, or YouTube-flavored titles like `"Long Season [LIVE][4K]"`.
        // Without normalization the Soulseek query "Adele Adele - Hello" or
        // "Long Season [LIVE][4K]" matches no peer.
        var cleanTitle = NormalizeTitle(routing.Title!, routing.Artist!);
        var primaryQuery = $"{routing.Artist} {cleanTitle}".Trim();

        Logger.LogInformation("Soulseek search-for-star: '{Query}'", primaryQuery);
        var hits = await _slskd.SearchAsync(primaryQuery, _settings.MinFileSizeBytes > 0 ? 30 : 10, cancellationToken);

        var ranked = RankCandidates(hits, routing.Title!);

        // Fallback search: if the artist+title combo returned nothing usable,
        // try with just the cleaned title. Catches cases where the Last.fm
        // artist field is junk (uploader names, weird capitalization) but the
        // title alone is enough for Soulseek to find the right file.
        if (ranked.Count == 0 && !string.IsNullOrWhiteSpace(cleanTitle))
        {
            Logger.LogInformation("Soulseek primary query returned no usable hits; retrying with title-only");
            hits = await _slskd.SearchAsync(cleanTitle, _settings.MinFileSizeBytes > 0 ? 30 : 10, cancellationToken);
            ranked = RankCandidates(hits, routing.Title!);
        }

        if (ranked.Count == 0)
            throw new FileNotFoundException(
                $"No Soulseek {_settings.PreferredExtension.ToUpper()} found for '{routing.Artist} - {routing.Title}'");

        Logger.LogInformation("Soulseek: {Count} candidate peers for '{Query}', trying in order",
            ranked.Count, primaryQuery);

        Exception? lastError = null;
        foreach (var (hit, attemptIdx) in ranked.Select((h, i) => (h, i + 1)))
        {
            Logger.LogInformation("Soulseek attempt {N}/{Total}: {User} -> {File} (queue={Q}, speed={S})",
                attemptIdx, ranked.Count, hit.Username, hit.Filename, hit.QueueLength, hit.UploadSpeed);

            try
            {
                await _slskd.EnqueueDownloadAsync(hit.Username, hit.Filename, hit.Size, cancellationToken);
            }
            catch (Exception ex)
            {
                Logger.LogWarning("Soulseek enqueue failed for {User} ({Msg}); trying next peer", hit.Username, ex.Message);
                lastError = ex;
                continue;
            }

            var state = await _slskd.WaitForCompletionAsync(hit.Username, hit.Filename, PerAttemptTimeoutSeconds, cancellationToken);

            // Regardless of slskd's reported final state, the authoritative
            // signal is the filesystem. slskd sometimes drops successful
            // transfers from /api/v0/transfers/downloads/<user> between our
            // polls, so we'd see Errored/timeout even though the file landed
            // on disk a second ago. Check disk first; fall back to "this
            // peer failed, try the next" only when the file truly isn't there.
            var localPath = ResolveLocalPath(hit.Filename, hit.Size);
            if (!string.IsNullOrEmpty(localPath))
            {
                // Apply the configured FolderStructure: slskd dumps to whatever
                // path the peer used (e.g. ".../MyMusic/Mark Morrison/Return of
                // the Mack/05 ...flac"), which is unpredictable per-peer. Move
                // to the canonical location now so Navidrome scans it under a
                // consistent layout.
                localPath = MoveToConfiguredLayout(localPath, routing) ?? localPath;
                Logger.LogInformation("Soulseek download complete (attempt {N}, slskd state={State}): {Path}",
                    attemptIdx, state, localPath);
                return localPath;
            }

            Logger.LogInformation("Soulseek attempt {N} failed (state={State}, no file on disk), advancing", attemptIdx, state);
            lastError = new Exception($"transfer ended in state {state} with no resulting file");
        }

        throw new Exception(
            $"All {ranked.Count} Soulseek peer attempts failed for '{routing.Artist} - {routing.Title}'. Last error: {lastError?.Message}");
    }

    /// <summary>
    /// Move/rename the just-downloaded file into the configured layout so
    /// Navidrome sees a consistent path regardless of how the original Soulseek
    /// peer organized their share.
    ///
    /// Layouts (driven by <c>Subsonic__FolderStructure</c>):
    ///   Flat       → <c>{DownloadPath}/{Artist} - {Title}.flac</c>   (no subfolder)
    ///   Organized  → <c>{DownloadPath}/{Artist}/{Title}/{originalFilename}</c>
    ///
    /// Returns the new path, or null if the move failed (caller falls back to
    /// the original path so the song still ends up registered).
    /// </summary>
    private string? MoveToConfiguredLayout(string currentPath, SoulseekRouting routing)
    {
        try
        {
            if (string.IsNullOrEmpty(DownloadPath) || !IOFile.Exists(currentPath)) return null;

            var artist = SanitizeForFs(routing.Artist) ?? "Unknown Artist";
            var title  = SanitizeForFs(routing.Title)  ?? "Unknown Title";
            var ext    = Path.GetExtension(currentPath);

            string targetPath = SubsonicSettings.FolderStructure switch
            {
                Models.Settings.FolderStructure.Flat
                    => Path.Combine(DownloadPath, $"{artist} - {title}{ext}"),
                Models.Settings.FolderStructure.Organized
                    => Path.Combine(DownloadPath, artist, title, Path.GetFileName(currentPath)),
                _ => currentPath,
            };

            if (string.Equals(Path.GetFullPath(targetPath), Path.GetFullPath(currentPath), StringComparison.OrdinalIgnoreCase))
                return currentPath;

            var targetDir = Path.GetDirectoryName(targetPath);
            if (!string.IsNullOrEmpty(targetDir)) Directory.CreateDirectory(targetDir);

            // If the destination already exists (re-download or hash collision),
            // overwrite — the user explicitly starred again, so they want the
            // freshly-downloaded copy.
            if (IOFile.Exists(targetPath)) IOFile.Delete(targetPath);

            IOFile.Move(currentPath, targetPath);
            Logger.LogInformation("Repositioned download to {Layout}: {From} -> {To}",
                SubsonicSettings.FolderStructure, currentPath, targetPath);

            // Clean up any now-empty parent directories slskd dumped into
            // (e.g. "/music/Return of the Mack/" if it's empty after we moved
            // the only file out). Don't recurse past DownloadPath.
            TryRemoveEmptyParents(Path.GetDirectoryName(currentPath), DownloadPath);

            return targetPath;
        }
        catch (Exception ex)
        {
            Logger.LogWarning(ex, "Failed to apply FolderStructure={Layout} to {Path}; leaving in place",
                SubsonicSettings.FolderStructure, currentPath);
            return null;
        }
    }

    private static string? SanitizeForFs(string? s)
    {
        if (string.IsNullOrWhiteSpace(s)) return null;
        var invalid = Path.GetInvalidFileNameChars();
        var cleaned = new string(s.Trim().Select(c => invalid.Contains(c) ? '_' : c).ToArray());
        // Avoid trailing dots/spaces (Windows-hostile, also looks ugly on Linux).
        cleaned = cleaned.TrimEnd('.', ' ');
        return string.IsNullOrEmpty(cleaned) ? null : cleaned;
    }

    private static void TryRemoveEmptyParents(string? startDir, string stopAt)
    {
        if (string.IsNullOrEmpty(startDir) || string.IsNullOrEmpty(stopAt)) return;
        var stop = Path.GetFullPath(stopAt).TrimEnd(Path.DirectorySeparatorChar, Path.AltDirectorySeparatorChar);
        var current = Path.GetFullPath(startDir).TrimEnd(Path.DirectorySeparatorChar, Path.AltDirectorySeparatorChar);
        // Walk up while we're inside DownloadPath and the directory is empty.
        while (!string.IsNullOrEmpty(current)
            && current.Length > stop.Length
            && current.StartsWith(stop, StringComparison.OrdinalIgnoreCase)
            && Directory.Exists(current))
        {
            try
            {
                if (Directory.EnumerateFileSystemEntries(current).Any()) break;
                Directory.Delete(current);
            }
            catch { break; }
            current = Path.GetDirectoryName(current) ?? "";
        }
    }

    /// <summary>
    /// Normalize a Last.fm/YouTube-flavored title for Soulseek search:
    ///  - Strip leading "Artist - " prefix (Last.fm sometimes does this).
    ///  - Strip trailing [bracketed] and (parenthesized) annotations like
    ///    "[LIVE]", "(Official Video)", "[Remastered 2009]". Soulseek peers
    ///    almost never have those in their filenames; with them included our
    ///    query gets zero hits.
    /// </summary>
    private static string NormalizeTitle(string title, string artist)
    {
        var t = (title ?? "").Trim();
        if (string.IsNullOrEmpty(t)) return t;

        // Strip "<Artist> - " prefix — case-insensitive, with optional surrounding spaces.
        if (!string.IsNullOrEmpty(artist))
        {
            var prefix = $"{artist.Trim()} - ";
            if (t.StartsWith(prefix, StringComparison.OrdinalIgnoreCase))
            {
                t = t.Substring(prefix.Length).Trim();
            }
        }

        // Strip [...] and (...) annotations. Repeat-replace until no more are found
        // so chained annotations like "[LIVE][4K][98.12.28]" all peel off.
        for (int i = 0; i < 5; i++)
        {
            var before = t;
            t = System.Text.RegularExpressions.Regex.Replace(t, @"\s*\[[^\]]*\]\s*", " ").Trim();
            t = System.Text.RegularExpressions.Regex.Replace(t, @"\s*\([^)]*\)\s*", " ").Trim();
            if (t == before) break;
        }

        // Collapse runs of whitespace
        t = System.Text.RegularExpressions.Regex.Replace(t, @"\s+", " ").Trim();
        return t;
    }

    private List<SoulseekFileHit> RankCandidates(List<SoulseekFileHit> hits, string title)
        => hits
            .Where(h => string.Equals(h.Extension, _settings.PreferredExtension, StringComparison.OrdinalIgnoreCase))
            .Where(h => h.Size >= _settings.MinFileSizeBytes)
            .Where(h => FilenamePlausiblyMatchesTitle(h.Filename, title))
            .OrderBy(h => h.QueueLength ?? int.MaxValue)
            .ThenByDescending(h => h.UploadSpeed ?? 0)
            .ThenByDescending(h => h.Size)
            .Take(MaxPeerAttempts)
            .ToList();

    /// <summary>
    /// Light sanity check: does the filename contain at least one significant
    /// token from the song title? Catches the "user shared a discography
    /// folder" case where the search hit's filename is unrelated to what we
    /// asked for. Only runs on tokens >=3 chars to avoid false negatives on
    /// short titles like "DNA." or "M.I.A.".
    /// </summary>
    private static bool FilenamePlausiblyMatchesTitle(string filename, string title)
    {
        if (string.IsNullOrEmpty(filename) || string.IsNullOrEmpty(title)) return true;
        var fn = filename.ToLowerInvariant();
        var tokens = title.ToLowerInvariant()
            .Split(new[] { ' ', '-', '(', ')', '[', ']', '_', '.', ',', '\'', '"' },
                   StringSplitOptions.RemoveEmptyEntries)
            .Where(t => t.Length >= 3)
            .ToList();
        if (tokens.Count == 0) return true;
        return tokens.Any(t => fn.Contains(t));
    }

    private string? ResolveLocalPath(string remoteFilename, long expectedSize)
    {
        var segments = remoteFilename
            .Replace('\\', '/')
            .Split('/', StringSplitOptions.RemoveEmptyEntries);
        if (segments.Length == 0) return null;

        var leaf = segments[^1];
        var parent = segments.Length >= 2 ? segments[^2] : null;

        var roots = new List<string>();
        if (!string.IsNullOrEmpty(DownloadPath)) roots.Add(DownloadPath);
        if (!roots.Contains("/music")) roots.Add("/music");

        foreach (var root in roots)
        {
            if (parent != null)
            {
                var candidate = Path.Combine(root, parent, leaf);
                if (FileMatches(candidate, expectedSize)) return candidate;
            }
            var flat = Path.Combine(root, leaf);
            if (FileMatches(flat, expectedSize)) return flat;
        }

        foreach (var root in roots)
        {
            if (!Directory.Exists(root)) continue;
            try
            {
                var matches = Directory
                    .EnumerateFiles(root, leaf, SearchOption.AllDirectories)
                    .Where(p => FileMatches(p, expectedSize))
                    .OrderByDescending(p => IOFile.GetCreationTimeUtc(p))
                    .ToList();
                if (matches.Count > 0) return matches[0];
            }
            catch (Exception ex)
            {
                Logger.LogDebug("Path scan failed under {Root}: {Msg}", root, ex.Message);
            }
        }

        return null;
    }

    private static bool FileMatches(string path, long expectedSize)
    {
        try
        {
            if (!IOFile.Exists(path)) return false;
            var actual = new FileInfo(path).Length;
            return actual == expectedSize || Math.Abs(actual - expectedSize) < 64 * 1024;
        }
        catch
        {
            return false;
        }
    }

    private sealed class OwningStream : Stream
    {
        private readonly Stream _inner;
        private readonly IDisposable _owner;
        public OwningStream(Stream inner, IDisposable owner) { _inner = inner; _owner = owner; }
        public override bool CanRead => _inner.CanRead;
        public override bool CanSeek => _inner.CanSeek;
        public override bool CanWrite => _inner.CanWrite;
        public override long Length => _inner.Length;
        public override long Position { get => _inner.Position; set => _inner.Position = value; }
        public override void Flush() => _inner.Flush();
        public override int Read(byte[] buffer, int offset, int count) => _inner.Read(buffer, offset, count);
        public override Task<int> ReadAsync(byte[] buffer, int offset, int count, CancellationToken ct) => _inner.ReadAsync(buffer, offset, count, ct);
        public override ValueTask<int> ReadAsync(Memory<byte> buffer, CancellationToken ct = default) => _inner.ReadAsync(buffer, ct);
        public override long Seek(long offset, SeekOrigin origin) => _inner.Seek(offset, origin);
        public override void SetLength(long value) => _inner.SetLength(value);
        public override void Write(byte[] buffer, int offset, int count) => _inner.Write(buffer, offset, count);
        protected override void Dispose(bool disposing)
        {
            if (disposing)
            {
                try { _inner.Dispose(); } catch { }
                try { _owner.Dispose(); } catch { }
            }
            base.Dispose(disposing);
        }
        public override async ValueTask DisposeAsync()
        {
            try { await _inner.DisposeAsync(); } catch { }
            try { _owner.Dispose(); } catch { }
            await base.DisposeAsync();
        }
    }
}
