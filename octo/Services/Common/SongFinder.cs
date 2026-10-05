using System.Collections.Concurrent;
using Microsoft.Extensions.Options;
using Octo.Models.Settings;
using Octo.Services.Library;
using Octo.Services.Lidarr;
using Octo.Services.Local;
using Octo.Services.Soulseek;

namespace Octo.Services.Common;

/// <summary>
/// The song Find songs looks for: an outside song by its Octo id, or a library song by its
/// Navidrome id, with what the library holds of it now.
/// </summary>
public sealed record FindTarget(string Artist, string Title, string? Album, int? Duration, string CoverArt,
    string? ExternalId = null, string? LibraryId = null, string? OwnedFormat = null, string? OwnedQuality = null,
    long? OwnedSize = null, string? OwnedPath = null);

public static class FindStates
{
    public const string Searching = "searching";
    public const string Done = "done";
    public const string Failed = "failed";
    /// <summary>A source that is not set up, or not one of the person's download sources.</summary>
    public const string Off = "off";
}

/// <summary>How one source's look went: searching, done, failed or off, in words, and what was asked.</summary>
public sealed record FindSource(string Name, string State, string? Text, IReadOnlyList<string> Queries);

/// <summary>One copy on the list: what the apps show, what picking it fetches, and its id on the
/// list, given when it is added and never reused within one look, which pickFoundSong takes.</summary>
public sealed record FoundCopy(AcquisitionCandidate Shown, PickedCopy Pick, string Id = "");

/// <summary>A look as it stands, copied out so it never changes under a reader.</summary>
public sealed record FindSnapshot(string Id, string State, FindTarget Target, IReadOnlyList<FindSource> Sources,
    IReadOnlyList<FoundCopy> Copies, DateTime StartedAt, DateTime UpdatedAt, string? Error);

/// <summary>What picking a copy did: queued (with the downloads row to follow, when there is one)
/// or skipped, with the words why.</summary>
public sealed record PickOutcome(string State, string Detail, string? Key = null);

/// <summary>
/// Find songs: a person runs a song's search again, by hand, across the download sources they
/// use (Soulseek and Lidarr), sees every copy found with its details, and picks the one to fetch.
///
/// A look runs in the background and is read by its id while it goes, the way the apps already
/// follow downloads. It never downloads anything; a pick does, through the same pipeline and
/// checks as any download, with the picked copy instead of a search. A pick for a song already
/// in the library goes through Better quality, so it only ever replaces a copy with a lossless
/// one, and the original stays until the new one passes. Looks are kept in memory for half an
/// hour, then forgotten.
/// </summary>
public sealed class SongFinder
{
    public const string SoulseekSource = "Soulseek";
    public const string LidarrSource = "Lidarr";

    internal static readonly TimeSpan Keep = TimeSpan.FromMinutes(30);
    internal const int Capacity = 40;
    internal const int MaxCopies = 300;

    private static readonly HashSet<string> LosslessFormats = new(StringComparer.OrdinalIgnoreCase)
        { "flac", "wav", "alac", "ape", "aiff", "aif", "wv" };

    private sealed class Job
    {
        public required string Id { get; init; }
        public required string User { get; init; }
        public required FindTarget Target { get; init; }
        public string State { get; set; } = FindStates.Searching;
        public List<FindSource> Sources { get; } = [];
        public List<FoundCopy> Copies { get; } = [];
        /// <summary>The last copy id given out on this look.</summary>
        public int LastCopy { get; set; }
        public DateTime StartedAt { get; init; }
        public DateTime UpdatedAt { get; set; }
        public string? Error { get; set; }
    }

    private readonly ConcurrentDictionary<string, Job> _jobs = new(StringComparer.Ordinal);
    private readonly IServiceProvider _services;
    private readonly ILogger<SongFinder> _logger;
    private readonly TimeProvider _time;

