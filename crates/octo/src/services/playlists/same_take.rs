//! Whether two songs are one recording and one take ([`same_take`]), the rule playlist merges,
//! replaces and binds use before an entry takes another library id; and the 1.12.0 phones' own
//! match rule ([`phone_match`]), which a replace from those phones is read against.

use std::collections::HashSet;

use octo_core::common::{SongIdentity, SongMatchOptions, SongRef};

use crate::services::library::navidrome_playlist_api::SongFacts;

/// How far apart two known lengths may be for one take, in seconds.
pub const TAKE_SECONDS: i32 = 2;

/// Words that name another take when one side's title or album has one and the other's does
/// not.
const TAKE_WORDS: &[&str] = &[
    "live",
    "acoustic",
    "remix",
    "edit",
    "instrumental",
    "karaoke",
    "demo",
];

/// The phones' length tolerance, in seconds.
const PHONE_SECONDS: i32 = 10;

/// The length the 1.12.0 phones read as a server's guess, not a length.
const GUESSED_SECONDS: i32 = 180;

/// What [`same_take`] and [`phone_match`] compare.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TakeFacts {
    pub title: String,
    pub artist: String,
    /// Empty when not known.
    pub album: String,
    pub seconds: Option<i32>,
    pub isrcs: Vec<String>,
    /// `explicit` or `e`, `clean` or `c`, or none.
    pub explicit_status: Option<String>,
}

impl From<&SongFacts> for TakeFacts {
    fn from(facts: &SongFacts) -> Self {
        TakeFacts {
            title: facts.title.clone(),
            artist: facts.artist.clone(),
            album: facts.album.clone(),
            seconds: facts.seconds,
            isrcs: facts.isrcs.clone(),
            explicit_status: facts.explicit_status.clone(),
        }
    }
}

/// One recording and one take: both lengths known and at most [`TAKE_SECONDS`] apart; the same
/// song by the strict title reading (a subtitle only one side has is another title, lookups
/// off); the same ISRC when both sides carry one; explicit marks that don't contradict; and no
/// take word (live, acoustic, remix, edit, instrumental, karaoke, demo) on one side only. Album
/// words count only when both albums are known.
pub fn same_take(a: &TakeFacts, b: &TakeFacts) -> bool {
    let (Some(left), Some(right)) = (known(a.seconds), known(b.seconds)) else {
        return false;
    };
    if (left - right).abs() > TAKE_SECONDS {
        return false;
    }
    let codes_a = SongIdentity::isrcs(&a.isrcs);
    let codes_b = SongIdentity::isrcs(&b.isrcs);
    if !codes_a.is_empty() && !codes_b.is_empty() && !codes_a.iter().any(|code| codes_b.contains(code)) {
        return false;
    }
    if let (Some(x), Some(y)) = (
        explicit_word(a.explicit_status.as_deref()),
        explicit_word(b.explicit_status.as_deref()),
    ) && x != y
    {
        return false;
    }
    let albums = !a.album.trim().is_empty() && !b.album.trim().is_empty();
    let has = |facts: &TakeFacts, word: &str| {
        has_word(&facts.title, word) || (albums && has_word(&facts.album, word))
    };
    if TAKE_WORDS.iter().any(|&word| has(a, word) != has(b, word)) {
        return false;
    }
    SongIdentity::same(
        &SongRef::new(a.title.as_str(), a.artist.as_str()),
        &SongRef::new(b.title.as_str(), b.artist.as_str()),
        Some(&SongIdentity::strict_titles()),
    )
    .is_same()
}

/// The 1.12.0 phones' decision that a library song is an outside one: the Android pull match
/// (a shared title lookup key, and the same song at a 10 s tolerance with no ISRCs, a length of
/// 0, 180 or none counting as unknown) or the iPhone adoption match (the same recording at any
/// length).
pub fn phone_match(a: &TakeFacts, b: &TakeFacts) -> bool {
    android_pull_match(a, b) || iphone_adoption_match(a, b)
}

fn android_pull_match(a: &TakeFacts, b: &TakeFacts) -> bool {
    let keys = title_lookup_keys(&a.title, &a.artist);
    if !title_lookup_keys(&b.title, &b.artist)
        .iter()
        .any(|key| keys.contains(key))
    {
        return false;
    }
    let within = SongMatchOptions {
        length_tolerance_seconds: Some(PHONE_SECONDS),
        ..Default::default()
    };
    SongIdentity::same(
        &SongRef::new(a.title.as_str(), a.artist.as_str()).with_seconds(phone_seconds(a.seconds)),
        &SongRef::new(b.title.as_str(), b.artist.as_str()).with_seconds(phone_seconds(b.seconds)),
        Some(&within),
    )
    .is_same()
}

