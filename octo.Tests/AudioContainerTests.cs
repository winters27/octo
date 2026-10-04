using Octo.Services.Tagging;

namespace Octo.Tests;

/// <summary>
/// A file's real format from its first bytes, and the rename that fixes a name that says another
/// one. Real headers, written to files in a temp folder.
/// </summary>
public sealed class AudioContainerTests : IDisposable
{
    private readonly string _root = Path.Combine(Path.GetTempPath(), "octo-container-" + Guid.NewGuid().ToString("N"));

    public AudioContainerTests() => Directory.CreateDirectory(_root);

    public void Dispose()
    {
        try { if (Directory.Exists(_root)) Directory.Delete(_root, recursive: true); } catch { /* best effort */ }
    }

    private string Write(string name, byte[] bytes)
    {
        var path = Path.Combine(_root, name);
        File.WriteAllBytes(path, bytes);
        return path;
    }

    private static byte[] Concat(params byte[][] parts) => parts.SelectMany(part => part).ToArray();

    /// <summary>An ID3v2.4 tag of <paramref name="bodySize"/> zero bytes, its size syncsafe.</summary>
    private static byte[] Id3(int bodySize) => Concat(
        "ID3"u8.ToArray(), [0x04, 0x00, 0x00,
            (byte)((bodySize >> 21) & 0x7F), (byte)((bodySize >> 14) & 0x7F), (byte)((bodySize >> 7) & 0x7F), (byte)(bodySize & 0x7F)],
        new byte[bodySize]);

    // An iTunes M4A's ftyp box, as the file starts.
    private static readonly byte[] M4aHead =
    [
        0x00, 0x00, 0x00, 0x20, (byte)'f', (byte)'t', (byte)'y', (byte)'p', (byte)'M', (byte)'4', (byte)'A', (byte)' ',
        0x00, 0x00, 0x00, 0x00, (byte)'M', (byte)'4', (byte)'A', (byte)' ', (byte)'m', (byte)'p', (byte)'4', (byte)'2',
        (byte)'i', (byte)'s', (byte)'o', (byte)'m', 0x00, 0x00, 0x00, 0x00,
    ];

    /// <summary>The first Ogg page of a stream whose first packet starts with <paramref name="packet"/>.</summary>
    private static byte[] OggPage(byte[] packet) => Concat(
        "OggS"u8.ToArray(), [0x00, 0x02], new byte[8], [0x01, 0x00, 0x00, 0x00], new byte[4], new byte[4],
        [0x01, (byte)packet.Length], packet);

    private static readonly byte[] WavHead = Concat("RIFF"u8.ToArray(), [0x24, 0x00, 0x00, 0x00], "WAVEfmt "u8.ToArray(),
        [0x10, 0x00, 0x00, 0x00, 0x01, 0x00, 0x02, 0x00, 0x44, 0xAC, 0x00, 0x00]);

    private static readonly byte[] AiffHead = Concat("FORM"u8.ToArray(), [0x00, 0x00, 0x00, 0x2E], "AIFFCOMM"u8.ToArray(), new byte[22]);

    // Two ADTS headers: AAC LC, 44.1 kHz, stereo.
    private static readonly byte[] AdtsHead = [0xFF, 0xF1, 0x50, 0x80, 0x02, 0x1F, 0xFC, 0x21, 0x00, 0x49, 0x90, 0x02, 0x19, 0x00];

    public static TheoryData<string, byte[], string?> Heads => new()
    {
        { "flac", AudioFixtures.Flac(), ".flac" },
        { "mp3", AudioFixtures.Mp3(), ".mp3" },
        { "flac behind an ID3 tag", Concat(Id3(200), AudioFixtures.Flac()), ".flac" },
        { "mp3 behind an ID3 tag and padding", Concat(Id3(64), new byte[300], AudioFixtures.Mp3()), ".mp3" },
        { "m4a", Concat(M4aHead, new byte[64]), ".m4a" },
        { "ogg vorbis", OggPage(Concat([0x01], "vorbis"u8.ToArray(), new byte[23])), ".ogg" },
        { "ogg opus", OggPage(Concat("OpusHead"u8.ToArray(), [0x01, 0x02, 0x38, 0x01, 0x80, 0xBB, 0x00, 0x00, 0x00, 0x00, 0x00])), ".opus" },
        { "wav", WavHead, ".wav" },
        { "aiff", AiffHead, ".aiff" },
        { "adts aac", AdtsHead, ".aac" },
        { "text", "not music at all"u8.ToArray(), null },
        { "empty", [], null },
    };

