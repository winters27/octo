using Microsoft.AspNetCore.Mvc;
using Microsoft.Extensions.Options;
using Octo.Models.Settings;
using Octo.Services.Admin;
using Octo.Services.Common;
using Octo.Services.CoverArt;
using Octo.Services.Library;
using Octo.Services.Local;
using Octo.Services.Soulseek;
using Octo.Services.Subsonic;

namespace Octo.Controllers;

/// <summary>
/// The dashboard's side of three things the Octo apps already do, for admins whose listeners use
/// other Subsonic apps: a download's full log on Fetched songs, Find songs (a search run again by
/// hand, every copy listed, one picked), and Recently removed with Put back.
///
/// Everything here names peers, files or people, or changes the library, so every call needs a
/// Navidrome admin's own dashboard sign-in (never the recovery code) and acts as that person:
/// their Find songs limits, and for a library file the library actions gates (switched on, on the
/// allowed list, dry run only rehearses), exactly as in the apps.
/// </summary>
[ApiController]
[Route("api/admin")]
public sealed class AdminDownloadsController : ControllerBase
{
    private const string SignInFirst = "Sign in with your Navidrome admin account first.";

    /// <summary>Outside songs a search on the Find songs page lists.</summary>
    internal const int OutsideLimit = 20;

    private const int CoverSide = 160;

    private readonly BrowseSessionStore _sessions;
    private readonly DownloadHistoryService _history;
    private readonly IServiceProvider _services;
    private readonly ILogger<AdminDownloadsController> _logger;

    public AdminDownloadsController(BrowseSessionStore sessions, DownloadHistoryService history,
        IServiceProvider services, ILogger<AdminDownloadsController> logger)
    {
        _sessions = sessions;
        _history = history;
        _services = services;
        _logger = logger;
    }

    /// <summary>The Navidrome admin signed in on this browser, or null (no sign-in, or the recovery code).</summary>
    private string? Admin() =>
        _sessions.NavidromeUserOf(Request.Cookies[AdminCookie.Name] ?? Request.Headers["X-Octo-Browse-Token"].FirstOrDefault());

    private IActionResult SignIn() => Unauthorized(new { error = SignInFirst });

    // ---- A download's log ---------------------------------------------------------------------

    /// <summary>
    /// One download's log, by its row key. With <c>at</c> (a Fetched songs entry's downloadedAt),
    /// the log saved with that entry wins; otherwise, or when none was saved, the live row's log,
    /// when the live list still has this run of it. kept says which: "saved", "live", or "none".
    /// </summary>
    [HttpGet("downloads/log")]
    public IActionResult DownloadLog([FromQuery] string? key, [FromQuery] string? at)
    {
        if (Admin() is null) return SignIn();
        if (string.IsNullOrWhiteSpace(key)) return BadRequest(new { error = "Which download? The key is missing." });
        key = key.Trim();

        var entry = string.IsNullOrWhiteSpace(at) ? null : _history.Find(key, at.Trim());
        if (entry?.Log is { Count: > 0 } saved)
            return Ok(new { key, kept = "saved", row = (object?)null, @event = saved.Select(SubsonicResponseBuilder.EventJson).ToList() });

        var row = _services.GetService<AcquisitionTracker>()?.Detail(key, username: null);
        // A live row that began after this entry was written is a later download of the same song.
        var sameRun = row is not null && (entry is null
            || DownloadHistoryService.WrittenAt(entry) is not { } written || row.StartedAt <= written.AddSeconds(1));
        if (row is not null && sameRun)
        {
            var json = SubsonicResponseBuilder.AcquisitionJson(row);
            json["provider"] = row.Provider;
            json["externalId"] = row.ExternalId;
            return Ok(new { key, kept = "live", row = json, @event = (row.Events ?? []).Select(SubsonicResponseBuilder.EventJson).ToList() });
        }
        return Ok(new { key, kept = "none", row = (object?)null, @event = Array.Empty<object>() });
    }

    // ---- Find songs ---------------------------------------------------------------------------

    public sealed record FindRequest(string? Id);
    public sealed record PickRequest(string? Copy);

