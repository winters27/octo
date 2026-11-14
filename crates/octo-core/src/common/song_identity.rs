//! One reading of song titles and artist credits for every place Octo decides whether two
//! songs are the same.
//!
//! Text arguments are `&str`, where a missing value is passed as `""` and reads as empty. An
//! `Option` is kept only where missing means something else: the artist of
//! [`SongIdentity::parse_title`], where `None` reads the title alone and empty means the song
//! has no artist.
//!
//! Regex notes. The `regex` crate has no lookaround, so the four patterns that need it are
//! matched in two steps that give the same answer: the track-number lookahead is a captured
//! character, the `x` separator's negative lookahead is checked after the match (and the search
//! resumes one character on, as a backtracking engine would), and the `VEVO` lookbehind is
//! tested by hand. Leftmost-first alternation and lazy quantifiers behave as in a backtracking
//! engine. Where the Unicode classes must answer differently from Rust's, the pattern is run over
//! a same-length view of the text: [`word_boundary_view`] for `\b` (word characters here are
//! letters, nonspacing marks, digits and connectors in one UTF-16 unit; Rust's also take spacing
//! marks and supplementary letters), [`utf16_class_view`] for `\d` and `\p{L}` (neither matches
//! a supplementary-plane character). Case-insensitive patterns fold case with no locale, as the
//! container runs with no `LANG`: under en-US, `(?i)i` would also match `İ`.
//!
//! Every public function here was compared with the reference output on two generated corpora
//! of about 25,000 titles, credits and pairs each (all of song-identity-cases.json, plus mixed
//! scripts, marks, supplementary characters, brackets and separators), with no difference.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::LazyLock;

use indexmap::IndexSet;
use regex::{Captures, Regex};
use unicode_normalization::UnicodeNormalization;

use super::fuzzy::{jaro_winkler, ratio, token_sort_ratio};
use super::romanize::{first_script, has_non_ascii_letters, romanized_key, romanized_words, words};

use super::dotnet::{
    eq_ignore_case, format_optional_decimals, is_blank, is_cased_letter_utf16, is_combining_mark,
    is_digit_utf16, is_letter_or_digit, is_letter_utf16, is_upper_utf16, round, starts_with_ignore_case,
    to_lower_invariant, to_upper_char, utf16_class_view, utf16_len, word_boundary_view,
};

/// What a comparison of two songs found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SongVerdict {
    /// Another song, or another artist's song.
    Different,

    /// The same song in another version: live against studio, a remix, a sped-up
    /// upload, a different guest, or a length that is too far off.
    SameSongDifferentVersion,

    /// The same recording.
    Same,
}

/// A verdict, how sure it is (0 to 1), and why, in words a log or a person can read.
#[derive(Debug, Clone, PartialEq)]
pub struct SongMatch {
    pub verdict: SongVerdict,
    pub confidence: f64,
    pub reason: String,
}

impl SongMatch {
    pub fn new(verdict: SongVerdict, confidence: f64, reason: impl Into<String>) -> Self {
        Self {
            verdict,
            confidence,
            reason: reason.into(),
        }
    }

    pub fn is_same(&self) -> bool {
        self.verdict == SongVerdict::Same
    }
}

/// What a comparison accepts beyond the rules every caller gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Lookup {
    /// Only the rules every caller gets.
    #[default]
    Off,

    /// Also the same title in another script ("Камин" and "Kamin"), and the same artists
    /// credited another way ("Fonsi, Luis" and "Luis Fonsi"). For deciding whether a song is
    /// already owned or which entry is the song asked for; never for deleting or replacing files.
    Owned,

    /// [`Lookup::Owned`], plus a title a typo away when both lengths are known and agree. Only
    /// where one search's results are ranked and a near tie picks nothing
    /// ([`SongIdentity::pick_best`]).
    Pick,
}

/// How one call site wants songs compared. The defaults are the strict reading.
#[derive(Debug, Clone, PartialEq)]
pub struct SongMatchOptions {
    /// How far apart two known lengths may be and still be one recording. None
    /// compares no lengths.
    pub length_tolerance_seconds: Option<i32>,

    /// When on, a bracketed subtitle only one title carries ("Blue (Da Ba Dee)"
    /// against "Blue") makes them different songs. Off, it only lowers the confidence.
    pub extras_must_agree: bool,

    /// Version markers this caller treats as the same recording, on top of the ones
    /// that always are (remaster, explicit, original mix, album version, mono, stereo).
    pub also_neutral: Vec<String>,

    /// What else is accepted, for lookups. Off by default.
    pub lookup: Lookup,
}

impl Default for SongMatchOptions {
    fn default() -> Self {
        Self {
            length_tolerance_seconds: Some(SongIdentity::LENGTH_TOLERANCE_SECONDS),
            extras_must_agree: false,
            also_neutral: Vec::new(),
            lookup: Lookup::Off,
        }
    }
}

static DEFAULT_OPTIONS: LazyLock<SongMatchOptions> = LazyLock::new(SongMatchOptions::default);

/// One side of a comparison. Seconds is the length when it is known.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SongRef {
    pub title: String,
    pub artist: String,
    pub seconds: Option<f64>,

    /// The ISRCs this side is known by, as a source wrote them: a Deezer track, a
    /// file's tags, a MusicBrainz recording. Anything that is not a valid ISRC is ignored.
    pub isrcs: Vec<String>,
}

impl SongRef {
    pub fn new(title: impl Into<String>, artist: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            artist: artist.into(),
            seconds: None,
            isrcs: Vec::new(),
        }
    }

    pub fn with_seconds(mut self, seconds: impl Into<Option<f64>>) -> Self {
        self.seconds = seconds.into();
        self
    }

    pub fn with_isrcs<I, S>(mut self, isrcs: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.isrcs = isrcs.into_iter().map(Into::into).collect();
        self
    }
}

/// One query to try against a search API, in the order [`SongIdentity::query_variants`]
/// gives them. Artist is empty for the title-only query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SongQuery {
    pub title: String,
    pub artist: String,
}

impl SongQuery {
    pub fn new(title: impl Into<String>, artist: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            artist: artist.into(),
        }
    }

    /// The artist and the title as one free-text query.
    pub fn text(&self) -> String {
        if self.artist.is_empty() {
            self.title.clone()
        } else {
            format!("{} {}", self.artist, self.title)
        }
    }
}

/// A title read into its parts.
#[derive(Debug, Clone, PartialEq)]
pub struct SongTitle {
    /// The title as given.
    pub raw: String,
    /// What is left once track numbers, a leading "Artist - ", brackets, features
    /// and version tails are taken off: the title a person would say.
    pub core: String,
    /// The core compared exactly: casefolded, accents and punctuation ignored,
    /// with any part number ("Pt. 2") kept.
    pub key: String,
    /// The key again with stylized characters read as letters ($ as s, 0 as
    /// o, 3 as e, @ as a, ! as i). An additional key, never a replacement.
    pub loose_key: String,
    /// Every version marker found, canonical names, sorted.
    pub versions: Vec<String>,
    /// Keys of whoever a remix, mix, dub or edit is credited to.
    pub remixers: Vec<String>,
    /// Artists the title credits, "(feat. X)".
    pub featured: Vec<String>,
    /// Keys of bracketed subtitles that are none of the above, "(Da Ba Dee)".
    pub extras: Vec<String>,
    /// The artist a title named in "Artist - Title" form, or None.
    pub artist_from_title: Option<String>,
    /// The core with its part numbers, before keying: where numbers are still apart.
    pub keyed: String,
}

/// An artist credit read into its parts.
#[derive(Debug, Clone, PartialEq)]
pub struct SongArtists {
    /// The credit as given.
    pub raw: String,
    /// The credit with channel suffixes, native-script aliases and bracketed
    /// guests taken off.
    pub display: String,
    /// The artists in it, split on every separator but never inside a known
    /// name ("Tyler, The Creator") or before "the" ("Bob Marley & The Wailers"). The first is the
    /// primary artist.
    pub names: Vec<String>,
    /// Guests credited in brackets, "Drake (feat. Rihanna)".
    pub featured: Vec<String>,
    /// Parts of a name kept whole only by the "the" rule, so "Bob Marley" alone
    /// still matches "Bob Marley & The Wailers".
    pub pieces: Vec<String>,
}

impl SongArtists {
    pub fn primary(&self) -> &str {
        self.names.first().map_or(self.display.as_str(), String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        SongIdentity::key(&self.display).is_empty() && self.names.is_empty()
    }
}

/// How two artist credits relate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArtistAgreement {
    /// Either side is empty.
    Unknown,
    /// No artist in common.
    None,
    /// They share an artist, but each names a guest the other does not.
    Conflict,
    /// They share an artist only once stylized characters are read as letters.
    Loose,
    /// They share an artist.
    Agree,
}

/// One reading of song titles and artist credits for every place Octo decides whether two
/// songs are the same: lyrics, AcoustID and MusicBrainz, Soulseek candidates, the library and
/// radio matchers, search merging, and the Deezer and Last.fm lookups. Each of those used to
/// carry its own normalizer, and they disagreed, so a song one of them recognized another did
/// not.
///
/// The rules, in the order a title is read:
///
/// 1. Fold: Unicode NFKC (fullwidth "＄" and "﹩" become "$", "（" becomes "("), curly quotes and
///    dashes made plain, underscores made spaces, Cyrillic and Greek lookalikes inside a Latin
///    word read as Latin.
/// 2. A track number in front ("01 - ", "01. ", "1-01 ") is dropped, and so is a leading
///    "Artist - " when it names the artist (or when no artist was given at all).
/// 3. Each bracket is read as a guest ("feat. X"), upload noise ("Official Video"), a part
///    number ("Pt. 2", kept in the key), one or more version markers, or a subtitle.
/// 4. A " - " tail that is a version or noise ("- Remastered 2011", "- Live at Wembley") is
///    read the same way, and so is a trailing "feat. X" and a trailing "Remix" or "Sped Up".
/// 5. The key is the rest, casefolded, accents stripped, and only letters, digits and the marks
///    some scripts need kept. A title of only symbols or emoji keeps its symbols instead.
///
/// Artist credits split on , & ; / 、 x × and with feat ft featuring vs, and the whole credit
/// is kept as a candidate beside its parts, so "Simon & Garfunkel" and "Earth, Wind & Fire"
/// match themselves however they are split. A small alias table covers renamed artists ("Ye"
/// for Kanye West), and a bracketed alias in another script ("Ye (侃爷)") is ignored.
///
/// An ISRC on both sides settles it before any of that: one ISRC is one recording, whatever
/// script or language its title is written in. Two different ISRCs settle nothing, because a
/// re-release or a remaster is often given a new code for the same audio.
///
/// The same rules, and a shared list of cases, live in the Octo app; docs/song-identity-cases.json
/// is the contract both run.
pub struct SongIdentity;

