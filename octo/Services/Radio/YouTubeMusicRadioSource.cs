using Microsoft.Extensions.Caching.Memory;
using Microsoft.Extensions.Options;
using Octo.Models.Settings;
using Octo.Services.Common;
using Octo.Services.LastFm;
using Octo.Services.YouTube;

namespace Octo.Services.Radio;

/// <summary>
/// YouTube Music's radio for the seed. It knows uploads Last.fm never heard of, nightcore and
/// phonk channels included, which is why it suits a library built from YouTube (#78).
/// </summary>
public sealed class YouTubeMusicRadioSource(YouTubeMusicClient ytm, IOptionsMonitor<RadioSourceSettings> settings)
    : IRadioSource, IDisposable
{
    internal const string Upload = "MUSIC_VIDEO_TYPE_UGC";
    internal const string Podcast = "MUSIC_VIDEO_TYPE_PODCAST_EPISODE";
    private readonly MemoryCache _seeds = new(new MemoryCacheOptions { SizeLimit = 4000 });

    public string Provider => RadioProvider.YouTubeMusic;
    public bool Available => settings.CurrentValue.YouTubeMusic && ytm.Configured;

    internal sealed record SeedVideo(string VideoId, string? VideoType, bool Exact);

    public async Task<RadioAnswer> SongsLikeAsync(RadioSeed seed, int count,
        IReadOnlyDictionary<string, string> auth, CancellationToken ct)
    {
        if (seed.IsArtist)
        {
            // Only when YouTube Music found this very artist: a search for a name it does not
            // know answers with whoever is closest, which is the #78 mistake again.
            var (artist, artistRows) = await ytm.ArtistRadioAsync(seed.Artist, Math.Clamp(count, 10, 100), ct);
            if (artist is null || SongIdentity.Key(artist) != SongIdentity.Key(SongIdentity.PrimaryArtist(seed.Artist)))
                return RadioAnswer.Nothing(Provider);
            var artistTracks = artistRows.Where(row => row.VideoType != Podcast && row.Artists.Count > 0)
                .Select(row => new LastFmService.SimilarTrack(string.Join(", ", row.Artists), row.Title, 1.0,
                    row.DurationSeconds, YouTubeId: row.VideoId, Provider: Provider))
                .Take(count).ToList();
            return new(Provider, artistTracks.Count > 0 ? RadioMatch.ArtistsTrusted : RadioMatch.None, artistTracks, []);
        }
        var found = await SeedVideoAsync(seed, ct);
        if (found is null) return RadioAnswer.Nothing(Provider);
        var rows = await ytm.RadioAsync(found.VideoId, Math.Clamp(count + 1, 10, 100), ct);
        // A radio from a release keeps to releases and official videos; from an upload, uploads
        // are the music (nightcore radio is mostly uploads). Podcast episodes are never music.
        var fromUpload = found.VideoType is null or Upload;
        var tracks = rows
            .Where(row => row.VideoId != found.VideoId && row.VideoType != Podcast)
            .Where(row => fromUpload || row.VideoType != Upload)
            .Where(row => row.Artists.Count > 0)
            .Select(row => new LastFmService.SimilarTrack(string.Join(", ", row.Artists), row.Title, 1.0,
                row.DurationSeconds, YouTubeId: row.VideoId, Provider: Provider))
            .Take(count).ToList();
        return new(Provider, found.Exact ? RadioMatch.Song : RadioMatch.Original, tracks, []);
    }

    internal async Task<SeedVideo?> SeedVideoAsync(RadioSeed seed, CancellationToken ct)
    {
        if (!string.IsNullOrEmpty(seed.YouTubeId)) return new SeedVideo(seed.YouTubeId!, null, Exact: true);
        var key = LastFmRadioSeedNormalizer.TrackKey(seed.Artist, seed.Title);
        if (_seeds.TryGetValue(key, out SeedVideo? cached)) return cached;
        var query = $"{seed.Artist} {seed.Title}";
        var found = Pick(seed, await ytm.SearchAsync(query, "songs", ct))
            ?? Pick(seed, await ytm.SearchAsync(query, "videos", ct));
        _seeds.Set(key, found, new MemoryCacheEntryOptions
        {
            Size = 1,
            AbsoluteExpirationRelativeToNow = found is null ? TimeSpan.FromHours(1) : TimeSpan.FromDays(7),
        });
        return found;
    }

    /// <summary>The same recording first; else the same song in another version, whose radio is
    /// still close (the original for a nightcore seed, or the other way round).</summary>
    internal static SeedVideo? Pick(RadioSeed seed, IReadOnlyList<YtmRow> rows)
    {
        var strict = new SongMatchOptions { LengthToleranceSeconds = 5 };
        foreach (var row in rows)
            if (SongIdentity.Same(new SongRef(seed.Title, seed.Artist, seed.Duration),
                    new SongRef(row.Title, string.Join(", ", row.Artists), row.DurationSeconds), strict).IsSame)
                return new SeedVideo(row.VideoId, row.VideoType, Exact: true);
        var core = SongIdentity.ParseTitle(seed.Title, seed.Artist).Key;
        var artist = SongIdentity.Key(SongIdentity.PrimaryArtist(seed.Artist));
        foreach (var row in rows)
            if (core.Length > 0 && SongIdentity.ParseTitle(row.Title).Key == core
                && row.Artists.Any(name => SongIdentity.Key(SongIdentity.PrimaryArtist(name)) == artist))
                return new SeedVideo(row.VideoId, row.VideoType, Exact: false);
        return null;
    }

    public void Dispose() => _seeds.Dispose();
}
