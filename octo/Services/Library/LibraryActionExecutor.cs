using System.Collections.Concurrent;
using Microsoft.Extensions.Options;
using Octo.Models.Settings;
using Octo.Services.Common;
using Octo.Services.Local;
using Octo.Services.Soulseek;

namespace Octo.Services.Library;

/// <param name="OnReplacementQueued">Told the provider and external id of the replacement download
/// the moment it is queued, so the upgrade queue can follow that download's progress.</param>
/// <param name="OnlySource">The one source to fetch the replacement from, for a copy someone picked
/// in Find songs; no other source is tried after it.</param>
/// <param name="OnlyACopy">A Delete of one copy of a song the library keeps another of (Library
/// health's duplicates). The song is still wanted, so it is not refused when asked for again.</param>
public sealed record LibraryActionRequest(LibraryAction Action, string NavidromeId, string Username,
    Octo.Services.Subsonic.SubsonicCredential? Credential = null,
    Action<string, string>? OnReplacementQueued = null,
    DownloadSource? OnlySource = null, bool OnlyACopy = false);

/// <summary>
/// Code says WHY, for callers that act on the reason: the upgrade queue waits for Soulseek on one
/// and reports "no FLAC found" on the other. Detail stays the words for people.
/// </summary>
public sealed record LibraryActionOutcome(LibraryActionState State, string? Detail, string? Code = null)
{
    /// <summary>
    /// Whether the request has been consumed. Anything else leaves the track in the playlist
    /// and the rating set, so a request is never silently swallowed and retries if the
    /// operator fixes whatever blocked it.
    /// </summary>
    public bool Consumed => State is LibraryActionState.Applied or LibraryActionState.Skipped;

    /// <summary>For a replacement that went in: the new file, so a caller can say what it is.</summary>
    public string? NewPath { get; init; }

    /// <summary>For a replacement that went in: where the original waits in quarantine.</summary>
    public string? QuarantinePath { get; init; }
}

public static class LibraryActionCodes
{
    /// <summary>slskd is not logged in to Soulseek, so a replacement could only fail.</summary>
    public const string SoulseekOffline = "soulseekOffline";

    /// <summary>The search found no copy good enough to replace the song with.</summary>
    public const string NoReplacement = "noReplacement";
}

/// <summary>
/// Applies one library action. The only code in this feature that touches a file.
///
/// Every path through here either proves what it is acting on or does nothing. The resolver
/// supplies the proof, the quarantine makes a wrong answer recoverable, and the journal makes
/// a half-finished action reconcilable rather than repeatable.
/// </summary>
public sealed class LibraryActionExecutor
{
    private readonly NavidromeSongPathResolver _resolver;
    private readonly LibraryActionQuarantine _quarantine;
    private readonly LibraryActionJournal _journal;
    private readonly ILocalLibraryService _library;
    private readonly ExternalIdRegistry _ids;
    private readonly RejectedPeerRegistry _rejectedPeers;
    private readonly TrackAcquisitionQueue _acquisitions;
    private readonly IOptionsMonitor<LibraryActionSettings> _settings;
    private readonly IOptionsMonitor<SoulseekSettings> _soulseek;
    private readonly IOptionsMonitor<SubsonicSettings> _subsonicSettings;
    private readonly ILogger<LibraryActionExecutor> _logger;
    private readonly NoticeQueue? _notices;
    private readonly Octo.Services.Fingerprint.SpectrumAnalyzer? _spectrum;
    private readonly StarOnArrival? _stars;
    private readonly ISoulseekLink? _soulseekLink;
    private readonly UpgradeSources? _sources;
    private int _reconciled;

    // The songs a library action is working on right now, by the original's full path. The
    // rating, playlist and weekly upgrade workers all call ApplyAsync, and two replacements of
    // one song at once share one download: the second could quarantine or delete what the
    // first had just put in place. A set rather than a lock per song, because nobody waits for
    // it (the second action is skipped), so an entry can simply be removed when the action ends.
    private readonly ConcurrentDictionary<string, byte> _busy = new(
        OperatingSystem.IsWindows() ? StringComparer.OrdinalIgnoreCase : StringComparer.Ordinal);
    internal const string BusyText = "Another action on this song is still running, so this one was skipped.";
    internal const string JoinedText = "Another download of this song was already running, so nothing changed.";

