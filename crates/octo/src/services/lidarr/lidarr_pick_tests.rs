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

/// A folder holding a link back up its own tree is read once: the look ends, with or without
/// audio, instead of following the loop for ever.
#[test]
fn a_folder_linking_back_up_its_tree_is_read_once() {
    let root = tempfile::tempdir().expect("a temp dir");
    let artist = root.path().join("Massive Attack");
    std::fs::create_dir_all(&artist).expect("the folder");
    let link = artist.join("again");
    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(&artist, &link).is_ok();
    #[cfg(windows)]
    let linked = std::os::windows::fs::symlink_dir(&artist, &link).is_ok();
    #[cfg(not(any(unix, windows)))]
    let linked = false;
    if !linked {
        // This system lets nobody but an administrator make a link: nothing to read here.
        return;
    }
    assert!(!holds_audio(root.path()));

    std::fs::write(artist.join("01 - Angel.flac"), b"the user's").expect("written");
    assert!(holds_audio(root.path()));
}
