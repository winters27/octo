//! The `Spotiflac` section: a SpotiFLAC server the person runs themselves, which Octo asks for
//! songs. Octo brings no SpotiFLAC and no providers of its own; everything that makes the source
//! work (the server, its extension registry, its extensions) is the person's.

use serde::{Deserialize, Serialize};

/// When SpotiFLAC is asked, against Soulseek.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Deserialize, Serialize)]
pub enum SpotiflacPlacement {
    /// Only once Soulseek found no copy that passed the checks (or Soulseek is off).
    #[default]
    AfterSoulseek,

    /// First for every song; Soulseek is asked when SpotiFLAC brings nothing.
    BeforeSoulseek,

    /// First for songs whose title, artist or album is written in Chinese, Japanese, Korean or
    /// Thai script, after Soulseek for every other song.
    BeforeSoulseekForAsianScripts,
}

impl SpotiflacPlacement {
    /// Every placement, in the order the dashboard lists them.
    pub const ALL: [SpotiflacPlacement; 3] = [
        SpotiflacPlacement::AfterSoulseek,
        SpotiflacPlacement::BeforeSoulseek,
        SpotiflacPlacement::BeforeSoulseekForAsianScripts,
    ];
}

/// The qualities Octo asks SpotiFLAC for: CD quality lossless, or hi-res lossless where there is
/// one (SpotiFLAC falls back to CD quality by itself). Anything else is asked as `LOSSLESS`.
pub const SPOTIFLAC_QUALITIES: [&str; 2] = ["LOSSLESS", "HI_RES_LOSSLESS"];

/// The SpotiFLAC source, off until someone turns it on and gives it an address.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct SpotiflacSettings {
    /// Whether downloads ask SpotiFLAC at all. SPOTIFLAC_ENABLED.
    pub enabled: bool,

    /// The address of the person's Mornye web server (`mornye --web`, or an older
    /// `spotiflac --web`), as Octo's container reaches it. SPOTIFLAC_URL.
    pub url: String,

    /// The web token (Mornye's `--web-token` or `MORNYE_WEB_TOKEN`; SpotiFLAC's before the
    /// rename), when it has one. SPOTIFLAC_WEB_TOKEN.
    pub token: String,

    /// The quality asked for: `LOSSLESS` or `HI_RES_LOSSLESS`. SPOTIFLAC_QUALITY.
    pub quality: String,

    /// The person's own SpotiFLAC extension ids to try, in order, separated by commas. Blank
    /// leaves the order to SpotiFLAC. SPOTIFLAC_SERVICES.
    pub services: String,

    /// Octo's music folder as SpotiFLAC sees it, when SpotiFLAC mounts it at another path.
    /// Blank: the same path Octo uses. SPOTIFLAC_FOLDER.
    pub folder: String,

    /// When SpotiFLAC is asked, against Soulseek. SPOTIFLAC_PLACEMENT.
    pub placement: SpotiflacPlacement,

    /// How long one song may take, from the ask to the file, in seconds (60 to 3600).
    /// SPOTIFLAC_TIMEOUT_SECONDS.
    pub timeout_seconds: i32,

    /// How many songs SpotiFLAC is asked for at once (1 to 4). SPOTIFLAC_AT_ONCE.
    pub at_once: i32,

    /// Mornye's own page as a browser reaches it, when the address above is one only Octo's
    /// container reaches. Blank: worked out from the address. SPOTIFLAC_WEB_URL.
    pub web_url: String,

    /// The port Mornye's page is published on, for working out that address. SPOTIFLAC_PORT.
    pub web_port: i32,

    /// Octo's assist helper beside Mornye, which "Pass Mornye's check" talks to. Blank: port
    /// 8011 of Octo's own spotiflac service, when the address is that service. SPOTIFLAC_ASSIST_URL.
    pub assist_url: String,
}

impl Default for SpotiflacSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            url: String::new(),
            token: String::new(),
            quality: "LOSSLESS".to_string(),
            services: String::new(),
            folder: String::new(),
            placement: SpotiflacPlacement::AfterSoulseek,
            timeout_seconds: 600,
            at_once: 2,
            web_url: String::new(),
            web_port: 8010,
            assist_url: String::new(),
        }
    }
}

/// The name of Octo's own Mornye service in its compose file.
pub const SPOTIFLAC_SERVICE_HOST: &str = "spotiflac";

/// The port of Octo's assist helper inside that service.
pub const ASSIST_PORT: u16 = 8011;

impl SpotiflacSettings {
    /// On, with an address to ask.
    pub fn is_on(&self) -> bool {
        self.enabled && !self.effective_url().is_empty()
    }

