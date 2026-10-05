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
/// Navidrome and Deezer as getTopSongs and "Popular right now" need them. Navidrome accepts any
/// token but "bad"; each listener has a library of their own (alice holds two Daft Punk songs,
/// bob nothing), searched by every word of the query the way Navidrome searches.
/// </summary>
internal sealed class ChartUpstream : HttpMessageHandler
{
    public ConcurrentQueue<string> Calls { get; } = new();

    /// <summary>Each listener's library songs, as Navidrome describes them.</summary>
    public ConcurrentDictionary<string, List<JsonObject>> Libraries { get; } = new(StringComparer.OrdinalIgnoreCase)
    {
        ["alice"] =
        [
            Song("nd-omt", "One More Time", "Daft Punk", "Discovery", extra: """
                ,"starred":"2026-01-01T00:00:00Z","genres":[{"name":"House"}],"isrc":["GBDUW0000059"],"replayGain":{"trackGain":-7.5}
                """),
            Song("nd-lucky", "Get Lucky", "Daft Punk", "Random Access Memories"),
        ],
        ["bob"] = [],
    };

    /// <summary>When set, every library search fails, as a Navidrome that stopped answering.</summary>
    public bool SearchFails { get; set; }

    public string Chart { get; set; } = """
        {"data":[
        {"id":10,"title":"Dracula","duration":209,"explicit_lyrics":true,"explicit_content_lyrics":1,"artist":{"id":134790,"name":"Tame Impala"},"album":{"id":11,"title":"Deadbeat","cover_xl":"https://cdn/deadbeat.jpg"}},
        {"id":2,"title":"One More Time","duration":320,"artist":{"id":27,"name":"Daft Punk"},"album":{"id":302127,"title":"Discovery","cover_xl":"https://cdn/discovery.jpg"}},
        {"id":12,"title":"Espresso","duration":175,"explicit_lyrics":false,"explicit_content_lyrics":0,"artist":{"id":9635624,"name":"Sabrina Carpenter"},"album":{"id":13,"title":"Short n' Sweet","cover_xl":"https://cdn/sns.jpg"}}
        ]}
        """;

    public static JsonObject Song(string id, string title, string artist, string album, string extra = "") =>
        JsonNode.Parse($$"""
            {"id":"{{id}}","title":"{{title}}","artist":"{{artist}}","album":"{{album}}","albumId":"al-{{id}}","artistId":"ar-{{artist.Replace(' ', '-')}}","coverArt":"al-{{id}}","duration":300,"suffix":"flac"{{extra.Trim()}}}
            """)!.AsObject();

