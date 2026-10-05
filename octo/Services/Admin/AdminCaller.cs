namespace Octo.Services.Admin;

/// <summary>How a caller got past the sign-in gate.</summary>
public enum AdminCallerKind
{
    /// <summary>A browser or script signed in with a Navidrome admin account.</summary>
    Dashboard,
    /// <summary>Signed in with the recovery code: settings yes, library files no.</summary>
    Recovery,
    /// <summary>The Octo app with a Navidrome admin's own Subsonic sign-in.</summary>
    NavidromeAdmin,
    /// <summary>The Octo app with a listener's Subsonic sign-in: their own stations only.</summary>
    Listener,
    /// <summary>ADMIN_SIGN_IN is off, so nobody was asked.</summary>
    SignInOff,
}

/// <summary>Who the gate let in, kept on the request for the endpoints that answer differently.</summary>
public sealed record AdminCaller(string? User, AdminCallerKind Kind)
{
    public const string ItemKey = "Octo.AdminCaller";

    /// <summary>Everyone but a listener may see and change everything the dashboard shows.</summary>
    public bool IsAdmin => Kind is not AdminCallerKind.Listener;

    public static AdminCaller? Of(HttpContext context) =>
        context.Items.TryGetValue(ItemKey, out var value) ? value as AdminCaller : null;

    /// <summary>
    /// Who made a change, in words for the log: the audit trail of who saved the settings or
    /// restarted Octo.
    /// </summary>
    public static string Describe(HttpContext context) => Of(context) switch
    {
        { Kind: AdminCallerKind.Recovery } => "someone with the recovery code",
        { Kind: AdminCallerKind.SignInOff } => "someone (the dashboard sign-in is off)",
        { Kind: AdminCallerKind.NavidromeAdmin, User: { } user } => $"{user} (from the Octo app)",
        { User: { } user } => user,
        _ => "someone",
    };
}
