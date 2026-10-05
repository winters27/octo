using System.Security.Cryptography;
using System.Text;
using Microsoft.Extensions.Options;
using Octo.Models.Radio;
using Octo.Models.Settings;
using Octo.Models.Domain;
using Octo.Services.Radio;

namespace Octo.Services.LastFm;

/// <summary>
/// Builds canonical station snapshots from bounded listening signals. Which candidates
/// make the cut is a weighted draw rather than a fixed top-N, so two refreshes of the
/// same profile give two different stations; the previous snapshot is demoted so a
/// refresh rotates the station instead of restating it.
/// </summary>
public sealed class LastFmRadioRecommendationService
{
    /// <summary>
    /// Share of its weight a track keeps when it was already in this station's
    /// previous snapshot. Low enough that a refresh is mostly new, high enough that a
    /// strong match can still come back.
    /// </summary>
    internal const double PreviousSnapshotWeight = 0.35;

    /// <summary>Source of the draw. Tests replace it with a seeded one so a build repeats exactly.</summary>
    internal Func<Random> Randomizer { get; set; } = () => new Random();

    /// <summary>
    /// How fast a provider's ranking fades. Rank r keeps 1 / (1 + r / depth) of its
    /// weight, so the top of a tag's or an artist's list still leads every draw and the
    /// tail is where refreshes differ. 20 is the middle of ListenBrainz's easy/medium/
    /// hard popularity windows: reachable, not bottom of the barrel.
    /// </summary>
    internal const double RankHalfDepth = 20;

    /// <summary>Top tracks of an artist similar to the seed, relative to the seed's own.</summary>
    internal const double NeighbourArtistAffinity = 0.6;

    /// <summary>Each older seed contributes this much of the previous one; hearts count half again.</summary>
    internal const double SeedRecencyDecay = 0.85;
    internal const double HeartedSeedAffinity = 1.5;

    /// <summary>A candidate with its final draw weight (match x rank decay x seed affinity)
    /// and the list it came from: a seed track, a tag, an artist, or the listener's history.</summary>
    internal sealed record Candidate(LastFmService.SimilarTrack Track, double Weight, string Source);

    internal const string FamiliarSource = "history";

    /// <summary>
    /// How much a play counts as a listening signal, by where it came from. A track the
    /// listener chose and scrobbled is the real signal. A track the radio played to the
    /// end says less: they did not pick it, they only did not switch it off. Left at full
    /// weight, a station slowly trains itself on its own output. A play recorded at
    /// bootstrap from a random library track says less still.
    /// </summary>
    internal const double RadioPlayWeight = 0.4;
    internal const double RandomBootstrapWeight = 0.5;

    /// <summary>A song heard this recently waits until nothing else fits.</summary>
    internal static readonly TimeSpan HeardLately = TimeSpan.FromDays(8);

    /// <summary>How many songs apart one artist must be, and one title (a cover counts).
    /// Never more than the station's candidates can give. The artist gap must stay at least
    /// <see cref="LastFmRadioStreamService.FlowWindow"/>: the stream may reorder within that
    /// window, and the gap is what keeps an artist from landing back to back after it.</summary>
    internal const int ArtistGap = 4;
    internal const int TitleGap = 8;

    /// <summary>Weight kept by an artist with a song the listener rated one star.</summary>
    internal const double DislikedArtistWeight = 0.5;

    internal static double SourceWeight(LastFmRadioPlay play) => play.Source switch
    {
        "internet-radio" => RadioPlayWeight,
        "bootstrap-random" => RandomBootstrapWeight,
        _ => 1d,
    };

    private static readonly HashSet<string> DeniedTags = new(StringComparer.OrdinalIgnoreCase)
    {
        // Last.fm's tags as listeners write them, in both spellings: these are data, not Octo's words.
        "seen live", "favorites", "favourites", "owned", "spotify", "albums i own",
        "under 2000 listeners", "awesome", "love", "best",
        // Sentiment and superlatives say how a listener felt, not what the music is.
        "favorite song", "favourite song", "favorite songs", "favourite songs", "my love",
        "love at first listen", "beautiful", "epic", "legendary", "classic", "amazing",
        "perfect", "masterpiece", "good", "great", "catchy", "fun", "chill", "cool"
    };
    private static readonly Dictionary<string, string> TagAliases = new(StringComparer.OrdinalIgnoreCase)
    {
        ["electronica"] = "electronic", ["hip hop"] = "hip-hop", ["hiphop"] = "hip-hop",
        ["rnb"] = "r&b", ["rhythm and blues"] = "r&b", ["alt rock"] = "alternative rock"
    };