    internal TimeSpan HistoryPoll { get; set; } = TimeSpan.FromSeconds(15);
    internal int HistoryAttempts { get; set; } = 40;
    /// <summary>Navidrome shows this id at this file. Tests set it; otherwise the resolver.</summary>
    internal Func<string, string, CancellationToken, Task<bool>>? ShowsAt { get; set; }
    internal Func<Task<bool>>? ForceScan { get; set; }
    internal const string HistoryKeptText = "Navidrome kept it as the same song, so its plays, favorites and playlist places stayed with it.";
    internal const string HistoryLostText = "Navidrome took it for a new song, so its plays and playlist places stayed with the old entry.";
    private sealed record Replaced(LibraryActionOutcome Outcome, string? QuarantinePath, string? NewPath);

    public LibraryActionExecutor(NavidromeSongPathResolver resolver, LibraryActionQuarantine quarantine,
        LibraryActionJournal journal, ILocalLibraryService library, ExternalIdRegistry ids,
        RejectedPeerRegistry rejectedPeers, TrackAcquisitionQueue acquisitions,
        IOptionsMonitor<LibraryActionSettings> settings, IOptionsMonitor<SoulseekSettings> soulseek,
        IOptionsMonitor<SubsonicSettings> subsonicSettings,
        ILogger<LibraryActionExecutor> logger,
        NoticeQueue? notices = null,
        Octo.Services.Fingerprint.SpectrumAnalyzer? spectrum = null,
        StarOnArrival? stars = null,
        ISoulseekLink? soulseekLink = null,
        UpgradeSources? sources = null)
    {
        _soulseekLink = soulseekLink;
        _sources = sources;
        _notices = notices;
        _spectrum = spectrum;
        _stars = stars;
        _resolver = resolver;
        _quarantine = quarantine;
        _journal = journal;
        _library = library;
        _ids = ids;
        _rejectedPeers = rejectedPeers;
        _acquisitions = acquisitions;
        _settings = settings;
        _soulseek = soulseek;
        _subsonicSettings = subsonicSettings;
        _logger = logger;
    }

