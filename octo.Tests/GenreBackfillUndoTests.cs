using System.Text;
using Microsoft.Extensions.Configuration;
using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.Logging.Abstractions;
using Octo.Models.Settings;
using Octo.Services.Metadata;

namespace Octo.Tests;

/// <summary>
/// Undo is the only thing that makes an apply reversible, so what it keeps in the journal is
/// pinned here. Before this, any undo cleared the whole journal when it finished, including one
/// that was cancelled half way, hit an unreadable file, or ran while the music folder's mount
/// was down, and the undo for everything it had not reached was gone.
/// </summary>
public class GenreBackfillUndoTests : IDisposable
{
    private readonly string _dir = Path.Combine(Path.GetTempPath(), "octo-undo-" + Guid.NewGuid());
    private readonly Xunit.Abstractions.ITestOutputHelper _output;

    public GenreBackfillUndoTests(Xunit.Abstractions.ITestOutputHelper output)
    {
        _output = output;
        Directory.CreateDirectory(_dir);
    }

    public void Dispose()
    {
        try { Directory.Delete(_dir, true); } catch { }
    }

    private static GenreJournalEntry Entry(string path, string before, string after, int minute) =>
        new(path, [before], [after], new DateTime(2026, 9, 22, 12, minute, 0, DateTimeKind.Utc), "run");

    private GenreBackfillWorker Worker(GenreBackfillStore store, GenreBackfillJournal journal) =>
        new(store, journal, TestOptions.Monitor(new GenreSettings()),
            new ConfigurationBuilder().Build(),
            new ServiceCollection().BuildServiceProvider().GetRequiredService<IServiceScopeFactory>(),
            NullLogger<GenreBackfillWorker>.Instance);

    private static async Task RunUntilIdle(GenreBackfillWorker worker, GenreBackfillStore store)
    {
        await worker.StartAsync(CancellationToken.None);
        try
        {
            var deadline = DateTime.UtcNow.AddSeconds(10);
            // Replace() flips the status to Running as the undo starts; wait for it to leave it.
            while (DateTime.UtcNow < deadline
                   && (store.Current.Status is GenreBackfillStatus.Idle or GenreBackfillStatus.Running))
                await Task.Delay(25);
        }
        finally
        {
            await worker.StopAsync(CancellationToken.None);
        }
    }

    /// <summary>A file that cannot be read and a file that is not there are both left for a later
    /// undo rather than dropped.</summary>
    [Fact]
    public async Task Undo_KeepsEntriesItCouldNotRestore()
    {
        var unreadable = Path.Combine(_dir, "not-audio.xyz");
        File.WriteAllBytes(unreadable, new byte[16]);
        var missing = Path.Combine(_dir, "moved-away.flac");

        using var store = new GenreBackfillStore(Path.Combine(_dir, "state.json"));
        var journal = new GenreBackfillJournal(Path.Combine(_dir, "journal.jsonl"));
        journal.Append(Entry(missing, "Rock", "Metal", 1));
        journal.Append(Entry(unreadable, "Jazz", "Blues", 2));

        var worker = Worker(store, journal);
        Assert.True(worker.TryEnqueue(new GenreBackfillRequest(GenreBackfillScope.WholeLibrary, false, Undo: true)));
        await RunUntilIdle(worker, store);

        Assert.Equal(GenreBackfillStatus.Completed, store.Current.Status);
        var left = journal.ReadAll().Select(entry => entry.Path).OrderBy(p => p).ToList();
        Assert.Equal(new[] { missing, unreadable }.OrderBy(p => p), left);
        Assert.Contains("stay in the journal", store.Current.Reason);
    }

    /// <summary>
    /// Best effort: a 44-byte PCM WAV is the smallest file TagLib will open and save. If this
    /// build of TagLib cannot tag one, the other tests still pin the retention rules through
    /// Remaining().
    /// </summary>
    [Fact]
    public async Task Undo_RestoresAWritableFile_AndDropsItsEntry()
    {
        var wav = Path.Combine(_dir, "tiny.wav");
        File.WriteAllBytes(wav, MinimalWav());
        try
        {
            using var probe = TagLib.File.Create(wav);
            probe.Tag.Genres = ["Metal"];
            probe.Save();
        }
        catch (Exception ex)
        {
            // This TagLib cannot tag a bare WAV; say so rather than pass silently.
            _output.WriteLine($"SKIPPED IN EFFECT: TagLib could not tag a minimal WAV ({ex.GetType().Name}: {ex.Message})");
            return;
        }
        _output.WriteLine("TagLib tagged the minimal WAV; the restore path is exercised for real.");

        using var store = new GenreBackfillStore(Path.Combine(_dir, "state.json"));
        var journal = new GenreBackfillJournal(Path.Combine(_dir, "journal.jsonl"));
        journal.Append(Entry(wav, "Rock", "Metal", 1));

        var worker = Worker(store, journal);
        Assert.True(worker.TryEnqueue(new GenreBackfillRequest(GenreBackfillScope.WholeLibrary, false, Undo: true)));
        await RunUntilIdle(worker, store);

        Assert.False(journal.Exists);
        using var after = TagLib.File.Create(wav);
        Assert.Equal(["Rock"], after.Tag.Genres);
    }