/// Version markers that never make a different recording.
pub const NEUTRAL_VERSIONS: [&str; 7] = [
    "remaster",
    "explicit",
    "original",
    "album version",
    "single version",
    "mono",
    "stereo",
];

fn rx(pattern: &str) -> Regex {
    Regex::new(pattern).expect("a fixed pattern compiles")
}

/// A case-insensitive pattern.
fn rxi(pattern: &str) -> Regex {
    rx(&format!("(?i){pattern}"))
}

// ---- folding ------------------------------------------------------------------------

static WHITESPACE: LazyLock<Regex> = LazyLock::new(|| rx(r"\s+"));

static WORD: LazyLock<Regex> = LazyLock::new(|| rx(r"[\p{L}\p{M}]+"));

/// Cyrillic and Greek letters that look Latin, read as Latin inside a word that
/// also has Latin letters. A word wholly in Cyrillic or Greek is left as it is.
fn homoglyph(ch: char) -> Option<char> {
    Some(match ch {
        'А' => 'A',
        'В' => 'B',
        'Е' => 'E',
        'К' => 'K',
        'М' => 'M',
        'Н' => 'H',
        'О' => 'O',
        'Р' => 'P',
        'С' => 'C',
        'Т' => 'T',
        'Х' => 'X',
        'І' => 'I',
        'Ј' => 'J',
        'Ѕ' => 'S',
        'а' => 'a',
        'е' => 'e',
        'о' => 'o',
        'р' => 'p',
        'с' => 'c',
        'у' => 'y',
        'х' => 'x',
        'і' => 'i',
        'ј' => 'j',
        'ѕ' => 's',
        'Α' => 'A',
        'Β' => 'B',
        'Ε' => 'E',
        'Ζ' => 'Z',
        'Η' => 'H',
        'Ι' => 'I',
        'Κ' => 'K',
        'Μ' => 'M',
        'Ν' => 'N',
        'Ο' => 'O',
        'Ρ' => 'P',
        'Τ' => 'T',
        'Υ' => 'Y',
        'Χ' => 'X',
        'ο' => 'o',
        _ => return None,
    })
}

fn fold_mixed_script_words(text: &str) -> String {
    // \p{L} matches no supplementary-plane letter here, so one splits a word.
    replace_in_view(text, &utf16_class_view(text), &WORD, |caps| {
        let word = &caps[0];
        if !word.chars().any(|ch| ch.is_ascii_alphabetic()) {
            return word.to_string();
        }
        if !word.chars().any(|ch| homoglyph(ch).is_some()) {
            return word.to_string();
        }
        word.chars().map(|ch| homoglyph(ch).unwrap_or(ch)).collect()
    })
}

/// Replaces every match of `pattern` with what the evaluator returns, matched in a view of the
/// text (see [`word_boundary_view`]) and replaced in the text itself. The evaluator sees the
/// view's captures, which only differ from the text in characters these patterns never capture.
fn replace_in_view(
    text: &str,
    view: &str,
    pattern: &Regex,
    mut evaluate: impl FnMut(&Captures) -> String,
) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for caps in pattern.captures_iter(view) {
        let whole = caps.get(0).expect("group 0 is the match");
        out.push_str(&text[last..whole.start()]);
        out.push_str(&evaluate(&caps));
        last = whole.end();
    }
    out.push_str(&text[last..]);
    out
}

/// Lowercase, the letters that do not decompose spelled out, accents stripped,
/// "&" and a spaced "+" read as "and".
fn lower_fold(folded: &str) -> String {
    if folded.is_empty() {
        return String::new();
    }
    let lower = to_lower_invariant(folded)
        .replace('ß', "ss")
        .replace('æ', "ae")
        .replace('œ', "oe")
        .replace('ø', "o")
        .replace(['đ', 'ð'], "d")
        .replace('ł', "l")
        .replace('þ', "th")
        .replace('ı', "i")
        .replace('&', " and ")
        .replace(" + ", " and ");
    // Only the Latin, Greek and Cyrillic accents. A kana's voicing mark is a different
    // letter, and stripping it would make two Japanese titles one.
    let stripped: String = lower
        .nfd()
        .filter(|ch| {
            !matches!(ch, '\u{0300}'..='\u{036F}' | '\u{1AB0}'..='\u{1AFF}' | '\u{1DC0}'..='\u{1DFF}' | '\u{FE20}'..='\u{FE2F}')
        })
        .collect();
    stripped.nfc().collect()
}

// ---- titles -------------------------------------------------------------------------

/// "01 - ", "01. ", "1-01 ", "2) " in front of a title, with a letter after it.
/// "1-800-273-8255" and "99 Problems" keep their numbers.
///
/// The letter after the number, `[^\d\s.]`, is group 1 rather than a lookahead, and the
/// number ends where it starts.
static TRACK_NUMBER: LazyLock<Regex> =
    LazyLock::new(|| rx(r"^(?:\d{1,2}-\d{1,3}\s+|\d{1,3}\s*[.)]\s*|\d{1,3}\s+-\s+)([^\d\s.])"));

static BRACKET: LazyLock<Regex> = LazyLock::new(|| rx(r"\s*[\(\[\{]([^\(\)\[\]\{\}]*)[\)\]\}]"));

/// A bracket a truncated title never closed, "Song (feat. X".
static OPEN_BRACKET: LazyLock<Regex> = LazyLock::new(|| rx(r"\s+[\(\[]([^\(\)\[\]]*)$"));

static DASH_TAIL: LazyLock<Regex> = LazyLock::new(|| rx(r"\s+-\s+"));

static TRAILING_FEATURE: LazyLock<Regex> = LazyLock::new(|| rxi(r"\s+(?:feat\.?|ft\.?|featuring)\s+(.+)$"));

/// A guest credit run on after a version inside one bracket, "Radio Edit - feat. X". Matched in
/// the view where `\b` falls by word characters in one UTF-16 unit.
static INNER_FEATURE: LazyLock<Regex> =
    LazyLock::new(|| rxi(r"\s*(?:[-\u{2013},;/]\s*)?\b(?:feat\.?|ft\.?|featuring)\s+.+$"));

/// Version words that mean a version even without brackets at the end of a title,
/// "Mask Off Remix", "Heat Waves Sped Up". Not "live" or "edit": too many titles end in them.
static TRAILING_VERSION: LazyLock<Regex> = LazyLock::new(|| {
    rxi(
        r"\s+(re-?mix|rmx|sped\s*up|speed\s*up|slowed(?:\s*(?:\+|&|and|n)\s*reverb(?:ed)?)?|slowed\s+down|nightcore|instrumental|acapella|a\s*cappella|karaoke(?:\s+version)?|drumless|8d\s+audio)$",
    )
});

static FEATURE_LEAD: LazyLock<Regex> =
    LazyLock::new(|| rxi(r"^(?:feat\.\s*|ft\.\s*|w/\s*|(?:feat|ft|featuring|with)\s+)(.+)$"));

static PART_NUMBER: LazyLock<Regex> =
    LazyLock::new(|| rxi(r"^(?:(?:pt|part|vol|volume|chapter|ch|no|book)\.?\s*)?(?:\d{1,3}|[ivx]{1,4})$"));

static YEAR: LazyLock<Regex> = LazyLock::new(|| rx(r"^(?:19|20)\d{2}$"));

/// A part number, "Pt. 2", "Part II", "Vol. 3", spelled one way so "Part II" and
/// "Pt. 2" agree.
static PART_WORD: LazyLock<Regex> =
    LazyLock::new(|| rxi(r"\b(?:(pt|part)|(vol|volume)|(chapter|ch)|(book))\.?\s*(\d{1,3}|[ivx]{1,4})\b"));

const ROMANS: [&str; 10] = ["i", "ii", "iii", "iv", "v", "vi", "vii", "viii", "ix", "x"];

fn part_of(caps: &Captures) -> String {
    let word = if caps.get(1).is_some() {
        "pt"
    } else if caps.get(2).is_some() {
        "vol"
    } else if caps.get(3).is_some() {
        "ch"
    } else {
        "book"
    };
    let number = to_lower_invariant(&caps[5]);
    match ROMANS.iter().position(|roman| *roman == number) {
        Some(roman) => format!("{word} {}", roman + 1),
        None => format!("{word} {number}"),
    }
}

/// Words that only ever describe how something was uploaded, never which recording
/// it is. A bracket made only of these is dropped. (Compared ignoring case.)
const UPLOAD_NOISE: [&str; 27] = [
    "official",
    "music",
    "video",
    "audio",
    "lyric",
    "lyrics",
    "visualizer",
    "visualiser",
    "hd",
    "hq",
    "uhd",
    "4k",
    "8k",
    "1080p",
    "720p",
    "480p",
    "mv",
    "m/v",
    "clip",
    "videoclip",
    "with",
    "full",
    "song",
    "only",
    "new",
    "premiere",
    "animated",
];

fn is_upload_noise(word: &str) -> bool {
    UPLOAD_NOISE.iter().any(|noise| eq_ignore_case(noise, word))
}

static NEUTRAL_PHRASE: LazyLock<Regex> = LazyLock::new(|| {
    rxi(concat!(
        r"^(?:from|taken from|as heard (?:in|on)|as featured in|as seen (?:in|on)|theme from|music from)\b",
        r"|\b(?:soundtrack|ost|motion picture|original score)\b",
        r"|^bonus(?:\s+tracks?)?(?:\s+(?:edition|version))?$|^(?:prod|produced)\b",
        // A release edition is the same recordings packaged again; "Drumless Edition" is a marker below.
        r"|^(?:(?:super\s+)?deluxe|expanded|(?:\d+(?:st|nd|rd|th)\s+)?anniversary|special|collector'?s|limited|tour|platinum)(?:\s+(?:edition|version))?$",
        r"|^(?:copyright free|free download|out now|audio only|single|ep)$",
    ))
});

struct Marker {
    pattern: Regex,
    name: &'static str,
    credited: bool,
    generic: bool,
}

fn marker(pattern: &str, name: &'static str) -> Marker {
    // Case-insensitive, with no locale.
    Marker {
        pattern: rxi(pattern),
        name,
        credited: false,
        generic: false,
    }
}

fn credited(pattern: &str, name: &'static str, generic: bool) -> Marker {
    Marker {
        pattern: rxi(pattern),
        name,
        credited: true,
        generic,
    }
}

