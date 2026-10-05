using System.Collections.Concurrent;
using System.Net;
using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;
using System.Xml.Linq;
using Microsoft.AspNetCore.Hosting;
using Microsoft.AspNetCore.Mvc.Testing;
using Microsoft.Extensions.Configuration;
using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.DependencyInjection.Extensions;
using Microsoft.Extensions.Hosting;
using Microsoft.Extensions.Options;
using Octo.Models.Domain;
using Octo.Models.Settings;
using Octo.Services.Common;
using Octo.Services.CoverArt;
using Octo.Services.Library;
using Octo.Services.Soulseek;
using Octo.Services.Subsonic;

namespace Octo.Tests;

/// <summary>
/// "Popular right now": the chart as a read-only playlist per listener, in getPlaylists and
/// getPlaylist for every app, made again each period, and left alone by a bad day.
/// </summary>
public sealed class PopularPlaylistTests
{
    private const string Alice = "u=alice&t=good&s=salt&v=1.16.1&c=Feishin";
    private const string Bob = "u=bob&t=good&s=salt&v=1.16.1&c=Feishin";

    private static async Task<JsonElement> JsonAsync(HttpClient client, string url)
    {
        using var document = JsonDocument.Parse(await client.GetStringAsync(url));
        return document.RootElement.GetProperty("subsonic-response").Clone();
    }

    private static List<JsonElement> Playlists(JsonElement response) =>
        response.GetProperty("playlists").TryGetProperty("playlist", out var rows) ? rows.EnumerateArray().ToList() : [];

    private static JsonElement? PopularRow(JsonElement response) =>
        Playlists(response).Cast<JsonElement?>().FirstOrDefault(row => row!.Value.GetProperty("name").GetString() == PopularPlaylistService.Name);

    private static List<JsonElement> Entries(JsonElement response) =>
        response.GetProperty("playlist").GetProperty("entry").EnumerateArray().ToList();

    [Fact]
    public async Task GetPlaylists_ListsItReadOnly_ForEachListener()
    {
        await using var factory = new ChartWebFactory();
        using var client = factory.CreateClient();

        var alice = PopularRow(await JsonAsync(client, $"/rest/getPlaylists.view?{Alice}&f=json"))!.Value;
        var bob = PopularRow(await JsonAsync(client, $"/rest/getPlaylists.view?{Bob}&f=json"))!.Value;

        Assert.StartsWith("og", alice.GetProperty("id").GetString());
        Assert.Equal(PopularPlaylistService.PlaylistId("alice"), alice.GetProperty("id").GetString());
        Assert.NotEqual(alice.GetProperty("id").GetString(), bob.GetProperty("id").GetString());
        Assert.True(alice.GetProperty("readonly").GetBoolean());
        Assert.False(alice.GetProperty("public").GetBoolean());
        Assert.Equal("alice", alice.GetProperty("owner").GetString());
        Assert.Equal(3, alice.GetProperty("songCount").GetInt32());
        Assert.Equal(alice.GetProperty("id").GetString(), alice.GetProperty("coverArt").GetString());
        Assert.Equal(SubsonicResponseBuilder.PopularComment, alice.GetProperty("comment").GetString());
    }

    [Fact]
    public async Task GetPlaylists_Xml_ListsItToo()
    {
        await using var factory = new ChartWebFactory();
        using var client = factory.CreateClient();

        var document = XDocument.Parse(await client.GetStringAsync($"/rest/getPlaylists.view?{Alice}"));
        var row = document.Descendants().Single(element => element.Name.LocalName == "playlist"
            && element.Attribute("name")?.Value == PopularPlaylistService.Name);

        Assert.Equal(PopularPlaylistService.PlaylistId("alice"), row.Attribute("id")!.Value);
        Assert.Equal("true", row.Attribute("readonly")!.Value);
        Assert.Equal("3", row.Attribute("songCount")!.Value);
    }

