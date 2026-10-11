//! #233 (PP's spawn finding, 11.10.2026): an ASIO output across two
//! processes, a child module of `asio_out.rs` (its 1000-line cap).
//!
//! A child process spawned FROM the process that has a Waves SoundGrid ASIO
//! driver loaded makes that driver resync (measured on PP: every
//! SongPlayer-spawned yt-dlp, never the same yt-dlp from a shell). So no
//! ASIO driver lives in SongPlayer any more: every ASIO output runs in the
//! ASIO host process `sp-audio-host` (`crate::audio_host`), which spawns
//! nothing. Each entry has an [`AsioOut`] on BOTH sides, built from the same
//! [`AsioSpec`]:
//!
//! - SongPlayer's side takes the program's blocks into its queue as before
//!   (`AudioOutputs::offer`); a forwarder thread ([`AsioOut::take_block`])
//!   sends them over the host's pipe, and the host's status reports land in
//!   its live state ([`AsioOut::apply_remote`]), so `GET /api/v1/program`
//!   and the dashboard read it as they always did. While the host is down it
//!   reads waiting, `host_down` ([`AsioOut::set_host_down`]).
//! - The host's side is the existing worker's output, unchanged: the
//!   pipe's blocks are pushed into its queue and `run_asio_worker` plays
//!   them on the driver.

use std::sync::atomic::Ordering;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::{AsioOut, AsioSnapshot, AsioStatus};
use crate::playback::asio_state::Reason;
use crate::playback::asrc_servo::BASE_LATENCY_100NS;
use crate::playback::audio_out::STATE_WAITING;
use crate::playback::audio_out_queue::{RunGuard, Take};

/// What an ASIO output is built from, on either side of the host's pipe:
/// its entry's id, driver, 0-based channels and delay.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AsioSpec {
    pub id: String,
    pub driver: String,
    pub channels: [u32; 2],
    pub delay_ms: u32,
}

impl AsioOut {
    /// The spec this output was built from ([`AsioOut::from_spec`]).
    pub fn spec(&self) -> AsioSpec {
        AsioSpec {
            id: self.id.clone(),
            driver: self.driver.clone(),
            channels: self.channels,
            delay_ms: ((self.target_100ns - BASE_LATENCY_100NS) / 10_000) as u32,
        }
    }

    /// The entry's id.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// SongPlayer's side: the host's report of this output, as its status
    /// (the host's queue drops add to SongPlayer's own).
    pub fn apply_remote(&self, snapshot: AsioSnapshot) {
        self.update(|l| {
            l.state = snapshot.state;
            l.reason = snapshot.reason;
            l.status = snapshot.status;
            l.blocks_sent = snapshot.blocks_sent;
            l.remote_dropped = snapshot.blocks_dropped;
        });
    }

    /// SongPlayer's side: the ASIO host is not running (why); the output
    /// waits for it. The counters and the last open's driver facts stay;
    /// the run's figures go, as at a close (`AsioWorker::wait`).
    pub fn set_host_down(&self, why: &str) {
        let reason = Reason::HostDown(why.to_string());
        self.update(|l| {
            l.state = STATE_WAITING;
            l.status.reason_code = Some(reason.code());
            l.reason = Some(reason);
            l.status.retry_in_s = None;
            clear_run_figures(&mut l.status);
        });
    }

    /// SongPlayer's side: the next block for the host's pipe, waiting at
    /// most `wait` (the forwarder, `audio_host::client::run_forwarder`).
    pub fn take_block(&self, wait: Duration) -> Take {
        self.queue.take_timeout(wait)
    }

    /// SongPlayer's side: one more block the host's link did not take (the
    /// host down, or its queue full); counted in `blocks_dropped`. Returns
    /// the count so far.
    pub fn note_unsent(&self) -> u64 {
        self.unsent.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Marks the output's thread running until the guard drops (the
    /// forwarder holds it, as the worker does).
    pub(crate) fn run_guard(&self) -> RunGuard<'_> {
        RunGuard::start(&self.running, &self.start_error, "ASIO")
    }
}

/// A closed or unreachable run's figures, cleared (the counters stay).
fn clear_run_figures(s: &mut AsioStatus) {
    s.ppm = 0.0;
    s.rate_ppm = 0.0;
    s.locked = false;
    s.latency_ms = 0.0;
    s.offset_ms = 0.0;
    s.slew_eta_s = None;
    s.cushion_ms = 0.0;
}

#[cfg(test)]
#[path = "asio_out_remote_tests.rs"]
mod tests;
