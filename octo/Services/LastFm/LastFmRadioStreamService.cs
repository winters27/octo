using System.Collections.Concurrent;
using Microsoft.Extensions.Options;
using Octo.Models.Domain;
using Octo.Models.Radio;
using Octo.Models.Settings;
using Octo.Services.Common;
using Octo.Services.Local;
using Octo.Services.Soulseek;
using Octo.Services.Subsonic;

namespace Octo.Services.LastFm;

/// <summary>Turns a ready generated station into one long MP3 response. Recommendation
/// state and stream orchestration stay in core Octo; ffmpeg is only a codec adapter.</summary>
public sealed class LastFmRadioStreamService
{
    public const int ReadyPoolSize = 3;
    private static readonly SemaphoreSlim ConcurrentStreams = new(8, 8);
    private readonly LastFmRadioStateStore _state;
    private readonly IOptionsMonitor<LastFmSettings> _settings;
    private readonly ILocalLibraryService _library;
    private readonly SubsonicProxyService _proxy;
    private readonly IDownloadService _downloads;
    private readonly ILastFmRadioAudioTranscoder _transcoder;
    private readonly LastFmRadioTrackCache _cache;
    private readonly LastFmRadioStreamSessionStore _sessions;
    private readonly LastFmRadioTrackResolver _resolver;
    private readonly IMusicMetadataService _metadata;
    private readonly RadioQueueStore _queues;
    private readonly LastFmRadioRefreshQueue _refreshQueue;
    private readonly IRadioTuneInSelector _tuneIn;
    private readonly LastFmService? _lastFm;
    private readonly ILogger<LastFmRadioStreamService> _logger;
    private readonly ConcurrentDictionary<string, Task> _poolWarmers = new();

    public LastFmRadioStreamService(LastFmRadioStateStore state,
        IOptionsMonitor<LastFmSettings> settings, ILocalLibraryService library,
        SubsonicProxyService proxy, IDownloadService downloads,
        ILastFmRadioAudioTranscoder transcoder, LastFmRadioTrackCache cache,
        LastFmRadioStreamSessionStore sessions,
        LastFmRadioTrackResolver resolver,
        IMusicMetadataService metadata,
        RadioQueueStore queues, LastFmRadioRefreshQueue refreshQueue,
        IRadioTuneInSelector tuneIn,
        ILogger<LastFmRadioStreamService> logger,
        LastFmService? lastFm = null,
        Octo.Services.ListenBrainz.ListenBrainzService? listenBrainz = null)
    {
        _state = state; _settings = settings; _library = library; _proxy = proxy;
        _downloads = downloads; _transcoder = transcoder; _cache = cache;
        _sessions = sessions;
        _resolver = resolver; _metadata = metadata;
        _queues = queues; _refreshQueue = refreshQueue; _tuneIn = tuneIn; _logger = logger;
        _lastFm = lastFm;
        _listenBrainz = listenBrainz;
    }

    private readonly Octo.Services.ListenBrainz.ListenBrainzService? _listenBrainz;

    public LastFmRadioStation? Resolve(LastFmRadioStreamSession session)
    {
        if (!_settings.CurrentValue.EnableRadio || !_settings.CurrentValue.ExposeRadioAsStreams)
            return null;
        var station = _state.FindStation(session.Username, session.StationId);
        if (station is not null && (station.Personalized
                ? !_settings.CurrentValue.EnablePersonalizedStations
                : !_settings.CurrentValue.EnableDiscoveryStations)) return null;
        return station is { Tracks.Count: > 0 } ? station : null;
    }

