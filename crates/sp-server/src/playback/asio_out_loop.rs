//! #233 (the SoundGrid loop check): the worker's side of its output's loop
//! check, a child module of `asio_out.rs` (its 1000-line cap). For an entry
//! with `asio.loop_check`, each driver start is given a run's taps
//! (`AsioDevice::tap_loop`, before the start) and the other ends go to the
//! analyzer thread (`asio_loop.rs`, started once per output through the
//! worker's seam, `with_loop_start`); every step copies the analyzer's
//! status into the output's (`outputs[i].asio.loop`).

use super::{AsioDevice, AsioOut, AsioWorker};
use crate::playback::asio_loop::{LoopFeed, LoopLink, StartLoop, loop_rings};

impl AsioWorker {
    /// This worker with its loop check's analyzer started by `start` (the
    /// tests' recorder; the worker's own spawns the `loop-<id>` thread).
    pub fn with_loop_start(mut self, start: StartLoop) -> Self {
        self.loop_start = start;
        self
    }

    /// A started run's rings to the analyzer, its thread started at the
    /// first.
    pub(super) fn run_loop(&mut self, out: &AsioOut, feed: Option<LoopFeed>) {
        let Some(feed) = feed else {
            return;
        };
        let start = &self.loop_start;
        self.loop_link
            .get_or_insert_with(|| LoopLink::start(&out.id, &**start))
            .run(feed);
    }

    /// The analyzer's last status into the output's.
    pub(super) fn publish_loop(&self, out: &AsioOut) {
        if let Some(link) = &self.loop_link {
            let status = link.status();
            out.update(|l| l.status.loop_status = status);
        }
    }
}

/// For an entry with `loop_check`: a run's rings at the driver's `rate`,
/// one end tapped by the device's next start, the other for the analyzer.
pub(super) fn tap(out: &AsioOut, device: &mut dyn AsioDevice, rate: f64) -> Option<LoopFeed> {
    out.loop_check.then(|| {
        let (taps, feed) = loop_rings(rate);
        device.tap_loop(taps);
        feed
    })
}
