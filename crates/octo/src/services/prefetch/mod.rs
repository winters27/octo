//! Getting upcoming songs ahead: the songs that play next fetched as real FLAC copies into a
//! folder of their own before they play, served to the players that can take them, and kept by a
//! heart without a second download. Beside them, whole copies of outside songs' previews.

pub mod heard_samples;
pub mod keep_ready;
pub mod play_source;
pub mod prefetch_service;
pub mod preview_copies;
pub mod queue_window;
pub mod store;

pub use heard_samples::{
    Capture, FpcalcClass, FpcalcSlot, HeardFetch, HeardFile, HeardPrints, HeardSampleParts, HeardSamples,
    Printer, ScratchFile, TopUp, WholePrint, sample_cap, sample_id_of,
};
pub use keep_ready::{
    AppKeepReadySources, KeepFlac, KeepReadyChoice, KeepReadyService, KeepReadySources, KeepReadyStore,
    KeepReadyView, ListSongs, ListView, Picks, UnknownList,
};
pub use play_source::{PlaySources, Source, from_start};
pub use prefetch_service::{JobView, ONE_AT_A_TIME, PrefetchParts, PrefetchService, PrefetchStatus};
pub use preview_copies::{
    Copying, HeardCopy, PreviewCopies, PreviewSource, WholePreview, asks_for_pieces, is_video_id,
};
pub use queue_window::{SavedQueue, window};
pub use store::{
    AheadSidecar, FOLDER_MARKER, FolderFn, PrefetchStore, PrefetchedFiles, ReadySong, Taken,
    VerificationMirror, file_name, free_bytes, owns_folder, prepare_folder, sidecar_path, write_sidecar,
};

#[cfg(test)]
#[path = "prefetch_service_tests.rs"]
mod prefetch_service_tests;

#[cfg(test)]
#[path = "preview_copies_tests.rs"]
mod preview_copies_tests;

#[cfg(test)]
#[path = "keep_ready_tests.rs"]
mod keep_ready_tests;
