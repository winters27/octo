using Octo.Models.Settings;
using Octo.Services.Library;

namespace Octo.Tests;

/// <summary>
/// There is no File.Delete anywhere in library actions except the retention sweep. These
/// actions remove files Octo did NOT create, on one tap in a music client with no confirmation
/// dialog, and the whole point of the feature is that the user is correcting a mistake, which
/// means they can make one.
/// </summary>
public class LibraryActionQuarantineTests : IDisposable
{
    private readonly string _root = Path.Combine(Path.GetTempPath(), "octo-quarantine-" + Guid.NewGuid());

    public LibraryActionQuarantineTests() => Directory.CreateDirectory(_root);

    public void Dispose()
    {
        try { Directory.Delete(_root, true); } catch { }
        GC.SuppressFinalize(this);
    }

    private LibraryActionQuarantine Quarantine(LibraryActionSettings? settings = null) =>
        new(TestOptions.Monitor(settings ?? new LibraryActionSettings()),
            new Microsoft.Extensions.Logging.Abstractions.NullLogger<LibraryActionQuarantine>());

    private ResolvedSongFile WriteTrack(string relative, int bytes = 1024)
    {
        var full = Path.Combine(_root, relative.Replace('/', Path.DirectorySeparatorChar));
        Directory.CreateDirectory(Path.GetDirectoryName(full)!);
        File.WriteAllBytes(full, new byte[bytes]);
        return new ResolvedSongFile("song-id", full, bytes, "Title", "Artist", "Album",
            Path.GetExtension(full).TrimStart('.'), 180, PathSource.NativeApi);
    }

    [Fact]
    public void Move_TakesTheFileOutOfTheLibraryWithoutDeletingIt()
    {
        var track = WriteTrack("Artist/Album/Song.flac");

        var result = Quarantine().Move(track, _root, LibraryAction.Delete, "alice");

        Assert.True(result.Moved);
        Assert.False(File.Exists(track.AbsolutePath));
        Assert.True(File.Exists(result.QuarantinePath!));
    }

    /// <summary>The layout is preserved so a restore is a straight move back.</summary>
    [Fact]
    public void Move_PreservesTheRelativeLayoutUnderADatedFolder()
    {
        var track = WriteTrack("Artist/Album/Song.flac");

        var result = Quarantine().Move(track, _root, LibraryAction.Delete, "alice");

        Assert.Contains(Path.Combine("Artist", "Album", "Song.flac"), result.QuarantinePath!);
        Assert.Contains(DateTime.UtcNow.ToString("yyyy-MM-dd"), result.QuarantinePath!);
    }

    [Fact]
    public void Restore_PutsTheFileBackWhereItCameFrom()
    {
        var track = WriteTrack("Artist/Album/Song.flac");
        var quarantine = Quarantine();

        var moved = quarantine.Move(track, _root, LibraryAction.Delete, "alice");
        var restored = quarantine.Restore(moved.QuarantinePath!);

        Assert.True(restored.Moved);
        Assert.True(File.Exists(track.AbsolutePath));
        Assert.False(File.Exists(moved.QuarantinePath!));
    }

    /// <summary>
    /// The manifest is written next to the quarantined file so a restore works even if the
    /// journal is lost.
    /// </summary>
    [Fact]
    public void Restore_WorksFromTheSidecarManifestAlone()
    {
        var track = WriteTrack("Artist/Album/Song.flac");
        var moved = Quarantine().Move(track, _root, LibraryAction.Delete, "alice");

        Assert.True(File.Exists(moved.QuarantinePath + ".octo-action.json"));

        // A completely separate instance, with no shared state at all.
        Assert.True(Quarantine().Restore(moved.QuarantinePath!).Moved);
        Assert.True(File.Exists(track.AbsolutePath));
    }