    public async Task<LibraryActionOutcome> ApplyAsync(LibraryActionRequest request, CancellationToken ct = default)
    {
        var settings = _settings.CurrentValue;

        if (!settings.Enabled) return new(LibraryActionState.Skipped, "Library actions are off.");

        // Reconcile once per process before the first action. The playlist worker does this at
        // startup, but an install with only star ratings enabled never starts that worker, so an
        // interrupted action there was never looked at again.
        if (Interlocked.Exchange(ref _reconciled, 1) == 0)
            _journal.Reconcile(path => _quarantine.Restore(path).Moved);
        if (!settings.IsAllowed(request.Username))
            return new(LibraryActionState.Skipped, $"{request.Username} is not on the allowlist.");

        var definition = settings.EffectiveActions()
            .FirstOrDefault(entry => entry.Action == request.Action && entry.Enabled);
        if (definition is null)
            return new(LibraryActionState.Skipped, $"{request.Action} is not enabled.");

        // Keep is an answer, not an operation. It touches no file, so there is nothing to rehearse
        // and nothing to prove about which file it is.
        if (request.Action == LibraryAction.Keep)
        {
            var kept = _notices?.MarkKept(request.Username, request.NavidromeId);
            // Not something Octo asked about: nothing to answer and nothing to record. Five stars
            // on any other track is just a rating, and the journal holds real actions.
            if (kept is null) return new(LibraryActionState.Skipped, "Octo had not asked about this track.");

            const string keptDetail = "Kept. Octo will not ask about this track again.";
            _journal.Record(Entry(request, null, LibraryActionState.Applied, keptDetail, dryRun: false) with
            {
                Key = LibraryActionJournal.MakeKey(request.Action, request.NavidromeId, $"keep:{DateTime.UtcNow.Ticks}"),
                Title = kept.Title,
                Artist = kept.Artist,
                Album = kept.Album ?? "",
            });
            _logger.LogInformation("{User} kept '{Artist} - {Title}'", request.Username, kept.Artist, kept.Title);
            return new(LibraryActionState.Applied, keptDetail);
        }

        var resolved = await _resolver.ResolveAsync(request.NavidromeId, ct);
        if (resolved is null)
        {
            // Never a success and never a delete. The request stays where it is, so fixing a
            // mount and waiting makes it work rather than requiring the user to ask again.
            var detail = "Could not work out which file this is, so nothing was touched.";
            _journal.Record(Entry(request, null, LibraryActionState.Unresolved, detail, settings.DryRun));
            return new(LibraryActionState.Unresolved, detail);
        }

        var fingerprint = LibraryActionJournal.Fingerprint(
            resolved.SizeBytes, File.GetLastWriteTimeUtc(resolved.AbsolutePath));
        if (_journal.AlreadyApplied(request.Action, request.NavidromeId, fingerprint))
            return new(LibraryActionState.Skipped, "Already done.");

        var key = LibraryActionJournal.MakeKey(request.Action, request.NavidromeId, fingerprint);
        var pending = Entry(request, resolved, LibraryActionState.Pending, null, settings.DryRun) with { Key = key };

        if (settings.DryRun)
        {
            var detail = $"Dry run: would {Describe(request.Action)} {resolved.AbsolutePath}";
            _journal.Record(pending with { State = LibraryActionState.Rehearsed, Detail = detail });
            _logger.LogInformation("Library action {Action} by {User} (DRY RUN): would {Verb} {Path}",
                request.Action, request.Username, Describe(request.Action), resolved.AbsolutePath);
            // Deliberately NOT consumed: the same set replays every cycle so the operator sees
            // a stable list rather than a rehearsal that quietly emptied the playlist.
            return new(LibraryActionState.Rehearsed, detail);
        }

        // Better quality with every source out could only fail, and before this it was journaled as
        // "no FLAC found" every sweep. Not consumed, so the request stays and runs once slskd is
        // back. Nothing is written and no file is touched. With Lidarr also set up, a Soulseek
        // outage just means Lidarr alone is asked.
        if (request.Action == LibraryAction.BetterQuality)
        {
            var sources = await SourcesForAsync(request.Action, ct, request.OnlySource);
            if (sources.Count == 0 && await WaitingForSoulseekAsync(ct))
                return new(LibraryActionState.Failed, SoulseekLink.OfflineText, LibraryActionCodes.SoulseekOffline);
            if (sources.Count == 0)
                return new(LibraryActionState.Failed, "Better quality has no source set up: it needs slskd or Lidarr.");
        }

        // Taken before the Pending entry is written: a second action on the same file content
        // has the same journal key and would overwrite the first one's entry.
        var songKey = Path.GetFullPath(resolved.AbsolutePath);
        if (!_busy.TryAdd(songKey, 0))
        {
            _journal.Record(Entry(request, resolved, LibraryActionState.Skipped, BusyText, dryRun: false) with
            {
                Key = LibraryActionJournal.MakeKey(request.Action, request.NavidromeId, $"busy:{DateTime.UtcNow.Ticks}"),
            });
            _logger.LogInformation("Library action {Action} by {User} skipped: another action on {Path} is still running",
                request.Action, request.Username, resolved.AbsolutePath);
            return new(LibraryActionState.Skipped, BusyText);
        }
        try
        {
            return await ApplyOnFileAsync(request, resolved, key, pending, ct);
        }
        finally
        {
            _busy.TryRemove(songKey, out _);
        }
    }

