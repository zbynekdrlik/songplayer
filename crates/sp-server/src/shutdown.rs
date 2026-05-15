//! Graceful-shutdown helpers exposed to the Tauri shell.
//!
//! The Tauri tray "Exit" handler needs to await the server's background
//! task with a real timeout instead of a fixed 500 ms sleep (#81). The
//! helper here is pure tokio so it's easy to unit-test from the workspace,
//! and the tray callback consumes it via `block_on`.

use std::time::Duration;
use tokio::task::JoinHandle;

/// Await the server `JoinHandle` up to `timeout`.
///
/// Returns `true` when the task completed cleanly inside the timeout,
/// `false` when the timeout fired or the task panicked. Callers should
/// treat `false` as "give up and exit anyway" — the alternative is
/// hanging the UI thread indefinitely.
pub async fn await_server_join(handle: JoinHandle<()>, timeout: Duration) -> bool {
    matches!(tokio::time::timeout(timeout, handle).await, Ok(Ok(())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::Instant;

    #[tokio::test]
    async fn await_server_join_returns_true_when_task_finishes_quickly() {
        let handle = tokio::spawn(async {
            tokio::time::sleep(Duration::from_millis(20)).await;
        });
        let started = Instant::now();
        let ok = await_server_join(handle, Duration::from_millis(500)).await;
        assert!(ok, "task finished inside timeout, expected true");
        // Sanity: shouldn't have waited the whole timeout.
        assert!(started.elapsed() < Duration::from_millis(400));
    }

    #[tokio::test]
    async fn await_server_join_returns_false_when_task_exceeds_timeout() {
        let handle = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        let started = Instant::now();
        let ok = await_server_join(handle, Duration::from_millis(50)).await;
        assert!(!ok, "task ran past timeout, expected false");
        // Should have returned shortly after the timeout, not waited 60 s.
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[tokio::test]
    async fn await_server_join_returns_false_when_task_panics() {
        let handle = tokio::spawn(async {
            panic!("simulated server panic");
        });
        let ok = await_server_join(handle, Duration::from_millis(500)).await;
        assert!(!ok, "panicked task must surface as false, not true");
    }
}
