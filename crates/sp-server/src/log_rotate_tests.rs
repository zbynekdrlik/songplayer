//! #261: a log SongPlayer writes on its own rotates to `.1` at its limit, at
//! the open and during a run, and never loses a line when it cannot.

use std::io::Write;
use std::path::Path;

use super::{ROTATE_AT_BYTES, RotatingFile, rotate_if_over, rotated_path};

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

#[test]
fn the_limit_is_ten_mebibytes_and_the_rotated_name_ends_in_dot_one() {
    assert_eq!(ROTATE_AT_BYTES, 10 * 1024 * 1024);
    assert_eq!(
        rotated_path(Path::new("/data/sp-audio-host.log")),
        Path::new("/data/sp-audio-host.log.1")
    );
}

/// Under the limit nothing moves; at it and over it the file becomes `.1`,
/// replacing an older `.1`; a log not there yet is no rotation.
#[test]
fn a_log_rotates_at_its_limit_and_not_below() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("x.log");
    assert!(!rotate_if_over(&log, 10).unwrap(), "no file yet");

    std::fs::write(&log, "123456789").unwrap();
    assert!(!rotate_if_over(&log, 10).unwrap(), "9 bytes of 10");
    assert_eq!(read(&log), "123456789");
    assert!(!rotated_path(&log).exists());

    std::fs::write(&log, "0123456789").unwrap();
    std::fs::write(rotated_path(&log), "older").unwrap();
    assert!(rotate_if_over(&log, 10).unwrap(), "10 bytes of 10");
    assert!(!log.exists());
    assert_eq!(
        read(&rotated_path(&log)),
        "0123456789",
        "the older .1 replaced"
    );

    std::fs::write(&log, "0123456789AB").unwrap();
    assert!(rotate_if_over(&log, 10).unwrap(), "12 bytes of 10");
    assert_eq!(read(&rotated_path(&log)), "0123456789AB");
}

/// The open rotates a full log first, then appends to a fresh one.
#[test]
fn opening_a_full_log_starts_a_fresh_one() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("host.log");
    std::fs::write(&log, "0123456789").unwrap();
    let mut file = RotatingFile::open(&log, 10).unwrap();
    file.write_all(b"new").unwrap();
    assert_eq!(read(&rotated_path(&log)), "0123456789");
    assert_eq!(read(&log), "new");
}

/// A log under its limit is appended to, never truncated.
#[test]
fn opening_a_log_under_its_limit_appends() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("host.log");
    std::fs::write(&log, "12345").unwrap();
    let mut file = RotatingFile::open(&log, 10).unwrap();
    file.write_all(b"678").unwrap();
    assert_eq!(read(&log), "12345678");
    assert!(!rotated_path(&log).exists());
}

/// During a run: the write that finds the file at its limit goes into a
/// fresh file (the full one is `.1`), and each write stays whole; the count
/// starts from what the file already held.
#[test]
fn a_log_rotates_during_a_run_before_the_write_that_finds_it_full() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("host.log");
    std::fs::write(&log, "abc").unwrap();
    let mut file = RotatingFile::open(&log, 10).unwrap();
    file.write_all(b"defg").unwrap(); // 7 of 10
    file.write_all(b"hijk").unwrap(); // 11: written whole, the limit passed
    assert_eq!(read(&log), "abcdefghijk");
    assert!(!rotated_path(&log).exists(), "nothing rotates mid-write");
    file.write_all(b"line2").unwrap();
    assert_eq!(read(&rotated_path(&log)), "abcdefghijk");
    assert_eq!(read(&log), "line2");
    file.write_all(b"12345").unwrap(); // exactly 10 now
    file.write_all(b"z").unwrap();
    assert_eq!(
        read(&rotated_path(&log)),
        "line212345",
        "rotated at exactly 10"
    );
    assert_eq!(read(&log), "z");
}

