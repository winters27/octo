//! `Octo.Models.Settings.LibraryActionSettings`, `LibraryActionDefinition` and their enums
//! (the `LibraryActions` section).

use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::text::{IgnoreCaseSet, eq_ignore_case, utf16_len};

/// New actions are appended, never inserted: the journal stores actions as numbers, so moving one
/// would change what every entry already written means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Deserialize, Serialize)]
pub enum LibraryAction {
    #[default]
    Delete,
    WrongSong,
    WrongVersion,
    BetterQuality,
    Keep,
    /// The song is right but its lyrics are not: show the next source's, never these again.
    WrongLyrics,
    /// The song is right but its album's cover is not: put in another, never this one again.
    WrongCover,
    /// The song is one of several copies: fix them all as Library health does, keeping every
    /// copy anyone starred or rated while Keep every rated copy is on.
    FixDuplicates,
    /// Check now that the song's file is the version its tags say; what is found is listed in
    /// Library health and on the action's playlist. Changes no file.
    CheckVersion,
}

impl LibraryAction {
    /// Every action, in declaration order.
    pub const ALL: [LibraryAction; 9] = [
        LibraryAction::Delete,
        LibraryAction::WrongSong,
        LibraryAction::WrongVersion,
        LibraryAction::BetterQuality,
        LibraryAction::Keep,
        LibraryAction::WrongLyrics,
        LibraryAction::WrongCover,
        LibraryAction::FixDuplicates,
        LibraryAction::CheckVersion,
    ];

    /// Whether the action answers about the song's lyrics or cover, leaving its audio alone.
    pub fn leaves_the_audio(self) -> bool {
        matches!(self, LibraryAction::WrongLyrics | LibraryAction::WrongCover)
    }

    /// Whether a definition still as the defaults have it is left out of the settings file: a
    /// build that does not know the action refuses a file naming it, so it is written only once
    /// someone changes it.
    pub fn written_once_changed(self) -> bool {
        self.leaves_the_audio() || self == LibraryAction::CheckVersion
    }

    /// The number the journal stores (`(int)action`).
    pub fn as_index(self) -> i32 {
        self as i32
    }

    /// The action a stored number names, or None for a number no action has.
    pub fn from_index(index: i32) -> Option<LibraryAction> {
        usize::try_from(index)
            .ok()
            .and_then(|i| Self::ALL.get(i).copied())
    }

    /// The built-in playlist name.
    pub fn default_name(self) -> &'static str {
        self.defaults().0
    }

    /// The built-in star rating; 0 is none.
    pub fn default_rating(self) -> i32 {
        self.defaults().1
    }

    /// The built-in playlist name and star rating.
    fn defaults(self) -> (&'static str, i32) {
        match self {
            LibraryAction::Delete => ("Delete", 1),
            LibraryAction::WrongSong => ("Wrong song", 2),
            LibraryAction::WrongVersion => ("Wrong version", 3),
            LibraryAction::BetterQuality => ("Better quality", 4),
            LibraryAction::Keep => ("Keep", 5),
            // No star by default: a rating someone already gave must not start asking for these.
            LibraryAction::WrongLyrics => ("Wrong lyrics", 0),
            LibraryAction::WrongCover => ("Wrong cover", 0),
            // No star by default either: every star already asks for something, and this one
            // removes files.
            LibraryAction::FixDuplicates => ("Fix duplicates", 0),
            // No star either: a check changes nothing, and every star already asks for something.
            LibraryAction::CheckVersion => ("Check version", 0),
        }
    }
}

/// How a user asks for an action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Deserialize, Serialize)]
pub enum LibraryActionTrigger {
    #[default]
    Playlists,
    Ratings,
}

/// Where a star rating counts as a command. Auto means only on a track in one of Octo's notice
/// playlists when Review or Duplicates is on, and anywhere otherwise, which is exactly how
/// ratings behaved before this setting existed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Deserialize, Serialize)]
pub enum LibraryRatingScope {
    #[default]
    Auto,
    NoticeOnly,
    Global,
}

/// The playlists Octo fills to ask a person something (#47, #53).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Deserialize, Serialize)]
pub enum NoticeKind {
    #[default]
    Review,
    Duplicates,
}

