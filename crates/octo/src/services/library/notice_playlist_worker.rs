//! The reconcile decision for one person's notice playlist, and the worker that fills and tidies
//! the playlists Octo asks through (#47) and sends back the answers AcoustID can use.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::{TimeDelta, Utc};
use futures::FutureExt;
use octo_core::common::dotnet;
use octo_core::fingerprint::acoust_id_client::AcoustIdSubmission;
use octo_core::fingerprint::verification::InconclusiveReason;
use octo_core::settings::{LibraryActionSettings, NoticeKind, SettingsStore, SoulseekSettings};
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use super::duplicate_fixer::duplicates_comment;
use super::library_action_playlist_worker::PlaylistRow;
use super::notice_queue::{NamedEnum, NoticeEntry, NoticeOrigin, NoticeQueue, NoticeState};
use super::{NavidromePlaylistApi, NavidromeSongPathResolver};
use crate::services::fingerprint::{AcoustIdClient, MusicBrainzClient};
use crate::services::subsonic::NavidromeIdentityService;

/// What one sweep does to one person's notice playlist.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct NoticePlan {
    pub dismiss: Vec<String>,
    pub adopt: Vec<String>,
    pub remove: Vec<String>,
    pub add: Vec<NoticeEntry>,
}

/// The whole reconcile decision, separated from the HTTP so it can be driven directly: what the
/// person answered by removing a track, what to take off because it is settled, and what fits.
pub struct NoticeReconcile;

impl NoticeReconcile {
    pub fn plan(entries: &[NoticeEntry], present: &HashSet<String>, max: i32) -> NoticePlan {
        let listed = |entry: &NoticeEntry| entry.navidrome_id.as_ref().is_some_and(|id| present.contains(id));

        // Queued and gone from the playlist: the person removed it by hand, which is an answer.
        // A duplicate group is one question, so taking any copy out answers it for the whole
        // group.
        let removed_by_hand: Vec<&NoticeEntry> = entries
            .iter()
            .filter(|entry| {
                entry.state == NoticeState::Queued && entry.navidrome_id.is_some() && !listed(entry)
            })
            .collect();
        let answered_groups: HashSet<&str> = removed_by_hand
            .iter()
            .filter_map(|entry| entry.group_key.as_deref())
            .collect();
        let removed_keys: HashSet<&str> = removed_by_hand.iter().map(|entry| entry.key.as_str()).collect();
        let dismiss: Vec<String> = entries
            .iter()
            .filter(|entry| {
                removed_keys.contains(entry.key.as_str())
                    || (entry.is_open()
                        && entry
                            .group_key
                            .as_deref()
                            .is_some_and(|group| answered_groups.contains(group)))
            })
            .map(|entry| entry.key.clone())
            .collect();
        let dismissing: HashSet<&str> = dismiss.iter().map(String::as_str).collect();
        let still_asking = |entry: &NoticeEntry| entry.is_open() && !dismissing.contains(entry.key.as_str());

        // Waiting but already in the playlist (a restart between adding and recording, or the
        // person added it themselves): count it as asked rather than adding it twice.
        let adopt: Vec<String> = entries
            .iter()
            .filter(|entry| entry.state == NoticeState::Waiting && listed(entry) && still_asking(entry))
            .map(|entry| entry.key.clone())
            .collect();

        // Settled but still listed: take it off. A track an open question still needs stays,
        // whatever an older, settled entry about it says; a duplicate group found again after
        // one expired would otherwise lose its own tracks.
        let needed: HashSet<&str> = entries
            .iter()
            .filter(|entry| still_asking(entry))
            .filter_map(|entry| entry.navidrome_id.as_deref())
            .collect();
        let mut remove: Vec<String> = Vec::new();
        for entry in entries {
            let Some(id) = entry.navidrome_id.as_deref() else {
                continue;
            };
            if listed(entry)
                && !needed.contains(id)
                && (!entry.is_open() || dismissing.contains(entry.key.as_str()))
                && !remove.iter().any(|r| r == id)
            {
                remove.push(id.to_string());
            }
        }

        let asked = entries
            .iter()
            .filter(|entry| entry.state == NoticeState::Queued && listed(entry) && still_asking(entry))
            .count()
            + adopt.len();
        let mut room = (i64::from(max) - asked as i64).max(0) as usize;

        let waiting: Vec<&NoticeEntry> = entries
            .iter()
            .filter(|entry| {
                entry.state == NoticeState::Waiting
                    && entry.navidrome_id.is_some()
                    && !listed(entry)
                    && still_asking(entry)
            })
            .collect();

        // Grouped by group key, else key: groups in the order their first member came, then
        // oldest first (a stable sort).
        let mut groups: Vec<(&str, Vec<&NoticeEntry>)> = Vec::new();
        for entry in waiting {
            let key = entry.group_key.as_deref().unwrap_or(&entry.key);
            match groups.iter_mut().find(|(k, _)| *k == key) {
                Some((_, members)) => members.push(entry),
                None => groups.push((key, vec![entry])),
            }
        }
        groups.sort_by_key(|(_, members)| members.iter().map(|entry| entry.created_utc).min());

        // A duplicate pair only makes sense together, so a group goes in whole or waits.
        let mut add: Vec<NoticeEntry> = Vec::new();
        for (key, mut members) in groups {
            members.sort_by_key(|entry| entry.order);
            if members.len() > room {
                if key == members[0].key {
                    break;
                }
                continue;
            }
            room -= members.len();
            add.extend(members.into_iter().cloned());
            if room == 0 {
                break;
            }
        }
        NoticePlan {
            dismiss,
            adopt,
            remove,
            add,
        }
    }
}

