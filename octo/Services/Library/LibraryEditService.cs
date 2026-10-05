using System.Collections.Concurrent;
using System.Globalization;
using Microsoft.Extensions.Options;
using Octo.Models.Domain;
using Octo.Models.Settings;
using Octo.Services.Common;
using Octo.Services.CoverArt;
using Octo.Services.Fingerprint;
using Octo.Services.Tagging;

namespace Octo.Services.Library;

/// <summary>How one edit from an app went. State is one of the libraryAction states.</summary>
public sealed record LibraryEditOutcome(string State, string? Detail)
{
    public IReadOnlyDictionary<string, string?>? Before { get; init; }
    public IReadOnlyDictionary<string, string?>? After { get; init; }
}

/// <summary>What a lookup found for one song: the file's tags now and what the lookup would write.</summary>
public sealed record SongLookup(string State, string? Detail,
    IReadOnlyDictionary<string, string?> Current, IReadOnlyDictionary<string, string?> Suggested,
    string? Confidence, string? Source, string? Release);

/// <summary>A song taken out of the library that is still in the server's trash.</summary>
public sealed record TrashedSong(string Id, string Title, string Artist, string Album, string Username,
    DateTime RemovedUtc, DateTime? GoneUtc);

/// <summary>
/// What the apps' Library health changes in a song's file, other than removing it: tags written
/// in place, an album joined to another, a cover added where there was none, a lookup of the
/// tags a download would get, and the undo of each, plus putting a removed song back.
///
/// The same gates as every library action (switched on, the caller on the allowed list, dry run
/// rehearses), and one more the controller checks: the caller is a Navidrome admin. Every file is
/// one the resolver proved, or one this service itself wrote a moment ago (Navidrome reports a
/// file's old size until it scans, and a second edit should not have to wait for that).
/// </summary>
public sealed class LibraryEditService
{
    private static readonly TimeSpan CoverMemory = TimeSpan.FromMinutes(15);
    private static readonly TimeSpan WrittenMemory = TimeSpan.FromMinutes(30);

    private readonly NavidromeSongPathResolver _resolver;
    private readonly LibraryActionQuarantine _quarantine;
    private readonly LibraryActionJournal _actions;
    private readonly TagEditJournal _journal;
    private readonly LibraryRescan _rescan;
    private readonly IOptionsMonitor<LibraryActionSettings> _settings;
    private readonly IServiceProvider _services;
    private readonly ILogger<LibraryEditService> _logger;

    // The files being changed right now, so two edits of one file never interleave.
    private readonly ConcurrentDictionary<string, byte> _busy = new(
        OperatingSystem.IsWindows() ? StringComparer.OrdinalIgnoreCase : StringComparer.Ordinal);

    // Files this service wrote, by Navidrome id: the path and what the file was right after.
    private readonly ConcurrentDictionary<string, (string Path, long Size, DateTime WriteUtc, DateTime AtUtc, ResolvedSongFile File)> _written = new();

    // Covers found lately, by album, so the songs of one album cost one lookup.
    private readonly ConcurrentDictionary<string, (FoundCover? Cover, DateTime AtUtc)> _covers = new();

    internal const string BusyText = "Another change to this song is still running, so this one was skipped.";

    public LibraryEditService(NavidromeSongPathResolver resolver, LibraryActionQuarantine quarantine,
        LibraryActionJournal actions, TagEditJournal journal, LibraryRescan rescan,
        IOptionsMonitor<LibraryActionSettings> settings, IServiceProvider services, ILogger<LibraryEditService> logger)
    {
        _resolver = resolver;
        _quarantine = quarantine;
        _actions = actions;
        _journal = journal;
        _rescan = rescan;
        _settings = settings;
        _services = services;
        _logger = logger;
    }

    /// <summary>Why this caller may not change library files, or null when they may.</summary>
    public string? Refusal(string? username, bool admin)
    {
        var settings = _settings.CurrentValue;
        if (string.IsNullOrWhiteSpace(username))
            return "Sign in with a username to change songs; an API key alone does not say who is asking.";
        if (!settings.Enabled) return "Library actions are off.";
        if (!settings.IsAllowed(username)) return $"{username} is not on the library actions allowed list.";
        if (!admin) return "Only a server admin can change the files in the library.";
        return null;
    }

    private bool DryRun => _settings.CurrentValue.DryRun;