    public async Task StreamAsync(LastFmRadioStreamSession session, Stream output,
        CancellationToken cancellationToken, bool includeIcyMetadata = false)
    {
        await ConcurrentStreams.WaitAsync(cancellationToken);
        try
        {
            var icyOutput = includeIcyMetadata ? new IcyMetadataStream(output) : null;
            var streamOutput = (Stream?)icyOutput ?? output;
            var station = Resolve(session)
                ?? throw new InvalidOperationException("Radio station is no longer available");
            var tracks = station.Tracks.Where(track => !string.IsNullOrWhiteSpace(track.ResolvedId)).ToList();
            if (tracks.Count == 0) throw new InvalidOperationException("Radio station has no playable tracks");
            var ids = tracks.Select(track => track.ResolvedId!).ToList();
            _queues.Register(ids);
            _ = _metadata.PrewarmYouTubeIdsForSongIdsAsync(ids, topN: 8);
            // A published session starts with three complete MP3 segments. Keep those
            // exact tracks even if the recommendation snapshot changes before tune-in;
            // the next replenishment crosses onto the current snapshot cleanly.
            var ready = (session.ReadyPool ?? [])
                .Where(item => _cache.IsReadyPath(item.Path)).ToList();
            foreach (var cached in GetReadyPool(session))
                if (ready.Count < ReadyPoolSize
                    && ready.All(item => item.CacheKey != cached.CacheKey))
                    ready.Add(cached);
            if (ready.Count == 0)
                ready = (await PrepareReadyPoolAsync(session, 1, cancellationToken)).ToList();
            if (ready.Count == 0)
                throw new InvalidOperationException("Radio station has no cached ready track");
            _sessions.AttachReadyPool(session.Token, ready);

            var queue = new Queue<PreparedRadioTrack>(ready);
            var nextIndex = (ready[^1].Index + 1) % tracks.Count;
            var failures = 0;

            while (!cancellationToken.IsCancellationRequested)
            {
                if (queue.Count == 0)
                {
                    var emergency = await PrepareNextAsync(session, nextIndex,
                        new HashSet<string>(StringComparer.Ordinal),
                        cancellationToken);
                    if (emergency is null)
                        throw new InvalidOperationException(
                            "No tracks in this Radio snapshot have a playable source");
                    queue.Enqueue(emergency);
                    nextIndex = emergency.Index + 1;
                }

                var prepared = queue.Dequeue();
                _sessions.ConsumeReadyTrack(session.Token, prepared.CacheKey);
                var reserved = queue.Select(item => item.CacheKey).ToHashSet(StringComparer.Ordinal);
                var replenishment = PrepareNextAsync(session, nextIndex, reserved,
                    CancellationToken.None, _cache.GetProfile(prepared.Path));
                _ = PersistReplenishmentAsync(session.Token, replenishment);

                try
                {
                    icyOutput?.SetTrack(prepared.Track);
                    await using (var cached = _cache.OpenRead(prepared.Path))
                        await cached.CopyToAsync(streamOutput, cancellationToken);
                    await streamOutput.FlushAsync(cancellationToken);
                    failures = 0;
                    var song = await _resolver.ResolveAsync(prepared.Track.Artist,
                        prepared.Track.Title, prepared.Track.Duration, session.Authentication,
                        cancellationToken);
                    if (song is not null)
                        await RecordCompletionAsync(session, prepared.Track, song, cancellationToken);

                    var replacement = await replenishment.WaitAsync(cancellationToken);
                    if (replacement is not null
                        && queue.All(item => item.CacheKey != replacement.CacheKey))
                    {
                        queue.Enqueue(replacement);
                        nextIndex = replacement.Index + 1;
                    }

                    var current = Resolve(session);
                    var upcomingTracks = current?.Tracks
                        .Where(track => !string.IsNullOrWhiteSpace(track.ResolvedId)).ToList() ?? [];
                    var upcoming = Enumerable.Range(0, Math.Min(8, upcomingTracks.Count))
                        .Select(offset => upcomingTracks[(nextIndex + offset) % upcomingTracks.Count]
                            .ResolvedId!)
                        .ToList();
                    _ = _metadata.PrewarmYouTubeIdsForSongIdsAsync(upcoming, topN: 8);
                }
                catch (OperationCanceledException) when (cancellationToken.IsCancellationRequested)
                {
                    break;
                }
                catch (Exception ex)
                {
                    failures++;
                    RejectAndRefill(session, prepared.Track);
                    _logger.LogWarning(ex, "Skipping unavailable continuous Radio track {Artist} - {Title}",
                        prepared.Track.Artist, prepared.Track.Title);
                    if (failures >= tracks.Count)
                        throw new InvalidOperationException(
                            "No tracks in this Radio snapshot have a playable source", ex);
                }
            }
        }
        finally { ConcurrentStreams.Release(); }
    }

