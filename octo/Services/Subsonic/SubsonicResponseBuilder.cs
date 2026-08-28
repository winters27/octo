using Microsoft.AspNetCore.Mvc;
using System.Xml.Linq;
using System.Text.Json;
using Octo.Models.Domain;
using Octo.Models.Subsonic;
using Octo.Models.Radio;
using Octo.Services.Soulseek;

namespace Octo.Services.Subsonic;

/// <summary>
/// Handles building Subsonic API responses in both XML and JSON formats.
/// </summary>
public class SubsonicResponseBuilder
{
    private const string SubsonicNamespace = "http://subsonic.org/restapi";
    private const string SubsonicVersion = "1.16.1";

    private readonly ExternalIdRegistry _idRegistry;

    /// <summary>
    /// Whether an external id resolves to a lossless file. Read once at construction on
    /// purpose: it decides what every search result DECLARES, so it must not change under
    /// a client that has already cached those rows. The setting is restart-required.
    /// </summary>
    private readonly bool _externalsAreLossless;

    public SubsonicResponseBuilder(ExternalIdRegistry idRegistry,
        Microsoft.Extensions.Options.IOptions<Models.Settings.SubsonicSettings> subsonicSettings)
    {
        _idRegistry = idRegistry;
        _externalsAreLossless = subsonicSettings.Value.WaitForLosslessOnPlay;
    }

    /// <summary>
    /// Creates a generic Subsonic response with status "ok".
    /// </summary>
    public IActionResult CreateResponse(string format, string elementName, object data)
    {
        if (format == "json")
        {
            return CreateJsonResponse(new { status = "ok", version = SubsonicVersion });
        }
        
        var ns = XNamespace.Get(SubsonicNamespace);
        var doc = new XDocument(
            new XElement(ns + "subsonic-response",
                new XAttribute("status", "ok"),
                new XAttribute("version", SubsonicVersion),
                new XElement(ns + elementName)
            )
        );
        return new ContentResult { Content = doc.ToString(), ContentType = "application/xml" };
    }

    /// <summary>
    /// Creates an ok response with a single element carrying string child fields,
    /// in BOTH json and xml (unlike CreateResponse, which emits an empty element).
    /// Used for albumInfo/artistInfo2 so the data survives regardless of format.
    /// </summary>
    public IActionResult CreateInfoResponse(string format, string elementName, Dictionary<string, string> fields)
    {
        if (format == "json")
        {
            return CreateJsonResponse(new Dictionary<string, object>
            {
                ["status"] = "ok",
                ["version"] = SubsonicVersion,
                [elementName] = fields.ToDictionary(kv => kv.Key, kv => (object)kv.Value),
            });
        }

        var ns = XNamespace.Get(SubsonicNamespace);
        var el = new XElement(ns + elementName);
        foreach (var f in fields)
            el.Add(new XElement(ns + f.Key, f.Value));
        var doc = new XDocument(
            new XElement(ns + "subsonic-response",
                new XAttribute("status", "ok"),
                new XAttribute("version", SubsonicVersion),
                el));
        return new ContentResult { Content = doc.ToString(), ContentType = "application/xml" };
    }

    /// <summary>
    /// Creates a Subsonic error response.
    /// </summary>
    public IActionResult CreateError(string format, int code, string message)
    {
        if (format == "json")
        {
            return CreateJsonResponse(new 
            { 
                status = "failed", 
                version = SubsonicVersion,
                error = new { code, message }
            });
        }
        
        var ns = XNamespace.Get(SubsonicNamespace);
        var doc = new XDocument(
            new XElement(ns + "subsonic-response",
                new XAttribute("status", "failed"),
                new XAttribute("version", SubsonicVersion),
                new XElement(ns + "error",
                    new XAttribute("code", code),
                    new XAttribute("message", message)
                )
            )
        );
        return new ContentResult { Content = doc.ToString(), ContentType = "application/xml" };
    }

