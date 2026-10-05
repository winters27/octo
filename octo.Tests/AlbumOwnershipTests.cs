using Microsoft.Extensions.Logging.Abstractions;
using Octo.Models.Domain;
using Octo.Services.Library;

namespace Octo.Tests;

/// <summary>
/// The library and the catalog of Brandon's "drake" search (2026-10-04), as Navidrome and Deezer
/// gave them: what search listed as not in the library, and what the library really held.
/// </summary>
internal static class DrakeSearch
{
    public const string FomoId = "al-fomo";
    public const string SexyDrakePartId = "3jmLzEVlwtrlq8XfVjei9k";
    public const string SexyPndPartId = "al-sss-pnd";
    public const string WattbaId = "al-wattba";
    public const string TakeCareId = "al-takecare";

    private static int _next;

    private static LibrarySongRow Row(string albumId, string album, string albumArtist, string artist, string title, int seconds,
        params string[] isrcs) =>
        new($"s-{albumId}-{++_next}", $"{albumArtist}/{album}/{title}.flac", null, 1, "flac", 900, title, artist, seconds,
            album, albumId, albumArtist, isrcs);

    /// <summary>The library: HABIBTI's expanded edition, the split "$ome $exy $ongs 4 U" (five
    /// songs credited to Drake apart from the rest, and one Drake song the library credits to
    /// PARTYNEXTDOOR), two songs of What A Time To Be Alive's explicit version, and Take Care.
    /// No ISRCs, as few library files carry one.</summary>
    public static readonly IReadOnlyList<LibrarySongRow> Library = BuildLibrary();

    private static List<LibrarySongRow> BuildLibrary()
    {
        var rows = new List<LibrarySongRow>();
        void Fomo(string title, int seconds) => rows.Add(Row(FomoId, "HABIBTI (FOMO)", "Drake", "Drake", title, seconds));
        Fomo("Rusty Intro", 62); Fomo("WNBA", 179); Fomo("Slap The City", 202); Fomo("High Fives", 256);
        Fomo("Hurrr Nor Thurrr", 188); Fomo("I'm Spent", 144); Fomo("Classic", 179); Fomo("Gen 5", 217);
        Fomo("White Bone", 297); Fomo("Fortworth", 231); Fomo("Prioritizing", 233); Fomo("Solar Eclipse", 218);
        Fomo("Quebec", 130); Fomo("Cold Shoulder", 234); Fomo("Classic PT2", 179);

        void Drake(string title, int seconds) => rows.Add(Row(SexyDrakePartId, "$ome $exy $ongs 4 U", "Drake", "Drake", title, seconds));
        Drake("CRYING IN CHANEL", 200); Drake("SMALL TOWN FAME", 149); Drake("BRIAN STEEL", 111);
        Drake("RAINING IN HOUSTON", 245); Drake("NOKIA", 241);
        void Pnd(string title, int seconds) => rows.Add(Row(SexyPndPartId, "$ome $exy $ongs 4 U", "PARTYNEXTDOOR", "PARTYNEXTDOOR", title, seconds));
        Pnd("CN TOWER", 242); Pnd("MOTH BALLS", 213); Pnd("SOMETHING ABOUT YOU", 219); Pnd("SPIDER-MAN SUPERMAN", 204);
        Pnd("DEEPER", 172); Pnd("GIMME A HUG", 193); Pnd("LASERS", 199); Pnd("MEET YOUR PADRE", 271);
        Pnd("DIE TRYING", 195); Pnd("SOMEBODY LOVES ME", 183); Pnd("CELIBACY", 236); Pnd("OMW", 233);
        Pnd("GLORIOUS", 205); Pnd("WHEN HE'S GONE", 210); Pnd("GREEDY", 386);

        rows.Add(Row(WattbaId, "What A Time To Be Alive (Explicit Version)", "Drake & Future", "Drake & Future", "Jumpman", 206));
        rows.Add(Row(WattbaId, "What A Time To Be Alive (Explicit Version)", "Drake & Future", "Drake & Future", "Live From The Gutter", 211));

        rows.Add(Row(TakeCareId, "Take Care", "Drake", "Drake", "Over My Dead Body", 272));
        rows.Add(Row(TakeCareId, "Take Care", "Drake", "Drake", "Shot For Me", 224));
        return rows;
    }

