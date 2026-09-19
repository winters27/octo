namespace Octo.Models.Download;

/// <summary>
/// One entry in the running log of songs Octo has fetched (via download-on-star or
/// permanent-mode playback). Surfaced in the admin dashboard's "Fetched songs" view.
/// </summary>
public class DownloadHistoryEntry
{
    public string Artist { get; set; } = string.Empty;
    public string Title { get; set; } = string.Empty;
    public string? Album { get; set; }

    /// <summary>Absolute path the file was saved to.</summary>
    public string Path { get; set; } = string.Empty;

    /// <summary>File format, upper-cased from the extension (FLAC, MP3, M4A).</summary>
    public string Format { get; set; } = string.Empty;

    /// <summary>Where it came from — "Soulseek" (FLAC) or "YouTube" (MP3).</summary>
    public string Source { get; set; } = string.Empty;

    /// <summary>Cover art URL (Deezer), for the thumbnail in the log.</summary>
    public string? CoverArtUrl { get; set; }

    public long SizeBytes { get; set; }

    /// <summary>When it was saved (ISO 8601, UTC).</summary>
    public string DownloadedAt { get; set; } = string.Empty;

    /// <summary>
    /// Who asked for this file, when Octo could tell. A star or a play carries the Subsonic
    /// username; an acquisition Octo started itself carries nobody, and so does every entry
    /// written before this field existed.
    ///
    /// A list rather than one name, because a second user starring a track that is already
    /// being fetched joins that transfer instead of starting another. Recording only whoever
    /// got there first would attribute the file to one of them and silently drop the rest.
    /// </summary>
    public List<string>? RequestedBy { get; set; }
}
