//! Playlists that keep songs found online, and playlist folders: the stores and the pure rules
//! shared by the held playlist service, the Subsonic and native routes and the folder endpoints.

pub mod binder;
pub mod caps;
pub mod entries;
pub mod folder_store;
pub mod held_playlists;
pub mod held_store;
pub mod merge;
pub mod same_take;
pub mod seed_index;

pub use caps::{CapCounts, CapRefusal, PlaylistCaps};
pub use folder_store::{FolderError, FolderState, PlaylistFolder, PlaylistFolderStore};
pub use held_store::{
    Creating, HeldEntry, HeldPlaylist, HeldStore, PendingWrite, PlaylistState, ReplacedRecord, SaveError,
    SongSnapshot, WritePhase,
};
pub use merge::{MergeError, RawRow, ids_needing_presence, merge, move_back_entries};
pub use same_take::{TakeFacts, phone_match, same_take};
pub use seed_index::{SeedIndex, family_key, held_key, replaced_key};
