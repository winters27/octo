//! `Octo.Models.Settings.SoulseekSettings` (the `Soulseek` section).

use serde::{Deserialize, Serialize};

/// Octo's own AcoustID application key, which every install that sets no key of its own shares.
/// It is never written in the source: the release build is given it as the
/// `OCTO_ACOUSTID_APPLICATION_KEY` variable, read here at compile time. A build given none has a
/// blank built-in key, which counts as no key at all, so AcoustID is off there for an install
/// without one of its own.
pub const OCTO_ACOUSTID_APPLICATION_KEY: &str = match option_env!("OCTO_ACOUSTID_APPLICATION_KEY") {
    Some(key) => key,
    None => "",
};

/// Whether the build was asked to carry Octo's own key: `OCTO_REQUIRE_ACOUSTID_KEY` set to
/// anything but blank (the release build sets it to 1).
const KEY_REQUIRED: bool = key_required(option_env!("OCTO_REQUIRE_ACOUSTID_KEY"));

const fn key_required(value: Option<&str>) -> bool {
    matches!(value, Some(v) if !v.is_empty())
}

/// A build asked to carry Octo's own key (the release build) does not build without it.
const _: () = assert!(
    !KEY_REQUIRED || !OCTO_ACOUSTID_APPLICATION_KEY.trim_ascii().is_empty(),
    "this build needs OCTO_ACOUSTID_APPLICATION_KEY, Octo's own AcoustID application key"
);

/// The `ACOUSTID_API_KEY` value that turns AcoustID off.
pub const ACOUSTID_OFF: &str = "off";