/// Version markers, most specific first. Each match is taken out of the text
/// before the next is looked for, so "Extended Mix" is extended and not also a mix.
static MARKERS: LazyLock<Vec<Marker>> = LazyLock::new(|| {
    vec![
        marker(r"\bradio\s+(?:edit|version|mix|cut)\b", "radio edit"),
        marker(r"\bextended(?:\s+(?:mix|version|edit|cut))?\b", "extended"),
        marker(r"\boriginal\s+(?:mix|version)\b", "original"),
        marker(r"\b(?:album|lp)\s+version\b", "album version"),
        marker(r"\bsingle\s+version\b", "single version"),
        marker(
            r"\b(?:\d{4}\s+)?(?:digital(?:ly)?\s+)?re-?master(?:ed)?(?:\s+\d{4})?(?:\s+(?:version|edition))?\b",
            "remaster",
        ),
        marker(r"\blive\b", "live"),
        marker(r"\bunplugged\b", "unplugged"),
        marker(r"\bacoustic(?:\s+version)?\b", "acoustic"),
        marker(r"\binstrumental(?:\s+version)?\b", "instrumental"),
        marker(r"\b(?:a\s*cappella|acapella)\b", "acapella"),
        marker(r"\bdemo(?:\s+version)?\b", "demo"),
        marker(r"\b(?:sped\s*up|speed\s*up)\b", "sped up"),
        marker(r"\bslowed(?:\s+down)?\b", "slowed"),
        marker(r"\breverb(?:ed)?\b", "reverb"),
        marker(r"\bnightcore\b", "nightcore"),
        // Editions that change what is played, unlike a deluxe or anniversary one (NEUTRAL_PHRASE).
        marker(r"\bdrumless\b", "drumless"),
        marker(r"\b8d(?:\s+audio)?\b", "8d"),
        marker(r"\bpiano(?:\s+(?:version|edition|arrangement))?\b", "piano"),
        marker(
            r"\b(?:orchestral|symphonic)(?:\s+(?:version|edition|mix))?\b",
            "orchestral",
        ),
        marker(r"\blo-?fi(?:\s+(?:version|edit|mix))?\b", "lofi"),
        marker(r"\bbass\s*boost(?:ed)?\b", "bass boosted"),
        // Two songs or more in one: never the song asked for. Ahead of "mix", so a megamix is not a mix.
        marker(r"\bmash[\s-]?ups?\b", "mashup"),
        marker(r"\bmedley\b", "medley"),
        marker(r"\bmega[\s-]?mix\b", "megamix"),
        marker(r"\bblend\b", "blend"),
        credited(r"^(.*?)\s*\b(?:re-?mix(?:ed)?|rmx)\b", "remix", false),
        marker(r"\bvip(?:\s+mix)?\b", "vip"),
        marker(r"\bbootleg\b", "bootleg"),
        marker(r"\brework(?:ed)?\b", "rework"),
        credited(r"^(.*?)\s*\bdub(?:\s+(?:mix|version))?\b", "dub", false),
        marker(r"\b(?:clean|censored)(?:\s+(?:version|edit))?\b", "clean"),
        marker(r"\b(?:explicit|dirty)(?:\s+version)?\b", "explicit"),
        marker(r"\bmono(?:\s+(?:version|mix))?\b", "mono"),
        marker(r"\bstereo(?:\s+(?:version|mix))?\b", "stereo"),
        marker(
            r"\bkaraoke(?:\s+version)?\b|\boriginally performed by\b|\bin the style of\b|\bmade (?:popular|famous) by\b|\bbacking (?:version|track)\b",
            "karaoke",
        ),
        marker(r"\bcover(?:\s+version)?\b", "cover"),
        marker(r"\breprise\b", "reprise"),
        marker(r"\bsessions?\b", "session"),
        credited(r"^(.*?)\s*\bmix\b", "mix", true),
        credited(r"^(.*?)\s*\bedit\b", "edit", true),
        Marker {
            pattern: rxi(r"\bversion\b"),
            name: "version",
            credited: false,
            generic: true,
        },
    ]
});

/// Words in front of a mix that name its kind, not who made it.
const NOT_A_CREDIT: [&str; 12] = [
    "", "the", "official", "club", "dance", "house", "main", "short", "long", "full", "new", "a",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Noise,
    Feature,
    Part,
    Version,
    Extra,
}

struct Reading {
    kind: Kind,
    versions: Vec<String>,
    credits: Vec<String>,
    names: Vec<String>,
}

impl Reading {
    fn of(kind: Kind) -> Self {
        Self {
            kind,
            versions: Vec::new(),
            credits: Vec::new(),
            names: Vec::new(),
        }
    }
}

/// What one bracket or " - " tail says.
fn read(inner: &str) -> Reading {
    let text = inner.trim().trim_matches(['-', ':', ',', ' ']);
    if text.is_empty() {
        return Reading::of(Kind::Noise);
    }

    if text
        .split([' ', '-'])
        .filter(|word| !word.is_empty())
        .all(is_upload_noise)
    {
        return Reading::of(Kind::Noise);
    }

    if let Some(feature) = FEATURE_LEAD.captures(text) {
        return Reading {
            names: split_names(&feature[1]).0,
            ..Reading::of(Kind::Feature)
        };
    }

    let digits_view = utf16_class_view(text);
    if PART_NUMBER.is_match(&digits_view) {
        return Reading::of(Kind::Part);
    }
    if NEUTRAL_PHRASE.is_match(&word_boundary_view(text)) || YEAR.is_match(&digits_view) {
        return Reading::of(Kind::Noise);
    }

    let mut versions: Vec<String> = Vec::new();
    let mut credits: Vec<String> = Vec::new();
    let mut rest = text.to_string();
    for marker in MARKERS.iter() {
        if marker.generic && !versions.is_empty() {
            continue;
        }
        let (whole, credit) = {
            let view = word_boundary_view(&rest);
            let Some(caps) = marker.pattern.captures(&view) else {
                continue;
            };
            let credit = caps.get(1).map(|group| SongIdentity::key(&rest[group.range()]));
            (caps.get(0).expect("group 0 is the match").range(), credit)
        };
        if !versions.iter().any(|version| version == marker.name) {
            versions.push(marker.name.to_string());
        }
        if marker.credited {
            let credit = credit.unwrap_or_default();
            if !NOT_A_CREDIT.contains(&credit.as_str()) {
                credits.push(credit);
            }
        }
        rest.replace_range(whole, " ");
    }
    let kind = if versions.is_empty() {
        Kind::Extra
    } else {
        Kind::Version
    };
    Reading {
        kind,
        versions,
        credits,
        names: Vec::new(),
    }
}

/// What a title's brackets and tails have said so far.
#[derive(Default)]
struct TitleParts {
    versions: Vec<String>,
    remixers: Vec<String>,
    featured: Vec<String>,
    extras: Vec<String>,
    parts: Vec<String>,
}

impl TitleParts {
    fn take(&mut self, reading: Reading, original: &str) {
        match reading.kind {
            Kind::Feature => self.featured.extend(reading.names),
            Kind::Part => self.parts.push(original.trim().to_string()),
            Kind::Version => {
                for version in reading.versions {
                    if !self.versions.contains(&version) {
                        self.versions.push(version);
                    }
                }
                self.remixers.extend(reading.credits);
            }
            Kind::Extra => self.extras.push(SongIdentity::key(original)),
            Kind::Noise => {}
        }
    }
}

/// The values with repeats dropped, each kept where it first appears.
fn distinct(values: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    values
        .into_iter()
        .filter(|value| seen.insert(value.clone()))
        .collect()
}

fn remove_insert_space(text: &str, range: std::ops::Range<usize>) -> String {
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..range.start]);
    out.push(' ');
    out.push_str(&text[range.end..]);
    out
}

// ---- artists ------------------------------------------------------------------------

/// Names that contain a separator and are still one artist. Keys.
static KNOWN_NAMES: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    HashSet::from([
        "simonandgarfunkel",
        "earthwindandfire",
        "tylerthecreator",
        "acdc",
        "crosbystillsandnash",
        "crosbystillsnashandyoung",
        "emersonlakeandpalmer",
        "bloodsweatandtears",
        "peterpaulandmary",
        "hallandoates",
        "darylhallandjohnoates",
        "mumfordandsons",
        "ofmonstersandmen",
        "belleandsebastian",
        "chaseandstatus",
        "nicoandvinz",
        "macklemoreandryanlewis",
        "samanddave",
        "peachesandherb",
        "brooksanddunn",
        "bigandrich",
        "danandshay",
        "mattandkim",
        "sheandhim",
        "ironandwine",
        "angusandjuliastone",
        "yearsandyears",
        "coheedandcambria",
        "chloexhalle",
        "aura",
        "axwellingrosso",
        "aboveandbeyond",
        "alyandfila",
        "gabrielanddresden",
        "dimitrivegasandlikemike",
        "sonnyandcher",
        "captainandtennille",
        "ikeandtinaturner",
        "teganandsara",
        "ashfordandsimpson",
        "shovelsandrope",
        "florenceandthemachine",
    ])
});

/// Other names an artist goes by, as keys, to one canonical key. Small on purpose:
/// renames and stage names that sources really do disagree on.
static ALIASES: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
    HashMap::from([
        ("ye", "kanyewest"),
        ("kanye", "kanyewest"),
        ("2pac", "2pac"),
        ("tupac", "2pac"),
        ("tupacshakur", "2pac"),
        ("makaveli", "2pac"),
        ("diddy", "diddy"),
        ("pdiddy", "diddy"),
        ("puffdaddy", "diddy"),
        ("seancombs", "diddy"),
        ("snooplion", "snoopdogg"),
        ("snoopdoggydogg", "snoopdogg"),
        ("yasiinbey", "mosdef"),
        ("biggie", "notoriousbig"),
        ("biggiesmalls", "notoriousbig"),
        ("thenotoriousbig", "notoriousbig"),
        ("donaldglover", "childishgambino"),
        ("princeandthenewpowergeneration", "prince"),
        ("theartistformerlyknownasprince", "prince"),
    ])
});

/// The name each artist in [`ALIASES`] is best known by, written the way
/// the catalogs write it, keyed by the canonical key the aliases lead to.
static ALIAS_NAMES: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
    HashMap::from([
        ("kanyewest", "Kanye West"),
        ("2pac", "2Pac"),
        ("diddy", "Diddy"),
        ("snoopdogg", "Snoop Dogg"),
        ("mosdef", "Mos Def"),
        ("notoriousbig", "The Notorious B.I.G."),
        ("childishgambino", "Childish Gambino"),
        ("prince", "Prince"),
    ])
});

/// The separator, `...|\sx\s(?!(?:feat|ft|featuring|with|and|x)\b|[&,;/])|...`, with the
/// `x` branch captured as group 1 and its negative lookahead checked by
/// [`X_SEPARATOR_REFUSED`] after the match.
static ARTIST_SEPARATOR: LazyLock<Regex> = LazyLock::new(|| {
    rxi(
        r"\s*(?:,|;|/|、|×|&|\s\+\s|\s[•·]\s|(\sx\s)|\s(?:and|with|feat\.?|ft\.?|featuring|vs\.?|pres\.|presents)\s)\s*",
    )
});

