using System.Net;
using Microsoft.AspNetCore.Mvc;
using Octo.Services.Admin;
using Octo.Services.Imports;

namespace Octo.Controllers;

/// <summary>
/// The dashboard's Spotify import page: connect a Spotify account, add a public link or an
/// exported file, see what the library has and what it is missing, keep a list as a playlist, and
/// the trickle that fetches the rest. Lists belong to the Navidrome user signed in on the page, the
/// same person an Octo app signed in as sees them, so it needs the dashboard's sign-in.
///
/// Also the page Spotify sends the browser back to when the redirect URI is Octo's own address.
/// </summary>
[ApiController]
public sealed class ImportsController : ControllerBase
{
    private readonly BrowseSessionStore _sessions;
    private readonly ImportService _imports;
    private readonly ILogger<ImportsController> _logger;

    public ImportsController(BrowseSessionStore sessions, ImportService imports, ILogger<ImportsController> logger)
    {
        _sessions = sessions;
        _imports = imports;
        _logger = logger;
    }

    private string? SignedIn(string? header) => _sessions.UserOf(Request.Cookies[AdminController.BrowseCookieName] ?? header);

    private IActionResult SignIn() => Unauthorized(new { error = "Sign in with your Navidrome account first." });

    private IActionResult Answer(ImportActionResult result) => result.Ok ? Ok(result) : BadRequest(result);

    public sealed record OnRequest(bool On);
    public sealed record PausedRequest(bool Paused);
    public sealed record KeysRequest(List<string>? Keys);
    public sealed record LinkRequest(string? Url);
    public sealed record FinishRequest(string? Address);

    [HttpGet("api/admin/imports")]
    public IActionResult Overview([FromHeader(Name = "X-Octo-Browse-Token")] string? token)
    {
        if (SignedIn(token) is not { } user) return SignIn();
        return Ok(new { user, overview = _imports.Overview(user) });
    }

    [HttpGet("api/admin/imports/lists/{id}")]
    public IActionResult List(string id, [FromHeader(Name = "X-Octo-Browse-Token")] string? token)
    {
        if (SignedIn(token) is not { } user) return SignIn();
        return _imports.Detail(user, id) is { } detail ? Ok(detail) : NotFound(new { error = "There is no such list." });
    }

    [HttpPost("api/admin/imports/spotify/connect")]
    public IActionResult Connect([FromHeader(Name = "X-Octo-Browse-Token")] string? token)
    {
        if (SignedIn(token) is not { } user) return SignIn();
        return Answer(_imports.BeginSpotify(user));
    }

    [HttpPost("api/admin/imports/spotify/finish")]
    public async Task<IActionResult> Finish([FromBody] FinishRequest request, [FromHeader(Name = "X-Octo-Browse-Token")] string? token)
    {
        if (SignedIn(token) is not { } user) return SignIn();
        return Answer(await _imports.FinishSpotifyAsync(request.Address ?? "", null, null, user, HttpContext.RequestAborted));
    }

    [HttpPost("api/admin/imports/spotify/disconnect")]
    public IActionResult Disconnect([FromHeader(Name = "X-Octo-Browse-Token")] string? token)
    {
        if (SignedIn(token) is not { } user) return SignIn();
        return Answer(_imports.DisconnectSpotify(user));
    }

    [HttpPost("api/admin/imports/spotify/read")]
    public IActionResult Read([FromHeader(Name = "X-Octo-Browse-Token")] string? token)
    {
        if (SignedIn(token) is not { } user) return SignIn();
        return Ok(new ImportActionResult(true, _imports.StartReading(user)
            ? "Reading your Spotify lists again." : "Octo is already reading your Spotify lists."));
    }

    [HttpPost("api/admin/imports/link")]
    public async Task<IActionResult> Link([FromBody] LinkRequest request, [FromHeader(Name = "X-Octo-Browse-Token")] string? token)
    {
        if (SignedIn(token) is not { } user) return SignIn();
        // Not tied to the request: a closed tab must not leave a list half read.
        return Answer(await _imports.AddLinkAsync(user, request.Url ?? "", CancellationToken.None));
    }

    [HttpPost("api/admin/imports/file")]
    [RequestSizeLimit(ImportFileReader.MaxBytes)]
    public async Task<IActionResult> File(IFormFile? file, [FromHeader(Name = "X-Octo-Browse-Token")] string? token)
    {
        if (SignedIn(token) is not { } user) return SignIn();
        if (file is null || file.Length == 0) return BadRequest(new ImportActionResult(false, "Choose a file first."));
        // A zip needs to seek, and the form's stream may not.
        await using var copy = new MemoryStream();
        await file.CopyToAsync(copy, HttpContext.RequestAborted);
        copy.Position = 0;
        return Answer(await _imports.AddFileAsync(user, file.FileName, copy, CancellationToken.None));
    }

