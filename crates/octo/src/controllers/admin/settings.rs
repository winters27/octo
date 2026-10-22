//! The settings actions: `GET /api/admin/settings` (the effective configuration the forms
//! pre-fill from) and `POST /api/admin/settings` (a partial patch merged into settings.json),
//! with the validation and the secret placeholder rules the save path applies. The effective
//! document is shared with `GET raw-config`.

use std::fmt::Debug;

use axum::Extension;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use octo_core::common::dotnet::{is_blank, ordinal_ignore_case_key, utf16_len};
use octo_core::json::dom::Node;
use octo_core::settings::{
    AnimatedArtMode, FamilySettings, GenreMatchMode, JsonObject, LastFmUserSession, LibraryAction,
    LibraryActionSettings, ListeningSettings, LyricsSaveTo, PrefetchScope, PrefetchSettings,
    SettingsWriteError, SpotiflacSettings,
};
use std::collections::HashSet;
use tracing::{error, info, warn};

use super::json_body::{body_text, parse_json_node};
use super::plumbing::{
    SECRET_PLACEHOLDER, child, child_mut, error_json, json_node, key_of, mask_secret, n_num, n_obj, n_str,
    n_str_array,
};
use super::secrets::{
    keep_saved_secrets, mask_tokens, normalize_saved_addresses, redact_secrets, secret_sent_elsewhere,
};
use crate::app::AppState;
use crate::http::error::{AppError, AppResult};
use crate::services::admin::AdminCaller;

/// An enum as `.ToString()` wrote it: the member's name.
fn name<T: Debug>(value: T) -> Node {
    n_str(&format!("{value:?}"))
}

/// The genre mappings as a [`Node`] tree, `Match` as its number.
fn genre_mappings_node(mappings: &[octo_core::settings::GenreMappingSettings]) -> Node {
    Node::Array(
        mappings
            .iter()
            .map(|m| {
                n_obj([
                    ("Id", n_str(&m.id)),
                    ("Pattern", n_str(&m.pattern)),
                    ("Genre", n_str(&m.genre)),
                    (
                        "Match",
                        n_num(match m.match_mode {
                            GenreMatchMode::Contains => 0,
                            GenreMatchMode::Exact => 1,
                        }),
                    ),
                    ("Enabled", Node::Bool(m.enabled)),
                ])
            })
            .collect(),
    )
}

/// `JsonSerializer.SerializeToNode(lastfm.DiscoveryStations)`.
fn discovery_stations_node(stations: &[octo_core::settings::DiscoveryStationSettings]) -> Node {
    Node::Array(
        stations
            .iter()
            .map(|s| {
                n_obj([
                    ("Id", n_str(&s.id)),
                    ("Name", n_str(&s.name)),
                    ("Enabled", Node::Bool(s.enabled)),
                    ("Tags", n_str_array(&s.tags)),
                ])
            })
            .collect(),
    )
}

/// `MaskSessions`: each listener's Last.fm link with the key masked, enough for the Raw editor
/// to round-trip it and for nobody to read it back. Entries with a blank key are left out.
pub fn mask_sessions<'a>(sessions: impl IntoIterator<Item = (&'a String, &'a LastFmUserSession)>) -> Node {
    Node::Object(
        sessions
            .into_iter()
            .filter(|(_, session)| !is_blank(&session.session_key))
            .map(|(user, session)| {
                (
                    user.clone(),
                    n_obj([
                        ("SessionKey", n_str(SECRET_PLACEHOLDER)),
                        ("LastFmUser", n_str(&session.last_fm_user)),
                    ]),
                )
            })
            .collect(),
    )
}

