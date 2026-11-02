//! The library tidy: bring the files already in the library in line with how a new download is
//! written. Two tasks: Artists (folders, spellings and guests, planned by `planner`) and File
//! layout (folders and file names, planned by `layout_pass`). The rest reads the library, writes,
//! waits for Navidrome and moves, with a journal for Undo.

pub mod journal;
pub mod layout_pass;
pub mod planner;
pub mod store;
pub mod worker;

pub use journal::{TidyJournal, TidyJournalEntry};
pub use store::{TidyMode, TidyRun, TidyScope, TidyStatus, TidyStore, TidyTask};
pub use worker::{TidyRequest, TidyServices, TidyWorker};
