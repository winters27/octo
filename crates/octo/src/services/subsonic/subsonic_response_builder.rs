//! The app half of the Subsonic response builder.
//!
//! The builder itself, with every shape that needs only the models, is
//! [`octo_subsonic::subsonic_response_builder::SubsonicResponseBuilder`]; its answers are
//! [`SubsonicReply`] values, which are axum responses (`IntoResponse`: 200, the reply's content
//! type, and a `Content-Length`). This module wires it to the app:
//!
//! - [`ExternalIdRegistry`] is its id registry;
//! - [`new_subsonic_response_builder`] builds it from the settings, read once;
//! - [`SubsonicResponseBuilderExt`] adds the answers that take app-service types: an
//!   acquisition snapshot, a generated mix, a library-action outcome and an upgrade job.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use octo_core::common::dotnet::to_lower_invariant;
use octo_core::json::dom::Node;
use octo_core::library::for_you_lists;
use octo_core::library::generated_playlist_service::{ARTIST_PREFIX, list_group};
use octo_core::settings::{GeneratedPlaylistSettings, SubsonicSettings};
use octo_core::soulseek::SoulseekRouting;
use octo_subsonic::subsonic_response_builder::{
    Fields, IdRegistry, LOOKUP_ACTION, REMOVE_ACTION, SUBSONIC_VERSION, SubsonicReply,
    SubsonicResponseBuilder,
};
use serde_json::{Value, json};

use octo_core::models::download::{AcquisitionCandidate, AcquisitionEvent};

use crate::services::common::acquisition_tracker::{AcquisitionSnapshot, key_of};
use crate::services::common::replace_anyway::{self, CopyFacts};
use crate::services::common::song_finder::{FindSnapshot, PickOutcome};
use crate::services::library::{
    GeneratedPlaylist, LibraryActionOutcome, LibraryEditOutcome, SongLookup, TrashedSong, UpgradeJob,
};
use crate::services::soulseek::ExternalIdRegistry;

impl IdRegistry for ExternalIdRegistry {
    fn register(&self, routing: SoulseekRouting) -> String {
        ExternalIdRegistry::register(self, routing)
    }
}

/// The app's builder: over the app's id registry, with `Subsonic:WaitForLosslessOnPlay` read once
/// here (it decides what every row declares, and is restart-required).
pub fn new_subsonic_response_builder(
    id_registry: Arc<ExternalIdRegistry>,
    subsonic: &SubsonicSettings,
) -> SubsonicResponseBuilder {
    SubsonicResponseBuilder::new(id_registry, subsonic)
}

/// The builder's answers that take app-service types.
pub trait SubsonicResponseBuilderExt {
    /// getAcquisitions: the caller's own downloads in flight or recently ended. Always JSON,
    /// whatever format was asked for; the only client that reads it parses JSON, and the
    /// extension it is advertised under says so.
    fn create_acquisitions_response(&self, rows: &[AcquisitionSnapshot]) -> SubsonicReply;

    /// A mix as a playlist row: the same keys a radio station has, read-only (#54).
    fn generated_playlist_fields(
        &self,
        mix: &GeneratedPlaylist,
        settings: &GeneratedPlaylistSettings,
    ) -> Fields;

    /// A mix with its songs. The songs are Navidrome's own, passed through as Navidrome described
    /// them; in XML only their scalar fields, since Subsonic's XML entry is attribute-only.
    fn create_generated_playlist_response(
        &self,
        format: &str,
        mix: &GeneratedPlaylist,
        settings: &GeneratedPlaylistSettings,
        entries: &[Node],
    ) -> SubsonicReply;

    /// libraryAction: what happened to one request. Always ok, so the client reads the state
    /// rather than an error, even when nothing was done. (The default action is
    /// [`REMOVE_ACTION`].)
    fn create_library_action_outcome_response(
        &self,
        song_id: &str,
        outcome: &LibraryActionOutcome,
        action: &str,
    ) -> SubsonicReply;

