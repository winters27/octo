//! The ntfy sink's pure parts: the topic URL, the body and the tag. The sink that publishes is
//! `octo::services::notifications::ntfy_sink`.

use super::i_notification_sink::NotificationMessage;
use super::notification_event::NotificationEventType;

/// ntfy is plain text, so the song card renders as lines rather than embed
/// fields: the album on its own line, then the stats dot-separated. Built
/// from the same NotificationMessage as Discord's card, so the transports
/// carry identical facts.
pub fn build_body(message: &NotificationMessage) -> String {
    let Some(fields) = message.fields.as_ref().filter(|fields| !fields.is_empty()) else {
        return message.body.clone();
    };

    let stats = fields
        .iter()
        .map(|(_, value)| value.as_str())
        .collect::<Vec<_>>()
        .join(" \u{b7} ");
    match message.description.as_deref() {
        None | Some("") => stats,
        Some(description) => format!("{description}\n{stats}"),
    }
}

/// Why [`parse_topic_url`] refused a URL, in the message the admin test button shows verbatim.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct TopicUrlError(pub String);

/// "https://ntfy.sh/octo" -> ("https://ntfy.sh", "octo");
/// "https://host/ntfy/octo" -> ("https://host/ntfy", "octo").
/// The config stays one field (the URL users copy out of the ntfy app) and the sink derives the
/// server root and topic itself. A URL with no topic is refused with a message the admin test
/// button surfaces verbatim.
pub fn parse_topic_url(url: &str) -> Result<(String, String), TopicUrlError> {
    let trimmed = url.trim().trim_end_matches('/');
    // An absolute URL only; anything else is refused in the admin test button's wording.
    let uri = url::Url::parse(trimmed).map_err(|error| {
        TopicUrlError(
            match error {
                url::ParseError::EmptyHost
                | url::ParseError::IdnaError
                | url::ParseError::InvalidDomainCharacter
                | url::ParseError::InvalidIpv4Address
                | url::ParseError::InvalidIpv6Address => "Invalid URI: The hostname could not be parsed.",
                url::ParseError::InvalidPort => "Invalid URI: Invalid port specified.",
                _ => "Invalid URI: The format of the URI could not be determined.",
            }
            .to_string(),
        )
    })?;
    // The path's segments; the topic is the last one.
    let segments: Vec<&str> = uri
        .path_segments()
        .map(|segments| segments.collect())
        .unwrap_or_default();
    let topic = match segments.as_slice() {
        [] | [""] => "",
        [.., last] => last.trim_matches('/'),
    };
    if topic.is_empty() {
        return Err(TopicUrlError(
            "ntfy URL must include a topic, e.g. https://ntfy.sh/your-topic".to_string(),
        ));
    }
    let root = &trimmed[..trimmed.rfind('/').unwrap_or(0)];
    Ok((root.to_string(), topic.to_string()))
}

pub fn tag_for(r#type: NotificationEventType) -> &'static str {
    match r#type {
        NotificationEventType::DownloadStarted => "arrow_down",
        NotificationEventType::DownloadCompleted => "white_check_mark",
        NotificationEventType::LosslessFallback => "warning",
        NotificationEventType::DownloadFailed => "x",
        NotificationEventType::AlbumCompleted => "cd",
        NotificationEventType::Test => "bell",
        NotificationEventType::FamilyRequestNew => "inbox_tray",
        NotificationEventType::FamilyRequestApproved => "thumbsup",
        NotificationEventType::FamilyRequestDeclined => "no_entry_sign",
        NotificationEventType::FamilyRequestAdded => "white_check_mark",
        NotificationEventType::FamilyRequestFailed => "x",
        NotificationEventType::FamilyDeviceNew => "iphone",
        NotificationEventType::MornyeCheck => "lock",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topic_url_splits_into_root_and_topic() {
        for (url, root, topic) in [
            ("https://ntfy.sh/octo", "https://ntfy.sh", "octo"),
            ("https://host/ntfy/octo", "https://host/ntfy", "octo"),
            ("https://ntfy.sh/octo/", "https://ntfy.sh", "octo"),
        ] {
            let parsed = parse_topic_url(url).expect(url);

            assert_eq!(parsed.0, root, "{url}");
            assert_eq!(parsed.1, topic, "{url}");
        }
    }

    #[test]
    fn bare_server_url_is_reported_not_swallowed() {
        // The admin test button surfaces this message verbatim, so it names the fix.
        let error = parse_topic_url("https://ntfy.sh").expect_err("no topic");

        assert!(error.to_string().contains("must include a topic"), "{error}");
    }

    #[test]
    fn fields_fold_into_dot_separated_stats_with_the_album_line_first() {
        let body = build_body(&NotificationMessage {
            description: Some("Some Album".into()),
            fields: Some(
                [
                    ("Format", "FLAC"),
                    ("Source", "Soulseek"),
                    ("Size", "33.1 MB"),
                    ("Length", "3:43"),
                ]
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .to_vec(),
            ),
            ..NotificationMessage::new(
                NotificationEventType::DownloadCompleted,
                "Downloaded: A \u{2013} B",
                "prose fallback",
                None,
            )
        });

        assert_eq!(
            body,
            "Some Album\nFLAC \u{b7} Soulseek \u{b7} 33.1 MB \u{b7} 3:43"
        );
    }

    #[test]
    fn prose_events_keep_their_body_verbatim() {
        let body = build_body(&NotificationMessage::new(
            NotificationEventType::LosslessFallback,
            "Lossless miss: A \u{2013} B",
            "Soulseek failed (nothing usable); settling for YouTube MP3.",
            None,
        ));

        assert_eq!(
            body,
            "Soulseek failed (nothing usable); settling for YouTube MP3."
        );
    }

    /// What the URL parser refuses, with the reference messages, and a query after the topic.
    #[test]
    fn refused_urls_give_the_reference_messages() {
        assert_eq!(
            parse_topic_url("ntfy.sh/octo").expect_err("relative").0,
            "Invalid URI: The format of the URI could not be determined."
        );
        assert_eq!(
            parse_topic_url("https://").expect_err("no host").0,
            "Invalid URI: The hostname could not be parsed."
        );
        assert_eq!(
            parse_topic_url("https://ntfy.sh/octo?x=a/b").expect("a topic"),
            ("https://ntfy.sh/octo?x=a".to_string(), "octo".to_string())
        );
    }
}