    [Fact]
    public void Restore_SomethingAlreadyAtTheOriginalPath_RefusesRatherThanOverwriting()
    {
        var track = WriteTrack("Artist/Album/Song.flac");
        var quarantine = Quarantine();
        var moved = quarantine.Move(track, _root, LibraryAction.Delete, "alice");

        // A replacement arrived in the meantime.
        WriteTrack("Artist/Album/Song.flac", 2048);

        var restored = quarantine.Restore(moved.QuarantinePath!);
        Assert.False(restored.Moved);
        Assert.True(File.Exists(moved.QuarantinePath!));
    }

    [Fact]
    public void Move_Collision_KeepsBothRatherThanOverwriting()
    {
        var quarantine = Quarantine();
        var first = WriteTrack("Artist/Album/Song.flac");
        var firstMoved = quarantine.Move(first, _root, LibraryAction.Delete, "alice");

        var second = WriteTrack("Artist/Album/Song.flac", 2048);
        var secondMoved = quarantine.Move(second, _root, LibraryAction.Delete, "alice");

        Assert.NotEqual(firstMoved.QuarantinePath, secondMoved.QuarantinePath);
        Assert.True(File.Exists(firstMoved.QuarantinePath!));
        Assert.True(File.Exists(secondMoved.QuarantinePath!));
    }

    [Fact]
    public void Move_FileOutsideTheMusicRoot_IsRefused()
    {
        var outside = Path.Combine(Path.GetTempPath(), "octo-outside-" + Guid.NewGuid() + ".flac");
        File.WriteAllBytes(outside, new byte[16]);
        try
        {
            var track = new ResolvedSongFile("id", outside, 16, "T", "A", "Al", "flac", 1, PathSource.NativeApi);

            var result = Quarantine().Move(track, _root, LibraryAction.Delete, "alice");

            Assert.False(result.Moved);
            Assert.True(File.Exists(outside));
        }
        finally { try { File.Delete(outside); } catch { } }
    }

    /// <summary>The only code in this feature that really deletes, and it goes on age.</summary>
    [Fact]
    public void Sweep_RemovesOnlyFoldersPastTheRetentionWindow()
    {
        var quarantine = Quarantine(new LibraryActionSettings { QuarantineRetentionDays = 30 });
        var root = quarantine.RootFor(_root);

        var old = Path.Combine(root, DateTime.UtcNow.AddDays(-40).ToString("yyyy-MM-dd"), "a");
        var recent = Path.Combine(root, DateTime.UtcNow.AddDays(-2).ToString("yyyy-MM-dd"), "b");
        Directory.CreateDirectory(old);
        Directory.CreateDirectory(recent);
        File.WriteAllBytes(Path.Combine(old, "gone.flac"), new byte[8]);
        File.WriteAllBytes(Path.Combine(recent, "kept.flac"), new byte[8]);

        Assert.Equal(1, quarantine.Sweep(_root));
        Assert.False(Directory.Exists(Path.GetDirectoryName(old)));
        Assert.True(File.Exists(Path.Combine(recent, "kept.flac")));
    }

    /// <summary>
    /// Retention 0 means never sweep, for anyone who would rather manage the space by hand.
    /// </summary>
    [Fact]
    public void Sweep_RetentionZero_DeletesNothingEver()
    {
        var quarantine = Quarantine(new LibraryActionSettings { QuarantineRetentionDays = 0 });
        var ancient = Path.Combine(quarantine.RootFor(_root),
            DateTime.UtcNow.AddYears(-5).ToString("yyyy-MM-dd"));
        Directory.CreateDirectory(ancient);
        File.WriteAllBytes(Path.Combine(ancient, "kept.flac"), new byte[8]);

        Assert.Equal(0, quarantine.Sweep(_root));
        Assert.True(File.Exists(Path.Combine(ancient, "kept.flac")));
    }

    [Fact]
    public void RootFor_UsesTheConfiguredDirectory()
        => Assert.Equal(Path.Combine(_root, "my-bin"),
            Quarantine(new LibraryActionSettings { QuarantineDirectory = "my-bin" }).RootFor(_root));
}
