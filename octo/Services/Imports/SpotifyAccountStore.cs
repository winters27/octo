using System.Text.Json;

namespace Octo.Services.Imports;

/// <summary>One person's Spotify sign-in. The tokens never leave Octo.</summary>
public sealed class SpotifyAccount
{
    public string AccessToken { get; set; } = "";
    public string RefreshToken { get; set; } = "";
    public DateTime ExpiresUtc { get; set; }
    /// <summary>When they allowed Octo. Spotify ends a sign-in six months after this, however often it is renewed.</summary>
    public DateTime ConnectedUtc { get; set; }
    public string? SpotifyId { get; set; }
    public string? DisplayName { get; set; }
    /// <summary>Why the sign-in no longer works, in words, or null while it does.</summary>
    public string? Problem { get; set; }

    public SpotifyAccount Copy() => (SpotifyAccount)MemberwiseClone();
}

/// <summary>
/// Spotify sign-ins by Navidrome user, in their own file beside settings.json rather than in it:
/// the access token changes every hour, settings.json is watched and shown on the dashboard, and
/// these must never be. Written whole each time, through a temporary file.
/// </summary>
public sealed class SpotifyAccountStore
{
    private readonly string? _path;
    private readonly ILogger<SpotifyAccountStore>? _logger;
    private readonly object _lock = new();
    private readonly Dictionary<string, SpotifyAccount> _accounts = new(StringComparer.OrdinalIgnoreCase);

    public SpotifyAccountStore(string? path = null, ILogger<SpotifyAccountStore>? logger = null)
    {
        _path = string.IsNullOrWhiteSpace(path) ? null : path;
        _logger = logger;
        try
        {
            if (_path is not null && File.Exists(_path))
                foreach (var (user, account) in JsonSerializer.Deserialize<Dictionary<string, SpotifyAccount>>(File.ReadAllText(_path)) ?? [])
                    _accounts[user] = account;
        }
        catch (Exception ex) { _logger?.LogWarning("the Spotify sign-ins could not be read: {M}", ex.Message); }
    }

    public SpotifyAccount? Get(string user)
    {
        lock (_lock) return _accounts.TryGetValue(user, out var account) ? account.Copy() : null;
    }

    public void Set(string user, SpotifyAccount account)
    {
        lock (_lock)
        {
            _accounts[user] = account.Copy();
            Write();
        }
    }

    public void Update(string user, Action<SpotifyAccount> change)
    {
        lock (_lock)
        {
            if (!_accounts.TryGetValue(user, out var account)) return;
            change(account);
            Write();
        }
    }

    public bool Remove(string user)
    {
        lock (_lock)
        {
            if (!_accounts.Remove(user)) return false;
            Write();
            return true;
        }
    }

    public IReadOnlyList<string> Users
    {
        get { lock (_lock) return _accounts.Keys.ToList(); }
    }

    // Called with the lock held.
    private void Write()
    {
        if (_path is null) return;
        try
        {
            Directory.CreateDirectory(Path.GetDirectoryName(_path)!);
            File.WriteAllText(_path + ".tmp", JsonSerializer.Serialize(_accounts));
            // Only Octo reads it. Where the platform lets us say so, nobody else may.
            if (!OperatingSystem.IsWindows())
                File.SetUnixFileMode(_path + ".tmp", UnixFileMode.UserRead | UnixFileMode.UserWrite);
            File.Move(_path + ".tmp", _path, overwrite: true);
        }
        catch (Exception ex) { _logger?.LogWarning("the Spotify sign-ins could not be written: {M}", ex.Message); }
    }
}
