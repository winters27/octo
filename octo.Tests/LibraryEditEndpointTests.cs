using System.Net;
using System.Text;
using System.Text.Json;
using Microsoft.AspNetCore.Hosting;
using Microsoft.AspNetCore.Mvc.Testing;
using Microsoft.Extensions.Configuration;
using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.DependencyInjection.Extensions;
using Microsoft.Extensions.Hosting;
using Microsoft.Extensions.Logging.Abstractions;
using Microsoft.Extensions.Options;
using Octo.Models.Domain;
using Octo.Models.Settings;
using Octo.Services.CoverArt;
using Octo.Services.Library;
using SixLabors.ImageSharp;
using SixLabors.ImageSharp.PixelFormats;

namespace Octo.Tests;

/// <summary>
/// octoLibraryActions version 3, as the apps' Library health uses it: removing needs an admin and
/// can be put back from the trash, tags are written in place and can be undone, a split album is
/// joined by copying the album's own tags, and a cover goes only into a file that has none. Real
/// files in a temporary music folder, a real resolver and quarantine, Navidrome faked.
/// </summary>
public sealed class LibraryEditEndpointTests
{
    private sealed record FakeSong(string Id, string RelativePath, long Size, string Title, string Artist, string Album, string? AlbumArtist);

    /// <summary>Navidrome as these calls need it. Each song's size is the one it had when Navidrome
    /// last scanned, so after a write it reports the old size, as the real one does until it scans.</summary>
    private sealed class FakeNavidrome(string libraryPath) : HttpMessageHandler
    {
        public bool Admin = true;
        public readonly Dictionary<string, FakeSong> Songs = [];

        protected override Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken ct)
        {
            var uri = request.RequestUri!;
            var query = System.Web.HttpUtility.ParseQueryString(uri.Query);
            if (uri.AbsolutePath.EndsWith("/rest/ping", StringComparison.Ordinal))
                return Task.FromResult(Json(query["t"] == "good"
                    ? """{"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome"}}"""
                    : """{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":40,"message":"Wrong username or password"}}}"""));
            if (uri.AbsolutePath.EndsWith("/rest/getUser", StringComparison.Ordinal))
                return Task.FromResult(Json(JsonSerializer.Serialize(new Dictionary<string, object>
                {
                    ["subsonic-response"] = new { status = "ok", version = "1.16.1", user = new { username = query["username"], adminRole = Admin } },
                })));
            if (uri.AbsolutePath == "/auth/login")
                return Task.FromResult(Json("""{"token":"admin-jwt","isAdmin":true,"username":"admin","subsonicToken":"st","subsonicSalt":"ss"}"""));
            if (uri.AbsolutePath.StartsWith("/api/song/", StringComparison.Ordinal)
                && Songs.TryGetValue(uri.AbsolutePath["/api/song/".Length..], out var song))
                return Task.FromResult(Json(JsonSerializer.Serialize(new
                {
                    id = song.Id, path = song.RelativePath, libraryPath, size = song.Size, suffix = "flac",
                    title = song.Title, artist = song.Artist, album = song.Album, albumArtist = song.AlbumArtist, duration = 2,
                })));
            if (uri.AbsolutePath.EndsWith("/rest/startScan", StringComparison.Ordinal))
                return Task.FromResult(Json("""{"subsonic-response":{"status":"ok","version":"1.16.1"}}"""));
            return Task.FromResult(new HttpResponseMessage(HttpStatusCode.NotFound));
        }

