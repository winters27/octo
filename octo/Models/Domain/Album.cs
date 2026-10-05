namespace Octo.Models.Domain;

/// <summary>
/// Represents an album
/// </summary>
public class Album
{
    public string Id { get; set; } = string.Empty;
    public string Title { get; set; } = string.Empty;
    public string Artist { get; set; } = string.Empty;
    public string? ArtistId { get; set; }
    public int? Year { get; set; }
    public int? SongCount { get; set; }
    public string? CoverArtUrl { get; set; }
    public string? Genre { get; set; }

    /// <summary>
    /// OpenSubsonic's release types, such as "album", "ep" or "single": lowercase, as
    /// Navidrome relays them for library albums. What lets a client group an artist's page
    /// the way the catalog does, without guessing from a track count.
    /// </summary>
    public List<string> ReleaseTypes { get; set; } = new();
    public bool IsLocal { get; set; }

    /// <summary>
    /// For an outside album: how many of its songs the library already holds, whatever album
    /// it filed them under, sent as <c>ownedCount</c> beside <c>songCount</c>. Null when its
    /// songs were not looked at, which a client reads as it always has.
    /// </summary>
    public int? OwnedCount { get; set; }
    public string? ExternalProvider { get; set; }
    public string? ExternalId { get; set; }
    public List<Song> Songs { get; set; } = new();
}
