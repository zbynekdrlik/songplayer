//! A fleet date step RELABELS time, it does not move content (#224 part 2,
//! design record 5899388193).
//!
//! dantesync steps the fleet date (the nightly correction up to ~±1.5 s, a
//! date-master restart mid-day). Every [`WallClock`] follows such a step at
//! the boundary it lands (#224). Before this module the wall's reading moved
//! by the whole step S, and the paced senders turned that into a catch-up
//! burst (forward) or a pause of |S| (backward): `pacer.rs` maps content onto
//! the wall with `present = wall_start + pts`.
//!
//! Now SongPlayer runs ONE continuous internal TIMELINE and puts the fleet
//! labels on only at the NDI wire. A confirmed step S of either sign (the
//! step probe's or the resample's, any armed 1 ms included) splits into:
//!
//! - N = ⌊S / P⌋ whole slots (P = 10⁷/30 in 100 ns): a RELABEL. The wall's
//!   timeline moves N slots behind its labels, and the wire stamp of every
//!   boundary moves N slots with it ([`wire_stamp_100ns`]);
//! - the remainder r = S − (D(K+N) − D(K)), with 0 ≤ r ≤ 1 slot and
//!   D(K) = ⌈K·P⌉ ([`shift_100ns`]): the only part the timeline sees, a small
//!   FORWARD jump. A date step never holds the timeline and never bursts it.
//!
//! D rounds up so the wire stamp `floor_boundary(b + D(K))` of an on-grid
//! boundary b is EXACTLY the boundary K slots later, and N rounds down so a
//! wire stamp is never future-dated (camera-box contract §4, issue 1009).
//!
//! Every wall, the pacers', the submit consumers', `SP-program`'s, VBAN's
//! and the NDI input's, must move by the SAME N, or two outputs' stamps would
//! differ by a slot. [`FleetShift`] is that one process-wide registry: the
//! first wall to confirm a step registers an EPOCH {S, N}; a later wall whose
//! own reading of the step lies within [`STEP_RESIDUE_100NS`] (3 ms) of its
//! unapplied epochs adopts their N ([`adopt`]) — the difference is the two
//! walls' line errors, never a new epoch. The production walls share
//! [`global`]; a test builds its own registry, never the global one.
//!
//! A wall that was not watching the clock cannot tell its own drift from a
//! date step, so it never registers one. Every tick publishes the wall's
//! line ([`FleetLine`]); a wall built now, or one ticking again after more
//! than 10 s idle (the legacy per-frame submit wall), JOINS the freshest
//! published line and its relabel ([`FleetShift::join`]). So a wall built
//! between a step and its registration starts on the pre-step line and
//! follows the step itself, like every other wall; with no fresh line it
//! starts on the realtime clock at the current K.
//!
//! Everything here but the registry lock is pure integer arithmetic.
//!
//! [`WallClock`]: super::wallclock::WallClock

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Instant;

use sp_core::genlock::{GENLOCK_GRID_FPS, UNITS_PER_SECOND, floor_boundary_100ns};
use tracing::info;

use crate::playback::wallclock::{Anchor, AnchorSample, WALL_REJOIN_IDLE, to_us, utc_now_100ns};

/// How far two walls' readings of ONE date step can differ (100 ns): twice a
/// wall's own line error, each at most the bounded resample's 1 ms (a lone
/// outlier) plus ≤ ~0.31 ms of slewing lag (94 ppm over a 3.33 s resample
/// period) — under 3 ms. A reading within it of a run of registered epochs is
/// that run ([`adopt`]); a step within it of nothing registered is never an
/// epoch (the fuzz after review round 1: at the probe's 2 ms two walls with
/// opposite outliers split K). Not covered: a wall anchored on its OWN wide
/// sample (no fresh line to join: the first wall of the process, or a rejoin
/// with every wall idle) whose 8 attempts were all preempted over 6 ms. Its
/// first probe then registers the anchor error as an epoch, which every
/// other wall (joining its line) adopts, so K stays one fleet-wide value.
pub const STEP_RESIDUE_100NS: i64 = 30_000;

