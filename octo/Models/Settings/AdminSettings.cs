namespace Octo.Models.Settings;

/// <summary>
/// Who may use the dashboard and the admin API, and which port they answer on.
/// </summary>
public class AdminSettings
{
    /// <summary>
    /// "required" (the default): the dashboard and /api/admin need a sign-in with a Navidrome admin
    /// account, or the recovery code. "off": anyone who can reach Octo can use them, as before the
    /// sign-in existed. Only for a proxy in front that signs people in itself (Authelia, Authentik).
    /// Any other value counts as "required".
    /// Environment variable: ADMIN__SIGNIN (docker-compose maps ADMIN_SIGN_IN)
    /// </summary>
    public string SignIn { get; set; } = "required";

    /// <summary>
    /// A second port for the dashboard and the admin API alone (default 0: they share Octo's port).
    /// When set, Octo's main port answers 404 for /admin and /api/admin, and this port answers 404
    /// for everything else. Restart to apply.
    /// Environment variable: ADMIN__PORT (docker-compose maps ADMIN_PORT)
    /// </summary>
    public int Port { get; set; }

    /// <summary>True only for the exact word "off", so a typo never opens the dashboard.</summary>
    public bool SignInOff => string.Equals(SignIn?.Trim(), "off", StringComparison.OrdinalIgnoreCase);
}