        private static HttpResponseMessage Json(string body) =>
            new(HttpStatusCode.OK) { Content = new StringContent(body, Encoding.UTF8, "application/json") };
    }

    private sealed class FixedFinder(FoundCover? cover) : IAlbumCoverFinder
    {
        public int Asked;

        public Task<FoundCover?> FindAsync(AlbumCoverQuery query, CancellationToken ct)
        {
            Interlocked.Increment(ref Asked);
            return Task.FromResult(cover);
        }
    }

    private sealed class Factory : WebApplicationFactory<Program>
    {
        private readonly Dictionary<string, string?> _overrides;

        public Factory(Dictionary<string, string?>? overrides = null, FoundCover? cover = null)
        {
            _overrides = overrides ?? [];
            Navidrome = new FakeNavidrome(Root);
            Finder = new FixedFinder(cover);
            Directory.CreateDirectory(Root);
        }

        public string Root { get; } = Path.Combine(Path.GetTempPath(), "octo-edit-web-" + Guid.NewGuid());
        public FakeNavidrome Navidrome { get; }
        public FixedFinder Finder { get; }
        public LibraryActionJournal Journal { get; } = new();
        public TagEditJournal Edits { get; } = new();
        public LibraryRescan Rescan { get; } = new();
        public string Trash => Path.Combine(Root, ".octo-trash");

        /// <summary>A FLAC in the music folder with these tags, known to Navidrome by id.</summary>
        public string Song(string id, string relative, string title, string artist, string album, string? albumArtist = null,
            uint year = 0, string? genre = null)
        {
            var path = Path.Combine(Root, relative);
            Directory.CreateDirectory(Path.GetDirectoryName(path)!);
            File.WriteAllBytes(path, AudioFixtures.Flac());
            using (var file = TagLib.File.Create(path))
            {
                file.Tag.Title = title;
                file.Tag.Performers = [artist];
                file.Tag.Album = album;
                if (albumArtist is not null) file.Tag.AlbumArtists = [albumArtist];
                file.Tag.Year = year;
                if (genre is not null) file.Tag.Genres = [genre];
                file.Save();
            }
            Navidrome.Songs[id] = new FakeSong(id, relative.Replace('\\', '/'), new FileInfo(path).Length, title, artist, album, albumArtist);
            return path;
        }

        protected override void ConfigureWebHost(IWebHostBuilder builder)
        {
            var settings = new Dictionary<string, string?>
            {
                ["Subsonic:Url"] = "http://navidrome.test",
                ["Subsonic:AutoDetectDownloadPath"] = "false",
                ["Subsonic:AdminUsername"] = "admin",
                ["Subsonic:AdminPassword"] = "admin-password",
                ["Soulseek:BaseUrl"] = "http://127.0.0.1:1",
                ["YouTube:ShimUrl"] = "http://127.0.0.1:1",
                ["Library:DownloadPath"] = Root,
                ["LibraryActions:Enabled"] = "true",
                ["LibraryActions:DryRun"] = "false",
                ["LibraryActions:AllowedUsers:0"] = "alice",
                ["LibraryActions:Actions:0:Action"] = "Delete",
                ["LibraryActions:Actions:0:Enabled"] = "true",
                ["LibraryActions:QuarantineRetentionDays"] = "14",
            };
            foreach (var (key, value) in _overrides) settings[key] = value;

            builder.ConfigureAppConfiguration((_, configuration) => configuration.AddInMemoryCollection(settings));
            builder.ConfigureServices(services =>
            {
                services.RemoveAll<IHostedService>();
                services.RemoveAll<IHttpClientFactory>();
                services.AddSingleton<IHttpClientFactory>(new ReviewFixtures.OneClientFactory(Navidrome));
                // In memory, so a test never writes beside a real settings file.
                services.RemoveAll<LibraryActionJournal>();
                services.AddSingleton(Journal);
                services.RemoveAll<TagEditJournal>();
                services.AddSingleton(Edits);
                services.RemoveAll<LibraryRescan>();
                services.AddSingleton(Rescan);
                services.RemoveAll<IAlbumCoverFinder>();
                services.AddSingleton<IAlbumCoverFinder>(Finder);
            });
        }

        public override async ValueTask DisposeAsync()
        {
            await base.DisposeAsync();
            try { Directory.Delete(Root, recursive: true); } catch { /* best effort */ }
        }
    }

    private const string Auth = "u=alice&t=good&s=salt&v=1.16.1&c=octo";

    private static async Task<JsonElement> Call(HttpClient client, string query)
    {
        var body = await client.GetStringAsync($"/rest/libraryAction.view?{Auth}&{query}");
        using var doc = JsonDocument.Parse(body);
        var envelope = doc.RootElement.GetProperty("subsonic-response");
        Assert.Equal("ok", envelope.GetProperty("status").GetString());
        return envelope.GetProperty("libraryAction").Clone();
    }

    private static string State(JsonElement action) => action.GetProperty("state").GetString()!;

    private static Dictionary<string, string?> Tags(string path) => LibraryTagEdits.Read(path);

    // Removing, the trash and putting back

    [Fact]
    public async Task Remove_SomeoneWhoIsNotAnAdmin_IsTurnedAway_AndTheFileStays()
    {
        await using var factory = new Factory();
        var path = factory.Song("s1", "Massive Attack/Mezzanine/Teardrop.flac", "Teardrop", "Massive Attack", "Mezzanine");
        factory.Navidrome.Admin = false;
        using var client = factory.CreateClient();

        var action = await Call(client, "id=s1&action=remove");

        Assert.Equal("skipped", State(action));
        Assert.Contains("admin", action.GetProperty("detail").GetString());
        Assert.True(File.Exists(path));
        Assert.False(Directory.Exists(factory.Trash));
        Assert.Empty(factory.Journal.Recent());

        using var doc = JsonDocument.Parse(await client.GetStringAsync($"/rest/getLibraryActions.view?{Auth}"));
        Assert.False(doc.RootElement.GetProperty("subsonic-response").GetProperty("libraryActions").GetProperty("admin").GetBoolean());
    }

    [Fact]
    public async Task Remove_GoesToTheTrash_AsksForAScan_IsListed_AndCanBePutBackThenRemovedAgain()
    {
        await using var factory = new Factory();
        var path = factory.Song("s1", "Massive Attack/Mezzanine/Teardrop.flac", "Teardrop", "Massive Attack", "Mezzanine");
        var bytes = await File.ReadAllBytesAsync(path);
        using var client = factory.CreateClient();

        Assert.Equal("applied", State(await Call(client, "id=s1&action=remove")));
        Assert.False(File.Exists(path));
        Assert.Equal(1, factory.Rescan.Asked);

        using (var doc = JsonDocument.Parse(await client.GetStringAsync($"/rest/getLibraryTrash.view?{Auth}")))
        {
            var trash = doc.RootElement.GetProperty("subsonic-response").GetProperty("libraryTrash");
            Assert.Equal(14, trash.GetProperty("keepDays").GetInt32());
            var song = Assert.Single(trash.GetProperty("songs").EnumerateArray());
            Assert.Equal("s1", song.GetProperty("id").GetString());
            Assert.Equal("Teardrop", song.GetProperty("title").GetString());
            Assert.Equal("alice", song.GetProperty("removedBy").GetString());
            Assert.NotNull(song.GetProperty("goneAt").GetString());
        }

        var restored = await Call(client, "id=s1&action=restore");
        Assert.Equal("applied", State(restored));
        Assert.Equal(bytes, await File.ReadAllBytesAsync(path));
        Assert.Equal(LibraryActionState.Restored, factory.Journal.Recent().Single().State);
        Assert.Equal(2, factory.Rescan.Asked);
        // Put back, so no longer a song the person said they never want again.
        Assert.False(factory.Journal.IsNeverRequested("Massive Attack", "Teardrop"));

        using (var doc = JsonDocument.Parse(await client.GetStringAsync($"/rest/getLibraryTrash.view?{Auth}")))
            Assert.Empty(doc.RootElement.GetProperty("subsonic-response").GetProperty("libraryTrash").GetProperty("songs").EnumerateArray());

        // The same file, the same size and time: a second remove is a real one, not "Already done".
        Assert.Equal("applied", State(await Call(client, "id=s1&action=remove")));
        Assert.False(File.Exists(path));
    }

    [Fact]
    public async Task Remove_TakesTheSongsLyricsWithIt_AndPutBackReturnsThem_LeavingTheAlbumsCover()
    {
        await using var factory = new Factory();
        var path = factory.Song("s1", "Daft Punk/Discovery/03 - Digital Love.flac", "Digital Love", "Daft Punk", "Discovery");
        var folder = Path.GetDirectoryName(path)!;
        var lyrics = Path.Combine(folder, "03 - Digital Love.lrc");
        var plain = Path.Combine(folder, "03 - Digital Love.txt");
        var cover = Path.Combine(folder, "cover.jpg");
        var other = Path.Combine(folder, "04 - Harder Better Faster Stronger.lrc");
        foreach (var file in new[] { lyrics, plain, cover, other }) await File.WriteAllTextAsync(file, Path.GetFileName(file));
        using var client = factory.CreateClient();

        Assert.Equal("applied", State(await Call(client, "id=s1&action=remove")));

        Assert.False(File.Exists(lyrics));
        Assert.False(File.Exists(plain));
        Assert.True(File.Exists(cover));
        Assert.True(File.Exists(other));
        var trashed = factory.Journal.Recent().Single().QuarantinePath!;
        Assert.True(File.Exists(Path.ChangeExtension(trashed, ".lrc")));
        Assert.Contains("03 - Digital Love.lrc", await File.ReadAllTextAsync(trashed + ".octo-action.json"));

        Assert.Equal("applied", State(await Call(client, "id=s1&action=restore")));

        Assert.Equal("03 - Digital Love.lrc", await File.ReadAllTextAsync(lyrics));
        Assert.Equal("03 - Digital Love.txt", await File.ReadAllTextAsync(plain));
        Assert.False(File.Exists(Path.ChangeExtension(trashed, ".lrc")));
    }

    [Fact]
    public async Task PutBack_LinksTheOutsideSongToTheFileAgain()
    {
        await using var factory = new Factory();
        var path = factory.Song("s1", "Daft Punk/Discovery/03 - Digital Love.flac", "Digital Love", "Daft Punk", "Discovery");
        using var client = factory.CreateClient();
        var library = factory.Services.GetRequiredService<Octo.Services.Local.ILocalLibraryService>();
        await library.RegisterDownloadedSongAsync(new Octo.Models.Domain.Song
        {
            ExternalProvider = "soulseek", ExternalId = "4H0vAhIr2YJ67Riz3Uv6wG", Title = "Digital Love", Artist = "Daft Punk",
            Album = "Discovery", SourcePeer = "peer1",
        }, path);

        Assert.Equal("applied", State(await Call(client, "id=s1&action=remove")));
        Assert.Null(await library.GetLocalPathForExternalSongAsync("soulseek", "4H0vAhIr2YJ67Riz3Uv6wG"));
        Assert.Empty(await library.GetMappingsAsync());
        // The link waits in the trash note beside the file, which is all Put back reads.
        var note = JsonSerializer.Deserialize<QuarantineManifest>(
            await File.ReadAllTextAsync(factory.Journal.Recent().Single().QuarantinePath + ".octo-action.json"))!;
        Assert.Equal("4H0vAhIr2YJ67Riz3Uv6wG", Assert.Single(note.Mappings!).ExternalId);

        Assert.Equal("applied", State(await Call(client, "id=s1&action=restore")));

        Assert.Equal(Path.GetFullPath(path), await library.GetLocalPathForExternalSongAsync("soulseek", "4H0vAhIr2YJ67Riz3Uv6wG"));
        Assert.Equal("peer1", (await library.GetMappingsAsync()).Single().SourcePeer);
    }

    [Fact]
    public async Task RemovingOneFormatOfASong_LeavesTheLyricsTheOtherCopyShares()
    {
        await using var factory = new Factory();
        var flac = factory.Song("s1", "Daft Punk/Discovery/03 - Digital Love.flac", "Digital Love", "Daft Punk", "Discovery");
        var folder = Path.GetDirectoryName(flac)!;
        await File.WriteAllTextAsync(Path.Combine(folder, "03 - Digital Love.mp3"), "the other copy");
        var lyrics = Path.Combine(folder, "03 - Digital Love.lrc");
        await File.WriteAllTextAsync(lyrics, "shared");
        using var client = factory.CreateClient();

        Assert.Equal("applied", State(await Call(client, "id=s1&action=remove&copy=true")));

        Assert.True(File.Exists(lyrics));
    }

    [Fact]
    public async Task RemovingASecondCopy_LeavesTheSongWanted_ButDeletingTheSongDoesNot()
    {
        await using var factory = new Factory();
        var copy = factory.Song("copy", "Bon Iver/Holocene/Holocene.flac", "Holocene", "Bon Iver", "Holocene");
        factory.Song("song", "Bon Iver/Bon Iver/Towers.flac", "Towers", "Bon Iver", "Bon Iver");
        using var client = factory.CreateClient();

        var removed = await Call(client, "id=copy&action=remove&copy=true");
        Assert.Equal("applied", State(removed));
        Assert.Contains("stays in the library", removed.GetProperty("detail").GetString());
        Assert.False(File.Exists(copy));
        // The kept copy can still be upgraded, or the song hearted, later.
        Assert.False(factory.Journal.IsNeverRequested("Bon Iver", "Holocene"));

        Assert.Equal("applied", State(await Call(client, "id=song&action=remove")));
        Assert.True(factory.Journal.IsNeverRequested("Bon Iver", "Towers"));
    }

    [Fact]
    public async Task Restore_ASongNotInTheTrash_ChangesNothing()
    {
        await using var factory = new Factory();
        factory.Song("s1", "A/B/Song.flac", "Song", "A", "B");
        using var client = factory.CreateClient();

        Assert.Equal("skipped", State(await Call(client, "id=s1&action=restore")));
    }

    [Fact]
    public async Task Restore_NeverPutsAFileOutsideTheMusicFolder_WhateverItsManifestSays()
    {
        await using var factory = new Factory();
        factory.Song("s1", "A/B/Song.flac", "Song", "A", "B");
        using var client = factory.CreateClient();
        Assert.Equal("applied", State(await Call(client, "id=s1&action=remove")));

        // Someone edits the note beside the trashed file to point somewhere else.
        var trashed = factory.Journal.Recent().Single().QuarantinePath!;
        var elsewhere = Path.Combine(Path.GetTempPath(), "octo-edit-elsewhere-" + Guid.NewGuid(), "Song.flac");
        await File.WriteAllTextAsync(trashed + ".octo-action.json", JsonSerializer.Serialize(
            new QuarantineManifest(elsewhere, "s1", "Delete", "alice", DateTime.UtcNow)));

        var action = await Call(client, "id=s1&action=restore");

        Assert.Equal("failed", State(action));
        Assert.False(File.Exists(elsewhere));
        Assert.True(File.Exists(trashed));
    }

    // Tags

    [Fact]
    public async Task Retag_WritesOnlyWhatWasSent_AndAnswersBeforeAndAfter()
    {
        await using var factory = new Factory();
        var path = factory.Song("s1", "Massive Attack/Mezzanine/Teardrop.flac", "Teardrop", "Massive Attack", "Mezzanine", year: 1998);
        using var client = factory.CreateClient();

        var action = await Call(client, "id=s1&action=retag&genre=Trip%20Hop&track=3");

        Assert.Equal("applied", State(action));
        Assert.Equal("Changed the genre and track number.", action.GetProperty("detail").GetString());
        Assert.Equal(JsonValueKind.Null, action.GetProperty("before").GetProperty("genre").ValueKind);
        Assert.Equal("Trip Hop", action.GetProperty("after").GetProperty("genre").GetString());
        var tags = Tags(path);
        Assert.Equal("Trip Hop", tags["genre"]);
        Assert.Equal("3", tags["track"]);
        Assert.Equal("1998", tags["year"]);
        Assert.Equal("Teardrop", tags["title"]);
        Assert.Equal(1, factory.Rescan.Asked);
        Assert.Single(factory.Edits.Recent());
    }

    [Fact]
    public async Task Retag_ASecondEditBeforeNavidromeScans_StillFindsTheFile_AndUndoPutsBackOnlyTheLast()
    {
        await using var factory = new Factory();
        var path = factory.Song("s1", "A/B/Song.flac", "Song", "A", "B");
        using var client = factory.CreateClient();

        Assert.Equal("applied", State(await Call(client, "id=s1&action=retag&year=2001")));
        // A tag that outgrew the file's padding: Navidrome reports the size from before the write
        // until it scans, so the resolver no longer recognises the file.
        factory.Navidrome.Songs["s1"] = factory.Navidrome.Songs["s1"] with { Size = new FileInfo(path).Length + 4096 };
        Assert.Equal("applied", State(await Call(client, "id=s1&action=retag&genre=Rock")));

        var undo = await Call(client, "id=s1&action=undo");

        Assert.Equal("applied", State(undo));
        var tags = Tags(path);
        Assert.Null(tags["genre"]);
        Assert.Equal("2001", tags["year"]);
    }

    [Fact]
    public async Task Undo_ALeftAloneFileChangedSince_IsLeftAlone()
    {
        await using var factory = new Factory();
        var path = factory.Song("s1", "A/B/Song.flac", "Song", "A", "B");
        using var client = factory.CreateClient();
        Assert.Equal("applied", State(await Call(client, "id=s1&action=retag&year=2001")));

        using (var file = TagLib.File.Create(path))
        {
            file.Tag.Comment = "someone else was here";
            file.Save();
        }
        File.SetLastWriteTimeUtc(path, DateTime.UtcNow.AddMinutes(5));

        Assert.Equal("failed", State(await Call(client, "id=s1&action=undo")));
        Assert.Equal("2001", Tags(path)["year"]);
    }

    [Fact]
    public async Task Retag_AStraySpaceIsAChange_SoASplitAlbumCanBeMended()
    {
        await using var factory = new Factory();
        var path = factory.Song("s1", "A/B/Song.flac", "Song", "A", "Mezzanine ");
        using var client = factory.CreateClient();

        Assert.Equal("applied", State(await Call(client, "id=s1&action=retag&album=Mezzanine")));
        Assert.Equal("Mezzanine", Tags(path)["album"]);
    }

    [Theory]
    [InlineData("year=abc")]
    [InlineData("title=")]
    [InlineData("track=0")]
    [InlineData("isrc=nope")]
    public async Task Retag_AValueATagCannotHold_WritesNothing(string query)
    {
        await using var factory = new Factory();
        var path = factory.Song("s1", "A/B/Song.flac", "Song", "A", "B", year: 1999);
        var before = await File.ReadAllBytesAsync(path);
        using var client = factory.CreateClient();

        Assert.Equal("failed", State(await Call(client, $"id=s1&action=retag&{query}")));
        Assert.Equal(before, await File.ReadAllBytesAsync(path));
        Assert.Empty(factory.Edits.Recent());
    }

    [Fact]
    public async Task Retag_InADryRun_IsRehearsedAndWritesNothing()
    {
        await using var factory = new Factory(new() { ["LibraryActions:DryRun"] = "true" });
        var path = factory.Song("s1", "A/B/Song.flac", "Song", "A", "B");
        var before = await File.ReadAllBytesAsync(path);
        using var client = factory.CreateClient();

        Assert.Equal("rehearsed", State(await Call(client, "id=s1&action=retag&year=2001")));
        Assert.Equal(before, await File.ReadAllBytesAsync(path));
    }

    [Fact]
    public async Task Retag_SomeoneNotOnTheAllowedList_IsTurnedAway()
    {
        await using var factory = new Factory();
        var path = factory.Song("s1", "A/B/Song.flac", "Song", "A", "B");
        var before = await File.ReadAllBytesAsync(path);
        using var client = factory.CreateClient();

        var body = await client.GetStringAsync("/rest/libraryAction.view?u=bob&t=good&s=salt&v=1.16.1&c=octo&id=s1&action=retag&year=2001");

        Assert.Contains("\"skipped\"", body);
        Assert.Equal(before, await File.ReadAllBytesAsync(path));
    }

    // Albums

    [Fact]
    public async Task JoinAlbum_GivesTheSongTheLeadsAlbumTagsAndYear_KeepsItsOwnTitle_AndUndoes()
    {
        await using var factory = new Factory();
        var lead = factory.Song("lead", "Massive Attack/Mezzanine/01 Angel.flac", "Angel", "Massive Attack", "Mezzanine", "Massive Attack", 1998);
        var stray = factory.Song("stray", "Massive Attack/Mezzanine (2)/03 Teardrop.flac", "Teardrop", "Massive Attack", "Mezzanine ", null);
        using var client = factory.CreateClient();

        var action = await Call(client, "id=stray&action=joinAlbum&like=lead");

        Assert.Equal("applied", State(action));
        var joined = KeptIdentityTags.Read(stray)!;
        var leading = KeptIdentityTags.Read(lead)!;
        Assert.Equal(leading.Album, joined.Album);
        Assert.Equal(leading.AlbumArtist, joined.AlbumArtist);
        Assert.Equal(leading.ReleaseDate, joined.ReleaseDate);
        Assert.Equal("Teardrop", joined.Title);
        Assert.Equal("1998", Tags(stray)["year"]);

        Assert.Equal("applied", State(await Call(client, "id=stray&action=undo")));
        Assert.Equal("Mezzanine ", KeptIdentityTags.Read(stray)!.Album);
        Assert.Null(Tags(stray)["year"]);
    }

    [Fact]
    public async Task JoinAlbum_AcrossAlbumArtists_GivesTheSongTheLeadsAlbumArtist_KeepsItsOwnArtist_AndUndoes()
    {
        // One album filed under each of its artists, as the live library had it: Drake's songs
        // on a part of their own, with no year, beside PARTYNEXTDOOR's.
        await using var factory = new Factory();
        var lead = factory.Song("lead", "PARTYNEXTDOOR/$ome $exy $ongs 4 U/CELIBACY.flac", "CELIBACY", "PARTYNEXTDOOR",
            "$ome $exy $ongs 4 U", "PARTYNEXTDOOR", 2025);
        var stray = factory.Song("stray", "Drake/$ome $exy $ongs 4 U/GIMME A HUG.flac", "GIMME A HUG", "Drake",
            "$ome $exy $ongs 4 U", "Drake");
        using var client = factory.CreateClient();

        Assert.Equal("applied", State(await Call(client, "id=stray&action=joinAlbum&like=lead")));

        var joined = KeptIdentityTags.Read(stray)!;
        var leading = KeptIdentityTags.Read(lead)!;
        Assert.Equal(leading.AlbumArtist, joined.AlbumArtist);
        Assert.Equal(leading.AlbumArtists, joined.AlbumArtists);
        Assert.Equal(KeptIdentityTags.PidInputs(leading with { Title = "" }), KeptIdentityTags.PidInputs(joined with { Title = "" }));
        Assert.Equal("Drake", Tags(stray)["artist"]);
        Assert.Equal("2025", Tags(stray)["year"]);

        Assert.Equal("applied", State(await Call(client, "id=stray&action=undo")));
        Assert.Equal(["Drake"], KeptIdentityTags.Read(stray)!.AlbumArtist);
        Assert.Null(Tags(stray)["year"]);
    }

    [Fact]
    public async Task JoinAlbum_TakesAwayAPeersStrayAlbumId_ReleaseDate_AndVersion_WhenTheAlbumHasNone()
    {
        await using var factory = new Factory();
        factory.Song("lead", "Uzi/Eternal Atake/01 Baby Pluto.flac", "Baby Pluto", "Lil Uzi Vert", "Eternal Atake", "Lil Uzi Vert", 2020);
        var stray = factory.Song("stray", "Uzi/Eternal Atake/02 Lo Mein.flac", "Lo Mein", "Lil Uzi Vert", "Eternal Atake", "Lil Uzi Vert", 2020);
        // What a Soulseek peer's tagger left on one song, and its album-mates lack: Navidrome
        // groups albums by these, so this song shows as an album of its own.
        using (var file = TagLib.File.Create(stray))
        {
            Octo.Services.Common.TagWriterExtras.SetText(file, Octo.Services.Common.TagFields.AlbumId, "4c6d8f8a-77a5-4a3b-9b49-6ac41a2b3e2c");
            Octo.Services.Common.TagWriterExtras.SetText(file, Octo.Services.Common.TagFields.ReleaseDate, "2020-03-06");
            Octo.Services.Common.TagWriterExtras.SetExact(file, Octo.Services.Common.TagFields.AlbumVersion, ["Deluxe"]);
            file.Save();
        }
        factory.Navidrome.Songs["stray"] = factory.Navidrome.Songs["stray"] with { Size = new FileInfo(stray).Length };
        using var client = factory.CreateClient();

        Assert.Equal("applied", State(await Call(client, "id=stray&action=joinAlbum&like=lead")));

        var joined = KeptIdentityTags.Read(stray)!;
        var lead = KeptIdentityTags.Read(Path.Combine(factory.Root, "Uzi/Eternal Atake/01 Baby Pluto.flac"))!;
        Assert.Null(joined.AlbumId);
        Assert.Null(joined.AlbumVersion);
        Assert.Equal(lead.ReleaseDate, joined.ReleaseDate);
        Assert.Equal(KeptIdentityTags.PidInputs(lead with { Title = "" }), KeptIdentityTags.PidInputs(joined with { Title = "" }));
    }

    // Covers

    private static byte[] Jpeg(int side)
    {
        using var image = new Image<Rgba32>(side, side, new Rgba32(40, 80, 120));
        using var stream = new MemoryStream();
        image.SaveAsJpeg(stream);
        return stream.ToArray();
    }

    [Fact]
    public async Task Cover_GoesOnlyIntoAFileWithNoPicture_OneLookupPerAlbum_AndUndoTakesItOut()
    {
        await using var factory = new Factory(cover: new FoundCover(Jpeg(600), "Apple Music", 600));
        var one = factory.Song("s1", "A/B/One.flac", "One", "A", "B", "A");
        var two = factory.Song("s2", "A/B/Two.flac", "Two", "A", "B", "A");
        using var client = factory.CreateClient();

        var preview = await Call(client, "id=s1&action=cover&preview=true");
        Assert.Equal("found", State(preview));
        Assert.False(LibraryTagEdits.HasPicture(one));

        Assert.Equal("applied", State(await Call(client, "id=s1&action=cover")));
        Assert.Equal("applied", State(await Call(client, "id=s2&action=cover")));
        Assert.True(LibraryTagEdits.HasPicture(one));
        Assert.True(LibraryTagEdits.HasPicture(two));
        Assert.Equal(1, factory.Finder.Asked);

        // A picture of its own already: left alone.
        Assert.Equal("skipped", State(await Call(client, "id=s1&action=cover")));

        Assert.Equal("applied", State(await Call(client, "id=s1&action=undo")));
        Assert.False(LibraryTagEdits.HasPicture(one));
        Assert.True(LibraryTagEdits.HasPicture(two));
    }

    [Fact]
    public async Task Cover_NothingFound_SaysSo_AndWritesNothing()
    {
        await using var factory = new Factory(cover: null);
        var one = factory.Song("s1", "A/B/One.flac", "One", "A", "B");
        using var client = factory.CreateClient();

        Assert.Equal("failed", State(await Call(client, "id=s1&action=cover")));
        Assert.False(LibraryTagEdits.HasPicture(one));
    }

    // Lookup

    [Fact]
    public async Task Lookup_AnUnknownSong_IsUnresolved_WithTheLookupShape()
    {
        await using var factory = new Factory();
        using var client = factory.CreateClient();

        var action = await Call(client, "id=nope&action=lookup");

        Assert.Equal("unresolved", State(action));
        Assert.Equal(JsonValueKind.Object, action.GetProperty("current").ValueKind);
        Assert.Equal(JsonValueKind.Object, action.GetProperty("suggested").ValueKind);
    }

    [Fact]
    public void Suggested_KeepsOnlyWhatTheLookupFound_InTheAppsNames()
    {
        var found = LibraryEditService.Suggested(new Song
        {
            Title = "Teardrop", Artist = "Massive Attack", Album = "Mezzanine", AlbumArtist = "Massive Attack",
            Year = 1998, Genre = "Trip Hop", Track = 3, DiscNumber = 1, Isrc = "gb-aaa-98-00003",
        });
        Assert.Equal("1998", found["year"]);
        Assert.Equal("3", found["track"]);
        Assert.Equal("GBAAA9800003", found["isrc"]);
        Assert.Equal("Trip Hop", found["genre"]);

        var thin = LibraryEditService.Suggested(new Song { Title = "Teardrop", Artist = "Massive Attack" });
        Assert.Equal(["artist", "title"], thin.Keys.OrderBy(k => k, StringComparer.Ordinal));
    }
}

