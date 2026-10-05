using Microsoft.AspNetCore.Mvc;
using Octo.Services.Admin;
using Octo.Services.Common;
using Octo.Services.CoverArt;
using Octo.Services.Imports;
using Octo.Services.Library;
using Octo.Services.Local;
using Octo.Services.Lyrics;
using Octo.Services.Metadata;
using Octo.Services.Soulseek;

namespace Octo.Controllers;

/// <summary>
/// Everything Octo is working on, in one shape, for the dashboard's progress toasts: a library
/// re-tag, a covers or lyrics run, better copies, hearted downloads, a Spotify read, a duplicate
/// scan, an slskd rescan and Navidrome's own scan. The dashboard asks often while something runs
/// and seldom while nothing does, so one cheap read replaces a poller per page.
///
/// A job that needs the Navidrome sign-in on its own page is listed only for a signed-in
/// browser. A job just finished stays listed for <see cref="RecentWindow"/>, so a toast can say
/// how it ended. Behind the same guard as the rest of /api/admin.
/// </summary>
[ApiController]
[Route("api/admin/activity")]
public class ActivityController(IServiceProvider services, BrowseSessionStore sessions) : ControllerBase
{
    /// <summary>How long a finished job stays listed.</summary>
    public static readonly TimeSpan RecentWindow = TimeSpan.FromMinutes(2);
    /// <summary>How long after a rescan from the dashboard slskd's share scan is watched.</summary>
    public static readonly TimeSpan RescanWatch = TimeSpan.FromMinutes(15);

    /// <summary>
    /// One job. <c>State</c> is running, done, stopped (cancelled or interrupted) or failed.
    /// <c>Done</c> of <c>Total</c> counts <c>Unit</c>; <c>Fraction</c> is set when only a share is
    /// known. <c>Page</c> is the dashboard page that shows it in full.
    /// </summary>
    public sealed record ActivityItem(string Id, string Kind, string Page, string Title, string State,
        int? Done = null, int? Total = null, string? Unit = null, double? Fraction = null, string? Detail = null,
        DateTime? StartedUtc = null, DateTime? FinishedUtc = null, string? Error = null);

    [HttpGet]
    public async Task<IActionResult> Get([FromHeader(Name = "X-Octo-Browse-Token")] string? token, CancellationToken ct)
    {
        var browse = Request.Cookies[AdminController.BrowseCookieName] ?? token;
        var signed = sessions.Validate(browse);
        var user = signed ? sessions.UserOf(browse) : null;
        var now = DateTime.UtcNow;
        var items = new List<ActivityItem>();

        if (signed)
        {
            if (services.GetService<GenreBackfillWorker>() is { } genre && Genre(genre.Current, genre.IsPending, now) is { } g) items.Add(g);
            if (services.GetService<CoverUpgradeWorker>() is { } covers && Covers(covers.Current, covers.IsBusy, now) is { } c) items.Add(c);
            if (services.GetService<LyricsLibraryWorker>() is { } lyrics && Lyrics(lyrics.Current, lyrics.IsRunning, now) is { } l) items.Add(l);
            if (services.GetService<UpgradeQueue>() is { } upgrades && Upgrades(upgrades.Snapshot(), now) is { } u) items.Add(u);
            if (user is not null && services.GetService<ImportService>() is { } imports) items.AddRange(Imports(imports.Overview(user), now));
        }
        if (services.GetService<AcquisitionTracker>() is { } acquisitions && Downloads(acquisitions.All(), now) is { } d) items.Add(d);
        if (services.GetService<DuplicateScanWorker>() is { } duplicates && Duplicates(duplicates, now) is { } dup) items.Add(dup);
        if (services.GetService<SoulseekSharing>() is { LastRescanUtc: { } rescan } sharing && now - rescan < RescanWatch)
        {
            try
            {
                var report = await sharing.ReportAsync(testPort: false, ct);
                items.Add(Rescan(report, rescan, now));
            }
            catch (Exception ex) when (ex is not OperationCanceledException) { /* slskd away: the Soulseek page says so */ }
        }
        if (services.GetService<ILocalLibraryService>() is { } library && await library.GetScanStatusAsync() is { Scanning: true } scan)
            items.Add(new ActivityItem("navidrome-scan", "scan", "library", "Navidrome is scanning your library", "running",
                Done: scan.Count, Unit: "songs"));

        return Ok(new { items, signedIn = signed });
    }