    [Theory]
    [MemberData(nameof(Heads))]
    public void TheFirstBytesSayTheFormat(string label, byte[] bytes, string? expected) =>
        Assert.True(expected == AudioContainer.Detect(Write($"{label}.bin", bytes)), $"{label}: expected {expected ?? "nothing"}");

    [Fact]
    public void AFlacNamedMp3_IsRenamedFlac_AndKeepsItsBytes()
    {
        var bytes = AudioFixtures.Flac();
        var path = Write("01 - Teardrop.mp3", bytes);

        var fix = AudioContainer.FixExtension(path);

        Assert.NotNull(fix);
        Assert.Equal((".mp3", ".flac", "a FLAC"), (fix.From, fix.To, fix.Format));
        Assert.Equal(Path.Combine(_root, "01 - Teardrop.flac"), fix.Path);
        Assert.False(File.Exists(path));
        Assert.Equal(bytes, File.ReadAllBytes(fix.Path));
    }

    [Fact]
    public void AnM4aNamedFlac_IsRenamedM4a()
    {
        var fix = AudioContainer.FixExtension(Write("Angel.flac", Concat(M4aHead, new byte[64])));

        Assert.Equal((".flac", ".m4a", "an M4A"), (fix!.From, fix.To, fix.Format));
        Assert.True(File.Exists(Path.Combine(_root, "Angel.m4a")));
    }

    [Fact]
    public void AnMp3NamedFlac_IsRenamedMp3()
    {
        var fix = AudioContainer.FixExtension(Write("Angel.flac", Concat(Id3(32), AudioFixtures.Mp3())));

        Assert.Equal((".flac", ".mp3"), (fix!.From, fix.To));
    }

    [Theory]
    [InlineData("right.flac")]
    [InlineData("upper.FLAC")]
    public void ARightName_IsLeftAlone(string name)
    {
        var path = Write(name, AudioFixtures.Flac());

        Assert.Null(AudioContainer.FixExtension(path));
        Assert.True(File.Exists(path));
    }

    [Fact]
    public void AnotherNameForTheSameFormat_IsLeftAlone()
    {
        // The tagger reads any Ogg under either name, and an MP4 under .mp4 as under .m4a.
        Assert.Null(AudioContainer.FixExtension(Write("vorbis.opus", OggPage(Concat([0x01], "vorbis"u8.ToArray(), new byte[23])))));
        Assert.Null(AudioContainer.FixExtension(Write("song.mp4", Concat(M4aHead, new byte[64]))));
    }

    [Fact]
    public void AFileNotNamedAsMusic_OrOfNoKnownFormat_IsLeftAlone()
    {
        Assert.Null(AudioContainer.FixExtension(Write("notes.txt", AudioFixtures.Flac())));
        var unknown = Write("mystery.mp3", "not music at all"u8.ToArray());
        Assert.Null(AudioContainer.FixExtension(unknown));
        Assert.True(File.Exists(unknown));
    }

    [Fact]
    public void AFileAlreadyAtTheRightName_IsNeverOverwritten()
    {
        var existing = Write("Teardrop.flac", [1, 2, 3]);
        var path = Write("Teardrop.mp3", AudioFixtures.Flac());

        var fix = AudioContainer.FixExtension(path);

        Assert.Equal(Path.Combine(_root, "Teardrop (2).flac"), fix!.Path);
        Assert.Equal([1, 2, 3], File.ReadAllBytes(existing));
    }
}
