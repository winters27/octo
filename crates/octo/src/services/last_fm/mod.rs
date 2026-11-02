//! Last.fm with I/O: the web-service client the radio and search read, the scrobbler with the
//! dashboard's Connect flow, and Last.fm radio (the state store, the recommendation build, the
//! refresh queue and worker, the stream and its cache, sessions and transcoder, and the warmup).
//! The records, the search cleanup, the request signature and the radio's pure rules are
//! `octo_core::last_fm`.

pub mod icy_metadata_stream;
pub mod last_fm_radio_audio_transcoder;
pub mod last_fm_radio_recommendation_service;
pub mod last_fm_radio_refresh_queue;
pub mod last_fm_radio_refresh_worker;
pub mod last_fm_radio_state_store;
pub mod last_fm_radio_stream_service;
pub mod last_fm_radio_stream_session_store;
pub mod last_fm_radio_track_cache;
pub mod last_fm_radio_track_resolver;
pub mod last_fm_radio_warmup_service;
pub mod last_fm_scrobble_service;
pub mod last_fm_service;
pub mod lookup_trouble;
pub mod radio_tune_in_selector;

pub use icy_metadata_stream::IcyMetadataStream;
pub use last_fm_radio_audio_transcoder::{FfmpegLastFmRadioAudioTranscoder, ILastFmRadioAudioTranscoder};
pub use last_fm_radio_recommendation_service::LastFmRadioRecommendationService;
pub use last_fm_radio_refresh_queue::{LastFmRadioRefreshJob, LastFmRadioRefreshQueue};
pub use last_fm_radio_refresh_worker::LastFmRadioRefreshWorker;
pub use last_fm_radio_state_store::LastFmRadioStateStore;
pub use last_fm_radio_stream_service::{
    LastFmRadioStreamService, LastFmRadioStreamServiceParts, RadioWarmupResult,
};
pub use last_fm_radio_stream_session_store::{
    LastFmRadioStreamSession, LastFmRadioStreamSessionStore, PreparedRadioTrack,
};
pub use last_fm_radio_track_cache::LastFmRadioTrackCache;
pub use last_fm_radio_track_resolver::LastFmRadioTrackResolver;
pub use last_fm_radio_warmup_service::LastFmRadioWarmupService;
pub use last_fm_scrobble_service::{LastFmConnectError, LastFmScrobbleService, ScrobbleTime, ScrobbleTuning};
pub use last_fm_service::LastFmService;
pub use radio_tune_in_selector::{IRadioTuneInSelector, RandomRadioTuneInSelector};

/// The caller's token, or a deadline, ran out. The radio services answer it as an error of its
/// own, so a caller can tell a cancellation from a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("The operation was canceled.")]
pub struct OperationCanceled;

/// The lenient number parsers the Last.fm readers lean on.
pub(crate) mod net_parse {
    /// The white space skipped at either end: spaces and the controls U+0009 to U+000D.
    fn trim(text: &str) -> &str {
        text.trim_matches(|c: char| c == ' ' || ('\u{9}'..='\u{d}').contains(&c))
    }

    /// An integer: optional white space, an optional sign, and decimal digits that fit.
    pub fn parse_long(text: &str) -> Option<i64> {
        let text = trim(text);
        let digits = text.strip_prefix(['+', '-']).unwrap_or(text);
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        text.parse().ok()
    }

    /// A floating-point number: optional white space, an optional sign, digits with an optional
    /// fraction and exponent, and commas in the whole part when `thousands`. "Infinity" and
    /// "NaN" read in any case; Rust's own "inf" does not.
    pub fn parse_double(text: &str, thousands: bool) -> Option<f64> {
        let text = trim(text);
        let unsigned = text.strip_prefix(['+', '-']).unwrap_or(text);
        let lower = unsigned.to_ascii_lowercase();
        if lower == "infinity" || lower == "nan" {
            return text.parse().ok();
        }
        if unsigned.is_empty() || !unsigned.starts_with(|c: char| c.is_ascii_digit() || c == '.') {
            return None;
        }
        let whole_end = unsigned.find(['.', 'e', 'E']).unwrap_or(unsigned.len());
        let candidate = if thousands && unsigned[..whole_end].contains(',') {
            if unsigned.starts_with(',') {
                return None;
            }
            text.replacen(&unsigned[..whole_end], &unsigned[..whole_end].replace(',', ""), 1)
        } else {
            text.to_string()
        };
        if !candidate
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-'))
        {
            return None;
        }
        candidate.parse().ok()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn numbers_read_with_white_space_signs_and_thousands() {
            assert_eq!(parse_long(" 180000 "), Some(180_000));
            assert_eq!(parse_long("-5"), Some(-5));
            assert_eq!(parse_long("+5"), Some(5));
            assert_eq!(parse_long("1.5"), None);
            assert_eq!(parse_long("1e3"), None);
            assert_eq!(parse_long(""), None);
            assert_eq!(parse_long("True"), None);
            assert_eq!(parse_long("99999999999999999999"), None);
            assert_eq!(parse_double("0.9", false), Some(0.9));
            assert_eq!(parse_double(" -1e3 ", false), Some(-1000.0));
            assert_eq!(parse_double(".5", false), Some(0.5));
            assert_eq!(parse_double("1,000.5", true), Some(1000.5));
            assert_eq!(parse_double("1,000.5", false), None);
            assert_eq!(parse_double("inf", false), None);
            assert_eq!(parse_double("Infinity", false), Some(f64::INFINITY));
            assert!(parse_double("NaN", false).is_some_and(f64::is_nan));
            assert_eq!(parse_double("True", false), None);
            assert_eq!(parse_double("", false), None);
        }
    }
}
