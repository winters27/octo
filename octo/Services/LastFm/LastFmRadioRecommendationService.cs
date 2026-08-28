using System.Security.Cryptography;
using System.Text;
using Microsoft.Extensions.Options;
using Octo.Models.Radio;
using Octo.Models.Settings;

namespace Octo.Services.LastFm;

/// <summary>Builds deterministic canonical station snapshots from bounded listening signals.</summary>
public sealed class LastFmRadioRecommendationService
{
    private static readonly HashSet<string> DeniedTags = new(StringComparer.OrdinalIgnoreCase)
    {
        "seen live", "favorites", "favourites", "owned", "spotify", "albums i own",
        "under 2000 listeners", "awesome", "love", "best"
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
        var plays = user.Plays.OrderByDescending(play => play.PlayedAtUtc).ToList();
        var unavailable = user.UnavailableTracks
            .Where(track => track.RetryAfterUtc > DateTime.UtcNow)
            .Select(track => track.Key).ToHashSet(StringComparer.OrdinalIgnoreCase);
        var refillHeadroom = Math.Min(unavailable.Count, settings.EffectiveRadioTrackCount);
        var candidateTarget = Math.Min(100,
            settings.EffectiveRadioTrackCount + refillHeadroom + 10);
        var artistScores = ScoreArtists(plays);
        var trackSeeds = plays.GroupBy(play => LastFmRadioSeedNormalizer.TrackKey(play.Artist, play.Title))
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
            var learned = plays.Count(play => play.LearnedSignal) >= settings.EffectiveMinimumPlays;
            var mixCandidates = await TracksFromSeeds(trackSeeds.Take(6), 12, ct);
            if (mixCandidates.Count == 0) mixCandidates.AddRange(plays.Select(ToCandidate));
            var blendedMix = Blend(plays.Select(ToCandidate), mixCandidates,
                settings.EffectiveDiscoveryPercent,
                Math.Min(settings.EffectiveRadioTrackCount * 2,
                    settings.EffectiveRadioTrackCount + refillHeadroom));
            stations.Add(Create(username, learned ? "your-mix" : "starter",
                learned ? "Your Mix" : "Starter Radio",
                learned ? LastFmRadioStationKind.YourMix : LastFmRadioStationKind.Starter,
                true, trackSeeds.Select(seed => seed.Artist),
                Shape(blendedMix, plays, settings, unavailable)));

            if (learned)
            {
                var topTags = tags.OrderByDescending(pair => pair.Value).ThenBy(pair => pair.Key)
                    .Select(pair => pair.Key).Take(3).ToList();
                if (topTags.Count > 0)
                {
                    var discovery = await TracksFromTags(topTags, candidateTarget, ct);
                    if (discovery.Count >= 5)
                        stations.Add(Create(username, "discovery", "Discovery Mix",
                            LastFmRadioStationKind.Discovery, true, topTags,
                            Shape(discovery, plays, settings, unavailable, excludeRecent: true)));
                }

                foreach (var artist in artistScores.Take(2).Select(pair => pair.Key))
                {
                    var candidates = await TracksFromArtist(artist, candidateTarget, ct);
                    if (candidates.Count >= 5)
                        stations.Add(Create(username, "artist-" + Key(artist), $"{artist} Radio",
                            LastFmRadioStationKind.Artist, true, [artist],
                            Shape(candidates, plays, settings, unavailable)));
                }

                foreach (var tag in tags.OrderByDescending(pair => pair.Value).Select(pair => pair.Key).Take(3))
                {
                    var candidates = await TracksFromTags([tag], candidateTarget, ct);
                    if (candidates.Select(item => item.Artist).Distinct(StringComparer.OrdinalIgnoreCase).Count() >= 4)
                        stations.Add(Create(username, "genre-" + Key(tag), Title(tag) + " Radio",
                            LastFmRadioStationKind.Genre, true, [tag],
                            Shape(candidates, plays, settings, unavailable)));
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
                if (candidates.Count > 0)
                    stations.Add(Create(username, "pinned-" + definition.Id, definition.Name,
                        LastFmRadioStationKind.Pinned, false, definition.Tags,
                        Shape(candidates, plays, settings, unavailable),
                        DefinitionVersion(definition)));
            }
        }

        SuppressStationOverlap(stations);
        foreach (var station in stations)
            station.ValidUntilUtc = station.ChangedUtc.AddHours(settings.EffectiveRefreshIntervalHours);
        _logger.LogInformation("Built {Count} Last.fm radio stations for {User}", stations.Count, username);
        return stations.Where(station => station.Tracks.Count > 0).ToList();
    }

    private async Task<List<LastFmService.SimilarTrack>> TracksFromSeeds(
        IEnumerable<LastFmRadioPlay> seeds, int each, CancellationToken ct)
    {
        var result = new List<LastFmService.SimilarTrack>();
        foreach (var seed in seeds)
        {
            try { result.AddRange(await _lastFm.GetSimilarTracksAsync(seed.Artist, seed.Title, each, ct)); }
            catch (OperationCanceledException) { break; }
        }
        return result;
    }