/// Where Better quality looks for a lossless copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Deserialize, Serialize)]
pub enum UpgradeSourceChoice {
    /// Every source that is set up: Soulseek first, then Lidarr for what Soulseek cannot find.
    #[default]
    Auto,
    Soulseek,
    Lidarr,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct LibraryActionDefinition {
    pub action: LibraryAction,

    /// The playlist name, after the prefix. Editable so it reads the way the user
    /// thinks about it rather than the way Octo names it internally.
    pub name: String,

    pub enabled: bool,

    /// Which star count maps to this action, 1-5, when ratings are on.
    ///
    /// Optional so an omitted value and a deliberate 0 mean different things: unset takes the
    /// built-in mapping, 0 means no rating ever triggers this action. Without that distinction
    /// a config that simply does not mention ratings would silently unmap every action, which
    /// is a setting doing something the user never asked for.
    pub rating: Option<i32>,
}

/// Fixing a wrong download from the player you are already using, rather than the dashboard.
///
/// Every part of this is configurable on purpose: which actions exist at all, what they are
/// called, who may trigger them, how they are triggered, whether anything is written on the
/// first run, where deleted files go and how long they stay. The defaults are the cautious
/// reading of every one of those, because the feature deletes files Octo did not create.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct LibraryActionSettings {
    /// Master switch. Off by default: these actions delete files Octo did not create.
    /// Environment variable: LIBRARY_ACTIONS_ENABLED
    pub enabled: bool,

    /// Option A. Octo keeps a playlist per enabled action; adding a track to one requests it.
    /// Works with every client, and the intent is unambiguous because the playlist says so.
    /// Environment variable: LIBRARY_ACTIONS_PLAYLISTS
    pub playlists_enabled: bool,

    /// Option B. Map star ratings onto the same actions.
    ///
    /// OFF by default and it should stay that way for most people: a star is a one-tap gesture
    /// with no confirmation anywhere in any client, and rating a track you dislike is a
    /// perfectly ordinary thing to do. It is configurable rather than absent because a user
    /// who never rates music may genuinely prefer it.
    /// Environment variable: LIBRARY_ACTIONS_RATINGS
    pub ratings_enabled: bool,

    /// Prefix on every action playlist, so they sort together. Set it to "" for no prefix.
    /// Environment variable: LIBRARY_ACTIONS_PREFIX
    pub playlist_prefix: String,

    /// Per-action names, enablement and star mapping. Managed in the dashboard or settings
    /// JSON rather than .env, the same call the pinned radio stations make.
    pub actions: Vec<LibraryActionDefinition>,

    /// Navidrome usernames allowed to trigger actions. EMPTY MEANS NOBODY, never everybody:
    /// the fail-open reading of an empty allowlist on a feature that deletes files is not a
    /// defensible default.
    pub allowed_users: Vec<String>,

    /// Do everything except touch a file. On by default so the first run of a newly enabled
    /// install is a rehearsal the operator reads before it is real.
    /// Environment variable: LIBRARY_ACTIONS_DRY_RUN
    pub dry_run: bool,

    /// Where a removed file goes, relative to the music root. The leading dot keeps it out of
    /// Navidrome's scan. Nothing here is ever a File.Delete except the retention sweep.
    /// Environment variable: LIBRARY_ACTIONS_TRASH_DIR
    pub quarantine_directory: String,

    /// How long a quarantined file is kept before it is really deleted. 0 keeps them forever,
    /// which is the right choice for anyone who would rather manage the space by hand.
    /// Environment variable: LIBRARY_ACTIONS_TRASH_DAYS
    pub quarantine_retention_days: i32,

    /// How often the action playlists are checked. Fast enough to feel responsive on a gesture
    /// the user performs and then watches for, slow enough that an idle install is quiet.
    /// Environment variable: LIBRARY_ACTIONS_POLL_SECONDS
    pub poll_interval_seconds: i32,

    /// Ceiling on actions applied per sweep. A throttle, not a stop: a client that dumps five
    /// thousand tracks into an action playlist gets this many a minute, which buys hours of
    /// reaction time.
    /// Environment variable: LIBRARY_ACTIONS_MAX_PER_CYCLE
    pub max_actions_per_cycle: i32,

    /// Keep the quarantined original after a replacement is verified, rather than relying on
    /// the retention sweep. Off means the retention window is the only safety net.
    /// Environment variable: LIBRARY_ACTIONS_KEEP_REPLACED
    pub keep_replaced_originals: bool,

    /// Prefix on the playlists Octo fills, so "Octo is telling me something" sorts apart from the
    /// action playlists, where the user tells Octo something (#47).
    /// Environment variable: LIBRARY_ACTIONS_NOTICE_PREFIX
    pub notice_prefix: String,

