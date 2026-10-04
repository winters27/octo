using System.Text.Json;

namespace Octo.Services.Imports;

/// <summary>
/// Every imported list, with its songs and what the last match found, on disk so a restart keeps
/// them and their playlist and fetch switches. Readers get copies, so a list never changes under them.
/// </summary>
public sealed class ImportStore
{
    private sealed class FileShape
    {
        public List<ImportList> Lists { get; set; } = [];
    }

    private readonly string? _path;
    private readonly ILogger<ImportStore>? _logger;
    private readonly object _lock = new();
    private readonly List<ImportList> _lists = [];

    public ImportStore(string? path = null, ILogger<ImportStore>? logger = null)
    {
        _path = string.IsNullOrWhiteSpace(path) ? null : path;
        _logger = logger;
        try
        {
            if (_path is not null && File.Exists(_path))
                _lists.AddRange(JsonSerializer.Deserialize<FileShape>(File.ReadAllText(_path))?.Lists ?? []);
        }
        catch (Exception ex) { _logger?.LogWarning("the imported lists could not be read: {M}", ex.Message); }
    }

    /// <summary>One person's lists, liked songs first, then by name.</summary>
    public IReadOnlyList<ImportList> ListsOf(string owner)
    {
        lock (_lock)
            return _lists.Where(list => Same(list.Owner, owner))
                .OrderBy(list => list.Source == ImportSources.SpotifyLiked ? 0 : 1)
                .ThenBy(list => list.Name, StringComparer.CurrentCultureIgnoreCase)
                .Select(list => list.Copy())
                .ToList();
    }

    /// <summary>Everyone's lists, for the work that runs for all of them.</summary>
    public IReadOnlyList<ImportList> All()
    {
        lock (_lock) return _lists.Select(list => list.Copy()).ToList();
    }

    public ImportList? Get(string owner, string id)
    {
        lock (_lock) return Find(owner, id)?.Copy();
    }

    /// <summary>
    /// Adds a list, or replaces the songs and details of the one with its id. What the person chose
    /// for it (a playlist, fetching, the playlist's id) stays, and so does what the last match found
    /// for each song that is still there, until the next match says otherwise.
    /// </summary>
    public ImportList Save(ImportList read)
    {
        lock (_lock)
        {
            var existing = Find(read.Owner, read.Id);
            if (existing is not null)
            {
                read.KeepPlaylist = existing.KeepPlaylist;
                read.PlaylistId = existing.PlaylistId;
                read.PlaylistNote = existing.PlaylistNote;
                read.GetMissing = existing.GetMissing;
                read.MatchedUtc = existing.MatchedUtc;
                var known = existing.Tracks.GroupBy(track => track.Key).ToDictionary(group => group.Key, group => group.First().LibraryId);
                foreach (var track in read.Tracks)
                    if (track.LibraryId is null && known.TryGetValue(track.Key, out var id)) track.LibraryId = id;
                _lists.Remove(existing);
            }
            _lists.Add(read.Copy());
            Write();
            return read.Copy();
        }
    }

    /// <summary>Changes one list in place. False when there is no such list.</summary>
    public bool Update(string owner, string id, Action<ImportList> change)
    {
        lock (_lock)
        {
            if (Find(owner, id) is not { } list) return false;
            change(list);
            Write();
            return true;
        }
    }

    /// <summary>Changes every list one person has, in one write.</summary>
    public void UpdateAll(string owner, Action<ImportList> change)
    {
        lock (_lock)
        {
            foreach (var list in _lists.Where(list => Same(list.Owner, owner))) change(list);
            Write();
        }
    }

    public bool Remove(string owner, string id)
    {
        lock (_lock)
        {
            if (Find(owner, id) is not { } list) return false;
            _lists.Remove(list);
            Write();
            return true;
        }
    }

    // Called with the lock held.
    private ImportList? Find(string owner, string id) =>
        _lists.FirstOrDefault(list => Same(list.Owner, owner) && list.Id == id);

    private static bool Same(string a, string b) => string.Equals(a, b, StringComparison.OrdinalIgnoreCase);

    // Called with the lock held.
    private void Write()
    {
        if (_path is null) return;
        try
        {
            Directory.CreateDirectory(Path.GetDirectoryName(_path)!);
            File.WriteAllText(_path + ".tmp", JsonSerializer.Serialize(new FileShape { Lists = _lists }));
            File.Move(_path + ".tmp", _path, overwrite: true);
        }
        catch (Exception ex) { _logger?.LogWarning("the imported lists could not be written: {M}", ex.Message); }
    }
}
