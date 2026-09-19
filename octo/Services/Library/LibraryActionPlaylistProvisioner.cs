using System.Collections.Concurrent;
using Microsoft.Extensions.Options;
using Octo.Models.Settings;
using Octo.Services.Subsonic;

namespace Octo.Services.Library;

/// <summary>
/// Makes sure an allowed user has a playlist for each enabled action.
///
/// Created PER USER with THEIR credentials, so the playlists are theirs and private. An
/// admin-owned public playlist would be visible and writable to every account on the server,
/// which would hand the delete button to people who are not on the allowlist. A private
/// playlist per user is exactly the right blast radius.
/// </summary>
public sealed class LibraryActionPlaylistProvisioner
{
    /// <summary>
    /// Usernames already provisioned this process, so this runs once per user per boot rather
    /// than on every playlist listing.
    /// </summary>
    private readonly ConcurrentDictionary<string, byte> _done = new(StringComparer.OrdinalIgnoreCase);

    private readonly SubsonicProxyService _proxy;
    private readonly IOptionsMonitor<LibraryActionSettings> _settings;
    private readonly ILogger<LibraryActionPlaylistProvisioner> _logger;

    public LibraryActionPlaylistProvisioner(SubsonicProxyService proxy,
        IOptionsMonitor<LibraryActionSettings> settings,
        ILogger<LibraryActionPlaylistProvisioner> logger)
    {
        _proxy = proxy;
        _settings = settings;
        _logger = logger;
    }

    /// <summary>
    /// Create whatever is missing.
    ///
    /// <paramref name="existingNames"/> comes from the playlist listing the caller just relayed,
    /// so this costs no extra request in the common case where everything already exists.
    /// </summary>
    public async Task EnsureAsync(string username, IReadOnlyCollection<string> existingNames,
        IDictionary<string, string> authParameters)
    {
        var settings = _settings.CurrentValue;
        if (!settings.Enabled || !settings.PlaylistsEnabled) return;
        if (!settings.IsAllowed(username)) return;
        if (!_done.TryAdd(username, 0)) return;

        var wanted = settings.EffectiveActions()
            .Where(action => action.Enabled)
            .Select(settings.PlaylistTitle)
            .Where(title => !existingNames.Contains(title, StringComparer.OrdinalIgnoreCase))
            .ToList();
        if (wanted.Count == 0) return;

        foreach (var title in wanted)
        {
            try
            {
                // The user's own credentials, so the playlist belongs to them. Drop anything
                // that would make this look like an edit of an existing playlist.
                var parameters = new Dictionary<string, string>(authParameters, StringComparer.OrdinalIgnoreCase);
                parameters.Remove("id");
                parameters.Remove("playlistId");
                parameters.Remove("songId");
                parameters["name"] = title;

                var result = await _proxy.RelaySafeAsync("rest/createPlaylist", parameters);
                if (result.Success) _logger.LogInformation("Created action playlist '{Title}' for {User}", title, username);
                else _logger.LogWarning("Could not create action playlist '{Title}' for {User}", title, username);
            }
            catch (Exception ex)
            {
                // Never throw into the playlist listing: a failure here costs a playlist, not
                // the user's ability to see their own.
                _logger.LogWarning("Could not create action playlist '{Title}' for {User}: {M}",
                    title, username, ex.Message);
            }
        }
    }

    /// <summary>Forget the once-per-boot marker, so a settings change re-provisions.</summary>
    public void Reset() => _done.Clear();
}
