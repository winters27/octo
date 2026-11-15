//! One provisioner per request, over that request's proxy, so its once-per-boot set is once per
//! request in practice. A caller builds it with `new(request's proxy, settings)`.
//!
//! Requests can overlap (an app listing playlists twice as it starts, two devices at once), and
//! each sees the same playlist missing. [`PENDING`] is shared by all of them, so a title is
//! created by whichever request claims it first.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use indexmap::IndexMap;
use octo_core::common::dotnet;
use octo_core::settings::SettingsStore;
use parking_lot::Mutex;
use tracing::{info, warn};

use octo_core::family::abilities::{AddToLibrary, RequestQuality};

use crate::services::family::FamilyStore;
use crate::services::subsonic::SubsonicProxyService;

/// Titles being created, or created within [`HOLD`], keyed by Navidrome address, user and title,
/// folded for case. It stays a few titles per user; stale entries are swept once it grows.
static PENDING: LazyLock<Mutex<HashMap<String, Instant>>> = LazyLock::new(Default::default);

/// How long a created title stays claimed: longer than a listing relayed before it existed takes
/// to reach [`LibraryActionPlaylistProvisioner::missing`].
const HOLD: Duration = Duration::from_secs(120);

fn pending_key(server: &str, username: &str, title: &str) -> String {
    let mut key = dotnet::ordinal_ignore_case_key(server.trim());
    key.push('\n');
    key.push_str(&dotnet::ordinal_ignore_case_key(username.trim()));
    key.push('\n');
    key.push_str(&dotnet::ordinal_ignore_case_key(title));
    key
}

/// Claims `title` for this request; false when another request holds it.
fn claim(server: &str, username: &str, title: &str) -> bool {
    let now = Instant::now();
    let mut pending = PENDING.lock();
    if pending.len() > 64 {
        pending.retain(|_, at| now.duration_since(*at) < HOLD);
    }
    match pending.entry(pending_key(server, username, title)) {
        Entry::Occupied(held) if now.duration_since(*held.get()) < HOLD => false,
        Entry::Occupied(mut stale) => {
            stale.insert(now);
            true
        }
        Entry::Vacant(free) => {
            free.insert(now);
            true
        }
    }
}

/// Gives a title back after its create failed, so the next listing tries again.
fn release(server: &str, username: &str, title: &str) {
    PENDING.lock().remove(&pending_key(server, username, title));
}

/// Forgets every claim on `server`, so a test's fresh Navidrome starts with none.
#[cfg(test)]
pub(crate) fn forget_claims(server: &str) {
    let prefix = {
        let mut key = dotnet::ordinal_ignore_case_key(server.trim());
        key.push('\n');
        key
    };
    PENDING.lock().retain(|key, _| !key.starts_with(&prefix));
}

/// Makes sure an allowed user has a playlist for each enabled action.
///
/// Created PER USER with THEIR credentials, so the playlists are theirs and private. An
/// admin-owned public playlist would be visible and writable to every account on the server,
/// which would hand the delete button to people who are not on the allowlist. A private
/// playlist per user is exactly the right blast radius.
pub struct LibraryActionPlaylistProvisioner {
    /// Usernames already provisioned by this instance (keys compared ignoring case), so this runs
    /// once per user rather than on every playlist listing.
    done: Mutex<HashSet<String>>,
    proxy: SubsonicProxyService,
    /// The library action settings, read at every call.
    settings: Arc<SettingsStore>,
    /// The family's members, for their own action playlists.
    family: Option<Arc<FamilyStore>>,
}

impl LibraryActionPlaylistProvisioner {
    pub fn new(proxy: SubsonicProxyService, settings: Arc<SettingsStore>) -> Self {
        LibraryActionPlaylistProvisioner {
            done: Mutex::new(HashSet::new()),
            proxy,
            settings,
            family: None,
        }
    }

