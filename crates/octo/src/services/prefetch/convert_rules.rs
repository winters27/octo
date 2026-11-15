//! What a client asks of an outside song's audio, what a source holds, and what to answer: the
//! source as it is, its Opus copied into Ogg, or a conversion. Pure rules with no files,
//! processes or clocks, so every answer is a function of its inputs.

use octo_subsonic::Parameters;

/// The lowest and highest bit rates a conversion is made at, in kbps.
pub const MIN_KBPS: u32 = 32;
pub const MAX_KBPS: u32 = 320;

/// The User-Agent marks of Octo's phone apps (`Octo/<version> (Android)`, `Octo/<version> (iPhone)`).
pub const GATE_PHONE_UA: [&str; 2] = ["(Android)", "(iPhone)"];

/// A format a conversion is made in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Codec {
    Mp3,
    Opus,
    Aac,
}

impl Codec {
    /// The file ending of a conversion in this format.
    pub fn ext(self) -> &'static str {
        match self {
            Codec::Mp3 => "mp3",
            Codec::Opus => "opus",
            Codec::Aac => "aac",
        }
    }

    /// The type a conversion in this format is served with: MP3, Opus in Ogg, AAC in ADTS.
    pub fn content_type(self) -> &'static str {
        match self {
            Codec::Mp3 => "audio/mpeg",
            Codec::Opus => "audio/ogg",
            Codec::Aac => "audio/aac",
        }
    }

    /// The bit rate a conversion is made at when the client names none (Navidrome's own).
    pub fn default_kbps(self) -> u32 {
        match self {
            Codec::Mp3 => 192,
            Codec::Opus => 128,
            Codec::Aac => 256,
        }
    }

    /// The `format` word a client asks for it with.
    pub fn param(self) -> &'static str {
        match self {
            Codec::Mp3 => "mp3",
            Codec::Opus => "opus",
            Codec::Aac => "aac",
        }
    }

    /// The ffmpeg encoder that makes it.
    pub fn encoder(self) -> &'static str {
        match self {
            Codec::Mp3 => "libmp3lame",
            Codec::Opus => "libopus",
            Codec::Aac => "aac",
        }
    }

    /// The format a `format` word names, when it is one a conversion is made in.
    pub fn named(word: &str) -> Option<Codec> {
        match word.trim().to_ascii_lowercase().as_str() {
            "mp3" => Some(Codec::Mp3),
            "opus" => Some(Codec::Opus),
            "aac" => Some(Codec::Aac),
            _ => None,
        }
    }
}

/// A conversion: its format and bit rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Target {
    pub codec: Codec,
    pub kbps: u32,
}

impl Target {
    /// Its part of a converted copy's name: `mp3-128`, `opus-96`, `aac-256`.
    pub fn name(&self) -> String {
        format!("{}-{}", self.codec.param(), self.kbps)
    }
}

/// Which Octo app, if any, a request comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateClient {
    OctoDesktop,
    OctoPhone,
    Other,
}

impl GateClient {
    /// The app a request comes from, by its `c` and its User-Agent. Octo's phone apps name
    /// their platform (`Octo/<version> (Android)`), the desktop app says `Octo` (its player) or
    /// `Octo/<version> (<system>)`, and anything else is another client: a speaker fetching an
    /// Octo app's address carries `c=Octo` with its own User-Agent.
    pub fn of(client: &str, user_agent: &str) -> GateClient {
        if !client.trim().eq_ignore_ascii_case("octo") {
            return GateClient::Other;
        }
        let agent = user_agent.trim();
        let octo = agent.eq_ignore_ascii_case("octo")
            || agent
                .get(..5)
                .is_some_and(|start| start.eq_ignore_ascii_case("octo/"));
        if !octo {
            return GateClient::Other;
        }
        if GATE_PHONE_UA.iter().any(|mark| agent.contains(mark)) {
            GateClient::OctoPhone
        } else {
            GateClient::OctoDesktop
        }
    }
}

/// What the client itself asked, as the family gate read it before setting the caller's
/// quality, and the ceiling it applied. A request the gate never saw has none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnAsk {
    pub format: Option<String>,
    pub rate: Option<u32>,
    pub ceiling: Option<u32>,
    pub client: GateClient,
}

/// What a request asks of an outside song's audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ask {
    /// The file as it is.
    None,
    /// This format at this bit rate or less.
    Format(Target),
    /// Any format at this bit rate or less; a conversion is made in Opus when `prefer_opus`
    /// (Octo's desktop app), else in MP3.
    Cap { kbps: u32, prefer_opus: bool },
}

/// The ask, where in the song it starts (`timeOffset`, whole seconds), and whether the client
/// needs the length up front (`estimateContentLength=true`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AskOf {
    pub ask: Ask,
    pub offset: u32,
    pub exact: bool,
}

