namespace Octo.Models.Settings;

public class LastFmSettings
{
    /// <summary>
    /// Last.fm API key for fetching similar tracks
    /// </summary>
    public string ApiKey { get; set; } = string.Empty;
    
    /// <summary>
    /// Enable/disable the radio feature
    /// </summary>
    public bool EnableRadio { get; set; } = true;
    
    /// <summary>
    /// Number of similar tracks to return
    /// </summary>
    public int RadioTrackCount { get; set; } = 50;
    
    /// <summary>
    /// Cache duration for Last.fm lookups in hours
    /// </summary>
    public int RadioCacheDurationHours { get; set; } = 24;
}