    /// Create whatever is missing.
    ///
    /// `existing_names` comes from the playlist listing the caller just relayed, so this costs
    /// no extra request in the common case where everything already exists.
    pub async fn ensure(
        &self,
        username: &str,
        existing_names: &[String],
        auth_parameters: &IndexMap<String, String>,
    ) {
        let titles = self.missing(username, existing_names);
        self.create(username, titles, auth_parameters).await;
    }

    /// The titles this request should create: wanted, absent from `existing_names`, and claimed
    /// from every other request. No I/O, so a caller can skip spawning when it is empty.
    pub fn missing(&self, username: &str, existing_names: &[String]) -> Vec<String> {
        let titles = self.upkeep_titles(username);
        if titles.is_empty() {
            return Vec::new();
        }
        if !self.done.lock().insert(dotnet::ordinal_ignore_case_key(username)) {
            return Vec::new();
        }

        let mut wanted: Vec<String> = Vec::new();
        for title in titles
            .actions
            .into_iter()
            .chain(titles.notices)
            .chain(titles.family)
        {
            let exists = existing_names
                .iter()
                .any(|name| dotnet::eq_ignore_case(name, &title));
            let repeated = wanted.iter().any(|name| dotnet::eq_ignore_case(name, &title));
            if !exists && !repeated {
                wanted.push(title);
            }
        }
        let current = self.settings.current();
        let server = current.subsonic.url.as_deref().unwrap_or_default();
        wanted.retain(|title| claim(server, username, title));
        wanted
    }

    /// The titles of `username`'s library upkeep playlists as the settings stand now: the library
    /// action titles that are on for them, the notice titles that are on, and their own family
    /// action titles while Family is on. A playlist is an upkeep one when it is theirs and its
    /// name is one of these, ignoring case. No I/O and no once-per-boot mark.
    pub fn upkeep_titles(&self, username: &str) -> UpkeepTitles {
        let current = self.settings.current();
        let settings = &current.library_actions;
        let actions_on = settings.enabled
            && (settings.playlists_enabled || settings.notices_enabled())
            && settings.is_allowed(Some(username));
        // The notice playlists Octo fills are the user's own too, for the same reason: they are
        // private, and the admin identity fills them.
        let actions: Vec<String> = if actions_on && settings.playlists_enabled {
            settings
                .effective_actions()
                .iter()
                .filter(|action| action.enabled)
                .map(|action| settings.playlist_title(action))
                .collect()
        } else {
            Vec::new()
        };
        let notices: Vec<String> = if actions_on {
            settings
                .enabled_notice_kinds()
                .into_iter()
                .map(|kind| settings.notice_title(kind))
                .collect()
        } else {
            Vec::new()
        };
        UpkeepTitles {
            actions,
            notices,
            family: self.family_titles(username),
        }
    }

    /// Creates `titles` with the user's own sign-in; each one that fails is released.
    pub async fn create(
        &self,
        username: &str,
        titles: Vec<String>,
        auth_parameters: &IndexMap<String, String>,
    ) {
        let server = self.settings.current().subsonic.url.clone().unwrap_or_default();
        for title in titles {
            // The user's own credentials, so the playlist belongs to them. Drop anything that
            // would make this look like an edit of an existing playlist. Keys compare ignoring
            // case.
            let mut parameters: IndexMap<String, String> = auth_parameters
                .iter()
                .filter(|(key, _)| {
                    !["id", "playlistId", "songId"]
                        .iter()
                        .any(|dropped| dotnet::eq_ignore_case(key, dropped))
                })
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            match parameters
                .keys()
                .find(|key| dotnet::eq_ignore_case(key, "name"))
                .cloned()
            {
                Some(key) => {
                    parameters.insert(key, title.clone());
                }
                None => {
                    parameters.insert("name".into(), title.clone());
                }
            }

            // Never fails into the playlist listing: a failure here costs a playlist, not the
            // user's ability to see their own.
            if self
                .proxy
                .relay_safe("rest/createPlaylist", parameters.iter())
                .await
                .is_some()
            {
                info!("Created action playlist '{title}' for {username}");
            } else {
                release(&server, username, &title);
                warn!("Could not create action playlist '{title}' for {username}");
            }
        }
    }

