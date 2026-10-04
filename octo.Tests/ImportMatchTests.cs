using Microsoft.Extensions.Logging.Abstractions;
using Octo.Services.Imports;
using Octo.Services.Library;

namespace Octo.Tests;

/// <summary>
/// Have or missing: an imported song against the library, by the same rules a heart uses, and a
/// list kept as a Navidrome playlist, written only where it differs.
/// </summary>
public sealed class ImportMatchTests
{
    private static LibrarySongRow Row(string id, string artist, string title, int? seconds = 200, string? album = null,
        string suffix = "mp3", int bitRate = 320) =>
        new(id, $"/music/{id}.{suffix}", null, 1, suffix, bitRate, title, artist, seconds, album);

    private static ImportTrack Track(string artist, string title, int? seconds = 200, string? album = null) =>
        new() { Key = ImportKeys.ForWords(artist, title), Artist = artist, Title = title, Seconds = seconds, Album = album };

    [Fact]
    public void TheSameSongIsFound_HoweverItsCreditAndVersionWordsAreWritten()
    {
        var index = new LibraryIndex([
            Row("1", "Drake", "Too Good (feat. Rihanna)", 263),
            Row("2", "The Beatles", "Let It Be - Remastered 2009", 243),
            Row("3", "Beyoncé", "Halo", 261),
            Row("4", "Rihanna, Calvin Harris", "We Found Love", 215),
        ]);
        Assert.Equal("1", index.Find(Track("Drake, Rihanna", "Too Good", 263))?.Id);
        Assert.Equal("2", index.Find(Track("The Beatles", "Let It Be", 243))?.Id);
        Assert.Equal("3", index.Find(Track("Beyonce", "Halo", 262))?.Id);
        // The other credit order still names the same artists.
        Assert.Equal("4", index.Find(Track("Calvin Harris, Rihanna", "We Found Love", 215))?.Id);
    }

    [Fact]
    public void ALiveTakeOrAFarLengthIsAnotherSong()
    {
        var index = new LibraryIndex([Row("1", "Nirvana", "Lithium (Live)", 260), Row("2", "Nirvana", "Polly", 300)]);
        Assert.Null(index.Find(Track("Nirvana", "Lithium", 257)));
        Assert.Null(index.Find(Track("Nirvana", "Polly", 177)));
        Assert.Null(index.Find(Track("Someone Else", "Polly", 300)));
    }

    [Fact]
    public void ASongWithNoLengthIsMatchedOnItsWords_AndTheClosestCopyWins()
    {
        var index = new LibraryIndex([
            Row("mp3", "Massive Attack", "Teardrop", 330, "Mezzanine"),
            Row("flac", "Massive Attack", "Teardrop", 331, "Mezzanine", "flac", 900),
            Row("edit", "Massive Attack", "Teardrop", 320, "Singles"),
        ]);
        Assert.Equal("flac", index.Find(Track("Massive Attack", "Teardrop", null, "Mezzanine"))?.Id);
        Assert.Equal("edit", index.Find(Track("Massive Attack", "Teardrop", 320))?.Id);
    }

    [Fact]
    public async Task TheMatcherReadsTheWholeLibraryOnce_AndAgainAfterForget()
    {
        var reads = 0;
        var library = Enumerable.Range(0, 1500).Select(i => Row($"{i}", $"Artist {i}", $"Song {i}")).ToList();
        var matcher = new ImportMatcher(null!, NullLogger<ImportMatcher>.Instance)
        {
            ReadPage = (start, count, _) =>
            {
                reads++;
                var page = library.Skip(start).Take(count).ToList();
                return Task.FromResult<(IReadOnlyList<LibrarySongRow>, int)?>((page, page.Count));
            },
        };
        var tracks = new List<ImportTrack> { Track("Artist 1499", "Song 1499"), Track("Nobody", "Nothing") };
        Assert.True(await matcher.MatchAsync(tracks, CancellationToken.None));
        Assert.Equal("1499", tracks[0].LibraryId);
        Assert.Null(tracks[1].LibraryId);
        Assert.Equal(2, reads);

        await matcher.MatchAsync(tracks, CancellationToken.None);
        Assert.Equal(2, reads);
        matcher.Forget();
        await matcher.MatchAsync(tracks, CancellationToken.None);
        Assert.Equal(4, reads);
    }

    [Fact]
    public async Task ALibraryThatCannotBeReadKeepsWhatTheLastMatchFound()
    {
        var matcher = new ImportMatcher(null!, NullLogger<ImportMatcher>.Instance)
        {
            ReadPage = (_, _, _) => Task.FromResult<(IReadOnlyList<LibrarySongRow>, int)?>(null),
        };
        var tracks = new List<ImportTrack> { Track("A", "B") };
        tracks[0].LibraryId = "kept";
        Assert.False(await matcher.MatchAsync(tracks, CancellationToken.None));
        Assert.Equal("kept", tracks[0].LibraryId);
    }

