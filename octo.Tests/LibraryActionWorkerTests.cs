using System.Text.Json;
using Octo.Models.Settings;
using Octo.Services.Library;

namespace Octo.Tests;

/// <summary>
/// Navidrome's native playlist rows are what the sweep reads. The shapes matter more than they
/// look: a playlist row's owner is the allowlist check, and a track row's id is a POSITION
/// rather than an identifier.
/// </summary>
public class LibraryActionWorkerTests
{
    private static JsonElement Json(string raw) => JsonDocument.Parse(raw).RootElement;

    [Fact]
    public void ParsePlaylists_ReadsIdNameAndOwner()
    {
        var rows = LibraryActionPlaylistWorker.ParsePlaylists(Json("""
        [
          {"id": "p1", "name": "🛠 Delete", "ownerName": "alice"},
          {"id": "p2", "name": "Road trip", "ownerName": "bob"}
        ]
        """));

        Assert.Equal(2, rows.Count);
        Assert.Equal("p1", rows[0].Id);
        Assert.Equal("alice", rows[0].Owner);
    }

    [Fact]
    public void ParsePlaylists_RowsMissingIdOrName_AreSkippedNotFatal()
    {
        var rows = LibraryActionPlaylistWorker.ParsePlaylists(Json("""
        [{"id": "p1"}, {"name": "no id"}, {"id": "p2", "name": "ok"}]
        """));

        Assert.Equal("p2", Assert.Single(rows).Id);
    }

    [Fact]
    public void ParsePlaylists_MissingOwner_IsEmptyRatherThanNull()
        => Assert.Equal("", LibraryActionPlaylistWorker
            .ParsePlaylists(Json("""[{"id": "p1", "name": "x"}]""")).Single().Owner);

    /// <summary>
    /// PlaylistTrack.ID is the 1-based POSITION, reassigned on every mutation. Keeping it
    /// separate from mediaFileId is what lets the sweep re-read before deleting and map the
    /// tracks it actually applied onto their CURRENT positions.
    /// </summary>
    [Fact]
    public void ParseTracks_KeepsThePositionAndTheMediaFileIdApart()
    {
        var rows = LibraryActionPlaylistWorker.ParseTracks(Json("""
        [
          {"id": "1", "mediaFileId": "song-a"},
          {"id": "2", "mediaFileId": "song-b"}
        ]
        """));

        Assert.Equal("1", rows[0].Position);
        Assert.Equal("song-a", rows[0].MediaFileId);
        Assert.Equal("2", rows[1].Position);
    }

    /// <summary>Navidrome has sent the position as a number in places, so both shapes parse.</summary>
    [Fact]
    public void ParseTracks_NumericPosition_IsRead()
        => Assert.Equal("3", LibraryActionPlaylistWorker
            .ParseTracks(Json("""[{"id": 3, "mediaFileId": "song-c"}]""")).Single().Position);

    [Fact]
    public void ParseTracks_RowWithoutAMediaFileId_IsSkipped()
        => Assert.Empty(LibraryActionPlaylistWorker.ParseTracks(Json("""[{"id": "1"}]""")));

    [Theory]
    [InlineData("{}")]
    [InlineData("null")]
    [InlineData("\"not an array\"")]
    public void Parsers_NonArrayPayloads_AreEmptyRatherThanThrowing(string raw)
    {
        Assert.Empty(LibraryActionPlaylistWorker.ParsePlaylists(Json(raw)));
        Assert.Empty(LibraryActionPlaylistWorker.ParseTracks(Json(raw)));
    }
}

/// <summary>
/// A request that could not be applied must stay where it is, so fixing whatever blocked it and
/// waiting makes it work rather than requiring the user to ask again.
/// </summary>
public class LibraryActionOutcomeTests
{
    [Theory]
    [InlineData(LibraryActionState.Applied, true)]
    [InlineData(LibraryActionState.Skipped, true)]
    [InlineData(LibraryActionState.Failed, false)]
    [InlineData(LibraryActionState.Unresolved, false)]
    [InlineData(LibraryActionState.Pending, false)]
    // A rehearsal is not a failure, and it is not consumed either: the same set has to replay
    // so the operator reads a stable list.
    [InlineData(LibraryActionState.Rehearsed, false)]
    public void Consumed_OnlyTerminalSuccessClearsTheRequest(LibraryActionState state, bool consumed)
        => Assert.Equal(consumed, new LibraryActionOutcome(state, null).Consumed);
}

public class LibraryActionJournalTests
{
    private static LibraryActionEntry Entry(LibraryAction action, string id, LibraryActionState state,
        string artist = "Artist", string title = "Title", bool dryRun = false, string fingerprint = "1:2") =>
        new(LibraryActionJournal.MakeKey(action, id, fingerprint), action, id, "alice",
            title, artist, "Album", $"/music/{id}.flac", null, PathSource.NativeApi,
            state, null, dryRun, DateTime.UtcNow);

    [Fact]
    public void AlreadyApplied_SameActionAndFileContent_IsTrue()
    {
        var journal = new LibraryActionJournal();
        journal.Record(Entry(LibraryAction.Delete, "song-a", LibraryActionState.Applied));

        Assert.True(journal.AlreadyApplied(LibraryAction.Delete, "song-a", "1:2"));
    }

    /// <summary>
    /// After a Better quality upgrade the SAME Navidrome id points at a NEW file, and the user
    /// is entitled to ask for a better copy of that one too. Fingerprinting on size and mtime
    /// rather than the id alone is what allows that.
    /// </summary>
    [Fact]
    public void AlreadyApplied_SameIdDifferentFileContent_IsFalse()
    {
        var journal = new LibraryActionJournal();
        journal.Record(Entry(LibraryAction.BetterQuality, "song-a", LibraryActionState.Applied));

        Assert.False(journal.AlreadyApplied(LibraryAction.BetterQuality, "song-a", "9:9"));
    }

