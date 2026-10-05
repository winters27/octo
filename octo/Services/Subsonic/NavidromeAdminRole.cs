using System.Text.Json;

namespace Octo.Services.Subsonic;

/// <summary>
/// Whether a Subsonic sign-in belongs to one of Navidrome's admins, asked with that sign-in. False
/// whenever Navidrome cannot say, so an outage never grants anything. Asked every time, so a role
/// taken away counts at once. The same check as SubSonicController.IsCallerAdminAsync.
/// </summary>
public static class NavidromeAdminRole
{
    public static async Task<bool> IsAdminAsync(SubsonicCredential credential, string username, SubsonicProxyService relay)
    {
        var answer = await relay.RelaySafeAsync("rest/getUser", credential.Parameters(("username", username)));
        if (!answer.Success || answer.Body is not { } body) return false;
        try
        {
            using var doc = JsonDocument.Parse(body);
            return doc.RootElement.TryGetProperty("subsonic-response", out var root)
                   && root.TryGetProperty("user", out var user)
                   && user.TryGetProperty("adminRole", out var role)
                   && role.ValueKind == JsonValueKind.True;
        }
        catch (JsonException) { return false; }
    }
}