/// Configuration for the Soulseek (slskd) integration.
/// Octo talks to a self-hosted slskd instance which fronts the Soulseek P2P network.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct SoulseekSettings {
    /// Soulseek is optional: false turns it off even with an address set. Off, Octo never calls
    /// slskd, and every download goes to the next source.
    /// Environment variable: Soulseek__Enabled
    pub enabled: bool,

    /// Base URL of the slskd REST API (e.g. http://slskd:5030 when running in the same docker
    /// network). Blank turns Soulseek off.
    pub base_url: Option<String>,

    /// slskd web UI / API admin username (Basic Auth).
    pub username: Option<String>,

    /// slskd web UI / API admin password (Basic Auth).
    pub password: Option<String>,

    /// The address the dashboard's Open slskd button opens in your browser. Blank works it out:
    /// BaseUrl when that is an address a browser can reach, otherwise this server's own name on
    /// BaseUrl's port (5030). Only http and https addresses are used. The browser gets the address
    /// and nothing else: slskd asks for its own sign-in, and Octo's slskd login never leaves Octo.
    /// Environment variable: SLSKD_WEB_URL
    pub web_url: String,

    /// Ask Soulseek's own port test (tools.slsknet.org) whether other people can connect to
    /// slskd's listening port, for the Sharing card. It tests the address the request comes from,
    /// so it only means something when Octo and slskd reach the internet the same way. Sends the
    /// port number and nothing else, at most every 6 hours unless you press Test. Off never asks.
    /// Environment variable: SLSKD_CHECK_PORT
    pub check_listen_port: bool,

    /// Share the music library with the Soulseek network, through slskd. On, Octo writes
    /// SharedFolders into slskd's own settings file and has slskd look through them; off, it
    /// writes an empty list, has slskd forget what it shared and cancels uploads still waiting or
    /// under way. Applied within seconds, without a restart. Off here so an existing install never
    /// starts sharing on an update by itself; the installer and .env.example turn it on for new
    /// ones.
    /// Environment variable: SLSKD_SHARE_LIBRARY
    pub share_library: bool,

    /// What slskd shares while ShareLibrary is on, as slskd sees the folders, separated by ";".
    /// "[Music]/share" is the library's read-only mount in the bundled compose file, shown to
    /// other people as a folder named Music. A "!" in front keeps a folder out.
    /// Environment variable: SLSKD_SHARED_DIR
    pub shared_folders: String,

    /// How long to wait (seconds) for a Soulseek search to gather peer responses
    /// before returning results. Soulseek searches stream in over time, so this is
    /// the difference between finding a lossless file and silently settling for a
    /// transcode.
    ///
    /// This was 6, which measurement showed is simply too short: polling slskd's
    /// /responses for the same query returned nothing at 6s, 10s or 15s, then 14
    /// responses including 5 FLACs at 20s: reproducibly, across three runs. The
    /// effect was that every star fell back to YouTube MP3 while lossless copies
    /// were sitting there unseen. Note that the search status object reports a
    /// responseCount well before /responses will hand the files over, so a status
    /// poll makes short waits look adequate when they are not.
    ///
    /// This is a CEILING, not a duration. slskd ends most searches itself, after 15 s
    /// with no new answer or at the response or file limit, and Octo reads the answers
    /// then. A search still running at the ceiling is canceled, which makes slskd hand
    /// over everything it gathered, so a long search costs time but never its results.
    ///
    /// Star-triggered downloads are fire-and-forget, so the wait costs the user
    /// nothing; it only delays the file landing.
    pub search_wait_seconds: i32,

    /// The search ceiling for Better quality and the weekly upgrade. Longer than SearchWaitSeconds
    /// because these searches exist for songs the quick search did not find.
    /// Environment variable: SLSKD_UPGRADE_SEARCH_WAIT_SECONDS
    pub upgrade_search_wait_seconds: i32,

    /// Minimum file size in bytes to consider a search hit a real lossless file. Default 5 MB
    /// filters out 30s teaser clips and mislabeled tiny files.
    pub min_file_size_bytes: i64,

    /// Preferred file extension. Hits with this extension are sorted first.
    pub preferred_extension: String,

    /// How long (seconds) a download may go without a new byte before Octo gives up on
    /// that peer, cancels the transfer in slskd and tries the next one. A peer that
    /// keeps sending is waited for however slow it is, up to an hour. Per attempt, not
    /// per track. A peer that rejects outright is detected in seconds.
    pub download_timeout_seconds: i32,

    /// Fingerprint every finished download with Chromaprint and ask AcoustID what it actually
    /// is before accepting it. A Soulseek file identified as a different recording is discarded
    /// and the next peer tried; a YouTube file has no second candidate, so it is kept and, with
    /// the Review playlist on, asked about. Needs the fpcalc binary. On by default: Octo's
    /// published images carry Octo's own AcoustID key, so a default install from them checks what
    /// it downloads (a build given no key needs a key of the user's own). Hearts and plays are
    /// checked by their sound whatever this says.
    ///
    /// This also switches on the rejected-peer memory. A file discarded for being the wrong
    /// recording, by this check OR by the pre-existing duration check, is remembered by peer
    /// and filename and never offered as a candidate again.
    /// Environment variable: SLSKD_VERIFY_DOWNLOADS
    pub verify_downloads: bool,

    /// Never download a cover, a karaoke backing track or an instrumental, even for a song whose
    /// title asks for one, and leave such outside songs out of search, radio, mixes and charts.
    /// On by default. Off, a download is one of them only when its title asks for it; a plain
    /// song never is, and they are listed among outside songs again.
    /// Environment variable: SKIP_COVERS_AND_INSTRUMENTALS
    pub skip_covers_and_instrumentals: bool,

    /// Your own AcoustID application key, free from https://acoustid.org/new-application. Blank
    /// uses Octo's own key, which every install without one shares; `off` turns AcoustID off.
    /// Named ...ApiKey so the Config-sources tab masks it.
    /// Environment variable: ACOUSTID_API_KEY
    pub acoust_id_api_key: String,

    /// On a confident AcoustID match, write that recording's title, artist, album and year,
    /// which are MusicBrainz's, onto the file instead of trusting the source's tags. Applies to
    /// Soulseek and YouTube downloads alike. Does nothing unless VerifyDownloads is on and a key
    /// is set.
    /// Environment variable: SLSKD_TAG_FROM_MUSICBRAINZ
    pub tag_from_music_brainz: bool,

    /// On a confident AcoustID match, name the file from the matched recording as well as
    /// tagging it: artist folder, title, album and track number all come from MusicBrainz, so
    /// the path and the tags are one decision (#48). Implies TagFromMusicBrainz for that file,
    /// because a path from MusicBrainz beside tags from the source is the split this removes.
    /// Off by default: a canonical name is not always the one a user wants on disk (a legal
    /// name, or a composer where the library files the performer). Only files Octo downloads
    /// and confirms are affected; nothing already in the library is renamed.
    /// Environment variable: NAME_FROM_MATCH
    pub name_from_match: bool,

    /// Minimum AcoustID fingerprint score, as a percentage, before a lookup result is
    /// allowed to decide anything at all.
    ///
    /// Counter-intuitively this is a permissiveness dial, not a strictness dial: results
    /// below it are ignored entirely rather than treated as rejections, so a HIGHER value
    /// rejects FEWER files. The admin help text says so out loud because the intuition runs
    /// the other way.
    /// Environment variable: SLSKD_MIN_MATCH_SCORE
    pub min_match_score: i32,

    /// How long a rejected peer and filename is remembered. Long enough that a wrong file is
    /// not re-fetched across a listening season, short enough that a verdict that was simply
    /// wrong lapses without the user ever learning the file existed. 0 never forgets, which
    /// suits anyone who would rather clear the list by hand.
    /// Environment variable: SLSKD_REJECTED_PEER_DAYS
    pub rejected_peer_ttl_days: i32,

    /// How many seconds of audio to fingerprint. AcoustID's own tools submit 120, and more
    /// costs decode time without identifying anything extra.
    /// Environment variable: SLSKD_FINGERPRINT_SECONDS
    pub fingerprint_seconds: i32,

    /// How long fpcalc may take before it is treated as hung. A FLAC fingerprints in about a
    /// second, so thirty is a hang rather than a slow disk; raise it for very slow storage.
    /// Environment variable: SLSKD_FINGERPRINT_TIMEOUT_SECONDS
    pub fingerprint_timeout_seconds: i32,

    /// How long an AcoustID lookup may take. Verification sits between a finished transfer and
    /// the file joining the library, so a slow AcoustID must cost seconds, never the download.
    /// Environment variable: SLSKD_ACOUSTID_TIMEOUT_SECONDS
    pub acoust_id_timeout_seconds: i32,

    /// Check a download that claims to be lossless (FLAC, WAV, AIFF and the like) for a lossy
    /// file converted to it, by the cutoff in its spectrum. A likely transcode is passed over
    /// for another lossless copy when there is one, and kept, marked as transcoded, when there
    /// is not: it is still the right song. Needs no API key and never fails a download; a
    /// missing ffmpeg or a file it cannot read is no opinion. On by default, since it costs a
    /// second or so of decoding per lossless download and only ever changes which copy is kept.
    /// Also used by the better-quality library action and the duplicate scan.
    pub detect_transcodes: bool,

    /// How long the transcode check may decode for, every window included. A FLAC takes well
    /// under a second, so twenty is a hang rather than a slow disk.
    pub transcode_check_timeout_seconds: i32,

    /// How long a Soulseek-first download waits while slskd is not logged in to the Soulseek
    /// network, before it goes to the next source. slskd answering is not slskd being able to
    /// search: during Soulseek's maintenance on 2026-10-03 it answered for three hours while
    /// every search failed, and a hearted album landed as YouTube MP3s. Six hours covers a
    /// normal maintenance window. 0 turns the wait off. Read live, no restart needed.
    /// Environment variable: SLSKD_OUTAGE_HOLD_HOURS
    pub outage_hold_hours: i32,

    /// How many downloads may transfer at once. Each Soulseek download lands in its own folder,
    /// which is what makes this safe; until slskd has put one there, Octo keeps to one at a time
    /// whatever this says. Placing files into the library stays one at a time either way. 1 is the
    /// old behavior exactly. Read live.
    /// Environment variable: SLSKD_PARALLEL_DOWNLOADS
    pub parallel_downloads: i32,

    /// An album heart searches the album once and takes one peer's folder of it in one batch, then
    /// searches song by song only for what that folder lacks. Off is the old song by song walk.
    /// Environment variable: SLSKD_ALBUM_FOLDERS
    pub album_folders: bool,

    /// Send AcoustID the fingerprints a person confirmed with Keep, so the next person who
    /// downloads that recording gets Confirmed instead of Inconclusive (#47). Only a fingerprint
    /// whose MusicBrainz recording is unambiguous, only after a human kept it, and never one
    /// AcoustID confidently called something else. Needs AcoustIdUserApiKey. Off by default: it
    /// writes to a public database.
    /// Environment variable: ACOUSTID_SUBMIT
    pub submit_confirmed_fingerprints: bool,

    /// Your personal AcoustID key, shown at acoustid.org after signing in. Separate from the
    /// application key, because AcoustID credits a submission to a person, not an app. Named
    /// ...ApiKey so the Config-sources tab masks it.
    /// Environment variable: ACOUSTID_USER_KEY
    pub acoust_id_user_api_key: String,
}

