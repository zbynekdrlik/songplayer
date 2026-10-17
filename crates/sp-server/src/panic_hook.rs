//! Process-global panic hook that captures a durable crash record before
//! `panic = "abort"` (release) kills the process (#156).
//!
//! Under the release `panic = "abort"` profile any Rust panic calls `abort()`
//! (Windows fast-fail `0xc0000409`) with no unwinding, so the default hook's
//! stderr output is discarded by the scheduled task, and the
//! `tracing_appender::non_blocking` file writer never flushes (its
//! `WorkerGuard` drains only on Drop, which `abort()` skips). This hook
//! therefore writes the panic record SYNCHRONOUSLY to a dedicated crash file
//! with an explicit flush, in addition to a best-effort `tracing::error!`, so
//! the next occurrence leaves a diagnosable record.

use std::backtrace::Backtrace;
use std::io::Write;
use std::panic::PanicHookInfo;
use std::path::{Path, PathBuf};
use std::sync::Once;

/// Crate version, embedded at compile time.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Short git sha, embedded by `build.rs` (`SP_GIT_SHA`). Falls back to
/// `"unknown"` when the build script could not resolve one (e.g. git absent),
/// via `option_env!` so the module compiles with or without the build script.
const GIT_SHA: &str = match option_env!("SP_GIT_SHA") {
    Some(s) => s,
    None => "unknown",
};

/// Extract the human-readable message from a panic payload (`&str` and
/// `String` payloads, the two the standard `panic!`/`assert!`/index-out-of-
/// bounds machinery produces).
fn panic_message(info: &PanicHookInfo<'_>) -> String {
    let payload = info.payload();
    if let Some(&s) = payload.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

/// Format one crash record from already-extracted fields. Pure — no clock, no
/// I/O, no real panic — so it is directly unit-testable.
fn format_panic_record(
    timestamp: &str,
    thread: &str,
    location: Option<&str>,
    message: &str,
    backtrace: &str,
) -> String {
    format!(
        "==== PANIC {timestamp} ====\n\
         version: {VERSION} ({GIT_SHA})\n\
         thread: {thread}\n\
         location: {loc}\n\
         message: {message}\n\
         backtrace:\n{backtrace}\n\
         ================================\n",
        loc = location.unwrap_or("<unknown>"),
    )
}

/// Append `record` to the crash file (creating it if needed) and flush so the
/// bytes reach the OS before the caller aborts. Append mode preserves earlier
/// crash records across restarts, mirroring the rolling log's never-truncate
/// discipline; #261: a file at `log_rotate::ROTATE_AT_BYTES` is rotated to
/// `.1` first, so a panic storm cannot fill the disk.
fn write_crash_record(path: &Path, record: &str) -> std::io::Result<()> {
    write_crash_record_at(path, record, crate::log_rotate::ROTATE_AT_BYTES)
}

/// One crash record at a time (#261): two threads panicking at once at the
/// limit would both rotate, the second moving the first's fresh record
/// over the full `.1`. (A panic inside the hook aborts the process, so
/// this lock is never taken twice by one thread.)
static WRITING: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// [`write_crash_record`] with the rotation's limit (a test's small one).
fn write_crash_record_at(path: &Path, record: &str, limit: u64) -> std::io::Result<()> {
    let _one_at_a_time = WRITING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Best-effort: ensure the crash dir exists so a very-early panic (before
    // the logger created the data dir) still lands a durable record.
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // A rotation that fails never costs the record: it is appended as before.
    let _ = crate::log_rotate::rotate_if_over(path, limit);
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all(record.as_bytes())?;
    f.flush()
}

/// The body run for every panic: build the record, write it durably to
/// `crash_log_path`, and emit a best-effort tracing error.
fn handle_panic(info: &PanicHookInfo<'_>, crash_log_path: &Path) {
    let timestamp = chrono::Utc::now().to_rfc3339();
    let thread = std::thread::current()
        .name()
        .unwrap_or("<unnamed>")
        .to_string();
    let location = info
        .location()
        .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()));
    let message = panic_message(info);
    let backtrace = Backtrace::force_capture().to_string();
    let record = format_panic_record(
        &timestamp,
        &thread,
        location.as_deref(),
        &message,
        &backtrace,
    );

    // 1. Durable synchronous write — survives `panic = "abort"`, unlike the
    //    non_blocking tracing writer whose flush-on-Drop the abort skips.
    let _ = write_crash_record(crash_log_path, &record);

    // 2. Best-effort structured tracing — the record operators watching the
    //    main log will see whenever the writer does drain (unwind/debug, or a
    //    lucky flush before abort). Greppable via `target=panic`.
    let log_line = format!(
        "SongPlayer panic: thread={thread} location={} message={message} (crash record: {})",
        location.as_deref().unwrap_or("<unknown>"),
        crash_log_path.display(),
    );
    tracing::error!(target: "panic", "{}", log_line);
}

