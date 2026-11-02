//! Apple's catalog: one pace for every request Octo sends to Apple's iTunes Search API and chart
//! feed, and the client that answers Deezer's hidden questions from Apple's catalog.

pub mod apple_catalog;
pub mod apple_pace;

pub use apple_catalog::{AppleAsked, AppleCatalog, Entity, StoreSource};
pub use apple_pace::{AppleClass, ApplePace, PaceConfig, Refused, Ticket};
