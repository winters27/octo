using System.Text.Json;
using System.Xml.Linq;
using Microsoft.Extensions.DependencyInjection;
using Octo.Services.Soulseek;
using Octo.Services.Subsonic;

namespace Octo.Tests;

/// <summary>
/// The standard Subsonic getTopSongs, which every app asks for an artist's page: answered by Octo
/// in its own ranking, the caller's library songs as their own entries, outside songs while
/// search discovery is on, and Navidrome's own answer when Octo has nothing.
/// </summary>
public sealed class StandardTopSongsTests
{
    private const string Alice = "u=alice&t=good&s=salt&v=1.16.1&c=Feishin";

    private static async Task<JsonElement> JsonAsync(HttpClient client, string url)
    {
        var response = await client.GetAsync(url);
        Assert.Equal("application/json", response.Content.Headers.ContentType?.MediaType);
        using var document = JsonDocument.Parse(await response.Content.ReadAsStringAsync());
        return document.RootElement.GetProperty("subsonic-response").Clone();
    }

    private static List<JsonElement> Songs(JsonElement response) =>
        response.GetProperty("topSongs").GetProperty("song").EnumerateArray().ToList();

    [Fact]
    public async Task Json_RanksAsOctoDoes_LibrarySongsAsTheCallersOwn()
    {
        await using var factory = new ChartWebFactory();
        using var client = factory.CreateClient();

        var response = await JsonAsync(client, $"/rest/getTopSongs.view?{Alice}&f=json&artist=Daft%20Punk");
        var songs = Songs(response);

        Assert.Equal("ok", response.GetProperty("status").GetString());
        Assert.Equal(["Instant Crush (feat. Julian Casablancas)", "One More Time", "Get Lucky", "Around the World"],
            songs.Select(song => song.GetProperty("title").GetString()));
        // The library's own entries, heart and all, marked as the library's.
        Assert.Equal("nd-omt", songs[1].GetProperty("id").GetString());
        Assert.True(songs[1].TryGetProperty("starred", out _));
        Assert.False(songs[1].GetProperty("isExternal").GetBoolean());
        Assert.Equal("nd-lucky", songs[2].GetProperty("id").GetString());
        // The rest are outside songs that play and can be hearted, as in search.
        Assert.True(songs[0].GetProperty("isExternal").GetBoolean());
        Assert.Equal("Random Access Memories", songs[0].GetProperty("album").GetString());
        Assert.Equal(songs[0].GetProperty("id").GetString(), songs[0].GetProperty("coverArt").GetString());
        // Navidrome's own list was never needed.
        Assert.DoesNotContain(factory.Upstream.Calls, call => call.Contains("rest/getTopSongs", StringComparison.Ordinal));
    }

    [Fact]
    public async Task Count_TakesTheTopOfTheList()
    {
        await using var factory = new ChartWebFactory();
        using var client = factory.CreateClient();

        var songs = Songs(await JsonAsync(client, $"/rest/getTopSongs.view?{Alice}&f=json&artist=daft%20punk&count=2"));

        Assert.Equal(["Instant Crush (feat. Julian Casablancas)", "One More Time"], songs.Select(song => song.GetProperty("title").GetString()));
    }

    [Fact]
    public async Task Xml_IsTheSameList_InSubsonicsXml()
    {
        await using var factory = new ChartWebFactory();
        using var client = factory.CreateClient();

        var response = await client.GetAsync($"/rest/getTopSongs.view?{Alice}&artist=Daft%20Punk");
        Assert.Equal("application/xml", response.Content.Headers.ContentType?.MediaType);
        var root = XDocument.Parse(await response.Content.ReadAsStringAsync()).Root!;
        var top = root.Elements().Single();
        var songs = top.Elements().ToList();

        Assert.Equal("ok", root.Attribute("status")!.Value);
        Assert.Equal("topSongs", top.Name.LocalName);
        Assert.All(songs, song => Assert.Equal("song", song.Name.LocalName));
        Assert.Equal(4, songs.Count);
        var omt = songs[1];
        Assert.Equal("nd-omt", omt.Attribute("id")!.Value);
        Assert.Equal("false", omt.Attribute("isExternal")!.Value);
        Assert.Equal("House", omt.Elements().Single(element => element.Name.LocalName == "genres").Attribute("name")!.Value);
        Assert.Equal("-7.5", omt.Elements().Single(element => element.Name.LocalName == "replayGain").Attribute("trackGain")!.Value);
        Assert.Equal("true", songs[0].Attribute("isExternal")!.Value);
        Assert.Equal("audio/mp4", songs[0].Attribute("contentType")!.Value);
    }

    [Fact]
    public async Task WithoutSearchDiscovery_OnlyTheCallersOwnSongs_StillInOctosOrder()
    {
        await using var factory = new ChartWebFactory(discovery: false);
        using var client = factory.CreateClient();

        var songs = Songs(await JsonAsync(client, $"/rest/getTopSongs.view?{Alice}&f=json&artist=Daft%20Punk"));
        Assert.Equal(["nd-omt", "nd-lucky"], songs.Select(song => song.GetProperty("id").GetString()));

        // The count is of what is listed: the first library song, not the first chart row.
        var one = Songs(await JsonAsync(client, $"/rest/getTopSongs.view?{Alice}&f=json&artist=Daft%20Punk&count=1"));
        Assert.Equal(["nd-omt"], one.Select(song => song.GetProperty("id").GetString()));
    }