/// What may not follow an " x " for it to be a separator.
static X_SEPARATOR_REFUSED: LazyLock<Regex> =
    LazyLock::new(|| rxi(r"^(?:(?:feat|ft|featuring|with|and|x)\b|[&,;/])"));

/// The channel suffix `(?:\s+-\s+topic|(?<=\p{Ll})vevo|\s+vevo)$`, in its parts.
static CHANNEL_TOPIC: LazyLock<Regex> = LazyLock::new(|| rxi(r"\s+-\s+topic$"));
static CHANNEL_SPACED_VEVO: LazyLock<Regex> = LazyLock::new(|| rxi(r"\s+vevo$"));

static HAS_LATIN: LazyLock<Regex> = LazyLock::new(|| rx(r"[A-Za-z]"));

/// The separators of an artist credit, every match of [`ARTIST_SEPARATOR`] in order.
fn artist_separators(text: &str) -> Vec<std::ops::Range<usize>> {
    let mut found = Vec::new();
    let mut at = 0;
    while at <= text.len() {
        let Some(caps) = ARTIST_SEPARATOR.captures_at(text, at) else {
            break;
        };
        let whole = caps.get(0).expect("group 0 is the match");
        if let Some(x) = caps.get(1)
            && X_SEPARATOR_REFUSED.is_match(&word_boundary_view(&text[x.end()..]))
        {
            // The lookahead refuses this " x ", and no other branch can match where it
            // started, so the backtracking engine moved on one character; so does this.
            at = whole.start() + text[whole.start()..].chars().next().map_or(1, char::len_utf8);
            continue;
        }
        found.push(whole.range());
        at = whole.end();
    }
    found
}

/// The text without its channel suffix: " - Topic", "ArtistVEVO" and "Artist VEVO" lose the
/// channel name.
fn strip_channel_suffix(text: &str) -> &str {
    if let Some(found) = CHANNEL_TOPIC.find(text) {
        return &text[..found.start()];
    }
    if let Some(found) = CHANNEL_SPACED_VEVO.find(text) {
        return &text[..found.start()];
    }
    // (?<=\p{Ll})vevo$: ignoring case widens \p{Ll} to every cased letter (Lu, Ll, Lt) in one
    // UTF-16 unit, so "DRAKEVEVO" loses its suffix too; a CJK name does not.
    let cut = text.len().wrapping_sub(4);
    if text.len() >= 4
        && text.is_char_boundary(cut)
        && text[cut..].eq_ignore_ascii_case("vevo")
        && text[..cut].chars().next_back().is_some_and(is_cased_letter_utf16)
    {
        return &text[..cut];
    }
    text
}

fn is_joiner(separator: &str) -> bool {
    matches!(to_lower_invariant(separator.trim()).as_str(), "&" | "and" | "+")
}

/// A credit split into its artists, keeping known names and "X & the Y" whole.
fn split_names(credit: &str) -> (Vec<String>, Vec<String>) {
    let text = credit.trim();
    if text.is_empty() {
        return (Vec::new(), Vec::new());
    }
    if KNOWN_NAMES.contains(SongIdentity::key(text).as_str()) {
        return (vec![text.to_string()], Vec::new());
    }

    // (start, end, the separator before it)
    let mut parts: Vec<(usize, usize, &str)> = Vec::new();
    let mut at = 0;
    let mut before = "";
    for separator in artist_separators(text) {
        if separator.start > at {
            parts.push((at, separator.start, before));
        }
        before = &text[separator.clone()];
        at = separator.end;
    }
    if at < text.len() {
        parts.push((at, text.len(), before));
    }
    if parts.is_empty() {
        return (vec![text.to_string()], Vec::new());
    }

    let mut names: Vec<String> = Vec::new();
    let mut starts: Vec<usize> = Vec::new();
    let mut pieces: Vec<String> = Vec::new();
    let mut i = 0;
    while i < parts.len() {
        // The longest run of parts that is a known name, "Tyler, The Creator".
        let mut end = i;
        for j in (i + 1..parts.len()).rev() {
            if KNOWN_NAMES.contains(SongIdentity::key(&text[parts[i].0..parts[j].1]).as_str()) {
                end = j;
                break;
            }
        }
        let name = text[parts[i].0..parts[end].1].trim();

        // "Bob Marley & The Wailers": a "the" after "&" or "and" belongs to the name before.
        if end == i && !names.is_empty() && is_joiner(parts[i].2) && starts_with_ignore_case(name, "the ") {
            let last = names.len() - 1;
            pieces.push(names[last].clone());
            pieces.push(name.to_string());
            names[last] = text[starts[last]..parts[i].1].trim().to_string();
            i += 1;
            continue;
        }
        names.push(name.to_string());
        starts.push(parts[i].0);
        i = end + 1;
    }
    (
        names
            .into_iter()
            .filter(|name| !SongIdentity::key(name).is_empty())
            .collect(),
        pieces
            .into_iter()
            .filter(|piece| !SongIdentity::key(piece).is_empty())
            .collect(),
    )
}

/// Every key a name can be matched by: itself, without a leading "the", and its
/// alias.
fn keys_of(name: &str, loose: bool, into: &mut HashSet<String>) {
    let key_fn = if loose {
        SongIdentity::loose_key
    } else {
        SongIdentity::key
    };
    let key = key_fn(name);
    if key.is_empty() {
        return;
    }
    let trimmed = SongIdentity::fold(name);
    if starts_with_ignore_case(&trimmed, "the ") {
        // The four characters matched ASCII "the " (nothing else folds to it), so four bytes.
        let bare = key_fn(&trimmed[4..]);
        if !bare.is_empty() {
            into.insert(bare);
        }
    }
    if let Some(alias) = ALIASES.get(key.as_str()) {
        into.insert((*alias).to_string());
    }
    into.insert(key);
}

fn keys<'a>(names: impl IntoIterator<Item = &'a str>, loose: bool) -> HashSet<String> {
    let mut set = HashSet::new();
    for name in names {
        keys_of(name, loose, &mut set);
    }
    set
}

fn overlaps(a: &HashSet<String>, b: &HashSet<String>) -> bool {
    let (small, large) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    small.iter().any(|key| large.contains(key))
}

/// One side's credit, and the groups of names it is matched by.
struct Credit<'a>(&'a SongArtists);

impl<'a> Credit<'a> {
    fn all(&self) -> impl Iterator<Item = &'a str> {
        let artists = self.0;
        artists
            .names
            .iter()
            .chain(&artists.featured)
            .chain(&artists.pieces)
            .map(String::as_str)
            .chain(std::iter::once(artists.display.as_str()))
    }

    fn primary(&self) -> impl Iterator<Item = &'a str> {
        [self.0.primary(), self.0.display.as_str()].into_iter()
    }

    fn named(&self) -> impl Iterator<Item = &'a str> {
        self.0.names.iter().chain(&self.0.featured).map(String::as_str)
    }
}

fn with_credits<S: AsRef<str>>(artists: SongArtists, credits: &[S]) -> SongArtists {
    let listed: Vec<&str> = credits
        .iter()
        .map(|credit| credit.as_ref().trim())
        .filter(|credit| !SongIdentity::key(credit).is_empty())
        .collect();
    if listed.is_empty() {
        return artists;
    }
    let names: Vec<String> = listed
        .iter()
        .map(|credit| SongIdentity::parse_artists(credit).display)
        .collect();
    let display = if artists.is_empty() {
        names.join(" & ")
    } else {
        artists.display.clone()
    };
    let listed_keys: Vec<String> = names.iter().map(|name| SongIdentity::key(name)).collect();
    let mut all_names = names;
    all_names.extend(
        artists
            .names
            .iter()
            .filter(|name| !listed_keys.contains(&SongIdentity::key(name)))
            .cloned(),
    );
    SongArtists {
        display,
        names: all_names,
        ..artists
    }
}

/// Whether a name is credited on the other side: one of its keys is one there, or
/// holds or is held in one of the other side's names (never its whole credit, which holds
/// every name).
fn credited_on(keys_of_name: &HashSet<String>, other: &Credit) -> bool {
    if overlaps(keys_of_name, &keys(other.all(), true)) {
        return true;
    }
    let named = keys(
        other.named().chain(other.0.pieces.iter().map(String::as_str)),
        true,
    );
    keys_of_name.iter().any(|key| {
        named.iter().any(|name| {
            utf16_len(key).min(utf16_len(name)) >= 3
                && (key.contains(name.as_str()) || name.contains(key.as_str()))
        })
    })
}

/// Whether a name is the artist, in any of the forms a credit is matched by.
fn names_artist(name: &str, artist: &str) -> bool {
    let candidate = SongIdentity::parse_artists(name);
    let parsed = SongIdentity::parse_artists(artist);
    let credit = Credit(&parsed);
    overlaps(
        &keys([candidate.display.as_str()], false),
        &keys(credit.all(), false),
    ) || overlaps(
        &keys([candidate.display.as_str()], true),
        &keys(credit.all(), true),
    )
}

// ---- ISRCs --------------------------------------------------------------------------

/// Two letters of country, three of registrant, two digits of year, five of designation.
static ISRC_SHAPE: LazyLock<Regex> = LazyLock::new(|| rx(r"^[A-Z]{2}[A-Z0-9]{3}[0-9]{7}$"));

// ---- comparing ----------------------------------------------------------------------

static DIGITS: LazyLock<Regex> = LazyLock::new(|| rx(r"\d+"));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TitleAgreement {
    None,
    Exact,
    Loose,
    Numbers,
    /// The same title written in two scripts, read through their Latin spellings.
    CrossScript,
    /// A title a typo away, with the guards of [`near_title`].
    NearTitle,
}

fn compare_keys(a: &SongTitle, b: &SongTitle) -> TitleAgreement {
    if a.key.is_empty() || b.key.is_empty() {
        return TitleAgreement::None;
    }
    if a.key == b.key {
        return TitleAgreement::Exact;
    }
    if a.loose_key == b.loose_key {
        return TitleAgreement::Loose;
    }

    // Numbers compare as sets: "Vol. 53" is "Vol. 53/66", but "Shotta Flow" is never
    // "Shotta Flow 4".
    let letters_a: String = a.key.chars().filter(|&ch| is_letter_utf16(ch)).collect();
    let letters_b: String = b.key.chars().filter(|&ch| is_letter_utf16(ch)).collect();
    if letters_a.is_empty() || letters_a != letters_b {
        return TitleAgreement::None;
    }
    let (keyed_a, keyed_b) = (utf16_class_view(&a.keyed), utf16_class_view(&b.keyed));
    let numbers_a: HashSet<&str> = DIGITS.find_iter(&keyed_a).map(|found| found.as_str()).collect();
    let numbers_b: HashSet<&str> = DIGITS.find_iter(&keyed_b).map(|found| found.as_str()).collect();
    if numbers_a.is_empty() || numbers_b.is_empty() {
        return TitleAgreement::None;
    }
    if numbers_a.is_subset(&numbers_b) || numbers_b.is_subset(&numbers_a) {
        TitleAgreement::Numbers
    } else {
        TitleAgreement::None
    }
}

