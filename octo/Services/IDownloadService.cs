using Octo.Models.Domain;
using Octo.Models.Settings;
using Octo.Models.Download;
using Octo.Models.Search;
using Octo.Models.Subsonic;

namespace Octo.Services;

/// <summary>
/// Interface for the music download service (Deezspot or other)
/// </summary>
public interface IDownloadService
{
    /// <summary>
    /// Downloads a song from an external provider
    /// </summary>
    /// <param name="externalProvider">The provider (deezer, spotify)</param>
    /// <param name="externalId">The ID on the external provider</param>
    /// <param name="cancellationToken">Cancellation token</param>
    /// <returns>The path to the downloaded file</returns>
    Task<string> DownloadSongAsync(string externalProvider, string externalId, CancellationToken cancellationToken = default);
    
    /// <summary>
    /// Downloads a song and streams the result progressively
    /// </summary>
    /// <param name="externalProvider">The provider (deezer, spotify)</param>
    /// <param name="externalId">The ID on the external provider</param>
    /// <param name="cancellationToken">Cancellation token</param>
    /// <returns>A stream of the audio file</returns>
    Task<Stream> DownloadAndStreamAsync(string externalProvider, string externalId, CancellationToken cancellationToken = default);
    
    /// <summary>
    /// Downloads remaining tracks from an album in background (excluding the specified track)
    /// </summary>
    /// <param name="externalProvider">The provider (deezer, spotify)</param>
    /// <param name="albumExternalId">The album ID on the external provider</param>
    /// <param name="excludeTrackExternalId">The track ID to exclude (already downloaded)</param>
    void DownloadRemainingAlbumTracksInBackground(string externalProvider, string albumExternalId, string excludeTrackExternalId);
    
    /// <summary>
    /// Checks if a song is currently being downloaded
    /// </summary>
    DownloadInfo? GetDownloadStatus(string songId);
    
    /// <summary>
    /// Gets the local path for a song if it has been downloaded already
    /// </summary>
    /// <param name="externalProvider">The provider (deezer, qobuz, etc.)</param>
    /// <param name="externalId">The ID on the external provider</param>
    /// <returns>The local file path if exists, null otherwise</returns>
    Task<string?> GetLocalPathIfExistsAsync(string externalProvider, string externalId);
    
    /// <summary>
    /// Checks if the service is properly configured and functional
    /// </summary>
    Task<bool> IsAvailableAsync();
    
    /// <summary>
    /// Gets a direct stream from the provider CDN (true streaming, no disk).
    /// Returns the stream and content type for proxying to client.
    /// <paramref name="rangeHeader"/> if non-null is the verbatim HTTP Range
    /// header from the incoming client request — it gets forwarded upstream so
    /// the response can be a 206 with Content-Range. iOS Subsonic clients
    /// require Range support for non-FLAC audio or they refuse to play.
    /// </summary>
    Task<DirectStreamInfo?> GetDirectStreamAsync(
        string externalProvider,
        string externalId,
        string? rangeHeader = null,
        CancellationToken cancellationToken = default);
}

/// <summary>
/// Information about a direct stream from a provider CDN.
/// </summary>
public class DirectStreamInfo
{
    public required Stream AudioStream { get; init; }
    public required string ContentType { get; init; }
    public long? ContentLength { get; init; }
    public string? Quality { get; init; }
    /// <summary>200 for full body, 206 for partial content.</summary>
    public int StatusCode { get; init; } = 200;
    /// <summary>Verbatim Content-Range header to forward to the client (only set when StatusCode == 206).</summary>
    public string? ContentRange { get; init; }
}