/// Fills and tidies the playlists Octo uses to ask a person something (#47), and sends back the
/// answers AcoustID can use.
///
/// Every tick reads its settings afresh, so switching Review on or off needs no restart. The
/// playlists themselves are created by LibraryActionPlaylistProvisioner with the person's own
/// credentials the next time they list their playlists; this worker only ever adds and removes
/// tracks, as the admin.
pub struct NoticePlaylistWorker {
    queue: Arc<NoticeQueue>,
    api: Arc<NavidromePlaylistApi>,
    resolver: Arc<NavidromeSongPathResolver>,
    identity: NavidromeIdentityService,
    acoust_id: Arc<AcoustIdClient>,
    music_brainz: Arc<MusicBrainzClient>,
    /// The library action and Soulseek settings, read at each sweep.
    settings: Arc<SettingsStore>,
    /// The description each notice playlist was last given, by playlist id.
    comments: Mutex<HashMap<String, String>>,
}

impl NoticePlaylistWorker {
    const LOOKUPS_PER_SWEEP: usize = 50;
    const SUBMISSION_BATCH: usize = 10;

    pub fn new(
        queue: Arc<NoticeQueue>,
        api: Arc<NavidromePlaylistApi>,
        resolver: Arc<NavidromeSongPathResolver>,
        identity: NavidromeIdentityService,
        acoust_id: Arc<AcoustIdClient>,
        music_brainz: Arc<MusicBrainzClient>,
        settings: Arc<SettingsStore>,
    ) -> Self {
        NoticePlaylistWorker {
            queue,
            api,
            resolver,
            identity,
            acoust_id,
            music_brainz,
            settings,
            comments: Mutex::new(HashMap::new()),
        }
    }

    /// Gives the playlist `comment` as its description, once per text: apps show it under the
    /// playlist's name, so a person who never opens the dashboard reads what to do there.
    async fn describe(&self, playlist_id: &str, comment: &str) {
        if self
            .comments
            .lock()
            .get(playlist_id)
            .is_some_and(|said| said == comment)
        {
            return;
        }
        if self.api.set_comment(playlist_id, comment).await {
            self.comments
                .lock()
                .insert(playlist_id.to_string(), comment.to_string());
        }
    }