    public SongFinder(IServiceProvider services, ILogger<SongFinder> logger, TimeProvider? time = null)
    {
        _services = services;
        _logger = logger;
        _time = time ?? TimeProvider.System;
        Resolve = ResolveAsync;
        SearchSoulseek = SearchSoulseekAsync;
        SearchLidarr = SearchLidarrAsync;
        SourcesFor = DefaultSources;
        Fetch = FetchAsync;
    }

    // Seams, so tests run a look without slskd, Lidarr or Navidrome.
    internal Func<string, CancellationToken, Task<FindTarget?>> Resolve { get; set; }
    internal Func<FindTarget, CancellationToken, Task<(IReadOnlyList<FoundCopy> Copies, IReadOnlyList<string> Queries, string? Text)>> SearchSoulseek { get; set; }
    internal Func<FindTarget, CancellationToken, Task<(IReadOnlyList<FoundCopy> Copies, IReadOnlyList<string> Queries, string? Text)>> SearchLidarr { get; set; }

    /// <summary>For each source, null when it may be searched, or the words for why not.</summary>
    internal Func<FindTarget, IReadOnlyDictionary<string, string?>> SourcesFor { get; set; }
    internal Func<FindTarget, FoundCopy, string, Task<PickOutcome>> Fetch { get; set; }

    private DateTime Now => _time.GetUtcNow().UtcDateTime;

    private bool Remembers => _services.GetService<Octo.Services.Fingerprint.DownloadVerificationService>()?.RemembersRejections ?? false;

    // ---------------------------------------------------------------------------------------
    // Looking
    // ---------------------------------------------------------------------------------------

    /// <summary>Starts a look for a song by its id, and answers it at once, still searching. Null
    /// when no song has this id.</summary>
    public async Task<FindSnapshot?> StartAsync(string id, string user, CancellationToken ct = default)
    {
        Prune();
        var target = await Resolve(id, ct);
        if (target is null) return null;
        var job = new Job { Id = Guid.NewGuid().ToString("N")[..12], User = user, Target = target, StartedAt = Now, UpdatedAt = Now };
        var allowed = SourcesFor(target);
        lock (job)
        {
            foreach (var (name, refusal) in allowed)
                job.Sources.Add(new FindSource(name, refusal is null ? FindStates.Searching : FindStates.Off, refusal, []));
            if (job.Sources.All(source => source.State == FindStates.Off))
            {
                job.State = FindStates.Failed;
                job.Error = "None of your download sources can search: Soulseek or Lidarr has to be set up and switched on for hearts.";
            }
        }
        _jobs[job.Id] = job;
        if (job.State == FindStates.Searching)
            using (ExecutionContext.SuppressFlow())
                _ = Task.Run(() => RunAsync(job));
        return Snapshot(job);
    }

    /// <summary>A look by its id, for the person who started it.</summary>
    public FindSnapshot? Get(string id, string user)
    {
        Prune();
        return _jobs.TryGetValue(id, out var job) && SameUser(job, user) ? Snapshot(job) : null;
    }

    /// <summary>Waits for a look to end. Only tests need it.</summary>
    internal async Task<FindSnapshot?> WaitAsync(string id, TimeSpan limit)
    {
        var until = DateTime.UtcNow + limit;
        while (DateTime.UtcNow < until)
        {
            if (_jobs.TryGetValue(id, out var job) && job.State != FindStates.Searching) return Snapshot(job);
            await Task.Delay(20);
        }
        return _jobs.TryGetValue(id, out var last) ? Snapshot(last) : null;
    }

    private async Task RunAsync(Job job)
    {
        var searches = new List<Task>();
        foreach (var source in job.Sources.ToList().Where(s => s.State == FindStates.Searching))
            searches.Add(SearchOneAsync(job, source.Name));
        await Task.WhenAll(searches);
        lock (job)
        {
            job.State = job.Sources.Any(s => s.State == FindStates.Done) ? FindStates.Done : FindStates.Failed;
            if (job.State == FindStates.Failed)
                job.Error ??= job.Sources.Select(s => s.Text).FirstOrDefault(text => text is not null) ?? "No source could search.";
            job.UpdatedAt = Now;
        }
    }