    /// <summary>Returns up to three current-snapshot tracks that already satisfy the
    /// existing radio-cache retention and size policy. This method never performs I/O
    /// beyond checking the cache, so station listings remain responsive. The scan
    /// starts where the tune-in selector says, so two listens open on different
    /// cached tracks once more than three are cached; playback continues in snapshot
    /// order from wherever the pool ends.</summary>
    public IReadOnlyList<PreparedRadioTrack> GetReadyPool(LastFmRadioStreamSession session)
    {
        // Rotate among the tracks that are actually cached, not among the whole
        // snapshot: with three cached out of twenty, a start drawn over twenty lands on
        // the first cached track seventeen times in twenty.
        var cached = new List<PreparedRadioTrack>();
        foreach (var candidate in Candidates(session))
        {
            var path = _cache.GetReadyPath(candidate.Key);
            if (path is not null) cached.Add(candidate.Prepared(path));
        }
        if (cached.Count == 0) return cached;
        var start = _tuneIn.Start(cached.Count) % cached.Count;
        var ready = new List<PreparedRadioTrack>(ReadyPoolSize);
        for (var offset = 0; offset < cached.Count && ready.Count < ReadyPoolSize; offset++)
            ready.Add(cached[(start + offset) % cached.Count]);
        return ready;
    }

    /// <summary>Waits for the minimum publication guarantee: one complete MP3.
    /// The remaining runway is filled in the background after the station URL is
    /// returned, so clients that fetch their Radio list only once still see it.</summary>
    public async Task<IReadOnlyList<PreparedRadioTrack>> PrepareForPublicationAsync(
        LastFmRadioStreamSession session, CancellationToken cancellationToken)
    {
        var ready = GetReadyPool(session);
        return ready.Count > 0
            ? ready
            : await PrepareReadyPoolAsync(session, 1, cancellationToken);
    }

    /// <summary>Warms persisted snapshots without a listener request. Startup has no
    /// user credentials by design, so it uses the registered external preview route;
    /// authenticated playback replenishment remains local-first.</summary>
    public async Task<RadioWarmupResult> WarmStoredStationsAsync(string username,
        CancellationToken cancellationToken)
    {
        var user = _state.GetUser(username);
        var stations = user.Stations.Where(station => station.Tracks.Count > 0
            && (station.Personalized
                ? _settings.CurrentValue.EnablePersonalizedStations
                : _settings.CurrentValue.EnableDiscoveryStations)).ToList();
        var tasks = stations.Select(async station =>
        {
            var session = new LastFmRadioStreamSession(
                $"warm-{Guid.NewGuid():N}", username, station.Id,
                new Dictionary<string, string>(), DateTime.UtcNow.AddMinutes(30));
            var ready = GetReadyPool(session);
            if (ready.Count < ReadyPoolSize)
                ready = await PrepareReadyPoolAsync(session, ReadyPoolSize,
                    cancellationToken, rejectFailures: false, externalOnly: true);
            return ready.Count;
        });
        var counts = await Task.WhenAll(tasks);
        return new RadioWarmupResult(stations.Count,
            counts.Count(count => count > 0), counts.Sum());
    }

