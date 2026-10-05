using System.Text.Json;
using System.Xml.Linq;
using Microsoft.Extensions.Options;
using Octo.Models.Domain;
using Octo.Models.Settings;
using Octo.Services.Radio;
using Octo.Services.Soulseek;
using Octo.Services.Subsonic;

namespace Octo.Tests;

/// <summary>
/// Radio songs say which source suggested them (octoSuggestedBy), in both formats, and station
/// songs keep the source their station was built with.
/// </summary>
public sealed class RadioProvenanceTests
{
    private static readonly XNamespace Ns = XNamespace.Get("http://subsonic.org/restapi");

    private static SubsonicResponseBuilder Builder() =>
        new(new ExternalIdRegistry(), Options.Create(new SubsonicSettings()));

    [Fact]
    public void TheSource_GoesOutInJsonAndXml_AndOnlyWhenThereIsOne()
    {
        var song = new Song { Id = "s1", Title = "Roads", Artist = "Portishead", IsLocal = true, SuggestedBy = "YouTube Music" };
        Assert.Equal("YouTube Music", Builder().ConvertSongToJson(song)["octoSuggestedBy"]);
        Assert.Equal("YouTube Music", Builder().ConvertSongToXml(song, Ns).Attribute("octoSuggestedBy")?.Value);

        song.SuggestedBy = null;
        Assert.False(Builder().ConvertSongToJson(song).ContainsKey("octoSuggestedBy"));
        Assert.Null(Builder().ConvertSongToXml(song, Ns).Attribute("octoSuggestedBy"));
    }

    [Theory]
    [InlineData(RadioProvider.LastFm, "Last.fm")]
    [InlineData(RadioProvider.YouTubeMusic, "YouTube Music")]
    [InlineData(RadioProvider.ListenBrainz, "ListenBrainz")]
    [InlineData(RadioProvider.SoundsAlike, "Sounds alike")]
    [InlineData(RadioProvider.Library, "Your library")]
    [InlineData(RadioProvider.History, null)]
    [InlineData("", null)]
    public void DisplayNames_AndBack(string provider, string? name)
    {
        Assert.Equal(name, RadioProvider.DisplayName(provider));
        if (name is not null) Assert.Equal(provider, RadioProvider.FromDisplayName(name));
    }

    [Fact]
    public async Task AStationSongThatSoundedAlike_ButIsNoLongerInTheLibrary_IsLeftOut()
    {
        await using var fixture = new RadioWebFactory();
        fixture.InstallStation();
        var station = fixture.State.FindStation("alice", fixture.StationId)!;
        station.Tracks.Add(new() { Artist = "Gone Artist", Title = "Gone Song", Duration = 200,
            Source = RadioProvider.SoundsAlike, ResolvedId = "ext-gone" });
        station.Tracks.Add(new() { Artist = "Online Artist", Title = "Online Song", Duration = 200,
            Source = RadioProvider.YouTubeMusic, ResolvedId = "ext-online" });
        fixture.State.ReplaceStations("alice", [station]);
        using var client = fixture.CreateClient();

        var body = await client.GetStringAsync($"/rest/getPlaylist?id={fixture.StationId}&u=alice&t=token&s=salt&f=json");

        // Neither is in the fake library, so both resolve as outside songs: the one found online
        // plays from outside, the one that only sounded alike does not.
        Assert.DoesNotContain("Gone Song", body);
        Assert.Contains("Online Song", body);
    }

    [Fact]
    public async Task AStationSong_SaysTheSourceItsStationWasBuiltWith()
    {
        await using var fixture = new RadioWebFactory();
        fixture.InstallStation();
        var station = fixture.State.FindStation("alice", fixture.StationId)!;
        foreach (var track in station.Tracks) track.Source = RadioProvider.YouTubeMusic;
        fixture.State.ReplaceStations("alice", [station]);
        using var client = fixture.CreateClient();

        var body = await client.GetStringAsync($"/rest/getPlaylist?id={fixture.StationId}&u=alice&t=token&s=salt&f=json");

        using var doc = JsonDocument.Parse(body);
        var entries = doc.RootElement.GetProperty("subsonic-response").GetProperty("playlist").GetProperty("entry").EnumerateArray().ToList();
        Assert.NotEmpty(entries);
        Assert.All(entries, entry => Assert.Equal("YouTube Music", entry.GetProperty("octoSuggestedBy").GetString()));
    }
}
