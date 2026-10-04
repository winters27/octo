using Octo.Models.Settings;

namespace Octo.Services.Soulseek;

/// <summary>
/// How long and how wide one Soulseek search looks. slskd ends a search at whichever comes first:
/// SearchTimeoutMs with no new answer, ResponseLimit peers, or FileLimit files. The file limit
/// counts every format. Octo asked for 150 files, so a popular song's first wave of MP3s ended
/// the search before the FLAC answers arrived (#70). CeilingSeconds is Octo's own limit: a search
/// still running then is cancelled, and slskd still hands over what it gathered.
/// </summary>
public sealed record SearchProfile(string Name, int CeilingSeconds, int SearchTimeoutMs, int ResponseLimit, int FileLimit)
{
    /// <summary>
    /// How wide every song search reads, a star's and Better quality's alike, so the first
    /// download sees what Better quality would. A star once asked for 500 files: "Drake Take Care"
    /// filled that with 33 peers' answers in one second, none of them a usable FLAC, and the song
    /// came from YouTube as an MP3; Better quality read 2,000 files from 117 peers and found the
    /// FLAC at once. Reading 2,000 took the same second.
    /// </summary>
    internal const int SongResponseLimit = 500;

    /// <inheritdoc cref="SongResponseLimit"/>
    internal const int SongFileLimit = 2_000;

    /// <summary>A star or a play. Searched as wide as Better quality; somebody may be waiting, so
    /// the configured ceiling holds and a quiet search ends after 15 s without a new answer.</summary>
    public static SearchProfile Interactive(SoulseekSettings s) =>
        new("interactive", s.SearchWaitSeconds, 15_000, SongResponseLimit, SongFileLimit);

    /// <summary>Better quality and the weekly upgrade. Nobody is waiting, so it waits longer for
    /// slow peers: the same width as a star, with more patience.</summary>
    public static SearchProfile Upgrade(SoulseekSettings s) =>
        new("upgrade", s.EffectiveUpgradeSearchWaitSeconds, 30_000, SongResponseLimit, SongFileLimit);

    /// <summary>An album heart's one search for the whole record. Each peer answers with many
    /// files, so the file limit is wide; nobody is waiting on one song, so it may look a little
    /// longer than a star does.</summary>
    public static SearchProfile Album(SoulseekSettings s) =>
        new("album", Math.Max(s.SearchWaitSeconds, 45), 20_000, 500, 3_000);

    /// <summary>
    /// The same search asked again, wider, once a search filled its limit and nothing in it was
    /// usable: the first, fastest answers to a very popular song can all be lossy, and the lossless
    /// ones were still on their way when slskd stopped counting.
    /// </summary>
    public SearchProfile Wider() =>
        this with { Name = $"{Name}, wider", ResponseLimit = ResponseLimit * 2, FileLimit = FileLimit * 4 };
}
