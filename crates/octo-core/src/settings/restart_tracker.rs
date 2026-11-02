//! `Octo.Services.Admin.RestartTracker`.

use std::collections::HashMap;

use super::store::RawConfig;

/// Which saved settings have not yet reached the services that read them once, at construction.
///
/// Snapshotted when the app starts and compared on every settings read, so the dashboard can say
/// "restart to apply" for exactly as long as it is true, and stop saying it after a restart,
/// without the browser keeping its own copy of what was saved. A service built lazily after a
/// change does see the new value, so this can over-report; that is the honest direction. Under-
/// reporting is the bug the dashboard's restart markers existed to prevent.
#[derive(Debug, Clone)]
pub struct RestartTracker {
    at_startup: HashMap<&'static str, Option<String>>,
}

impl RestartTracker {
    /// Keys whose consumers capture the value when they are constructed. Keep in step with the
    /// dashboard's data-restart markers, which name the same settings as "Section.Key".
    pub const KEYS: [&'static str; 16] = [
        "Library:DownloadPath", // BaseDownloadService, LocalLibraryService, PlaylistSyncService
        "YouTube:ShimUrl",      // YouTubeResolver
        "Subsonic:Url",         // live in most places; a restart is still the safe advice
        "Subsonic:LibraryPath", //
        "Subsonic:WaitForLosslessOnPlay", // SubsonicResponseBuilder, deliberately
        "Soulseek:Enabled",     // SoulseekClient and the Soulseek link read it at startup
        "Soulseek:BaseUrl",     // SoulseekClient and SoulseekDownloadService read it at startup
        "Soulseek:Username",
        "Soulseek:Password",
        "Soulseek:SearchWaitSeconds",
        "Soulseek:DownloadTimeoutSeconds",
        "Soulseek:MinFileSizeBytes",
        "Soulseek:PreferredExtension",
        "LibraryActions:Enabled", // the playlist worker decides once, at start
        "LibraryActions:PlaylistsEnabled",
        "LibraryActions:PollIntervalSeconds",
    ];

    pub fn new(configuration: &impl RawConfig) -> Self {
        Self {
            at_startup: Self::KEYS
                .iter()
                .map(|key| (*key, normalize(configuration.raw(key).as_deref())))
                .collect(),
        }
    }

    /// The keys, as "Section:Key", whose current value differs from the one at startup.
    pub fn pending(&self, configuration: &impl RawConfig) -> Vec<String> {
        Self::KEYS
            .iter()
            .filter(|key| {
                let now = normalize(configuration.raw(key).as_deref());
                let then = self.at_startup.get(*key).cloned().flatten();
                !same(now.as_deref(), then.as_deref())
            })
            .map(|key| key.to_string())
            .collect()
    }
}

// Empty and missing mean the same thing to every consumer here.
fn normalize(value: Option<&str>) -> Option<String> {
    value.map(str::trim).filter(|v| !v.is_empty()).map(str::to_string)
}

/// `bool.TryParse`: `true` or `false` in any case (the value is already trimmed).
fn parse_bool(value: Option<&str>) -> Option<bool> {
    match value {
        Some(v) if v.eq_ignore_ascii_case("true") => Some(true),
        Some(v) if v.eq_ignore_ascii_case("false") => Some(false),
        _ => None,
    }
}

// A bool can arrive as "True" from an env var and "true" from the file. Anything else is
// compared exactly: a password or a Linux path that changes only in case has changed.
fn same(now: Option<&str>, at_startup: Option<&str>) -> bool {
    match (parse_bool(now), parse_bool(at_startup)) {
        (Some(a), Some(b)) => a == b,
        _ => now == at_startup,
    }
}

#[cfg(test)]
mod tests {
    //! The dashboard's "restart to apply" banner is computed from this, so it has to name a
    //! changed startup-only setting and nothing else.

    use super::*;
    use crate::config::ConfigTree;

    fn config(values: &[(&str, &str)]) -> ConfigTree {
        let mut tree = ConfigTree::new();
        for (k, v) in values {
            tree.set(k, Some(v.to_string()));
        }
        tree
    }

    #[test]
    fn pending_is_empty_at_startup() {
        let config = config(&[
            ("Soulseek:Password", "secret"),
            ("Library:DownloadPath", "/music"),
        ]);
        assert!(RestartTracker::new(&config).pending(&config).is_empty());
    }

    #[test]
    fn pending_names_a_changed_key() {
        let mut config = config(&[("Soulseek:Password", "secret")]);
        let tracker = RestartTracker::new(&config);

        config.set("Soulseek:Password", Some("changed".into()));

        assert_eq!(tracker.pending(&config), ["Soulseek:Password"]);
    }

    /// Switching Soulseek on or off reaches slskd's client only at startup.
    #[test]
    fn pending_names_soulseek_switched_on_or_off() {
        let mut config = config(&[("Soulseek:Enabled", "true")]);
        let tracker = RestartTracker::new(&config);

        config.set("Soulseek:Enabled", Some("false".into()));

        assert_eq!(tracker.pending(&config), ["Soulseek:Enabled"]);
    }

    /// A hot-reload setting is not a restart setting, however it changes.
    #[test]
    fn pending_ignores_settings_that_apply_live() {
        let mut config = config(&[("Genre:MaxGenres", "10")]);
        let tracker = RestartTracker::new(&config);

        config.set("Genre:MaxGenres", Some("1".into()));

        assert!(tracker.pending(&config).is_empty());
    }

    #[test]
    fn pending_ignores_boolean_casing() {
        let mut config = config(&[("LibraryActions:Enabled", "True")]);
        let tracker = RestartTracker::new(&config);

        config.set("LibraryActions:Enabled", Some("true".into()));

        assert!(tracker.pending(&config).is_empty());
    }

    /// Only booleans compare without case. A password or a Linux path that changes only
    /// in case has changed, and still needs the restart.
    #[test]
    fn pending_names_a_case_only_change_to_a_string() {
        let mut config = config(&[("Soulseek:Password", "secret")]);
        let tracker = RestartTracker::new(&config);

        config.set("Soulseek:Password", Some("Secret".into()));

        assert_eq!(tracker.pending(&config), ["Soulseek:Password"]);
    }

    #[test]
    fn pending_treats_empty_and_missing_alike() {
        let mut config = config(&[]);
        let tracker = RestartTracker::new(&config);

        config.set("YouTube:ShimUrl", Some("  ".into()));

        assert!(tracker.pending(&config).is_empty());
    }
}