    private async Task<LibraryActionOutcome> ApplyOnFileAsync(LibraryActionRequest request, ResolvedSongFile resolved,
        string key, LibraryActionEntry pending, CancellationToken ct)
    {
        // Read now, while Navidrome still knows the song here. Only the acting user's own
        // favorite can be read, with their own sign-in; anyone else's is out of reach.
        var carryStar = await WasStarredByRequesterAsync(request);

        // Written BEFORE the file is touched. A crash between the two leaves this Pending, and
        // startup reconciles it against the filesystem rather than blindly re-running.
        _journal.Record(pending);
        if (!_journal.Flush())
        {
            // The whole safety story rests on this entry being on disk before the file moves.
            const string unrecorded = "Could not write the action journal, so the file was not touched.";
            _journal.Complete(key, LibraryActionState.Failed, unrecorded);
            return new(LibraryActionState.Failed, unrecorded);
        }

        var musicRoot = _resolver.MusicRoot();
        LibraryActionOutcome outcome;
        Replaced? replaced = null;
        string? quarantinePath = null;
        if (request.Action == LibraryAction.Delete)
        {
            var moved = _quarantine.Move(resolved, musicRoot, request.Action, request.Username);
            if (!moved.Moved)
            {
                _journal.Complete(key, LibraryActionState.Failed, moved.Error);
                return new(LibraryActionState.Failed, moved.Error);
            }
            quarantinePath = moved.QuarantinePath;
            _journal.Complete(key, LibraryActionState.Pending, "Moved to quarantine; finishing.", quarantinePath);
            _journal.Flush();
            await _library.ForgetMappingAsync(resolved.AbsolutePath);
            outcome = new(LibraryActionState.Applied, request.OnlyACopy
                ? "Removed this copy. The song stays in the library."
                : "Removed. It will not be downloaded again.");
        }
        else
        {
            // The original stays in place, playable, until its replacement has passed; the two
            // swap places in one moment (W8).
            replaced = await ReacquireAsync(request, resolved, key, musicRoot, ct);
            (outcome, quarantinePath) = (replaced.Outcome, replaced.QuarantinePath);
        }

        _journal.Complete(key, outcome.State, outcome.Detail, quarantinePath);
        if (outcome.State == LibraryActionState.Applied) _notices?.MarkActed(request.NavidromeId);
        _journal.Flush();
        // Off this call, which the rating and playlist workers wait on: it can take ten minutes.
        if (outcome.State == LibraryActionState.Applied && replaced?.NewPath is { } newPath)
            using (ExecutionContext.SuppressFlow())
                _ = Task.Run(() => ConfirmHistoryKeptAsync(request, resolved, newPath, key, carryStar));
        return outcome with { NewPath = replaced?.NewPath, QuarantinePath = quarantinePath };
    }

