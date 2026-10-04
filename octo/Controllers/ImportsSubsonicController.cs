using Microsoft.AspNetCore.Mvc;
using Octo.Services.Imports;
using Octo.Services.Subsonic;

namespace Octo.Controllers;

/// <summary>
/// octoImports v1: the Spotify import page for the Octo apps, the same lists and trickle the
/// dashboard shows, for whoever the app signed in as. Every call checks the caller's sign-in with
/// Navidrome first, as getUpgrades does, and answers JSON.
///
/// The Spotify sign-in from an app: importAction connect with the app's own loopback redirect
/// (http://127.0.0.1:port/path) gives the address to open; the app's loopback page catches
/// Spotify's answer and sends its code and state back with importAction finish.
/// </summary>
[ApiController]
[Route("")]
public sealed class ImportsSubsonicController : ControllerBase
{
    private readonly ImportService _imports;
    private readonly SubsonicRequestParser _parser;
    private readonly SubsonicResponseBuilder _responses;
    private readonly SubsonicProxyService _proxy;
    private readonly ILogger<ImportsSubsonicController> _logger;

    public ImportsSubsonicController(ImportService imports, SubsonicRequestParser parser, SubsonicResponseBuilder responses,
        SubsonicProxyService proxy, ILogger<ImportsSubsonicController> logger)
    {
        _imports = imports;
        _parser = parser;
        _responses = responses;
        _proxy = proxy;
        _logger = logger;
    }

    /// <summary>The caller's username once Navidrome accepts their sign-in, or the error to answer with.</summary>
    private async Task<(string? User, IActionResult? Refused, Dictionary<string, string> Parameters)> CallerAsync()
    {
        var parameters = await _parser.ExtractAllParametersAsync(Request);
        var auth = new Dictionary<string, string>(parameters) { ["f"] = "json" };
        var check = await _proxy.RelaySafeAsync("rest/ping", auth);
        if (!check.Success || check.Body is null)
            return (null, _responses.CreateError("json", 0, "Octo can't reach Navidrome to check who is asking"), parameters);
        if (!Accepted(check.Body))
            return (null, _responses.CreateError("json", 40, "Wrong username or password"), parameters);
        // An API key alone names nobody here, and lists belong to someone.
        var user = parameters.GetValueOrDefault("u");
        if (string.IsNullOrWhiteSpace(user))
            return (null, _responses.CreateError("json", 0, "Sign in with a username to import; an API key alone does not say who is asking."), parameters);
        return (user.Trim(), null, parameters);
    }

    private static bool Accepted(byte[] body)
    {
        try
        {
            using var doc = System.Text.Json.JsonDocument.Parse(body);
            return doc.RootElement.TryGetProperty("subsonic-response", out var response)
                && response.TryGetProperty("status", out var status) && status.GetString() == "ok";
        }
        catch { return false; }
    }

    [HttpGet, HttpPost]
    [Route("rest/getImports")]
    [Route("rest/getImports.view")]
    public async Task<IActionResult> GetImports()
    {
        var (user, refused, _) = await CallerAsync();
        if (refused is not null) return refused;
        return _responses.CreateImportsResponse(_imports.Overview(user!));
    }

    [HttpGet, HttpPost]
    [Route("rest/getImport")]
    [Route("rest/getImport.view")]
    public async Task<IActionResult> GetImport()
    {
        var (user, refused, parameters) = await CallerAsync();
        if (refused is not null) return refused;
        var id = parameters.GetValueOrDefault("id", "").Trim();
        if (id.Length == 0) return _responses.CreateError("json", 10, "Required parameter is missing: id");
        return _imports.Detail(user!, id) is { } detail
            ? _responses.CreateImportResponse(detail)
            : _responses.CreateError("json", 70, "There is no such list.");
    }

    /// <summary>
    /// One thing to do: connect (redirect), finish (code and state), disconnect, read, addLink (url),
    /// playlist (id, on), fetch (id, on), songs (id, key...), refresh (id), remove (id), pause, resume,
    /// retry (key... or none for every song not found), skip (key...), clear.
    /// </summary>
    [HttpGet, HttpPost]
    [Route("rest/importAction")]
    [Route("rest/importAction.view")]
    public async Task<IActionResult> ImportAction()
    {
        var (user, refused, parameters) = await CallerAsync();
        if (refused is not null) return refused;
        var action = parameters.GetValueOrDefault("action", "").Trim();
        var id = parameters.GetValueOrDefault("id", "").Trim();
        var on = !string.Equals(parameters.GetValueOrDefault("on", "true"), "false", StringComparison.OrdinalIgnoreCase);
        var keys = await _parser.ExtractParameterValuesAsync(Request, "key", HttpContext.RequestAborted);
        // Slow work is not tied to the request: an app that gives up must not leave a list half done.
        var ct = CancellationToken.None;
        ImportActionResult result = action.ToLowerInvariant() switch
        {
            "connect" => _imports.BeginSpotify(user!, parameters.GetValueOrDefault("redirect")),
            "finish" => await _imports.FinishSpotifyAsync(parameters.GetValueOrDefault("address"),
                parameters.GetValueOrDefault("code"), parameters.GetValueOrDefault("state"), user, ct),
            "disconnect" => _imports.DisconnectSpotify(user!),
            "read" => new(true, _imports.StartReading(user!) ? "Reading your Spotify lists again." : "Octo is already reading your Spotify lists."),
            "addlink" => await _imports.AddLinkAsync(user!, parameters.GetValueOrDefault("url", ""), ct),
            "playlist" => await _imports.SetPlaylistAsync(user!, id, on, ct),
            "fetch" => _imports.SetGetMissing(user!, id, on),
            "songs" => _imports.GetSongs(user!, id, keys),
            "refresh" => await _imports.RefreshAsync(user!, id, ct),
            "remove" => _imports.Remove(user!, id),
            "pause" => _imports.Pause(user!, true),
            "resume" => _imports.Pause(user!, false),
            "retry" => _imports.Retry(user!, keys),
            "skip" => _imports.Skip(user!, keys),
            "clear" => _imports.ClearFinished(user!),
            _ => new(false, $"Unknown action \"{action}\"."),
        };
        if (!result.Ok) _logger.LogInformation("Import action {Action} for {User}: {Message}", action, user, result.Message);
        return _responses.CreateImportActionResponse(result);
    }
}