    [Fact]
    public async Task GetPlaylist_LibrarySongsAreTheListenersOwn_TheRestOutside()
    {
        await using var factory = new ChartWebFactory();
        using var client = factory.CreateClient();
        await client.GetStringAsync($"/rest/getPlaylists.view?{Alice}&f=json");

        var playlist = await JsonAsync(client,
            $"/rest/getPlaylist.view?{Alice}&f=json&id={PopularPlaylistService.PlaylistId("alice")}");
        var entries = Entries(playlist);

        Assert.Equal(["Dracula", "One More Time", "Espresso"], entries.Select(e => e.GetProperty("title").GetString()));
        // The library's copy, as Navidrome described it to alice: heart, genres and gain kept.
        var omt = entries[1];
        Assert.Equal("nd-omt", omt.GetProperty("id").GetString());
        Assert.True(omt.TryGetProperty("starred", out _));
        Assert.Equal(-7.5, omt.GetProperty("replayGain").GetProperty("trackGain").GetDouble());
        // The others are outside songs that play and can be hearted, explicit marks and all.
        var dracula = entries[0];
        Assert.True(dracula.GetProperty("isExternal").GetBoolean());
        Assert.Equal("Deadbeat", dracula.GetProperty("album").GetString());
        Assert.Equal("explicit", dracula.GetProperty("explicitStatus").GetString());
        Assert.NotNull(factory.Services.GetRequiredService<ExternalIdRegistry>().Lookup(dracula.GetProperty("id").GetString()!));
        Assert.Equal(3, playlist.GetProperty("playlist").GetProperty("songCount").GetInt32());
        Assert.Equal(209 + 300 + 175, playlist.GetProperty("playlist").GetProperty("duration").GetInt32());

        // bob has none of it, so every song is outside for him.
        var bobs = Entries(await JsonAsync(client,
            $"/rest/getPlaylist.view?{Bob}&f=json&id={PopularPlaylistService.PlaylistId("bob")}"));
        Assert.All(bobs, entry => Assert.True(entry.GetProperty("isExternal").GetBoolean()));
    }

    [Fact]
    public async Task GetPlaylist_Xml_EntriesCarryTheirFieldsAndLists()
    {
        await using var factory = new ChartWebFactory();
        using var client = factory.CreateClient();

        var document = XDocument.Parse(await client.GetStringAsync(
            $"/rest/getPlaylist.view?{Alice}&id={PopularPlaylistService.PlaylistId("alice")}"));
        var playlist = document.Root!.Elements().Single();
        var entries = playlist.Elements().Where(element => element.Name.LocalName == "entry").ToList();

        Assert.Equal("playlist", playlist.Name.LocalName);
        Assert.Equal(3, entries.Count);
        var omt = entries[1];
        Assert.Equal("nd-omt", omt.Attribute("id")!.Value);
        Assert.Equal("House", omt.Elements().Single(element => element.Name.LocalName == "genres").Attribute("name")!.Value);
        Assert.Equal("GBDUW0000059", omt.Elements().Single(element => element.Name.LocalName == "isrc").Value);
        Assert.Equal("true", entries[0].Attribute("isExternal")!.Value);
        Assert.Equal("m4a", entries[0].Attribute("suffix")!.Value);
    }

    [Fact]
    public async Task GetPlaylist_AWrongPassword_IsRefused()
    {
        await using var factory = new ChartWebFactory();
        using var client = factory.CreateClient();

        var refused = await JsonAsync(client,
            $"/rest/getPlaylist.view?u=alice&t=bad&s=salt&f=json&id={PopularPlaylistService.PlaylistId("alice")}");

        Assert.Equal(40, refused.GetProperty("error").GetProperty("code").GetInt32());
        Assert.DoesNotContain(factory.Upstream.Calls, call => call.StartsWith("api.deezer.com/chart", StringComparison.Ordinal));
    }

    [Fact]
    public async Task TheList_HoldsStillForItsPeriod_ThenIsMadeAgain()
    {
        await using var factory = new ChartWebFactory();
        using var client = factory.CreateClient();
        // Half an hour into a period: periods start every 6 hours from midnight UTC.
        var start = new DateTime(2026, 10, 4, 0, 30, 0, DateTimeKind.Utc);
        factory.Popular.Clock = () => start;
        var url = $"/rest/getPlaylist.view?{Alice}&f=json&id={PopularPlaylistService.PlaylistId("alice")}";
        Assert.Equal("explicit", Entries(await JsonAsync(client, url))[0].GetProperty("explicitStatus").GetString());

        // alice adds Dracula. Within the period the list holds still, so every app agrees.
        factory.Upstream.Libraries["alice"].Add(ChartUpstream.Song("nd-dracula", "Dracula", "Tame Impala", "Deadbeat"));
        factory.Popular.Clock = () => start.AddHours(PopularPlaylistService.RefreshHours - 2);
        Assert.True(Entries(await JsonAsync(client, url))[0].GetProperty("isExternal").GetBoolean());

        // The next period makes it again, and Dracula is now hers.
        factory.Popular.Clock = () => start.AddHours(PopularPlaylistService.RefreshHours);
        var renewed = Entries(await JsonAsync(client, url));
        Assert.Equal("nd-dracula", renewed[0].GetProperty("id").GetString());
        var row = PopularRow(await JsonAsync(client, $"/rest/getPlaylists.view?{Alice}&f=json"))!.Value;
        Assert.Equal("2026-10-04T12:00:00.000Z", row.GetProperty("validUntil").GetString());
    }

