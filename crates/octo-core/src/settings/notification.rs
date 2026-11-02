//! The push notification settings (the `Notifications` section).

use serde::{Deserialize, Serialize};

/// Push notifications for the download lifecycle. Subsonic has no notification
/// mechanism and Navidrome's event stream only reaches its own web UI, so without
/// this nothing tells the user a starred track landed, settled for a lossy source,
/// or failed: the SearchWaitSeconds bug went unnoticed for exactly that reason.
///
/// A transport is enabled simply by its URL being non-empty. Every value is read fresh at send
/// time, so changes apply without a restart.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct NotificationSettings {
    /// Full ntfy topic URL, e.g. "https://ntfy.sh/my-octo-topic". Non-empty enables
    /// the ntfy sink. Subscribe to the same topic in the ntfy app to receive pushes.
    /// Environment variable: NOTIFICATIONS__NTFYURL
    pub ntfy_url: String,

    /// Optional ntfy access token, sent as "Authorization: Bearer". Only needed on
    /// servers with access control.
    /// Environment variable: NOTIFICATIONS__NTFYTOKEN
    pub ntfy_token: String,

    /// Discord webhook URL. Non-empty enables the Discord sink. The URL embeds the
    /// webhook token, so the whole URL is treated as a secret and masked in the
    /// config-sources view.
    /// Environment variable: NOTIFICATIONS__DISCORDWEBHOOKURL
    pub discord_webhook_url: String,

    /// A transfer actually began, saying up front whether it found lossless or is
    /// settling for MP3. Off by default: it doubles volume for information
    /// DownloadCompleted mostly repeats, and LosslessFallback (on by default)
    /// already covers the settling case.
    pub notify_download_started: bool,

    /// A track landed: format, source, size, album art.
    pub notify_download_completed: bool,

    /// Soulseek came up empty and Octo settled for a YouTube MP3. On by default:
    /// silent quality loss is the failure mode this whole feature exists to expose.
    pub notify_lossless_fallback: bool,

    /// Both sources failed for a starred track. Stars only: a shed
    /// play-triggered acquisition is a hint, not an unmet promise.
    pub notify_download_failed: bool,

    /// One summary per album walk (tracks fetched, how many lossless)
    /// instead of a ping per track.
    pub notify_album_completed: bool,

    /// Family: a member asked for a copy that waits for approval, or a request failed.
    pub notify_family_requests: bool,

    /// Family: a member's device, or a new app of the owner, was seen for the first time.
    pub notify_family_devices: bool,

    /// Mornye could not pass its check for a gateway: once per stretch of it failing, with the
    /// way to pass it on the dashboard. On by default: songs only Mornye can bring fail meanwhile.
    pub notify_mornye_check: bool,
}

impl Default for NotificationSettings {
    fn default() -> Self {
        Self {
            ntfy_url: String::new(),
            ntfy_token: String::new(),
            discord_webhook_url: String::new(),
            notify_download_started: false,
            notify_download_completed: true,
            notify_lossless_fallback: true,
            notify_download_failed: true,
            notify_album_completed: true,
            notify_family_requests: true,
            notify_family_devices: true,
            notify_mornye_check: true,
        }
    }
}
