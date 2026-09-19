namespace Octo.Models.Settings;

public enum LibraryAction { Delete, WrongSong, WrongVersion, BetterQuality }

/// <summary>How a user asks for an action.</summary>
public enum LibraryActionTrigger { Playlists, Ratings }

public sealed class LibraryActionDefinition
{
    public LibraryAction Action { get; set; }

    /// <summary>The playlist name, after the prefix. Editable so it reads the way the user
    /// thinks about it rather than the way Octo names it internally.</summary>
    public string Name { get; set; } = "";

    public bool Enabled { get; set; }

    /// <summary>
    /// Which star count maps to this action, 1-5, when ratings are on.
    ///
    /// Nullable so an omitted value and a deliberate 0 mean different things: unset takes the
    /// built-in mapping, 0 means no rating ever triggers this action. Without that distinction
    /// a config that simply does not mention ratings would silently unmap every action, which
    /// is a setting doing something the user never asked for.
    /// </summary>
    public int? Rating { get; set; }
}

/// <summary>
/// Fixing a wrong download from the player you are already using, rather than the dashboard.
///
/// Every part of this is configurable on purpose: which actions exist at all, what they are
/// called, who may trigger them, how they are triggered, whether anything is written on the
/// first run, where deleted files go and how long they stay. The defaults are the cautious
/// reading of every one of those, because the feature deletes files Octo did not create.
/// </summary>
public class LibraryActionSettings
{
    /// <summary>
    /// Master switch. Off by default: these actions delete files Octo did not create.
    /// Environment variable: LIBRARY_ACTIONS_ENABLED
    /// </summary>
    public bool Enabled { get; set; } = false;

    /// <summary>
    /// Option A. Octo keeps a playlist per enabled action; adding a track to one requests it.
    /// Works with every client, and the intent is unambiguous because the playlist says so.
    /// Environment variable: LIBRARY_ACTIONS_PLAYLISTS
    /// </summary>
    public bool PlaylistsEnabled { get; set; } = true;

    /// <summary>
    /// Option B. Map star ratings onto the same actions.
    ///
    /// OFF by default and it should stay that way for most people: a star is a one-tap gesture
    /// with no confirmation anywhere in any client, and rating a track you dislike is a
    /// perfectly ordinary thing to do. It is configurable rather than absent because a user
    /// who never rates music may genuinely prefer it.
    /// Environment variable: LIBRARY_ACTIONS_RATINGS
    /// </summary>
    public bool RatingsEnabled { get; set; } = false;

    /// <summary>
    /// Prefix on every action playlist, so they sort together. Set it to "" for no prefix.
    /// Environment variable: LIBRARY_ACTIONS_PREFIX
    /// </summary>
    public string PlaylistPrefix { get; set; } = "\U0001F6E0 ";

    /// <summary>
    /// Per-action names, enablement and star mapping. Managed in the dashboard or settings
    /// JSON rather than .env, the same call the pinned radio stations make.
    /// </summary>
    public List<LibraryActionDefinition> Actions { get; set; } = [];

    /// <summary>
    /// Navidrome usernames allowed to trigger actions. EMPTY MEANS NOBODY, never everybody:
    /// the fail-open reading of an empty allowlist on a feature that deletes files is not a
    /// defensible default.
    /// </summary>
    public List<string> AllowedUsers { get; set; } = [];

    /// <summary>
    /// Do everything except touch a file. On by default so the first run of a newly enabled
    /// install is a rehearsal the operator reads before it is real.
    /// Environment variable: LIBRARY_ACTIONS_DRY_RUN
    /// </summary>
    public bool DryRun { get; set; } = true;

    /// <summary>
    /// Where a removed file goes, relative to the music root. The leading dot keeps it out of
    /// Navidrome's scan. Nothing here is ever a File.Delete except the retention sweep.
    /// Environment variable: LIBRARY_ACTIONS_TRASH_DIR
    /// </summary>
    public string QuarantineDirectory { get; set; } = ".octo-trash";

    /// <summary>
    /// How long a quarantined file is kept before it is really deleted. 0 keeps them forever,
    /// which is the right choice for anyone who would rather manage the space by hand.
    /// Environment variable: LIBRARY_ACTIONS_TRASH_DAYS
    /// </summary>
    public int QuarantineRetentionDays { get; set; } = 30;

    /// <summary>
    /// How often the action playlists are checked. Fast enough to feel responsive on a gesture
    /// the user performs and then watches for, slow enough that an idle install is quiet.
    /// Environment variable: LIBRARY_ACTIONS_POLL_SECONDS
    /// </summary>
    public int PollIntervalSeconds { get; set; } = 60;

