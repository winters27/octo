//! The rendering half of the notification service: which events are switched on, and the text
//! every transport carries. The fan-out is `octo::services::notifications::notification_service`.

use super::i_notification_sink::NotificationMessage;
use super::notification_event::{NotificationEvent, NotificationEventType};
use crate::settings::NotificationSettings;

pub fn is_enabled(s: &NotificationSettings, r#type: NotificationEventType) -> bool {
    match r#type {
        NotificationEventType::DownloadStarted => s.notify_download_started,
        NotificationEventType::DownloadCompleted => s.notify_download_completed,
        NotificationEventType::LosslessFallback => s.notify_lossless_fallback,
        NotificationEventType::DownloadFailed => s.notify_download_failed,
        NotificationEventType::AlbumCompleted => s.notify_album_completed,
        // The test button bypasses toggles by design: it verifies transports.
        NotificationEventType::Test => true,
        NotificationEventType::FamilyRequestNew
        | NotificationEventType::FamilyRequestApproved
        | NotificationEventType::FamilyRequestDeclined
        | NotificationEventType::FamilyRequestAdded
        | NotificationEventType::FamilyRequestFailed => s.notify_family_requests,
        NotificationEventType::FamilyDeviceNew => s.notify_family_devices,
        NotificationEventType::MornyeCheck => s.notify_mornye_check,
    }
}

/// A family member's name for a sentence: their name, or "Someone" when it is not known.
fn member_name(evt: &NotificationEvent) -> String {
    evt.family_member
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or("Someone")
        .to_string()
}

/// "Title by Artist", or the title alone, or "a song" when neither is known.
fn song_words(evt: &NotificationEvent) -> String {
    let title = evt.title.as_deref().map(str::trim).filter(|t| !t.is_empty());
    let artist = evt.artist.as_deref().map(str::trim).filter(|a| !a.is_empty());
    match (title, artist) {
        (Some(title), Some(artist)) => format!("{title} by {artist}"),
        (Some(title), None) => title.to_string(),
        (None, Some(artist)) => format!("a song by {artist}"),
        (None, None) => "a song".to_string(),
    }
}

/// The note an approver left, as a sentence of its own after the title; empty without one.
fn note_words(evt: &NotificationEvent) -> String {
    match evt
        .family_note
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
    {
        Some(note) => format!(". {note}"),
        None => String::new(),
    }
}

/// The text a family member's own push carries, in plain words. Events that are not about a
/// member's request render as for the owner.
pub fn render_for_member(evt: &NotificationEvent) -> NotificationMessage {
    let song = song_words(evt);
    let message = |title: String, body: &str| NotificationMessage {
        r#type: evt.r#type,
        title,
        body: body.to_string(),
        image_url: evt.cover_art_url.clone(),
        description: None,
        fields: None,
    };
    match evt.r#type {
        NotificationEventType::FamilyRequestApproved => {
            message(format!("Approved: {song}"), "It will be in your library soon.")
        }
        NotificationEventType::FamilyRequestDeclined => message(
            format!("Declined: {song}{}", note_words(evt)),
            "You can still play it from where you found it.",
        ),
        NotificationEventType::FamilyRequestAdded => message(
            format!("Added: {song} is in your library"),
            "Open your library to play it.",
        ),
        NotificationEventType::FamilyRequestFailed => message(
            format!("Could not get {song}"),
            evt.detail
                .as_deref()
                .filter(|d| !d.trim().is_empty())
                .unwrap_or("Octo could not find a copy to download."),
        ),
        _ => render(evt),
    }
}

/// The family texts the owner's channels carry.
fn render_family(evt: &NotificationEvent) -> NotificationMessage {
    let who = member_name(evt);
    let song = song_words(evt);
    let message = |title: String, body: String| NotificationMessage {
        r#type: evt.r#type,
        title,
        body,
        image_url: evt.cover_art_url.clone(),
        description: None,
        fields: None,
    };
    let quality = evt
        .format
        .as_deref()
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .map(|q| format!(" ({q})"))
        .unwrap_or_default();
    match evt.r#type {
        NotificationEventType::FamilyRequestNew => message(
            format!("{who} asked for {song}{quality}"),
            "Approve or decline it on the Family page.".to_string(),
        ),
        NotificationEventType::FamilyRequestApproved => {
            message(format!("Approved for {who}: {song}"), String::new())
        }
        NotificationEventType::FamilyRequestDeclined => message(
            format!("Declined for {who}: {song}{}", note_words(evt)),
            String::new(),
        ),
        NotificationEventType::FamilyRequestAdded => message(
            format!("Added for {who}: {song}"),
            "It is in their library now.".to_string(),
        ),
        NotificationEventType::FamilyRequestFailed => message(
            format!("Could not get {song} for {who}"),
            evt.detail
                .clone()
                .filter(|d| !d.trim().is_empty())
                .unwrap_or_else(|| "Octo could not find a copy to download.".to_string()),
        ),
        NotificationEventType::FamilyDeviceNew => {
            let app = evt
                .source
                .as_deref()
                .map(str::trim)
                .filter(|a| !a.is_empty())
                .unwrap_or("a new app");
            message(format!("New device for {who}: {app}"), String::new())
        }
        _ => message(String::new(), String::new()),
    }
}