    private readonly LastFmService _lastFm;
    private readonly LastFmRadioStateStore _state;
    private readonly IOptionsMonitor<LastFmSettings> _settings;
    private readonly ILogger<LastFmRadioRecommendationService> _logger;

    /// <param name="sources">Every radio source; null keeps stations on Last.fm alone.</param>
    public LastFmRadioRecommendationService(LastFmService lastFm, LastFmRadioStateStore state,
        IOptionsMonitor<LastFmSettings> settings,
        ILogger<LastFmRadioRecommendationService> logger, RadioSourceSet? sources = null)
    {
        _lastFm = lastFm;
        _state = state;
        _settings = settings;
        _logger = logger;
        _sources = sources;
    }

    private readonly RadioSourceSet? _sources;
    private static readonly IReadOnlyDictionary<string, string> EmptyAuth = new Dictionary<string, string>();
    /// <summary>Seeds asked at once when several sources answer: the sources' own gates still pace them.</summary>
    internal const int SeedsAtOnce = 3;

    public async Task<IReadOnlyList<LastFmRadioStation>> BuildAsync(string username,
        CancellationToken cancellationToken = default)
    {
        var settings = _settings.CurrentValue;
        if (!settings.EnableRadio) return [];
        var user = _state.GetUser(username);
        var random = Randomizer();
        var previousSnapshots = user.Stations.GroupBy(station => station.Key, StringComparer.OrdinalIgnoreCase)
            .ToDictionary(group => group.Key, group => (IReadOnlySet<string>)group.First().Tracks
                .Select(track => LastFmRadioSeedNormalizer.TrackKey(track.Artist, track.Title))
                .ToHashSet(StringComparer.OrdinalIgnoreCase), StringComparer.OrdinalIgnoreCase);
        IReadOnlySet<string> Previous(string stationKey) =>
            previousSnapshots.GetValueOrDefault(stationKey) ?? new HashSet<string>(StringComparer.OrdinalIgnoreCase);
        var plays = user.Plays.OrderByDescending(play => play.PlayedAtUtc).ToList();
        var unavailable = user.UnavailableTracks
            .Where(track => track.RetryAfterUtc > DateTime.UtcNow)
            .Select(track => track.Key).ToHashSet(StringComparer.OrdinalIgnoreCase);
        var banned = user.RadioBans.Select(ban => ban.Key).ToHashSet(StringComparer.OrdinalIgnoreCase);
        var disliked = user.RadioBans.Select(ban => ArtistKey(ban.Artist)).ToHashSet(StringComparer.OrdinalIgnoreCase);
        var refillHeadroom = Math.Min(unavailable.Count, settings.EffectiveRadioTrackCount);
        var candidateTarget = Math.Min(100,
            settings.EffectiveRadioTrackCount + refillHeadroom + 10);
        var artistScores = ScoreArtists(plays);
        // Seeds are the strongest recent signals, not simply the newest plays: a heart
        // outranks a play, a chosen play outranks one the radio served, and age decays.
        var trackSeeds = plays.OrderByDescending(SeedScore)
            .GroupBy(play => LastFmRadioSeedNormalizer.TrackKey(play.Artist, play.Title))
            .Select(group => group.First()).Take(8).ToList();
        var tags = ScoreLocalTags(plays);

        // Provider expansion has a hard fan-out and deadline. Partial results are useful.
        using var budget = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken);
        budget.CancelAfter(TimeSpan.FromSeconds(25));
        var ct = budget.Token;
        foreach (var artist in artistScores.Take(5).Select(pair => pair.Key))
        {
            try
            {
                foreach (var tag in await _lastFm.GetArtistTopTagsAsync(artist, 6, ct)) AddTag(tags, tag, 1);
            }
            catch (OperationCanceledException) when (!cancellationToken.IsCancellationRequested) { break; }
        }