fn clamp_kbps(kbps: u32) -> u32 {
    kbps.clamp(MIN_KBPS, MAX_KBPS)
}

/// A parameter by name, any case, as the family gate reads them.
fn parameter<'a>(parameters: &'a Parameters, name: &str) -> Option<&'a str> {
    parameters
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// The parameter a bit rate comes in: `bitrate` on `download`, `maxBitRate` everywhere else.
pub fn rate_key(endpoint: &str) -> &'static str {
    let bare = endpoint
        .trim()
        .rsplit('/')
        .next()
        .unwrap_or("")
        .trim_end_matches(".view");
    if bare.eq_ignore_ascii_case("download") {
        "bitrate"
    } else {
        "maxBitRate"
    }
}

/// What a request asks, from the client's own values: the gate's record of them when there
/// is one, else the parameters. A bit rate of 0 is "no limit". A ceiling lowers an asked
/// format's rate, and turns any other ask (the file as it is, a lossless format, an unknown
/// word) into a cap, so a ceiling is never stepped around.
pub fn ask_of(parameters: &Parameters, endpoint: &str, own: Option<&OwnAsk>, client: GateClient) -> AskOf {
    let (format, rate, ceiling, client) = match own {
        Some(own) => (own.format.clone(), own.rate, own.ceiling, own.client),
        None => (
            parameter(parameters, "format").map(str::to_string),
            parameter(parameters, rate_key(endpoint)).and_then(|value| value.trim().parse::<u32>().ok()),
            None,
            client,
        ),
    };
    let format = format
        .map(|word| word.trim().to_ascii_lowercase())
        .unwrap_or_default();
    let own_rate = rate.filter(|rate| *rate > 0);
    let ceiling = ceiling.filter(|ceiling| *ceiling > 0);
    let ask = if let Some(codec) = Codec::named(&format) {
        let kbps = own_rate.unwrap_or(codec.default_kbps());
        let kbps = ceiling.map_or(kbps, |ceiling| kbps.min(ceiling));
        Ask::Format(Target {
            codec,
            kbps: clamp_kbps(kbps),
        })
    } else if let Some(ceiling) = ceiling {
        let kbps = own_rate.map_or(ceiling, |rate| rate.min(ceiling));
        Ask::Cap {
            kbps: clamp_kbps(kbps),
            prefer_opus: client == GateClient::OctoDesktop,
        }
    } else if format.is_empty()
        && let Some(rate) = own_rate
    {
        Ask::Cap {
            kbps: clamp_kbps(rate),
            prefer_opus: false,
        }
    } else {
        Ask::None
    };
    let offset = parameter(parameters, "timeOffset")
        .and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
        .map_or(0, |seconds| seconds.floor().min(f64::from(u32::MAX)) as u32);
    let exact = parameter(parameters, "estimateContentLength")
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("true"));
    AskOf { ask, offset, exact }
}

/// The audio a source holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceCodec {
    Mp3,
    Aac,
    Opus,
    Vorbis,
    Lossless,
    Unknown,
}

/// The container a source holds its audio in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    Mpeg,
    Mp4,
    Adts,
    Ogg,
    Webm,
    Flac,
    Other,
}

/// A source's audio: what it is, in what, and its bit rate in kbps when known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceAudio {
    pub codec: SourceCodec,
    pub container: Container,
    pub kbps: Option<u32>,
}

/// A stream or a copy of one, by the type it is served with (YouTube sends Opus, never Vorbis,
/// in its Ogg and WebM).
pub fn source_of_type(content_type: &str, kbps: Option<u32>) -> SourceAudio {
    let bare = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let (codec, container) = match bare.as_str() {
        "audio/mp4" | "audio/x-m4a" | "audio/m4a" => (SourceCodec::Aac, Container::Mp4),
        "audio/webm" => (SourceCodec::Opus, Container::Webm),
        "audio/ogg" | "audio/opus" | "application/ogg" => (SourceCodec::Opus, Container::Ogg),
        "audio/mpeg" | "audio/mp3" => (SourceCodec::Mp3, Container::Mpeg),
        "audio/aac" | "audio/aacp" | "audio/x-aac" => (SourceCodec::Aac, Container::Adts),
        "audio/flac" | "audio/x-flac" => (SourceCodec::Lossless, Container::Flac),
        "audio/wav" | "audio/x-wav" | "audio/wave" | "audio/aiff" | "audio/x-aiff" => {
            (SourceCodec::Lossless, Container::Other)
        }
        _ => (SourceCodec::Unknown, Container::Other),
    };
    let kbps = match codec {
        SourceCodec::Lossless | SourceCodec::Unknown => None,
        _ => kbps.filter(|kbps| *kbps > 0),
    };
    SourceAudio {
        codec,
        container,
        kbps,
    }
}