    /// <summary>
    /// A bounded channel in DropWrite mode reports a dropped write as a success, so a second
    /// request while the first was still enumerating used to come back 202 and then vanish, or
    /// run afterwards without a second confirmation.
    /// </summary>
    [Fact]
    public void TryEnqueue_RefusesWhileARequestIsWaiting()
    {
        using var store = new GenreBackfillStore();
        var worker = Worker(store, new GenreBackfillJournal());

        Assert.True(worker.TryEnqueue(new GenreBackfillRequest(GenreBackfillScope.OctoDownloads, true)));
        Assert.False(worker.TryEnqueue(new GenreBackfillRequest(GenreBackfillScope.OctoDownloads, false)));
    }

    [Fact]
    public void Remaining_KeepsFailedSkippedAndUnreachedEntries_OldestFirst()
    {
        var newestFirst = new[]
        {
            Entry("/m/c.flac", "C0", "C1", 3),   // restored
            Entry("/m/b.flac", "B0", "B1", 2),   // failed
            Entry("/m/a.flac", "A0", "A1", 1),   // never reached
        };
        var outcomes = new Dictionary<int, bool?> { [0] = true, [1] = false };

        var left = GenreBackfillWorker.Remaining(newestFirst, outcomes);

        Assert.Equal(["/m/a.flac", "/m/b.flac"], left.Select(entry => entry.Path));
    }

    /// <summary>A file changed twice: the newer entry failed, the older one restored the original.
    /// Keeping the newer one would put the in-between genre back on the next undo.</summary>
    [Fact]
    public void Remaining_DropsNewerEntriesAnOlderRestoreSuperseded()
    {
        var newestFirst = new[]
        {
            Entry("/m/x.flac", "Mid", "Final", 2),   // failed
            Entry("/m/x.flac", "Original", "Mid", 1), // restored
        };
        var outcomes = new Dictionary<int, bool?> { [0] = false, [1] = true };

        Assert.Empty(GenreBackfillWorker.Remaining(newestFirst, outcomes));
    }

    [Fact]
    public void Rewrite_RoundTripsOldestFirst()
    {
        var journal = new GenreBackfillJournal(Path.Combine(_dir, "journal.jsonl"));
        journal.Rewrite([Entry("/m/old.flac", "A", "B", 1), Entry("/m/new.flac", "C", "D", 2)]);

        // ReadAll is newest first.
        Assert.Equal(["/m/new.flac", "/m/old.flac"], journal.ReadAll().Select(entry => entry.Path));

        journal.Rewrite([]);
        Assert.False(journal.Exists);
    }

    [Fact]
    public void HashSettings_IsStable()
    {
        Assert.Equal(GenreBackfillWorker.HashSettings(new GenreSettings()),
            GenreBackfillWorker.HashSettings(new GenreSettings()));
    }

    /// <summary>How long a run keeps trying is not what it writes, so changing it must not
    /// withdraw a preview's Apply.</summary>
    [Fact]
    public void HashSettings_IgnoresTheFailureCeiling()
    {
        Assert.Equal(GenreBackfillWorker.HashSettings(new GenreSettings()),
            GenreBackfillWorker.HashSettings(new GenreSettings { BackfillMaxConsecutiveFailures = 3 }));
    }

    /// <summary>Journal paths come from one walk; on Linux a.flac and A.flac are two files.</summary>
    [Fact]
    public void Remaining_ComparesPathsExactly()
    {
        var newestFirst = new[]
        {
            Entry("/m/A.flac", "X", "Y", 2),   // failed
            Entry("/m/a.flac", "P", "Q", 1),   // restored
        };
        var outcomes = new Dictionary<int, bool?> { [0] = false, [1] = true };

        Assert.Equal(["/m/A.flac"], GenreBackfillWorker.Remaining(newestFirst, outcomes).Select(entry => entry.Path));
    }

    [Fact]
    public void HashSettings_ChangesWithAMapping()
    {
        var edited = new GenreSettings();
        edited.Mappings.Add(new GenreMappingSettings { Pattern = "garage", Genre = "Rock" });

        Assert.NotEqual(GenreBackfillWorker.HashSettings(new GenreSettings()),
            GenreBackfillWorker.HashSettings(edited));
    }

    private static byte[] MinimalWav()
    {
        using var stream = new MemoryStream();
        using var writer = new BinaryWriter(stream, Encoding.ASCII);
        writer.Write("RIFF"u8.ToArray());
        writer.Write(36);                 // chunk size: 4 + (8 + 16) + (8 + 0)
        writer.Write("WAVE"u8.ToArray());
        writer.Write("fmt "u8.ToArray());
        writer.Write(16);
        writer.Write((short)1);           // PCM
        writer.Write((short)1);           // mono
        writer.Write(44100);
        writer.Write(88200);
        writer.Write((short)2);
        writer.Write((short)16);
        writer.Write("data"u8.ToArray());
        writer.Write(0);
        writer.Flush();
        return stream.ToArray();
    }
}
