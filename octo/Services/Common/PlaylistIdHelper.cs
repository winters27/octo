namespace Octo.Services.Common;

/// <summary>
/// Helper class for handling external playlist IDs.
/// Playlist IDs use the format: "pl-{provider}-{externalId}"
/// Example: "pl-deezer-123456", "pl-qobuz-789"
/// </summary>
public static class PlaylistIdHelper
{
    private const string PlaylistPrefix = "pl-";

    /// <summary>
    /// The providers Octo can actually resolve a playlist from.
    ///
    /// Single source of truth: PlaylistSyncService asks here before mapping a provider onto
    /// a metadata service, so there is one list rather than two that have to agree.
    /// </summary>
    private static readonly HashSet<string> KnownProviders =
        new(StringComparer.OrdinalIgnoreCase) { "deezer", "qobuz" };

    /// <summary>
    /// Checks whether a provider name is one we have a client for.
    /// </summary>
    /// <param name="provider">The provider name, e.g. "deezer"</param>
    /// <returns>True if Octo can resolve playlists from this provider</returns>
    public static bool IsKnownProvider(string? provider)
    {
        return !string.IsNullOrEmpty(provider) && KnownProviders.Contains(provider);
    }

    /// <summary>
    /// Checks if an ID represents an external playlist.
    ///
    /// The provider has to be one we recognise, not merely present. Navidrome names its own
    /// playlist cover art "pl-{id}_{unixhex}", which carries the same "pl-" prefix, so
    /// matching on the prefix alone claimed every Navidrome playlist cover, failed to parse
    /// a provider out of it, and served Octo's placeholder instead of relaying the request
    /// upstream (issue #43).
    ///
    /// Requiring a KNOWN provider rather than a second dash keeps that fixed whatever
    /// Navidrome decides its ids look like. A shape-only check would work today only because
    /// Navidrome ids happen to be base62 with no dashes, which is not ours to rely on.
    /// </summary>
    /// <param name="id">The ID to check</param>
    /// <returns>True if the ID is "pl-{knownProvider}-{externalId}", false otherwise</returns>
    public static bool IsExternalPlaylist(string? id)
    {
        return TryParsePlaylistId(id, out _, out _);
    }

    /// <summary>
    /// Parses a playlist ID to extract provider and external ID.
    /// </summary>
    /// <param name="id">The playlist ID in format "pl-{provider}-{externalId}"</param>
    /// <returns>A tuple containing (provider, externalId)</returns>
    /// <exception cref="ArgumentException">Thrown if the ID format is invalid</exception>
    public static (string provider, string externalId) ParsePlaylistId(string id)
    {
        if (!TryParsePlaylistId(id, out var provider, out var externalId))
        {
            throw new ArgumentException($"Invalid playlist ID format. Expected 'pl-{{provider}}-{{externalId}}', got '{id}'", nameof(id));
        }

        return (provider, externalId);
    }

    /// <summary>
    /// The one place the format is decided, so the predicate and the parser can never
    /// disagree about what counts as an external playlist ID.
    /// </summary>
    private static bool TryParsePlaylistId(string? id, out string provider, out string externalId)
    {
        provider = string.Empty;
        externalId = string.Empty;

        if (string.IsNullOrEmpty(id) || !id.StartsWith(PlaylistPrefix, StringComparison.OrdinalIgnoreCase))
        {
            return false;
        }

        // Remove "pl-" prefix
        var withoutPrefix = id.Substring(PlaylistPrefix.Length);

        // Split by first dash to get provider and externalId
        var dashIndex = withoutPrefix.IndexOf('-');
        if (dashIndex <= 0 || dashIndex == withoutPrefix.Length - 1)
        {
            return false;
        }

        var candidateProvider = withoutPrefix.Substring(0, dashIndex);
        if (!IsKnownProvider(candidateProvider))
        {
            return false;
        }

        provider = candidateProvider;
        externalId = withoutPrefix.Substring(dashIndex + 1);
        return true;
    }

    /// <summary>
    /// Creates a playlist ID from provider and external ID.
    /// </summary>
    /// <param name="provider">The provider name (e.g., "deezer", "qobuz")</param>
    /// <param name="externalId">The external ID from the provider</param>
    /// <returns>A playlist ID in format "pl-{provider}-{externalId}"</returns>
    public static string CreatePlaylistId(string provider, string externalId)
    {
        if (string.IsNullOrEmpty(provider))
        {
            throw new ArgumentException("Provider cannot be null or empty", nameof(provider));
        }

        if (string.IsNullOrEmpty(externalId))
        {
            throw new ArgumentException("External ID cannot be null or empty", nameof(externalId));
        }

        return $"{PlaylistPrefix}{provider.ToLowerInvariant()}-{externalId}";
    }
}
