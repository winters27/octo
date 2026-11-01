//! Keep anyway: a person's own way out of a hold nothing can prove. A held album track lands the
//! file its hold kept, and a song held while AcoustID is off keeps the very audio its person heard;
//! either is marked "kept by you, version not proven" and stays on the path that checks kept files
//! again, so a copy proven later replaces it. The request is saved with the hold before anything
//! is carried out, so a restart in the middle finishes it.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use octo_core::common::dotnet::eq_ignore_case;
use octo_core::fingerprint::heart_gate::{HeardReading, HeartGate, WaitingFor};
use octo_core::settings::DownloadSource;
use tracing::{debug, info, warn};

use super::{HEARD_GONE, HeartAcquisitionCoordinator, track_base_key};
use crate::services::common::acquisition_tracker::{AcquisitionSnapshot, key_of};
use crate::services::common::soulseek_hold_store::Holder;
use crate::services::common::{AcquisitionHeld, AcquisitionTarget, HeldAcquisition, HeldKind};

/// The downloads line of a song whose keep waits for YouTube to answer for the heard video.
pub const KEEP_WAITS_FOR_YOUTUBE: &str = "Keeping the version you heard once YouTube answers";

/// What asking to keep a held row anyway came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepAnyway {
    /// Saved with its holds, and being carried out.
    Keeping,
    /// The person holds the row, and nothing of it waits to be kept for them now: it does not
    /// offer Keep anyway, or they asked already.
    NothingWaiting,
    /// The row's holds list other people, never this person.
    NotAHolder,
    /// Nothing of the row is held.
    NothingHeld,
}

/// What one person's downloads list shows of the holds: the rows they may keep anyway now, and how
/// many held tracks each hearted album's row holds.
#[derive(Debug, Clone, Default)]
pub struct KeepMarks {
    keepable: HashSet<String>,
    held_tracks: HashMap<String, usize>,
}

impl KeepMarks {
    /// Whether the person may keep the row `key` anyway now.
    pub fn keeps(&self, key: &str) -> bool {
        self.keepable.contains(&row_key(key))
    }

    /// How many held tracks the album row `key` holds, when it holds any.
    pub fn held_tracks(&self, key: &str) -> Option<usize> {
        self.held_tracks.get(&row_key(key)).copied()
    }

    /// The row as this person sees it: Keep anyway only when they may, and an album row's count
    /// of held tracks.
    pub fn mark(&self, row: &mut AcquisitionSnapshot) {
        row.keep_anyway = self.keeps(&row.key);
        row.held_tracks = self
            .held_tracks(&row.key)
            .map(|held| i32::try_from(held).unwrap_or(i32::MAX));
    }
}

/// A row's key as the downloads list writes it, `provider:id` with the provider in lower case.
fn row_key(key: &str) -> String {
    match key.trim().split_once(':') {
        Some((provider, id)) => key_of(provider, id),
        None => key.trim().to_string(),
    }
}

/// The rows a hold is listed under: its own acquisition's, and a held album track's album row.
fn rows_of(record: &HeldAcquisition) -> Vec<String> {
    let mut rows = vec![key_of(&record.provider, &record.external_id)];
    if let Some(track) = &record.album_track {
        rows.push(row_key(&track.album_key));
    }
    rows
}

/// Whether `user` waits in the hold.
fn holds_it(record: &HeldAcquisition, user: &str) -> bool {
    record
        .holders
        .iter()
        .any(|holder| eq_ignore_case(holder.user.trim(), user.trim()))
}

/// Whether `user` asked to keep the hold anyway.
fn asked(record: &HeldAcquisition, user: &str) -> bool {
    record
        .keep_requested
        .iter()
        .any(|asker| eq_ignore_case(asker.trim(), user.trim()))
}

/// Whether `user` may keep the hold anyway now: they wait in it, it offers Keep anyway, and they
/// have not asked yet.
fn keepable_by(record: &HeldAcquisition, user: &str) -> bool {
    record.keeps_anyway() && holds_it(record, user) && !asked(record, user)
}