    /// <summary>Starts one deduplicated background fill for this station snapshot.
    /// Request cancellation deliberately does not own cache production: a client that
    /// refreshes or navigates away must not discard work needed by its next listing.</summary>
    public void WarmReadyPool(LastFmRadioStreamSession session)
    {
        var station = Resolve(session);
        if (station is null) return;
        var key = string.Join('|', session.Username, station.Id, station.ChangedUtc.Ticks,
            _settings.CurrentValue.EffectiveRadioStreamBitrateKbps);
        _poolWarmers.GetOrAdd(key, ignoredKey => Task.Run(async () =>
        {
            try
            {
                var ready = await PrepareReadyPoolAsync(session, ReadyPoolSize,
                    CancellationToken.None);
                _logger.LogInformation("Radio ready pool {Ready}/{Target} for {Station}",
                    ready.Count, ReadyPoolSize, station.Name);
            }
            catch (Exception ex)
            {
                _logger.LogWarning(ex, "Could not warm Radio ready pool for {Station}",
                    station.Name);
            }
            finally { _poolWarmers.TryRemove(key, out _); }
        }));
    }

    private async Task<IReadOnlyList<PreparedRadioTrack>> PrepareReadyPoolAsync(
        LastFmRadioStreamSession session, int target, CancellationToken cancellationToken,
        bool rejectFailures = true, bool externalOnly = false)
    {
        var prepared = new List<PreparedRadioTrack>();
        var rejectedAny = false;
        foreach (var candidate in Candidates(session))
        {
            try
            {
                prepared.Add(await PrepareCandidateAsync(session, candidate, cancellationToken,
                    externalOnly));
                if (prepared.Count == target) break;
            }
            catch (OperationCanceledException) when (cancellationToken.IsCancellationRequested)
            {
                throw;
            }
            catch (Exception ex)
            {
                if (rejectFailures)
                {
                    rejectedAny |= _state.RejectTrack(session.Username, candidate.Track) > 0;
                    _logger.LogWarning(ex,
                        "Could not prepare Radio pool track {Artist} - {Title}; trying the next track",
                        candidate.Track.Artist, candidate.Track.Title);
                }
                else
                    _logger.LogDebug(ex,
                        "Startup Radio warm could not prepare {Artist} - {Title}",
                        candidate.Track.Artist, candidate.Track.Title);
            }
        }
        if (rejectFailures && rejectedAny) _refreshQueue.Enqueue(session.Username);
        return prepared;
    }

    private async Task<PreparedRadioTrack?> PrepareNextAsync(LastFmRadioStreamSession session,
        int startIndex, IReadOnlySet<string> reserved, CancellationToken cancellationToken,
        RadioAudioProfile? current = null)
    {
        var candidates = Candidates(session);
        if (candidates.Count == 0) return null;

        // Flow: look at the next few unreserved tracks and, where their profiles are
        // known, lead with the one that follows the current track most smoothly. The
        // rest of the window is not skipped, only deferred: the scan below starts at
        // the chosen one and wraps, so an unchosen track is still next in line.
        var window = new List<RadioCandidate>();
        for (var offset = 0; offset < candidates.Count && window.Count < FlowWindow; offset++)
        {
            var candidate = candidates[(startIndex + offset) % candidates.Count];
            if (!reserved.Contains(candidate.Key)) window.Add(candidate);
        }
        var lead = ChooseByFlow(current, window.Select(candidate =>
        {
            var path = _cache.GetReadyPath(candidate.Key);
            return path is null ? null : _cache.GetProfile(path);
        }).ToList());
        if (lead > 0)
        {
            _logger.LogDebug("Radio flow chose {Artist} - {Title} over the next in snapshot order",
                window[lead].Track.Artist, window[lead].Track.Title);
            startIndex = window[lead].Index;
        }

        for (var offset = 0; offset < candidates.Count; offset++)
        {
            var candidate = candidates[(startIndex + offset) % candidates.Count];
            if (reserved.Contains(candidate.Key)) continue;
            try { return await PrepareCandidateAsync(session, candidate, cancellationToken); }
            catch (OperationCanceledException) when (cancellationToken.IsCancellationRequested)
            { throw; }
            catch (Exception ex)
            {
                RejectAndRefill(session, candidate.Track);
                _logger.LogWarning(ex,
                    "Could not replenish Radio pool with {Artist} - {Title}",
                    candidate.Track.Artist, candidate.Track.Title);
            }
        }
        return null;
    }