/// An optional value as text: None is the empty string.
fn text<T: ToString>(value: &Option<T>) -> String {
    value.as_ref().map(ToString::to_string).unwrap_or_default()
}

fn is_null_or_empty(value: &Option<String>) -> bool {
    value.as_deref().is_none_or(str::is_empty)
}

/// Single source of truth for the text both transports carry. Track
/// events additionally get structured stat fields so layout-capable transports
/// can render a song card instead of prose.
pub fn render(evt: &NotificationEvent) -> NotificationMessage {
    let track = if is_null_or_empty(&evt.artist) {
        evt.title.clone().unwrap_or_else(|| "unknown track".to_string())
    } else {
        format!("{} \u{2013} {}", text(&evt.artist), text(&evt.title))
    };
    let size = evt.size_bytes.filter(|bytes| *bytes > 0);
    let duration = evt.duration_seconds.filter(|seconds| *seconds > 0);

    match evt.r#type {
        NotificationEventType::DownloadStarted => NotificationMessage {
            r#type: evt.r#type,
            title: format!("Downloading: {track}"),
            body: format!("{} via {}", text(&evt.format), text(&evt.source))
                + &size
                    .map(|sb| format!(" ({})", format_size(sb)))
                    .unwrap_or_default(),
            image_url: evt.cover_art_url.clone(),
            description: evt.album.clone(),
            fields: Some(track_fields(evt)),
        },

        NotificationEventType::DownloadCompleted => NotificationMessage {
            r#type: evt.r#type,
            title: format!("Downloaded: {track}"),
            body: (if is_null_or_empty(&evt.album) {
                String::new()
            } else {
                format!("{}\n", text(&evt.album))
            }) + &format!("{} via {}", text(&evt.format), text(&evt.source))
                + &size
                    .map(|cb| format!(", {}", format_size(cb)))
                    .unwrap_or_default()
                + &duration
                    .map(|ds| format!(" \u{b7} {}", format_duration(ds)))
                    .unwrap_or_default(),
            image_url: evt.cover_art_url.clone(),
            description: evt.album.clone(),
            fields: Some(track_fields(evt)),
        },

        NotificationEventType::LosslessFallback => NotificationMessage {
            r#type: evt.r#type,
            title: format!("Lossless miss: {track}"),
            body: format!(
                "Soulseek failed ({}); settling for YouTube MP3.",
                evt.detail.as_deref().unwrap_or("no usable result")
            ),
            image_url: evt.cover_art_url.clone(),
            description: None,
            fields: None,
        },

        NotificationEventType::DownloadFailed => NotificationMessage {
            r#type: evt.r#type,
            title: format!("Download failed: {track}"),
            body: evt
                .detail
                .clone()
                .unwrap_or_else(|| "Every enabled source failed.".to_string()),
            image_url: evt.cover_art_url.clone(),
            description: None,
            fields: None,
        },

        NotificationEventType::AlbumCompleted => NotificationMessage {
            r#type: evt.r#type,
            // Sent after every album walk, so it has to say when nothing arrived at all.
            title: if evt.track_count == Some(0) && evt.failed_count.is_some_and(|f| f > 0) {
                format!("Album failed: {track}")
            } else {
                format!("Album complete: {track}")
            },
            body: format!(
                "{} tracks fetched, {} lossless",
                text(&evt.track_count),
                text(&evt.lossless_count)
            ) + &evt
                .kept_count
                .filter(|k| *k > 0)
                .map(|k| format!(", {k} already yours"))
                .unwrap_or_default()
                + &evt
                    .upgrading_count
                    .filter(|u| *u > 0)
                    .map(|u| format!(", {u} upgrading"))
                    .unwrap_or_default()
                + &evt
                    .failed_count
                    .filter(|f| *f > 0)
                    .map(|f| format!(", {f} failed"))
                    .unwrap_or_default(),
            image_url: evt.cover_art_url.clone(),
            description: None,
            fields: Some(album_fields(evt)),
        },

        NotificationEventType::Test => NotificationMessage::new(
            evt.r#type,
            "Octo test notification",
            "Transports are wired up correctly.",
            None,
        ),

        NotificationEventType::FamilyRequestNew
        | NotificationEventType::FamilyRequestApproved
        | NotificationEventType::FamilyRequestDeclined
        | NotificationEventType::FamilyRequestAdded
        | NotificationEventType::FamilyRequestFailed
        | NotificationEventType::FamilyDeviceNew => render_family(evt),

        NotificationEventType::MornyeCheck => NotificationMessage::new(
            evt.r#type,
            "Mornye could not pass its check",
            evt.detail.clone().unwrap_or_default(),
            None,
        ),
    }
}

