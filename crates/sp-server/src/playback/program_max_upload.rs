//! #279 P0: what the `program-max` thread's composes move from RAM to the
//! GPU, a child module of `program_max.rs` (it keeps its window under the
//! hand-off's stats lock). Every upload is a CPU copy into driver memory and
//! a PCIe transfer, and that traffic is what makes Arena show older
//! pictures on SNV's wall (#251 comment 6212436810). The GPU-resident path
//! (epic #279) is judged by one number: `max.upload_mb_s` = 0 with a
//! hardware-decodable song on program.
//!
//! Every compose that ran records the plane bytes it uploaded
//! (`sp_gpu::ComposeStats::upload_bytes`, both targets drawn from the same
//! upload, so the FHD sender adds nothing) and the pictures it uploaded,
//! at the instant the thread served the boundary (`MaxWorker::draw`): a
//! boundary whose Spout send then failed moved its bytes all the same. The
//! rates are counted over the last [`MAX_STAT_WINDOW`] composes that are at
//! most [`UPLOAD_RATE_SPAN`] older than the newest (a window that spans a
//! time MAX was off would read too low): everything after the oldest
//! compose, divided by the time since it. While MAX composes nothing (off,
//! quiesced, waiting a backoff) the rates keep their last value: read them
//! with `max.state` `running`.
//!
//! #279 P3a: each compose also records its sides by kind — the ring slots
//! drawn in place (`ComposeStats::gpu_sides`) and the CPU pictures drawn
//! (`Composition::uploads`, uploaded or resident) — over the same window,
//! and the ring slots' failures since the start (`km_acquire_timeouts`,
//! `abandoned`, `open_failures`, `gpu_opens`): `GET /api/v1/program` →
//! `gpu` ([`GpuStatus`]).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use serde::Serialize;
use sp_gpu::ComposeStats;

use super::{MAX_STAT_WINDOW, MaxOut};

/// The longest span a rate covers: the window's 900 boundaries at 30 per
/// second.
pub const UPLOAD_RATE_SPAN: Duration = Duration::from_secs(30);

/// One megabyte, as #279's byte budget counts it: 10^6 bytes.
const MB: f64 = 1_000_000.0;

/// One compose: when it was served, the plane bytes and pictures it
/// uploaded, and (#279 P3a) the ring slots and CPU pictures it drew.
#[derive(Clone, Copy, Debug)]
struct Compose {
    at: Instant,
    bytes: u64,
    pictures: u32,
    gpu_sides: u32,
    cpu_sides: u32,
}

/// The window's composes after its oldest, summed.
#[derive(Clone, Copy, Debug, Default)]
struct Sums {
    bytes: u64,
    pictures: u64,
    gpu_sides: u64,
    cpu_sides: u64,
}

/// The ring slots' failures since the start (#279 P3a, `ComposeStats`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SlotTotals {
    km_acquire_timeouts: u64,
    abandoned: u64,
    open_failures: u64,
    gpu_opens: u64,
}

/// The composes' window and the slots' totals.
#[derive(Clone, Debug, Default)]
pub(super) struct Uploads {
    window: VecDeque<Compose>,
    slots: SlotTotals,
}

impl Uploads {
    /// A compose of a boundary served `at` that uploaded `bytes` in
    /// `pictures` pictures (0 and 0 when every picture was resident, or for
    /// Black) and drew no ring slot. Tests only: production records
    /// [`push_compose`](Self::push_compose).
    #[cfg(test)]
    pub(super) fn push(&mut self, at: Instant, bytes: u64, pictures: u32) {
        self.keep(Compose {
            at,
            bytes,
            pictures,
            gpu_sides: 0,
            cpu_sides: 0,
        });
    }

    /// #279 P3a: the compose `stats` of a boundary served `at`, which drew
    /// `cpu_sides` CPU pictures; its slots' failures add to the totals.
    pub(super) fn push_compose(&mut self, at: Instant, stats: &ComposeStats, cpu_sides: u32) {
        self.keep(Compose {
            at,
            bytes: stats.upload_bytes,
            pictures: stats.uploads,
            gpu_sides: stats.gpu_sides,
            cpu_sides,
        });
        let slots = &mut self.slots;
        slots.km_acquire_timeouts += u64::from(stats.km_acquire_timeouts);
        slots.abandoned += u64::from(stats.abandoned);
        slots.open_failures += u64::from(stats.open_failures);
        slots.gpu_opens += u64::from(stats.gpu_opens);
    }

    /// Keep `compose` in the window: the last [`MAX_STAT_WINDOW`], none
    /// over [`UPLOAD_RATE_SPAN`] older than it.
    fn keep(&mut self, compose: Compose) {
        let at = compose.at;
        self.window.push_back(compose);
        if self.window.len() > MAX_STAT_WINDOW {
            self.window.pop_front();
        }
        while self
            .window
            .front()
            .is_some_and(|first| at.saturating_duration_since(first.at) > UPLOAD_RATE_SPAN)
        {
            self.window.pop_front();
        }
    }