impl Default for SoulseekSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            base_url: None,
            username: None,
            password: None,
            web_url: String::new(),
            check_listen_port: true,
            share_library: false,
            shared_folders: "[Music]/share".to_string(),
            search_wait_seconds: 30,
            upgrade_search_wait_seconds: 90,
            min_file_size_bytes: 5 * 1024 * 1024,
            preferred_extension: "flac".to_string(),
            download_timeout_seconds: 180,
            verify_downloads: true,
            skip_covers_and_instrumentals: true,
            acoust_id_api_key: String::new(),
            tag_from_music_brainz: false,
            name_from_match: false,
            min_match_score: 85,
            rejected_peer_ttl_days: 30,
            fingerprint_seconds: 120,
            fingerprint_timeout_seconds: 30,
            acoust_id_timeout_seconds: 10,
            detect_transcodes: true,
            transcode_check_timeout_seconds: 20,
            outage_hold_hours: 6,
            parallel_downloads: 3,
            album_folders: true,
            submit_confirmed_fingerprints: false,
            acoust_id_user_api_key: String::new(),
        }
    }
}

impl SoulseekSettings {
    /// 0 means never forget, so it is not clamped upward.
    pub fn effective_rejected_peer_ttl_days(&self) -> i32 {
        if self.rejected_peer_ttl_days <= 0 {
            0
        } else {
            self.rejected_peer_ttl_days.clamp(1, 3650)
        }
    }