/// Where the audio of a file starts, past an ID3v2 tag when it has one.
fn past_id3(head: &[u8]) -> usize {
    if head.len() < 10 || &head[..3] != b"ID3" {
        return 0;
    }
    let size = head[6..10]
        .iter()
        .fold(0usize, |size, byte| (size << 7) | usize::from(byte & 0x7F));
    let footer = if head[5] & 0x10 != 0 { 10 } else { 0 };
    10 + size + footer
}

/// A file's audio by its first bytes. An MP4 is read as AAC here; an ALAC one is told apart
/// by its headers (`converted_copies::describe_file`).
pub fn sniff_file(head: &[u8]) -> (SourceCodec, Container) {
    let start = past_id3(head);
    if start > 0 && start + 4 > head.len() {
        // A tag longer than what was read: an ID3 tag fronts MP3 audio.
        return (SourceCodec::Mp3, Container::Mpeg);
    }
    let bytes = &head[start.min(head.len())..];
    if bytes.starts_with(b"fLaC") {
        return (SourceCodec::Lossless, Container::Flac);
    }
    if bytes.len() >= 8 && &bytes[4..8] == b"ftyp" {
        return (SourceCodec::Aac, Container::Mp4);
    }
    if bytes.starts_with(b"OggS") {
        let segments = bytes.get(26).map_or(0, |count| usize::from(*count));
        let packet = bytes.get(27 + segments..).unwrap_or(&[]);
        let codec = if packet.starts_with(b"OpusHead") {
            SourceCodec::Opus
        } else if packet.starts_with(b"\x01vorbis") {
            SourceCodec::Vorbis
        } else if packet.starts_with(b"\x7fFLAC") {
            SourceCodec::Lossless
        } else {
            SourceCodec::Unknown
        };
        return (codec, Container::Ogg);
    }
    if bytes.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        let codec = if contains(bytes, b"A_OPUS") {
            SourceCodec::Opus
        } else if contains(bytes, b"A_VORBIS") {
            SourceCodec::Vorbis
        } else {
            SourceCodec::Unknown
        };
        return (codec, Container::Webm);
    }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WAVE" {
        return (SourceCodec::Lossless, Container::Other);
    }
    if bytes.len() >= 12 && &bytes[..4] == b"FORM" && (&bytes[8..12] == b"AIFF" || &bytes[8..12] == b"AIFC") {
        return (SourceCodec::Lossless, Container::Other);
    }
    if bytes.starts_with(b"MAC ") || bytes.starts_with(b"wvpk") {
        return (SourceCodec::Lossless, Container::Other);
    }
    if bytes.len() >= 2 && bytes[0] == 0xFF {
        if bytes[1] & 0xF6 == 0xF0 {
            return (SourceCodec::Aac, Container::Adts);
        }
        if bytes[1] & 0xE0 == 0xE0 && bytes[1] & 0x06 != 0 {
            return (SourceCodec::Mp3, Container::Mpeg);
        }
    }
    if start > 0 {
        return (SourceCodec::Mp3, Container::Mpeg);
    }
    (SourceCodec::Unknown, Container::Other)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| window == needle)
}

/// Whether an MP4 can be read from its first byte on (its `moov` or first `moof` before any
/// `mdat`): Some(true) or Some(false) once its first boxes tell, None while they do not.
pub fn mp4_streams_from_start(head: &[u8]) -> Option<bool> {
    let mut at = 0usize;
    while at + 8 <= head.len() {
        let size = u32::from_be_bytes([head[at], head[at + 1], head[at + 2], head[at + 3]]) as u64;
        let kind = &head[at + 4..at + 8];
        match kind {
            b"moov" | b"moof" => return Some(true),
            b"mdat" => return Some(false),
            _ => {}
        }
        let size = match size {
            0 => return None,
            1 => {
                let wide = head.get(at + 8..at + 16)?;
                u64::from_be_bytes(wide.try_into().ok()?)
            }
            size => size,
        };
        if size < 8 {
            return Some(false);
        }
        at = at.checked_add(usize::try_from(size).ok()?)?;
    }
    None
}

/// What to answer for an ask from a source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    /// The source as it is.
    AsIs,
    /// The source's Opus copied from WebM into Ogg, with no new encode.
    CopyOgg,
    /// A conversion.
    Convert(Target),
}

impl Plan {
    /// Its part of a converted copy's name, when it makes one.
    pub fn name(&self) -> Option<String> {
        match self {
            Plan::AsIs => None,
            Plan::CopyOgg => Some("ogg-copy".to_string()),
            Plan::Convert(target) => Some(target.name()),
        }
    }

    /// The file ending of the copy it makes.
    pub fn ext(&self) -> Option<&'static str> {
        match self {
            Plan::AsIs => None,
            Plan::CopyOgg => Some("opus"),
            Plan::Convert(target) => Some(target.codec.ext()),
        }
    }

    /// The format whose encoder it needs: none for the source as it is or a copy.
    pub fn encoder_codec(&self) -> Option<Codec> {
        match self {
            Plan::Convert(target) => Some(target.codec),
            _ => None,
        }
    }
}

