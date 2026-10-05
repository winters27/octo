namespace Octo.Services.Admin;

/// <summary>
/// The dashboard sign-in's cookie. HttpOnly so the page's script cannot read it, SameSite=Strict
/// so no other site's page sends it, and scoped to /api/admin so it never goes out with the
/// Subsonic traffic Octo relays. Secure only over HTTPS: on a LAN over plain HTTP a Secure cookie
/// would simply be dropped. Its age is set again on every signed-in request, so the browser keeps
/// it exactly as long as the server keeps the session.
/// </summary>
public static class AdminCookie
{
    public const string Name = "octo_browse";
    public const string HeaderName = "X-Octo-Browse-Token";

    public static CookieOptions Options(HttpRequest request, TimeSpan? maxAge = null) => new()
    {
        HttpOnly = true,
        SameSite = SameSiteMode.Strict,
        Secure = request.IsHttps,
        Path = "/api/admin",
        MaxAge = maxAge ?? BrowseSessionStore.Ttl,
    };

    /// <summary>The cookie first (the dashboard), the header second (curl, tests).</summary>
    public static string? Token(HttpRequest request) =>
        request.Cookies[Name] is { Length: > 0 } cookie ? cookie
        : request.Headers[HeaderName] is { Count: > 0 } header ? header.ToString()
        : null;
}