    public static AlbumOwnership.CatalogTrack T(string title, string artist, int seconds, string isrc) => new(title, artist, seconds, isrc);

    /// <summary>Deezer's tracklists, ISRCs and all.</summary>
    public static readonly IReadOnlyList<AlbumOwnership.CatalogTrack> Habibti =
    [
        T("Rusty Intro", "Drake", 62, "USUG12602381"), T("WNBA", "Drake", 178, "USUG12602382"),
        T("Slap The City", "Drake", 202, "USUG12602383"), T("High Fives", "Drake", 256, "USUG12602384"),
        T("Hurrr Nor Thurrr", "Drake", 188, "USUG12602385"), T("I’m Spent", "Drake", 144, "USUG12602440"),
        T("Classic", "Drake", 179, "USUG12602441"), T("Gen 5", "Drake", 217, "USUG12602442"),
        T("White Bone", "Drake", 297, "USUG12602443"), T("Fortworth", "Drake", 231, "USUG12602444"),
        T("Prioritizing", "Drake", 233, "USUG12602445"),
    ];

    public static readonly IReadOnlyList<AlbumOwnership.CatalogTrack> HabibtiFomo =
    [
        .. Habibti,
        T("Solar Eclipse", "Drake", 218, "USUG12604795"), T("Quebec", "Drake", 130, "USUG12604801"),
        T("Cold Shoulder", "Drake", 234, "USUG12604803"), T("Classic PT2", "Drake", 179, "USUG12604805"),
    ];

    public static readonly IReadOnlyList<AlbumOwnership.CatalogTrack> SomeSexySongs =
    [
        T("CN TOWER", "PARTYNEXTDOOR", 242, "USLD91772019"), T("MOTH BALLS", "PARTYNEXTDOOR", 213, "USLD91772020"),
        T("SOMETHING ABOUT YOU", "PARTYNEXTDOOR", 219, "USLD91772021"), T("CRYING IN CHANEL", "Drake", 200, "USLD91772022"),
        T("SPIDER-MAN SUPERMAN", "PARTYNEXTDOOR", 204, "USLD91772023"), T("DEEPER", "PARTYNEXTDOOR", 172, "USLD91772024"),
        T("SMALL TOWN FAME", "Drake", 149, "USLD91772025"), T("PIMMIE'S DILEMMA", "Pimmie", 118, "USLD91772026"),
        T("BRIAN STEEL", "Drake", 111, "USLD91772027"), T("GIMME A HUG", "Drake", 193, "USLD91772028"),
        T("RAINING IN HOUSTON", "Drake", 245, "USLD91772029"), T("LASERS", "PARTYNEXTDOOR", 199, "USLD91772030"),
        T("MEET YOUR PADRE", "PARTYNEXTDOOR", 271, "USLD91772031"), T("NOKIA", "Drake", 241, "USLD91772032"),
        T("DIE TRYING", "PARTYNEXTDOOR", 195, "USLD91772033"), T("SOMEBODY LOVES ME", "PARTYNEXTDOOR", 183, "USLD91772034"),
        T("CELIBACY", "PARTYNEXTDOOR", 236, "USLD91772035"), T("OMW", "PARTYNEXTDOOR", 233, "USLD91772036"),
        T("GLORIOUS", "PARTYNEXTDOOR", 205, "USLD91772037"), T("WHEN HE'S GONE", "PARTYNEXTDOOR", 210, "USLD91772038"),
        T("GREEDY", "PARTYNEXTDOOR", 386, "USLD91772039"),
    ];