    // ---- Playlists -----------------------------------------------------------------------------

    private sealed class Navidrome
    {
        public readonly Dictionary<string, List<string>> Playlists = new();
        public readonly List<string> Calls = [];
        public string Admin = "admin";
        public bool GiveWorks = true;
        public string? GivenTo;

        public ImportPlaylists Writer() => new(null!, null!, NullLogger<ImportPlaylists>.Instance)
        {
            AdminName = () => Admin,
            Create = (name, _, _) =>
            {
                var id = $"pl{Playlists.Count + 1}";
                Playlists[id] = [];
                Calls.Add($"create {name}");
                return Task.FromResult<string?>(id);
            },
            GiveTo = (id, user, _) => { Calls.Add($"give {id} {user}"); GivenTo = user; return Task.FromResult(GiveWorks); },
            Exists = (id, _) => Task.FromResult<bool?>(Playlists.ContainsKey(id)),
            ReadTracks = (id, _) => Task.FromResult<IReadOnlyList<(string, string)>?>(
                Playlists[id].Select((song, i) => ((i + 1).ToString(), song)).ToList()),
            Remove = (id, positions, _) =>
            {
                Calls.Add($"remove {string.Join(",", positions)}");
                foreach (var position in positions.Select(int.Parse).OrderDescending()) Playlists[id].RemoveAt(position - 1);
                return Task.FromResult(true);
            },
            Add = (id, songs, _) =>
            {
                Calls.Add($"add {string.Join(",", songs)}");
                Playlists[id].AddRange(songs);
                return Task.FromResult(true);
            },
        };
    }

    private static ImportList List(string owner, params string?[] libraryIds) => new()
    {
        Id = "spotify-p1", Owner = owner, Name = "Road trip", Source = ImportSources.SpotifyPlaylist,
        Tracks = libraryIds.Select((id, i) => new ImportTrack { Key = $"k{i}", Title = $"S{i}", Artist = "A", LibraryId = id }).ToList(),
    };

    [Fact]
    public async Task APlaylistIsMadeForItsOwner_WithTheSongsTheLibraryHas()
    {
        var navidrome = new Navidrome();
        var result = await navidrome.Writer().SyncAsync(List("alice", "a", null, "c"), CancellationToken.None);
        Assert.True(result.Ok);
        Assert.Equal("pl1", result.PlaylistId);
        Assert.Null(result.Note);
        Assert.Equal(["a", "c"], navidrome.Playlists["pl1"]);
        Assert.Equal(["create Road trip", "give pl1 alice", "add a,c"], navidrome.Calls);
    }

    [Fact]
    public async Task APlaylistNavidromeWouldNotHandOnSaysWhoseItIs()
    {
        var navidrome = new Navidrome { GiveWorks = false };
        var result = await navidrome.Writer().SyncAsync(List("alice", "a"), CancellationToken.None);
        Assert.Contains("under admin", result.Note);
        var own = new Navidrome();
        await own.Writer().SyncAsync(List("admin", "a"), CancellationToken.None);
        Assert.Null(own.GivenTo);
    }

    [Fact]
    public async Task OnlyWhatDiffersIsWritten_KeepingTheSongsBeforeIt()
    {
        var navidrome = new Navidrome();
        navidrome.Playlists["pl1"] = ["a", "c", "d"];
        var list = List("alice", "a", "b", "c", "d");
        list.PlaylistId = "pl1";
        var writer = navidrome.Writer();
        await writer.SyncAsync(list, CancellationToken.None);
        Assert.Equal(["a", "b", "c", "d"], navidrome.Playlists["pl1"]);
        Assert.Equal(["remove 2,3", "add b,c,d"], navidrome.Calls);

        navidrome.Calls.Clear();
        var again = await writer.SyncAsync(list, CancellationToken.None);
        Assert.True(again.Ok);
        Assert.Empty(navidrome.Calls);
    }

    [Fact]
    public async Task ALongTailIsRemovedFromTheEndInChunks()
    {
        var navidrome = new Navidrome();
        navidrome.Playlists["pl1"] = Enumerable.Range(0, 250).Select(i => $"old{i}").ToList();
        var list = List("alice", "new");
        list.PlaylistId = "pl1";
        await navidrome.Writer().SyncAsync(list, CancellationToken.None);
        Assert.Equal(["new"], navidrome.Playlists["pl1"]);
        var removals = navidrome.Calls.Where(call => call.StartsWith("remove")).ToList();
        Assert.Equal(3, removals.Count);
        Assert.StartsWith("remove 151,", removals[0]);
    }

    [Fact]
    public async Task APlaylistThePersonDeletedStaysDeleted()
    {
        var navidrome = new Navidrome();
        var list = List("alice", "a");
        list.PlaylistId = "gone";
        var result = await navidrome.Writer().SyncAsync(list, CancellationToken.None);
        Assert.True(result.Deleted);
        Assert.Empty(navidrome.Calls);
    }
}