/// D(K) = ⌈K·P⌉ in 100 ns: how far a timeline K slots behind its labels sits
/// behind them (P = 10⁷/30, one grid slot). Rounded UP, so an on-grid boundary
/// `b` relabels onto exactly the boundary K slots later
/// (`floor_boundary(b + D(K))`, [`wire_stamp_100ns`]).
pub fn shift_100ns(slots: i64) -> i64 {
    // ⌈a / b⌉ = −⌊−a / b⌋ (b > 0); signed `div_ceil` is not stable.
    let down = (-slots * UNITS_PER_SECOND).div_euclid(GENLOCK_GRID_FPS);
    -down
}

/// N = ⌊S / P⌋: the whole slots of a step `step_100ns`, rounded toward −∞ so
/// the remainder is never negative and a relabelled stamp never future-dated.
pub fn whole_slots(step_100ns: i64) -> i64 {
    (step_100ns * GENLOCK_GRID_FPS).div_euclid(UNITS_PER_SECOND)
}

/// A step split into its relabel and its remainder, for a wall `k` slots in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Regrid {
    /// N: whole slots the labels move over the timeline.
    pub slots: i64,
    /// r = S − (D(K+N) − D(K)): what the timeline itself moves, 0 ≤ r ≤ 1 slot.
    pub remainder_100ns: i64,
}

/// Split the step `step_100ns` of a wall `k` slots in ([`Regrid`]).
pub fn split(step_100ns: i64, k: i64) -> Regrid {
    let slots = whole_slots(step_100ns);
    Regrid {
        slots,
        remainder_100ns: step_100ns - relabel_100ns(k, slots),
    }
}

/// D(K+N) − D(K): how much the labels move over the timeline when a wall `k`
/// slots in relabels `slots` more.
pub fn relabel_100ns(k: i64, slots: i64) -> i64 {
    shift_100ns(k + slots) - shift_100ns(k)
}

/// The NDI wire stamp of the internal boundary `stamp_100ns` under the fleet
/// shift `k`: `floor_boundary(stamp + D(k))`, the one mapping every paced
/// sender's stamps go through at the submit edge (`FrameSubmitter`).
pub fn wire_stamp_100ns(stamp_100ns: i64, k: i64) -> i64 {
    floor_boundary_100ns(stamp_100ns + shift_100ns(k), GENLOCK_GRID_FPS)
}

/// One date step the fleet took, as the first wall that confirmed it split it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Epoch {
    /// The step S (100 ns, signed) the registering wall measured.
    pub step_100ns: i64,
    /// Its relabel N.
    pub slots: i64,
}

/// What a wall adopts for its confirmed step ([`adopt`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Adoption {
    /// The relabel the wall applies: ΣN of the epochs it adopts, plus the new
    /// epoch's.
    pub slots: i64,
    /// How many of its unapplied epochs it adopts.
    pub consumed: usize,
    /// A step no registered epoch covers: the wall registers it.
    pub new_epoch: Option<Epoch>,
}

/// The pure registry rule for a wall that confirmed a step `step_100ns` and
/// has not applied `unapplied` yet (oldest first).
///
/// - The run of its unapplied epochs (the first j, none included) whose
///   summed step lies CLOSEST to `step_100ns` is the step it saw, when it
///   lies within [`STEP_RESIDUE_100NS`] (3 ms): it adopts their summed N,
///   and the difference is the walls' line errors (a lone-outlier resample,
///   slewing lag), which only its own timeline takes. On a tie the shorter
///   run wins. So two walls reading one step either side of a slot multiple
///   still move by ONE N, a wall two epochs behind adopts both, and a step
///   within 3 ms of nothing registered is never an epoch: every wall applies
///   it on its own (N = 0).
/// - Otherwise the step is new (beyond every unapplied epoch): the wall
///   adopts them all and registers the rest as a new epoch. A registered
///   epoch is therefore always over 3 ms.
pub fn adopt(unapplied: &[Epoch], step_100ns: i64) -> Adoption {
    let (mut step_sum, mut slots) = (0, 0);
    // (distance, consumed, slots) of the closest run so far: none adopted.
    let mut closest = (step_100ns.abs(), 0, 0);
    for (i, epoch) in unapplied.iter().enumerate() {
        step_sum += epoch.step_100ns;
        slots += epoch.slots;
        let distance = (step_100ns - step_sum).abs();
        if distance < closest.0 {
            closest = (distance, i + 1, slots);
        }
    }
    if closest.0 <= STEP_RESIDUE_100NS {
        return Adoption {
            slots: closest.2,
            consumed: closest.1,
            new_epoch: None,
        };
    }
    let rest = step_100ns - step_sum;
    let epoch = Epoch {
        step_100ns: rest,
        slots: whole_slots(rest),
    };
    Adoption {
        slots: slots + epoch.slots,
        consumed: unapplied.len(),
        new_epoch: Some(epoch),
    }
}