    /// A "Review" playlist per allowed user, filled with downloads a person can settle by
    /// listening: AcoustID had never heard the recording, was not sure, or (for YouTube) thought it
    /// was something else. Empty it by moving tracks into the action playlists, by Keep, or by
    /// removing a track, which counts as an answer too.
    /// Environment variable: LIBRARY_ACTIONS_REVIEW
    pub review_enabled: bool,

    /// The Review playlist's name, after the notice prefix.
    pub review_playlist_name: String,

    /// How many songs already in the library the Review playlist checks an hour (#72). 0, the
    /// default, is off. Downloads are checked as they arrive; this asks about music that was
    /// already there, and only while nothing is downloading.
    /// Environment variable: LIBRARY_ACTIONS_REVIEW_SWEEP_PER_HOUR
    pub review_sweep_per_hour: i32,

    /// Check Octo's own downloads too. Off by default: those were checked when they arrived, as
    /// long as download verification was on then.
    /// Environment variable: LIBRARY_ACTIONS_REVIEW_SWEEP_OCTO_DOWNLOADS
    pub review_sweep_octo_downloads: bool,

    /// Let the sweep replace a file that clearly is another song, through Wrong song, instead
    /// of asking. Off by default. Only a confident AcoustID match, at or above the download
    /// threshold, to a recording whose title is not the file's own title counts; another take,
    /// a wrong length and anything AcoustID was unsure of still ask in Review. Respects DryRun
    /// and the allowlist, and runs as the person the sweep asks.
    /// Environment variable: LIBRARY_ACTIONS_REVIEW_SWEEP_AUTO_REPLACE
    pub review_sweep_auto_replace: bool,

    /// Most files the sweep replaces by itself in a UTC day (1 to 100, default 10). Past it, or
    /// after three replacements fail in a day, the rest of the day's matches are asked about.
    /// Environment variable: LIBRARY_ACTIONS_REVIEW_SWEEP_AUTO_REPLACE_PER_DAY
    pub review_sweep_auto_replace_per_day: i32,

    /// Let the sweep also replace a song AcoustID has never heard when its YouTube preview, one
    /// with the song's title, artist and length, sounds nothing like the file, through Wrong song,
    /// instead of asking. Off by default. The replacement is checked like any download, against
    /// the same preview. Counts toward the same daily limit and breaker as the replacing above,
    /// and needs the same: DryRun off, Wrong song on and the keeper on the allowlist. A song with
    /// an ISRC tag, or that anyone starred or rated, is still asked about.
    /// Environment variable: LIBRARY_ACTIONS_REVIEW_SWEEP_AUTO_REPLACE_PREVIEW
    pub review_sweep_auto_replace_preview: bool,
    /// How many library songs an hour have their tags checked against their audio: each is
    /// fingerprinted in AcoustID's spare lane, and what disagrees shows in Library health under
    /// "Tags that disagree with the audio". 0 is off. Only while nothing is downloading. Stray
    /// spaces are taken off by itself when library actions may change files; everything else
    /// waits for a person.
    /// Environment variable: LIBRARY_ACTIONS_TAG_CHECK_PER_HOUR
    pub tag_check_per_hour: i32,

    /// A "Duplicates" playlist per allowed user: recordings the library holds more than once,
    /// side by side, the copy worth keeping first. Octo only points them out; nothing is removed
    /// unless you remove it (#53).
    /// Environment variable: LIBRARY_ACTIONS_DUPLICATES
    pub duplicates_enabled: bool,

    /// The Duplicates playlist's name, after the notice prefix.
    pub duplicates_playlist_name: String,

    /// How often the library is walked for duplicates.
    /// Environment variable: LIBRARY_ACTIONS_DUPLICATES_SCAN_HOURS
    pub duplicates_scan_hours: i32,

    /// Most tracks a notice playlist holds at once. A newly enabled install with a large library
    /// would otherwise get a wall, not a queue.
    /// Environment variable: LIBRARY_ACTIONS_NOTICE_MAX
    pub notice_max_tracks: i32,

    /// Where a star rating counts as a command. NoticeOnly: only on a track in one of Octo's
    /// notice playlists, where the only reason to rate it is to answer. Global: any track. Auto
    /// (the default) is NoticeOnly while Review or Duplicates is on, Global otherwise.
    /// Environment variable: LIBRARY_ACTIONS_RATINGS_SCOPE
    pub ratings_scope: LibraryRatingScope,

    /// Songs a week Octo tries to upgrade to lossless by itself, spread evenly across the week, one
    /// at a time, through Better quality. 0 is off. Needs Better quality switched on, and runs as
    /// the first person on AllowedUsers.
    /// Environment variable: LIBRARY_ACTIONS_UPGRADE_PER_WEEK
    pub upgrade_per_week: i32,