/// The *effective* configuration the app sees right now, as GET settings and GET raw-config
/// both build it: every section, PascalCase keys (a `Dictionary`/`JsonObject`, so no camelCase),
/// enums by name, and every credential (`secrets::SECRET_FIELDS` and `SECRET_MAPS`) as the
/// placeholder.
/// `raw_config` selects raw-config's one difference: `DiscoveryStations` is always the class
/// serialized, never the file's.
pub fn effective_document(state: &AppState, raw_config: bool) -> JsonObject {
    let current = state.settings.current();
    let subsonic = &current.subsonic;
    let soulseek = &current.soulseek;
    let lidarr = &current.lidarr;
    let lastfm = &current.last_fm;
    let genre = &current.genre;
    let actions = &current.library_actions;
    let mixes = &current.generated_playlists;
    let notif = &current.notifications;
    let metadata = &current.metadata;
    let listen_brainz = &current.listen_brainz;
    let file = state.settings_writer.load();
    let opt = |v: &Option<String>| n_str(v.as_deref().unwrap_or(""));

    let mut doc = JsonObject::new();
    doc.insert(
        "Subsonic".into(),
        n_obj([
            ("Url", opt(&subsonic.url)),
            // Both are fields on the Music server form. Missing here, they never pre-filled, so
            // every save of that form wrote empty strings over them and quietly took away the
            // admin identity library actions and authenticated rescans depend on. The password
            // goes out as a placeholder the save swaps back, so it never reaches a browser.
            ("AdminUsername", opt(&subsonic.admin_username)),
            (
                "AdminPassword",
                n_str(&mask_secret(subsonic.admin_password.as_deref())),
            ),
            (
                "EnableSearchDiscovery",
                Node::Bool(subsonic.enable_search_discovery),
            ),
            (
                "WaitForSearchDurations",
                Node::Bool(subsonic.wait_for_search_durations),
            ),
            ("SearchDurationWaitMs", n_num(subsonic.search_duration_wait_ms)),
            ("SearchLastFmWaitMs", n_num(subsonic.search_last_fm_wait_ms)),
            ("EnableSyncCatalog", Node::Bool(subsonic.enable_sync_catalog)),
            ("SyncCatalogClients", n_str(&subsonic.sync_catalog_clients)),
            ("SyncCatalogMaxSongs", n_num(subsonic.sync_catalog_max_songs)),
            (
                "StreamSizeWarmPerMinute",
                n_num(subsonic.stream_size_warm_per_minute),
            ),
            ("CoverCacheMb", n_num(subsonic.cover_cache_mb)),
            ("CoverBadge", Node::Bool(subsonic.cover_badge)),
            ("SizeFlagClients", n_str(&subsonic.size_flag_clients)),
            ("StorageMode", name(subsonic.storage_mode)),
            ("DownloadMode", name(subsonic.download_mode)),
            ("DownloadOnStar", Node::Bool(subsonic.download_on_star)),
            ("DownloadAlbumOnStar", Node::Bool(subsonic.download_album_on_star)),
            ("RecordRequestedBy", Node::Bool(subsonic.record_requested_by)),
            (
                "StarDownloadsForRequester",
                Node::Bool(subsonic.star_downloads_for_requester),
            ),
            ("SkipOwnedSongs", Node::Bool(subsonic.skip_owned_songs)),
            (
                "WaitForLosslessOnPlay",
                Node::Bool(subsonic.wait_for_lossless_on_play),
            ),
            (
                "LosslessWaitTimeoutSeconds",
                n_num(subsonic.lossless_wait_timeout_seconds),
            ),
            ("DownloadOnPlay", Node::Bool(subsonic.download_on_play)),
            ("LidarrAlbumOnPlay", Node::Bool(subsonic.lidarr_album_on_play)),
            (
                "DownloadAlbumOnSongStar",
                Node::Bool(subsonic.download_album_on_song_star),
            ),
            // These two are rendered by the dashboard but were missing here, so their fields
            // never pre-filled with the saved value.
            ("DownloadSource", name(subsonic.download_source)),
            (
                "HeartDownloadSources",
                Node::Array(
                    subsonic
                        .effective_heart_download_sources()
                        .iter()
                        .map(|step| {
                            n_obj([
                                ("Source", name(step.source)),
                                ("SongEnabled", Node::Bool(step.song_enabled == Some(true))),
                                ("AlbumEnabled", Node::Bool(step.album_enabled == Some(true))),
                            ])
                        })
                        .collect(),
                ),
            ),
            (
                "AutoDetectDownloadPath",
                Node::Bool(subsonic.auto_detect_download_path),
            ),
            ("LibraryPath", n_str(&subsonic.library_path)),
            ("FolderStructure", name(subsonic.folder_structure)),
            ("UseLocalStaging", Node::Bool(subsonic.use_local_staging)),
            ("ExplicitFilter", name(subsonic.explicit_filter)),
            ("CacheDurationHours", n_num(subsonic.cache_duration_hours)),
            (
                "EnableExternalPlaylists",
                Node::Bool(subsonic.enable_external_playlists),
            ),
            ("PlaylistsDirectory", n_str(&subsonic.playlists_directory)),
        ]),
    );
    doc.insert(
        "Library".into(),
        n_obj([(
            "DownloadPath",
            n_str(
                &state
                    .settings
                    .raw("Library:DownloadPath")
                    .unwrap_or_else(|| "/music".to_string()),
            ),
        )]),
    );
    // Must be listed even though nothing reads it back: PUT raw-config writes this document
    // wholesale, so a section missing from the GET is a section the next plain Save deletes.
    doc.insert(
        "Server".into(),
        n_obj([("PublicUrl", n_str(&current.server.public_url))]),
    );
    // The Updates section as configured now; read when asked, so a save shows at once.
    doc.insert(
        "Updates".into(),
        n_obj([
            ("Check", Node::Bool(current.updates.check)),
            ("Repo", n_str(&current.updates.repo)),
        ]),
    );
    doc.insert(
        "Soulseek".into(),
        n_obj([
            ("Enabled", Node::Bool(soulseek.enabled)),
            ("BaseUrl", opt(&soulseek.base_url)),
            ("Username", opt(&soulseek.username)),
            // slskd's web login opens slskd entirely, so it goes out as the placeholder too.
            ("Password", n_str(&mask_secret(soulseek.password.as_deref()))),
            ("WebUrl", n_str(&soulseek.web_url)),
            ("CheckListenPort", Node::Bool(soulseek.check_listen_port)),
            ("ShareLibrary", Node::Bool(soulseek.share_library)),
            ("SharedFolders", n_str(&soulseek.shared_folders)),
            ("SearchWaitSeconds", n_num(soulseek.search_wait_seconds)),
            (
                "UpgradeSearchWaitSeconds",
                n_num(soulseek.upgrade_search_wait_seconds),
            ),
            ("MinFileSizeBytes", n_num(soulseek.min_file_size_bytes)),
            ("PreferredExtension", n_str(&soulseek.preferred_extension)),
            ("DownloadTimeoutSeconds", n_num(soulseek.download_timeout_seconds)),
            ("VerifyDownloads", Node::Bool(soulseek.verify_downloads)),
            (
                "SkipCoversAndInstrumentals",
                Node::Bool(soulseek.skip_covers_and_instrumentals),
            ),
            (
                "AcoustIdApiKey",
                n_str(&mask_secret(Some(&soulseek.acoust_id_api_key))),
            ),
            ("MinMatchScore", n_num(soulseek.min_match_score)),
            ("TagFromMusicBrainz", Node::Bool(soulseek.tag_from_music_brainz)),
            ("NameFromMatch", Node::Bool(soulseek.name_from_match)),
            ("RejectedPeerTtlDays", n_num(soulseek.rejected_peer_ttl_days)),
            ("FingerprintSeconds", n_num(soulseek.fingerprint_seconds)),
            (
                "FingerprintTimeoutSeconds",
                n_num(soulseek.fingerprint_timeout_seconds),
            ),
            (
                "AcoustIdTimeoutSeconds",
                n_num(soulseek.acoust_id_timeout_seconds),
            ),
            ("DetectTranscodes", Node::Bool(soulseek.detect_transcodes)),
            (
                "TranscodeCheckTimeoutSeconds",
                n_num(soulseek.transcode_check_timeout_seconds),
            ),
            ("OutageHoldHours", n_num(soulseek.outage_hold_hours)),
            ("ParallelDownloads", n_num(soulseek.parallel_downloads)),
            ("AlbumFolders", Node::Bool(soulseek.album_folders)),
            (
                "SubmitConfirmedFingerprints",
                Node::Bool(soulseek.submit_confirmed_fingerprints),
            ),
            (
                "AcoustIdUserApiKey",
                n_str(&mask_secret(Some(&soulseek.acoust_id_user_api_key))),
            ),
        ]),
    );
    doc.insert(
        "Lidarr".into(),
        n_obj([
            ("BaseUrl", opt(&lidarr.base_url)),
            ("ApiKey", n_str(&mask_secret(lidarr.api_key.as_deref()))),
            ("RootFolderPath", opt(&lidarr.root_folder_path)),
            ("QualityProfileId", n_num(lidarr.quality_profile_id)),
            ("MetadataProfileId", n_num(lidarr.metadata_profile_id)),
            ("CompletionMode", name(lidarr.completion_mode)),
            ("ImportTimeoutSeconds", n_num(lidarr.import_timeout_seconds)),
            (
                "WebhookSecret",
                n_str(&mask_secret(lidarr.webhook_secret.as_deref())),
            ),
        ]),
    );
    doc.insert(
        "YouTube".into(),
        n_obj([(
            "ShimUrl",
            n_str(&state.settings.raw("YouTube:ShimUrl").unwrap_or_default()),
        )]),
    );
    // Radio's other sources and their weights (multi-source radio).
    let radio = &current.radio_sources;
    doc.insert(
        "RadioSources".into(),
        n_obj([
            ("YouTubeMusic", Node::Bool(radio.you_tube_music)),
            ("ListenBrainz", Node::Bool(radio.listen_brainz)),
            ("SoundsAlike", Node::Bool(radio.sounds_alike)),
            ("SonicUrl", n_str(&radio.sonic_url)),
            ("SonicPauseSeconds", n_num(radio.sonic_pause_seconds)),
            ("ListenBrainzAlgorithm", n_str(&radio.listen_brainz_algorithm)),
            ("ListenBrainzRadioMode", n_str(&radio.listen_brainz_radio_mode)),
            ("LastFmWeight", n_num(radio.last_fm_weight)),
            ("YouTubeMusicWeight", n_num(radio.you_tube_music_weight)),
            ("ListenBrainzWeight", n_num(radio.listen_brainz_weight)),
            ("SoundsAlikeWeight", n_num(radio.sounds_alike_weight)),
            ("LearnFromListening", Node::Bool(radio.learn_from_listening)),
        ]),
    );
    let discovery_stations = if raw_config {
        discovery_stations_node(&lastfm.discovery_stations)
    } else {
        file.get("LastFm")
            .and_then(|section| section.get("DiscoveryStations"))
            .filter(|node| !matches!(node, Node::Null))
            .cloned()
            .unwrap_or_else(|| discovery_stations_node(&lastfm.discovery_stations))
    };
    doc.insert(
        "LastFm".into(),
        n_obj([
            ("ApiKey", n_str(&mask_secret(Some(&lastfm.api_key)))),
            // Placeholders, which the saves swap back for what is stored.
            ("ApiSecret", n_str(&mask_secret(Some(&lastfm.api_secret)))),
            (
                "ScrobbleExternalPlays",
                Node::Bool(lastfm.scrobble_external_plays),
            ),
            ("ScrobbleLibraryPlays", Node::Bool(lastfm.scrobble_library_plays)),
            ("UserSessions", mask_sessions(&lastfm.user_sessions)),
            ("EnableRadio", Node::Bool(lastfm.enable_radio)),
            ("RadioTrackCount", n_num(lastfm.radio_track_count)),
            (
                "RadioCacheDurationHours",
                n_num(lastfm.radio_cache_duration_hours),
            ),
            (
                "EnablePersonalizedStations",
                Node::Bool(lastfm.enable_personalized_stations),
            ),
            ("EnableYourMix", Node::Bool(lastfm.enable_your_mix)),
            ("EnableDiscoveryMix", Node::Bool(lastfm.enable_discovery_mix)),
            ("ArtistStationCount", n_num(lastfm.artist_station_count)),
            ("GenreStationCount", n_num(lastfm.genre_station_count)),
            (
                "EnableDiscoveryStations",
                Node::Bool(lastfm.enable_discovery_stations),
            ),
            (
                "ExposeRadioAsPlaylists",
                Node::Bool(lastfm.expose_radio_as_playlists),
            ),
            ("ExposeRadioAsStreams", Node::Bool(lastfm.expose_radio_as_streams)),
            ("RadioStreamBitrateKbps", n_num(lastfm.radio_stream_bitrate_kbps)),
            ("EnableIcyMetadata", Node::Bool(lastfm.enable_icy_metadata)),
            (
                "StarterPublishTimeoutSeconds",
                n_num(lastfm.starter_publish_timeout_seconds),
            ),
            (
                "RadioLoudnessTargetLufs",
                n_num(lastfm.radio_loudness_target_lufs),
            ),
            ("HistoryRetentionDays", n_num(lastfm.history_retention_days)),
            ("DiscoveryPercent", n_num(lastfm.discovery_percent)),
            ("RefreshIntervalHours", n_num(lastfm.refresh_interval_hours)),
            ("MinimumPlays", n_num(lastfm.minimum_plays)),
            ("DiscoveryStations", discovery_stations),
        ]),
    );
    doc.insert(
        "LibraryActions".into(),
        n_obj([
            ("Enabled", Node::Bool(actions.enabled)),
            ("PlaylistsEnabled", Node::Bool(actions.playlists_enabled)),
            ("RatingsEnabled", Node::Bool(actions.ratings_enabled)),
            ("PlaylistPrefix", n_str(&actions.playlist_prefix)),
            ("DryRun", Node::Bool(actions.dry_run)),
            ("QuarantineDirectory", n_str(&actions.quarantine_directory)),
            (
                "QuarantineRetentionDays",
                n_num(actions.quarantine_retention_days),
            ),
            ("PollIntervalSeconds", n_num(actions.poll_interval_seconds)),
            ("MaxActionsPerCycle", n_num(actions.max_actions_per_cycle)),
            (
                "KeepReplacedOriginals",
                Node::Bool(actions.keep_replaced_originals),
            ),
            ("NoticePrefix", n_str(&actions.notice_prefix)),
            ("ReviewEnabled", Node::Bool(actions.review_enabled)),
            ("ReviewPlaylistName", n_str(&actions.review_playlist_name)),
            ("ReviewSweepPerHour", n_num(actions.review_sweep_per_hour)),
            (
                "ReviewSweepOctoDownloads",
                Node::Bool(actions.review_sweep_octo_downloads),
            ),
            (
                "ReviewSweepAutoReplace",
                Node::Bool(actions.review_sweep_auto_replace),
            ),
            (
                "ReviewSweepAutoReplacePerDay",
                n_num(actions.review_sweep_auto_replace_per_day),
            ),
            (
                "ReviewSweepAutoReplacePreview",
                Node::Bool(actions.review_sweep_auto_replace_preview),
            ),
            ("TagCheckPerHour", n_num(actions.tag_check_per_hour)),
            ("DuplicatesEnabled", Node::Bool(actions.duplicates_enabled)),
            ("DuplicatesPlaylistName", n_str(&actions.duplicates_playlist_name)),
            ("DuplicatesScanHours", n_num(actions.duplicates_scan_hours)),
            ("UpgradePerWeek", n_num(actions.upgrade_per_week)),
            ("UpgradeSource", name(actions.upgrade_source)),
            ("NoticeMaxTracks", n_num(actions.notice_max_tracks)),
            ("RatingsScope", name(actions.ratings_scope)),
            ("KeepRatedCopies", Node::Bool(actions.keep_rated_copies)),
            ("AllowedUsers", n_str_array(&actions.allowed_users)),
            // Effective rather than raw, because the editor needs every action present even
            // when the config names only some of them. Projected so the enum lands as its NAME:
            // serialized directly it becomes a number, and the editor looks the action up by
            // name to label the row.
            (
                "Actions",
                Node::Array(
                    actions
                        .effective_actions()
                        .iter()
                        .map(|action| {
                            n_obj([
                                ("Action", name(action.action)),
                                ("Name", n_str(&action.name)),
                                ("Enabled", Node::Bool(action.enabled)),
                                ("Rating", action.rating.map_or(Node::Null, n_num)),
                            ])
                        })
                        .collect(),
                ),
            ),
        ]),
    );
    doc.insert(
        "Metadata".into(),
        n_obj([
            ("Language", n_str(&metadata.language)),
            ("AlbumFromTitle", Node::Bool(metadata.album_from_title)),
            ("UseCoverArtArchive", Node::Bool(metadata.use_cover_art_archive)),
            ("ReplaceVideoCovers", Node::Bool(metadata.replace_video_covers)),
            ("WriteCoverFile", Node::Bool(metadata.write_cover_file)),
            ("EmbedFullSizeCovers", Node::Bool(metadata.embed_full_size_covers)),
            ("FetchLyrics", Node::Bool(metadata.fetch_lyrics)),
            ("LyricsSources", n_str(&metadata.lyrics_sources)),
            (
                "PreferWordTimedLyrics",
                Node::Bool(metadata.prefer_word_timed_lyrics),
            ),
            (
                "WriteLyricsBesideAllSongs",
                Node::Bool(metadata.write_lyrics_beside_all_songs),
            ),
            (
                "SaveLyricsTo",
                n_str(LyricsSaveTo::normalize(Some(&metadata.save_lyrics_to))),
            ),
            ("PreferOriginalAlbum", Node::Bool(metadata.prefer_original_album)),
            (
                "YearFromOriginalRelease",
                Node::Bool(metadata.year_from_original_release),
            ),
            ("PreferredCountries", n_str(&metadata.preferred_countries)),
            (
                "ReleaseDetailsLookup",
                Node::Bool(metadata.release_details_lookup),
            ),
            ("ReplayGain", Node::Bool(metadata.replay_gain)),
            (
                "ReplayGainTimeoutSeconds",
                n_num(metadata.replay_gain_timeout_seconds),
            ),
            ("TagRehearsal", Node::Bool(metadata.tag_rehearsal)),
            (
                "AnimatedArt",
                n_str(match metadata.animated_art() {
                    AnimatedArtMode::Direct => AnimatedArtMode::DIRECT,
                    AnimatedArtMode::Relay => AnimatedArtMode::RELAY,
                    AnimatedArtMode::Off => AnimatedArtMode::OFF,
                }),
            ),
        ]),
    );
    doc.insert(
        "GeneratedPlaylists".into(),
        n_obj([
            ("PopularNow", Node::Bool(mixes.popular_now)),
            ("Charts", n_str(&mixes.charts)),
            ("ChartCountry", n_str(&mixes.chart_country)),
            ("Enabled", Node::Bool(mixes.enabled)),
            ("Genres", Node::Bool(mixes.genres)),
            ("Decades", Node::Bool(mixes.decades)),
            ("TrackCount", n_num(mixes.track_count)),
            ("MaxPerArtist", n_num(mixes.max_per_artist)),
            ("CreateAt", n_num(mixes.create_at)),
            ("RemoveBelow", n_num(mixes.remove_below)),
            ("MaxPlaylists", n_num(mixes.max_playlists)),
            ("RefreshHours", n_num(mixes.refresh_hours)),
            ("NewShare", n_num(mixes.new_share)),
            ("NewDays", n_num(mixes.new_days)),
            ("NameFormat", n_str(&mixes.name_format)),
            ("CountryMixes", Node::Bool(mixes.country_mixes)),
            ("Countries", n_str(&mixes.countries)),
            ("MaxCountryPlaylists", n_num(mixes.max_country_playlists)),
            (
                "CountryLookupsPerRefresh",
                n_num(mixes.country_lookups_per_refresh),
            ),
            ("Years", Node::Bool(mixes.years)),
            ("MaxYearPlaylists", n_num(mixes.max_year_playlists)),
            ("Artists", Node::Bool(mixes.artists)),
            ("ArtistCount", n_num(mixes.artist_count)),
            ("NewReleases", Node::Bool(mixes.new_releases)),
            ("Rediscover", Node::Bool(mixes.rediscover)),
            ("DeepCuts", Node::Bool(mixes.deep_cuts)),
            ("NewReleaseWeeks", n_num(mixes.new_release_weeks)),
            ("NewReleaseArtists", n_num(mixes.new_release_artists)),
            ("RediscoverMonths", n_num(mixes.rediscover_months)),
            ("LikedSongs", Node::Bool(mixes.liked_songs)),
            ("LikedSongsName", n_str(&mixes.liked_songs_name)),
            ("LikedSongsMax", n_num(mixes.liked_songs_max)),
        ]),
    );
    // Read from the raw file, for the same reason as DiscoveryStations: EffectiveMappings()
    // drops rows, and the editor has to show the user what they typed rather than what
    // survived.
    let mappings = file
        .get("Genre")
        .and_then(|section| section.get("Mappings"))
        .filter(|node| !matches!(node, Node::Null))
        .cloned()
        .unwrap_or_else(|| genre_mappings_node(&genre.mappings));
    doc.insert(
        "Genre".into(),
        n_obj([
            ("Enabled", Node::Bool(genre.enabled)),
            ("MaxGenres", n_num(genre.max_genres)),
            ("OnEmpty", name(genre.on_empty)),
            ("Fallback", name(genre.fallback)),
            ("UnknownLabel", n_str(&genre.unknown_label)),
            ("Blocklist", n_str_array(&genre.blocklist)),
            ("Mappings", mappings),
            (
                "BackfillMaxConsecutiveFailures",
                n_num(genre.backfill_max_consecutive_failures),
            ),
            ("BackfillExtensions", n_str_array(&genre.backfill_extensions)),
        ]),
    );
    doc.insert(
        "Notifications".into(),
        n_obj([
            ("NtfyUrl", n_str(&notif.ntfy_url)),
            ("NtfyToken", n_str(&mask_secret(Some(&notif.ntfy_token)))),
            // The address carries the webhook's own token, so the whole address is the secret.
            (
                "DiscordWebhookUrl",
                n_str(&mask_secret(Some(&notif.discord_webhook_url))),
            ),
            ("NotifyDownloadStarted", Node::Bool(notif.notify_download_started)),
            (
                "NotifyDownloadCompleted",
                Node::Bool(notif.notify_download_completed),
            ),
            (
                "NotifyLosslessFallback",
                Node::Bool(notif.notify_lossless_fallback),
            ),
            ("NotifyDownloadFailed", Node::Bool(notif.notify_download_failed)),
            ("NotifyAlbumCompleted", Node::Bool(notif.notify_album_completed)),
        ]),
    );
    // Present even when unset: a section missing from this document is a section the next
    // Raw Config save silently deletes.
    doc.insert(
        "ListenBrainz".into(),
        n_obj([
            ("Token", n_str(&mask_secret(Some(&listen_brainz.token)))),
            (
                "SubmitExternalPlays",
                Node::Bool(listen_brainz.submit_external_plays),
            ),
            ("UserTokens", mask_tokens(&listen_brainz.user_tokens)),
        ]),
    );
    let imports = &current.imports;
    doc.insert(
        "Imports".into(),
        n_obj([
            ("SongsPerHour", n_num(imports.songs_per_hour)),
            ("RefreshHours", n_num(imports.refresh_hours)),
        ]),
    );
    // Family is off by default. Its section is listed once it is in use (turned on, changed
    // from its defaults, or saved in settings.json), so with Family off this document has no
    // Family section at all, and a Raw config save still keeps a saved one.
    let family = &current.family;
    if *family != FamilySettings::default() || file.keys().any(|key| key.eq_ignore_ascii_case("Family")) {
        doc.insert(
            "Family".into(),
            n_obj([
                ("Enabled", Node::Bool(family.enabled)),
                ("MembersPath", n_str(&family.members_path)),
                ("HomeNetworks", n_str(&family.home_networks)),
                ("TrustedProxies", n_str(&family.trusted_proxies)),
                ("InviteDays", n_num(family.invite_days)),
                (
                    "OwnerSeesMemberLibraries",
                    Node::Bool(family.owner_sees_member_libraries),
                ),
                (
                    "OwnerSeesMemberPlaylists",
                    Node::Bool(family.owner_sees_member_playlists),
                ),
                ("UnmanagedAccounts", n_str(&family.unmanaged_accounts)),
                ("KidUnratedIsExplicit", Node::Bool(family.kid_unrated_is_explicit)),
                ("FastPullFactor", n_num(family.fast_pull_factor)),
                ("RadioStreamsPerPerson", n_num(family.radio_streams_per_person)),
                ("ManageFromAway", Node::Bool(family.manage_from_away)),
            ]),
        );
        // The family notices are listed with the Family section, for the same reason.
        let notif = &current.notifications;
        if let Some(Node::Object(section)) = doc.get_mut("Notifications") {
            section.insert(
                "NotifyFamilyRequests".into(),
                Node::Bool(notif.notify_family_requests),
            );
            section.insert(
                "NotifyFamilyDevices".into(),
                Node::Bool(notif.notify_family_devices),
            );
        }
    }
    // SpotiFLAC is off by default, and listed like Family: once it is in use or saved.
    let spotiflac = &current.spotiflac;
    if *spotiflac != SpotiflacSettings::default()
        || file.keys().any(|key| key.eq_ignore_ascii_case("Spotiflac"))
    {
        doc.insert("Spotiflac".into(), spotiflac_section(spotiflac));
    }
    // Getting upcoming songs ahead is off by default, and listed the same way.
    let prefetch = &current.prefetch;
    if *prefetch != PrefetchSettings::default() || file.keys().any(|key| key.eq_ignore_ascii_case("Prefetch"))
    {
        doc.insert(
            "Prefetch".into(),
            n_obj([
                ("Enabled", Node::Bool(prefetch.enabled)),
                ("Scope", n_str(prefetch.scope.name())),
                ("Count", n_num(prefetch.count)),
                ("AtOnce", n_num(prefetch.at_once)),
                ("CacheGb", n_num(prefetch.cache_gb)),
                ("KeepDays", n_num(prefetch.keep_days)),
                ("MinFreeGb", n_num(prefetch.min_free_gb)),
                ("PauseWhenStopped", Node::Bool(prefetch.pause_when_stopped)),
                ("Folder", n_str(&prefetch.folder)),
                ("KeepMixesReady", Node::Bool(prefetch.keep_mixes_ready)),
                ("KeepReadyCount", n_num(prefetch.keep_ready_count)),
            ]),
        );
    }
    // Listening is on by default, and listed like SpotiFLAC: once changed or saved.
    let listening = &current.listening;
    if *listening != ListeningSettings::default()
        || file.keys().any(|key| key.eq_ignore_ascii_case("Listening"))
    {
        doc.insert(
            "Listening".into(),
            n_obj([
                ("Enabled", Node::Bool(listening.enabled)),
                ("KeepDays", n_num(listening.effective_keep_days() as i32)),
            ]),
        );
    }
    doc
}

