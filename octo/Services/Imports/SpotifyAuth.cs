using System.Collections.Concurrent;
using System.Net;
using System.Security.Cryptography;
using System.Text;

namespace Octo.Services.Imports;

/// <summary>A sign-in Spotify has not sent back yet: whose it is and the PKCE secret that finishes it.</summary>
public sealed record PendingSpotifySignIn(string User, string Verifier, string RedirectUri, DateTime ExpiresUtc);

/// <summary>
/// Spotify's authorization code flow with PKCE, the one Spotify asks apps without a safe place for
/// a client secret to use. Octo makes the code verifier, keeps it here and never sends it to the
/// browser, so a code that leaks on its way back (a pasted address, a loopback page) is worth
/// nothing to anyone else.
///
/// Spotify takes only HTTPS redirect URIs, or a loopback address written as an IP (127.0.0.1 or
/// [::1], never "localhost"), and only the ones registered on the app. A loopback one registered
/// without a port takes any port, which is how the Octo apps sign in on a port they pick.
/// </summary>
public sealed class SpotifyAuth
{
    public const string AuthorizeUrl = "https://accounts.spotify.com/authorize";

    /// <summary>Liked songs, and every playlist the account owns, follows or works on.</summary>
    public const string Scopes = "user-library-read playlist-read-private playlist-read-collaborative";

    internal static readonly TimeSpan PendingFor = TimeSpan.FromMinutes(15);

    private readonly ConcurrentDictionary<string, PendingSpotifySignIn> _pending = new(StringComparer.Ordinal);

    internal Func<DateTime> Clock { get; set; } = () => DateTime.UtcNow;

    /// <summary>The address to send someone to, and the state that brings their answer back to them.</summary>
    public (string Url, string State) Begin(string user, string clientId, string redirectUri)
    {
        Prune();
        var verifier = Verifier();
        var state = Base64Url(RandomNumberGenerator.GetBytes(18));
        _pending[state] = new PendingSpotifySignIn(user, verifier, redirectUri, Clock() + PendingFor);
        var url = $"{AuthorizeUrl}?response_type=code&client_id={Uri.EscapeDataString(clientId)}"
            + $"&scope={Uri.EscapeDataString(Scopes)}&redirect_uri={Uri.EscapeDataString(redirectUri)}"
            + $"&state={Uri.EscapeDataString(state)}&code_challenge_method=S256&code_challenge={Challenge(verifier)}";
        return (url, state);
    }

    /// <summary>The sign-in this state belongs to, once: a state answers one return only.</summary>
    public PendingSpotifySignIn? Take(string? state)
    {
        Prune();
        return state is not null && _pending.TryRemove(state, out var pending) && pending.ExpiresUtc > Clock() ? pending : null;
    }

    private void Prune()
    {
        var now = Clock();
        foreach (var (state, pending) in _pending)
            if (pending.ExpiresUtc <= now) _pending.TryRemove(state, out _);
    }

    /// <summary>A PKCE code verifier: 64 characters from the unreserved set.</summary>
    internal static string Verifier() => Base64Url(RandomNumberGenerator.GetBytes(48));

    /// <summary>S256: the verifier's SHA-256, base64url without padding.</summary>
    internal static string Challenge(string verifier) => Base64Url(SHA256.HashData(Encoding.ASCII.GetBytes(verifier)));

    private static string Base64Url(byte[] bytes) =>
        Convert.ToBase64String(bytes).TrimEnd('=').Replace('+', '-').Replace('/', '_');

    /// <summary>Why Spotify would refuse this redirect URI, in words, or null when it is one Spotify takes.</summary>
    public static string? RedirectProblem(string? redirectUri)
    {
        if (!Uri.TryCreate(redirectUri?.Trim(), UriKind.Absolute, out var uri))
            return "The redirect URI is not a full address.";
        if (uri.Host.Equals("localhost", StringComparison.OrdinalIgnoreCase))
            return "Spotify does not take localhost. Use 127.0.0.1 instead.";
        if (uri.Scheme == Uri.UriSchemeHttps) return null;
        if (uri.Scheme == Uri.UriSchemeHttp && IsLoopback(uri)) return null;
        return "Spotify takes only an https address, or http on 127.0.0.1 or [::1].";
    }

    public static bool IsLoopback(Uri uri) =>
        IPAddress.TryParse(uri.Host.Trim('[', ']'), out var address) && IPAddress.IsLoopback(address);

    /// <summary>
    /// The redirect URI an app may use: the registered one exactly, or, when the registered one is
    /// a loopback address with no port, the same address on any port. Null when it is neither,
    /// since Spotify would refuse the sign-in anyway.
    /// </summary>
    public static string? AllowedRedirect(string registered, string? asked)
    {
        if (string.IsNullOrWhiteSpace(asked)) return registered;
        if (!Uri.TryCreate(registered, UriKind.Absolute, out var mine)
            || !Uri.TryCreate(asked.Trim(), UriKind.Absolute, out var theirs)) return null;
        if (Uri.Compare(mine, theirs, UriComponents.AbsoluteUri, UriFormat.UriEscaped, StringComparison.Ordinal) == 0)
            return asked.Trim();
        var portless = IsLoopback(mine) && mine.IsDefaultPort && !HasExplicitPort(registered);
        if (!portless || !IsLoopback(theirs)) return null;
        var sameButPort = mine.Scheme == theirs.Scheme
            && mine.Host.Equals(theirs.Host, StringComparison.OrdinalIgnoreCase)
            && mine.AbsolutePath == theirs.AbsolutePath && theirs.Query.Length == 0;
        return sameButPort ? asked.Trim() : null;
    }

    private static bool HasExplicitPort(string uri)
    {
        var afterScheme = uri.IndexOf("://", StringComparison.Ordinal);
        var authority = afterScheme < 0 ? uri : uri[(afterScheme + 3)..];
        var end = authority.IndexOfAny(['/', '?', '#']);
        if (end >= 0) authority = authority[..end];
        // [::1]:8080 has its port after the bracket; 127.0.0.1:8080 after the only colon.
        var close = authority.LastIndexOf(']');
        return authority.IndexOf(':', close + 1) >= 0;
    }

    /// <summary>
    /// The code, state and any error from the address Spotify sent the browser to, whether someone
    /// pasted all of it, only its query, or the code by itself.
    /// </summary>
    public static (string? Code, string? State, string? Error) ParseReturn(string? pasted)
    {
        var text = pasted?.Trim() ?? "";
        if (text.Length == 0) return (null, null, null);
        var query = text;
        var mark = text.IndexOf('?');
        if (mark >= 0) query = text[(mark + 1)..];
        else if (!text.Contains('=')) return (text, null, null);
        var hash = query.IndexOf('#');
        if (hash >= 0) query = query[..hash];
        string? code = null, state = null, error = null;
        foreach (var part in query.Split('&', StringSplitOptions.RemoveEmptyEntries))
        {
            var eq = part.IndexOf('=');
            var name = eq < 0 ? part : part[..eq];
            var value = eq < 0 ? "" : Uri.UnescapeDataString(part[(eq + 1)..].Replace('+', ' '));
            switch (name)
            {
                case "code": code = value; break;
                case "state": state = value; break;
                case "error": error = value; break;
            }
        }
        return (code, state, error);
    }
}
