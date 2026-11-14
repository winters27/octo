//! The decisions abilities lead to, as pure functions: what a heart does, how a library
//! stream is capped, whether downloads and shares are allowed, and what happens to a request.

use super::abilities::{Abilities, AddToLibrary, RequestQuality};
use super::device::NetworkPlace;
use super::quality::{DeviceQualityMode, QualityPreference};

/// What a heart on a song does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeartAction {
    /// A library song: Navidrome stars it, as today.
    Relay,
    /// An outside song: Octo downloads it (into the member's own library for a member).
    Download,
    /// An outside song: Octo saves a link; no download.
    Save,
}

/// What a heart does for a caller. `abilities` is None for the owner and unmanaged accounts,
/// who keep today's behavior.
pub fn heart_action(abilities: Option<&Abilities>, is_outside: bool) -> HeartAction {
    if !is_outside {
        return HeartAction::Relay;
    }
    match abilities.map(|a| a.add_to_library) {
        None | Some(AddToLibrary::Direct) => HeartAction::Download,
        Some(AddToLibrary::Request | AddToLibrary::SaveOnly) => HeartAction::Save,
    }
}

/// How a library `stream` (or `download`) request is rewritten.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StreamRewrite {
    /// The format to ask Navidrome for: `opus` when a ceiling applies and the client named no
    /// format of its own; None leaves the client's format as it is.
    pub format: Option<&'static str>,
    /// The `maxBitRate` to ask for.
    pub max_bit_rate: Option<u32>,
    /// Remove `format=raw`, which skips every ceiling.
    pub drop_raw: bool,
}

/// Why a stream is refused, with the words the member reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Away from home with listening away turned off.
    Away,
}

impl Refusal {
    pub fn message(self) -> &'static str {
        match self {
            Refusal::Away => "Listening away from home is off for this account.",
        }
    }
}

/// The family limit for `place`: the away cap when away and set, else the stream cap. None
/// means no limit.
pub fn cap_for(a: &Abilities, place: NetworkPlace) -> Option<u32> {
    if place == NetworkPlace::Away && a.away_cap > 0 {
        return Some(a.away_cap);
    }
    a.stream_cap.kbps()
}

/// What the client itself asked for on a stream: its `format` and `maxBitRate` (0 or absent
/// means none).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClientAsk<'a> {
    pub format: Option<&'a str>,
    pub max_bit_rate: Option<u32>,
}

/// The rewrite for one library stream. The client's own request comes first. A device in
/// [`DeviceQualityMode::Account`] then takes the account's quality for `place` as a ceiling
/// (Original adds none). The family limit is a ceiling after that. The lowest bitrate of the
/// client's request and the ceilings wins, so a ceiling never raises what the client asked
/// for, and any ceiling drops `format=raw`. With no ceiling nothing changes.
pub fn stream_rewrite(
    a: &Abilities,
    place: NetworkPlace,
    preference: QualityPreference,
    mode: DeviceQualityMode,
    client: ClientAsk<'_>,
) -> Result<StreamRewrite, Refusal> {
    if place == NetworkPlace::Away && !a.away {
        return Err(Refusal::Away);
    }
    let account = match mode {
        DeviceQualityMode::Account => preference.for_place(place).kbps(),
        DeviceQualityMode::App => None,
    };
    let ceiling = [account, cap_for(a, place)].into_iter().flatten().min();
    let Some(ceiling) = ceiling else {
        return Ok(StreamRewrite::default());
    };
    let rate = match client.max_bit_rate.filter(|m| *m > 0) {
        Some(asked) => asked.min(ceiling),
        None => ceiling,
    };
    let own_format = client
        .format
        .map(str::trim)
        .is_some_and(|f| !f.is_empty() && !f.eq_ignore_ascii_case("raw"));
    Ok(StreamRewrite {
        format: (!own_format).then_some("opus"),
        max_bit_rate: Some(rate),
        drop_raw: true,
    })
}

/// `download` and zips.
pub fn may_download(a: &Abilities) -> bool {
    a.download_files
}

/// Share links.
pub fn may_share(a: &Abilities) -> bool {
    a.share
}