/// The Spotiflac section as the forms read it, the token as the placeholder.
fn spotiflac_section(spotiflac: &SpotiflacSettings) -> Node {
    n_obj([
        ("Enabled", Node::Bool(spotiflac.enabled)),
        ("Url", n_str(&spotiflac.url)),
        ("Token", n_str(&mask_secret(Some(&spotiflac.token)))),
        ("Quality", n_str(spotiflac.effective_quality())),
        ("Services", n_str(&spotiflac.services)),
        ("Folder", n_str(&spotiflac.folder)),
        ("Placement", name(spotiflac.placement)),
        ("TimeoutSeconds", n_num(spotiflac.timeout_seconds)),
        ("AtOnce", n_num(spotiflac.at_once)),
    ])
}

/// `GET /api/admin/settings`: the *effective* configuration the app sees right now, so the UI
/// shows the same values code is using regardless of whether they came from an env var,
/// appsettings.json, or the editable settings file. No credential is returned in clear: each
/// reads as the placeholder.
pub async fn get_settings(State(state): State<AppState>) -> Response {
    let mut doc = effective_document(&state, false);
    let path = state.settings_writer.file_path();
    let pending = state.restart_tracker.pending(&*state.settings);
    doc.insert(
        "_meta".into(),
        n_obj([
            ("ConfigFilePath", n_str(&path.display().to_string())),
            // Drives the "Forget rejected peers" button's label, so an empty list is visibly
            // empty rather than a button that looks like it did nothing.
            ("RejectedPeerCount", n_num(state.rejected_peers.count())),
            ("ConfigFileExists", Node::Bool(path.is_file())),
            // False when the file is there but unparseable, which is when saves are refused.
            ("ConfigFileValid", Node::Bool(state.settings_writer.is_readable())),
            // Restart-only settings whose saved value has not reached the running services.
            ("RestartPending", n_str_array(&pending)),
            // What a saved-but-hidden secret reads as, so the dashboard can recognize it.
            ("SecretPlaceholder", n_str(SECRET_PLACEHOLDER)),
            // So a bug report can name a build.
            ("Version", n_str(octo_core::VERSION)),
        ]),
    );
    json_node(StatusCode::OK, &Node::Object(doc))
}