    /// <summary>
    /// Starts a Find songs search for one song (an outside song's id or key, or a library song's
    /// id) as the signed-in admin, on their download sources, and answers at once while it runs.
    /// </summary>
    [HttpPost("find")]
    public async Task<IActionResult> StartFind([FromBody] FindRequest? request, CancellationToken ct)
    {
        var user = Admin();
        if (user is null) return SignIn();
        if (string.IsNullOrWhiteSpace(request?.Id)) return BadRequest(new { error = "Which song? The id is missing." });
        if (_services.GetService<SongFinder>() is not { } finder) return NotFound(new { error = "Find songs is not available on this server." });
        var found = await finder.StartAsync(request.Id.Trim(), user, ct);
        if (found is null) return NotFound(new { error = "Octo could not find that song." });
        _logger.LogInformation("{User} started Find songs for '{Artist} - {Title}' from the dashboard",
            user, found.Target.Artist, found.Target.Title);
        return Ok(SubsonicResponseBuilder.FoundSongsJson(found));
    }

    /// <summary>A Find songs search as it stands, for the admin who started it.</summary>
    [HttpGet("find/{search}")]
    public IActionResult GetFind(string search)
    {
        var user = Admin();
        if (user is null) return SignIn();
        var found = _services.GetService<SongFinder>()?.Get(search, user);
        return found is null
            ? NotFound(new { error = "This search is gone. Search again." })
            : Ok(SubsonicResponseBuilder.FoundSongsJson(found));
    }

    /// <summary>
    /// Fetches exactly one copy the search listed, by its id there. A library song's copy goes
    /// through Better quality, so only someone on the library actions allowed list may pick one,
    /// and the original stays until the new file passes every check.
    /// </summary>
    [HttpPost("find/{search}/pick")]
    public async Task<IActionResult> PickFound(string search, [FromBody] PickRequest? request)
    {
        var user = Admin();
        if (user is null) return SignIn();
        if (string.IsNullOrWhiteSpace(request?.Copy)) return BadRequest(new { error = "Which copy? The copy is missing." });
        if (_services.GetService<SongFinder>() is not { } finder) return NotFound(new { error = "Find songs is not available on this server." });
        var outcome = await finder.PickAsync(search, request.Copy.Trim(), user);
        return Ok(new { state = outcome.State, detail = outcome.Detail, key = outcome.Key });
    }

    /// <summary>
    /// Songs Octo can find outside the library for this search, the ones the apps' search lists
    /// under the library's (Last.fm, with Deezer's covers). Empty, with available false, when
    /// there is no Last.fm key to search with.
    /// </summary>
    [HttpGet("find/outside")]
    public async Task<IActionResult> OutsideSongs([FromQuery] string? q, CancellationToken ct)
    {
        if (Admin() is null) return SignIn();
        var available = _services.GetService<Octo.Services.LastFm.LastFmService>()?.HasApiKey == true;
        if (string.IsNullOrWhiteSpace(q) || !available || _services.GetService<ExternalSearchService>() is not { } search)
            return Ok(new { available, songs = Array.Empty<object>() });
        var songs = await search.GetAsync(q.Trim(), ct);
        return Ok(new
        {
            available,
            songs = songs.Take(OutsideLimit).Select(song => new
            {
                id = string.IsNullOrWhiteSpace(song.ExternalId) ? song.Id : song.ExternalId,
                song.Title,
                song.Artist,
                album = string.IsNullOrWhiteSpace(song.Album) ? null : song.Album,
                song.Duration,
                coverUrl = song.CoverArtUrl ?? song.CoverArtUrlLarge,
            }),
        });
    }

