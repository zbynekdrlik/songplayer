//! The clocks the VBAN thread and the NDI input pace on (#210, #212), and
//! VBAN's policy at a fleet date step (#224 part 2).
//!
//! Both are a [`WallClock`] ticked once per grid boundary passed
//! ([`BoundaryTicker`], the program wall's cadence), so each follows a UTC
//! step at the same time as the program and the source walls — one clock
//! domain. [`WallVbanClock::new`] follows its wall (the NDI input: at a date
//! step its boundaries come at most r, under one slot, early).
//! [`WallVbanClock::slewing`] is VBAN's: VBAN has no timecode and its
//! receiver (VB-Matrix at FOH) paces by arrival, so a jump of its clock would
//! send audio at once and a hold would leave a gap. It owes every regrid's
//! movement of the timeline, forward or back, and pays it at
//! [`VBAN_SLEW_PPM`] ([`RemainderSlew`]): every packet interval stays within
//! 4.1667 ms ± 100 ppm. It reads the timeline's LINE
//! ([`WallClock::line_100ns`]), which runs on through a hold, so a residue
//! hold is slewed like a jump.
//!
//! Moved out of `vban_out.rs` (review round 1: that file neared the
//! 1000-line cap); `vban_out` re-exports every item.

use std::time::Duration;

use crate::playback::program_output::BoundaryTicker;
use crate::playback::vban_out::VbanClock;
use crate::playback::wallclock::WallClock;

/// How fast VBAN's clock pays back a date step's movement (#224 part 2):
/// 50 ppm of its timeline, 100 ns per 2 ms, so a whole slot (33.3 ms) is paid
/// in ~11 min. Below 100 ppm with a margin for the grid's own 41 666 /
/// 41 667 / 41 668 × 100 ns packet spacing and the 100-ns rounding of each
/// wait, so every packet interval stays within 4.1667 ms ± 100 ppm (VB-Matrix
/// at FOH runs an ASRC that follows the arrival rate).
pub const VBAN_SLEW_PPM: i64 = 50;

/// VBAN's clock policy at a fleet date step (#224 part 2): SlewRemainder.
///
/// A follow relabels the wall (the whole slots N never reach the timeline)
/// and moves its timeline by the remainder r: a jump ahead, or a hold of a
/// residue under 3 ms. VBAN reads `line − owed`, where `owed` takes that
/// movement at the follow (signed), so VBAN's clock neither jumps nor stops,
/// and then shrinks toward 0 at [`VBAN_SLEW_PPM`] of the elapsed line: every
/// packet goes out on its cadence, r ends up paid over minutes (the queue
/// holds up to r more meanwhile, under one block; a hold is sent up to its
/// size early, inside the 2-slot send latency). Pure: the caller passes the
/// line readings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RemainderSlew {
    /// What was owed at `since_100ns` (100 ns, signed: negative = VBAN's
    /// clock runs ahead of the line, paying a hold back).
    owed_100ns: i64,
    /// The line reading of the last movement.
    since_100ns: i64,
}

impl RemainderSlew {
    /// What is still owed at line reading `t_100ns`: the owed movement,
    /// [`VBAN_SLEW_PPM`] of the line since it closer to 0, never past 0.
    pub fn owed_at(&self, t_100ns: i64) -> i64 {
        let elapsed = (t_100ns - self.since_100ns).max(0);
        let paid = elapsed.saturating_mul(VBAN_SLEW_PPM) / 1_000_000;
        (self.owed_100ns - paid).max(0)
    }

    /// The line moved by `jump_100ns` (signed), reading `t_100ns` after the
    /// movement: owe it on top of what is still owed, so the clock reads the
    /// same instant as right before it.
    pub fn owe(&mut self, jump_100ns: i64, t_100ns: i64) {
        let before = self.owed_at(t_100ns - jump_100ns);
        self.owed_100ns = before + jump_100ns;
        self.since_100ns = t_100ns;
    }

    /// VBAN's clock at line reading `t_100ns`.
    pub fn clock_100ns(&self, t_100ns: i64) -> i64 {
        t_100ns - self.owed_at(t_100ns)
    }
}

/// Production clock (module doc): a [`WallClock`] ticked once per grid
/// boundary passed. [`new`](Self::new) follows the wall (the NDI input);
/// [`slewing`](Self::slewing) is VBAN's ([`RemainderSlew`]).
pub struct WallVbanClock {
    wall: WallClock,
    ticker: BoundaryTicker,
    /// `Some` = SlewRemainder (VBAN).
    slew: Option<RemainderSlew>,
    /// The wall's regrid count already owed.
    regrids_seen: u64,
}

impl WallVbanClock {
    pub fn new(wall: WallClock) -> Self {
        Self {
            regrids_seen: wall.shift().regrids,
            wall,
            ticker: BoundaryTicker::default(),
            slew: None,
        }
    }

    /// VBAN's clock: `wall`'s timeline line minus the date-step movement it
    /// still owes ([`RemainderSlew`], #224 part 2).
    pub fn slewing(wall: WallClock) -> Self {
        Self {
            slew: Some(RemainderSlew::default()),
            ..Self::new(wall)
        }
    }

    /// After a wall tick: a regrid (or a rejoin) moved the timeline — owe
    /// the movement.
    fn owe_regrid(&mut self) {
        let shift = self.wall.shift();
        if shift.regrids == self.regrids_seen {
            return;
        }
        self.regrids_seen = shift.regrids;
        let line = self.wall.line_100ns();
        if let Some(slew) = self.slew.as_mut() {
            slew.owe(shift.last_jump_100ns, line);
        }
    }
}

impl VbanClock for WallVbanClock {
    fn now_100ns(&mut self) -> i64 {
        let now = self.wall.now_100ns();
        for _ in 0..self.ticker.advance(now) {
            self.wall.tick();
            self.owe_regrid();
        }
        match self.slew {
            Some(slew) => slew.clock_100ns(self.wall.line_100ns()),
            None => self.wall.now_100ns(),
        }
    }

    fn sleep_100ns(&mut self, d_100ns: i64) {
        std::thread::sleep(Duration::from_nanos(d_100ns.max(0) as u64 * 100));
    }

    fn slew_owed_100ns(&self) -> i64 {
        self.slew
            .map_or(0, |slew| slew.owed_at(self.wall.line_100ns()))
    }
}