// ---- Typed reads of a node, which fail on a node of the wrong kind ---------------------------

fn wrong_kind(node: &Node, target: &str) -> AppError {
    let kind = match node {
        Node::Null => "Null",
        Node::Bool(true) => "True",
        Node::Bool(false) => "False",
        Node::Number(_) => "Number",
        Node::String(_) => "String",
        Node::Array(_) => "Array",
        Node::Object(_) => "Object",
    };
    AppError::InvalidOperation(format!(
        "An element of type '{kind}' cannot be converted to a '{target}'."
    ))
}

/// `node?.GetValue<bool>()`.
fn get_bool(node: Option<&Node>) -> Result<Option<bool>, AppError> {
    match node {
        None | Some(Node::Null) => Ok(None),
        Some(Node::Bool(b)) => Ok(Some(*b)),
        Some(other) => Err(wrong_kind(other, "System.Boolean")),
    }
}

/// `node?.GetValue<string>()`.
fn get_string(node: Option<&Node>) -> Result<Option<&str>, AppError> {
    match node {
        None | Some(Node::Null) => Ok(None),
        Some(Node::String(s)) => Ok(Some(s)),
        Some(other) => Err(wrong_kind(other, "System.String")),
    }
}

/// `node?.GetValue<int>()`.
fn get_int(node: Option<&Node>) -> Result<Option<i32>, AppError> {
    match node {
        None | Some(Node::Null) => Ok(None),
        Some(Node::Number(text)) => text
            .parse::<i32>()
            .map(Some)
            .map_err(|_| wrong_kind(&Node::Number(text.clone()), "System.Int32")),
        Some(other) => Err(wrong_kind(other, "System.Int32")),
    }
}

