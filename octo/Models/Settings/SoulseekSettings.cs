namespace Octo.Models.Settings;

/// <summary>
/// Configuration for the Soulseek (slskd) integration.
/// Octo talks to a self-hosted slskd instance which fronts the Soulseek P2P network.
/// </summary>
public class SoulseekSettings
{
    /// <summary>
    /// Base URL of the slskd REST API (e.g. http://slskd:5030 when running in the same docker network).
    /// </summary>
    public string? BaseUrl { get; set; }

    /// <summary>
    /// slskd web UI / API admin username (Basic Auth).
    /// </summary>
    public string? Username { get; set; }

    /// <summary>
    /// slskd web UI / API admin password (Basic Auth).
    /// </summary>
    public string? Password { get; set; }

    /// <summary>
    /// How long to wait (seconds) for a Soulseek search to gather peer responses
    /// before returning results. Soulseek searches stream in over time; a longer wait
    /// gives more candidates, a shorter wait keeps radio responsive.
    /// </summary>
    public int SearchWaitSeconds { get; set; } = 6;

    /// <summary>
    /// Minimum file size in bytes to consider a search hit a real lossless file.
    /// Default 5 MB filters out 30s teaser clips and mislabelled tiny files.
    /// </summary>
    public long MinFileSizeBytes { get; set; } = 5 * 1024 * 1024;

    /// <summary>
    /// Preferred file extension. Hits with this extension are sorted first.
    /// </summary>
    public string PreferredExtension { get; set; } = "flac";

    /// <summary>
    /// Max time to wait (seconds) for a download to complete before giving up.
    /// </summary>
    public int DownloadTimeoutSeconds { get; set; } = 180;
}