    /// Where Better quality looks for a lossless copy (default: Auto). Auto uses every source that
    /// is set up, Soulseek first and then Lidarr for what Soulseek cannot find; Soulseek or Lidarr
    /// uses only that one.
    /// Environment variable: LIBRARY_ACTIONS_UPGRADE_SOURCE
    pub upgrade_source: UpgradeSourceChoice,

    /// When Library health fixes duplicates, every copy anyone starred or rated stays, and only
    /// copies nobody starred or rated go to the trash. Off by default: the fix keeps one copy of
    /// each song. Set from the switch on the dashboard's Library health page.
    pub keep_rated_copies: bool,
}

impl Default for LibraryActionSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            playlists_enabled: true,
            ratings_enabled: false,
            playlist_prefix: "\u{1F6E0} ".to_string(),
            actions: Vec::new(),
            allowed_users: Vec::new(),
            dry_run: true,
            quarantine_directory: ".octo-trash".to_string(),
            quarantine_retention_days: 30,
            poll_interval_seconds: 60,
            max_actions_per_cycle: 20,
            keep_replaced_originals: true,
            notice_prefix: "\u{25B8} ".to_string(),
            review_enabled: false,
            review_playlist_name: "Review".to_string(),
            review_sweep_per_hour: 0,
            review_sweep_octo_downloads: false,
            review_sweep_auto_replace: false,
            review_sweep_auto_replace_per_day: 10,
            review_sweep_auto_replace_preview: false,
            tag_check_per_hour: 60,
            duplicates_enabled: false,
            duplicates_playlist_name: "Duplicates".to_string(),
            duplicates_scan_hours: 24,
            notice_max_tracks: 100,
            ratings_scope: LibraryRatingScope::Auto,
            upgrade_per_week: 0,
            upgrade_source: UpgradeSourceChoice::Auto,
            keep_rated_copies: false,
        }
    }
}

impl LibraryActionSettings {
    pub fn effective_upgrade_per_week(&self) -> i32 {
        self.upgrade_per_week.clamp(0, 500)
    }

    pub fn effective_notice_max_tracks(&self) -> i32 {
        self.notice_max_tracks.clamp(1, 500)
    }

    /// 0 is off. The ceiling is one file every ten seconds: the mount and the decoder are
    /// the limit, not AcoustID, and a gap that long keeps a download's lookup from ever queueing
    /// behind more than one of the sweep's.
    pub fn effective_review_sweep_per_hour(&self) -> i32 {
        if self.review_sweep_per_hour <= 0 {
            0
        } else {
            self.review_sweep_per_hour.min(360)
        }
    }

    pub fn effective_review_sweep_auto_replace_per_day(&self) -> i32 {
        self.review_sweep_auto_replace_per_day.clamp(1, 100)
    }

    /// 0 is off; the same ceiling as the Review sweep, one file every ten seconds.
    pub fn effective_tag_check_per_hour(&self) -> i32 {
        self.tag_check_per_hour.clamp(0, 360)
    }

    pub fn effective_duplicates_scan_interval(&self) -> Duration {
        Duration::from_secs(self.duplicates_scan_hours.clamp(1, 168) as u64 * 3600)
    }

    pub fn notices_enabled(&self) -> bool {
        self.review_enabled || self.duplicates_enabled
    }

    pub fn effective_ratings_scope(&self) -> LibraryRatingScope {
        if self.ratings_scope != LibraryRatingScope::Auto {
            self.ratings_scope
        } else if self.notices_enabled() {
            LibraryRatingScope::NoticeOnly
        } else {
            LibraryRatingScope::Global
        }
    }

    pub fn enabled_notice_kinds(&self) -> Vec<NoticeKind> {
        let mut kinds = Vec::new();
        if self.review_enabled {
            kinds.push(NoticeKind::Review);
        }
        if self.duplicates_enabled {
            kinds.push(NoticeKind::Duplicates);
        }
        kinds
    }

    pub fn notice_title(&self, kind: NoticeKind) -> String {
        let name = match kind {
            NoticeKind::Review => named_or(&self.review_playlist_name, "Review"),
            NoticeKind::Duplicates => named_or(&self.duplicates_playlist_name, "Duplicates"),
        };
        format!("{}{}", self.notice_prefix, name)
    }

    pub fn effective_poll_interval(&self) -> Duration {
        Duration::from_secs(self.poll_interval_seconds.clamp(15, 3600) as u64)
    }

