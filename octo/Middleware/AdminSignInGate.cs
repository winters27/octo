using Microsoft.Extensions.Options;
using Octo.Models.Settings;
using Octo.Services.Admin;
using Octo.Services.Subsonic;

namespace Octo.Middleware;

/// <summary>
/// Nothing under /api/admin answers without a sign-in, apart from the endpoints marked
/// [AdminOpen]. The dashboard signs in with a Navidrome admin account (browse/auth) or the
/// recovery code; a script does the same with a cookie jar; the Octo app signs the calls marked
/// [AdminAppCall] with its own Subsonic sign-in. One gate in front of every controller, so an
/// endpoint added later is signed in without anyone remembering to.
///
/// Runs after route matching (Octo never calls UseRouting, so WebApplication matches first) and
/// after AdminRequestGuard: that stops other websites, this stops people. Both are needed: a page
/// on a sibling subdomain or another port of the same host is same-site, so the browser sends it
/// the cookie, and only the guard's header rule stops its writes.
/// </summary>
public static class AdminSignInGate
{
    public static IApplicationBuilder UseAdminSignInGate(this IApplicationBuilder app) =>
        app.Use(async (context, next) =>
        {
            if (!context.Request.Path.StartsWithSegments("/api/admin"))
            {
                await next(context);
                return;
            }

            var services = context.RequestServices;
            if (services.GetRequiredService<IOptionsMonitor<AdminSettings>>().CurrentValue.SignInOff)
            {
                context.Items[AdminCaller.ItemKey] = new AdminCaller(null, AdminCallerKind.SignInOff);
                await next(context);
                return;
            }

            var endpoint = context.GetEndpoint();
            var open = endpoint?.Metadata.GetMetadata<AdminOpenAttribute>() is not null;
            var appCall = endpoint?.Metadata.GetMetadata<AdminAppCallAttribute>();

            var sessions = services.GetRequiredService<BrowseSessionStore>();
            if (sessions.UserOf(AdminCookie.Token(context.Request)) is { } user)
            {
                var recovery = user == BrowseSessionStore.RecoveryUser;
                if (!recovery && !await services.GetRequiredService<AdminRoleCheck>()
                        .StillAdminAsync(user, services.GetRequiredService<SubsonicProxyService>(), context.RequestAborted))
                {
                    if (open)
                    {
                        await next(context);
                        return;
                    }
                    await SignInRequired(context);
                    return;
                }
                context.Items[AdminCaller.ItemKey] = new AdminCaller(user, recovery ? AdminCallerKind.Recovery : AdminCallerKind.Dashboard);
                if (context.Request.Cookies[AdminCookie.Name] is { Length: > 0 } cookie)
                    context.Response.Cookies.Append(AdminCookie.Name, cookie,
                        AdminCookie.Options(context.Request, recovery ? BrowseSessionStore.RecoveryTtl : null));
                await next(context);
                return;
            }

            if (appCall is not null)
            {
                var parameters = context.Request.Query.ToDictionary(pair => pair.Key, pair => pair.Value.ToString(), StringComparer.Ordinal);
                var throttle = services.GetRequiredService<SignInThrottle>();
                var tryKey = SignInThrottle.KeyOf(context);
                if (SubsonicCredential.From(parameters) is { } credential && throttle.RetryAfter(tryKey) is null)
                {
                    var relay = services.GetRequiredService<SubsonicProxyService>();
                    var verdict = await services.GetRequiredService<CredentialCheck>().CheckAsync(credential, relay, context.RequestAborted);
                    if (verdict == CredentialVerdict.Refused) throttle.Failed(tryKey);
                    if (verdict == CredentialVerdict.Accepted
                        && await services.GetRequiredService<RequestIdentity>().UsernameAsync(parameters, relay, context.RequestAborted) is { } name)
                    {
                        if (await NavidromeAdminRole.IsAdminAsync(credential, name, relay))
                        {
                            context.Items[AdminCaller.ItemKey] = new AdminCaller(name, AdminCallerKind.NavidromeAdmin);
                            await next(context);
                            return;
                        }
                        if (!appCall.AdminOnly)
                        {
                            context.Items[AdminCaller.ItemKey] = new AdminCaller(name, AdminCallerKind.Listener);
                            await next(context);
                            return;
                        }
                        if (!open)
                        {
                            context.Response.StatusCode = StatusCodes.Status403Forbidden;
                            await context.Response.WriteAsJsonAsync(new { error = "That needs a Navidrome admin account." });
                            return;
                        }
                    }
                }
            }

            if (open)
            {
                await next(context);
                return;
            }
            await SignInRequired(context);
        });

    private static Task SignInRequired(HttpContext context)
    {
        context.Response.StatusCode = StatusCodes.Status401Unauthorized;
        return context.Response.WriteAsJsonAsync(new
        {
            error = "Sign in to Octo's dashboard with a Navidrome admin account.",
            signIn = true,
        });
    }

    /// <summary>
    /// The dashboard may be framed only by Octo itself, nothing sniffs its answers into another type,
    /// no address leaks into a referrer, and admin answers (settings among them) are never kept by a
    /// browser or proxy cache unless the endpoint says otherwise (cover thumbnails do).
    /// </summary>
    public static IApplicationBuilder UseAdminSecurityHeaders(this IApplicationBuilder app) =>
        app.Use(async (context, next) =>
        {
            var path = context.Request.Path;
            var api = path.StartsWithSegments("/api/admin");
            if (api || path.StartsWithSegments("/admin"))
            {
                context.Response.OnStarting(() =>
                {
                    var headers = context.Response.Headers;
                    headers["X-Frame-Options"] = "SAMEORIGIN";
                    headers["Content-Security-Policy"] = "frame-ancestors 'self'";
                    headers["X-Content-Type-Options"] = "nosniff";
                    headers["Referrer-Policy"] = "same-origin";
                    if (api && !headers.ContainsKey("Cache-Control")) headers["Cache-Control"] = "no-store";
                    return Task.CompletedTask;
                });
            }
            await next(context);
        });
}
