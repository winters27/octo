//! The heard-master check as Octo runs it: a file against the very audio a person heard, by
//! sound, window by window, both ways (`octo_core::fingerprint::heard_master`).
//!
//! The heard audio is made local once per round, a round being one pass of a heart's chain or one
//! re-check: the held copy as it is, else one fresh fetch of the heard video on the shim's
//! background lane into a scratch file. Every candidate of the round is compared with that one
//! copy, so five peers make one fetch. A round takes one of two heard-copy places before it makes
//! its copy, and gives it back (deleting a fetched copy) when it ends or waits on something
//! outside, so at most two heard copies exist at once and a long wait never keeps one. The rounds
//! of one acquisition (one per person who asked) take turns at one place: a round about to wait
//! for a place first frees the place its siblings hold, so people judged in one pass never wait on
//! each other's places.
//!
//! The comparison takes one `fpcalc` place (the queue every heart's sound work shares) once both
//! files are local, and runs off the async threads: three passes over each file, each decoded by
//! ffmpeg to 11,025 Hz stereo under the fingerprint timeout. A decode that fails or runs out of
//! time leaves the file undecided, never accepted.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use octo_core::fingerprint::heard_master::{
    BlockSums, F32leFrames, FirstPass, HeardMasterVerdict, RefinePlan, Side, StretchPass, verdict,
};
use octo_core::fingerprint::heart_gate::{HeardMaster, HeardRecord};
use parking_lot::Mutex;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::{debug, warn};

use crate::services::prefetch::heard_samples::{
    FpcalcClass, HeardFetch, HeardFile, HeardSamples, WHOLE_SONG_CAP_BYTES,
};

/// How many heard copies exist at once.
pub const HEARD_COPIES_AT_ONCE: usize = 2;
/// Bytes one read of a decode takes.
const READ_BYTES: usize = 64 * 1024;

/// What a heard-master check compares a file with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeardAudio {
    /// What a person heard: its copy is made from the held copy, else one fresh fetch.
    Heard(HeardRecord),
    /// A local file that is the heard audio itself (a kept-as-heard file, a library original).
    File(PathBuf),
}

/// Starts decodes of audio files to `f32le` stereo at 11,025 Hz.
pub trait Decoder: Send + Sync {
    fn open(&self, path: &Path) -> std::io::Result<DecodeRun>;
}

/// One running decode: its output, and a way to stop it from another thread.
pub struct DecodeRun {
    pub output: Box<dyn Read + Send>,
    pub stop: Box<dyn Fn() + Send + Sync>,
    /// Waits for the decode's end: true when it ended well.
    pub wait: Box<dyn FnOnce() -> bool + Send>,
}

/// ffmpeg, as Octo's image has it.
pub struct Ffmpeg;

impl Decoder for Ffmpeg {
    fn open(&self, path: &Path) -> std::io::Result<DecodeRun> {
        let mut child = Command::new("ffmpeg")
            .args(["-v", "error", "-nostdin", "-i"])
            .arg(path)
            .args(["-vn", "-f", "f32le", "-ac", "2", "-ar", "11025", "-"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let output = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("no stdout"))?;
        let child: Arc<Mutex<Child>> = Arc::new(Mutex::new(child));
        let stopper = Arc::clone(&child);
        Ok(DecodeRun {
            output: Box::new(output),
            stop: Box::new(move || {
                let _ = stopper.lock().kill();
            }),
            wait: Box::new(move || child.lock().wait().is_ok_and(|status| status.success())),
        })
    }
}

/// One decode under its timeout: a watchdog stops it when the time runs out.
struct Decode {
    output: Box<dyn Read + Send>,
    frames: F32leFrames,
    buffer: Vec<u8>,
    timed_out: Arc<AtomicBool>,
    stop: Arc<dyn Fn() + Send + Sync>,
    done: Option<mpsc::Sender<()>>,
    watchdog: Option<std::thread::JoinHandle<()>>,
    wait: Option<Box<dyn FnOnce() -> bool + Send>>,
    failed: bool,
}

impl Decode {
    fn start(decoder: &dyn Decoder, path: &Path, timeout: Duration) -> Option<Decode> {
        let run = match decoder.open(path) {
            Ok(run) => run,
            Err(e) => {
                warn!("ffmpeg could not start for the heard-master check: {e}");
                return None;
            }
        };
        let timed_out = Arc::new(AtomicBool::new(false));
        let (done, finished) = mpsc::channel::<()>();
        let flag = Arc::clone(&timed_out);
        let stop: Arc<dyn Fn() + Send + Sync> = Arc::from(run.stop);
        let stopper = Arc::clone(&stop);
        let watchdog = std::thread::spawn(move || {
            if let Err(mpsc::RecvTimeoutError::Timeout) = finished.recv_timeout(timeout) {
                flag.store(true, Ordering::SeqCst);
                stopper();
            }
        });
        Some(Decode {
            output: run.output,
            frames: F32leFrames::new(),
            buffer: vec![0; READ_BYTES],
            timed_out,
            stop,
            done: Some(done),
            watchdog: Some(watchdog),
            wait: Some(run.wait),
            failed: false,
        })
    }