/// The key a title is compared by across scripts: its Latin spelling when it has letters
/// outside ASCII, else its key. Only computed for a lookup.
fn cross_key(title: &SongTitle) -> String {
    if has_non_ascii_letters(&title.keyed) {
        romanized_key(&title.keyed)
    } else {
        title.key.clone()
    }
}

/// The same title in two scripts: the cores start in different scripts and their Latin
/// spellings key the same. Two titles in one script never compare this way, since a spelling
/// loses tones and two different Chinese titles can share one.
fn same_across_scripts(a: &SongTitle, b: &SongTitle) -> bool {
    let (script_a, script_b) = (first_script(&a.core), first_script(&b.core));
    if script_a.is_none() || script_b.is_none() || script_a == script_b {
        return false;
    }
    let key = cross_key(a);
    !key.is_empty() && key == cross_key(b)
}

/// A roman numeral from 1 to 39, the way a sequel or a part is numbered.
static ROMAN_NUMERAL: LazyLock<Regex> = LazyLock::new(|| rx(r"^x{0,3}(?:ix|iv|v?i{0,3})$"));

fn title_words(title: &SongTitle) -> Vec<String> {
    words(&title.keyed)
        .split(' ')
        .filter(|word| !word.is_empty())
        .map(str::to_string)
        .collect()
}

/// A title a typo away: both lengths known and within the tolerance, the shorter key at least six
/// letters, the same numbers, the same trailing roman numeral, the same number of words, and an
/// indel ratio of at least 0.90 between the keys. Short titles ("Mine" and "Mind"), plurals
/// ("Angel" and "Angels") and numbered parts never pass.
fn near_title(a: &SongTitle, b: &SongTitle, options: &SongMatchOptions, lengths: Option<(f64, f64)>) -> bool {
    let (Some(tolerance), Some((seconds_a, seconds_b))) = (options.length_tolerance_seconds, lengths) else {
        return false;
    };
    if (seconds_a - seconds_b).abs() > f64::from(tolerance) {
        return false;
    }
    let letters = |key: &str| key.chars().filter(|&ch| is_letter_utf16(ch)).count();
    if letters(&a.key).min(letters(&b.key)) < 6 {
        return false;
    }
    let (words_a, words_b) = (title_words(a), title_words(b));
    if words_a.len() != words_b.len() {
        return false;
    }
    let numbers = |title: &SongTitle| -> BTreeSet<String> {
        DIGITS
            .find_iter(&utf16_class_view(&title.keyed))
            .map(|found| found.as_str().trim_start_matches('0').to_string())
            .collect()
    };
    if numbers(a) != numbers(b) {
        return false;
    }
    let roman = |words: &[String]| words.last().filter(|word| ROMAN_NUMERAL.is_match(word)).cloned();
    if roman(&words_a) != roman(&words_b) {
        return false;
    }
    ratio(&a.key, &b.key) >= 0.90
}

/// [`compare_keys`], then the lookup tiers the options allow when the keys disagree.
fn compare_keys_lookup(
    a: &SongTitle,
    b: &SongTitle,
    options: &SongMatchOptions,
    lengths: Option<(f64, f64)>,
) -> TitleAgreement {
    let agreement = compare_keys(a, b);
    if agreement != TitleAgreement::None
        || options.lookup == Lookup::Off
        || a.key.is_empty()
        || b.key.is_empty()
    {
        return agreement;
    }
    if same_across_scripts(a, b) {
        return TitleAgreement::CrossScript;
    }
    if options.lookup == Lookup::Pick && near_title(a, b, options, lengths) {
        return TitleAgreement::NearTitle;
    }
    TitleAgreement::None
}

const CREDITED_VERSIONS: [&str; 4] = ["remix", "mix", "dub", "edit"];

/// `lengths` is both lengths, when both are known.
fn compare_titles(
    a: &SongTitle,
    b: &SongTitle,
    options: &SongMatchOptions,
    lengths: Option<(f64, f64)>,
) -> SongMatch {
    let agreement = compare_keys_lookup(a, b, options, lengths);
    if agreement == TitleAgreement::None {
        let reason = if a.key.is_empty() || b.key.is_empty() {
            "a title is empty".to_string()
        } else {
            format!("different titles ('{}' and '{}')", a.core, b.core)
        };
        return SongMatch::new(SongVerdict::Different, 0.95, reason);
    }

    let mut confidence = 1.0;
    let mut reasons: Vec<&str> = Vec::new();
    if agreement == TitleAgreement::Loose {
        confidence -= 0.15;
        reasons.push("the same title once stylized characters are read as letters");
    }
    if agreement == TitleAgreement::Numbers {
        confidence -= 0.1;
        reasons.push("the same title with numbers that overlap");
    }
    if agreement == TitleAgreement::CrossScript {
        confidence -= 0.1;
        reasons.push("the same title in another script");
    }
    if agreement == TitleAgreement::NearTitle {
        confidence -= 0.2;
        reasons.push("nearly the same title");
    }

    let versions_a = SongIdentity::distinct_versions(a, Some(options));
    let versions_b = SongIdentity::distinct_versions(b, Some(options));
    if versions_a != versions_b {
        return SongMatch::new(
            SongVerdict::SameSongDifferentVersion,
            0.9,
            format!(
                "a different version ({} against {})",
                describe(&versions_a),
                describe(&versions_b)
            ),
        );
    }

    if versions_a
        .iter()
        .any(|version| CREDITED_VERSIONS.contains(&version.as_str()))
    {
        if !a.remixers.is_empty()
            && !b.remixers.is_empty()
            && !a.remixers.iter().any(|remixer| b.remixers.contains(remixer))
        {
            return SongMatch::new(
                SongVerdict::SameSongDifferentVersion,
                0.85,
                "remixes by different people",
            );
        }
        if a.remixers.is_empty() != b.remixers.is_empty() {
            confidence -= 0.05;
            reasons.push("only one says who made the remix");
        }
    }

    if !a.extras.is_empty() && !b.extras.is_empty() && !a.extras.iter().any(|extra| b.extras.contains(extra))
    {
        return SongMatch::new(SongVerdict::Different, 0.7, "different subtitles");
    }
    if a.extras.is_empty() != b.extras.is_empty() {
        if options.extras_must_agree {
            return SongMatch::new(SongVerdict::Different, 0.7, "only one title has a subtitle");
        }
        confidence -= 0.1;
        reasons.push("only one title has a subtitle");
    }

    let reason = if reasons.is_empty() {
        "the same title".to_string()
    } else {
        reasons.join("; ")
    };
    SongMatch::new(SongVerdict::Same, round(confidence, 2), reason)
}

fn describe(versions: &BTreeSet<String>) -> String {
    if versions.is_empty() {
        "the original".to_string()
    } else {
        versions
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(" + ")
    }
}

/// The artist credit of one side, with the guests its title names, and the artist
/// an "Artist - Title" title gave when the credit itself is empty.
fn credit_of(artist: &str, title: &SongTitle) -> SongArtists {
    let mut credit = SongIdentity::parse_artists(credit_text(artist, title));
    credit.featured.extend(title.featured.iter().cloned());
    credit
}

/// The credit when it says anything, else the artist an "Artist - Title" title named.
fn credit_text<'a>(artist: &'a str, title: &'a SongTitle) -> &'a str {
    if is_blank(artist) {
        title.artist_from_title.as_deref().unwrap_or("")
    } else {
        artist
    }
}

/// Credits that say a song is someone else playing it. A credit with one of these words is never
/// read as the same artists as one without.
const TRIBUTE_WORDS: [&str; 6] = ["tribute", "karaoke", "cover", "covers", "rendition", "renditions"];

/// Credits that name nobody, as keys.
const PLACEHOLDER_ARTISTS: [&str; 9] = [
    "variousartists",
    "variousartist",
    "various",
    "va",
    "unknownartist",
    "unknown",
    "releasetopic",
    "release",
    "artist",
];

fn is_placeholder(name: &str) -> bool {
    let key = SongIdentity::key(name);
    key.is_empty() || PLACEHOLDER_ARTISTS.contains(&key.as_str())
}

/// The words of two names in any order, the better of as written and spelled in Latin letters.
fn names_alike(a: &str, b: &str) -> f64 {
    let written = token_sort_ratio(&words(a), &words(b));
    if has_non_ascii_letters(a) || has_non_ascii_letters(b) {
        written.max(token_sort_ratio(&romanized_words(a), &romanized_words(b)))
    } else {
        written
    }
}

/// The tribute words a credit carries, read from the credit as given, so a bracketed "(Karaoke)"
/// that the parsed credit drops still counts.
fn tribute_words(artists: &SongArtists) -> BTreeSet<&'static str> {
    let text = if has_non_ascii_letters(&artists.raw) {
        romanized_words(&artists.raw)
    } else {
        words(&artists.raw)
    };
    let found: HashSet<&str> = text.split(' ').collect();
    TRIBUTE_WORDS
        .iter()
        .copied()
        .filter(|word| found.contains(word))
        .collect()
}

/// Two credits that share no artist by key, read as the same artists written another way: the
/// tribute words agree, the whole credits have the same words allowing a typo, and so does every
/// name of the credit with fewer names against some name of the other ("Fonsi, Luis" and "Luis
/// Fonsi"). A placeholder such as "Various Artists" never passes.
fn credits_alike(a: &SongArtists, b: &SongArtists) -> bool {
    if is_placeholder(&a.display) || is_placeholder(&b.display) || tribute_words(a) != tribute_words(b) {
        return false;
    }
    let named = |artists: &SongArtists| -> Vec<String> {
        artists
            .names
            .iter()
            .chain(&artists.featured)
            .filter(|name| !is_placeholder(name))
            .cloned()
            .collect()
    };
    let (names_a, names_b) = (named(a), named(b));
    if names_a.is_empty() || names_b.is_empty() || names_alike(&a.display, &b.display) < 0.92 {
        return false;
    }
    let (short, long) = if names_a.len() <= names_b.len() {
        (&names_a, b)
    } else {
        (&names_b, a)
    };
    let others: Vec<&str> = Credit(long).all().filter(|name| !is_placeholder(name)).collect();
    short
        .iter()
        .all(|name| others.iter().any(|other| names_alike(name, other) >= 0.92))
}

/// Both lengths, when both are known.
fn known_lengths(a: &SongRef, b: &SongRef) -> Option<(f64, f64)> {
    match (a.seconds, b.seconds) {
        (Some(seconds_a), Some(seconds_b)) if seconds_a > 0.0 && seconds_b > 0.0 => {
            Some((seconds_a, seconds_b))
        }
        _ => None,
    }
}

