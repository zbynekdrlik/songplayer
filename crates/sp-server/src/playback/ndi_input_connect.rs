//! #212 follow-up: the NDI input's receiver lifecycle, OFF the grid thread.
//! Design record: #212 comment 5849076208 (Approach 1).
//!
//! The SDK's receiver create (`recv_create_v3` + `framesync_create`) and its
//! destroy (`framesync_destroy` + `recv_destroy`) can each block for ~0.5 s
//! (box log, #212 comment 5849047061). The grid thread owns a program boundary
//! every 33.3 ms, so it never calls either:
//!
//! - A (re)connect runs on a short-lived `ndi-input-connect` thread, which
//!   hands the pair back through a one-slot channel. The grid thread polls it
//!   once per boundary ([`NdiInput::apply_settings`]) and swaps the pair in.
//! - A replaced or disabled pair is dropped on an `ndi-input-close` thread.
//! - Only one connect is in flight. A settings change while it runs
//!   supersedes it: its pair is closed off the grid thread when it lands, and
//!   the connect for the newest settings starts on that boundary.
//! - A failed connect is retried [`INPUT_RECONNECT_100NS`] after the boundary
//!   that requested it.
//! - Meanwhile every boundary is served like a disconnected source: `capture`
//!   finds no receiver, so the input offers its standby pair.
//! - The helpers time themselves (`last_connect_ms` / `last_close_ms`, and the
//!   log lines carry `connect_ms` / `close_ms`); the grid publishes
//!   `connects_pending` (0 / 1).

use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use sp_ndi::{NdiError, NdiFrameSync};
use tracing::{info, warn};

use super::{INPUT_RECONNECT_100NS, INPUT_RECV_NAME, InputSettings, NdiInput, NdiInputShared};

/// What a connect helper hands back.
type ConnectResult = Result<NdiFrameSync, NdiError>;

/// A (re)connect running on its `ndi-input-connect` helper.
pub(super) struct PendingConnect {
    /// The settings it connects for: superseded once the applied ones differ.
    settings: InputSettings,
    /// The boundary that requested it (a failure retries 5 s after it).
    requested_100ns: i64,
    /// The one-slot hand-over from the helper.
    rx: Receiver<ConnectResult>,
}

impl NdiInput {
    /// Once per boundary, with no SDK call on this thread: apply a settings
    /// change (the current pair is closed off-thread), take a connect that
    /// finished, and request the connect the settings need — or retry a failed
    /// one after [`INPUT_RECONNECT_100NS`].
    pub(super) fn apply_settings(&mut self, boundary_100ns: i64) {
        let want = self.shared.settings();
        if want != self.applied {
            info!(
                enabled = want.enabled,
                source = %want.source,
                previous = %self.applied.source,
                "ndi input: settings changed — (re)connecting"
            );
            self.release();
            self.applied = want;
            self.retry_at = i64::MIN;
        }
        self.poll_connect();
        if self.applied.active()
            && self.sync.is_none()
            && self.pending.is_none()
            && boundary_100ns >= self.retry_at
        {
            self.connect(boundary_100ns);
        }
    }