    /// Stops a decode whose rest is not needed, and waits for its end.
    fn abandon(mut self) {
        (self.stop)();
        drop(self.done.take());
        if let Some(watchdog) = self.watchdog.take() {
            let _ = watchdog.join();
        }
        if let Some(wait) = self.wait.take() {
            wait();
        }
    }

    /// The next frames into `out` (cleared first): false once the decode has ended.
    fn next(&mut self, out: &mut Vec<[f32; 2]>) -> bool {
        out.clear();
        while out.is_empty() {
            match self.output.read(&mut self.buffer) {
                Ok(0) => return false,
                Ok(read) => self.frames.push(&self.buffer[..read], out),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => {
                    self.failed = true;
                    return false;
                }
            }
        }
        true
    }

    /// Ends the decode: true when it read to its end in time and the decoder ended well.
    fn finish(mut self) -> bool {
        drop(self.done.take());
        if let Some(watchdog) = self.watchdog.take() {
            let _ = watchdog.join();
        }
        let ended_well = self.wait.take().is_some_and(|wait| wait());
        ended_well && !self.failed && !self.timed_out.load(Ordering::SeqCst)
    }
}

/// Decodes `path` whole into `push`; false when the decode failed or ran out of time.
fn decode_into(
    decoder: &dyn Decoder,
    path: &Path,
    timeout: Duration,
    mut push: impl FnMut(&[[f32; 2]]) -> bool,
) -> bool {
    let Some(mut decode) = Decode::start(decoder, path, timeout) else {
        return false;
    };
    let mut frames = Vec::new();
    while decode.next(&mut frames) {
        if !push(&frames) {
            // Everything this pass needs is read: the rest is not decoded.
            let in_time = !decode.timed_out.load(Ordering::SeqCst);
            decode.abandon();
            return in_time;
        }
    }
    decode.finish()
}

/// The three passes over `heard` and `file`: the verdict, with each window's residual and why it
/// passed or failed, or None when a decode failed or ran out of time.
pub fn run_measured(
    decoder: &dyn Decoder,
    heard: &Path,
    file: &Path,
    timeout: Duration,
) -> Option<HeardMasterVerdict> {
    let mut first_heard = FirstPass::new();
    if !decode_into(decoder, heard, timeout, |frames| {
        first_heard.push(frames);
        true
    }) {
        return None;
    }
    let mut first_file = FirstPass::new();
    if !decode_into(decoder, file, timeout, |frames| {
        first_file.push(frames);
        true
    }) {
        return None;
    }
    let plan = RefinePlan::new(&first_heard, &first_file);
    if plan.is_empty() {
        return Some(HeardMasterVerdict::no_shift());
    }
    let mut heard_stretches = StretchPass::new(&plan.heard_ranges());
    if !decode_into(decoder, heard, timeout, |frames| {
        heard_stretches.push(frames);
        !heard_stretches.is_done()
    }) {
        return None;
    }
    let mut file_stretches = StretchPass::new(&plan.file_ranges());
    if !decode_into(decoder, file, timeout, |frames| {
        file_stretches.push(frames);
        !file_stretches.is_done()
    }) {
        return None;
    }
    let shift = plan.pick_shift(&heard_stretches.finish(), &file_stretches.finish())?;
    let mut sums = BlockSums::new(shift);
    let mut heard_decode = Decode::start(decoder, heard, timeout)?;
    let Some(mut file_decode) = Decode::start(decoder, file, timeout) else {
        heard_decode.abandon();
        return None;
    };
    let (mut heard_open, mut file_open) = (true, true);
    let mut frames = Vec::new();
    while let Some(side) = sums.behind() {
        match side {
            Side::Heard if heard_open && heard_decode.next(&mut frames) => sums.push_heard(&frames),
            Side::Heard => {
                heard_open = false;
                sums.end_heard();
            }
            Side::File if file_open && file_decode.next(&mut frames) => sums.push_file(&frames),
            Side::File => {
                file_open = false;
                sums.end_file();
            }
        }
    }
    let heard_ok = heard_decode.finish();
    let file_ok = file_decode.finish();
    if !heard_ok || !file_ok {
        return None;
    }
    let reading = verdict(shift, &sums.finish());
    debug!(
        "heard master: {} (shift {shift}, long worst {:?}, short worst {:?})",
        reading.reason.as_str(),
        reading.long_worst(),
        reading.short_worst()
    );
    Some(reading)
}

/// The open rounds, by acquisition and person.
type Rounds = Mutex<HashMap<(String, String), HeardRound>>;

/// What every round shares.
struct Parts {
    samples: Arc<HeardSamples>,
    permits: Arc<Semaphore>,
    decoder: Mutex<Arc<dyn Decoder>>,
    timeout: Mutex<Option<Duration>>,
    rounds: Rounds,
}

impl Parts {
    fn timeout(&self) -> Duration {
        self.timeout
            .lock()
            .unwrap_or_else(|| self.samples.fingerprint_timeout())
    }
}

/// A round's heard copy and the place it holds; the copy goes when the last user lets go.
struct RoundCopy {
    file: HeardFile,
    _permit: OwnedSemaphorePermit,
}

struct RoundInner {
    parts: Arc<Parts>,
    heard: HeardAudio,
    /// The acquisition the round judges files of, whose rounds take turns at one place.
    acquisition: Option<String>,
    copy: Mutex<Option<Arc<RoundCopy>>>,
    /// One copy is made at a time.
    making: tokio::sync::Mutex<()>,
}

/// One open round: a cheap handle a verify step finds by the acquisition's key.
#[derive(Clone)]
pub struct HeardRound(Arc<RoundInner>);

impl HeardRound {
    fn new(parts: Arc<Parts>, heard: HeardAudio, acquisition: Option<String>) -> Self {
        HeardRound(Arc::new(RoundInner {
            parts,
            heard,
            acquisition,
            copy: Mutex::new(None),
            making: tokio::sync::Mutex::new(()),
        }))
    }