fn iphone_adoption_match(a: &TakeFacts, b: &TakeFacts) -> bool {
    let any_length = SongMatchOptions {
        length_tolerance_seconds: None,
        ..Default::default()
    };
    SongIdentity::same(
        &SongRef::new(a.title.as_str(), a.artist.as_str()),
        &SongRef::new(b.title.as_str(), b.artist.as_str()),
        Some(&any_length),
    )
    .is_same()
}

/// The two keys the phones look a title up by: the key's letters (the key itself when it has
/// none) and the loose key.
fn title_lookup_keys(title: &str, artist: &str) -> HashSet<String> {
    let parsed = SongIdentity::parse_title(title, Some(artist));
    if parsed.key.is_empty() {
        return HashSet::new();
    }
    let letters: String = parsed.key.chars().filter(|c| c.is_alphabetic()).collect();
    let first = if letters.is_empty() {
        parsed.key.clone()
    } else {
        letters
    };
    HashSet::from([first, parsed.loose_key])
}

/// A length the phones count as known.
fn phone_seconds(seconds: Option<i32>) -> Option<f64> {
    seconds.filter(|&s| s > 0 && s != GUESSED_SECONDS).map(f64::from)
}

fn known(seconds: Option<i32>) -> Option<i32> {
    seconds.filter(|&s| s > 0)
}

/// `explicit` or `clean` for a mark in either spelling (Navidrome's `e` and `c`, or the words).
pub fn explicit_word(status: Option<&str>) -> Option<&'static str> {
    match status?.trim().to_ascii_lowercase().as_str() {
        "e" | "explicit" => Some("explicit"),
        "c" | "clean" => Some("clean"),
        _ => None,
    }
}

/// Whether `text` has `word` as a whole word, ignoring case.
fn has_word(text: &str, word: &str) -> bool {
    text.split(|c: char| !c.is_alphanumeric())
        .any(|part| part.eq_ignore_ascii_case(word))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn take(title: &str, seconds: i32) -> TakeFacts {
        TakeFacts {
            title: title.into(),
            artist: "Radiohead".into(),
            album: "In Rainbows".into(),
            seconds: Some(seconds),
            ..Default::default()
        }
    }

    #[test]
    fn one_take_within_two_seconds_with_matching_codes_and_marks() {
        assert!(same_take(&take("Nude", 255), &take("Nude", 257)));
        assert!(!same_take(&take("Nude", 255), &take("Nude", 258)));
        let unknown = TakeFacts {
            seconds: None,
            ..take("Nude", 255)
        };
        assert!(!same_take(&unknown, &take("Nude", 255)), "a length must be known");
        let coded = |code: &str| TakeFacts {
            isrcs: vec![code.into()],
            ..take("Nude", 255)
        };
        assert!(same_take(&coded("GBSTK0700015"), &coded("GB-STK-07-00015")));
        assert!(!same_take(&coded("GBSTK0700015"), &coded("GBSTK0700099")));
        assert!(
            same_take(&coded("GBSTK0700015"), &take("Nude", 255)),
            "one side without a code"
        );
        let marked = |mark: &str| TakeFacts {
            explicit_status: Some(mark.into()),
            ..take("Nude", 255)
        };
        assert!(!same_take(&marked("e"), &marked("clean")));
        assert!(same_take(&marked("explicit"), &marked("e")));
        assert!(same_take(&marked("e"), &take("Nude", 255)));
    }

    #[test]
    fn a_take_word_on_one_side_only_is_another_take() {
        assert!(!same_take(&take("Nude", 255), &take("Nude (Live)", 255)));
        assert!(!same_take(&take("Nude", 255), &take("Nude - Radio Edit", 255)));
        assert!(!same_take(&take("Nude", 255), &take("Nude (Acoustic)", 255)));
        let live_album = TakeFacts {
            album: "Live at the Basement".into(),
            ..take("Nude", 255)
        };
        assert!(!same_take(&live_album, &take("Nude", 255)));
        let no_album = TakeFacts {
            album: String::new(),
            ..take("Nude", 255)
        };
        assert!(
            same_take(&live_album, &no_album),
            "album words only when both are known"
        );
        let deluxe = TakeFacts {
            album: "In Rainbows (Deluxe Edition)".into(),
            ..take("Nude", 255)
        };
        assert!(same_take(&deluxe, &take("Nude", 255)), "edition is not edit");
        assert!(!same_take(&take("Nude", 255), &take("Reckoner", 255)));
    }

    #[test]
    fn the_phones_match_at_ten_seconds_and_read_180_as_unknown() {
        assert!(phone_match(&take("Nude", 255), &take("Nude", 263)));
        assert!(
            phone_match(&take("Nude", 180), &take("Nude", 255)),
            "180 is a guess"
        );
        assert!(
            phone_match(&take("Nude", 255), &take("Nude", 300)),
            "the iPhone match takes any length"
        );
        assert!(!phone_match(&take("Song", 200), &take("Song (Live)", 200)));
        assert!(!phone_match(&take("Nude", 255), &take("Reckoner", 255)));
    }
}