/// <summary>The last check before a file leaves the library: inside the music folder, not in the
/// trash already, and a real file.</summary>
public sealed class QuarantineContainmentTests : IDisposable
{
    private readonly string _root = Path.Combine(Path.GetTempPath(), "octo-contain-" + Guid.NewGuid());
    private readonly LibraryActionQuarantine _quarantine = new(
        new TestOptionsMonitor<LibraryActionSettings>(new LibraryActionSettings()), NullLogger<LibraryActionQuarantine>.Instance);

    public QuarantineContainmentTests() => Directory.CreateDirectory(Path.Combine(_root, "music"));

    public void Dispose()
    {
        try { Directory.Delete(_root, true); } catch { /* best effort */ }
    }

    private string Music => Path.Combine(_root, "music");

    private ResolvedSongFile File(string path)
    {
        Directory.CreateDirectory(Path.GetDirectoryName(path)!);
        System.IO.File.WriteAllBytes(path, [1, 2, 3]);
        return new ResolvedSongFile("id", path, 3, "T", "A", "B", "flac", null, PathSource.NativeApi);
    }

    [Fact]
    public void AFolderThatOnlyStartsWithTheMusicFoldersName_IsOutside()
    {
        var file = File(Path.Combine(_root, "music-old", "Song.flac"));
        var moved = _quarantine.Move(file, Music, LibraryAction.Delete, "alice");
        Assert.False(moved.Moved);
        Assert.True(System.IO.File.Exists(file.AbsolutePath));
    }

