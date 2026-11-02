//! `Services/Admin`: the dashboard's sign-ins, folder picker, covers and Afterglow light.

pub mod admin_caller;
pub mod admin_recovery_code;
pub mod admin_role_check;
pub mod ambient_palette_service;
pub mod browse_session_store;
pub mod cover_palette;
pub mod directory_browser;
pub mod fetched_covers;
pub mod sign_in_throttle;

pub use admin_caller::{AdminCaller, AdminCallerKind};
pub use admin_recovery_code::AdminRecoveryCode;
pub use admin_role_check::AdminRoleCheck;
pub use ambient_palette_service::AmbientPaletteService;
pub use browse_session_store::{BrowseSessionStore, SessionKind};
pub use directory_browser::{BrowseEntry, BrowseResult, DirectoryBrowser};
pub use fetched_covers::FetchedCovers;
pub use sign_in_throttle::SignInThrottle;