    private async Task SearchOneAsync(Job job, string source)
    {
        FindSource result;
        try
        {
            using var limit = new CancellationTokenSource(TimeSpan.FromMinutes(4));
            var (copies, queries, text) = source == SoulseekSource
                ? await SearchSoulseek(job.Target, limit.Token)
                : await SearchLidarr(job.Target, limit.Token);
            result = new FindSource(source, FindStates.Done, text, queries);
            lock (job) Merge(job, copies);
        }
        catch (Exception ex)
        {
            _logger.LogInformation("Find songs on {Source} for '{Artist} - {Title}' failed: {Message}",
                source, job.Target.Artist, job.Target.Title, ex.Message);
            result = new FindSource(source, FindStates.Failed,
                AcquisitionTracker.UserSafe(ex.Message) ?? $"{source} did not answer", []);
        }
        lock (job)
        {
            var at = job.Sources.FindIndex(s => s.Name == source);
            if (at >= 0) job.Sources[at] = result;
            job.UpdatedAt = Now;
        }
    }

    /// <summary>Where a copy goes on the list: each source's own first choices, then the rest,
    /// Soulseek before Lidarr where they tie.</summary>
    private static (int Rank, int Source) Place(FoundCopy copy) =>
        (copy.Shown.Rank ?? int.MaxValue, copy.Shown.Source == SoulseekSource ? 0 : 1);

    /// <summary>
    /// Adds one source's copies to the list, each with an id of its own, in their place. The
    /// copies already listed keep their order and their ids: a source that answers later only
    /// adds rows between them, so nothing a person is looking at moves past another. Caller
    /// holds the job's lock. Each source caps its own list.
    /// </summary>
    private static void Merge(Job job, IReadOnlyList<FoundCopy> copies)
    {
        foreach (var copy in copies.OrderBy(Place).Select(copy => copy with { Id = $"c{++job.LastCopy}" }))
        {
            var place = Place(copy);
            var at = job.Copies.FindLastIndex(listed => Place(listed).CompareTo(place) <= 0) + 1;
            job.Copies.Insert(at, copy);
        }
    }

    // ---------------------------------------------------------------------------------------
    // Picking
    // ---------------------------------------------------------------------------------------

    internal const string StillSearchingText = "Octo is still searching. Pick a copy once the search ends.";

    /// <summary>Fetches exactly the copy with this id on the look's list.</summary>
    public Task<PickOutcome> PickAsync(string id, string copyId, string user) =>
        PickAsync(id, user, copies => copies.FirstOrDefault(copy => copy.Id == copyId.Trim()));

    /// <summary>Fetches the copy at this place on the look's list. Kept for apps that pick by
    /// place; it is only safe once the search has ended, so a pick while it runs is refused.</summary>
    public Task<PickOutcome> PickAsync(string id, int index, string user) =>
        PickAsync(id, user, copies => index >= 0 && index < copies.Count ? copies[index] : null);

    private async Task<PickOutcome> PickAsync(string id, string user, Func<List<FoundCopy>, FoundCopy?> find)
    {
        if (!_jobs.TryGetValue(id, out var job) || !SameUser(job, user))
            return new(LibraryActionStates.Skipped, "This search is gone. Search again.");
        FoundCopy? copy;
        lock (job)
        {
            // A source that answers later adds copies to the list, so a pick waits for the end.
            if (job.State == FindStates.Searching) return new(LibraryActionStates.Skipped, StillSearchingText);
            copy = find(job.Copies);
        }
        if (copy is null) return new(LibraryActionStates.Skipped, "That copy is not on the list.");
        try
        {
            return await Fetch(job.Target, copy, user);
        }
        catch (Exception ex)
        {
            _logger.LogWarning("Picking a copy of '{Artist} - {Title}' failed: {Message}", job.Target.Artist, job.Target.Title, ex.Message);
            return new(LibraryActionStates.Skipped, AcquisitionTracker.UserSafe(ex.Message) ?? "The copy could not be asked for.");
        }
    }

