using System.Globalization;

namespace Octo.Models.Settings;

/// <summary>
/// Genre and decade mixes Octo builds from the library and serves like radio stations: per
/// listener, read-only, drawn again on a schedule (#54). Nothing is written to Navidrome, so a
/// rescan cannot empty them and a listener cannot edit one by accident.
/// </summary>
public class GeneratedPlaylistSettings
{
    /// <summary>Environment variable: MIXES_ENABLED</summary>
    public bool Enabled { get; set; } = false;

    /// <summary>A mix per genre. Environment variable: MIXES_GENRES</summary>
    public bool Genres { get; set; } = true;

    /// <summary>A mix per decade. Environment variable: MIXES_DECADES</summary>
    public bool Decades { get; set; } = true;

    /// <summary>Tracks per mix. Environment variable: MIX_TRACK_COUNT</summary>
    public int TrackCount { get; set; } = 100;

    /// <summary>
    /// At most this many tracks by one artist in a mix: without it a lopsided genre becomes half
    /// an album. Environment variable: MIX_MAX_PER_ARTIST
    /// </summary>
    public int MaxPerArtist { get; set; } = 3;

    /// <summary>A mix appears once its genre or decade has this many tracks.
    /// Environment variable: MIX_CREATE_AT</summary>
    public int CreateAt { get; set; } = 20;

    /// <summary>...and goes only below this many, so one at the boundary does not come and go.
    /// Environment variable: MIX_REMOVE_BELOW</summary>
    public int RemoveBelow { get; set; } = 10;

    /// <summary>Most mixes shown, largest first. Environment variable: MIX_MAX_PLAYLISTS</summary>
    public int MaxPlaylists { get; set; } = 20;

    /// <summary>How long one draw lasts before the mixes are drawn again.
    /// Environment variable: MIX_REFRESH_HOURS</summary>
    public int RefreshHours { get; set; } = 24;

    /// <summary>
    /// Percent of each mix, and of Discovery Mix, kept for tracks new to the listener: never
    /// played, or added in the last NewDays. 0 draws from everything alike, so the default changes
    /// nothing. Environment variable: MIX_NEW_SHARE
    /// </summary>
    public int NewShare { get; set; } = 0;

    /// <summary>How recently added a track still counts as new. Environment variable: MIX_NEW_DAYS</summary>
    public int NewDays { get; set; } = 30;

    /// <summary>
    /// How a mix is named; "{0}" is the genre or decade. Empty means "{0} Mix".
    /// Environment variable: MIX_NAME_FORMAT
    /// </summary>
    public string NameFormat { get; set; } = "{0} Mix";

    // ---- Made for you: three lists per listener, on whether or not the genre and decade mixes
    // are. Each is rebuilt once a RefreshHours period from that listener's own plays.

    /// <summary>New releases from the listener's most-played artists, from Deezer.
    /// Environment variable: FOR_YOU_NEW_RELEASES</summary>
    public bool NewReleases { get; set; } = true;

    /// <summary>Songs the listener loved and has not played for RediscoverMonths.
    /// Environment variable: FOR_YOU_REDISCOVER</summary>
    public bool Rediscover { get; set; } = true;

    /// <summary>Songs played at most once, from the listener's most-played artists.
    /// Environment variable: FOR_YOU_DEEP_CUTS</summary>
    public bool DeepCuts { get; set; } = true;

    /// <summary>How far back New Releases looks. Environment variable: NEW_RELEASE_WEEKS</summary>
    public int NewReleaseWeeks { get; set; } = 8;

    /// <summary>How many of the listener's most-played artists New Releases follows.
    /// Environment variable: NEW_RELEASE_ARTISTS</summary>
    public int NewReleaseArtists { get; set; } = 50;

    /// <summary>How long since a loved song was last played before Rediscover brings it back.
    /// Environment variable: REDISCOVER_MONTHS</summary>
    public int RediscoverMonths { get; set; } = 6;

    public int EffectiveNewReleaseWeeks => Math.Clamp(NewReleaseWeeks, 1, 26);
    public int EffectiveNewReleaseArtists => Math.Clamp(NewReleaseArtists, 10, 200);
    public int EffectiveRediscoverMonths => Math.Clamp(RediscoverMonths, 3, 24);

    /// <summary>Whether any of the three Made for you lists is on.</summary>
    public bool AnyForYou => NewReleases || Rediscover || DeepCuts;

    public int EffectiveTrackCount => Math.Clamp(TrackCount, 10, 500);
    public int EffectiveMaxPerArtist => Math.Clamp(MaxPerArtist, 1, 50);
    public int EffectiveCreateAt => Math.Clamp(CreateAt, 1, 10_000);
    public int EffectiveRemoveBelow => Math.Clamp(RemoveBelow, 0, EffectiveCreateAt);
    public int EffectiveMaxPlaylists => Math.Clamp(MaxPlaylists, 1, 100);
    public int EffectiveRefreshHours => Math.Clamp(RefreshHours, 1, 24 * 14);
    public int EffectiveNewShare => Math.Clamp(NewShare, 0, 100);
    public int EffectiveNewDays => Math.Clamp(NewDays, 1, 3650);

    /// <summary>A format that is not a format (a stray brace) names the mix the default way
    /// rather than failing the whole playlist list.</summary>
    public string Name(string label)
    {
        if (NameFormat is { } format && format.Contains("{0}", StringComparison.Ordinal))
        {
            try { return string.Format(CultureInfo.InvariantCulture, format, label).Trim(); }
            catch (FormatException) { /* fall through */ }
        }
        return $"{label} Mix";
    }

    /// <summary>
    /// "Popular right now": the chart of the moment as a read-only playlist for every listener,
    /// in every app, on whether or not the mixes are. Songs a listener has are their library's
    /// own; the rest play right away and are added with a heart. Made again every few hours.
    /// Environment variable: POPULAR_NOW
    /// </summary>
    public bool PopularNow { get; set; } = true;
}