    /// <summary>Writes these tags into the song's file.</summary>
    public async Task<LibraryEditOutcome> RetagAsync(string id, string username, IReadOnlyDictionary<string, string?> changes,
        CancellationToken ct = default)
    {
        if (LibraryTagEdits.Invalid(changes) is { } invalid) return new("failed", invalid);
        var file = await ResolveAsync(id, ct);
        if (file is null) return Unresolved();
        if (DryRun) return new("rehearsed", $"Dry run: would change {Fields(changes.Keys)} of {file.AbsolutePath}");
        return Locked(file, () =>
        {
            var result = LibraryTagEdits.Write(file.AbsolutePath, changes);
            return Finish(file, username, TagEditKinds.Retag, result, null,
                $"Changed the {Fields(Changed(result))}.", "Nothing to change: the file already says that.");
        });
    }

    /// <summary>Makes the song part of the album the lead song is on.</summary>
    public async Task<LibraryEditOutcome> JoinAlbumAsync(string id, string leadId, string username, CancellationToken ct = default)
    {
        if (id == leadId) return new("skipped", "The song is already on that album.");
        var file = await ResolveAsync(id, ct);
        var lead = await ResolveAsync(leadId, ct);
        if (file is null || lead is null) return Unresolved();
        if (DryRun) return new("rehearsed", $"Dry run: would put {file.AbsolutePath} on the album of {lead.AbsolutePath}");
        return Locked(file, () =>
        {
            var albumBefore = KeptIdentityTags.Read(file.AbsolutePath, file.AlbumArtist);
            var result = LibraryTagEdits.JoinAlbum(file.AbsolutePath, lead.AbsolutePath, lead.AlbumArtist, file.AlbumArtist);
            return Finish(file, username, TagEditKinds.JoinAlbum, result, albumBefore,
                $"Moved onto {lead.Album}.", "It was on that album already.");
        });
    }

    /// <summary>Finds a cover for the song's album and puts it inside the file, when the file has none.</summary>
    public async Task<LibraryEditOutcome> AddCoverAsync(string id, string username, bool preview, CancellationToken ct = default)
    {
        var file = await ResolveAsync(id, ct);
        if (file is null) return Unresolved();
        if (LibraryTagEdits.HasPicture(file.AbsolutePath))
            return new("skipped", "The song has a picture of its own already, so it was left alone.");
        var found = await FindCoverAsync(file, ct);
        if (found is null) return new("failed", "No cover for this album could be found.");
        var words = $"a {found.Side} pixel cover from {found.Source}";
        if (preview) return new("found", $"Found {words}.");
        if (DryRun) return new("rehearsed", $"Dry run: would put {words} inside {file.AbsolutePath}");
        var cover = CoverImage.FitWithin(found.Bytes, MetadataSettings.EmbeddedCoverSide);
        return Locked(file, () =>
        {
            var result = LibraryTagEdits.AddCover(file.AbsolutePath, cover);
            return Finish(file, username, TagEditKinds.Cover, result, null, $"Added {words}.", "The song has a picture already.");
        });
    }

    /// <summary>Puts back the song's last edit made here, when the file has not changed since.</summary>
    public async Task<LibraryEditOutcome> UndoAsync(string id, string username, CancellationToken ct = default)
    {
        if (_journal.Last(id) is not { } entry) return new("skipped", "There is no change to this song to undo.");
        var file = await ResolveAsync(id, ct);
        var path = file?.AbsolutePath ?? entry.Path;
        var info = new FileInfo(path);
        if (!info.Exists || !SamePath(path, entry.Path)
            || !NavidromeSongPathResolver.IsInside(Path.GetFullPath(path), _resolver.MusicRoot())) return Unresolved();
        if (info.Length != entry.SizeAfter || info.LastWriteTimeUtc.Ticks != entry.WriteTicksAfter)
            return new("failed", "The file has changed since, so it was left as it is.");
        if (DryRun) return new("rehearsed", $"Dry run: would undo the last change to {path}");
        var target = file ?? new ResolvedSongFile(id, path, info.Length, "", "", "", "", null, PathSource.LocalMappings);
        return Locked(target, () =>
        {
            TagEditResult result = entry.Kind switch
            {
                TagEditKinds.Cover => LibraryTagEdits.RemoveCover(path),
                TagEditKinds.Advisory => LibraryTagEdits.SetAdvisory(path,
                    int.TryParse(entry.Before.GetValueOrDefault(TagEditKinds.Advisory), out var advisory) ? advisory : null),
                TagEditKinds.JoinAlbum when entry.AlbumBefore is { } album =>
                    LibraryTagEdits.RestoreAlbum(path, album, entry.Before.GetValueOrDefault(SongTagFields.Year)),
                _ => LibraryTagEdits.Write(path, entry.Before
                    .Where(pair => entry.After.GetValueOrDefault(pair.Key) != pair.Value)
                    .ToDictionary(pair => pair.Key, pair => pair.Value)),
            };
            if (result.Error is { } error) return new LibraryEditOutcome("failed", $"Could not undo it: {error}.");
            _journal.MarkUndone(entry.Id);
            Remember(target);
            _rescan.Soon();
            _logger.LogInformation("{User} undid the {Kind} of {Path} from an app", username, entry.Kind, path);
            return new LibraryEditOutcome("applied", "Put back as it was.") { Before = result.Before, After = result.After };
        });
    }

