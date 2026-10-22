//! The Raw config editor and the config-sources table: `GET`/`PUT /api/admin/raw-config` and
//! `GET /api/admin/config-sources`.

use axum::Extension;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use octo_core::common::dotnet::{is_blank, utf16_len};
use octo_core::json::dom::Node;
use octo_core::settings::JsonObject;
use octo_core::settings::writer::to_indented_json;
use serde_json::json;
use tracing::{error, info};

use super::json_body::{body_text, parse_json_node};
use super::plumbing::{error_json, json_text};
use super::secrets::{
    normalize_saved_addresses, restore_secret_placeholders, running_secrets, secret_sent_elsewhere,
    secret_typed_into_placeholder, session_key_typed_into_placeholder,
};
use super::settings::effective_document;
use crate::app::AppState;
use crate::http::error::json_ok;
use crate::services::admin::AdminCaller;

/// `GET /api/admin/raw-config`: the *effective* configuration as a JSON document, with values
/// from the live settings (what the app actually sees). Anything persisted to settings.json
/// sits on top of env vars and appsettings.json defaults; that merged result is what goes out,
/// so the Raw Config editor shows the full picture rather than only the sparse overrides file.
///
/// On PUT the entire body is written wholesale to settings.json, which is a no-op if the user
/// just hits Save without editing (values match env) and a real override otherwise.
pub async fn get_raw_config(State(state): State<AppState>) -> Response {
    let document = effective_document(&state, true);
    // Content(json, "application/json"): no charset.
    json_text(StatusCode::OK, to_indented_json(&document), "application/json")
}

/// `PUT /api/admin/raw-config`: replaces settings.json wholesale with the request body, after
/// checking that it parses as a JSON object, so the user cannot save a broken file that crashes
/// the next start.
pub async fn put_raw_config(
    State(state): State<AppState>,
    caller: Option<Extension<AdminCaller>>,
    body: Bytes,
) -> Response {
    let body = body_text(&body);
    if is_blank(&body) {
        return error_json(StatusCode::BAD_REQUEST, "empty body");
    }
    let mut parsed = match parse_json_node(&body) {
        Ok(Node::Object(map)) => map,
        Ok(_) => {
            return error_json(
                StatusCode::BAD_REQUEST,
                "invalid JSON: top level must be an object",
            );
        }
        Err(message) => return error_json(StatusCode::BAD_REQUEST, &format!("invalid JSON: {message}")),
    };

    // Don't merge: this is the "I know exactly what I want" power-user endpoint. It goes
    // through the writer so it shares the lock and atomic write with form saves, and every hidden
    // credential comes back as a placeholder that must not be saved literally. When the file
    // cannot be read, the running values are the only copy of the credentials left, and this
    // save is the recovery path the 409 points people to.
    let existing: JsonObject = if state.settings_writer.is_readable() {
        state.settings_writer.load()
    } else {
        running_secrets(&state)
    };
    if let Some(elsewhere) = secret_sent_elsewhere(&parsed, |section, name| {
        state.settings.raw(&format!("{section}:{name}"))
    }) {
        return error_json(StatusCode::BAD_REQUEST, &elsewhere);
    }
    restore_secret_placeholders(&mut parsed, &existing);
    if let Some(typed) = secret_typed_into_placeholder(&parsed) {
        return error_json(
            StatusCode::BAD_REQUEST,
            &format!("Retype the whole {typed}; it was added to the hidden placeholder."),
        );
    }
    if let Some(user) = session_key_typed_into_placeholder(&parsed) {
        return error_json(
            StatusCode::BAD_REQUEST,
            &format!(
                "Connect {user} to Last.fm again, or paste their whole session key; it was added to the hidden placeholder."
            ),
        );
    }
    normalize_saved_addresses(&mut parsed);
    let pretty = to_indented_json(&parsed);
    match state.settings_writer.replace(&parsed) {
        Ok(()) => {
            // As a settings save does: the next request sees the new file, not the one before it.
            let _ = state.settings.reload_now();
            let bytes = utf16_len(&pretty);
            info!(
                "{} saved the raw config ({bytes} bytes)",
                AdminCaller::describe(caller.as_ref().map(|Extension(caller)| caller))
            );
            json_ok(&json!({ "ok": true, "bytes": bytes }))
        }
        Err(e) => {
            error!(
                "Failed to write raw config to {}: {e}",
                state.settings_writer.file_path().display()
            );
            error_json(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())
        }
    }
}