    /// The other open rounds of this round's acquisition give their places back, so this one
    /// never waits for a place while its own acquisition holds one.
    fn pause_siblings(&self) {
        let Some(acquisition) = &self.0.acquisition else {
            return;
        };
        let siblings: Vec<HeardRound> = self
            .0
            .parts
            .rounds
            .lock()
            .iter()
            .filter(|((key, _), round)| key == acquisition && !Arc::ptr_eq(&round.0, &self.0))
            .map(|(_, round)| round.clone())
            .collect();
        for round in siblings {
            round.pause();
        }
    }

    /// Whether `file` is the master the person heard. The round's heard copy is made at its first
    /// call and serves every later one.
    pub async fn check(&self, file: &Path) -> HeardMaster {
        self.check_measured(file).await.0
    }

    /// [`Self::check`] with what the check measured, when both files were decoded.
    pub async fn check_measured(&self, file: &Path) -> (HeardMaster, Option<HeardMasterVerdict>) {
        let copy = match self.copy().await {
            Ok(copy) => copy,
            Err(answer) => return (answer, None),
        };
        let parts = &self.0.parts;
        let _slot = parts.samples.slot(FpcalcClass::Heart).await;
        let decoder = parts.decoder.lock().clone();
        let timeout = parts.timeout();
        let heard = copy.file.path().to_path_buf();
        let file = file.to_path_buf();
        let reading =
            tokio::task::spawn_blocking(move || run_measured(decoder.as_ref(), &heard, &file, timeout)).await;
        drop(copy);
        match reading {
            Ok(Some(measured)) if measured.passed() => (HeardMaster::Same, Some(measured)),
            Ok(Some(measured)) => (HeardMaster::Differs, Some(measured)),
            Ok(None) | Err(_) => (HeardMaster::Undecided, None),
        }
    }

    /// The round's heard copy, made now when it has none.
    async fn copy(&self) -> Result<Arc<RoundCopy>, HeardMaster> {
        let _one = self.0.making.lock().await;
        if let Some(copy) = self.0.copy.lock().clone() {
            return Ok(copy);
        }
        self.pause_siblings();
        let permit = Arc::clone(&self.0.parts.permits)
            .acquire_owned()
            .await
            .map_err(|_| HeardMaster::Undecided)?;
        let file = match &self.0.heard {
            HeardAudio::File(path) if path.is_file() => HeardFile::Held(path.clone()),
            HeardAudio::File(_) => return Err(HeardMaster::HeardGone),
            HeardAudio::Heard(record) => match self
                .0
                .parts
                .samples
                .heard_file(record, WHOLE_SONG_CAP_BYTES)
                .await
            {
                HeardFetch::Ready(file) => file,
                HeardFetch::Busy => return Err(HeardMaster::Busy),
                HeardFetch::Gone => return Err(HeardMaster::HeardGone),
            },
        };
        let copy = Arc::new(RoundCopy {
            file,
            _permit: permit,
        });
        *self.0.copy.lock() = Some(Arc::clone(&copy));
        Ok(copy)
    }

