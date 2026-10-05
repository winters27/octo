using System.Text.Json;
using Microsoft.AspNetCore.Mvc;
using Microsoft.Extensions.Options;
using Octo.Models.Settings;
using Octo.Services.Admin;
using Octo.Services.Library;
using Octo.Services.Local;
using Octo.Services.Lyrics;
using Octo.Services.Soulseek;
using Octo.Services.Subsonic;

namespace Octo.Controllers;

/// <summary>
/// The dashboard's lyrics tools: "Find lyrics for the library", its review list, and a picker
/// that chooses, hides or resets one song's lyrics for every client, the same pins the Octo
/// app sets through setLyricsChoice. Everything here names files or changes what every
/// listener sees, so all of it needs a Navidrome admin sign-in, as the genre backfill does.
/// </summary>
[ApiController]
[Route("api/admin/lyrics")]
public sealed class LyricsAdminController : ControllerBase
{
    private static readonly TimeSpan CandidatesBudget = TimeSpan.FromSeconds(15);

    private readonly BrowseSessionStore _sessions;
    private readonly LyricsLibraryWorker _job;
    private readonly LyricsChoiceService _choices;
    private readonly LyricsSidecarWriter _writer;
    private readonly NavidromeSongPathResolver _paths;
    private readonly NavidromeIdentityService _identity;
    private readonly ExternalIdRegistry _registry;
    private readonly ILocalLibraryService _library;
    private readonly IOptionsMonitor<MetadataSettings> _metadata;
    private readonly IOptionsMonitor<SubsonicSettings> _subsonic;
    private readonly IHttpClientFactory _http;
    private readonly ILogger<LyricsAdminController> _logger;

    public LyricsAdminController(BrowseSessionStore sessions, LyricsLibraryWorker job, LyricsChoiceService choices,
        LyricsSidecarWriter writer, NavidromeSongPathResolver paths, NavidromeIdentityService identity,
        ExternalIdRegistry registry, ILocalLibraryService library, IOptionsMonitor<MetadataSettings> metadata,
        IOptionsMonitor<SubsonicSettings> subsonic, IHttpClientFactory http, ILogger<LyricsAdminController> logger)
    {
        _sessions = sessions;
        _job = job;
        _choices = choices;
        _writer = writer;
        _paths = paths;
        _identity = identity;
        _registry = registry;
        _library = library;
        _metadata = metadata;
        _subsonic = subsonic;
        _http = http;
        _logger = logger;
    }

    private bool Signed(string? header) =>
        _sessions.NavidromeUserOf(Request.Cookies[AdminController.BrowseCookieName] ?? header) is not null;

    private IActionResult SignIn() => Unauthorized(new { error = "Sign in with your Navidrome admin account first." });

    // ---- Find lyrics for the library ------------------------------------------------------

    /// <summary>Upgrade is for a walk. Mode is a step of the lyrics page (Scan, Preview, Save,
    /// Undo), Scope is for a scan, Picked the rows for Preview and Save.</summary>
    public sealed record LibraryStartRequest(bool Upgrade, string? Mode = null, string? Scope = null, List<string>? Picked = null);

    [HttpGet("library")]
    public IActionResult GetLibraryRun([FromHeader(Name = "X-Octo-Browse-Token")] string? token)
    {
        if (!Signed(token)) return SignIn();
        var run = _job.Current;
        return Ok(new
        {
            run.RunId,
            status = run.Status.ToString(),
            run.Scope,
            run.Upgrade,
            run.StartedUtc,
            run.FinishedUtc,
            run.Total,
            run.Processed,
            run.Written,
            run.WordTimed,
            run.Upgraded,
            run.AlreadyHad,
            run.NotFound,
            run.Instrumental,
            run.Busy,
            run.Skipped,
            run.Failed,
            run.LastPath,
            run.Reason,
            run.Errors,
            canResume = run.CanResume,
            mode = run.Mode.ToString(),
            run.WordAlready,
            picked = run.Picked?.Count,
            // What a preview found stays here; the dashboard gets the first lines.
            rows = run.Rows.Select(row => new
            {
                row.Id, row.Path, row.Artist, row.Title, row.Album, row.Has, row.Result,
                row.Source, row.Kind, row.CandidateId, row.Doubt, row.Preview,
            }),
            // Not "busy": that is the run's count of songs no service answered for, and two
            // properties of one name cannot be written.
            running = _job.IsRunning,
            canUndo = _job.CanUndo,
            saveTo = LyricsSaveTo.Normalize(_metadata.CurrentValue.SaveLyricsTo),
            review = run.Review.OrderByDescending(entry => entry.AtUtc).ToList(),
            writesBesideAll = _metadata.CurrentValue.WriteLyricsBesideAllSongs,
            fetching = _metadata.CurrentValue.FetchLyrics,
        });
    }