/// A wall's relabel after one confirmed step ([`FleetShift::follow`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Relabel {
    /// N the wall moves by now.
    pub slots: i64,
    /// The registry epochs the wall has applied from now on.
    pub epochs: usize,
}

/// One wall's line as it last ticked (module doc): what a wall built now, or
/// one rejoining after an idle gap, copies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FleetLine {
    /// When that wall ticked (monotonic).
    pub at: Instant,
    /// Its monotonic→UTC anchor, a hold in progress included.
    pub anchor: Anchor,
    /// Its K_w.
    pub slots: i64,
    /// The registry epochs it had applied.
    pub epochs: usize,
}

/// What the registry lock guards.
#[derive(Debug, Default)]
struct Registry {
    epochs: Vec<Epoch>,
    /// The line the last wall to tick published.
    line: Option<FleetLine>,
}

/// The process-wide fleet relabel registry (module doc). Its K is the sum of
/// every registered epoch's N: the shift the wire stamps are put on with.
/// A few epochs a day (16 bytes each) are kept for the process lifetime.
/// `FleetShift::default()` is an empty registry: K = 0.
#[derive(Debug, Default)]
pub struct FleetShift {
    registry: Mutex<Registry>,
    /// K_F, read lock-free on every submitted pair.
    slots: AtomicI64,
}

impl FleetShift {
    fn lock(&self) -> MutexGuard<'_, Registry> {
        self.registry.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// K_F: the fleet shift in whole slots, the sum of every epoch's N.
    pub fn slots(&self) -> i64 {
        self.slots.load(Ordering::SeqCst)
    }

    /// Epochs registered so far (a wall built now has applied them all).
    pub fn epochs(&self) -> usize {
        self.lock().epochs.len()
    }

    /// K_F with every epoch applied, read together under the lock (only a
    /// registration moves K_F): where a wall with no fresh line to join
    /// starts ([`join`](Self::join)).
    pub fn current(&self) -> WallShift {
        self.current_of(&self.lock())
    }

    /// [`current`](Self::current) with the lock already held.
    fn current_of(&self, registry: &Registry) -> WallShift {
        WallShift {
            slots: self.slots(),
            epochs: registry.epochs.len(),
            ..WallShift::default()
        }
    }

    /// A wall ticked: its line is the one a joining wall copies next.
    pub fn publish(&self, line: FleetLine) {
        self.lock().line = Some(line);
    }

    /// Where a wall joining at `sample` starts (module doc): the line a wall
    /// published at most [`WALL_REJOIN_IDLE`] before it, with that wall's
    /// relabel; else `sample` itself at the current K. The pre-step line of a
    /// wall that has not followed a step yet is as good as a followed one:
    /// the joining wall reads that step itself at its first tick.
    pub fn join(&self, sample: &AnchorSample) -> (Anchor, WallShift) {
        let registry = self.lock();
        let fresh = registry
            .line
            .filter(|line| sample.instant.saturating_duration_since(line.at) <= WALL_REJOIN_IDLE);
        match fresh {
            Some(line) => (
                line.anchor,
                WallShift {
                    slots: line.slots,
                    epochs: line.epochs,
                    ..WallShift::default()
                },
            ),
            None => (
                Anchor {
                    instant: sample.instant,
                    utc_100ns: sample.utc_100ns,
                },
                self.current_of(&registry),
            ),
        }
    }