    /// <summary>
    /// Ceiling on actions applied per sweep. A throttle, not a stop: a client that dumps five
    /// thousand tracks into an action playlist gets this many a minute, which buys hours of
    /// reaction time.
    /// Environment variable: LIBRARY_ACTIONS_MAX_PER_CYCLE
    /// </summary>
    public int MaxActionsPerCycle { get; set; } = 20;

    /// <summary>
    /// Keep the quarantined original after a replacement is verified, rather than relying on
    /// the retention sweep. Off means the retention window is the only safety net.
    /// Environment variable: LIBRARY_ACTIONS_KEEP_REPLACED
    /// </summary>
    public bool KeepReplacedOriginals { get; set; } = true;

    public TimeSpan EffectivePollInterval =>
        TimeSpan.FromSeconds(Math.Clamp(PollIntervalSeconds, 15, 3600));

    public int EffectiveMaxActionsPerCycle => Math.Clamp(MaxActionsPerCycle, 1, 200);

    /// <summary>0 is a real choice here, meaning "never sweep", so it is not clamped upward.</summary>
    public int EffectiveQuarantineRetentionDays =>
        QuarantineRetentionDays <= 0 ? 0 : Math.Clamp(QuarantineRetentionDays, 1, 3650);

    public string EffectivePrefix => PlaylistPrefix ?? "";

    /// <summary>
    /// Sanitised, so a directory traversal cannot be typed into a settings field. One path
    /// segment, and a leading dot is preserved because it is what keeps the folder out of
    /// Navidrome's scan.
    /// </summary>
    public string EffectiveQuarantineDirectory
    {
        get
        {
            var value = (QuarantineDirectory ?? "").Trim().Trim('/', '\\');
            if (value.Length == 0 || value is "." or ".." || value.Contains("..")) return ".octo-trash";
            return value.Split('/', '\\')[0];
        }
    }

    private static readonly Dictionary<LibraryAction, (string Name, int Rating)> Defaults = new()
    {
        [LibraryAction.Delete] = ("Delete", 1),
        [LibraryAction.WrongSong] = ("Wrong song", 2),
        [LibraryAction.WrongVersion] = ("Wrong version", 3),
        [LibraryAction.BetterQuality] = ("Better quality", 4),
    };

    /// <summary>
    /// Every action, back-filled and de-duplicated.
    ///
    /// Back-filling matters: disabling one action must not delete the names chosen for the
    /// others, and a config naming only two actions must not leave the other two undefined. A
    /// name colliding with another action's is dropped, because two playlists with the same
    /// effective title are indistinguishable to the sweep and one would apply the wrong action.
    /// </summary>
    public IReadOnlyList<LibraryActionDefinition> EffectiveActions()
    {
        var seenTitles = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
        var seenRatings = new HashSet<int>();
        var result = new List<LibraryActionDefinition>();

        foreach (var action in Enum.GetValues<LibraryAction>())
        {
            var configured = Actions?.FirstOrDefault(entry => entry.Action == action);
            var (defaultName, defaultRating) = Defaults[action];

            var name = (configured?.Name ?? "").Trim();
            if (name.Length is 0 or > 80) name = defaultName;
            if (!seenTitles.Add(EffectivePrefix + name)) continue;

            var rating = configured?.Rating ?? defaultRating;
            if (rating is < 0 or > 5 || !seenRatings.Add(rating)) rating = 0;
            // seenRatings must not collapse every unmapped action onto one another, so 0 is
            // never treated as a collision.
            if (rating == 0) seenRatings.Remove(0);

            result.Add(new LibraryActionDefinition
            {
                Action = action,
                Name = name,
                Enabled = configured?.Enabled ?? false,
                Rating = rating,
            });
        }
        return result;
    }

    public string PlaylistTitle(LibraryActionDefinition definition) => EffectivePrefix + definition.Name;

    /// <summary>The action a star count asks for, or null. 5 is deliberately unmapped by
    /// default and means "keep", so the top of the scale is never destructive.</summary>
    public LibraryActionDefinition? ActionForRating(int rating) =>
        rating is < 1 or > 5
            ? null
            : EffectiveActions().FirstOrDefault(entry => entry.Enabled && entry.Rating == rating);

    /// <summary>The star ratings that currently do something, for the dashboard to show.</summary>
    public IReadOnlyList<int> MappedRatings() =>
        EffectiveActions().Where(entry => entry.Enabled && entry.Rating is > 0)
            .Select(entry => entry.Rating!.Value).Order().ToList();

    /// <summary>
    /// Per-user gate. Ordinal-ignore-case because Navidrome usernames are. An empty list is
    /// nobody.
    /// </summary>
    public bool IsAllowed(string? username) =>
        !string.IsNullOrWhiteSpace(username)
        && (AllowedUsers ?? []).Any(user =>
            user.Trim().Equals(username.Trim(), StringComparison.OrdinalIgnoreCase));
}
