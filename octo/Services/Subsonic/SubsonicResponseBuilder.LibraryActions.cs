using Microsoft.AspNetCore.Mvc;
using Octo.Models.Settings;
using Octo.Services.Library;

namespace Octo.Services.Subsonic;

public partial class SubsonicResponseBuilder
{
    /// <summary>The OpenSubsonic extension a client checks for before it offers a library action.</summary>
    public const string LibraryActionsExtension = "octoLibraryActions";

    /// <summary>Version 2 adds the upgrade action and getUpgrades; version 3 the edits below,
    /// getLibraryTrash and the admin flag. Every older version is still listed.</summary>
    public const int LibraryActionsExtensionVersion = 3;

    /// <summary>
    /// The Delete action, named for what a person sees: the song leaves the library, and the file
    /// waits in quarantine.
    /// </summary>
    public const string RemoveAction = "remove";

    /// <summary>
    /// The Better quality action, from version 2: look for a higher quality copy and swap it in, the
    /// song keeping its place. Queued, never run inside the request.
    /// </summary>
    public const string UpgradeAction = "upgrade";

    /// <summary>Version 3: write tags into a song's file, in place.</summary>
    public const string RetagAction = "retag";

    /// <summary>Version 3: put a song on the album another song is on, by copying that album's tags.</summary>
    public const string JoinAlbumAction = "joinAlbum";

    /// <summary>Version 3: find a cover for a song's album and put it inside a file that has none.</summary>
    public const string CoverAction = "cover";

    /// <summary>Version 3: the tags a download of the song would get. Writes nothing.</summary>
    public const string LookupAction = "lookup";

    /// <summary>Version 3: put back the last retag, joinAlbum or cover of a song.</summary>
    public const string UndoAction = "undo";

    /// <summary>Version 3: put a removed song back from the trash.</summary>
    public const string RestoreAction = "restore";

    public static readonly string[] EditActions = [RetagAction, JoinAlbumAction, CoverAction, LookupAction, UndoAction, RestoreAction];

    /// <summary>
    /// getLibraryActions: what this server lets the caller do. Always JSON. The field names are a
    /// contract with the Octo app.
    /// </summary>
    public IActionResult CreateLibraryActionsResponse(LibraryActionSettings settings, string? username, int parallel = 1,
        bool upgradeReady = true, string upgradeSource = "Soulseek", bool admin = false, bool edits = false, bool covers = false) =>
        CreateJsonResponse(new Dictionary<string, object?>
        {
            ["status"] = "ok",
            ["version"] = SubsonicVersion,
            ["type"] = "octo",
            ["openSubsonic"] = true,
            ["libraryActions"] = new Dictionary<string, object?>
            {
                ["enabled"] = settings.Enabled,
                ["allowed"] = settings.IsAllowed(username),
                ["dryRun"] = settings.DryRun,
                ["actions"] = OfferedActions(settings, upgradeReady, edits, covers),
                // Whether the caller is a Navidrome admin. Removing and every edit need it.
                ["admin"] = admin,
                // 0 means kept until someone removes it by hand.
                ["keepDays"] = settings.EffectiveQuarantineRetentionDays,
                // How many upgrades run at once, which is how many downloads may.
                ["parallel"] = parallel,
                // Where an upgrade looks, for a client to say so rather than assume. Null when upgrade is not offered.
                ["upgradeSource"] = upgradeReady ? upgradeSource : null,
            },
        });

    private static string[] OfferedActions(LibraryActionSettings settings, bool upgradeReady, bool edits, bool covers)
    {
        var enabled = settings.EffectiveActions().Where(action => action.Enabled).Select(action => action.Action).ToHashSet();
        var offered = new List<string>();
        if (enabled.Contains(LibraryAction.Delete)) offered.Add(RemoveAction);
        if (enabled.Contains(LibraryAction.BetterQuality) && upgradeReady) offered.Add(UpgradeAction);
        if (edits)
        {
            offered.AddRange([RetagAction, JoinAlbumAction, LookupAction, UndoAction, RestoreAction]);
            if (covers) offered.Add(CoverAction);
        }
        return offered.ToArray();
    }

    /// <summary>
    /// libraryAction for a version 3 edit: the state, the server's words, and the song's tags
    /// before and after (null for restore), which an app shows and can send back to undo.
    /// </summary>
    public IActionResult CreateLibraryEditResponse(string songId, string action, LibraryEditOutcome outcome) =>
        CreateJsonResponse(new Dictionary<string, object?>
        {
            ["status"] = "ok",
            ["version"] = SubsonicVersion,
            ["type"] = "octo",
            ["openSubsonic"] = true,
            ["libraryAction"] = new Dictionary<string, object?>
            {
                ["id"] = songId,
                ["action"] = action,
                ["state"] = outcome.State,
                ["detail"] = outcome.Detail,
                ["before"] = outcome.Before,
                ["after"] = outcome.After,
            },
        });

