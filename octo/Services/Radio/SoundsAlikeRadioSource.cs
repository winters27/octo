using Microsoft.Extensions.Options;
using Octo.Models.Domain;
using Octo.Models.Settings;
using Octo.Services.Sonic;

namespace Octo.Services.Radio;

/// <summary>Library songs that sound like the seed, from octo-sonic's analysis. Only for library seeds.</summary>
public sealed class SoundsAlikeRadioSource(SonicStore store, IOptionsMonitor<RadioSourceSettings> settings) : IRadioSource
{
    public string Provider => RadioProvider.SoundsAlike;
    public bool Available => settings.CurrentValue.SoundsAlike;

    public Task<RadioAnswer> SongsLikeAsync(RadioSeed seed, int count,
        IReadOnlyDictionary<string, string> auth, CancellationToken ct)
    {
        if (seed.IsArtist || seed.LibrarySong is not { Id.Length: > 0 } song) return Task.FromResult(RadioAnswer.Nothing(Provider));
        var near = store.Nearest(song.Id, count);
        // Stubs: SongRadioService reads each picked song again with the listener's own sign-in.
        var songs = near.Select(item => new Song
        {
            Id = item.Id, Title = item.Song.Title, Artist = item.Song.Artist, Album = item.Song.Album ?? "",
            Duration = item.Song.Duration, IsLocal = true,
        }).ToList();
        return Task.FromResult(new RadioAnswer(Provider, songs.Count > 0 ? RadioMatch.Song : RadioMatch.None, [], songs,
            LibrarySongsComplete: false));
    }
}