/// How alike two titles read, 0 to 1, for ranking: the best of their words in any order and
/// Jaro-Winkler over their keys, as written and spelled in Latin letters.
fn title_similarity(a: &SongTitle, b: &SongTitle) -> f64 {
    let mut best = token_sort_ratio(&words(&a.keyed), &words(&b.keyed)).max(jaro_winkler(&a.key, &b.key));
    if has_non_ascii_letters(&a.keyed) || has_non_ascii_letters(&b.keyed) {
        best = best
            .max(token_sort_ratio(
                &romanized_words(&a.keyed),
                &romanized_words(&b.keyed),
            ))
            .max(jaro_winkler(&cross_key(a), &cross_key(b)));
    }
    best
}

/// How alike two credits read, 0 to 1, for ranking: 1 when they share an artist, else the best of
/// the whole credits and the primary artists compared word by word.
fn artist_similarity(a: &SongArtists, b: &SongArtists) -> f64 {
    if matches!(
        SongIdentity::compare_parsed_artists(a, b),
        ArtistAgreement::Agree | ArtistAgreement::Loose
    ) {
        return 1.0;
    }
    names_alike(&a.display, &b.display).max(names_alike(a.primary(), b.primary()))
}

fn versioned_key(parsed: &SongTitle) -> String {
    let versions = SongIdentity::distinct_versions(parsed, None);
    if versions.is_empty() {
        parsed.key.clone()
    } else {
        format!(
            "{}|{}",
            parsed.key,
            versions.iter().map(String::as_str).collect::<Vec<_>>().join("+")
        )
    }
}

impl SongIdentity {
    /// How far apart two lengths may be and still be one recording.
    pub const LENGTH_TOLERANCE_SECONDS: i32 = 3;

    /// Version markers that never make a different recording.
    pub const NEUTRAL_VERSIONS: [&'static str; 7] = NEUTRAL_VERSIONS;

    // ---- folding ------------------------------------------------------------------------

    /// The text with its lookalikes made plain, case and accents untouched: NFKC, curly quotes
    /// and dashes folded, underscores as spaces, whitespace collapsed. What every other reading
    /// starts from, and what a query is sent as.
    pub fn fold(value: &str) -> String {
        if is_blank(value) {
            return String::new();
        }
        let text: String = value.replace('\u{00B4}', "'").nfkc().collect();
        let mapped: String =
            text.chars()
                .map(|ch| match ch {
                    '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' | '\u{2032}' | '`' => '\'',
                    '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' | '\u{2033}' => '"',
                    '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
                    | '\u{2212}' => '-',
                    '\u{3010}' | '\u{3016}' => '[',
                    '\u{3011}' | '\u{3017}' => ']',
                    '\u{3014}' => '(',
                    '\u{3015}' => ')',
                    '_' => ' ',
                    c if c.is_whitespace() => ' ',
                    c => c,
                })
                .collect();
        WHITESPACE
            .replace_all(&fold_mixed_script_words(&mapped), " ")
            .trim()
            .to_string()
    }

    /// The exact key: casefolded, accents stripped, "&" read as "and", and only
    /// letters, digits and combining marks kept. Symbols only when there is nothing else.
    pub fn key(value: &str) -> String {
        let lower = lower_fold(&Self::fold(value));
        if lower.is_empty() {
            return String::new();
        }
        let kept: String = lower
            .chars()
            .filter(|&ch| is_letter_or_digit(ch) || is_combining_mark(ch))
            .collect();
        if !kept.is_empty() {
            return kept;
        }
        // "!!!" or an emoji: nothing is a letter, so the symbols are the name.
        lower.chars().filter(|ch| !ch.is_whitespace()).collect()
    }

    /// The key with stylized characters read as letters, so "$uicideboy$" and
    /// "Suicideboys" agree. Only ever an additional key.
    pub fn loose_key(value: &str) -> String {
        Self::key(&Self::fold_stylized(&Self::fold(value)))
    }

    /// Stylized characters read as the letters they stand for: $ as s, @ as a, 0 as o and 3 as
    /// e when they sit against a letter, ! as i between two letters. "2003", "Blink-182" and
    /// "Help!" are left alone.
    pub fn fold_stylized(value: &str) -> String {
        if value.is_empty() {
            return String::new();
        }
        // Letters, digits and uppercase are tested on UTF-16 units; a supplementary character's
        // surrogates are none of letter, digit or upper, and neither is the char here.
        let chars: Vec<char> = value.chars().collect();
        let mut out = String::with_capacity(value.len());
        for (i, &ch) in chars.iter().enumerate() {
            let prev = if i > 0 { chars[i - 1] } else { ' ' };
            let next = chars.get(i + 1).copied().unwrap_or(' ');
            let letter_beside = is_letter_utf16(prev) || is_letter_utf16(next);
            let digit_beside = is_digit_utf16(prev) || is_digit_utf16(next);
            let upper = if is_letter_utf16(next) {
                is_upper_utf16(next)
            } else {
                is_upper_utf16(prev)
            };
            let read = match ch {
                '$' if letter_beside => Some('s'),
                '@' if letter_beside => Some('a'),
                '!' if is_letter_utf16(prev) && is_letter_utf16(next) => Some('i'),
                '0' if letter_beside && !digit_beside => Some('o'),
                '3' if letter_beside && !digit_beside => Some('e'),
                _ => None,
            };
            out.push(match read {
                Some(letter) if upper => to_upper_char(letter),
                Some(letter) => letter,
                None => ch,
            });
        }
        out
    }

    /// The folded text lowercased and without accents, spacing and punctuation kept:
    /// for matching words inside something that is not a title, such as a file name.
    pub fn plain(value: &str) -> String {
        lower_fold(&Self::fold(value))
    }

    // ---- titles -------------------------------------------------------------------------

    /// A title read into its core, its key, its version markers and its guests. The
    /// artist, when given, lets a leading "Artist - " be recognized; an artist given as empty
    /// means the song has none, and then any "X - Y" title is read as artist X and title Y. None
    /// reads the title alone.
    pub fn parse_title(title: &str, artist: Option<&str>) -> SongTitle {
        let raw = title;
        let mut text = Self::fold(raw);
        let mut found = TitleParts::default();
        let mut artist_from_title: Option<String> = None;

        let numbered = TRACK_NUMBER
            .captures(&utf16_class_view(&text))
            .map(|numbered| numbered.get(1).expect("the character after the number").start());
        if let Some(length) = numbered
            && text[length..].chars().any(is_letter_utf16)
        {
            text = text[length..].to_string();
        }

        // "Artist - Title": the artist named in front, or any name at all when no artist was given.
        if let Some(dash) = DASH_TAIL.find(&text)
            && dash.start() > 0
        {
            let left = &text[..dash.start()];
            let right = &text[dash.end()..];
            let tail = read(right);
            let named = matches!(artist, Some(artist) if !is_blank(artist) && names_artist(left, artist));
            let no_artist = matches!(artist, Some(artist) if is_blank(artist))
                && tail.kind == Kind::Extra
                && !Self::key(right).is_empty();
            if (named && !Self::key(right).is_empty()) || no_artist {
                artist_from_title = Some(left.trim().to_string());
                text = right.to_string();
            }
        }

        for _guard in 0..12 {
            let Some(caps) = BRACKET.captures(&text) else {
                break;
            };
            let whole = caps.get(0).expect("group 0 is the match").range();
            let inner = caps[1].to_string();
            let rest = remove_insert_space(&text, whole).trim().to_string();
            // A title that is nothing but a bracket, "(Exchange)", is the words inside it.
            if Self::key(&rest).is_empty() && matches!(read(&inner).kind, Kind::Extra | Kind::Noise) {
                text = inner;
                break;
            }
            found.take(read(&inner), &inner);
            text = rest;
        }
        if let Some(open) = OPEN_BRACKET.captures(&text) {
            let inner = open[1].to_string();
            let start = open.get(0).expect("group 0 is the match").start();
            found.take(read(&inner), &inner);
            text.truncate(start);
        }

        // " - Live at Wembley", " - Remastered 2011": only a tail that says what kind of
        // recording this is. "Pt. 2 - The Return" keeps its tail.
        for _guard in 0..4 {
            let Some(last) = DASH_TAIL.find_iter(&text).last() else {
                break;
            };
            if last.start() == 0 {
                break;
            }
            let tail = text[last.end()..].to_string();
            let reading = read(&tail);
            if !matches!(reading.kind, Kind::Version | Kind::Noise | Kind::Feature) {
                break;
            }
            found.take(reading, &tail);
            text.truncate(last.start());
        }

        if let Some(trailing) = TRAILING_FEATURE.captures(&text) {
            let start = trailing.get(0).expect("group 0 is the match").start();
            if start > 0 {
                found.featured.extend(split_names(&trailing[1]).0);
                text.truncate(start);
            }
        }

        for _guard in 0..3 {
            let Some(trailing) = TRAILING_VERSION.captures(&text) else {
                break;
            };
            let start = trailing.get(0).expect("group 0 is the match").start();
            if start == 0 {
                break;
            }
            let word = trailing[1].to_string();
            found.take(read(&word), &word);
            text.truncate(start);
        }

        let collapsed = WHITESPACE.replace_all(&text, " ");
        let mut core = collapsed
            .trim()
            .trim_end_matches(['-', ':', ',', '/', ' '])
            .trim()
            .to_string();
        if Self::key(&core).is_empty() {
            // Nothing is left but brackets or symbols: the whole title is the name.
            let unbracketed = Self::fold(raw).replace(['(', ')', '[', ']'], " ");
            core = WHITESPACE.replace_all(&unbracketed, " ").trim().to_string();
            found.extras.clear();
        }
        let with_parts = if found.parts.is_empty() {
            core.clone()
        } else {
            format!("{core} {}", found.parts.join(" "))
        };
        let keyed = replace_in_view(&with_parts, &word_boundary_view(&with_parts), &PART_WORD, part_of);

        found.versions.sort();
        SongTitle {
            raw: raw.to_string(),
            key: Self::key(&keyed),
            loose_key: Self::loose_key(&keyed),
            core,
            versions: found.versions,
            remixers: distinct(found.remixers.into_iter().filter(|remixer| !remixer.is_empty())),
            featured: distinct(
                found
                    .featured
                    .into_iter()
                    .filter(|name| !Self::key(name).is_empty()),
            ),
            extras: distinct(found.extras.into_iter().filter(|extra| !extra.is_empty())),
            artist_from_title,
            keyed,
        }
    }

    /// The markers of a title that make it a different recording, for this caller, in sorted
    /// order.
    pub fn distinct_versions(title: &SongTitle, options: Option<&SongMatchOptions>) -> BTreeSet<String> {
        title
            .versions
            .iter()
            .filter(|version| {
                !NEUTRAL_VERSIONS.contains(&version.as_str())
                    && !options.is_some_and(|options| options.also_neutral.contains(version))
            })
            .cloned()
            .collect()
    }

    /// Version markers the candidate carries that the request does not ("Song (Live)"
    /// for "Song"), empty when it adds none. One-directional, for a request whose title may be
    /// more specific than its match's.
    pub fn added_versions(
        requested: &str,
        candidate: &str,
        options: Option<&SongMatchOptions>,
    ) -> BTreeSet<String> {
        let want = Self::distinct_versions(&Self::parse_title(requested, None), options);
        Self::distinct_versions(&Self::parse_title(candidate, None), options)
            .into_iter()
            .filter(|version| !want.contains(version))
            .collect()
    }

    /// The title without its guest credits, as a person would write it.
    pub fn strip_features(title: &str) -> String {
        let text = title.trim();
        let text = BRACKET.replace_all(text, |caps: &Captures| {
            let inner = &caps[1];
            if read(&Self::fold(inner)).kind == Kind::Feature {
                return String::new();
            }
            // "(Radio Edit - feat. Pharrell Williams)": the credit goes, the version stays.
            let start = match INNER_FEATURE.find(&word_boundary_view(inner)) {
                Some(credit) if credit.start() > 0 => credit.start(),
                _ => return caps[0].to_string(),
            };
            let kept = inner[..start]
                .trim()
                .trim_end_matches(['-', '\u{2013}', ',', ';', '/', ' ']);
            if kept.is_empty() {
                String::new()
            } else {
                caps[0].replace(inner, kept)
            }
        });
        let mut text = text.into_owned();
        if let Some(trailing) = TRAILING_FEATURE.find(&text)
            && trailing.start() > 0
        {
            // "Song feat. X (Radio Edit)": the credit goes, a bracket after it stays.
            let rest = &text[trailing.start()..];
            text = match rest.find(['(', '[']) {
                Some(bracket) if bracket > 0 => format!("{} {}", &text[..trailing.start()], &rest[bracket..]),
                _ => text[..trailing.start()].to_string(),
            };
        }
        // "Song - Radio Edit - feat. X" leaves the dash that led to the credit.
        let text = text.trim_end().trim_end_matches(['-', '\u{2013}', ',', ' ']);
        WHITESPACE.replace_all(text, " ").trim().to_string()
    }

    // ---- artists ------------------------------------------------------------------------

    /// An artist credit read into its artists.
    pub fn parse_artists(artist: &str) -> SongArtists {
        let raw = artist;
        let folded = Self::fold(raw);
        let text = strip_channel_suffix(&folded).trim();
        let mut featured: Vec<String> = Vec::new();
        // Every bracket goes: "(feat. X)" is a guest, "Ye (侃爷)" the same artist in another
        // script, and "Nirvana (US)" a disambiguation.
        let text = BRACKET.replace_all(text, |caps: &Captures| {
            if let Some(feature) = FEATURE_LEAD.captures(caps[1].trim()) {
                featured.extend(split_names(&feature[1]).0);
            }
            " "
        });
        let mut text = WHITESPACE.replace_all(&text, " ").trim().to_string();
        if Self::key(&text).is_empty() {
            text = WHITESPACE.replace_all(&folded, " ").trim().to_string();
        }

        // "Drake feat. Rihanna" names its guest after the separator; the names list keeps it.
        let (names, pieces) = split_names(&text);
        SongArtists {
            raw: raw.to_string(),
            display: text,
            names,
            featured,
            pieces,
        }
    }

    /// A name that credits nobody: empty, "Various Artists", "Unknown Artist" and the like.
    pub fn is_placeholder_artist(name: &str) -> bool {
        is_placeholder(name)
    }

    /// The artist a credit names first, as written: "Beyoncé" for "Beyoncé feat. Jay-Z",
    /// "Tyler, The Creator" for itself.
    pub fn primary_artist(artist: &str) -> String {
        Self::parse_artists(artist).primary().to_string()
    }

    /// How two credits relate.
    pub fn compare_artists(a: &str, b: &str) -> ArtistAgreement {
        Self::compare_parsed_artists(&Self::parse_artists(a), &Self::parse_artists(b))
    }

    /// How two credits relate. `b_credits`, when a source lists its artists one by one, is used
    /// instead of splitting `b`; empty is the same as not given.
    pub fn compare_artists_with_credits<S: AsRef<str>>(a: &str, b: &str, b_credits: &[S]) -> ArtistAgreement {
        Self::compare_parsed_artists(
            &Self::parse_artists(a),
            &with_credits(Self::parse_artists(b), b_credits),
        )
    }

    pub fn compare_parsed_artists(a: &SongArtists, b: &SongArtists) -> ArtistAgreement {
        if a.is_empty() || b.is_empty() {
            return ArtistAgreement::Unknown;
        }
        let left = Credit(a);
        let right = Credit(b);

        let mut found: Option<ArtistAgreement> = None;
        for loose in [false, true] {
            let all_left = keys(left.all(), loose);
            let all_right = keys(right.all(), loose);
            if overlaps(&keys(left.primary(), loose), &all_right)
                || overlaps(&keys(right.primary(), loose), &all_left)
            {
                found = Some(if loose {
                    ArtistAgreement::Loose
                } else {
                    ArtistAgreement::Agree
                });
                break;
            }
        }
        let Some(found) = found else {
            return ArtistAgreement::None;
        };

        // "Bizarrap, Duki" against "Bizarrap & Rauw Alejandro": one artist in common, and each
        // names a guest the other does not. A name in another script is not counted, since it
        // may be one of the Latin names written its own way. Nor is a name that holds, or is
        // held in, one on the other side: "feat 2 Chainz B.O.B" never had a separator to split on.
        let left_keys: Vec<HashSet<String>> = left
            .named()
            .filter(|name| HAS_LATIN.is_match(name))
            .map(|name| keys([name], true))
            .collect();
        let right_keys: Vec<HashSet<String>> = right
            .named()
            .filter(|name| HAS_LATIN.is_match(name))
            .map(|name| keys([name], true))
            .collect();
        let only_left = left_keys.iter().any(|keys| !credited_on(keys, &right));
        let only_right = right_keys.iter().any(|keys| !credited_on(keys, &left));
        if only_left && only_right {
            ArtistAgreement::Conflict
        } else {
            found
        }
    }

    /// The two credits share an artist and do not disagree about the guests.
    pub fn artists_agree(a: &str, b: &str) -> bool {
        matches!(
            Self::compare_artists(a, b),
            ArtistAgreement::Agree | ArtistAgreement::Loose
        )
    }

    /// [`SongIdentity::artists_agree`] with `b`'s artists listed one by one.
    pub fn artists_agree_with_credits<S: AsRef<str>>(a: &str, b: &str, b_credits: &[S]) -> bool {
        matches!(
            Self::compare_artists_with_credits(a, b, b_credits),
            ArtistAgreement::Agree | ArtistAgreement::Loose
        )
    }

    // ---- ISRCs --------------------------------------------------------------------------

    /// An ISRC in its one canonical spelling, or None when the value is not one. Sources write
    /// them "USRC17607839", "US-RC1-76-07839", "us rc1 76 07839" and with dots; all of those are
    /// one code. Anything that is still not twelve characters of the right shape once the
    /// separators are gone is treated as absent, never as a code that matches nothing.
    pub fn normalize_isrc(value: &str) -> Option<String> {
        if is_blank(value) {
            return None;
        }
        let isrc: String = value
            .nfkc()
            .filter(|&ch| ch != '-' && ch != '.' && !ch.is_whitespace())
            .map(to_upper_char)
            .collect();
        ISRC_SHAPE.is_match(&isrc).then_some(isrc)
    }

    /// Every valid ISRC among the values, normalized, in the order first seen.
    pub fn isrcs<I, S>(values: I) -> IndexSet<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        values
            .into_iter()
            .filter_map(|value| Self::normalize_isrc(value.as_ref()))
            .collect()
    }