    /// <summary>
    /// Why a picked copy cannot replace a library song, or null when it can: only a lossless copy
    /// replaces a lossy one, which is what Better quality checks again when the file arrives, and
    /// a Soulseek file must pass the download's own name, length and version checks (see
    /// <see cref="SoulseekCandidates.PickRefusal"/>).
    /// </summary>
    internal static string? ReplaceRefusal(FindTarget target, FoundCopy copy, SoulseekSettings? settings = null,
        RejectedPeerRegistry? rejected = null, bool remembers = false)
    {
        if (IsLossless(target.OwnedFormat)) return "Your copy is already lossless, so nothing would be better.";
        if (copy.Pick.IsSoulseek && !IsLossless(copy.Shown.Format))
            return "Only a lossless copy can take the place of the one in your library.";
        if (!copy.Pick.IsSoulseek && copy.Pick.Release is not null && !IsLossless(FirstWord(copy.Shown.Format)))
            return "Only a lossless release can take the place of the one in your library.";
        if (copy.Pick.ToHit() is { } hit
            && SoulseekCandidates.PickRefusal(hit, target.Title, target.Album, target.Duration, settings ?? new SoulseekSettings(),
                rejected, remembers, target.Artist, replacing: true) is { } why)
            return $"{why}, so it cannot take the place of the one in your library.";
        return null;
    }

    /// <summary>Why a picked copy of a song not in the library may not be fetched, or null.</summary>
    internal static string? PickRefusal(FindTarget target, FoundCopy copy, RejectedPeerRegistry? rejected, bool remembers) =>
        copy.Pick.ToHit() is { } hit
        && SoulseekCandidates.PickRefusal(hit, target.Title, target.Album, target.Duration, new SoulseekSettings(),
            rejected, remembers, target.Artist, replacing: false) is { } why
            ? $"{why}, so it is not fetched again."
            : null;

    private async Task<PickOutcome> FetchAsync(FindTarget target, FoundCopy copy, string user)
    {
        if (target.LibraryId is { } libraryId)
        {
            if (ReplaceRefusal(target, copy, _services.GetService<IOptionsMonitor<SoulseekSettings>>()?.CurrentValue,
                    _services.GetService<RejectedPeerRegistry>(), Remembers) is { } refused)
                return new(LibraryActionStates.Skipped, refused);
            var settings = _services.GetService<IOptionsMonitor<LibraryActionSettings>>()?.CurrentValue;
            var queue = _services.GetService<UpgradeQueue>();
            var sources = _services.GetService<UpgradeSources>();
            var gate = UpgradeGate.Refusal(settings, user, queue is not null && _services.GetService<LibraryActionExecutor>() is not null,
                sources?.Ready ?? false, sources?.Name ?? "Soulseek");
            if (gate is not null) return new(LibraryActionStates.Skipped, gate);
            var (jobs, full) = queue!.Add([new UpgradeAsk(libraryId, target.Title, target.Artist, target.Album, target.OwnedFormat, Pick: copy.Pick)],
                user, "find");
            if (jobs.Count == 0) return new(LibraryActionStates.Skipped, full ?? "The upgrade queue is full.");
            if (!copy.Pick.SameAs(jobs[0].Pick))
            {
                // The song already waits in the queue. Picking again before it starts means the
                // person changed their mind, so the newer pick takes the old one's place. Once it
                // runs, or when it is someone else's, the copy it fetches is settled.
                if (queue.ReplacePick(libraryId, user, copy.Pick) is null)
                    return new(LibraryActionStates.Skipped, jobs[0].Pick is null
                        ? "A higher quality copy of this song is already being looked for. Pick again once it ends."
                        : "Another copy of this song is already on its way. Pick again once it ends.");
                _logger.LogInformation("Find songs: {User} changed the pick for '{Artist} - {Title}' to {Copy}",
                    user, target.Artist, target.Title, copy.Pick.Describe());
                return new(LibraryActionStates.Queued,
                    "Changed to the copy you picked. Your copy stays until the new one passes every check.");
            }
            _logger.LogInformation("Find songs: {User} picked {Copy} to replace '{Artist} - {Title}'",
                user, copy.Pick.Describe(), target.Artist, target.Title);
            return new(LibraryActionStates.Queued, "Getting the copy you picked. Your copy stays until the new one passes every check.");
        }

        if (PickRefusal(target, copy, _services.GetService<RejectedPeerRegistry>(), Remembers) is { } denied)
            return new(LibraryActionStates.Skipped, denied);
        var externalId = target.ExternalId!;
        var provider = SoulseekMetadataService.ProviderName;
        var tracker = _services.GetService<AcquisitionTracker>();
        var key = AcquisitionTracker.KeyOf(provider, externalId);
        if (tracker?.IsRunning(key) == true)
            return new(LibraryActionStates.Skipped, "This song is downloading right now. Pick again once it ends.");
        var acquisitions = _services.GetRequiredService<TrackAcquisitionQueue>();
        _services.GetRequiredService<DownloadPicks>().Pin(externalId, copy.Pick);
        tracker?.Begin(provider, externalId, externalId, user, target.Artist, target.Title, target.Album, AcquisitionKinds.Pick);
        tracker?.Stage(provider, externalId, AcquisitionState.Queued, copy.Pick.Source, $"Getting {copy.Pick.Describe()}");
        _logger.LogInformation("Find songs: {User} picked {Copy} for '{Artist} - {Title}'", user, copy.Pick.Describe(), target.Artist, target.Title);
        var download = acquisitions.Enqueue(provider, externalId, isStar: true, triggerAlbumDownload: false, forcePermanent: true,
            sourceOverride: copy.Pick.IsSoulseek ? DownloadSource.Soulseek : DownloadSource.Lidarr,
            notifyOnFailure: true, requestedBy: user);
        _ = download.ContinueWith(task => tracker?.Fail(provider, externalId, task.Exception?.GetBaseException().Message),
            CancellationToken.None, TaskContinuationOptions.OnlyOnFaulted, TaskScheduler.Default);
        return new(LibraryActionStates.Queued, $"Getting {copy.Pick.Describe()}.", key);
    }

