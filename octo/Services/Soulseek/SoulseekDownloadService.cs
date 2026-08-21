using Microsoft.Extensions.Options;
using Octo.Models.Domain;
using Octo.Models.Settings;
using Octo.Services.Common;
using Octo.Services.Local;
using Octo.Services.Subsonic;
using Octo.Services.YouTube;
using System.Text.RegularExpressions;
using IOFile = System.IO.File;

namespace Octo.Services.Soulseek;

/// <summary>
/// Hybrid download service:
///   - GetDirectStreamAsync   -> instant lossy preview via YouTube (yt-dlp)
///   - DownloadTrackAsync     -> permanent FLAC fetch via slskd. Runs when the user
///                              stars a track, and in Permanent mode when one is
///                              played. Soulseek is searched here on demand using
///                              the encoded artist+title.
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
        IOptionsMonitor<SubsonicSettings> subsonicSettings,
        IOptions<SoulseekSettings> soulseekSettings,
        SoulseekClient slskd,
        YouTubeResolver youtube,
        ExternalIdRegistry idRegistry,
        IHttpClientFactory httpClientFactory,
        NavidromeIdentityService navIdentity,
        DownloadHistoryService history,
        Octo.Services.Notifications.NotificationService notifications,
        IServiceProvider serviceProvider,
        ILogger<SoulseekDownloadService> logger)
        : base(configuration, localLibraryService, metadataService, subsonicSettings, navIdentity, history, notifications, serviceProvider, logger)
    {
        _slskd = slskd;
        _settings = soulseekSettings.Value;
        _youtube = youtube;
        _idRegistry = idRegistry;
        _httpClient = httpClientFactory.CreateClient();
        _httpClient.Timeout = TimeSpan.FromMinutes(10);
    }

    public override Task<bool> IsAvailableAsync() => _slskd.IsReachableAsync();

    // Octo's album ids ARE the external id, so this is identity plus a kind check that
    // stops a song or artist id being walked as if it were an album.
    protected override string? ExtractExternalIdFromAlbumId(string albumId)
        => _idRegistry.Lookup(albumId)?.Kind == RoutingKind.Album ? albumId : null;

    /// <summary>
    /// Restore an album track's routing if the registry evicted it mid-download.
    /// The fields here MUST match what SoulseekMetadataService.GetAlbumAsync registered
    /// (YouTubeId left null, the Song's own Duration) or this hashes to a different id and
    /// fails to restore anything. Routings are mutated in place elsewhere, so rebuild from
    /// the Song, which still carries the values used at registration time.
    /// </summary>
    protected override void EnsureRoutingRegistered(Song track)
    {
        if (string.IsNullOrEmpty(track.ExternalId)) return;
        if (_idRegistry.Lookup(track.ExternalId) is not null) return;

        _idRegistry.Register(new SoulseekRouting
        {
            Kind = RoutingKind.Song,
            Artist = track.Artist,
            Title = track.Title,
            Album = track.Album,
            Duration = track.Duration,
            Track = track.Track,
            DiscNumber = track.DiscNumber,
            TotalTracks = track.TotalTracks,
        });
    }

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
            var hit = await _youtube.SearchAsync($"{routing.Artist} {routing.Title}", routing.Duration, ct: cancellationToken);
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
    // Permanent download path (a star, or a play while in Permanent mode)
    // Search Soulseek, walk the top-N peers in quality order, first successful
    // transfer wins. ~30-50% of Soulseek peer requests are rejected (queue
    // full / overwhelmed / banned), so trying just the top hit fails too often.
    //
    // Each attempt is bounded by Soulseek:DownloadTimeoutSeconds, so that setting is
    // per peer and a full walk can spend it MaxPeerAttempts times over.
    // =========================================================================
    private const int MaxPeerAttempts = 5;

    protected override async Task<string> DownloadTrackAsync(
        string trackId, Song song, bool suppressNotify,
        DownloadSource? sourceOverride, CancellationToken cancellationToken)
    {
        var routing = _idRegistry.Lookup(song.ExternalId ?? "") ?? SoulseekMetadataService.TryDecodeExternalId(song.ExternalId ?? "");
        if (routing is null || !routing.HasArtistTitle)
            throw new InvalidOperationException(
                $"Cannot download '{song.Artist} - {song.Title}': missing artist/title in external id");

        // DownloadOnStar decides WHETHER to download; DownloadSource decides FROM WHERE.
        switch (sourceOverride ?? SubsonicSettings.DownloadSource)
        {
            case DownloadSource.YouTube:
                return await DownloadViaYouTubeAsync(routing, suppressNotify, announceStart: true, cancellationToken);
            case DownloadSource.SoulseekThenYouTube:
                // The filter matters: a cancelled token means nobody is waiting for
                // this any more, so falling back would start a second download only
                // to have it throw on the same token.
                try { return await DownloadViaSoulseekAsync(routing, suppressNotify, cancellationToken); }
                catch (Exception ex) when (!cancellationToken.IsCancellationRequested)
                {
                    Logger.LogWarning("Soulseek download failed ({Msg}); falling back to YouTube MP3", ex.Message);
                    if (!suppressNotify)
                    {
                        Notifications.Notify(new Octo.Services.Notifications.NotificationEvent
                        {
                            Type = Octo.Services.Notifications.NotificationEventType.LosslessFallback,
                            Artist = routing.Artist,
                            Title = routing.Title,
                            Album = routing.Album,
                            Source = "YouTube",
                            Format = "MP3",
                            Detail = ex.Message,
                        });
                    }
                    // announceStart false: the fallback event above already announces
                    // the MP3, and one gesture should never ping twice.
                    return await DownloadViaYouTubeAsync(routing, suppressNotify, announceStart: false, cancellationToken);
                }
            default:
                return await DownloadViaSoulseekAsync(routing, suppressNotify, cancellationToken);
        }
    }

    // Lossy MP3 via the yt-dlp shim's /download. The shim writes <dest>.mp3 in
    // the final layout with clean tags + cover, so there is no post-move.
    private async Task<string> DownloadViaYouTubeAsync(SoulseekRouting routing, bool suppressNotify, bool announceStart, CancellationToken cancellationToken)
    {
        if (string.IsNullOrEmpty(DownloadPath))
            throw new InvalidOperationException("DownloadPath is not configured");

        var videoId = routing.YouTubeId;
        if (string.IsNullOrEmpty(videoId))
        {
            var hit = await _youtube.SearchAsync($"{routing.Artist} {routing.Title}", routing.Duration, ct: cancellationToken);
            videoId = hit?.VideoId;
            if (!string.IsNullOrEmpty(videoId)) routing.YouTubeId = videoId;
        }
        if (string.IsNullOrEmpty(videoId))
            throw new FileNotFoundException($"No YouTube match for '{routing.Artist} - {routing.Title}'");

        if (!suppressNotify && announceStart)
        {
            Notifications.Notify(new Octo.Services.Notifications.NotificationEvent
            {
                Type = Octo.Services.Notifications.NotificationEventType.DownloadStarted,
                Artist = routing.Artist,
                Title = routing.Title,
                Album = routing.Album,
                Source = "YouTube",
                Format = "MP3",
                DurationSeconds = routing.Duration,
            });
        }

        var ytArtist = SanitizeForFs(routing.Artist) ?? "Unknown Artist";
        // Strip a redundant "Artist - " prefix from the title before naming the file,
        // so a Last.fm title like "Massive Attack - Teardrop" doesn't produce
        // "Massive Attack - Massive Attack - Teardrop.mp3".
        var ytTitle  = SanitizeForFs(NormalizeTitle(routing.Title ?? "", routing.Artist ?? "")) ?? "Unknown Title";
        var destWithoutExt = SubsonicSettings.FolderStructure switch
        {
            // Extension is left empty: the shim appends .mp3 itself.
            Models.Settings.FolderStructure.Organized => BuildOrganizedPath(routing, ytArtist, ytTitle, ""),
            _ => Path.Combine(DownloadPath, $"{ytArtist} - {ytTitle}"),
        };

        var path = await _youtube.DownloadAsync(videoId, destWithoutExt, routing.Artist, routing.Title, cancellationToken);
        if (string.IsNullOrEmpty(path) || !IOFile.Exists(path))
            throw new FileNotFoundException($"YouTube MP3 download failed for '{routing.Artist} - {routing.Title}'");

        Logger.LogInformation("YouTube MP3 download complete: {Path}", path);
        return path;
    }

    // Lossless FLAC via Soulseek/slskd: walk the top-N peers in quality order,
    // first successful transfer wins.
    private async Task<string> DownloadViaSoulseekAsync(SoulseekRouting routing, bool suppressNotify, CancellationToken cancellationToken)
    {
        // Clean the title before searching Soulseek. Last.fm's track.search
        // sometimes returns `title="Adele - Hello"` with the artist redundantly
        // prefixed, or YouTube-flavored titles like `"Long Season [LIVE][4K]"`.
        // Without normalization the Soulseek query "Adele Adele - Hello" or
        // "Long Season [LIVE][4K]" matches no peer.
        var cleanTitle = NormalizeTitle(routing.Title!, routing.Artist!);
        var primaryQuery = $"{routing.Artist} {cleanTitle}".Trim();

        Logger.LogInformation("Soulseek search-for-star: '{Query}'", primaryQuery);
        var hits = await _slskd.SearchAsync(
            primaryQuery,
            _settings.MinFileSizeBytes > 0 ? 30 : 10,
            cancellationToken,
            // Stop waiting once there is a real choice to make. Not on the first
            // usable hit: ranking picks on queue length and upload speed, so
            // committing to a single candidate would often mean committing to the
            // slowest peer that happened to answer first. A handful is enough to
            // choose well without waiting for stragglers.
            enough: h => RankCandidates(h.ToList(), routing.Title!, routing.Duration).Count >= 3);

        var ranked = RankCandidates(hits, routing.Title!, routing.Duration);

        // Fallback search: if the artist+title combo returned nothing usable,
        // try with just the cleaned title. Catches cases where the Last.fm
        // artist field is junk (uploader names, weird capitalization) but the
        // title alone is enough for Soulseek to find the right file.
        if (ranked.Count == 0 && !string.IsNullOrWhiteSpace(cleanTitle))
        {
            Logger.LogInformation("Soulseek primary query returned no usable hits; retrying with title-only");
            hits = await _slskd.SearchAsync(
                cleanTitle,
                _settings.MinFileSizeBytes > 0 ? 30 : 10,
                cancellationToken,
                enough: h => RankCandidates(h.ToList(), routing.Title!, routing.Duration).Count >= 3);
            ranked = RankCandidates(hits, routing.Title!, routing.Duration);
        }

        if (ranked.Count == 0)
            throw new FileNotFoundException(
                $"No Soulseek {_settings.PreferredExtension.ToUpper()} found for '{routing.Artist} - {routing.Title}'");

        Logger.LogInformation("Soulseek: {Count} candidate peers for '{Query}', trying in order",
            ranked.Count, primaryQuery);

        Exception? lastError = null;
        var startAnnounced = false;
        foreach (var (hit, attemptIdx) in ranked.Select((h, i) => (h, i + 1)))
        {
            Logger.LogInformation("Soulseek attempt {N}/{Total}: {User} -> {File} (queue={Q}, speed={S})",
                attemptIdx, ranked.Count, hit.Username, hit.Filename, hit.QueueLength, hit.UploadSpeed);

            // Used to decide whether a rejected file is ours to delete.
            var attemptStartedUtc = DateTime.UtcNow;

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

            // Announced only after a peer actually accepted the transfer -- firing
            // before the loop would claim a start that five straight rejections later
            // never happened. Once per track: a retry on the next peer is the same
            // download, not a new one. SizeBytes is this candidate's advertised size;
            // DownloadCompleted carries the real file's.
            if (!suppressNotify && !startAnnounced)
            {
                startAnnounced = true;
                Notifications.Notify(new Octo.Services.Notifications.NotificationEvent
                {
                    Type = Octo.Services.Notifications.NotificationEventType.DownloadStarted,
                    Artist = routing.Artist,
                    Title = routing.Title,
                    Album = routing.Album,
                    Source = "Soulseek",
                    Format = _settings.PreferredExtension.ToUpperInvariant(),
                    SizeBytes = hit.Size,
                    DurationSeconds = routing.Duration,
                });
            }

            // Cancelling the WAIT must never cancel the TRANSFER. slskd already
            // accepted the enqueue and keeps going on its own, so letting this
            // throw straight out of the loop is what used to lose a finished
            // download: the disk check, the move, the registration and the
            // rescan were all skipped while the file quietly landed anyway.
            SoulseekTransferState? state = null;
            Exception? waitError = null;
            try
            {
                state = await _slskd.WaitForCompletionAsync(
                    hit.Username,
                    hit.Filename,
                    _settings.DownloadTimeoutSeconds,
                    cancellationToken);
            }
            catch (Exception ex)
            {
                waitError = ex;
            }

            // An slskd HTTP timeout and a client disconnect both surface as
            // TaskCanceledException, so the token is the only reliable way to
            // tell "the caller left" from "slskd was slow".
            var callerGaveUp = cancellationToken.IsCancellationRequested;

            // Regardless of slskd's reported final state, the authoritative
            // signal is the filesystem. slskd sometimes drops successful
            // transfers from /api/v0/transfers/downloads/<user> between our
            // polls, so we'd see Errored/timeout even though the file landed
            // on disk a second ago. Check disk first; fall back to "this
            // peer failed, try the next" only when the file truly isn't there.
            //
            // The usual 64KB size tolerance absorbs slskd's own size drift, but
            // an interrupted transfer is far more likely to be genuinely
            // truncated, so demand an exact match before promoting one.
            //
            // The check re-polls the disk for a bounded window: slskd reports
            // Succeeded BEFORE moving the file out of its incomplete directory,
            // and on bind mounts that move is a copy that can take seconds.
            var localPath = await ResolveLocalPathWithRetryAsync(
                hit.Filename, hit.Size,
                requireExactSize: callerGaveUp,
                maxWait: state == SoulseekTransferState.Succeeded
                    ? TimeSpan.FromSeconds(15)
                    : TimeSpan.FromSeconds(5),
                cancellationToken);
            if (!string.IsNullOrEmpty(localPath))
            {
                // Last line of defence, and the only one that inspects the actual audio.
                // A peer can advertise a length it does not deliver, and the tagger runs
                // straight after this and would stamp the RIGHT title onto the wrong
                // recording, leaving a library that looks correct and plays wrong.
                if (!DownloadedDurationMatches(localPath, routing.Duration, out var actualSecs))
                {
                    Logger.LogWarning(
                        "Soulseek attempt {N} delivered the wrong recording for '{Artist} - {Title}': "
                        + "{Actual}s against an expected {Expected}s; discarding and advancing",
                        attemptIdx, routing.Artist, routing.Title, actualSecs, routing.Duration);
                    DiscardRejectedDownload(localPath, attemptStartedUtc);
                    lastError = new Exception(
                        $"peer delivered a {actualSecs}s file for a {routing.Duration}s track");
                    continue;
                }

                // Apply the configured FolderStructure: slskd dumps to whatever
                // path the peer used (e.g. ".../MyMusic/Mark Morrison/Return of
                // the Mack/05 ...flac"), which is unpredictable per-peer. Move
                // to the canonical location now so Navidrome scans it under a
                // consistent layout.
                localPath = MoveToConfiguredLayout(localPath, routing) ?? localPath;
                Logger.LogInformation("Soulseek download complete (attempt {N}, slskd state={State}{Aborted}): {Path}",
                    attemptIdx, state?.ToString() ?? "interrupted", callerGaveUp ? ", caller had already left" : "", localPath);
                return localPath;
            }

            if (waitError is not null)
            {
                // Nothing on disk and the caller is gone: no later peer attempt
                // has anywhere to be delivered, so stop instead of burning the
                // rest of the list.
                if (callerGaveUp) throw waitError;
                Logger.LogWarning("Soulseek attempt {N} wait failed ({Msg}); advancing", attemptIdx, waitError.Message);
                lastError = waitError;
                continue;
            }

            Logger.LogInformation("Soulseek attempt {N} failed (state={State}, no file on disk), advancing", attemptIdx, state);
            lastError = new Exception($"transfer ended in state {state} with no resulting file");
        }

        throw new Exception(
            $"All {ranked.Count} Soulseek peer attempts failed for '{routing.Artist} - {routing.Title}'. Last error: {lastError?.Message}. "
            + $"If slskd shows these transfers as Completed, slskd's downloads directory is not the directory Octo watches ({DownloadPath}); "
            + "set SLSKD_DOWNLOADS_DIR=/music on the slskd container (see issue #17).");
    }

    /// <summary>
    /// Move/rename the just-downloaded file into the configured layout so
    /// Navidrome sees a consistent path regardless of how the original Soulseek
    /// peer organized their share.
    ///
    /// Layouts (driven by <c>Subsonic__FolderStructure</c>):
    ///   Flat       → <c>{DownloadPath}/{Artist} - {Title}.flac</c>   (no subfolder)
    ///   Organized  → <c>{DownloadPath}/{Artist}/{Album}/{NN - Title}.flac</c>
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
            // Same prefix cleanup as the YouTube path, so FLAC files don't double the
            // artist ("Massive Attack - Massive Attack - Teardrop.flac").
            var title  = SanitizeForFs(NormalizeTitle(routing.Title ?? "", routing.Artist ?? "")) ?? "Unknown Title";
            var ext    = Path.GetExtension(currentPath);

            string targetPath = SubsonicSettings.FolderStructure switch
            {
                Models.Settings.FolderStructure.Flat
                    => Path.Combine(DownloadPath, $"{artist} - {title}{ext}"),
                Models.Settings.FolderStructure.Organized
                    => BuildOrganizedPath(routing, artist, title, ext),
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

    /// <summary>
    /// Organized layout: <c>{DownloadPath}/{Artist}/{Album}/{NN - Title}{ext}</c>.
    ///
    /// The album folder is what makes a hearted album land as one album on disk. Before
    /// albums existed Octo only fetched standalone singles, so this used the TRACK title
    /// as the folder and scattered an album's tracks into a folder each. The routing now
    /// carries the album, so use it, falling back to the title for a track that genuinely
    /// has no album (which reproduces the old shape).
    ///
    /// Existing files are never moved: this only names the download currently in flight,
    /// so an upgrade leaves previously-downloaded files exactly where they are.
    /// </summary>
    private string BuildOrganizedPath(SoulseekRouting routing, string artist, string title, string ext)
    {
        var album = string.IsNullOrWhiteSpace(routing.Album) ? title : routing.Album!;
        return PathHelper.BuildTrackPath(DownloadPath, artist, album, title, routing.Track, ext);
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

        // Some titles ARE an annotation, e.g. Mezzanine's "(Exchange)" or a bare
        // "(Interlude)". Stripping leaves nothing, which used to surface as an
        // "Unknown Title" filename and an empty search query. Keep the original.
        if (t.Length == 0) return (title ?? "").Trim();

        return t;
    }

    /// <summary>
    /// Correct rips of the same recording drift by a second or two between masterings.
    /// A different recording does not.
    ///
    /// Measured over two full walks of the same album: every correctly-matched track came
    /// in within 4s of the catalog length, while wrong ones were 11s, 12s, 93s, 98s and
    /// 140s out. 8 sits in that gap. An earlier 15 was too generous and let two dub mixes
    /// through at 11s and 12s.
    /// </summary>
    private const int DurationToleranceSeconds = 8;

    /// <summary>
    /// Words that mean "a different recording of this song". When the title we asked for
    /// carries none of these and a candidate does, it is the wrong version. This is the
    /// only signal that separates "Group Four" from "Group Four (Security Forces dub)",
    /// whose runtimes are two seconds apart.
    /// </summary>
    private static readonly string[] VariantMarkers =
    {
        "dub", "remix", "live", "instrumental", "acoustic", "edit", "mix",
        "version", "demo", "session", "karaoke", "cover", "reprise",
    };

    private List<SoulseekFileHit> RankCandidates(List<SoulseekFileHit> hits, string title, int? expectedDuration)
    {
        var wanted = SoulseekClient.NormalizeExtension(_settings.PreferredExtension, "");
        return hits
            .Where(h => string.Equals(h.Extension, wanted, StringComparison.OrdinalIgnoreCase))
            .Where(h => h.Size >= _settings.MinFileSizeBytes)
            .Where(h => FilenamePlausiblyMatchesTitle(h.Filename, title))
            .Where(h => DurationPlausible(h.Length, expectedDuration))
            // Variant mixes sort last rather than being dropped: sometimes a remix really
            // is what was asked for, and sometimes it is all a peer has.
            .OrderBy(h => VariantPenalty(h.Filename, title))
            .ThenBy(h => QualityPenalty(h))
            .ThenBy(h => h.QueueLength ?? int.MaxValue)
            .ThenByDescending(h => h.UploadSpeed ?? 0)
            .ThenBy(h => SizeSortKey(h.Size, wanted))
            .Take(MaxPeerAttempts)
            .ToList();
    }

    /// <summary>
    /// Extensions where a bigger file means a longer or higher-resolution recording
    /// rather than a better one. Used to decide which way the size tiebreak points.
    /// </summary>
    private static readonly string[] LosslessExtensions =
    {
        "flac", "wav", "alac", "ape", "aiff", "aif", "wv",
    };

    /// <summary>
    /// The last signal left when everything above it ties, and it ties often: slskd
    /// reports queue length and upload speed per RESPONSE, not per file, so every file
    /// one peer offers carries identical values and size is what actually separates them.
    ///
    /// Which direction helps depends on what is being chased. Chasing lossless, the
    /// smaller of two otherwise-equal candidates is the CD rip rather than the hi-res
    /// transfer, which is the same preference QualityPenalty encodes and the only way to
    /// express it when a peer reports no bit depth at all. Chasing a lossy format, the
    /// bigger file is simply the higher bitrate, and preferring the smaller one would
    /// walk an mp3 library down to its worst copy of every track.
    /// </summary>
    internal static long SizeSortKey(long size, string? preferredExtension)
        => LosslessExtensions.Contains(SoulseekClient.NormalizeExtension(preferredExtension, ""))
            ? size
            : -size;

    /// <summary>
    /// How far a candidate sits from ordinary CD quality, 16-bit/44.1kHz.
    ///
    /// CD is the target because it is what the master almost always was: a 24/96 transfer
    /// of a 1998 pop record carries no more music than the 16/44.1 one, at several times
    /// the bytes on a disk the user is paying for and a transfer that takes proportionally
    /// longer over Soulseek. Hi-res is ranked down rather than rejected, because sometimes
    /// it is the only copy a peer has.
    ///
    /// Unknown sits deliberately between the two. Most peers report neither field, so
    /// treating unknown as hi-res would bury the majority of a normal search, and treating
    /// it as CD would let an unlabelled 24/96 outrank a labelled 16/44.1.
    /// </summary>
    internal static int QualityPenalty(SoulseekFileHit h)
    {
        var bitDepthPenalty = h.BitDepth switch
        {
            16 => 0,
            null => 3,
            24 => 10,
            > 24 => 20,
            _ => 5,
        };

        var sampleRatePenalty = h.SampleRate switch
        {
            44100 => 0,
            48000 => 1,
            null => 3,
            88200 => 10,
            96000 => 11,
            176400 => 20,
            192000 => 21,
            > 96000 => 20,
            > 48000 => 10,
            _ => 4,
        };

        return bitDepthPenalty + sampleRatePenalty;
    }

    private static string LeafOf(string path) =>
        path.Replace('\\', '/').Split('/', StringSplitOptions.RemoveEmptyEntries) is { Length: > 0 } s
            ? s[^1] : path;

    private static List<string> TitleTokens(string title) =>
        title.ToLowerInvariant()
            .Split(new[] { ' ', '-', '(', ')', '[', ']', '_', '.', ',', '\'', '"' },
                   StringSplitOptions.RemoveEmptyEntries)
            .Where(t => t.Length >= 3)
            .ToList();

    /// <summary>
    /// A roman numeral on the end of a title, which TitleTokens cannot see.
    ///
    /// Tokens shorter than 3 characters are dropped so that "DNA." and "M.I.A." are not
    /// over-filtered, and that quietly deletes the entire difference between "Trilogy I"
    /// and "Trilogy II": both reduce to the single token "trilogy", so either file
    /// satisfies a request for the other. Anchored to the end so an "I" inside a sentence
    /// is left alone, and matched on word boundaries in the filename so "I" does not find
    /// itself inside "II" and "V" does not find itself inside "IV".
    /// </summary>
    private static readonly Regex TrailingRomanNumeral = new(
        @"\b(I|II|III|IV|V|VI|VII|VIII|IX|X)\b\s*$",
        RegexOptions.IgnoreCase | RegexOptions.Compiled);

    /// <summary>
    /// Does the FILENAME look like the track we asked for?
    ///
    /// Two things here were wrong and both let the wrong song through. It matched the
    /// whole path, so a folder named "Mezzanine Remix Tapes '98" satisfied a search for
    /// the track "Mezzanine" while the file inside was a different song entirely. And it
    /// accepted any single token, so "Group Four" would have been satisfied by "Four
    /// Seasons". Now every significant token must appear in the leaf name. Tokens are
    /// >=3 chars so short titles like "DNA." or "M.I.A." are not over-filtered, with a
    /// trailing roman numeral handled separately since that rule would erase it.
    /// </summary>
    internal static bool FilenamePlausiblyMatchesTitle(string filename, string title)
    {
        if (string.IsNullOrEmpty(filename) || string.IsNullOrEmpty(title)) return true;
        var leaf = LeafOf(filename).ToLowerInvariant();

        var wantedRoman = TrailingRomanNumeral.Match(title.Trim());
        if (wantedRoman.Success
            && !Regex.IsMatch(leaf, $@"\b{Regex.Escape(wantedRoman.Groups[1].Value)}\b", RegexOptions.IgnoreCase))
        {
            return false;
        }

        var tokens = TitleTokens(title);
        if (tokens.Count == 0) return true;
        return tokens.All(t => leaf.Contains(t));
    }

    /// <summary>Reject a candidate whose advertised length is nowhere near the known one.</summary>
    internal static bool DurationPlausible(int? candidateSeconds, int? expectedSeconds)
    {
        // Unknown either side is not evidence of a bad match, so let it through and let
        // the post-download check have the final say.
        if (candidateSeconds is not int c || expectedSeconds is not int e || c <= 0 || e <= 0) return true;
        return Math.Abs(c - e) <= DurationToleranceSeconds;
    }

    /// <summary>
    /// How many "this is a different recording" signals the candidate carries that the
    /// requested title never asked for.
    ///
    /// The generic half matters more than the word list. "Angel (Angel Dust)" and
    /// "Inertia Creeps (Floating on Dubwise)" are both dub mixes, and neither contains a
    /// keyword any sane list would hold — but both are bracketed additions the title did
    /// not ask for, and that is the thing they have in common with every other wrong take.
    ///
    /// Ranked rather than rejected: sometimes a remaster is all a peer has, and sometimes
    /// the remix genuinely is what was requested.
    /// </summary>
    internal static int VariantPenalty(string filename, string title)
    {
        var leaf = LeafOf(filename);
        var leafLower = leaf.ToLowerInvariant();
        var wanted = (title ?? "").ToLowerInvariant();

        var penalty = VariantMarkers.Count(m =>
            Regex.IsMatch(leafLower, $@"\b{m}\b") && !Regex.IsMatch(wanted, $@"\b{m}\b"));

        foreach (Match group in Regex.Matches(leaf, @"[\(\[]([^\)\]]*)[\)\]]"))
        {
            var inner = group.Groups[1].Value.Trim().ToLowerInvariant();
            if (inner.Length == 0) continue;
            // A year or a format tag is how peers label a good rip, not a different take.
            if (Regex.IsMatch(inner, @"^(19|20)\d{2}$")) continue;
            if (inner is "flac" or "hi-res" or "hires" or "16-44" or "24-96" or "24-44") continue;
            if (!wanted.Contains(inner)) penalty++;
        }

        return penalty;
    }

    /// <summary>
    /// Read the real duration off the downloaded file and compare it against what the
    /// catalog says the track should be. Anything unknown or unreadable passes: this
    /// exists to catch a confidently wrong file, not to reject an unusual one.
    /// </summary>
    private bool DownloadedDurationMatches(string path, int? expectedSeconds, out int actualSeconds)
    {
        actualSeconds = 0;
        if (expectedSeconds is not int expected || expected <= 0) return true;

        try
        {
            using var file = TagLib.File.Create(path);
            actualSeconds = (int)Math.Round(file.Properties.Duration.TotalSeconds);
        }
        catch (Exception ex)
        {
            Logger.LogDebug("Could not read duration of {Path}: {M}", path, ex.Message);
            return true;
        }

        if (actualSeconds <= 0) return true;
        return Math.Abs(actualSeconds - expected) <= DurationToleranceSeconds;
    }

    /// <summary>
    /// Remove a file we have judged to be the wrong recording, so it does not sit in the
    /// music folder waiting to be scanned or matched by a later ResolveLocalPath.
    ///
    /// Only deletes what this attempt actually created. ResolveLocalPath matches on leaf
    /// name and approximate size across the whole library, so without that guard a bad
    /// match could delete a file the user already owned.
    /// </summary>
    private void DiscardRejectedDownload(string path, DateTime attemptStartedUtc)
    {
        try
        {
            if (!IOFile.Exists(path)) return;

            if (IOFile.GetCreationTimeUtc(path) < attemptStartedUtc.AddSeconds(-5))
            {
                Logger.LogWarning(
                    "Leaving {Path} in place: it predates this download, so it is not ours to delete", path);
                return;
            }

            IOFile.Delete(path);
            Logger.LogInformation("Deleted mismatched download {Path}", path);
        }
        catch (Exception ex)
        {
            Logger.LogWarning("Could not delete mismatched download {Path}: {M}", path, ex.Message);
        }
    }

    /// <summary>
    /// slskd flips a transfer to Succeeded BEFORE moving the file out of its
    /// incomplete directory, and on bind mounts that move is a cross-filesystem
    /// copy that can take seconds for a FLAC. Without this window the attempt
    /// fails on "no file on disk" and the next peer re-downloads the same track.
    /// FileMatches rejects a partial copy by size, so the loop naturally waits
    /// out an in-flight move. A cancelled caller gets one final check instead of
    /// a wait, mirroring the wait-cancel handling above.
    /// </summary>
    private Task<string?> ResolveLocalPathWithRetryAsync(
        string remoteFilename, long expectedSize, bool requireExactSize, TimeSpan maxWait, CancellationToken ct)
        => RetryResolveAsync(
            () => ResolveLocalPath(remoteFilename, expectedSize, requireExactSize),
            maxWait, TimeSpan.FromSeconds(1), ct);

    internal static async Task<string?> RetryResolveAsync(
        Func<string?> resolve, TimeSpan maxWait, TimeSpan pollInterval, CancellationToken ct)
    {
        var deadline = DateTime.UtcNow + maxWait;
        while (true)
        {
            var path = resolve();
            if (path is not null || DateTime.UtcNow >= deadline) return path;
            try
            {
                await Task.Delay(pollInterval, ct);
            }
            catch (TaskCanceledException)
            {
                // Caller left: no point waiting out the window, but the file may
                // have just landed, so look once more before giving up.
                return resolve();
            }
        }
    }

    /// <param name="requireExactSize">
    /// Drop the usual near-miss tolerance. Used when a transfer was interrupted,
    /// where a slightly-short file is more likely truncated than size drift.
    /// </param>
    private string? ResolveLocalPath(string remoteFilename, long expectedSize, bool requireExactSize = false)
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
                if (FileMatches(candidate, expectedSize, requireExactSize)) return candidate;
            }
            var flat = Path.Combine(root, leaf);
            if (FileMatches(flat, expectedSize, requireExactSize)) return flat;
        }

        foreach (var root in roots)
        {
            if (!Directory.Exists(root)) continue;
            try
            {
                var matches = Directory
                    .EnumerateFiles(root, leaf, SearchOption.AllDirectories)
                    .Where(p => FileMatches(p, expectedSize, requireExactSize))
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

    private static bool FileMatches(string path, long expectedSize, bool requireExactSize = false)
    {
        try
        {
            if (!IOFile.Exists(path)) return false;
            var actual = new FileInfo(path).Length;
            if (actual == expectedSize) return true;
            return !requireExactSize && Math.Abs(actual - expectedSize) < 64 * 1024;
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
