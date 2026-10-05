using Microsoft.Extensions.Options;
using Octo.Models.Domain;
using Octo.Models.Settings;
using Octo.Services.LastFm;

namespace Octo.Services.Radio;

/// <summary>
/// Radio from one song, album or artist (getSimilarSongs): every source asked at once, their
/// answers blended, resolved local first, and each song marked with the source it came from.
/// </summary>
public sealed class SongRadioService(RadioSourceSet sources, LastFmRadioTrackResolver resolver,
    IOptionsMonitor<LastFmSettings> lastFmSettings,
    IOptionsMonitor<SubsonicSettings> subsonicSettings,
    ILogger<SongRadioService> logger)
{
    /// <summary>Picks resolved beyond the count asked for, so the sound check has some to drop.</summary>
    internal const double PickHeadroom = 1.5;

    public bool Enabled => lastFmSettings.CurrentValue.EnableRadio && sources.Available.Count > 0;

    /// <param name="listener">Who the radio is for, named the way scrobbles name them
    /// (RequestIdentity), so what they play teaches their own weights; null for nobody.</param>
    public async Task<List<Song>> BuildAsync(RadioSeed seed, int count, string? listener,
        IReadOnlyDictionary<string, string> auth, IReadOnlySet<string> bans, CancellationToken ct)
    {
        // Strip collab/feature decoration so the catalogs find the canonical artist.
        var lookup = seed with
        {
            Artist = LastFmRadioSeedNormalizer.Artist(seed.Artist) ?? seed.Artist,
            Title = LastFmRadioSeedNormalizer.Title(seed.Title) ?? seed.Title,
        };
        // A few more than asked for: filler and the listener's one-star songs are left out.
        var answers = (await sources.AskAllAsync(lookup, Math.Min(count + 10, 100), auth, ct)).ToList();

        // When no catalog has songs like this very song, the seed's own tags know its style better
        // than a guess from its artist's name, which for a YouTube upload is only the channel's (#78).
        var libraryLed = seed.LibrarySong is not null && !seed.IsArtist && RadioBlend.LibraryLed(answers);
        if (libraryLed)
        {
            answers.Add(new RadioAnswer(RadioProvider.Library, RadioMatch.Song, [],
                await resolver.LibraryNeighboursAsync(seed.LibrarySong!, count, auth)));
            if (RadioBlend.PrimaryGenre(seed.LibrarySong!.Genre) is { } genre)
                answers.AddRange(await sources.TagAllAsync(genre, count, ct));
        }

        answers = answers.Select(answer => RadioBlend.WithoutFillerOrBans(answer, bans)).ToList();
        var weights = sources.Weights(listener, libraryLed);
        var picks = RadioBlend.Blend(answers, weights, (int)Math.Ceiling(count * PickHeadroom),
            LastFmRadioSeedNormalizer.TrackKey(seed.Artist, seed.Title), seed.LibrarySong?.Id);
        logger.LogInformation("Radio for '{Artist} - {Title}': {Answers}; weights {Weights}; {Picks} picked{Led}",
            seed.Artist, seed.Title,
            string.Join(", ", answers.Select(answer => $"{answer.Provider} {answer.Match} {answer.Count}")),
            string.Join(", ", weights.Select(pair => $"{pair.Key} {pair.Value:0.00}")),
            picks.Count, libraryLed ? ", led by the library" : "");

        // For each pick, prefer the local copy if we own it. Tracks the user already has play at
        // full quality from Navidrome and skip the YouTube round trip. Lookups run in parallel.
        using var gate = new SemaphoreSlim(10);
        var resolved = await Task.WhenAll(picks.Select(async pick =>
        {
            await gate.WaitAsync(ct);
            try
            {
                var song = pick.LibrarySong is { } library
                    ? (pick.NeedsReading ? await resolver.ReadLibrarySongAsync(library.Id, auth) : library)
                    : await resolver.ResolveAsync(pick.Track!.Artist, pick.Track.Title, pick.Track.Duration,
                        auth, ct, pick.Track.YouTubeId);
                if (song is not null) song.SuggestedBy = RadioProvider.DisplayName(pick.Provider);
                return song;
            }
            catch (Exception ex) when (ex is not OperationCanceledException)
            {
                logger.LogDebug(ex, "radio resolve failed for a {Provider} pick", pick.Provider);
                return null;
            }
            finally { gate.Release(); }
        }));
        // One per song, and never the seed itself.
        var scored = picks.Zip(resolved, (pick, song) => (Song: song, pick.Score))
            .Where(item => item.Song is not null && item.Song.Id != seed.LibrarySong?.Id)
            .Select(item => (Song: item.Song!, item.Score)).DistinctBy(item => item.Song.Id).ToList();
        // The admin's explicit filter, as stations apply it: with outside songs from every source,
        // radio is where most unknown lyrics now come from.
        var filter = subsonicSettings.CurrentValue.ExplicitFilter;
        var songs = scored.Select(item => item.Song).Where(song => filter switch
            {
                ExplicitFilter.CleanOnly => song.ExplicitContentLyrics is not 1,
                ExplicitFilter.ExplicitOnly => song.ExplicitContentLyrics is not 3,
                _ => true,
            }).Take(count).ToList();

        // A library-led radio keeps its order: its neighbours often share the seed's artist (one
        // uploader for a whole playlist), and spacing by artist would push every one of them back.
        return libraryLed ? songs : LastFmRadioSpacing.Spread(songs, song => song.Artist, seed.Artist);
    }
}