impl HeartAcquisitionCoordinator {
    /// What `user`'s downloads list shows of the holds now: each row a hold of theirs offering Keep
    /// anyway is listed under (dropped once they asked), and each hearted album row's count of
    /// held tracks, whoever holds them.
    pub fn keep_marks(&self, user: &str) -> KeepMarks {
        let mut marks = KeepMarks::default();
        let Some(holds) = &self.extras.holds else {
            return marks;
        };
        for record in holds.snapshot() {
            if let Some(track) = &record.album_track {
                *marks.held_tracks.entry(row_key(&track.album_key)).or_default() += 1;
            }
            if !user.trim().is_empty() && keepable_by(&record, user) {
                marks.keepable.extend(rows_of(&record));
            }
        }
        marks
    }

    /// Who may keep the row `key` anyway now: each person waiting in a hold of it that offers Keep
    /// anyway and has not asked yet, in the order they joined. The dashboard's holder menu.
    pub fn keepers(&self, key: &str) -> Vec<String> {
        let mut keepers: Vec<String> = Vec::new();
        for record in self.records_of_row(key) {
            for holder in &record.holders {
                let user = holder.user.trim();
                if !user.is_empty()
                    && keepable_by(&record, user)
                    && !keepers.iter().any(|known| eq_ignore_case(known, user))
                {
                    keepers.push(user.to_string());
                }
            }
        }
        keepers
    }

