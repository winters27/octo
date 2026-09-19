using Octo.Services.Library;

namespace Octo.Tests;

/// <summary>
/// The resolver is the last thing standing between a Navidrome song id and anything that acts
/// on a file.
///
/// Navidrome's Subsonic `path` is SYNTHESISED FROM TAGS unless the calling player has
/// ReportRealPath set, which defaults off. Verified against the production library on
/// 2026-09-16: six random tracks, six different answers, because that library is flat
/// (`Artist - Title.flac`) while the API reports `Artist/Album/Title.flac`. On a flat library
/// the fake path resolves to nothing, which fails safe. On an Organized library it can name a
/// real file that is a DIFFERENT recording, and the byte-size check is what catches that.
/// </summary>
public class NavidromeSongPathResolverTests : IDisposable
{
    private readonly string _root = Path.Combine(Path.GetTempPath(), "octo-resolve-" + Guid.NewGuid());

    public NavidromeSongPathResolverTests() => Directory.CreateDirectory(_root);

    public void Dispose()
    {
        try { Directory.Delete(_root, true); } catch { }
        GC.SuppressFinalize(this);
    }

    private string WriteFile(string relative, int bytes)
    {
        var full = Path.Combine(_root, relative.Replace('/', Path.DirectorySeparatorChar));
        Directory.CreateDirectory(Path.GetDirectoryName(full)!);
        File.WriteAllBytes(full, new byte[bytes]);
        return full;
    }

    private static NavidromeSongPathResolver.Candidate Candidate(
        string? path, long size, string suffix = "flac", string? libraryPath = null) =>
        new("song-id", path, libraryPath, size, "Title", "Artist", "Album", suffix, 180,
            PathSource.SubsonicGetSong);

    private NavidromeSongPathResolver Resolver() => new(
        identity: null!, library: null!, http: null!, subsonic: null!, config: null!,
        logger: new Microsoft.Extensions.Logging.Abstractions.NullLogger<NavidromeSongPathResolver>());

    [Fact]
    public void Verify_PathAndSizeAgree_Resolves()
    {
        WriteFile("Artist/Album/Song.flac", 2048);

        var resolved = Resolver().Verify(Candidate("Artist/Album/Song.flac", 2048), _root);

        Assert.NotNull(resolved);
        Assert.Equal(2048, resolved!.SizeBytes);
        Assert.EndsWith("Song.flac", resolved.AbsolutePath);
    }

    /// <summary>
    /// The fakePath case, and the reason the size check exists. The file at the reported path
    /// is real; it is simply not the recording that was asked for. Without this check that is a
    /// silent action on the wrong file.
    /// </summary>
    [Fact]
    public void Verify_SizeMismatch_RefusesEvenThoughTheFileExists()
    {
        WriteFile("Artist/Album/Song.flac", 1024);

        Assert.Null(Resolver().Verify(Candidate("Artist/Album/Song.flac", 9999), _root));
    }

    [Fact]
    public void Verify_SuffixMismatch_IsRefused()
    {
        WriteFile("Artist/Album/Song.mp3", 2048);

        Assert.Null(Resolver().Verify(Candidate("Artist/Album/Song.mp3", 2048, suffix: "flac"), _root));
    }

    /// <summary>
    /// A path from a differently-mounted Navidrome must never reach outside the music root,
    /// which is where Octo's own config and the quarantine directory live.
    /// </summary>
    [Theory]
    [InlineData("../../etc/passwd")]
    [InlineData("Artist/../../../outside.flac")]
    public void Verify_PathEscapingTheMusicRoot_IsRejected(string path)
        => Assert.Null(Resolver().Verify(Candidate(path, 2048), _root));

    [Fact]
    public void Verify_NoPathReported_ResolvesNothing()
    {
        Assert.Null(Resolver().Verify(Candidate(null, 2048), _root));
        Assert.Null(Resolver().Verify(Candidate("", 2048), _root));
    }

    /// <summary>
    /// Navidrome reporting size 0 means it did not say, not that the file is empty. The other
    /// three checks still have to hold.
    /// </summary>
    [Fact]
    public void Verify_NoSizeReported_FallsBackToTheOtherChecks()
    {
        WriteFile("Artist/Album/Song.flac", 2048);

        Assert.NotNull(Resolver().Verify(Candidate("Artist/Album/Song.flac", 0), _root));
        Assert.Null(Resolver().Verify(Candidate("Artist/Album/Missing.flac", 0), _root));
    }

    /// <summary>
    /// libraryPath is Navidrome's own root and wins when the two containers share a mount.
    /// Octo's root is the fallback, which is what the shipped compose file produces since both
    /// mount the same directory at /music.
    /// </summary>
    [Fact]
    public void CandidatePaths_PrefersLibraryPathThenTheMusicRoot()
    {
        var paths = NavidromeSongPathResolver
            .CandidatePaths(Candidate("Artist/Album/Song.flac", 1, libraryPath: _root), _root)
            .ToList();

        Assert.Equal(2, paths.Count);
        Assert.All(paths, path => Assert.EndsWith(
            Path.Combine("Artist", "Album", "Song.flac"), path));
    }

    /// <summary>
    /// A pre-0.58 Navidrome stored absolute paths. If the two containers mount the library at
    /// different places, the tail is the only part still worth trying.
    /// </summary>
    [Fact]
    public void CandidatePaths_AbsolutePath_AlsoTriesTheRootRelativeTail()
    {
        var absolute = OperatingSystem.IsWindows()
            ? @"D:\media\Artist\Album\Song.flac"
            : "/media/Artist/Album/Song.flac";

        var paths = NavidromeSongPathResolver.CandidatePaths(Candidate(absolute, 1), _root).ToList();

        Assert.Contains(paths, path => path == absolute);
        Assert.Contains(paths, path =>
            path.EndsWith(Path.Combine("Artist", "Album", "Song.flac"), StringComparison.Ordinal)
            && path.StartsWith(_root, StringComparison.Ordinal));
    }

    [Fact]
    public void IsInside_OnlyAcceptsPathsUnderTheRoot()
    {
        var inside = Path.Combine(_root, "Artist", "Song.flac");
        var sibling = _root + "-other";

        Assert.True(NavidromeSongPathResolver.IsInside(inside, _root));
        Assert.False(NavidromeSongPathResolver.IsInside(Path.Combine(sibling, "Song.flac"), _root));
        Assert.False(NavidromeSongPathResolver.IsInside(_root, _root));
    }
}