    pub fn effective_max_actions_per_cycle(&self) -> i32 {
        self.max_actions_per_cycle.clamp(1, 200)
    }

    /// 0 is a real choice here, meaning "never sweep", so it is not clamped upward.
    pub fn effective_quarantine_retention_days(&self) -> i32 {
        if self.quarantine_retention_days <= 0 {
            0
        } else {
            self.quarantine_retention_days.clamp(1, 3650)
        }
    }

    pub fn effective_prefix(&self) -> &str {
        &self.playlist_prefix
    }

    /// Sanitized, so a directory traversal cannot be typed into a settings field. One path
    /// segment, and a leading dot is preserved because it is what keeps the folder out of
    /// Navidrome's scan.
    pub fn effective_quarantine_directory(&self) -> String {
        let value = self.quarantine_directory.trim().trim_matches(['/', '\\']);
        if value.is_empty() || value == "." || value == ".." || value.contains("..") {
            return ".octo-trash".to_string();
        }
        value.split(['/', '\\']).next().unwrap_or(value).to_string()
    }

    /// Every action, back-filled and de-duplicated.
    ///
    /// Back-filling matters: disabling one action must not delete the names chosen for the
    /// others, and a config naming only two actions must not leave the other two undefined. A
    /// name colliding with another action's is dropped, because two playlists with the same
    /// effective title are indistinguishable to the sweep and one would apply the wrong action.
    pub fn effective_actions(&self) -> Vec<LibraryActionDefinition> {
        let mut seen_titles = IgnoreCaseSet::new();
        let mut seen_ratings: Vec<i32> = Vec::new();
        let mut result = Vec::new();

        for action in LibraryAction::ALL {
            let configured = self.actions.iter().find(|entry| entry.action == action);
            let (default_name, default_rating) = action.defaults();

            let mut name = configured.map(|c| c.name.trim()).unwrap_or("").to_string();
            let name_len = utf16_len(&name);
            if name_len == 0 || name_len > 80 {
                name = default_name.to_string();
            }
            if !seen_titles.insert(format!("{}{}", self.effective_prefix(), name)) {
                continue;
            }

            let mut rating = configured.and_then(|c| c.rating).unwrap_or(default_rating);
            if !(0..=5).contains(&rating) || seen_ratings.contains(&rating) {
                rating = 0;
            } else {
                seen_ratings.push(rating);
            }
            // seenRatings must not collapse every unmapped action onto one another, so 0 is
            // never treated as a collision.
            seen_ratings.retain(|r| *r != 0);

            result.push(LibraryActionDefinition {
                action,
                name,
                // Keep removes nothing, so it comes on by itself with the playlists it answers.
                enabled: configured
                    .map(|c| c.enabled)
                    .unwrap_or(action == LibraryAction::Keep && self.notices_enabled()),
                rating: Some(rating),
            });
        }
        result
    }

    pub fn playlist_title(&self, definition: &LibraryActionDefinition) -> String {
        format!("{}{}", self.effective_prefix(), definition.name)
    }

    /// The action a star count asks for, or None. 5 is Keep by default, which removes
    /// nothing, so the top of the scale is never destructive.
    pub fn action_for_rating(&self, rating: i32) -> Option<LibraryActionDefinition> {
        if !(1..=5).contains(&rating) {
            return None;
        }
        self.effective_actions()
            .into_iter()
            .find(|entry| entry.enabled && entry.rating == Some(rating))
    }

    /// The star ratings that currently do something, for the dashboard to show.
    pub fn mapped_ratings(&self) -> Vec<i32> {
        let mut ratings: Vec<i32> = self
            .effective_actions()
            .into_iter()
            .filter(|entry| entry.enabled)
            .filter_map(|entry| entry.rating.filter(|r| *r > 0))
            .collect();
        ratings.sort_unstable();
        ratings
    }

    /// Per-user gate. Ordinal-ignore-case because Navidrome usernames are. An empty list is
    /// nobody.
    pub fn is_allowed(&self, username: Option<&str>) -> bool {
        let Some(username) = username.filter(|u| !u.trim().is_empty()) else {
            return false;
        };
        self.allowed_users
            .iter()
            .any(|user| eq_ignore_case(user.trim(), username.trim()))
    }
}

fn named_or<'a>(name: &'a str, fallback: &'a str) -> &'a str {
    if name.trim().is_empty() {
        fallback
    } else {
        name.trim()
    }
}

#[cfg(test)]
#[path = "library_action_tests.rs"]
mod tests;
