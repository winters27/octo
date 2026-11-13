//! What a family member may do: five presets and per-member edits stored as a sparse map over
//! the preset, so a preset change keeps the edits.
//!
//! [`Abilities`] serializes with the field names of the `octoFamily` API
//! (`family.me.abilities`): camelCase, caps as kbps with `0` for no cap.

use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::{Map, Value};

/// The five starting points an owner picks from when adding someone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Preset {
    Owner,
    CoAdmin,
    Member,
    Listener,
    Kid,
}

/// Who a caller is to the family: a preset, or a Navidrome account Octo does not manage
/// (`role` in the `octoFamily` API).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Role {
    Owner,
    CoAdmin,
    Member,
    Listener,
    Kid,
    Unmanaged,
}

impl From<Preset> for Role {
    fn from(preset: Preset) -> Self {
        match preset {
            Preset::Owner => Role::Owner,
            Preset::CoAdmin => Role::CoAdmin,
            Preset::Member => Role::Member,
            Preset::Listener => Role::Listener,
            Preset::Kid => Role::Kid,
        }
    }
}

/// What a heart (or any add) on an outside song does for this member.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AddToLibrary {
    /// Downloads into the member's own library at once.
    Direct,
    /// Saves a link; a copy needs a request.
    Request,
    /// Saves a link; no requests.
    SaveOnly,
}

/// Where a member's songs go: their own library, or the shared library everyone in the family
/// plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MusicTarget {
    Own,
    Shared,
}

/// The quality a request asks for, and the highest one a member may ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RequestQuality {
    Best,
    Flac,
    Mp3,
}

impl RequestQuality {
    /// `Mp3 < Flac < Best`: a request above the member's maximum is refused.
    pub fn rank(self) -> u8 {
        match self {
            RequestQuality::Mp3 => 0,
            RequestQuality::Flac => 1,
            RequestQuality::Best => 2,
        }
    }

    /// The name as a member reads it ("best", "FLAC", "MP3").
    pub fn label(self) -> &'static str {
        match self {
            RequestQuality::Best => "best",
            RequestQuality::Flac => "FLAC",
            RequestQuality::Mp3 => "MP3",
        }
    }

    /// Parses `Best`, `Flac` or `Mp3` in any case.
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "best" => Some(RequestQuality::Best),
            "flac" => Some(RequestQuality::Flac),
            "mp3" => Some(RequestQuality::Mp3),
            _ => None,
        }
    }
}

/// The highest bitrate a member's library streams arrive at. Written as a number of kbps,
/// with `0` for the original file; read from a number, `"Original"`, or a
/// number in a string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum StreamCap {
    #[default]
    Original,
    Kbps(u32),
}

impl StreamCap {
    /// `Kbps(0)` is the original as well.
    pub fn from_kbps(kbps: u32) -> Self {
        if kbps == 0 {
            StreamCap::Original
        } else {
            StreamCap::Kbps(kbps)
        }
    }

    /// The cap in kbps, or None for the original.
    pub fn kbps(self) -> Option<u32> {
        match self {
            StreamCap::Original => None,
            StreamCap::Kbps(0) => None,
            StreamCap::Kbps(kbps) => Some(kbps),
        }
    }
}

impl Serialize for StreamCap {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u32(self.kbps().unwrap_or(0))
    }
}

impl<'de> Deserialize<'de> for StreamCap {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct CapVisitor;
        impl Visitor<'_> for CapVisitor {
            type Value = StreamCap;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("\"Original\" or a bitrate in kbps")
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> Result<StreamCap, E> {
                u32::try_from(v)
                    .map(StreamCap::from_kbps)
                    .map_err(|_| E::custom("bitrate too large"))
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> Result<StreamCap, E> {
                u32::try_from(v)
                    .map(StreamCap::from_kbps)
                    .map_err(|_| E::custom("bitrate must be 0 or more"))
            }

            fn visit_f64<E: de::Error>(self, v: f64) -> Result<StreamCap, E> {
                if v.fract() == 0.0 && (0.0..=u32::MAX as f64).contains(&v) {
                    Ok(StreamCap::from_kbps(v as u32))
                } else {
                    Err(E::custom("bitrate must be a whole number"))
                }
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<StreamCap, E> {
                let v = v.trim();
                if v.eq_ignore_ascii_case("original") {
                    return Ok(StreamCap::Original);
                }
                v.parse::<u32>()
                    .map(StreamCap::from_kbps)
                    .map_err(|_| E::custom("\"Original\" or a bitrate in kbps"))
            }
        }
        d.deserialize_any(CapVisitor)
    }
}

