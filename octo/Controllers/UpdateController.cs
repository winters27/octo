using Microsoft.AspNetCore.Mvc;
using Octo.Services.Admin;
using Octo.Services.Updates;

namespace Octo.Controllers;

/// <summary>
/// The dashboard's update card: whether a newer release is out, and Update now, which hands the
/// update to the host helper (scripts/updater). Without the helper the card shows the command.
///
/// Behind the same guard as the rest of /api/admin: writes need X-Octo-Admin. Anyone who can use
/// the dashboard can already restart Octo; this only adds moving it to the newest published
/// release of the configured repo, which the helper checks again on its side.
/// </summary>
[ApiController]
[Route("api/admin/update")]
public class UpdateController(ReleaseCheck releases, UpdateHost host, BrowseSessionStore? sessions = null) : ControllerBase
{
    public sealed record UpdateRequest(string? Tag);

    [HttpGet]
    public IActionResult Get() => Ok(View(releases.View()));

    /// <summary>Ask GitHub now. At most once a minute; sooner returns the last answer.</summary>
    [HttpPost("check")]
    public async Task<IActionResult> Check(CancellationToken ct) => Ok(View(await releases.CheckAsync(manual: true, ct)));

    /// <summary>Update to the newest release, through the host helper.</summary>
    [HttpPost]
    public IActionResult Update([FromBody] UpdateRequest request)
    {
        var view = releases.View();
        if (!view.Enabled)
            return Conflict(new { error = "Update checks are off, so Octo does not know which release is newest." });
        if (view.Latest is null || !view.UpdateAvailable)
            return Conflict(new { error = "There is no newer release to update to." });
        if (!string.Equals(request.Tag, view.Latest.Tag, StringComparison.Ordinal))
            return Conflict(new { error = $"Only the newest release, {view.Latest.Tag}, can be installed from here." });
        if (host.Helper() is null)
            return Conflict(new { error = "The update helper is not installed on this server's host. Run the command shown instead." });
        if (host.Busy())
            return Conflict(new { error = "An update is already under way." });

        // Who asked, for the update log: a script signed in with a cookie jar or a header counts too.
        var user = AdminCaller.Of(HttpContext) is not null
            ? AdminCaller.Describe(HttpContext)
            : sessions?.UserOf(Request.Cookies[AdminController.BrowseCookieName]) ?? "dashboard";
        var id = host.Request(view.Latest.Tag, user);
        return Accepted(new { ok = true, id, tag = view.Latest.Tag });
    }

    private object View(ReleaseCheckView view)
    {
        var helper = host.Helper();
        var pending = host.Pending();
        var status = host.Status();
        var run = status is null ? null : Describe(status, view.Running);
        var tag = view.Latest?.Tag ?? "<release>";
        return new
        {
            enabled = view.Enabled,
            repo = view.Repo,
            running = view.Running,
            latest = view.Latest,
            newer = view.Newer,
            updateAvailable = view.UpdateAvailable,
            standing = view.Standing,
            checkedUtc = view.CheckedUtc,
            error = view.Error,
            helper = helper is null
                ? (object)new { installed = false }
                : new { installed = true, helper.Version, helper.Mode, helper.Dir, helper.InstalledUtc },
            pending = pending is not null,
            pendingId = pending,
            unanswered = host.Unanswered,
            run,
            // What to run by hand, from the Octo folder: the built-from-source install, and the image one.
            command = $"git fetch --tags && git checkout --detach {tag} && docker compose build && docker compose up -d",
            imageCommand = "docker compose pull octo && docker compose up -d octo",
        };
    }

    private object Describe(UpdateRunStatus status, string running)
    {
        var state = status.State;
        // The helper's last word before Octo restarted was "restarting"; Octo being back on that
        // release is the proof it finished.
        if (state == UpdateRunStates.Restarting && status.Tag is not null
            && string.Equals(status.Tag, running, StringComparison.Ordinal))
            state = UpdateRunStates.Done;
        string? error = status.Error;
        if (UpdateRunStates.Running(state) && DateTime.UtcNow - (status.StartedUtc ?? DateTime.MinValue) > UpdateHost.RunTimeout)
        {
            state = UpdateRunStates.Failed;
            error ??= "The update helper stopped reporting. Its log on the host says why.";
        }
        return new
        {
            status.Id, status.Tag, status.From, state, status.Step, error,
            status.StartedUtc, status.FinishedUtc,
            log = state == UpdateRunStates.Failed ? host.LogTail() : [],
        };
    }
}
