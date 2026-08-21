namespace Octo.Models.Settings;

public enum LidarrCompletionMode
{
    /// <summary>The heart is considered handed off once Lidarr accepts AlbumSearch.</summary>
    Accepted,

    /// <summary>Completion/failure notifications follow the actual import or timeout.</summary>
    Imported,
}

/// <summary>Connection and add-album defaults for an existing Lidarr server.</summary>
public sealed class LidarrSettings
{
    public string? BaseUrl { get; set; }
    public string? ApiKey { get; set; }
    public string? RootFolderPath { get; set; }
    public int QualityProfileId { get; set; }
    public int MetadataProfileId { get; set; }
    public LidarrCompletionMode CompletionMode { get; set; } = LidarrCompletionMode.Accepted;
    public int ImportTimeoutSeconds { get; set; } = 1800;
}
