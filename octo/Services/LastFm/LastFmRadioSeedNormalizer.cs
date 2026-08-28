using System.Text.RegularExpressions;

namespace Octo.Services.LastFm;

/// <summary>Canonicalizes artist/title seeds for every Last.fm radio path.</summary>
public static partial class LastFmRadioSeedNormalizer
{
    private static readonly string[] ArtistSeparators =
    [
        " • ", " · ", " & ", " feat. ", " feat ", " ft. ", " ft ",
        " x ", " X ", " / ", ", ", " with "
    ];

    public static string? Artist(string? artist)
    {
        if (string.IsNullOrWhiteSpace(artist)) return artist;
        var normalized = artist;
        foreach (var separator in ArtistSeparators)
        {
            var index = normalized.IndexOf(separator, StringComparison.OrdinalIgnoreCase);
            if (index > 0) normalized = normalized[..index];
        }
        return normalized.Trim();
    }

    public static string? Title(string? title)
    {
        if (string.IsNullOrWhiteSpace(title)) return title;
        return FeaturedArtistSuffix().Replace(title, "").Trim();
    }

    public static string TrackKey(string? artist, string? title) =>
        $"{Artist(artist)?.Trim().ToLowerInvariant()}|{Title(title)?.Trim().ToLowerInvariant()}";

    [GeneratedRegex(@"\s*[\(\[](?:feat\.?|featuring|with|ft\.?)\s*[^\)\]]*[\)\]]\s*",
        RegexOptions.IgnoreCase | RegexOptions.CultureInvariant)]
    private static partial Regex FeaturedArtistSuffix();
}