/// Every ability one member has, after the preset and their edits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Abilities {
    pub add_to_library: AddToLibrary,
    /// Where what they add lands. `Shared` sends it to the shared library, where the storage
    /// limit does not apply.
    pub music_goes_to: MusicTarget,
    pub request_quality: RequestQuality,
    pub auto_approve: bool,
    /// Requests per 7 days; 0 means no limit.
    pub weekly_request_limit: u32,
    pub stream_cap: StreamCap,
    /// kbps away from home; 0 means the same as `stream_cap`.
    pub away_cap: u32,
    /// Listening away from home allowed.
    pub away: bool,
    /// Devices playing at once; 0 means no limit.
    pub devices_at_once: u32,
    /// `download` and zips.
    pub download_files: bool,
    /// Offline copies in the Octo apps.
    pub offline_copies: bool,
    /// Share links.
    pub share: bool,
    pub import_playlists: bool,
    pub clean_only: bool,
    pub family_playlists_edit: bool,
    pub manage_family: bool,
    pub approve_requests: bool,
    /// Library actions (delete, better quality on shared songs). For the owner this means
    /// "as `LibraryActions.AllowedUsers` says"; for everyone else it is false. Not part of the
    /// app API.
    #[serde(default, skip_serializing)]
    pub library_actions: bool,
    /// Gigabytes the member's personal library may hold; 0 means no limit.
    pub storage_limit_gb: u32,
    /// A song another member already has on disk is added at once (no download, no request).
    pub instant_from_family: bool,
}

impl Abilities {
    /// The preset's abilities.
    pub fn preset(preset: Preset) -> Self {
        match preset {
            Preset::Owner => Abilities {
                add_to_library: AddToLibrary::Direct,
                music_goes_to: MusicTarget::Own,
                request_quality: RequestQuality::Best,
                // "n/a" in the table: the owner never waits on anyone.
                auto_approve: true,
                weekly_request_limit: 0,
                stream_cap: StreamCap::Original,
                away_cap: 0,
                away: true,
                devices_at_once: 0,
                download_files: true,
                offline_copies: true,
                share: true,
                import_playlists: true,
                clean_only: false,
                family_playlists_edit: true,
                manage_family: true,
                approve_requests: true,
                library_actions: true,
                storage_limit_gb: 0,
                instant_from_family: true,
            },
            Preset::CoAdmin => Abilities {
                library_actions: false,
                ..Abilities::preset(Preset::Owner)
            },
            Preset::Member => Abilities {
                manage_family: false,
                approve_requests: false,
                library_actions: false,
                storage_limit_gb: 50,
                ..Abilities::preset(Preset::Owner)
            },
            Preset::Listener => Abilities {
                add_to_library: AddToLibrary::Request,
                music_goes_to: MusicTarget::Own,
                request_quality: RequestQuality::Best,
                auto_approve: false,
                weekly_request_limit: 10,
                stream_cap: StreamCap::Original,
                away_cap: 0,
                away: true,
                devices_at_once: 0,
                download_files: false,
                offline_copies: true,
                share: false,
                import_playlists: true,
                clean_only: false,
                family_playlists_edit: true,
                manage_family: false,
                approve_requests: false,
                library_actions: false,
                storage_limit_gb: 10,
                instant_from_family: true,
            },
            Preset::Kid => Abilities {
                request_quality: RequestQuality::Mp3,
                weekly_request_limit: 5,
                clean_only: true,
                family_playlists_edit: false,
                storage_limit_gb: 5,
                instant_from_family: false,
                ..Abilities::preset(Preset::Listener)
            },
        }
    }

