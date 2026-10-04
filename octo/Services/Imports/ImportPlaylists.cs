using Octo.Services.Library;
using Octo.Services.Subsonic;

namespace Octo.Services.Imports;

/// <summary>How keeping a list's playlist went.</summary>
public sealed record PlaylistSyncResult(bool Ok, string? PlaylistId, string? Note, bool Deleted, int Songs);

/// <summary>
/// Keeps an imported list as a Navidrome playlist of the songs the library has, in the source's
/// order, so it is there in every Subsonic app. Made through Navidrome's own API as the admin and
/// then handed to the person, as Navidrome's playlist editor lets an admin do, so a song that
/// arrives later can be put in its place without anyone's password.
///
/// Only what differs is written: the songs up to the first difference stay, everything after it is
/// replaced. Songs nobody has yet are left out until they arrive. A playlist the person deleted
/// stays deleted.
/// </summary>
public sealed class ImportPlaylists
{
    private const int RemoveChunk = 100;
    private const int AddChunk = 500;

    private readonly ILogger<ImportPlaylists> _logger;

    public ImportPlaylists(NavidromePlaylistApi api, NavidromeIdentityService identity, ILogger<ImportPlaylists> logger)
    {
        _logger = logger;
        // Lambdas rather than method groups, so a test can build one with no Navidrome at all.
        Create = (name, comment, ct) => api.CreatePlaylistAsync(name, comment, ct);
        GiveTo = (id, user, ct) => api.GiveToAsync(id, user, ct);
        Exists = (id, ct) => api.PlaylistExistsAsync(id, ct);
        ReadTracks = async (id, ct) => (await api.ReadAllTracksAsync(id, ct))?.Select(row => (row.Position, row.MediaFileId)).ToList();
        Remove = (id, positions, ct) => api.RemovePositionsAsync(id, positions, ct);
        Add = (id, songs, ct) => api.AddTracksAsync(id, songs, ct);
        AdminName = () => identity.GetScanAuth()?.user;
    }

    // Seams: tests answer from memory instead of a Navidrome.
    internal Func<string, string?, CancellationToken, Task<string?>> Create { get; set; }
    internal Func<string, string, CancellationToken, Task<bool>> GiveTo { get; set; }
    internal Func<string, CancellationToken, Task<bool?>> Exists { get; set; }
    internal Func<string, CancellationToken, Task<IReadOnlyList<(string Position, string SongId)>?>> ReadTracks { get; set; }
    internal Func<string, IReadOnlyCollection<string>, CancellationToken, Task<bool>> Remove { get; set; }
    internal Func<string, IReadOnlyCollection<string>, CancellationToken, Task<bool>> Add { get; set; }
    internal Func<string?> AdminName { get; set; }

    public async Task<PlaylistSyncResult> SyncAsync(ImportList list, CancellationToken ct)
    {
        var playlistId = list.PlaylistId;
        var note = list.PlaylistNote;
        if (playlistId is not null)
        {
            var exists = await Exists(playlistId, ct);
            if (exists is null) return new(false, playlistId, note, false, 0);
            if (exists == false)
            {
                _logger.LogInformation("Playlist for imported list '{Name}' ({Owner}) was deleted in Navidrome; not making it again",
                    list.Name, list.Owner);
                return new(true, null, null, Deleted: true, 0);
            }
        }
        else
        {
            playlistId = await Create(list.Name, Comment(list), ct);
            if (playlistId is null) return new(false, null, note, false, 0);
            var admin = AdminName();
            if (!string.Equals(admin, list.Owner, StringComparison.OrdinalIgnoreCase)
                && !await GiveTo(playlistId, list.Owner, ct))
            {
                note = $"Navidrome lists it under {admin ?? "Octo's admin account"}: it could not be given to {list.Owner}.";
                _logger.LogWarning("Playlist {Id} for {Owner} stays the admin's: Navidrome would not change its owner", playlistId, list.Owner);
            }
            else note = null;
            _logger.LogInformation("Made playlist '{Name}' ({Id}) for {Owner}", list.Name, playlistId, list.Owner);
        }

        var wanted = list.Tracks.Select(track => track.LibraryId).OfType<string>().Distinct(StringComparer.Ordinal).ToList();
        if (await ReadTracks(playlistId, ct) is not { } current) return new(false, playlistId, note, false, 0);

        var same = 0;
        while (same < current.Count && same < wanted.Count && current[same].SongId == wanted[same]) same++;
        if (same == current.Count && same == wanted.Count) return new(true, playlistId, note, false, wanted.Count);

        // From the end backwards: removing the last positions leaves the earlier ones where they are.
        var stale = current.Skip(same).Select(row => row.Position).ToList();
        for (var end = stale.Count; end > 0; end -= RemoveChunk)
        {
            var start = Math.Max(0, end - RemoveChunk);
            if (!await Remove(playlistId, stale.GetRange(start, end - start), ct)) return new(false, playlistId, note, false, same);
        }
        foreach (var chunk in wanted.Skip(same).Chunk(AddChunk))
            if (!await Add(playlistId, chunk, ct)) return new(false, playlistId, note, false, same);
        _logger.LogInformation("Playlist '{Name}' ({Id}) now holds {Count} songs ({Kept} kept in place)",
            list.Name, playlistId, wanted.Count, same);
        return new(true, playlistId, note, false, wanted.Count);
    }

    private static string Comment(ImportList list) => list.Source switch
    {
        ImportSources.SpotifyLiked => "Your liked songs on Spotify, from the ones in your library. Octo keeps it up to date.",
        ImportSources.File => "Imported by Octo from a file.",
        _ => "Imported from Spotify by Octo, from the songs in your library. Octo keeps it up to date.",
    };
}