    internal static string Ended(string status) => status switch
    {
        "Completed" => "done",
        "Failed" => "failed",
        _ => "stopped",
    };

    private static bool Recent(DateTime? finished, DateTime now) => finished is { } f && now - f < RecentWindow;

    internal static ActivityItem? Genre(GenreBackfillRun run, bool pending, DateTime now)
    {
        var title = run.DryRun ? "Previewing genre changes" : "Re-tagging genres";
        if (pending && run.Status != GenreBackfillStatus.Running)
            return new ActivityItem("genre:pending", "genre", "tags", title, "running", Detail: "Reading your library");
        if (run.Status == GenreBackfillStatus.Running)
            return new ActivityItem($"genre:{run.RunId}", "genre", "tags", title, "running", run.Processed, run.Total, "songs",
                Detail: run.Changed > 0 ? $"{run.Changed:N0} changed so far" : null, StartedUtc: run.StartedUtc);
        if (run.Status is GenreBackfillStatus.Idle || !Recent(run.FinishedUtc, now)) return null;
        var state = Ended(run.Status.ToString());
        var detail = state == "done"
            ? run.DryRun ? $"{run.Changed:N0} songs would change" : $"{run.Changed:N0} songs re-tagged"
            : run.Reason;
        return new ActivityItem($"genre:{run.RunId}", "genre", "tags", title, state, run.Processed, run.Total, "songs",
            Detail: detail, StartedUtc: run.StartedUtc, FinishedUtc: run.FinishedUtc, Error: state == "failed" ? run.Reason : null);
    }

    internal static ActivityItem? Covers(CoverUpgradeRun run, bool busy, DateTime now)
    {
        var title = run.Undo ? "Putting covers back"
            : run.Mode switch
            {
                CoverUpgradeMode.Scan => "Looking for soft covers",
                CoverUpgradeMode.Preview => "Finding better covers",
                _ => "Replacing covers",
            };
        var byAlbum = run.Mode != CoverUpgradeMode.Scan && run.AlbumsTotal > 0;
        int done = byAlbum ? run.AlbumsDone : run.Processed, total = byAlbum ? run.AlbumsTotal : run.Total;
        var unit = byAlbum ? "albums" : "folders";
        if (busy && run.Status != CoverUpgradeStatus.Running)
            return new ActivityItem("covers:pending", "covers", "covers", title, "running", Detail: "Getting started");
        if (run.Status == CoverUpgradeStatus.Running)
            return new ActivityItem($"covers:{run.RunId}", "covers", "covers", title, "running", done, total, unit,
                StartedUtc: run.StartedUtc);
        if (run.Status is CoverUpgradeStatus.Idle || !Recent(run.FinishedUtc, now)) return null;
        var state = Ended(run.Status.ToString());
        var detail = state != "done" ? run.Reason
            : run.Undo ? "The old covers are back"
            : run.Mode switch
            {
                CoverUpgradeMode.Scan => run.Soft == 0 ? "Every cover is sharp" : $"{run.Soft:N0} albums have soft covers",
                CoverUpgradeMode.Preview => $"{run.Upgraded:N0} larger covers found",
                _ => $"{run.Files:N0} covers replaced",
            };
        return new ActivityItem($"covers:{run.RunId}", "covers", "covers", title, state, done, total, unit,
            Detail: detail, StartedUtc: run.StartedUtc, FinishedUtc: run.FinishedUtc, Error: state == "failed" ? run.Reason : null);
    }

