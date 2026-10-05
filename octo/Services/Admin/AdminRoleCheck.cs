using System.Collections.Concurrent;
using System.Text.Json;
using Octo.Services.Subsonic;

namespace Octo.Services.Admin;

/// <summary>
/// A dashboard sign-in lasts 90 days, so once an hour per person Octo asks Navidrome, as its own
/// admin identity, whether that person is still an admin. Gone or no longer an admin ends every
/// session of theirs. Navidrome unreachable, or no admin identity yet, never locks anyone out:
/// Octo asks again in five minutes.
/// </summary>
public sealed class AdminRoleCheck(NavidromeIdentityService identity, BrowseSessionStore sessions, ILogger<AdminRoleCheck> logger)
{
    public static readonly TimeSpan Every = TimeSpan.FromHours(1);
    private static readonly TimeSpan RetryUnknown = TimeSpan.FromMinutes(5);
    private static readonly TimeSpan AskFor = TimeSpan.FromSeconds(5);

    private readonly ConcurrentDictionary<string, DateTime> _nextAsk = new(StringComparer.OrdinalIgnoreCase);

    internal Func<DateTime> Clock { get; set; } = () => DateTime.UtcNow;

    private enum Role { Admin, NotAdmin, Gone, Unknown }

    /// <summary>False only when Navidrome said the person is gone or not an admin; their sessions are then ended.</summary>
    public async Task<bool> StillAdminAsync(string user, SubsonicProxyService relay, CancellationToken ct)
    {
        var now = Clock();
        if (_nextAsk.TryGetValue(user, out var next) && now < next) return true;
        var role = await AskAsync(user, relay, ct);
        if (role is Role.NotAdmin or Role.Gone)
        {
            _nextAsk.TryRemove(user, out _);
            var ended = sessions.RevokeUser(user);
            logger.LogWarning("{User} is {What} in Navidrome, so {Count} dashboard sign-ins of theirs were ended.",
                user, role == Role.Gone ? "gone" : "no longer an admin", ended);
            return false;
        }
        _nextAsk[user] = now + (role == Role.Admin ? Every : RetryUnknown);
        return true;
    }

    private async Task<Role> AskAsync(string user, SubsonicProxyService relay, CancellationToken ct)
    {
        if (identity.GetScanAuth() is not { } auth) return Role.Unknown;
        var parameters = new Dictionary<string, string>
        {
            ["u"] = auth.user, ["t"] = auth.token, ["s"] = auth.salt,
            ["v"] = "1.16.1", ["c"] = "octo", ["f"] = "json", ["username"] = user,
        };
        try
        {
            var answer = await relay.RelaySafeAsync("rest/getUser", parameters).WaitAsync(AskFor, ct);
            if (!answer.Success || answer.Body is not { } body) return Role.Unknown;
            using var doc = JsonDocument.Parse(body);
            if (!doc.RootElement.TryGetProperty("subsonic-response", out var root)) return Role.Unknown;
            var status = root.TryGetProperty("status", out var s) ? s.GetString() : null;
            if (status == "ok" && root.TryGetProperty("user", out var found))
                return found.TryGetProperty("adminRole", out var r) && r.ValueKind == JsonValueKind.True ? Role.Admin : Role.NotAdmin;
            // 70 is Subsonic's "not found". Anything else (40, a stale Octo identity) says nothing about this person.
            if (status == "failed" && root.TryGetProperty("error", out var error)
                && error.TryGetProperty("code", out var code) && code.TryGetInt32(out var c) && c == 70)
                return Role.Gone;
            return Role.Unknown;
        }
        catch (Exception ex) when (ex is TimeoutException or JsonException or HttpRequestException or OperationCanceledException)
        {
            return Role.Unknown;
        }
    }
}
