namespace Octo.Services.Admin;

/// <summary>
/// Which saved settings have not yet reached the services that read them once, at construction.
///
/// Snapshotted when the app starts and compared on every settings read, so the dashboard can say
/// "restart to apply" for exactly as long as it is true, and stop saying it after a restart,
/// without the browser keeping its own copy of what was saved. A service built lazily after a
/// change does see the new value, so this can over-report; that is the honest direction. Under-
/// reporting is the bug the dashboard's restart markers existed to prevent.
/// </summary>
public sealed class RestartTracker
{
    /// <summary>
    /// Keys whose consumers capture the value when they are constructed. Keep in step with the
    /// dashboard's data-restart markers, which name the same settings as "Section.Key".
    /// </summary>
    public static readonly string[] Keys =
    [
        "Library:DownloadPath",             // BaseDownloadService, LocalLibraryService, PlaylistSyncService
        "YouTube:ShimUrl",                  // YouTubeResolver
        "Subsonic:Url",                     // live in most places; a restart is still the safe advice
        "Subsonic:LibraryPath",
        "Subsonic:WaitForLosslessOnPlay",   // SubsonicResponseBuilder, deliberately
        "Soulseek:BaseUrl",                 // SoulseekClient and SoulseekDownloadService take IOptions
        "Soulseek:Username",
        "Soulseek:Password",
        "Soulseek:SearchWaitSeconds",
        "Soulseek:DownloadTimeoutSeconds",
        "Soulseek:MinFileSizeBytes",
        "Soulseek:PreferredExtension",
        "LibraryActions:Enabled",           // the playlist worker decides once, at start
        "LibraryActions:PlaylistsEnabled",
        "LibraryActions:PollIntervalSeconds",
    ];

    private readonly Dictionary<string, string?> _atStartup;

    public RestartTracker(IConfiguration configuration) =>
        _atStartup = Keys.ToDictionary(key => key, key => Normalize(configuration[key]));

    /// <summary>The keys, as "Section:Key", whose current value differs from the one at startup.</summary>
    public IReadOnlyList<string> Pending(IConfiguration configuration) =>
        Keys.Where(key => !Same(Normalize(configuration[key]), _atStartup[key])).ToList();

    // Empty and missing mean the same thing to every consumer here.
    internal static string? Normalize(string? value) =>
        string.IsNullOrWhiteSpace(value) ? null : value.Trim();

    // A bool can arrive as "True" from an env var and "true" from the file. Anything else is
    // compared exactly: a password or a Linux path that changes only in case has changed.
    private static bool Same(string? now, string? atStartup) =>
        bool.TryParse(now, out var a) && bool.TryParse(atStartup, out var b)
            ? a == b
            : string.Equals(now, atStartup, StringComparison.Ordinal);
}
