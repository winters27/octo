//! What the heart gate's tests stand on: files whose sound is named in their own bytes, a printer
//! and a decoder that read that name, an AcoustID that answers each print by the name it was sent
//! under, a MusicBrainz that knows every recording id as itself, and the gate's service over them.

use std::collections::HashMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use octo_core::fingerprint::heart_release::{CatalogAlbum, CatalogAlbumHit, CatalogTrack};
use octo_core::settings::{AppSettings, SettingsStore, SoulseekSettings};
use octo_media::audio::RawFingerprint;
use parking_lot::Mutex;
use url::Url;
use wiremock::matchers::{any, path_regex};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use super::acoust_id_client::AcoustIdClient;
use super::acoust_id_rate_limit_handler::AcoustIdRateLimitHandler;
use super::acoust_id_rate_limiter::AcoustIdRateLimiter;
use super::catalog_clip::CatalogClip;
use super::download_verification_service::{DownloadVerificationService, HeartParts};
use super::heard_master_check::{DecodeRun, Decoder, HeardMasterCheck};
use super::music_brainz_client::{CatalogQuestions, Lane, MusicBrainzClient};
use crate::services::common::soulseek_hold_store::SoulseekHoldStore;
use crate::services::metadata::deezer_metadata_service::{DeezerAsked, DeezerMetadataService};
use crate::services::metadata::deezer_rate_limit_handler::DeezerRateLimitHandler;
use crate::services::metadata::deezer_rate_limiter::DeezerRateLimiter;
use crate::services::prefetch::heard_samples::tests::{Rig, items_of, rig_in};
use crate::services::prefetch::heard_samples::{ITEM_SECONDS, Printer};
use crate::services::soulseek::rejected_peer_registry::RejectedPeerRegistry;
use crate::services::you_tube::YouTubeResolver;

/// Octo's own AcoustID key in these tests.
pub(crate) const OCTO_KEY: &str = "octo-key";

const MARK: &[u8] = b"OCTO-SOUND[";

/// The bytes that name a file's sound: `print` names its fingerprint (files of one print name
/// print alike), `master` its audio (files of one master name are one master, window by window),
/// and `answer` the name its print is sent to AcoustID under.
pub(crate) fn sound_bytes(print: &str, master: &str, answer: &str) -> Vec<u8> {
    let mut bytes = MARK.to_vec();
    bytes.extend_from_slice(format!("{print}|{master}|{answer}]").as_bytes());
    bytes
}

/// The sound a file's bytes name: (print, master, answer).
fn sound_in(bytes: &[u8]) -> Option<(String, String, String)> {
    let start = bytes.windows(MARK.len()).position(|w| w == MARK)? + MARK.len();
    let end = start + bytes[start..].iter().position(|b| *b == b']')?;
    let text = std::str::from_utf8(&bytes[start..end]).ok()?;
    let mut parts = text.split('|');
    Some((
        parts.next()?.to_string(),
        parts.next()?.to_string(),
        parts.next()?.to_string(),
    ))
}

/// A FLAC header that says `seconds` long, as a peer's file of that length lists.
pub(crate) fn flac_header(seconds: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"fLaC");
    bytes.extend_from_slice(&[0x80, 0x00, 0x00, 0x22]);
    bytes.extend_from_slice(&[0x10, 0x00, 0x10, 0x00]);
    bytes.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
    let (sample_rate, channels_minus_one, bits_minus_one): (u64, u64, u64) = (44100, 1, 15);
    let total_samples = 44100u64 * seconds;
    let packed = (sample_rate << 44) | (channels_minus_one << 41) | (bits_minus_one << 36) | total_samples;
    bytes.extend_from_slice(&packed.to_be_bytes());
    bytes.extend_from_slice(&[0u8; 16]);
    bytes
}

/// A FLAC of `seconds` at `path` whose sound is named, with `isrc` as its own ISRC tag.
pub(crate) fn sound_flac(path: &Path, seconds: u64, sound: &[u8], isrc: Option<&str>) {
    let mut bytes = flac_header(seconds);
    bytes.extend_from_slice(sound);
    std::fs::write(path, bytes).expect("the file is written");
    if let Some(isrc) = isrc {
        let mut file = octo_media::tags::TagFile::open(path).expect("the FLAC opens");
        octo_media::tags::tag_writer_extras::set_text(&mut file, octo_media::tags::TagFields::ISRC, Some(isrc));
        file.save().expect("the tag is saved");
    }
}