    /// <summary>Puts a removed song back where it was, from the server's trash.</summary>
    public async Task<LibraryEditOutcome> RestoreAsync(string id, string username)
    {
        var entry = _actions.Recent(int.MaxValue).FirstOrDefault(e =>
            e.NavidromeId == id && e.Action == LibraryAction.Delete && !e.DryRun
            && e.State == LibraryActionState.Applied && e.QuarantinePath is not null);
        if (entry is null) return new("skipped", "This song is not in the server's trash.");
        if (!File.Exists(entry.QuarantinePath)) return new("failed", "The file is no longer in the server's trash.");
        if (DryRun) return new("rehearsed", $"Dry run: would put back {entry.SourcePath}");
        var restored = _quarantine.Restore(entry.QuarantinePath!, _resolver.MusicRoot());
        if (!restored.Moved) return new("failed", $"Could not put it back: {restored.Error}.");
        // The outside songs Octo knew this file as point at it again, as before it was removed.
        if (restored.Mappings is { Count: > 0 } mappings
            && _services.GetService<Octo.Services.Local.ILocalLibraryService>() is { } library)
            await library.RestoreMappingsAsync(mappings, restored.QuarantinePath!);
        _actions.Complete(entry.Key, LibraryActionState.Restored, $"Put back by {username}.");
        _actions.Flush();
        _rescan.Soon();
        _logger.LogInformation("{User} put back {Path} from the trash", username, restored.QuarantinePath);
        return new("applied", "Put back in your library.");
    }

    /// <summary>The songs removed from the apps or the playlists that are still in the trash, newest first.</summary>
    public IReadOnlyList<TrashedSong> Trash()
    {
        var days = _settings.CurrentValue.EffectiveQuarantineRetentionDays;
        return _actions.Recent(int.MaxValue)
            .Where(e => e.Action == LibraryAction.Delete && !e.DryRun && e.State == LibraryActionState.Applied
                        && e.QuarantinePath is not null && File.Exists(e.QuarantinePath))
            .GroupBy(e => e.NavidromeId).Select(group => group.First())
            .Select(e => new TrashedSong(e.NavidromeId, e.Title, e.Artist, e.Album, e.Username, e.AtUtc,
                // The sweep goes by the day folder, so the file goes at the start of that day plus the days kept.
                days > 0 ? e.AtUtc.Date.AddDays(days + 1) : null))
            .Take(500)
            .ToList();
    }

    /// <summary>
    /// The tags a download of this song would get, found the way a download finds them (its
    /// fingerprint when that is set up, then the catalogs), beside what the file says now.
    /// Writes nothing.
    /// </summary>
    public async Task<SongLookup> LookupAsync(string id, CancellationToken ct = default)
    {
        var empty = new Dictionary<string, string?>();
        var file = await ResolveAsync(id, ct);
        if (file is null) return new("unresolved", UnresolvedText, empty, empty, null, null, null);
        if (_services.GetService<ReleaseIdentifier>() is not { } identifier)
            return new("failed", "This server cannot look songs up.", empty, empty, null, null, null);

        Dictionary<string, string?> current;
        try { current = LibraryTagEdits.Read(file.AbsolutePath); }
        catch (Exception ex) { return new("failed", $"Could not read the song's tags ({ex.Message}).", empty, empty, null, null, null); }

        var song = new Song
        {
            Artist = current[SongTagFields.Artist] ?? file.Artist,
            Title = current[SongTagFields.Title] ?? file.Title,
            Album = "",
            Duration = file.DurationSeconds,
        };
        try
        {
            var verification = _services.GetService<DownloadVerificationService>();
            if (verification is { IsFingerprintingEnabled: true })
                song.Verification = await verification.VerifyAsync(file.AbsolutePath, song.Artist, song.Title);
            var request = ReleaseIdentifier.RequestFor(song, song.Artist, song.Title, null, null);
            var plan = await identifier.IdentifyAsync(song, request, file.AbsolutePath, tagsAreEvidence: true, null, ct);
            plan.ApplyTo(song);
            BaseDownloadService.FillBlanksFromCatalog(song, plan.CatalogBest);
            var chosen = plan.Chosen?.Candidate;
            return new("found", null, current, Suggested(song), plan.Confidence.ToString(),
                chosen?.Source.ToString() ?? (plan.CatalogBest is null ? null : "Deezer"),
                chosen is null ? null : TagPlan.Describe(chosen));
        }
        catch (Exception ex) when (ex is not OperationCanceledException)
        {
            _logger.LogInformation("Lookup of {Path} failed: {M}", file.AbsolutePath, ex.Message);
            return new("failed", $"The lookup did not work ({ex.Message}).", current, empty, null, null, null);
        }
    }