    [HttpPost("library")]
    public IActionResult StartLibraryRun([FromBody] LibraryStartRequest request,
        [FromHeader(Name = "X-Octo-Browse-Token")] string? token)
    {
        if (!Signed(token)) return SignIn();
        if (!Enum.TryParse<LyricsLibraryMode>(request.Mode, ignoreCase: true, out var mode)) mode = LyricsLibraryMode.Walk;
        // A scan and an undo look nothing up.
        if (!_metadata.CurrentValue.FetchLyrics && mode is LyricsLibraryMode.Walk or LyricsLibraryMode.Preview)
            return BadRequest(new { error = "Turn on Fetch lyrics first, or a run would find nothing." });
        if (mode is LyricsLibraryMode.Preview or LyricsLibraryMode.Save && request.Picked is not { Count: > 0 })
            return BadRequest(new { error = "Pick at least one song." });
        if (mode == LyricsLibraryMode.Undo && !_job.CanUndo)
            return BadRequest(new { error = "There is nothing to undo." });
        if (!_job.TryEnqueue(new LyricsLibraryRequest(request.Upgrade, Mode: mode, Scope: request.Scope, Picked: request.Picked)))
            return Conflict(new { error = "Lyrics are already being found for the library." });
        _logger.LogInformation("Lyrics for the library requested: {Mode}, scope {Scope}, {Picked} (upgrade {Upgrade})",
            mode, request.Scope ?? "default", request.Picked is null ? "no pick" : $"{request.Picked.Count} picked", request.Upgrade);
        return Accepted(new { started = true });
    }

    [HttpPost("library/cancel")]
    public IActionResult CancelLibraryRun([FromHeader(Name = "X-Octo-Browse-Token")] string? token)
    {
        if (!Signed(token)) return SignIn();
        _job.RequestCancel();
        return Accepted(new { cancelling = true });
    }

    [HttpPost("library/resume")]
    public IActionResult ResumeLibraryRun([FromHeader(Name = "X-Octo-Browse-Token")] string? token)
    {
        if (!Signed(token)) return SignIn();
        if (!_job.Current.CanResume) return BadRequest(new { error = "There is nothing to resume." });
        if (!_job.TryEnqueue(new LyricsLibraryRequest(_job.Current.Upgrade, Resume: true, Mode: _job.Current.Mode)))
            return Conflict(new { error = "Lyrics are already being found for the library." });
        return Accepted(new { resumed = true });
    }

    public sealed record ReviewDismissRequest(string Path);

    [HttpPost("review/dismiss")]
    public IActionResult DismissReview([FromBody] ReviewDismissRequest request,
        [FromHeader(Name = "X-Octo-Browse-Token")] string? token)
    {
        if (!Signed(token)) return SignIn();
        _job.DismissReview(request.Path);
        return Ok(new { ok = true });
    }

    // ---- Choosing one song's lyrics -------------------------------------------------------

    /// <summary>Library songs by name, as Octo's admin account sees them, each with its
    /// current choice, for picking one to fix.</summary>
    [HttpGet("songs")]
    public async Task<IActionResult> SearchSongs([FromQuery] string? q,
        [FromHeader(Name = "X-Octo-Browse-Token")] string? token, CancellationToken ct)
    {
        if (!Signed(token)) return SignIn();
        if (string.IsNullOrWhiteSpace(q)) return Ok(new { songs = Array.Empty<object>() });

        using var doc = await AdminSubsonicAsync("search3",
            $"query={Uri.EscapeDataString(q.Trim())}&songCount=25&albumCount=0&artistCount=0", ct);
        if (doc is null) return BadRequest(new { error = "Octo needs a Navidrome admin sign-in to search the library." });
        var songs = doc.RootElement.TryGetProperty("subsonic-response", out var envelope)
            && envelope.TryGetProperty("searchResult3", out var result) && result.TryGetProperty("song", out var list)
            && list.ValueKind == JsonValueKind.Array
            ? list.EnumerateArray().Select(song => new
            {
                id = Str(song, "id"),
                title = Str(song, "title"),
                artist = Str(song, "artist"),
                album = Str(song, "album"),
                duration = song.TryGetProperty("duration", out var d) && d.TryGetInt32(out var s) ? s : (int?)null,
                choice = _choices.ChoiceFor(Str(song, "id") ?? "", Str(song, "artist"), Str(song, "title")),
            }).ToList<object>()
            : [];
        return Ok(new { songs });
    }