    /// <summary>
    /// The one that bit on the first live run. A rehearsal writes an entry under the same key,
    /// so counting it would mean every dry run permanently disarmed the real action for that
    /// file, and the sweep would answer Skipped and consume the request having done nothing.
    /// </summary>
    [Fact]
    public void AlreadyApplied_DryRunEntry_DoesNotSuppressTheRealAction()
    {
        var journal = new LibraryActionJournal();
        journal.Record(Entry(LibraryAction.Delete, "song-a", LibraryActionState.Applied, dryRun: true));

        Assert.False(journal.AlreadyApplied(LibraryAction.Delete, "song-a", "1:2"));
    }

    [Fact]
    public void AlreadyApplied_FailedEntry_IsRetried()
    {
        var journal = new LibraryActionJournal();
        journal.Record(Entry(LibraryAction.Delete, "song-a", LibraryActionState.Failed));

        Assert.False(journal.AlreadyApplied(LibraryAction.Delete, "song-a", "1:2"));
    }

    /// <summary>A delete means the user does not want it back.</summary>
    [Fact]
    public void IsNeverRequested_AfterAnAppliedDelete_IsTrue()
    {
        var journal = new LibraryActionJournal();
        journal.Record(Entry(LibraryAction.Delete, "song-a", LibraryActionState.Applied,
            artist: "Drake", title: "Rich Flex"));

        Assert.True(journal.IsNeverRequested("Drake", "Rich Flex"));
        Assert.True(journal.IsNeverRequested(" drake ", "RICH FLEX"));
        Assert.False(journal.IsNeverRequested("Drake", "Something Else"));
    }

    /// <summary>A rehearsal must not silently stop a track being downloadable.</summary>
    [Fact]
    public void IsNeverRequested_DryRunDelete_DoesNotBlockAnything()
    {
        var journal = new LibraryActionJournal();
        journal.Record(Entry(LibraryAction.Delete, "song-a", LibraryActionState.Applied,
            artist: "Drake", title: "Rich Flex", dryRun: true));

        Assert.False(journal.IsNeverRequested("Drake", "Rich Flex"));
    }

    [Fact]
    public void IsNeverRequested_OtherActions_DoNotBlockARedownload()
    {
        var journal = new LibraryActionJournal();
        journal.Record(Entry(LibraryAction.WrongSong, "song-a", LibraryActionState.Applied,
            artist: "Drake", title: "Rich Flex"));

        Assert.False(journal.IsNeverRequested("Drake", "Rich Flex"));
    }

    /// <summary>
    /// The move landed and only the bookkeeping after it was lost, so the action is done. It
    /// must not run again.
    /// </summary>
    [Fact]
    public void Reconcile_QuarantinedButNotRecorded_BecomesApplied()
    {
        var quarantine = Path.Combine(Path.GetTempPath(), "octo-recon-" + Guid.NewGuid() + ".flac");
        File.WriteAllBytes(quarantine, new byte[8]);
        try
        {
            var journal = new LibraryActionJournal();
            journal.Record(Entry(LibraryAction.Delete, "song-a", LibraryActionState.Pending)
                with { SourcePath = "/music/gone.flac", QuarantinePath = quarantine });

            Assert.Equal(1, journal.Reconcile());
            Assert.True(journal.AlreadyApplied(LibraryAction.Delete, "song-a", "1:2"));
        }
        finally { try { File.Delete(quarantine); } catch { } }
    }

    /// <summary>
    /// The file is still where it was, so nothing happened. Marking it Failed rather than
    /// leaving it Pending stops it being treated as in flight forever.
    /// </summary>
    [Fact]
    public void Reconcile_SourceStillPresent_BecomesFailedWithoutRerunning()
    {
        var source = Path.Combine(Path.GetTempPath(), "octo-recon-" + Guid.NewGuid() + ".flac");
        File.WriteAllBytes(source, new byte[8]);
        try
        {
            var journal = new LibraryActionJournal();
            journal.Record(Entry(LibraryAction.Delete, "song-a", LibraryActionState.Pending)
                with { SourcePath = source, QuarantinePath = null });

            Assert.Equal(1, journal.Reconcile());
            Assert.False(journal.AlreadyApplied(LibraryAction.Delete, "song-a", "1:2"));
            Assert.Empty(journal.Pending());
        }
        finally { try { File.Delete(source); } catch { } }
    }

    [Fact]
    public void Entries_SurviveARestart()
    {
        var path = Path.Combine(Path.GetTempPath(), "octo-actions-" + Guid.NewGuid() + ".json");
        try
        {
            using (var first = new LibraryActionJournal(path))
                first.Record(Entry(LibraryAction.Delete, "song-a", LibraryActionState.Applied,
                    artist: "Drake", title: "Rich Flex"));

            using var second = new LibraryActionJournal(path);
            Assert.True(second.IsNeverRequested("Drake", "Rich Flex"));
        }
        finally { try { File.Delete(path); } catch { } }
    }

    [Fact]
    public void Recent_IsNewestFirst()
    {
        var journal = new LibraryActionJournal();
        journal.Record(Entry(LibraryAction.Delete, "song-a", LibraryActionState.Applied));
        journal.Record(Entry(LibraryAction.Delete, "song-b", LibraryActionState.Applied));

        Assert.Equal("song-b", journal.Recent().First().NavidromeId);
    }
}