/// Install the process-global panic hook (idempotent). Records every panic to
/// `crash_log_path` before the process aborts, chaining the previous hook so
/// the default stderr output is preserved. Emits a one-time startup INFO line
/// carrying the version + git sha. #261: a crash file already at
/// `log_rotate::ROTATE_AT_BYTES` is rotated to `.1` here, at the start.
pub fn install_panic_hook(crash_log_path: PathBuf) {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let rotated =
            crate::log_rotate::rotate_if_over(&crash_log_path, crate::log_rotate::ROTATE_AT_BYTES)
                .unwrap_or(false);
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            handle_panic(info, &crash_log_path);
            prev(info);
        }));
        tracing::info!(
            target: "panic",
            rotated,
            "SongPlayer v{} ({}) — panic hook installed",
            VERSION,
            GIT_SHA
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serialize the tests that touch the process-global panic hook.
    static HOOK_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn format_panic_record_contains_thread_location_and_message() {
        let record = format_panic_record(
            "2026-09-14T22:04:43Z",
            "ndi-pipeline-7",
            Some("crates/sp-server/src/playback/pacer.rs:789:9"),
            "index out of bounds: the len is 0 but the index is 18446744073709551615",
            "0: some::frame\n1: another::frame",
        );
        assert!(
            record.contains("ndi-pipeline-7"),
            "thread name present: {record}"
        );
        assert!(
            record.contains("pacer.rs:789:9"),
            "location present: {record}"
        );
        assert!(
            record.contains("index out of bounds"),
            "message present: {record}"
        );
        assert!(
            record.contains("2026-09-14T22:04:43Z"),
            "timestamp present: {record}"
        );
    }

    #[test]
    fn write_crash_record_appends_and_is_durable() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("songplayer-panic.log");
        write_crash_record(&path, "first-record\n").expect("write 1");
        write_crash_record(&path, "second-record\n").expect("write 2");
        let content = std::fs::read_to_string(&path).expect("crash file exists");
        assert!(
            content.contains("first-record"),
            "append kept the first record: {content}"
        );
        assert!(
            content.contains("second-record"),
            "append kept the second record: {content}"
        );
    }

    /// #261: a crash file at its limit is rotated to `.1` before the next
    /// record, which starts a fresh file; under the limit it is appended to.
    #[test]
    fn a_full_crash_file_is_rotated_before_the_next_record() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("songplayer-panic.log");
        write_crash_record_at(&path, "123456789", 10).expect("write 1");
        write_crash_record_at(&path, "A", 10).expect("write 2");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "123456789A");
        write_crash_record_at(&path, "B", 10).expect("write 3");
        let rotated = crate::log_rotate::rotated_path(&path);
        assert_eq!(std::fs::read_to_string(rotated).unwrap(), "123456789A");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "B");
    }

    /// #261: one crash record at a time — a record waits while another is
    /// written (two at the limit would both rotate), then lands.
    #[test]
    fn a_crash_record_waits_for_the_one_being_written() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("songplayer-panic.log");
        let (go_tx, go_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let writer = {
            let path = path.clone();
            std::thread::spawn(move || {
                go_rx.recv().expect("go");
                write_crash_record_at(&path, "waited", 10).expect("write");
                done_tx.send(()).expect("send");
            })
        };
        // Nothing that can panic runs while the lock is held (a panic here
        // would wait for it in an installed hook's own record): the writer
        // is spawned before it, and the asserts run once it is released.
        let held = WRITING.lock().unwrap_or_else(|e| e.into_inner());
        let _ = go_tx.send(());
        let early = done_rx
            .recv_timeout(std::time::Duration::from_millis(200))
            .is_ok();
        let written_early = path.exists();
        drop(held);
        assert!(!early, "the record waits for the lock");
        assert!(!written_early);
        done_rx
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("written once the lock is free");
        writer.join().expect("join");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "waited");
    }

    #[test]
    fn hook_captures_panic_location_and_message_to_crash_file() {
        let _guard = HOOK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().expect("tempdir");
        let crash_path = tmp.path().join("songplayer-panic.log");
        let hook_path = crash_path.clone();

        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            handle_panic(info, &hook_path);
        }));

        let joined = std::thread::Builder::new()
            .name("panic-capture-thread".to_string())
            .spawn(|| panic!("boom-unique-2026-marker"))
            .expect("spawn")
            .join();

        // Restore the default hook before asserting so a failing assert here
        // does not run under our custom hook.
        std::panic::set_hook(prev);

        assert!(joined.is_err(), "the spawned thread must have panicked");
        let content = std::fs::read_to_string(&crash_path)
            .expect("panic hook must have written the crash file");
        assert!(
            content.contains("boom-unique-2026-marker"),
            "panic message captured: {content}"
        );
        assert!(
            content.contains("panic_hook.rs"),
            "panic location captured: {content}"
        );
        assert!(
            content.contains("panic-capture-thread"),
            "panic thread name captured: {content}"
        );
    }

    /// The only test that installs the process-global hook (its `Once`). #261:
    /// a crash file already at the limit is rotated at the install, before
    /// any panic, and the next record starts a fresh file.
    #[test]
    fn install_panic_hook_writes_the_crash_file_for_a_thread_panic() {
        let _guard = HOOK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().expect("tempdir");
        let crash_path = tmp.path().join("songplayer-panic.log");
        let full = vec![b'x'; crate::log_rotate::ROTATE_AT_BYTES as usize];
        std::fs::write(&crash_path, full).expect("a full crash file");
        install_panic_hook(crash_path.clone());
        let rotated = crate::log_rotate::rotated_path(&crash_path);
        assert_eq!(
            std::fs::metadata(rotated).map(|m| m.len()).ok(),
            Some(crate::log_rotate::ROTATE_AT_BYTES),
            "rotated at the install"
        );
        let joined = std::thread::Builder::new()
            .name("install-hook-thread".to_string())
            .spawn(|| panic!("install-hook-marker-2026"))
            .expect("spawn")
            .join();
        assert!(joined.is_err(), "the spawned thread must have panicked");
        let content = std::fs::read_to_string(&crash_path)
            .expect("install_panic_hook must have written the crash file");
        assert!(
            content.contains("install-hook-marker-2026"),
            "message captured: {content}"
        );
        assert!(
            content.contains("install-hook-thread"),
            "thread captured: {content}"
        );
        // A fresh file after the rotation (another test's panic may add a
        // record of its own: the hook is process-wide).
        assert!(
            (content.len() as u64) < crate::log_rotate::ROTATE_AT_BYTES,
            "a fresh crash file, {} bytes",
            content.len()
        );
    }
}