/// What becomes of a new request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestDecision {
    /// Not allowed, with the words the member reads.
    Refuse(String),
    /// Waits for an approver.
    Pending,
    /// Approved at once.
    AutoApprove,
}

/// `Some("Your library is full (10.2 of 10 GB)")` when the member's personal library holds
/// their storage limit or more; None when there is room or no limit.
pub fn storage_full(a: &Abilities, used_bytes: u64) -> Option<String> {
    let limit = a.storage_limit_bytes()?;
    (used_bytes >= limit).then(|| {
        format!(
            "Your library is full ({:.1} of {} GB)",
            used_bytes as f64 / 1_000_000_000.0,
            a.storage_limit_gb
        )
    })
}

/// The decision for a new request: above the member's highest quality or past the weekly
/// limit is refused; otherwise auto-approve when the member has it and their library has
/// room, else it waits. `used` is [`super::request::quota_used`].
pub fn request_decision(
    a: &Abilities,
    quality: RequestQuality,
    used: u32,
    storage_used_bytes: u64,
) -> RequestDecision {
    if quality.rank() > a.request_quality.rank() {
        return RequestDecision::Refuse(format!(
            "This account can ask for {} copies at most.",
            a.request_quality.label()
        ));
    }
    if a.weekly_request_limit > 0 && used >= a.weekly_request_limit {
        return RequestDecision::Refuse(format!(
            "You have used all {} of your requests this week.",
            a.weekly_request_limit
        ));
    }
    if a.auto_approve && storage_full(a, storage_used_bytes).is_none() {
        RequestDecision::AutoApprove
    } else {
        RequestDecision::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::family::abilities::{MusicTarget, Preset, StreamCap};

    fn listener() -> Abilities {
        Abilities::preset(Preset::Listener)
    }

    #[test]
    fn listener_outside_heart_saves() {
        assert_eq!(heart_action(Some(&listener()), true), HeartAction::Save);
        let save_only = Abilities {
            add_to_library: AddToLibrary::SaveOnly,
            ..listener()
        };
        assert_eq!(heart_action(Some(&save_only), true), HeartAction::Save);
        assert_eq!(heart_action(Some(&listener()), false), HeartAction::Relay);
    }

    #[test]
    fn member_outside_heart_downloads() {
        let member = Abilities::preset(Preset::Member);
        assert_eq!(heart_action(Some(&member), true), HeartAction::Download);
        assert_eq!(heart_action(Some(&member), false), HeartAction::Relay);
    }

    use crate::family::quality::StreamQuality;

    /// A listener the owner limited to 192 kbps at home and 128 away.
    fn capped() -> Abilities {
        Abilities {
            stream_cap: StreamCap::Kbps(192),
            away_cap: 128,
            ..listener()
        }
    }

    fn no_preference() -> QualityPreference {
        QualityPreference::default()
    }

    fn ask(format: Option<&str>, max_bit_rate: Option<u32>) -> ClientAsk<'_> {
        ClientAsk { format, max_bit_rate }
    }

    /// The rewrite in Account mode with no preference and nothing asked.
    fn plain(a: &Abilities, place: NetworkPlace) -> Result<StreamRewrite, Refusal> {
        stream_rewrite(
            a,
            place,
            no_preference(),
            DeviceQualityMode::Account,
            ClientAsk::default(),
        )
    }

    #[test]
    fn owner_is_unchanged() {
        assert_eq!(heart_action(None, true), HeartAction::Download);
        assert_eq!(heart_action(None, false), HeartAction::Relay);
        let owner = Abilities::preset(Preset::Owner);
        for place in [NetworkPlace::Home, NetworkPlace::Away] {
            assert_eq!(
                stream_rewrite(
                    &owner,
                    place,
                    no_preference(),
                    DeviceQualityMode::Account,
                    ask(Some("raw"), Some(320))
                ),
                Ok(StreamRewrite::default())
            );
        }
    }

    #[test]
    fn every_preset_streams_the_original_until_someone_chooses() {
        for preset in [
            Preset::Owner,
            Preset::CoAdmin,
            Preset::Member,
            Preset::Listener,
            Preset::Kid,
        ] {
            let a = Abilities::preset(preset);
            for place in [NetworkPlace::Home, NetworkPlace::Away] {
                assert_eq!(plain(&a, place), Ok(StreamRewrite::default()), "{preset:?}");
            }
        }
    }

    #[test]
    fn away_off_refuses() {
        let kept_home = Abilities {
            away: false,
            ..Abilities::preset(Preset::Kid)
        };
        assert_eq!(plain(&kept_home, NetworkPlace::Away), Err(Refusal::Away));
        assert!(plain(&kept_home, NetworkPlace::Home).is_ok());
    }

    #[test]
    fn away_cap_applies_only_away() {
        let home = plain(&capped(), NetworkPlace::Home).expect("allowed");
        assert_eq!(home.max_bit_rate, Some(192));
        let away = plain(&capped(), NetworkPlace::Away).expect("allowed");
        assert_eq!(away.max_bit_rate, Some(128));
        assert_eq!(away.format, Some("opus"));
        // An away cap of 0 means the stream cap away too.
        let same = Abilities {
            away_cap: 0,
            ..capped()
        };
        assert_eq!(
            plain(&same, NetworkPlace::Away).expect("allowed").max_bit_rate,
            Some(192)
        );
        // An away cap alone caps only away.
        let away_only = Abilities {
            stream_cap: StreamCap::Original,
            away_cap: 96,
            ..listener()
        };
        assert_eq!(
            plain(&away_only, NetworkPlace::Home),
            Ok(StreamRewrite::default())
        );
        assert_eq!(
            plain(&away_only, NetworkPlace::Away)
                .expect("allowed")
                .max_bit_rate,
            Some(96)
        );
    }

    #[test]
    fn client_lower_request_wins() {
        let rewrite = |max| {
            stream_rewrite(
                &capped(),
                NetworkPlace::Home,
                QualityPreference {
                    home: StreamQuality::High,
                    away: StreamQuality::High,
                },
                DeviceQualityMode::Account,
                ask(Some("mp3"), max),
            )
            .expect("allowed")
        };
        let lower = rewrite(Some(96));
        assert_eq!(lower.max_bit_rate, Some(96));
        // The client's own format stays.
        assert_eq!(lower.format, None);
        assert_eq!(rewrite(Some(320)).max_bit_rate, Some(192));
        // maxBitRate=0 is "no limit" in Subsonic: the ceiling stands.
        assert_eq!(rewrite(Some(0)).max_bit_rate, Some(192));
    }

    #[test]
    fn original_cap_rewrites_nothing() {
        let member = Abilities::preset(Preset::Member);
        let rewrite = stream_rewrite(
            &member,
            NetworkPlace::Away,
            no_preference(),
            DeviceQualityMode::Account,
            ask(Some("raw"), Some(128)),
        )
        .expect("allowed");
        assert_eq!(rewrite, StreamRewrite::default());
        assert!(!rewrite.drop_raw);
        let capped = plain(&capped(), NetworkPlace::Home).expect("allowed");
        assert!(capped.drop_raw);
    }

    #[test]
    fn account_away_preference_caps_raw() {
        let member = Abilities::preset(Preset::Member);
        let preference = QualityPreference {
            home: StreamQuality::Original,
            away: StreamQuality::Standard,
        };
        let away = stream_rewrite(
            &member,
            NetworkPlace::Away,
            preference,
            DeviceQualityMode::Account,
            ask(Some("raw"), None),
        )
        .expect("allowed");
        assert_eq!(
            away,
            StreamRewrite {
                format: Some("opus"),
                max_bit_rate: Some(160),
                drop_raw: true,
            }
        );
        // At home the account chose the original: raw stays raw.
        let home = stream_rewrite(
            &member,
            NetworkPlace::Home,
            preference,
            DeviceQualityMode::Account,
            ask(Some("raw"), None),
        )
        .expect("allowed");
        assert_eq!(home, StreamRewrite::default());
    }

    #[test]
    fn app_mode_ignores_account_preference() {
        let saver = QualityPreference {
            home: StreamQuality::DataSaver,
            away: StreamQuality::DataSaver,
        };
        let member = Abilities::preset(Preset::Member);
        assert_eq!(
            stream_rewrite(
                &member,
                NetworkPlace::Away,
                saver,
                DeviceQualityMode::App,
                ask(Some("raw"), None)
            ),
            Ok(StreamRewrite::default())
        );
        // The family limit still holds in App mode.
        let limited = stream_rewrite(
            &capped(),
            NetworkPlace::Away,
            saver,
            DeviceQualityMode::App,
            ClientAsk::default(),
        )
        .expect("allowed");
        assert_eq!(limited.max_bit_rate, Some(128));
    }

    #[test]
    fn family_limit_wins_over_original_preference() {
        let original = QualityPreference::default();
        let home = stream_rewrite(
            &capped(),
            NetworkPlace::Home,
            original,
            DeviceQualityMode::Account,
            ask(Some("raw"), None),
        )
        .expect("allowed");
        assert_eq!(home.max_bit_rate, Some(192));
        assert!(home.drop_raw);
        // A preference above the limit cannot lift it; one below it wins.
        let high = QualityPreference {
            home: StreamQuality::High,
            away: StreamQuality::DataSaver,
        };
        let rate = |place| {
            stream_rewrite(
                &capped(),
                place,
                high,
                DeviceQualityMode::Account,
                ClientAsk::default(),
            )
            .expect("allowed")
            .max_bit_rate
        };
        assert_eq!(rate(NetworkPlace::Home), Some(192));
        assert_eq!(rate(NetworkPlace::Away), Some(96));
    }

    #[test]
    fn downloads_and_shares_follow_abilities() {
        assert!(!may_download(&listener()));
        assert!(!may_share(&listener()));
        let member = Abilities::preset(Preset::Member);
        assert!(may_download(&member));
        assert!(may_share(&member));
    }

    #[test]
    fn flac_above_kid_max_is_refused() {
        let kid = Abilities::preset(Preset::Kid);
        let RequestDecision::Refuse(message) = request_decision(&kid, RequestQuality::Flac, 0, 0) else {
            panic!("refused");
        };
        assert!(message.contains("MP3"), "{message}");
        assert_eq!(
            request_decision(&kid, RequestQuality::Mp3, 0, 0),
            RequestDecision::Pending
        );
    }

    #[test]
    fn limit_reached_is_refused_with_count() {
        let RequestDecision::Refuse(message) = request_decision(&listener(), RequestQuality::Best, 10, 0)
        else {
            panic!("refused");
        };
        assert!(message.contains("10"), "{message}");
        assert_eq!(
            request_decision(&listener(), RequestQuality::Best, 9, 0),
            RequestDecision::Pending
        );
        // No limit: never refused for the count.
        let member = Abilities::preset(Preset::Member);
        assert_eq!(
            request_decision(&member, RequestQuality::Best, 10_000, 0),
            RequestDecision::AutoApprove
        );
    }

    #[test]
    fn auto_approve_under_limit() {
        let listener_auto = Abilities {
            auto_approve: true,
            ..listener()
        };
        assert_eq!(
            request_decision(&listener_auto, RequestQuality::Flac, 3, 0),
            RequestDecision::AutoApprove
        );
    }

    #[test]
    fn a_full_library_waits_for_an_approver() {
        let member = Abilities::preset(Preset::Member); // 50 GB
        assert_eq!(storage_full(&member, 49_999_999_999), None);
        assert_eq!(
            storage_full(&member, 51_200_000_000).as_deref(),
            Some("Your library is full (51.2 of 50 GB)")
        );
        assert_eq!(
            request_decision(&member, RequestQuality::Best, 0, 60_000_000_000),
            RequestDecision::Pending
        );
        let owner = Abilities::preset(Preset::Owner);
        assert_eq!(storage_full(&owner, u64::MAX), None);
    }

    #[test]
    fn shared_music_is_not_held_to_the_storage_limit() {
        let shared = Abilities {
            music_goes_to: MusicTarget::Shared,
            ..Abilities::preset(Preset::Member)
        };
        assert_eq!(storage_full(&shared, u64::MAX), None);
        assert_eq!(
            request_decision(&shared, RequestQuality::Best, 0, 60_000_000_000),
            RequestDecision::AutoApprove
        );
    }
}