    /// <summary>
    /// Creates a Subsonic response containing a single song.
    /// </summary>
    public IActionResult CreateSongResponse(string format, Song song)
    {
        if (format == "json")
        {
            return CreateJsonResponse(new 
            { 
                status = "ok", 
                version = SubsonicVersion,
                song = ConvertSongToJson(song)
            });
        }
        
        var ns = XNamespace.Get(SubsonicNamespace);
        var doc = new XDocument(
            new XElement(ns + "subsonic-response",
                new XAttribute("status", "ok"),
                new XAttribute("version", SubsonicVersion),
                ConvertSongToXml(song, ns)
            )
        );
        return new ContentResult { Content = doc.ToString(), ContentType = "application/xml" };
    }

    public Dictionary<string, object> RadioPlaylistFields(LastFmRadioStation station)
    {
        var fields = new Dictionary<string, object>
        {
            ["id"] = station.Id, ["name"] = station.Name, ["owner"] = station.Owner,
            ["public"] = false, ["songCount"] = station.Tracks.Count,
            ["duration"] = station.Tracks.Sum(track => track.Duration ?? 180),
            ["created"] = station.CreatedUtc.ToString("yyyy-MM-ddTHH:mm:ss.fffZ"),
            ["changed"] = station.ChangedUtc.ToString("yyyy-MM-ddTHH:mm:ss.fffZ"),
            ["coverArt"] = "octo-radio", ["readonly"] = true,
            ["validUntil"] = station.ValidUntilUtc.ToString("yyyy-MM-ddTHH:mm:ss.fffZ")
        };
        return fields;
    }

    public IActionResult CreateRadioPlaylistResponse(string format, LastFmRadioStation station,
        IReadOnlyList<Song> songs)
    {
        var fields = RadioPlaylistFields(station);
        fields["songCount"] = songs.Count;
        fields["duration"] = songs.Sum(song => song.Duration ?? 180);
        if (format.Equals("json", StringComparison.OrdinalIgnoreCase))
        {
            var playlist = new Dictionary<string, object>(fields)
            {
                ["entry"] = songs.Select(ConvertSongToJson).ToList()
            };
            return CreateJsonResponse(new Dictionary<string, object>
            {
                ["status"] = "ok", ["version"] = SubsonicVersion, ["playlist"] = playlist
            });
        }
        var ns = XNamespace.Get(SubsonicNamespace);
        var document = new XDocument(new XElement(ns + "subsonic-response",
            new XAttribute("status", "ok"), new XAttribute("version", SubsonicVersion),
            new XElement(ns + "playlist", Attributes(fields),
                songs.Select(song => ConvertSongToXml(song, ns)))));
        return new ContentResult { Content = document.ToString(), ContentType = "application/xml" };
    }

    /// <summary>
    /// Creates a Subsonic response containing an album with songs.
    /// </summary>
    public IActionResult CreateAlbumResponse(string format, Album album)
    {
        var fields = new Dictionary<string, object>
        {
            ["id"] = album.Id,
            ["name"] = album.Title,
            ["artist"] = album.Artist ?? "",
            ["coverArt"] = album.Id,
            ["songCount"] = album.Songs.Count > 0 ? album.Songs.Count : (album.SongCount ?? 0),
            ["duration"] = album.Songs.Sum(s => s.Duration ?? 0),
            ["genre"] = album.Genre ?? "",
            ["isCompilation"] = false,
        };
        if (album.ArtistId is not null) fields["artistId"] = album.ArtistId;
        if (album.Year is int albumYear) fields["year"] = albumYear;

        if (format == "json")
        {
            var body = new Dictionary<string, object>(fields)
            {
                ["song"] = album.Songs.Select(ConvertSongToJson).ToList(),
            };
            return CreateJsonResponse(new Dictionary<string, object>
            {
                ["status"] = "ok",
                ["version"] = SubsonicVersion,
                ["album"] = body,
            });
        }

        var ns = XNamespace.Get(SubsonicNamespace);
        var doc = new XDocument(
            new XElement(ns + "subsonic-response",
                new XAttribute("status", "ok"),
                new XAttribute("version", SubsonicVersion),
                new XElement(ns + "album",
                    Attributes(fields),
                    album.Songs.Select(s => ConvertSongToXml(s, ns))
                )
            )
        );
        return new ContentResult { Content = doc.ToString(), ContentType = "application/xml" };
    }
    