    pub fn effective_fingerprint_seconds(&self) -> i32 {
        self.fingerprint_seconds.clamp(15, 600)
    }

    pub fn effective_fingerprint_timeout_seconds(&self) -> i32 {
        self.fingerprint_timeout_seconds.clamp(5, 300)
    }

    pub fn effective_acoust_id_timeout_seconds(&self) -> i32 {
        self.acoust_id_timeout_seconds.clamp(2, 120)
    }

    pub fn effective_transcode_check_timeout_seconds(&self) -> i32 {
        self.transcode_check_timeout_seconds.clamp(5, 300)
    }

    pub fn effective_upgrade_search_wait_seconds(&self) -> i32 {
        self.upgrade_search_wait_seconds.clamp(30, 300)
    }

    /// Whether Octo uses Soulseek at all: switched on and given slskd's address.
    pub fn is_on(&self) -> bool {
        self.enabled && self.base_url.as_deref().is_some_and(|url| !url.trim().is_empty())
    }

    /// Two days at most: past that the next source is the better answer.
    pub fn effective_outage_hold_hours(&self) -> i32 {
        self.outage_hold_hours.clamp(0, 48)
    }

    /// Six at most: more peers at once gains little and spends the Soulseek network's patience.
    pub fn effective_parallel_downloads(&self) -> i32 {
        self.parallel_downloads.clamp(1, 6)
    }

    /// Below 50 an AcoustID score is noise and acting on it manufactures false rejections;
    /// 100 is a score no real fingerprint reaches, which would silently disable the feature.
    pub fn effective_min_match_score(&self) -> i32 {
        self.min_match_score.clamp(50, 99)
    }

    /// The same threshold as AcoustID reports it, in 0..1.
    pub fn effective_min_score_fraction(&self) -> f64 {
        f64::from(self.effective_min_match_score()) / 100.0
    }

    /// AcoustID is switched off by the `off` setting.
    pub fn acoust_id_off(&self) -> bool {
        self.acoust_id_api_key.trim().eq_ignore_ascii_case(ACOUSTID_OFF)
    }

    /// The user's own application key: set, and not `off`.
    pub fn acoust_id_user_key(&self) -> Option<String> {
        let key = self.acoust_id_api_key.trim();
        (!key.is_empty() && !self.acoust_id_off()).then(|| key.to_string())
    }