fn seed_of(name: &str) -> u32 {
    name.bytes()
        .fold(2_166_136_261u32, |hash, byte| (hash ^ u32::from(byte)).wrapping_mul(16_777_619))
}

/// `seconds` of print items for a print name: one name, one print.
pub(crate) fn print_items(name: &str, seconds: f64) -> Vec<u32> {
    let mut x = seed_of(name);
    (0..(seconds / ITEM_SECONDS) as usize)
        .map(|_| {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            x
        })
        .collect()
}

/// Prints a file from the sound its bytes name, or a frozen `fpcalc -raw -json` print as it is.
/// A print name registered with [`SoundPrinter::register`] prints as the items given.
#[derive(Default)]
pub(crate) struct SoundPrinter {
    prints: Mutex<HashMap<String, Vec<u32>>>,
}

impl SoundPrinter {
    /// Files whose print is `name` print as `items`.
    pub(crate) fn register(&self, name: &str, items: Vec<u32>) {
        self.prints.lock().insert(name.to_string(), items);
    }

    /// The items files whose print is `name` print as.
    pub(crate) fn items(&self, name: &str) -> Vec<u32> {
        self.prints
            .lock()
            .get(name)
            .cloned()
            .unwrap_or_else(|| print_items(name, 240.0))
    }

    fn read(&self, path: &Path) -> Option<(Vec<u32>, String)> {
        let bytes = std::fs::read(path).ok()?;
        if let Some((print, _, answer)) = sound_in(&bytes) {
            return Some((self.items(&print), answer));
        }
        let items = items_of(path);
        let name = path.file_stem()?.to_string_lossy().to_string();
        (!items.is_empty()).then_some((items, name))
    }
}

#[async_trait]
impl Printer for SoundPrinter {
    async fn raw(&self, path: &Path, length_seconds: i32, _: i32) -> Option<RawFingerprint> {
        let (items, _) = self.read(path)?;
        let seconds = items.len() as f64 * ITEM_SECONDS;
        let kept = if length_seconds > 0 {
            items
                .into_iter()
                .take((f64::from(length_seconds) / ITEM_SECONDS) as usize)
                .collect()
        } else {
            items
        };
        Some(RawFingerprint { items: kept, seconds })
    }

    async fn compressed(&self, path: &Path, length_seconds: i32, _: i32) -> Option<(String, i32)> {
        let (items, answer) = self.read(path)?;
        let seconds = (items.len() as f64 * ITEM_SECONDS) as i32;
        let seconds = if length_seconds > 0 {
            seconds.min(length_seconds)
        } else {
            seconds
        };
        Some((answer, seconds))
    }
}

/// The seconds of audio a master decodes to: enough long windows for the heard-master rule.
const MASTER_SECONDS: f64 = 24.0;
const MASTER_RATE: f64 = 11_025.0;

/// A master's audio as `f32le` stereo at 11,025 Hz: beating sine voices and drum hits whose
/// pitches and times come from the master's name, so two names are two masters.
fn master_audio(name: &str) -> Vec<u8> {
    let mut x = seed_of(name);
    let mut noise = move || {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        f64::from(x) / 4_294_967_296.0 * 2.0 - 1.0
    };
    let voices: Vec<(f64, f64, f64)> = (0..6)
        .map(|i| {
            let frequency = 110.0 * (1.0 + f64::from(i) * 0.5) * (1.0 + 0.2 * noise());
            (frequency, 0.5 + f64::from(i) * 0.25, -0.8 + f64::from(i) * 0.33)
        })
        .collect();
    let mut hits = vec![0.2];
    while *hits.last().expect("a hit") < MASTER_SECONDS {
        let gap = 0.3 + 0.4 * (noise() + 1.0) / 2.0;
        hits.push(hits.last().expect("a hit") + gap);
    }
    let frames = (MASTER_SECONDS * MASTER_RATE) as usize;
    let mut out = Vec::with_capacity(frames * 8);
    let mut last = 0;
    for i in 0..frames {
        let t = i as f64 / MASTER_RATE;
        let mut frame = [0.0f64; 2];
        for &(frequency, beat, pan) in &voices {
            let value = 0.15
                * (0.5 + 0.5 * (2.0 * std::f64::consts::PI * beat * t).cos())
                * (2.0 * std::f64::consts::PI * frequency * t).sin();
            frame[0] += value * ((pan + 1.0) * std::f64::consts::PI / 4.0).cos();
            frame[1] += value * ((pan + 1.0) * std::f64::consts::PI / 4.0).sin();
        }
        while last + 1 < hits.len() && hits[last + 1] <= t {
            last += 1;
        }
        if t >= hits[0] {
            let drum = noise() * 0.3 * (-(t - hits[last]) / 0.05).exp();
            frame[0] += drum;
            frame[1] += drum;
        }
        out.extend_from_slice(&(frame[0] as f32).to_le_bytes());
        out.extend_from_slice(&(frame[1] as f32).to_le_bytes());
    }
    out
}

