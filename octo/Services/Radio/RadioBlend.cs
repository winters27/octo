using Octo.Models.Domain;
using Octo.Services.LastFm;

namespace Octo.Services.Radio;

/// <summary>A song the blend kept, and the source that counted most for it.</summary>
/// <param name="NeedsReading">The library song is a stub to read from Navidrome first.</param>
public sealed record RadioPick(LastFmService.SimilarTrack? Track, Song? LibrarySong, string Provider, double Score,
    bool NeedsReading = false);

/// <summary>
/// How radio weighs its sources' answers. Each song scores weight x match x rank decay, the
/// stations' shape (<see cref="RankHalfDepth"/>), and the same song from several sources adds
/// up, since agreement is the best sign a suggestion fits.
/// </summary>
public static class RadioBlend
{
    internal const int RankHalfDepth = 20;
    /// <summary>What a guess from similar artists counts, when nothing better came back.</summary>
    internal const double GuessFactor = 0.3;

    public static double MatchFactor(RadioMatch match) => match switch
    {
        RadioMatch.Song => 1.0,
        RadioMatch.Genre => 0.8,
        RadioMatch.Original => 0.6,
        RadioMatch.ArtistsTrusted => 0.5,
        _ => 0,
    };

    /// <summary>Songs a catalog answer needs before it counts in full.</summary>
    internal const int FullAnswer = 20;
    /// <summary>What Sounds alike counts at least when no catalog knows the song: then the sound is
    /// one of the few signs of its style.</summary>
    internal const double SoundsAlikeWhenUnknown = 0.8;

    /// <summary>A catalog answer of five songs is a weaker sign than one of fifty. Library answers
    /// (an album of three, say) are what they are and count in full.</summary>
    public static double Thin(RadioAnswer answer) =>
        RadioProvider.IsCatalog(answer.Provider) ? Math.Min(1.0, answer.Count / (double)FullAnswer) : 1.0;

    /// <summary>Stations always had similar artists to fall back on, guesses included, so they keep them at a discount.</summary>
    public static double StationFactor(RadioMatch match) =>
        match == RadioMatch.ArtistsGuessed ? GuessFactor : MatchFactor(match);

    /// <summary>
    /// The catalogs found fewer than <see cref="FullAnswer"/> songs like this very song, so the
    /// seed's own album and genre join and lead, and no spacing by artist is applied (#78). A thin
    /// exact answer counts as none: three YouTube Music rows must not push a playlist's own songs
    /// out of the radio.
    /// </summary>
    public static bool LibraryLed(IEnumerable<RadioAnswer> answers) =>
        answers.Where(answer => RadioProvider.IsCatalog(answer.Provider) && answer.Match == RadioMatch.Song)
            .Sum(answer => answer.Count) < FullAnswer;

    /// <summary>Filler (intros, skits, interviews, extreme lengths) and the listener's one-star songs, out.</summary>
    public static RadioAnswer WithoutFillerOrBans(RadioAnswer answer, IReadOnlySet<string> bans) => answer with
    {
        Tracks = answer.Tracks.Where(track => !RadioFiller.IsFiller(track.Title, track.Duration)
            && !bans.Contains(LastFmRadioSeedNormalizer.TrackKey(track.Artist, track.Title))).ToList(),
        LibrarySongs = answer.LibrarySongs.Where(song => !RadioFiller.IsFiller(song.Title, song.Duration)
            && !bans.Contains(LastFmRadioSeedNormalizer.TrackKey(song.Artist, song.Title))).ToList(),
    };

    public static List<RadioPick> Blend(IReadOnlyList<RadioAnswer> answers,
        IReadOnlyDictionary<string, double> weights, int count, string seedKey, string? seedId)
    {
        var picks = Score(answers, weights, seedKey, seedId, guesses: false);
        if (picks.Count == 0) picks = Score(answers, weights, seedKey, seedId, guesses: true);
        return picks.Take(count).ToList();
    }

    private sealed class Entry(int order)
    {
        public int Order { get; } = order;
        public double Score { get; set; }
        public double Best { get; set; }
        public string Provider { get; set; } = "";
        public Song? Song { get; set; }
        public bool SongComplete { get; set; }
        public LastFmService.SimilarTrack? Track { get; set; }
    }

    private static List<RadioPick> Score(IReadOnlyList<RadioAnswer> answers,
        IReadOnlyDictionary<string, double> weights, string seedKey, string? seedId, bool guesses)
    {
        var byKey = new Dictionary<string, Entry>(StringComparer.Ordinal);
        foreach (var answer in answers)
        {
            var match = guesses && answer.Match == RadioMatch.ArtistsGuessed ? GuessFactor : MatchFactor(answer.Match);
            var factor = match * Thin(answer) * weights.GetValueOrDefault(answer.Provider, 1.0);
            if (factor <= 0) continue;
            var rank = 0;
            foreach (var (artist, title, song, track) in answer.Items())
            {
                var term = factor / (1 + rank++ / (double)RankHalfDepth);
                var key = LastFmRadioSeedNormalizer.TrackKey(artist, title);
                if (key.Length == 0 || key == seedKey || (song is not null && song.Id == seedId)) continue;
                if (!byKey.TryGetValue(key, out var entry)) byKey[key] = entry = new Entry(byKey.Count);
                entry.Score += term;
                if (term > entry.Best) { entry.Best = term; entry.Provider = answer.Provider; }
                // A library copy wins over a track to look up: it needs no resolving. A complete
                // copy wins over a stub, which would have to be read again.
                if (song is not null && (entry.Song is null || (!entry.SongComplete && answer.LibrarySongsComplete)))
                {
                    entry.Song = song;
                    entry.SongComplete = answer.LibrarySongsComplete;
                }
                entry.Track ??= track;
            }
        }
        return byKey.Values.OrderByDescending(entry => entry.Score).ThenBy(entry => entry.Order)
            .Select(entry => new RadioPick(entry.Track, entry.Song, entry.Provider, entry.Score,
                NeedsReading: entry.Song is not null && !entry.SongComplete)).ToList();
    }

    /// <summary>The first of a song's genres, the one a tag lookup can use; null for none or a
    /// placeholder.</summary>
    internal static string? PrimaryGenre(string? genre)
    {
        var first = (genre ?? "").Split([';', '/', '|', ','], StringSplitOptions.TrimEntries | StringSplitOptions.RemoveEmptyEntries)
            .FirstOrDefault();
        return first is null || first.Equals("Unknown", StringComparison.OrdinalIgnoreCase)
            || first.Equals("Other", StringComparison.OrdinalIgnoreCase) ? null : first;
    }
}
