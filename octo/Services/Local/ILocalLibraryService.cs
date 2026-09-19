using Octo.Models.Domain;
using Octo.Models.Settings;
using Octo.Models.Download;
using Octo.Models.Search;
using Octo.Models.Subsonic;

namespace Octo.Services.Local;

/// <summary>
/// Interface for local music library management
/// </summary>
public interface ILocalLibraryService
{
    /// <summary>
    /// Checks if an external song already exists locally
    /// </summary>
    Task<string?> GetLocalPathForExternalSongAsync(string externalProvider, string externalId);
    
    /// <summary>
    /// Registers a downloaded song in the local library
    /// </summary>
    Task RegisterDownloadedSongAsync(Song song, string localPath);
    
    /// <summary>
    /// Gets the mapping between external ID and local ID
    /// </summary>
    Task<string?> GetLocalIdForExternalSongAsync(string externalProvider, string externalId);
    
    /// <summary>
    /// Parses a song ID to determine if it is external or local
    /// </summary>
    (bool isExternal, string? provider, string? externalId) ParseSongId(string songId);
    
    /// <summary>
    /// Parses an external ID to extract the provider, type and ID
    /// Format: ext-{provider}-{type}-{id} (e.g., ext-deezer-artist-259, ext-deezer-album-96126, ext-deezer-song-12345)
    /// Also supports legacy format: ext-{provider}-{id} (assumes song type)
    /// </summary>
    (bool isExternal, string? provider, string? type, string? externalId) ParseExternalId(string id);
    
    /// <summary>
    /// Triggers a Subsonic library scan
    /// </summary>
    /// <param name="force">
    /// Bypass the debounce. Needed when a caller must guarantee the scan actually runs,
    /// e.g. after each track of an album download so the album fills in progressively
    /// and the final tracks are never left stranded by a swallowed trigger.
    /// </param>
    /// <summary>
    /// Every download Octo has a record of. The genre backfill uses this to scope a run to
    /// files Octo itself created, which is the only scope where rewriting a tag is rewriting
    /// our own output rather than someone's hand-curated rip.
    /// </summary>
    Task<IReadOnlyList<LocalSongMapping>> GetMappingsAsync();

    Task<bool> TriggerLibraryScanAsync(bool force = false);
    
    /// <summary>
    /// Gets the current scan status
    /// </summary>
    Task<ScanStatus?> GetScanStatusAsync();
}
