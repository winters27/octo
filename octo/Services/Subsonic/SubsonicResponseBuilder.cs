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
public partial class SubsonicResponseBuilder
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
            // The station's own id, so each station gets its own cover rather than one shared one.
            ["coverArt"] = station.Id, ["readonly"] = true,
            ["validUntil"] = station.ValidUntilUtc.ToString("yyyy-MM-ddTHH:mm:ss.fffZ")
        };
        return fields;
    }

    /// <summary>A mix as a playlist row: the same keys a radio station has, read-only (#54).</summary>
    public Dictionary<string, object> GeneratedPlaylistFields(Octo.Services.Library.GeneratedPlaylist mix,
        Octo.Models.Settings.GeneratedPlaylistSettings settings)
    {
        var songCount = Math.Min(settings.EffectiveTrackCount, mix.PoolSize);
        return new Dictionary<string, object>
        {
            ["id"] = mix.Id, ["name"] = mix.Name, ["owner"] = mix.Owner,
            ["public"] = false, ["songCount"] = songCount, ["duration"] = songCount * 210,
            ["comment"] = "Generated by Octo from your library",
            ["created"] = mix.PeriodStartUtc.ToString("yyyy-MM-ddTHH:mm:ss.fffZ"),
            ["changed"] = mix.PeriodStartUtc.ToString("yyyy-MM-ddTHH:mm:ss.fffZ"),
            ["coverArt"] = mix.Id, ["readonly"] = true,
            ["validUntil"] = mix.PeriodEndUtc.ToString("yyyy-MM-ddTHH:mm:ss.fffZ"),
        };
    }

    /// <summary>
    /// A mix with its songs. The songs are Navidrome's own, passed through as Navidrome described
    /// them; in XML only their scalar fields, since Subsonic's XML entry is attribute-only.
    /// </summary>
    public IActionResult CreateGeneratedPlaylistResponse(string format, Octo.Services.Library.GeneratedPlaylist mix,
        Octo.Models.Settings.GeneratedPlaylistSettings settings, IReadOnlyList<System.Text.Json.Nodes.JsonObject> entries)
    {
        var fields = GeneratedPlaylistFields(mix, settings);
        fields["songCount"] = entries.Count;
        fields["duration"] = entries.Sum(entry =>
            entry["duration"] is System.Text.Json.Nodes.JsonValue value && value.TryGetValue<int>(out var seconds) ? seconds : 0);
        if (format.Equals("json", StringComparison.OrdinalIgnoreCase))
        {
            var playlist = new Dictionary<string, object>(fields)
            {
                ["entry"] = new System.Text.Json.Nodes.JsonArray(entries.Select(entry => (System.Text.Json.Nodes.JsonNode)entry.DeepClone()).ToArray()),
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
                entries.Select(entry => new XElement(ns + "entry", ScalarAttributes(entry))))));
        return new ContentResult { Content = document.ToString(), ContentType = "application/xml" };
    }

    private static IEnumerable<XAttribute> ScalarAttributes(System.Text.Json.Nodes.JsonObject entry)
    {
        foreach (var (name, node) in entry)
        {
            if (node is not System.Text.Json.Nodes.JsonValue value) continue;
            var text = value.GetValueKind() switch
            {
                JsonValueKind.String => value.GetValue<string>(),
                JsonValueKind.Number => value.ToJsonString(),
                JsonValueKind.True => "true",
                JsonValueKind.False => "false",
                _ => null,
            };
            if (text is not null) yield return new XAttribute(name, text);
        }
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
            // Required by the OpenSubsonic schema, and strict clients validate the payload
            // before they play anything: Music Assistant rejected every external album with
            // "Field created of type str is missing in AlbumID3WithSongs" (issue #35).
            // Lenient clients never noticed, so this looked like a Music Assistant problem.
            // BuildAlbumFields, which renders the album ROWS in a search, has always sent
            // it; this builds the album DETAIL and did not, so the two disagreed.
            ["created"] = DateTime.UtcNow.ToString("yyyy-MM-ddTHH:mm:ss.fffZ"),
            ["releaseTypes"] = album.ReleaseTypes.ToArray(),
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
                    TextLists(fields, ns),
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
    /// An ok response in the format the client asked for, from the JSON-shaped data Octo
    /// merges (Navidrome's JSON with outside songs or albums added). The merge only reads
    /// JSON, so a client asking for XML used to get Navidrome's own XML back, without the
    /// outside songs: half an album. XML is written the way OpenSubsonic writes it, see
    /// <see cref="JsonShapeToXml"/>.
    /// </summary>
    public IActionResult CreateMergedResponse(string format, string elementName, object data)
    {
        if (format == "json")
        {
            return CreateJsonResponse(new Dictionary<string, object>
            {
                ["status"] = "ok",
                ["version"] = SubsonicVersion,
                [elementName] = data,
            });
        }

        var ns = XNamespace.Get(SubsonicNamespace);
        var doc = new XDocument(
            new XElement(ns + "subsonic-response",
                new XAttribute("status", "ok"),
                new XAttribute("version", SubsonicVersion),
                JsonShapeToXml(ns, elementName, data)));
        return new ContentResult { Content = doc.ToString(), ContentType = "application/xml" };
    }

    /// <summary>
    /// JSON-shaped data as a Subsonic XML element, by OpenSubsonic's rules: a plain value is
    /// an attribute, one object is a child element (replayGain), a list of objects is one child
    /// element per object named for the list (song, genres, artists), and a list of plain
    /// values is one text element per value (isrc). Missing values are left out.
    /// </summary>
    internal static XElement JsonShapeToXml(XNamespace ns, string name, object? value)
    {
        var element = new XElement(ns + name);
        if (value is not IDictionary<string, object> fields)
        {
            if (value is not null) element.Value = Scalar(value);
            return element;
        }

        foreach (var (key, field) in fields)
        {
            switch (field)
            {
                case null:
                    break;
                case string text:
                    element.SetAttributeValue(key, text);
                    break;
                case IDictionary<string, object> nested:
                    element.Add(JsonShapeToXml(ns, key, nested));
                    break;
                case System.Collections.IEnumerable list:
                    foreach (var item in list)
                    {
                        if (item is not null) element.Add(JsonShapeToXml(ns, key, item));
                    }
                    break;
                default:
                    element.SetAttributeValue(key, Scalar(field));
                    break;
            }
        }
        return element;
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

    /// <summary>The OpenSubsonic extension a client checks for before it asks for getAcquisitions.
    /// Version 2 adds each row's log (getAcquisition), clearing finished rows (clearAcquisitions)
    /// and Find songs (findSongs, getFoundSongs, pickFoundSong).</summary>
    public const string AcquisitionsExtension = "octoAcquisitions";
    public const int AcquisitionsExtensionVersion = 2;

    /// <summary>
    /// getAcquisitions: the caller's own downloads in flight or recently ended. Always JSON,
    /// whatever format was asked for; the only client that reads it parses JSON, and the
    /// extension it is advertised under says so.
    /// </summary>
    public IActionResult CreateAcquisitionsResponse(IEnumerable<Octo.Services.Common.AcquisitionSnapshot> rows) =>
        CreateJsonResponse(new Dictionary<string, object?>
        {
            ["status"] = "ok",
            ["version"] = SubsonicVersion,
            ["type"] = "octo",
            ["acquisitions"] = new Dictionary<string, object?>
            {
                ["acquisition"] = rows.Select(row => AcquisitionJson(row)).ToList(),
            },
        });

    /// <summary>
    /// One acquisition on the wire. The field names are a contract with the Octo app; a rename
    /// here breaks its progress ring. Nulls are sent as null rather than left out.
    /// </summary>
    public static Dictionary<string, object?> AcquisitionJson(Octo.Services.Common.AcquisitionSnapshot row) => new()
    {
        ["id"] = row.Id,
        ["artist"] = row.Artist,
        ["title"] = row.Title,
        ["album"] = row.Album,
        ["state"] = row.State.ToString().ToLowerInvariant(),
        ["progress"] = row.Progress,
        ["bytesDone"] = row.BytesDone,
        ["bytesTotal"] = row.BytesTotal,
        ["source"] = row.Source,
        ["startedAt"] = Utc(row.StartedAt),
        ["updatedAt"] = Utc(row.UpdatedAt),
        ["error"] = row.Error,
        ["libraryId"] = row.LibraryId,
        // Added for the app's words beside the ring. Older apps ignore them.
        ["ahead"] = row.Ahead,
        ["note"] = row.Note,
        // Added for the downloads drawer: the row's own key for getAcquisition, what it is for,
        // the copy being fetched, the picture to draw, and how long its log is.
        ["key"] = row.Key,
        ["kind"] = row.Kind,
        ["quality"] = row.Quality,
        ["peer"] = row.Peer,
        ["coverArt"] = row.LibraryId ?? row.Id,
        ["logLines"] = row.LogLines,
    };

    /// <summary>getAcquisition: one row with its whole log.</summary>
    public IActionResult CreateAcquisitionResponse(Octo.Services.Common.AcquisitionSnapshot row)
    {
        var json = AcquisitionJson(row);
        json["event"] = (row.Events ?? []).Select(EventJson).ToList();
        return CreateJsonResponse(new Dictionary<string, object?>
        {
            ["status"] = "ok",
            ["version"] = SubsonicVersion,
            ["type"] = "octo",
            ["acquisition"] = json,
        });
    }

    private static Dictionary<string, object?> EventJson(Octo.Services.Common.AcquisitionEvent line) => new()
    {
        ["at"] = Utc(line.At),
        ["kind"] = line.Kind,
        ["text"] = line.Text,
        ["detail"] = line.Detail,
        ["candidate"] = line.Candidates?.Select(CandidateJson).ToList(),
    };

    /// <summary>One copy a source offered, in the log and in Find songs alike.</summary>
    public static Dictionary<string, object?> CandidateJson(Octo.Services.Common.AcquisitionCandidate copy) => new()
    {
        ["source"] = copy.Source,
        ["peer"] = copy.Peer,
        ["file"] = copy.File,
        ["folder"] = copy.Folder,
        ["title"] = copy.Title,
        ["album"] = copy.Album,
        ["format"] = copy.Format,
        ["quality"] = copy.Quality,
        ["bitRate"] = copy.BitRate,
        ["bitDepth"] = copy.BitDepth,
        ["sampleRate"] = copy.SampleRate,
        ["size"] = copy.Size,
        ["length"] = copy.Length,
        ["queueLength"] = copy.QueueLength,
        ["freeSlot"] = copy.FreeSlot,
        ["speed"] = copy.Speed,
        ["rank"] = copy.Rank,
        ["note"] = copy.Note,
    };

    private static string Utc(DateTime value) =>
        DateTime.SpecifyKind(value, DateTimeKind.Utc)
            .ToString("yyyy-MM-dd'T'HH:mm:ss'Z'", System.Globalization.CultureInfo.InvariantCulture);

    /// <summary>
    /// The extensions Octo answers itself, with their versions. songLyrics is here because Octo
    /// answers getLyricsBySongId for every song, and for the ones it answers itself it honours
    /// enhanced=true (version 2) with word cues; a Navidrome that lists fewer versions has them
    /// added, never removed.
    /// </summary>
    internal static readonly (string Name, int[] Versions)[] OwnExtensions =
    [
        (AcquisitionsExtension, [1, AcquisitionsExtensionVersion]),
        (LyricsExtension, [LyricsExtensionVersion]),
        (LibraryActionsExtension, [1, LibraryActionsExtensionVersion]),
        ("songLyrics", [1, 2]),
        (TopSongsExtension, [TopSongsExtensionVersion]),
    ];

    /// <summary>
    /// Navidrome's getOpenSubsonicExtensions with Octo's own added, in the format asked for.
    /// A failed answer passes through untouched; with no answer at all, Octo lists its own.
    /// </summary>
    public IActionResult MergeOpenSubsonicExtensions(string format, byte[]? upstream, string? contentType,
        bool lyricsChoices = true, bool libraryActions = false, bool topSongs = true)
    {
        var json = format.Equals("json", StringComparison.OrdinalIgnoreCase);
        // octoLyrics is only listed while its lookups can run, so a client never offers a
        // "choose lyrics" that can only answer that lookups are off. octoLibraryActions is only
        // listed while library actions are on, and octoTopSongs while search discovery is, for
        // the same reason.
        var own = OwnExtensions
            .Where(extension => lyricsChoices || extension.Name != LyricsExtension)
            .Where(extension => libraryActions || extension.Name != LibraryActionsExtension)
            .Where(extension => topSongs || extension.Name != TopSongsExtension)
            .ToArray();
        try
        {
            if (upstream is { Length: > 0 })
            {
                if (json)
                {
                    var root = System.Text.Json.Nodes.JsonNode.Parse(upstream)!.AsObject();
                    var envelope = root["subsonic-response"]!.AsObject();
                    if ((string?)envelope["status"] == "ok")
                    {
                        if (envelope["openSubsonicExtensions"] is not System.Text.Json.Nodes.JsonArray list)
                            envelope["openSubsonicExtensions"] = list = new System.Text.Json.Nodes.JsonArray();
                        foreach (var (name, versions) in own)
                        {
                            var existing = list.FirstOrDefault(item => (string?)item?["name"] == name) as System.Text.Json.Nodes.JsonObject;
                            var have = (existing?["versions"] as System.Text.Json.Nodes.JsonArray)?
                                .Select(version => version?.GetValue<int>() ?? 0).ToHashSet() ?? [];
                            var all = have.Union(versions).Where(version => version > 0).Order().ToArray();
                            if (existing is null)
                                list.Add(new System.Text.Json.Nodes.JsonObject
                                {
                                    ["name"] = name,
                                    ["versions"] = new System.Text.Json.Nodes.JsonArray(all.Select(v => (System.Text.Json.Nodes.JsonNode?)v).ToArray()),
                                });
                            else if (all.Length != have.Count)
                                existing["versions"] = new System.Text.Json.Nodes.JsonArray(all.Select(v => (System.Text.Json.Nodes.JsonNode?)v).ToArray());
                        }
                    }
                    return new ContentResult { Content = root.ToJsonString(), ContentType = "application/json" };
                }

                var document = XDocument.Parse(System.Text.Encoding.UTF8.GetString(upstream));
                if (document.Root is { } response && (string?)response.Attribute("status") == "ok")
                {
                    var ns = response.Name.Namespace;
                    foreach (var (name, versions) in own)
                    {
                        var existing = response.Elements(ns + "openSubsonicExtensions")
                            .FirstOrDefault(item => (string?)item.Attribute("name") == name);
                        if (existing is null)
                        {
                            response.Add(new XElement(ns + "openSubsonicExtensions", new XAttribute("name", name),
                                versions.Select(version => new XElement(ns + "versions", version))));
                            continue;
                        }
                        var have = existing.Elements(ns + "versions")
                            .Select(version => int.TryParse(version.Value, out var number) ? number : 0).ToHashSet();
                        foreach (var version in versions.Where(version => !have.Contains(version)))
                            existing.Add(new XElement(ns + "versions", version));
                    }
                }
                return new ContentResult { Content = document.ToString(), ContentType = contentType ?? "application/xml" };
            }
        }
        catch (Exception)
        {
            // Not something this can read. Hand it back as it came rather than break the call.
            return new FileContentResult(upstream!, contentType ?? (json ? "application/json" : "application/xml"));
        }

        if (json)
            return CreateJsonResponse(new Dictionary<string, object?>
            {
                ["status"] = "ok",
                ["version"] = SubsonicVersion,
                ["type"] = "octo",
                ["openSubsonicExtensions"] = own
                    .Select(extension => new Dictionary<string, object?> { ["name"] = extension.Name, ["versions"] = extension.Versions })
                    .ToArray(),
            });
        var xmlNs = XNamespace.Get(SubsonicNamespace);
        var ours = new XDocument(new XElement(xmlNs + "subsonic-response",
            new XAttribute("status", "ok"), new XAttribute("version", SubsonicVersion),
            own.Select(extension => new XElement(xmlNs + "openSubsonicExtensions",
                new XAttribute("name", extension.Name),
                extension.Versions.Select(version => new XElement(xmlNs + "versions", version))))));
        return new ContentResult { Content = ours.ToString(), ContentType = "application/xml" };
    }

    /// <summary>
    /// Converts a Song domain model to Subsonic JSON format.
    /// </summary>
    public Dictionary<string, object> ConvertSongToJson(Song song) => ConvertSongFields(song);

    /// <summary>
    /// The song shape both serializers render. See <see cref="Attributes"/> for why there
    /// is only one of these.
    /// </summary>
    /// <summary>The content type a client picks its decoder from, for a library file's suffix.</summary>
    internal static string ContentTypeFor(string suffix) => suffix switch
    {
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "m4a" or "aac" or "alac" or "mp4" => "audio/mp4",
        "ogg" or "opus" or "oga" => "audio/ogg",
        "wav" => "audio/wav",
        _ => "application/octet-stream",
    };

    private Dictionary<string, object> ConvertSongFields(Song song)
    {
        // A song outside the library says so: isExternal is true, and it carries no
        // file facts (path, size, created, bit depth, sample rate, channels), because
        // there is no file. It used to borrow a whole library song's shape, with a
        // path, a size and "now" as the date added, so an album page showed songs the
        // server does not hold as if they were in the library. What it keeps is how it
        // streams: suffix and contentType, which a client picks its decoder from.
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
        // from and are exact. For an outside song the 950 is only an estimate, so it is
        // not sent (see the end of this method).
        //
        // A library song is declared as Navidrome described it when that is known, and as FLAC
        // only when it is not: radio and the Discovery blend put library MP3s here too.
        var losslessExternal = !song.IsLocal && _externalsAreLossless;
        var localSuffix = string.IsNullOrWhiteSpace(song.Suffix) ? "flac" : song.Suffix.Trim().ToLowerInvariant();
        var bitRate  = song.IsLocal ? song.BitRate is > 0 ? song.BitRate.Value : 1411 : losslessExternal ? 950 : 128;
        var suffix   = song.IsLocal ? localSuffix : losslessExternal ? "flac" : "m4a";
        var contentType = song.IsLocal ? ContentTypeFor(localSuffix) : losslessExternal ? "audio/flac" : "audio/mp4";
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

        // The path and the file defaults below (bit depth, sample rate, channels) are for
        // library songs that reach here without them; an outside song drops them.
        //
        // Year is omitted entirely when unknown, rather than
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
            // OpenSubsonic's isrc is a list. An album track Deezer described carries its code,
            // and a library song keeps the ones Navidrome gave it.
            ["isrc"] = song.IsrcsForClients().ToArray(),
            ["genres"] = Array.Empty<object>(),
            ["moods"] = Array.Empty<object>(),
            ["replayGain"] = new Dictionary<string, object>(),
            ["sortName"] = (song.Title ?? "").ToLowerInvariant(),
            ["isExternal"] = false
        };

        if (song.Year is int knownYear) fields["year"] = knownYear;

        if (!song.IsLocal)
        {
            // No file on the server, so nothing about one.
            foreach (var key in FileOnlyFields) fields.Remove(key);
            // A FLAC's rate is a guess until it is fetched; the 128 of the AAC stream is real.
            if (losslessExternal) fields.Remove("bitRate");
            fields["isExternal"] = true;
        }

        return fields;
    }

    /// <summary>Fields only a file in the library can truthfully have.</summary>
    internal static readonly string[] FileOnlyFields =
        ["path", "size", "created", "bitDepth", "samplingRate", "channelCount"];

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
            ["duration"] = album.Songs.Sum(s => s.Duration ?? 0),
            ["genre"] = album.Genre ?? "",
            ["coverArt"] = album.Id,
            ["created"] = DateTime.UtcNow.ToString("yyyy-MM-ddTHH:mm:ss.fffZ"),
            ["mediaType"] = "album",
            ["displayArtist"] = album.Artist ?? "",
            // OpenSubsonic asks for the list even when it is empty, so a client knows the server
            // speaks it. Only an outside album comes through here; a library album keeps the
            // types Navidrome gave it.
            ["releaseTypes"] = album.ReleaseTypes.ToArray(),
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
    {
        var fields = ConvertSongFields(song);
        return new(ns + "song", Attributes(fields), TextLists(fields, ns));
    }

    /// <summary>
    /// Converts an Album domain model to Subsonic XML format.
    /// </summary>
    public XElement ConvertAlbumToXml(Album album, XNamespace ns)
    {
        var fields = BuildAlbumFields(album);
        return new(ns + "album", Attributes(fields), TextLists(fields, ns));
    }

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

            // Subsonic carries collections as child elements, not attributes. The lists of
            // plain text among them are written by TextLists; the rest are emitted empty.
            if (value is not string && value is System.Collections.IEnumerable) continue;

            yield return new XAttribute(name, Scalar(value));
        }
    }

    /// <summary>
    /// A list of plain text, such as OpenSubsonic's <c>isrc</c>, as one child element per value:
    /// <c>&lt;isrc&gt;USRC17607839&lt;/isrc&gt;</c>, the shape the upstream server writes.
    /// </summary>
    private static IEnumerable<XElement> TextLists(IEnumerable<KeyValuePair<string, object>> fields, XNamespace ns) =>
        fields.Where(field => field.Value is string[])
            .SelectMany(field => ((string[])field.Value).Select(value => new XElement(ns + field.Key, value)));

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