    /// <summary>
    /// Creates a Subsonic response for a playlist represented as an album.
    /// Playlists appear as albums with genre "Playlist".
    /// </summary>
    public IActionResult CreatePlaylistAsAlbumResponse(string format, ExternalPlaylist playlist, List<Song> tracks)
    {
        var totalDuration = tracks.Sum(s => s.Duration ?? 0);
        
        // Build artist name with emoji and curator
        var artistName = $"🎵 {char.ToUpper(playlist.Provider[0])}{playlist.Provider.Substring(1)}";
        if (!string.IsNullOrEmpty(playlist.CuratorName))
        {
            artistName += $" {playlist.CuratorName}";
        }
        
        var artistId = $"curator-{playlist.Provider}-{playlist.CuratorName?.ToLowerInvariant().Replace(" ", "-") ?? "unknown"}";
        
        if (format == "json")
        {
            return CreateJsonResponse(new 
            { 
                status = "ok", 
                version = SubsonicVersion,
                album = new
                {
                    id = playlist.Id,
                    name = playlist.Name,
                    artist = artistName,
                    artistId = artistId,
                    coverArt = playlist.Id,
                    songCount = tracks.Count,
                    duration = totalDuration,
                    year = playlist.CreatedDate?.Year ?? 0,
                    genre = "Playlist",
                    isCompilation = false,
                    created = playlist.CreatedDate?.ToString("yyyy-MM-ddTHH:mm:ss"),
                    song = tracks.Select(s => ConvertSongToJson(s)).ToList()
                }
            });
        }
        
        var ns = XNamespace.Get(SubsonicNamespace);
        var albumElement = new XElement(ns + "album",
            new XAttribute("id", playlist.Id),
            new XAttribute("name", playlist.Name),
            new XAttribute("artist", artistName),
            new XAttribute("artistId", artistId),
            new XAttribute("songCount", tracks.Count),
            new XAttribute("duration", totalDuration),
            new XAttribute("genre", "Playlist"),
            new XAttribute("coverArt", playlist.Id)
        );
        
        if (playlist.CreatedDate.HasValue)
        {
            albumElement.Add(new XAttribute("year", playlist.CreatedDate.Value.Year));
            albumElement.Add(new XAttribute("created", playlist.CreatedDate.Value.ToString("yyyy-MM-ddTHH:mm:ss")));
        }
        
        // Add songs
        foreach (var song in tracks)
        {
            albumElement.Add(ConvertSongToXml(song, ns));
        }
        
        var doc = new XDocument(
            new XElement(ns + "subsonic-response",
                new XAttribute("status", "ok"),
                new XAttribute("version", SubsonicVersion),
                albumElement
            )
        );
        return new ContentResult { Content = doc.ToString(), ContentType = "application/xml" };
    }

    /// <summary>
    /// Creates a Subsonic response containing an artist with albums.
    /// </summary>
    public IActionResult CreateArtistResponse(string format, Artist artist, List<Album> albums)
    {
        if (format == "json")
        {
            return CreateJsonResponse(new 
            { 
                status = "ok", 
                version = SubsonicVersion,
                artist = new
                {
                    id = artist.Id,
                    name = artist.Name,
                    coverArt = artist.Id,
                    albumCount = albums.Count,
                    artistImageUrl = artist.ImageUrl,
                    album = albums.Select(a => ConvertAlbumToJson(a)).ToList()
                }
            });
        }
        
        var ns = XNamespace.Get(SubsonicNamespace);
        var doc = new XDocument(
            new XElement(ns + "subsonic-response",
                new XAttribute("status", "ok"),
                new XAttribute("version", SubsonicVersion),
                new XElement(ns + "artist",
                    new XAttribute("id", artist.Id),
                    new XAttribute("name", artist.Name),
                    new XAttribute("coverArt", artist.Id),
                    new XAttribute("albumCount", albums.Count),
                    albums.Select(a => ConvertAlbumToXml(a, ns))
                )
            )
        );
        return new ContentResult { Content = doc.ToString(), ContentType = "application/xml" };
    }