    /// getUpgrades: the caller's upgrade jobs, a plain array under "upgrades". The id is the
    /// Navidrome id that was asked for, and it stays the same after the swap. progress is 0 to
    /// 1, or null.
    fn create_upgrades_response(&self, jobs: &[(UpgradeJob, Option<f64>)]) -> SubsonicReply;

    /// libraryAction for a version 3 edit: the state, the server's words, and the song's tags
    /// before and after (null for restore), which an app shows and can send back to undo.
    fn create_library_edit_response(
        &self,
        song_id: &str,
        action: &str,
        outcome: &LibraryEditOutcome,
    ) -> SubsonicReply;

    /// libraryAction?action=lookup: the file's tags now ("current") and what a download would
    /// write ("suggested", only the fields it found), with how sure the match is and where from.
    fn create_lookup_response(&self, song_id: &str, lookup: &SongLookup) -> SubsonicReply;

    /// getLibraryTrash: removed songs still in the trash, a plain array under "trash".
    fn create_library_trash_response(&self, songs: &[TrashedSong], keep_days: i32) -> SubsonicReply;

    /// getAcquisition: one row with its whole log.
    fn create_acquisition_response(&self, row: &AcquisitionSnapshot) -> SubsonicReply;

    /// clearAcquisitions: how many rows left the caller's list.
    fn create_cleared_response(&self, cleared: i32) -> SubsonicReply;

    /// findSongs and getFoundSongs: the song, how each source's look went, and every copy found.
    /// A copy's id names it on this look for good, and pickFoundSong takes it as copy; its index
    /// is its place in the list, which older apps pick by once the search has ended.
    fn create_find_songs_response(&self, found: &FindSnapshot) -> SubsonicReply;

    /// pickFoundSong: what picking did.
    fn create_pick_response(&self, outcome: &PickOutcome) -> SubsonicReply;
}

impl SubsonicResponseBuilderExt for SubsonicResponseBuilder {
    fn create_acquisitions_response(&self, rows: &[AcquisitionSnapshot]) -> SubsonicReply {
        self.create_json_response(json!({
            "status": "ok",
            "version": SUBSONIC_VERSION,
            "type": "octo",
            "acquisitions": {
                "acquisition": rows.iter().map(|row| Value::Object(acquisition_json(row))).collect::<Vec<_>>(),
            },
        }))
    }

    fn generated_playlist_fields(
        &self,
        mix: &GeneratedPlaylist,
        settings: &GeneratedPlaylistSettings,
    ) -> Fields {
        let for_you = for_you_lists::is_for_you(&mix.kind);
        let artist = mix.key.starts_with(ARTIST_PREFIX);
        // A Made for you list or an artist mix is exactly what was stored; a mix draws its track
        // count from a pool.
        let song_count = if for_you || artist {
            mix.pool_size
        } else {
            settings.effective_track_count().min(mix.pool_size)
        };
        let comment = if mix.kind == for_you_lists::NEW_RELEASES_KIND {
            "Made for you by Octo from your artists' new releases"
        } else if for_you {
            "Made for you by Octo from your plays"
        } else if artist {
            "Generated by Octo from this artist and the artists like them in your library"
        } else {
            "Generated by Octo from your library"
        };
        let mut fields = Fields::new();
        fields.insert("id".into(), mix.id.clone().into());
        fields.insert("name".into(), mix.name.clone().into());
        fields.insert("owner".into(), mix.owner.clone().into());
        fields.insert("public".into(), false.into());
        fields.insert("songCount".into(), song_count.into());
        fields.insert("duration".into(), (i64::from(song_count) * 210).into());
        fields.insert("comment".into(), comment.into());
        fields.insert("created".into(), millis(&mix.period_start_utc).into());
        fields.insert("changed".into(), millis(&mix.period_start_utc).into());
        // The cover id names the picture as drawn: it changes with the design and the count the
        // cover shows, so apps that keep covers by id fetch the new one.
        let version = octo_media::cover::CoverBook::shipped().version;
        fields.insert(
            "coverArt".into(),
            format!("{}_{version}c{song_count}", mix.id).into(),
        );
        fields.insert("readonly".into(), true.into());
        fields.insert("validUntil".into(), millis(&mix.period_end_utc).into());
        // octoLists: which Made for you list this is, so the Octo apps can put it on Home.
        if for_you {
            fields.insert("octoList".into(), mix.kind.clone().into());
        }
        // octoLists v2: which group of Octo's lists this row belongs to, so apps can group them.
        fields.insert("octoGroup".into(), list_group(&mix.kind).into());
        fields
    }

