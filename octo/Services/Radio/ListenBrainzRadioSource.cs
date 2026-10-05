using Microsoft.Extensions.Options;
using Octo.Models.Settings;
using Octo.Services.LastFm;
using Octo.Services.ListenBrainz;

namespace Octo.Services.Radio;

/// <summary>
/// ListenBrainz's similar recordings for a song, found by the recording's MusicBrainz id; LB Radio
/// for an artist or a tag when Octo holds a ListenBrainz token.
/// </summary>
public sealed class ListenBrainzRadioSource(ListenBrainzRadioClient client, IOptionsMonitor<RadioSourceSettings> settings)
    : IRadioSource
{
    public string Provider => RadioProvider.ListenBrainz;
    public bool Available => settings.CurrentValue.ListenBrainz;

    public async Task<RadioAnswer> SongsLikeAsync(RadioSeed seed, int count,
        IReadOnlyDictionary<string, string> auth, CancellationToken ct)
    {
        if (seed.IsArtist)
        {
            var mbid = seed.LibrarySong?.MusicBrainzArtistIds?.FirstOrDefault();
            var radio = await client.LbRadioAsync($"artist:({mbid ?? seed.Artist})", count, ct);
            return new(Provider, radio.Count > 0 ? RadioMatch.ArtistsTrusted : RadioMatch.None, Tracks(radio), []);
        }
        var recording = seed.LibrarySong?.MusicBrainzRecordingId is { Length: > 0 } tagged ? tagged
            : await client.RecordingMbidAsync(seed.Artist, seed.Title, ct);
        if (recording is null) return RadioAnswer.Nothing(Provider);
        var similar = await client.SimilarRecordingsAsync(recording, settings.CurrentValue.ListenBrainzAlgorithm, count, ct);
        return new(Provider, similar.Count > 0 ? RadioMatch.Song : RadioMatch.None, Tracks(similar), []);
    }

    public async Task<RadioAnswer> TagAsync(string tag, int count, CancellationToken ct)
    {
        var radio = await client.LbRadioAsync($"tag:({tag})", count, ct);
        return new(Provider, radio.Count > 0 ? RadioMatch.Genre : RadioMatch.None, Tracks(radio), []);
    }

    private List<LastFmService.SimilarTrack> Tracks(IEnumerable<ListenBrainzTrack> tracks) =>
        tracks.Select(track => new LastFmService.SimilarTrack(track.Artist, track.Title, track.Score,
            track.DurationSeconds, Provider: Provider)).ToList();
}