    /// <summary>
    /// Creates a JSON Subsonic response with "subsonic-response" key (with hyphen).
    /// </summary>
    public IActionResult CreateJsonResponse(object responseContent)
    {
        var response = new Dictionary<string, object>
        {
            ["subsonic-response"] = responseContent
        };
        return new JsonResult(response);
    }

    /// <summary>
    /// Converts a Song domain model to Subsonic JSON format.
    /// </summary>
    public Dictionary<string, object> ConvertSongToJson(Song song) => ConvertSongFields(song);

    /// <summary>
    /// The song shape both serializers render. See <see cref="Attributes"/> for why there
    /// is only one of these.
    /// </summary>
    private Dictionary<string, object> ConvertSongFields(Song song)
    {
        // External (Soulseek/YouTube) songs are presented as ordinary streamable
        // tracks. Setting isExternal=true causes some Subsonic clients (Arpeggio,
        // Narjo) to filter them out of play queues. We populate every field
        // Navidrome normally returns so clients don't reject the entries on
        // missing-metadata heuristics.
        // Generate Navidrome-shaped 22-char base62 ids for any external entity that
        // doesn't already have a real id. Registering here lets getCoverArt later
        // reverse-resolve the id to artist/album and look up artwork on iTunes.
        // Subsonic clients (Arpeggio in particular) drop entries whose cover-art
        // request 404s, so making these ids resolvable is what gets external songs
        // queued and played at all.
        // External (radio) songs stream as YouTube format-140 audio: m4a / AAC LC
        // inside an mp4 container, ~128kbps. The shim does NOT transcode — it
        // proxies the googlevideo bytes directly. Declared metadata MUST match the
        // real bytes, otherwise Subsonic clients prep the wrong decoder and the
        // play silently fails (Feishin holds at "loading", Arpeggi drops the entry
        // from the queue). Earlier versions claimed mp3/192k here; that was a lie.
        // With WaitForLosslessOnPlay on, /rest/stream serves the fetched FLAC under this
        // same id, so it has to be declared as one. 950 rather than 1411 because that
        // figure is uncompressed PCM and real FLAC compresses well below it: a measured
        // Mezzanine track came out at ~840kbps, where 1411 would have overstated its size
        // by about 70%. suffix and contentType are the contract a client picks its decoder
        // from and are exact; size and bitRate are estimates either way, since a FLAC's
        // true size cannot be known before it is fetched.
        var losslessExternal = !song.IsLocal && _externalsAreLossless;
        var bitRate  = song.IsLocal ? 1411 : losslessExternal ? 950 : 128;
        var suffix   = song.IsLocal || losslessExternal ? "flac" : "m4a";
        var contentType = song.IsLocal || losslessExternal ? "audio/flac" : "audio/mp4";
        var duration = song.Duration ?? 180;
        var estSize  = (long)duration * bitRate * 125;

        // Resolve a real-looking album for placeholder songs. Last.fm's
        // track.search/getsimilar don't include album names, so song.Album is
        // the empty string for nearly every external song. iOS Subsonic
        // clients (Arpeggi, Narjo) silently drop songs with album="" because
        // their library views index by album — invisible album = invisible
        // song. Apple Music represents singles as "song-name = album-name", so
        // doing the same here makes each placeholder look like a single and
        // satisfies the album-required filter.
        var albumName = string.IsNullOrWhiteSpace(song.Album)
            ? (song.Title ?? "Singles")
            : song.Album;

        var artistId = song.ArtistId ?? _idRegistry.Register(new SoulseekRouting
        {
            Kind = RoutingKind.Artist,
            Artist = song.Artist,
        });
        var albumId  = song.AlbumId  ?? _idRegistry.Register(new SoulseekRouting
        {
            Kind = RoutingKind.Album,
            Artist = song.Artist,
            Album = albumName,
        });

        // Avoid empty path segments — clients that lex on '/' (Arpeggio in
        // particular) treat double-slash as malformed and quietly drop the entry.
        var path = $"{song.Artist}/{albumName}/{song.Title}.{suffix}";
        var artistList = new[] { new Dictionary<string, object> { ["id"] = artistId, ["name"] = song.Artist ?? "" } };
        var albumArtistList = artistList;

        // Plausible defaults so external entries don't read as "obviously fake"
        // to client metadata heuristics. bitDepth/samplingRate/track of 0 were the
        // giveaways the old version emitted.
        //
        // Year is the exception and is omitted entirely when unknown, rather than
        // defaulted. It used to fall back to the current year, which is not a plausible
        // default but a wrong one: a 1995 track was published to the client as this year's
        // release, and unlike a missing field that is something the user can see and
        // sort by. Every real library is full of untagged files, so a client that rejected
        // entries without a year would already be broken against the server it is pointed
        // at.
        var track = song.Track ?? 1;
        var bitDepth = song.IsLocal ? 16 : 16;

        var fields = new Dictionary<string, object>
        {
            ["id"] = song.Id,
            ["parent"] = albumId,
            ["isDir"] = false,
            ["title"] = song.Title ?? "",
            ["album"] = albumName,
            ["artist"] = song.Artist ?? "",
            ["track"] = track,
            ["genre"] = song.Genre ?? "",
            ["coverArt"] = song.Id,
            ["size"] = estSize,
            ["contentType"] = contentType,
            ["suffix"] = suffix,
            ["duration"] = duration,
            ["bitRate"] = bitRate,
            ["path"] = path,
            ["created"] = DateTime.UtcNow.ToString("yyyy-MM-ddTHH:mm:ss.fffZ"),
            ["albumId"] = albumId,
            ["artistId"] = artistId,
            ["type"] = "music",
            ["isVideo"] = false,
            ["mediaType"] = "song",
            ["channelCount"] = 2,
            ["samplingRate"] = 44100,
            ["bitDepth"] = bitDepth,
            ["artists"] = artistList,
            ["displayArtist"] = song.Artist ?? "",
            ["albumArtists"] = albumArtistList,
            ["displayAlbumArtist"] = song.Artist ?? "",
            ["contributors"] = Array.Empty<object>(),
            ["explicitStatus"] = "",
            ["isrc"] = Array.Empty<string>(),
            ["genres"] = Array.Empty<object>(),
            ["moods"] = Array.Empty<object>(),
            ["replayGain"] = new Dictionary<string, object>(),
            ["sortName"] = (song.Title ?? "").ToLowerInvariant(),
            ["isExternal"] = false
        };

        if (song.Year is int knownYear) fields["year"] = knownYear;

        return fields;
    }

