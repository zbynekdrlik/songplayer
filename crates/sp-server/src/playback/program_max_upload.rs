//! #279 P0: what the `program-max` thread's composes move from RAM to the
//! GPU, a child module of `program_max.rs` (it keeps its window under the
//! hand-off's stats lock). Every upload is a CPU copy into driver memory and
//! a PCIe transfer, and that traffic is what makes Arena show older
//! pictures on SNV's wall (#251 comment 6212436810). The GPU-resident path
//! (epic #279) is judged by one number: `max.upload_mb_s` = 0 with a
//! hardware-decodable song on program.
//!
//! Each sent boundary records the plane bytes its ONE compose uploaded
//! (`sp_gpu::ComposeStats::upload_bytes`, both targets drawn from the same
//! upload, so the FHD sender adds nothing) and the pictures it uploaded,
//! at the instant the thread served it. The rates are counted over the
//! window of the last [`MAX_STAT_WINDOW`] sent boundaries that are at most
//! [`UPLOAD_RATE_SPAN`] older than the newest (a window that spans a time
//! MAX was off would read too low): everything after the oldest boundary,
//! divided by the time since it.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use super::{MAX_STAT_WINDOW, MaxOut};

/// The longest span a rate covers: the window's 900 boundaries at 30 per
/// second.
pub const UPLOAD_RATE_SPAN: Duration = Duration::from_secs(30);

/// One megabyte, as the design's byte budget counts it (§5.12: 10^6).
const MB: f64 = 1_000_000.0;

/// The sent boundaries' uploads: (served at, plane bytes, pictures).
#[derive(Clone, Debug, Default)]
pub(super) struct Uploads(VecDeque<(Instant, u64, u32)>);

impl Uploads {
    /// A boundary served `at` whose compose uploaded `bytes` in `pictures`
    /// pictures (0 and 0 when every picture was resident, or for Black).
    pub(super) fn push(&mut self, at: Instant, bytes: u64, pictures: u32) {
        self.0.push_back((at, bytes, pictures));
        if self.0.len() > MAX_STAT_WINDOW {
            self.0.pop_front();
        }
        while self
            .0
            .front()
            .is_some_and(|&(first, ..)| at.saturating_duration_since(first) > UPLOAD_RATE_SPAN)
        {
            self.0.pop_front();
        }
    }

    /// (MB/s, pictures/s) over the window, each to a tenth; 0 below two
    /// boundaries or over no time.
    pub(super) fn rates(&self) -> (f64, f64) {
        let (Some(&(first, ..)), Some(&(last, ..))) = (self.0.front(), self.0.back()) else {
            return (0.0, 0.0);
        };
        let span = last.saturating_duration_since(first);
        let (bytes, pictures) = self
            .0
            .iter()
            .skip(1)
            .fold((0u64, 0u64), |(bytes, pictures), &(_, b, p)| {
                (bytes + b, pictures + u64::from(p))
            });
        (
            tenth(per_second(bytes, span) / MB),
            tenth(per_second(pictures, span)),
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

impl MaxOut {
    /// #279 P0: a sent boundary served `at` uploaded `bytes` in `pictures`
    /// pictures (`ComposeStats::upload_bytes` / `uploads`).
    pub fn record_uploads(&self, at: Instant, bytes: u64, pictures: u32) {
        self.lock_stats().uploads.push(at, bytes, pictures);
    }

    /// `max.upload_mb_s` and `max.uploads_per_s` ([`Uploads::rates`]),
    /// counted outside the stats lock.
    pub(super) fn upload_rates(&self) -> (f64, f64) {
        let uploads = self.lock_stats().uploads.clone();
        uploads.rates()
    }
}

#[cfg(test)]
#[path = "program_max_upload_tests.rs"]
mod tests;
