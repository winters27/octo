using System.Text;
using System.Threading.Channels;
using Microsoft.Extensions.Options;
using Octo.Models.Settings;

namespace Octo.Services.Lyrics;

/// <param name="Acquisition">The downloads log row ("provider:id") that hears how the look went,
/// when a download asked for it.</param>
public sealed record LyricsJob(string AudioPath, string Artist, string Title, string? Album, int? DurationSeconds,
    int Attempt = 1, string? Acquisition = null);

public enum LyricsWriteOutcome { Written, Instrumental, NotFound, AlreadyThere, Gone, Retrying, GaveUp, Upgraded }

/// <summary>What a write did, and the lyrics it wrote, for the library job's review list.</summary>
public sealed record LyricsWrite(LyricsWriteOutcome Outcome, LyricsResult? Result);

/// <summary>
/// Writes a lyrics file beside a download (#52), off the download path. Downloads run one at a
/// time under a lock, and LRCLIB can take seconds or shed load with a 503, so fetching lyrics
/// inline would stall every download queued behind this one. A sidecar Navidrome reads at request
/// time needs no rescan, so arriving a little later costs nothing.
///
/// Synced lyrics go in a .lrc, plain ones in a .txt: both are in Navidrome's default
/// LyricsPriority, and a .txt says plainly that there is no timing. Word-timed lyrics are
/// enhanced LRC: every line keeps its standard [mm:ss.xx] tag, so any player that reads .lrc
/// shows them line by line, and one that knows &lt;mm:ss.xx&gt; word tags (Navidrome among them,
/// which turns them into OpenSubsonic word cues) gets the words too.
///
/// A .lrc Octo writes opens with [re:Octo], LRC's own "made by" tag, which every reader skips,
/// and so do lyrics Octo writes inside a song (LYRICS_SAVE_TO inside or both). It is how Octo
/// knows lyrics are its own: an instrumental gets nothing, and lyrics Octo did not write, beside
/// the song or inside it, are never replaced. Asked to upgrade, Octo looks again for a song whose
/// lyrics are weaker than the sources would choose now (the song's own lyrics rank like any
/// source, see LYRICS_SOURCES): its own are replaced, and anyone else's get the better ones
/// beside them, as a .lrc, which Navidrome serves ahead of lyrics in the tags.
/// </summary>
public sealed class LyricsSidecarWriter : BackgroundService
{
    internal static TimeSpan RetryDelay = TimeSpan.FromMinutes(10);
    internal const int MaxAttempts = 3;

    /// <summary>The first line of every .lrc Octo writes.</summary>
    public const string OctoMark = "[re:Octo]";

    private static readonly UTF8Encoding Utf8NoBom = new(false);

    private readonly Channel<LyricsJob> _queue =
        Channel.CreateBounded<LyricsJob>(new BoundedChannelOptions(256) { FullMode = BoundedChannelFullMode.DropWrite });

    /// <summary>How long after lyrics are written inside a song Navidrome is asked to scan, so a
    /// run of writes asks once.</summary>
    internal static TimeSpan ScanDelay = TimeSpan.FromSeconds(60);

    private readonly LyricsService _lyrics;
    private readonly ILogger<LyricsSidecarWriter> _logger;
    private readonly IOptionsMonitor<MetadataSettings>? _settings;
    private readonly IServiceScopeFactory? _scopes;
    private int _scanPending;

    private readonly Octo.Services.Common.AcquisitionTracker? _tracker;

    public LyricsSidecarWriter(LyricsService lyrics, ILogger<LyricsSidecarWriter> logger,
        IOptionsMonitor<MetadataSettings>? settings = null, IServiceScopeFactory? scopes = null,
        Octo.Services.Common.AcquisitionTracker? tracker = null)
    {
        _tracker = tracker;
        _lyrics = lyrics;
        _logger = logger;
        _settings = settings;
        _scopes = scopes;
    }

    private string SaveTo => LyricsSaveTo.Normalize(_settings?.CurrentValue.SaveLyricsTo);

    public bool TryEnqueue(LyricsJob job) => _queue.Writer.TryWrite(job);