    /// Both sides carry a valid ISRC and at least one is on both.
    pub fn shares_isrc<A, B, SA, SB>(a: A, b: B) -> bool
    where
        A: IntoIterator<Item = SA>,
        B: IntoIterator<Item = SB>,
        SA: AsRef<str>,
        SB: AsRef<str>,
    {
        let left = Self::isrcs(a);
        !left.is_empty() && Self::isrcs(b).iter().any(|isrc| left.contains(isrc))
    }

    // ---- comparing ----------------------------------------------------------------------

    /// For telling duplicates apart (#53) and choosing the one MusicBrainz recording a kept
    /// fingerprint belongs to (#47): a subtitle only one title has ("Song (Interlude)") is
    /// another title, because a false yes there deletes a file or files it under the wrong
    /// recording.
    pub fn strict_titles() -> SongMatchOptions {
        SongMatchOptions {
            extras_must_agree: true,
            length_tolerance_seconds: None,
            ..Default::default()
        }
    }

    /// For deciding whether a song is already owned, or which entry is the song asked for: the
    /// default reading plus [`Lookup::Owned`], with lengths that may be `tolerance_seconds` apart.
    pub fn owned(tolerance_seconds: i32) -> SongMatchOptions {
        SongMatchOptions {
            length_tolerance_seconds: Some(tolerance_seconds),
            lookup: Lookup::Owned,
            ..Default::default()
        }
    }

    /// For picking one of a search's results with [`SongIdentity::pick_best`]: the default
    /// reading plus [`Lookup::Pick`], with lengths that may be `tolerance_seconds` apart.
    pub fn pick(tolerance_seconds: i32) -> SongMatchOptions {
        SongMatchOptions {
            length_tolerance_seconds: Some(tolerance_seconds),
            lookup: Lookup::Pick,
            ..Default::default()
        }
    }

    /// Whether two titles are one song, and one version of it. Artists are not looked at.
    pub fn same_title(a: &str, b: &str, options: Option<&SongMatchOptions>) -> SongMatch {
        compare_titles(
            &Self::parse_title(a, None),
            &Self::parse_title(b, None),
            options.unwrap_or(&DEFAULT_OPTIONS),
            None,
        )
    }

