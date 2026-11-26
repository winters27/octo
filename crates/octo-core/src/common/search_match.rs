//! How well a song's artist and title answer the words of a search.
//!
//! A search is often a title and an artist typed together ("after the storm kali uchis"), so a
//! row is judged by how many of the query's words its artist and title name between them, not
//! by its title alone. Last.fm's track.search and Deezer's catalog both answer such a query
//! with every song that shares the title, and ordered by listeners alone the one by the artist
//! the user named could sit under ten others. Two readings, best first:
//!
//!   1. How many of the query's words the row's artist and title hold. "After the Storm" by
//!      Kali Uchis holds all five words of "after the storm kali uchis"; "After the Storm" by
//!      Mumford & Sons holds three.
//!   2. Whether the query names the row's whole title (its guest credits and version markers
//!      taken off). For "stay", "Stay" is named and "Stay With Me" is not.
//!
//! Words are compared casefolded and without accents or apostrophes, so "Beyoncé" is "beyonce"
//! and "Taylor's" is "taylors". Everything else (listeners, the source's own order) decides
//! between rows that match equally well.

use std::cmp::Ordering;
use std::collections::HashSet;

use super::SongIdentity;
use super::song_versions::strip_versions;

/// How well one row answers a query. Greater is better.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct SearchMatch {
    /// How many of the query's words are in the row's artist or title.
    pub named: usize,
    /// Whether every word of the row's title (without credits or version markers) is in the
    /// query.
    pub title_named: bool,
}

impl SearchMatch {
    /// How well `artist` and `title` answer a query whose words are `query_words` (see
    /// [`SearchMatch::words`]).
    pub fn of(query_words: &HashSet<String>, artist: &str, title: &str) -> Self {
        if query_words.is_empty() {
            return Self::default();
        }
        let mut row_words = Self::words(artist);
        let title_words = Self::words(&strip_versions(&SongIdentity::strip_features(title)));
        row_words.extend(Self::words(title));
        let named = query_words
            .iter()
            .filter(|word| row_words.contains(*word))
            .count();
        let title_named =
            !title_words.is_empty() && title_words.iter().all(|word| query_words.contains(word));
        Self { named, title_named }
    }

    /// The words of `text` as a search compares them: casefolded, accents and apostrophes
    /// dropped, split at anything that is not a letter or a digit.
    pub fn words(text: &str) -> HashSet<String> {
        SongIdentity::fold(text)
            .to_lowercase()
            .replace('\'', "")
            .split(|c: char| !c.is_alphanumeric())
            .map(SongIdentity::key)
            .filter(|word| !word.is_empty())
            .collect()
    }

    /// The best match first. For a sort that should put better rows ahead of worse ones.
    pub fn best_first(a: &Self, b: &Self) -> Ordering {
        b.cmp(a)
    }

    /// Artist and title pairs, the rows that answer `query` best first. A stable sort, so rows
    /// that match equally well keep the order they came in (a catalog's own relevance).
    pub fn order_rows(query: &str, rows: Vec<(String, String)>) -> Vec<(String, String)> {
        let query_words = Self::words(query);
        let mut judged: Vec<(Self, (String, String))> = rows
            .into_iter()
            .map(|(artist, title)| (Self::of(&query_words, &artist, &title), (artist, title)))
            .collect();
        judged.sort_by(|(a, _), (b, _)| Self::best_first(a, b));
        judged.into_iter().map(|(_, row)| row).collect()
    }
}

#[cfg(test)]
#[path = "search_match_tests.rs"]
mod tests;
