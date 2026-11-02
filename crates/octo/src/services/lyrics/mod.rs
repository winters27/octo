//! Lyrics with I/O: the sources, the service that ranks them, choosing by hand and the pins on
//! disk, the writer that saves lyrics beside or inside songs (a queue worker too), its undo
//! journal, what a song file has now, and the library job ("find lyrics for the library" and the
//! lyrics page's steps) with its run on disk. The models, LRC handling and matching are
//! `octo_core::lyrics`.

pub mod kugou_lyrics_source;
pub mod lrclib_lyrics_source;
pub mod lyrics_choices;
pub mod lyrics_http;
pub mod lyrics_library_job;
mod lyrics_library_steps;
pub mod lyrics_ovh_lyrics_source;
pub mod lyrics_service;
pub mod lyrics_sidecar_writer;
pub mod lyrics_undo_journal;
mod memory_cache;
pub mod netease_lyrics_source;
pub mod song_lyrics;
mod web_utility;

#[cfg(test)]
mod test_support;

pub use kugou_lyrics_source::KugouLyricsSource;
pub use lrclib_lyrics_source::LrclibLyricsSource;
pub use lyrics_choices::{LyricsChoiceService, LyricsChoiceStore};
pub use lyrics_library_job::{LyricsLibraryStore, LyricsLibraryWorker};
pub use lyrics_ovh_lyrics_source::LyricsOvhLyricsSource;
pub use lyrics_service::LyricsService;
pub use lyrics_sidecar_writer::{
    LyricsJob, LyricsPathRemap, LyricsSidecarWriter, LyricsSongTags, LyricsTagAccess, LyricsWrite,
    LyricsWriteOutcome, TagLibLyricsTags,
};
pub use lyrics_undo_journal::{LyricsUndoEntry, LyricsUndoJournal};
pub use netease_lyrics_source::NeteaseLyricsSource;
