namespace Octo.Models.Settings;

/// <summary>
/// Importing songs from somewhere else: a Spotify account, a public Spotify link, or a file
/// another service exported. Octo shows what the library already has, keeps a Navidrome playlist
/// of it, and fetches the rest a few songs an hour through the same chain a heart uses.
/// </summary>
public class ImportSettings
{
    /// <summary>
    /// The Client ID of a Spotify app you registered at developer.spotify.com. Empty means no
    /// Spotify sign-in; links and files still work. Not a secret: the sign-in uses PKCE, which
    /// has no client secret. Environment variable: IMPORTS__SPOTIFYCLIENTID
    /// </summary>
    public string SpotifyClientId { get; set; } = "";

    /// <summary>
    /// The redirect URI registered on that Spotify app, exactly as typed there. Spotify takes only
    /// HTTPS or a loopback address (127.0.0.1 or [::1], never "localhost"). The default is a
    /// loopback address with no port, which also lets the Octo apps sign in on any free port.
    /// When it points at Octo itself (Octo behind HTTPS), Octo finishes the sign-in on its own;
    /// otherwise the dashboard asks for the address the browser ended on.
    /// Environment variable: IMPORTS__SPOTIFYREDIRECTURI
    /// </summary>
    public string SpotifyRedirectUri { get; set; } = DefaultRedirectUri;

    public const string DefaultRedirectUri = "http://127.0.0.1/callback";

    /// <summary>
    /// How many missing songs the trickle starts in an hour, at most. It also waits while anyone's
    /// own downloads are running, so it never holds up a heart. 0 stops it starting any.
    /// Environment variable: IMPORTS__SONGSPERHOUR
    /// </summary>
    public int SongsPerHour { get; set; } = 20;

    /// <summary>
    /// How often a list that is kept as a playlist or fetched in full is read again from Spotify,
    /// so new songs there reach the playlist and the trickle. 0 reads them only when asked.
    /// Environment variable: IMPORTS__REFRESHHOURS
    /// </summary>
    public int RefreshHours { get; set; } = 6;

    public string EffectiveRedirectUri =>
        string.IsNullOrWhiteSpace(SpotifyRedirectUri) ? DefaultRedirectUri : SpotifyRedirectUri.Trim();

    public int EffectiveSongsPerHour => Math.Clamp(SongsPerHour, 0, 600);
}
