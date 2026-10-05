using System.Security.Cryptography;
using System.Text;

namespace Octo.Services.Admin;

/// <summary>
/// The way into the dashboard when Navidrome cannot vouch for anyone: its URL is wrong or not set
/// yet, or it is down and no browser is still signed in. A random code in a file beside the
/// settings, readable only by whoever can open the Octo host or container, which is the person who
/// owns Octo anyway. Each code works once; a new one is written as soon as it is used.
/// </summary>
public sealed class AdminRecoveryCode
{
    private const string Alphabet = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    private readonly object _lock = new();
    private readonly ILogger<AdminRecoveryCode> _logger;

    public AdminRecoveryCode(string path, ILogger<AdminRecoveryCode> logger)
    {
        FilePath = path;
        _logger = logger;
    }

    public string FilePath { get; }

    /// <summary>The code in the file, written first if there is none. Null when it cannot be written.</summary>
    public string? Current()
    {
        lock (_lock)
        {
            try
            {
                if (File.Exists(FilePath) && File.ReadAllText(FilePath).Trim() is { Length: > 0 } saved) return saved;
                return Write(New());
            }
            catch (Exception ex)
            {
                _logger.LogWarning("The recovery code could not be read or written at {Path}: {Message}", FilePath, ex.Message);
                return null;
            }
        }
    }

    /// <summary>True when the typed code is the current one, which is then replaced.</summary>
    public bool TryUse(string? typed)
    {
        lock (_lock)
        {
            if (string.IsNullOrWhiteSpace(typed) || Current() is not { } current) return false;
            var a = Encoding.UTF8.GetBytes(Normal(current));
            var b = Encoding.UTF8.GetBytes(Normal(typed));
            if (a.Length != b.Length || !CryptographicOperations.FixedTimeEquals(a, b)) return false;
            try { Write(New()); }
            catch (Exception ex) { _logger.LogWarning("A used recovery code could not be replaced: {Message}", ex.Message); }
            return true;
        }
    }

    // Typed by hand, so spaces, dashes and case do not matter.
    private static string Normal(string code) =>
        new string(code.Where(char.IsLetterOrDigit).Select(char.ToUpperInvariant).ToArray());

    private static string New()
    {
        var raw = RandomNumberGenerator.GetString(Alphabet, 20);
        return string.Join('-', Enumerable.Range(0, 4).Select(i => raw.Substring(i * 5, 5)));
    }

    private string Write(string code)
    {
        Directory.CreateDirectory(Path.GetDirectoryName(FilePath)!);
        var temp = FilePath + ".tmp";
        File.WriteAllText(temp, code + "\n");
        // Best effort: a bind mount that refuses chmod must not make the way back in disappear.
        if (!OperatingSystem.IsWindows())
        {
            try { File.SetUnixFileMode(temp, UnixFileMode.UserRead | UnixFileMode.UserWrite); }
            catch (Exception ex) { _logger.LogWarning("The recovery code file could not be made private: {Message}", ex.Message); }
        }
        File.Move(temp, FilePath, overwrite: true);
        return code;
    }
}
