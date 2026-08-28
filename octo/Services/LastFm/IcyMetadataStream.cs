using System.Text;
using Octo.Models.Radio;

namespace Octo.Services.LastFm;

/// <summary>
/// Adds ICY framing around Octo's existing station-track metadata. This stream
/// owns no discovery or artwork logic; it only transports the artist/title that
/// the radio snapshot already selected.
/// </summary>
internal sealed class IcyMetadataStream(Stream inner, int interval = IcyMetadataStream.DefaultInterval)
    : Stream
{
    public const int DefaultInterval = 16 * 1024;
    private const int MaximumMetadataBytes = byte.MaxValue * 16;
    private readonly Stream _inner = inner;
    private readonly int _interval = interval > 0
        ? interval : throw new ArgumentOutOfRangeException(nameof(interval));
    private int _audioBytesUntilMetadata = interval;
    private byte[] _metadataBlock = [0];

    public void SetTrack(LastFmRadioTrack track) =>
        _metadataBlock = Encode(track.Artist, track.Title);

    public override async ValueTask WriteAsync(ReadOnlyMemory<byte> buffer,
        CancellationToken cancellationToken = default)
    {
        while (!buffer.IsEmpty)
        {
            var count = Math.Min(buffer.Length, _audioBytesUntilMetadata);
            await _inner.WriteAsync(buffer[..count], cancellationToken);
            buffer = buffer[count..];
            _audioBytesUntilMetadata -= count;
            if (_audioBytesUntilMetadata != 0) continue;

            await _inner.WriteAsync(_metadataBlock, cancellationToken);
            _audioBytesUntilMetadata = _interval;
        }
    }

    public override Task WriteAsync(byte[] buffer, int offset, int count,
        CancellationToken cancellationToken) =>
        WriteAsync(buffer.AsMemory(offset, count), cancellationToken).AsTask();

    public override void Write(byte[] buffer, int offset, int count)
    {
        var remaining = buffer.AsSpan(offset, count);
        while (!remaining.IsEmpty)
        {
            var writeCount = Math.Min(remaining.Length, _audioBytesUntilMetadata);
            _inner.Write(remaining[..writeCount]);
            remaining = remaining[writeCount..];
            _audioBytesUntilMetadata -= writeCount;
            if (_audioBytesUntilMetadata != 0) continue;

            _inner.Write(_metadataBlock);
            _audioBytesUntilMetadata = _interval;
        }
    }

    private static byte[] Encode(string artist, string title)
    {
        var streamTitle = string.Join(" - ", new[] { artist, title }
            .Where(value => !string.IsNullOrWhiteSpace(value)));
        streamTitle = new string(streamTitle.Where(character => !char.IsControl(character)).ToArray())
            .Replace('\'', '’');
        var payload = Encoding.UTF8.GetBytes($"StreamTitle='{streamTitle}';");
        var payloadLength = Math.Min(payload.Length, MaximumMetadataBytes);
        while (payloadLength > 0 && payloadLength < payload.Length
            && (payload[payloadLength] & 0xc0) == 0x80)
            payloadLength--;

        var blockCount = (payloadLength + 15) / 16;
        var block = new byte[1 + blockCount * 16];
        block[0] = checked((byte)blockCount);
        payload.AsSpan(0, payloadLength).CopyTo(block.AsSpan(1));
        return block;
    }

    public override void Flush() => _inner.Flush();
    public override Task FlushAsync(CancellationToken cancellationToken) =>
        _inner.FlushAsync(cancellationToken);
    public override bool CanRead => false;
    public override bool CanSeek => false;
    public override bool CanWrite => true;
    public override long Length => throw new NotSupportedException();
    public override long Position
    {
        get => throw new NotSupportedException();
        set => throw new NotSupportedException();
    }
    public override int Read(byte[] buffer, int offset, int count) => throw new NotSupportedException();
    public override long Seek(long offset, SeekOrigin origin) => throw new NotSupportedException();
    public override void SetLength(long value) => throw new NotSupportedException();
}