    private async Task<List<LastFmService.SimilarTrack>> TracksFromTags(IEnumerable<string> tags,
        int each, CancellationToken ct)
    {
        var result = new List<LastFmService.SimilarTrack>();
        foreach (var tag in tags.Take(5))
        {
            try { result.AddRange(await _lastFm.GetTagTopTracksAsync(tag, each, ct)); }
            catch (OperationCanceledException) { break; }
        }
        return result;
    }

    private async Task<List<LastFmService.SimilarTrack>> TracksFromArtist(string artist,
        int candidateTarget, CancellationToken ct)
    {
        var result = await _lastFm.GetArtistTopTracksAsync(artist,
            Math.Min(50, candidateTarget), ct);
        foreach (var similar in (await _lastFm.GetSimilarArtistsAsync(artist, 6, ct)).Take(5))
            result.AddRange(await _lastFm.GetArtistTopTracksAsync(similar.Name,
                Math.Min(20, Math.Max(6, candidateTarget / 5)), ct));
        return result;
    }

    private static List<LastFmRadioTrack> Shape(IEnumerable<LastFmService.SimilarTrack> candidates,
        IReadOnlyCollection<LastFmRadioPlay> plays, LastFmSettings settings,
        IReadOnlySet<string> unavailable, bool excludeRecent = false)
    {
        var recent = plays.Take(30).Select(play => LastFmRadioSeedNormalizer.TrackKey(play.Artist, play.Title))
            .ToHashSet(StringComparer.OrdinalIgnoreCase);
        var lastArtist = "";
        var output = new List<LastFmRadioTrack>();
        foreach (var candidate in candidates
                     .Where(item => item.Artist.Length > 0 && item.Title.Length > 0)
                     .GroupBy(item => LastFmRadioSeedNormalizer.TrackKey(item.Artist, item.Title))
                     .Select(group => group.OrderByDescending(item => item.Match).First())
                     .OrderByDescending(item => item.Match)
                     .ThenBy(item => StableOrder(item.Artist, item.Title)))
        {
            var key = LastFmRadioSeedNormalizer.TrackKey(candidate.Artist, candidate.Title);
            if (unavailable.Contains(key)) continue;
            if (excludeRecent && recent.Contains(key)) continue;
            if (string.Equals(lastArtist, candidate.Artist, StringComparison.OrdinalIgnoreCase)) continue;
            output.Add(new LastFmRadioTrack
            {
                Artist = LastFmRadioSeedNormalizer.Artist(candidate.Artist) ?? candidate.Artist,
                Title = LastFmRadioSeedNormalizer.Title(candidate.Title) ?? candidate.Title,
                Duration = candidate.Duration, Score = candidate.Match, Source = "lastfm"
            });
            lastArtist = candidate.Artist;
            if (output.Count >= settings.EffectiveRadioTrackCount) break;
        }
        return output;
    }

    private static LastFmService.SimilarTrack ToCandidate(LastFmRadioPlay play) =>
        new(play.Artist, play.Title, play.Hearted ? 2 : 1, play.Duration);

    private static IEnumerable<LastFmService.SimilarTrack> Blend(
        IEnumerable<LastFmService.SimilarTrack> familiar,
        IEnumerable<LastFmService.SimilarTrack> discovery, int discoveryPercent, int count)
    {
        var familiarCount = count - (int)Math.Round(count * discoveryPercent / 100d);
        return familiar.Take(Math.Max(0, familiarCount))
            .Concat(discovery.Take(Math.Max(0, count - familiarCount)));
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
            scores[group.Key] = group.Take(3).Sum(play => Math.Exp(-(DateTime.UtcNow - play.PlayedAtUtc).TotalDays / 45)
                * (play.Hearted ? 2 : 1));
        return scores.OrderByDescending(pair => pair.Value).ThenBy(pair => pair.Key)
            .ToDictionary(pair => pair.Key, pair => pair.Value, StringComparer.OrdinalIgnoreCase);
    }

    private static Dictionary<string, double> ScoreLocalTags(IEnumerable<LastFmRadioPlay> plays)
    {
        var tags = new Dictionary<string, double>(StringComparer.OrdinalIgnoreCase);
        foreach (var play in plays.Where(play => !string.IsNullOrWhiteSpace(play.Genre))) AddTag(tags, play.Genre!, 2);
        return tags;
    }

    private static void AddTag(Dictionary<string, double> scores, string value, double score)
    {
        var tag = DiscoveryStationSettings.NormalizeTag(value);
        if (TagAliases.TryGetValue(tag, out var alias)) tag = alias;
        if (tag.Length == 0 || DeniedTags.Contains(tag)) return;
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
