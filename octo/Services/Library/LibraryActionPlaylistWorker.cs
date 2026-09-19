using System.Text.Json;
using Microsoft.Extensions.Options;
using Octo.Models.Settings;
using Octo.Services.Local;
using Octo.Services.Subsonic;

namespace Octo.Services.Library;

/// <summary>
/// Watches the action playlists and applies what people put in them.
///
/// A BackgroundService rather than a request hook, because applying an action can involve a
/// download and nothing that removes a file should run inside a request.
/// </summary>
public sealed class LibraryActionPlaylistWorker : BackgroundService
{
    private readonly LibraryActionExecutor _executor;
    private readonly LibraryActionJournal _journal;
    private readonly LibraryActionQuarantine _quarantine;
    private readonly NavidromeSongPathResolver _resolver;
    private readonly NavidromeIdentityService _identity;
    private readonly IHttpClientFactory _http;
    private readonly IOptionsMonitor<LibraryActionSettings> _settings;
    private readonly IOptionsMonitor<SubsonicSettings> _subsonic;
    private readonly IServiceScopeFactory _scopes;
    private readonly ILogger<LibraryActionPlaylistWorker> _logger;

    public LibraryActionPlaylistWorker(LibraryActionExecutor executor, LibraryActionJournal journal,
        LibraryActionQuarantine quarantine, NavidromeSongPathResolver resolver,
        NavidromeIdentityService identity, IHttpClientFactory http,
        IOptionsMonitor<LibraryActionSettings> settings, IOptionsMonitor<SubsonicSettings> subsonic,
        IServiceScopeFactory scopes, ILogger<LibraryActionPlaylistWorker> logger)
    {
        _executor = executor;
        _journal = journal;
        _quarantine = quarantine;
        _resolver = resolver;
        _identity = identity;
        _http = http;
        _settings = settings;
        _subsonic = subsonic;
        _scopes = scopes;
        _logger = logger;
    }

    protected override async Task ExecuteAsync(CancellationToken stoppingToken)
    {
        var settings = _settings.CurrentValue;

        // Feature-gated by early return, which leaves the service registered but idle. Turning
        // file removal on is a deliberate act, so it is not worth a restart-free toggle.
        if (!settings.Enabled || !settings.PlaylistsEnabled)
        {
            _logger.LogInformation("Library actions are off; the playlist worker is idle");
            return;
        }

        if (!_identity.HasAdminIdentity)
        {
            // One clear line at startup rather than one silent failure per action.
            _logger.LogWarning(
                "Library actions are enabled but Octo has no Navidrome admin credential. Set "
                + "Subsonic:AdminUsername and AdminPassword, or sign in through Octo once as a "
                + "Navidrome admin. Until then no action can find its file, so none will run.");
            return;
        }

        // Decide what half-finished actions meant before doing anything new.
        _journal.Reconcile();

        using var timer = new PeriodicTimer(settings.EffectivePollInterval);
        while (!stoppingToken.IsCancellationRequested)
        {
            try { await SweepAsync(stoppingToken); }
            catch (OperationCanceledException) { break; }
            catch (Exception ex) { _logger.LogError(ex, "Library action sweep failed"); }

            try { if (!await timer.WaitForNextTickAsync(stoppingToken)) break; }
            catch (OperationCanceledException) { break; }
        }
    }

    private async Task SweepAsync(CancellationToken ct)
    {
        var settings = _settings.CurrentValue;
        if (!settings.Enabled || !settings.PlaylistsEnabled) return;

        var jwt = await _identity.EnsureAdminJwtAsync(ct);
        if (string.IsNullOrEmpty(jwt)) return;

        var wanted = settings.EffectiveActions()
            .Where(action => action.Enabled)
            .ToDictionary(settings.PlaylistTitle, action => action, StringComparer.OrdinalIgnoreCase);
        if (wanted.Count == 0) return;

        var budget = settings.EffectiveMaxActionsPerCycle;

        foreach (var playlist in await ListPlaylistsAsync(jwt, ct))
        {
            if (budget <= 0) break;
            if (!wanted.TryGetValue(playlist.Name, out var action)) continue;

            // The owner comes from Navidrome's own record, not from a request parameter. That
            // is the strongest form of the allowlist check available.
            if (!settings.IsAllowed(playlist.Owner))
            {
                _logger.LogDebug("Skipping '{Playlist}': {Owner} is not on the allowlist",
                    playlist.Name, playlist.Owner);
                continue;
            }

            var applied = await ApplyPlaylistAsync(playlist, action.Action, jwt, budget, ct);
            budget -= applied;
        }

        _quarantine.Sweep(_resolver.MusicRoot());
    }

    private async Task<int> ApplyPlaylistAsync(PlaylistRow playlist, LibraryAction action,
        string jwt, int budget, CancellationToken ct)
    {
        var tracks = await ListTracksAsync(playlist.Id, jwt, ct);
        if (tracks.Count == 0) return 0;

        var consumed = new List<string>();
        var applied = 0;

        foreach (var track in tracks.Take(budget))
        {
            if (ct.IsCancellationRequested) break;

            // Per-item catch is mandatory: BackgroundServiceExceptionBehavior defaults to
            // StopHost, so one unhandled exception here would take Octo down.
            try
            {
                var outcome = await _executor.ApplyAsync(
                    new LibraryActionRequest(action, track.MediaFileId, playlist.Owner), ct);

                _logger.LogInformation("Library action {Action} for {Id} by {User}: {State} - {Detail}",
                    action, track.MediaFileId, playlist.Owner, outcome.State, outcome.Detail);

                if (outcome.Consumed) consumed.Add(track.MediaFileId);
                applied++;
            }
            catch (Exception ex)
            {
                _logger.LogError(ex, "Library action {Action} threw for {Id}", action, track.MediaFileId);
            }
        }

        if (consumed.Count > 0) await RemoveTracksAsync(playlist.Id, consumed, jwt, ct);
        return applied;
    }

