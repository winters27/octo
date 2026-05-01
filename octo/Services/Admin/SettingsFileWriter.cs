using System.Text.Json;
using System.Text.Json.Nodes;

namespace Octo.Services.Admin;

/// <summary>
/// Reads and writes the editable settings JSON file. Writes do a deep merge
/// on top of any existing content so partial updates from the admin UI never
/// blow away unrelated keys (e.g. saving the LastFm tab won't drop Soulseek
/// settings).
/// </summary>
public class SettingsFileWriter
{
    private readonly string _path;
    private readonly object _lock = new();

    public string FilePath => _path;

    public SettingsFileWriter(string path)
    {
        _path = path;
    }

    public JsonObject Load()
    {
        lock (_lock)
        {
            if (!File.Exists(_path)) return new JsonObject();
            try
            {
                var json = File.ReadAllText(_path);
                if (string.IsNullOrWhiteSpace(json)) return new JsonObject();
                return JsonNode.Parse(json) as JsonObject ?? new JsonObject();
            }
            catch
            {
                // Corrupt file — start fresh rather than crash. Caller can
                // overwrite with a clean structure on next save.
                return new JsonObject();
            }
        }
    }

    /// <summary>
    /// Deep-merge <paramref name="patch"/> into the existing file and write
    /// atomically (write to .tmp, then rename). Returns the merged result so
    /// the caller can echo it back to the UI without re-reading.
    /// </summary>
    public JsonObject Merge(JsonObject patch)
    {
        lock (_lock)
        {
            var current = Load();
            DeepMerge(current, patch);

            Directory.CreateDirectory(Path.GetDirectoryName(_path) ?? ".");
            var tmp = _path + ".tmp";
            var opts = new JsonSerializerOptions { WriteIndented = true };
            File.WriteAllText(tmp, current.ToJsonString(opts));
            File.Move(tmp, _path, overwrite: true);
            return current;
        }
    }

    private static void DeepMerge(JsonObject target, JsonObject patch)
    {
        foreach (var (key, value) in patch)
        {
            if (value is JsonObject patchChild
                && target[key] is JsonObject targetChild)
            {
                DeepMerge(targetChild, patchChild);
            }
            else
            {
                // Replace primitives, arrays, or null values wholesale.
                target[key] = value?.DeepClone();
            }
        }
    }
}
