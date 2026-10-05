using Octo.Services.LastFm;

namespace Octo.Services.Radio;

/// <summary>Last.fm's songs like a song, or the top songs of artists like an artist.</summary>
public sealed class LastFmRadioSource(LastFmService lastFm) : IRadioSource
{
    public string Provider => RadioProvider.LastFm;
    public bool Available => lastFm.HasApiKey;

    public async Task<RadioAnswer> SongsLikeAsync(RadioSeed seed, int count,
        IReadOnlyDictionary<string, string> auth, CancellationToken ct)
    {
        if (seed.IsArtist)
            return new(Provider, RadioMatch.ArtistsTrusted,
                Mark(await lastFm.GetSimilarArtistTracksAsync(seed.Artist, count, ct)), []);
        var answer = await lastFm.FindSimilarTracksAsync(seed.Artist, seed.Title, count, ct);
        var match = answer.Source switch
        {
            LastFmService.SimilarSource.Song => RadioMatch.Song,
            LastFmService.SimilarSource.Original => RadioMatch.Original,
            LastFmService.SimilarSource.SimilarArtists when answer.ArtistTrusted => RadioMatch.ArtistsTrusted,
            LastFmService.SimilarSource.SimilarArtists => RadioMatch.ArtistsGuessed,
            _ => RadioMatch.None,
        };
        return new(Provider, match, Mark(answer.Tracks), []);
    }

    public async Task<RadioAnswer> TagAsync(string tag, int count, CancellationToken ct) =>
        new(Provider, RadioMatch.Genre, Mark(await lastFm.GetTagTopTracksAsync(tag, count, ct)), []);

    private List<LastFmService.SimilarTrack> Mark(IEnumerable<LastFmService.SimilarTrack> tracks) =>
        tracks.Select(track => track with { Provider = Provider }).ToList();
}
