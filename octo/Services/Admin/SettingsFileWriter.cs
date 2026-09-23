using System.Text.Json;
using System.Text.Json.Nodes;

namespace Octo.Services.Admin;

/// <summary>
/// Thrown instead of writing when settings.json exists but cannot be parsed. Overwriting it would
/// replace everything the user had saved with whatever one form happened to send.
/// </summary>
public sealed class SettingsFileCorruptException(string path, string message) : Exception(message)
{
    public string Path { get; } = path;
}

/// <summary>
/// Reads and writes the editable settings JSON file. Writes do a deep merge
/// on top of any existing content so partial updates from the admin UI never
/// blow away unrelated keys (e.g. saving the LastFm tab won't drop Soulseek
/// settings).
///
/// Parsing accepts comments and trailing commas, because the configuration provider that reads
/// this same file does, so a hand-annotated file is valid for Octo. Comments are dropped on the
/// next save. A file that cannot be parsed at all is refused rather than overwritten.
/// </summary>
public class SettingsFileWriter
{
    // The same leniency Microsoft.Extensions.Configuration.Json applies when it loads the file.
    private static readonly JsonDocumentOptions ParseOptions = new()
    {
        CommentHandling = JsonCommentHandling.Skip,
        AllowTrailingCommas = true,
    };

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
                return JsonNode.Parse(json, null, ParseOptions) as JsonObject ?? new JsonObject();
            }
            catch
            {
                // Display-only readers get an empty object; writers go through ReadForWrite,
                // which refuses instead.
                return new JsonObject();
            }
        }
    }

    /// <summary>False only when the file exists, has content, and that content is not a JSON
    /// object. Missing and empty files are fine: there is simply nothing saved yet.</summary>
    public bool IsReadable()
    {
        lock (_lock)
        {
            try
            {
                ReadForWrite();
                return true;
            }
            catch (SettingsFileCorruptException)
            {
                return false;
            }
        }
    }

    /// <summary>
    /// Deep-merge <paramref name="patch"/> into the existing file and write
    /// atomically (write to .tmp, then rename). Returns the merged result so
    /// the caller can echo it back to the UI without re-reading.
    ///
    /// <paramref name="replaceObjects"/> names "Section.Key" objects that are replaced whole
    /// rather than merged. A dictionary setting needs this, or a key the user removed would
    /// survive in the file forever.
    /// </summary>
    public JsonObject Merge(JsonObject patch, IReadOnlyCollection<string>? replaceObjects = null)
    {
        lock (_lock)
        {
            var current = ReadForWrite();

            foreach (var path in replaceObjects ?? [])
            {
                var parts = path.Split('.', 2);
                if (parts.Length != 2) continue;
                if (patch[parts[0]] is JsonObject patchSection
                    && patchSection[parts[1]] is JsonObject
                    && current[parts[0]] is JsonObject currentSection)
                    currentSection.Remove(parts[1]);
            }

            DeepMerge(current, patch);
            Write(current);
            return current;
        }
    }

    /// <summary>Replace the whole file with <paramref name="content"/>. The Raw config editor's
    /// save, routed through here so it shares the lock and the atomic write with Merge.</summary>
    public void Replace(JsonObject content)
    {
        lock (_lock)
        {
            Write(content);
        }
    }

    private JsonObject ReadForWrite()
    {
        if (!File.Exists(_path)) return new JsonObject();
        var json = File.ReadAllText(_path);
        if (string.IsNullOrWhiteSpace(json)) return new JsonObject();
        try
        {
            if (JsonNode.Parse(json, null, ParseOptions) is JsonObject parsed) return parsed;
        }
        catch (JsonException)
        {
            // Falls through to the refusal below.
        }
        throw new SettingsFileCorruptException(_path,
            "settings.json is not valid JSON, so Octo will not write over it.");
    }

    private void Write(JsonObject content)
    {
        Directory.CreateDirectory(Path.GetDirectoryName(_path) ?? ".");
        var tmp = _path + ".tmp";
        var opts = new JsonSerializerOptions { WriteIndented = true };
        File.WriteAllText(tmp, content.ToJsonString(opts));
        File.Move(tmp, _path, overwrite: true);
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