    fn create_generated_playlist_response(
        &self,
        format: &str,
        mix: &GeneratedPlaylist,
        settings: &GeneratedPlaylistSettings,
        entries: &[Node],
    ) -> SubsonicReply {
        let fields = self.generated_playlist_fields(mix, settings);
        self.create_entries_playlist_response(format, fields, entries)
    }

    fn create_library_action_outcome_response(
        &self,
        song_id: &str,
        outcome: &LibraryActionOutcome,
        action: &str,
    ) -> SubsonicReply {
        self.create_library_action_response(
            song_id,
            &to_lower_invariant(outcome.state.name()),
            outcome.detail.as_deref(),
            action,
        )
    }

    fn create_library_edit_response(
        &self,
        song_id: &str,
        action: &str,
        outcome: &LibraryEditOutcome,
    ) -> SubsonicReply {
        self.create_json_response(json!({
            "status": "ok",
            "version": SUBSONIC_VERSION,
            "type": "octo",
            "openSubsonic": true,
            "libraryAction": {
                "id": song_id,
                "action": action,
                "state": outcome.state,
                "detail": outcome.detail,
                "before": outcome.before,
                "after": outcome.after,
            },
        }))
    }

    fn create_lookup_response(&self, song_id: &str, lookup: &SongLookup) -> SubsonicReply {
        self.create_json_response(json!({
            "status": "ok",
            "version": SUBSONIC_VERSION,
            "type": "octo",
            "openSubsonic": true,
            "libraryAction": {
                "id": song_id,
                "action": LOOKUP_ACTION,
                "state": lookup.state,
                "detail": lookup.detail,
                "current": lookup.current,
                "suggested": lookup.suggested,
                "confidence": lookup.confidence,
                "source": lookup.source,
                "release": lookup.release,
            },
        }))
    }

    fn create_library_trash_response(&self, songs: &[TrashedSong], keep_days: i32) -> SubsonicReply {
        self.create_json_response(json!({
            "status": "ok",
            "version": SUBSONIC_VERSION,
            "type": "octo",
            "openSubsonic": true,
            "libraryTrash": {
                "keepDays": keep_days,
                "songs": songs.iter().map(|song| {
                    let mut json = json!({
                        "id": song.id,
                        "title": song.title,
                        "artist": song.artist,
                        "album": song.album,
                        "removedBy": song.username,
                        "removedAt": round_trip(&song.removed_utc),
                        "goneAt": song.gone_utc.as_ref().map(round_trip),
                    });
                    with_replaced(&mut json, song);
                    json
                }).collect::<Vec<_>>(),
            },
        }))
    }

    fn create_upgrades_response(&self, jobs: &[(UpgradeJob, Option<f64>)]) -> SubsonicReply {
        self.create_json_response(json!({
            "status": "ok",
            "version": SUBSONIC_VERSION,
            "type": "octo",
            "openSubsonic": true,
            "upgrades": jobs.iter().map(|(job, progress)| json!({
                "id": job.navidrome_id,
                "title": job.title,
                "artist": job.artist,
                "album": job.album,
                "state": job.state,
                "detail": job.detail,
                "progress": progress,
                "updatedAt": round_trip(&job.updated_utc),
                // The downloads row that fetches the replacement, once there is one, for its log.
                "acquisition": job.acquisition_key.as_deref().and_then(|key| {
                    key.split_once(':')
                        .filter(|(provider, _)| !provider.is_empty())
                        .map(|(provider, id)| key_of(provider, id))
                }),
                "picked": job.pick.as_ref().map(|pick| pick.describe()),
            })).collect::<Vec<_>>(),
        }))
    }