    // ---------------------------------------------------------------------------------------
    // The song, and where it may be looked for
    // ---------------------------------------------------------------------------------------

    private async Task<FindTarget?> ResolveAsync(string id, CancellationToken ct)
    {
        var raw = id.Trim();
        if (raw.StartsWith("find:", StringComparison.OrdinalIgnoreCase)) raw = raw[5..];
        var provider = SoulseekMetadataService.ProviderName;
        if (raw.StartsWith(provider + ":", StringComparison.OrdinalIgnoreCase)) raw = raw[(provider.Length + 1)..];
        var library = _services.GetRequiredService<ILocalLibraryService>();
        if (raw.StartsWith("ext-", StringComparison.Ordinal) && library.ParseSongId(raw) is (true, _, { } outside)) raw = outside;
        if (raw.Length == 0) return null;

        var routing = _services.GetService<ExternalIdRegistry>()?.Lookup(raw) ?? SoulseekMetadataService.TryDecodeExternalId(raw);
        var resolver = _services.GetService<NavidromeSongPathResolver>();
        if (routing is { Kind: RoutingKind.Song, HasArtistTitle: true })
        {
            // Downloaded since it was found: the library's copy is the one a pick would replace.
            var local = await library.GetLocalPathForExternalSongAsync(provider, raw);
            if (local is not null && File.Exists(local) && resolver is not null
                && await resolver.FindIdByPathAsync(routing.Artist!, routing.Title!, local, ct) is { } libraryId
                && await LibrarySongAsync(resolver, libraryId, ct) is { } owned)
                return owned;
            return new FindTarget(routing.Artist!, routing.Title!, routing.Album, routing.Duration, raw, ExternalId: raw);
        }
        return resolver is null ? null : await LibrarySongAsync(resolver, raw, ct);
    }