    private async Task<Replaced> ReacquireAsync(LibraryActionRequest request, ResolvedSongFile original,
        string key, string musicRoot, CancellationToken ct)
    {
        if (request.Action == LibraryAction.WrongSong) BlacklistSource(original, request);

        // Read while the original is in place: Navidrome built this song's ids from these tags.
        var identity = KeptIdentityTags.Read(original.AbsolutePath, original.AlbumArtist);
        // What the original looked like when the action started. A download takes minutes, and
        // whatever is at that path by the end may no longer be the file this action was about.
        var startedLastWrite = File.GetLastWriteTimeUtc(original.AbsolutePath);
        string? quarantinePath = null;
        ReplacementHandoff? handoff = null;
        // What each source tried before the last one said, for the words when all of them miss.
        var misses = new List<string>();
        string Earlier() => misses.Count == 0 ? "" : $" Before that, {string.Join("; ", misses)}.";

        // Judges the new file and, only when it passes, moves the original out. Called by the
        // download just before the replacement moves in, or here for one that ran without the
        // handoff, so a scan never sees the song missing for the length of a download.
        async Task<string?> AdmitAsync(string? candidate)
        {
            var problem = Unacceptable(request.Action, candidate, original);
            if (problem is null && request.Action == LibraryAction.BetterQuality)
                problem = NotReallyLossless(await SpectrumOfAsync(candidate!));
            if (problem is not null) return problem;
            var now = new FileInfo(original.AbsolutePath);
            if (!now.Exists || now.Length != original.SizeBytes || now.LastWriteTimeUtc != startedLastWrite)
                return "found the original changed while it was downloading";
            var moved = _quarantine.Move(original, musicRoot, request.Action, request.Username);
            if (!moved.Moved) return $"could not take the original's place ({moved.Error})";
            quarantinePath = moved.QuarantinePath;
            // On disk before the replacement moves in: a crash in between puts the original back.
            _journal.Complete(key, LibraryActionState.Pending, "Moved to quarantine; finishing.", quarantinePath);
            _journal.Flush();
            // Only now that the original is out: a refused replacement leaves it with its
            // mapping, and so with the record of who sent it.
            await _library.ForgetMappingAsync(original.AbsolutePath);
            return null;
        }

        // Written the moment the replacement is in the original's place, so a restart after it
        // finds the swap done rather than putting the original back beside it.
        void Revealed(string path)
        {
            _journal.Complete(key, LibraryActionState.Pending, "Replacement placed; finishing.", quarantinePath, revealedPath: path);
            _journal.Flush();
        }

        try
        {
            var routing = new SoulseekRouting
            {
                Kind = RoutingKind.Song, Artist = original.Artist, Title = original.Title,
                Album = original.Album, Duration = original.DurationSeconds,
            };
            var externalId = _ids.Register(routing);
            try { request.OnReplacementQueued?.Invoke(SoulseekMetadataService.ProviderName, externalId); }
            catch (Exception ex) { _logger.LogDebug("Replacement listener failed: {M}", ex.Message); }
            if (identity is null)
                _logger.LogWarning("Library action {Action}: could not read the tags of {Path}, so Navidrome will treat its replacement as a new song",
                    request.Action, original.AbsolutePath);
            handoff = identity is null ? null : HandoffFor(original, identity, AdmitAsync, Revealed);
            // DownloadSongInternalAsync hands back a file it has a mapping for instead of
            // fetching. The handoff skips that, so only a download without one needs this.
            if (handoff is null) await _library.ForgetMappingAsync(original.AbsolutePath);

            // In order, and on to the next when one finds nothing or its copy fails the checks:
            // Soulseek first, then Lidarr, for Better quality with both set up.
            var sources = await SourcesForAsync(request.Action, ct, request.OnlySource);
            var upgradeSearch = request.Action == LibraryAction.BetterQuality;
            string? replacement = null;
            for (var attempt = 0; ; attempt++)
            {
                var source = sources[attempt];
                try
                {
                    replacement = await _acquisitions.Enqueue(
                        SoulseekMetadataService.ProviderName, externalId, isStar: true,
                        triggerAlbumDownload: false, forcePermanent: true,
                        sourceOverride: source, notifyOnFailure: false,
                        requestedBy: _subsonicSettings.CurrentValue.RecordRequestedBy ? request.Username : null,
                        upgradeSearch: upgradeSearch, replacement: handoff);
                    break;
                }
                catch (Exception ex) when (attempt < sources.Count - 1 && handoff?.RevealedPath is null
                                           && ex is FileNotFoundException or ReplacementRejectedException
                                               or InvalidOperationException)
                {
                    var why = ex is ReplacementRejectedException rejected ? rejected.Problem : ex.Message;
                    misses.Add($"{UpgradeSources.Word(source ?? DownloadSource.Soulseek)}: {why}");
                    _logger.LogInformation("Library action {Action}: {Source} had no copy of '{Artist} - {Title}' ({Why}); trying {Next}",
                        request.Action, UpgradeSources.Word(source ?? DownloadSource.Soulseek), original.Artist, original.Title,
                        why, UpgradeSources.Word(sources[attempt + 1] ?? DownloadSource.Soulseek));
                }
            }

            // A handoff that was never used: this joined a download of the same song already in
            // flight, whose file belongs to whoever started it. Judging it here could quarantine
            // or delete a replacement that action had just put in place, so nothing is touched.
            if (handoff is not null && handoff.RevealedPath is null)
            {
                LogRefused(request, original, "was another action's download");
                return new(new(LibraryActionState.Failed, JoinedText), null, null);
            }
            // Without the handoff the file is already in the library under its own name: judged now.
            if (handoff is null && await AdmitAsync(replacement) is { } late)
            {
                LogRefused(request, original, late);
                if (!string.IsNullOrEmpty(replacement)) TryDelete(replacement);
                return new(new(LibraryActionState.Failed, $"The replacement {late}, so nothing changed."), null, null);
            }
            if (!_settings.CurrentValue.KeepReplacedOriginals) TryDelete(quarantinePath!);
            return new(new(LibraryActionState.Applied, $"Replaced with {Path.GetFileName(replacement)}."),
                quarantinePath, replacement);
        }

        catch (ReplacementRejectedException rejected)
        {
            // Refused before it was ever in the library; the original never moved.
            LogRefused(request, original, rejected.Problem);
            return new(new(LibraryActionState.Failed, $"The replacement {rejected.Problem}, so nothing changed.{Earlier()}"), null, null);
        }
        catch (Exception ex)
        {
            if (handoff?.RevealedPath is { } revealed)
            {
                // The replacement already took the original's place and only the bookkeeping
                // after it failed. Putting the original back now would undo a finished swap.
                _logger.LogWarning("Library action {Action}: the replacement for '{Artist} - {Title}' is in place at {Path}, but what followed failed: {Message}",
                    request.Action, original.Artist, original.Title, revealed, ex.Message);
                if (!_settings.CurrentValue.KeepReplacedOriginals && quarantinePath is not null) TryDelete(quarantinePath);
                return new(new(LibraryActionState.Applied, $"Replaced with {Path.GetFileName(revealed)}."), quarantinePath, revealed);
            }
            // A search that found nothing usable throws FileNotFoundException, passed through the
            // queue unchanged, and the upgrade queue reports exactly that case as "no FLAC found".
            if (quarantinePath is null)
                return new(new(LibraryActionState.Failed, $"Could not find a replacement ({ex.Message}), so nothing changed.{Earlier()}",
                    ex is FileNotFoundException ? LibraryActionCodes.NoReplacement : null), null, null);
            // Out, and nothing moved in: back to its exact path, whose row Navidrome still has.
            return new(RestoreOriginal(quarantinePath, $"Could not finish the replacement ({ex.Message}), so nothing changed."),
                quarantinePath, null);
        }
    }

