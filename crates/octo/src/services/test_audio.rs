//! Real, tiny audio files made with ffmpeg, for the tests that tag songs on disk. Each kind is
//! made once per test run and copied from there.

use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use parking_lot::Mutex;

/// A fifth of a second of silence in the container `extension` names ("mp3", "flac", "m4a",
/// "ogg"), or None when ffmpeg is not on the PATH (the test then skips, as `FfmpegFact` did).
pub(crate) fn audio(extension: &str) -> Option<Vec<u8>> {
    static MADE: OnceLock<Mutex<HashMap<String, Option<Vec<u8>>>>> = OnceLock::new();
    let made = MADE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(bytes) = made.lock().get(extension) {
        return bytes.clone();
    }
    let bytes = make(extension);
    made.lock().insert(extension.to_string(), bytes.clone());
    bytes
}

fn make(extension: &str) -> Option<Vec<u8>> {
    let dir = tempfile::tempdir().ok()?;
    let out = dir.path().join(format!("silence.{extension}"));
    let codec: &[&str] = match extension {
        "mp3" => &["-c:a", "libmp3lame", "-b:a", "64k"],
        "m4a" => &["-c:a", "aac", "-b:a", "64k"],
        "ogg" => &["-c:a", "libvorbis"],
        "webm" | "opus" => &["-c:a", "libopus", "-b:a", "64k"],
        _ => &[],
    };
    let status = Command::new("ffmpeg")
        .args(["-y", "-nostdin", "-hide_banner", "-v", "error"])
        .args(["-f", "lavfi", "-i", "anullsrc=r=44100:cl=mono", "-t", "0.2"])
        .args(codec)
        .arg(&out)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }
    std::fs::read(&out).ok()
}

/// Whether a missing ffmpeg fails the test instead of skipping it: with
/// `OCTO_TESTS_NEED_FFMPEG=1` (set on the gate), a test that needs real audio never passes by
/// skipping.
pub(crate) fn missing_ffmpeg_fails(setting: Option<&str>) -> bool {
    setting.is_some_and(|value| value.trim() == "1")
}

/// Returns early from a test, with the reason printed, when ffmpeg cannot make the audio; fails
/// it instead when `OCTO_TESTS_NEED_FFMPEG=1`.
macro_rules! audio_or_skip {
    ($extension:expr) => {
        match $crate::services::test_audio::audio($extension) {
            Some(bytes) => bytes,
            None => {
                if $crate::services::test_audio::missing_ffmpeg_fails(
                    std::env::var("OCTO_TESTS_NEED_FFMPEG").ok().as_deref(),
                ) {
                    panic!("ffmpeg is not on the PATH, and OCTO_TESTS_NEED_FFMPEG=1 needs it");
                }
                eprintln!("skipped: ffmpeg is not on the PATH");
                return;
            }
        }
    };
}
pub(crate) use audio_or_skip;

#[cfg(test)]
mod tests {
    use super::missing_ffmpeg_fails;

    #[test]
    fn only_the_gates_setting_turns_a_skip_into_a_failure() {
        assert!(missing_ffmpeg_fails(Some("1")));
        assert!(missing_ffmpeg_fails(Some(" 1 ")));
        assert!(!missing_ffmpeg_fails(Some("0")));
        assert!(!missing_ffmpeg_fails(Some("")));
        assert!(!missing_ffmpeg_fails(None));
    }
}