    /// The worker loop, until the host stops.
    pub async fn run(self: Arc<Self>, stopping: CancellationToken) -> anyhow::Result<()> {
        while !stopping.is_cancelled() {
            let settings = self.settings.current().library_actions.clone();
            // Per-sweep catch is mandatory: one failed sweep must never stop the worker, so a
            // sweep that panics is caught here.
            if settings.enabled && settings.notices_enabled() && self.identity.has_admin_identity() {
                let sweep = tokio::select! {
                    sweep = std::panic::AssertUnwindSafe(self.sweep(&settings)).catch_unwind() => sweep,
                    _ = stopping.cancelled() => break,
                };
                match sweep {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => error!("Notice playlist sweep failed: {e}"),
                    Err(_) => error!("Notice playlist sweep failed"),
                }
            }

            tokio::select! {
                _ = tokio::time::sleep(settings.effective_poll_interval()) => {}
                _ = stopping.cancelled() => break,
            }
        }
        Ok(())
    }

    async fn sweep(&self, settings: &LibraryActionSettings) -> anyhow::Result<()> {
        self.resolve_ids().await;

        let playlists = self.api.list_playlists().await;
        for user in settings
            .allowed_users
            .iter()
            .filter(|user| !dotnet::is_blank(user))
        {
            for kind in settings.enabled_notice_kinds() {
                if let Err(e) = self.reconcile(user.trim(), kind, settings, &playlists).await {
                    warn!("Could not update {} for {user}: {e}", kind.name());
                }
            }
        }

        self.submit_kept(settings).await?;
        self.queue.flush();
        Ok(())
    }

    /// A new download only has a Navidrome id once Navidrome has scanned it.
    async fn resolve_ids(&self) {
        let now = Utc::now();
        for entry in self.queue.due_for_lookup(now, Self::LOOKUPS_PER_SWEEP) {
            let exists = std::fs::metadata(&entry.local_path).is_ok_and(|m| m.is_file());
            if !exists || now - entry.created_utc > TimeDelta::days(7) {
                self.queue.resolve(&entry.key, NoticeState::Expired);
                continue;
            }
            match self
                .resolver
                .find_id_by_path(&entry.artist, &entry.title, &entry.local_path)
                .await
            {
                None => self.queue.defer_lookup(&entry.key, now),
                Some(id) => self.queue.set_navidrome_id(&entry.key, &id),
            }
        }
    }

    async fn reconcile(
        &self,
        user: &str,
        kind: NoticeKind,
        settings: &LibraryActionSettings,
        playlists: &[PlaylistRow],
    ) -> Result<(), String> {
        let title = settings.notice_title(kind);
        // Created with the person's own credentials the next time they list their playlists.
        let Some(playlist) = playlists.iter().find(|row| {
            dotnet::eq_ignore_case(&row.owner, user) && dotnet::eq_ignore_case(&row.name, &title)
        }) else {
            return Ok(());
        };
        if kind == NoticeKind::Duplicates {
            self.describe(&playlist.id, &duplicates_comment(settings)).await;
        }

        let tracks = self.api.list_tracks(&playlist.id).await;
        let present: HashSet<String> = tracks.iter().map(|track| track.media_file_id.clone()).collect();
        let plan = NoticeReconcile::plan(
            &self.queue.for_user(user, kind),
            &present,
            settings.effective_notice_max_tracks(),
        );

        for key in &plan.dismiss {
            self.queue.resolve(key, NoticeState::Dismissed);
        }
        self.queue.mark_queued(&plan.adopt);

        if !plan.remove.is_empty() {
            let positions: Vec<String> = tracks
                .iter()
                .filter(|track| plan.remove.contains(&track.media_file_id))
                .map(|track| track.position.clone())
                .filter(|position| !position.is_empty())
                .collect();
            self.api.remove_positions(&playlist.id, &positions).await?;
        }

        if !plan.add.is_empty() {
            let ids: Vec<String> = plan
                .add
                .iter()
                .filter_map(|entry| entry.navidrome_id.clone())
                .collect();
            if self.api.add_tracks(&playlist.id, &ids).await? {
                let keys: Vec<&str> = plan.add.iter().map(|entry| entry.key.as_str()).collect();
                self.queue.mark_queued(&keys);
            }
        }

        if plan.dismiss.len() + plan.remove.len() + plan.add.len() > 0 {
            info!(
                "{} for {user}: {} added, {} settled, {} dismissed",
                kind.name(),
                plan.add.len(),
                plan.remove.len(),
                plan.dismiss.len()
            );
        }
        Ok(())
    }