    internal static ActivityItem? Lyrics(LyricsLibraryRun run, bool running, DateTime now)
    {
        var title = run.Mode switch
        {
            LyricsLibraryMode.Undo => "Putting lyrics back",
            LyricsLibraryMode.Save => "Saving lyrics",
            LyricsLibraryMode.Preview => "Finding better lyrics",
            LyricsLibraryMode.Scan => "Reading your lyrics",
            _ => "Finding lyrics",
        };
        if (running && run.Status != LyricsLibraryStatus.Running)
            return new ActivityItem("lyrics:pending", "lyrics", "lyrics", title, "running", Detail: "Getting started");
        if (run.Status == LyricsLibraryStatus.Running)
            return new ActivityItem($"lyrics:{run.RunId}", "lyrics", "lyrics", title, "running", run.Processed, run.Total, "songs",
                Detail: run.Written > 0 ? $"{run.Written:N0} written so far" : null, StartedUtc: run.StartedUtc);
        if (run.Status is LyricsLibraryStatus.Idle || !Recent(run.FinishedUtc, now)) return null;
        var state = Ended(run.Status.ToString());
        var detail = state != "done" ? run.Reason
            : run.Mode is LyricsLibraryMode.Scan or LyricsLibraryMode.Preview ? $"{run.Processed:N0} songs read"
            : run.Mode == LyricsLibraryMode.Undo ? "The old lyrics are back"
            : $"{run.Written:N0} songs got lyrics";
        return new ActivityItem($"lyrics:{run.RunId}", "lyrics", "lyrics", title, state, run.Processed, run.Total, "songs",
            Detail: detail, StartedUtc: run.StartedUtc, FinishedUtc: run.FinishedUtc, Error: state == "failed" ? run.Reason : null);
    }

    /// <summary>Better copies of lossy songs: one item for the whole queue while any is open.</summary>
    internal static ActivityItem? Upgrades(IReadOnlyList<UpgradeJob> jobs, DateTime now)
    {
        var open = jobs.Where(j => UpgradeStates.Open(j.State)).ToList();
        if (open.Count == 0)
        {
            var ended = jobs.Where(j => !UpgradeStates.Open(j.State) && now - j.UpdatedUtc < RecentWindow).ToList();
            if (ended.Count == 0) return null;
            var upgraded = ended.Count(j => j.State == UpgradeStates.Upgraded);
            var failed = ended.Count(j => j.State == UpgradeStates.Failed);
            return new ActivityItem($"upgrades:{ended.Max(j => j.UpdatedUtc).Ticks}", "upgrades", "lossy", "Finding better copies",
                failed > 0 && upgraded == 0 ? "failed" : "done", ended.Count, ended.Count, "songs",
                Detail: $"{upgraded:N0} upgraded" + (failed > 0 ? $", {failed:N0} failed" : ""),
                FinishedUtc: ended.Max(j => j.UpdatedUtc));
        }
        var working = open.FirstOrDefault(j => j.State == UpgradeStates.Working);
        var detail = working is not null
            ? $"{working.Artist} · {working.Title}" + (open.Count > 1 ? $", {open.Count - 1:N0} more waiting" : "")
            : $"{open.Count:N0} waiting";
        return new ActivityItem("upgrades", "upgrades", "lossy", "Finding better copies", "running",
            Detail: detail, StartedUtc: open.Min(j => j.StartedUtc ?? j.QueuedUtc));
    }

    /// <summary>Hearted downloads: one item while any is on its way, the song in front named.</summary>
    internal static ActivityItem? Downloads(IReadOnlyList<AcquisitionSnapshot> all, DateTime now)
    {
        var moving = all.Where(a => a.State is not (AcquisitionState.Done or AcquisitionState.Failed)).ToList();
        if (moving.Count == 0)
        {
            var landed = all.Where(a => a.State == AcquisitionState.Done && now - a.UpdatedAt < RecentWindow).ToList();
            var failed = all.Where(a => a.State == AcquisitionState.Failed && now - a.UpdatedAt < RecentWindow).ToList();
            if (landed.Count + failed.Count == 0) return null;
            var last = landed.Concat(failed).MaxBy(a => a.UpdatedAt)!;
            var title = landed.Count == 1 && failed.Count == 0 ? $"Added {landed[0].Title}"
                : landed.Count > 0 ? $"Added {landed.Count:N0} songs" : $"{failed[0].Title} could not be fetched";
            var detail = landed.Count == 1 && failed.Count == 0 ? landed[0].Artist
                : failed.Count > 0 && landed.Count > 0 ? $"{failed.Count:N0} could not be fetched" : failed.Count > 0 ? failed[0].Error : null;
            return new ActivityItem($"downloads:{last.UpdatedAt.Ticks}", "downloads", "fetched", title,
                landed.Count == 0 ? "failed" : "done", Detail: detail, FinishedUtc: last.UpdatedAt,
                Error: landed.Count == 0 ? failed[0].Error : null);
        }
        var front = moving.FirstOrDefault(a => a.State == AcquisitionState.Downloading) ?? moving[0];
        var name = $"{front.Artist} · {front.Title}";
        return new ActivityItem("downloads", "downloads", "fetched",
            moving.Count == 1 ? $"Fetching {front.Title}" : $"Fetching {moving.Count:N0} songs", "running",
            Fraction: front.State == AcquisitionState.Downloading ? front.Progress : null,
            Detail: moving.Count == 1 ? $"{front.Artist} · {StateWords(front.State)}" : $"{name} · {StateWords(front.State)}",
            StartedUtc: moving.Min(a => a.StartedAt));
    }

