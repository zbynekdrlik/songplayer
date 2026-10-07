//! #229: reads and transfers at a bounded rate. The serving node's uplink
//! also carries the live stream, and its disk also feeds the wall.

use std::time::Duration;

/// How long to pause after `done` bytes in `elapsed` so that the average
/// stays at or under `rate` bytes/s. `rate` 0 = no limit.
pub fn wait_for(done: u64, elapsed: Duration, rate: u64) -> Duration {
    if rate == 0 {
        return Duration::ZERO;
    }
    let due_us = u128::from(done) * 1_000_000 / u128::from(rate);
    let due = Duration::from_micros(u64::try_from(due_us).unwrap_or(u64::MAX));
    due.saturating_sub(elapsed)
}

/// Mbit/s as bytes/s.
pub fn mbps_to_bytes(mbps: u32) -> u64 {
    u64::from(mbps) * 125_000
}

#[cfg(test)]
#[path = "throttle_tests.rs"]
mod tests;
