namespace Octo.Services.Tagging;

/// <summary>A file renamed to the extension its bytes call for: the old and new extensions, the
/// format in words, and where the file is now.</summary>
public sealed record ExtensionFix(string From, string To, string Format, string Path);

/// <summary>
/// What a file really is, read from its first bytes rather than its name. A peer's file can be
/// named for one format and hold another: a FLAC named .mp3, an M4A named .flac. The tagger picks
/// how to read and write a file by its extension, so a wrong one writes tags the library server
/// cannot read, or none at all, and the library shows the wrong format. Tubifarry renames such
/// files the same way.
/// </summary>
public static class AudioContainer
{
    /// <summary>One format: the extension it gets, the other extensions that also name it, and
    /// how it reads in a sentence ("a FLAC").</summary>
    private sealed record Kind(string Extension, string[] Aliases, string Name);

    private static readonly Kind Flac = new(".flac", [], "a FLAC");
    private static readonly Kind Mp3 = new(".mp3", [], "an MP3");
    private static readonly Kind Aac = new(".aac", [], "a raw AAC stream");
    private static readonly Kind M4a = new(".m4a", [".mp4", ".m4b", ".alac"], "an M4A");
    // Ogg holds Vorbis, Opus or FLAC alike, and the tagger reads all three under any of these names.
    private static readonly Kind Ogg = new(".ogg", [".oga", ".opus"], "an Ogg");
    private static readonly Kind Opus = new(".opus", [".ogg", ".oga"], "an Opus");
    private static readonly Kind Wav = new(".wav", [], "a WAV");
    private static readonly Kind Aiff = new(".aiff", [".aif", ".aifc"], "an AIFF");
    private static readonly Kind Ape = new(".ape", [], "a Monkey's Audio");
    private static readonly Kind WavPack = new(".wv", [], "a WavPack");
    private static readonly Kind Wma = new(".wma", [".asf"], "a WMA");
    private static readonly Kind Dsf = new(".dsf", [], "a DSD");

    private static readonly Kind[] Kinds = [Flac, Mp3, Aac, M4a, Ogg, Opus, Wav, Aiff, Ape, WavPack, Wma, Dsf];

    /// <summary>The extensions this knows. A file named anything else is never renamed: it is not
    /// a music file Octo would tag.</summary>
    private static readonly HashSet<string> Known = Kinds
        .SelectMany(kind => kind.Aliases.Prepend(kind.Extension))
        .ToHashSet(StringComparer.OrdinalIgnoreCase);

    private static readonly byte[] AsfHeader = [0x30, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11];

    /// <summary>How far past an ID3 tag an MP3's first frame may sit, after padding.</summary>
    private const int FrameSearchBytes = 4096;

    /// <summary>The extension the file's bytes call for (".flac"), or null when they match no
    /// format this knows or the file cannot be read.</summary>
    public static string? Detect(string path) => DetectKind(path)?.Extension;

    /// <summary>
    /// Renames a file whose extension names another format than its bytes, before anything
    /// tags it. Null when the name is already right, the format is unknown, or the file is not
    /// named as music at all. A file already at the new name is never overwritten.
    /// </summary>
    public static ExtensionFix? FixExtension(string path)
    {
        var current = Path.GetExtension(path);
        if (!Known.Contains(current)) return null;
        if (DetectKind(path) is not { } kind) return null;
        if (string.Equals(current, kind.Extension, StringComparison.OrdinalIgnoreCase)
            || kind.Aliases.Contains(current, StringComparer.OrdinalIgnoreCase)) return null;

        var target = Path.ChangeExtension(path, kind.Extension);
        for (var n = 2; File.Exists(target); n++)
            target = Path.Combine(Path.GetDirectoryName(path) ?? "",
                $"{Path.GetFileNameWithoutExtension(path)} ({n}){kind.Extension}");
        File.Move(path, target);
        return new ExtensionFix(current.ToLowerInvariant(), kind.Extension, kind.Name, target);
    }

    private static Kind? DetectKind(string path)
    {
        try
        {
            using var stream = new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.Read);
            var head = Read(stream, 0, 64);
            if (Magic(head) is { } kind) return kind;
            if (Frame(head, 0) is { } frame) return frame;
            if (!StartsWith(head, "ID3"u8) || head.Length < 10) return null;

            // An ID3 tag in front: what follows it says the format. FLACs carry one now and then.
            var size = (head[6] & 0x7F) << 21 | (head[7] & 0x7F) << 14 | (head[8] & 0x7F) << 7 | (head[9] & 0x7F);
            var end = 10L + size + ((head[5] & 0x10) != 0 ? 10 : 0);
            var after = Read(stream, end, FrameSearchBytes);
            if (Magic(after) is { } behind) return behind;
            for (var i = 0; i + 1 < after.Length; i++)
                if (Frame(after, i) is { } found) return found;
            return null;
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException)
        {
            return null;
        }
    }

    /// <summary>Formats that start with a fixed signature.</summary>
    private static Kind? Magic(byte[] head)
    {
        if (StartsWith(head, "fLaC"u8)) return Flac;
        if (head.Length >= 12 && head.AsSpan(4, 4).SequenceEqual("ftyp"u8)) return M4a;
        if (StartsWith(head, "OggS"u8)) return head.AsSpan().IndexOf("OpusHead"u8) >= 0 ? Opus : Ogg;
        if (head.Length >= 12 && StartsWith(head, "RIFF"u8) && head.AsSpan(8, 4).SequenceEqual("WAVE"u8)) return Wav;
        if (head.Length >= 12 && StartsWith(head, "FORM"u8)
            && (head.AsSpan(8, 4).SequenceEqual("AIFF"u8) || head.AsSpan(8, 4).SequenceEqual("AIFC"u8))) return Aiff;
        if (StartsWith(head, "MAC "u8)) return Ape;
        if (StartsWith(head, "wvpk"u8)) return WavPack;
        if (StartsWith(head, AsfHeader)) return Wma;
        if (StartsWith(head, "DSD "u8)) return Dsf;
        return null;
    }

    /// <summary>An MPEG audio frame's sync at <paramref name="at"/>: eleven bits set, then the
    /// layer. Layer bits of 00 are an ADTS header, which is AAC; any other layer is MP3's family.</summary>
    private static Kind? Frame(byte[] bytes, int at)
    {
        if (at + 1 >= bytes.Length || bytes[at] != 0xFF || (bytes[at + 1] & 0xE0) != 0xE0) return null;
        var version = (bytes[at + 1] >> 3) & 0x03;
        var layer = (bytes[at + 1] >> 1) & 0x03;
        if (layer == 0) return (bytes[at + 1] & 0xF0) == 0xF0 ? Aac : null;
        // 01 is a reserved MPEG version: not a frame.
        return version == 1 ? null : Mp3;
    }

    private static bool StartsWith(byte[] bytes, ReadOnlySpan<byte> prefix) => bytes.AsSpan().StartsWith(prefix);

    private static byte[] Read(Stream stream, long offset, int count)
    {
        if (offset >= stream.Length) return [];
        stream.Seek(offset, SeekOrigin.Begin);
        var buffer = new byte[(int)Math.Min(count, stream.Length - offset)];
        var read = 0;
        while (read < buffer.Length)
        {
            var n = stream.Read(buffer, read, buffer.Length - read);
            if (n == 0) break;
            read += n;
        }
        return read == buffer.Length ? buffer : buffer[..read];
    }
}
