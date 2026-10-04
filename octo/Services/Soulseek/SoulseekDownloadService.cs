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
    private readonly RejectedPeerRegistry _rejectedPeers;
    private readonly Octo.Services.Fingerprint.DownloadVerificationService _verification;
    private readonly SoulseekSettings _settings;
    private readonly YouTubeResolver _youtube;
    private readonly ExternalIdRegistry _idRegistry;
    private readonly HttpClient _httpClient;

    // Set once slskd has said where its incomplete folder is; until then each download asks again.
    private string[]? _excludedFolders;

    protected override string ProviderName => SoulseekMetadataService.ProviderName;

    public SoulseekDownloadService(
        IConfiguration configuration,
        ILocalLibraryService localLibraryService,
        IMusicMetadataService metadataService,
        IOptionsMonitor<SubsonicSettings> subsonicSettings,
        IOptionsMonitor<GenreSettings> genreSettings,
        IOptions<SoulseekSettings> soulseekSettings,
        SoulseekClient slskd,
        YouTubeResolver youtube,
        ExternalIdRegistry idRegistry,
        IHttpClientFactory httpClientFactory,
        NavidromeIdentityService navIdentity,
        DownloadHistoryService history,
        Octo.Services.Notifications.NotificationService notifications,
        RejectedPeerRegistry rejectedPeers,
        Octo.Services.Fingerprint.DownloadVerificationService verification,
        IServiceProvider serviceProvider,
        ILogger<SoulseekDownloadService> logger)
        : base(configuration, localLibraryService, metadataService, subsonicSettings, genreSettings, navIdentity, history, notifications, serviceProvider, logger)
    {
        _slskd = slskd;
        _rejectedPeers = rejectedPeers;
        _verification = verification;
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
            Isrc = track.Isrc,
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
        DownloadSource? sourceOverride, bool upgradeSearch, CancellationToken cancellationToken)
    {
        var routing = _idRegistry.Lookup(song.ExternalId ?? "") ?? SoulseekMetadataService.TryDecodeExternalId(song.ExternalId ?? "");
        if (routing is null || !routing.HasArtistTitle)
            throw new InvalidOperationException(
                $"Cannot download '{song.Artist} - {song.Title}': missing artist/title in external id");
        var profile = upgradeSearch ? SearchProfile.Upgrade(_settings) : SearchProfile.Interactive(_settings);

        // Only ever asked for by name: a library action's replacement. DownloadSource set to
        // Lidarr means hearts go to Lidarr, and every other download still uses Soulseek below.
        if (sourceOverride == DownloadSource.Lidarr)
            return await DownloadViaLidarrAsync(routing, song, upgradeSearch, cancellationToken);

        // DownloadOnStar decides WHETHER to download; DownloadSource decides FROM WHERE.
        switch (sourceOverride ?? SubsonicSettings.DownloadSource)
        {
            case DownloadSource.YouTube:
                return await DownloadViaYouTubeAsync(routing, song, suppressNotify, announceStart: true, cancellationToken);
            case DownloadSource.SoulseekThenYouTube:
                // The filter matters: a cancelled token means nobody is waiting for
                // this any more, so falling back would start a second download only
                // to have it throw on the same token.
                try { return await DownloadViaSoulseekAsync(routing, song, suppressNotify, profile, cancellationToken); }
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
                    return await DownloadViaYouTubeAsync(routing, song, suppressNotify, announceStart: false, cancellationToken);
                }
            default:
                return await DownloadViaSoulseekAsync(routing, song, suppressNotify, profile, cancellationToken);
        }
    }

    /// <summary>
    /// Where the shim writes a download before Octo has decided its name. A dot folder, which
    /// Navidrome's scanner skips (Scanner.IgnoreDotFolders, on by default), the same way it
    /// skips the library-action quarantine.
    /// </summary>
    internal const string IncomingFolderName = ".octo-incoming";

    /// <summary>
    /// A file a Lidarr heart brought in, taken into a job folder and held to the checks a Soulseek
    /// download meets. AcoustID naming another recording, or a live take nobody asked for, throws
    /// the file away: Lidarr has no second copy, so the heart's next source tries instead. A
    /// FLAC made from an MP3 is kept and marked, as Soulseek keeps its last resort.
    /// </summary>
    private async Task<string> AdoptLidarrImportAsync(SoulseekRouting routing, Song song,
        Octo.Services.Lidarr.LidarrImport import, CancellationToken cancellationToken)
    {
        FetchedFrom.AddOrUpdate(song, "Lidarr");
        if (import.Quiet) Muted.AddOrUpdate(song, true);
        if (!IOFile.Exists(import.Path))
            throw new FileNotFoundException($"Lidarr's file for '{routing.Artist} - {routing.Title}' is gone: {import.Path}");

        var jobDir = Path.Combine(DownloadPath, IncomingFolderName, "lidarr", Guid.NewGuid().ToString("N"));
        Directory.CreateDirectory(jobDir);
        var local = Path.Combine(jobDir, Path.GetFileName(import.Path));
        try { IOFile.Move(import.Path, local); }
        catch (IOException)
        {
            // Another volume: copied, then the import removed.
            IOFile.Copy(import.Path, local);
            IOFile.Delete(import.Path);
        }
        TryRemoveEmptyParents(Path.GetDirectoryName(import.Path), DownloadPath);

        if (song is { ExternalProvider: { Length: > 0 } provider, ExternalId: { Length: > 0 } id })
            Track(t => t.Stage(provider, id, AcquisitionState.Verifying, "Lidarr"));
        var verdict = await _verification.VerifyAsync(local, routing.Artist, routing.Title, song.Isrc ?? routing.Isrc,
            refuseLive: !LiveVersion.Requested(routing.Title, routing.Album));
        if (verdict.Verdict == Octo.Services.Fingerprint.VerificationVerdict.Mismatch)
        {
            Logger.LogWarning("Lidarr brought {Actual} for '{Artist} - {Title}' (AcoustID score {Score:P0}); discarding it",
                verdict.Describe(), routing.Artist, routing.Title, verdict.Score);
            try { IOFile.Delete(local); } catch { /* swept with the job folders */ }
            throw new FileNotFoundException($"AcoustID identified Lidarr's file as {verdict.Describe()}");
        }
        var spectrum = await _verification.CheckLosslessAsync(local, routing.Artist, routing.Title);
        if (spectrum.IsLikelyLossy)
        {
            Logger.LogWarning("Lidarr's copy of '{Artist} - {Title}' is {Spectrum}; keeping it, marked as a transcode",
                routing.Artist, routing.Title, spectrum.Describe());
            song.TranscodedFrom = spectrum.Estimate;
        }
        Logger.LogInformation("Took Lidarr's import of '{Artist} - {Title}' into the pipeline: {Path}", routing.Artist, routing.Title, local);
        return local;
    }

    /// <summary>
    /// One song through Lidarr, copied into a job folder under the incoming dot folder, which no
    /// scan looks at. From there it is identified, checked and swapped in like any download.
    /// </summary>
    private async Task<string> DownloadViaLidarrAsync(SoulseekRouting routing, Song song, bool losslessOnly,
        CancellationToken cancellationToken)
    {
        // A heart's album that Lidarr already brought in: the file is here, and only needs taking.
        if (song.ExternalId is { Length: > 0 } externalId
            && OptionalService<Octo.Services.Lidarr.LidarrImportHandoff>()?.Take(externalId) is { } import)
            return await AdoptLidarrImportAsync(routing, song, import, cancellationToken);

        var fetcher = OptionalService<Octo.Services.Lidarr.ILidarrTrackFetcher>()
            ?? throw new InvalidOperationException("Lidarr is not available on this server.");
        if (song is { ExternalProvider: { Length: > 0 } provider, ExternalId: { Length: > 0 } id })
            Track(t => t.Stage(provider, id, AcquisitionState.Searching, "Lidarr", "Lidarr is searching the album"));
        var jobDir = Path.Combine(DownloadPath, IncomingFolderName, "lidarr", Guid.NewGuid().ToString("N"));
        FetchedFrom.AddOrUpdate(song, "Lidarr");
        return await fetcher.FetchAsync(new Octo.Services.Lidarr.LidarrTrackRequest(
            routing.Artist!, routing.Title!, routing.Album, routing.Duration ?? song.Duration, losslessOnly, ReplacingPath),
            jobDir, cancellationToken);
    }

    // Lossy MP3 via the yt-dlp shim's /download. The shim writes <dest>.mp3 into the staging
    // folder with clean tags and a cover; PlaceInLibraryAsync moves it once the tags are settled.
    private async Task<string> DownloadViaYouTubeAsync(SoulseekRouting routing, Song song, bool suppressNotify, bool announceStart, CancellationToken cancellationToken)
    {
        if (string.IsNullOrEmpty(DownloadPath))
            throw new InvalidOperationException("DownloadPath is not configured");

        var trackKey = song.ExternalId ?? "";
        Track(t => t.Stage(ProviderName, trackKey, AcquisitionState.Searching, "YouTube"));

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

        // Staged, not written into the library. Writing straight to the layout path let the shim
        // overwrite a different file that happened to share the name before anything could
        // protect it. Extension left empty: the shim appends .mp3 itself.
        var incoming = Path.Combine(DownloadPath, IncomingFolderName);
        SweepIncoming(incoming);
        var destWithoutExt = Path.Combine(incoming, $"{videoId}-{Guid.NewGuid():N}");

        // The shim answers only once the file is written, so there is nothing to count here.
        Track(t => t.Transfer(ProviderName, trackKey, null, null, null, "YouTube"));
        var path = await _youtube.DownloadAsync(videoId, destWithoutExt, routing.Artist, routing.Title, cancellationToken);
        if (string.IsNullOrEmpty(path) || !IOFile.Exists(path))
            throw new FileNotFoundException($"YouTube MP3 download failed for '{routing.Artist} - {routing.Title}'");

        Logger.LogInformation("YouTube MP3 download complete: {Path}", path);

        // Identification only. YouTube has no second candidate to fall back to, so a
        // disagreement is something to ask a person about (the Review playlist), never a reason
        // to throw the song away.
        Track(t => t.Stage(ProviderName, trackKey, AcquisitionState.Verifying));
        var verdict = await _verification.VerifyAsync(path, routing.Artist, routing.Title, song.Isrc ?? routing.Isrc);
        if (verdict.Verdict == Octo.Services.Fingerprint.VerificationVerdict.Mismatch)
        {
            if (verdict.Match is null && string.IsNullOrEmpty(verdict.MatchedTitle))
            {
                // No decodable audio at all: a broken file, not a question.
                try { IOFile.Delete(path); } catch { /* best effort */ }
                throw new InvalidOperationException(
                    $"YouTube delivered no decodable audio for '{routing.Artist} - {routing.Title}'");
            }
            Logger.LogWarning(
                "AcoustID says the YouTube file for '{Artist} - {Title}' is {Actual}; keeping it and asking about it",
                routing.Artist, routing.Title, verdict.Describe());
            verdict = verdict with
            {
                Verdict = Octo.Services.Fingerprint.VerificationVerdict.Inconclusive,
                Reason = Octo.Services.Fingerprint.InconclusiveReason.SourceDisagreed,
            };
        }
        else verdict.ApplyTagsTo(song);
        song.Verification = verdict;
        return path;
    }

    /// <summary>
    /// Clear what a crash or a failed download left in the staging folder. A day old is long past
    /// any download still in flight, and nothing outside this one folder is touched.
    /// </summary>
    private void SweepIncoming(string incoming)
    {
        try
        {
            if (!Directory.Exists(incoming)) return;
            var root = Path.GetFullPath(incoming) + Path.DirectorySeparatorChar;
            foreach (var file in Directory.EnumerateFiles(incoming))
            {
                if (!Path.GetFullPath(file).StartsWith(root, StringComparison.Ordinal)) continue;
                if (IOFile.GetLastWriteTimeUtc(file) > DateTime.UtcNow.AddHours(-24)) continue;
                IOFile.Delete(file);
                Logger.LogInformation("Removed a stale staged download {Path}", file);
            }
        }
        catch (Exception ex)
        {
            Logger.LogDebug("Could not sweep {Incoming}: {M}", incoming, ex.Message);
        }
    }

    /// <summary>A new job folder, relative to slskd's downloads directory. A dot folder, so
    /// Navidrome's scan never sees a download before Octo has placed it.</summary>
    internal static string NewJobDir() => $"{IncomingFolderName}/slskd/{Guid.NewGuid():N}";

    // Songs whose file an album walk has queued already, by external id, taken by the song's own
    // download. A walk removes whatever it left here when it ends.
    private readonly System.Collections.Concurrent.ConcurrentDictionary<string, PreparedTransfer> _prepared = new();

    // slskd's downloads directory as Octo sees it, once slskd has said; empty when it is not a
    // path here. And roots learned from a job folder found somewhere else.
    private string? _slskdDownloads;
    private readonly System.Collections.Concurrent.ConcurrentDictionary<string, byte> _learnedRoots = new();

    /// <summary>
    /// Where a job folder can be, in order: slskd's own downloads directory when that path exists
    /// here too, the download path, /music, and any root learned from a job folder found elsewhere.
    /// </summary>
    private async Task<IReadOnlyList<string>> JobRootsAsync(CancellationToken ct)
    {
        if (_slskdDownloads is null && await _slskd.GetDownloadsDirectoryAsync(ct) is { } reported)
            _slskdDownloads = Directory.Exists(reported) ? reported : "";
        var roots = new List<string>();
        void Add(string? root)
        {
            if (!string.IsNullOrEmpty(root) && !roots.Contains(root)) roots.Add(root);
        }
        Add(_slskdDownloads);
        Add(DownloadPath);
        Add("/music");
        foreach (var learned in _learnedRoots.Keys) Add(learned);
        return roots;
    }

    /// <summary>
    /// The file an attempt asked slskd to put in its own folder. In that folder only: the leaf
    /// itself, slskd's renamed copy of it, or, since slskd may clean a name up, the one file of
    /// exactly this size, then the one within the usual size drift. Never anything outside it.
    /// </summary>
    internal static string? ResolveInJob(IReadOnlyList<string> roots, string jobDir, string remoteFilename,
        long expectedSize, bool requireExactSize)
    {
        var leaf = remoteFilename.Replace('\\', '/').Split('/', StringSplitOptions.RemoveEmptyEntries).LastOrDefault();
        if (string.IsNullOrEmpty(leaf)) return null;
        var stem = Path.GetFileNameWithoutExtension(leaf);
        var ext = Path.GetExtension(leaf);
        var renamed = new Regex($"^{Regex.Escape(stem)}_[0-9]{{15,}}{Regex.Escape(ext)}$",
            RegexOptions.CultureInvariant | (OperatingSystem.IsWindows() ? RegexOptions.IgnoreCase : 0));

        foreach (var root in roots)
        {
            var folder = Path.Combine(root, jobDir);
            try
            {
                if (!Directory.Exists(folder)) continue;
                var exact = Path.Combine(folder, leaf);
                if (FileMatches(exact, expectedSize, requireExactSize)) return exact;
                var files = Directory.EnumerateFiles(folder).ToList();
                var pick = files.Where(f => renamed.IsMatch(Path.GetFileName(f)) && FileMatches(f, expectedSize, requireExactSize))
                    .OrderByDescending(IOFile.GetCreationTimeUtc).FirstOrDefault();
                if (pick is not null) return pick;
                var sameSize = files.Where(f => new FileInfo(f).Length == expectedSize).ToList();
                if (sameSize.Count == 1) return sameSize[0];
                if (!requireExactSize)
                {
                    var near = files.Where(f => FileMatches(f, expectedSize)).ToList();
                    if (near.Count == 1) return near[0];
                }
            }
            catch
            {
                // A folder that cannot be read holds nothing this attempt can use.
            }
        }
        return null;
    }

    /// <summary>
    /// slskd says the transfer finished, and its file is not in the job folder where Octo looks.
    /// Either slskd's downloads directory is somewhere else under the music folder, and the job
    /// folder is found and its root learned, so downloads stay parallel and exact; or slskd ignored
    /// the folder (a subdirectory pattern of {}), and the file is found by name the old way and
    /// downloads go one at a time from here on.
    /// </summary>
    private string? FindOutsideJob(string jobDir, SoulseekFileHit hit, bool requireExactSize,
        IReadOnlyCollection<string> excluded)
    {
        var job = jobDir.Split('/')[^1];
        var tail = Path.Combine(IncomingFolderName, "slskd", job);
        foreach (var root in new[] { DownloadPath, "/music" }.Where(r => !string.IsNullOrEmpty(r)).Distinct())
        {
            try
            {
                if (!Directory.Exists(root)) continue;
                var folder = Directory.EnumerateDirectories(root, job, SearchOption.AllDirectories)
                    .FirstOrDefault(d => d.EndsWith(tail, StringComparison.Ordinal));
                if (folder is null) continue;
                var learned = folder[..^tail.Length].TrimEnd('/', '\\');
                if (ResolveInJob([learned], jobDir, hit.Filename, hit.Size, requireExactSize) is { } found)
                {
                    _learnedRoots.TryAdd(learned, 0);
                    Logger.LogInformation("slskd's downloads directory is {Root} as Octo sees it; job folders are looked for there too", learned);
                    Concurrency?.Prove();
                    return found;
                }
            }
            catch (Exception ex)
            {
                Logger.LogDebug("Looking for job folder {Job} under {Root} failed: {M}", job, root, ex.Message);
            }
        }

        if (ResolveLanded(hit.Filename, hit.Size, requireExactSize, excluded) is not { } elsewhere) return null;
        Concurrency?.Refuse("slskd put a download outside the folder Octo asked for");
        return elsewhere;
    }

    /// <summary>
    /// Removes job folders a placed or discarded download left empty, after half an hour so an album
    /// batch between two files is left alone, and any job folder a day old. Only inside Octo's own
    /// slskd job folder; nothing else is touched.
    /// </summary>
    private void SweepJobFolders()
    {
        if (string.IsNullOrEmpty(DownloadPath)) return;
        var jobs = Path.Combine(DownloadPath, IncomingFolderName, "slskd");
        try
        {
            if (!Directory.Exists(jobs)) return;
            foreach (var dir in Directory.EnumerateDirectories(jobs))
            {
                var empty = !Directory.EnumerateFileSystemEntries(dir).Any();
                var age = DateTime.UtcNow - Directory.GetLastWriteTimeUtc(dir);
                if (empty ? age < TimeSpan.FromMinutes(30) : age < TimeSpan.FromHours(24)) continue;
                Directory.Delete(dir, recursive: !empty);
                if (!empty) Logger.LogInformation("Removed a stale download folder {Path}", dir);
            }
        }
        catch (Exception ex)
        {
            Logger.LogDebug("Could not sweep {Jobs}: {M}", jobs, ex.Message);
        }
    }

    // The batches an album walk queued, by the walk's prepared track ids, so the walk's end can let
    // go of what it did not use.
    private readonly System.Collections.Concurrent.ConcurrentDictionary<string, (string Username, string JobDir)> _albumBatches = new();

    /// <summary>
    /// One search for the whole album, and the one peer folder that covers most of it queued in one
    /// batch into one job folder. Each covered track then takes its file without a search. Nothing
    /// is prepared (the walk is song by song, as before) when album folders are off, the source has
    /// no Soulseek in it, slskd is not logged in, there are fewer than three tracks, too few lengths
    /// are known, no folder covers half the album, or slskd takes no batches.
    /// </summary>
    protected override async Task<IReadOnlyCollection<string>> PrepareAlbumAsync(Album album, IReadOnlyList<Song> tracks,
        DownloadSource? source, CancellationToken cancellationToken)
    {
        var none = Array.Empty<string>();
        var settings = CurrentSoulseekSettings;
        if (!settings.AlbumFolders || tracks.Count < 3) return none;
        if ((source ?? SubsonicSettings.DownloadSource) is not (DownloadSource.Soulseek or DownloadSource.SoulseekThenYouTube))
            return none;
        if (OptionalService<ISoulseekLink>() is { } link
            && (await link.ReadAsync(fresh: false, cancellationToken))?.Link == SoulseekLinkState.NotLoggedIn)
            return none;
        if (AlbumSearchText(album.Artist, album.Title) is not { } text) return none;

        var wanted = tracks
            .Where(t => !string.IsNullOrEmpty(t.ExternalId) && !string.IsNullOrWhiteSpace(t.Title))
            .Select(t => new AlbumTrack(t.ExternalId!, t.Title!, t.Duration, t.Track))
            .ToList();
        try
        {
            Logger.LogInformation("Soulseek album search: '{Query}' for {Count} tracks", text, wanted.Count);
            var hits = await _slskd.SearchAsync(text, SearchProfile.Album(_settings), cancellationToken);
            var wantedExt = SoulseekClient.NormalizeExtension(_settings.PreferredExtension, "");
            var choice = AlbumFolderPicker.Choose(hits, wanted, hit =>
                CandidateAllowed(hit, _rejectedPeers, _verification.RemembersRejections)
                && string.Equals(hit.Extension, wantedExt, StringComparison.OrdinalIgnoreCase)
                && hit.Size >= _settings.MinFileSizeBytes
                // A studio album is never taken from a live album's folder, nor from a remix or
                // radio edit compilation's.
                && !FromVersionFolder(hit.Filename, "", album.Title, album.Artist));
            if (choice is null)
            {
                Logger.LogInformation("Album '{Album}': no one folder covers enough of it; searching song by song", album.Title);
                return none;
            }

            var jobDir = NewJobDir();
            var batch = await _slskd.EnqueueBatchAsync(choice.Username,
                choice.Files.Select(pair => (pair.File.Filename, pair.File.Size)).ToList(), jobDir, cancellationToken);
            if (!batch.Supported) return none;

            var queued = DateTime.UtcNow;
            var prepared = new List<string>();
            foreach (var (track, file) in choice.Files)
            {
                if (!batch.TransferIds.TryGetValue(file.Filename, out var transferId)) continue;
                _prepared[track.ExternalId] = new PreparedTransfer(file, jobDir, transferId, queued);
                _albumBatches[track.ExternalId] = (choice.Username, jobDir);
                prepared.Add(track.ExternalId);
            }
            Logger.LogInformation("Album '{Album}': {Taken} of {Total} tracks from {User}'s folder '{Folder}' in one batch",
                album.Title, prepared.Count, wanted.Count, choice.Username, choice.Folder);
            return prepared;
        }
        catch (Exception ex) when (ex is not OperationCanceledException)
        {
            Logger.LogWarning("Album '{Album}': the album search failed ({Message}); searching song by song", album.Title, ex.Message);
            return none;
        }
    }

    /// <summary>Drops every song still waiting on one album batch and cancels its transfers.</summary>
    private async Task AbandonAlbumBatchAsync(string jobDir)
    {
        var waiting = _prepared.Where(pair => pair.Value.JobDir == jobDir).Select(pair => pair.Key).ToList();
        if (waiting.Count == 0) return;
        Logger.LogInformation("The album folder's peer did not send; {Count} more songs search on their own", waiting.Count);
        foreach (var id in waiting)
        {
            if (!_prepared.TryRemove(id, out var dropped)) continue;
            try { await _slskd.CancelTransferAsync(dropped.Hit.Username, dropped.Hit.Filename, dropped.TransferId); }
            catch (Exception ex) { Logger.LogDebug("Could not cancel {File}: {M}", dropped.Hit.Filename, ex.Message); }
        }
    }

    /// <summary>
    /// The walk is over: cancel in slskd every prepared transfer no track took, so a folder that did
    /// not work out stops downloading files nobody will use, and remove the walk's job folder once
    /// it is empty.
    /// </summary>
    protected override async Task FinishAlbumAsync(IReadOnlyCollection<string> prepared)
    {
        var folders = new HashSet<string>(StringComparer.Ordinal);
        foreach (var id in prepared)
        {
            if (_albumBatches.TryRemove(id, out var batch)) folders.Add(batch.JobDir);
            if (!_prepared.TryRemove(id, out var unused)) continue;
            try { await _slskd.CancelTransferAsync(unused.Hit.Username, unused.Hit.Filename, unused.TransferId); }
            catch (Exception ex) { Logger.LogDebug("Could not cancel an unused album file {File}: {M}", unused.Hit.Filename, ex.Message); }
        }
        foreach (var jobDir in folders)
        foreach (var root in await JobRootsAsync(CancellationToken.None))
        {
            var folder = Path.Combine(root, jobDir);
            try
            {
                if (Directory.Exists(folder) && !Directory.EnumerateFileSystemEntries(folder).Any()) Directory.Delete(folder);
            }
            catch (Exception ex) { Logger.LogDebug("Could not remove album job folder {Folder}: {M}", folder, ex.Message); }
        }
    }

    /// <summary>
    /// The words of an album search: the artist and the album, with " - Single" or " - EP" and any
    /// bracket taken off, as a song's album search does. Null for a placeholder album name.
    /// </summary>
    internal static string? AlbumSearchText(string? artist, string? albumTitle)
    {
        var who = (artist ?? "").Trim();
        var record = Regex.Replace((albumTitle ?? "").Trim(), @"\s*-\s*(Single|EP)$", "", RegexOptions.IgnoreCase).Trim();
        if (who.Length == 0 || record.Length == 0) return null;
        if (PlaceholderAlbums.Contains(SpaceNormalize(SongIdentity.Plain(record)))) return null;
        var words = Regex.Replace(record, @"\s*[\(\[\{][^\)\]\}]*[\)\]\}]", "").Trim();
        return words.Length == 0 ? null : $"{who} {words}";
    }

    // Lossless FLAC via Soulseek/slskd: walk the top-N peers in quality order,
    // first successful transfer wins.
    private async Task<string> DownloadViaSoulseekAsync(SoulseekRouting routing, Song song, bool suppressNotify,
        SearchProfile profile, CancellationToken cancellationToken)
    {
        var queries = PlannedQueries(routing.Title!, routing.Artist!, routing.Album, routing.Duration);
        var primaryQuery = queries[0].Query.Text;
        var trackKey = song.ExternalId ?? "";
        Track(t => t.Stage(ProviderName, trackKey, AcquisitionState.Searching, "Soulseek"));
        SweepJobFolders();

        // An album walk may have queued this song's file already, from one peer's folder of the
        // album. That file is tried first, without a search; the search runs only if it fails.
        _prepared.TryRemove(trackKey, out var prepared);
        var searched = prepared is null;
        // Whether the album folder's file for this song arrived at all, whatever the checks said.
        var preparedArrived = false;
        var ranked = prepared is null ? await SearchRankedAsync(null) : [prepared.Hit];

        async Task<List<SoulseekFileHit>> SearchRankedAsync(SoulseekFileHit? passOver)
        {
            List<SoulseekFileHit> hits = [];
            List<SoulseekFileHit> found = [];
            // Every query's answers, for the folder look below.
            List<SoulseekFileHit> everyHit = [];
            // One wider search per song at most: each one waits seconds.
            var widened = false;
            foreach (var (query, strict) in queries)
            {
                if (ReferenceEquals(query, queries[0].Query))
                    Logger.LogInformation("Soulseek search-for-star: '{Query}'", query.Text);
                else
                    Logger.LogInformation("Soulseek query returned no usable hits; retrying with '{Query}'", query.Text);
                // Ranked once, on the whole search: slskd hands over a search's answers only when it
                // ends, so there is nothing to stop early on.
                var outcome = await _slskd.SearchWithEndAsync(query.Text, profile, cancellationToken);
                hits = outcome.Hits;
                everyHit.AddRange(hits);
                found = RankCandidates(hits, routing.Title!, routing.Duration, strict, routing.Album, routing.Artist)
                    .Where(h => passOver is null || h.Username != passOver.Username || h.Filename != passOver.Filename)
                    .ToList();
                if (found.Count == 0 && outcome.HitLimit && !widened)
                {
                    // The limit filled before the answers ran out, so the peers still on their way
                    // were never heard. The same words once more, wider, before other words.
                    widened = true;
                    var wider = profile.Wider();
                    Logger.LogInformation(
                        "Soulseek search '{Query}' filled its limit with nothing usable; asking again for up to {Files} files",
                        query.Text, wider.FileLimit);
                    hits = (await _slskd.SearchWithEndAsync(query.Text, wider, cancellationToken)).Hits;
                    everyHit.AddRange(hits);
                    found = RankCandidates(hits, routing.Title!, routing.Duration, strict, routing.Album, routing.Artist)
                        .Where(h => passOver is null || h.Username != passOver.Username || h.Filename != passOver.Filename)
                        .ToList();
                }
                if (found.Count > 0) break;
            }
            if (found.Count == 0)
                found = (await BesideLossyCopiesAsync(everyHit, routing, cancellationToken))
                    .Where(h => passOver is null || h.Username != passOver.Username || h.Filename != passOver.Filename)
                    .ToList();

            // Logged here, once per song, rather than inside RankCandidates. This is the line that
            // explains a track that used to download and now does not.
            if (_verification.RemembersRejections)
            {
                var denied = hits.Count(h => _rejectedPeers.IsDenied(h.Username, h.Filename));
                if (denied > 0)
                    Logger.LogInformation(
                        "Soulseek: {Denied} of {Total} hits for '{Artist} - {Title}' were downloaded before and "
                        + "rejected as the wrong recording, so they are skipped. Use 'Forget rejected peers' on "
                        + "the Soulseek admin page if that is wrong.",
                        denied, hits.Count, routing.Artist, routing.Title);
            }
            return found;
        }

        if (ranked.Count == 0)
            throw new FileNotFoundException(
                $"No Soulseek {_settings.PreferredExtension.ToUpper()} found for '{routing.Artist} - {routing.Title}'");

        Logger.LogInformation("Soulseek: {Count} candidate peers for '{Query}', trying in order",
            ranked.Count, primaryQuery);

        // Read before the first attempt: every resolve in the loop must already know which
        // folder holds slskd's unfinished copies (#69).
        var excluded = await ExcludedFoldersAsync(cancellationToken);

        Exception? lastError = null;
        var startAnnounced = false;

        // A file that claims to be lossless and whose spectrum says it was made from a lossy
        // one. It is still the right song, so it is held back rather than thrown away: a later
        // peer's genuine copy replaces it, and when no peer has one it is what this download
        // delivers, never a reason to fail the song or fall back to YouTube.
        TranscodedReserve? reserve = null;

        // Records the ids of a confirmed match, and its name too when tagging from MusicBrainz
        // is on. Reads and writes the Song, never the routing. It reaches the tagger and
        // PlaceInLibraryAsync because DownloadSongInternalAsync passes ONE Song instance through
        // the download, EnrichAsync, placement and WriteMetadataAsync. A refactor that clones
        // the song between those turns this into a silent no-op.
        //
        // Also writes down who delivered the file. It is the only chance: after the transfer
        // ends nothing else in Octo remembers, and "Wrong song" needs it to blacklist the peer
        // rather than re-rolling the same search.
        string Accept(string path, SoulseekFileHit source, Octo.Services.Fingerprint.VerificationResult verdict,
            string? transcodedFrom)
        {
            verdict.ApplyTagsTo(song);
            song.Verification = verdict;
            song.SourcePeer = source.Username;
            song.SourceFile = source.Filename;
            song.TranscodedFrom = transcodedFrom;
            return path;
        }
        // Every peer tried, across the album folder's copy and the search after it.
        var tried = 0;
        while (true)
        {
            foreach (var (hit, attemptIdx) in ranked.Select((h, i) => (h, i + 1)))
            {
                tried++;
                Logger.LogInformation("Soulseek attempt {N}/{Total}: {User} -> {File} (queue={Q}, speed={S})",
                    attemptIdx, ranked.Count, hit.Username, hit.Filename, hit.QueueLength, hit.UploadSpeed);

                // Each attempt lands in a folder of its own, so finding its file never rests on the
                // file's name, and a download beside it can never claim it. A file an album walk queued
                // is already on its way into the walk's folder.
                var fromAlbum = prepared is not null && ReferenceEquals(hit, prepared.Hit);
                var jobDir = fromAlbum ? prepared!.JobDir : NewJobDir();
                var transferId = fromAlbum ? prepared!.TransferId : null;
                var batched = fromAlbum;

                // Used to decide whether a rejected file is ours to delete.
                var attemptStartedUtc = fromAlbum ? prepared!.QueuedUtc : DateTime.UtcNow;

                if (!fromAlbum)
                {
                    try
                    {
                        var batch = await _slskd.EnqueueBatchAsync(hit.Username, [(hit.Filename, hit.Size)], jobDir, cancellationToken);
                        if (batch.Supported)
                        {
                            if (!batch.TransferIds.TryGetValue(hit.Filename, out transferId))
                                throw new Exception(batch.Failures.Select(f => f.Message).FirstOrDefault(m => m.Length > 0)
                                    ?? "slskd queued nothing");
                            batched = true;
                        }
                        else
                        {
                            await _slskd.EnqueueDownloadAsync(hit.Username, hit.Filename, hit.Size, cancellationToken);
                        }
                    }
                    catch (Exception ex)
                    {
                        Logger.LogWarning("Soulseek enqueue failed for {User} ({Msg}); trying next peer", hit.Username, ex.Message);
                        lastError = ex;
                        continue;
                    }
                }

                // A peer took it, but it can sit in that peer's queue for a while, so this still
                // reads as searching until bytes move. A retry on the next peer starts from nothing.
                Track(t => t.Stage(ProviderName, trackKey, AcquisitionState.Searching, "Soulseek"));

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
                        cancellationToken,
                        // slskd's size is the real one once the peer answers; the search's is
                        // what the peer advertised, kept for polls that leave it out.
                        onProgress: p =>
                        {
                            if (p.IsMoving) Track(t => t.Transfer(ProviderName, trackKey,
                                p.BytesTransferred, p.Size ?? hit.Size, p.PercentComplete, "Soulseek"));
                        },
                        transferId: transferId);
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
                var maxWait = state == SoulseekTransferState.Succeeded ? TimeSpan.FromSeconds(15) : TimeSpan.FromSeconds(5);
                var jobRoots = batched ? await JobRootsAsync(cancellationToken) : [];
                // Where this attempt's file is now, asked again after a check that took seconds.
                string? FindOwnFile() => batched
                    ? ResolveInJob(jobRoots, jobDir, hit.Filename, hit.Size, callerGaveUp)
                    : ResolveLanded(hit.Filename, hit.Size, callerGaveUp, excluded);
                var localPath = batched
                    ? await RetryResolveAsync(FindOwnFile, maxWait, TimeSpan.FromSeconds(1), cancellationToken)
                    : await ResolveLocalPathWithRetryAsync(
                        hit.Filename, hit.Size, excluded,
                        requireExactSize: callerGaveUp,
                        maxWait: maxWait,
                        cancellationToken);
                if (batched && localPath is null && state == SoulseekTransferState.Succeeded)
                    localPath = FindOutsideJob(jobDir, hit, callerGaveUp, excluded);
                else if (batched && localPath is not null)
                    Concurrency?.Prove();
                if (fromAlbum && localPath is not null) preparedArrived = true;
                if (!string.IsNullOrEmpty(localPath) && reserve is not null && SamePath(reserve.Path, localPath))
                {
                    // The resolver matches on leaf name and size, so a peer offering the same rip as
                    // the copy held back can resolve to that very file. It is not a new copy, and
                    // every check below would either repeat itself or, worse, delete the only one.
                    Logger.LogInformation("Soulseek attempt {N} resolved to the copy already held back, not a new file; advancing",
                        attemptIdx);
                    lastError = new Exception("the file found is the copy already held back");
                    if (callerGaveUp) break;
                    continue;
                }

                if (!string.IsNullOrEmpty(localPath))
                {
                    Track(t => t.Stage(ProviderName, trackKey, AcquisitionState.Verifying));

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
                        DenyCandidate(hit, routing, $"delivered {actualSecs}s for a {routing.Duration}s track");
                        lastError = new Exception(
                            $"peer delivered a {actualSecs}s file for a {routing.Duration}s track");
                        continue;
                    }

                    // Apply the configured FolderStructure: slskd dumps to whatever
                    // path the peer used (e.g. ".../MyMusic/Mark Morrison/Return of
                    // the Mack/05 ...flac"), which is unpredictable per-peer. Move
                    // to the canonical location now so Navidrome scans it under a
                    // consistent layout.
                    // Same job as the duration check, one layer deeper: that one proves the file
                    // is the right LENGTH, and a cover, a live take or an unrelated song of the
                    // same runtime all survive it. This asks what the audio actually IS.
                    //
                    // Second on purpose. The check above reads a TagLib header; this one spawns a
                    // process and makes a network call, and neither is worth spending on a file
                    // already known to be wrong.
                    var verdict = await _verification.VerifyAsync(localPath, routing.Artist, routing.Title, song.Isrc ?? routing.Isrc,
                        refuseLive: !LiveVersion.Requested(routing.Title, routing.Album));
                    // fpcalc reports a file that is not there as undecodable audio, which is a Mismatch.
                    // If the file moved during the check, find it and ask again instead of blaming the
                    // peer for slskd's own move.
                    if (verdict.Verdict == Octo.Services.Fingerprint.VerificationVerdict.Mismatch && !IOFile.Exists(localPath)
                        && FindOwnFile() is { } movedTo)
                    {
                        localPath = movedTo;
                        verdict = await _verification.VerifyAsync(localPath, routing.Artist, routing.Title, song.Isrc ?? routing.Isrc,
                        refuseLive: !LiveVersion.Requested(routing.Title, routing.Album));
                    }
                    if (verdict.Verdict == Octo.Services.Fingerprint.VerificationVerdict.Mismatch)
                    {
                        if (!IOFile.Exists(localPath))
                        {
                            // Nothing was judged, so nothing is held against the peer: a deny-list entry
                            // lasts weeks and this one would be wrong.
                            Logger.LogWarning("Soulseek attempt {N}: {Path} disappeared while it was being identified; advancing without blaming {User}",
                                attemptIdx, localPath, hit.Username);
                            lastError = new Exception("the downloaded file disappeared while it was being identified");
                            // Like the held-back copy: with the caller gone, no other peer is tried.
                            if (callerGaveUp) break;
                            continue;
                        }
                        Logger.LogWarning(
                            "Soulseek attempt {N} delivered {Actual} for a request of '{Artist} - {Title}' "
                            + "(AcoustID score {Score:P0}); discarding, remembering the peer and advancing",
                            attemptIdx, verdict.Describe(), routing.Artist, routing.Title, verdict.Score);
                        DiscardRejectedDownload(localPath, attemptStartedUtc);
                        DenyCandidate(hit, routing, verdict.DenyReason);
                        lastError = new Exception($"AcoustID identified the file as {verdict.Describe()}");
                        continue;
                    }

                    // Last, and the only check that never rejects: the right song made from an MP3 is
                    // still the right song. It decides only whether another peer's copy is worth a
                    // try, which is why it runs after the checks that can throw a file away.
                    var spectrum = await _verification.CheckLosslessAsync(localPath, routing.Artist, routing.Title);
                    if (spectrum.IsLikelyLossy)
                    {
                        switch (WeighTranscode(reserve?.Path, reserve?.Spectrum.CutoffHz, localPath, spectrum.CutoffHz))
                        {
                            case ReserveChoice.AlreadyHeld:
                                Logger.LogInformation("Soulseek attempt {N} resolved to the copy already held back; advancing", attemptIdx);
                                break;
                            case ReserveChoice.Hold:
                                if (reserve is not null) DiscardRejectedDownload(reserve.Path, reserve.StartedUtc);
                                reserve = new TranscodedReserve(localPath, hit, attemptIdx, verdict, spectrum, attemptStartedUtc);
                                break;
                            default:
                                DiscardRejectedDownload(localPath, attemptStartedUtc);
                                break;
                        }

                        Logger.LogWarning(
                            "Soulseek attempt {N} for '{Artist} - {Title}' is {Spectrum}; {Plan}",
                            attemptIdx, routing.Artist, routing.Title, spectrum.Describe(),
                            callerGaveUp ? "the caller has left, so no other copy is tried"
                                : "holding it back and trying the next lossless copy");
                        lastError = new Exception($"the file is {spectrum.Describe()}");
                        if (callerGaveUp) break;
                        continue;
                    }

                    // Checked again at the last moment, and before the held-back copy is let go. The
                    // checks above take seconds, and a path that stopped existing in that time would be
                    // placed, tagged and recorded as a download with nothing on disk (#69).
                    if (!FileMatches(localPath, hit.Size, callerGaveUp))
                    {
                        var foundAgain = FindOwnFile();
                        if (foundAgain is null)
                        {
                            Logger.LogWarning("Soulseek attempt {N}: {Path} is gone since it was checked; advancing",
                                attemptIdx, localPath);
                            lastError = new Exception("the downloaded file disappeared before it could be kept");
                            // Like the held-back copy: with the caller gone, no other peer is tried.
                            if (callerGaveUp) break;
                            continue;
                        }
                        localPath = foundAgain;
                    }

                    if (reserve is not null && !SamePath(reserve.Path, localPath))
                    {
                        Logger.LogInformation("Soulseek attempt {N} is a genuine copy; it replaces the transcoded one from attempt {Reserve}",
                            attemptIdx, reserve.Attempt);
                        DiscardRejectedDownload(reserve.Path, reserve.StartedUtc);
                    }

                    // The file stays where slskd put it; PlaceInLibraryAsync moves it once it knows
                    // the album and the credit it will be filed under.
                    Logger.LogInformation("Soulseek download complete (attempt {N}, slskd state={State}{Aborted}): {Path}",
                        attemptIdx, state?.ToString() ?? "interrupted", callerGaveUp ? ", caller had already left" : "", localPath);
                    return Accept(localPath, hit, verdict, transcodedFrom: null);
                }

                if (waitError is not null)
                {
                    // Nothing on disk and the caller is gone: no later peer attempt
                    // has anywhere to be delivered, so stop instead of burning the
                    // rest of the list. A copy held back is still delivered.
                    if (callerGaveUp)
                    {
                        if (reserve is not null) break;
                        throw waitError;
                    }
                    Logger.LogWarning("Soulseek attempt {N} wait failed ({Msg}); advancing", attemptIdx, waitError.Message);
                    lastError = waitError;
                    continue;
                }

                Logger.LogInformation("Soulseek attempt {N} failed (state={State}, no file on disk), advancing", attemptIdx, state);
                lastError = new Exception($"transfer ended in state {state} with no resulting file");
            }

            // The album folder's copy did not work out: now search for this song like any other.
            if (searched || cancellationToken.IsCancellationRequested) break;
            // A peer that never sent this file will not send the rest either: let the walk's other
            // songs search now instead of each waiting out the same silence.
            if (!preparedArrived) await AbandonAlbumBatchAsync(prepared!.JobDir);
            searched = true;
            ranked = await SearchRankedAsync(prepared!.Hit);
            if (ranked.Count == 0) break;
            Logger.LogInformation("Soulseek: the album folder's copy of '{Artist} - {Title}' did not work out; {Count} other peers to try",
                routing.Artist, routing.Title, ranked.Count);
        }

        if (reserve is { } kept)
        {
            if (IOFile.Exists(kept.Path))
            {
                // Kept, not failed: the song asked for is on disk, only not in the quality its
                // extension claims. Written down so it can be found and upgraded later.
                Logger.LogWarning(
                    "Soulseek: no genuine lossless copy of '{Artist} - {Title}' among {Count} candidates; keeping attempt {N} "
                    + "from {User}, which is {Spectrum}: {Path}",
                    routing.Artist, routing.Title, ranked.Count, kept.Attempt, kept.Hit.Username, kept.Spectrum.Describe(), kept.Path);
                return Accept(kept.Path, kept.Hit, kept.Verdict, kept.Spectrum.Estimate);
            }
            Logger.LogWarning("Soulseek: the copy held back from attempt {N} is no longer at {Path}", kept.Attempt, kept.Path);
            lastError = new Exception("the copy held back is no longer on disk");
        }

        throw new Exception(
            $"All {tried} Soulseek peer attempts failed for '{routing.Artist} - {routing.Title}'. Last error: {lastError?.Message}. "
            + $"If slskd shows these transfers as Completed, slskd's downloads directory is not the directory Octo watches ({DownloadPath}); "
            + "set SLSKD_DOWNLOADS_DIR=/music on the slskd container (see issue #17).");
    }

    /// <summary>A song's file an album walk has queued already: the hit, the walk's job folder, the
    /// transfer slskd gave it, and when it was queued, which says whether a rejected file is ours.</summary>
    internal sealed record PreparedTransfer(SoulseekFileHit Hit, string JobDir, string? TransferId, DateTime QueuedUtc);

    /// <summary>A likely transcode held back while other peers are tried, and what it took to get it.</summary>
    private sealed record TranscodedReserve(string Path, SoulseekFileHit Hit, int Attempt,
        Octo.Services.Fingerprint.VerificationResult Verdict, Octo.Services.Fingerprint.SpectrumReport Spectrum,
        DateTime StartedUtc);

    internal enum ReserveChoice { Hold, AlreadyHeld, DiscardNew }

    /// <summary>
    /// What to do with a likely transcode, given the one already held back, if any. The first is
    /// held. A later one replaces it only with a higher cutoff, which is the higher bitrate it
    /// was made from. And the resolver matches on leaf name and size, so a later peer offering
    /// the same rip can resolve to the very file already held back: that is not a new file, and
    /// deleting it as one would lose the only copy.
    /// </summary>
    internal static ReserveChoice WeighTranscode(string? heldPath, double? heldCutoffHz, string path, double? cutoffHz) =>
        heldPath is not null && SamePath(heldPath, path) ? ReserveChoice.AlreadyHeld
        : heldPath is null || (cutoffHz ?? 0) > (heldCutoffHz ?? 0) ? ReserveChoice.Hold
        : ReserveChoice.DiscardNew;

    private static bool SamePath(string a, string b) =>
        string.Equals(Path.GetFullPath(a), Path.GetFullPath(b),
            OperatingSystem.IsWindows() ? StringComparison.OrdinalIgnoreCase : StringComparison.Ordinal);

    /// <summary>
    /// The Soulseek searches for a song, in order, from <see cref="SongIdentity.QueryVariants"/>.
    ///
    /// Peers name files, not catalogue entries, so a query carrying a bracket finds nothing:
    /// Last.fm and YouTube titles such as "Adele - Hello" or "Long Season [LIVE][4K]" are
    /// searched as "Adele Hello" and "Long Season". A title that is only an annotation,
    /// Mezzanine's "(Exchange)", keeps it. Then the stylized spelling read as letters
    /// ("suicideboys SUICIDE", for a peer who tagged it that way), and the title alone last.
    /// At most two queries with the artist and one without: each search waits seconds.
    /// </summary>
    internal static IReadOnlyList<SongQuery> SearchQueries(string title, string artist)
    {
        var variants = SongIdentity.QueryVariants(title, artist);
        var withArtist = variants
            .Where(query => query.Artist.Length > 0 && query.Title.IndexOfAny(['(', '[', '{']) < 0)
            .Take(2);
        var titleOnly = variants.Where(query => query.Artist.Length == 0).Take(1);
        var queries = withArtist.Concat(titleOnly).ToList();
        return queries.Count > 0 ? queries : [new SongQuery((title ?? "").Trim(), (artist ?? "").Trim())];
    }

    /// <summary>
    /// The searches to run, in order, and whether each is read strictly. Strict means the title must
    /// appear as a phrase in the filename and the file must state a length within the window. The
    /// title-only search needs that because the artist is gone. The album search needs it because it
    /// lists the whole record, so every other track on it is an answer too.
    /// </summary>
    internal static IReadOnlyList<(SongQuery Query, bool Strict)> PlannedQueries(string title, string artist,
        string? album, int? durationSeconds)
    {
        var planned = SearchQueries(title, artist).Select(q => (q, q.Artist.Length == 0)).ToList();
        if (AlbumQuery(title, artist, album, durationSeconds) is { } byAlbum) planned.Add((byAlbum, true));
        return planned;
    }

    /// <summary>Album names that say nothing about which record a song is on.</summary>
    private static readonly HashSet<string> PlaceholderAlbums = new(StringComparer.Ordinal)
    {
        "unknown", "unknown album", "single", "singles", "non album", "non album single", "non album tracks",
    };

    /// <summary>
    /// Artist and album, for a peer who files by folder and names tracks only by number and title, so
    /// the artist and title search never reaches the file. Null when the album would add nothing: it
    /// is empty, a placeholder, the title itself, or the length is unknown. Without a length the
    /// strict reading cannot tell which track on the record is the one asked for. Never carries
    /// "flac": slskd matches words in paths, and few paths say it.
    /// </summary>
    internal static SongQuery? AlbumQuery(string? title, string? artist, string? album, int? durationSeconds)
    {
        if (durationSeconds is not > 0) return null;
        var who = (artist ?? "").Trim();
        var record = Regex.Replace((album ?? "").Trim(), @"\s*-\s*(Single|EP)$", "", RegexOptions.IgnoreCase).Trim();
        if (who.Length == 0 || record.Length == 0) return null;
        if (PlaceholderAlbums.Contains(SpaceNormalize(SongIdentity.Plain(record)))) return null;
        if (SongIdentity.Key(record) == SongIdentity.Key(title) || SongIdentity.SameTitle(record, title).IsSame) return null;
        // A title that is the artist's name, or a phrase of the album's, is in the filenames of the
        // record's other tracks too ("Artist - 03 - Another Song"), so the strict reading would take
        // any of them for the song.
        if (SongIdentity.Key(title) == SongIdentity.Key(who) || SongIdentity.SameTitle(title, who).IsSame) return null;
        if (LeafContainsTitlePhrase(SongIdentity.Plain(record), title ?? "")) return null;
        // A bracket finds nothing on Soulseek, the same reason SearchQueries drops one from a title.
        var words = Regex.Replace(record, @"\s*[\(\[\{][^\)\]\}]*[\)\]\}]", "").Trim();
        // SongQuery's Title is just the search words here; Text reads "artist album".
        return words.Length == 0 ? null : new SongQuery(words, who);
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
    /// Whether a file is a version of the song the request did not ask for: a live take, a
    /// remix, a dub, a sped-up upload. This is the only signal that separates "Group Four" from
    /// "Group Four (Security Forces dub)", whose runtimes are two seconds apart.
    ///
    /// The same reading TrackMatchComparer applies to the title AcoustID identified, so a peer
    /// is never chosen for a file the verification would then reject, delete and deny-list. A
    /// remaster, an explicit tag or an "Original Mix" is the same recording and passes. A name
    /// that runs its version on with no brackets ("Too Close-radio edit") is read too.
    /// </summary>
    internal static bool AddsVersion(string filename, string title, string? artist = null) =>
        SongIdentity.AddedVersions(title, LeafTitle(filename)).Count > 0
        || VersionVariant.UnrequestedInName(filename, title, artist).Count > 0;

    /// <summary>How many peers' folders are looked in when a search finds only lossy copies.</summary>
    internal const int PeerFoldersToBrowse = 3;

    /// <summary>How long a peer gets to list one folder.</summary>
    internal static readonly TimeSpan BrowseTimeout = TimeSpan.FromSeconds(20);

    /// <summary>
    /// A search that found the song only lossy (an MP3 where FLAC is preferred) looks in the
    /// folders those copies sit in: a peer often shares an album in both formats, side by side,
    /// and only one format answered the search (#70). The best few peers are asked for that one
    /// folder, and what they list is ranked exactly like search hits, so title, length, version
    /// and live folder all count, and AcoustID and the spectrum still check the download.
    /// </summary>
    private async Task<List<SoulseekFileHit>> BesideLossyCopiesAsync(List<SoulseekFileHit> hits, SoulseekRouting routing,
        CancellationToken cancellationToken)
    {
        var wanted = SoulseekClient.NormalizeExtension(_settings.PreferredExtension, "");
        var title = routing.Title!;
        var lossy = hits
            .Where(h => !string.Equals(h.Extension, wanted, StringComparison.OrdinalIgnoreCase))
            .Where(h => FolderOfFile(h.Filename).Length > 0)
            .Where(h => FilenamePlausiblyMatchesTitle(h.Filename, title, requirePhrase: false))
            .Where(h => DurationPlausible(h.Length, routing.Duration, requireKnownLength: false))
            .Where(h => !AddsVersion(h.Filename, title, routing.Artist))
            .Where(h => !FromVersionFolder(h.Filename, title, routing.Album, routing.Artist))
            .GroupBy(h => (h.Username, Folder: FolderOfFile(h.Filename)))
            .Select(group => group.First())
            .OrderByDescending(h => h.HasFreeUploadSlot == true)
            .ThenBy(h => h.QueueLength ?? int.MaxValue)
            .ThenByDescending(h => h.UploadSpeed ?? 0)
            .Take(PeerFoldersToBrowse)
            .ToList();
        if (lossy.Count == 0) return [];

        var listed = new List<SoulseekFileHit>();
        // One at a time: slskd runs one peer operation at a time anyway.
        foreach (var hit in lossy)
            listed.AddRange(await _slskd.BrowseFolderAsync(hit, FolderOfFile(hit.Filename), BrowseTimeout, cancellationToken));
        var found = RankCandidates(listed, title, routing.Duration, strict: false, routing.Album, routing.Artist);
        Logger.LogInformation(
            "Soulseek: no {Ext} of '{Artist} - {Title}' in the search; looked in {Peers} peers' folders beside their lossy copy and found {Count}",
            wanted.ToUpperInvariant(), routing.Artist, title, lossy.Count, found.Count);
        return found;
    }

    /// <summary>The folder a remote file sits in, as the peer names it; empty at the share's root.</summary>
    internal static string FolderOfFile(string filename)
    {
        var slash = filename.LastIndexOfAny(['\\', '/']);
        return slash > 0 ? filename[..slash] : "";
    }

    /// <summary>
    /// Whether a plainly named file sits in a live album's folder: "Decade (live at the El
    /// Mocambo) (2010)/17 - Smile in Your Sleep.flac" is the live take, though its own name says
    /// nothing. The album folder and the one above it are read (a disc folder sits between);
    /// not the share's top level. Never when the request itself asks for a live song or album.
    /// </summary>
    internal static bool FromLiveFolder(string filename, string title, string? album, string? artist = null)
    {
        if (LiveVersion.Requested(title, album)) return false;
        return VersionVariant.AlbumFolders(filename).Any(folder => LiveVersion.Mentions(VersionVariant.WithoutArtist(folder, artist)));
    }

    /// <summary>
    /// Whether a file's album folder makes it a version nobody asked for, as a live folder does:
    /// "Too Close (Radio Edit) - Single\01 - Too Close.flac" is the radio edit, and "Mezzanine
    /// Remix Tapes '98\03 - Angel.flac" a remix, though neither file name says so. The artist's
    /// own name is never read as a version (a band called Live).
    /// </summary>
    internal static bool FromVersionFolder(string filename, string title, string? album, string? artist = null) =>
        FromLiveFolder(filename, title, album, artist)
        || VersionVariant.UnrequestedFromFolder(filename, title, album, artist).Count > 0;

    private static string LeafTitle(string filename)
    {
        var leaf = LeafOf(filename);
        var dot = leaf.LastIndexOf('.');
        return dot > 0 ? leaf[..dot] : leaf;
    }

    private List<SoulseekFileHit> RankCandidates(List<SoulseekFileHit> hits, string title, int? expectedDuration,
        bool strict = false, string? album = null, string? artist = null) =>
        Rank(hits, new CandidateWant(title, expectedDuration, strict, album, artist),
            _settings.PreferredExtension, _settings.MinFileSizeBytes,
            h => CandidateAllowed(h, _rejectedPeers, _verification.RemembersRejections));

    /// <summary>What a song's search is ranked against: its title, length, album and artist, and
    /// whether the strict reading applies (see <see cref="PlannedQueries"/>).</summary>
    internal sealed record CandidateWant(string Title, int? Duration, bool Strict = false, string? Album = null, string? Artist = null);

    /// <summary>
    /// The one ranking every Soulseek download uses: a star, a play, an album walk's song, Better
    /// quality and the weekly upgrade. Only how wide the search behind it looks differs.
    /// </summary>
    internal static List<SoulseekFileHit> Rank(IEnumerable<SoulseekFileHit> hits, CandidateWant want,
        string preferredExtension, long minFileSizeBytes, Func<SoulseekFileHit, bool> allowed)
    {
        var wanted = SoulseekClient.NormalizeExtension(preferredExtension, "");
        var title = want.Title;
        return hits
            // First because it is the cheapest filter and the only one backed by evidence
            // from a completed transfer: these exact files were downloaded, inspected and
            // found to be the wrong recording. Offering them again spends a whole transfer
            // to reach the same verdict.
            .Where(allowed)
            .Where(h => string.Equals(h.Extension, wanted, StringComparison.OrdinalIgnoreCase))
            .Where(h => h.Size >= minFileSizeBytes)
            .Where(h => FilenamePlausiblyMatchesTitle(h.Filename, title, requirePhrase: want.Strict))
            .Where(h => DurationPlausible(h.Length, want.Duration, requireKnownLength: want.Strict))
            .Where(h => !AddsVersion(h.Filename, title, want.Artist))
            .Where(h => !FromVersionFolder(h.Filename, title, want.Album, want.Artist))
            // "Song (Acoustic)" is never answered by the plain studio file. A radio edit may be
            // named plainly, so that one is only ranked, below.
            .Where(h => !VersionVariant.LacksRequested(h.Filename, title, want.Artist))
            // The copy that says it is the version asked for goes first: "Too Close (Radio Edit)"
            // takes "Too Close [Radio Edit].flac" before a plain "Too Close.flac".
            .OrderBy(h => VersionVariant.Missing(h.Filename, title, want.Artist).Count)
            // An unnamed bracketed addition sorts last rather than being dropped: it may be a
            // different take ("Angel (Angel Dust)"), or only a peer's own label.
            .ThenBy(h => VariantPenalty(h.Filename, title))
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
        SongIdentity.Plain(title)
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
    ///
    /// requirePhrase is the title-only fallback's stricter contract. Scattered tokens
    /// are enough when the artist was in the query, but with the artist gone they are
    /// the whole defense, and "The Truth" scattered across "The Greataxe of Shining
    /// Truth" is how a 136 MB dungeon-synth track answered a country-song star. The
    /// title must then appear as a contiguous phrase in the leaf, with a leading
    /// article allowed to drop ("Truth.flac" still answers "The Truth") and dotted
    /// acronyms allowed their compact form ("MIA.flac" still answers "M.I.A.").
    /// </summary>
    internal static bool FilenamePlausiblyMatchesTitle(string filename, string title, bool requirePhrase = false)
    {
        if (string.IsNullOrEmpty(filename) || string.IsNullOrEmpty(title)) return true;
        title = NameTitle(title);
        // Folded the way titles are, so "Huntin’ Wabbitz" finds "Huntin' Wabbitz" and
        // "Hoppípolla" finds "Hoppipolla".
        var leaf = SongIdentity.Plain(LeafOf(filename));

        var wantedRoman = TrailingRomanNumeral.Match(title.Trim());
        if (wantedRoman.Success
            && !Regex.IsMatch(leaf, $@"\b{Regex.Escape(wantedRoman.Groups[1].Value)}\b", RegexOptions.IgnoreCase))
        {
            return false;
        }

        // Phrase evidence supersedes token scattering rather than adding to it: the
        // token rule would demand a "the" from a filename that legitimately dropped
        // the article, and its scattered matches are exactly what this mode distrusts.
        if (requirePhrase)
            return LeafContainsTitlePhrase(leaf, title)
                || LeafContainsTitlePhrase(Stylized(leaf), SongIdentity.FoldStylized(title));

        // A stylized title ("$UICIDE") and a peer who spelled it out ("Suicide"), either way
        // round: each word may match as written or with its stylized characters read as letters.
        var tokens = TitleTokens(title);
        // Every word of the title is under three letters ("Up", "M.I.A.", "I Am"), so the token rule
        // has nothing to check, and this used to let any file through. Ask for the whole title instead,
        // as words of their own in the filename, spaced or with the dots dropped.
        if (tokens.Count == 0)
            return LeafContainsTitlePhrase(leaf, title)
                || LeafContainsTitlePhrase(Stylized(leaf), SongIdentity.FoldStylized(title));
        var looseLeaf = Stylized(leaf);
        return tokens.All(t => leaf.Contains(t) || looseLeaf.Contains(Stylized(t)));
    }

    private static readonly Regex TitleBracket = new(@"\s*[\(\[\{]([^\(\)\[\]\{\}]*)[\)\]\}]", RegexOptions.Compiled);

    /// <summary>
    /// The part of a title a file name has to carry: the title without its guest credit and its
    /// version tags. "Take Care (feat. Rihanna)" used to demand the words "feat" and "Rihanna",
    /// and "Too Close (Radio Edit)" the words "radio" and "edit", so a plainly named
    /// "04 - Take Care.flac" never answered either and only files that spelled the tag out were
    /// ever downloaded. Which version a file is gets decided by the version rules, not here. A
    /// title with nothing left once they are off is kept whole.
    /// </summary>
    internal static string NameTitle(string title)
    {
        static bool IsVersion(string tag) => SongIdentity.ParseTitle($"x ({tag})").Versions.Count > 0;

        var text = SongIdentity.StripFeatures(title);
        text = TitleBracket.Replace(text, match => IsVersion(match.Groups[1].Value) ? " " : match.Value);
        // " - Radio Edit", " - Remastered 2011": a dash tail that is only a version tag.
        for (var guard = 0; guard < 3; guard++)
        {
            var dash = text.LastIndexOf(" - ", StringComparison.Ordinal);
            if (dash <= 0 || !IsVersion(text[(dash + 3)..])) break;
            text = text[..dash];
        }
        text = Regex.Replace(text, @"\s+", " ").Trim();
        return SongIdentity.Key(text).Length == 0 ? title : text;
    }

    private static string Stylized(string value) => SongIdentity.Plain(SongIdentity.FoldStylized(value));

    private static readonly string[] LeadingArticles = { "the ", "a ", "an " };

    private static bool LeafContainsTitlePhrase(string leaf, string title)
    {
        var leafNorm = $" {SpaceNormalize(leaf)} ";
        var phrase = SpaceNormalize(SongIdentity.Plain(title));
        if (phrase.Length == 0) return true;

        if (leafNorm.Contains($" {phrase} ", StringComparison.Ordinal)) return true;

        var compact = phrase.Replace(" ", "");
        if (compact != phrase && compact.Length >= 3
            && leafNorm.Contains($" {compact} ", StringComparison.Ordinal)) return true;

        // A dropped leading article is tolerated only when what remains names a whole
        // separator-delimited SEGMENT of the filename. As a bare word it proves
        // nothing: "Truth" is the track in "Jason Aldean - Truth" but a fragment in
        // "The Greataxe of Shining Truth", and word-level tolerance here is precisely
        // the hole the phrase rule exists to close.
        foreach (var article in LeadingArticles)
        {
            if (!phrase.StartsWith(article, StringComparison.Ordinal)
                || phrase.Length <= article.Length) continue;
            if (LeafSegments(leaf).Contains(phrase[article.Length..])) return true;
        }
        return false;
    }

    private static List<string> LeafSegments(string leaf)
    {
        var dot = leaf.LastIndexOf('.');
        var withoutExtension = dot > 0 ? leaf[..dot] : leaf;
        return withoutExtension
            .Split(new[] { '-', '_' }, StringSplitOptions.RemoveEmptyEntries)
            .Select(SpaceNormalize)
            .Where(segment => segment.Length > 0)
            .ToList();
    }

    private static string SpaceNormalize(string value) =>
        Regex.Replace(Regex.Replace(value, @"[^\p{L}\p{N}]+", " "), @"\s+", " ").Trim();

    /// <summary>Reject a candidate whose advertised length is nowhere near the known one.</summary>
    internal static bool DurationPlausible(int? candidateSeconds, int? expectedSeconds,
        bool requireKnownLength = false)
    {
        // Unknown either side is normally not evidence of a bad match, so it passes and
        // the post-download check has the final say. The title-only fallback revokes
        // that benefit of the doubt: when the catalog length is known and the artist is
        // no longer in the query, a candidate that will not say how long it is has
        // already used up its plausibility, and the post-download check rejecting it
        // later still costs the full transfer.
        if (candidateSeconds is not int c || c <= 0)
            return !(requireKnownLength && expectedSeconds is > 0);
        if (expectedSeconds is not int e || e <= 0) return true;
        return Math.Abs(c - e) <= DurationToleranceSeconds;
    }

    /// <summary>
    /// How many "this is a different recording" signals the candidate carries that the
    /// requested title never asked for: the version markers <see cref="SongIdentity"/> reads,
    /// and every bracketed addition.
    ///
    /// The generic half matters more than the word list. "Angel (Angel Dust)" and
    /// "Inertia Creeps (Floating on Dubwise)" are both dub mixes, and neither contains a
    /// keyword any sane list would hold, but both are bracketed additions the title did
    /// not ask for, and that is the thing they have in common with every other wrong take.
    ///
    /// Ranked rather than rejected, for the generic half: a bracket may only be a peer's own
    /// label. A named version the request lacks is rejected before ranking, by AddsVersion.
    /// </summary>
    internal static int VariantPenalty(string filename, string title)
    {
        var leaf = LeafOf(filename);
        var wanted = SongIdentity.Plain(title);

        var penalty = SongIdentity.AddedVersions(title, LeafTitle(filename)).Count;

        foreach (Match group in Regex.Matches(leaf, @"[\(\[]([^\)\]]*)[\)\]]"))
        {
            var inner = SongIdentity.Plain(group.Groups[1].Value).Trim();
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
    /// <summary>
    /// Is this candidate still allowed, given what a previous download proved about it?
    ///
    /// Static and separate so the deny-list can be driven in tests without a download
    /// service. The failure it guards against is invisible from outside: a filter that denies
    /// everything leaves every track unfetchable and looks exactly like Soulseek having no
    /// copies.
    /// </summary>
    internal static bool CandidateAllowed(SoulseekFileHit hit, RejectedPeerRegistry? denyList, bool enabled)
        => !enabled || denyList is null || !denyList.IsDenied(hit.Username, hit.Filename);

    /// <summary>
    /// Remember a peer and file we downloaded and rejected, so RankCandidates never offers it
    /// again. Before this, a rejection was deleted and the fact thrown away: the next star
    /// re-ran the same search, ranked the same peer first for the same reasons, and paid for
    /// the same wrong file again.
    ///
    /// Gated on VerifyDownloads so that with the flag off, which is the default, nothing about
    /// the existing behaviour changes, including the memory.
    /// </summary>
    private void DenyCandidate(SoulseekFileHit hit, SoulseekRouting routing, string reason)
    {
        if (!_verification.RemembersRejections) return;
        _rejectedPeers.Deny(hit.Username, hit.Filename, reason, $"{routing.Artist} - {routing.Title}");
    }

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
            Logger.LogInformation("Deleted rejected download {Path}", path);
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
    /// FileMatches rejects a partial copy by size, and the incomplete folder is never
    /// searched (#69), so a full-size copy that slskd has not moved yet cannot end
    /// the wait early. A cancelled caller gets one final check instead of a wait.
    /// </summary>
    private Task<string?> ResolveLocalPathWithRetryAsync(
        string remoteFilename, long expectedSize, IReadOnlyCollection<string> excluded,
        bool requireExactSize, TimeSpan maxWait, CancellationToken ct)
        => RetryResolveAsync(
            () => ResolveLanded(remoteFilename, expectedSize, requireExactSize, excluded),
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

    /// <summary>slskd's default name for the folder a transfer is written into before it is moved.</summary>
    internal const string DefaultIncompleteFolderName = "incomplete";

    private async Task<IReadOnlyCollection<string>> ExcludedFoldersAsync(CancellationToken ct)
    {
        if (_excludedFolders is { } known) return known;
        var configured = await _slskd.GetIncompleteDirectoryAsync(ct);
        var names = ExcludedFolderNames(configured);
        // Kept only when slskd answered, so a failed read is tried again on the next download
        // instead of settling on the default for the life of the process.
        if (configured is not null) _excludedFolders = names;
        return names;
    }

    /// <summary>
    /// The folder names a finished download is never taken from. The default is always in the
    /// list, so the usual layout stays safe even when slskd's options cannot be read.
    /// </summary>
    internal static string[] ExcludedFolderNames(string? slskdIncompleteDir)
    {
        var last = (slskdIncompleteDir ?? "").Replace('\\', '/').TrimEnd('/').Split('/')[^1];
        // Octo's own staging, slskd job folders included: a file there belongs to one download,
        // and a search by name must never hand it to another.
        return string.IsNullOrWhiteSpace(last)
            || string.Equals(last, DefaultIncompleteFolderName, StringComparison.OrdinalIgnoreCase)
            ? [DefaultIncompleteFolderName, IncomingFolderName]
            : [DefaultIncompleteFolderName, last, IncomingFolderName];
    }

    private string? ResolveLanded(string remoteFilename, long expectedSize, bool requireExactSize,
        IReadOnlyCollection<string> excluded)
    {
        var roots = new List<string>();
        if (!string.IsNullOrEmpty(DownloadPath)) roots.Add(DownloadPath);
        if (!roots.Contains("/music")) roots.Add("/music");
        return ResolveLocalPath(remoteFilename, expectedSize, requireExactSize, roots, excluded,
            (root, message) => Logger.LogDebug("Path scan failed under {Root}: {Msg}", root, message));
    }

    private static readonly StringComparison NameComparison =
        OperatingSystem.IsWindows() ? StringComparison.OrdinalIgnoreCase : StringComparison.Ordinal;

    /// <param name="requireExactSize">
    /// Drop the usual near-miss tolerance. Used when a transfer was interrupted,
    /// where a slightly-short file is more likely truncated than size drift.
    /// </param>
    /// <param name="excludedFolders">
    /// slskd's incomplete folder names. A full-size copy there is one slskd is about to move
    /// and delete; taking it is how a song was tagged at a path that was gone a second later (#69).
    /// </param>
    internal static string? ResolveLocalPath(
        string remoteFilename, long expectedSize, bool requireExactSize,
        IReadOnlyList<string> roots, IReadOnlyCollection<string> excludedFolders,
        Action<string, string>? onScanError = null)
    {
        var segments = remoteFilename
            .Replace('\\', '/')
            .Split('/', StringSplitOptions.RemoveEmptyEntries);
        if (segments.Length == 0) return null;

        var leaf = segments[^1];
        var parent = segments.Length >= 2 ? segments[^2] : null;

        bool Usable(string root, string path) =>
            !InExcludedFolder(root, path, parent, excludedFolders) && FileMatches(path, expectedSize, requireExactSize);

        foreach (var root in roots)
        {
            if (parent != null)
            {
                var candidate = Path.Combine(root, parent, leaf);
                if (Usable(root, candidate)) return candidate;
            }
            var flat = Path.Combine(root, leaf);
            if (Usable(root, flat)) return flat;
        }

        // slskd never overwrites: when the name is taken it saves the new file as
        // <name>_<DateTime.UtcNow ticks><ext> in the same folder. Ticks are 18 digits today, so
        // demanding 15 keeps a peer's own "Song_2.flac" out. Only a fallback, behind any exact name.
        var stem = Path.GetFileNameWithoutExtension(leaf);
        var ext = Path.GetExtension(leaf);
        var renamed = new Regex($"^{Regex.Escape(stem)}_[0-9]{{15,}}{Regex.Escape(ext)}$",
            RegexOptions.CultureInvariant | (OperatingSystem.IsWindows() ? RegexOptions.IgnoreCase : 0));

        foreach (var root in roots)
        {
            if (!Directory.Exists(root)) continue;
            try
            {
                var found = Directory
                    .EnumerateFiles(root, stem + "*" + ext, SearchOption.AllDirectories)
                    .Where(p => Usable(root, p))
                    .ToList();
                var pick = Newest(found.Where(p => string.Equals(Path.GetFileName(p), leaf, NameComparison)))
                    ?? Newest(found.Where(p => parent != null
                        && renamed.IsMatch(Path.GetFileName(p))
                        && string.Equals(Path.GetFileName(Path.GetDirectoryName(p)), parent, NameComparison)));
                if (pick is not null) return pick;
            }
            catch (Exception ex)
            {
                onScanError?.Invoke(root, ex.Message);
            }
        }

        return null;

        static string? Newest(IEnumerable<string> paths) =>
            paths.OrderByDescending(p => IOFile.GetCreationTimeUtc(p)).FirstOrDefault();
    }

    /// <summary>
    /// Whether a path sits in one of slskd's incomplete folders below the root. The file's own
    /// folder is exempt when it is the peer's folder name, which slskd keeps when it files a
    /// finished download: a peer who named a folder "incomplete" is not slskd's work area.
    /// </summary>
    internal static bool InExcludedFolder(string root, string path, string? remoteParent,
        IReadOnlyCollection<string> excludedFolders)
    {
        if (excludedFolders.Count == 0) return false;
        var parts = Path.GetRelativePath(root, path).Split(['/', '\\'],StringSplitOptions.RemoveEmptyEntries);
        for (var i = 0; i < parts.Length - 1; i++)
        {
            if (!excludedFolders.Contains(parts[i], StringComparer.OrdinalIgnoreCase)) continue;
            if (i == parts.Length - 2 && string.Equals(parts[i], remoteParent, StringComparison.OrdinalIgnoreCase)) continue;
            return true;
        }
        return false;
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