    public static readonly IReadOnlyList<AlbumOwnership.CatalogTrack> WhatATime =
    [
        T("Digital Dash", "Drake", 231, "USCM51500292"), T("Big Rings", "Drake", 217, "USCM51500293"),
        T("Live From The Gutter", "Drake", 211, "USCM51500295"), T("Diamonds Dancing", "Drake", 314, "USCM51500294"),
        T("Scholarships", "Drake", 209, "USCM51500296"), T("Plastic Bag", "Drake", 202, "USCM51500297"),
        T("I'm The Plug", "Drake", 180, "USCM51500298"), T("Change Locations", "Drake", 220, "USCM51500299"),
        T("Jumpman", "Drake", 205, "USCM51500300"), T("Jersey", "Future", 188, "USCM51500301"),
        T("30 for 30 Freestyle", "Drake", 253, "USCM51500302"),
    ];

    public static readonly IReadOnlyList<AlbumOwnership.CatalogTrack> MaidOfHonour =
    [
        T("Hoe Phase", "Drake", 203, "USUG12602459"), T("Road Trips", "Drake", 243, "USUG12602460"),
        T("Outside Tweaking", "Drake", 190, "USUG12602461"), T("Cheetah Print", "Drake", 202, "USUG12602462"),
        T("Which One", "Drake", 169, "USUG12506621"), T("Amazing Shape", "Drake", 176, "USUG12602463"),
        T("BBW", "Drake", 211, "USUG12602464"), T("True Bestie", "Drake", 148, "USUG12602465"),
        T("Where’s Your Stuff Interlude", "Drake", 52, "USUG12602466"), T("New Bestie", "Drake", 259, "USUG12602467"),
        T("Q&A", "Drake", 223, "USUG12602468"), T("Stuck", "Drake", 177, "USUG12602469"),
        T("Goose and The Juice", "Drake", 263, "USUG12602470"), T("Princess", "Drake", 193, "USUG12602471"),
    ];

    public static Album Outside(string title, string artist, int songs) =>
        new() { Id = "ext-" + title, Title = title, Artist = artist, SongCount = songs, IsLocal = false };
}

public sealed class AlbumOwnershipTests
{
    private static readonly LibraryAlbumIndex Index = new(DrakeSearch.Library);

    [Fact]
    public void HABIBTI_IsHeldWholeInsideTheExpandedEdition()
    {
        var standing = AlbumOwnership.Judge(DrakeSearch.Outside("HABIBTI", "Drake", 11), DrakeSearch.Habibti, Index);

        Assert.Equal(11, standing.Owned);
        Assert.True(standing.FullyOwned);
        Assert.Equal([DrakeSearch.FomoId], standing.Holders.Select(h => h.Id));
        Assert.Equal([DrakeSearch.FomoId], standing.StandIns.Select(h => h.Id));
    }

    [Fact]
    public void HABIBTI_FOMO_IsTheLibrarysAlbumByItsVeryNameAndArtist()
    {
        Assert.Equal([DrakeSearch.FomoId], Index.SameAlbum("HABIBTI (FOMO)", "Drake").Select(a => a.Id));
        // Its songs say the same, the four extra ones too.
        Assert.True(AlbumOwnership.Judge(DrakeSearch.Outside("HABIBTI (FOMO)", "Drake", 15), DrakeSearch.HabibtiFomo, Index).FullyOwned);
    }

    [Fact]
    public void SomeSexySongs_IsHeldButOne_AcrossTheSplitAlbum_WhateverTheCredit()
    {
        var standing = AlbumOwnership.Judge(DrakeSearch.Outside("$ome $exy $ongs 4 U", "PARTYNEXTDOOR", 21), DrakeSearch.SomeSexySongs, Index);

        // All but "PIMMIE'S DILEMMA": "GIMME A HUG" is Drake's on Deezer and PARTYNEXTDOOR's in
        // the library, and is still found, on the album of the same name at the same length.
        Assert.Equal(20, standing.Owned);
        Assert.False(standing.FullyOwned);
        Assert.Equal([DrakeSearch.SexyPndPartId, DrakeSearch.SexyDrakePartId], standing.Holders.Select(h => h.Id));
    }

    [Fact]
    public void SomeSexySongs_HasTheLibrarysNameAndArtist_DollarSignsAndAll()
    {
        Assert.Contains(DrakeSearch.SexyPndPartId, Index.SameAlbum("$ome $exy $ongs 4 U", "PARTYNEXTDOOR").Select(a => a.Id));
        Assert.Contains(DrakeSearch.SexyDrakePartId, Index.SameAlbum("$ome $exy $ongs 4 U", "Drake").Select(a => a.Id));
        Assert.Empty(Index.SameAlbum("Some Sexy Songs 4 U", "Pimmie"));
    }