/// The answer for an ask from a source:
///
/// - a format: the source as it is when it is that format in its usual container at or under
///   the rate; Opus in WebM at or under the rate is copied into Ogg; anything else converts;
/// - a cap: MP3 or AAC at or under it stays as it is, and so do Ogg and WebM for Octo's desktop
///   app, which plays them; anything else converts, to Opus for the desktop app, else to MP3.
///
/// A source whose rate is not known converts.
pub fn plan(ask: &Ask, source: &SourceAudio) -> Plan {
    match *ask {
        Ask::None => Plan::AsIs,
        Ask::Format(target) => {
            let within = source.kbps.is_some_and(|kbps| kbps <= target.kbps);
            match (target.codec, source.codec, source.container) {
                (Codec::Mp3, SourceCodec::Mp3, Container::Mpeg) if within => Plan::AsIs,
                (Codec::Aac, SourceCodec::Aac, Container::Mp4 | Container::Adts) if within => Plan::AsIs,
                (Codec::Opus, SourceCodec::Opus, Container::Ogg) if within => Plan::AsIs,
                (Codec::Opus, SourceCodec::Opus, Container::Webm) if within => Plan::CopyOgg,
                _ => Plan::Convert(target),
            }
        }
        Ask::Cap { kbps, prefer_opus } => {
            let within = source.kbps.is_some_and(|rate| rate <= kbps);
            let as_is = within
                && match (source.codec, source.container) {
                    (SourceCodec::Mp3, Container::Mpeg)
                    | (SourceCodec::Aac, Container::Mp4 | Container::Adts) => true,
                    (SourceCodec::Opus | SourceCodec::Vorbis, Container::Ogg)
                    | (SourceCodec::Opus, Container::Webm) => prefer_opus,
                    _ => false,
                };
            if as_is {
                Plan::AsIs
            } else {
                Plan::Convert(Target {
                    codec: if prefer_opus { Codec::Opus } else { Codec::Mp3 },
                    kbps,
                })
            }
        }
    }
}

fn strings<const N: usize>(words: [&str; N]) -> Vec<String> {
    words.iter().map(|word| word.to_string()).collect()
}

/// Reads only the first audio stream, with no picture, no tags, and no varying header bytes,
/// so a rebuild writes the very same bytes.
const READ: [&str; 9] = [
    "-map",
    "0:a:0",
    "-vn",
    "-map_metadata",
    "-1",
    "-fflags",
    "+bitexact",
    "-flags:a",
    "+bitexact",
];

/// An MP3 written as bare frames: no Xing or Info frame and no ID3 tags.
const MP3_FRAMES_ONLY: [&str; 6] = ["-write_xing", "0", "-id3v2_version", "0", "-write_id3v1", "0"];

fn encode(target: Target) -> Vec<String> {
    let rate = format!("{}k", target.kbps);
    let mut args = match target.codec {
        Codec::Mp3 => strings(["-c:a", "libmp3lame", "-b:a"]),
        Codec::Opus => strings(["-c:a", "libopus", "-b:a"]),
        Codec::Aac => strings(["-c:a", "aac", "-b:a"]),
    };
    args.push(rate);
    match target.codec {
        Codec::Mp3 => {
            args.extend(strings(MP3_FRAMES_ONLY));
            args.extend(strings(["-f", "mp3"]));
        }
        Codec::Opus => args.extend(strings(["-f", "ogg"])),
        Codec::Aac => args.extend(strings(["-f", "adts"])),
    }
    args
}

/// ffmpeg's arguments for a converted copy read from `input` (`pipe:0` or a path), written to
/// its output. Empty for the source as it is, which is never built.
pub fn build_args(plan: &Plan, input: &str) -> Vec<String> {
    let tail = match plan {
        Plan::AsIs => return Vec::new(),
        Plan::CopyOgg => strings(["-c:a", "copy", "-f", "ogg"]),
        Plan::Convert(target) => encode(*target),
    };
    let mut args = strings(["-v", "error", "-nostdin", "-xerror", "-i"]);
    args.push(input.to_string());
    args.extend(strings(READ));
    args.extend(tail);
    args.push("pipe:1".to_string());
    args
}

/// The streamable container the source's own audio is copied into for an answer from a later
/// time, and its type. None for audio with none to copy into.
pub fn copy_container_of(source: &SourceAudio) -> Option<(&'static str, &'static str)> {
    match (source.codec, source.container) {
        (SourceCodec::Aac, Container::Mp4 | Container::Adts) => Some(("adts", "audio/aac")),
        (SourceCodec::Opus | SourceCodec::Vorbis, Container::Ogg | Container::Webm) => {
            Some(("ogg", "audio/ogg"))
        }
        (SourceCodec::Mp3, _) => Some(("mp3", "audio/mpeg")),
        (SourceCodec::Lossless, Container::Flac) => Some(("flac", "audio/flac")),
        _ => None,
    }
}