    /// <summary>
    /// A small cover for a song on the Find songs page: a library song's own picture, read from
    /// its file, or for an outside song the catalog's cover (a redirect to it).
    /// </summary>
    [HttpGet("find/cover")]
    public async Task<IActionResult> FindCover([FromQuery] string? id, CancellationToken ct)
    {
        if (Admin() is null) return SignIn();
        if (string.IsNullOrWhiteSpace(id)) return NotFound();
        var raw = id.Trim();
        var provider = SoulseekMetadataService.ProviderName;
        if (raw.StartsWith(provider + ":", StringComparison.OrdinalIgnoreCase)) raw = raw[(provider.Length + 1)..];
        if (raw.StartsWith("ext-", StringComparison.Ordinal)
            && _services.GetService<ILocalLibraryService>()?.ParseSongId(raw) is (true, _, { } outside)) raw = outside;

        var routing = _services.GetService<ExternalIdRegistry>()?.Lookup(raw) ?? SoulseekMetadataService.TryDecodeExternalId(raw);
        if (routing is { Kind: RoutingKind.Song, HasArtistTitle: true })
        {
            try
            {
                var deezer = _services.GetService<Octo.Services.Metadata.DeezerMetadataService>();
                var meta = deezer is null ? null : await deezer.EnrichTrackAsync(routing.Artist, routing.Title, includeYear: false);
                if (meta?.AlbumCoverUrl is { Length: > 0 } url && Uri.TryCreate(url, UriKind.Absolute, out var cover)
                    && cover.Scheme is "https" or "http")
                {
                    Response.Headers.CacheControl = "private, max-age=3600";
                    return Redirect(cover.ToString());
                }
            }
            catch (Exception ex) when (!ct.IsCancellationRequested)
            {
                _logger.LogDebug("Cover for {Id} could not be looked up: {Message}", raw, ex.Message);
            }
            return NotFound();
        }

        if (_services.GetService<NavidromeSongPathResolver>() is not { } resolver
            || await resolver.ResolveAsync(raw, ct) is not { } file || !System.IO.File.Exists(file.AbsolutePath))
            return NotFound();
        try
        {
            using var tags = TagLib.File.Create(file.AbsolutePath, TagLib.ReadStyle.None);
            var pictures = tags.Tag.Pictures ?? [];
            var picture = pictures.FirstOrDefault(p => p.Type == TagLib.PictureType.FrontCover) ?? pictures.FirstOrDefault();
            if (picture?.Data?.Data is not { Length: > 0 } bytes) return NotFound();
            Response.Headers.CacheControl = "private, max-age=3600";
            return File(CoverImage.ToJpeg(CoverImage.FitWithin(bytes, CoverSide)), "image/jpeg");
        }
        catch (Exception ex)
        {
            _logger.LogDebug("Cover of {Path} could not be read: {Message}", file.AbsolutePath, ex.Message);
            return NotFound();
        }
    }

    // ---- Recently removed ---------------------------------------------------------------------

    public sealed record RestoreRequest(string? Id);

    /// <summary>Why this admin may not see or put back removed songs, or null when they may.</summary>
    private string? TrashRefusal(string user, out LibraryEditService? edits)
    {
        edits = _services.GetService<LibraryEditService>();
        return edits is null ? "This server cannot change library files." : edits.Refusal(user, admin: true);
    }

    /// <summary>
    /// The songs removed from the library that are still in the server's trash, newest first:
    /// who removed each and when, when the sweep deletes it for good, the files of its own that
    /// went with it, and whether the removal keeps it from being downloaded again.
    /// </summary>
    [HttpGet("trash")]
    public IActionResult Trash()
    {
        var user = Admin();
        if (user is null) return SignIn();
        if (TrashRefusal(user, out var edits) is { } refused) return StatusCode(403, new { error = refused });
        var settings = _services.GetService<IOptionsMonitor<LibraryActionSettings>>()?.CurrentValue;
        return Ok(new
        {
            keepDays = settings?.EffectiveQuarantineRetentionDays ?? 0,
            dryRun = settings?.DryRun ?? false,
            songs = edits!.Trash().Select(song => new
            {
                song.Id, song.Title, song.Artist, song.Album,
                removedBy = song.Username,
                removedAt = song.RemovedUtc,
                goneAt = song.GoneUtc,
                song.Sidecars,
                song.Key,
                song.BlocksDownloads,
            }),
        });
    }

    /// <summary>Puts a removed song back where it was, with its own files, as the signed-in admin.</summary>
    [HttpPost("trash/restore")]
    public async Task<IActionResult> Restore([FromBody] RestoreRequest? request)
    {
        var user = Admin();
        if (user is null) return SignIn();
        if (TrashRefusal(user, out var edits) is { } refused) return StatusCode(403, new { error = refused });
        if (string.IsNullOrWhiteSpace(request?.Id)) return BadRequest(new { error = "Which song? The id is missing." });
        // Not tied to the request: a move that has started finishes.
        var outcome = await edits!.RestoreAsync(request.Id.Trim(), user);
        _logger.LogInformation("{User} asked to put back {Id} from the dashboard: {State} - {Detail}",
            user, request.Id, outcome.State, outcome.Detail);
        return Ok(new { state = outcome.State, detail = outcome.Detail });
    }
}
