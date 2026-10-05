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
/// longer an admin ends every session of theirs. Navidrome unreachable, no admin identity, or a
/// list that does not look like an admin's view never locks anyone out: Octo asks again in five
/// minutes.
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
            var jwt = await identity.EnsureAdminJwtAsync(wait.Token);
            if (string.IsNullOrEmpty(jwt)) return Role.Unknown;

            var first = await ListAsync(url, jwt, wait.Token);
            using var response = first.StatusCode == HttpStatusCode.Unauthorized
                ? await RetryWithFreshTokenAsync(first, url, jwt, wait.Token)
                : first;
            if (response is not { IsSuccessStatusCode: true }) return Role.Unknown;

            using var doc = JsonDocument.Parse(await response.Content.ReadAsStringAsync(wait.Token));
            if (doc.RootElement.ValueKind != JsonValueKind.Array) return Role.Unknown;
            var users = doc.RootElement.EnumerateArray().ToList();
            // A non-admin's view of the list is just themselves. Only an admin's view, which always
            // holds at least one admin, can say that someone is missing.
            if (!users.Any(IsAdmin)) return Role.Unknown;
            var found = users.FirstOrDefault(entry =>
                entry.TryGetProperty("userName", out var name)
                && string.Equals(name.GetString(), user, StringComparison.OrdinalIgnoreCase));
            if (found.ValueKind == JsonValueKind.Undefined) return Role.Gone;
            return IsAdmin(found) ? Role.Admin : Role.NotAdmin;
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

    // Octo's captured token expires; log in again once, as the playlist calls do.
    private async Task<HttpResponseMessage?> RetryWithFreshTokenAsync(HttpResponseMessage refused, string url, string jwt, CancellationToken ct)
    {
        refused.Dispose();
        identity.InvalidateAdminJwt(jwt);
        var fresh = await identity.EnsureAdminJwtAsync(ct);
        if (string.IsNullOrEmpty(fresh) || fresh == jwt) return null;
        return await ListAsync(url, fresh, ct);
    }
}