    /// Whether the address is Octo's own spotiflac service.
    pub fn is_octo_service(&self) -> bool {
        url::Url::parse(&self.effective_url())
            .ok()
            .and_then(|url| {
                url.host_str()
                    .map(|host| host.eq_ignore_ascii_case(SPOTIFLAC_SERVICE_HOST))
            })
            .unwrap_or(false)
    }

    /// The assist helper's address: the setting, or port 8011 of Octo's own service. None for a
    /// Mornye Octo does not run, with no address set.
    pub fn effective_assist_url(&self) -> Option<String> {
        let set = super::normalize_server_url(&self.assist_url);
        if !set.is_empty() {
            return Some(set);
        }
        self.is_octo_service()
            .then(|| format!("http://{SPOTIFLAC_SERVICE_HOST}:{ASSIST_PORT}"))
    }

    /// Mornye's page for a browser: the setting, else `http://<dashboard host>:<port>` for Octo's
    /// own service (the caller gives the dashboard's host), else the address.
    pub fn effective_web_url(&self, dashboard_host: &str) -> String {
        let set = super::normalize_server_url(&self.web_url);
        if !set.is_empty() {
            return set;
        }
        if self.is_octo_service() && !dashboard_host.trim().is_empty() {
            let port = if (1..=65535).contains(&self.web_port) {
                self.web_port
            } else {
                8010
            };
            return format!("http://{}:{port}", dashboard_host.trim());
        }
        self.effective_url()
    }

    /// The address, trimmed, with no trailing slash.
    pub fn effective_url(&self) -> String {
        super::normalize_server_url(&self.url)
    }

    /// The quality as SpotiFLAC names it: one of [`SPOTIFLAC_QUALITIES`], in any case, or
    /// `LOSSLESS`.
    pub fn effective_quality(&self) -> &'static str {
        let asked = self.quality.trim();
        SPOTIFLAC_QUALITIES
            .into_iter()
            .find(|known| known.eq_ignore_ascii_case(asked))
            .unwrap_or("LOSSLESS")
    }

    /// The extension ids, trimmed, blanks left out, each once, in the order given.
    pub fn effective_services(&self) -> Vec<String> {
        let mut services: Vec<String> = Vec::new();
        for service in self.services.split([',', '\n']) {
            let service = service.trim();
            if !service.is_empty() && !services.iter().any(|s| s == service) {
                services.push(service.to_string());
            }
        }
        services
    }

    /// The music folder as SpotiFLAC sees it, without a trailing slash, or None for the same
    /// path as Octo's.
    pub fn effective_folder(&self) -> Option<String> {
        let folder = self.folder.trim();
        let folder = if folder.len() > 1 {
            folder.trim_end_matches('/')
        } else {
            folder
        };
        (!folder.is_empty()).then(|| folder.to_string())
    }

    /// The time one song may take, 60 to 3600 seconds.
    pub fn effective_timeout_seconds(&self) -> u64 {
        self.timeout_seconds.clamp(60, 3600) as u64
    }

    /// Songs asked for at once, 1 to 4.
    pub fn effective_at_once(&self) -> usize {
        self.at_once.clamp(1, 4) as usize
    }

    /// Why a web token cannot be used, or None. The token travels in a cookie, whose value
    /// may hold only printable ASCII other than space, `"`, `,`, `;` and `\` (RFC 6265).
    pub fn token_problem(token: &str) -> Option<&'static str> {
        let fits = |c: char| matches!(c as u32, 0x21 | 0x23..=0x2B | 0x2D..=0x3A | 0x3C..=0x5B | 0x5D..=0x7E);
        (!token.chars().all(fits)).then_some(
            "The web token travels in a cookie, so it can't contain spaces, quotes, commas, semicolons, \
             backslashes or letters outside plain ASCII. Use letters, digits and simple symbols such as - _ . ~",
        )
    }

    /// Whether SpotiFLAC is asked before Soulseek for a song with these names.
    pub fn goes_first(&self, names: &[Option<&str>]) -> bool {
        match self.placement {
            SpotiflacPlacement::AfterSoulseek => false,
            SpotiflacPlacement::BeforeSoulseek => true,
            SpotiflacPlacement::BeforeSoulseekForAsianScripts => {
                names.iter().flatten().any(|name| in_asian_script(name))
            }
        }
    }
}