/// ffmpeg's arguments for an answer from `offset` seconds in: the plan's conversion, or for
/// the source as it is its audio copied into its streamable container. A piped input cannot be
/// sought, so its `-ss` goes after `-i` (output side: frames before the time are dropped); a
/// file's goes before `-i`. Empty when the source has no container to copy into.
pub fn offset_args(plan: &Plan, source: &SourceAudio, input: &str, offset: u32) -> Vec<String> {
    let tail = match plan {
        Plan::Convert(target) => encode(*target),
        Plan::CopyOgg => strings(["-c:a", "copy", "-f", "ogg"]),
        Plan::AsIs => match copy_container_of(source) {
            Some(("mp3", _)) => {
                let mut tail = strings(["-c:a", "copy"]);
                tail.extend(strings(MP3_FRAMES_ONLY));
                tail.extend(strings(["-f", "mp3"]));
                tail
            }
            Some((format, _)) => vec![
                "-c:a".to_string(),
                "copy".to_string(),
                "-f".to_string(),
                format.to_string(),
            ],
            None => return Vec::new(),
        },
    };
    let seek = offset.to_string();
    let mut args = strings(["-v", "error", "-nostdin", "-xerror"]);
    if input == "pipe:0" {
        args.extend(["-i".to_string(), input.to_string(), "-ss".to_string(), seek]);
    } else {
        args.extend(["-ss".to_string(), seek, "-i".to_string(), input.to_string()]);
    }
    args.extend(strings(READ));
    args.extend(tail);
    args.push("pipe:1".to_string());
    args
}

/// The type of an answer from a later time.
pub fn offset_type(plan: &Plan, source: &SourceAudio) -> Option<&'static str> {
    match plan {
        Plan::Convert(target) => Some(target.codec.content_type()),
        Plan::CopyOgg => Some("audio/ogg"),
        Plan::AsIs => copy_container_of(source).map(|(_, content_type)| content_type),
    }
}