    /// <summary>
    /// Every lyrics entry for one song, by its id (a library or an outside song), or by the file
    /// a review entry names. artist and title, when given, search for those instead.
    /// </summary>
    [HttpGet("candidates")]
    public async Task<IActionResult> Candidates([FromQuery] string? id, [FromQuery] string? path,
        [FromQuery] string? artist, [FromQuery] string? title,
        [FromHeader(Name = "X-Octo-Browse-Token")] string? token, CancellationToken ct)
    {
        if (!Signed(token)) return SignIn();
        if (!_metadata.CurrentValue.FetchLyrics) return BadRequest(new { error = "Turn on Fetch lyrics first." });

        var song = await SongAsync(id, path, ct);
        if (song is null) return NotFound(new { error = "Octo could not find that song." });

        var query = new LyricsQuery(
            string.IsNullOrWhiteSpace(artist) ? song.Artist : artist.Trim(),
            LyricsText.QueryTitle(string.IsNullOrWhiteSpace(title) ? song.Title : title.Trim(),
                string.IsNullOrWhiteSpace(artist) ? song.Artist : artist.Trim()),
            song.Album, song.Duration);
        using var budget = CancellationTokenSource.CreateLinkedTokenSource(ct);
        budget.CancelAfter(CandidatesBudget);
        var candidates = await _choices.CandidatesAsync(query, budget.Token);
        return Ok(new
        {
            id = song.Id,
            path = song.Path,
            song.Artist,
            song.Title,
            song.Album,
            duration = song.Duration,
            choice = song.Id is null ? LyricsPin.Auto : _choices.ChoiceFor(song.Id, song.Artist, song.Title),
            candidates,
        });
    }

    public sealed record ChoiceRequest(string? Id, string? Path, string Candidate);

    /// <summary>
    /// Set one song's lyrics: a candidate, "none" to show none, or "auto". The choice is a pin
    /// every client sees. From the review list it also rewrites the lyrics file, but only one
    /// Octo may write (its own, beside a song it is allowed to write beside).
    /// </summary>
    [HttpPost("choice")]
    public async Task<IActionResult> Choose([FromBody] ChoiceRequest request,
        [FromHeader(Name = "X-Octo-Browse-Token")] string? token, CancellationToken ct)
    {
        if (!Signed(token)) return SignIn();
        var candidate = (request.Candidate ?? "").Trim();
        if (candidate.Length == 0) return BadRequest(new { error = "Say which lyrics: a candidate, none or auto." });

        var song = await SongAsync(request.Id, request.Path, ct);
        if (song is null) return NotFound(new { error = "Octo could not find that song." });

        if (candidate.Equals(LyricsPin.Auto, StringComparison.OrdinalIgnoreCase))
        {
            if (song.Id is not null) _choices.Clear(song.Id, song.Artist, song.Title);
            return Ok(new { id = song.Id, choice = LyricsPin.Auto });
        }
        if (candidate.Equals(LyricsPin.Hidden, StringComparison.OrdinalIgnoreCase))
        {
            if (song.Id is null)
                return BadRequest(new { error = "Navidrome has not scanned this song yet, so it cannot be hidden. Try again after a scan." });
            _choices.Hide(song.Id, song.Artist, song.Title, "dashboard");
            if (song.Path is not null) _job.DismissReview(song.Path);
            return Ok(new { id = song.Id, choice = LyricsPin.Hidden });
        }

        if (!_metadata.CurrentValue.FetchLyrics) return BadRequest(new { error = "Turn on Fetch lyrics first." });
        using var budget = CancellationTokenSource.CreateLinkedTokenSource(ct);
        budget.CancelAfter(CandidatesBudget);
        var lyrics = await _choices.LyricsOfAsync(candidate, budget.Token);
        if (lyrics is null) return NotFound(new { error = "Those lyrics could not be found; search again." });

        var pinned = song.Id is not null && await _choices.PinAsync(song.Id, candidate, song.Artist, song.Title, "dashboard", budget.Token);
        var rewrote = song.Path is not null && await MayWriteBesideAsync(song.Path)
            && await _writer.ReplaceAsync(song.Path, lyrics, budget.Token);
        if (!pinned && !rewrote)
            return BadRequest(new { error = "Navidrome has not scanned this song yet and its lyrics file is not Octo's to change." });
        if (song.Path is not null) _job.DismissReview(song.Path);
        return Ok(new { id = song.Id, choice = candidate, pinned, rewroteFile = rewrote });
    }

