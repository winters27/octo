using Octo.Models.Settings;
using Octo.Services.LastFm;
using Octo.Services.Metadata;

namespace Octo.Tests;

/// <summary>
/// Downloads arrive with whatever the source called a genre. The reporter's library had 316
/// genres for 1,900 tracks, and the mechanism was subtler than it looks: genre was only ever
/// written when non-empty and never cleared, so junk reached the library through the ABSENCE
/// of a write rather than a bad one.
/// </summary>
public class GenreNormalizerTests
{
    private static GenreSettings Preset(int max = 1, GenreEmptyBehavior onEmpty = GenreEmptyBehavior.Clear) =>
        new()
        {
            Enabled = true,
            MaxGenres = max,
            OnEmpty = onEmpty,
            Mappings = GenreSettings.BroadGenrePreset().ToList(),
        };

    private static GenreSettings Bare(params (string Pattern, string Genre)[] rules) => new()
    {
        Enabled = true,
        MaxGenres = 1,
        Mappings = rules.Select(rule => new GenreMappingSettings
        {
            Pattern = rule.Pattern, Genre = rule.Genre, Enabled = true,
        }).ToList(),
    };

    /// <summary>The exact strings from issue #41. This is the test that would have caught it.</summary>
    [Theory]
    [InlineData("chicago rap", "Hip-Hop")]
    [InlineData("pop rap", "Hip-Hop")]
    [InlineData("trap latino", "Latin")]
    [InlineData("dance-pop", "Pop")]
    [InlineData("alternative rock", "Rock")]
    public void Normalize_CollapsesTheReportersJunk(string raw, string expected)
    {
        var result = GenreNormalizer.Normalize([raw], Preset());
        Assert.Equal(expected, result.Primary);
    }

    [Theory]
    [InlineData("Music")]
    [InlineData("People & Blogs")]
    [InlineData("Gaming")]
    [InlineData("1998")]
    [InlineData("90s")]
    [InlineData("2020s")]
    public void Normalize_NonGenresAndYears_AreDropped(string raw)
        => Assert.Empty(GenreNormalizer.Normalize([raw], Preset()).Genres);

    /// <summary>
    /// Order is the entire semantics, which is why the dashboard control is an ordered
    /// drag-and-drop list and not the unordered pinned-stations editor.
    /// </summary>
    [Fact]
    public void Normalize_AppliesRulesInOrder_FirstMatchWins()
    {
        Assert.Equal("Hip-Hop",
            GenreNormalizer.Normalize(["trap latino"], Bare(("trap", "Hip-Hop"), ("trap latino", "Latin"))).Primary);

        Assert.Equal("Latin",
            GenreNormalizer.Normalize(["trap latino"], Bare(("trap latino", "Latin"), ("trap", "Hip-Hop"))).Primary);
    }

    /// <summary>
    /// Splitting on '&amp;' was the obvious implementation and it destroys two real genres.
    /// </summary>
    [Theory]
    [InlineData("R&B")]
    [InlineData("Drum & Bass")]
    public void Normalize_NeverSplitsOnAmpersand(string raw)
    {
        var result = GenreNormalizer.Normalize([raw], new GenreSettings { Enabled = true, MaxGenres = 5 });
        Assert.Single(result.Genres);
    }

    [Theory]
    [InlineData("Rock; Pop")]
    [InlineData("Rock/Pop")]
    [InlineData("Rock, Pop")]
    public void Normalize_SplitsTheUsualSeparators(string raw)
    {
        var result = GenreNormalizer.Normalize([raw], new GenreSettings { Enabled = true, MaxGenres = 5 });
        Assert.Equal(["Rock", "Pop"], result.Genres);
    }

    /// <summary>An ID3v1 numeric genre sometimes leaks through a v2 frame as literal text.</summary>
    [Fact]
    public void Normalize_StripsId3v1NumericResidue()
        => Assert.Equal("Rock",
            GenreNormalizer.Normalize(["(17)Rock"], new GenreSettings { Enabled = true }).Primary);

    /// <summary>
    /// A blocklist entry means "this is not a genre at all", so it has to beat a substring
    /// rule that would otherwise rescue it.
    /// </summary>
    [Fact]
    public void Normalize_BlocklistBeatsMapping()
        => Assert.Empty(GenreNormalizer.Normalize(["gaming"], Bare(("gam", "Games"))).Genres);

    /// <summary>
    /// The key is already lowercased by the time the title-caser sees it, so the acronym guard
    /// has to read the original text or EDM, IDM and UKG all come back as Edm, Idm and Ukg.
    /// </summary>
    [Theory]
    [InlineData("EDM", "EDM")]
    [InlineData("IDM", "IDM")]
    [InlineData("indie rock", "Indie Rock")]
    [InlineData("shoegaze", "Shoegaze")]
    public void Normalize_UnmappedGenresAreTitleCasedButAcronymsSurvive(string raw, string expected)
        => Assert.Equal(expected,
            GenreNormalizer.Normalize([raw], new GenreSettings { Enabled = true }).Primary);

