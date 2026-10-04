namespace Octo.Services.Imports;

/// <summary>Where a list came from. The words are a contract with the dashboard and the Octo apps.</summary>
public static class ImportSources
{
    public const string SpotifyLiked = "spotifyLiked";
    public const string SpotifyPlaylist = "spotifyPlaylist";
    public const string Link = "link";
    public const string File = "file";

    /// <summary>Read again on a schedule: from the account, or from a public link.</summary>
    public static bool CanRefresh(string source) => source is SpotifyLiked or SpotifyPlaylist or Link;
}

/// <summary>How one song of a list stands. The words are a contract with the dashboard and the Octo apps.</summary>
public static class ImportTrackStates
{
    /// <summary>In the library already.</summary>
    public const string Have = "have";
    /// <summary>Not in the library, and nobody asked for it yet.</summary>
    public const string Missing = "missing";
    public const string Queued = "queued";
    public const string Downloading = "downloading";
    /// <summary>Fetched; waiting for Navidrome to show it, after which it reads as have.</summary>
    public const string Done = "done";
    public const string NotFound = "notFound";
    public const string Skipped = "skipped";

    /// <summary>Still to run or running.</summary>
    public static bool Open(string state) => state is Queued or Downloading;
}

/// <summary>One song as the source named it.</summary>
public sealed class ImportTrack
{
    /// <summary>The same song in every list: Spotify's track id when there is one, else its artist and title.</summary>
    public string Key { get; set; } = "";
    public string Title { get; set; } = "";
    /// <summary>Every credited artist, joined with commas, the way Spotify shows them.</summary>
    public string Artist { get; set; } = "";
    public string? Album { get; set; }
    public int? Seconds { get; set; }
    public string? Isrc { get; set; }

    /// <summary>The library's song for it, from the last match, or null.</summary>
    public string? LibraryId { get; set; }

    public ImportTrack Copy() => (ImportTrack)MemberwiseClone();
}

/// <summary>One list of songs someone imported: their liked songs, a playlist, a link or a file.</summary>
public sealed class ImportList
{
    /// <summary>Stable for the same source, so reading a list again updates it rather than adding one.</summary>
    public string Id { get; set; } = "";
    /// <summary>The Navidrome user it belongs to.</summary>
    public string Owner { get; set; } = "";
    public string Name { get; set; } = "";
    public string Source { get; set; } = ImportSources.File;
    /// <summary>Spotify's playlist id, or the link it was read from.</summary>
    public string? SourceRef { get; set; }
    /// <summary>Who made it on Spotify, when that is someone else.</summary>
    public string? By { get; set; }
    public string? ImageUrl { get; set; }
    /// <summary>Spotify's version of the playlist, so an unchanged one is not read again.</summary>
    public string? SnapshotId { get; set; }
    /// <summary>How many songs the source says it has, which can be more than were readable.</summary>
    public int Total { get; set; }
    public List<ImportTrack> Tracks { get; set; } = [];
    /// <summary>Why only part of it could be read, in words, or null when it is whole.</summary>
    public string? Partial { get; set; }
    public DateTime ReadUtc { get; set; }
    public DateTime? MatchedUtc { get; set; }

    /// <summary>Kept as a Navidrome playlist of the songs the library has, in the source's order.</summary>
    public bool KeepPlaylist { get; set; }
    public string? PlaylistId { get; set; }
    /// <summary>Whose the playlist is in Navidrome, when Octo could not give it to the owner.</summary>
    public string? PlaylistNote { get; set; }

    /// <summary>Its missing songs go to the trickle, and so does every song added to it later.</summary>
    public bool GetMissing { get; set; }

    /// <summary>No longer on the account. Kept while it is a playlist or fetching, so nothing is lost.</summary>
    public bool Gone { get; set; }

    public ImportList Copy()
    {
        var copy = (ImportList)MemberwiseClone();
        copy.Tracks = Tracks.Select(track => track.Copy()).ToList();
        return copy;
    }
}

/// <summary>One missing song the trickle is to fetch for someone.</summary>
public sealed class TrickleJob
{
    public string Owner { get; set; } = "";
    public string Key { get; set; } = "";
    public string Title { get; set; } = "";
    public string Artist { get; set; } = "";
    public string? Album { get; set; }
    public int? Seconds { get; set; }
    public string? Isrc { get; set; }

    public string State { get; set; } = ImportTrackStates.Queued;
    /// <summary>What happened, in words, or what it is waiting for.</summary>
    public string? Detail { get; set; }
    /// <summary>provider:externalId of its download, while and after it runs.</summary>
    public string? AcquisitionKey { get; set; }
    public string? LibraryId { get; set; }
    public int Attempts { get; set; }

    public DateTime QueuedUtc { get; set; }
    public DateTime UpdatedUtc { get; set; }
    public DateTime? StartedUtc { get; set; }

    public TrickleJob Copy() => (TrickleJob)MemberwiseClone();
}