    [Fact]
    public async Task ALibraryThatStopsAnswering_LeavesTheLastListAlone()
    {
        await using var factory = new ChartWebFactory();
        using var client = factory.CreateClient();
        var start = new DateTime(2026, 10, 4, 1, 0, 0, DateTimeKind.Utc);
        factory.Popular.Clock = () => start;
        var url = $"/rest/getPlaylist.view?{Alice}&f=json&id={PopularPlaylistService.PlaylistId("alice")}";
        await JsonAsync(client, url);

        factory.Upstream.SearchFails = true;
        factory.Popular.Clock = () => start.AddHours(PopularPlaylistService.RefreshHours);
        var entries = Entries(await JsonAsync(client, url));

        // One More Time is still alice's own copy, not turned into an outside song.
        Assert.Equal("nd-omt", entries[1].GetProperty("id").GetString());
    }

    [Fact]
    public async Task Off_ItIsNotListed_AndItsIdIsNavidromes()
    {
        await using var factory = new ChartWebFactory(popularNow: false);
        using var client = factory.CreateClient();

        Assert.Null(PopularRow(await JsonAsync(client, $"/rest/getPlaylists.view?{Alice}&f=json")));
        var opened = await JsonAsync(client,
            $"/rest/getPlaylist.view?{Alice}&f=json&id={PopularPlaylistService.PlaylistId("alice")}");
        Assert.Equal(70, opened.GetProperty("error").GetProperty("code").GetInt32());
        Assert.DoesNotContain(factory.Upstream.Calls, call => call.StartsWith("api.deezer.com/chart", StringComparison.Ordinal));
    }

    [Fact]
    public async Task WithoutSearchDiscovery_OnlyTheListenersOwnSongs()
    {
        await using var factory = new ChartWebFactory(discovery: false);
        using var client = factory.CreateClient();

        var row = PopularRow(await JsonAsync(client, $"/rest/getPlaylists.view?{Alice}&f=json"))!.Value;
        Assert.Equal(1, row.GetProperty("songCount").GetInt32());
        var entries = Entries(await JsonAsync(client,
            $"/rest/getPlaylist.view?{Alice}&f=json&id={PopularPlaylistService.PlaylistId("alice")}"));
        Assert.Equal(["nd-omt"], entries.Select(e => e.GetProperty("id").GetString()));

        // bob has none of the chart, so he has nothing to list.
        Assert.Null(PopularRow(await JsonAsync(client, $"/rest/getPlaylists.view?{Bob}&f=json")));
    }

    [Fact]
    public async Task TheExplicitFilter_LeavesOutWhatItShould()
    {
        await using var factory = new ChartWebFactory(explicitFilter: "CleanOnly");
        using var client = factory.CreateClient();

        var entries = Entries(await JsonAsync(client,
            $"/rest/getPlaylist.view?{Bob}&f=json&id={PopularPlaylistService.PlaylistId("bob")}"));

        Assert.Equal(["One More Time", "Espresso"], entries.Select(e => e.GetProperty("title").GetString()));
    }

    [Fact]
    public async Task ItsCover_IsDrawnLikeAMixs()
    {
        await using var factory = new ChartWebFactory();
        using var client = factory.CreateClient();
        await client.GetStringAsync($"/rest/getPlaylists.view?{Alice}&f=json");

        using var response = await client.GetAsync(
            $"/rest/getCoverArt.view?{Alice}&id={PopularPlaylistService.PlaylistId("alice")}&size=300");

        Assert.Equal(HttpStatusCode.OK, response.StatusCode);
        Assert.Equal("image/jpeg", response.Content.Headers.ContentType?.MediaType);
        Assert.True((await response.Content.ReadAsByteArrayAsync()).Length > 1000);
    }

