//! #229 (the owner's ruling, 9.11.2026; the main session's option H): a
//! card that ticks far off the rate its driver reports is no usable clock,
//! a calm wait like a driver that gives none. A child module of
//! `asio_out.rs` (its 1000-line cap).
//!
//! Found at PP: Waves SoundGrid with its network off opens at 44 100 Hz /
//! 1024 frames, calls back regularly and takes ~2.9 % more audio than its
//! rate, ~100 times what the drift servo follows, so the floor spliced
//! silence in every second (a hard re-centre, a ledger loss).
//!
//! - Every running output measures the card's consumed frames against the
//!   clock in back-to-back windows of at least 10 s
//!   (`asio_state::RateWatch`), from a quarter second after its ticks start
//!   (the open's first callbacks never weigh in a window). A gap in the
//!   card's callbacks (a driver stall) starts the windows and the strikes
//!   again once the card moves (`RateWatch::track`, review round 2).
//! - A PLAYING output (an open at the network's rate) stops after two
//!   windows in a row more than 1 % off: one WARN, waiting, reason
//!   `clock_off_rate`, and the driver is opened again at once to measure
//!   silently.
//! - A SILENT open: a reopen at the rate and buffer the card was found off
//!   at, or (option H) an open at a rate other than the network's — the
//!   `rate_note` condition, which a Dante output never meets (it has no
//!   network rate here). It feeds nothing, so the card plays silence, no
//!   underrun is counted and no re-centre happens, until one window shows
//!   the rate within 1 %; then it runs, primed by its next block. One
//!   window off = waiting `clock_off_rate` (the WARN once per wait), the
//!   driver kept open and opened again 60 s after its open, `clock_waits`
//!   counting, never a reset, a backoff or a fault.
//! - An open at another rate or buffer than the one the card was found off
//!   at (SoundGrid's network back: 96 kHz) is a fresh open.

use tracing::{debug, info, warn};

use super::{AsioDevice, AsioOut, AsioWorker, Closed, Opened, Run};
use crate::playback::asio_state::{
    NO_CLOCK_REOPEN_100NS, OFF_RATE_STRIKES_PLAYING, RateStep, Reason, rate_note, rate_settled,
};
use crate::playback::audio_out::{STATE_OPENING, STATE_RUNNING, STATE_WAITING};

/// The card's rate and buffer it was found off at, while the output waits
/// for a usable clock (`AsioWorker::off_rate`).
pub(super) type OffRate = Option<(u32, u32)>;

impl AsioOut {
    /// The network's rate this ASIO output's driver is held against at an
    /// open (#229, option H): an open at another rate proves the card's
    /// rate silently first. A Dante output has none.
    pub fn with_network_rate(mut self, rate: u32) -> Self {
        self.network_rate = Some(rate);
        self
    }
}

impl AsioWorker {
    /// At an open read as `opened`: whether the run proves the card's rate
    /// silently first. A rate or buffer other than the one the card was
    /// found off at ends that wait (a fresh open).
    pub(super) fn proves_at_open(&mut self, out: &AsioOut, opened: &Opened) -> bool {
        let this = (opened.rate.round() as u32, opened.buffer_frames);
        if self.off_rate.is_some_and(|off| off != this) {
            self.off_rate = None;
        }
        self.off_rate.is_some()
            || out
                .network_rate
                .is_some_and(|n| rate_note(this.0, n).is_some())
    }
}

/// The state an open output shows while its driver gives a clock: a silent
/// proof reads opening, or waiting `clock_off_rate` once the card was found
/// off; else running.
pub(super) fn run_state(off_rate: OffRate, proving: bool) -> (&'static str, Option<Reason>) {
    match (proving, off_rate) {
        (false, _) => (STATE_RUNNING, None),
        (true, None) => (STATE_OPENING, None),
        (true, Some(_)) => (STATE_WAITING, Some(Reason::ClockOffRate)),
    }
}

/// One rate step of a run whose driver ticks: whether the driver is to be
/// closed and opened again now ([`reopen_for_clock`]).
pub(super) fn rate_step(
    off_rate: &mut OffRate,
    run: &mut Run,
    out: &AsioOut,
    device: &dyn AsioDevice,
    now_100ns: i64,
) -> bool {
    // A card found off its rate is a silent run (a run that played was
    // opened again), so the wait alone decides the 60 s reopen.
    if off_rate.is_some() && now_100ns - run.opened_at_100ns >= NO_CLOCK_REOPEN_100NS {
        return true;
    }
    let consumed = device.consumed_frames();
    let ticks = consumed.saturating_sub(run.consumed_at_prime);
    if !run.clock_base || !rate_settled(ticks, run.opened.rate) {
        return false;
    }
    let strikes = if run.proving {
        1
    } else {
        OFF_RATE_STRIKES_PLAYING
    };
    let driver = (run.opened.rate, run.opened.buffer_frames);
    run.rate.track(consumed, now_100ns, driver);
    match run.rate.step(consumed, now_100ns, driver, strikes) {
        RateStep::Measuring => false,
        RateStep::Plausible => {
            if run.proving {
                run.proving = false;
                let was_off = off_rate.take().is_some();
                info!(
                    id = %out.id,
                    driver = %out.driver,
                    was_off,
                    "asio output: the card takes audio at its driver's rate - running"
                );
                out.update(|l| {
                    l.state = STATE_RUNNING;
                    l.reason = None;
                    l.status.reason_code = None;
                });
            }
            false
        }
        RateStep::OffRate { ppm } => {
            let rate = run.opened.rate.round() as u32;
            if off_rate.is_none() {
                warn!(
                    id = %out.id,
                    driver = %out.driver,
                    rate,
                    ppm,
                    "asio output: the card takes audio more than 1 % off its driver's rate - no usable clock; the output waits (the driver opened again every 60 s); e.g. Waves SoundGrid with its network off"
                );
            }
            *off_rate = Some((rate, run.opened.buffer_frames));
            out.update(|l| {
                l.state = STATE_WAITING;
                l.status.reason_code = Some(Reason::ClockOffRate.code());
                l.reason = Some(Reason::ClockOffRate);
                l.status.retry_in_s = None;
            });
            // A run that played holds a primed ring the card would drain
            // into underruns: it is opened again to wait silently.
            run.primed
        }
    }
}

/// Close the driver to open it again at once, for a driver that gives no
/// usable clock (`what`): counted in `clock_waits`, a DEBUG, never a reset,
/// a backoff or a fault. The caller marks the output closed, due now.
pub(super) fn reopen_for_clock(
    (clock_waits, closed): (&mut u64, &mut Closed),
    run: &Run,
    out: &AsioOut,
    device: &mut dyn AsioDevice,
    what: &str,
) {
    *clock_waits += 1;
    closed.add(run, &*device);
    let (closed, waits) = (*closed, *clock_waits);
    out.update(|l| {
        closed.write(&mut l.status);
        l.status.clock_waits = waits;
    });
    debug!(
        id = %out.id,
        driver = %out.driver,
        waits,
        "asio output: {what} - closing and opening the driver again"
    );
    device.close();
}