    /// Send the fingerprints people confirmed. Only with consent and a user key, never in a dry
    /// run, never for a track AcoustID confidently called something else, only for the standard
    /// 120-second fingerprint AcoustID's own tools make, and only with one unambiguous recording.
    /// `Err` when MusicBrainz timed out or answered nonsense.
    async fn submit_kept(&self, settings: &LibraryActionSettings) -> anyhow::Result<()> {
        let soulseek = self.settings.current().soulseek.clone();
        if !Self::may_submit(settings, &soulseek) {
            return Ok(());
        }

        let mut ready: Vec<(NoticeEntry, AcoustIdSubmission)> = Vec::new();
        let mut refused: Vec<String> = Vec::new();
        for entry in self.queue.awaiting_submission() {
            if !Self::submittable(&entry, &soulseek) {
                refused.push(entry.key.clone());
                continue;
            }
            let recording = match entry.candidate_recording_id.clone() {
                Some(id) => Some(id),
                None => {
                    self.music_brainz
                        .find_recording(&entry.artist, &entry.title, entry.duration_seconds)
                        .await?
                }
            };
            let Some(recording) = recording else {
                info!(
                    "Kept '{} - {}', but no single MusicBrainz recording fits it, so nothing was sent",
                    entry.artist, entry.title
                );
                refused.push(entry.key.clone());
                continue;
            };
            let item = AcoustIdSubmission {
                fingerprint: entry.fingerprint.clone().unwrap_or_default(),
                duration_seconds: entry.duration_seconds,
                recording_id: recording,
                file_format: entry.file_format.clone(),
            };
            ready.push((entry, item));
        }
        if !refused.is_empty() {
            self.queue.mark_submitted(&refused, false);
        }

        for batch in ready.chunks(Self::SUBMISSION_BATCH) {
            let items: Vec<AcoustIdSubmission> = batch.iter().map(|(_, item)| item.clone()).collect();
            // The user's own application key only: a submission never uses Octo's shared one.
            let Some(client_key) = soulseek.acoust_id_key_for_bulk() else {
                break;
            };
            if !self
                .acoust_id
                .submit(
                    &client_key,
                    &soulseek.acoust_id_user_api_key,
                    &items,
                    soulseek.effective_acoust_id_timeout_seconds(),
                )
                .await
            {
                continue;
            }
            let keys: Vec<&str> = batch.iter().map(|(entry, _)| entry.key.as_str()).collect();
            self.queue.mark_submitted(&keys, true);
            info!("Sent {} confirmed fingerprint(s) to AcoustID", batch.len());
        }
        Ok(())
    }

    /// Consent and both of the user's own keys (never Octo's shared one, and never with AcoustID
    /// `off`), and never in a dry run.
    pub fn may_submit(settings: &LibraryActionSettings, soulseek: &SoulseekSettings) -> bool {
        soulseek.submit_confirmed_fingerprints
            && !settings.dry_run
            && soulseek.acoust_id_key_for_bulk().is_some()
            && !dotnet::is_blank(&soulseek.acoust_id_user_api_key)
    }

    /// Only what AcoustID could not place: a track it confidently named as something else was
    /// kept for a reason that is not "AcoustID is missing this". And only the standard
    /// 120-second fingerprint AcoustID's own tools make, so a shortened one never lands beside
    /// them. Never a library sweep question: its tags were never confirmed by anyone.
    pub fn submittable(entry: &NoticeEntry, soulseek: &SoulseekSettings) -> bool {
        entry.origin == NoticeOrigin::Download
            && matches!(
                entry.cause,
                InconclusiveReason::NoEntry | InconclusiveReason::BelowThreshold
            )
            && soulseek.fingerprint_seconds == 120
            && entry.duration_seconds > 0
            && entry.fingerprint.as_deref().is_some_and(|f| !f.is_empty())
    }
}

#[cfg(test)]
#[path = "notice_playlist_worker_tests.rs"]
mod tests;
