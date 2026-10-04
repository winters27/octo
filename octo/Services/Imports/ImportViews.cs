namespace Octo.Services.Imports;

// What the dashboard and the Octo apps read. The names are a contract with both: the apps decode
// these fields by name, so a rename here is a change there.

/// <summary>
/// Everything the import page shows for one person. LibraryProblem says why Octo cannot tell
/// which songs the library has, when it cannot; until then nothing is fetched.
/// </summary>
public sealed record ImportOverview(SpotifyView Spotify, ReadView Reading, IReadOnlyList<ListSummary> Lists, TrickleView Trickle,
    string? LibraryProblem);

/// <summary>
/// The Spotify sign-in. Configured: a Client ID is set. OctoFinishes: the redirect URI is Octo's
/// own address, so the browser comes back here by itself; otherwise the person pastes the address
/// the browser ended on. EndsUtc: Spotify ends every sign-in six months after it was made.
/// </summary>
public sealed record SpotifyView(bool Configured, bool Connected, string? Account, string? Problem,
    string RedirectUri, string? RedirectProblem, bool OctoFinishes, DateTime? EndsUtc);

/// <summary>Reading lists from Spotify, which runs in the background after a sign-in or a refresh.</summary>
public sealed record ReadView(bool Busy, string? Step, string? Error, DateTime? FinishedUtc);

public sealed record ListSummary(string Id, string Name, string Source, string? By, string? ImageUrl, int Total,
    int Have, int Missing, int Queued, int Downloading, int Done, int NotFound, int Skipped,
    bool KeepPlaylist, string? PlaylistId, string? PlaylistNote, bool GetMissing, string? Partial, bool Gone,
    bool CanRefresh, DateTime ReadUtc, DateTime? MatchedUtc);

/// <summary>One song of a list, or of the trickle. Progress is 0 to 1 while it downloads.</summary>
public sealed record TrackView(string Key, string Title, string Artist, string? Album, int? Seconds, string State,
    string? Detail, string? LibraryId, double? Progress, DateTime? UpdatedUtc);

public sealed record ListDetail(ListSummary List, IReadOnlyList<TrackView> Tracks);

/// <summary>
/// The trickle as one person sees it: its state (see TrickleStates), when the next song starts, the
/// pace, their counts, the song running now, and the songs that finished last.
/// </summary>
public sealed record TrickleView(string State, DateTime? NextUtc, int PerHour, int Queued, int Downloading, int Done,
    int NotFound, int Skipped, TrackView? Current, IReadOnlyList<TrackView> Recent, IReadOnlyList<TrackView> Next);

/// <summary>The answer to an action: whether it did what was asked, what to tell the person, and anything it made.</summary>
public sealed record ImportActionResult(bool Ok, string Message, string? Url = null, string? ListId = null, int Count = 0);