/// `JsonValue.TryGetValue<string>`: the text of a string value, None for anything else.
pub fn string_value(node: Option<&Node>) -> Option<&str> {
    match node {
        Some(Node::String(s)) => Some(s),
        _ => None,
    }
}

/// The one rule with teeth: the dashboard must not be able to produce a live but unrestricted
/// configuration. An empty allowlist means nobody, so enabling the feature without naming
/// anyone is a mistake rather than a permissive choice.
pub fn validate_library_actions(
    actions: &JsonObject,
    current: &LibraryActionSettings,
) -> Result<Option<String>, AppError> {
    let enabled = get_bool(actions.get("Enabled"))?.unwrap_or(current.enabled);
    let allowed = match actions.get("AllowedUsers") {
        Some(Node::Array(items)) => Some(items),
        _ => None,
    };
    let allowed_count = match allowed {
        Some(items) => {
            let mut count = 0;
            for entry in items {
                if !get_string(Some(entry))?.is_none_or(is_blank) {
                    count += 1;
                }
            }
            count
        }
        None => current.allowed_users.len(),
    };

    if enabled && allowed_count == 0 {
        return Ok(Some(
            "Library actions need at least one allowed user. An empty list means nobody.".into(),
        ));
    }
    if allowed.is_some_and(|items| items.len() > 50) {
        return Ok(Some(
            "LibraryActions.AllowedUsers supports at most 50 entries".into(),
        ));
    }

    if let Some(Node::Array(definitions)) = actions.get("Actions") {
        if definitions.len() > LibraryAction::ALL.len() {
            return Ok(Some(format!(
                "There are only {} library actions",
                LibraryAction::ALL.len()
            )));
        }
        let mut ratings = HashSet::new();
        for node in definitions {
            let Node::Object(definition) = node else {
                return Ok(Some("Every library action must be an object".into()));
            };
            let name = get_string(definition.get("Name"))?.map(str::trim).unwrap_or("");
            if utf16_len(name) > 80 {
                return Ok(Some(format!("'{name}' is longer than 80 characters")));
            }
            let rating = get_int(definition.get("Rating"))?.unwrap_or(0);
            if !(0..=5).contains(&rating) {
                return Ok(Some("A star rating must be between 0 and 5".into()));
            }
            if rating > 0 && !ratings.insert(rating) {
                return Ok(Some(format!(
                    "Two actions both use {rating} star(s); a rating can only mean one thing"
                )));
            }
        }
    }
    Ok(None)
}

/// A Wrong lyrics or Wrong cover definition exactly as the defaults have it, which the editor
/// sends back with every save. Such a definition is left out of the file: a build that does not
/// know these actions refuses a settings file naming them, so they are written only once someone
/// changes them.
fn untouched_answer_action(definition: &JsonObject) -> bool {
    let field = |name: &str| key_of(definition, name).and_then(|key| definition.get(&key));
    let action = match field("Action") {
        Some(Node::String(name)) => LibraryAction::ALL
            .into_iter()
            .find(|action| format!("{action:?}").eq_ignore_ascii_case(name.trim())),
        Some(Node::Number(number)) => number.parse().ok().and_then(LibraryAction::from_index),
        _ => None,
    };
    let Some(action) = action.filter(|action| action.written_once_changed()) else {
        return false;
    };
    let name = string_value(field("Name")).map(str::trim).unwrap_or("");
    let enabled = matches!(field("Enabled"), Some(Node::Bool(true)));
    let rating = match field("Rating") {
        None | Some(Node::Null) => Some(action.default_rating()),
        Some(Node::Number(number)) => number.parse::<i32>().ok(),
        Some(_) => None,
    };
    (name.is_empty() || name == action.default_name()) && !enabled && rating == Some(action.default_rating())
}

/// Takes the untouched Wrong lyrics and Wrong cover definitions out of a settings patch.
fn drop_untouched_answer_actions(patch: &mut JsonObject) {
    let Some(actions) = child_mut(patch, "LibraryActions") else {
        return;
    };
    let Some(key) = key_of(actions, "Actions") else {
        return;
    };
    if let Some(Node::Array(definitions)) = actions.get_mut(&key) {
        definitions.retain(|node| !node.as_object().is_some_and(untouched_answer_action));
    }
}

/// `Enum.TryParse<GenreMatchMode>(value, ignoreCase: true, out _)`: a name in any case, a
/// number (defined or not), or a comma list of either.
fn parses_as_match_mode(value: &str) -> bool {
    let value = value.trim();
    if value.is_empty() {
        return false;
    }
    if value.parse::<i64>().is_ok() {
        return true;
    }
    value.split(',').all(|part| {
        let part = part.trim();
        part.eq_ignore_ascii_case("Contains") || part.eq_ignore_ascii_case("Exact")
    })
}

/// The POST path is untyped, so nothing checks a mapping table's shape unless this does.
/// Mirrors `validate_discovery_stations`, and is the second of three layers: the editor
/// validates in the browser and `effective_mappings()` sanitizes again at read time.
pub fn validate_genre_mappings(mappings: &[Node]) -> Result<Option<String>, AppError> {
    if mappings.len() > 200 {
        return Ok(Some("Genre.Mappings supports at most 200 rules".into()));
    }
    let mut patterns = HashSet::new();
    for node in mappings {
        let Node::Object(rule) = node else {
            return Ok(Some("Every genre rule must be an object".into()));
        };
        let pattern = get_string(rule.get("Pattern"))?.map(str::trim).unwrap_or("");
        let genre = get_string(rule.get("Genre"))?.map(str::trim).unwrap_or("");
        let match_mode = get_string(rule.get("Match"))?
            .map(str::trim)
            .unwrap_or("Contains");

        let length = utf16_len(pattern);
        if length == 0 || length > 60 {
            return Ok(Some(
                "Every genre rule needs a pattern of at most 60 characters".into(),
            ));
        }
        if !patterns.insert(ordinal_ignore_case_key(pattern)) {
            return Ok(Some(format!(
                "Duplicate genre rule pattern '{pattern}': only the first could ever fire"
            )));
        }
        if utf16_len(genre) > 60 {
            return Ok(Some(format!(
                "The genre for '{pattern}' must be at most 60 characters"
            )));
        }
        if !parses_as_match_mode(match_mode) {
            return Ok(Some(format!(
                "The match mode for '{pattern}' must be Contains or Exact"
            )));
        }
    }
    Ok(None)
}