    private static async Task<FindTarget?> LibrarySongAsync(NavidromeSongPathResolver resolver, string id, CancellationToken ct)
    {
        var song = await resolver.ResolveAsync(id, ct);
        if (song is null) return null;
        return new FindTarget(song.Artist, song.Title, string.IsNullOrWhiteSpace(song.Album) ? null : song.Album, song.DurationSeconds,
            song.NavidromeId, LibraryId: song.NavidromeId, OwnedFormat: song.Suffix,
            OwnedQuality: AudioSummary.Describe(song.AbsolutePath).Text ?? song.Suffix?.ToUpperInvariant(),
            OwnedSize: song.SizeBytes > 0 ? song.SizeBytes : null, OwnedPath: song.AbsolutePath);
    }

    /// <summary>Each source, and why it may not be searched. An outside song uses the sources
    /// hearts download from; a library song the sources Better quality uses.</summary>
    private IReadOnlyDictionary<string, string?> DefaultSources(FindTarget target)
    {
        var soulseekUp = UpgradeSources.SoulseekSetUp(_services.GetService<IOptionsMonitor<SoulseekSettings>>()?.CurrentValue ?? new SoulseekSettings());
        var lidarrUp = UpgradeSources.LidarrSetUp(_services.GetService<IOptionsMonitor<LidarrSettings>>()?.CurrentValue ?? new LidarrSettings());
        bool soulseekUsed, lidarrUsed;
        if (target.LibraryId is not null)
        {
            var wanted = _services.GetService<UpgradeSources>()?.Wanted() ?? [DownloadSource.Soulseek];
            soulseekUsed = wanted.Contains(DownloadSource.Soulseek);
            lidarrUsed = wanted.Contains(DownloadSource.Lidarr);
        }
        else
        {
            var steps = _services.GetService<IOptionsMonitor<SubsonicSettings>>()?.CurrentValue.EffectiveHeartDownloadSources() ?? [];
            soulseekUsed = steps.Any(step => step.Source == HeartDownloadSource.Soulseek && step.SongEnabled == true);
            lidarrUsed = steps.Any(step => step.Source == HeartDownloadSource.Lidarr && step.SongEnabled == true);
        }
        return new Dictionary<string, string?>
        {
            [SoulseekSource] = !soulseekUp ? "Soulseek is not set up on this server"
                : !soulseekUsed ? "Soulseek is not one of your download sources" : null,
            [LidarrSource] = !lidarrUp ? "Lidarr is not set up on this server"
                : !lidarrUsed ? "Lidarr is not one of your download sources" : null,
        };
    }

    // ---------------------------------------------------------------------------------------
    // The searches
    // ---------------------------------------------------------------------------------------

    private async Task<(IReadOnlyList<FoundCopy>, IReadOnlyList<string>, string?)> SearchSoulseekAsync(FindTarget target,
        CancellationToken ct)
    {
        if (_services.GetService<IDownloadService>() is not SoulseekDownloadService soulseek)
            throw new InvalidOperationException("Soulseek is not available on this server.");
        var link = _services.GetService<ISoulseekLink>();
        if (link is not null && (await link.ReadAsync(fresh: false, ct))?.Link == SoulseekLinkState.NotLoggedIn)
            throw new InvalidOperationException(SoulseekLink.OfflineText);

        var found = await soulseek.FindCopiesAsync(target.Artist, target.Title, target.Album, target.Duration, ct);
        var settings = _services.GetService<IOptionsMonitor<SoulseekSettings>>()?.CurrentValue ?? new SoulseekSettings();
        var rejected = _services.GetService<RejectedPeerRegistry>();
        var remembers = _services.GetService<Octo.Services.Fingerprint.DownloadVerificationService>()?.RemembersRejections ?? false;
        var copies = SoulseekCopies(found.Hits, found.Ranked, target, settings, rejected, remembers);
        var peers = found.Hits.Select(h => h.Username).Distinct(StringComparer.OrdinalIgnoreCase).Count();
        var text = found.Hits.Count == 0 ? "Nothing found"
            : $"{found.Hits.Count} files from {peers} {(peers == 1 ? "peer" : "peers")}; {found.Ranked.Count} {(found.Ranked.Count == 1 ? "fits" : "fit")} the song";
        return (copies, found.Queries, text);
    }