    /// <summary>
    /// Converts an Album domain model to Subsonic JSON format.
    /// </summary>
    public object ConvertAlbumToJson(Album album) => BuildAlbumFields(album);

    /// <summary>
    /// The album shape both serializers render. A client browsing by folder rather than by
    /// tags reads <c>title</c>, <c>isDir</c> and <c>parent</c>; emitting only <c>name</c>
    /// left injected albums looking unlike anything the upstream server returns.
    /// </summary>
    private Dictionary<string, object> BuildAlbumFields(Album album)
    {
        var artistId = album.ArtistId ?? _idRegistry.Register(new SoulseekRouting
        {
            Kind = RoutingKind.Artist,
            Artist = album.Artist,
        });

        var fields = new Dictionary<string, object>
        {
            ["id"] = album.Id,
            ["parent"] = artistId,
            ["isDir"] = true,
            ["title"] = album.Title,
            ["name"] = album.Title,
            ["album"] = album.Title,
            ["artist"] = album.Artist ?? "",
            ["artistId"] = artistId,
            ["songCount"] = album.SongCount ?? 0,
            ["genre"] = album.Genre ?? "",
            ["coverArt"] = album.Id,
            ["created"] = DateTime.UtcNow.ToString("yyyy-MM-ddTHH:mm:ss.fffZ"),
            ["mediaType"] = "album",
            ["displayArtist"] = album.Artist ?? "",
            ["sortName"] = (album.Title ?? "").ToLowerInvariant(),
            ["isExternal"] = !album.IsLocal,
        };

        // Deezer's album search payload carries no release date; only the per-album detail
        // call does, and fetching that for every search row would be twenty extra requests
        // against a budget this project has already been burned by. So the year is genuinely
        // unknown here and is left out rather than reported as zero.
        if (album.Year is int knownYear) fields["year"] = knownYear;

        return fields;
    }

