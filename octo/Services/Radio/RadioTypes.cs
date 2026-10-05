using Octo.Models.Domain;
using Octo.Services.LastFm;

namespace Octo.Services.Radio;

/// <summary>What a radio starts from.</summary>
/// <param name="Title">Empty for an artist's radio.</param>
/// <param name="LibrarySong">The library song, when it is one: its album, genre and file.</param>
/// <param name="YouTubeId">A video already known to play the seed.</param>
public sealed record RadioSeed(string Artist, string Title, int? Duration, Song? LibrarySong,
    string? YouTubeId = null)
{
    public bool IsArtist => Title.Length == 0;
}

/// <summary>How closely a source's answer is about the seed itself.</summary>
public enum RadioMatch
{
    None,
    /// <summary>Songs like this very song.</summary>
    Song,
    /// <summary>Songs like the song a nightcore, sped up or remixed seed came from.</summary>
    Original,
    /// <summary>The most played songs of the seed's genre.</summary>
    Genre,
    /// <summary>Songs of artists like the seed's artist, an artist the source knows for who they are.</summary>
    ArtistsTrusted,
    /// <summary>Songs of artists like a name the source may only know as an uploader.</summary>
    ArtistsGuessed,
}

/// <summary>One source's answer: songs outside the library to resolve, or library songs.</summary>
/// <param name="LibrarySongsComplete">False when the library songs carry only an id, a title and an
/// artist (Sounds alike builds them from its own store): those are read from Navidrome, with the
/// listener's sign-in, before they go out.</param>
public sealed record RadioAnswer(string Provider, RadioMatch Match,
    IReadOnlyList<LastFmService.SimilarTrack> Tracks, IReadOnlyList<Song> LibrarySongs,
    bool LibrarySongsComplete = true)
{
    public static RadioAnswer Nothing(string provider) => new(provider, RadioMatch.None, [], []);

    public int Count => Tracks.Count + LibrarySongs.Count;

    /// <summary>Library songs first, then tracks, in the source's own order.</summary>
    public IEnumerable<(string Artist, string Title, Song? Song, LastFmService.SimilarTrack? Track)> Items()
    {
        foreach (var song in LibrarySongs) yield return (song.Artist, song.Title, song, null);
        foreach (var track in Tracks) yield return (track.Artist, track.Title, null, track);
    }
}

/// <summary>A place radio's suggestions come from.</summary>
public interface IRadioSource
{
    /// <summary>A <see cref="RadioProvider"/> key.</summary>
    string Provider { get; }

    /// <summary>Switched on and set up. A source that is unreachable still says true and
    /// answers nothing.</summary>
    bool Available { get; }

    Task<RadioAnswer> SongsLikeAsync(RadioSeed seed, int count,
        IReadOnlyDictionary<string, string> auth, CancellationToken ct);

    /// <summary>Songs for a station built on a tag ("phonk"). Most sources have none.</summary>
    Task<RadioAnswer> TagAsync(string tag, int count, CancellationToken ct) =>
        Task.FromResult(RadioAnswer.Nothing(Provider));
}
