using Microsoft.AspNetCore.Mvc;
using Octo.Models.Settings;
using Octo.Services.Library;

namespace Octo.Services.Subsonic;

public partial class SubsonicResponseBuilder
{
    /// <summary>The OpenSubsonic extension a client checks for before it offers a library action.</summary>
    public const string LibraryActionsExtension = "octoLibraryActions";

    /// <summary>Version 2 adds the upgrade action and getUpgrades; version 1 is still listed.</summary>
    public const int LibraryActionsExtensionVersion = 2;

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

    /// <summary>
    /// getLibraryActions: what this server lets the caller do. Always JSON. The field names are a
    /// contract with the Octo app.
    /// </summary>
    public IActionResult CreateLibraryActionsResponse(LibraryActionSettings settings, string? username, int parallel = 1,
        bool upgradeReady = true, string upgradeSource = "Soulseek") =>
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
                ["actions"] = OfferedActions(settings, upgradeReady),
                // 0 means kept until someone removes it by hand.
                ["keepDays"] = settings.EffectiveQuarantineRetentionDays,
                // How many upgrades run at once, which is how many downloads may.
                ["parallel"] = parallel,
                // Where an upgrade looks, for a client to say so rather than assume. Null when upgrade is not offered.
                ["upgradeSource"] = upgradeReady ? upgradeSource : null,
            },
        });

    private static string[] OfferedActions(LibraryActionSettings settings, bool upgradeReady)
    {
        var enabled = settings.EffectiveActions().Where(action => action.Enabled).Select(action => action.Action).ToHashSet();
        var offered = new List<string>();
        if (enabled.Contains(LibraryAction.Delete)) offered.Add(RemoveAction);
        if (enabled.Contains(LibraryAction.BetterQuality) && upgradeReady) offered.Add(UpgradeAction);
        return offered.ToArray();
    }

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
