namespace Octo.Services.Radio;

/// <summary>
/// Where a radio suggestion came from. The keys are stored with station tracks, so they never
/// change; the display names are what the apps show after "Suggested by".
/// </summary>
public static class RadioProvider
{
    public const string LastFm = "lastfm";
    public const string YouTubeMusic = "youtube-music";
    public const string ListenBrainz = "listenbrainz";
    public const string SoundsAlike = "sounds-alike";
    public const string Library = "library";
    /// <summary>A song from the listener's own plays, which no source suggested.</summary>
    public const string History = "history";

    /// <summary>The sources that know songs beyond the library.</summary>
    public static bool IsCatalog(string provider) =>
        provider is LastFm or YouTubeMusic or ListenBrainz;

    /// <summary>What the apps show; null for a song the listener played themselves.</summary>
    public static string? DisplayName(string? provider) => provider switch
    {
        LastFm => "Last.fm",
        YouTubeMusic => "YouTube Music",
        ListenBrainz => "ListenBrainz",
        SoundsAlike => "Sounds alike",
        Library => "Your library",
        _ => null,
    };

    /// <summary>The key behind a display name, for a song that carries only the name.</summary>
    public static string? FromDisplayName(string? name) =>
        new[] { LastFm, YouTubeMusic, ListenBrainz, SoundsAlike, Library }
            .FirstOrDefault(key => DisplayName(key) == name);
}