    [Fact]
    public void WhatATimeToBeAlive_IsHeldInPart()
    {
        var standing = AlbumOwnership.Judge(DrakeSearch.Outside("What A Time To Be Alive", "Drake", 11), DrakeSearch.WhatATime, Index);

        Assert.Equal(2, standing.Owned);
        Assert.Equal(11, standing.Total);
        Assert.False(standing.FullyOwned);
        Assert.Empty(standing.StandIns);
    }

    [Fact]
    public void MaidOfHonour_IsNotHeldAtAll()
    {
        var standing = AlbumOwnership.Judge(DrakeSearch.Outside("MAID OF HONOUR", "Drake", 14), DrakeSearch.MaidOfHonour, Index);

        Assert.Equal(0, standing.Owned);
        Assert.Empty(standing.Holders);
    }

    [Fact]
    public void ADeluxeEdition_IsNotHeldBecauseTheStandardOneIs()
    {
        var deluxe = new[]
        {
            DrakeSearch.T("Over My Dead Body", "Drake", 272, "USCM51100001"),
            DrakeSearch.T("Shot For Me", "Drake", 224, "USCM51100002"),
            DrakeSearch.T("The Motto", "Drake", 181, "USCM51100003"),
        };
        var standing = AlbumOwnership.Judge(DrakeSearch.Outside("Take Care (Deluxe)", "Drake", 3), deluxe, Index);

        Assert.Equal(2, standing.Owned);
        Assert.False(standing.FullyOwned);
        Assert.True(Index.Likely("Take Care (Deluxe)", "Drake"));
        Assert.Empty(Index.SameAlbum("Take Care (Deluxe)", "Drake"));
    }

    [Fact]
    public void AVersion_IsAnotherSong()
    {
        var live = new[] { DrakeSearch.T("Jumpman (Live)", "Drake", 205, "USCM51599999") };
        Assert.Equal(0, AlbumOwnership.Judge(DrakeSearch.Outside("Jumpman (Live)", "Drake", 1), live, Index).Owned);
    }

    [Fact]
    public void AnotherArtistsSongOfTheSameTitle_IsNotOwned()
    {
        var other = new[] { DrakeSearch.T("Classic", "Someone Else", 179, "GBXXX2600001") };
        Assert.Equal(0, AlbumOwnership.Judge(DrakeSearch.Outside("Elsewhere", "Someone Else", 1), other, Index).Owned);
    }

    [Theory]
    [InlineData("HABIBTI (FOMO)", "habibti")]
    [InlineData("What A Time To Be Alive (Explicit Version)", "whatatimetobealive")]
    [InlineData("Take Care (Deluxe) [Explicit]", "takecare")]
    [InlineData("NOKIA - Single", "nokia")]
    [InlineData("$ome $exy $ongs 4 U", "omeexyongs4u")]
    [InlineData("(What's the Story) Morning Glory?", "whatsthestorymorningglory")]
    [InlineData("(Untitled)", "untitled")]
    public void BaseKey_SetsTheEditionAside(string title, string key) => Assert.Equal(key, AlbumOwnership.BaseKey(title));

    [Fact]
    public void Likely_GoesByNamesAndAnArtistInCommon()
    {
        Assert.True(Index.Likely("HABIBTI", "Drake"));
        Assert.True(Index.Likely("What A Time To Be Alive", "Drake"));
        // A single named as a library song.
        Assert.True(Index.Likely("NOKIA", "Drake"));
        Assert.False(Index.Likely("MAID OF HONOUR", "Drake"));
        // Another artist's album of a name the library has.
        Assert.False(Index.Likely("HABIBTI", "Nick Drake"));
        Assert.True(Index.KnowsArtist("Drake"));
        Assert.True(Index.KnowsArtist("PARTYNEXTDOOR & Drake"));
        Assert.False(Index.KnowsArtist("Nick Drake"));
    }