    /// Octo's own application key, when it has one.
    pub fn acoust_id_built_in_key() -> Option<&'static str> {
        Self::built_in_key_of(OCTO_ACOUSTID_APPLICATION_KEY)
    }

    /// The key one person's own action looks up with (a heart's or a play's gate, a family
    /// request, a click): the user's key, else Octo's own. None when AcoustID is off.
    pub fn acoust_id_key_for_heart(&self) -> Option<String> {
        self.key_for_heart_with(Self::acoust_id_built_in_key())
    }

    /// The key work nobody waits on looks up with: the user's own only. Without one, such work
    /// may still use Octo's own key, at the shared lane's lowest place, but never a submission.
    pub fn acoust_id_key_for_bulk(&self) -> Option<String> {
        self.acoust_id_user_key()
    }

    /// [`Self::acoust_id_key_for_heart`] with a given built-in key.
    pub fn key_for_heart_with(&self, built_in: Option<&str>) -> Option<String> {
        if self.acoust_id_off() {
            return None;
        }
        self.acoust_id_user_key()
            .or_else(|| built_in.and_then(Self::built_in_key_of).map(str::to_string))
    }

    fn built_in_key_of(key: &str) -> Option<&str> {
        let key = key.trim();
        (!key.is_empty()).then_some(key)
    }

    /// SharedFolders as a list, blanks dropped.
    pub fn shared_folder_list(&self) -> Vec<String> {
        self.shared_folders
            .split(';')
            .map(str::trim)
            .filter(|folder| !folder.is_empty())
            .map(str::to_string)
            .collect()
    }

    /// WebUrl when it is an absolute http or https address, otherwise None.
    pub fn effective_web_url(&self) -> Option<String> {
        Self::safe_web_url(Some(&self.web_url))
    }

    /// An absolute http or https address, trimmed, or None. Anything else (a script: or file:
    /// address, a bare host) is never handed to the browser as a link.
    ///
    /// The `url` crate parses and writes the address (a bare host gains its "/").
    pub fn safe_web_url(value: Option<&str>) -> Option<String> {
        let parsed = url::Url::parse(value?.trim()).ok()?;
        let web = matches!(parsed.scheme(), "http" | "https");
        (web && parsed.host_str().is_some_and(|host| !host.is_empty())).then(|| parsed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_setting_is_clamped() {
        let with = |n| SoulseekSettings {
            parallel_downloads: n,
            ..Default::default()
        };
        assert_eq!(with(50).effective_parallel_downloads(), 6);
        assert_eq!(with(0).effective_parallel_downloads(), 1);
        assert_eq!(SoulseekSettings::default().parallel_downloads, 3);
    }

    #[test]
    fn rejected_peer_ttl_treats_zero_as_forever() {
        for (configured, expected) in [(0, 0), (-3, 0), (30, 30), (99999, 3650)] {
            let s = SoulseekSettings {
                rejected_peer_ttl_days: configured,
                ..Default::default()
            };
            assert_eq!(s.effective_rejected_peer_ttl_days(), expected, "{configured}");
        }
    }

    #[test]
    fn match_score_is_clamped_and_read_as_a_fraction() {
        let s = SoulseekSettings {
            min_match_score: 100,
            ..Default::default()
        };
        assert_eq!(s.effective_min_match_score(), 99);
        assert_eq!(SoulseekSettings::default().effective_min_score_fraction(), 0.85);
    }

    #[test]
    fn shared_folders_split_on_semicolons_and_drop_blanks() {
        let s = SoulseekSettings {
            shared_folders: " [Music]/share ; ;!/share/private;".to_string(),
            ..Default::default()
        };
        assert_eq!(s.shared_folder_list(), ["[Music]/share", "!/share/private"]);
        assert_eq!(
            SoulseekSettings::default().shared_folder_list(),
            ["[Music]/share"]
        );
    }

    #[test]
    fn only_a_whole_web_address_is_a_web_url() {
        let cases: [(&str, Option<&str>); 7] = [
            ("http://192.168.1.5:5030", Some("http://192.168.1.5:5030/")),
            ("  https://slskd.example/ ", Some("https://slskd.example/")),
            ("javascript:alert(1)", None),
            ("file:///etc/passwd", None),
            ("slskd:5030", None),
            ("/slskd", None),
            ("", None),
        ];
        for (value, expected) in cases {
            assert_eq!(
                SoulseekSettings::safe_web_url(Some(value)).as_deref(),
                expected,
                "{value}"
            );
        }
        assert_eq!(SoulseekSettings::default().effective_web_url(), None);
    }
}

#[cfg(test)]
mod acoust_id_key_tests {
    use super::*;

    fn with_key(key: &str) -> SoulseekSettings {
        SoulseekSettings {
            acoust_id_api_key: key.to_string(),
            ..Default::default()
        }
    }

    /// A blank key means Octo's own; `off`, in any case and with spaces, turns AcoustID off; the
    /// user's own key wins for both kinds of work, and bulk work never gets Octo's own as a key.
    #[test]
    fn a_blank_key_means_octos_own_and_off_turns_acoustid_off() {
        let octo = Some("octo-key");
        for blank in ["", "   "] {
            let s = with_key(blank);
            assert_eq!(
                s.key_for_heart_with(octo).as_deref(),
                Some("octo-key"),
                "{blank:?}"
            );
            assert_eq!(s.acoust_id_key_for_bulk(), None);
            assert!(!s.acoust_id_off());
        }
        for off in ["off", " OFF ", "Off"] {
            let s = with_key(off);
            assert!(s.acoust_id_off());
            assert_eq!(s.key_for_heart_with(octo), None, "{off:?}");
            assert_eq!(s.acoust_id_key_for_bulk(), None);
            assert_eq!(s.acoust_id_user_key(), None);
        }
        let own = with_key(" user-key ");
        assert_eq!(own.key_for_heart_with(octo).as_deref(), Some("user-key"));
        assert_eq!(own.key_for_heart_with(None).as_deref(), Some("user-key"));
        assert_eq!(own.acoust_id_key_for_bulk().as_deref(), Some("user-key"));
    }

    /// Octo's own key counts only once it is set: a blank one is no key, so an install without a
    /// key of its own has AcoustID off until then.
    #[test]
    fn octos_own_key_counts_only_once_it_is_set() {
        let keyless = SoulseekSettings::default();
        assert_eq!(keyless.key_for_heart_with(None), None);
        assert_eq!(keyless.key_for_heart_with(Some("  ")), None);
        let built_in = OCTO_ACOUSTID_APPLICATION_KEY.trim();
        if built_in.is_empty() {
            assert_eq!(SoulseekSettings::acoust_id_built_in_key(), None);
            assert_eq!(keyless.acoust_id_key_for_heart(), None);
        } else {
            assert_eq!(SoulseekSettings::acoust_id_built_in_key(), Some(built_in));
            assert_eq!(keyless.acoust_id_key_for_heart().as_deref(), Some(built_in));
        }
        assert_eq!(with_key("off").acoust_id_key_for_heart(), None);
    }

    /// Verification is on by default in the code and in the three files an install starts from.
    #[test]
    fn downloads_are_verified_by_default_everywhere() {
        assert!(SoulseekSettings::default().verify_downloads);
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let read =
            |name: &str| std::fs::read_to_string(repo.join(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(read("octo/appsettings.json").contains(r#""VerifyDownloads": true,"#));
        assert!(
            read("docker-compose.yml").contains("Soulseek__VerifyDownloads: ${SLSKD_VERIFY_DOWNLOADS:-true}")
        );
        let example = read(".env.example");
        assert!(example.lines().any(|line| line == "SLSKD_VERIFY_DOWNLOADS=true"));
        assert!(example.lines().any(|line| line == "ACOUSTID_API_KEY="));
        assert!(example.contains("ACOUSTID_API_KEY=off"));
    }

    /// The published server image is built with Octo's own AcoustID key from the repository's
    /// secret, and its build refuses to go on when that key is blank: the workflow checks the
    /// secret before building and hands the key to the server's build only as a build secret
    /// (never a build argument, which an image's history and provenance keep), with the
    /// requirement and the key's hash as build arguments and the provenance kept to its minimum;
    /// the Dockerfile mounts the secret as a file into the server's build step alone (no `env=`,
    /// which an older BuildKit cannot build), reads the key from that file only, never prints it
    /// or its hash, checks it again, and the compile refuses a blank key.
    #[test]
    fn the_published_image_is_built_with_octos_own_acoustid_key() {
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let read =
            |name: &str| std::fs::read_to_string(repo.join(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
        let workflow = read(".github/workflows/docker.yml");
        assert!(
            workflow.contains("OCTO_ACOUSTID_APPLICATION_KEY: ${{ secrets.OCTO_ACOUSTID_APPLICATION_KEY }}")
        );
        assert!(workflow.contains("'OCTO_REQUIRE_ACOUSTID_KEY=1'"));
        assert!(workflow.contains("arg=OCTO_ACOUSTID_KEY_SHA256="));
        let check = workflow
            .find("name: Octo's own AcoustID key is set")
            .expect("the check of the secret");
        let build = workflow
            .find("name: Build and push by digest")
            .expect("the image build");
        assert!(check < build, "the secret is checked before the build");
        let step = &workflow[build..];
        let step = &step[..step.find("\n      - name:").unwrap_or(step.len())];
        let args = &step[step.find("build-args:").expect("the build arguments")..];
        let args = &args[..args.find("\n          secrets:").expect("the secrets after them")];
        assert!(
            !args.contains("secrets."),
            "no secret is a build argument: {args}"
        );
        assert!(args.contains("${{ steps.acoustid.outputs.arg }}"));
        assert!(
            step.contains("format('acoustid_key={0}', secrets.OCTO_ACOUSTID_APPLICATION_KEY)"),
            "the key is a build secret"
        );
        assert!(step.contains("provenance: mode=min"));

        let dockerfile = read("Dockerfile");
        assert!(dockerfile.starts_with("# syntax=docker/dockerfile:1\n"));
        assert!(
            !dockerfile.contains("ARG OCTO_ACOUSTID_APPLICATION_KEY"),
            "the key is no build argument"
        );
        let from_the_secret =
            "OCTO_ACOUSTID_APPLICATION_KEY=\"$(cat /run/secrets/acoustid_key 2>/dev/null || true)\"";
        assert_eq!(
            dockerfile.matches("OCTO_ACOUSTID_APPLICATION_KEY=").count(),
            dockerfile.matches(from_the_secret).count(),
            "the Dockerfile names no key: it reads it from the secret's file alone"
        );
        let required = dockerfile
            .find("ARG OCTO_REQUIRE_ACOUSTID_KEY")
            .expect("the requirement's build argument");
        let hash = dockerfile
            .find("ARG OCTO_ACOUSTID_KEY_SHA256")
            .expect("the key's hash, which rebuilds the server when the key changes");
        let mount = dockerfile
            .find("RUN --mount=type=secret,id=acoustid_key \\\n")
            .expect("the secret mounted as a file into the server's build step");
        let server = dockerfile
            .find("cargo build --release --locked --package octo --bin octo")
            .expect("the server's build");
        assert!(required < mount && hash < mount && mount < server);
        assert!(
            !dockerfile.contains(",env="),
            "no secret mounted as an environment variable"
        );
        assert_eq!(
            dockerfile.matches("--mount=type=secret").count(),
            1,
            "no other step sees the key"
        );
        let step = &dockerfile[mount..server];
        assert!(step.contains(from_the_secret));
        assert!(step.contains("exit 1"));
        assert!(
            !dockerfile.contains("${OCTO_ACOUSTID_KEY_SHA256"),
            "the build log prints neither the key nor its hash"
        );
    }

    /// Only a requirement given a value asks the build for Octo's own key: one set but blank does
    /// not.
    #[test]
    fn a_blank_requirement_asks_for_no_key() {
        assert!(!key_required(None));
        assert!(!key_required(Some("")));
        assert!(key_required(Some("1")));
    }
}

#[cfg(test)]
mod switch_tests {
    use super::*;

    #[test]
    fn soulseek_is_on_only_when_switched_on_and_given_an_address() {
        let with = |enabled: bool, base_url: Option<&str>| SoulseekSettings {
            enabled,
            base_url: base_url.map(str::to_string),
            ..Default::default()
        };
        assert!(with(true, Some("http://slskd:5030")).is_on());
        assert!(!with(false, Some("http://slskd:5030")).is_on());
        assert!(!with(true, Some("  ")).is_on());
        assert!(!with(true, None).is_on());
        assert!(SoulseekSettings::default().enabled);
    }
}