/// The type an answer is served with: the source's own for the source as it is.
pub fn answer_type(plan: &Plan, source_type: &str) -> String {
    match plan {
        Plan::AsIs => source_type.to_string(),
        Plan::CopyOgg => "audio/ogg".to_string(),
        Plan::Convert(target) => target.codec.content_type().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(pairs: &[(&str, &str)]) -> Parameters {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    fn ask(pairs: &[(&str, &str)]) -> Ask {
        ask_of(&params(pairs), "stream", None, GateClient::Other).ask
    }

    fn mp3(kbps: u32) -> Target {
        Target {
            codec: Codec::Mp3,
            kbps,
        }
    }

    fn opus(kbps: u32) -> Target {
        Target {
            codec: Codec::Opus,
            kbps,
        }
    }

    fn aac(kbps: u32) -> Target {
        Target {
            codec: Codec::Aac,
            kbps,
        }
    }

    #[test]
    fn a_format_is_read_with_its_rate_and_a_rate_of_zero_is_no_limit() {
        assert_eq!(
            ask(&[("format", "mp3"), ("maxBitRate", "128")]),
            Ask::Format(mp3(128))
        );
        assert_eq!(
            ask(&[("format", "MP3"), ("maxBitRate", "0")]),
            Ask::Format(mp3(192))
        );
        assert_eq!(ask(&[("format", "opus")]), Ask::Format(opus(128)));
        assert_eq!(
            ask(&[("format", "aac"), ("maxBitRate", "x")]),
            Ask::Format(aac(256))
        );
        assert_eq!(
            ask(&[("format", "mp3"), ("maxBitRate", "8")]),
            Ask::Format(mp3(32))
        );
        assert_eq!(
            ask(&[("format", "mp3"), ("maxBitRate", "999")]),
            Ask::Format(mp3(320))
        );
        assert_eq!(
            ask(&[("FORMAT", "opus"), ("MAXBITRATE", "96")]),
            Ask::Format(opus(96))
        );
    }

    #[test]
    fn a_rate_alone_is_a_cap_and_raw_or_unknown_words_are_the_file_as_it_is() {
        assert_eq!(
            ask(&[("maxBitRate", "96")]),
            Ask::Cap {
                kbps: 96,
                prefer_opus: false
            }
        );
        assert_eq!(ask(&[("format", "raw"), ("maxBitRate", "96")]), Ask::None);
        assert_eq!(ask(&[("format", "flac")]), Ask::None);
        assert_eq!(ask(&[("maxBitRate", "0")]), Ask::None);
        assert_eq!(ask(&[]), Ask::None);
    }

    #[test]
    fn a_download_reads_its_rate_from_bitrate() {
        let pairs = params(&[("format", "mp3"), ("bitrate", "96"), ("maxBitRate", "256")]);
        assert_eq!(
            ask_of(&pairs, "download", None, GateClient::Other).ask,
            Ask::Format(mp3(96))
        );
        assert_eq!(
            ask_of(&pairs, "rest/download.view", None, GateClient::Other).ask,
            Ask::Format(mp3(96))
        );
        assert_eq!(
            ask_of(&pairs, "stream", None, GateClient::Other).ask,
            Ask::Format(mp3(256))
        );
    }

    #[test]
    fn the_gates_record_is_read_and_a_ceiling_cannot_be_stepped_around() {
        // The parameters hold what the gate wrote; the client's own ask is in the record.
        let rewritten = params(&[("format", "opus"), ("maxBitRate", "96")]);
        let own = |format: Option<&str>, rate: Option<u32>, client: GateClient| OwnAsk {
            format: format.map(str::to_string),
            rate,
            ceiling: Some(96),
            client,
        };
        let read = |own: OwnAsk| ask_of(&rewritten, "stream", Some(&own), GateClient::Other).ask;
        assert_eq!(
            read(own(Some("raw"), None, GateClient::OctoPhone)),
            Ask::Cap {
                kbps: 96,
                prefer_opus: false
            }
        );
        assert_eq!(
            read(own(Some("opus"), Some(160), GateClient::Other)),
            Ask::Format(opus(96))
        );
        assert_eq!(
            read(own(Some("flac"), None, GateClient::Other)),
            Ask::Cap {
                kbps: 96,
                prefer_opus: false
            }
        );
        assert_eq!(
            read(own(None, None, GateClient::OctoDesktop)),
            Ask::Cap {
                kbps: 96,
                prefer_opus: true
            }
        );
        assert_eq!(
            read(own(None, Some(64), GateClient::OctoDesktop)),
            Ask::Cap {
                kbps: 64,
                prefer_opus: true
            }
        );
    }

    #[test]
    fn the_time_and_the_length_wish_are_read() {
        let read = ask_of(
            &params(&[
                ("format", "opus"),
                ("timeOffset", "61.7"),
                ("estimateContentLength", "true"),
            ]),
            "stream",
            None,
            GateClient::OctoDesktop,
        );
        assert_eq!(read.offset, 61);
        assert!(read.exact);
        let plain = ask_of(
            &params(&[("timeOffset", "-3")]),
            "stream",
            None,
            GateClient::Other,
        );
        assert_eq!(plain.offset, 0);
        assert!(!plain.exact);
    }

    #[test]
    fn octo_apps_are_told_apart_by_their_user_agent() {
        assert_eq!(
            GateClient::of("Octo", "Octo/1.13.0 (Android)"),
            GateClient::OctoPhone
        );
        assert_eq!(
            GateClient::of("octo", "Octo/1.13.0 (iPhone)"),
            GateClient::OctoPhone
        );
        assert_eq!(GateClient::of("Octo", "Octo"), GateClient::OctoDesktop);
        assert_eq!(
            GateClient::of("Octo", "Octo/1.13.0 (Windows)"),
            GateClient::OctoDesktop
        );
        assert_eq!(GateClient::of("Octo", "Lavf/60.3.100"), GateClient::Other);
        assert_eq!(GateClient::of("Octo", ""), GateClient::Other);
        assert_eq!(
            GateClient::of("Symfonium", "Octo/1.13.0 (Android)"),
            GateClient::Other
        );
        assert_eq!(GateClient::of("Octo", "OctoMusic/2"), GateClient::Other);
    }

    fn source(codec: SourceCodec, container: Container, kbps: Option<u32>) -> SourceAudio {
        SourceAudio {
            codec,
            container,
            kbps,
        }
    }

    #[test]
    fn a_format_ask_keeps_the_source_only_when_it_is_already_that() {
        let m4a = source(SourceCodec::Aac, Container::Mp4, Some(128));
        let webm = source(SourceCodec::Opus, Container::Webm, Some(130));
        let ogg = source(SourceCodec::Opus, Container::Ogg, Some(128));
        let mpeg = source(SourceCodec::Mp3, Container::Mpeg, Some(320));
        let flac = source(SourceCodec::Lossless, Container::Flac, None);
        let unknown_rate = source(SourceCodec::Aac, Container::Mp4, None);
        let cases = [
            (Ask::Format(aac(256)), m4a, Plan::AsIs),
            (Ask::Format(aac(96)), m4a, Plan::Convert(aac(96))),
            (Ask::Format(mp3(128)), m4a, Plan::Convert(mp3(128))),
            (Ask::Format(opus(160)), webm, Plan::CopyOgg),
            (Ask::Format(opus(96)), webm, Plan::Convert(opus(96))),
            (Ask::Format(opus(160)), ogg, Plan::AsIs),
            (Ask::Format(mp3(320)), mpeg, Plan::AsIs),
            (Ask::Format(mp3(128)), mpeg, Plan::Convert(mp3(128))),
            (Ask::Format(opus(96)), flac, Plan::Convert(opus(96))),
            (Ask::Format(aac(256)), unknown_rate, Plan::Convert(aac(256))),
            (Ask::None, flac, Plan::AsIs),
        ];
        for (ask, source, want) in cases {
            assert_eq!(plan(&ask, &source), want, "{ask:?} on {source:?}");
        }
    }

    #[test]
    fn a_cap_keeps_what_the_client_plays_at_or_under_it() {
        let cap = |kbps, prefer_opus| Ask::Cap { kbps, prefer_opus };
        let m4a = source(SourceCodec::Aac, Container::Mp4, Some(128));
        let adts = source(SourceCodec::Aac, Container::Adts, Some(128));
        let webm = source(SourceCodec::Opus, Container::Webm, Some(130));
        let vorbis = source(SourceCodec::Vorbis, Container::Ogg, Some(112));
        let flac = source(SourceCodec::Lossless, Container::Flac, None);
        let cases = [
            (cap(160, false), m4a, Plan::AsIs),
            (cap(160, false), adts, Plan::AsIs),
            (cap(96, false), m4a, Plan::Convert(mp3(96))),
            (cap(160, false), webm, Plan::Convert(mp3(160))),
            (cap(160, true), webm, Plan::AsIs),
            (cap(160, true), vorbis, Plan::AsIs),
            (cap(160, false), vorbis, Plan::Convert(mp3(160))),
            (cap(96, true), webm, Plan::Convert(opus(96))),
            (cap(96, true), flac, Plan::Convert(opus(96))),
            (cap(96, false), flac, Plan::Convert(mp3(96))),
        ];
        for (ask, source, want) in cases {
            assert_eq!(plan(&ask, &source), want, "{ask:?} on {source:?}");
        }
    }

    #[test]
    fn a_files_first_bytes_name_its_audio() {
        let ogg = |packet: &[u8]| {
            let mut page = b"OggS".to_vec();
            page.extend([0u8; 22]);
            page.push(1); // one segment
            page.push(packet.len() as u8);
            page.extend_from_slice(packet);
            page
        };
        let mut id3_flac = b"ID3\x04\x00\x00\x00\x00\x00\x02".to_vec();
        id3_flac.extend([0u8; 2]);
        id3_flac.extend_from_slice(b"fLaC....");
        let mut id3_mp3 = b"ID3\x04\x00\x00\x00\x00\x00\x00".to_vec();
        id3_mp3.extend([0xFF, 0xFB, 0x90, 0x00]);
        let mut webm = vec![0x1A, 0x45, 0xDF, 0xA3];
        webm.extend_from_slice(b"....A_OPUS..");
        let cases: [(Vec<u8>, (SourceCodec, Container)); 11] = [
            (b"fLaC\x00\x00".to_vec(), (SourceCodec::Lossless, Container::Flac)),
            (
                b"\x00\x00\x00\x18ftypM4A ".to_vec(),
                (SourceCodec::Aac, Container::Mp4),
            ),
            (ogg(b"OpusHead\x01\x02"), (SourceCodec::Opus, Container::Ogg)),
            (ogg(b"\x01vorbis\x00"), (SourceCodec::Vorbis, Container::Ogg)),
            (vec![0xFF, 0xF1, 0x50, 0x80], (SourceCodec::Aac, Container::Adts)),
            (vec![0xFF, 0xFB, 0x90, 0x00], (SourceCodec::Mp3, Container::Mpeg)),
            (vec![0xFF, 0xF3, 0x84, 0x00], (SourceCodec::Mp3, Container::Mpeg)),
            (id3_flac, (SourceCodec::Lossless, Container::Flac)),
            (id3_mp3, (SourceCodec::Mp3, Container::Mpeg)),
            (webm, (SourceCodec::Opus, Container::Webm)),
            (
                b"RIFF\x00\x00\x00\x00WAVEfmt ".to_vec(),
                (SourceCodec::Lossless, Container::Other),
            ),
        ];
        for (head, want) in cases {
            assert_eq!(sniff_file(&head), want, "{head:02x?}");
        }
        assert_eq!(sniff_file(b"junk"), (SourceCodec::Unknown, Container::Other));
    }

    #[test]
    fn a_stream_is_read_by_its_type() {
        assert_eq!(
            source_of_type("audio/mp4; codecs=mp4a.40.2", Some(128)),
            source(SourceCodec::Aac, Container::Mp4, Some(128))
        );
        assert_eq!(
            source_of_type("audio/webm", None),
            source(SourceCodec::Opus, Container::Webm, None)
        );
        assert_eq!(
            source_of_type("audio/flac", Some(900)),
            source(SourceCodec::Lossless, Container::Flac, None)
        );
    }

    fn words(args: &[String]) -> String {
        args.join(" ")
    }

    #[test]
    fn a_build_reads_one_audio_stream_and_writes_the_same_bytes_every_time() {
        assert_eq!(
            words(&build_args(&Plan::Convert(mp3(128)), "pipe:0")),
            "-v error -nostdin -xerror -i pipe:0 -map 0:a:0 -vn -map_metadata -1 -fflags +bitexact \
             -flags:a +bitexact -c:a libmp3lame -b:a 128k -write_xing 0 -id3v2_version 0 -write_id3v1 0 \
             -f mp3 pipe:1"
        );
        assert_eq!(
            words(&build_args(&Plan::Convert(opus(96)), "/c/a.flac")),
            "-v error -nostdin -xerror -i /c/a.flac -map 0:a:0 -vn -map_metadata -1 -fflags +bitexact \
             -flags:a +bitexact -c:a libopus -b:a 96k -f ogg pipe:1"
        );
        assert_eq!(
            words(&build_args(&Plan::Convert(aac(256)), "pipe:0")),
            "-v error -nostdin -xerror -i pipe:0 -map 0:a:0 -vn -map_metadata -1 -fflags +bitexact \
             -flags:a +bitexact -c:a aac -b:a 256k -f adts pipe:1"
        );
        assert_eq!(
            words(&build_args(&Plan::CopyOgg, "pipe:0")),
            "-v error -nostdin -xerror -i pipe:0 -map 0:a:0 -vn -map_metadata -1 -fflags +bitexact \
             -flags:a +bitexact -c:a copy -f ogg pipe:1"
        );
        assert!(build_args(&Plan::AsIs, "pipe:0").is_empty());
    }

    #[test]
    fn a_later_time_is_sought_after_the_input_on_a_pipe_and_before_it_on_a_file() {
        let m4a = source(SourceCodec::Aac, Container::Mp4, Some(128));
        assert_eq!(
            words(&offset_args(&Plan::AsIs, &m4a, "pipe:0", 30)),
            "-v error -nostdin -xerror -i pipe:0 -ss 30 -map 0:a:0 -vn -map_metadata -1 -fflags +bitexact \
             -flags:a +bitexact -c:a copy -f adts pipe:1"
        );
        assert_eq!(
            words(&offset_args(
                &Plan::Convert(opus(96)),
                &m4a,
                "/c/v.mp3-128.mp3",
                30
            )),
            "-v error -nostdin -xerror -ss 30 -i /c/v.mp3-128.mp3 -map 0:a:0 -vn -map_metadata -1 \
             -fflags +bitexact -flags:a +bitexact -c:a libopus -b:a 96k -f ogg pipe:1"
        );
        let mpeg = source(SourceCodec::Mp3, Container::Mpeg, Some(320));
        assert!(
            words(&offset_args(&Plan::AsIs, &mpeg, "/c/a.mp3", 5))
                .contains("-c:a copy -write_xing 0 -id3v2_version 0 -write_id3v1 0 -f mp3 pipe:1")
        );
        assert_eq!(offset_type(&Plan::AsIs, &m4a), Some("audio/aac"));
        let unknown = source(SourceCodec::Unknown, Container::Other, None);
        assert!(offset_args(&Plan::AsIs, &unknown, "/c/a.wma", 5).is_empty());
        assert_eq!(offset_type(&Plan::AsIs, &unknown), None);
    }

    #[test]
    fn an_mp4_tells_whether_it_reads_from_its_first_byte() {
        let boxed = |kind: &[u8; 4], size: u32| {
            let mut bytes = size.to_be_bytes().to_vec();
            bytes.extend_from_slice(kind);
            bytes.extend(vec![0u8; size as usize - 8]);
            bytes
        };
        let fragmented = [boxed(b"ftyp", 24), boxed(b"moov", 16)].concat();
        let late = [boxed(b"ftyp", 24), boxed(b"mdat", 16)].concat();
        assert_eq!(mp4_streams_from_start(&fragmented), Some(true));
        assert_eq!(mp4_streams_from_start(&late), Some(false));
        assert_eq!(mp4_streams_from_start(&boxed(b"ftyp", 24)), None);
    }

    #[test]
    fn answers_are_typed_by_what_they_hold() {
        assert_eq!(answer_type(&Plan::AsIs, "audio/mp4"), "audio/mp4");
        assert_eq!(answer_type(&Plan::CopyOgg, "audio/webm"), "audio/ogg");
        assert_eq!(answer_type(&Plan::Convert(aac(256)), "audio/flac"), "audio/aac");
        assert_eq!(Plan::Convert(mp3(128)).name().as_deref(), Some("mp3-128"));
        assert_eq!(Plan::CopyOgg.ext(), Some("opus"));
        assert_eq!(Plan::CopyOgg.encoder_codec(), None);
    }
}