    /// <summary>
    /// Soulseek's files as the list shows them: the ones the download would try first, in its
    /// order, then every other file, the likeliest first, each with the reason it was passed over.
    /// </summary>
    internal static IReadOnlyList<FoundCopy> SoulseekCopies(IReadOnlyList<SoulseekFileHit> hits, IReadOnlyList<SoulseekFileHit> ranked,
        FindTarget target, SoulseekSettings settings, RejectedPeerRegistry? rejected, bool remembers)
    {
        var order = ranked.Select((hit, i) => (hit, i)).ToDictionary(pair => (pair.hit.Username, pair.hit.Filename), pair => pair.i + 1);
        return hits
            .Where(hit => !string.IsNullOrEmpty(hit.Username) && !string.IsNullOrEmpty(hit.Filename))
            .Select(hit =>
            {
                int? rank = order.TryGetValue((hit.Username, hit.Filename), out var place) ? place : null;
                var (why, tier) = rank is not null ? (null, 0)
                    : SoulseekCandidates.Judge(hit, target.Title, target.Album, target.Duration, settings, rejected, remembers, target.Artist);
                var note = rank is null ? why ?? "Fits too; a download tries the first few" : null;
                return (hit, rank, note, tier);
            })
            .OrderBy(row => row.rank ?? int.MaxValue)
            .ThenBy(row => row.tier)
            .ThenByDescending(row => SoulseekCandidates.IsLossless(row.hit))
            .ThenByDescending(row => row.hit.HasFreeUploadSlot == true)
            .ThenBy(row => row.hit.QueueLength ?? int.MaxValue)
            .ThenByDescending(row => row.hit.Size)
            .Take(MaxCopies)
            .Select(row => new FoundCopy(SoulseekCandidates.Of(row.hit, row.rank, row.note), new PickedCopy
            {
                Source = SoulseekSource, Peer = row.hit.Username, File = row.hit.Filename, Size = row.hit.Size,
                Format = SoulseekCandidates.Extension(row.hit), BitRate = row.hit.BitRate, BitDepth = row.hit.BitDepth,
                SampleRate = row.hit.SampleRate, Length = row.hit.Length,
            }))
            .ToList();
    }

    private async Task<(IReadOnlyList<FoundCopy>, IReadOnlyList<string>, string?)> SearchLidarrAsync(FindTarget target,
        CancellationToken ct)
    {
        if (_services.GetService<ILidarrTrackFetcher>() is not LidarrTrackFetcher fetcher || _services.GetService<LidarrClient>() is not { } client)
            throw new InvalidOperationException("Lidarr is not available on this server.");
        var album = await fetcher.FindAlbumAsync(new LidarrTrackRequest(target.Artist, target.Title, target.Album, target.Duration,
            LosslessOnly: target.LibraryId is not null, OriginalPath: target.OwnedPath), ct);
        if (album is null) return ([], [], "Lidarr knows no album with this song on it");

        var record = $"{album.Artist} - {album.Title}{(album.Year is > 0 ? $" ({album.Year})" : "")}";
        var copies = new List<FoundCopy>
        {
            new(new AcquisitionCandidate(LidarrSource, File: record, Rank: 1, Title: target.Title, Album: album.Title,
                    Note: "Lidarr searches the album and takes the release your quality profile prefers"),
                new PickedCopy { Source = LidarrSource }),
        };
        var existing = await client.FindAlbumAsync(album.ForeignAlbumId, ct);
        if (existing is null)
            return (copies, [record], "This album is not in Lidarr yet, so its releases cannot be listed; Lidarr's choice still works");
        var releases = await client.ListReleasesAsync(existing.Id, ct);
        copies.AddRange(LidarrCopies(releases, target, album.Title));
        return (copies, [record], releases.Count == 0 ? "The indexers offered no release of this album"
            : $"{releases.Count} {(releases.Count == 1 ? "release" : "releases")} of {album.Title}");
    }