    internal static ReplacementHandoff HandoffFor(ResolvedSongFile original, KeptIdentity identity,
        Func<string, Task<string?>> admit, Action<string>? revealed = null) =>
        new() { OriginalPath = original.AbsolutePath, Identity = identity, BeforeReveal = admit, OnRevealed = revealed };

    private void LogRefused(LibraryActionRequest request, ResolvedSongFile original, string problem) =>
        _logger.LogInformation("Library action {Action}: the replacement for '{Artist} - {Title}' {Problem}; the original stays",
            request.Action, original.Artist, original.Title, problem);

    /// <summary>
    /// One scan, then watch the ORIGINAL id until it shows the replacement (W8): proof that the
    /// plays, favorites and playlist places stayed with the song. About ten minutes, scanning
    /// again every two in case Navidrome was busy. If not shown, the rater's favorite (W6).
    /// </summary>
    internal async Task<bool> ConfirmHistoryKeptAsync(LibraryActionRequest request, ResolvedSongFile original,
        string newPath, string key, bool carryStar)
    {
        var kept = false;
        try
        {
            var showsAt = ShowsAt ?? _resolver.ShowsAtAsync;
            var scan = ForceScan ?? (() => _library.TriggerLibraryScanAsync(force: true));
            for (var attempt = 0; attempt < Math.Max(1, HistoryAttempts) && !kept; attempt++)
            {
                if (attempt % 8 == 0) await scan();
                await Task.Delay(HistoryPoll);
                kept = await showsAt(original.NavidromeId, newPath, CancellationToken.None);
            }
        }
        catch (Exception ex) { _logger.LogDebug("History check for {Id} failed: {M}", original.NavidromeId, ex.Message); }

        _journal.Complete(key, LibraryActionState.Applied,
            $"Replaced with {Path.GetFileName(newPath)}. {(kept ? HistoryKeptText : HistoryLostText)}", historyKept: kept);
        _journal.Flush();
        if (kept)
        {
            _logger.LogInformation("Navidrome kept '{Artist} - {Title}' as the same song after {Action}", original.Artist, original.Title, request.Action);
            return true;
        }
        _logger.LogWarning("Navidrome did not keep '{Artist} - {Title}' as the same song after {Action}; its plays and playlist places stay with the old entry",
            original.Artist, original.Title, request.Action);
        if (carryStar && _stars is not null && request.Credential is { } credential)
            _stars.StarWhenVisible(credential, request.Username, original.Artist, original.Title, newPath);
        return false;
    }