    /// <summary>
    /// libraryAction?action=lookup: the file's tags now ("current") and what a download would
    /// write ("suggested", only the fields it found), with how sure the match is and where from.
    /// </summary>
    public IActionResult CreateLookupResponse(string songId, SongLookup lookup) =>
        CreateJsonResponse(new Dictionary<string, object?>
        {
            ["status"] = "ok",
            ["version"] = SubsonicVersion,
            ["type"] = "octo",
            ["openSubsonic"] = true,
            ["libraryAction"] = new Dictionary<string, object?>
            {
                ["id"] = songId,
                ["action"] = LookupAction,
                ["state"] = lookup.State,
                ["detail"] = lookup.Detail,
                ["current"] = lookup.Current,
                ["suggested"] = lookup.Suggested,
                ["confidence"] = lookup.Confidence,
                ["source"] = lookup.Source,
                ["release"] = lookup.Release,
            },
        });

    /// <summary>getLibraryTrash: removed songs still in the trash, a plain array under "trash".</summary>
    public IActionResult CreateLibraryTrashResponse(IReadOnlyList<TrashedSong> songs, int keepDays) =>
        CreateJsonResponse(new Dictionary<string, object?>
        {
            ["status"] = "ok",
            ["version"] = SubsonicVersion,
            ["type"] = "octo",
            ["openSubsonic"] = true,
            ["libraryTrash"] = new Dictionary<string, object?>
            {
                ["keepDays"] = keepDays,
                ["songs"] = songs.Select(song => new Dictionary<string, object?>
                {
                    ["id"] = song.Id,
                    ["title"] = song.Title,
                    ["artist"] = song.Artist,
                    ["album"] = song.Album,
                    ["removedBy"] = song.Username,
                    ["removedAt"] = song.RemovedUtc.ToString("O"),
                    ["goneAt"] = song.GoneUtc?.ToString("O"),
                }).ToList(),
            },
        });

    /// <summary>
    /// libraryAction: what happened to one request. Always ok, so the client reads the state
    /// rather than an error, even when nothing was done.
    /// </summary>
    public IActionResult CreateLibraryActionResponse(string songId, LibraryActionOutcome outcome, string action = RemoveAction) =>
        CreateLibraryActionResponse(songId, outcome.State.ToString().ToLowerInvariant(), outcome.Detail, action);

    /// <summary>libraryAction with a state of its own, such as "queued" for an upgrade.</summary>
    public IActionResult CreateLibraryActionResponse(string songId, string state, string? detail, string action) =>
        CreateJsonResponse(new Dictionary<string, object?>
        {
            ["status"] = "ok",
            ["version"] = SubsonicVersion,
            ["type"] = "octo",
            ["openSubsonic"] = true,
            ["libraryAction"] = new Dictionary<string, object?>
            {
                ["id"] = songId,
                ["action"] = action,
                ["state"] = state,
                ["detail"] = detail,
            },
        });

    /// <summary>
    /// getUpgrades: the caller's upgrade jobs, a plain array under "upgrades". The id is the Navidrome
    /// id that was asked for, and it stays the same after the swap. progress is 0 to 1, or null.
    /// </summary>
    public IActionResult CreateUpgradesResponse(IEnumerable<(UpgradeJob Job, double? Progress)> jobs) =>
        CreateJsonResponse(new Dictionary<string, object?>
        {
            ["status"] = "ok",
            ["version"] = SubsonicVersion,
            ["type"] = "octo",
            ["openSubsonic"] = true,
            ["upgrades"] = jobs.Select(pair => new Dictionary<string, object?>
            {
                ["id"] = pair.Job.NavidromeId,
                ["title"] = pair.Job.Title,
                ["artist"] = pair.Job.Artist,
                ["album"] = pair.Job.Album,
                ["state"] = pair.Job.State,
                ["detail"] = pair.Job.Detail,
                ["progress"] = pair.Progress,
                ["updatedAt"] = pair.Job.UpdatedUtc.ToString("O"),
                // The downloads row that fetches the replacement, once there is one, for its log.
                ["acquisition"] = pair.Job.AcquisitionKey is { } key && key.IndexOf(':') is > 0 and var split
                    ? Octo.Services.Common.AcquisitionTracker.KeyOf(key[..split], key[(split + 1)..]) : null,
                ["picked"] = pair.Job.Pick?.Describe(),
            }).ToList(),
        });
}