    /// <summary>
    /// Found by a whole-library preview against a real 2,364-file library: the only proposed
    /// change for one file was "Alternatif et Indé" -> "Alternatif Et Indé". No rule matched;
    /// title-casing alone counted as a change, so the backfill would have rewritten the file
    /// purely to capitalise "et". Tidying case is only worth a write when the source clearly
    /// did not bother.
    /// </summary>
    [Theory]
    [InlineData("Alternatif et Indé")]
    [InlineData("Psychedelic Rock")]
    [InlineData("Hip Hop")]
    public void Normalize_UnmappedGenreThatIsAlreadyCapitalised_KeepsItsOwnSpelling(string raw)
        => Assert.Equal(raw, GenreNormalizer.Normalize([raw], new GenreSettings { Enabled = true }).Primary);

    /// <summary>A file whose genre only needs re-casing is left byte-identical.</summary>
    [Fact]
    public void Plan_OnlyDifferenceWouldBeCasing_IsNotAChange()
    {
        var settings = new GenreSettings { Enabled = true, MaxGenres = 5 };
        var plan = GenreNormalizer.Plan(["Alternatif et Indé"], null, settings);

        Assert.Equal(GenreTagAction.Write, plan.Action);
        // The backfill compares this against the existing frame ordinally, so an identical
        // sequence means it writes nothing at all.
        Assert.Equal(["Alternatif et Indé"], plan.Genres);
    }

    [Fact]
    public void Normalize_CapsAtMaxGenresAndDedupes()
    {
        var result = GenreNormalizer.Normalize(
            ["Rock; Pop; Rock; Jazz"], new GenreSettings { Enabled = true, MaxGenres = 2 });

        Assert.Equal(["Rock", "Pop"], result.Genres);
    }

    /// <summary>
    /// The whole point of the feature. Without a Clear, "People &amp; Blogs" survives
    /// normalisation and the library keeps it forever.
    /// </summary>
    [Fact]
    public void Plan_EverythingNormalisesAway_Clears()
    {
        var plan = GenreNormalizer.Plan(["People & Blogs", "Music"], null, Preset());

        Assert.Equal(GenreTagAction.Clear, plan.Action);
        Assert.Empty(plan.Genres);
    }

    /// <summary>
    /// Writing an empty frame over an absent one is a rewrite with no benefit, and it dirties
    /// a file the run should have left byte-identical.
    /// </summary>
    [Fact]
    public void Plan_AbsentFrameAndNothingResolved_LeavesTheFileAlone()
        => Assert.Equal(GenreTagAction.None, GenreNormalizer.Plan([], null, Preset()).Action);

    [Fact]
    public void Plan_OnEmptyUnknown_WritesTheLabelInstead()
    {
        var plan = GenreNormalizer.Plan(["Music"], null, Preset(onEmpty: GenreEmptyBehavior.Unknown));

        Assert.Equal(GenreTagAction.Write, plan.Action);
        Assert.Equal("Unknown", plan.Primary);
    }

    [Fact]
    public void Plan_OnEmptyLeave_IsTheOldBehaviour()
        => Assert.Equal(GenreTagAction.None,
            GenreNormalizer.Plan(["Music"], null, Preset(onEmpty: GenreEmptyBehavior.Leave)).Action);

    /// <summary>A resolved genre outranks a stranger's tag, but does not erase a usable one.</summary>
    [Fact]
    public void Plan_ResolvedGenreLeadsTheFramesOwnValues()
    {
        var plan = GenreNormalizer.Plan(["Trap"], "Rock", Preset(max: 2));
        Assert.Equal("Rock", plan.Primary);
    }

    /// <summary>The fallback earns a turn only when nothing in the file survived.</summary>
    [Fact]
    public void Plan_FallbackOnlyAppliesWhenNothingSurvived()
    {
        // "Music" is blocklisted, so without the fallback this would clear. shoegaze maps to
        // Rock under the preset, which is the answer that proves the fallback was consulted.
        var withFallback = GenreNormalizer.Plan(["Music"], null, Preset(), ["shoegaze"]);
        Assert.Equal(GenreTagAction.Write, withFallback.Action);
        Assert.Equal("Rock", withFallback.Primary);

        var unused = GenreNormalizer.Plan(["Jazz"], null, Preset(), ["shoegaze"]);
        Assert.Equal("Jazz", unused.Primary);
    }

    /// <summary>
    /// A resumed backfill re-processes files it already touched, so running the plan over its
    /// own output has to be a no-change.
    /// </summary>
    [Fact]
    public void Plan_IsIdempotent()
    {
        var first = GenreNormalizer.Plan(["chicago rap"], null, Preset());
        var second = GenreNormalizer.Plan(first.Genres, null, Preset());

        Assert.Equal(first.Genres, second.Genres);
    }