    [HttpPost("api/admin/imports/lists/{id}/playlist")]
    public async Task<IActionResult> Playlist(string id, [FromBody] OnRequest request, [FromHeader(Name = "X-Octo-Browse-Token")] string? token)
    {
        if (SignedIn(token) is not { } user) return SignIn();
        return Answer(await _imports.SetPlaylistAsync(user, id, request.On, CancellationToken.None));
    }

    [HttpPost("api/admin/imports/lists/{id}/fetch")]
    public IActionResult Fetch(string id, [FromBody] OnRequest request, [FromHeader(Name = "X-Octo-Browse-Token")] string? token)
    {
        if (SignedIn(token) is not { } user) return SignIn();
        return Answer(_imports.SetGetMissing(user, id, request.On));
    }

    [HttpPost("api/admin/imports/lists/{id}/songs")]
    public IActionResult Songs(string id, [FromBody] KeysRequest request, [FromHeader(Name = "X-Octo-Browse-Token")] string? token)
    {
        if (SignedIn(token) is not { } user) return SignIn();
        return Answer(_imports.GetSongs(user, id, request.Keys ?? []));
    }

    [HttpPost("api/admin/imports/lists/{id}/refresh")]
    public async Task<IActionResult> Refresh(string id, [FromHeader(Name = "X-Octo-Browse-Token")] string? token)
    {
        if (SignedIn(token) is not { } user) return SignIn();
        return Answer(await _imports.RefreshAsync(user, id, CancellationToken.None));
    }

    [HttpDelete("api/admin/imports/lists/{id}")]
    public IActionResult Remove(string id, [FromHeader(Name = "X-Octo-Browse-Token")] string? token)
    {
        if (SignedIn(token) is not { } user) return SignIn();
        return Answer(_imports.Remove(user, id));
    }

    [HttpPost("api/admin/imports/trickle/pause")]
    public IActionResult Pause([FromBody] PausedRequest request, [FromHeader(Name = "X-Octo-Browse-Token")] string? token)
    {
        if (SignedIn(token) is not { } user) return SignIn();
        return Answer(_imports.Pause(user, request.Paused));
    }

    [HttpPost("api/admin/imports/trickle/retry")]
    public IActionResult Retry([FromBody] KeysRequest request, [FromHeader(Name = "X-Octo-Browse-Token")] string? token)
    {
        if (SignedIn(token) is not { } user) return SignIn();
        return Answer(_imports.Retry(user, request.Keys));
    }

    [HttpPost("api/admin/imports/trickle/skip")]
    public IActionResult Skip([FromBody] KeysRequest request, [FromHeader(Name = "X-Octo-Browse-Token")] string? token)
    {
        if (SignedIn(token) is not { } user) return SignIn();
        return Answer(_imports.Skip(user, request.Keys ?? []));
    }

    [HttpPost("api/admin/imports/trickle/clear")]
    public IActionResult Clear([FromHeader(Name = "X-Octo-Browse-Token")] string? token)
    {
        if (SignedIn(token) is not { } user) return SignIn();
        return Answer(_imports.ClearFinished(user));
    }

    /// <summary>
    /// Where Spotify sends the browser when the redirect URI is Octo's own address. The state says
    /// whose sign-in it is, so no dashboard sign-in is needed here, and the PKCE verifier Octo kept
    /// is what makes the code worth anything.
    /// </summary>
    [HttpGet(ImportService.CallbackPath)]
    public async Task<IActionResult> Callback([FromQuery] string? code, [FromQuery] string? state, [FromQuery] string? error)
    {
        ImportActionResult result;
        if (error is not null)
            result = new(false, error == "access_denied" ? "Spotify says access was not allowed." : $"Spotify sent back an error: {error}");
        else
            result = await _imports.FinishSpotifyAsync(null, code, state, null, CancellationToken.None);
        if (!result.Ok) _logger.LogInformation("Spotify sign-in callback refused: {Message}", result.Message);
        var title = result.Ok ? "Connected to Spotify" : "Spotify sign-in did not finish";
        var html = $$"""
            <!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
            <title>{{title}}</title><style>body{font:16px system-ui,sans-serif;background:#0d0f14;color:#e8e8ef;display:grid;place-items:center;min-height:100vh;margin:0}
            main{max-width:28rem;padding:2rem}a{color:#9db4ff}</style></head>
            <body><main><h1>{{title}}</h1><p>{{WebUtility.HtmlEncode(result.Message)}}</p>
            <p>You can close this tab and go back to Octo.</p><p><a href="/admin/#imports">Open the Spotify import page</a></p></main></body></html>
            """;
        return Content(html, "text/html");
    }
}
