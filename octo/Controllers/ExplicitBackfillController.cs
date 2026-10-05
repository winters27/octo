using Microsoft.AspNetCore.Mvc;
using Octo.Services.Admin;
using Octo.Services.Library;

namespace Octo.Controllers;

/// <summary>
/// The dashboard's "Mark explicit songs": look the library up, then write the explicit and
/// clean marks the preview found, and undo them. It rewrites the owner's files, so every call
/// needs a Navidrome admin sign-in like the genre re-tag, and Apply needs the music path typed.
/// </summary>
[ApiController]
[Route("api/admin/explicit/backfill")]
public sealed class ExplicitBackfillController : ControllerBase
{
    private readonly BrowseSessionStore _sessions;
    private readonly ExplicitBackfill _worker;
    private readonly ILogger<ExplicitBackfillController> _logger;

    /// <summary>How many rows a status answer carries; the counts cover them all.</summary>
    internal const int RowsShown = 400;

    public ExplicitBackfillController(BrowseSessionStore sessions, ExplicitBackfill worker, ILogger<ExplicitBackfillController> logger)
    {
        _sessions = sessions;
        _worker = worker;
        _logger = logger;
    }

    private string? User(string? header) => _sessions.UserOf(Request.Cookies[AdminController.BrowseCookieName] ?? header);
    private IActionResult SignIn() => Unauthorized(new { error = "Sign in with your Navidrome admin account first." });
    private IActionResult Busy() => Conflict(new { error = "Explicit marking is already running." });

    public sealed record ApplyRequest(string? Confirm);

    [HttpGet]
    public IActionResult Get([FromHeader(Name = "X-Octo-Browse-Token")] string? token, [FromQuery] string? show = null)
    {
        if (User(token) is null) return SignIn();
        var run = _worker.Current;
        // Explicit and clean first (what Apply writes), then unsure (why nothing was written),
        // then not explicit; "show" narrows it to one kind.
        static int Order(string outcome) => outcome switch { "explicit" => 0, "clean" => 1, "unsure" => 2, _ => 3 };
        var rows = run.Rows
            .Where(row => string.IsNullOrEmpty(show) || row.Outcome == show)
            .OrderBy(row => Order(row.Outcome)).ThenBy(row => row.Path, StringComparer.Ordinal)
            .Take(RowsShown)
            .Select(row => new { row.Path, row.Artist, row.Title, row.Outcome, row.How, row.Written })
            .ToList();
        return Ok(new
        {
            run.RunId,
            status = run.Status.ToString(),
            mode = run.Mode.ToString(),
            run.StartedUtc,
            run.FinishedUtc,
            run.Total,
            run.Processed,
            run.Explicit,
            run.Clean,
            run.NotExplicit,
            run.Unsure,
            run.AlreadyMarked,
            run.Failed,
            run.StepTotal,
            run.StepDone,
            run.Written,
            run.LeftAlone,
            run.LastSong,
            run.Reason,
            run.Errors,
            toWrite = run.Rows.Count(row => !row.Written && ExplicitBackfill.Writes(row.Outcome)),
            rows,
            canResume = run.CanResume,
            canApply = run.CanApply,
            canUndo = _worker.CanUndo && run.Status != Octo.Services.Library.ExplicitBackfillStatus.Running,
            musicPath = _worker.MusicPath(),
        });
    }

    /// <summary>Look every unmarked song up. Writes nothing.</summary>
    [HttpPost("preview")]
    public IActionResult Preview([FromHeader(Name = "X-Octo-Browse-Token")] string? token)
    {
        if (User(token) is not { } user) return SignIn();
        if (!_worker.TryEnqueue(new ExplicitBackfillRequest(ExplicitBackfillMode.Preview, user))) return Busy();
        _logger.LogInformation("Explicit marking preview requested by {User}", user);
        return Accepted(new { started = true });
    }

    /// <summary>Carry on a stopped or interrupted preview where it was.</summary>
    [HttpPost("resume")]
    public IActionResult Resume([FromHeader(Name = "X-Octo-Browse-Token")] string? token)
    {
        if (User(token) is not { } user) return SignIn();
        if (!_worker.Current.CanResume) return BadRequest(new { error = "There is nothing to resume." });
        if (!_worker.TryEnqueue(new ExplicitBackfillRequest(ExplicitBackfillMode.Preview, user))) return Busy();
        return Accepted(new { resumed = true });
    }

    /// <summary>Write what the finished preview found explicit or clean.</summary>
    [HttpPost("apply")]
    public IActionResult Apply([FromBody] ApplyRequest request, [FromHeader(Name = "X-Octo-Browse-Token")] string? token)
    {
        if (User(token) is not { } user) return SignIn();
        if (!_worker.Current.CanApply) return BadRequest(new { error = "Preview first: nothing found is waiting to be written." });
        var root = _worker.MusicPath();
        if (!string.Equals(request.Confirm?.Trim(), root, StringComparison.Ordinal))
            return BadRequest(new { error = $"To mark songs in your library, type the music path exactly: {root}" });
        if (!_worker.TryEnqueue(new ExplicitBackfillRequest(ExplicitBackfillMode.Apply, user))) return Busy();
        _logger.LogInformation("Explicit marking apply requested by {User}", user);
        return Accepted(new { started = true });
    }

    [HttpPost("cancel")]
    public IActionResult Cancel([FromHeader(Name = "X-Octo-Browse-Token")] string? token)
    {
        if (User(token) is null) return SignIn();
        _worker.RequestCancel();
        return Accepted(new { cancelling = true });
    }

    /// <summary>Take out every mark the last Apply wrote, where the file has not changed since.</summary>
    [HttpPost("undo")]
    public IActionResult Undo([FromHeader(Name = "X-Octo-Browse-Token")] string? token)
    {
        if (User(token) is not { } user) return SignIn();
        if (!_worker.CanUndo) return BadRequest(new { error = "There is no marking to undo." });
        if (!_worker.TryEnqueue(new ExplicitBackfillRequest(ExplicitBackfillMode.Undo, user))) return Busy();
        _logger.LogInformation("Explicit marking undo requested by {User}", user);
        return Accepted(new { started = true });
    }
}
