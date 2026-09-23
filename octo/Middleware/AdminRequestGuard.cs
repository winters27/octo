namespace Octo.Middleware;

/// <summary>
/// The admin API has no login, so the browser's same-origin policy is the only thing between it
/// and any web page a LAN user happens to visit. Octo's CORS policy allows every origin, which
/// Subsonic web players need, and it used to apply to /api/admin as well: a page anywhere could
/// read every stored key and password and post new settings.
///
/// Reads: CORS headers are stripped, so a browser on another origin cannot read the answer.
/// Writes: must carry X-Octo-Admin. A page on another origin cannot add a custom header without
/// a preflight, and the preflight is answered without CORS approval, so the write never leaves
/// the browser. A script (curl, Home Assistant) can send the header deliberately.
///
/// This is not authentication. Anyone who can reach the port directly, or a DNS-rebinding page
/// that makes itself look same-origin, still gets through. /admin and /api/admin must stay off
/// the internet.
/// </summary>
public static class AdminRequestGuard
{
    public const string HeaderName = "X-Octo-Admin";

    public static IApplicationBuilder UseAdminRequestGuard(this IApplicationBuilder app) =>
        app.Use(async (context, next) =>
        {
            if (!context.Request.Path.StartsWithSegments("/api/admin"))
            {
                await next(context);
                return;
            }

            // Registered ahead of UseCors, so this callback runs after CORS has added its headers
            // (OnStarting callbacks run in reverse order of registration).
            context.Response.OnStarting(() =>
            {
                foreach (var header in context.Response.Headers.Keys
                             .Where(key => key.StartsWith("Access-Control-", StringComparison.OrdinalIgnoreCase))
                             .ToList())
                    context.Response.Headers.Remove(header);
                return Task.CompletedTask;
            });

            var method = context.Request.Method;
            if (HttpMethods.IsGet(method) || HttpMethods.IsHead(method))
            {
                await next(context);
                return;
            }

            if (HttpMethods.IsOptions(method))
            {
                // A preflight answered without Access-Control-Allow-* is a refusal.
                context.Response.StatusCode = StatusCodes.Status204NoContent;
                return;
            }

            if (!context.Request.Headers.ContainsKey(HeaderName))
            {
                context.Response.StatusCode = StatusCodes.Status403Forbidden;
                await context.Response.WriteAsJsonAsync(new
                {
                    error = $"Admin changes must come from Octo's dashboard. A script can send the {HeaderName} header to opt in.",
                });
                return;
            }

            await next(context);
        });
}
