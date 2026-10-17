//! #261: the data dir's measure — every file counted once, the cache inside
//! the data dir or beside it, no symlink followed, the newest write found.

use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

use super::{measure, modified_ms, walk};

fn write(path: &Path, bytes: usize) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, vec![b'x'; bytes]).unwrap();
}

/// A file's last write set to `ms` after the epoch.
fn set_modified(path: &Path, ms: u64) {
    let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    file.set_modified(UNIX_EPOCH + Duration::from_millis(ms))
        .unwrap();
}

#[test]
fn a_walk_counts_every_file_in_the_tree_and_its_newest_write() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    write(&root.join("a"), 3);
    write(&root.join("sub/b"), 5);
    write(&root.join("sub/deeper/c"), 7);
    // a write far ahead of the dirs' own times: the tree's newest
    set_modified(&root.join("sub/deeper/c"), 4_102_444_800_123);
    let tree = walk(&root, None);
    assert_eq!(tree.bytes, 15);
    // The listing's times: on NTFS the directory index may lag a write
    // (the exact walk below reads each file's own record on every system).
    if cfg!(unix) {
        assert_eq!(tree.newest_ms, Some(4_102_444_800_123));
    }
    assert!(tree.complete);
    assert!(!tree.skipped);
}

/// The dir's own write counts too: an empty dir has its own time.
#[test]
fn an_empty_dir_has_its_own_write() {
    let dir = tempfile::tempdir().unwrap();
    let meta = std::fs::metadata(dir.path()).unwrap();
    let tree = walk(dir.path(), None);
    assert_eq!(tree.bytes, 0);
    assert_eq!(tree.newest_ms, modified_ms(&meta));
    assert!(tree.newest_ms.is_some());
}

/// A dir that is not there is an incomplete, empty walk (never "old").
#[test]
fn a_missing_dir_is_an_incomplete_walk() {
    let dir = tempfile::tempdir().unwrap();
    let tree = walk(&dir.path().join("none"), None);
    assert_eq!(tree.bytes, 0);
    assert_eq!(tree.newest_ms, None);
    assert!(!tree.complete);
}

/// The cache inside the data dir is walked once and counted in it; a cache
/// beside it is counted on its own.
#[test]
fn the_cache_is_counted_once_inside_the_data_dir_or_beside_it() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("SongPlayer");
    write(&data.join("songplayer.db"), 100);
    write(&data.join("logs.log"), 20);
    write(&data.join("cache/song_audio.flac"), 1_000);
    write(&data.join("cache/tools/yt-dlp.exe"), 300);
    assert_eq!(measure(&data, &data.join("cache")), (1_420, 1_300));

    let beside = dir.path().join("elsewhere");
    write(&beside.join("x"), 50);
    assert_eq!(measure(&data, &beside), (1_420, 50));
}

/// A symlink is never followed (a link to a big dir outside adds nothing,
/// and a link to its own parent never loops). Unix only: a test cannot
/// make a symlink on the Windows runner without privilege.
#[cfg(unix)]
#[test]
fn a_symlink_is_never_followed() {
    let dir = tempfile::tempdir().unwrap();
    let outside = dir.path().join("outside");
    write(&outside.join("big"), 10_000);
    let root = dir.path().join("root");
    write(&root.join("a"), 3);
    std::os::unix::fs::symlink(&outside, root.join("to-outside")).unwrap();
    std::os::unix::fs::symlink(&root, root.join("loop")).unwrap();
    std::os::unix::fs::symlink(outside.join("big"), root.join("big-file")).unwrap();
    let tree = walk(&root, None);
    assert_eq!(tree.bytes, 3);
    assert!(tree.complete);
}

/// The exact walk (the cache sweep's work dirs) finds what the walk finds,
/// each entry read from its own record.
#[test]
fn an_exact_walk_reads_the_same_tree() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    write(&root.join("a"), 3);
    write(&root.join("sub/b"), 5);
    set_modified(&root.join("sub/b"), 4_102_444_800_456);
    let exact = super::walk_exact(&root);
    assert_eq!(exact.bytes, 8);
    assert_eq!(exact.newest_ms, Some(4_102_444_800_456));
    assert!(exact.complete);
    assert!(!exact.skipped);
}

/// The exact walk reads a file a job still holds open (the sweep's work
/// dirs) from the file's own record: its size and time as they are now,
/// where NTFS's listing may still show those of the open (the Windows job
/// writes both readings to its log).
#[test]
fn an_exact_walk_reads_a_file_still_open_for_writing() {
    use std::io::Write;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir(&root).unwrap();
    let mut open = std::fs::File::create(root.join("growing")).unwrap();
    open.write_all(&[b'x'; 4096]).unwrap();
    open.set_modified(UNIX_EPOCH + Duration::from_millis(4_102_444_800_789))
        .unwrap();
    let listed = walk(&root, None);
    let exact = super::walk_exact(&root);
    let _ = writeln!(
        std::io::stderr(),
        "a file still open: listing {} bytes {:?}, exact {} bytes {:?}",
        listed.bytes,
        listed.newest_ms,
        exact.bytes,
        exact.newest_ms
    );
    assert_eq!(exact.bytes, 4096);
    assert_eq!(exact.newest_ms, Some(4_102_444_800_789));
    drop(open);
}
