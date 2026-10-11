//! #233 (PP's Waves SoundGrid output pops): what an ASIO driver tells the
//! worker besides its callbacks, a child module of `asio_out.rs` (its
//! 1000-line cap).
//!
//! A resync request (`kAsioResyncRequest`, answered "supported" by
//! `asio_state::reply`): the driver's sample position jumped — a lost or a
//! repeated buffer, a re-locked clock. The drift servo measured the old
//! position (its rate regression, its window, its level loop), so it starts
//! afresh, as when a driver's clock arrives (`ClockStep::ClockArrived`): its
//! next block primes the ring to the target, and its first window measures
//! the latency again. Never a close and a reopen: a SoundGrid driver takes
//! about 10 s to open again. The old servo's hard re-centres stay counted,
//! and every resync is counted (`outputs[i].asio.resyncs`) and WARNed, at
//! most once per 5 s with the number held back.

use tracing::warn;

use super::{AsioOut, Closed, Run, new_servo};
use crate::playback::stat_window::WarnLimiter;

/// At most one resync WARN per this much of the wall (5 s, 100 ns).
const RESYNC_WARN_EVERY_100NS: i64 = 50_000_000;

/// The driver's resync requests since the output was built, and their
/// WARN's limit.
#[derive(Default)]
pub(super) struct Resyncs {
    count: u64,
    warns: WarnLimiter,
}

/// A resync request of `run`'s driver at `now_100ns` (see the module doc):
/// the servo rebased, the count shown, a WARN unless one went out in the
/// last 5 s.
pub(super) fn resync(
    run: &mut Run,
    out: &AsioOut,
    closed: &mut Closed,
    resyncs: &mut Resyncs,
    now_100ns: i64,
) {
    // The rebased servo counts its hard re-centres from 0: the old one's
    // stay counted with the closed runs'.
    closed.hard_recentres += run.servo.status().hard_recentres;
    run.servo = new_servo(out, run.opened);
    resyncs.count += 1;
    let count = resyncs.count;
    if let Some(held_back) = resyncs.warns.admit(now_100ns, RESYNC_WARN_EVERY_100NS) {
        log_resync(&out.id, &out.driver, count, held_back);
    }
    out.update(|l| l.status.resyncs = count);
}

/// A resync's WARN (`held_back`: those since the last WARN).
#[cfg_attr(test, mutants::skip)] // logging only; the decision is WarnLimiter's (tested)
fn log_resync(id: &str, driver: &str, resyncs: u64, held_back: u64) {
    warn!(
        id,
        driver,
        resyncs,
        held_back,
        "asio output: the driver asked for a resync (kAsioResyncRequest: its sample position jumped) - the drift servo starts afresh and primes the ring; the driver stays open"
    );
}