/// The chart list and the chart country, when the patch sets them. Keys are matched without
/// regard to case, as the configuration system matches them.
pub fn validate_charts(lists: &JsonObject) -> Option<String> {
    use octo_core::charts::chart_settings::{is_valid_charts, is_valid_country};
    let value = |name: &str| key_of(lists, name).and_then(|key| lists.get(&key));
    match value("Charts") {
        None | Some(Node::Null) => {}
        Some(Node::String(charts)) if is_valid_charts(charts) => {}
        Some(_) => {
            return Some("Charts are new, trending and Apple genre numbers, separated by commas.".into());
        }
    }
    match value("ChartCountry") {
        None | Some(Node::Null) => {}
        Some(Node::String(country)) if is_valid_country(country) => {}
        Some(_) => return Some("Chart country is auto or a two-letter country such as us.".into()),
    }
    None
}

pub fn validate_discovery_stations(stations: &[Node]) -> Result<Option<String>, AppError> {
    if stations.len() > 12 {
        return Ok(Some(
            "LastFm.DiscoveryStations supports at most 12 entries".into(),
        ));
    }
    let mut ids = HashSet::new();
    let mut names = HashSet::new();
    for node in stations {
        let Node::Object(station) = node else {
            return Ok(Some("Every discovery station must be an object".into()));
        };
        let id = get_string(station.get("Id"))?.map(str::trim).unwrap_or("");
        let name = get_string(station.get("Name"))?.map(str::trim).unwrap_or("");
        let tags = match station.get("Tags") {
            Some(Node::Array(tags)) => Some(tags),
            _ => None,
        };
        if id.is_empty() || !ids.insert(ordinal_ignore_case_key(id)) {
            return Ok(Some("Discovery station IDs must be present and unique".into()));
        }
        let name_length = utf16_len(name);
        if name_length == 0 || name_length > 100 || !names.insert(ordinal_ignore_case_key(name)) {
            return Ok(Some(
                "Discovery station names must be present, unique, and at most 100 characters".into(),
            ));
        }
        let bad_tags = match tags {
            None => true,
            Some(tags) if tags.is_empty() || tags.len() > 5 => true,
            Some(tags) => {
                let mut any_blank = false;
                for tag in tags {
                    if get_string(Some(tag))?.is_none_or(is_blank) {
                        any_blank = true;
                        break;
                    }
                }
                any_blank
            }
        };
        if bad_tags {
            return Ok(Some(format!(
                "{name} must contain between one and five non-empty tags"
            )));
        }
    }
    Ok(None)
}

/// The Prefetch section a save sends: a scope Octo knows, whole numbers in range for the limits
/// (each is also clamped at read time), and a folder of its own: never the config folder, the
/// music folder or any folder inside them, where clearing it would delete what is theirs.
/// `taken` are those folders; `default` is the folder a blank one means, which is always
/// allowed. The message to answer with, or None.
pub fn validate_prefetch(
    patch: &JsonObject,
    taken: &[std::path::PathBuf],
    default: Option<&std::path::Path>,
) -> Result<Option<String>, AppError> {
    if let Some(key) = key_of(patch, "Scope")
        && let Some(scope) = get_string(patch.get(&key))?
        && !PrefetchScope::ALL
            .iter()
            .any(|known| known.name().eq_ignore_ascii_case(scope.trim()))
    {
        return Ok(Some(format!("Scope must be RadioOnly or All, not \"{scope}\".")));
    }
    for (name, low, high) in [
        ("Count", 1, 10),
        ("AtOnce", 1, 3),
        ("CacheGb", 1, 2000),
        ("KeepDays", 1, 365),
        ("MinFreeGb", 1, 500),
        ("KeepReadyCount", 1, 20),
    ] {
        if let Some(key) = key_of(patch, name)
            && let Some(value) = get_int(patch.get(&key))?
            && !(low..=high).contains(&value)
        {
            return Ok(Some(format!("{name} must be from {low} to {high}.")));
        }
    }
    if let Some(key) = key_of(patch, "Folder")
        && let Some(folder) = get_string(patch.get(&key))?
        && !is_blank(folder)
    {
        let folder = std::path::Path::new(folder.trim());
        if !folder.is_absolute() {
            return Ok(Some(
                "Folder must be a whole path, such as /data/prefetch.".to_string(),
            ));
        }
        let folder = lexical(folder);
        if default.is_some_and(|default| lexical(default) == folder) {
            return Ok(None);
        }
        if let Some(root) = taken
            .iter()
            .map(|root| lexical(root))
            .find(|root| folder.starts_with(root))
        {
            return Ok(Some(format!(
                "Folder must be a folder of its own, outside {}: songs downloaded ahead are deleted from it.",
                root.display()
            )));
        }
    }
    Ok(None)
}