    internal static Dictionary<string, string?> Suggested(Song song)
    {
        static string? Text(string? value) => string.IsNullOrWhiteSpace(value) ? null : value.Trim();
        static string? Number(int? value) => value is > 0 ? value.Value.ToString(CultureInfo.InvariantCulture) : null;
        var found = new Dictionary<string, string?>
        {
            [SongTagFields.Title] = Text(song.Title),
            [SongTagFields.Artist] = Text(song.Artist),
            [SongTagFields.Album] = Text(song.Album),
            [SongTagFields.AlbumArtist] = Text(song.AlbumArtist),
            [SongTagFields.Year] = Number(song.Year),
            [SongTagFields.Genre] = Text(song.Genre),
            [SongTagFields.Track] = Number(song.Track),
            [SongTagFields.Disc] = Number(song.DiscNumber),
            [SongTagFields.Isrc] = SongIdentity.NormalizeIsrc(song.Isrc),
        };
        return found.Where(pair => pair.Value is not null).ToDictionary(pair => pair.Key, pair => pair.Value);
    }

    private async Task<FoundCover?> FindCoverAsync(ResolvedSongFile file, CancellationToken ct)
    {
        if (_services.GetService<IAlbumCoverFinder>() is not { } finder) return null;
        Dictionary<string, string?> tags;
        try { tags = LibraryTagEdits.Read(file.AbsolutePath); }
        catch (Exception) { return null; }
        var artist = tags[SongTagFields.AlbumArtist] ?? file.AlbumArtist ?? tags[SongTagFields.Artist] ?? file.Artist;
        var album = tags[SongTagFields.Album] ?? file.Album;
        var key = SongIdentity.MatchKey(artist, album);
        if (_covers.TryGetValue(key, out var kept) && DateTime.UtcNow - kept.AtUtc < CoverMemory) return kept.Cover;

        string? releaseId = null, barcode = null;
        try
        {
            using var tagFile = TagLib.File.Create(file.AbsolutePath);
            releaseId = TagWriterExtras.ReadText(tagFile, TagFields.AlbumId);
            barcode = TagWriterExtras.ReadText(tagFile, TagFields.Barcode);
        }
        catch (Exception) { /* the names alone still find most covers */ }

        FoundCover? found;
        try
        {
            found = await finder.FindAsync(new AlbumCoverQuery(artist, album, tags[SongTagFields.Title] ?? file.Title,
                releaseId, null, barcode), ct);
        }
        catch (Exception ex) when (ex is not OperationCanceledException)
        {
            _logger.LogInformation("Cover lookup for {Artist} - {Album} failed: {M}", artist, album, ex.Message);
            return null;
        }
        foreach (var (old, value) in _covers)
            if (DateTime.UtcNow - value.AtUtc >= CoverMemory) _covers.TryRemove(old, out _);
        _covers[key] = (found, DateTime.UtcNow);
        return found;
    }

    /// <summary>The verified file, or the one this service wrote lately when Navidrome still
    /// reports its size from before the write.</summary>
    private async Task<ResolvedSongFile?> ResolveAsync(string id, CancellationToken ct)
    {
        if (await _resolver.ResolveAsync(id, ct) is { } resolved) return resolved;
        if (!_written.TryGetValue(id, out var written)) return null;
        if (DateTime.UtcNow - written.AtUtc > WrittenMemory) return null;
        var info = new FileInfo(written.Path);
        if (!info.Exists || info.Length != written.Size || info.LastWriteTimeUtc != written.WriteUtc) return null;
        if (!NavidromeSongPathResolver.IsInside(Path.GetFullPath(written.Path), _resolver.MusicRoot())) return null;
        return written.File with { SizeBytes = info.Length };
    }

