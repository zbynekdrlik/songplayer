//! #212: the NDI input's receiver lifecycle — (re)connect on a settings
//! change, retry a failed receiver after 5 s, close on the stop path — timed
//! (`last_connect_ms` / `last_close_ms`, and the connect / close log lines
//! carry `connect_ms` / `close_ms`).

use std::time::Instant;

use sp_ndi::NdiFrameSync;
use tracing::{info, warn};

use super::{INPUT_RECONNECT_100NS, INPUT_RECV_NAME, NdiInput, NdiInputShared};

impl NdiInput {
    /// Reconnect when the settings changed, or retry a failed receiver after
    /// [`INPUT_RECONNECT_100NS`].
    pub(super) fn apply_settings(&mut self, boundary_100ns: i64) {
        let want = self.shared.settings();
        if want != self.applied {
            info!(
                enabled = want.enabled,
                source = %want.source,
                previous = %self.applied.source,
                "ndi input: settings changed — (re)connecting"
            );
            self.disconnect();
            self.applied = want;
            self.retry_at = i64::MIN;
        }
        if self.applied.active() && self.sync.is_none() && boundary_100ns >= self.retry_at {
            self.connect(boundary_100ns);
        }
    }

    fn connect(&mut self, boundary_100ns: i64) {
        let Some(backend) = self.backend.clone() else {
            warn!("ndi input: no NDI SDK — the input offers its standby pair");
            self.retry_at = i64::MAX; // never retried without an SDK
            return;
        };
        let started = Instant::now();
        let result = NdiFrameSync::connect(backend, &self.applied.source, INPUT_RECV_NAME);
        let connect_ms = elapsed_ms(started);
        self.shared.counters().last_connect_ms = Some(connect_ms);
        match result {
            Ok(sync) => {
                info!(source = %self.applied.source, connect_ms, "ndi input: receiver + FrameSync created");
                self.sync = Some(sync);
            }
            Err(e) => {
                warn!(%e, source = %self.applied.source, connect_ms, "ndi input: creating the receiver failed — retry in 5 s");
                self.retry_at = boundary_100ns + INPUT_RECONNECT_100NS;
            }
        }
    }

    /// Drop the receiver (FrameSync first) and forget the last frame.
    pub(super) fn disconnect(&mut self) {
        if let Some(sync) = self.sync.take() {
            close_now(&self.shared, sync);
        }
        self.set_connected(false);
        self.last = None;
    }
}

/// Close `sync` (FrameSync, then its receiver) now, timed: `last_close_ms` and
/// the close log line.
fn close_now(shared: &NdiInputShared, sync: NdiFrameSync) {
    let source = sync.source().to_string();
    let started = Instant::now();
    drop(sync);
    let close_ms = elapsed_ms(started);
    shared.counters().last_close_ms = Some(close_ms);
    info!(source = %source, close_ms, "ndi input: receiver closed");
}

/// Whole milliseconds since `started`.
fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}