    /// A member's abilities: the preset with their edits on top. The owner's abilities are
    /// fixed, so edits never reach them. Returns the edit keys that were not applied, each as
    /// a warning line.
    pub fn for_member(preset: Preset, edits: &Map<String, Value>) -> (Self, Vec<String>) {
        let base = Abilities::preset(preset);
        if preset == Preset::Owner {
            let ignored = edits
                .keys()
                .map(|key| format!("{key}: the owner's abilities cannot be edited"))
                .collect();
            return (base, ignored);
        }
        base.with_edits(edits)
    }

    /// Applies every known key of `edits` (PascalCase as stored, or camelCase as the API
    /// sends; any case) and returns the keys that were unknown or held the wrong kind of
    /// value, each as a warning line. A wrong value leaves that ability as it was.
    pub fn with_edits(mut self, edits: &Map<String, Value>) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        for (key, value) in edits {
            if let Err(problem) = self.apply(key, value) {
                warnings.push(format!("{key}: {problem}"));
            }
        }
        (self, warnings)
    }

    fn apply(&mut self, key: &str, value: &Value) -> Result<(), &'static str> {
        fn flag(value: &Value) -> Result<bool, &'static str> {
            value.as_bool().ok_or("expected true or false")
        }
        fn count(value: &Value) -> Result<u32, &'static str> {
            value
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or("expected a whole number, 0 or more")
        }
        fn parsed<T: for<'de> Deserialize<'de>>(
            value: &Value,
            problem: &'static str,
        ) -> Result<T, &'static str> {
            serde_json::from_value(value.clone()).map_err(|_| problem)
        }
        match key.to_ascii_lowercase().as_str() {
            "addtolibrary" => {
                self.add_to_library = parsed(value, "expected Direct, Request or SaveOnly")?;
            }
            "musicgoesto" => self.music_goes_to = parsed(value, "expected Own or Shared")?,
            "requestquality" => {
                self.request_quality = value
                    .as_str()
                    .and_then(RequestQuality::parse)
                    .ok_or("expected Best, Flac or Mp3")?;
            }
            "autoapprove" => self.auto_approve = flag(value)?,
            "weeklyrequestlimit" => self.weekly_request_limit = count(value)?,
            "streamcap" => self.stream_cap = parsed(value, "expected Original or a bitrate in kbps")?,
            "awaycap" => self.away_cap = count(value)?,
            "away" => self.away = flag(value)?,
            "devicesatonce" => self.devices_at_once = count(value)?,
            "downloadfiles" => self.download_files = flag(value)?,
            "offlinecopies" => self.offline_copies = flag(value)?,
            "share" => self.share = flag(value)?,
            "importplaylists" => self.import_playlists = flag(value)?,
            "cleanonly" => self.clean_only = flag(value)?,
            "familyplaylistsedit" => self.family_playlists_edit = flag(value)?,
            "managefamily" => self.manage_family = flag(value)?,
            "approverequests" => self.approve_requests = flag(value)?,
            "storagelimitgb" => self.storage_limit_gb = count(value)?,
            "instantfromfamily" => self.instant_from_family = flag(value)?,
            // Library actions follow LibraryActions.AllowedUsers for the owner and are off
            // for everyone else; there is nothing to edit.
            _ => return Err("not an ability Octo knows"),
        }
        Ok(())
    }

    /// True when library streams are capped somewhere (at home or away).
    pub fn is_capped(&self) -> bool {
        self.stream_cap.kbps().is_some() || self.away_cap > 0
    }

    /// The storage limit in bytes, or None for no limit. Someone whose music goes to the shared
    /// library has none: the limit measures their own folder, which such songs never reach.
    pub fn storage_limit_bytes(&self) -> Option<u64> {
        if self.music_goes_to == MusicTarget::Shared {
            return None;
        }
        (self.storage_limit_gb > 0).then(|| u64::from(self.storage_limit_gb) * 1_000_000_000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn edits(value: Value) -> Map<String, Value> {
        value.as_object().cloned().expect("an object")
    }

    #[test]
    fn presets_match_the_table() {
        use AddToLibrary::*;
        use Preset::*;
        let o = Abilities::preset(Owner);
        let c = Abilities::preset(CoAdmin);
        let m = Abilities::preset(Member);
        let l = Abilities::preset(Listener);
        let k = Abilities::preset(Kid);

        // AddToLibrary
        assert_eq!(o.add_to_library, Direct);
        assert_eq!(c.add_to_library, Direct);
        assert_eq!(m.add_to_library, Direct);
        assert_eq!(l.add_to_library, Request);
        assert_eq!(k.add_to_library, Request);
        // RequestQuality
        assert_eq!(o.request_quality, RequestQuality::Best);
        assert_eq!(c.request_quality, RequestQuality::Best);
        assert_eq!(m.request_quality, RequestQuality::Best);
        assert_eq!(l.request_quality, RequestQuality::Best);
        assert_eq!(k.request_quality, RequestQuality::Mp3);
        // AutoApprove (n/a for the owner, held true)
        assert!(o.auto_approve);
        assert!(c.auto_approve);
        assert!(m.auto_approve);
        assert!(!l.auto_approve);
        assert!(!k.auto_approve);
        // WeeklyRequestLimit
        assert_eq!(o.weekly_request_limit, 0);
        assert_eq!(c.weekly_request_limit, 0);
        assert_eq!(m.weekly_request_limit, 0);
        assert_eq!(l.weekly_request_limit, 10);
        assert_eq!(k.weekly_request_limit, 5);
        // StreamCap, AwayCap, Away, DevicesAtOnce, OfflineCopies, ImportPlaylists: no limit
        // and allowed for everyone until the owner sets one for a person.
        for a in [&o, &c, &m, &l, &k] {
            assert_eq!(a.stream_cap, StreamCap::Original);
            assert_eq!(a.away_cap, 0);
            assert!(a.away);
            assert_eq!(a.devices_at_once, 0);
            assert!(a.offline_copies);
            assert!(a.import_playlists);
        }
        // DownloadFiles, Share
        for (a, on) in [(&o, true), (&c, true), (&m, true), (&l, false), (&k, false)] {
            assert_eq!(a.download_files, on);
            assert_eq!(a.share, on);
        }
        // FamilyPlaylistsEdit
        for (a, on) in [(&o, true), (&c, true), (&m, true), (&l, true), (&k, false)] {
            assert_eq!(a.family_playlists_edit, on);
        }
        // CleanOnly
        for (a, on) in [(&o, false), (&c, false), (&m, false), (&l, false), (&k, true)] {
            assert_eq!(a.clean_only, on);
        }
        // ManageFamily, ApproveRequests
        for (a, on) in [(&o, true), (&c, true), (&m, false), (&l, false), (&k, false)] {
            assert_eq!(a.manage_family, on);
            assert_eq!(a.approve_requests, on);
        }
        // LibraryActions (the owner's follows LibraryActions.AllowedUsers)
        for (a, on) in [(&o, true), (&c, false), (&m, false), (&l, false), (&k, false)] {
            assert_eq!(a.library_actions, on);
        }
        // MusicGoesTo
        for a in [&o, &c, &m, &l, &k] {
            assert_eq!(a.music_goes_to, MusicTarget::Own);
        }
        // StorageLimitGb
        for (a, gb) in [(&o, 0), (&c, 0), (&m, 50), (&l, 10), (&k, 5)] {
            assert_eq!(a.storage_limit_gb, gb);
        }
        // InstantFromFamily
        for (a, on) in [(&o, true), (&c, true), (&m, true), (&l, true), (&k, false)] {
            assert_eq!(a.instant_from_family, on);
        }
    }

    #[test]
    fn edits_override_only_named_fields() {
        let (a, warnings) = Abilities::preset(Preset::Listener).with_edits(&edits(json!({
            "OfflineCopies": false,
            "streamCap": 192,
            "WeeklyRequestLimit": 3,
            "addToLibrary": "SaveOnly",
        })));
        assert!(warnings.is_empty(), "{warnings:?}");
        let expected = Abilities {
            offline_copies: false,
            stream_cap: StreamCap::Kbps(192),
            weekly_request_limit: 3,
            add_to_library: AddToLibrary::SaveOnly,
            ..Abilities::preset(Preset::Listener)
        };
        assert_eq!(a, expected);
    }

    #[test]
    fn unknown_edit_keys_are_reported() {
        let (a, warnings) = Abilities::preset(Preset::Member).with_edits(&edits(json!({
            "Teleport": true,
            "Share": "yes",
            "DevicesAtOnce": 1,
        })));
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings.iter().any(|w| w.starts_with("Teleport:")));
        assert!(warnings.iter().any(|w| w.starts_with("Share:")));
        // The good edit still applied, the bad one left the ability alone.
        assert_eq!(a.devices_at_once, 1);
        assert!(a.share);
    }

    #[test]
    fn owner_preset_ignores_edits() {
        let (a, warnings) = Abilities::for_member(Preset::Owner, &edits(json!({ "Share": false })));
        assert_eq!(a, Abilities::preset(Preset::Owner));
        assert_eq!(warnings.len(), 1);
        let (m, none) = Abilities::for_member(Preset::Member, &edits(json!({ "Share": false })));
        assert!(!m.share);
        assert!(none.is_empty());
    }

    #[test]
    fn abilities_serialize_with_the_api_names() {
        let value = serde_json::to_value(Abilities::preset(Preset::Listener)).expect("serializes");
        assert_eq!(
            value,
            json!({
                "addToLibrary": "Request",
                "musicGoesTo": "Own",
                "requestQuality": "Best",
                "autoApprove": false,
                "weeklyRequestLimit": 10,
                "streamCap": 0,
                "awayCap": 0,
                "away": true,
                "devicesAtOnce": 0,
                "downloadFiles": false,
                "offlineCopies": true,
                "share": false,
                "importPlaylists": true,
                "cleanOnly": false,
                "familyPlaylistsEdit": true,
                "manageFamily": false,
                "approveRequests": false,
                "storageLimitGb": 10,
                "instantFromFamily": true
            })
        );
        let owner = serde_json::to_value(Abilities::preset(Preset::Owner)).expect("serializes");
        assert_eq!(owner["streamCap"], json!(0));
        let back: Abilities = serde_json::from_value(value).expect("reads back");
        assert_eq!(back, Abilities::preset(Preset::Listener));
    }

    #[test]
    fn music_goes_to_is_editable_and_round_trips() {
        let (a, warnings) =
            Abilities::preset(Preset::Member).with_edits(&edits(json!({ "MusicGoesTo": "Shared" })));
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(a.music_goes_to, MusicTarget::Shared);
        // The storage limit stays stored, but does not apply to shared music.
        assert_eq!(a.storage_limit_gb, 50);
        assert_eq!(a.storage_limit_bytes(), None);
        assert_eq!(
            Abilities::preset(Preset::Member).storage_limit_bytes(),
            Some(50_000_000_000)
        );
        let value = serde_json::to_value(&a).expect("serializes");
        assert_eq!(value["musicGoesTo"], json!("Shared"));
        let back: Abilities = serde_json::from_value(value).expect("reads back");
        assert_eq!(back, a);

        let (bad, warnings) =
            Abilities::preset(Preset::Member).with_edits(&edits(json!({ "musicGoesTo": "Elsewhere" })));
        assert_eq!(bad.music_goes_to, MusicTarget::Own);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
    }

    #[test]
    fn stream_cap_reads_every_spelling() {
        for (text, cap) in [
            ("0", StreamCap::Original),
            ("192", StreamCap::Kbps(192)),
            ("\"Original\"", StreamCap::Original),
            ("\"original\"", StreamCap::Original),
            ("\"256\"", StreamCap::Kbps(256)),
        ] {
            assert_eq!(
                serde_json::from_str::<StreamCap>(text).expect(text),
                cap,
                "{text}"
            );
        }
        assert!(serde_json::from_str::<StreamCap>("-1").is_err());
        assert!(serde_json::from_str::<StreamCap>("\"fast\"").is_err());
    }

    #[test]
    fn quality_ranks_mp3_below_flac_below_best() {
        assert!(RequestQuality::Mp3.rank() < RequestQuality::Flac.rank());
        assert!(RequestQuality::Flac.rank() < RequestQuality::Best.rank());
        assert_eq!(RequestQuality::parse(" FLAC "), Some(RequestQuality::Flac));
        assert_eq!(RequestQuality::parse("ogg"), None);
    }
}