/// Decodes a file to the master its bytes name; a file that names none decodes to nothing.
#[derive(Default)]
pub(crate) struct SoundDecoder {
    made: Mutex<HashMap<String, Arc<Vec<u8>>>>,
}

impl Decoder for SoundDecoder {
    fn open(&self, path: &Path) -> std::io::Result<DecodeRun> {
        let bytes = std::fs::read(path)?;
        let audio = match sound_in(&bytes) {
            Some((_, master, _)) => {
                let made = self.made.lock().get(&master).cloned();
                match made {
                    Some(audio) => audio,
                    None => {
                        let audio = Arc::new(master_audio(&master));
                        self.made.lock().insert(master, audio.clone());
                        audio
                    }
                }
            }
            None => Arc::new(Vec::new()),
        };
        Ok(DecodeRun {
            output: Box::new(Cursor::new(audio.as_ref().clone())),
            stop: Box::new(|| {}),
            wait: Box::new(|| true),
        })
    }
}

/// AcoustID, answering each lookup by the fingerprint it was sent: a body set for that name, else
/// a status set for it, else no entry.
#[derive(Clone, Default)]
pub(crate) struct AcoustIdByPrint {
    answers: Arc<Mutex<HashMap<String, (u16, String)>>>,
    asked: Arc<Mutex<Vec<String>>>,
}

impl AcoustIdByPrint {
    /// Prints sent as `name` are answered `body`.
    pub(crate) fn answer(&self, name: &str, body: &str) {
        self.answers
            .lock()
            .insert(name.to_string(), (200, body.to_string()));
    }

    /// Prints sent as `name` are answered with `status` and no body.
    pub(crate) fn status(&self, name: &str, status: u16) {
        self.answers
            .lock()
            .insert(name.to_string(), (status, String::new()));
    }

    /// The names prints were sent as, in order.
    pub(crate) fn asked(&self) -> Vec<String> {
        self.asked.lock().clone()
    }
}

impl Respond for AcoustIdByPrint {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let name = url::form_urlencoded::parse(&request.body)
            .find(|(key, _)| key == "fingerprint")
            .map(|(_, value)| value.into_owned())
            .unwrap_or_default();
        self.asked.lock().push(name.clone());
        match self.answers.lock().get(&name).cloned() {
            Some((200, body)) => ResponseTemplate::new(200).set_body_raw(body, "application/json"),
            Some((status, _)) => ResponseTemplate::new(status),
            None => ResponseTemplate::new(200)
                .set_body_raw(r#"{"status":"ok","results":[]}"#, "application/json"),
        }
    }
}

/// The frozen AcoustID answer `name` (a `gate/answers` file), as AcoustID sent it.
pub(crate) fn frozen_body(name: &str) -> String {
    let text = std::fs::read_to_string(gate_fixture(&format!("answers/{name}.json"))).expect("the frozen answer");
    let root: serde_json::Value = serde_json::from_str(&text).expect("JSON");
    root["answer"].to_string()
}

/// A file under the frozen gate fixtures.
pub(crate) fn gate_fixture(name: &str) -> PathBuf {
    PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../octo-core/tests/fixtures/heart_album/gate"
    ))
    .join(name)
}