    /// Everything after the oldest compose, and the time since it.
    fn after_oldest(&self) -> (Sums, Duration) {
        let span = match (self.window.front(), self.window.back()) {
            (Some(first), Some(last)) => last.at.saturating_duration_since(first.at),
            _ => Duration::ZERO,
        };
        let sums = self
            .window
            .iter()
            .skip(1)
            .fold(Sums::default(), |sums, c| Sums {
                bytes: sums.bytes + c.bytes,
                pictures: sums.pictures + u64::from(c.pictures),
                gpu_sides: sums.gpu_sides + u64::from(c.gpu_sides),
                cpu_sides: sums.cpu_sides + u64::from(c.cpu_sides),
            });
        (sums, span)
    }

    /// (MB/s, pictures/s) over the window, each to a tenth; 0 below two
    /// composes or over no time.
    pub(super) fn rates(&self) -> (f64, f64) {
        let (sums, span) = self.after_oldest();
        (
            tenth(per_second(sums.bytes, span) / MB),
            tenth(per_second(sums.pictures, span)),
        )
    }

    /// #279 P3a: (ring slots, CPU pictures) drawn per second over the
    /// window, each to a tenth; 0 below two composes or over no time.
    pub(super) fn side_rates(&self) -> (f64, f64) {
        let (sums, span) = self.after_oldest();
        (
            tenth(per_second(sums.gpu_sides, span)),
            tenth(per_second(sums.cpu_sides, span)),
        )
    }
}

/// `total` per second over `span`; 0 over no time.
pub fn per_second(total: u64, span: Duration) -> f64 {
    if span.is_zero() {
        0.0
    } else {
        total as f64 / span.as_secs_f64()
    }
}

/// `x` rounded to a tenth (`93.312` → `93.3`).
pub fn tenth(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

/// #279 P3a: `GET /api/v1/program` → `gpu`: how the compositor drew the
/// program's pictures. The rates cover the window above (read them with
/// `max.state` `running`, or with NDI from the GPU); the counts run since
/// the start. A GPU-resident song on program reads `gpu_sides_per_s` at
/// the program's rate (up to two per boundary for a song the pacer blends:
/// a ring slot per side) and `cpu_sides_per_s` (and `max.upload_mb_s`) 0.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct GpuStatus {
    /// Ring slots drawn in place per second.
    pub gpu_sides_per_s: f64,
    /// CPU pictures drawn per second (uploaded, or resident on their slot).
    pub cpu_sides_per_s: f64,
    /// Ring slots whose keyed mutex another side still held: drawn black.
    pub km_acquire_timeouts: u64,
    /// Ring slots abandoned (a device lost holding the key) or of a dead
    /// ring: drawn black, the ring dead.
    pub abandoned: u64,
    /// Ring slots that did not open: drawn black, their ring killed.
    pub open_failures: u64,
    /// Ring slots the compositor opened on its device (each once).
    pub gpu_opens: u64,
}

impl MaxOut {
    /// #279 P0: a compose of a boundary served `at` uploaded `bytes` in
    /// `pictures` pictures (`ComposeStats::upload_bytes` / `uploads`).
    /// Tests only: `MaxWorker::draw` records [`record_compose`](Self::record_compose).
    #[cfg(test)]
    pub fn record_uploads(&self, at: Instant, bytes: u64, pictures: u32) {
        self.lock_stats().uploads.push(at, bytes, pictures);
    }

    /// #279 P3a: the compose `stats` of a boundary served `at`, which drew
    /// `cpu_sides` CPU pictures (`Composition::uploads().len()`): its
    /// uploads, its sides by kind, its slots' failures.
    pub fn record_compose(&self, at: Instant, stats: &ComposeStats, cpu_sides: usize) {
        let cpu_sides = u32::try_from(cpu_sides).unwrap_or(u32::MAX);
        self.lock_stats().uploads.push_compose(at, stats, cpu_sides);
    }

    /// `max.upload_mb_s` and `max.uploads_per_s` ([`Uploads::rates`]),
    /// counted outside the stats lock.
    pub(super) fn upload_rates(&self) -> (f64, f64) {
        let uploads = self.lock_stats().uploads.clone();
        uploads.rates()
    }

    /// #279 P3a: `GET /api/v1/program` → `gpu`, counted outside the stats
    /// lock.
    pub fn gpu_status(&self) -> GpuStatus {
        let uploads = self.lock_stats().uploads.clone();
        let (gpu_sides_per_s, cpu_sides_per_s) = uploads.side_rates();
        let slots = uploads.slots;
        GpuStatus {
            gpu_sides_per_s,
            cpu_sides_per_s,
            km_acquire_timeouts: slots.km_acquire_timeouts,
            abandoned: slots.abandoned,
            open_failures: slots.open_failures,
            gpu_opens: slots.gpu_opens,
        }
    }
}

#[cfg(test)]
#[path = "program_max_upload_tests.rs"]
mod tests;