    /// <summary>Every song whose lyrics someone chose or hid, newest first.</summary>
    [HttpGet("choices")]
    public IActionResult Choices([FromHeader(Name = "X-Octo-Browse-Token")] string? token)
    {
        if (!Signed(token)) return SignIn();
        return Ok(new
        {
            choices = _choices.All().Select(pin => new
            {
                id = pin.SongId,
                pin.Artist,
                pin.Title,
                choice = pin.Choice,
                pin.Source,
                kind = pin.IsHidden ? "hidden" : LyricsChoiceService.KindOf(pin.Lyrics!),
                pin.SetBy,
                pin.SetUtc,
            }),
        });
    }

    private sealed record SongRef(string? Id, string? Path, string Artist, string Title, string? Album, int? Duration);

    /// <summary>
    /// A song by id (outside songs from the registry, library songs from Navidrome) or by a
    /// file the review list names. A path is only taken from the review list, never as given,
    /// so this cannot be pointed at any file on the disk.
    /// </summary>
    private async Task<SongRef?> SongAsync(string? id, string? path, CancellationToken ct)
    {
        if (!string.IsNullOrWhiteSpace(path))
        {
            var entry = _job.Current.Review.FirstOrDefault(review => string.Equals(review.Path, path, StringComparison.Ordinal));
            if (entry is null || LyricsLibraryWorker.ReadTags(entry.Path) is not { } tags) return null;
            var found = await _paths.FindIdByPathAsync(tags.Artist, tags.Title, entry.Path, ct);
            return new SongRef(found, entry.Path, tags.Artist, tags.Title, tags.Album, tags.DurationSeconds);
        }
        if (string.IsNullOrWhiteSpace(id)) return null;

        if ((_registry.Lookup(id) ?? SoulseekMetadataService.TryDecodeExternalId(id)) is { HasArtistTitle: true } routing)
            return new SongRef(id, null, routing.Artist!, routing.Title!, routing.Album, routing.Duration);

        using var doc = await AdminSubsonicAsync("getSong", $"id={Uri.EscapeDataString(id)}", ct);
        if (doc is null || !doc.RootElement.TryGetProperty("subsonic-response", out var envelope)
            || !envelope.TryGetProperty("song", out var song)) return null;
        var artist = Str(song, "artist");
        var title = Str(song, "title");
        if (string.IsNullOrWhiteSpace(artist) || string.IsNullOrWhiteSpace(title)) return null;
        return new SongRef(id, null, artist, title, Str(song, "album"),
            song.TryGetProperty("duration", out var d) && d.TryGetInt32(out var s) ? s : null);
    }

    /// <summary>The download rule: beside Octo's own downloads, and beside everything only when
    /// "Write lyrics files beside all library songs" is on.</summary>
    private async Task<bool> MayWriteBesideAsync(string path)
    {
        if (!NavidromeSongPathResolver.IsInside(Path.GetFullPath(path), Path.GetFullPath(_paths.MusicRoot()))) return false;
        if (_metadata.CurrentValue.WriteLyricsBesideAllSongs) return true;
        var mappings = await _library.GetMappingsAsync();
        return mappings.Any(mapping => string.Equals(mapping.LocalPath, path, StringComparison.OrdinalIgnoreCase));
    }

    /// <summary>A Subsonic call as Octo's admin account, or null without one.</summary>
    private async Task<JsonDocument?> AdminSubsonicAsync(string endpoint, string query, CancellationToken ct)
    {
        var baseUrl = _subsonic.CurrentValue.Url;
        if (string.IsNullOrWhiteSpace(baseUrl) || _identity.GetScanAuth() is not { } auth) return null;
        try
        {
            var url = $"{baseUrl.TrimEnd('/')}/rest/{endpoint}?f=json&c=octo&v=1.16.1&{query}"
                + $"&u={Uri.EscapeDataString(auth.user)}&t={auth.token}&s={auth.salt}";
            using var response = await _http.CreateClient().GetAsync(url, ct);
            if (!response.IsSuccessStatusCode) return null;
            return JsonDocument.Parse(await response.Content.ReadAsByteArrayAsync(ct));
        }
        catch (Exception ex) when (ex is not OperationCanceledException)
        {
            _logger.LogDebug("admin {Endpoint} failed: {M}", endpoint, ex.Message);
            return null;
        }
    }

    private static string? Str(JsonElement element, string name) =>
        element.TryGetProperty(name, out var value) && value.ValueKind == JsonValueKind.String ? value.GetString() : null;
}