/// MusicBrainz, knowing every recording id as itself and each code by the recordings set for it;
/// any other question is one it does not answer.
#[derive(Clone, Default)]
pub(crate) struct MusicBrainzByQuestion {
    codes: Arc<Mutex<HashMap<String, (u16, String)>>>,
    asked: Arc<Mutex<Vec<String>>>,
}

impl MusicBrainzByQuestion {
    /// The code lookup of `code` answers `body`.
    pub(crate) fn code(&self, code: &str, body: &str) {
        self.codes
            .lock()
            .insert(code.to_string(), (200, body.to_string()));
    }

    /// The code lookup of `code` answers `status`.
    pub(crate) fn code_status(&self, code: &str, status: u16) {
        self.codes
            .lock()
            .insert(code.to_string(), (status, String::new()));
    }

    /// The paths asked, in order.
    pub(crate) fn asked(&self) -> Vec<String> {
        self.asked.lock().clone()
    }
}

impl Respond for MusicBrainzByQuestion {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let path = request.url.path().to_string();
        self.asked.lock().push(path.clone());
        if let Some(id) = path.strip_prefix("/ws/2/recording/") {
            return ResponseTemplate::new(200)
                .set_body_raw(format!(r#"{{"id":"{id}","title":"x"}}"#), "application/json");
        }
        if let Some(code) = path.strip_prefix("/ws/2/isrc/") {
            return match self.codes.lock().get(code).cloned() {
                Some((200, body)) => ResponseTemplate::new(200).set_body_raw(body, "application/json"),
                Some((status, _)) => ResponseTemplate::new(status),
                None => ResponseTemplate::new(404),
            };
        }
        ResponseTemplate::new(500)
    }
}

/// Deezer, never asked by these tests' identities.
pub(crate) struct NoCatalog;

#[async_trait]
impl CatalogQuestions for NoCatalog {
    async fn album_search(&self, _: &str, _: &str, _: Lane) -> DeezerAsked<Vec<CatalogAlbumHit>> {
        DeezerAsked::NoData
    }

