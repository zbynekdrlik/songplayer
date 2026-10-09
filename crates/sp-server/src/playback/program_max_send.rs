//! When an `SP-program-MAX` boundary leaves (#223 follow-up, 9.10.2026).
//!
//! Arena renders at 60 Hz and takes whatever Spout's shared texture holds at
//! its own instant. A 30 fps boundary sent the moment the compositor is done
//! leaves at the offer + a compose time that varies by several ms (measured:
//! a 5.9 ms p1–p99 phase spread on the 30 fps grid), so it lands sometimes
//! before and sometimes after Arena's instant and shows for 1 or 3 output
//! frames instead of 2 — the wall stutters. `SP-program`'s NDI submit is
//! paced on the grid; this paces the Spout send the same way: each boundary
//! goes out [`MAX_SEND_LEAD`] after the program offered it ([`send_due`]),
//! whatever its compose cost. A compose that ends after that instant sends
//! at once and is counted late.
//!
//! The wait is a [`SendClock`] so the decisions run on Linux with a fake
//! that never sleeps; production waits on [`SpinClock`]: sleep to
//! [`SEND_SPIN_MARGIN`] short of the instant ([`send_wait_step`]), then spin.

use std::time::{Duration, Instant};

/// How long after the program offered a boundary its Spout send is due:
/// above the compose's p99 (upload ≤ 2.4 ms + draw ≤ 5.7 ms on SNV's RTX)
/// with margin, well under the 33.3 ms slot.
pub const MAX_SEND_LEAD: Duration = Duration::from_micros(1);

/// How close to the due instant the wait stops sleeping and spins: the
/// paced threads' 1 ms timer can oversleep by about a ms.
pub const SEND_SPIN_MARGIN: Duration = Duration::from_millis(2);

/// The instant a boundary offered at `offered` is sent.
pub fn send_due(offered: Instant) -> Instant {
    offered + MAX_SEND_LEAD
}

/// What one check of the wait does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitStep {
    /// The due instant is here: send.
    Done,
    /// Sleep this long (to [`SEND_SPIN_MARGIN`] short of the instant).
    Sleep(Duration),
    /// Within the margin: `std::hint::spin_loop()`.
    Spin,
}

/// The pure decision for one check, `remaining` = due − now (zero once due).
pub fn send_wait_step(remaining: Duration) -> WaitStep {
    if remaining.is_zero() {
        WaitStep::Done
    } else if remaining > SEND_SPIN_MARGIN {
        WaitStep::Sleep(remaining - SEND_SPIN_MARGIN)
    } else {
        WaitStep::Spin
    }
}

/// The clock a paced send runs on.
pub trait SendClock {
    fn now(&mut self) -> Instant;
    /// Return at `due` (at once when it passed).
    fn wait_until(&mut self, due: Instant);
}

/// Production: the monotonic clock; sleep, then spin to the instant.
pub struct SpinClock;

impl SendClock for SpinClock {
    fn now(&mut self) -> Instant {
        Instant::now()
    }

    /// `mutants::skip`: real time; the decision is [`send_wait_step`],
    /// tested on its own.
    #[cfg_attr(test, mutants::skip)]
    fn wait_until(&mut self, due: Instant) {
        loop {
            match send_wait_step(due.saturating_duration_since(Instant::now())) {
                WaitStep::Done => return,
                WaitStep::Sleep(nap) => std::thread::sleep(nap),
                WaitStep::Spin => std::hint::spin_loop(),
            }
        }
    }
}

/// No wait: the send goes out at once. The worker's default, for the tests
/// whose subject is not the timing (they pass made-up instants).
pub struct NoWait;

impl SendClock for NoWait {
    fn now(&mut self) -> Instant {
        Instant::now()
    }

    fn wait_until(&mut self, _due: Instant) {}
}

/// When a boundary went out, relative to its offer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SendTiming {
    /// Send done − offer, µs.
    pub at_us: u64,
    /// The compose ended after the due instant: sent at once.
    pub late: bool,
}

/// Wait on `clock` for the due instant of a boundary offered at `offered`
/// (unless it passed: late), run `send`, and time it.
pub fn send_paced<T, E>(
    clock: &mut dyn SendClock,
    offered: Instant,
    send: impl FnOnce() -> Result<T, E>,
) -> Result<(T, SendTiming), E> {
    let due = send_due(offered);
    let late = clock.now() > due;
    if !late {
        clock.wait_until(due);
    }
    let sent = send()?;
    let at = clock.now().saturating_duration_since(offered);
    let at_us = u64::try_from(at.as_micros()).unwrap_or(u64::MAX);
    Ok((sent, SendTiming { at_us, late }))
}

#[cfg(test)]
#[path = "program_max_send_tests.rs"]
pub(crate) mod tests;