/// The stats a song card shows, in display order, skipping unknowns.
/// Bitrate is derived from size and duration: it describes the actual file,
/// which is the number a lossless-vs-lossy glance wants.
fn track_fields(evt: &NotificationEvent) -> Vec<(String, String)> {
    let mut fields = Vec::new();
    let mut add = |name: &str, value: Option<String>| {
        if let Some(value) = value.filter(|v| !v.is_empty()) {
            fields.push((name.to_string(), value));
        }
    };

    add("Format", evt.format.clone());
    add("Source", evt.source.clone());
    let size = evt.size_bytes.filter(|bytes| *bytes > 0);
    if let Some(size) = size {
        add("Size", Some(format_size(size)));
    }
    if let Some(dur) = evt.duration_seconds.filter(|seconds| *seconds > 0) {
        add("Length", Some(format_duration(dur)));
        if let Some(b) = size {
            add(
                "Bitrate",
                Some(format!(
                    "\u{2248}{} kbps",
                    group_thousands(b * 8 / i64::from(dur) / 1000)
                )),
            );
        }
    }
    if let Some(year) = evt.year.filter(|year| *year > 0) {
        add("Year", Some(year.to_string()));
    }
    // Last, because on a single-user library it is always the same name and the
    // fields above are what anyone is actually reading.
    if let Some(askers) = evt.requested_by.as_ref().filter(|askers| !askers.is_empty()) {
        add("Requested by", Some(askers.join(", ")));
    }
    fields
}

fn album_fields(evt: &NotificationEvent) -> Vec<(String, String)> {
    [
        ("Tracks", evt.track_count),
        ("Lossless", evt.lossless_count),
        ("Failed", evt.failed_count),
        ("Already yours", evt.kept_count),
        ("Upgrading", evt.upgrading_count),
    ]
    .into_iter()
    .map(|(name, count)| (name.to_string(), count.unwrap_or(0).to_string()))
    .collect()
}

/// A whole number with its thousands separated by commas.
fn group_thousands(value: i64) -> String {
    let digits = value.unsigned_abs().to_string();
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    if value < 0 { format!("-{out}") } else { out }
}

/// One decimal (or none) rounded half to even on the double's exact value, as `{:.1}` does.
pub fn format_size(bytes: i64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    }
}