    private static AlbumOwnership Service(Func<Album, bool, Task<IReadOnlyList<AlbumOwnership.CatalogTrack>?>> tracklist,
        List<(string Title, bool Background)> asked) =>
        new(NullLogger<AlbumOwnership>.Instance)
        {
            Library = _ => Task.FromResult<LibraryAlbumIndex?>(Index),
            Tracklist = (album, background, _) =>
            {
                lock (asked) asked.Add((album.Title, background));
                return tracklist(album, background);
            },
        };

    private static Task<IReadOnlyList<AlbumOwnership.CatalogTrack>?> Tracks(string title) =>
        Task.FromResult<IReadOnlyList<AlbumOwnership.CatalogTrack>?>(title switch
        {
            "HABIBTI" => DrakeSearch.Habibti,
            "HABIBTI (FOMO)" => DrakeSearch.HabibtiFomo,
            "$ome $exy $ongs 4 U" => DrakeSearch.SomeSexySongs,
            "What A Time To Be Alive" => DrakeSearch.WhatATime,
            "MAID OF HONOUR" => DrakeSearch.MaidOfHonour,
            _ => null,
        });

    [Fact]
    public async Task Judge_ReadsLikelyAlbumsAtOnce_KnownArtistsInTheBackground_AndNoOthers()
    {
        var asked = new List<(string, bool)>();
        var service = Service((album, _) => Tracks(album.Title), asked);
        var albums = new[]
        {
            DrakeSearch.Outside("HABIBTI", "Drake", 11),
            DrakeSearch.Outside("HABIBTI (FOMO)", "Drake", 15),
            DrakeSearch.Outside("$ome $exy $ongs 4 U", "PARTYNEXTDOOR", 21),
            DrakeSearch.Outside("What A Time To Be Alive", "Drake", 11),
            DrakeSearch.Outside("MAID OF HONOUR", "Drake", 14),
            DrakeSearch.Outside("Five Leaves Left", "Nick Drake", 10),
        };

        var standings = (await service.JudgeAsync(albums, TimeSpan.FromSeconds(5))).ToDictionary(s => s.Album.Title);

        // The namesakes need no tracklist; Nick Drake is not in the library at all.
        Assert.Equal([("HABIBTI", false), ("MAID OF HONOUR", true), ("What A Time To Be Alive", false)],
            asked.OrderBy(a => a.Item1, StringComparer.Ordinal).ToList());
        Assert.True(standings["HABIBTI"].FullyOwned);
        Assert.Equal([DrakeSearch.FomoId], standings["HABIBTI (FOMO)"].StandIns.Select(a => a.Id));
        Assert.Contains(DrakeSearch.SexyPndPartId, standings["$ome $exy $ongs 4 U"].StandIns.Select(a => a.Id));
        Assert.Equal(2, standings["What A Time To Be Alive"].Owned);
        Assert.Equal(0, standings["MAID OF HONOUR"].Owned);
        Assert.Null(standings["Five Leaves Left"].Owned);
    }

    [Fact]
    public async Task Judge_AsksForAFewTracklistsAtATime_AndAlwaysCountsCachedOnes()
    {
        var asked = new List<(string Title, bool Background)>();
        var service = Service((album, _) => Tracks("What A Time To Be Alive"), asked);
        var cachedTitle = "Take Care (Edition 3)";
        service.Cached = album => album.Title == cachedTitle ? DrakeSearch.WhatATime : null;
        // Thirteen albums named as one the library has, and six more by an artist it knows.
        var likely = Enumerable.Range(1, 13).Select(i => DrakeSearch.Outside($"Take Care (Edition {i})", "Drake", 11)).ToList();
        var known = Enumerable.Range(1, 6).Select(i => DrakeSearch.Outside($"Other Record {i}", "Drake", 11)).ToList();
        foreach (var album in likely) Assert.True(Index.Likely(album.Title, album.Artist), album.Title);

        var standings = await service.JudgeAsync([.. likely, .. known], TimeSpan.FromSeconds(5));

        Assert.Equal(AlbumOwnership.MaxReads, asked.Count(a => !a.Background));
        Assert.Equal(AlbumOwnership.MaxBackgroundReads, asked.Count(a => a.Background));
        Assert.DoesNotContain(asked, a => a.Title == cachedTitle);
        Assert.Equal(2, standings.Single(s => s.Album.Title == cachedTitle).Owned);
        Assert.Equal(1 + AlbumOwnership.MaxReads + AlbumOwnership.MaxBackgroundReads, standings.Count(s => s.Owned is not null));
    }