    [Fact]
    public void APathThatClimbsOut_IsOutside()
    {
        var real = File(Path.Combine(_root, "elsewhere", "Song.flac"));
        var climbing = real with { AbsolutePath = Path.Combine(Music, "..", "elsewhere", "Song.flac") };
        Assert.False(_quarantine.Move(climbing, Music, LibraryAction.Delete, "alice").Moved);
        Assert.True(System.IO.File.Exists(real.AbsolutePath));
    }

    [Fact]
    public void AFileInTheTrashAlready_IsNotMovedAgain()
    {
        var file = File(Path.Combine(Music, ".octo-trash", "2026-10-01", "Song.flac"));
        Assert.False(_quarantine.Move(file, Music, LibraryAction.Delete, "alice").Moved);
        Assert.True(System.IO.File.Exists(file.AbsolutePath));
    }

    [Fact]
    public void ASongInsideTheMusicFolder_GoesToTheTrash()
    {
        var file = File(Path.Combine(Music, "A", "B", "Song.flac"));
        var moved = _quarantine.Move(file, Music, LibraryAction.Delete, "alice");
        Assert.True(moved.Moved);
        Assert.False(System.IO.File.Exists(file.AbsolutePath));
        Assert.StartsWith(Path.Combine(Music, ".octo-trash"), moved.QuarantinePath);
    }
}
