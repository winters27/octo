using System.Globalization;
using Microsoft.AspNetCore.Mvc;
using Octo.Services.Admin;
using Octo.Services.Health;
using Octo.Services.Library;

namespace Octo.Controllers;

/// <summary>
/// The dashboard's Library health: what the Octo app's Library health finds, worked out on the
/// server, and its fixes, for people on other Subsonic clients. Admins only: behind the dashboard
/// sign-in like every admin endpoint, every call needs a Navidrome admin (the recovery code is not
/// one), and the writes act as that admin, refused unless they are on the library actions allowed
/// list, exactly as the app's libraryAction is. Writes carry X-Octo-Admin, as every dashboard write does.
/// </summary>
[ApiController]
[Route("api/admin/health")]
public sealed class LibraryHealthController(LibraryHealthService health, BrowseSessionStore sessions,
    ILogger<LibraryHealthController> logger) : ControllerBase
{
    private const string SignInFirst = "Sign in with your Navidrome admin account first.";

    /// <summary>The Navidrome admin asking: signed in on the dashboard, or the app with an admin's sign-in.</summary>
    private string? NavidromeUser() =>
        AdminCaller.Of(HttpContext) is { Kind: AdminCallerKind.Dashboard or AdminCallerKind.NavidromeAdmin, User: { } user }
            ? user
            : sessions.NavidromeUserOf(Request.Cookies[AdminController.BrowseCookieName]);

    // As every endpoint that reads or changes library files answers the recovery code: no.
    private UnauthorizedObjectResult NotAdmin() => Unauthorized(new { error = SignInFirst });

    // ---- reading ----------------------------------------------------------------------------

    /// <summary>The findings, grouped, with counts, plain-words reasons, paths, and what can be fixed.</summary>
    [HttpGet]
    public async Task<IActionResult> Get([FromQuery] bool fresh = false, CancellationToken ct = default)
    {
        if (NavidromeUser() is not { } user) return NotAdmin();
        var found = await health.ReportAsync(fresh, ct);
        if (found is null)
            return StatusCode(503, new
            {
                error = "Octo could not read your library from Navidrome. It needs a Navidrome admin sign-in under Music server.",
            });
        var can = health.Abilities(user);
        var report = found.Report;
        var fields = found.Fields;
        var used = new HashSet<string>(StringComparer.Ordinal);
        string Use(LibrarySongRow song)
        {
            used.Add(song.Id);
            return song.Id;
        }

        var checks = report.Findings.Select(check =>
        {
            var count = report.Count(check);
            var item = new Dictionary<string, object?>
            {
                ["check"] = CheckName(check),
                ["title"] = HealthWords.Title(check),
                ["count"] = count,
                ["countLabel"] = HealthWords.CountLabel(check, count),
                ["meaning"] = HealthWords.Meaning(check),
                ["advice"] = HealthWords.Advice(check),
                ["fixMeaning"] = HealthWords.FixMeaning(check),
            };
            switch (check)
            {
                case HealthCheck.Duplicates:
                    item["fixAllLabel"] = HealthWords.FixAllLabel(check, count);
                    item["sets"] = report.Duplicates.Select(set =>
                    {
                        var fix = HealthFixes.DuplicateFix(set, fields);
                        return new
                        {
                            key = SetKey(set, fields),
                            heading = HealthWords.Heading(set, fields),
                            summary = HealthWords.Summary(set, fields),
                            basis = HealthWords.Words(set.Basis),
                            copies = set.Copies.Select(Use).ToList(),
                            keep = fix.Keep.Id,
                            why = fix.Why,
                            line = FixLine(fix, fields, can.Edit),
                            fills = (can.Edit ? fix.Fills : []).Select(Change).ToList(),
                            note = fix.Note,
                        };
                    }).ToList();
                    break;
                case HealthCheck.SplitAlbums:
                    item["fixAllLabel"] = HealthWords.FixAllLabel(check, count);
                    item["albums"] = report.SplitAlbums.Select(album =>
                    {
                        var join = HealthFixes.AlbumJoin(album);
                        return new
                        {
                            key = AlbumKey(album),
                            heading = HealthWords.Heading(album),
                            summary = HealthWords.Summary(album),
                            reasons = album.Reasons.Select(HealthWords.Words).ToList(),
                            differences = album.Differences.Select(HealthWords.Words).ToList(),
                            parts = album.Parts.Select(part => new { albumId = part.AlbumId, songs = part.Songs.Select(Use).ToList() }).ToList(),
                            join = new { words = join.Words, lead = join.Lead.Id, moving = join.Moving.Count },
                        };
                    }).ToList();
                    break;
                default:
                    var songs = report.Songs(check);
                    item["songs"] = songs.Select(Use).ToList();
                    if (LibraryHealth.TagOf(check) is HealthTag.Year or HealthTag.Genre or HealthTag.AlbumArtist)
                    {
                        var fills = HealthFixes.FillsFromAlbum(songs, found.Songs, LibraryHealth.TagOf(check)!.Value, fields);
                        item["fills"] = fills.Select(fill => new { id = fill.Song.Id, fill.Change.Tag, fill.Change.Value, words = fill.Change.Words }).ToList();
                        item["fixAllLabel"] = fills.Count > 0 ? HealthWords.FixAllLabel(check, fills.Count) : null;
                    }
                    else item["fixAllLabel"] = HealthWords.FixAllLabel(check, songs.Count);
                    break;
            }
            return item;
        }).ToList();

        var byId = found.Songs.Where(song => used.Contains(song.Id)).ToDictionary(song => song.Id, song => SongView(song, fields));
        return Ok(new
        {
            @checked = report.Checked,
            overview = HealthWords.Overview(report),
            clean = report.Clean,
            allClear = HealthWords.AllClear,
            readUtc = found.ReadUtc,
            user,
            can,
            lookupBatch = LibraryHealthService.LookupBatch,
            checks,
            songs = byId,
            run = RunView(health.Run),
            undoCount = health.UndoCount,
        });
    }

    /// <summary>The run going now or last: how far, and how it went.</summary>
    [HttpGet("run")]
    public IActionResult GetRun()
    {
        if (NavidromeUser() is null) return NotAdmin();
        return Ok(new { run = RunView(health.Run), undoCount = health.UndoCount, checking = health.CheckingSince is not null });
    }

    // ---- previews ---------------------------------------------------------------------------

    public sealed record PreviewRequest(string? Check, List<string>? Keys, string? Keep, List<string>? Fills,
        Dictionary<string, string>? Choose);

    /// <summary>
    /// What a fix would do, before anything changes: the same plans the app makes. Duplicates keep
    /// the best copy (or <c>keep</c>) with the reason, fill blanks from the copies (only the tags in
    /// <c>fills</c> when given) and take the values picked in <c>choose</c>; split albums join onto
    /// their lead part; missing year, genre and album artist fill from the album; covers are looked
    /// for. <c>keys</c> narrows it to some sets, albums or songs. Writes nothing.
    /// </summary>
    [HttpPost("preview")]
    public async Task<IActionResult> Preview([FromBody] PreviewRequest request, CancellationToken ct)
    {
        if (NavidromeUser() is not { } user) return NotAdmin();
        if (ParseCheck(request.Check) is not { } check) return BadRequest(new { error = "Say which finding to fix." });
        var found = await health.ReportAsync(fresh: false, ct);
        if (found is null) return StatusCode(503, new { error = "Octo could not read your library from Navidrome." });
        var can = health.Abilities(user);
        var report = found.Report;
        var fields = found.Fields;
        var keys = request.Keys is { Count: > 0 } picked ? picked.ToHashSet(StringComparer.Ordinal) : null;

        switch (check)
        {
            case HealthCheck.Duplicates:
            {
                var sets = report.Duplicates.Where(set => keys is null || keys.Contains(SetKey(set, fields))).ToList();
                if (sets.Count == 0) return Gone();
                var one = sets.Count == 1;
                var plans = sets.Select(set =>
                {
                    var keep = one && request.Keep is { } keepId ? set.Copies.FirstOrDefault(copy => copy.Id == keepId) : null;
                    var fix = HealthFixes.DuplicateFix(set, fields, keep);
                    var fills = !can.Edit ? [] : one && request.Fills is { } tags
                        ? fix.Fills.Where(change => tags.Contains(change.Tag)).ToList()
                        : fix.Fills.ToList();
                    var chosen = !can.Edit || !one ? [] : fix.Differs.Select(choice =>
                    {
                        var now = choice.Values.FirstOrDefault(pair => pair.Key == fix.Keep.Id).Value;
                        return request.Choose?.GetValueOrDefault(choice.Tag) is { } value && value != now
                            && choice.Values.Any(pair => pair.Value == value)
                            ? new TagChange(choice.Tag, now, value)
                            : null;
                    }).OfType<TagChange>().ToList();
                    return (Set: set, Fix: fix, Changes: fills.Concat(chosen).ToList());
                }).ToList();
                var steps = plans.SelectMany(plan => HealthFixes.Steps(plan.Fix, fields, plan.Changes)).ToList();
                var removing = plans.Sum(plan => plan.Fix.Remove.Count);
                return Ok(new
                {
                    title = one ? "Fix these copies" : $"Fix {HealthWords.CountText(sets.Count, "song", "songs")} you have twice",
                    meaning = HealthWords.FixMeaning(check),
                    go = $"Move {HealthWords.CountText(removing, "copy", "copies")} to the trash",
                    label = "Fixing copies",
                    canFix = can.Remove,
                    why = can.Remove ? null : can.Why ?? "Removing songs is off: turn on the Delete action under Library actions.",
                    sets = plans.Select(plan => new
                    {
                        key = SetKey(plan.Set, fields),
                        heading = HealthWords.Heading(plan.Set, fields),
                        why = plan.Fix.Why,
                        line = FixLine(plan.Fix with { Fills = plan.Changes }, fields, can.Edit),
                        note = plan.Fix.Note,
                        keep = plan.Fix.Keep.Id,
                        copies = plan.Set.Copies.Select(copy => SongView(copy, fields)).ToList(),
                        fills = (can.Edit ? plan.Fix.Fills : []).Select(Change).ToList(),
                        differs = (can.Edit ? plan.Fix.Differs : []).Select(choice => new
                        {
                            choice.Tag,
                            label = HealthFixes.TagName(choice.Tag),
                            values = choice.Values.Select(pair => new { id = pair.Key, value = pair.Value }).ToList(),
                            picked = request.Choose?.GetValueOrDefault(choice.Tag)
                                     ?? choice.Values.First(pair => pair.Key == plan.Fix.Keep.Id).Value,
                        }).ToList(),
                        diff = one ? DuplicateDiff(plan.Set, plan.Fix, plan.Changes, fields) : null,
                    }).ToList(),
                    steps = steps.Select(StepView).ToList(),
                });
            }
            case HealthCheck.SplitAlbums:
            {
                var albums = report.SplitAlbums.Where(album => keys is null || keys.Contains(AlbumKey(album))).ToList();
                if (albums.Count == 0) return Gone();
                var joins = albums.Select(HealthFixes.AlbumJoin).ToList();
                var moving = joins.Sum(join => join.Moving.Count);
                return Ok(new
                {
                    title = joins.Count == 1 ? "Join this album" : $"Join {HealthWords.CountText(joins.Count, "album", "albums")}",
                    meaning = HealthWords.FixMeaning(check),
                    go = $"Move {HealthWords.CountText(moving, "song", "songs")}",
                    label = "Joining albums",
                    canFix = can.JoinAlbums,
                    why = can.JoinAlbums ? null : can.Why,
                    albums = joins.Select(join => new
                    {
                        key = AlbumKey(join.Album),
                        heading = HealthWords.Heading(join.Album),
                        words = join.Words,
                        reasons = join.Album.Reasons.Select(HealthWords.Words).ToList(),
                        diff = JoinDiff(join, fields),
                    }).ToList(),
                    steps = joins.SelectMany(join => HealthFixes.Steps(join, fields)).Select(StepView).ToList(),
                });
            }
            case HealthCheck.NoYear or HealthCheck.NoGenre or HealthCheck.NoAlbumArtist:
            {
                var tag = LibraryHealth.TagOf(check)!.Value;
                var songs = report.Songs(check).Where(song => keys is null || keys.Contains(song.Id)).ToList();
                var fills = HealthFixes.FillsFromAlbum(songs, found.Songs, tag, fields);
                if (fills.Count == 0)
                    return Ok(new { title = HealthWords.Title(check), meaning = HealthWords.FixMeaning(check), canFix = false,
                        why = "The rest of the album does not agree on it, so it can only be looked up.", steps = Array.Empty<object>() });
                return Ok(new
                {
                    title = $"Fill in {HealthWords.CountText(fills.Count, "song", "songs")}",
                    meaning = HealthWords.FixMeaning(check),
                    go = $"Fill in {HealthWords.CountText(fills.Count, "song", "songs")}",
                    label = "Filling in tags",
                    canFix = can.Edit,
                    why = can.Edit ? null : can.Why,
                    lines = fills.Select(fill => new { id = fill.Song.Id, title = fill.Song.Title, words = fill.Change.Words }).ToList(),
                    steps = fills.Select(fill => StepView(FixStep.RetagWith(fill.Song.Id, fill.Song.Title, [fill.Change]))).ToList(),
                });
            }
            case HealthCheck.NoCover:
            {
                var songs = report.Songs(check).Where(song => keys is null || keys.Contains(song.Id)).ToList();
                if (songs.Count == 0) return Gone();
                var label = HealthWords.FixAllLabel(check, songs.Count);
                return Ok(new
                {
                    title = label,
                    meaning = HealthWords.FixMeaning(check),
                    go = label,
                    label = "Finding covers",
                    canFix = can.AddCover,
                    why = can.AddCover ? null : can.Why,
                    lines = songs.Select(song => new { id = song.Id, title = song.Title, words = $"{song.Artist} · {song.Album}" }).ToList(),
                    steps = songs.Select(song => StepView(new FixStep(FixActions.Cover, song.Id, song.Title))).ToList(),
                });
            }
            case HealthCheck.NoLength:
            {
                var songs = report.Songs(check).Where(song => keys is null || keys.Contains(song.Id)).ToList();
                if (songs.Count == 0) return Gone();
                var label = HealthWords.FixAllLabel(check, songs.Count);
                // Better quality's own queue does this, with its own gates; the page posts these there.
                return Ok(new
                {
                    title = label,
                    meaning = HealthWords.FixMeaning(check),
                    go = label,
                    canFix = can.Upgrade,
                    why = can.Upgrade ? null : can.Why ?? "Turn on the Better quality action under Library actions, and set up Soulseek or Lidarr.",
                    lines = songs.Select(song => new { id = song.Id, title = song.Title, words = $"{song.Artist} · {song.Album}" }).ToList(),
                    upgrade = songs.Select(song => new { navidromeId = song.Id, song.Title, song.Artist, song.Album }).ToList(),
                    steps = Array.Empty<object>(),
                });
            }
            default:
                return BadRequest(new { error = "Songs with no track number are looked up, not filled in: use lookup." });
        }
    }

    private NotFoundObjectResult Gone() =>
        NotFound(new { error = "That finding is no longer in your library. Check again to see what is left." });

    // ---- writes -----------------------------------------------------------------------------

    public sealed record StepRequest(string? Action, string? Id, string? Title, Dictionary<string, string>? With);
    public sealed record ApplyRequest(string? Label, string? Check, List<StepRequest>? Steps);

    /// <summary>Runs the steps a preview gave (or a person picked from a lookup), one song at a time,
    /// as the signed-in admin. 202 when it started; 409 while another run is going.</summary>
    [HttpPost("apply")]
    public IActionResult Apply([FromBody] ApplyRequest request)
    {
        if (NavidromeUser() is not { } user) return NotAdmin();
        if (health.Refusal(user) is { } refused) return StatusCode(403, new { error = refused });
        var steps = (request.Steps ?? []).Select(step => new FixStep((step.Action ?? "").Trim(), (step.Id ?? "").Trim(),
            string.IsNullOrWhiteSpace(step.Title) ? step.Id ?? "" : step.Title.Trim(),
            step.With is { Count: > 0 } with ? new Dictionary<string, string>(with, StringComparer.Ordinal) : null)).ToList();
        if (LibraryHealthService.Invalid(steps) is { } invalid) return BadRequest(new { error = invalid });
        var label = string.IsNullOrWhiteSpace(request.Label) ? "Fixing your library" : request.Label.Trim();
        if (label.Length > 80) label = label[..80];
        if (health.StartFix(label, ParseCheck(request.Check), steps, user) is { } busy) return Busy(busy);
        return Accepted(new { started = true, run = RunView(health.Run) });
    }

    public sealed record LookupRequest(List<string>? Ids, string? Check);

    /// <summary>Looks songs up the way a download is tagged, at most 25 a run, and writes nothing.
    /// What it found comes back in the run, each change marked picked or not, for a person to choose.</summary>
    [HttpPost("lookup")]
    public async Task<IActionResult> Lookup([FromBody] LookupRequest request, CancellationToken ct)
    {
        if (NavidromeUser() is not { } user) return NotAdmin();
        if (health.Refusal(user) is { } refused) return StatusCode(403, new { error = refused });
        var ids = (request.Ids ?? []).Where(id => !string.IsNullOrWhiteSpace(id)).Distinct(StringComparer.Ordinal).ToList();
        if (ids.Count == 0) return BadRequest(new { error = "No songs picked." });
        if (ids.Count > LibraryHealthService.LookupBatch)
            return BadRequest(new { error = $"At most {LibraryHealthService.LookupBatch} songs at a time." });
        var found = await health.ReportAsync(fresh: false, ct);
        var titles = found?.Songs.Where(song => ids.Contains(song.Id)).ToDictionary(song => song.Id, song => song.Title)
                     ?? new Dictionary<string, string>();
        var songs = ids.Select(id => (id, titles.GetValueOrDefault(id) ?? id)).ToList();
        if (health.StartLookup(songs, ParseCheck(request.Check), user) is { } busy) return Busy(busy);
        logger.LogInformation("{User} is looking up {Count} song(s) from Library health", user, ids.Count);
        return Accepted(new { started = true, run = RunView(health.Run) });
    }

    /// <summary>Ends the run going now after the song it is on.</summary>
    [HttpPost("stop")]
    public IActionResult Stop()
    {
        if (NavidromeUser() is null) return NotAdmin();
        return Ok(new { stopping = health.Stop() });
    }

    /// <summary>Puts back everything the last finished run did: removed copies out of the trash,
    /// tags, albums and covers as they were (each only while the file has not changed since).</summary>
    [HttpPost("undo")]
    public IActionResult Undo()
    {
        if (NavidromeUser() is not { } user) return NotAdmin();
        if (health.Refusal(user) is { } refused) return StatusCode(403, new { error = refused });
        if (health.StartUndo(user) is { } why) return Conflict(new { error = why });
        return Accepted(new { started = true, run = RunView(health.Run) });
    }

    private ConflictObjectResult Busy(HealthRun busy) => Conflict(new
    {
        error = $"{busy.Label} is still going ({busy.Done} of {busy.Total}). Try again once it is done.",
        run = RunView(busy),
    });

    // ---- shapes -----------------------------------------------------------------------------

    internal static string CheckName(HealthCheck check) => check switch
    {
        HealthCheck.Duplicates => "duplicates",
        HealthCheck.SplitAlbums => "splitAlbums",
        HealthCheck.NoLength => "noLength",
        HealthCheck.NoTrackNumber => "noTrackNumber",
        HealthCheck.NoAlbumArtist => "noAlbumArtist",
        HealthCheck.NoCover => "noCover",
        HealthCheck.NoYear => "noYear",
        _ => "noGenre",
    };

    internal static HealthCheck? ParseCheck(string? name) =>
        Enum.GetValues<HealthCheck>().Cast<HealthCheck?>().FirstOrDefault(check => string.Equals(CheckName(check!.Value), name, StringComparison.OrdinalIgnoreCase));

    private static string SetKey(DuplicateSet<LibrarySongRow> set, IHealthFields<LibrarySongRow> fields) =>
        "dup|" + string.Join(",", set.Copies.Select(fields.Id).Order(StringComparer.Ordinal));

    private static string AlbumKey(SplitAlbum<LibrarySongRow> album) =>
        "split|" + string.Join(",", album.Parts.Select(part => part.AlbumId).Order(StringComparer.Ordinal));

    /// <summary>What fixing one set of copies does, in a line, as the desktop says it.</summary>
    private static string FixLine(DuplicateFix<LibrarySongRow> fix, IHealthFields<LibrarySongRow> fields, bool canFill)
    {
        var gone = HealthWords.CountText(fix.Remove.Count, "copy", "copies");
        var filled = !canFill || fix.Fills.Count == 0
            ? ""
            : $" Fills in {string.Join(", ", fix.Fills.Select(change => HealthFixes.TagName(change.Tag).ToLowerInvariant()))}.";
        return $"Keeps {HealthFixes.CopyName(fix.Keep, fields)} and moves {gone} to the trash.{filled}";
    }

    private static object Change(TagChange change) => new
    {
        change.Tag,
        label = HealthFixes.TagName(change.Tag),
        change.Now,
        change.Value,
        change.From,
        words = change.Words,
    };

    private static object StepView(FixStep step) => new { step.Action, step.Id, step.Title, with = step.With };

    /// <summary>Every tag of every copy, side by side, and what the kept copy says after the fix.</summary>
    private static object DuplicateDiff(DuplicateSet<LibrarySongRow> set, DuplicateFix<LibrarySongRow> fix,
        IReadOnlyList<TagChange> changes, IHealthFields<LibrarySongRow> fields)
    {
        var copies = new[] { fix.Keep }.Concat(fix.Remove).ToList();
        var values = copies.Select(copy => HealthFixes.TagValues(copy, fields)).ToList();
        var after = changes.GroupBy(change => change.Tag).ToDictionary(group => group.Key, group => group.Last().Value);
        return new
        {
            copies = copies.Select(copy => new { id = copy.Id, kept = copy.Id == fix.Keep.Id, quality = HealthWords.QualityText(copy, fields) }).ToList(),
            rows = SongTagFields.All.Select(tag => new
            {
                tag,
                label = HealthFixes.TagName(tag),
                values = values.Select(tags => tags.GetValueOrDefault(tag)).ToList(),
                after = after.GetValueOrDefault(tag) ?? values[0].GetValueOrDefault(tag),
                changes = after.ContainsKey(tag),
            }).Where(row => row.values.Any(value => value is not null)).ToList(),
        };
    }

    /// <summary>For each part that moves, its album tags now and after the join.</summary>
    private static object JoinDiff(AlbumJoin<LibrarySongRow> join, IHealthFields<LibrarySongRow> fields)
    {
        var lead = HealthFixes.TagValues(join.Lead, fields);
        return join.Album.Parts.Skip(1).Select(part =>
        {
            var first = HealthFixes.TagValues(part.Songs[0], fields);
            return new
            {
                albumId = part.AlbumId,
                songs = part.Songs.Count,
                fields = new[] { SongTagFields.Album, SongTagFields.AlbumArtist, SongTagFields.Year }.Select(tag => new
                {
                    tag,
                    label = HealthFixes.TagName(tag),
                    now = first.GetValueOrDefault(tag),
                    after = tag == SongTagFields.Year ? lead.GetValueOrDefault(tag) ?? first.GetValueOrDefault(tag) : lead.GetValueOrDefault(tag),
                }).ToList(),
            };
        }).ToList();
    }

    private static object SongView(LibrarySongRow song, IHealthFields<LibrarySongRow> fields) => new
    {
        id = song.Id,
        title = song.Title,
        artist = song.Artist,
        album = song.Album,
        albumId = song.AlbumId,
        albumArtist = fields.AlbumArtist(song),
        year = song.Year is > 0 ? song.Year : null,
        genre = string.Join("; ", song.Genres),
        track = song.Track is > 0 ? song.Track : null,
        disc = song.Disc is > 0 ? song.Disc : null,
        seconds = fields.Seconds(song),
        quality = HealthWords.QualityText(song, fields),
        lossless = fields.Lossless(song),
        cover = song.HasCover,
        path = song.Path,
    };

    internal static object? RunView(HealthRun? run) => run is null ? null : new
    {
        run.Id,
        run.Kind,
        run.Label,
        check = run.Check is { } check ? CheckName(check) : null,
        run.State,
        run.Done,
        run.Total,
        run.Current,
        run.StartedUtc,
        run.FinishedUtc,
        run.Error,
        summary = run.Outcome?.Summary(),
        changed = run.Outcome?.Done.Select(step => step.Id).Distinct().ToList(),
        failed = run.Outcome?.Failed.Select(fail => new { fail.Step.Id, fail.Step.Title, fail.Why }).ToList(),
        lookups = run.Kind != "lookup" ? null : run.LookupsNow().Select(lookup => new
        {
            lookup.Id,
            lookup.Title,
            lookup.State,
            lookup.Detail,
            lookup.Origin,
            changes = lookup.Changes.Select(pair => new
            {
                pair.Change.Tag,
                label = HealthFixes.TagName(pair.Change.Tag),
                pair.Change.Now,
                pair.Change.Value,
                words = pair.Change.Words,
                pick = pair.Pick,
            }).ToList(),
        }).ToList(),
    };
}