    private static string StateWords(AcquisitionState state) => state switch
    {
        AcquisitionState.Queued => "waiting its turn",
        AcquisitionState.Searching => "searching",
        AcquisitionState.Downloading => "downloading",
        AcquisitionState.Verifying => "checking the file",
        AcquisitionState.Importing => "adding it to your library",
        _ => state.ToString().ToLowerInvariant(),
    };

    internal static IEnumerable<ActivityItem> Imports(ImportOverview overview, DateTime now)
    {
        if (overview.Reading.Busy)
            yield return new ActivityItem("imports:read", "imports", "imports", "Reading your Spotify lists", "running",
                Detail: overview.Reading.Step);
        else if (overview.Reading.Error is { } error && Recent(overview.Reading.FinishedUtc, now))
            yield return new ActivityItem($"imports:read:{overview.Reading.FinishedUtc!.Value.Ticks}", "imports", "imports",
                "Reading your Spotify lists", "failed", Detail: error, FinishedUtc: overview.Reading.FinishedUtc, Error: error);
        else if (Recent(overview.Reading.FinishedUtc, now))
            yield return new ActivityItem($"imports:read:{overview.Reading.FinishedUtc!.Value.Ticks}", "imports", "imports",
                "Read your Spotify lists", "done", FinishedUtc: overview.Reading.FinishedUtc);

        var trickle = overview.Trickle;
        if (trickle.Downloading > 0)
            yield return new ActivityItem("imports:trickle", "imports", "imports", "Fetching songs from your lists", "running",
                Detail: $"{trickle.Done:N0} done, {trickle.Queued:N0} to go");
    }

    internal static ActivityItem? Duplicates(DuplicateScanWorker worker, DateTime now)
    {
        if (worker.IsScanning || worker.IsRequested)
            return new ActivityItem("duplicates", "duplicates", "libraryactions", "Looking for duplicates", "running",
                Detail: worker.IsScanning ? "Reading every song's recording" : "Starting", StartedUtc: worker.ScanStartedUtc);
        if (worker.LastResult is not { } last || now - last.AtUtc >= RecentWindow) return null;
        var detail = last.Groups == 0 ? "No duplicates" : $"{last.Groups:N0} sets of duplicates, {last.Added:N0} new";
        return new ActivityItem($"duplicates:{last.AtUtc.Ticks}", "duplicates", "libraryactions", "Looked for duplicates", "done",
            Detail: last.Complete ? detail : detail + " (part of the library)", FinishedUtc: last.AtUtc);
    }

    /// <summary>slskd starts a rescan a moment after it is asked, so the first seconds read as starting.</summary>
    internal static readonly TimeSpan RescanStart = TimeSpan.FromSeconds(15);

    internal static ActivityItem Rescan(SharingReport report, DateTime requested, DateTime now)
    {
        var id = $"share-rescan:{requested.Ticks}";
        if (report.ScanFailed)
            return new ActivityItem(id, "share", "soulseek", "slskd could not rescan your share", "failed",
                Error: "slskd stopped the scan. Its log says why.");
        if (report.Scanning || now - requested < RescanStart)
            return new ActivityItem(id, "share", "soulseek", "slskd is rescanning your share", "running",
                Fraction: report.Scanning ? report.ScanProgress : null, Detail: report.Scanning ? null : "Starting",
                StartedUtc: requested);
        // No finish time: slskd does not say when it ended, so only a browser that saw it running says done.
        return new ActivityItem(id, "share", "soulseek", "slskd rescanned your share", "done",
            Detail: report.Files is { } files ? $"{files:N0} files shared" : null);
    }
}