    private async Task<PreparedRadioTrack> PrepareCandidateAsync(
        LastFmRadioStreamSession session, RadioCandidate candidate,
        CancellationToken cancellationToken, bool externalOnly = false)
    {
        var settings = _settings.CurrentValue;
        var bitrateKbps = settings.EffectiveRadioStreamBitrateKbps;
        RadioAudioProfile? profile = null;
        var path = await _cache.GetOrCreateAsync(candidate.Key, async (output, token) =>
        {
            var opened = externalOnly
                ? await OpenExternalTrackAsync(candidate.Track, token)
                : await OpenTrackAsync(candidate.Track, session.Authentication, token);
            if (opened is null) throw new InvalidOperationException("No playable source");
            await using (opened.Source.AudioStream)
                profile = await _transcoder.TranscodeToMp3Async(opened.Source.AudioStream, output,
                    bitrateKbps, settings.EffectiveRadioLoudnessTarget, token);
        }, cancellationToken);
        // Only the producer holds a profile; joiners of the same single-flight get the
        // path and read the sidecar the producer writes here. The tags ride along so
        // the flow picker can weigh what a track is next to how it sounds.
        if (profile is not null)
            _cache.SaveProfile(path, profile with
            {
                Genre = candidate.Track.Genre,
                Tags = await TagsForAsync(candidate.Track, cancellationToken),
            });
        return candidate.Prepared(path);
    }

    /// <summary>Last.fm's top tags for the track, the artist's when the track has none.
    /// Both are cached by <see cref="LastFmService"/>; a miss is an empty list, never a failure.</summary>
    private async Task<IReadOnlyList<string>> TagsForAsync(LastFmRadioTrack track,
        CancellationToken cancellationToken)
    {
        if (_lastFm is null || !_lastFm.HasApiKey) return [];
        try
        {
            var tags = await _lastFm.GetTrackTopTagsAsync(track.Artist, track.Title, 10, cancellationToken);
            if (tags.Count == 0) tags = await _lastFm.GetArtistTopTagsAsync(track.Artist, 10, cancellationToken);
            return KinshipTags(tags, track.Artist);
        }
        catch (Exception ex) when (ex is not OperationCanceledException)
        {
            _logger.LogDebug(ex, "No Last.fm tags for {Artist} - {Title}", track.Artist, track.Title);
            return [];
        }
    }

    /// <summary>
    /// Last.fm tags with the ones that say nothing about kinship removed: years, the
    /// artist's own name, and the listener-bookkeeping tags the recommender also
    /// ignores. Two tracks by the same artist already share their sound; a shared
    /// "2018" would only inflate the overlap.
    /// </summary>
    internal static IReadOnlyList<string> KinshipTags(IEnumerable<string> tags, string artist)
    {
        var artistTag = DiscoveryStationSettings.NormalizeTag(artist);
        return tags.Select(LastFmRadioRecommendationService.CanonicalTag)
            .Where(tag => tag.Length > 0
                && !tag.Equals(artistTag, StringComparison.OrdinalIgnoreCase)
                // One year rule, shared with the genre normaliser. Two hand-rolled copies is
                // how one of them quietly stops dropping "2020s" when someone fixes the other.
                && !Octo.Services.Metadata.GenreNormalizer.IsYearLike(tag))
            .Distinct(StringComparer.OrdinalIgnoreCase)
            .Take(8)
            .ToList();
    }

    /// <summary>How many upcoming snapshot tracks the flow picker may choose between.</summary>
    internal const int FlowWindow = 4;

    /// <summary>Weight of kinship (tags, genre) against sound in the flow score. At 1.5 a
    /// track from another genre costs more than an octave of brightness.</summary>
    internal const double KinshipWeight = 1.5;