    /// <summary>
    /// Where a replacement may come from, in the order tried. Better quality uses the upgrade
    /// sources that are set up and can search now (never YouTube, whose MP3 a lossless check
    /// refuses anyway), and searches the slow way, because nobody is waiting (#70). Wrong song and
    /// wrong version use the download source, except that Lidarr there means Lidarr too: before,
    /// those replacements went to Soulseek whatever was configured. A null entry is the default.
    /// </summary>
    internal async Task<IReadOnlyList<DownloadSource?>> SourcesForAsync(LibraryAction action, CancellationToken ct,
        DownloadSource? only = null)
    {
        if (action == LibraryAction.BetterQuality && only is { } picked)
            return picked == DownloadSource.Soulseek && await SoulseekOutAsync(ct) ? [] : [picked];
        if (action == LibraryAction.BetterQuality)
        {
            // Without the plan (hosts that build the executor by hand): Soulseek, as it always was.
            if (_sources is null) return await SoulseekOutAsync(ct) ? [] : [DownloadSource.Soulseek];
            return (await _sources.AvailableAsync(ct)).Select(s => (DownloadSource?)s).ToList();
        }
        var lidarrHearts = _subsonicSettings.CurrentValue.DownloadSource == DownloadSource.Lidarr
                           && _sources?.Plan().Contains(DownloadSource.Lidarr) == true;
        return lidarrHearts ? [DownloadSource.Lidarr, null] : [null];
    }

    private async Task<bool> WaitingForSoulseekAsync(CancellationToken ct) =>
        _sources is not null ? await _sources.WaitingForSoulseekAsync(ct) : await SoulseekOutAsync(ct);

    private async Task<bool> SoulseekOutAsync(CancellationToken ct) =>
        _soulseekLink is not null && (await _soulseekLink.ReadAsync(fresh: false, ct))?.Link == SoulseekLinkState.NotLoggedIn;

    /// <summary>Whether the person asking for a replacement had favorited the song. False when
    /// nothing will replace it, when the request carries no sign-in (playlist actions never
    /// do), or when Navidrome cannot say.</summary>
    internal async Task<bool> WasStarredByRequesterAsync(LibraryActionRequest request) =>
        request.Action is LibraryAction.WrongSong or LibraryAction.WrongVersion or LibraryAction.BetterQuality
        && request.Credential is { } credential && _stars is not null
        && await _stars.IsStarredAsync(credential, request.NavidromeId);

    /// <summary>Why a replacement is not good enough to keep, or null when it is.</summary>
    private static string? Unacceptable(LibraryAction action, string? path, ResolvedSongFile original)
    {
        if (string.IsNullOrEmpty(path) || !File.Exists(path)) return "never arrived";

        var info = new FileInfo(path);
        if (info.Length == 0) return "was empty";

        if (action != LibraryAction.BetterQuality) return null;

        // Better quality has to actually be better, or the action quietly downgrades a library.
        var lossless = new[] { ".flac", ".wav", ".aiff", ".aif", ".alac", ".ape" };
        if (!lossless.Contains(Path.GetExtension(path), StringComparer.OrdinalIgnoreCase))
            return "is not lossless";
        if (info.Length <= original.SizeBytes) return "is no larger than the original";

        return null;
    }