pub fn format_duration(seconds: i32) -> String {
    if seconds >= 3600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            seconds % 3600 / 60,
            seconds % 60
        )
    } else {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(r#type: NotificationEventType) -> NotificationEvent {
        NotificationEvent::new(r#type)
    }

    #[test]
    fn started_message_names_the_format_up_front() {
        let msg = render(&NotificationEvent {
            artist: Some("Randy Rogers Band".into()),
            title: Some("In My Arms Instead".into()),
            format: Some("FLAC".into()),
            source: Some("Soulseek".into()),
            size_bytes: Some(34_684_600),
            ..event(NotificationEventType::DownloadStarted)
        });

        // "Did it find lossless or is it settling?" is the event's entire reason
        // to exist, so the answer leads the body.
        assert!(msg.body.contains("FLAC via Soulseek"), "{}", msg.body);
        assert!(msg.body.contains("33.1 MB"), "{}", msg.body);
        assert!(msg.title.starts_with("Downloading:"), "{}", msg.title);
    }

    #[test]
    fn mornyes_check_push_carries_its_words_and_has_its_own_switch() {
        let words = "Mornye could not pass the check for Example, so Octo asks Soulseek in the meantime.";
        let msg = render(&NotificationEvent {
            detail: Some(words.into()),
            ..event(NotificationEventType::MornyeCheck)
        });
        assert_eq!(msg.title, "Mornye could not pass its check");
        assert_eq!(msg.body, words);
        let on = NotificationSettings::default();
        assert!(on.notify_mornye_check, "on unless switched off");
        assert!(is_enabled(&on, NotificationEventType::MornyeCheck));
        let off = NotificationSettings {
            notify_mornye_check: false,
            ..Default::default()
        };
        assert!(!is_enabled(&off, NotificationEventType::MornyeCheck));
    }

    #[test]
    fn fallback_message_says_what_was_lost() {
        let msg = render(&NotificationEvent {
            artist: Some("A".into()),
            title: Some("B".into()),
            detail: Some("No Soulseek FLAC found".into()),
            ..event(NotificationEventType::LosslessFallback)
        });

        assert!(msg.body.contains("MP3"));
        assert!(msg.body.contains("No Soulseek FLAC found"));
    }

    #[test]
    fn test_event_bypasses_every_toggle() {
        let everything_off = NotificationSettings {
            notify_download_started: false,
            notify_download_completed: false,
            notify_lossless_fallback: false,
            notify_download_failed: false,
            notify_album_completed: false,
            ..Default::default()
        };

        // The test button exists to verify transports; toggles must not mute it.
        assert!(is_enabled(&everything_off, NotificationEventType::Test));
    }

    #[test]
    fn album_summary_counts_tracks_and_losslessness() {
        let msg = render(&NotificationEvent {
            artist: Some("Tame Impala".into()),
            title: Some("Currents".into()),
            track_count: Some(15),
            lossless_count: Some(12),
            failed_count: Some(1),
            ..event(NotificationEventType::AlbumCompleted)
        });

        assert!(
            msg.body.contains("15 tracks fetched, 12 lossless, 1 failed"),
            "{}",
            msg.body
        );
    }

    /// Every track failing used to arrive titled "Album complete".
    #[test]
    fn album_summary_with_nothing_fetched_says_failed() {
        let msg = render(&NotificationEvent {
            artist: Some("Tame Impala".into()),
            title: Some("Currents".into()),
            track_count: Some(0),
            lossless_count: Some(0),
            failed_count: Some(12),
            ..event(NotificationEventType::AlbumCompleted)
        });

        assert!(msg.title.starts_with("Album failed:"), "{}", msg.title);
    }

    /// The heart chain has one to three sources, so "Both" was wrong for most setups.
    #[test]
    fn failure_without_detail_does_not_assume_two_sources() {
        let msg = render(&NotificationEvent {
            artist: Some("A".into()),
            title: Some("B".into()),
            ..event(NotificationEventType::DownloadFailed)
        });

        assert!(!msg.body.contains("Both"));
    }

    /// The exact texts, checked against the reference rendering.
    #[test]
    fn rendered_texts_match_word_for_word() {
        let completed = render(&NotificationEvent {
            artist: Some("Sigur Rós".into()),
            title: Some("Hoppípolla".into()),
            album: Some("Takk...".into()),
            format: Some("FLAC".into()),
            source: Some("Soulseek".into()),
            size_bytes: Some(34_684_600),
            duration_seconds: Some(4 * 60 + 28),
            year: Some(2005),
            requested_by: Some(vec!["alice".into(), "bob".into()]),
            ..event(NotificationEventType::DownloadCompleted)
        });
        assert_eq!(completed.title, "Downloaded: Sigur Rós \u{2013} Hoppípolla");
        assert_eq!(completed.body, "Takk...\nFLAC via Soulseek, 33.1 MB \u{b7} 4:28");
        assert_eq!(completed.description.as_deref(), Some("Takk..."));
        assert_eq!(
            completed.fields.expect("a song card"),
            [
                ("Format", "FLAC"),
                ("Source", "Soulseek"),
                ("Size", "33.1 MB"),
                ("Length", "4:28"),
                ("Bitrate", "\u{2248}1,035 kbps"),
                ("Year", "2005"),
                ("Requested by", "alice, bob"),
            ]
            .map(|(name, value)| (name.to_string(), value.to_string()))
        );

        let bare = render(&event(NotificationEventType::DownloadStarted));
        assert_eq!(bare.title, "Downloading: unknown track");
        assert_eq!(bare.body, " via ");
        assert_eq!(bare.fields, Some(Vec::new()));

        let album = render(&NotificationEvent {
            title: Some("Currents".into()),
            kept_count: Some(2),
            upgrading_count: Some(1),
            ..event(NotificationEventType::AlbumCompleted)
        });
        assert_eq!(album.title, "Album complete: Currents");
        assert_eq!(
            album.body,
            " tracks fetched,  lossless, 2 already yours, 1 upgrading"
        );
        assert_eq!(
            album.fields.expect("counts"),
            [
                ("Tracks", "0"),
                ("Lossless", "0"),
                ("Failed", "0"),
                ("Already yours", "2"),
                ("Upgrading", "1")
            ]
            .map(|(name, value)| (name.to_string(), value.to_string()))
        );

        let test = render(&event(NotificationEventType::Test));
        assert_eq!(
            (test.title.as_str(), test.body.as_str(), test.image_url),
            (
                "Octo test notification",
                "Transports are wired up correctly.",
                None
            )
        );
    }

    /// Sizes and lengths as the cards show them (one decimal, rounded half to even).
    #[test]
    fn sizes_and_durations_format_with_half_to_even_rounding() {
        assert_eq!(format_size(512), "0 KB");
        assert_eq!(format_size(1536), "2 KB");
        assert_eq!(format_size(2560), "2 KB");
        assert_eq!(format_size(1_310_720), "1.2 MB");
        assert_eq!(format_size(34_684_600), "33.1 MB");
        assert_eq!(format_size(3 * 1024 * 1024 * 1024 / 2), "1.5 GB");
        assert_eq!(format_duration(59), "0:59");
        assert_eq!(format_duration(223), "3:43");
        assert_eq!(format_duration(3600), "1:00:00");
        assert_eq!(format_duration(3725), "1:02:05");
        assert_eq!(group_thousands(1_234_567), "1,234,567");
        assert_eq!(group_thousands(999), "999");
        assert_eq!(group_thousands(1000), "1,000");
    }

    fn family_event(r#type: NotificationEventType) -> NotificationEvent {
        NotificationEvent {
            title: Some("Hyperballad".into()),
            artist: Some("Björk".into()),
            family_member: Some("Alex".into()),
            ..event(r#type)
        }
    }

    #[test]
    fn render_family_request() {
        let new = render(&NotificationEvent {
            format: Some("FLAC".into()),
            ..family_event(NotificationEventType::FamilyRequestNew)
        });
        assert_eq!(new.title, "Alex asked for Hyperballad by Björk (FLAC)");
        assert_eq!(new.body, "Approve or decline it on the Family page.");

        let device = render(&NotificationEvent {
            source: Some("Symfonium".into()),
            ..family_event(NotificationEventType::FamilyDeviceNew)
        });
        assert_eq!(device.title, "New device for Alex: Symfonium");

        let failed = render(&NotificationEvent {
            detail: Some("No real FLAC copy was found.".into()),
            ..family_event(NotificationEventType::FamilyRequestFailed)
        });
        assert_eq!(failed.title, "Could not get Hyperballad by Björk for Alex");
        assert_eq!(failed.body, "No real FLAC copy was found.");

        let nobody = render(&NotificationEvent::new(NotificationEventType::FamilyRequestNew));
        assert_eq!(nobody.title, "Someone asked for a song");
    }

    #[test]
    fn render_member_decision() {
        let approved = render_for_member(&family_event(NotificationEventType::FamilyRequestApproved));
        assert_eq!(approved.title, "Approved: Hyperballad by Björk");
        let added = render_for_member(&family_event(NotificationEventType::FamilyRequestAdded));
        assert_eq!(added.title, "Added: Hyperballad by Björk is in your library");
        let declined = render_for_member(&NotificationEvent {
            family_note: Some("We have it on vinyl".into()),
            ..family_event(NotificationEventType::FamilyRequestDeclined)
        });
        assert_eq!(
            declined.title,
            "Declined: Hyperballad by Björk. We have it on vinyl"
        );
        let failed = render_for_member(&family_event(NotificationEventType::FamilyRequestFailed));
        assert_eq!(failed.title, "Could not get Hyperballad by Björk");
        // The member's text never names who asked.
        for message in [&approved, &added, &declined, &failed] {
            assert!(!message.title.contains("Alex") && !message.body.contains("Alex"));
        }
    }

    #[test]
    fn family_switches_follow_their_settings() {
        let mut s = NotificationSettings::default();
        assert!(is_enabled(&s, NotificationEventType::FamilyRequestNew));
        assert!(is_enabled(&s, NotificationEventType::FamilyDeviceNew));
        s.notify_family_requests = false;
        assert!(!is_enabled(&s, NotificationEventType::FamilyRequestNew));
        assert!(!is_enabled(&s, NotificationEventType::FamilyRequestFailed));
        assert!(is_enabled(&s, NotificationEventType::FamilyDeviceNew));
        s.notify_family_devices = false;
        assert!(!is_enabled(&s, NotificationEventType::FamilyDeviceNew));
    }
}