    /// Take the pending connect's result if it landed: swap the pair in, close
    /// it off-thread when the settings moved on meanwhile, or schedule the
    /// retry of a failure.
    fn poll_connect(&mut self) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        let result = match pending.rx.try_recv() {
            Err(TryRecvError::Empty) => {
                self.pending = Some(pending);
                return;
            }
            Ok(result) => result,
            Err(TryRecvError::Disconnected) => {
                warn!(source = %pending.settings.source, "ndi input: the connect thread ended without a result");
                Err(NdiError::ReceiveFailed("the connect thread ended"))
            }
        };
        self.publish_pending();
        if pending.settings != self.applied {
            info!(
                source = %pending.settings.source,
                "ndi input: a superseded connect finished — closing it off the grid thread"
            );
            if let Ok(sync) = result {
                close_off_thread(&self.shared, sync);
            }
            return;
        }
        match result {
            Ok(sync) => self.sync = Some(sync),
            Err(_) => self.retry_at = pending.requested_100ns + INPUT_RECONNECT_100NS,
        }
    }

    /// Request a connect for the applied settings on an `ndi-input-connect`
    /// helper. Without an NDI SDK there is nothing to connect: the standby pair
    /// only.
    fn connect(&mut self, boundary_100ns: i64) {
        let Some(backend) = self.backend.clone() else {
            warn!("ndi input: no NDI SDK — the input offers its standby pair");
            self.retry_at = i64::MAX; // never retried without an SDK
            return;
        };
        let (tx, rx) = mpsc::sync_channel(1);
        let source = self.applied.source.clone();
        let shared = self.shared.clone();
        let spawned = thread::Builder::new()
            .name("ndi-input-connect".into())
            .spawn(move || {
                let started = Instant::now();
                let result = NdiFrameSync::connect(backend, &source, INPUT_RECV_NAME);
                let connect_ms = elapsed_ms(started);
                match &result {
                    Ok(_) => {
                        info!(source = %source, connect_ms, "ndi input: receiver + FrameSync created");
                    }
                    Err(e) => {
                        warn!(%e, source = %source, connect_ms, "ndi input: creating the receiver failed — retry in 5 s");
                    }
                }
                // No receiver any more (the input stopped): the pair is
                // dropped — closed — here, on this helper.
                let _ = tx.send(result);
                // Published after the hand-over: once it reads `Some`, the
                // result is in the channel (or was closed here).
                shared.counters().last_connect_ms = Some(connect_ms);
            });
        if let Err(e) = spawned {
            // The helper never ran, so its channel is closed: the next
            // boundary takes it as a failed connect (retry in 5 s).
            warn!(%e, "ndi input: spawning the connect thread failed");
        }
        self.pending = Some(PendingConnect {
            settings: self.applied.clone(),
            requested_100ns: boundary_100ns,
            rx,
        });
        self.publish_pending();
    }

    /// A settings change: close the current pair off the grid thread and
    /// forget the last frame. The boundaries are standby until the next pair.
    fn release(&mut self) {
        if let Some(sync) = self.sync.take() {
            close_off_thread(&self.shared, sync);
        }
        self.set_connected(false);
        self.last = None;
    }

    /// The stop path (the grid loop has ended). The current pair, and a
    /// connect result already handed over but not taken yet, are closed on
    /// the close helper and awaited. A connect still running is abandoned: its
    /// helper closes the pair when the hand-over finds no receiver. Closes
    /// started by earlier settings changes may still be running, detached.
    pub(super) fn disconnect(&mut self) {
        let handed_over = self
            .pending
            .take()
            .and_then(|pending| pending.rx.try_recv().ok())
            .and_then(Result::ok);
        for sync in self.sync.take().into_iter().chain(handed_over) {
            if let Some(closer) = close_off_thread(&self.shared, sync) {
                let _ = closer.join(); // a panic there is logged by the panic hook
            }
        }
        self.publish_pending();
        self.set_connected(false);
        self.last = None;
    }

    /// Publish `connects_pending` (0 / 1) for the API.
    fn publish_pending(&self) {
        self.shared.counters().connects_pending = u32::from(self.pending.is_some());
    }
}

/// Drop `sync` (FrameSync, then its receiver) on an `ndi-input-close` helper
/// and return its handle. `None` when the helper could not start; the pair was
/// then closed right here.
fn close_off_thread(shared: &Arc<NdiInputShared>, sync: NdiFrameSync) -> Option<JoinHandle<()>> {
    let shared = shared.clone();
    let spawned = thread::Builder::new()
        .name("ndi-input-close".into())
        .spawn(move || close_now(&shared, sync));
    match spawned {
        Ok(closer) => Some(closer),
        Err(e) => {
            warn!(%e, "ndi input: spawning the close thread failed — closed inline");
            None
        }
    }
}

/// Close `sync` now, timed: `last_close_ms` and the close log line.
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