    /// <summary>
    /// Remove the tracks whose action landed.
    ///
    /// PlaylistTrack.ID is the 1-based POSITION, reassigned on every mutation, so the list is
    /// re-read immediately before deleting and positions are mapped from the media file ids that
    /// were actually applied. A track the user removed in the meantime is simply not there and
    /// is skipped rather than deleting whatever now sits at its old position. The delete is one
    /// bulk call because two sequential single deletes renumber between them.
    /// </summary>
    private async Task RemoveTracksAsync(string playlistId, List<string> mediaFileIds, string jwt,
        CancellationToken ct)
    {
        try
        {
            var current = await ListTracksAsync(playlistId, jwt, ct);
            var positions = current
                .Where(track => mediaFileIds.Contains(track.MediaFileId, StringComparer.Ordinal))
                .Select(track => track.Position)
                .Where(position => !string.IsNullOrEmpty(position))
                .ToList();
            if (positions.Count == 0) return;

            var baseUrl = _subsonic.CurrentValue.Url!.TrimEnd('/');
            var query = string.Join('&', positions.Select(p => $"id={Uri.EscapeDataString(p)}"));
            using var request = new HttpRequestMessage(HttpMethod.Delete,
                $"{baseUrl}/api/playlist/{Uri.EscapeDataString(playlistId)}/tracks?{query}");
            request.Headers.TryAddWithoutValidation("X-Nd-Authorization", $"Bearer {jwt}");

            using var response = await _http.CreateClient().SendAsync(request, ct);
            if (!response.IsSuccessStatusCode)
                _logger.LogWarning(
                    "Could not clear {Count} applied track(s) from playlist {Id}: HTTP {Status}. "
                    + "They stay put; the journal stops the action running twice.",
                    positions.Count, playlistId, (int)response.StatusCode);
        }
        catch (Exception ex)
        {
            _logger.LogWarning("Could not clear applied tracks from playlist {Id}: {M}", playlistId, ex.Message);
        }
    }

    internal sealed record PlaylistRow(string Id, string Name, string Owner);
    internal sealed record PlaylistTrackRow(string Position, string MediaFileId);

    private async Task<IReadOnlyList<PlaylistRow>> ListPlaylistsAsync(string jwt, CancellationToken ct)
    {
        try
        {
            var baseUrl = _subsonic.CurrentValue.Url!.TrimEnd('/');
            using var request = new HttpRequestMessage(HttpMethod.Get,
                $"{baseUrl}/api/playlist?_start=0&_end=1000");
            request.Headers.TryAddWithoutValidation("X-Nd-Authorization", $"Bearer {jwt}");

            using var response = await _http.CreateClient().SendAsync(request, ct);
            if (!response.IsSuccessStatusCode) return [];

            using var doc = JsonDocument.Parse(await response.Content.ReadAsByteArrayAsync(ct));
            return ParsePlaylists(doc.RootElement);
        }
        catch (Exception ex)
        {
            _logger.LogWarning("Could not list playlists: {M}", ex.Message);
            return [];
        }
    }

    internal static IReadOnlyList<PlaylistRow> ParsePlaylists(JsonElement root)
    {
        if (root.ValueKind != JsonValueKind.Array) return [];

        var rows = new List<PlaylistRow>();
        foreach (var item in root.EnumerateArray())
        {
            var id = Str(item, "id");
            var name = Str(item, "name");
            if (string.IsNullOrEmpty(id) || string.IsNullOrEmpty(name)) continue;
            rows.Add(new PlaylistRow(id, name, Str(item, "ownerName") ?? ""));
        }
        return rows;
    }

    private async Task<IReadOnlyList<PlaylistTrackRow>> ListTracksAsync(string playlistId, string jwt,
        CancellationToken ct)
    {
        try
        {
            var baseUrl = _subsonic.CurrentValue.Url!.TrimEnd('/');
            using var request = new HttpRequestMessage(HttpMethod.Get,
                $"{baseUrl}/api/playlist/{Uri.EscapeDataString(playlistId)}/tracks?_start=0&_end=500");
            request.Headers.TryAddWithoutValidation("X-Nd-Authorization", $"Bearer {jwt}");

            using var response = await _http.CreateClient().SendAsync(request, ct);
            if (!response.IsSuccessStatusCode) return [];

            using var doc = JsonDocument.Parse(await response.Content.ReadAsByteArrayAsync(ct));
            return ParseTracks(doc.RootElement);
        }
        catch (Exception ex)
        {
            _logger.LogWarning("Could not list tracks in playlist {Id}: {M}", playlistId, ex.Message);
            return [];
        }
    }

    internal static IReadOnlyList<PlaylistTrackRow> ParseTracks(JsonElement root)
    {
        if (root.ValueKind != JsonValueKind.Array) return [];

        var rows = new List<PlaylistTrackRow>();
        foreach (var item in root.EnumerateArray())
        {
            var mediaFileId = Str(item, "mediaFileId");
            if (string.IsNullOrEmpty(mediaFileId)) continue;
            rows.Add(new PlaylistTrackRow(Str(item, "id") ?? "", mediaFileId));
        }
        return rows;
    }

    private static string? Str(JsonElement element, string name) =>
        element.TryGetProperty(name, out var value)
            ? value.ValueKind switch
            {
                JsonValueKind.String => value.GetString(),
                JsonValueKind.Number => value.ToString(),
                _ => null,
            }
            : null;
}