    /// A wall that has applied `applied` epochs confirmed a step
    /// `step_100ns`: what it relabels by ([`adopt`]). A new step is
    /// registered under the lock, with ONE INFO line. K_F moves BEFORE the
    /// registering wall's timeline does (the wall applies the relabel after
    /// this returns): a stamp is never put on the labels with a K_F older
    /// than the timeline it was taken on, which after a backward step would
    /// future-date it; a newer K_F only leaves it up to r stale.
    pub fn follow(&self, applied: usize, step_100ns: i64) -> Relabel {
        let mut registry = self.lock();
        let epochs = &mut registry.epochs;
        let from = applied.min(epochs.len());
        let adoption = adopt(&epochs[from..], step_100ns);
        let mut applied = from + adoption.consumed;
        if let Some(epoch) = adoption.new_epoch {
            epochs.push(epoch);
            applied += 1;
            let k_before = self.slots.fetch_add(epoch.slots, Ordering::SeqCst);
            let regrid = split(epoch.step_100ns, k_before);
            info!(
                step_us = to_us(epoch.step_100ns),
                shift_slots = regrid.slots,
                remainder_us = to_us(regrid.remainder_100ns),
                fleet_slots = self.slots(),
                epoch = epochs.len(),
                "fleet shift: a date step registered — the labels move, the timeline moves only the remainder (#224)"
            );
        }
        Relabel {
            slots: adoption.slots,
            epochs: applied,
        }
    }

    /// The internal TIMELINE now: realtime minus D(K_F). For a reader that
    /// compares the realtime clock with internal stamps and has no wall of
    /// its own (a program cut, the scene-go-off hold re-check).
    pub fn timeline_now_100ns(&self) -> i64 {
        utc_now_100ns() - shift_100ns(self.slots())
    }

    /// The wire stamp of internal boundary `stamp_100ns` under K_F now
    /// ([`wire_stamp_100ns`]), for a log line or an API field that shows it.
    pub fn wire_100ns(&self, stamp_100ns: i64) -> i64 {
        wire_stamp_100ns(stamp_100ns, self.slots())
    }

    /// The fleet label of an internal timeline READING `t_100ns` (not a
    /// boundary, so not floored): `t + D(K_F)`, for a log line that shows one
    /// next to wire stamps.
    pub fn label_100ns(&self, t_100ns: i64) -> i64 {
        t_100ns + shift_100ns(self.slots())
    }
}

/// A wall's own relabel state and its last regrid, for its telemetry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WallShift {
    /// K_w: whole slots this wall's timeline sits behind its labels.
    pub slots: i64,
    /// The registry epochs this wall has applied.
    pub epochs: usize,
    /// Steps this wall regridded (followed with the relabel split), and
    /// rejoins after an idle gap.
    pub regrids: u64,
    /// r of the last regrid: the step minus its relabel (100 ns). For the
    /// wall that registered the step 0 ≤ r ≤ one slot; a wall that adopted
    /// another wall's N for a reading up to 3 ms off, or applied a step
    /// under 3 ms on its own, shows that residue too. A wall adopting several
    /// epochs at once keeps up to one slot of each: −3 ms … (epochs applied)
    /// slots + 3 ms.
    pub last_remainder_100ns: i64,
    /// How far the last regrid (or rejoin) moved the wall READING
    /// (`now_100ns`) at once (100 ns, signed): a jump ahead, or a hold of that
    /// size when negative. For a follow it is measured from the frozen wall,
    /// so it leaves out a resample's 1 ms armed in the same tick and a rejoin
    /// hold the follow re-anchors through (the line's movement is
    /// `moved_100ns`).
    pub last_jump_100ns: i64,
    /// The timeline LINE's net movement in every tick that regridded or
    /// rejoined, summed (100 ns, signed; measured before and after the tick
    /// at its instant, so a resample's 1 ms armed in the SAME tick and a
    /// rejoin hold a follow re-anchors through count as the line really
    /// moved). VBAN's clock owes its change since it last looked
    /// ([`crate::playback::vban_clock::RemainderSlew`]).
    pub moved_100ns: i64,
}

/// The registry every production wall shares (`WallClock::system`).
pub fn global() -> &'static Arc<FleetShift> {
    static GLOBAL: OnceLock<Arc<FleetShift>> = OnceLock::new();
    GLOBAL.get_or_init(Arc::default)
}

/// [`FleetShift::timeline_now_100ns`] of the [`global`] registry.
pub fn timeline_now_100ns() -> i64 {
    global().timeline_now_100ns()
}

/// [`FleetShift::wire_100ns`] of the [`global`] registry.
pub fn wire_100ns(stamp_100ns: i64) -> i64 {
    global().wire_100ns(stamp_100ns)
}

/// [`FleetShift::label_100ns`] of the [`global`] registry.
pub fn label_100ns(t_100ns: i64) -> i64 {
    global().label_100ns(t_100ns)
}

#[cfg(test)]
#[path = "fleet_shift_tests.rs"]
mod tests;