/// The keys `GET config-sources` lists, in the dashboard's order.
const CONFIG_SOURCE_KEYS: &[&str] = &[
    "Subsonic:Url",
    "Subsonic:StorageMode",
    "Subsonic:DownloadMode",
    "Subsonic:DownloadOnStar",
    "Subsonic:DownloadAlbumOnStar",
    "Subsonic:RecordRequestedBy",
    "Subsonic:StarDownloadsForRequester",
    "Subsonic:WaitForLosslessOnPlay",
    "Subsonic:LosslessWaitTimeoutSeconds",
    "Subsonic:DownloadOnPlay",
    "Subsonic:LidarrAlbumOnPlay",
    "Subsonic:DownloadSource",
    "Subsonic:AutoDetectDownloadPath",
    "Subsonic:LibraryPath",
    "Subsonic:FolderStructure",
    "Subsonic:UseLocalStaging",
    "Subsonic:ExplicitFilter",
    "Subsonic:CacheDurationHours",
    "Subsonic:EnableExternalPlaylists",
    "Subsonic:WaitForSearchDurations",
    "Subsonic:SkipOwnedSongs",
    "Subsonic:PlaylistsDirectory",
    "Library:DownloadPath",
    "Server:PublicUrl",
    "Updates:Check",
    "Updates:Repo",
    "Soulseek:Enabled",
    "Soulseek:BaseUrl",
    "Soulseek:Username",
    "Soulseek:Password",
    "Soulseek:WebUrl",
    "Soulseek:CheckListenPort",
    "Soulseek:ShareLibrary",
    "Soulseek:SharedFolders",
    "Soulseek:SearchWaitSeconds",
    "Soulseek:UpgradeSearchWaitSeconds",
    "Soulseek:MinFileSizeBytes",
    "Soulseek:PreferredExtension",
    "Soulseek:DownloadTimeoutSeconds",
    "Soulseek:RejectedPeerTtlDays",
    "Soulseek:FingerprintSeconds",
    "Soulseek:FingerprintTimeoutSeconds",
    "Soulseek:AcoustIdTimeoutSeconds",
    "Soulseek:DetectTranscodes",
    "Soulseek:TranscodeCheckTimeoutSeconds",
    "Soulseek:OutageHoldHours",
    "Soulseek:ParallelDownloads",
    "Soulseek:AlbumFolders",
    "Genre:BackfillMaxConsecutiveFailures",
    "Genre:BackfillExtensions",
    "LibraryActions:Enabled",
    "LibraryActions:PlaylistsEnabled",
    "LibraryActions:RatingsEnabled",
    "LibraryActions:PlaylistPrefix",
    "LibraryActions:DryRun",
    "LibraryActions:QuarantineDirectory",
    "LibraryActions:QuarantineRetentionDays",
    "LibraryActions:PollIntervalSeconds",
    "LibraryActions:MaxActionsPerCycle",
    "LibraryActions:KeepReplacedOriginals",
    "LibraryActions:Actions",
    "LibraryActions:AllowedUsers",
    "LibraryActions:NoticePrefix",
    "LibraryActions:ReviewEnabled",
    "LibraryActions:ReviewPlaylistName",
    "LibraryActions:NoticeMaxTracks",
    "LibraryActions:RatingsScope",
    "LibraryActions:DuplicatesEnabled",
    "LibraryActions:DuplicatesPlaylistName",
    "LibraryActions:DuplicatesScanHours",
    "LibraryActions:UpgradePerWeek",
    "LibraryActions:UpgradeSource",
    "LibraryActions:ReviewSweepPerHour",
    "LibraryActions:ReviewSweepOctoDownloads",
    "LibraryActions:TagCheckPerHour",
    "Soulseek:SubmitConfirmedFingerprints",
    "Soulseek:AcoustIdUserApiKey",
    "Genre:Enabled",
    "Genre:MaxGenres",
    "Genre:OnEmpty",
    "Genre:Fallback",
    "Genre:UnknownLabel",
    "Genre:Mappings",
    "Genre:Blocklist",
    "Soulseek:VerifyDownloads",
    "Soulseek:SkipCoversAndInstrumentals",
    "Soulseek:AcoustIdApiKey",
    "Soulseek:MinMatchScore",
    "Soulseek:TagFromMusicBrainz",
    "Soulseek:NameFromMatch",
    "Lidarr:BaseUrl",
    "Lidarr:ApiKey",
    "Lidarr:RootFolderPath",
    "Lidarr:QualityProfileId",
    "Lidarr:MetadataProfileId",
    "Lidarr:CompletionMode",
    "Lidarr:ImportTimeoutSeconds",
    "Lidarr:WebhookSecret",
    "YouTube:ShimUrl",
    "LastFm:ApiKey",
    "LastFm:ApiSecret",
    "LastFm:ScrobbleExternalPlays",
    "LastFm:ScrobbleLibraryPlays",
    "LastFm:EnableRadio",
    "LastFm:RadioTrackCount",
    "LastFm:RadioCacheDurationHours",
    "LastFm:StarterPublishTimeoutSeconds",
    "LastFm:RadioLoudnessTargetLufs",
    "LastFm:EnablePersonalizedStations",
    "LastFm:EnableYourMix",
    "LastFm:EnableDiscoveryMix",
    "LastFm:ArtistStationCount",
    "LastFm:GenreStationCount",
    "LastFm:EnableDiscoveryStations",
    "LastFm:HistoryRetentionDays",
    "LastFm:DiscoveryPercent",
    "LastFm:RefreshIntervalHours",
    "LastFm:MinimumPlays",
    "LastFm:DiscoveryStations",
    "RadioSources:YouTubeMusic",
    "RadioSources:ListenBrainz",
    "RadioSources:SoundsAlike",
    "RadioSources:SonicUrl",
    "RadioSources:SonicPauseSeconds",
    "RadioSources:ListenBrainzAlgorithm",
    "RadioSources:ListenBrainzRadioMode",
    "RadioSources:LastFmWeight",
    "RadioSources:YouTubeMusicWeight",
    "RadioSources:ListenBrainzWeight",
    "RadioSources:SoundsAlikeWeight",
    "RadioSources:LearnFromListening",
    "Metadata:Language",
    "Metadata:AlbumFromTitle",
    "Metadata:UseCoverArtArchive",
    "Metadata:ReplaceVideoCovers",
    "Metadata:WriteCoverFile",
    "Metadata:EmbedFullSizeCovers",
    "Metadata:FetchLyrics",
    "Metadata:LyricsSources",
    "Metadata:PreferWordTimedLyrics",
    "Metadata:WriteLyricsBesideAllSongs",
    "Metadata:SaveLyricsTo",
    "Metadata:PreferOriginalAlbum",
    "Metadata:YearFromOriginalRelease",
    "Metadata:PreferredCountries",
    "Metadata:ReleaseDetailsLookup",
    "Metadata:ReplayGain",
    "Metadata:ReplayGainTimeoutSeconds",
    "Metadata:TagRehearsal",
    "GeneratedPlaylists:PopularNow",
    "GeneratedPlaylists:Charts",
    "GeneratedPlaylists:ChartCountry",
    "GeneratedPlaylists:Enabled",
    "GeneratedPlaylists:Genres",
    "GeneratedPlaylists:Decades",
    "GeneratedPlaylists:TrackCount",
    "GeneratedPlaylists:MaxPerArtist",
    "GeneratedPlaylists:CreateAt",
    "GeneratedPlaylists:RemoveBelow",
    "GeneratedPlaylists:MaxPlaylists",
    "GeneratedPlaylists:RefreshHours",
    "GeneratedPlaylists:NewShare",
    "GeneratedPlaylists:NewDays",
    "GeneratedPlaylists:NameFormat",
    "GeneratedPlaylists:CountryMixes",
    "GeneratedPlaylists:Countries",
    "GeneratedPlaylists:MaxCountryPlaylists",
    "GeneratedPlaylists:CountryLookupsPerRefresh",
    "GeneratedPlaylists:NewReleases",
    "GeneratedPlaylists:Rediscover",
    "GeneratedPlaylists:DeepCuts",
    "GeneratedPlaylists:NewReleaseWeeks",
    "GeneratedPlaylists:NewReleaseArtists",
    "GeneratedPlaylists:RediscoverMonths",
    "GeneratedPlaylists:LikedSongs",
    "GeneratedPlaylists:LikedSongsName",
    "GeneratedPlaylists:LikedSongsMax",
    "Notifications:NtfyUrl",
    "Notifications:NtfyToken",
    "Notifications:DiscordWebhookUrl",
    "Notifications:NotifyDownloadStarted",
    "Notifications:NotifyDownloadCompleted",
    "Notifications:NotifyLosslessFallback",
    "Notifications:NotifyDownloadFailed",
    "Notifications:NotifyAlbumCompleted",
    "ListenBrainz:Token",
    "ListenBrainz:SubmitExternalPlays",
    "Imports:SongsPerHour",
    "Imports:RefreshHours",
    "Subsonic:StreamSizeWarmPerMinute",
    "Subsonic:CoverCacheMb",
    "Subsonic:SearchDurationWaitMs",
    "Subsonic:SearchLastFmWaitMs",
    "Subsonic:SizeFlagClients",
    "GeneratedPlaylists:Years",
    "GeneratedPlaylists:MaxYearPlaylists",
    "GeneratedPlaylists:Artists",
    "GeneratedPlaylists:ArtistCount",
    "LibraryActions:ReviewSweepAutoReplace",
    "LibraryActions:ReviewSweepAutoReplacePerDay",
    "LibraryActions:KeepRatedCopies",
    "Subsonic:DownloadAlbumOnSongStar",
    "Metadata:AnimatedArt",
    "Subsonic:CoverBadge",
    "LibraryActions:ReviewSweepAutoReplacePreview",
];

