using Microsoft.AspNetCore.Mvc;
using System.Xml.Linq;
using System.Text.Json;
using Octo.Models.Domain;
using Octo.Models.Subsonic;
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

    public SubsonicResponseBuilder(ExternalIdRegistry idRegistry)
    {
        _idRegistry = idRegistry;
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

    /// <summary>
    /// Creates a Subsonic response containing an album with songs.
    /// </summary>
    public IActionResult CreateAlbumResponse(string format, Album album)
    {
        var totalDuration = album.Songs.Sum(s => s.Duration ?? 0);
        
        if (format == "json")
        {
            return CreateJsonResponse(new 
            { 
                status = "ok", 
                version = SubsonicVersion,
                album = new
                {
                    id = album.Id,
                    name = album.Title,
                    artist = album.Artist,
                    artistId = album.ArtistId,
                    coverArt = album.Id,
                    songCount = album.Songs.Count > 0 ? album.Songs.Count : (album.SongCount ?? 0),
                    duration = totalDuration,
                    year = album.Year ?? 0,
                    genre = album.Genre ?? "",
                    isCompilation = false,
                    song = album.Songs.Select(s => ConvertSongToJson(s)).ToList()
                }
            });
        }
        
        var ns = XNamespace.Get(SubsonicNamespace);
        var doc = new XDocument(
            new XElement(ns + "subsonic-response",
                new XAttribute("status", "ok"),
                new XAttribute("version", SubsonicVersion),
                new XElement(ns + "album",
                    new XAttribute("id", album.Id),
                    new XAttribute("name", album.Title),
                    new XAttribute("artist", album.Artist ?? ""),
                    new XAttribute("songCount", album.SongCount ?? 0),
                    new XAttribute("year", album.Year ?? 0),
                    new XAttribute("coverArt", album.Id),
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
    public Dictionary<string, object> ConvertSongToJson(Song song)
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
        var bitRate  = song.IsLocal ? 1411 : 128;
        var suffix   = song.IsLocal ? "flac" : "m4a";
        var contentType = song.IsLocal ? "audio/flac" : "audio/mp4";
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
        // to client metadata heuristics. bitDepth/samplingRate/track/year of 0
        // were the giveaways the old version emitted.
        var year = song.Year ?? DateTime.UtcNow.Year;
        var track = song.Track ?? 1;
        var bitDepth = song.IsLocal ? 16 : 16;

        return new Dictionary<string, object>
        {
            ["id"] = song.Id,
            ["parent"] = albumId,
            ["isDir"] = false,
            ["title"] = song.Title,
            ["album"] = albumName,
            ["artist"] = song.Artist ?? "",
            ["track"] = track,
            ["year"] = year,
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
    }

    /// <summary>
    /// Converts an Album domain model to Subsonic JSON format.
    /// </summary>
    public object ConvertAlbumToJson(Album album)
    {
        return new
        {
            id = album.Id,
            name = album.Title,
            artist = album.Artist,
            artistId = album.ArtistId,
            songCount = album.SongCount ?? 0,
            year = album.Year ?? 0,
            coverArt = album.Id,
            isExternal = !album.IsLocal
        };
    }

    /// <summary>
    /// Converts an Artist domain model to Subsonic JSON format.
    /// </summary>
    public object ConvertArtistToJson(Artist artist)
    {
        return new
        {
            id = artist.Id,
            name = artist.Name,
            albumCount = artist.AlbumCount ?? 0,
            coverArt = artist.Id,
            isExternal = !artist.IsLocal
        };
    }

    /// <summary>
    /// Converts a Song domain model to Subsonic XML format.
    /// </summary>
    public XElement ConvertSongToXml(Song song, XNamespace ns)
    {
        return new XElement(ns + "song",
            new XAttribute("id", song.Id),
            new XAttribute("title", song.Title),
            new XAttribute("album", song.Album ?? ""),
            new XAttribute("artist", song.Artist ?? ""),
            new XAttribute("duration", song.Duration ?? 0),
            new XAttribute("track", song.Track ?? 0),
            new XAttribute("year", song.Year ?? 0),
            new XAttribute("coverArt", song.Id),
            new XAttribute("isExternal", (!song.IsLocal).ToString().ToLower())
        );
    }

    /// <summary>
    /// Converts an Album domain model to Subsonic XML format.
    /// </summary>
    public XElement ConvertAlbumToXml(Album album, XNamespace ns)
    {
        return new XElement(ns + "album",
            new XAttribute("id", album.Id),
            new XAttribute("name", album.Title),
            new XAttribute("artist", album.Artist ?? ""),
            new XAttribute("songCount", album.SongCount ?? 0),
            new XAttribute("year", album.Year ?? 0),
            new XAttribute("coverArt", album.Id),
            new XAttribute("isExternal", (!album.IsLocal).ToString().ToLower())
        );
    }

    /// <summary>
    /// Converts an Artist domain model to Subsonic XML format.
    /// </summary>
    public XElement ConvertArtistToXml(Artist artist, XNamespace ns)
    {
        return new XElement(ns + "artist",
            new XAttribute("id", artist.Id),
            new XAttribute("name", artist.Name),
            new XAttribute("albumCount", artist.AlbumCount ?? 0),
            new XAttribute("coverArt", artist.Id),
            new XAttribute("isExternal", (!artist.IsLocal).ToString().ToLower())
        );
    }

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