    /// <summary>
    /// How far apart two tracks are as neighbours in a stream, as one score with two
    /// halves. Sound: brightness as octaves between spectral centroids, dynamics as
    /// loudness range, texture as spectral flatness (loudness itself is not a term
    /// because every track has been brought to the same level). Kinship: one minus the
    /// overlap of their Last.fm tags, the catalogue genre when tags are missing, and a
    /// neutral middle when nothing is known so an unmeasured track is neither favoured
    /// nor punished.
    /// </summary>
    internal static double FlowDistance(RadioAudioProfile current, RadioAudioProfile next)
    {
        var brightness = Math.Abs(Math.Log2(Math.Max(20, next.SpectralCentroidHz)
            / Math.Max(20, current.SpectralCentroidHz)));
        var dynamics = Math.Abs(next.LoudnessRangeLu - current.LoudnessRangeLu) / 5d;
        var texture = Math.Abs(next.SpectralFlatness - current.SpectralFlatness) * 5d;
        return brightness + dynamics + texture + KinshipWeight * Estrangement(current, next);
    }

    /// <summary>0 for the same tags, 1 for none in common, by genre when tags are missing.</summary>
    internal static double Estrangement(RadioAudioProfile current, RadioAudioProfile next)
    {
        if (current.Tags is { Count: > 0 } mine && next.Tags is { Count: > 0 } theirs)
        {
            var shared = mine.Intersect(theirs, StringComparer.OrdinalIgnoreCase).Count();
            var union = mine.Union(theirs, StringComparer.OrdinalIgnoreCase).Count();
            return union == 0 ? 0.35 : 1d - (double)shared / union;
        }
        if (!string.IsNullOrWhiteSpace(current.Genre) && !string.IsNullOrWhiteSpace(next.Genre))
            return string.Equals(current.Genre, next.Genre, StringComparison.OrdinalIgnoreCase) ? 0 : 0.7;
        return 0.35;
    }

    /// <summary>
    /// Among the next few snapshot tracks, the one that follows the current track most
    /// smoothly. Only tracks that are already cached and measured can be compared; when
    /// none is, or there is nothing to compare against, the answer is snapshot order,
    /// which is what the stream did before profiles existed.
    /// </summary>
    internal static int ChooseByFlow(RadioAudioProfile? current, IReadOnlyList<RadioAudioProfile?> window)
    {
        if (current is null || window.Count == 0) return 0;
        var best = 0;
        var bestDistance = double.PositiveInfinity;
        for (var offset = 0; offset < window.Count; offset++)
        {
            if (window[offset] is not { } profile) continue;
            var distance = FlowDistance(current, profile);
            if (distance < bestDistance) { bestDistance = distance; best = offset; }
        }
        return double.IsPositiveInfinity(bestDistance) ? 0 : best;
    }

    private List<RadioCandidate> Candidates(LastFmRadioStreamSession session)
    {
        var station = Resolve(session);
        if (station is null) return [];
        var bitrateKbps = _settings.CurrentValue.EffectiveRadioStreamBitrateKbps;
        // The cache key is the resolved source, not the station: a track that survives
        // a refresh, or sits in two of a listener's stations, is transcoded once.
        return station.Tracks
            .Where(track => !string.IsNullOrWhiteSpace(track.ResolvedId))
            .Select((track, index) => new RadioCandidate(track, index,
                _cache.Key(session.Username, string.Empty, track.ResolvedId!, bitrateKbps)))
            .ToList();
    }

    private async Task PersistReplenishmentAsync(string token,
        Task<PreparedRadioTrack?> replenishment)
    {
        try
        {
            var track = await replenishment;
            if (track is not null) _sessions.AppendReadyTrack(token, track, ReadyPoolSize);
        }
        catch (Exception ex)
        {
            _logger.LogDebug(ex, "Radio pool replenishment ended without a ready track");
        }
    }

    private void RejectAndRefill(LastFmRadioStreamSession session, LastFmRadioTrack track)
    {
        if (_state.RejectTrack(session.Username, track) <= 0) return;
        _refreshQueue.Enqueue(session.Username);
    }