    /// Forget the once-per-boot marker, so a settings change re-provisions.
    pub fn reset(&self) {
        self.done.lock().clear();
    }

    /// The same provisioner, also making family members' own action playlists.
    pub fn with_family(mut self, family: Arc<FamilyStore>) -> Self {
        self.family = Some(family);
        self
    }

    /// A family member's own action playlists: "Request a copy" (and "Request a copy in FLAC"
    /// when they may ask for FLAC) for members who ask before adding, and "Remove from my
    /// library" for every member. None while Family is off or for anyone else.
    fn family_titles(&self, username: &str) -> Vec<String> {
        let Some(family) = &self.family else {
            return Vec::new();
        };
        if !self.settings.current().family.enabled {
            return Vec::new();
        }
        let Some(member) = family.member(username) else {
            return Vec::new();
        };
        let abilities = member.abilities();
        let mut titles = Vec::new();
        if abilities.add_to_library == AddToLibrary::Request {
            titles.push(REQUEST_PLAYLIST.to_string());
            if abilities.request_quality.rank() >= RequestQuality::Flac.rank() {
                titles.push(REQUEST_FLAC_PLAYLIST.to_string());
            }
        }
        titles.push(REMOVE_PLAYLIST.to_string());
        titles
    }
}

/// A family member's playlist: songs added to it are asked for (best quality).
pub const REQUEST_PLAYLIST: &str = "Request a copy";

/// A family member's playlist: songs added to it are asked for in FLAC.
pub const REQUEST_FLAC_PLAYLIST: &str = "Request a copy in FLAC";

/// A family member's playlist: songs added to it leave their own library.
pub const REMOVE_PLAYLIST: &str = "Remove from my library";

/// One person's library upkeep playlist titles
/// ([`LibraryActionPlaylistProvisioner::upkeep_titles`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpkeepTitles {
    /// Library action titles: a song put in one is an instruction for the action worker.
    pub actions: Vec<String>,
    /// Notice titles: Octo fills them with the songs it asks about.
    pub notices: Vec<String>,
    /// A family member's own action titles: a song added becomes a request or a removal.
    pub family: Vec<String>,
}

impl UpkeepTitles {
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty() && self.notices.is_empty() && self.family.is_empty()
    }

    /// Whether `name` is one of the titles, ignoring case.
    pub fn is_upkeep(&self, name: &str) -> bool {
        self.actions
            .iter()
            .chain(&self.notices)
            .chain(&self.family)
            .any(|title| dotnet::eq_ignore_case(title, name))
    }

    /// Whether a playlist named `name` refuses songs found online: a library action or a notice
    /// playlist. A family action playlist takes them, as requests.
    pub fn refuses_outside(&self, name: &str) -> bool {
        self.actions
            .iter()
            .chain(&self.notices)
            .any(|title| dotnet::eq_ignore_case(title, name))
    }

    /// Which kind of upkeep playlist `name` is, ignoring case: an action title first, then a
    /// notice title, then a family title; `None` when it is none of them.
    pub fn kind(&self, name: &str) -> Option<UpkeepKind> {
        let named = |titles: &[String]| titles.iter().any(|title| dotnet::eq_ignore_case(title, name));
        if named(&self.actions) {
            Some(UpkeepKind::Action)
        } else if named(&self.notices) {
            Some(UpkeepKind::Notice)
        } else if named(&self.family) {
            Some(UpkeepKind::Request)
        } else {
            None
        }
    }
}

