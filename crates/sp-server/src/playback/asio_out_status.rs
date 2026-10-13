//! #233: an ASIO output's status as `GET /api/v1/program` shows it
//! (`outputs[i].asio`), a child module of `asio_out.rs` (its 1000-line
//! cap).

use serde::Serialize;

use super::LateCallback;
use crate::playback::asio_loop::LoopStatus;

/// `outputs[i].asio`.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct AsioStatus {
    pub driver: String,
    /// 0-based (the dashboard shows them 1-based).
    pub channels: [u32; 2],
    pub driver_rate: u32,
    pub buffer_frames: u32,
    pub out_channels: u32,
    /// The driver's sample type (`AsioSample::name`), "" before the first open.
    pub sample_type: &'static str,
    /// The correction applied to the resampler.
    pub ppm: f64,
    /// The card's rate against the program wall (the regression).
    pub rate_ppm: f64,
    pub locked: bool,
    /// The output's latency from the boundary, ms (`asio_latency_ms`; 0
    /// until the servo measured its first window, and while waiting).
    pub latency_ms: f64,
    /// The servo's last window: the offset the resampler's ratio drains,
    /// ms — the latency outside [target, target + cushion] (positive =
    /// later; 0 inside it and while waiting).
    pub offset_ms: f64,
    /// The seconds the drain still needs (`None` inside the calm zone, and
    /// while waiting).
    pub slew_eta_s: Option<f64>,
    /// The excess the card's underruns left, kept over the target, ms (#233
    /// comment 6056680979, Q1: at most one slot; it decays slowly through
    /// the level loop; 0 while waiting).
    pub cushion_ms: f64,
    /// Callbacks that found the ring short, since the output was built.
    pub underruns: u64,
    /// Closes (a reset, a rate change, a stall) since the output was built.
    pub resets: u64,
    /// Hard re-centres (a fade, then silence inserted or audio skipped,
    /// because too little was buffered — the ring would run dry, or a
    /// delayed output played more than 4 slots early — or too much, the ring
    /// would overflow), since the output was built: faults. An open's
    /// priming is none.
    pub hard_recentres: u64,
    /// The last hard re-centre, since the output was built.
    pub last_hard_recentre: Option<HardRecentre>,
    /// Frames the ring had no room for, since the output was built.
    pub overflows: u64,
    /// The driver's `kAsioOverload` messages, since the output was built.
    pub overloads: u64,
    /// While waiting: the seconds to the next open (None for a parked
    /// driver, which is never reopened, or for one waiting for its clock —
    /// `no_clock`, the driver open, reopened each minute on its own).
    pub retry_in_s: Option<f64>,
    /// While waiting: the reason's stable code (`Reason::code`).
    pub reason_code: Option<&'static str>,
    /// The times the driver, giving no clock for 60 s after an open, was
    /// closed and opened again (`Reason::NoClock`), since the output was
    /// built: a calm wait, neither a reset nor a fault.
    pub clock_waits: u64,
    /// The driver's resync requests (`kAsioResyncRequest`: its sample
    /// position jumped), since the output was built: counted only (the
    /// servo and the driver run on; #233 measures them first).
    pub resyncs: u64,
    /// The longest time between two buffer switches since the output was
    /// built, ms (`asio_out_events.rs`).
    pub callback_gap_max_ms: f64,
    /// Buffer switches over two buffer periods after the one before, and
    /// under a quarter of one (a catch-up burst), since the output was built.
    pub late_callbacks: u64,
    pub fast_callbacks: u64,
    /// The last late buffer switch and how long ago (to line up with a pop).
    pub last_late_callback: Option<LateCallback>,
    /// The loop check (`asio_loop.rs`, an entry with `asio.loop_check`):
    /// what the output sends against what returns on the driver's
    /// inputs; `off` for any other entry.
    #[serde(rename = "loop")]
    pub loop_status: LoopStatus,
}

/// A hard re-centre as the status shows it.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct HardRecentre {
    /// `deficit` (too little buffered: the ring would run dry, or a delayed
    /// output played more than 4 slots early) or `excess` (the ring would
    /// overflow).
    pub cause: &'static str,
    /// Inserted (> 0) or skipped (< 0), ms.
    pub ms: f64,
    /// The block's hand-off lateness (handled − its boundary), ms.
    pub lateness_ms: f64,
    /// How long ago, s (as of the status's last update).
    pub ago_s: f64,
}