    /// <summary>
    /// Better quality means genuinely lossless: a FLAC made from an MP3 is no upgrade over the MP3
    /// it replaces, and over a real FLAC it is a downgrade. Null when the spectrum says nothing
    /// against it, which includes the check being off or unable to run.
    /// </summary>
    internal static string? NotReallyLossless(Octo.Services.Fingerprint.SpectrumReport report) =>
        report.IsLikelyLossy ? $"is {report.Describe()}" : null;

    private async Task<Octo.Services.Fingerprint.SpectrumReport> SpectrumOfAsync(string path)
    {
        var settings = _soulseek.CurrentValue;
        if (_spectrum is null || !settings.DetectTranscodes)
            return Octo.Services.Fingerprint.SpectrumReport.Unknown("not checked");
        return await _spectrum.AnalyzeAsync(path, settings.EffectiveTranscodeCheckTimeoutSeconds);
    }

    private LibraryActionOutcome RestoreOriginal(string quarantinePath, string detail)
    {
        var restored = _quarantine.Restore(quarantinePath);
        return restored.Moved
            ? new(LibraryActionState.Failed, detail)
            : new(LibraryActionState.Failed,
                $"{detail} The original could not be put back automatically and is in the quarantine folder.");
    }

    /// <summary>
    /// Remember the peer that delivered a wrong file, so the ranking never offers it again.
    ///
    /// Only possible when the download recorded who sent it, which Octo started doing alongside
    /// this feature. For a file downloaded before that, or one Octo never downloaded, there is
    /// nothing to blacklist and the re-acquire is an ordinary search.
    /// </summary>
    private void BlacklistSource(ResolvedSongFile original, LibraryActionRequest request)
    {
        if (!_soulseek.CurrentValue.VerifyDownloads) return;

        var mapping = _library.FindMappingByTagsAsync(original.Artist, original.Title, original.Album)
            .GetAwaiter().GetResult();

        if (mapping?.SourcePeer is not { Length: > 0 } peer || mapping.SourceFile is not { Length: > 0 } file)
        {
            _logger.LogInformation(
                "Library action Wrong song: no record of who sent '{Artist} - {Title}', so the search "
                + "runs again without a blacklist", original.Artist, original.Title);
            return;
        }

        _rejectedPeers.Deny(peer, file, $"reported as the wrong song by {request.Username}",
            $"{original.Artist} - {original.Title}");
    }

    private static LibraryActionEntry Entry(LibraryActionRequest request, ResolvedSongFile? resolved,
        LibraryActionState state, string? detail, bool dryRun) =>
        new(Key: LibraryActionJournal.MakeKey(request.Action, request.NavidromeId,
                resolved is null ? "unresolved" : "pending"),
            Action: request.Action,
            NavidromeId: request.NavidromeId,
            Username: request.Username,
            Title: resolved?.Title ?? "",
            Artist: resolved?.Artist ?? "",
            Album: resolved?.Album ?? "",
            SourcePath: resolved?.AbsolutePath,
            QuarantinePath: null,
            Resolution: resolved?.Source,
            State: state,
            Detail: detail,
            DryRun: dryRun,
            AtUtc: DateTime.UtcNow) { Copy = request.OnlyACopy };

    private static string Describe(LibraryAction action) => action switch
    {
        LibraryAction.Delete => "remove",
        LibraryAction.WrongSong => "replace (wrong song)",
        LibraryAction.WrongVersion => "replace (wrong version)",
        LibraryAction.BetterQuality => "upgrade",
        LibraryAction.Keep => "keep",
        _ => "act on",
    };

    private static void TryDelete(string path)
    {
        try { if (File.Exists(path)) File.Delete(path); } catch { /* best effort */ }
    }
}
