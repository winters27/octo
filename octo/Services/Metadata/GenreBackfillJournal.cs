using System.Text.Json;
using System.Text.Json.Serialization;

namespace Octo.Services.Metadata;

/// <summary>
/// One changed genre frame. Short property names because this file gets one line per changed
/// file and a whole-library run writes thousands.
/// </summary>
public sealed record GenreJournalEntry(
    [property: JsonPropertyName("p")] string Path,
    [property: JsonPropertyName("b")] IReadOnlyList<string> Before,
    [property: JsonPropertyName("a")] IReadOnlyList<string> After,
    [property: JsonPropertyName("t")] DateTime AtUtc,
    [property: JsonPropertyName("r")] string RunId);

/// <summary>
/// Append-only record of every genre frame the backfill changed, and the only thing that makes
/// an apply reversible in practice rather than in principle.
///
/// One JSON object per line rather than one array, so an interrupted run leaves a readable
/// file instead of an unparseable one, and appending never rewrites what is already there.
/// A torn final line is skipped on read rather than failing the whole journal.
///
/// What this CANNOT undo, and the UI has to say so:
///  - TagLib's Save() rewrites the whole tag block. Anything it does not round-trip was lost
///    on the first save and no amount of journal replay brings it back.
///  - Entries are keyed by path, so a file moved or renamed since the run stays rewritten.
///  - No journal, no undo. /app/config is a bind mount the user may not back up.
/// </summary>
public sealed class GenreBackfillJournal
{
    private readonly string? _path;
    private readonly ILogger<GenreBackfillJournal>? _logger;
    private readonly object _lock = new();

    public GenreBackfillJournal(string? path = null, ILogger<GenreBackfillJournal>? logger = null)
    {
        _path = string.IsNullOrWhiteSpace(path) ? null : path;
        _logger = logger;
    }

    public bool Exists => _path is not null && File.Exists(_path);

    public void Append(GenreJournalEntry entry)
    {
        if (_path is null) return;
        try
        {
            lock (_lock)
            {
                Directory.CreateDirectory(Path.GetDirectoryName(_path)!);
                File.AppendAllText(_path, JsonSerializer.Serialize(entry) + Environment.NewLine);
            }
        }
        catch (Exception ex)
        {
            // A journal write that fails costs the undo for that one file. It must never stop
            // the run, but it is a Warning because it silently reduces what can be recovered.
            _logger?.LogWarning("genre backfill journal could not record {Path}: {M}", entry.Path, ex.Message);
        }
    }

    /// <summary>
    /// Newest first, which is the order an undo replays them in: if a file was changed twice,
    /// the oldest entry holds the frame it started with, so the LAST one applied wins.
    /// </summary>
    public IReadOnlyList<GenreJournalEntry> ReadAll()
    {
        if (_path is null || !File.Exists(_path)) return [];

        var entries = new List<GenreJournalEntry>();
        var skipped = 0;
        try
        {
            lock (_lock)
            {
                foreach (var line in File.ReadLines(_path))
                {
                    if (string.IsNullOrWhiteSpace(line)) continue;
                    try
                    {
                        var entry = JsonSerializer.Deserialize<GenreJournalEntry>(line);
                        if (entry is not null && !string.IsNullOrEmpty(entry.Path)) entries.Add(entry);
                    }
                    catch (JsonException) { skipped++; }
                }
            }
        }
        catch (Exception ex)
        {
            _logger?.LogWarning("genre backfill journal could not be read: {M}", ex.Message);
            return entries;
        }

        // A run killed mid-append leaves a partial last line. Skipping it costs the undo for
        // one file; refusing to parse the file would cost the undo for all of them.
        if (skipped > 0)
            _logger?.LogWarning("genre backfill journal had {Count} unreadable line(s), skipped", skipped);

        entries.Reverse();
        return entries;
    }

    public void Clear()
    {
        if (_path is null) return;
        try
        {
            lock (_lock) { if (File.Exists(_path)) File.Delete(_path); }
        }
        catch (Exception ex)
        {
            _logger?.LogWarning("genre backfill journal could not be cleared: {M}", ex.Message);
        }
    }
}