    fn create_acquisition_response(&self, row: &AcquisitionSnapshot) -> SubsonicReply {
        let mut json = acquisition_json(row);
        json.insert(
            "event".into(),
            Value::Array(
                row.events
                    .iter()
                    .flatten()
                    .map(|line| Value::Object(event_json(line)))
                    .collect(),
            ),
        );
        self.create_json_response(json!({
            "status": "ok",
            "version": SUBSONIC_VERSION,
            "type": "octo",
            "acquisition": json,
        }))
    }

    fn create_cleared_response(&self, cleared: i32) -> SubsonicReply {
        self.create_json_response(json!({
            "status": "ok",
            "version": SUBSONIC_VERSION,
            "type": "octo",
            "cleared": { "count": cleared },
        }))
    }

    fn create_find_songs_response(&self, found: &FindSnapshot) -> SubsonicReply {
        self.create_json_response(json!({
            "status": "ok",
            "version": SUBSONIC_VERSION,
            "type": "octo",
            "foundSongs": found_songs_json(found),
        }))
    }

    fn create_pick_response(&self, outcome: &PickOutcome) -> SubsonicReply {
        self.create_json_response(json!({
            "status": "ok",
            "version": SUBSONIC_VERSION,
            "type": "octo",
            "pick": pick_json(outcome),
        }))
    }
}

/// A trashed song that another file replaced says so, and names both files: `kind` is
/// "replaced", `was` the file in the trash, `now` the one in the library. A removed song is left
/// as it always was.
pub fn with_replaced(json: &mut Value, song: &TrashedSong) {
    if let (Some(replaced), Some(fields)) = (&song.replaced, json.as_object_mut()) {
        fields.insert("kind".into(), "replaced".into());
        fields.insert("was".into(), json!(replaced.was));
        fields.insert("now".into(), json!(replaced.now));
    }
}

/// What a pick did, as the apps and the dashboard read it: its state, the words, the downloads
/// row to follow, and, for a pick that asks first, the two copies side by side.
pub fn pick_json(outcome: &PickOutcome) -> Value {
    let mut fields = Fields::new();
    fields.insert("state".into(), outcome.state.clone().into());
    fields.insert("detail".into(), outcome.detail.clone().into());
    fields.insert("key".into(), json!(outcome.key));
    if let Some(compare) = &outcome.compare {
        let side = |facts: &CopyFacts| {
            json!({
                "quality": facts.quality,
                "length": facts.length,
                "take": facts.take,
            })
        };
        fields.insert(
            "compare".into(),
            json!({ "yours": side(&compare.yours), "copy": side(&compare.copy) }),
        );
    }
    Value::Object(fields)
}