    /// <summary>
    /// Converts an Artist domain model to Subsonic JSON format.
    /// </summary>
    public object ConvertArtistToJson(Artist artist) => BuildArtistFields(artist);

    private static Dictionary<string, object> BuildArtistFields(Artist artist) =>
        new()
        {
            ["id"] = artist.Id,
            ["name"] = artist.Name,
            ["albumCount"] = artist.AlbumCount ?? 0,
            ["coverArt"] = artist.Id,
            ["isExternal"] = !artist.IsLocal,
        };

    /// <summary>
    /// Converts a Song domain model to Subsonic XML format.
    /// </summary>
    public XElement ConvertSongToXml(Song song, XNamespace ns)
        => new(ns + "song", Attributes(ConvertSongFields(song)));

    /// <summary>
    /// Converts an Album domain model to Subsonic XML format.
    /// </summary>
    public XElement ConvertAlbumToXml(Album album, XNamespace ns)
        => new(ns + "album", Attributes(BuildAlbumFields(album)));

    /// <summary>
    /// Converts an Artist domain model to Subsonic XML format.
    /// </summary>
    public XElement ConvertArtistToXml(Artist artist, XNamespace ns)
        => new(ns + "artist", Attributes(BuildArtistFields(artist)));

    /// <summary>
    /// Renders the shared field set as XML attributes.
    ///
    /// The two serializers used to be written out by hand, separately, and drifted badly:
    /// XML emitted nine attributes for a song where JSON emitted twenty-seven, so an
    /// XML-only client received external tracks with no <c>suffix</c>, <c>contentType</c>
    /// or <c>bitRate</c> at all — the fields a client picks its decoder from, and the ones
    /// this file already warns must describe the bytes that will actually arrive. Deriving
    /// one from the other is what stops that happening again.
    /// </summary>
    private static IEnumerable<XAttribute> Attributes(IEnumerable<KeyValuePair<string, object>> fields)
    {
        foreach (var (name, value) in fields)
        {
            if (value is null) continue;

            // Subsonic carries collections as child elements, not attributes. Every
            // collection in the shared shape is emitted empty, so skipping them keeps the
            // two formats equivalent rather than merely similar.
            if (value is not string && value is System.Collections.IEnumerable) continue;

            yield return new XAttribute(name, Scalar(value));
        }
    }

    /// <summary>
    /// Invariant rendering. A comma decimal separator under a European locale would produce
    /// numbers no Subsonic client can parse.
    /// </summary>
    private static string Scalar(object value) => value switch
    {
        bool b => b ? "true" : "false",
        IFormattable f => f.ToString(null, System.Globalization.CultureInfo.InvariantCulture),
        _ => value.ToString() ?? "",
    };

    /// <summary>
    /// Converts a Subsonic JSON element to a dictionary.
    /// </summary>
    public object ConvertSubsonicJsonElement(JsonElement element, bool isLocal)
    {
        var dict = new Dictionary<string, object>();
        foreach (var prop in element.EnumerateObject())
        {
            dict[prop.Name] = ConvertJsonValue(prop.Value);
        }
        dict["isExternal"] = !isLocal;
        return dict;
    }

    /// <summary>
    /// Converts a Subsonic XML element.
    /// </summary>
    public XElement ConvertSubsonicXmlElement(XElement element, string type)
    {
        var newElement = new XElement(element);
        newElement.SetAttributeValue("isExternal", "false");
        return newElement;
    }

    private object ConvertJsonValue(JsonElement value)
    {
        return value.ValueKind switch
        {
            JsonValueKind.String => value.GetString() ?? "",
            JsonValueKind.Number => value.TryGetInt32(out var i) ? i : value.GetDouble(),
            JsonValueKind.True => true,
            JsonValueKind.False => false,
            JsonValueKind.Array => value.EnumerateArray().Select(ConvertJsonValue).ToList(),
            JsonValueKind.Object => value.EnumerateObject().ToDictionary(p => p.Name, p => ConvertJsonValue(p.Value)),
            JsonValueKind.Null => null!,
            _ => value.ToString()
        };
    }
}
