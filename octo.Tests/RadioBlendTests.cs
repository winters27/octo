using Octo.Models.Domain;
using Octo.Services.LastFm;
using Octo.Services.Radio;

namespace Octo.Tests;

/// <summary>
/// How radio weighs its sources' answers (multi-source radio): rank decay, agreement adding up,
/// weights, thin answers, guesses only as a last resort, and when the library leads.
/// </summary>
public sealed class RadioBlendTests
{
    private static LastFmService.SimilarTrack Track(string artist, string title, int? seconds = 200) =>
        new(artist, title, 1.0, seconds);

    private static RadioAnswer Catalog(string provider, RadioMatch match, params (string Artist, string Title)[] songs) =>
        new(provider, match, songs.Select(song => Track(song.Artist, song.Title)).ToList(), []);

    private static RadioAnswer Full(string provider, string prefix, int count = 25) =>
        new(provider, RadioMatch.Song, Enumerable.Range(1, count).Select(i => Track($"{prefix} {i}", $"Song {i}")).ToList(), []);

    private static readonly IReadOnlyDictionary<string, double> Even = new Dictionary<string, double>
    {
        [RadioProvider.LastFm] = 1, [RadioProvider.YouTubeMusic] = 1, [RadioProvider.ListenBrainz] = 1,
        [RadioProvider.SoundsAlike] = 1, [RadioProvider.Library] = 1,
    };

    private static List<string> Titles(IEnumerable<RadioPick> picks) =>
        picks.Select(pick => pick.Track?.Title ?? pick.LibrarySong!.Title).ToList();

    [Fact]
    public void ASongTwoSourcesSuggest_OutranksOneOnlyOneDoes()
    {
        var lastFm = Full(RadioProvider.LastFm, "A");
        var ytm = new RadioAnswer(RadioProvider.YouTubeMusic, RadioMatch.Song,
            [Track("B 1", "Song 1"), .. Enumerable.Range(2, 24).Select(i => Track($"A {i + 5}", $"Song {i + 5}"))], []);
        var picks = RadioBlend.Blend([lastFm, ytm], Even, 10, "seed", null);
        // "A 7 - Song 7" is seventh for Last.fm and second for YouTube Music: together it beats
        // either source's own first song.
        Assert.Equal("Song 7", Titles(picks)[0]);
    }

    [Fact]
    public void AWeightOfNothing_LeavesASourceOut()
    {
        var weights = new Dictionary<string, double>(Even) { [RadioProvider.YouTubeMusic] = 0 };
        var picks = RadioBlend.Blend([Full(RadioProvider.LastFm, "A"), Full(RadioProvider.YouTubeMusic, "Y")], weights, 50, "seed", null);
        Assert.DoesNotContain(picks, pick => pick.Provider == RadioProvider.YouTubeMusic);
        Assert.Equal(25, picks.Count);
    }

    [Fact]
    public void AThinAnswer_CountsLessThanAFullOne()
    {
        var thin = Catalog(RadioProvider.LastFm, RadioMatch.Song, ("Thin", "Only"));
        var full = Full(RadioProvider.YouTubeMusic, "Y");
        Assert.Equal(1.0 / RadioBlend.FullAnswer, RadioBlend.Thin(thin), 6);
        Assert.Equal(1.0, RadioBlend.Thin(full));
        var picks = RadioBlend.Blend([thin, full], Even, 50, "seed", null);
        Assert.True(Titles(picks).IndexOf("Only") > 10, "a one-song answer does not lead");
    }

    [Fact]
    public void ALibraryAnswer_IsNeverThin()
    {
        var album = new RadioAnswer(RadioProvider.Library, RadioMatch.Song, [],
            [new Song { Id = "a", Artist = "X", Title = "One" }, new Song { Id = "b", Artist = "X", Title = "Two" }]);
        Assert.Equal(1.0, RadioBlend.Thin(album));
    }