    private async Task<OpenedRadioTrack?> OpenTrackAsync(LastFmRadioTrack track,
        IReadOnlyDictionary<string, string> authentication, CancellationToken cancellationToken)
    {
        var song = await _resolver.ResolveAsync(track.Artist, track.Title, track.Duration,
            authentication, cancellationToken);
        if (song is null) return null;
        var id = song.Id;
        var (external, provider, externalId) = _library.ParseSongId(id);
        if (external)
        {
            var source = await _downloads.GetDirectStreamAsync(
                provider ?? song.ExternalProvider ?? track.ExternalProvider ?? "lastfm",
                externalId ?? song.ExternalId ?? id, null, cancellationToken);
            return source is null ? null : new OpenedRadioTrack(source, song);
        }

        var parameters = authentication.ToDictionary(pair => pair.Key, pair => pair.Value,
            StringComparer.OrdinalIgnoreCase);
        parameters["id"] = id;
        parameters["format"] = "raw";
        var localSource = await _proxy.OpenAudioStreamAsync(parameters, cancellationToken);
        return localSource is null ? null : new OpenedRadioTrack(localSource, song);
    }

    private async Task<OpenedRadioTrack?> OpenExternalTrackAsync(LastFmRadioTrack track,
        CancellationToken cancellationToken)
    {
        if (string.IsNullOrWhiteSpace(track.ResolvedId)) return null;
        var provider = track.ExternalProvider ?? SoulseekMetadataService.ProviderName;
        var source = await _downloads.GetDirectStreamAsync(provider, track.ResolvedId,
            null, cancellationToken);
        if (source is null) return null;
        return new OpenedRadioTrack(source, new Song
        {
            Id = track.ResolvedId, Artist = track.Artist, Title = track.Title,
            Album = track.Album ?? string.Empty, Genre = track.Genre,
            Duration = track.Duration, IsLocal = false,
            ExternalProvider = provider, ExternalId = track.ResolvedId,
        });
    }

    private async Task RecordCompletionAsync(LastFmRadioStreamSession session,
        LastFmRadioTrack track, Song song, CancellationToken cancellationToken)
    {
        var id = song.Id;
        _state.RecordPlay(session.Username, new LastFmRadioPlay
        {
            SongId = id, Artist = track.Artist, Title = track.Title, Album = track.Album,
            Genre = track.Genre ?? song.Genre, Duration = track.Duration ?? song.Duration,
            IsLocal = song.IsLocal,
            Source = "internet-radio", PlayedAtUtc = DateTime.UtcNow,
        });
        if (!song.IsLocal)
        {
            // A local track's play is scrobbled by Navidrome below; an external one
            // would otherwise vanish from the listener's history. Not awaited: the
            // stream is mid-song and a listen is a record, not a step.
            if (_listenBrainz is not null)
                _ = _listenBrainz.SubmitListenAsync(session.Username, track.Artist, track.Title,
                    track.Album, track.Duration ?? song.Duration, DateTime.UtcNow);
            return;
        }
        var parameters = session.Authentication.ToDictionary(pair => pair.Key, pair => pair.Value,
            StringComparer.OrdinalIgnoreCase);
        parameters["id"] = id;
        parameters["submission"] = "true";
        parameters["time"] = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds().ToString();
        await _proxy.RelaySafeAsync("rest/scrobble", parameters);
    }

    private sealed record RadioCandidate(LastFmRadioTrack Track, int Index, string Key)
    {
        public PreparedRadioTrack Prepared(string path) => new(path, Track, Index, Key);
    }

    private sealed record OpenedRadioTrack(DirectStreamInfo Source, Song Song);
}

public sealed record PreparedRadioTrack(
    string Path, LastFmRadioTrack Track, int Index, string CacheKey = "");

public sealed record RadioWarmupResult(int StationCount, int ReadyStationCount,
    int ReadyTrackCount);