    private void Remember(ResolvedSongFile file)
    {
        var info = new FileInfo(file.AbsolutePath);
        if (info.Exists)
            _written[file.NavidromeId] = (file.AbsolutePath, info.Length, info.LastWriteTimeUtc, DateTime.UtcNow, file);
    }

    private LibraryEditOutcome Locked(ResolvedSongFile file, Func<LibraryEditOutcome> work)
    {
        var key = Path.GetFullPath(file.AbsolutePath);
        if (!_busy.TryAdd(key, 0)) return new("skipped", BusyText);
        try { return work(); }
        finally { _busy.TryRemove(key, out _); }
    }

    private LibraryEditOutcome Finish(ResolvedSongFile file, string username, string kind, TagEditResult result,
        KeptIdentity? albumBefore, string changedText, string sameText)
    {
        if (result.Error is { } error) return new("failed", $"Could not change the song: {error}.");
        if (!result.Changed) return new("skipped", sameText) { Before = result.Before, After = result.After };
        var info = new FileInfo(file.AbsolutePath);
        var kept = _journal.Record(new TagEditEntry(Guid.NewGuid().ToString("N"), file.NavidromeId, file.AbsolutePath, kind,
            username, new(result.Before), new(result.After), info.Length, info.LastWriteTimeUtc.Ticks, DateTime.UtcNow)
        {
            AlbumBefore = albumBefore,
        });
        if (!kept) _logger.LogWarning("The {Kind} of {Path} was written but could not be kept for an undo", kind, file.AbsolutePath);
        Remember(file);
        _rescan.Soon();
        _logger.LogInformation("{User} made a {Kind} of {Path} from an app: {Detail}", username, kind, file.AbsolutePath, changedText);
        return new("applied", changedText) { Before = result.Before, After = result.After };
    }

    private const string UnresolvedText = "Could not work out which file this is, so nothing was touched.";
    private static LibraryEditOutcome Unresolved() => new("unresolved", UnresolvedText);

    private static IEnumerable<string> Changed(TagEditResult result) =>
        result.After.Where(pair => result.Before.GetValueOrDefault(pair.Key) != pair.Value).Select(pair => pair.Key);

    private static string Fields(IEnumerable<string> names)
    {
        var words = names.Select(name => SongTagFields.Canonical(name) ?? name).Select(LibraryTagEdits.Words).Distinct().ToList();
        return words.Count switch
        {
            0 => "tags",
            1 => words[0],
            _ => $"{string.Join(", ", words.Take(words.Count - 1))} and {words[^1]}",
        };
    }

    private static bool SamePath(string a, string b) => string.Equals(Path.GetFullPath(a), Path.GetFullPath(b),
        OperatingSystem.IsWindows() ? StringComparison.OrdinalIgnoreCase : StringComparison.Ordinal);
}

/// <summary>
/// One Navidrome scan a little after a run of changes, rather than one per change. A normal scan,
/// never a full one: over the cloud mount a full scan reads every file again (over fifteen minutes
/// for 2,500 songs), and a normal one finds a changed or missing file by its folder.
/// </summary>
public sealed class LibraryRescan
{
    private readonly IServiceScopeFactory? _scopes;
    private readonly ILogger<LibraryRescan>? _logger;
    private int _pending;

    internal TimeSpan Delay { get; set; } = TimeSpan.FromSeconds(8);

    /// <summary>Counted for tests.</summary>
    internal int Asked;

    public LibraryRescan(IServiceScopeFactory? scopes = null, ILogger<LibraryRescan>? logger = null)
    {
        _scopes = scopes;
        _logger = logger;
    }

    public void Soon()
    {
        Interlocked.Increment(ref Asked);
        if (_scopes is null || Interlocked.Exchange(ref _pending, 1) == 1) return;
        using (ExecutionContext.SuppressFlow())
            _ = Task.Run(async () =>
            {
                try
                {
                    await Task.Delay(Delay);
                    Interlocked.Exchange(ref _pending, 0);
                    using var scope = _scopes.CreateScope();
                    await scope.ServiceProvider.GetRequiredService<Octo.Services.Local.ILocalLibraryService>()
                        .TriggerLibraryScanAsync(force: true);
                }
                catch (Exception ex)
                {
                    Interlocked.Exchange(ref _pending, 0);
                    _logger?.LogDebug("Scan after a library change failed: {M}", ex.Message);
                }
            });
    }
}