/// Whether a config-sources key names a secret: a password, an API key, a secret, a token
/// (ntfy tokens are credentials outright) or a webhook URL (a Discord webhook URL embeds its
/// token, so the whole URL is the secret).
fn is_secret_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    ["password", "apikey", "secret", "token", "webhookurl"]
        .iter()
        .any(|suffix| lower.ends_with(suffix))
}

/// What a set secret reads as on the config-sources page: always the same eight bullets, so the
/// page never tells how long a password or key is.
pub const CONFIG_SOURCES_MASK: &str = "\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}";

/// `GET /api/admin/config-sources`: every config key the app effectively sees, read by its
/// `Section:Key` path (a list reads as `""`, having no value of its own), with anything that
/// smells like a secret masked so a screenshot of the page does not leak it.
pub async fn get_config_sources(State(state): State<AppState>) -> Response {
    let rows: Vec<serde_json::Value> = CONFIG_SOURCE_KEYS
        .iter()
        .map(|key| {
            let value = state.settings.raw(key).unwrap_or_default();
            let secret = is_secret_key(key);
            let display = if secret && !value.is_empty() {
                CONFIG_SOURCES_MASK.to_string()
            } else {
                value
            };
            json!({ "Key": key, "Value": display, "IsSecret": secret })
        })
        .collect();
    json_ok(&json!({
        "keys": rows,
        "configFile": state.settings_writer.file_path().display().to_string(),
    }))
}
