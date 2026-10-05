namespace Octo.Models.Settings;

/// <summary>
/// Where radio's suggestions come from besides Last.fm, and how much each counts. Song radio
/// (getSimilarSongs) and the stations ask every source that is on, and blend their answers.
/// </summary>
public class RadioSourceSettings
{
    /// <summary>YouTube Music's radio for the song, through the yt-dlp shim. RADIO_YOUTUBE_MUSIC.</summary>
    public bool YouTubeMusic { get; set; } = true;

    /// <summary>ListenBrainz's similar recordings, and LB Radio when Octo has a ListenBrainz
    /// token. RADIO_LISTENBRAINZ.</summary>
    public bool ListenBrainz { get; set; } = true;

    /// <summary>Library songs that sound like the seed, from octo-sonic's analysis of every
    /// file. Off also stops the analysis. RADIO_SOUNDS_ALIKE.</summary>
    public bool SoundsAlike { get; set; } = true;

    /// <summary>The octo-sonic sidecar. RADIO_SONIC_URL.</summary>
    public string SonicUrl { get; set; } = "http://octo-sonic:8080";

    /// <summary>Seconds between two songs in the analysis pass, so a library on a network
    /// mount is read gently. RADIO_SONIC_PAUSE_SECONDS.</summary>
    public int SonicPauseSeconds { get; set; } = 2;

    /// <summary>The ListenBrainz similarity dataset, as Navidrome uses. RADIO_LISTENBRAINZ_ALGORITHM.</summary>
    public string ListenBrainzAlgorithm { get; set; } =
        "session_based_days_9000_session_300_contribution_5_threshold_15_limit_50_skip_30";

    /// <summary>How much each source counts in the blend before learning, 0 to 3. 0 leaves a
    /// source out of the blend. Dashboard only. Reasoned starting points: YouTube Music has the
    /// most listening data and knows uploads; Last.fm is strong for songs it knows; ListenBrainz
    /// is precise but has fewer listeners; how a song sounds says tempo and timbre, not genre or
    /// mood, so alone it counts least.</summary>
    public double LastFmWeight { get; set; } = 0.9;
    public double YouTubeMusicWeight { get; set; } = 1.0;
    public double ListenBrainzWeight { get; set; } = 0.7;
    public double SoundsAlikeWeight { get; set; } = 0.4;

    /// <summary>Let each listener's plays and skips move these weights (0.3 to 1.5).
    /// RADIO_LEARN_FROM_LISTENING.</summary>
    public bool LearnFromListening { get; set; } = true;

    public int EffectiveSonicPauseSeconds => Math.Clamp(SonicPauseSeconds, 0, 600);

    public static double EffectiveWeight(double weight) =>
        double.IsFinite(weight) ? Math.Clamp(weight, 0, 3) : 1;
}
