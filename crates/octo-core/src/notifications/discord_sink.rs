//! The Discord sink's wire shape ([`build_payload`], its colors and [`truncate`]). The sink
//! that posts it is `octo::services::notifications::discord_sink`.

use chrono::{DateTime, Utc};
use serde_json::{Map, Value, json};

use super::i_notification_sink::NotificationMessage;
use super::notification_event::NotificationEventType;
use crate::common::dotnet;

/// One rich embed and no top-level content. Taking the time as an argument lets tests pin the
/// exact wire shape without HTTP; the sink passes the current UTC time.
pub fn build_payload(message: &NotificationMessage, now: DateTime<Utc>) -> Value {
    let mut embed = Map::new();
    embed.insert("title".into(), json!(truncate(&message.title, 256)));
    embed.insert("color".into(), json!(color_for(message.r#type)));
    embed.insert("footer".into(), json!({ "text": "Octo" }));
    embed.insert("timestamp".into(), json!(round_trip(now)));

    let image = message.image_url.as_deref().filter(|url| !url.is_empty());
    match message.fields.as_ref().filter(|fields| !fields.is_empty()) {
        Some(fields) => {
            // Song-card layout: the album as the description line, the stats as
            // inline fields (Discord flows up to three per row), and the cover
            // full-width. Body prose is NOT repeated here: the fields carry the
            // same facts, structured.
            if let Some(description) = message.description.as_deref().filter(|d| !d.is_empty()) {
                embed.insert("description".into(), json!(truncate(description, 4096)));
            }
            embed.insert(
                "fields".into(),
                Value::Array(
                    fields
                        .iter()
                        .map(|(name, value)| {
                            json!({
                                "name": truncate(name, 256),
                                "value": truncate(value, 1024),
                                "inline": true,
                            })
                        })
                        .collect(),
                ),
            );
            // Full-width art, not the corner thumbnail: on a song card the cover
            // IS the card.
            if let Some(url) = image {
                embed.insert("image".into(), json!({ "url": url }));
            }
        }
        None => {
            embed.insert("description".into(), json!(truncate(&message.body, 4096)));
            // Key omitted entirely when there is no cover: Discord rejects a null url.
            if let Some(url) = image {
                embed.insert("thumbnail".into(), json!({ "url": url }));
            }
        }
    }

    json!({ "embeds": [Value::Object(embed)] })
}

pub fn color_for(r#type: NotificationEventType) -> i32 {
    match r#type {
        NotificationEventType::DownloadStarted => 0x3B82F6,
        NotificationEventType::DownloadCompleted => 0x22C55E,
        NotificationEventType::LosslessFallback => 0xF59E0B,
        NotificationEventType::DownloadFailed => 0xEF4444,
        NotificationEventType::AlbumCompleted => 0x8B5CF6,
        NotificationEventType::Test => 0x64748B,
        NotificationEventType::FamilyRequestNew => 0x3B82F6,
        NotificationEventType::FamilyRequestApproved => 0x22C55E,
        NotificationEventType::FamilyRequestDeclined => 0x64748B,
        NotificationEventType::FamilyRequestAdded => 0x22C55E,
        NotificationEventType::FamilyRequestFailed => 0xEF4444,
        NotificationEventType::FamilyDeviceNew => 0x8B5CF6,
        NotificationEventType::MornyeCheck => 0xF59E0B,
    }
}

/// The first `max` UTF-16 code units of `s`, or all of it when it fits. A surrogate pair that
/// the cut would split is left out whole, since half of one is not a valid string.
pub fn truncate(s: &str, max: usize) -> String {
    if dotnet::utf16_len(s) <= max {
        return s.to_string();
    }
    let mut units = 0;
    let mut out = String::new();
    for c in s.chars() {
        units += c.len_utf16();
        if units > max {
            break;
        }
        out.push(c);
    }
    out
}

/// A UTC time in the round-trip format: seven fractional digits and a `Z`.
pub fn round_trip(now: DateTime<Utc>) -> String {
    format!(
        "{}.{:07}Z",
        now.format("%Y-%m-%dT%H:%M:%S"),
        now.timestamp_subsec_nanos() / 100
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notifications::notification_event::NotificationEvent;
    use crate::notifications::notification_service::render;

    fn message(title: &str, body: &str, image: Option<&str>) -> NotificationMessage {
        NotificationMessage::new(NotificationEventType::DownloadCompleted, title, body, image)
    }

    fn default_message() -> NotificationMessage {
        message(
            "Downloaded: A \u{2013} B",
            "FLAC via Soulseek, 33.1 MB",
            Some("https://cdn.example/cover.jpg"),
        )
    }

    fn embed(payload: &Value) -> &Value {
        &payload["embeds"][0]
    }

    #[test]
    fn embed_carries_title_body_thumbnail_and_color() {
        let payload = build_payload(&default_message(), Utc::now());
        let embed = embed(&payload);

        assert_eq!(embed["title"], "Downloaded: A \u{2013} B");
        assert_eq!(embed["description"], "FLAC via Soulseek, 33.1 MB");
        assert_eq!(embed["color"], 0x22C55E);
        assert_eq!(embed["thumbnail"]["url"], "https://cdn.example/cover.jpg");
        assert_eq!(embed["footer"]["text"], "Octo");
        assert!(embed.get("timestamp").is_some());
    }

    #[test]
    fn discord_limits_are_enforced() {
        let payload = build_payload(
            &message(
                &"t".repeat(300),
                &"d".repeat(5000),
                Some("https://cdn.example/cover.jpg"),
            ),
            Utc::now(),
        );
        let embed = embed(&payload);

        assert_eq!(embed["title"].as_str().map(str::len), Some(256));
        assert_eq!(embed["description"].as_str().map(str::len), Some(4096));
    }

    #[test]
    fn fields_turn_the_embed_into_a_song_card() {
        let rendered = render(&NotificationEvent {
            artist: Some("Randy Rogers Band".into()),
            title: Some("In My Arms Instead".into()),
            album: Some("Randy Rogers Band".into()),
            format: Some("FLAC".into()),
            source: Some("Soulseek".into()),
            size_bytes: Some(34_684_600),
            duration_seconds: Some(223),
            year: Some(2008),
            cover_art_url: Some("https://cdn.example/cover.jpg".into()),
            ..NotificationEvent::new(NotificationEventType::DownloadCompleted)
        });
        let payload = build_payload(&rendered, Utc::now());
        let embed = embed(&payload);

        // Album as the description line, stats as inline fields, cover full-width.
        assert_eq!(embed["description"], "Randy Rogers Band");
        let fields = embed["fields"].as_array().expect("fields");
        let field = |name: &str| {
            fields
                .iter()
                .find(|f| f["name"] == name)
                .and_then(|f| f["value"].as_str())
                .unwrap_or_else(|| panic!("no {name} field"))
                .to_string()
        };
        assert_eq!(field("Format"), "FLAC");
        assert_eq!(field("Source"), "Soulseek");
        assert_eq!(field("Size"), "33.1 MB");
        assert_eq!(field("Length"), "3:43");
        assert_eq!(field("Bitrate"), "\u{2248}1,244 kbps");
        assert_eq!(field("Year"), "2008");
        assert!(fields.iter().all(|f| f["inline"] == true));
        // The card uses the full-width image slot, not the corner thumbnail, and
        // does not repeat the prose body alongside the structured fields.
        assert_eq!(embed["image"]["url"], "https://cdn.example/cover.jpg");
        assert!(embed.get("thumbnail").is_none());
        assert!(
            !embed["description"]
                .as_str()
                .unwrap_or("")
                .contains("FLAC via Soulseek")
        );
    }

    #[test]
    fn unknown_stats_are_omitted_from_the_card_not_rendered_as_placeholders() {
        let rendered = render(&NotificationEvent {
            artist: Some("A".into()),
            title: Some("B".into()),
            format: Some("MP3".into()),
            source: Some("YouTube".into()),
            // no size, no duration, no year: the YouTube started path
            ..NotificationEvent::new(NotificationEventType::DownloadStarted)
        });
        let payload = build_payload(&rendered, Utc::now());

        let names: Vec<&str> = embed(&payload)["fields"]
            .as_array()
            .expect("fields")
            .iter()
            .filter_map(|f| f["name"].as_str())
            .collect();
        assert_eq!(names, ["Format", "Source"]);
    }

    #[test]
    fn album_summary_renders_its_counts_as_fields() {
        let rendered = render(&NotificationEvent {
            artist: Some("Tame Impala".into()),
            title: Some("Currents".into()),
            track_count: Some(15),
            lossless_count: Some(12),
            failed_count: Some(1),
            ..NotificationEvent::new(NotificationEventType::AlbumCompleted)
        });
        let payload = build_payload(&rendered, Utc::now());

        let fields = embed(&payload)["fields"].as_array().expect("fields").clone();
        let field = |name: &str| {
            fields
                .iter()
                .find(|f| f["name"] == name)
                .map(|f| f["value"].clone())
        };
        assert_eq!(field("Tracks"), Some(json!("15")));
        assert_eq!(field("Lossless"), Some(json!("12")));
        assert_eq!(field("Failed"), Some(json!("1")));
    }

    #[test]
    fn no_thumbnail_key_without_cover_art() {
        let payload = build_payload(
            &message("Downloaded: A \u{2013} B", "FLAC via Soulseek, 33.1 MB", None),
            Utc::now(),
        );

        // Discord 400s on {"url": null}; the key must be absent, not null.
        assert!(embed(&payload).get("thumbnail").is_none());
    }

    /// The whole payload byte for byte, the timestamp in the round-trip format, and a cut that
    /// would split a surrogate pair.
    #[test]
    fn the_payload_keeps_its_wire_shape() {
        let now = DateTime::parse_from_rfc3339("2026-10-04T12:00:00.1234567Z")
            .expect("a date")
            .with_timezone(&Utc);
        let payload = build_payload(&message("Sigur Rós", "b", None), now);
        assert_eq!(
            crate::json::to_string(&payload),
            r#"{"embeds":[{"title":"Sigur R\u00F3s","color":2278750,"footer":{"text":"Octo"},"timestamp":"2026-10-04T12:00:00.1234567Z","description":"b"}]}"#
        );
        assert_eq!(
            round_trip(now - chrono::TimeDelta::nanoseconds(123_456_700)),
            "2026-10-04T12:00:00.0000000Z"
        );
        assert_eq!(truncate("ab\u{1F3B5}", 3), "ab");
        assert_eq!(truncate("ab\u{1F3B5}", 4), "ab\u{1F3B5}");
    }
}