        var stations = new List<LastFmRadioStation>();
        if (settings.EnablePersonalizedStations)
        {
            var learned = plays.Where(play => play.LearnedSignal).Sum(SourceWeight)
                >= settings.EffectiveMinimumPlays;
            var mixKey = learned ? "your-mix" : "starter";
            if (settings.EnableYourMix)
            {
                var mixCandidates = await TracksFromSeeds(username, trackSeeds.Take(6), 12, ct);
                var familiar = plays.Select(ToCandidate).ToList();
                // The familiar share of the mix is a quota the walk enforces, so both halves
                // are drawn over their whole pools rather than the top of each list.
                int? familiarQuota = mixCandidates.Count == 0
                    ? null
                    : settings.EffectiveRadioTrackCount
                        - (int)Math.Round(settings.EffectiveRadioTrackCount * settings.EffectiveDiscoveryPercent / 100d);
                if (mixCandidates.Count == 0) mixCandidates.AddRange(familiar);
                stations.Add(Create(username, mixKey,
                    learned ? "Your Mix" : "Starter Radio",
                    learned ? LastFmRadioStationKind.YourMix : LastFmRadioStationKind.Starter,
                    true, trackSeeds.Select(seed => seed.Artist),
                    Shape(familiar.Concat(mixCandidates), plays, settings, unavailable, random, Previous(mixKey),
                        ArtistCap(settings, LastFmRadioStationKind.YourMix), familiarQuota: familiarQuota,
                        banned: banned, disliked: disliked)));
            }

            if (learned)
            {
                var topTags = tags.OrderByDescending(pair => pair.Value).ThenBy(pair => pair.Key)
                    .Select(pair => pair.Key).Take(3).ToList();
                if (settings.EnableDiscoveryMix && topTags.Count > 0)
                {
                    var discovery = await TracksFromTags(username, topTags, candidateTarget, ct);
                    if (discovery.Count >= 5)
                        stations.Add(Create(username, "discovery", "Discovery Mix",
                            LastFmRadioStationKind.Discovery, true, topTags,
                            Shape(discovery, plays, settings, unavailable, random, Previous("discovery"),
                                ArtistCap(settings, LastFmRadioStationKind.Discovery), excludeRecent: true,
                                banned: banned, disliked: disliked)));
                }

                foreach (var artist in artistScores.Take(settings.EffectiveArtistStationCount).Select(pair => pair.Key))
                {
                    var candidates = await TracksFromArtist(username, artist, candidateTarget, ct);
                    var stationKey = "artist-" + Key(artist);
                    if (candidates.Count >= 5)
                        stations.Add(Create(username, stationKey, $"{artist} Radio",
                            LastFmRadioStationKind.Artist, true, [artist],
                            Shape(candidates, plays, settings, unavailable, random, Previous(stationKey),
                                ArtistCap(settings, LastFmRadioStationKind.Artist),
                                banned: banned, disliked: disliked)));
                }

                foreach (var tag in tags.OrderByDescending(pair => pair.Value).Select(pair => pair.Key)
                             .Take(settings.EffectiveGenreStationCount))
                {
                    var candidates = await TracksFromTags(username, [tag], candidateTarget, ct);
                    var stationKey = "genre-" + Key(tag);
                    if (candidates.Select(item => item.Track.Artist).Distinct(StringComparer.OrdinalIgnoreCase).Count() >= 4)
                        stations.Add(Create(username, stationKey, Title(tag) + " Radio",
                            LastFmRadioStationKind.Genre, true, [tag],
                            Shape(candidates, plays, settings, unavailable, random, Previous(stationKey),
                                ArtistCap(settings, LastFmRadioStationKind.Genre),
                                banned: banned, disliked: disliked)));
                }
            }
        }

