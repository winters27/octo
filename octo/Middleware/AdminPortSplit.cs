namespace Octo.Middleware;

/// <summary>
/// With Admin:Port set, the dashboard and the admin API answer only on that port and everything
/// else only on Octo's main port. A proxy or a port-forward can then expose the main port for
/// music apps while the admin port stays on this machine or the home network.
/// </summary>
public static class AdminPortSplit
{
    public static IApplicationBuilder UseAdminPortSplit(this IApplicationBuilder app, int adminPort)
    {
        if (adminPort <= 0) return app;
        return app.Use(async (context, next) =>
        {
            if (Refuse(context, adminPort)) return;
            await next(context);
        });
    }

    /// <summary>True when the request was answered here: a redirect to the dashboard, or a 404.</summary>
    internal static bool Refuse(HttpContext context, int adminPort)
    {
        var path = context.Request.Path;
        var admin = path.StartsWithSegments("/admin") || path.StartsWithSegments("/api/admin");
        var onAdminPort = context.Connection.LocalPort == adminPort;
        if (onAdminPort && (!path.HasValue || path == "/"))
        {
            context.Response.Redirect("/admin/index.html");
            return true;
        }
        // The logo under /Assets is shared by both.
        var allowed = onAdminPort ? admin || path.StartsWithSegments("/Assets") : !admin;
        if (allowed) return false;
        context.Response.StatusCode = StatusCodes.Status404NotFound;
        return true;
    }

    /// <summary>
    /// The addresses to listen on when the dashboard gets a port of its own: the ones Octo already
    /// uses (ASPNETCORE_URLS as "urls", else ASPNETCORE_HTTP_PORTS as "http_ports", else Kestrel's
    /// own default) plus the admin port. Null when the admin port is off or is already a main port.
    /// </summary>
    public static string? ListenUrls(string? urls, string? httpPorts, int adminPort)
    {
        if (adminPort <= 0) return null;
        if (string.IsNullOrWhiteSpace(urls) && !string.IsNullOrWhiteSpace(httpPorts))
            urls = string.Join(';', httpPorts.Split(';', StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries)
                .Select(port => $"http://+:{port}"));
        if (string.IsNullOrWhiteSpace(urls)) urls = "http://localhost:5000";
        if (urls.Split(';').Any(url => url.Trim().TrimEnd('/').EndsWith($":{adminPort}", StringComparison.Ordinal))) return null;
        return $"{urls};http://+:{adminPort}";
    }
}
