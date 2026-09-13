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
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
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
/// discipline.
fn write_crash_record(path: &Path, record: &str) -> std::io::Result<()> {
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
/// carrying the version + git sha.
pub fn install_panic_hook(crash_log_path: PathBuf) {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            handle_panic(info, &crash_log_path);
            prev(info);
        }));
        tracing::info!(
            target: "panic",
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
}