    /// <summary>Lidarr's releases as the list shows them, Lidarr's own order kept. A release it
    /// would take on its own is ranked after Lidarr's choice; one it rejects says why.</summary>
    internal static IReadOnlyList<FoundCopy> LidarrCopies(IReadOnlyList<LidarrRelease> releases, FindTarget target, string albumTitle)
    {
        var rank = 1;
        return releases.Take(MaxCopies).Select(release => new FoundCopy(
            new AcquisitionCandidate(LidarrSource, Peer: release.Indexer, File: release.Title, Format: release.Quality,
                Size: release.Size, QueueLength: null, FreeSlot: null, Rank: release.Rejected ? null : ++rank,
                Note: release.Rejected
                    ? release.Rejections.FirstOrDefault() ?? "Lidarr would not take this release on its own"
                    : string.Join(", ", new[]
                    {
                        release.Protocol,
                        release.Seeders is { } seeders ? $"{seeders} {(seeders == 1 ? "seeder" : "seeders")}" : null,
                        release.AgeDays is { } days ? $"{days} {(days == 1 ? "day" : "days")} old" : null,
                    }.Where(part => !string.IsNullOrEmpty(part))),
                Title: target.Title, Album: albumTitle),
            new PickedCopy { Source = LidarrSource, ReleaseGuid = release.Guid, IndexerId = release.IndexerId, ReleaseTitle = release.Title }))
            .ToList();
    }

    // ---------------------------------------------------------------------------------------
    // Helpers
    // ---------------------------------------------------------------------------------------

    internal static bool IsLossless(string? format) => format is { } kind && LosslessFormats.Contains(kind.Trim().TrimStart('.'));

    // Lidarr names qualities "FLAC", "FLAC 24bit", "MP3-320".
    private static string? FirstWord(string? quality) =>
        quality?.Split([' ', '-'], StringSplitOptions.RemoveEmptyEntries).FirstOrDefault();

    private static bool SameUser(Job job, string user) => string.Equals(job.User, user.Trim(), StringComparison.OrdinalIgnoreCase);

    private static FindSnapshot Snapshot(Job job)
    {
        lock (job)
            return new FindSnapshot(job.Id, job.State, job.Target, job.Sources.ToList(), job.Copies.ToList(),
                job.StartedAt, job.UpdatedAt, job.Error);
    }

    private void Prune()
    {
        var now = Now;
        foreach (var (id, job) in _jobs)
            if (now - job.UpdatedAt >= Keep && job.State != FindStates.Searching) _jobs.TryRemove(id, out _);
        if (_jobs.Count < Capacity) return;
        foreach (var (id, _) in _jobs.OrderBy(pair => pair.Value.UpdatedAt).Take(_jobs.Count - Capacity + 1).ToList())
            _jobs.TryRemove(id, out _);
    }
}

/// <summary>The states a pick, or an app's library action, answers with on the wire.</summary>
public static class LibraryActionStates
{
    public const string Queued = "queued";
    public const string Skipped = "skipped";
}

/// <summary>
/// Whether someone may have a higher quality copy fetched, read now, so an app hears a reason
/// straight away. The upgrade queue's executor checks all of it again when the job runs.
/// </summary>
public static class UpgradeGate
{
    public static string? Refusal(LibraryActionSettings? settings, string? username, bool available, bool ready, string sourceName) =>
        string.IsNullOrWhiteSpace(username) ? "Sign in with a username to upgrade songs; an API key alone does not say who is asking."
        : settings is null || !available || !settings.Enabled ? "Library actions are off."
        : !settings.IsAllowed(username) ? $"{username} is not on the library actions allowed list."
        : !settings.EffectiveActions().Any(a => a.Action == LibraryAction.BetterQuality && a.Enabled) ? "Better quality is not switched on."
        : settings.DryRun ? "Library actions only rehearse while dry run is on, so nothing would change."
        : !ready ? $"Better quality looks for copies on {sourceName}, which is not set up on this server."
        : null;
}