    /// Every hold the row `key` lists, oldest first.
    fn records_of_row(&self, key: &str) -> Vec<HeldAcquisition> {
        let key = row_key(key);
        self.extras
            .holds
            .as_ref()
            .map(|holds| {
                holds
                    .snapshot()
                    .into_iter()
                    .filter(|record| rows_of(record).contains(&key))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// `user` keeps the row `key` anyway: Keep anyway on a song's row, Keep these anyway on a
    /// hearted album's. Each hold of the row they wait in that offers it is saved with their request
    /// before anything else, and then carried out: a held album track's file lands for everyone who
    /// holds it (the files are one set), and a song keeps only this person's own heard audio. The
    /// one action behind both the apps' call and the dashboard's button.
    pub fn keep_anyway(self: &Arc<Self>, key: &str, user: &str) -> KeepAnyway {
        let user = user.trim();
        let Some(holds) = self.extras.holds.clone() else {
            return KeepAnyway::NothingHeld;
        };
        let records = self.records_of_row(key);
        if records.is_empty() {
            return KeepAnyway::NothingHeld;
        }
        if user.is_empty() || !records.iter().any(|record| holds_it(record, user)) {
            return KeepAnyway::NotAHolder;
        }
        let mut saved = Vec::new();
        for record in records.iter().filter(|record| keepable_by(record, user)) {
            let kept = holds.update(&record.key(), |kept| {
                if !asked(kept, user) {
                    kept.keep_requested.push(user.to_string());
                }
            });
            if kept.is_some() {
                saved.push(record.key());
            }
        }
        if saved.is_empty() {
            return KeepAnyway::NothingWaiting;
        }
        info!("{user} keeps {} anyway: {}", row_key(key), saved.join(", "));
        self.keep_saved(saved);
        KeepAnyway::Keeping
    }

    /// Carries out the keeps saved in these holds, one hold after another.
    pub(crate) fn keep_saved(self: &Arc<Self>, keys: Vec<String>) {
        let coordinator = Arc::clone(self);
        tokio::spawn(async move { coordinator.finish_keeps(keys).await });
    }

    /// [`Self::keep_saved`], run to its end here.
    pub(crate) async fn finish_keeps(&self, keys: Vec<String>) {
        for key in keys {
            self.keeps_after(&key, &[]).await;
        }
    }

    /// Carries out every keep saved in the hold `key` that `done` does not name, until none is
    /// left. Nothing while the hold runs again: that pass carries them out when it ends.
    pub(crate) async fn keeps_after(&self, key: &str, done: &[String]) {
        let Some(holds) = self.extras.holds.clone() else {
            return;
        };
        let mut done = done.to_vec();
        loop {
            let Some(record) = holds.get(key) else {
                return;
            };
            let fresh: Vec<String> = record
                .keep_requested
                .iter()
                .filter(|user| !done.iter().any(|known| eq_ignore_case(known.trim(), user.trim())))
                .cloned()
                .collect();
            if fresh.is_empty() || !self.resuming.lock().insert(key.to_string()) {
                return;
            }
            self.carry_out(&record, &fresh).await;
            self.resuming.lock().remove(key);
            done.extend(fresh);
        }
    }

    /// The keeps saved in `record`, carried out before it runs again: what is left of it for the
    /// people who did not ask to keep it, or None when nobody is.
    pub(crate) async fn keeps_first(&self, record: &HeldAcquisition) -> Option<HeldAcquisition> {
        if record.keep_requested.is_empty() {
            return Some(record.clone());
        }
        self.carry_out(record, &record.keep_requested).await;
        let mut rest = self.extras.holds.as_ref()?.get(&record.key())?;
        let waiting_to_keep = rest.clone();
        rest.holders
            .retain(|holder| !asked(&waiting_to_keep, &holder.user));
        (!rest.holders.is_empty()).then_some(rest)
    }

    /// One hold's keeps: its held files when any is left (one set, for everyone who holds it),
    /// else each asker's own heard audio.
    async fn carry_out(&self, record: &HeldAcquisition, users: &[String]) {
        if record.held_files.iter().any(|file| Path::new(file).is_file()) {
            self.keep_held_files(record).await;
            return;
        }
        for user in users {
            self.keep_heard_audio(record, user).await;
        }
    }

    /// A hold's file lands as it is, for everyone who holds it, and the hold ends. A keep that
    /// cannot land clears the request, so the row offers it again; a hold with no file left ends.
    async fn keep_held_files(&self, record: &HeldAcquisition) {
        let Some(holds) = self.extras.holds.clone() else {
            return;
        };
        let key = record.key();
        let origin = record.origin();
        let requested_by = record
            .requested_by
            .clone()
            .or_else(|| record.holders.first().map(|holder| holder.user.clone()))
            .filter(|user| !user.trim().is_empty());
        let target = requested_by
            .as_deref()
            .and_then(|user| self.import_target(user).ok())
            .unwrap_or(AcquisitionTarget::Shared);
        let r = record.album_track.as_ref().map(|track| track.r.clone());
        // Everyone who holds it counts the file as theirs, by no proof: nothing heard is read.
        let gates: Vec<HeartGate> = record
            .holders
            .iter()
            .map(|holder| HeartGate {
                user: holder.user.trim().to_string(),
                origin,
                r: r.clone(),
                heard: None,
                heard_seconds: None,
                reading: HeardReading::NotHeard,
                stand_in: None,
            })
            .collect();
        for attempt in 0..2 {
            let files: Vec<String> = holds
                .get(&key)
                .unwrap_or_else(|| record.clone())
                .held_files
                .into_iter()
                .filter(|file| Path::new(file).is_file())
                .collect();
            if files.is_empty() {
                info!("Held {key} has no file left to keep; it ends");
                self.end_kept(record);
                return;
            }
            let (completion, joined) = self.direct_queue.enqueue_keeping(
                origin,
                gates.clone(),
                files,
                &target,
                &record.provider,
                &record.external_id,
                Some(DownloadSource::Soulseek),
                requested_by.as_deref(),
            );
            match completion.wait().await {
                Ok(path) => {
                    info!("Held {key} is kept anyway: {path}");
                    self.end_kept(record);
                    return;
                }
                // It joined a download already running for the song, which is not a keep: the
                // keep runs again on its own.
                Err(failure) if joined && attempt == 0 => {
                    debug!("The keep of {key} joined another download ({failure}); it runs again");
                }
                Err(failure) => {
                    warn!("The keep of {key} could not land its file: {failure}");
                    break;
                }
            }
        }
        // Not kept: the row goes back to the line it waits with, offering the keep again.
        if let Some(still) = holds.update(&key, |kept| kept.keep_requested.clear()) {
            self.hold_row_for(&still);
        }
    }

    /// A hold whose file was kept anyway ends, and an album track's album row is counted again. The
    /// song's row lets go, or is held again for another hold of the song still waiting.
    fn end_kept(&self, record: &HeldAcquisition) {
        if record.album_track.is_some() {
            self.end_album_track(record);
        }
        let Some(holds) = self.extras.holds.clone() else {
            return;
        };
        holds.release(&record.key());
        for holder in &record.holders {
            self.let_go_of_copies(&record.provider, &record.external_id, holder);
        }
        let song = track_base_key(&record.provider, &record.external_id);
        match holds.records_for(&song).into_iter().next() {
            Some(still) => self.hold_row_for(&still),
            None => {
                if let Some(tracker) = self.tracker() {
                    tracker.release_row(&record.provider, &record.external_id, &holds);
                }
            }
        }
    }

    /// `user` keeps their own heard audio of a held song: the whole copy of the very stream that
    /// played (or the very copy fetched ahead that played), nothing else. Kept, they leave the
    /// hold and the others stay; with YouTube not answering for the heard video, the request waits
    /// in a hold of its own for the shim's rounds; with the video gone, their heart ends.
    async fn keep_heard_audio(&self, record: &HeldAcquisition, user: &str) {
        let Some(holds) = self.extras.holds.clone() else {
            return;
        };
        let key = record.key();
        let Some(holder) = record
            .holders
            .iter()
            .find(|holder| eq_ignore_case(holder.user.trim(), user.trim()))
            .cloned()
        else {
            holds.update(&key, |kept| {
                kept.keep_requested
                    .retain(|asker| !eq_ignore_case(asker.trim(), user.trim()))
            });
            return;
        };
        if holder.heard.is_none() {
            info!("{user} asked to keep {key}, and nothing they heard of it is known");
            self.heard_gone(record, &holder);
            return;
        }
        let origin = record.origin();
        let target = self
            .import_target(&holder.user)
            .unwrap_or(AcquisitionTarget::Shared);
        let gate = HeartGate {
            user: holder.user.trim().to_string(),
            origin,
            r: record.album_track.as_ref().map(|track| track.r.clone()),
            heard: holder.heard.clone(),
            heard_seconds: None,
            reading: HeardReading::OnlyHeardOrMaster,
            stand_in: None,
        };
        for attempt in 0..2 {
            let (completion, joined) = self.direct_queue.enqueue_keeping(
                origin,
                vec![gate.clone()],
                Vec::new(),
                &target,
                &record.provider,
                &record.external_id,
                Some(DownloadSource::YouTube),
                Some(holder.user.trim()),
            );
            match completion.wait().await {
                Ok(path) => {
                    self.kept_heard(record, &holder, &path);
                    return;
                }
                Err(failure)
                    if failure.downcast_ref::<AcquisitionHeld>().is_some_and(|held| {
                        held.waiting_for == WaitingFor::Shim && held.line == KEEP_WAITS_FOR_YOUTUBE
                    }) =>
                {
                    self.keep_waits_for_youtube(record, &holder);
                    return;
                }
                Err(failure) if joined && attempt == 0 => {
                    debug!("The keep of {key} for {user} joined another download ({failure}); it runs again");
                }
                Err(failure) => {
                    info!("The audio {user} heard of {key} cannot be had: {failure}");
                    self.heard_gone(record, &holder);
                    return;
                }
            }
        }
    }

    /// `holder`'s heard audio was kept: they leave the hold, which stays for whoever else waits in
    /// it, its row held again for them.
    fn kept_heard(&self, record: &HeldAcquisition, holder: &Holder, path: &str) {
        info!(
            "{} kept the version they heard of {}: {path}",
            holder.user,
            record.key()
        );
        self.leave_after_keep(record, holder, None);
    }

    /// `holder` leaves the hold their keep was asked in; when nobody is left it ends, and an album
    /// track's album row is counted again. While another hold of the song waits (this one, or one
    /// for something else), the song's row is held again for it, with `ended` in its log; once
    /// nothing of the song is held, the row says `ended`.
    fn leave_after_keep(&self, record: &HeldAcquisition, holder: &Holder, ended: Option<&str>) {
        let Some(holds) = self.extras.holds.clone() else {
            return;
        };
        let left = holds.release_holder(&record.key(), &holder.user);
        self.let_go_of_copies(&record.provider, &record.external_id, holder);
        if left.is_none() && record.album_track.is_some() {
            self.end_album_track(record);
        }
        let song = track_base_key(&record.provider, &record.external_id);
        let waiting = left.or_else(|| holds.records_for(&song).into_iter().next());
        let Some(tracker) = self.tracker() else {
            return;
        };
        match waiting {
            Some(still) => {
                self.hold_row_for(&still);
                if let Some(line) = ended {
                    tracker.log(
                        &record.provider,
                        &record.external_id,
                        crate::services::common::acquisition_tracker::acquisition_event_kinds::NOTE,
                        line,
                        None,
                    );
                }
            }
            None => {
                tracker.release_row(&record.provider, &record.external_id, &holds);
                if let Some(line) = ended {
                    tracker.fail(&record.provider, &record.external_id, Some(line));
                }
            }
        }
    }

    /// The heard video does not answer now: `holder` waits in a hold for the shim with their keep
    /// saved there, so the round that finds the video answering keeps it.
    fn keep_waits_for_youtube(&self, record: &HeldAcquisition, holder: &Holder) {
        let Some(holds) = self.extras.holds.clone() else {
            return;
        };
        let user = holder.user.trim();
        info!(
            "YouTube does not answer for what {user} heard of {}; the keep waits for it",
            record.key()
        );
        self.file_hold(
            &record.provider,
            &record.external_id,
            Some(user),
            record.origin(),
            WaitingFor::Shim,
            vec![holder.clone()],
            KEEP_WAITS_FOR_YOUTUBE,
            None,
            None,
            &mut Vec::new(),
        );
        let waiting = HeldAcquisition {
            kind: HeldKind::Track,
            provider: record.provider.clone(),
            external_id: record.external_id.clone(),
            waiting_for: Some(WaitingFor::Shim),
            ..Default::default()
        }
        .key();
        let filed = holds.update(&waiting, |kept| {
            if !asked(kept, user) {
                kept.keep_requested.push(user.to_string());
            }
        });
        if filed.is_none() {
            // Nothing of this origin waits for the shim: the keep is let go, and the row offers it
            // again.
            holds.update(&record.key(), |kept| {
                kept.keep_requested
                    .retain(|asker| !eq_ignore_case(asker.trim(), user))
            });
            return;
        }
        if waiting != record.key() && holds.release_holder(&record.key(), user).is_none() {
            debug!("{} ends: its last holder waits for the shim", record.key());
        }
    }

    /// The audio `holder` heard cannot be had: their heart of the song ends with the plain line.
    fn heard_gone(&self, record: &HeldAcquisition, holder: &Holder) {
        info!(
            "The version {} heard of {} is gone; their heart ends",
            holder.user,
            record.key()
        );
        self.leave_after_keep(record, holder, Some(HEARD_GONE));
    }
}

#[cfg(test)]
#[path = "heart_keep_anyway_tests.rs"]
mod tests;
