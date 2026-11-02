//! Metadata, the pure parts (`Services/Metadata`): the genre normalizer, the Accept-Language
//! header, Deezer's pictures without its stand-in, the Deezer record types the tagging plan
//! holds, and Apple's catalog answers in Deezer's shape. The genre backfill's stores and the
//! Deezer client are `octo::services::metadata`.

pub mod accept_language_header;
pub mod apple_as_deezer;
pub mod deezer_image;
pub mod deezer_metadata_service;
pub mod genre_normalizer;

pub use accept_language_header::{AcceptLanguageHeader, StringWithQuality};
pub use genre_normalizer::{GenreNormalizationResult, GenreNormalizer, GenreTagAction, GenreTagPlan};