    /// The round waits on something outside it: its copy goes and its place is freed. The next
    /// check makes the copy again.
    pub fn pause(&self) {
        self.0.copy.lock().take();
    }

    /// Whether the round holds a heard copy now.
    pub fn has_copy(&self) -> bool {
        self.0.copy.lock().is_some()
    }
}

/// Ends its round when dropped: the round's copy goes and its place is freed.
pub struct RoundGuard {
    key: (String, String),
    round: HeardRound,
}

impl RoundGuard {
    /// The round this guard keeps open.
    pub fn round(&self) -> HeardRound {
        self.round.clone()
    }
}

impl Drop for RoundGuard {
    fn drop(&mut self) {
        let mut rounds = self.round.0.parts.rounds.lock();
        if rounds
            .get(&self.key)
            .is_some_and(|open| Arc::ptr_eq(&open.0, &self.round.0))
        {
            rounds.remove(&self.key);
        }
        drop(rounds);
        self.round.pause();
    }
}

/// The heard-master runner: rounds by acquisition and person, the two heard-copy places, and the
/// decoder.
pub struct HeardMasterCheck {
    parts: Arc<Parts>,
}

impl HeardMasterCheck {
    pub fn new(samples: Arc<HeardSamples>) -> Self {
        HeardMasterCheck {
            parts: Arc::new(Parts {
                samples,
                permits: Arc::new(Semaphore::new(HEARD_COPIES_AT_ONCE)),
                decoder: Mutex::new(Arc::new(Ffmpeg)),
                timeout: Mutex::new(None),
                rounds: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Decodes with `decoder` from now on, in place of ffmpeg.
    pub fn use_decoder(&self, decoder: Arc<dyn Decoder>) {
        *self.parts.decoder.lock() = decoder;
    }

    /// Gives each decode `timeout` from now on, in place of the fingerprint timeout.
    pub fn set_decode_timeout(&self, timeout: Duration) {
        *self.parts.timeout.lock() = Some(timeout);
    }

    /// How many heard-copy places are free.
    pub fn free_copy_places(&self) -> usize {
        self.parts.permits.available_permits()
    }

    /// Opens a round for `user` in the acquisition `acquisition_key`, comparing with `heard`;
    /// dropping the guard ends it.
    pub fn open_round(&self, acquisition_key: &str, user: &str, heard: HeardAudio) -> RoundGuard {
        let key = (acquisition_key.to_string(), user.trim().to_lowercase());
        let round = HeardRound::new(Arc::clone(&self.parts), heard, Some(acquisition_key.to_string()));
        self.parts.rounds.lock().insert(key.clone(), round.clone());
        RoundGuard { key, round }
    }

    /// The open round of `user` in `acquisition_key`.
    pub fn round(&self, acquisition_key: &str, user: &str) -> Option<HeardRound> {
        self.parts
            .rounds
            .lock()
            .get(&(acquisition_key.to_string(), user.trim().to_lowercase()))
            .cloned()
    }

    /// Every open round of `acquisition_key` gives its place back: the acquisition waits on
    /// something outside it, and the next check makes its copy again.
    pub fn pause_acquisition(&self, acquisition_key: &str) {
        let open: Vec<HeardRound> = self
            .parts
            .rounds
            .lock()
            .iter()
            .filter(|((key, _), _)| key == acquisition_key)
            .map(|(_, round)| round.clone())
            .collect();
        for round in open {
            round.pause();
        }
    }

    /// A round of one candidate.
    pub async fn check_once(&self, heard: HeardAudio, file: &Path) -> HeardMaster {
        self.check_once_in(None, heard, file).await
    }

    /// A round of one candidate of the acquisition `acquisition_key`, which takes its turn at the
    /// acquisition's place as the acquisition's open rounds do.
    pub async fn check_once_in(
        &self,
        acquisition_key: Option<&str>,
        heard: HeardAudio,
        file: &Path,
    ) -> HeardMaster {
        self.check_once_measured(acquisition_key, heard, file).await.0
    }

    /// [`Self::check_once_in`] with what the check measured, when both files were decoded.
    pub async fn check_once_measured(
        &self,
        acquisition_key: Option<&str>,
        heard: HeardAudio,
        file: &Path,
    ) -> (HeardMaster, Option<HeardMasterVerdict>) {
        let round = HeardRound::new(
            Arc::clone(&self.parts),
            heard,
            acquisition_key.map(str::to_string),
        );
        let answer = round.check_measured(file).await;
        round.pause();
        answer
    }
}

#[cfg(test)]
#[path = "heard_master_check_tests.rs"]
mod tests;