    async fn album(&self, _: &str, _: Lane) -> DeezerAsked<(CatalogAlbum, Vec<CatalogTrack>)> {
        DeezerAsked::NoData
    }
}

/// How a gate's service is set up.
pub(crate) struct GateSetup {
    /// AcoustID turned off in the settings.
    pub acoust_id_off: bool,
    /// MusicBrainz at its own mock server.
    pub music_brainz: bool,
    /// Deezer's side of a heart's questions.
    pub catalog: Arc<dyn CatalogQuestions>,
}

impl Default for GateSetup {
    fn default() -> Self {
        Self {
            acoust_id_off: false,
            music_brainz: true,
            catalog: Arc::new(NoCatalog),
        }
    }
}

/// The heart gate's service over mock hosts: AcoustID answering by print, the shim and Deezer on
/// one server (clips fetched from it), MusicBrainz knowing ids as themselves, and every print and
/// decode read from the sounds files name.
pub(crate) struct GateRig {
    pub service: Arc<DownloadVerificationService>,
    pub rig: Rig,
    pub holds: Arc<SoulseekHoldStore>,
    pub settings: Arc<SettingsStore>,
    pub acoust_id: MockServer,
    pub answers: AcoustIdByPrint,
    /// The shim, Deezer and the clips' CDN.
    pub shim: MockServer,
    pub music_brainz: Option<(MockServer, MusicBrainzByQuestion, Arc<MusicBrainzClient>)>,
    pub master: Arc<HeardMasterCheck>,
    pub printer: Arc<SoundPrinter>,
    pub rejected: Arc<RejectedPeerRegistry>,
    pub limiter: Arc<AcoustIdRateLimiter>,
    pub dir: tempfile::TempDir,
}

pub(crate) async fn gate_rig(setup: GateSetup) -> GateRig {
    let acoust_id = MockServer::start().await;
    let answers = AcoustIdByPrint::default();
    Mock::given(path_regex("^/v2/lookup$"))
        .respond_with(answers.clone())
        .mount(&acoust_id)
        .await;
    let shim = MockServer::start().await;
    let dir = tempfile::tempdir().expect("a temp dir");
    let holds = Arc::new(SoulseekHoldStore::new(None::<PathBuf>));
    let rig = rig_in(dir.path(), &shim.uri(), holds.clone(), 16, 64 << 20);
    let printer = Arc::new(SoundPrinter::default());
    rig.samples.use_printer(printer.clone());
    let settings = Arc::new(SettingsStore::for_tests(AppSettings {
        soulseek: SoulseekSettings {
            acoust_id_timeout_seconds: 2,
            acoust_id_api_key: if setup.acoust_id_off {
                octo_core::settings::soulseek::ACOUSTID_OFF.to_string()
            } else {
                String::new()
            },
            ..Default::default()
        },
        ..Default::default()
    }));
    let limiter = Arc::new(
        AcoustIdRateLimiter::with_gaps(Duration::from_millis(1), Duration::from_millis(20)).with_jitter(|| 1.0),
    );
    let base = Url::parse(&format!("{}/", acoust_id.uri())).expect("parses");
    let client = Arc::new(AcoustIdClient::with_built_in_key(
        Arc::new(AcoustIdRateLimitHandler::with_base_url(limiter.clone(), base)),
        settings.clone(),
        Some(OCTO_KEY),
    ));
    let music_brainz = if setup.music_brainz {
        let server = MockServer::start().await;
        let questions = MusicBrainzByQuestion::default();
        Mock::given(any())
            .respond_with(questions.clone())
            .mount(&server)
            .await;
        let base = Url::parse(&format!("{}/ws/2/", server.uri())).expect("parses");
        let quick = Duration::from_millis(5);
        let client = Arc::new(MusicBrainzClient::with_base_url(base).with_pace(quick, Duration::from_millis(500), quick));
        Some((server, questions, client))
    } else {
        None
    };
    let service = Arc::new(DownloadVerificationService::new(
        Arc::new(octo_media::audio::AudioFingerprinter::new()),
        client,
        settings.clone(),
        music_brainz.as_ref().map(|(_, _, client)| client.clone()),
        None,
    ));
    let clip = CatalogClip::new(
        Arc::new(DeezerMetadataService::with_base_url(
            Arc::new(DeezerRateLimitHandler::new(Arc::new(DeezerRateLimiter::new()))),
            settings.clone(),
            shim.uri(),
        )),
        rig.samples.clone(),
        Arc::new(YouTubeResolver::with_base_url(Some(&shim.uri()))),
    );
    let cdn = Url::parse(&shim.uri()).expect("parses");
    clip.use_clip_hosts(Arc::new(move |url: &Url| {
        url.host_str() == cdn.host_str() && url.port() == cdn.port()
    }));
    let master = Arc::new(HeardMasterCheck::new(rig.samples.clone()));
    master.use_decoder(Arc::new(SoundDecoder::default()));
    let rejected = Arc::new(RejectedPeerRegistry::in_memory());
    service.use_heart(HeartParts {
        registry: rig.registry.clone(),
        samples: rig.samples.clone(),
        master: master.clone(),
        clip: Arc::new(clip),
        catalog: setup.catalog,
        holds: holds.clone(),
        rejected: rejected.clone(),
    });
    GateRig {
        service,
        rig,
        holds,
        settings,
        acoust_id,
        answers,
        shim,
        music_brainz,
        master,
        printer,
        rejected,
        limiter,
        dir,
    }
}

impl GateRig {
    /// The shim's fresh `/stream` of `video` sends `bytes`.
    pub(crate) async fn shim_streams(&self, video: &str, bytes: Vec<u8>) {
        Mock::given(wiremock::matchers::path("/stream"))
            .and(wiremock::matchers::query_param("id", video))
            .and(wiremock::matchers::query_param("fresh", "1"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("X-Octo-Fresh", "1")
                    .set_body_raw(bytes, "audio/mp4"),
            )
            .mount(&self.shim)
            .await;
    }

    /// How often the shim's fresh `/stream` was asked for `video`.
    pub(crate) async fn streams_of(&self, video: &str) -> usize {
        self.shim
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.url.path() == "/stream")
            .filter(|r| r.url.query_pairs().any(|(k, v)| k == "id" && v == video))
            .count()
    }

    /// The MusicBrainz mock's questions.
    pub(crate) fn music_brainz(&self) -> &MusicBrainzByQuestion {
        &self.music_brainz.as_ref().expect("MusicBrainz is set up").1
    }
}