    [Fact]
    public async Task WithoutSearchDiscovery_AndNoneOfItInTheLibrary_IsNavidromesAnswer()
    {
        await using var factory = new ChartWebFactory(discovery: false);
        using var client = factory.CreateClient();

        var songs = Songs(await JsonAsync(client, "/rest/getTopSongs.view?u=bob&t=good&s=salt&v=1.16.1&c=x&f=json&artist=Daft%20Punk"));

        Assert.Equal(["nd-relayed"], songs.Select(song => song.GetProperty("id").GetString()));
    }

    [Fact]
    public async Task AnArtistNoSourceKnows_IsNavidromesAnswer_InEitherFormat()
    {
        await using var factory = new ChartWebFactory();
        using var client = factory.CreateClient();

        var json = Songs(await JsonAsync(client, $"/rest/getTopSongs.view?{Alice}&f=json&artist=Nobody%20Known"));
        Assert.Equal(["nd-relayed"], json.Select(song => song.GetProperty("id").GetString()));

        var xml = XDocument.Parse(await client.GetStringAsync($"/rest/getTopSongs.view?{Alice}&artist=Nobody%20Known"));
        Assert.Equal("nd-relayed", xml.Descendants().Single(element => element.Name.LocalName == "song").Attribute("id")!.Value);
    }

    [Fact]
    public async Task AWrongPassword_IsRefused_AndAMissingArtistIsNavidromesToRefuse()
    {
        await using var factory = new ChartWebFactory();
        using var client = factory.CreateClient();

        var refused = await JsonAsync(client, "/rest/getTopSongs.view?u=alice&t=bad&s=salt&v=1.16.1&c=x&f=json&artist=Daft%20Punk");
        Assert.Equal(40, refused.GetProperty("error").GetProperty("code").GetInt32());
        Assert.DoesNotContain(factory.Upstream.Calls, call => call.StartsWith("api.deezer.com", StringComparison.Ordinal));

        await client.GetStringAsync($"/rest/getTopSongs.view?{Alice}&f=json");
        Assert.Contains(factory.Upstream.Calls, call => call.Contains("rest/getTopSongs", StringComparison.Ordinal));
    }

    [Fact]
    public async Task ByArtistId_ALibraryArtistIsNamedByNavidrome_AnOutsideOneByOcto()
    {
        await using var factory = new ChartWebFactory();
        using var client = factory.CreateClient();

        // topSongsByArtistId: the library's own artist id, named by Navidrome as the caller.
        var mine = Songs(await JsonAsync(client, $"/rest/getTopSongs.view?{Alice}&f=json&id=ar-Daft-Punk&count=2"));
        Assert.Equal(["Instant Crush (feat. Julian Casablancas)", "nd-omt"],
            mine.Select(song => song.GetProperty("isExternal").GetBoolean() ? song.GetProperty("title").GetString() : song.GetProperty("id").GetString()));

        // An outside artist's id from search, which Navidrome has never heard of.
        var registry = factory.Services.GetRequiredService<ExternalIdRegistry>();
        var outsideId = registry.Register(new SoulseekRouting { Kind = RoutingKind.Artist, Artist = "Daft Punk", ExternalArtistId = "27" });
        var outside = Songs(await JsonAsync(client, $"/rest/getTopSongs.view?{Alice}&f=json&id={outsideId}"));
        Assert.Equal(4, outside.Count);

        // An outside artist no source knows gets an empty list, never Navidrome's "not found".
        var strangerId = registry.Register(new SoulseekRouting { Kind = RoutingKind.Artist, Artist = "Nobody Known" });
        var none = await JsonAsync(client, $"/rest/getTopSongs.view?{Alice}&f=json&id={strangerId}");
        Assert.Equal("ok", none.GetProperty("status").GetString());
        Assert.Empty(Songs(none));
        Assert.DoesNotContain(factory.Upstream.Calls, call => call.Contains("rest/getTopSongs", StringComparison.Ordinal));
    }

    [Fact]
    public void JsonAsXml_WritesSubsonicsShape()
    {
        using var document = JsonDocument.Parse("""
            {"id":"s1","title":"T","duration":200,"starred":"2026-01-01","isDir":false,"track":null,
             "genres":[{"name":"Rock"},{"name":"Pop"}],"isrc":["A","B"],"replayGain":{"trackGain":-6.1},
             "contributors":[{"role":"composer","artist":{"id":"a1","name":"Someone"}}]}
            """);
        var ns = System.Xml.Linq.XNamespace.Get("http://subsonic.org/restapi");

        var song = SubsonicResponseBuilder.LibrarySongXml(document.RootElement, ns);

        Assert.Equal("song", song.Name.LocalName);
        Assert.Equal("200", song.Attribute("duration")!.Value);
        Assert.Equal("false", song.Attribute("isDir")!.Value);
        Assert.Null(song.Attribute("track"));
        Assert.Equal("false", song.Attribute("isExternal")!.Value);
        Assert.Equal(["Rock", "Pop"], song.Elements(ns + "genres").Select(genre => genre.Attribute("name")!.Value));
        Assert.Equal(["A", "B"], song.Elements(ns + "isrc").Select(isrc => isrc.Value));
        Assert.Equal("-6.1", song.Element(ns + "replayGain")!.Attribute("trackGain")!.Value);
        Assert.Equal("Someone", song.Element(ns + "contributors")!.Element(ns + "artist")!.Attribute("name")!.Value);
    }
}