        if (settings.EnableDiscoveryStations)
        {
            foreach (var definition in settings.EffectiveDiscoveryStations().Where(item => item.Enabled))
            {
                var candidates = await TracksFromTags(username, definition.Tags, candidateTarget, ct);
                if (candidates.Count == 0)
                    candidates.AddRange(plays.Where(play => definition.Tags.Any(tag =>
                            (play.Genre ?? "").Contains(tag, StringComparison.OrdinalIgnoreCase)))
                        .Select(ToCandidate));
                var stationKey = "pinned-" + definition.Id;
                if (candidates.Count > 0)
                    stations.Add(Create(username, stationKey, definition.Name,
                        LastFmRadioStationKind.Pinned, false, definition.Tags,
                        Shape(candidates, plays, settings, unavailable, random, Previous(stationKey),
                            ArtistCap(settings, LastFmRadioStationKind.Pinned),
                            banned: banned, disliked: disliked),
                        DefinitionVersion(definition)));
            }
        }

        SuppressStationOverlap(stations);
        foreach (var station in stations)
            station.ValidUntilUtc = station.ChangedUtc.AddHours(settings.EffectiveRefreshIntervalHours);
        _logger.LogInformation("Built {Count} Last.fm radio stations for {User}", stations.Count, username);
        return stations.Where(station => station.Tracks.Count > 0).ToList();
    }

    /// <summary>
    /// Tracks similar to each seed, the seed's own weight riding along: the newest seed
    /// leads, each older one contributes <see cref="SeedRecencyDecay"/> of the previous,
    /// and a hearted seed counts half again. With Last.fm alone, its match score stays the
    /// per-track signal; with several sources, each source's list is ranked by position and
    /// weighted by its provider (see <see cref="RadioSourceSet.Weights"/>), and seeds are asked
    /// <see cref="SeedsAtOnce"/> at a time so a slow source costs one timeout per three seeds.
    /// </summary>
    private async Task<List<Candidate>> TracksFromSeeds(string? username,
        IEnumerable<LastFmRadioPlay> seeds, int each, CancellationToken ct)
    {
        var ordered = seeds.Select((seed, position) => (Seed: seed,
            Affinity: Math.Pow(SeedRecencyDecay, position) * (seed.Hearted ? HeartedSeedAffinity : 1d))).ToList();
        if (_sources is null)
        {
            var result = new List<Candidate>();
            foreach (var (seed, affinity) in ordered)
            {
                var source = LastFmRadioSeedNormalizer.TrackKey(seed.Artist, seed.Title);
                try { result.AddRange(Ranked(await _lastFm.GetSimilarTracksAsync(seed.Artist, seed.Title, each, ct), source, affinity)); }
                catch (OperationCanceledException) { break; }
            }
            return result;
        }

        var weights = _sources.Weights(username);
        using var gate = new SemaphoreSlim(SeedsAtOnce);
        var lists = await Task.WhenAll(ordered.Select(async item =>
        {
            try { await gate.WaitAsync(ct); }
            catch (OperationCanceledException) { return new List<Candidate>(); }
            try
            {
                var (seed, affinity) = item;
                var source = LastFmRadioSeedNormalizer.TrackKey(seed.Artist, seed.Title);
                var radioSeed = new RadioSeed(seed.Artist, seed.Title, seed.Duration,
                    seed.IsLocal && seed.SongId.Length > 0
                        ? new Song { Id = seed.SongId, Title = seed.Title, Artist = seed.Artist, IsLocal = true }
                        : null);
                var found = new List<Candidate>();
                foreach (var answer in await _sources.AskAllAsync(radioSeed, each, EmptyAuth, ct))
                {
                    var factor = RadioBlend.StationFactor(answer.Match) * RadioBlend.Thin(answer)
                        * weights.GetValueOrDefault(answer.Provider, 1.0);
                    if (factor <= 0) continue;
                    // Sources score on different scales (Last.fm's match, a flat 1 from YouTube Music),
                    // so with several sources each list is ranked by position alone, as song radio
                    // does, and the provider weights say how much each list counts.
                    var tracks = answer.LibrarySongs.Select(song => new LastFmService.SimilarTrack(song.Artist,
                            song.Title, 1.0, song.Duration, Provider: answer.Provider))
                        .Concat(answer.Tracks.Select(track => track with { Match = 1.0 }));
                    found.AddRange(Ranked(tracks, answer.Provider + ":" + source, affinity * factor));
                }
                return found;
            }
            catch (OperationCanceledException) { return new List<Candidate>(); }
            finally { gate.Release(); }
        }));
        return lists.SelectMany(list => list).ToList();
    }

    private async Task<List<Candidate>> TracksFromTags(string? username, IEnumerable<string> tags,
        int each, CancellationToken ct)
    {
        var result = new List<Candidate>();
        foreach (var tag in tags.Take(5))
        {
            try
            {
                result.AddRange(Ranked(await _lastFm.GetTagTopTracksAsync(tag, each, ct), "tag:" + tag));
                // Last.fm's tag chart is the line above; the other sources' answers for the tag
                // (LB Radio, with a ListenBrainz token) join it, each list weighted by its provider.
                if (_sources is not null)
                {
                    var weights = _sources.Weights(username);
                    foreach (var answer in (await _sources.TagAllAsync(tag, each, ct)).Where(a => a.Provider != RadioProvider.LastFm))
                        result.AddRange(Ranked(answer.Tracks, answer.Provider + ":tag:" + tag,
                            weights.GetValueOrDefault(answer.Provider, 1.0)));
                }
            }
            catch (OperationCanceledException) { break; }
        }
        return result;
    }

    /// <summary>The seed artist's own top tracks lead; similar artists' top tracks ride at
    /// <see cref="NeighbourArtistAffinity"/> so the station stays about who it is named for.</summary>
    private async Task<List<Candidate>> TracksFromArtist(string? username, string artist,
        int candidateTarget, CancellationToken ct)
    {
        var result = Ranked(await _lastFm.GetArtistTopTracksAsync(artist,
            Math.Min(50, candidateTarget), ct), "artist:" + artist);
        foreach (var similar in (await _lastFm.GetSimilarArtistsAsync(artist, 6, ct)).Take(5))
            result.AddRange(Ranked(await _lastFm.GetArtistTopTracksAsync(similar.Name,
                Math.Min(20, Math.Max(6, candidateTarget / 5)), ct), "artist:" + similar.Name,
                NeighbourArtistAffinity));
        // The other sources' radio for the artist (YouTube Music's, LB Radio's), beside the
        // similar artists' songs.
        if (_sources is not null)
        {
            var weights = _sources.Weights(username);
            foreach (var answer in await _sources.AskAllAsync(new RadioSeed(artist, "", null, null), candidateTarget, EmptyAuth, ct))
                if (answer.Provider != RadioProvider.LastFm)
                    result.AddRange(Ranked(answer.Tracks, answer.Provider + ":artist:" + artist,
                        NeighbourArtistAffinity * weights.GetValueOrDefault(answer.Provider, 1.0)));
        }
        return result;
    }

    /// <summary>
    /// Selects the station's tracks from its weighted candidates. Filler and songs rated one
    /// star never make it, and an artist with a one-star song keeps half its weight. One
    /// weighted draw orders the pool (see <see cref="WeightedOrder"/>), then the walk applies
    /// the rules: nothing unavailable, nothing just played when asked, no artist past its cap
    /// for this station kind, no source list past its share, the familiar quota when set, an
    /// artist not within <see cref="ArtistGap"/> songs of itself and a title not within
    /// <see cref="TitleGap"/>. A new song heard in the last <see cref="HeardLately"/>, or one
    /// the gaps refused, waits; if the walk falls short they fill the rest, never the same
    /// artist twice in a row. The caps are the marginal-relevance idea applied greedily, with
    /// artist and source repetition as the redundancy.
    /// </summary>
    private static List<LastFmRadioTrack> Shape(IEnumerable<Candidate> candidates,
        IReadOnlyCollection<LastFmRadioPlay> plays, LastFmSettings settings,
        IReadOnlySet<string> unavailable, Random random, IReadOnlySet<string> previous,
        int artistCap, bool excludeRecent = false, int? familiarQuota = null,
        IReadOnlySet<string>? banned = null, IReadOnlySet<string>? disliked = null)
    {
        var target = settings.EffectiveRadioTrackCount;
        var recent = plays.Take(30).Select(play => LastFmRadioSeedNormalizer.TrackKey(play.Artist, play.Title))
            .ToHashSet(StringComparer.OrdinalIgnoreCase);
        var heardAfter = DateTime.UtcNow - HeardLately;
        var heard = plays.Where(play => play.PlayedAtUtc > heardAfter)
            .Select(play => LastFmRadioSeedNormalizer.TrackKey(play.Artist, play.Title))
            .ToHashSet(StringComparer.OrdinalIgnoreCase);
        var perArtist = new Dictionary<string, int>(StringComparer.OrdinalIgnoreCase);
        var perSource = new Dictionary<string, int>(StringComparer.OrdinalIgnoreCase);
        var familiarTaken = 0;
        var output = new List<LastFmRadioTrack>();
        var outputArtists = new List<string>();
        var outputTitles = new List<string>();
        var distinct = candidates
            .Where(item => item.Track.Artist.Length > 0 && item.Track.Title.Length > 0)
            .Where(item => !RadioFiller.IsFiller(item.Track.Title, item.Track.Duration))
            .Where(item => banned is null
                || !banned.Contains(LastFmRadioSeedNormalizer.TrackKey(item.Track.Artist, item.Track.Title)))
            .Select(item => disliked is not null && disliked.Contains(ArtistKey(item.Track.Artist))
                ? item with { Weight = item.Weight * DislikedArtistWeight }
                : item)
            .GroupBy(item => LastFmRadioSeedNormalizer.TrackKey(item.Track.Artist, item.Track.Title))
            .Select(group => group.OrderByDescending(item => item.Weight).First())
            .ToList();
        var sourceCap = SourceCap(target, distinct.Select(item => item.Source)
            .Distinct(StringComparer.OrdinalIgnoreCase).Count());
        var artistGap = Math.Clamp(distinct.Select(item => ArtistKey(item.Track.Artist))
            .Distinct(StringComparer.OrdinalIgnoreCase).Count() - 1, 1, ArtistGap);
        var held = new List<Candidate>();
        foreach (var candidate in WeightedOrder(distinct, random, previous))
        {
            if (output.Count >= target) break;
            if (Admit(candidate, spaced: true) == Verdict.Wait) held.Add(candidate);
        }
        foreach (var candidate in held)
        {
            if (output.Count >= target) break;
            Admit(candidate, spaced: false);
        }
        return output;

        Verdict Admit(Candidate candidate, bool spaced)
        {
            var track = candidate.Track;
            var key = LastFmRadioSeedNormalizer.TrackKey(track.Artist, track.Title);
            var artist = ArtistKey(track.Artist);
            if (unavailable.Contains(key)) return Verdict.Skip;
            if (excludeRecent && recent.Contains(key)) return Verdict.Skip;
            if (perArtist.GetValueOrDefault(artist) >= artistCap) return Verdict.Skip;
            if (perSource.GetValueOrDefault(candidate.Source) >= sourceCap) return Verdict.Skip;
            var isFamiliar = candidate.Source == FamiliarSource;
            if (familiarQuota is { } quota)
            {
                if (isFamiliar && familiarTaken >= quota) return Verdict.Skip;
                if (!isFamiliar && output.Count - familiarTaken >= target - quota) return Verdict.Skip;
            }
            var title = (LastFmRadioSeedNormalizer.Title(track.Title) ?? track.Title).ToLowerInvariant();
            if (spaced)
            {
                // A familiar song is there because it was heard; only new ones wait.
                if (!isFamiliar && heard.Contains(key)) return Verdict.Wait;
                if (outputArtists.TakeLast(artistGap).Contains(artist, StringComparer.OrdinalIgnoreCase)) return Verdict.Wait;
                if (outputTitles.TakeLast(TitleGap).Contains(title)) return Verdict.Wait;
            }
            else if (outputArtists.Count > 0 && string.Equals(outputArtists[^1], artist, StringComparison.OrdinalIgnoreCase))
            {
                return Verdict.Skip;
            }
            output.Add(new LastFmRadioTrack
            {
                Artist = LastFmRadioSeedNormalizer.Artist(track.Artist) ?? track.Artist,
                Title = LastFmRadioSeedNormalizer.Title(track.Title) ?? track.Title,
                Duration = track.Duration, Score = track.Match, YouTubeId = track.YouTubeId,
                // Which source suggested it, shown in the apps; a familiar song was the
                // listener's own pick.
                Source = track.Provider ?? (isFamiliar ? RadioProvider.History : RadioProvider.LastFm)
            });
            outputArtists.Add(artist);
            outputTitles.Add(title);
            perArtist[artist] = perArtist.GetValueOrDefault(artist) + 1;
            perSource[candidate.Source] = perSource.GetValueOrDefault(candidate.Source) + 1;
            if (isFamiliar) familiarTaken++;
            return Verdict.Taken;
        }
    }

    private enum Verdict { Taken, Skip, Wait }

    /// <summary>One spelling per artist for caps and spacing: the credit without its
    /// featured guests, as seeding spells it.</summary>
    private static string ArtistKey(string artist) =>
        (LastFmRadioSeedNormalizer.Artist(artist) ?? artist).Trim().ToLowerInvariant();

    /// <summary>
    /// How many tracks one source list may hold: an even share and a half, never fewer
    /// than three. Six seeds feeding a 50-track mix each get at most 13; a station built
    /// from one tag is unconstrained by it.
    /// </summary>
    internal static int SourceCap(int target, int sources) =>
        Math.Max(3, (int)Math.Ceiling(1.5 * target / Math.Max(1, sources)));

    /// <summary>The seed ranking: provenance, hearts, and a 45-day recency decay.</summary>
    private static double SeedScore(LastFmRadioPlay play) =>
        SourceWeight(play) * (play.Hearted ? 2 : 1)
        * Math.Exp(-(DateTime.UtcNow - play.PlayedAtUtc).TotalDays / 45);

    /// <summary>
    /// How many tracks one artist may hold in a station. An artist station is about its
    /// artist, so a quarter of it may be theirs; everywhere else an artist is a guest.
    /// </summary>
    internal static int ArtistCap(LastFmSettings settings, LastFmRadioStationKind kind) =>
        kind == LastFmRadioStationKind.Artist
            ? Math.Max(3, settings.EffectiveRadioTrackCount / 4)
            : Math.Max(2, settings.EffectiveRadioTrackCount / 10);

    /// <summary>A play as a candidate: hearted counts double, provenance scales it, and its
    /// place in the recency order decays the same way a provider rank does.</summary>
    private static Candidate ToCandidate(LastFmRadioPlay play, int recencyRank) =>
        Weigh(new LastFmService.SimilarTrack(play.Artist, play.Title, play.Hearted ? 2 : 1, play.Duration),
            recencyRank, FamiliarSource, SourceWeight(play));

    private static double RankDecay(int rank) => 1d / (1d + rank / RankHalfDepth);

    private static Candidate Weigh(LastFmService.SimilarTrack track, int rank, string source,
        double affinity = 1d) =>
        new(track, Math.Max(0.05, track.Match) * RankDecay(rank) * affinity, source);

    /// <summary>Weights a provider's list in the order it came, which is the provider's ranking.</summary>
    private static List<Candidate> Ranked(IEnumerable<LastFmService.SimilarTrack> tracks, string source,
        double affinity = 1d) =>
        tracks.Select((track, rank) => Weigh(track, rank, source, affinity)).ToList();

    /// <summary>
    /// One weighted draw over the candidates (Efraimidis-Spirakis: each candidate draws
    /// u^(1/weight) and the pool is sorted by that), so a track's chance of landing near
    /// the front is proportional to its weight and every build draws differently. Tracks
    /// that were in this station's previous snapshot keep
    /// <see cref="PreviousSnapshotWeight"/> of their weight. Ties fall back to the stable
    /// hash so equal draws are not order-of-arrival.
    /// </summary>
    private static IEnumerable<Candidate> WeightedOrder(
        IEnumerable<Candidate> candidates, Random random, IReadOnlySet<string> previous) =>
        candidates
            .Select(item => (Item: item, Draw: Math.Pow(random.NextDouble(), 1d / Weight(item, previous))))
            .OrderByDescending(pair => pair.Draw)
            .ThenBy(pair => StableOrder(pair.Item.Track.Artist, pair.Item.Track.Title))
            .Select(pair => pair.Item);

    private static double Weight(Candidate item, IReadOnlySet<string> previous)
    {
        var weight = Math.Max(0.01, item.Weight);
        return previous.Contains(LastFmRadioSeedNormalizer.TrackKey(item.Track.Artist, item.Track.Title))
            ? weight * PreviousSnapshotWeight
            : weight;
    }

    private static void SuppressStationOverlap(IReadOnlyList<LastFmRadioStation> stations)
    {
        var used = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
        foreach (var station in stations)
        {
            var unique = station.Tracks.Where(track =>
                !used.Contains(LastFmRadioSeedNormalizer.TrackKey(track.Artist, track.Title))).ToList();
            if (unique.Count >= Math.Min(10, station.Tracks.Count)) station.Tracks = unique;
            foreach (var track in station.Tracks)
                used.Add(LastFmRadioSeedNormalizer.TrackKey(track.Artist, track.Title));
        }
    }

    private static Dictionary<string, double> ScoreArtists(IEnumerable<LastFmRadioPlay> plays)
    {
        var scores = new Dictionary<string, double>(StringComparer.OrdinalIgnoreCase);
        foreach (var group in plays.GroupBy(play => LastFmRadioSeedNormalizer.Artist(play.Artist) ?? play.Artist,
                     StringComparer.OrdinalIgnoreCase))
            scores[group.Key] = group.Take(3).Sum(SeedScore);
        return scores.OrderByDescending(pair => pair.Value).ThenBy(pair => pair.Key)
            .ToDictionary(pair => pair.Key, pair => pair.Value, StringComparer.OrdinalIgnoreCase);
    }

    private static Dictionary<string, double> ScoreLocalTags(IEnumerable<LastFmRadioPlay> plays)
    {
        var tags = new Dictionary<string, double>(StringComparer.OrdinalIgnoreCase);
        foreach (var play in plays.Where(play => !string.IsNullOrWhiteSpace(play.Genre)))
            AddTag(tags, play.Genre!, 2 * SourceWeight(play));
        return tags;
    }

    /// <summary>
    /// One spelling per tag, shared by seeding and by kinship: lower case, single
    /// spaces, the alias table applied ("hip hop" and "hiphop" are "hip-hop"), and empty
    /// for tags that describe the listener rather than the music ("seen live",
    /// "owned", "favorites").
    /// </summary>
    internal static string CanonicalTag(string value)
    {
        var tag = DiscoveryStationSettings.NormalizeTag(value);
        if (TagAliases.TryGetValue(tag, out var alias)) tag = alias;
        return DeniedTags.Contains(tag) ? string.Empty : tag;
    }

    private static void AddTag(Dictionary<string, double> scores, string value, double score)
    {
        var tag = CanonicalTag(value);
        if (tag.Length == 0) return;
        scores[tag] = scores.GetValueOrDefault(tag) + score;
    }

    private static LastFmRadioStation Create(string username, string key, string name,
        LastFmRadioStationKind kind, bool personalized, IEnumerable<string> seeds,
        List<LastFmRadioTrack> tracks, int definitionVersion = 1)
    {
        var now = DateTime.UtcNow;
        return new LastFmRadioStation
        {
            Id = LastFmRadioStateStore.StationId(username, key), Key = key, Name = name,
            Owner = username, Kind = kind, Personalized = personalized,
            DefinitionVersion = definitionVersion, CreatedUtc = now, ChangedUtc = now,
            ValidUntilUtc = now.AddHours(12), Seeds = seeds.Distinct(StringComparer.OrdinalIgnoreCase).ToList(),
            Tracks = tracks
        };
    }

    private static int DefinitionVersion(DiscoveryStationSettings settings) =>
        BitConverter.ToInt32(SHA256.HashData(Encoding.UTF8.GetBytes(
            settings.Id + "|" + settings.Name + "|" + string.Join('|', settings.Tags))), 0);
    private static string Key(string value) => Convert.ToHexString(
        SHA256.HashData(Encoding.UTF8.GetBytes(value.ToLowerInvariant()))[..6]).ToLowerInvariant();
    private static string Title(string value) => System.Globalization.CultureInfo.InvariantCulture.TextInfo
        .ToTitleCase(value.ToLowerInvariant());
    private static string StableOrder(string artist, string title) => Key(artist + "|" + title);
}
