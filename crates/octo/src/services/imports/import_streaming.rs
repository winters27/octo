//! An imported snapshot as an Octo-only playlist. All metadata comes from the import;
//! minting a preview never searches YouTube or starts an acquisition.

use octo_core::library::generated_playlist_service;
use octo_core::models::domain::Song;
use octo_core::soulseek::{RoutingKind, SoulseekRouting};
use serde_json::{Map, Value, json};

use super::{ImportList, ImportTrack};
use crate::services::soulseek::{ExternalIdRegistry, SoulseekMetadataService};

/// Navidrome-shaped, stable for the owner and import, independent of its name or matches.
pub fn playlist_id(list: &ImportList) -> String {
    let hashed = generated_playlist_service::playlist_id(&list.owner, &format!("import:{}", list.id));
    format!("oi{}", &hashed[2..])
}

pub fn playlist_fields(list: &ImportList) -> Map<String, Value> {
    json!({
        "id": playlist_id(list),
        "name": list.name,
        "owner": list.owner,
        "public": false,
        "readonly": true,
        "songCount": list.tracks.len(),
        "duration": list.tracks.iter().map(|t| i64::from(t.seconds.unwrap_or(0).max(0))).sum::<i64>(),
        "comment": "Imported snapshot. Missing songs stream from YouTube; heart a song to keep it.",
        "created": list.read_utc.to_rfc3339(),
        "changed": list.matched_utc.unwrap_or(list.read_utc).max(list.read_utc).to_rfc3339(),
    })
    .as_object()
    .expect("an object")
    .clone()
}

/// The same song routing the import trickle uses. The registry preserves a video/stream
/// already found for this id, so opening the playlist never forgets a pinned preview.
pub fn preview_song(registry: &ExternalIdRegistry, track: &ImportTrack) -> Song {
    let id = registry.register(preview_routing(track));
    Song {
        id: id.clone(),
        artist: track.artist.clone(),
        title: track.title.clone(),
        album: track.album.clone().unwrap_or_default(),
        duration: track.seconds,
        isrc: track.isrc.clone(),
        is_local: false,
        external_provider: Some(SoulseekMetadataService::PROVIDER_NAME.to_string()),
        external_id: Some(id),
        ..Default::default()
    }
}

pub fn preview_routing(track: &ImportTrack) -> SoulseekRouting {
    SoulseekRouting {
        kind: RoutingKind::Song,
        artist: Some(track.artist.clone()),
        title: Some(track.title.clone()),
        album: track.album.clone(),
        duration: track.seconds,
        isrc: track.isrc.clone(),
        ..Default::default()
    }
}

#[cfg(test)]
#[path = "import_streaming_tests.rs"]
mod tests;
