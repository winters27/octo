using Microsoft.Extensions.Options;
using Octo.Models.Settings;
using Octo.Services.Common;
using Octo.Services.Local;
using Octo.Services.Soulseek;

namespace Octo.Services.Library;

public sealed record LibraryActionRequest(LibraryAction Action, string NavidromeId, string Username);

public sealed record LibraryActionOutcome(LibraryActionState State, string? Detail)
{
    /// <summary>
    /// Whether the request has been consumed. Anything else leaves the track in the playlist
    /// and the rating set, so a request is never silently swallowed and retries if the
    /// operator fixes whatever blocked it.
    /// </summary>
    public bool Consumed => State is LibraryActionState.Applied or LibraryActionState.Skipped;
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
    private readonly ILogger<LibraryActionExecutor> _logger;

    public LibraryActionExecutor(NavidromeSongPathResolver resolver, LibraryActionQuarantine quarantine,
        LibraryActionJournal journal, ILocalLibraryService library, ExternalIdRegistry ids,
        RejectedPeerRegistry rejectedPeers, TrackAcquisitionQueue acquisitions,
        IOptionsMonitor<LibraryActionSettings> settings, IOptionsMonitor<SoulseekSettings> soulseek,
        ILogger<LibraryActionExecutor> logger)
    {
        _resolver = resolver;
        _quarantine = quarantine;
        _journal = journal;
        _library = library;
        _ids = ids;
        _rejectedPeers = rejectedPeers;
        _acquisitions = acquisitions;
        _settings = settings;
        _soulseek = soulseek;
        _logger = logger;
    }

    public async Task<LibraryActionOutcome> ApplyAsync(LibraryActionRequest request, CancellationToken ct = default)
    {
        var settings = _settings.CurrentValue;

        if (!settings.Enabled) return new(LibraryActionState.Skipped, "Library actions are off.");
        if (!settings.IsAllowed(request.Username))
            return new(LibraryActionState.Skipped, $"{request.Username} is not on the allowlist.");

        var definition = settings.EffectiveActions()
            .FirstOrDefault(entry => entry.Action == request.Action && entry.Enabled);
        if (definition is null)
            return new(LibraryActionState.Skipped, $"{request.Action} is not enabled.");

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

        // Written BEFORE the file is touched. A crash between the two leaves this Pending, and
        // startup reconciles it against the filesystem rather than blindly re-running.
        _journal.Record(pending);

        var musicRoot = _resolver.MusicRoot();
        var moved = _quarantine.Move(resolved, musicRoot, request.Action, request.Username);
        if (!moved.Moved)
        {
            _journal.Complete(key, LibraryActionState.Failed, moved.Error);
            return new(LibraryActionState.Failed, moved.Error);
        }

        await _library.ForgetMappingAsync(resolved.AbsolutePath);

        var outcome = request.Action switch
        {
            LibraryAction.Delete => new LibraryActionOutcome(LibraryActionState.Applied,
                "Removed. It will not be downloaded again."),
            _ => await ReacquireAsync(request, resolved, moved.QuarantinePath!, musicRoot, ct),
        };

        _journal.Complete(key, outcome.State, outcome.Detail, moved.QuarantinePath);
        return outcome;
    }

    /// <summary>
    /// Replace the file that was just quarantined.
    ///
    /// This is where "keep the current file until the new one is verified" actually lives, and
    /// it looks contradictory until you see the ordering: DownloadSongInternalAsync
    /// short-circuits on an existing file, so a re-acquire that left the original in place
    /// would be a no-op. Moving it out first is what makes the download happen, and the bytes
    /// are still recoverable the whole time. If the replacement never arrives or does not pass,
    /// the original goes back exactly where it was.
    /// </summary>
    private async Task<LibraryActionOutcome> ReacquireAsync(LibraryActionRequest request,
        ResolvedSongFile original, string quarantinePath, string musicRoot, CancellationToken ct)
    {
        if (request.Action == LibraryAction.WrongSong) BlacklistSource(original, request);

        try
        {
            var routing = new SoulseekRouting
            {
                Kind = RoutingKind.Song,
                Artist = original.Artist,
                Title = original.Title,
                Album = original.Album,
                Duration = original.DurationSeconds,
            };
            var externalId = _ids.Register(routing);

            var replacement = await _acquisitions.Enqueue(
                SoulseekMetadataService.ProviderName, externalId, isStar: true,
                triggerAlbumDownload: false, forcePermanent: true,
                sourceOverride: null, notifyOnFailure: false);

            var problem = Unacceptable(request.Action, replacement, original);
            if (problem is null)
            {
                if (!_settings.CurrentValue.KeepReplacedOriginals) TryDelete(quarantinePath);
                return new(LibraryActionState.Applied, $"Replaced with {Path.GetFileName(replacement)}.");
            }

            _logger.LogInformation(
                "Library action {Action}: the replacement for '{Artist} - {Title}' {Problem}; putting the original back",
                request.Action, original.Artist, original.Title, problem);
            TryDelete(replacement);
            return RestoreOriginal(quarantinePath, $"The replacement {problem}, so nothing changed.");
        }
        catch (Exception ex)
        {
            return RestoreOriginal(quarantinePath,
                $"Could not find a replacement ({ex.Message}), so nothing changed.");
        }
    }

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
            AtUtc: DateTime.UtcNow);

    private static string Describe(LibraryAction action) => action switch
    {
        LibraryAction.Delete => "remove",
        LibraryAction.WrongSong => "replace (wrong song)",
        LibraryAction.WrongVersion => "replace (wrong version)",
        LibraryAction.BetterQuality => "upgrade",
        _ => "act on",
    };

    private static void TryDelete(string path)
    {
        try { if (File.Exists(path)) File.Delete(path); } catch { /* best effort */ }
    }
}