    [Fact]
    public async Task ASyncingApp_HoldsItsOutsideSongsUnderThePlaylistsIds()
    {
        await using var factory = new ChartWebFactory();
        using var client = factory.CreateClient();
        const string symfonium = "u=alice&t=good&s=salt&v=1.16.1&c=Symfonium";
        await client.GetStringAsync($"/rest/getPlaylists.view?{symfonium}&f=json");

        var walk = await JsonAsync(client,
            $"/rest/search3.view?{symfonium}&f=json&query=%22%22&songCount=1000&songOffset=0&albumCount=0&artistCount=0");
        var synced = walk.GetProperty("searchResult3").GetProperty("song").EnumerateArray()
            .Select(song => song.GetProperty("id").GetString()).ToList();
        var listed = Entries(await JsonAsync(client,
            $"/rest/getPlaylist.view?{symfonium}&f=json&id={PopularPlaylistService.PlaylistId("alice")}"));

        // The library first, then the catalog, holding the list's outside songs by the very ids
        // the playlist lists.
        Assert.Equal(["nd-omt", "nd-lucky"], synced.Take(2));
        var outside = listed.Where(entry => entry.TryGetProperty("isExternal", out var external) && external.GetBoolean())
            .Select(entry => entry.GetProperty("id").GetString()).ToList();
        Assert.Equal(2, outside.Count);
        // Other stations' songs may follow too; the list's own must be among them.
        Assert.Subset(synced.Skip(2).ToHashSet(), outside.ToHashSet());
    }

    // ---- the pieces ---------------------------------------------------------------------

    private static TopSongsService.TopSong Top(int rank, string title, int? explicitContent = null) =>
        new(rank, new Song { Id = $"out-{rank}", Artist = "Someone", Title = title, ExplicitContentLyrics = explicitContent }, null, null);

    [Fact]
    public void Assemble_KeepsChartOrder_TheLibrarysCopyFirst()
    {
        var rows = new[] { Top(1, "A", 1), Top(2, "B"), Top(3, "C", 3), Top(4, "D", 4) };
        var library = new JsonElement?[] { null, JsonDocument.Parse("""{"id":"nd-b","title":"B"}""").RootElement.Clone(), null, null };
        JsonObject AsJson(Song song) => new() { ["id"] = song.Id, ["isExternal"] = true };

        var (all, outside) = PopularPlaylistService.Assemble(rows, library, true, ExplicitFilter.All, AsJson);
        Assert.Equal(["out-1", "nd-b", "out-3", "out-4"], all.Select(entry => entry["id"]!.GetValue<string>()));
        Assert.Equal(3, outside.Count);

        // Clean only drops the explicit ones, partly explicit (4) included.
        var clean = PopularPlaylistService.Assemble(rows, library, true, ExplicitFilter.CleanOnly, AsJson).Entries;
        Assert.Equal(["nd-b", "out-3"], clean.Select(entry => entry["id"]!.GetValue<string>()));
        var explicitOnly = PopularPlaylistService.Assemble(rows, library, true, ExplicitFilter.ExplicitOnly, AsJson).Entries;
        Assert.Equal(["out-1", "nd-b", "out-4"], explicitOnly.Select(entry => entry["id"]!.GetValue<string>()));

        // Without outside songs only the library's are left, and a library song is never filtered.
        var (mine, none) = PopularPlaylistService.Assemble(rows, library, false, ExplicitFilter.CleanOnly, AsJson);
        Assert.Equal(["nd-b"], mine.Select(entry => entry["id"]!.GetValue<string>()));
        Assert.Empty(none);
    }

    [Fact]
    public void TheCover_SaysChart()
    {
        var spec = CoverArtService.Spec(PopularPlaylistService.Name, ListKinds.Chart, 50, null);
        Assert.Equal("Popular right now", spec.Name);
        Assert.Equal("Chart", spec.Line);
        Assert.Equal("50 songs", spec.Footer);
        Assert.Equal("Top 50", CoverArtService.Spec("Top 50 Chart", ListKinds.Chart, null, null).Name);
    }

    [Fact]
    public void KeepPlayable_MakesAForgottenOutsideSongKnownAgain_UnderTheSameId()
    {
        var registry = new ExternalIdRegistry();
        var id = registry.Register(new SoulseekRouting { Kind = RoutingKind.Song, Artist = "Tame Impala", Title = "Dracula", Album = "Deadbeat", Duration = 209 });
        var song = new Song { Id = id, Artist = "Tame Impala", Title = "Dracula", Album = "Deadbeat", Duration = 209 };
        // Octo forgot it: a registry that never saw it, as after an eviction.
        var forgetful = new ExternalIdRegistry();
        var service = new PopularPlaylistService(null!, null!, forgetful, null!,
            TestOptions.Monitor(new GeneratedPlaylistSettings()), TestOptions.Monitor(new SubsonicSettings()),
            Microsoft.Extensions.Logging.Abstractions.NullLogger<PopularPlaylistService>.Instance);
        Assert.Null(forgetful.Lookup(id));

        service.KeepPlayable([song]);

        var known = forgetful.Lookup(id);
        Assert.NotNull(known);
        Assert.Equal("Dracula", known!.Title);
        Assert.Equal("Deadbeat", known.Album);
    }
}
