namespace Octo.Services.Admin;

/// <summary>An admin endpoint that answers without a sign-in. AdminSignInTests lists every one.</summary>
[AttributeUsage(AttributeTargets.Method)]
public sealed class AdminOpenAttribute : Attribute;

/// <summary>An admin endpoint the Octo app may call with its own Subsonic sign-in.</summary>
[AttributeUsage(AttributeTargets.Method)]
public sealed class AdminAppCallAttribute(bool adminOnly) : Attribute
{
    /// <summary>True: only a Navidrome admin's sign-in. False: any listener, scoped to themselves.</summary>
    public bool AdminOnly { get; } = adminOnly;
}
