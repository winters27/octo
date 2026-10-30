//! Tests of what a pick reads before its search: an artist's music in Lidarr and in its folder,
//! and the waits on Lidarr's own commands.

use super::*;

#[test]
fn audio_in_a_folder_counts_and_a_dot_folder_never_does() {
    let root = tempfile::tempdir().expect("a temp dir");
    let held = root.path().join(".octo-incoming/held/1");
    std::fs::create_dir_all(&held).expect("the folder");
    std::fs::write(held.join("01 - Angel.flac"), b"held").expect("written");
    std::fs::write(root.path().join("cover.jpg"), b"art").expect("written");
    assert!(!holds_audio(root.path()));

    let album = root.path().join("Mezzanine");
    std::fs::create_dir_all(&album).expect("the folder");
    std::fs::write(album.join("03 - Teardrop.FLAC"), b"the user's").expect("written");
    assert!(holds_audio(root.path()));
    assert!(!holds_audio(&root.path().join("missing")));
}