    [Fact]
    public void GuessedArtists_PlayOnlyWhenNothingElseCame()
    {
        var guess = Catalog(RadioProvider.LastFm, RadioMatch.ArtistsGuessed, ("The Year", "Full Damage"));
        Assert.Equal(["Full Damage"], Titles(RadioBlend.Blend([guess], Even, 10, "seed", null)));
        var genre = Catalog(RadioProvider.LastFm, RadioMatch.Genre, ("Kordhell", "Murder In My Mind"));
        Assert.Equal(["Murder In My Mind"], Titles(RadioBlend.Blend([guess, genre], Even, 10, "seed", null)));
    }

    [Fact]
    public void TheSeed_NeverComesBack()
    {
        var seedKey = LastFmRadioSeedNormalizer.TrackKey("Massive Attack", "Teardrop");
        var answer = Catalog(RadioProvider.LastFm, RadioMatch.Song, ("Massive Attack", "Teardrop"), ("Portishead", "Roads"));
        var library = new RadioAnswer(RadioProvider.Library, RadioMatch.Song, [], [new Song { Id = "seed-id", Artist = "Other", Title = "Other" }]);
        Assert.Equal(["Roads"], Titles(RadioBlend.Blend([answer, library], Even, 10, seedKey, "seed-id")));
    }

    [Fact]
    public void ALibraryCopy_WinsOverATrackToLookUp_AndACompleteCopyOverAStub()
    {
        var stub = new RadioAnswer(RadioProvider.SoundsAlike, RadioMatch.Song, [],
            [new Song { Id = "lib-1", Artist = "Portishead", Title = "Roads" }], LibrarySongsComplete: false);
        var track = Catalog(RadioProvider.LastFm, RadioMatch.Song, ("Portishead", "Roads"));
        var onlyStub = RadioBlend.Blend([stub, track], Even, 10, "seed", null).Single();
        Assert.Equal("lib-1", onlyStub.LibrarySong?.Id);
        Assert.True(onlyStub.NeedsReading);

        var complete = new RadioAnswer(RadioProvider.Library, RadioMatch.Song, [],
            [new Song { Id = "lib-1", Artist = "Portishead", Title = "Roads", Suffix = "flac" }]);
        var both = RadioBlend.Blend([stub, complete], Even, 10, "seed", null).Single();
        Assert.False(both.NeedsReading);
        Assert.Equal("flac", both.LibrarySong?.Suffix);
    }

    [Theory]
    [InlineData(19, true)]
    [InlineData(20, false)]
    public void TheLibraryLeads_WhenTheCatalogsHaveFewerThanTwentySongsLikeIt(int songs, bool led)
    {
        var exact = Full(RadioProvider.YouTubeMusic, "Y", songs);
        var guesses = Full(RadioProvider.LastFm, "A") with { Match = RadioMatch.ArtistsTrusted };
        Assert.Equal(led, RadioBlend.LibraryLed([exact, guesses]));
    }

    [Fact]
    public void FillerAndOneStarSongs_AreLeftOut()
    {
        var answer = new RadioAnswer(RadioProvider.LastFm, RadioMatch.Song,
            [Track("A", "Intro", 60), Track("B", "Keep"), Track("C", "Banned"), Track("D", "Interview with D")], []);
        var bans = new HashSet<string> { LastFmRadioSeedNormalizer.TrackKey("C", "Banned") };
        Assert.Equal(["Keep"], RadioBlend.WithoutFillerOrBans(answer, bans).Tracks.Select(track => track.Title));
    }

    [Theory]
    [InlineData("Phonk", "Phonk")]
    [InlineData("Phonk; Drift Phonk", "Phonk")]
    [InlineData("Electronic/Dance", "Electronic")]
    [InlineData("Unknown", null)]
    [InlineData("", null)]
    [InlineData(null, null)]
    public void PrimaryGenre_IsTheFirstRealOne(string? genre, string? expected) =>
        Assert.Equal(expected, RadioBlend.PrimaryGenre(genre));
}