/// A path with its `.` and `..` parts worked out, without reading the disk.
fn lexical(path: &std::path::Path) -> std::path::PathBuf {
    let mut out = std::path::PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The folders a Prefetch folder may not be or sit in: the config folder, and the music folder
/// Octo downloads into (as configured, and as Navidrome's detected one).
fn taken_folders(state: &AppState) -> Vec<std::path::PathBuf> {
    let mut taken = Vec::new();
    if let Some(config) = state.settings_writer.file_path().parent() {
        taken.push(config.to_path_buf());
    }
    let configured = state.settings.raw("Library:DownloadPath").unwrap_or_default();
    for music in [
        configured.clone(),
        state.navidrome_identity.effective_download_path(&configured),
    ] {
        if !is_blank(&music) {
            taken.push(std::path::PathBuf::from(music));
        }
    }
    taken
}

/// `POST /api/admin/settings`: writes a partial settings patch to the JSON file. The body
/// shape matches the GET; any subset of keys may be supplied. The live settings are read again
/// before the answer, so the very next request sees the save, but services that captured a
/// value at startup keep theirs until a restart.
pub async fn save_settings(
    State(state): State<AppState>,
    caller: Option<Extension<AdminCaller>>,
    body: Bytes,
) -> AppResult {
    let body = body_text(&body);
    if is_blank(&body) {
        return Ok(error_json(StatusCode::BAD_REQUEST, "empty body"));
    }
    let mut patch = match parse_json_node(&body) {
        Ok(Node::Object(map)) => map,
        Ok(_) => {
            return Ok(error_json(
                StatusCode::BAD_REQUEST,
                "invalid JSON: expected object",
            ));
        }
        Err(message) => {
            return Ok(error_json(
                StatusCode::BAD_REQUEST,
                &format!("invalid JSON: {message}"),
            ));
        }
    };

    // Strip any meta-only keys the UI might echo back so they don't end up persisted to disk.
    patch.shift_remove("_meta");

    let current = state.settings.current();
    if let Some(Node::Object(actions_patch)) = patch.get("LibraryActions")
        && let Some(message) = validate_library_actions(actions_patch, &current.library_actions)?
    {
        return Ok(error_json(StatusCode::BAD_REQUEST, &message));
    }
    if let Some(Node::Object(genre_patch)) = patch.get("Genre")
        && let Some(Node::Array(mappings)) = genre_patch.get("Mappings")
        && let Some(message) = validate_genre_mappings(mappings)?
    {
        return Ok(error_json(StatusCode::BAD_REQUEST, &message));
    }
    if let Some(Node::Object(lastfm_patch)) = patch.get("LastFm")
        && let Some(Node::Array(stations)) = lastfm_patch.get("DiscoveryStations")
        && let Some(message) = validate_discovery_stations(stations)?
    {
        return Ok(error_json(StatusCode::BAD_REQUEST, &message));
    }
    if let Some(lists_patch) = child(&patch, "GeneratedPlaylists")
        && let Some(message) = validate_charts(lists_patch)
    {
        return Ok(error_json(StatusCode::BAD_REQUEST, &message));
    }
    if let Some(prefetch_patch) = child(&patch, "Prefetch")
        && let Some(message) = validate_prefetch(
            prefetch_patch,
            &taken_folders(&state),
            state
                .settings_writer
                .file_path()
                .parent()
                .map(|config| config.join("prefetch"))
                .as_deref(),
        )?
    {
        return Ok(error_json(StatusCode::BAD_REQUEST, &message));
    }

    drop_untouched_answer_actions(&mut patch);

    // A saved credential never follows a new address it was not typed again for: with the admin
    // API open to the LAN, that would hand it to whatever server the address names.
    if let Some(elsewhere) = secret_sent_elsewhere(&patch, |section, name| {
        state.settings.raw(&format!("{section}:{name}"))
    }) {
        return Ok(error_json(StatusCode::BAD_REQUEST, &elsewhere));
    }

    // The form echoes the placeholder back when the admin password was left alone; that means
    // "keep what is saved", so it must not be written. Anything typed after the placeholder
    // would otherwise be saved as the password. Keys are matched without regard to case, as
    // the configuration system matches them.
    if let Some(subsonic_patch) = child_mut(&mut patch, "Subsonic")
        && let Some(password_key) = key_of(subsonic_patch, "AdminPassword")
        && let Some(password) = string_value(subsonic_patch.get(&password_key))
    {
        if password.starts_with(SECRET_PLACEHOLDER) && password != SECRET_PLACEHOLDER {
            return Ok(error_json(
                StatusCode::BAD_REQUEST,
                "Retype the whole admin password; it was added to the hidden placeholder.",
            ));
        }
        if password == SECRET_PLACEHOLDER {
            // A different username with the old password kept is a pairing nobody asked for.
            let new_user = key_of(subsonic_patch, "AdminUsername")
                .and_then(|key| string_value(subsonic_patch.get(&key)).map(str::to_string));
            if let Some(new_user) = new_user
                && new_user.trim() != current.subsonic.admin_username.as_deref().unwrap_or("").trim()
            {
                return Ok(error_json(
                    StatusCode::BAD_REQUEST,
                    "Retype the admin password for the new username.",
                ));
            }
            subsonic_patch.shift_remove(&password_key);
        }
    }

    // The slskd sign-in the same way.
    if let Some(soulseek_patch) = child_mut(&mut patch, "Soulseek")
        && let Some(password_key) = key_of(soulseek_patch, "Password")
        && let Some(password) = string_value(soulseek_patch.get(&password_key))
        && password.starts_with(SECRET_PLACEHOLDER)
    {
        if password != SECRET_PLACEHOLDER {
            return Ok(error_json(
                StatusCode::BAD_REQUEST,
                "Retype the whole slskd password; it was added to the hidden placeholder.",
            ));
        }
        let new_user = key_of(soulseek_patch, "Username")
            .and_then(|key| string_value(soulseek_patch.get(&key)).map(str::to_string));
        if let Some(new_user) = new_user
            && new_user.trim() != current.soulseek.username.as_deref().unwrap_or("").trim()
        {
            return Ok(error_json(
                StatusCode::BAD_REQUEST,
                "Retype the slskd password for the new username.",
            ));
        }
        soulseek_patch.shift_remove(&password_key);
    }

    // Open slskd puts this address in a link, so only a web address is kept.
    if let Some(web_patch) = child(&patch, "Soulseek")
        && let Some(web_key) = key_of(web_patch, "WebUrl")
        && let Some(web) = string_value(web_patch.get(&web_key))
        && !is_blank(web)
        && octo_core::settings::SoulseekSettings::safe_web_url(Some(web)).is_none()
    {
        return Ok(error_json(
            StatusCode::BAD_REQUEST,
            "slskd's page must be a whole http:// or https:// address, such as http://192.168.1.5:5030.",
        ));
    }

    // Sessions are made by Connect and removed by Disconnect. No form sends them, and an echoed
    // placeholder must never overwrite a real key.
    if let Some(lastfm_secrets) = child_mut(&mut patch, "LastFm")
        && let Some(sessions_key) = key_of(lastfm_secrets, "UserSessions")
    {
        lastfm_secrets.shift_remove(&sessions_key);
    }
    // Every other credential the same way: the placeholder sent back keeps what is saved.
    if let Some(secret_error) = keep_saved_secrets(&mut patch, Some(&current.listen_brainz.user_tokens)) {
        return Ok(error_json(StatusCode::BAD_REQUEST, &secret_error));
    }

    normalize_saved_addresses(&mut patch);

    // UserTokens is a dictionary: merged, a user removed in the dashboard would stay.
    match state.settings_writer.merge(&patch, &["ListenBrainz.UserTokens"]) {
        Ok(merged) => {
            // The file watcher reads the file again only 250 ms after it settles, and a request
            // that came in meanwhile answered from the settings before the save: a list switched
            // off stayed in getPlaylists, and stayed for as long as that request took. Reading it
            // here means whatever comes after the answer sees the save. A file that does not read
            // back keeps the last good settings, and the reader logs why.
            let _ = state.settings.reload_now();
            let keys: Vec<&str> = patch.keys().map(String::as_str).collect();
            info!(
                "{} saved settings: {}",
                AdminCaller::describe(caller.as_ref().map(|Extension(caller)| caller)),
                keys.join(",")
            );
            let mut answer = JsonObject::new();
            answer.insert("ok".into(), Node::Bool(true));
            answer.insert("persisted".into(), Node::Object(redact_secrets(&merged)));
            Ok(json_node(StatusCode::OK, &Node::Object(answer)))
        }
        Err(SettingsWriteError::Corrupt(e)) => {
            warn!("Refused to save settings: {} is not valid JSON", e.path.display());
            Ok(error_json(
                StatusCode::CONFLICT,
                &format!(
                    "{} Fix it in Raw config, or on disk at {}.",
                    e.message,
                    e.path.display()
                ),
            ))
        }
        Err(SettingsWriteError::Io(e)) => {
            error!(
                "Failed to persist settings to {}: {e}",
                state.settings_writer.file_path().display()
            );
            Ok(error_json(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::secrets::{restore_secret_placeholders, session_key_typed_into_placeholder};
    use super::*;

    fn obj(json: &str) -> JsonObject {
        match Node::parse(json).expect("test JSON parses") {
            Node::Object(map) => map,
            _ => panic!("an object"),
        }
    }

    fn validate_prefetch_alone(patch: &JsonObject) -> Result<Option<String>, AppError> {
        validate_prefetch(patch, &[], None)
    }

    #[test]
    fn validate_prefetch_refuses_a_folder_in_the_config_or_music_folder() {
        let taken = [
            std::path::PathBuf::from("/app/config"),
            std::path::PathBuf::from("/music"),
        ];
        // What a blank folder means, beside settings.json: Octo's own, marked when made.
        let default = Some(std::path::Path::new("/app/config/prefetch"));
        for bad in [
            r#"{"Folder":"/app/config"}"#,
            r#"{"Folder":"/app/config/"}"#,
            r#"{"Folder":"/app/config/other"}"#,
            r#"{"Folder":"/music/.ahead"}"#,
            r#"{"Folder":"/data/../music"}"#,
        ] {
            let message = validate_prefetch(&obj(bad), &taken, default).unwrap();
            assert!(
                message
                    .as_deref()
                    .is_some_and(|m| m.starts_with("Folder must be a folder of its own")),
                "{bad}: {message:?}"
            );
        }
        assert_eq!(
            validate_prefetch(&obj(r#"{"Folder":"prefetch"}"#), &taken, default)
                .unwrap()
                .as_deref(),
            Some("Folder must be a whole path, such as /data/prefetch.")
        );
        for good in [
            r#"{"Folder":""}"#,
            r#"{"Folder":"/data/prefetch"}"#,
            r#"{"Folder":"/musicx"}"#,
            r#"{"Folder":"/app/config/prefetch"}"#,
        ] {
            assert_eq!(
                validate_prefetch(&obj(good), &taken, default).unwrap(),
                None,
                "{good}"
            );
        }
    }

    #[test]
    fn validate_prefetch_takes_known_scopes_and_limits_in_range() {
        assert_eq!(
            validate_prefetch_alone(&obj(r#"{"Scope":"All","Count":10,"AtOnce":1,"CacheGb":2000}"#)).unwrap(),
            None
        );
        assert_eq!(
            validate_prefetch_alone(&obj(r#"{"Scope":"radioonly","KeepReadyCount":20}"#)).unwrap(),
            None
        );
        assert_eq!(
            validate_prefetch_alone(&obj(r#"{"Scope":"Everything"}"#))
                .unwrap()
                .as_deref(),
            Some("Scope must be RadioOnly or All, not \"Everything\".")
        );
        for (bad, message) in [
            (r#"{"Count":0}"#, "Count must be from 1 to 10."),
            (r#"{"AtOnce":4}"#, "AtOnce must be from 1 to 3."),
            (r#"{"KeepDays":366}"#, "KeepDays must be from 1 to 365."),
            (r#"{"MinFreeGb":0}"#, "MinFreeGb must be from 1 to 500."),
        ] {
            assert_eq!(
                validate_prefetch_alone(&obj(bad)).unwrap().as_deref(),
                Some(message),
                "{bad}"
            );
        }
    }

    #[test]
    fn validate_charts_takes_good_lists_and_countries() {
        for ok in [
            r#"{ "Charts": "new,trending,18,14,6", "ChartCountry": "auto" }"#,
            r#"{ "charts": "", "chartcountry": "GB" }"#,
            r#"{ "PopularNow": false }"#,
            r#"{ "Charts": null }"#,
        ] {
            assert_eq!(validate_charts(&obj(ok)), None, "{ok}");
        }
    }

    #[test]
    fn save_settings_refuses_a_bad_chart_list() {
        for bad in [
            r#"{ "Charts": "pop,18" }"#,
            r#"{ "Charts": 18 }"#,
            r#"{ "charts": "new;18" }"#,
        ] {
            assert_eq!(
                validate_charts(&obj(bad)).as_deref(),
                Some("Charts are new, trending and Apple genre numbers, separated by commas."),
                "{bad}"
            );
        }
    }

    #[test]
    fn save_settings_refuses_a_bad_chart_country() {
        for bad in [
            r#"{ "ChartCountry": "usa" }"#,
            r#"{ "ChartCountry": "../" }"#,
            r#"{ "ChartCountry": true }"#,
        ] {
            assert_eq!(
                validate_charts(&obj(bad)).as_deref(),
                Some("Chart country is auto or a two-letter country such as us."),
                "{bad}"
            );
        }
    }

    #[test]
    fn restore_secret_placeholders_keeps_the_stored_password() {
        let mut incoming = obj(&format!(
            r#"{{ "Subsonic": {{ "AdminPassword": "{SECRET_PLACEHOLDER}" }} }}"#
        ));
        restore_secret_placeholders(
            &mut incoming,
            &obj(r#"{ "Subsonic": { "AdminPassword": "stored" } }"#),
        );
        assert_eq!(
            incoming["Subsonic"].get("AdminPassword").and_then(Node::as_str),
            Some("stored")
        );
    }

    /// With nothing stored in the file the password came from the environment, so the key is
    /// dropped and the environment keeps applying.
    #[test]
    fn restore_secret_placeholders_drops_the_key_when_the_file_has_none() {
        let mut incoming = obj(&format!(
            r#"{{ "Subsonic": {{ "AdminPassword": "{SECRET_PLACEHOLDER}" }} }}"#
        ));
        restore_secret_placeholders(&mut incoming, &obj("{}"));
        assert!(incoming["Subsonic"].get("AdminPassword").is_none());
    }

    #[test]
    fn restore_secret_placeholders_leaves_a_real_value_alone() {
        let mut incoming = obj(r#"{ "Subsonic": { "AdminPassword": "typed" } }"#);
        restore_secret_placeholders(
            &mut incoming,
            &obj(r#"{ "Subsonic": { "AdminPassword": "stored" } }"#),
        );
        assert_eq!(
            incoming["Subsonic"].get("AdminPassword").and_then(Node::as_str),
            Some("typed")
        );
    }

    /// Configuration keys ignore case, so a hand-edited or scripted lowercase key must be
    /// handled exactly like the canonical one.
    #[test]
    fn placeholders_are_found_whatever_the_key_case() {
        let mut incoming = obj(&format!(
            r#"{{ "subsonic": {{ "adminPassword": "{SECRET_PLACEHOLDER}" }} }}"#
        ));
        restore_secret_placeholders(
            &mut incoming,
            &obj(r#"{ "Subsonic": { "AdminPassword": "stored" } }"#),
        );
        assert_eq!(
            incoming["subsonic"].get("adminPassword").and_then(Node::as_str),
            Some("stored")
        );

        let echoed = redact_secrets(&obj(r#"{ "subsonic": { "adminpassword": "stored" } }"#));
        assert_eq!(
            echoed["subsonic"].get("adminpassword").and_then(Node::as_str),
            Some(SECRET_PLACEHOLDER)
        );
    }

    #[test]
    fn redact_secrets_masks_the_admin_password_in_the_save_echo() {
        let merged = obj(r#"{ "Subsonic": { "AdminPassword": "stored", "Url": "http://x" } }"#);
        let echoed = redact_secrets(&merged);
        assert_eq!(
            echoed["Subsonic"].get("AdminPassword").and_then(Node::as_str),
            Some(SECRET_PLACEHOLDER)
        );
        assert_eq!(
            merged["Subsonic"].get("AdminPassword").and_then(Node::as_str),
            Some("stored")
        );
    }

    /// Check version made nine actions. A tenth is a config that does not mean anything.
    #[test]
    fn validate_library_actions_accepts_nine_refuses_ten() {
        fn with_actions(count: usize) -> JsonObject {
            let actions = (0..count)
                .map(|i| n_obj([("Name", n_str(&format!("a{i}"))), ("Rating", n_num(0))]))
                .collect();
            let mut map = JsonObject::new();
            map.insert("Actions".into(), Node::Array(actions));
            map
        }
        let current = LibraryActionSettings::default();
        assert_eq!(
            validate_library_actions(&with_actions(9), &current).unwrap(),
            None
        );
        assert_eq!(
            validate_library_actions(&with_actions(10), &current)
                .unwrap()
                .as_deref(),
            Some("There are only 9 library actions")
        );
    }

    #[test]
    fn validators_throw_on_a_value_of_the_wrong_kind_as_get_value_did() {
        let current = LibraryActionSettings::default();
        let wrong = obj(r#"{ "Enabled": "true" }"#);
        assert!(matches!(
            validate_library_actions(&wrong, &current),
            Err(AppError::InvalidOperation(_))
        ));
        assert_eq!(
            validate_genre_mappings(&[n_obj([("Pattern", n_str("rock")), ("Match", n_str("exact"))])])
                .unwrap(),
            None
        );
        assert_eq!(
            validate_genre_mappings(&[n_obj([("Pattern", n_str("rock")), ("Match", n_str("7"))])]).unwrap(),
            None,
            "Enum.TryParse takes an undefined number"
        );
        assert!(
            validate_genre_mappings(&[n_obj([("Pattern", n_str("rock")), ("Match", n_str("Fuzzy"))])])
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn a_session_key_typed_onto_the_placeholder_names_its_listener() {
        let incoming = obj(&format!(
            r#"{{ "LastFm": {{ "UserSessions": {{ "alice": {{ "SessionKey": "{SECRET_PLACEHOLDER}x" }} }} }} }}"#
        ));
        assert_eq!(
            session_key_typed_into_placeholder(&incoming).as_deref(),
            Some("alice")
        );
        assert_eq!(session_key_typed_into_placeholder(&obj("{}")), None);
    }
}