/// A Find songs search as the apps and the dashboard read it.
pub fn found_songs_json(found: &FindSnapshot) -> Fields {
    let mut fields = Fields::new();
    fields.insert("id".into(), found.id.clone().into());
    fields.insert("state".into(), found.state.clone().into());
    fields.insert("error".into(), json!(found.error));
    fields.insert("startedAt".into(), utc(&found.started_at).into());
    let target = &found.target;
    let mut song = json!({
        "artist": target.artist,
        "title": target.title,
        "album": target.album,
        "duration": target.duration,
        "coverArt": target.cover_art,
        "libraryId": target.library_id,
        "format": target.owned_format,
        "quality": target.owned_quality,
        "size": target.owned_size,
    });
    // A library song whose file is another take says so before anything is picked.
    if let (Some(take), Some(song)) = (found.take, song.as_object_mut()) {
        song.insert("take".into(), replace_anyway::take_header(take).into());
        song.insert("takeKind".into(), replace_anyway::take_kind(take).into());
    }
    fields.insert("song".into(), song);
    fields.insert(
        "source".into(),
        Value::Array(
            found
                .sources
                .iter()
                .map(|source| {
                    json!({
                        "name": source.name,
                        "state": source.state,
                        "text": source.text,
                        "query": source.queries,
                    })
                })
                .collect(),
        ),
    );
    fields.insert(
        "candidate".into(),
        Value::Array(
            found
                .copies
                .iter()
                .enumerate()
                .map(|(index, copy)| {
                    let mut json = candidate_json(&copy.shown);
                    json.insert("id".into(), copy.id.clone().into());
                    json.insert("index".into(), index.into());
                    // A copy no better than the library's says so on the list.
                    if let Some((vs, text)) = replace_anyway::vs_yours(target, copy) {
                        json.insert("vsYours".into(), vs.into());
                        json.insert("vsYoursText".into(), text.into());
                    }
                    Value::Object(json)
                })
                .collect(),
        ),
    );
    fields
}

/// One line of a download's log, for the apps and the dashboard alike.
pub fn event_json(line: &AcquisitionEvent) -> Fields {
    let mut fields = Fields::new();
    fields.insert("at".into(), utc(&line.at).into());
    fields.insert("kind".into(), line.kind.clone().into());
    fields.insert("text".into(), line.text.clone().into());
    fields.insert("detail".into(), json!(line.detail));
    fields.insert(
        "candidate".into(),
        match &line.candidates {
            Some(copies) => Value::Array(copies.iter().map(|c| Value::Object(candidate_json(c))).collect()),
            None => Value::Null,
        },
    );
    // The other recordings AcoustID heard, only on a check line that has them.
    if let Some(heard) = line.heard.as_ref().filter(|heard| !heard.is_empty()) {
        fields.insert(
            "heard".into(),
            Value::Array(
                heard
                    .iter()
                    .map(|h| json!({ "title": h.title, "artist": h.artist, "score": h.score }))
                    .collect(),
            ),
        );
    }
    fields
}

/// One copy a source offered, in the log and in Find songs alike.
pub fn candidate_json(copy: &AcquisitionCandidate) -> Fields {
    let mut fields = Fields::new();
    fields.insert("source".into(), copy.source.clone().into());
    fields.insert("peer".into(), json!(copy.peer));
    fields.insert("file".into(), json!(copy.file));
    fields.insert("folder".into(), json!(copy.folder));
    fields.insert("title".into(), json!(copy.title));
    fields.insert("album".into(), json!(copy.album));
    fields.insert("format".into(), json!(copy.format));
    fields.insert("quality".into(), json!(copy.quality()));
    fields.insert("bitRate".into(), json!(copy.bit_rate));
    fields.insert("bitDepth".into(), json!(copy.bit_depth));
    fields.insert("sampleRate".into(), json!(copy.sample_rate));
    fields.insert("size".into(), json!(copy.size));
    fields.insert("length".into(), json!(copy.length));
    fields.insert("queueLength".into(), json!(copy.queue_length));
    fields.insert("freeSlot".into(), json!(copy.free_slot));
    fields.insert("speed".into(), json!(copy.speed));
    fields.insert("rank".into(), json!(copy.rank));
    fields.insert("note".into(), json!(copy.note));
    fields
}

/// The action a libraryAction outcome answers with when none is given.
pub const DEFAULT_LIBRARY_ACTION: &str = REMOVE_ACTION;