/// What a library upkeep playlist is for, as a playlist row's `octoUpkeepKind` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpkeepKind {
    /// A library action playlist: a song put in one is an instruction for the action worker.
    Action,
    /// A notice playlist (Review, Duplicates): Octo fills it with the songs it asks about.
    Notice,
    /// A family member's own request or remove playlist: a song added becomes a request.
    Request,
}

impl UpkeepKind {
    /// The word a row's `octoUpkeepKind` carries.
    pub fn wire(self) -> &'static str {
        match self {
            UpkeepKind::Action => "action",
            UpkeepKind::Notice => "notice",
            UpkeepKind::Request => "request",
        }
    }
}

#[cfg(test)]
mod tests {
    use octo_core::settings::{AppSettings, LibraryActionSettings, SubsonicSettings};
    use wiremock::matchers::{any, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    /// The missing titles are created once, with the user's own sign-in and never an id that
    /// would make it an edit.
    #[tokio::test]
    async fn the_missing_playlists_are_created_once_as_the_user() {
        let navidrome = MockServer::builder().start().await;
        forget_claims(&navidrome.uri());
        Mock::given(path("/rest/createPlaylist"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(r#"{"subsonic-response":{"status":"ok"}}"#, "application/json"),
            )
            .mount(&navidrome)
            .await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(404))
            .mount(&navidrome)
            .await;
        let settings = Arc::new(SettingsStore::for_tests(AppSettings {
            library_actions: LibraryActionSettings {
                enabled: true,
                review_enabled: true,
                allowed_users: vec!["alice".into()],
                ..Default::default()
            },
            subsonic: SubsonicSettings {
                url: Some(navidrome.uri()),
                ..Default::default()
            },
            ..Default::default()
        }));
        let provisioner =
            LibraryActionPlaylistProvisioner::new(SubsonicProxyService::new(settings.clone()), settings);
        let auth: IndexMap<String, String> = [("u", "alice"), ("t", "tok"), ("s", "salt"), ("ID", "p9")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();

        // Keep comes on with Review; the Review playlist itself exists already.
        provisioner.ensure("bob", &[], &auth).await;
        provisioner
            .ensure("Alice", &["▸ review".to_string()], &auth)
            .await;
        provisioner.ensure("alice", &[], &auth).await;

        let created: Vec<(String, String, bool)> = navidrome
            .received_requests()
            .await
            .expect("recorded")
            .iter()
            .map(|r| {
                let query: Vec<(String, String)> = r.url.query_pairs().into_owned().collect();
                let get = |k: &str| query.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());
                (
                    get("name").unwrap_or_default(),
                    get("u").unwrap_or_default(),
                    query.iter().any(|(k, _)| k.eq_ignore_ascii_case("id")),
                )
            })
            .collect();
        assert_eq!(created, [("🛠 Keep".to_string(), "alice".to_string(), false)]);
    }

    fn keep_settings(navidrome: &MockServer) -> Arc<SettingsStore> {
        Arc::new(SettingsStore::for_tests(AppSettings {
            library_actions: LibraryActionSettings {
                enabled: true,
                review_enabled: true,
                allowed_users: vec!["alice".into()],
                ..Default::default()
            },
            subsonic: SubsonicSettings {
                url: Some(navidrome.uri()),
                ..Default::default()
            },
            ..Default::default()
        }))
    }

    fn creates(requests: &[wiremock::Request]) -> usize {
        requests
            .iter()
            .filter(|r| r.url.path() == "/rest/createPlaylist")
            .count()
    }

    /// Requests overlap: several listings, each from its own request, all saw the playlist
    /// missing. Only one creates it, and a listing started a moment later still does not.
    #[tokio::test]
    async fn overlapping_listings_create_a_missing_playlist_once() {
        let navidrome = MockServer::builder().start().await;
        forget_claims(&navidrome.uri());
        Mock::given(path("/rest/createPlaylist"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(r#"{"subsonic-response":{"status":"ok"}}"#, "application/json")
                    .set_delay(std::time::Duration::from_millis(50)),
            )
            .mount(&navidrome)
            .await;
        let settings = keep_settings(&navidrome);
        let auth: IndexMap<String, String> = [("u", "alice"), ("t", "tok"), ("s", "salt")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let listing = |user: &'static str| {
            let settings = settings.clone();
            let auth = auth.clone();
            async move {
                LibraryActionPlaylistProvisioner::new(SubsonicProxyService::new(settings.clone()), settings)
                    .ensure(user, &["▸ review".to_string()], &auth)
                    .await;
            }
        };
        tokio::join!(listing("alice"), listing("ALICE"), listing(" alice"));
        listing("alice").await;

        let requests = navidrome.received_requests().await.expect("recorded");
        assert_eq!(creates(&requests), 1);
    }

    /// A create Navidrome refused gives the title back, so the next listing tries again.
    #[tokio::test]
    async fn a_failed_create_is_tried_again_by_the_next_listing() {
        let navidrome = MockServer::builder().start().await;
        forget_claims(&navidrome.uri());
        Mock::given(path("/rest/createPlaylist"))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(1)
            .mount(&navidrome)
            .await;
        Mock::given(path("/rest/createPlaylist"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(r#"{"subsonic-response":{"status":"ok"}}"#, "application/json"),
            )
            .mount(&navidrome)
            .await;
        let settings = keep_settings(&navidrome);
        let auth: IndexMap<String, String> = [("u", "alice"), ("t", "tok"), ("s", "salt")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        for _ in 0..3 {
            LibraryActionPlaylistProvisioner::new(
                SubsonicProxyService::new(settings.clone()),
                settings.clone(),
            )
            .ensure("alice", &["▸ review".to_string()], &auth)
            .await;
        }

        let requests = navidrome.received_requests().await.expect("recorded");
        assert_eq!(creates(&requests), 2, "the refused one, then the one that worked");
    }

    /// Upkeep titles are the action and notice titles that are on for the person, read without
    /// touching the once-per-boot provisioning.
    #[test]
    fn upkeep_titles_name_the_action_and_notice_playlists_that_are_on() {
        let settings = Arc::new(SettingsStore::for_tests(AppSettings {
            library_actions: LibraryActionSettings {
                enabled: true,
                review_enabled: true,
                allowed_users: vec!["alice".into()],
                ..Default::default()
            },
            ..Default::default()
        }));
        let provisioner =
            LibraryActionPlaylistProvisioner::new(SubsonicProxyService::new(settings.clone()), settings);
        let titles = provisioner.upkeep_titles("Alice");
        assert!(titles.is_upkeep("▸ review"));
        assert!(titles.is_upkeep("🛠 KEEP"));
        assert!(titles.refuses_outside("▸ Review"));
        assert!(titles.refuses_outside("🛠 Keep"));
        assert_eq!(titles.kind("▸ REVIEW"), Some(UpkeepKind::Notice));
        assert_eq!(titles.kind("🛠 keep"), Some(UpkeepKind::Action));
        assert_eq!(titles.kind("Road trip"), None);
        assert_eq!(
            (UpkeepKind::Action.wire(), UpkeepKind::Notice.wire()),
            ("action", "notice")
        );
        assert!(!titles.is_upkeep("Road trip"));
        assert_eq!(provisioner.upkeep_titles("alice"), titles);
        assert!(provisioner.upkeep_titles("bob").is_empty());
        let listed = ["▸ Review".to_string(), "🛠 KEEP".to_string()];
        assert!(provisioner.missing("alice", &listed).is_empty());
    }

    /// Nothing missing means nothing to spawn: the listing path skips the create step.
    #[test]
    fn nothing_is_missing_when_every_title_is_listed() {
        let settings = Arc::new(SettingsStore::for_tests(AppSettings {
            library_actions: LibraryActionSettings {
                enabled: true,
                review_enabled: true,
                allowed_users: vec!["alice".into()],
                ..Default::default()
            },
            ..Default::default()
        }));
        let provisioner =
            LibraryActionPlaylistProvisioner::new(SubsonicProxyService::new(settings.clone()), settings);
        let listed = ["▸ Review".to_string(), "🛠 KEEP".to_string()];
        assert!(provisioner.missing("alice", &listed).is_empty());
    }
}

#[cfg(test)]
mod family_tests {
    use octo_core::family::abilities::Preset;
    use octo_core::settings::{AppSettings, SubsonicSettings};
    use wiremock::matchers::path;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::services::family::test_support::member;

    async fn created_for(user: &str, family_on: bool) -> Vec<String> {
        let navidrome = MockServer::builder().start().await;
        forget_claims(&navidrome.uri());
        Mock::given(path("/rest/createPlaylist"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(r#"{"subsonic-response":{"status":"ok"}}"#, "application/json"),
            )
            .mount(&navidrome)
            .await;
        let mut app = AppSettings {
            subsonic: SubsonicSettings {
                url: Some(navidrome.uri()),
                ..Default::default()
            },
            ..Default::default()
        };
        app.family.enabled = family_on;
        let settings = Arc::new(SettingsStore::for_tests(app));
        let family = Arc::new(FamilyStore::in_memory());
        family.upsert_member(member("alex", Preset::Member, 3));
        family.upsert_member(member("sam", Preset::Listener, 4));
        family.upsert_member(member("kim", Preset::Kid, 5));
        let provisioner =
            LibraryActionPlaylistProvisioner::new(SubsonicProxyService::new(settings.clone()), settings)
                .with_family(family);
        let auth: IndexMap<String, String> = [("u", user), ("t", "tok"), ("s", "salt")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        provisioner.ensure(user, &[], &auth).await;
        navidrome
            .received_requests()
            .await
            .expect("recorded")
            .iter()
            .filter_map(|r| {
                r.url
                    .query_pairs()
                    .find(|(k, _)| k == "name")
                    .map(|(_, v)| v.into_owned())
            })
            .collect()
    }

    #[test]
    fn a_members_own_action_titles_are_upkeep_but_take_songs_found_online() {
        let mut app = AppSettings::default();
        app.family.enabled = true;
        let settings = Arc::new(SettingsStore::for_tests(app));
        let family = Arc::new(FamilyStore::in_memory());
        family.upsert_member(member("sam", Preset::Listener, 4));
        let provisioner =
            LibraryActionPlaylistProvisioner::new(SubsonicProxyService::new(settings.clone()), settings)
                .with_family(family);
        let titles = provisioner.upkeep_titles("sam");
        assert!(titles.is_upkeep("request a copy"));
        assert!(titles.is_upkeep(REMOVE_PLAYLIST));
        assert!(!titles.refuses_outside(REQUEST_PLAYLIST));
        assert_eq!(titles.kind(REQUEST_PLAYLIST), Some(UpkeepKind::Request));
        assert_eq!(titles.kind(REMOVE_PLAYLIST), Some(UpkeepKind::Request));
        assert_eq!(UpkeepKind::Request.wire(), "request");
        assert!(provisioner.upkeep_titles("winters").is_empty());
    }

    #[tokio::test]
    async fn members_get_their_own_request_and_remove_playlists() {
        assert_eq!(
            created_for("sam", true).await,
            [REQUEST_PLAYLIST, REQUEST_FLAC_PLAYLIST, REMOVE_PLAYLIST]
        );
        // A kid may ask for MP3 copies only.
        assert_eq!(
            created_for("kim", true).await,
            [REQUEST_PLAYLIST, REMOVE_PLAYLIST]
        );
        // A member who adds songs at once has nothing to ask for.
        assert_eq!(created_for("alex", true).await, [REMOVE_PLAYLIST]);
        assert!(created_for("winters", true).await.is_empty());
        assert!(created_for("sam", false).await.is_empty(), "Family off");
    }
}