    protected override Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken ct)
    {
        var uri = request.RequestUri!;
        var query = System.Web.HttpUtility.ParseQueryString(uri.Query);
        var path = uri.AbsolutePath.Trim('/');
        Calls.Enqueue(uri.Host + "/" + path + "?" + query["query"] + query["q"]);
        if (uri.Host == "navidrome.test") return Navidrome(path, query);
        if (uri.Host == "api.deezer.com") return Deezer(path, query);
        return Task.FromResult(new HttpResponseMessage(HttpStatusCode.NotFound));
    }

    private Task<HttpResponseMessage> Navidrome(string path, System.Collections.Specialized.NameValueCollection query)
    {
        var xml = query["f"] != "json";
        string Ok(string body) => xml
            ? $"""<subsonic-response xmlns="http://subsonic.org/restapi" status="ok" version="1.16.1">{body}</subsonic-response>"""
            : "{\"subsonic-response\":{\"status\":\"ok\",\"version\":\"1.16.1\",\"type\":\"navidrome\"" + (body.Length > 0 ? "," + body : "") + "}}";
        string Failed(int code, string message) => xml
            ? $"""<subsonic-response xmlns="http://subsonic.org/restapi" status="failed" version="1.16.1"><error code="{code}" message="{message}"/></subsonic-response>"""
            : "{\"subsonic-response\":{\"status\":\"failed\",\"version\":\"1.16.1\",\"error\":{\"code\":" + code
              + ",\"message\":\"" + message + "\"}}}";

        if (query["t"] == "bad") return Answer(Failed(40, "Wrong username or password"), xml);
        var user = query["u"] ?? "";
        var library = Libraries.GetOrAdd(user, _ => []);
        switch (path.Replace(".view", ""))
        {
            case "rest/ping":
                return Answer(Ok(""), xml);
            case "rest/getPlaylists":
                return Answer(xml ? Ok("<playlists/>") : Ok("\"playlists\":{}"), xml);
            case "rest/getPlaylist":
                return Answer(Failed(70, "playlist not found"), xml);
            case "rest/getTopSongs":
                return Answer(xml
                    ? Ok("""<topSongs><song id="nd-relayed" title="From Navidrome" artist="Someone"/></topSongs>""")
                    : Ok("""
                        "topSongs":{"song":[{"id":"nd-relayed","title":"From Navidrome","artist":"Someone"}]}
                        """.Trim()), xml);
            case "rest/search3":
                if (SearchFails) return Task.FromResult(new HttpResponseMessage(HttpStatusCode.InternalServerError));
                return Answer(Ok("\"searchResult3\":" + Search(library, query).ToJsonString()), xml: false);
            case "rest/getArtist":
                var named = library.FirstOrDefault(song => song["artistId"]?.ToString() == query["id"]);
                return named is null
                    ? Answer(Failed(70, "Artist not found"), xml)
                    : Answer(Ok("\"artist\":{\"id\":\"" + query["id"] + "\",\"name\":\"" + named["artist"] + "\"}"), xml);
            case "rest/getCoverArt":
                return Task.FromResult(new HttpResponseMessage(HttpStatusCode.NotFound));
            default:
                return Answer(Ok(""), xml);
        }
    }

    /// <summary>Navidrome's search: every word of the query in the song's artist, title or album,
    /// and an empty query is the whole library, paged.</summary>
    private static JsonObject Search(List<JsonObject> library, System.Collections.Specialized.NameValueCollection query)
    {
        int Number(string name, int fallback) => int.TryParse(query[name], out var value) ? value : fallback;
        var term = (query["query"] ?? "").Trim().Trim('"').ToLowerInvariant();
        List<JsonObject> songs;
        if (term.Length == 0)
        {
            songs = library.Skip(Number("songOffset", 0)).Take(Number("songCount", 20)).ToList();
        }
        else
        {
            var words = term.Split(' ', StringSplitOptions.RemoveEmptyEntries);
            songs = library.Where(song =>
            {
                var text = $"{song["artist"]} {song["title"]} {song["album"]}".ToLowerInvariant();
                return words.All(text.Contains);
            }).Take(Number("songCount", 20)).ToList();
        }
        var result = new JsonObject { ["song"] = new JsonArray(songs.Select(song => (JsonNode)song.DeepClone()).ToArray()) };
        if (term.Length > 0 && Number("artistCount", 20) > 0)
        {
            var artists = library.Where(song => string.Equals(song["artist"]?.ToString(), term, StringComparison.OrdinalIgnoreCase))
                .Select(song => new JsonObject { ["id"] = song["artistId"]!.ToString(), ["name"] = song["artist"]!.ToString() })
                .DistinctBy(artist => artist["id"]!.ToString()).ToArray();
            if (artists.Length > 0) result["artist"] = new JsonArray(artists.Select(artist => (JsonNode)artist).ToArray());
        }
        return result;
    }

    private Task<HttpResponseMessage> Deezer(string path, System.Collections.Specialized.NameValueCollection query) => path switch
    {
        "search/artist" => Json((query["q"] ?? "").Contains("daft", StringComparison.OrdinalIgnoreCase)
            ? """{"data":[{"id":27,"name":"Daft Punk","nb_album":40,"nb_fan":5000000}]}"""
            : """{"data":[]}"""),
        "artist/27/top" => Json("""
            {"data":[
            {"id":1,"title":"Instant Crush (feat. Julian Casablancas)","duration":337,"artist":{"id":27,"name":"Daft Punk"},"album":{"id":6575789,"title":"Random Access Memories","cover_xl":"https://cdn/ram.jpg"}},
            {"id":2,"title":"One More Time","duration":320,"artist":{"id":27,"name":"Daft Punk"},"album":{"id":302127,"title":"Discovery","cover_xl":"https://cdn/discovery.jpg"}},
            {"id":4,"title":"Get Lucky (feat. Pharrell Williams and Nile Rodgers)","duration":369,"artist":{"id":27,"name":"Daft Punk"},"album":{"id":6575789,"title":"Random Access Memories","cover_xl":"https://cdn/ram.jpg"}},
            {"id":5,"title":"Around the World","duration":429,"artist":{"id":27,"name":"Daft Punk"},"album":{"id":7,"title":"Homework","cover_xl":"https://cdn/homework.jpg"}}
            ]}
            """),
        "chart/0/tracks" => Json(Chart),
        _ => Task.FromResult(new HttpResponseMessage(HttpStatusCode.NotFound)),
    };

    private static Task<HttpResponseMessage> Answer(string body, bool xml) =>
        Task.FromResult(new HttpResponseMessage(HttpStatusCode.OK)
        {
            Content = new StringContent(body, Encoding.UTF8, xml ? "application/xml" : "application/json"),
        });

    private static Task<HttpResponseMessage> Json(string body) => Answer(body, xml: false);
}

/// <summary>An Octo in front of <see cref="ChartUpstream"/>, with the settings these features read.</summary>
internal sealed class ChartWebFactory(bool discovery = true, bool popularNow = true, string explicitFilter = "All")
    : WebApplicationFactory<Program>
{
    private readonly string _directory = Path.Combine(Path.GetTempPath(), "octo-chart-" + Guid.NewGuid());
    public ChartUpstream Upstream { get; } = new();
    public PopularPlaylistService Popular => Services.GetRequiredService<PopularPlaylistService>();

    protected override void ConfigureWebHost(IWebHostBuilder builder)
    {
        Directory.CreateDirectory(_directory);
        builder.ConfigureAppConfiguration((_, configuration) => configuration.AddInMemoryCollection(
            new Dictionary<string, string?>
            {
                ["Subsonic:Url"] = "http://navidrome.test",
                ["Subsonic:AutoDetectDownloadPath"] = "false",
                ["Subsonic:EnableSearchDiscovery"] = discovery ? "true" : "false",
                ["Subsonic:ExplicitFilter"] = explicitFilter,
                ["GeneratedPlaylists:PopularNow"] = popularNow ? "true" : "false",
                ["Soulseek:BaseUrl"] = "http://127.0.0.1:1",
                ["YouTube:ShimUrl"] = "http://127.0.0.1:1",
                ["Library:DownloadPath"] = _directory,
                ["LastFm:ApiKey"] = "",
            }));
        builder.ConfigureServices(services =>
        {
            services.RemoveAll<IHostedService>();
            services.RemoveAll<IHttpClientFactory>();
            services.AddSingleton<IHttpClientFactory>(new ReviewFixtures.OneClientFactory(Upstream));
        });
    }

    public override async ValueTask DisposeAsync()
    {
        await base.DisposeAsync();
        try { Directory.Delete(_directory, recursive: true); } catch { /* best effort */ }
    }
}