/// One acquisition on the wire. The field names are a contract with the Octo app; a rename
/// here breaks its progress ring. Nulls are sent as null rather than left out.
pub fn acquisition_json(row: &AcquisitionSnapshot) -> Fields {
    let mut fields = Fields::new();
    fields.insert("id".into(), row.id.clone().into());
    fields.insert("artist".into(), json!(row.artist));
    fields.insert("title".into(), json!(row.title));
    fields.insert("album".into(), json!(row.album));
    fields.insert("state".into(), to_lower_invariant(row.state.name()).into());
    fields.insert("progress".into(), json!(row.progress));
    fields.insert("bytesDone".into(), json!(row.bytes_done));
    fields.insert("bytesTotal".into(), json!(row.bytes_total));
    fields.insert("source".into(), json!(row.source));
    fields.insert("startedAt".into(), utc(&row.started_at).into());
    fields.insert("updatedAt".into(), utc(&row.updated_at).into());
    fields.insert("error".into(), json!(row.error));
    fields.insert("libraryId".into(), json!(row.library_id));
    // Added for the app's words beside the ring. Older apps ignore them.
    fields.insert("ahead".into(), json!(row.ahead));
    fields.insert("note".into(), json!(row.note));
    // Added for the downloads drawer: the row's own key for getAcquisition, what it is for, the
    // copy being fetched, the picture to draw, and how long its log is.
    fields.insert("key".into(), row.key.clone().into());
    fields.insert("kind".into(), row.kind.clone().into());
    fields.insert("quality".into(), json!(row.quality));
    fields.insert("peer".into(), json!(row.peer));
    fields.insert(
        "coverArt".into(),
        row.library_id.clone().unwrap_or_else(|| row.id.clone()).into(),
    );
    fields.insert("logLines".into(), row.log_lines.into());
    // octoAcquisitions 4: written only when set, never as null.
    if row.keep_anyway {
        fields.insert("keepAnyway".into(), true.into());
    }
    if let Some(held) = row.held_tracks.filter(|held| *held > 0) {
        fields.insert("heldTracks".into(), held.into());
    }
    if row.stop_waiting {
        fields.insert("stopWaiting".into(), true.into());
    }
    fields
}