    protected override async Task ExecuteAsync(CancellationToken stoppingToken)
    {
        await foreach (var job in _queue.Reader.ReadAllAsync(stoppingToken))
        {
            // Per-item catch is mandatory: BackgroundServiceExceptionBehavior defaults to
            // StopHost, so one unhandled exception here would take Octo down.
            try
            {
                var write = await WriteAsync(job, upgrade: false, stoppingToken);
                Report(job, write);
                if (write.Outcome == LyricsWriteOutcome.Retrying) _ = RetryLaterAsync(job with { Attempt = job.Attempt + 1 }, stoppingToken);
            }
            catch (OperationCanceledException) when (stoppingToken.IsCancellationRequested) { break; }
            catch (Exception ex)
            {
                _logger.LogWarning("Could not write lyrics for {Path}: {M}", job.AudioPath, ex.Message);
            }
        }
    }

    internal async Task<LyricsWriteOutcome> WriteAsync(LyricsJob job, CancellationToken ct) =>
        (await WriteAsync(job, upgrade: false, ct)).Outcome;

    /// <summary>Tells the download's log what the look found, in words, and where the lyrics went.</summary>
    internal void Report(LyricsJob job, LyricsWrite write)
    {
        if (_tracker is null || job.Acquisition is not { } row) return;
        var split = row.IndexOf(':');
        if (split <= 0) return;
        var (text, detail) = ReportText(job, write, File.Exists(job.AudioPath) ? SongLyrics.Of(job.AudioPath).Where : SongLyricsPlace.None);
        if (text is null) return;
        _tracker.Log(row[..split], row[(split + 1)..], Octo.Services.Common.AcquisitionEventKinds.Lyrics, text, detail);
    }

    /// <summary>The log's words for one look, or null when there is nothing to say yet.</summary>
    internal static (string? Text, string? Detail) ReportText(LyricsJob job, LyricsWrite write, SongLyricsPlace place) =>
        write.Outcome switch
        {
            LyricsWriteOutcome.Written or LyricsWriteOutcome.Upgraded when write.Result is { } found => (
                $"{TimingWords(found.Timing)} from {found.Source}",
                place switch
                {
                    SongLyricsPlace.Inside => "Embedded in the song",
                    SongLyricsPlace.Beside => $"Saved beside the song as a {(found.HasSynced ? ".lrc" : ".txt")} file",
                    _ => "Saved",
                }),
            LyricsWriteOutcome.Instrumental => ("No lyrics: an instrumental",
                write.Result is { } said ? $"{said.Source} says it has no words" : null),
            LyricsWriteOutcome.NotFound => ("No lyrics found", "None of the lyrics sources has this song"),
            LyricsWriteOutcome.AlreadyThere => ("The song already has lyrics", "Left as they are"),
            LyricsWriteOutcome.Retrying => ("No lyrics service answered",
                $"Trying again in {(int)RetryDelay.TotalMinutes} minutes (try {job.Attempt} of {MaxAttempts})"),
            LyricsWriteOutcome.GaveUp => ("No lyrics service answered", $"Gave up after {MaxAttempts} tries"),
            _ => (null, null),
        };

    private static string TimingWords(LyricsTiming timing) => timing switch
    {
        LyricsTiming.Word => "Word by word lyrics",
        LyricsTiming.Line => "Synced lyrics",
        _ => "Plain lyrics",
    };

    /// <summary>
    /// Look the song up and save what was found. A song that already has lyrics is left alone,
    /// unless <paramref name="upgrade"/>: then it is looked up with its own lyrics ranked among
    /// the sources, and better ones are saved (see the class summary for where).
    /// </summary>
    internal async Task<LyricsWrite> WriteAsync(LyricsJob job, bool upgrade, CancellationToken ct)
    {
        if (!File.Exists(job.AudioPath)) return new(LyricsWriteOutcome.Gone, null);
        var has = SongLyrics.Of(job.AudioPath);
        if (has.Where != SongLyricsPlace.None && (!upgrade || has.Timing == LyricsTiming.Word || has.Unknown))
            return new(LyricsWriteOutcome.AlreadyThere, null);

        var lookup = await _lyrics.FindAsync(
            new LyricsQuery(job.Artist, job.Title, job.Album, job.DurationSeconds ?? ReadDuration(job.AudioPath)), ct,
            has.Timing);
        if (lookup.Transient)
        {
            if (job.Attempt < MaxAttempts) return new(LyricsWriteOutcome.Retrying, null);
            _logger.LogInformation("No lyrics service answered for '{Artist} - {Title}' after {N} tries",
                job.Artist, job.Title, job.Attempt);
            return new(LyricsWriteOutcome.GaveUp, null);
        }

        if (has.Where != SongLyricsPlace.None)
        {
            if (lookup.Result is not { IsSongsOwn: false } better || better.Timing <= has.Timing
                || !await SaveAsync(job.AudioPath, better, ct))
                return new(LyricsWriteOutcome.AlreadyThere, null);
            _logger.LogInformation("Lyrics for '{Artist} - {Title}' upgraded from {Was} to {Now} from {Source}",
                job.Artist, job.Title, has.Timing, better.Timing, better.Source);
            return new(LyricsWriteOutcome.Upgraded, better);
        }

        switch (lookup.Result)
        {
            case { Instrumental: true } instrumental:
                _logger.LogInformation("{Source} says '{Artist} - {Title}' is instrumental; no lyrics file",
                    instrumental.Source, job.Artist, job.Title);
                return new(LyricsWriteOutcome.Instrumental, instrumental);
            case { IsSongsOwn: false } found when found.HasSynced || found.HasPlain:
                if (!await SaveAsync(job.AudioPath, found, ct)) return new(LyricsWriteOutcome.AlreadyThere, null);
                _logger.LogInformation("Lyrics for '{Artist} - {Title}' from {Source} ({Timing})",
                    job.Artist, job.Title, found.Source, found.Timing.ToString().ToLowerInvariant());
                return new(LyricsWriteOutcome.Written, found);
            default:
                _logger.LogInformation("No lyrics found for '{Artist} - {Title}'", job.Artist, job.Title);
                return new(LyricsWriteOutcome.NotFound, null);
        }
    }

