using System.Collections.Concurrent;
using System.Net;
using System.Text.Json;
using Microsoft.Extensions.Options;
using Octo.Models.Settings;
using Octo.Services.Subsonic;

namespace Octo.Services.Admin;

/// <summary>
/// A dashboard sign-in lasts 90 days, so once an hour per person Octo reads Navidrome's user list,
/// as its own admin identity, and checks that person is still there and still an admin. Gone or no
/// longer an admin ends every session of theirs. Navidrome unreachable, or no admin login that can
/// read the whole list, never locks anyone out: Octo asks again in five minutes.
///
/// Octo's token is whichever admin login it saw last, so it may be the demoted person's own. Then
/// Navidrome shows it only that person, marked not an admin, which is answer enough; for anyone
/// else Octo drops the token and asks again with the admin login in its settings.
///
/// The list, not Subsonic getUser: Navidrome answers getUser only for the caller's own name, and an
/// admin asking about anyone else gets error 50, so getUser can never say someone was demoted.
/// </summary>
public sealed class AdminRoleCheck(
    NavidromeIdentityService identity,
    BrowseSessionStore sessions,
    IHttpClientFactory httpFactory,
    IOptionsMonitor<SubsonicSettings> subsonic,
    ILogger<AdminRoleCheck> logger)
{
    public static readonly TimeSpan Every = TimeSpan.FromHours(1);
    private static readonly TimeSpan RetryUnknown = TimeSpan.FromMinutes(5);
    private static readonly TimeSpan AskFor = TimeSpan.FromSeconds(5);

    private readonly ConcurrentDictionary<string, DateTime> _nextAsk = new(StringComparer.OrdinalIgnoreCase);

    internal Func<DateTime> Clock { get; set; } = () => DateTime.UtcNow;

    private enum Role { Admin, NotAdmin, Gone, Unknown }

    /// <summary>False only when Navidrome said the person is gone or not an admin; their sessions are then ended.</summary>
    public async Task<bool> StillAdminAsync(string user, CancellationToken ct)
    {
        var now = Clock();
        if (_nextAsk.TryGetValue(user, out var next) && now < next) return true;
        var role = await AskAsync(user, ct);
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

    private async Task<Role> AskAsync(string user, CancellationToken ct)
    {
        var url = subsonic.CurrentValue.Url?.TrimEnd('/');
        if (string.IsNullOrEmpty(url)) return Role.Unknown;
        try
        {
            using var wait = CancellationTokenSource.CreateLinkedTokenSource(ct);
            wait.CancelAfter(AskFor);
            // Twice at most: Octo's token is whichever admin login it saw last, so it can expire
            // (a 401) or belong to someone Navidrome has since demoted (a non-admin's view). Either
            // way it is dropped, and the second try logs in with the admin login in the settings.
            for (var attempt = 0; attempt < 2; attempt++)
            {
                var jwt = await identity.EnsureAdminJwtAsync(wait.Token);
                if (string.IsNullOrEmpty(jwt)) return Role.Unknown;

                using var response = await ListAsync(url, jwt, wait.Token);
                if (response.StatusCode == HttpStatusCode.Unauthorized)
                {
                    identity.InvalidateAdminJwt(jwt);
                    continue;
                }
                if (!response.IsSuccessStatusCode) return Role.Unknown;

                using var doc = JsonDocument.Parse(await response.Content.ReadAsStringAsync(wait.Token));
                if (doc.RootElement.ValueKind != JsonValueKind.Array) return Role.Unknown;
                var users = doc.RootElement.EnumerateArray().ToList();
                var found = users.FirstOrDefault(entry =>
                    entry.TryGetProperty("userName", out var name)
                    && string.Equals(name.GetString(), user, StringComparison.OrdinalIgnoreCase));

                if (users.Any(IsAdmin))
                {
                    if (found.ValueKind == JsonValueKind.Undefined) return Role.Gone;
                    return IsAdmin(found) ? Role.Admin : Role.NotAdmin;
                }

                // A non-admin's view: Navidrome shows a non-admin only themselves, so the token is
                // no longer an admin's. If it is this person's own, it already says they are not
                // an admin. Otherwise it cannot say anything about anyone else.
                identity.InvalidateAdminJwt(jwt);
                if (found.ValueKind != JsonValueKind.Undefined && !IsAdmin(found)) return Role.NotAdmin;
            }
            return Role.Unknown;
        }
        catch (Exception ex) when (ex is JsonException or HttpRequestException or OperationCanceledException)
        {
            return Role.Unknown;
        }
    }

    private static bool IsAdmin(JsonElement entry) =>
        entry.TryGetProperty("isAdmin", out var admin) && admin.ValueKind == JsonValueKind.True;

    private async Task<HttpResponseMessage> ListAsync(string url, string jwt, CancellationToken ct)
    {
        using var request = new HttpRequestMessage(HttpMethod.Get, $"{url}/api/user");
        request.Headers.TryAddWithoutValidation("X-Nd-Authorization", $"Bearer {jwt}");
        return await httpFactory.CreateClient().SendAsync(request, ct);
    }
}
