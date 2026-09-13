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

use std::panic::PanicHookInfo;
use std::path::{Path, PathBuf};

/// Format one crash record from already-extracted fields. Pure — no clock, no
/// I/O, no real panic — so it is directly unit-testable.
fn format_panic_record(
    _timestamp: &str,
    _thread: &str,
    _location: Option<&str>,
    _message: &str,
    _backtrace: &str,
) -> String {
    // RED stub (#156): not implemented yet — returns nothing so the field
    // assertions fail before the GREEN implementation lands.
    String::new()
}

/// Append `record` to the crash file (creating it if needed) and flush so the
/// bytes reach the OS before the caller aborts.
fn write_crash_record(_path: &Path, _record: &str) -> std::io::Result<()> {
    // RED stub (#156): does not write, so the read-back assertion fails.
    Ok(())
}

/// The body run for every panic: build the record, write it durably, and emit
/// a best-effort tracing error.
fn handle_panic(_info: &PanicHookInfo<'_>, _crash_log_path: &Path) {
    // RED stub (#156): captures nothing, so the crash file is never written.
}

/// Install the process-global panic hook (idempotent). Records every panic to
/// `crash_log_path` before the process aborts.
pub fn install_panic_hook(_crash_log_path: PathBuf) {
    // RED stub (#156): installs nothing.
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
        assert!(record.contains("ndi-pipeline-7"), "thread name present: {record}");
        assert!(record.contains("pacer.rs:789:9"), "location present: {record}");
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