    /// <summary>What the sources find for a song, its own lyrics ranked among them, without
    /// saving anything: the lyrics page's preview.</summary>
    internal Task<LyricsLookup> LookUpAsync(LyricsJob job, LyricsTiming songsOwn, CancellationToken ct) =>
        _lyrics.FindAsync(new LyricsQuery(job.Artist, job.Title, job.Album, job.DurationSeconds ?? ReadDuration(job.AudioPath)),
            ct, songsOwn);

    /// <summary>
    /// Replace a song's lyrics with lyrics someone chose. Only where Octo may write: where the
    /// song has no lyrics, or only Octo's; lyrics the owner put there are never touched.
    /// </summary>
    internal async Task<bool> ReplaceAsync(string audioPath, LyricsResult chosen, CancellationToken ct)
    {
        if (!File.Exists(audioPath) || (!chosen.HasSynced && !chosen.HasPlain)) return false;
        var has = SongLyrics.Of(audioPath);
        if (has.Where != SongLyricsPlace.None && !has.Octos) return false;
        return await SaveAsync(audioPath, chosen, ct);
    }

    /// <summary>
    /// Save lyrics where LYRICS_SAVE_TO says, replacing only Octo's own. Where that is not
    /// allowed (inside a song whose tags hold someone else's lyrics), they go beside it instead,
    /// which takes nothing away. False when there was nowhere to put them.
    /// </summary>
    private async Task<bool> SaveAsync(string audioPath, LyricsResult found, CancellationToken ct,
        Action<string, string, string?>? record = null)
    {
        var stem = Stem(audioPath);
        var saveTo = SaveTo;
        var inside = saveTo != LyricsSaveTo.Beside && SongLyrics.MayWriteInside(audioPath);
        var beside = saveTo != LyricsSaveTo.Inside || !inside;
        var saved = false;
        if (inside)
        {
            record?.Invoke(audioPath, LyricsUndoJournal.Inside, ReadTagLyrics(audioPath));
            if (WriteInside(audioPath, found))
            {
                saved = true;
                ScanSoon();
            }
        }
        if (beside && SongLyrics.MayWriteBeside(stem, found.HasSynced))
        {
            var target = stem + (found.HasSynced ? ".lrc" : ".txt");
            record?.Invoke(target, LyricsUndoJournal.Beside, File.Exists(target) ? await File.ReadAllTextAsync(target, ct) : null);
            await WriteFileAsync(stem, found, ct);
            saved = true;
        }
        return saved;
    }

    /// <summary>
    /// Save lyrics someone picked on the lyrics page, where LYRICS_SAVE_TO says and Octo may
    /// write, telling <paramref name="record"/> what each write replaces (path, kind, what was
    /// there) before it happens. False when there was nowhere Octo may write.
    /// </summary>
    internal Task<bool> SaveChosenAsync(string audioPath, LyricsResult found, Action<string, string, string?> record,
        CancellationToken ct) => SaveAsync(audioPath, found, ct, record);