/// A rename that fails (a non-empty directory under the `.1` name, on Linux
/// and Windows alike) loses no line: the log goes on in the same file, and
/// the next write tries again once the name is free.
#[test]
fn a_rotation_that_fails_keeps_every_line_and_tries_again() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("host.log");
    let blocker = rotated_path(&log);
    std::fs::create_dir(&blocker).unwrap();
    std::fs::write(blocker.join("keep"), "x").unwrap();
    std::fs::write(&log, "0123456789").unwrap();

    let mut file = RotatingFile::open(&log, 10).unwrap();
    file.write_all(b"A").unwrap();
    assert_eq!(read(&log), "0123456789A", "kept, appended");
    file.write_all(b"B").unwrap();
    assert_eq!(read(&log), "0123456789AB");

    std::fs::remove_dir_all(&blocker).unwrap();
    file.write_all(b"C").unwrap();
    assert_eq!(read(&rotated_path(&log)), "0123456789AB");
    assert_eq!(read(&log), "C");
}

/// A log whose file is gone (deleted by hand during a run) is created again
/// at the next write; a rotation then still counts from the new file.
#[test]
fn a_log_deleted_during_a_run_is_created_again() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("host.log");
    let mut file = RotatingFile::open(&log, 4).unwrap();
    file.write_all(b"abcd").unwrap();
    std::fs::remove_file(&log).unwrap();
    file.write_all(b"e").unwrap(); // 4 of 4 counted: rotates, the rename fails, reopens
    assert_eq!(read(&log), "e");
    assert!(!rotated_path(&log).exists());
}

/// A reopen that fails (the log's dir gone) is an error for that write, and
/// the next write opens the log again once it can; that fresh file rotates
/// at its own limit (the failed try's spacing went with the old file). Unix
/// only: Windows will not move a directory that holds an open file.
#[cfg(unix)]
#[test]
fn a_log_that_cannot_be_reopened_errors_then_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let logs = dir.path().join("logs");
    std::fs::create_dir(&logs).unwrap();
    let log = logs.join("host.log");
    let mut file = RotatingFile::open(&log, 32).unwrap();
    file.write_all(&[b'a'; 32]).unwrap();
    std::fs::rename(&logs, dir.path().join("moved")).unwrap();
    assert!(file.write_all(b"e").is_err(), "nowhere to write");
    std::fs::create_dir(&logs).unwrap();
    file.write_all(b"f").unwrap();
    assert_eq!(read(&log), "f");
    file.write_all("g".repeat(31).as_bytes()).unwrap(); // 32 of 32
    file.write_all(b"h").unwrap();
    assert_eq!(read(&rotated_path(&log)), format!("f{}", "g".repeat(31)));
    assert_eq!(read(&log), "h");
}

/// A stat the system cannot answer (a path "inside" a regular file:
/// ENOTDIR) is an error, never "no log yet". Unix only: Windows reads that
/// path as not found.
#[cfg(unix)]
#[test]
fn a_stat_that_fails_is_an_error_never_no_log() {
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().join("plain");
    std::fs::write(&plain, "x").unwrap();
    assert!(rotate_if_over(&plain.join("x.log"), 10).is_err());
}

/// A rotation that failed is tried again only once the file grew by another
/// sixteenth of the limit, never on every line; one that worked starts the
/// count afresh.
#[test]
fn a_failed_rotation_is_retried_a_sixteenth_of_the_limit_later() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("host.log");
    let blocker = rotated_path(&log);
    std::fs::create_dir(&blocker).unwrap();
    std::fs::write(blocker.join("keep"), "x").unwrap();
    std::fs::write(&log, "x".repeat(32)).unwrap();

    let mut file = RotatingFile::open(&log, 32).unwrap();
    file.write_all(b"a").unwrap(); // tried at 32 and failed: next try at 34
    std::fs::remove_dir_all(&blocker).unwrap();
    file.write_all(b"b").unwrap(); // 33: not yet
    assert!(!rotated_path(&log).exists(), "no try before 34");
    file.write_all(b"c").unwrap(); // 34: rotates
    assert_eq!(read(&rotated_path(&log)), format!("{}ab", "x".repeat(32)));
    assert_eq!(read(&log), "c");

    file.write_all("y".repeat(31).as_bytes()).unwrap(); // 32 again
    file.write_all(b"d").unwrap(); // the count started afresh: rotates at 32
    assert_eq!(read(&rotated_path(&log)), format!("c{}", "y".repeat(31)));
    assert_eq!(read(&log), "d");
}