    [Fact]
    public async Task Judge_SendsWhatIsNotReadInTimeUncounted()
    {
        var asked = new List<(string, bool)>();
        var never = new TaskCompletionSource<IReadOnlyList<AlbumOwnership.CatalogTrack>?>();
        var service = Service((album, _) => album.Title == "HABIBTI" ? Tracks("HABIBTI") : never.Task, asked);

        var standings = await service.JudgeAsync(
            [DrakeSearch.Outside("HABIBTI", "Drake", 11), DrakeSearch.Outside("What A Time To Be Alive", "Drake", 11)],
            TimeSpan.FromMilliseconds(200));

        Assert.Equal(11, standings[0].Owned);
        Assert.Null(standings[1].Owned);
    }

    [Fact]
    public async Task Judge_WithoutTheLibrary_CountsNothing()
    {
        var service = new AlbumOwnership(NullLogger<AlbumOwnership>.Instance);
        Assert.Empty(await service.JudgeAsync([DrakeSearch.Outside("HABIBTI", "Drake", 11)], TimeSpan.FromSeconds(1)));
    }
}

public sealed class LibrarySnapshotTests
{
    private sealed class Pages
    {
        public int Reads;
        public TaskCompletionSource? Hold;
        public bool Fail;

        public async Task<(IReadOnlyList<LibrarySongRow> Rows, int Count)?> Read(int start, int count, CancellationToken _)
        {
            Interlocked.Increment(ref Reads);
            if (Hold is { } hold) await hold.Task;
            if (Fail) return null;
            var rows = DrakeSearch.Library.Skip(start).Take(count).ToList();
            return (rows, rows.Count);
        }
    }

    private static LibrarySnapshot Snapshot(Pages pages, Func<DateTime> clock) =>
        new(null!, NullLogger<LibrarySnapshot>.Instance) { ReadPage = pages.Read, Clock = clock };

    [Fact]
    public async Task AnOldCopyIsAnsweredWith_WhileANewOneIsRead()
    {
        var now = new DateTime(2026, 10, 4, 12, 0, 0, DateTimeKind.Utc);
        var pages = new Pages();
        var snapshot = Snapshot(pages, () => now);

        var first = await snapshot.CurrentAsync();
        Assert.NotNull(first);
        Assert.Same(first, await snapshot.CurrentAsync());
        Assert.Equal(1, pages.Reads);

        now += LibrarySnapshot.FreshFor + TimeSpan.FromSeconds(1);
        pages.Hold = new TaskCompletionSource();
        // Stale: the old copy at once, and one read under way however many ask.
        Assert.Same(first, await snapshot.CurrentAsync());
        Assert.Same(first, await snapshot.CurrentAsync());
        pages.Hold.SetResult();
        // The read finishes in the background; a loaded machine can take a while.
        for (var i = 0; i < 500 && ReferenceEquals(first, await snapshot.CurrentAsync()); i++) await Task.Delay(10);
        Assert.Equal(2, pages.Reads);
        Assert.NotSame(first, await snapshot.CurrentAsync());
    }

    [Fact]
    public async Task AFailedRead_IsNotTriedAgainAtOnce()
    {
        var now = new DateTime(2026, 10, 4, 12, 0, 0, DateTimeKind.Utc);
        var pages = new Pages { Fail = true };
        var snapshot = Snapshot(pages, () => now);

        Assert.Null(await snapshot.CurrentAsync());
        Assert.Null(await snapshot.CurrentAsync());
        Assert.Equal(1, pages.Reads);

        now += LibrarySnapshot.RetryAfter;
        pages.Fail = false;
        Assert.NotNull(await snapshot.CurrentAsync());
    }
}