/// `yyyy-MM-dd'T'HH:mm:ss'Z'`.
fn utc(value: &DateTime<Utc>) -> String {
    value.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// `yyyy-MM-ddTHH:mm:ss.fffZ`.
fn millis(value: &DateTime<Utc>) -> String {
    value.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// A UTC time in the round-trip format: seven fraction digits and `Z`.
fn round_trip(value: &DateTime<Utc>) -> String {
    format!(
        "{}.{:07}Z",
        value.format("%Y-%m-%dT%H:%M:%S"),
        value.timestamp_subsec_nanos() / 100
    )
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use octo_subsonic::ReplyKind;

    use super::*;
    use crate::services::common::acquisition_tracker::AcquisitionState;
    use crate::services::library::LibraryActionState;

    fn builder() -> SubsonicResponseBuilder {
        new_subsonic_response_builder(
            Arc::new(ExternalIdRegistry::in_memory()),
            &SubsonicSettings::default(),
        )
    }

    #[test]
    fn the_registry_mints_the_ids_of_rows_without_one() {
        let registry = Arc::new(ExternalIdRegistry::in_memory());
        let builder = new_subsonic_response_builder(registry.clone(), &SubsonicSettings::default());
        let row = builder.convert_song_to_json(&octo_core::models::domain::Song {
            id: "s".into(),
            title: "Borealis".into(),
            artist: "Aurora Vale".into(),
            ..Default::default()
        });
        let album_id = row["albumId"].as_str().expect("album id");
        let routing = registry.lookup(album_id).expect("registered").snapshot();
        assert_eq!(routing.album.as_deref(), Some("Borealis"));
        assert_eq!(routing.artist.as_deref(), Some("Aurora Vale"));
        assert!(
            registry
                .lookup(row["artistId"].as_str().expect("artist id"))
                .is_some()
        );
    }

    #[test]
    fn acquisitions_write_every_field_with_nulls() {
        let at = Utc.with_ymd_and_hms(2026, 10, 4, 8, 7, 45).unwrap() + chrono::Duration::milliseconds(123);
        let row = AcquisitionSnapshot {
            id: "deezer:1".into(),
            artist: Some("A".into()),
            state: AcquisitionState::Downloading,
            progress: Some(0.25),
            bytes_done: Some(10),
            started_at: at,
            updated_at: at,
            ahead: Some(2),
            ..Default::default()
        };
        let reply = builder().create_acquisitions_response(&[row]);
        assert_eq!(reply.kind, ReplyKind::Json);
        assert_eq!(
            reply.text(),
            concat!(
                r#"{"subsonic-response":{"status":"ok","version":"1.16.1","type":"octo","acquisitions":{"acquisition":[{"#,
                r#""id":"deezer:1","artist":"A","title":null,"album":null,"state":"downloading","progress":0.25,"#,
                r#""bytesDone":10,"bytesTotal":null,"source":null,"startedAt":"2026-10-04T08:07:45Z","#,
                r#""updatedAt":"2026-10-04T08:07:45Z","error":null,"libraryId":null,"ahead":2,"note":null,"#,
                r#""key":"","kind":"","quality":null,"peer":null,"coverArt":"deezer:1","logLines":0}]}}}"#
            )
        );
    }

    #[test]
    fn library_action_outcomes_and_upgrades_answer_in_json() {
        let outcome = LibraryActionOutcome::new(LibraryActionState::Rehearsed, Some("dry run".into()));
        assert_eq!(
            builder()
                .create_library_action_outcome_response("s1", &outcome, DEFAULT_LIBRARY_ACTION)
                .text(),
            r#"{"subsonic-response":{"status":"ok","version":"1.16.1","type":"octo","openSubsonic":true,"libraryAction":{"id":"s1","action":"remove","state":"rehearsed","detail":"dry run"}}}"#
        );
        let job = UpgradeJob {
            navidrome_id: "n1".into(),
            title: Some("T".into()),
            state: "working".into(),
            updated_utc: Utc.with_ymd_and_hms(2026, 10, 4, 8, 7, 45).unwrap()
                + chrono::Duration::nanoseconds(123_456_700),
            ..Default::default()
        };
        assert_eq!(
            builder().create_upgrades_response(&[(job, Some(0.5))]).text(),
            r#"{"subsonic-response":{"status":"ok","version":"1.16.1","type":"octo","openSubsonic":true,"upgrades":[{"id":"n1","title":"T","artist":null,"album":null,"state":"working","detail":null,"progress":0.5,"updatedAt":"2026-10-04T08:07:45.1234567Z","acquisition":null,"picked":null}]}}"#
        );
    }

    #[test]
    fn a_generated_mix_is_a_read_only_playlist_of_navidromes_songs() {
        let start = Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap();
        let mix = GeneratedPlaylist {
            id: "og1".into(),
            kind: "genre".into(),
            name: "Rock mix".into(),
            owner: "alice".into(),
            pool_size: 10,
            period_start_utc: start,
            period_end_utc: start + chrono::Duration::days(7),
            ..Default::default()
        };
        let settings = GeneratedPlaylistSettings::default();
        let fields = builder().generated_playlist_fields(&mix, &settings);
        let count = settings.effective_track_count().min(10);
        assert_eq!(fields["songCount"], count);
        assert_eq!(fields["duration"], i64::from(count) * 210);
        assert_eq!(fields["validUntil"], "2026-10-08T00:00:00.000Z");
        let entries = [Node::parse(r#"{"id":"n1","duration":200}"#).expect("JSON")];
        assert_eq!(
            builder()
                .create_generated_playlist_response("json", &mix, &settings, &entries)
                .text(),
            r#"{"subsonic-response":{"status":"ok","version":"1.16.1","playlist":{"id":"og1","name":"Rock mix","owner":"alice","public":false,"songCount":1,"duration":200,"comment":"Generated by Octo from your library","created":"2026-10-01T00:00:00.000Z","changed":"2026-10-01T00:00:00.000Z","coverArt":"og1_2c10","readonly":true,"validUntil":"2026-10-08T00:00:00.000Z","octoGroup":"genre","entry":[{"id":"n1","duration":200}]}}}"#
        );
    }
}
