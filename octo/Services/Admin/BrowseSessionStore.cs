using System.Collections.Concurrent;
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;

namespace Octo.Services.Admin;

/// <summary>
/// Tokens proving the holder authenticated as a Navidrome admin.
///
/// The dashboard and the admin API sign in with the Navidrome that Octo already fronts rather
/// than a login system of their own, and hand back one of these. A session made with the
/// recovery code is one too, but it names RecoveryUser, lasts an hour, is never written to disk,
/// and the endpoints that touch library files do not accept it (NavidromeUserOf).
///
/// Remembered per browser, on disk, for as long as it keeps being used. These used to live
/// in memory for twelve hours, so every restart and every deploy asked for the sign-in again,
/// which made the dashboard tiresome to use (Brandon, 2026-10-03). Only a hash of each token
/// is written, so the file holds nothing a browser could present; the token itself lives only
/// in the browser's HttpOnly, SameSite=Strict cookie.
/// </summary>
public class BrowseSessionStore
{
    /// <summary>
    /// Sliding lifetime of a session: it lapses only after this long without a visit.
    /// </summary>
    public static readonly TimeSpan Ttl = TimeSpan.FromDays(90);

    /// <summary>The name a recovery-code session carries. Not a name Navidrome would give anyone.</summary>
    public const string RecoveryUser = "(recovery code)";

    /// <summary>Sliding lifetime of a recovery-code session: long enough to fix the settings.</summary>
    public static readonly TimeSpan RecoveryTtl = TimeSpan.FromHours(1);

    // A slide is written to disk at most once a day per session; the rest only moves it in memory.
    private static readonly TimeSpan SaveSlideEvery = TimeSpan.FromDays(1);

    private sealed record Session(string User, DateTime Expires, DateTime SavedExpires, TimeSpan Lifetime, bool Persist);
    private sealed record Saved(string Hash, string User, DateTime Expires);

    private readonly ConcurrentDictionary<string, Session> _sessions = new(StringComparer.Ordinal);
    private readonly string? _path;
    private readonly ILogger<BrowseSessionStore>? _logger;
    private readonly object _saveLock = new();

    internal Func<DateTime> Clock { get; set; } = () => DateTime.UtcNow;

    /// <summary>In memory only, for tests and for a host with nowhere to keep it.</summary>
    public BrowseSessionStore() : this(null) { }

    public BrowseSessionStore(string? path, ILogger<BrowseSessionStore>? logger = null)
    {
        _path = string.IsNullOrWhiteSpace(path) ? null : path;
        _logger = logger;
        try
        {
            if (_path is not null && File.Exists(_path))
                foreach (var saved in JsonSerializer.Deserialize<List<Saved>>(File.ReadAllText(_path)) ?? [])
                    if (saved.Expires > DateTime.UtcNow)
                        _sessions[saved.Hash] = new Session(saved.User, saved.Expires, saved.Expires, Ttl, true);
        }
        catch (Exception ex) { _logger?.LogWarning("browse sessions could not be read, so everyone signs in again: {M}", ex.Message); }
    }

    /// <summary>Mint a token for a verified admin. Sliding expiry starts now.</summary>
    public string Create(string username) => Mint(username, Ttl, persist: true);

    /// <summary>Mint a recovery-code session: an hour, in memory only.</summary>
    public string CreateRecovery() => Mint(RecoveryUser, RecoveryTtl, persist: false);

    private string Mint(string username, TimeSpan lifetime, bool persist)
    {
        Prune();
        var token = Convert.ToHexString(RandomNumberGenerator.GetBytes(32));
        var expires = Clock().Add(lifetime);
        _sessions[Hash(token)] = new Session(username, expires, expires, lifetime, persist);
        if (persist) Save();
        return token;
    }

    /// <summary>
    /// True when the token is live. Valid use slides the expiry, so a browser in use stays
    /// signed in while an abandoned one still lapses.
    /// </summary>
    public bool Validate(string? token) => Touch(token) is not null;

    /// <summary>
    /// The Navidrome admin a live token was minted for, or null. Slides the expiry like Validate.
    /// What the Better quality page acts as, so a file is only ever changed for a person who signed in.
    /// </summary>
    public string? UserOf(string? token) => Touch(token)?.User;

    /// <summary>
    /// Like UserOf, but null for a recovery-code session: what the endpoints that read or change
    /// library files accept, so the recovery code fixes settings and nothing else.
    /// </summary>
    public string? NavidromeUserOf(string? token) => UserOf(token) is { } user && user != RecoveryUser ? user : null;

    /// <summary>Forget a token: the browser's Sign out.</summary>
    public void Revoke(string? token)
    {
        if (string.IsNullOrWhiteSpace(token)) return;
        if (_sessions.TryRemove(Hash(token), out var gone) && gone.Persist) Save();
    }

    /// <summary>Forget every session of one person: Sign out everywhere, or Navidrome took their admin role away.</summary>
    public int RevokeUser(string user)
    {
        var gone = 0;
        foreach (var kv in _sessions)
            if (string.Equals(kv.Value.User, user, StringComparison.OrdinalIgnoreCase) && _sessions.TryRemove(kv.Key, out _))
                gone++;
        if (gone > 0) Save();
        return gone;
    }

    private Session? Touch(string? token)
    {
        if (string.IsNullOrWhiteSpace(token)) return null;
        var key = Hash(token);
        if (!_sessions.TryGetValue(key, out var session)) return null;
        var now = Clock();
        if (session.Expires <= now)
        {
            if (_sessions.TryRemove(key, out var gone) && gone.Persist) Save();
            return null;
        }
        var slid = session with { Expires = now.Add(session.Lifetime) };
        var save = session.Persist && slid.Expires - session.SavedExpires >= SaveSlideEvery;
        if (save) slid = slid with { SavedExpires = slid.Expires };
        _sessions[key] = slid;
        if (save) Save();
        return slid;
    }

    private static string Hash(string token) =>
        Convert.ToHexString(SHA256.HashData(Encoding.UTF8.GetBytes(token)));

    private void Prune()
    {
        var now = Clock();
        foreach (var kv in _sessions)
            if (kv.Value.Expires <= now)
                _sessions.TryRemove(kv.Key, out _);
    }

    private void Save()
    {
        if (_path is null) return;
        lock (_saveLock)
        {
            try
            {
                var saved = _sessions.Where(kv => kv.Value.Persist).Select(kv => new Saved(kv.Key, kv.Value.User, kv.Value.Expires)).ToList();
                Directory.CreateDirectory(Path.GetDirectoryName(_path)!);
                File.WriteAllText(_path + ".tmp", JsonSerializer.Serialize(saved));
                File.Move(_path + ".tmp", _path, overwrite: true);
            }
            catch (Exception ex) { _logger?.LogWarning("browse sessions could not be written: {M}", ex.Message); }
        }
    }
}