    /// <summary>
    /// This is the test that enforces the two year rules agree. Two hand-rolled copies is how
    /// one of them quietly stops dropping "2020s" when someone fixes the other.
    /// </summary>
    [Theory]
    [InlineData("1998", true)]
    [InlineData("2026", true)]
    [InlineData("80", true)]
    [InlineData("90s", true)]
    [InlineData("1990s", true)]
    [InlineData("2020s", true)]
    [InlineData("nu metal", false)]
    [InlineData("4ad", false)]
    public void IsYearLike_MatchesWhatTheRadioKinshipFilterAlsoDrops(string tag, bool expected)
    {
        Assert.Equal(expected, GenreNormalizer.IsYearLike(tag));

        var kinship = LastFmRadioStreamService.KinshipTags([tag], artist: "Some Artist");
        Assert.Equal(expected, !kinship.Contains(tag, StringComparer.OrdinalIgnoreCase));
    }

    [Fact]
    public void IsYearLike_EmptyIsNotAYear()
        => Assert.False(GenreNormalizer.IsYearLike(""));

    /// <summary>
    /// A Turkish-locale container turns "I" into a dotless "ı" through ToLower and ToTitleCase,
    /// which silently stops "Indie" matching "indie". A container's locale is not something a
    /// user thinks of as a genre setting.
    /// </summary>
    [Fact]
    public void Normalize_IsCultureInvariant()
    {
        var original = System.Globalization.CultureInfo.CurrentCulture;
        try
        {
            System.Globalization.CultureInfo.CurrentCulture = new System.Globalization.CultureInfo("tr-TR");

            Assert.Equal("Indie", GenreNormalizer.Normalize(["indie"], new GenreSettings { Enabled = true }).Primary);
            Assert.Equal("EDM", GenreNormalizer.Normalize(["EDM"], new GenreSettings { Enabled = true }).Primary);
        }
        finally { System.Globalization.CultureInfo.CurrentCulture = original; }
    }

    /// <summary>
    /// Switching normalisation on, with nothing else configured, must not throw away accurate
    /// genres. The default used to be one genre per track, so simply enabling the feature turned
    /// "Cloud Rap, Emo, Hip Hop, Trap" into "Cloud Rap" with no mapping table involved at all.
    /// Dropping junk is what this is for; minifying real tags is a choice the user has to make.
    /// </summary>
    [Theory]
    [InlineData("Cloud Rap, Emo, Hip Hop, Trap", 4)]
    [InlineData("Psychedelic Rock, Neo-Psychedelia", 2)]
    [InlineData("Electronic, Disco, Funk, Electro", 4)]
    public void Defaults_KeepEveryRealGenreOnAWellTaggedFile(string raw, int expected)
    {
        var shippedDefaults = new GenreSettings { Enabled = true };

        var plan = GenreNormalizer.Plan([raw], null, shippedDefaults);

        Assert.Equal(expected, plan.Genres.Count);
        Assert.Equal(raw.Split(',').Select(part => part.Trim()), plan.Genres);
    }

    /// <summary>The junk-dropping half still works with the defaults; that is the part nobody
    /// has to opt into.</summary>
    [Fact]
    public void Defaults_StillDropJunkAndDuplicates()
    {
        var shippedDefaults = new GenreSettings { Enabled = true };

        var plan = GenreNormalizer.Plan(["Rock, Music, 1998, Rock, People & Blogs"], null, shippedDefaults);

        Assert.Equal(["Rock"], plan.Genres);
    }

    [Fact]
    public void EffectiveMappings_KeepsTheFirstDuplicateAndTheUsersCasing()
    {
        var settings = new GenreSettings
        {
            Mappings =
            [
                new() { Pattern = " RAP ", Genre = "Hip-Hop" },
                new() { Pattern = "rap", Genre = "Rap" },
            ],
        };

        var rule = Assert.Single(settings.EffectiveMappings());
        Assert.Equal("rap", rule.Pattern);
        Assert.Equal("Hip-Hop", rule.Genre);
    }

    [Fact]
    public void EffectiveBlocklist_UserEntriesAddAndCannotRemoveBuiltIns()
    {
        var settings = new GenreSettings { Blocklist = ["My Rip"] };
        var blocked = settings.EffectiveBlocklist();

        Assert.Contains("music", blocked);
        Assert.Contains("my rip", blocked);
    }

    [Fact]
    public void BroadGenrePreset_CollapsesRealWorldJunkToAShortList()
    {
        var settings = Preset();
        string[] fixture =
        [
            "chicago rap", "pop rap", "trap latino", "dance-pop", "k-pop", "alternative rock",
            "neo-soul", "drum and bass", "synthwave", "grindcore", "bluegrass", "bebop",
            "shoegaze", "dancehall", "trance", "baroque", "singer-songwriter", "phonk",
        ];

        var produced = fixture
            .Select(raw => GenreNormalizer.Normalize([raw], settings).Primary)
            .Where(genre => genre is not null)
            .Distinct(StringComparer.OrdinalIgnoreCase)
            .ToList();

        Assert.Equal(fixture.Length, fixture.Count(raw => GenreNormalizer.Normalize([raw], settings).Primary is not null));
        Assert.InRange(produced.Count, 1, 14);
    }
}