/// Whether the text has a letter of Chinese, Japanese (kana), Korean (hangul) or Thai script.
pub fn in_asian_script(text: &str) -> bool {
    text.chars().any(|c| {
        matches!(c as u32,
            0x0E00..=0x0E7F       // Thai
            | 0x1100..=0x11FF     // Hangul jamo
            | 0x3040..=0x309F     // Hiragana
            | 0x30A0..=0x30FF     // Katakana
            | 0x3130..=0x318F     // Hangul compatibility jamo
            | 0x3400..=0x4DBF     // CJK extension A
            | 0x4E00..=0x9FFF     // CJK unified ideographs
            | 0xAC00..=0xD7AF     // Hangul syllables
            | 0xF900..=0xFAFF     // CJK compatibility ideographs
            | 0xFF66..=0xFF9F     // Half-width katakana
            | 0x20000..=0x2FA1F   // CJK extensions B and later
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigTree;

    #[test]
    fn off_by_default_with_nothing_to_ask() {
        let settings = SpotiflacSettings::default();
        assert!(!settings.enabled);
        assert!(!settings.is_on());
        assert_eq!(settings.effective_quality(), "LOSSLESS");
        assert!(settings.effective_services().is_empty());
        assert_eq!(settings.effective_folder(), None);
        assert_eq!(settings.placement, SpotiflacPlacement::AfterSoulseek);
        assert_eq!(settings.effective_timeout_seconds(), 600);
        assert_eq!(settings.effective_at_once(), 2);
    }

    #[test]
    fn a_token_that_cannot_travel_in_a_cookie_is_refused() {
        assert_eq!(
            SpotiflacSettings::token_problem("abc-DEF_123.~!#$%&'()*+/:<=>?@[]^`{|}"),
            None
        );
        assert_eq!(SpotiflacSettings::token_problem(""), None);
        for bad in ["a b", "a\"b", "a,b", "a;b", "a\\b", "a\tb", "é"] {
            assert!(SpotiflacSettings::token_problem(bad).is_some(), "{bad:?}");
        }
    }

    #[test]
    fn on_needs_both_the_switch_and_an_address() {
        let mut settings = SpotiflacSettings {
            enabled: true,
            ..Default::default()
        };
        assert!(!settings.is_on());
        settings.url = " http://spotiflac:8000/ ".into();
        assert!(settings.is_on());
        assert_eq!(settings.effective_url(), "http://spotiflac:8000");
        settings.enabled = false;
        assert!(!settings.is_on());
    }

    #[test]
    fn binds_from_the_section_with_env_style_values() {
        let mut tree = ConfigTree::new();
        tree.set("Spotiflac:Enabled", Some("true".into()));
        tree.set("Spotiflac:Url", Some("http://spotiflac:8000".into()));
        tree.set("Spotiflac:Quality", Some("hi_res_lossless".into()));
        tree.set("Spotiflac:Services", Some(" ext:one, ext:two ,,ext:one".into()));
        tree.set("Spotiflac:Folder", Some("/data/music/".into()));
        tree.set(
            "Spotiflac:Placement",
            Some("BeforeSoulseekForAsianScripts".into()),
        );
        tree.set("Spotiflac:TimeoutSeconds", Some("5".into()));
        tree.set("Spotiflac:AtOnce", Some("9".into()));
        let (all, warnings) = crate::settings::AppSettings::bind(&tree);
        let settings = all.spotiflac;
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(settings.is_on());
        assert_eq!(settings.effective_quality(), "HI_RES_LOSSLESS");
        assert_eq!(settings.effective_services(), vec!["ext:one", "ext:two"]);
        assert_eq!(settings.effective_folder().as_deref(), Some("/data/music"));
        assert_eq!(
            settings.placement,
            SpotiflacPlacement::BeforeSoulseekForAsianScripts
        );
        assert_eq!(settings.effective_timeout_seconds(), 60);
        assert_eq!(settings.effective_at_once(), 4);
    }

    #[test]
    fn an_unknown_quality_is_asked_as_lossless() {
        let settings = SpotiflacSettings {
            quality: "MP3".into(),
            ..Default::default()
        };
        assert_eq!(settings.effective_quality(), "LOSSLESS");
    }

    #[test]
    fn placement_decides_who_goes_first() {
        let mut settings = SpotiflacSettings::default();
        let latin = [Some("Daft Punk"), Some("One More Time"), None];
        let korean = [Some("아이유"), Some("Blueming"), None];
        assert!(!settings.goes_first(&latin));
        assert!(!settings.goes_first(&korean));
        settings.placement = SpotiflacPlacement::BeforeSoulseek;
        assert!(settings.goes_first(&latin));
        settings.placement = SpotiflacPlacement::BeforeSoulseekForAsianScripts;
        assert!(!settings.goes_first(&latin));
        assert!(settings.goes_first(&korean));
    }

    #[test]
    fn asian_scripts_are_told_apart_from_latin_and_accents() {
        for text in [
            "周杰伦",
            "晴天",
            "あいみょん",
            "マリーゴールド",
            "아이유",
            "ลิซ่า",
            "ｱｲｳ",
        ] {
            assert!(in_asian_script(text), "{text}");
        }
        for text in ["Björk", "Sigur Rós", "Beyoncé", "Мумий Тролль", ""] {
            assert!(!in_asian_script(text), "{text}");
        }
    }
}
