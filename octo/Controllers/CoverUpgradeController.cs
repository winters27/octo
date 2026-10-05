using Microsoft.AspNetCore.Mvc;
using Octo.Services.Admin;
using Octo.Services.CoverArt;
using Octo.Services.Subsonic;

namespace Octo.Controllers;

/// <summary>
/// The dashboard's "Upgrade cover art": find the largest cover for every album and embed it.
/// It rewrites the owner's files, so it needs a Navidrome admin sign-in like the genre backfill,
/// and a whole-library run (not a preview) needs the music path typed back.
/// </summary>
[ApiController]
[Route("api/admin/covers/upgrade")]
public sealed class CoverUpgradeController : ControllerBase
{
    private readonly BrowseSessionStore _sessions;
    private readonly CoverUpgradeWorker _worker;
    private readonly NavidromeIdentityService _identity;
    private readonly IConfiguration _config;
    private readonly ILogger<CoverUpgradeController> _logger;

    public CoverUpgradeController(BrowseSessionStore sessions, CoverUpgradeWorker worker,
        NavidromeIdentityService identity, IConfiguration config, ILogger<CoverUpgradeController> logger)
    {
        _sessions = sessions;
        _worker = worker;
        _identity = identity;
        _config = config;
        _logger = logger;
    }

    private bool Signed(string? header) =>
        _sessions.NavidromeUserOf(Request.Cookies[AdminController.BrowseCookieName] ?? header) is not null;

    private IActionResult SignIn() => Unauthorized(new { error = "Sign in with your Navidrome admin account first." });

    private string MusicPath()
    {
        var fallback = _config["Library:DownloadPath"] ?? "/music";
        return _identity.EffectiveDownloadPath(fallback);
    }

    public sealed record StartRequest(string? Scope, string? Mode, bool FolderCovers = true,
        int SmallerThan = CoverUpgradeWorker.DefaultSmallerThan, List<string>? Albums = null, string? Confirm = null);

    [HttpGet]
    public IActionResult Get([FromHeader(Name = "X-Octo-Browse-Token")] string? token)
    {
        if (!Signed(token)) return SignIn();
        var run = _worker.Current;
        return Ok(new
        {
            run.RunId,
            status = run.Status.ToString(),
            scope = run.Scope.ToString(),
            mode = run.Mode.ToString(),
            run.DryRun,
            run.SmallerThan,
            picked = run.Selected?.Count,
            run.Soft,
            run.FolderCovers,
            run.FullSize,
            run.Undo,
            run.StartedUtc,
            run.FinishedUtc,
            run.Total,
            run.Processed,
            run.Upgraded,
            run.Kept,
            run.Files,
            run.Failed,
            run.LastFolder,
            run.Reason,
            run.SongsTotal,
            run.SongsRead,
            run.AlbumsTotal,
            run.AlbumsDone,
            run.Errors,
            // Without each album's file list, which only the next run needs.
            preview = run.Preview.Select(row => new
            {
                row.Id, row.Folder, row.Artist, row.Album, row.FromSide, row.ToSide, row.Source, row.Files,
                row.FolderCover, row.Result, row.LooksSame,
            }),
            canResume = run.CanResume,
            // Accepted and not yet started, or started: the dashboard watches while this is true.
            busy = _worker.IsBusy,
            canUndo = _worker.CanUndo,
            musicPath = MusicPath(),
        });
    }

    [HttpPost]
    public IActionResult Start([FromBody] StartRequest request, [FromHeader(Name = "X-Octo-Browse-Token")] string? token)
    {
        if (!Signed(token)) return SignIn();
        if (!Enum.TryParse<CoverUpgradeScope>(request.Scope, ignoreCase: true, out var scope))
            scope = CoverUpgradeScope.OctoDownloads;
        if (!Enum.TryParse<CoverUpgradeMode>(request.Mode, ignoreCase: true, out var mode))
            mode = CoverUpgradeMode.Scan;
        var smallerThan = Math.Clamp(request.SmallerThan, 1, 10_000);
        if (request.Albums is { Count: 0 })
            return BadRequest(new { error = "Pick at least one album." });

        // Picked albums came off a list the admin was shown; a run over everything in the whole
        // library is the one that needs the path typed back.
        var root = MusicPath();
        if (scope == CoverUpgradeScope.WholeLibrary && mode == CoverUpgradeMode.Apply && request.Albums is null
            && !string.Equals(request.Confirm?.Trim(), root, StringComparison.Ordinal))
            return BadRequest(new { error = $"To rewrite the whole library, type the music path exactly: {root}" });

        if (!_worker.TryEnqueue(new CoverUpgradeRequest(scope, mode, request.FolderCovers, smallerThan, request.Albums)))
            return Conflict(new { error = "A cover upgrade is already running." });
        _logger.LogInformation("Cover upgrade requested: {Mode}, scope {Scope}, under {Side} px, folder covers {Folder}, {Picked}",
            mode, scope, smallerThan, request.FolderCovers,
            request.Albums is null ? "every album" : $"{request.Albums.Count} picked");
        return Accepted(new { started = true });
    }

    [HttpPost("cancel")]
    public IActionResult Cancel([FromHeader(Name = "X-Octo-Browse-Token")] string? token)
    {
        if (!Signed(token)) return SignIn();
        _worker.RequestCancel();
        return Accepted(new { cancelling = true });
    }

    [HttpPost("resume")]
    public IActionResult Resume([FromHeader(Name = "X-Octo-Browse-Token")] string? token)
    {
        if (!Signed(token)) return SignIn();
        var run = _worker.Current;
        if (!run.CanResume) return BadRequest(new { error = "There is nothing to resume." });
        if (!_worker.TryEnqueue(new CoverUpgradeRequest(run.Scope, run.Mode, run.FolderCovers, run.SmallerThan, run.Selected)))
            return Conflict(new { error = "A cover upgrade is already running." });
        return Accepted(new { resumed = true });
    }

    /// <summary>The cover an album on the list has now, small, so the soft ones can be seen; with
    /// <c>found=true</c>, the larger one a preview found for it.</summary>
    [HttpGet("thumb/{id}")]
    public async Task<IActionResult> Thumb(string id, [FromQuery] bool found,
        [FromHeader(Name = "X-Octo-Browse-Token")] string? token, CancellationToken ct)
    {
        if (!Signed(token)) return SignIn();
        Response.Headers.CacheControl = "private, max-age=300";
        // The cover Navidrome shows, already made at the apps' size; reading the song is the
        // fallback, and over a network mount it is the slow one.
        if (!found && await _worker.NavidromeThumbnailAsync(id, ct) is { } served)
            return File(served.Bytes, served.Type);
        var bytes = found ? _worker.FoundThumbnail(id) : _worker.Thumbnail(id);
        if (bytes is null) return NotFound();
        return File(bytes, CoverImage.MimeType(bytes));
    }

    [HttpPost("undo")]
    public IActionResult Undo([FromHeader(Name = "X-Octo-Browse-Token")] string? token)
    {
        if (!Signed(token)) return SignIn();
        if (!_worker.CanUndo) return BadRequest(new { error = "There is no cover upgrade to undo." });
        if (!_worker.TryEnqueue(new CoverUpgradeRequest(CoverUpgradeScope.WholeLibrary, CoverUpgradeMode.Apply, FolderCovers: true, Undo: true)))
            return Conflict(new { error = "A cover upgrade is already running." });
        _logger.LogInformation("Cover upgrade undo requested");
        return Accepted(new { started = true });
    }
}