    /// <summary>
    /// Put back what one Save replaced. A file Octo added is removed, and only while it is still
    /// Octo's; lyrics in a song's tags go back only while the tags hold Octo's. False when the
    /// file has changed since, and was left alone.
    /// </summary>
    internal bool Restore(LyricsUndoJournal.Entry entry)
    {
        try
        {
            if (entry.Kind == LyricsUndoJournal.Inside)
            {
                if (!File.Exists(entry.Path) || !SongLyrics.MayWriteInside(entry.Path)) return false;
                using var file = TagLib.File.Create(entry.Path);
                file.Tag.Lyrics = entry.Before;
                file.Save();
                ScanSoon();
                return true;
            }
            var ours = entry.Path.EndsWith(".lrc", StringComparison.OrdinalIgnoreCase) ? IsOctos(entry.Path) : File.Exists(entry.Path);
            if (!ours) return false;
            if (entry.Before is null) File.Delete(entry.Path);
            else File.WriteAllText(entry.Path, entry.Before, Utf8NoBom);
            return true;
        }
        catch (Exception ex)
        {
            _logger.LogWarning("Could not put back the lyrics of {Path}: {M}", entry.Path, ex.Message);
            return false;
        }
    }

    private static string? ReadTagLyrics(string audioPath)
    {
        try
        {
            using var file = TagLib.File.Create(audioPath);
            return string.IsNullOrEmpty(file.Tag.Lyrics) ? null : file.Tag.Lyrics;
        }
        catch
        {
            return null;
        }
    }

    private static async Task WriteFileAsync(string stem, LyricsResult found, CancellationToken ct)
    {
        if (found.HasSynced)
            await File.WriteAllTextAsync(stem + ".lrc",
                OctoMark + "\n" + found.Synced!.Replace("\r\n", "\n").Trim() + "\n", Utf8NoBom, ct);
        else
            await File.WriteAllTextAsync(stem + ".txt", found.Plain!.Replace("\r\n", "\n").Trim() + "\n", Utf8NoBom, ct);
    }

    /// <summary>The lyrics in the song's own tags, marked as Octo's. False when the file could
    /// not be written.</summary>
    private bool WriteInside(string audioPath, LyricsResult found)
    {
        try
        {
            using var file = TagLib.File.Create(audioPath);
            var text = found.HasSynced ? found.Synced! : found.Plain!;
            file.Tag.Lyrics = OctoMark + "\n" + text.Replace("\r\n", "\n").Trim();
            file.Save();
            return true;
        }
        catch (Exception ex)
        {
            _logger.LogWarning("Could not write lyrics inside {Path}: {M}", audioPath, ex.Message);
            return false;
        }
    }

    /// <summary>Navidrome reads a song's tags only when it scans, so one scan is asked for a
    /// little after lyrics are written inside, once for a run of them.</summary>
    private void ScanSoon()
    {
        if (_scopes is null || Interlocked.Exchange(ref _scanPending, 1) == 1) return;
        _ = Task.Run(async () =>
        {
            try
            {
                await Task.Delay(ScanDelay);
                Interlocked.Exchange(ref _scanPending, 0);
                using var scope = _scopes.CreateScope();
                await scope.ServiceProvider.GetRequiredService<Octo.Services.Local.ILocalLibraryService>()
                    .TriggerLibraryScanAsync();
            }
            catch (Exception ex)
            {
                Interlocked.Exchange(ref _scanPending, 0);
                _logger.LogDebug("Scan after writing lyrics inside songs failed: {M}", ex.Message);
            }
        });
    }

    private static string Stem(string audioPath) =>
        Path.Combine(Path.GetDirectoryName(audioPath)!, Path.GetFileNameWithoutExtension(audioPath));

    /// <summary>Whether a .lrc opens with Octo's own mark.</summary>
    internal static bool IsOctos(string lrcPath)
    {
        try
        {
            if (!File.Exists(lrcPath)) return false;
            using var reader = new StreamReader(lrcPath, Encoding.UTF8);
            return string.Equals(reader.ReadLine()?.Trim().TrimStart('﻿'), OctoMark, StringComparison.Ordinal);
        }
        catch
        {
            return false;
        }
    }

    private async Task RetryLaterAsync(LyricsJob job, CancellationToken ct)
    {
        try
        {
            await Task.Delay(RetryDelay, ct);
            TryEnqueue(job);
        }
        catch (OperationCanceledException) { /* shutting down */ }
    }

    private static int? ReadDuration(string path)
    {
        try
        {
            using var file = TagLib.File.Create(path);
            var seconds = (int)Math.Round(file.Properties.Duration.TotalSeconds);
            return seconds > 0 ? seconds : null;
        }
        catch
        {
            return null;
        }
    }
}
