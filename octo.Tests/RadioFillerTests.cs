using Octo.Services.LastFm;

namespace Octo.Tests;

public class RadioFillerTests
{
    [Theory]
    [InlineData("Song", 30, true)]
    [InlineData("Song", 3000, true)]
    [InlineData("Song", 200, false)]
    [InlineData("Song", null, false)]
    [InlineData("Intro", 60, true)]
    [InlineData("Interlude No. 2", 90, true)]
    [InlineData("Intro", 300, false)]
    [InlineData("Introducing the Band", 180, false)]
    [InlineData("Intro", null, false)]
    [InlineData("Interview with the Band", 600, true)]
    [InlineData("Skité", 60, false)]
    [InlineData("Café Intro", 60, true)]
    public void IsFiller_LeavesOutShortPiecesTalkAndOddLengths(string title, int? seconds, bool filler) =>
        Assert.Equal(filler, RadioFiller.IsFiller(title, seconds));
}
