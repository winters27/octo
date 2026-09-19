using System.Security.Cryptography;
using System.Text;
using Microsoft.Extensions.Options;
using Octo.Models.Radio;
using Octo.Models.Settings;

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

    internal static double SourceWeight(LastFmRadioPlay play) => play.Source switch
    {
        "internet-radio" => RadioPlayWeight,
        "bootstrap-random" => RandomBootstrapWeight,
        _ => 1d,
    };

    private static readonly HashSet<string> DeniedTags = new(StringComparer.OrdinalIgnoreCase)
    {
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

    public LastFmRadioRecommendationService(LastFmService lastFm, LastFmRadioStateStore state,
        IOptionsMonitor<LastFmSettings> settings,
        ILogger<LastFmRadioRecommendationService> logger)
    {
        _lastFm = lastFm;
        _state = state;
        _settings = settings;
        _logger = logger;
    }

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
                var mixCandidates = await TracksFromSeeds(trackSeeds.Take(6), 12, ct);
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
                        ArtistCap(settings, LastFmRadioStationKind.YourMix), familiarQuota: familiarQuota)));
            }

            if (learned)
            {
                var topTags = tags.OrderByDescending(pair => pair.Value).ThenBy(pair => pair.Key)
                    .Select(pair => pair.Key).Take(3).ToList();
                if (settings.EnableDiscoveryMix && topTags.Count > 0)
                {
                    var discovery = await TracksFromTags(topTags, candidateTarget, ct);
                    if (discovery.Count >= 5)
                        stations.Add(Create(username, "discovery", "Discovery Mix",
                            LastFmRadioStationKind.Discovery, true, topTags,
                            Shape(discovery, plays, settings, unavailable, random, Previous("discovery"),
                                ArtistCap(settings, LastFmRadioStationKind.Discovery), excludeRecent: true)));
                }

                foreach (var artist in artistScores.Take(settings.EffectiveArtistStationCount).Select(pair => pair.Key))
                {
                    var candidates = await TracksFromArtist(artist, candidateTarget, ct);
                    var stationKey = "artist-" + Key(artist);
                    if (candidates.Count >= 5)
                        stations.Add(Create(username, stationKey, $"{artist} Radio",
                            LastFmRadioStationKind.Artist, true, [artist],
                            Shape(candidates, plays, settings, unavailable, random, Previous(stationKey),
                                ArtistCap(settings, LastFmRadioStationKind.Artist))));
                }

                foreach (var tag in tags.OrderByDescending(pair => pair.Value).Select(pair => pair.Key)
                             .Take(settings.EffectiveGenreStationCount))
                {
                    var candidates = await TracksFromTags([tag], candidateTarget, ct);
                    var stationKey = "genre-" + Key(tag);
                    if (candidates.Select(item => item.Track.Artist).Distinct(StringComparer.OrdinalIgnoreCase).Count() >= 4)
                        stations.Add(Create(username, stationKey, Title(tag) + " Radio",
                            LastFmRadioStationKind.Genre, true, [tag],
                            Shape(candidates, plays, settings, unavailable, random, Previous(stationKey),
                                ArtistCap(settings, LastFmRadioStationKind.Genre))));
                }
            }
        }

        if (settings.EnableDiscoveryStations)
        {
            foreach (var definition in settings.EffectiveDiscoveryStations().Where(item => item.Enabled))
            {
                var candidates = await TracksFromTags(definition.Tags, candidateTarget, ct);
                if (candidates.Count == 0)
                    candidates.AddRange(plays.Where(play => definition.Tags.Any(tag =>
                            (play.Genre ?? "").Contains(tag, StringComparison.OrdinalIgnoreCase)))
                        .Select(ToCandidate));
                var stationKey = "pinned-" + definition.Id;
                if (candidates.Count > 0)
                    stations.Add(Create(username, stationKey, definition.Name,
                        LastFmRadioStationKind.Pinned, false, definition.Tags,
                        Shape(candidates, plays, settings, unavailable, random, Previous(stationKey),
                            ArtistCap(settings, LastFmRadioStationKind.Pinned)),
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
    /// and a hearted seed counts half again. Last.fm's match score stays as the
    /// per-track signal inside a seed's list.
    /// </summary>
    private async Task<List<Candidate>> TracksFromSeeds(
        IEnumerable<LastFmRadioPlay> seeds, int each, CancellationToken ct)
    {
        var result = new List<Candidate>();
        var position = 0;
        foreach (var seed in seeds)
        {
            var affinity = Math.Pow(SeedRecencyDecay, position++) * (seed.Hearted ? HeartedSeedAffinity : 1d);
            var source = LastFmRadioSeedNormalizer.TrackKey(seed.Artist, seed.Title);
            try
            {
                result.AddRange(Ranked(
                    await _lastFm.GetSimilarTracksAsync(seed.Artist, seed.Title, each, ct), source, affinity));
            }
            catch (OperationCanceledException) { break; }
        }
        return result;
    }

    private async Task<List<Candidate>> TracksFromTags(IEnumerable<string> tags,
        int each, CancellationToken ct)
    {
        var result = new List<Candidate>();
        foreach (var tag in tags.Take(5))
        {
            try { result.AddRange(Ranked(await _lastFm.GetTagTopTracksAsync(tag, each, ct), "tag:" + tag)); }
            catch (OperationCanceledException) { break; }
        }
        return result;
    }

    /// <summary>The seed artist's own top tracks lead; similar artists' top tracks ride at
    /// <see cref="NeighbourArtistAffinity"/> so the station stays about who it is named for.</summary>
    private async Task<List<Candidate>> TracksFromArtist(string artist,
        int candidateTarget, CancellationToken ct)
    {
        var result = Ranked(await _lastFm.GetArtistTopTracksAsync(artist,
            Math.Min(50, candidateTarget), ct), "artist:" + artist);
        foreach (var similar in (await _lastFm.GetSimilarArtistsAsync(artist, 6, ct)).Take(5))
            result.AddRange(Ranked(await _lastFm.GetArtistTopTracksAsync(similar.Name,
                Math.Min(20, Math.Max(6, candidateTarget / 5)), ct), "artist:" + similar.Name,
                NeighbourArtistAffinity));
        return result;
    }

    /// <summary>
    /// Selects the station's tracks from its weighted candidates. One weighted draw
    /// orders the pool (see <see cref="WeightedOrder"/>), then the walk applies the
    /// spacing rules: nothing unavailable, nothing just played when asked, never the
    /// same artist twice in a row, no artist past its cap for this station kind, no
    /// source list (one seed's neighbours, one tag, one artist) past its share, and
    /// when a familiar quota is set, that many tracks from the listener's own history
    /// and the rest from discovery. The caps are the marginal-relevance idea applied
    /// greedily, with artist and source repetition as the redundancy.
    /// </summary>
    private static List<LastFmRadioTrack> Shape(IEnumerable<Candidate> candidates,
        IReadOnlyCollection<LastFmRadioPlay> plays, LastFmSettings settings,
        IReadOnlySet<string> unavailable, Random random, IReadOnlySet<string> previous,
        int artistCap, bool excludeRecent = false, int? familiarQuota = null)
    {
        var target = settings.EffectiveRadioTrackCount;
        var recent = plays.Take(30).Select(play => LastFmRadioSeedNormalizer.TrackKey(play.Artist, play.Title))
            .ToHashSet(StringComparer.OrdinalIgnoreCase);
        var lastArtist = "";
        var perArtist = new Dictionary<string, int>(StringComparer.OrdinalIgnoreCase);
        var perSource = new Dictionary<string, int>(StringComparer.OrdinalIgnoreCase);
        var familiarTaken = 0;
        var output = new List<LastFmRadioTrack>();
        var distinct = candidates
            .Where(item => item.Track.Artist.Length > 0 && item.Track.Title.Length > 0)
            .GroupBy(item => LastFmRadioSeedNormalizer.TrackKey(item.Track.Artist, item.Track.Title))
            .Select(group => group.OrderByDescending(item => item.Weight).First())
            .ToList();
        var sourceCap = SourceCap(target, distinct.Select(item => item.Source)
            .Distinct(StringComparer.OrdinalIgnoreCase).Count());
        foreach (var candidate in WeightedOrder(distinct, random, previous))
        {
            var track = candidate.Track;
            var key = LastFmRadioSeedNormalizer.TrackKey(track.Artist, track.Title);
            if (unavailable.Contains(key)) continue;
            if (excludeRecent && recent.Contains(key)) continue;
            if (string.Equals(lastArtist, track.Artist, StringComparison.OrdinalIgnoreCase)) continue;
            if (perArtist.GetValueOrDefault(track.Artist) >= artistCap) continue;
            if (perSource.GetValueOrDefault(candidate.Source) >= sourceCap) continue;
            var isFamiliar = candidate.Source == FamiliarSource;
            if (familiarQuota is { } quota)
            {
                if (isFamiliar && familiarTaken >= quota) continue;
                if (!isFamiliar && output.Count - familiarTaken >= target - quota) continue;
            }
            output.Add(new LastFmRadioTrack
            {
                Artist = LastFmRadioSeedNormalizer.Artist(track.Artist) ?? track.Artist,
                Title = LastFmRadioSeedNormalizer.Title(track.Title) ?? track.Title,
                Duration = track.Duration, Score = track.Match, Source = "lastfm"
            });
            lastArtist = track.Artist;
            perArtist[track.Artist] = perArtist.GetValueOrDefault(track.Artist) + 1;
            perSource[candidate.Source] = perSource.GetValueOrDefault(candidate.Source) + 1;
            if (isFamiliar) familiarTaken++;
            if (output.Count >= target) break;
        }
        return output;
    }

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