    /// Whether two songs are the same recording. Both artists must be known: a title
    /// alone is not an identity, unless both sides share an ISRC, which is.
    pub fn same(a: &SongRef, b: &SongRef, options: Option<&SongMatchOptions>) -> SongMatch {
        // First, and above the text: a romanised title and the same title in its own script
        // share no letter, and one ISRC still says they are one recording. Different ISRCs fall
        // through to the text, since a re-release can carry a new code for the same audio.
        if Self::shares_isrc(&a.isrcs, &b.isrcs) {
            return SongMatch::new(SongVerdict::Same, 1.0, "same ISRC");
        }

        let options = options.unwrap_or(&DEFAULT_OPTIONS);
        let title_a = Self::parse_title(&a.title, Some(&a.artist));
        let title_b = Self::parse_title(&b.title, Some(&b.artist));
        let title = compare_titles(&title_a, &title_b, options, known_lengths(a, b));
        if title.verdict == SongVerdict::Different {
            return title;
        }

        let artist_a = credit_of(&a.artist, &title_a);
        let artist_b = credit_of(&b.artist, &title_b);
        let mut artist = Self::compare_parsed_artists(&artist_a, &artist_b);
        let credited_alike = artist == ArtistAgreement::None
            && options.lookup != Lookup::Off
            && credits_alike(&artist_a, &artist_b);
        if credited_alike {
            artist = ArtistAgreement::Loose;
        }
        match artist {
            ArtistAgreement::Unknown => {
                return SongMatch::new(SongVerdict::Different, 0.6, "no artist to compare");
            }
            ArtistAgreement::None => {
                return SongMatch::new(
                    SongVerdict::Different,
                    0.9,
                    format!(
                        "different artists ('{}' and '{}')",
                        artist_a.display, artist_b.display
                    ),
                );
            }
            _ => {}
        }
        if title.verdict == SongVerdict::SameSongDifferentVersion {
            return title;
        }
        if artist == ArtistAgreement::Conflict {
            return SongMatch::new(
                SongVerdict::SameSongDifferentVersion,
                0.75,
                "different guest artists",
            );
        }

        let mut confidence = title.confidence;
        let mut reasons: Vec<String> = Vec::new();
        if title.reason != "the same title" {
            reasons.push(title.reason);
        }
        if credited_alike {
            confidence -= 0.1;
            reasons.push("the same artists written another way".to_string());
        } else if artist == ArtistAgreement::Loose {
            confidence -= 0.1;
            reasons.push("the same artist once stylized characters are read as letters".to_string());
        }

        if let Some(tolerance) = options.length_tolerance_seconds {
            match (a.seconds, b.seconds) {
                (Some(seconds_a), Some(seconds_b)) if seconds_a > 0.0 && seconds_b > 0.0 => {
                    let apart = (seconds_a - seconds_b).abs();
                    if apart > f64::from(tolerance) {
                        return SongMatch::new(
                            SongVerdict::SameSongDifferentVersion,
                            0.8,
                            format!("lengths differ by {} s", format_optional_decimals(apart, 1)),
                        );
                    }
                }
                _ => {
                    confidence -= 0.05;
                    reasons.push("a length is unknown".to_string());
                }
            }
        }

        let reason = if reasons.is_empty() {
            "the same song".to_string()
        } else {
            reasons.join("; ")
        };
        SongMatch::new(SongVerdict::Same, round(confidence.clamp(0.0, 1.0), 2), reason)
    }

    /// How well `b` fits `a`, 0 to 1, for ranking songs that [`SongIdentity::same`] already
    /// accepted: 0.45 for the title, 0.35 for the artist and 0.20 for the length (1 when equal,
    /// falling to 0 ten seconds apart, 0.5 when either is unknown). Accepts nothing by itself.
    pub fn score(a: &SongRef, b: &SongRef) -> f64 {
        let title_a = Self::parse_title(&a.title, Some(&a.artist));
        let title_b = Self::parse_title(&b.title, Some(&b.artist));
        let name = title_similarity(&title_a, &title_b);
        let artist = artist_similarity(&credit_of(&a.artist, &title_a), &credit_of(&b.artist, &title_b));
        let length = known_lengths(a, b).map_or(0.5, |(seconds_a, seconds_b)| {
            (1.0 - (seconds_a - seconds_b).abs() / 10.0).max(0.0)
        });
        0.45 * name + 0.35 * artist + 0.20 * length
    }

    /// The candidate that is `want`, among one search's results: of those [`SongIdentity::same`]
    /// accepts, the best by [`SongIdentity::score`], the earlier one on a tie. A candidate within
    /// 0.05 of the best that is not the same song as the best (by the same options, with
    /// [`Lookup::Pick`] read as [`Lookup::Owned`]) makes the pick ambiguous, and None is chosen.
    pub fn pick_best(want: &SongRef, candidates: &[SongRef], options: &SongMatchOptions) -> Option<usize> {
        let mut ranked: Vec<(usize, f64)> = candidates
            .iter()
            .enumerate()
            .filter(|(_, candidate)| Self::same(want, candidate, Some(options)).is_same())
            .map(|(at, candidate)| (at, Self::score(want, candidate)))
            .collect();
        // A stable sort, so equal scores keep the search's order.
        ranked.sort_by(|left, right| right.1.total_cmp(&left.1));
        let (&(best, top), rest) = ranked.split_first()?;
        let between = SongMatchOptions {
            lookup: if options.lookup == Lookup::Pick {
                Lookup::Owned
            } else {
                options.lookup
            },
            ..options.clone()
        };
        let ambiguous = rest
            .iter()
            .take_while(|(_, score)| top - score < 0.05 - 1e-9)
            .any(|&(other, _)| !Self::same(&candidates[best], &candidates[other], Some(&between)).is_same());
        (!ambiguous).then_some(best)
    }

    /// [`SongIdentity::same`] for two songs known only by title and artist.
    pub fn same_text(
        a_title: &str,
        a_artist: &str,
        b_title: &str,
        b_artist: &str,
        options: Option<&SongMatchOptions>,
    ) -> SongMatch {
        Self::same(
            &SongRef::new(a_title, a_artist),
            &SongRef::new(b_title, b_artist),
            options,
        )
    }

    /// Within the tolerance, or unknown on either side. (The usual tolerance is
    /// [`SongIdentity::LENGTH_TOLERANCE_SECONDS`].)
    pub fn length_fits(want: Option<i32>, got: Option<f64>, tolerance: i32) -> bool {
        match (want, got) {
            (Some(want), Some(got)) if want > 0 && got > 0.0 => {
                (got - f64::from(want)).abs() <= f64::from(tolerance)
            }
            _ => true,
        }
    }

    /// A key for "this song in this version", for deduplicating and for remembering songs across
    /// sources: the primary artist and the title keys, and the version markers that make a
    /// different recording. "Drake feat. Rihanna - Too Good" and "Drake - Too Good (feat.
    /// Rihanna)" share one; "Song (Live)" has its own.
    pub fn match_key(artist: &str, title: &str) -> String {
        let parsed = Self::parse_title(title, Some(artist));
        let credit = Self::parse_artists(credit_text(artist, &parsed));
        let mut primary = Self::key(credit.primary());
        if let Some(alias) = ALIASES.get(primary.as_str()) {
            primary = (*alias).to_string();
        }
        format!("{primary}|{}", versioned_key(&parsed))
    }

    /// The title part of [`SongIdentity::match_key`], for songs already known to share an
    /// artist, such as the tracks of one album.
    pub fn title_key(title: &str) -> String {
        versioned_key(&Self::parse_title(title, None))
    }

    /// Whether two names are one artist, whole: case, accents, a leading "The", stylized
    /// characters and the alias table ignored, but never split, so "Bob Marley" is not "Bob
    /// Marley & The Wailers". For an artist page, where a credit's guests do not belong.
    pub fn same_artist_name(a: &str, b: &str) -> bool {
        let left = Self::parse_artists(a).display;
        let right = Self::parse_artists(b).display;
        if Self::key(&left).is_empty() || Self::key(&right).is_empty() {
            return false;
        }
        overlaps(&keys([left.as_str()], false), &keys([right.as_str()], false))
            || overlaps(&keys([left.as_str()], true), &keys([right.as_str()], true))
    }

    /// The name an artist is best known by, when `artist` credits them under
    /// another name in the alias table: "Kanye West" for "Ye". None when the name is not an
    /// alias or already is that name. A source that files a renamed artist under one name only
    /// finds nothing under the other, so this is the second name to ask.
    pub fn known_name(artist: &str) -> Option<String> {
        let key = Self::key(Self::parse_artists(artist).primary());
        let canonical = ALIASES.get(key.as_str())?;
        let name = ALIAS_NAMES.get(canonical)?;
        if Self::key(name) == key {
            None
        } else {
            Some((*name).to_string())
        }
    }

    /// Whether the alias table knows other names for the artist's main credit: true for "Kanye
    /// West" and for "Ye", false for an artist it has no entry for. A source may file such an
    /// artist's songs under a name other than the one asked.
    pub fn has_other_names(artist: &str) -> bool {
        let key = Self::key(Self::parse_artists(artist).primary());
        !key.is_empty()
            && (ALIASES.contains_key(key.as_str()) || ALIASES.values().any(|canonical| *canonical == key))
    }

    // ---- searching ----------------------------------------------------------------------

    /// The queries to try against a search API, in order, stopping at the first that finds the
    /// song:
    ///
    /// 1. the title and artist as given;
    /// 2. the title without brackets, guests or version tails, and the credit without aliases;
    /// 3. the same with stylized characters read as letters ("suicideboys suicide");
    /// 4. the primary artist only;
    /// 5. the title alone, as a last resort.
    ///
    /// A variant that reads the same as an earlier one is left out, so a plain "Drake - Landed"
    /// has two. Whatever a variant finds must still pass [`SongIdentity::same`] against the song
    /// as asked: a looser query never means a looser match.
    pub fn query_variants(title: &str, artist: &str) -> Vec<SongQuery> {
        let mut variants: Vec<SongQuery> = Vec::new();
        let mut add = |title: &str, artist: &str| {
            let title = WHITESPACE.replace_all(title, " ").trim().to_string();
            let artist = WHITESPACE.replace_all(artist, " ").trim().to_string();
            if Self::key(&title).is_empty() {
                return;
            }
            if variants.iter().any(|variant| {
                eq_ignore_case(&Self::fold(&variant.title), &Self::fold(&title))
                    && eq_ignore_case(&Self::fold(&variant.artist), &Self::fold(&artist))
            }) {
                return;
            }
            variants.push(SongQuery { title, artist });
        };

        let parsed = Self::parse_title(title, Some(artist));
        let credit = Self::parse_artists(credit_text(artist, &parsed));
        let mut clean_title = parsed.core.clone();
        let folded = Self::fold(title);
        let parts: Vec<&str> = BRACKET
            .captures_iter(&folded)
            .map(|caps| caps.get(1).map_or("", |inner| inner.as_str()).trim())
            .filter(|inner| PART_NUMBER.is_match(&utf16_class_view(inner)))
            .collect();
        if !parts.is_empty() {
            clean_title = format!("{clean_title} {}", parts.join(" "));
        }

        add(title.trim(), artist.trim());
        add(&clean_title, &credit.display);
        add(
            &Self::fold_stylized(&clean_title),
            &Self::fold_stylized(&credit.display),
        );
        add(&clean_title, credit.primary());
        add(&clean_title, "");
        variants
    }
}

#[cfg(test)]
#[path = "song_identity_tests.rs"]
mod tests;
