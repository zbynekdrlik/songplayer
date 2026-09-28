//! The program bus (#209, B1 of EPIC #174): SongPlayer is the master switcher.
//!
//! One process-wide [`ProgramBus`] feeds SongPlayer's own program output, the
//! NDI sender [`PROGRAM_NDI_NAME`] (`SP-program`). Every paced playlist output
//! already emits ONE video frame + ONE 1600-sample audio block per boundary of
//! the shared 30 fps genlock grid, idle or playing (#147). Right after its own
//! submit, each paced submit thread OFFERS the SAME boundary job to the bus: the
//! same `SharedFrame` (an `Arc` bump, zero pixel copy) and the same audio block,
//! with the same stamps. The bus forwards it only when that source OWNS the
//! boundary, so the program frame at boundary B IS the selected source's frame
//! at B — no NDI re-receive, no second decode, no second clock.
//!
//! **Ownership.** The cut state is a short list of `(first_stamp, pid)`
//! segments. After a cut from `from` to `to` at `cut_boundary`, `to` owns every
//! stamp `>= cut_boundary` and `from` owns every stamp `< cut_boundary`. A cut
//! lands on the NEXT boundary + [`CUT_LEAD_SLOTS`] slot: far enough ahead that
//! both sources emit it after the cut is recorded, so exactly one frame per
//! boundary reaches the program — no hole, no double.
//!
//! **One clock domain: the sources' stamps.** The API's realtime clock and a
//! submit thread's per-song wall can both sit off the stamp walls after a UTC
//! step (the pacer walls slew it in at ≤ 1 ms per resample). So "next" for a
//! cut is measured from the newest stamp the program sources, the source cut
//! to, or the program itself reached (the caller's clock is only the fallback
//! when nothing was seen), and a missed boundary is declared by TIME only on
//! the `SP-program` sender's own long-lived wall (`release`), which it ticks
//! once per grid boundary like the pacer walls (`program_output::BoundaryTicker`)
//! so both slew a UTC step in at the same rate. The offer path (`offer`) never
//! reads a clock: it forwards, fills only a gap the owner is already past (or
//! the missing boundary when the reorder buffer overflows), and measures a
//! resync against the waiting frame. Every paced source reports its progress on every boundary
//! (`touch`), so the source cut to is known to be live before its first owned
//! frame arrives.
//!
//! **Order.** Two sources offer from two submit threads, so the new source's
//! first frame can arrive before the old source's last one. The bus keeps a
//! small stamp-ordered reorder buffer and releases strictly one boundary after
//! the other. A boundary nobody delivers is MISSED and gets the program's own
//! standby pair (the #147 NV12 black + one silent block), so the program keeps a
//! constant cadence. On the sender's wall a boundary is declared missed, once
//! it has been reached, when (see [`ProgramCore::fill_due`]):
//!
//! - it has no owner (no source selected yet);
//! - its owner already touched or offered a LATER stamp (a coalesce gap on it —
//!   one source works in stamp order, so the boundary will never come);
//! - its owner has touched or offered nothing for [`PROGRAM_LIVE_WINDOW_100NS`] (an absent
//!   source is filled on time, not 100 ms late);
//! - [`PROGRAM_FILL_GRACE_SLOTS`] slots after the boundary (a stalled source).
//!
//! A frame that arrives for a boundary already served is dropped and counted
//! (`late_dropped`). More than [`GENLOCK_MAX_CATCHUP_INTERVALS`] missed slots in
//! a row RESYNC (like the pacer) instead of bursting old black frames.
//!
//! **Transitions (#215).** A cut is a transition WINDOW
//! (`program_transition::Window`): `to` owns every boundary from the cut
//! boundary on, and `from` ALSO contributes to the window's `n` boundaries. For
//! each of them the bus waits for BOTH sources' pairs, with the same reorder
//! and fill rules per source (a side that is missed is left `None` and mixed
//! against the standby). It then queues ONE [`ProgramJob::Mix`], which the
//! sender crossfades. A Cut is a zero-length window, the #209 behaviour above,
//! unchanged. A fade first waits for the incoming source's first LIVE pair
//! (the cue gate), with the outgoing source held on program at full level —
//! the window half lives in the child module `program_bus_window.rs`. The
//! engine asks [`ProgramCore::hold_for`] whether a source that left its OBS
//! scene must keep playing through a window ([`Hold`]).
//!
//! This file is the PURE, Linux-tested decision layer ([`ProgramCore`]) plus
//! its `Mutex`/`Condvar` wrapper ([`ProgramBus`]) and the settings persistence
//! of the selected source. The `SP-program` sender + its thread live in
//! `program_output.rs`; the offer hook sits in the paced submit thread
//! (`paced_output.rs`).

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use serde::Serialize;
use sp_core::genlock::{
    GENLOCK_GRID_FPS, GENLOCK_MAX_CATCHUP_INTERVALS, UNITS_PER_SECOND, floor_boundary_100ns,
    interval_100ns, lag_slots_100ns, strict_next_boundary_100ns,
};
use sqlx::SqlitePool;
use tokio::sync::watch;
use tracing::{info, warn};

use crate::playback::ndi_input::NdiInputShared;
use crate::playback::program_follow::FollowShared;
use crate::playback::program_on_air::OnAir;
use crate::playback::program_transition::{
    ActiveWindow, Cue, MixJob, SpecSource, TransitionCounters, TransitionSpec, TransitionStatus,
    Window,
};
use crate::playback::submit_handoff::{HandoffOutcome, SubmitJob, SubmitQueue};
use crate::playback::vban_out::VbanOut;
use crate::playback::wallclock::utc_now_100ns;
use crate::remote::RemoteShared;

/// The program output's NDI source name.
pub const PROGRAM_NDI_NAME: &str = "SP-program";

/// DB setting that persists the selected program source (a playlist id, or
/// `sp_core::config::PROGRAM_INPUT_ID` for the #212 NDI input).
pub const SETTING_PROGRAM_SOURCE: &str = "program_source";

/// A cut takes effect this many slots after the NEXT boundary (design: next
/// boundary + 1 slot, ~33–67 ms after the click).
pub const CUT_LEAD_SLOTS: i64 = 1;

/// How long the program waits for a LIVE source's boundary before it fills the
/// slot with its own standby pair: 3 slots (100 ms) covers the measured p99
/// submit-thread lateness (~90 ms, #168 box test 5).
pub const PROGRAM_FILL_GRACE_SLOTS: i64 = 3;

/// A source that has offered nothing for this long (1 s) is ABSENT: its
/// boundaries are filled on time instead of after the grace.
pub const PROGRAM_LIVE_WINDOW_100NS: i64 = UNITS_PER_SECOND;

/// Program submit-queue depth: one full catch-up gap fill (8 slots) plus the
/// forwarded frame behind it, with one to spare.
pub const PROGRAM_QUEUE_BOUND: usize = GENLOCK_MAX_CATCHUP_INTERVALS as usize + 2;

/// Reorder-buffer bound: more owned frames than this waiting behind one missing
/// boundary force that boundary to be filled.
pub const PROGRAM_PENDING_BOUND: usize = 2 * GENLOCK_MAX_CATCHUP_INTERVALS as usize;

/// Upper bound on release steps per call (fills + forwards + a resync), so a
/// release is always bounded work.
const MAX_RELEASE_STEPS: usize = 64;

// #215: what a boundary inside a transition window becomes (the mix, the cue
// gate's held boundaries) — an `impl ProgramCore` child for the 1000-line cap.
#[path = "program_bus_window.rs"]
mod window;
use window::WindowStep;

/// One program boundary for the `SP-program` sender.
pub enum ProgramJob {
    /// A source's own boundary job, forwarded unchanged (same `Arc` frame, same
    /// audio block, same stamps).
    Source(SubmitJob),
    /// The program's own standby pair for a missed boundary (#147 NV12 black +
    /// one silent block), stamped on that boundary.
    Standby { stamp_100ns: i64 },
    /// #215: a boundary inside a transition window: both sources' pairs,
    /// crossfaded by the sender.
    Mix(MixJob),
}

impl ProgramJob {
    /// The boundary (video timecode, 100 ns) this job is stamped on.
    pub fn stamp_100ns(&self) -> i64 {
        match self {
            ProgramJob::Source(job) => job.video_tc_100ns,
            ProgramJob::Standby { stamp_100ns } => *stamp_100ns,
            ProgramJob::Mix(mix) => mix.stamp_100ns,
        }
    }
}

/// What [`ProgramCore::offer`] did with a source's boundary job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OfferOutcome {
    /// Another source owns this boundary — not forwarded.
    NotOwner,
    /// The boundary was already served (filled or forwarded) — dropped.
    Late,
    /// Taken into the program (released at once, or once earlier boundaries are
    /// settled).
    Accepted,
}

/// Program telemetry (`GET /api/v1/program` → `health`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ProgramHealth {
    /// Source boundaries forwarded to the program.
    pub forwarded: u64,
    /// Missed boundaries filled with the program's standby pair.
    pub filled: u64,
    /// Owned source frames that arrived after their boundary was served.
    pub late_dropped: u64,
    /// Runs of more than 8 missed slots skipped instead of burst-filled.
    pub resyncs: u64,
    /// Program jobs dropped because the program sender fell a full queue behind.
    pub coalesced: u64,
    /// Cuts recorded.
    pub cuts: u64,
    /// Pairs the `SP-program` sender actually submitted.
    pub submitted: u64,
    /// `SP-program` receiver connections (polled ~1/s by the output thread).
    pub connections: i32,
    /// Boundary of the last submitted pair (100 ns); 0 = none yet.
    pub last_stamp_100ns: i64,
}

/// The program state served by `GET /api/v1/program`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProgramStatus {
    pub ndi_name: &'static str,
    /// The selected source (playlist id); `None` = nothing selected yet (the
    /// program carries its standby pair).
    pub source: Option<i64>,
    /// The source before the latest cut, while it still owns boundaries.
    pub previous: Option<i64>,
    /// First boundary the selected source owns; `None` for a source restored
    /// at startup (it owns every boundary).
    pub cut_boundary_100ns: Option<i64>,
    pub health: ProgramHealth,
    /// #215: the transition the next cut uses, the running window and the
    /// transition counters.
    pub transition: TransitionStatus,
}

/// #215: why the engine keeps a playlist that left its OBS scene playing
/// ([`ProgramCore::hold_for`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hold {
    /// The source is the `from` of a window (or a cut) not served yet: keep it
    /// playing until this stamp, one slot after the window's end.
    Until(i64),
    /// The source is still the selected one. A cut away from it may still be
    /// on its way: the follow task and the #213 remote control cut only AFTER
    /// cg OBS switched.
    OnProgram,
}

/// The pure program-bus decision layer: ownership, the reorder buffer, the
/// standby fill, and the bounded queue to the `SP-program` sender. No clock and
/// no threads — the time-based calls (`release`, `fill_due`, `cut`) take
/// `now_100ns`.
pub struct ProgramCore {
    fps: i64,
    /// `(first_stamp, pid)`, ascending by `first_stamp`.
    segments: Vec<(i64, i64)>,
    /// The last boundary committed to the program queue.
    last: Option<i64>,
    /// Owned source jobs waiting for an earlier boundary, keyed by stamp.
    pending: BTreeMap<i64, SubmitJob>,
    /// #215: the outgoing sources' jobs for window boundaries, keyed by stamp.
    from_pending: BTreeMap<i64, SubmitJob>,
    /// #215: transition windows not served to their end yet, ascending, never
    /// overlapping.
    windows: Vec<Window>,
    /// #215: the transition the next cut uses.
    spec: TransitionSpec,
    counters: TransitionCounters,
    /// The last stamp each source touched or offered (its liveness + progress).
    last_offer: HashMap<i64, i64>,
    queue: SubmitQueue<ProgramJob>,
    health: ProgramHealth,
}

impl Default for ProgramCore {
    fn default() -> Self {
        Self::new()
    }
}

impl ProgramCore {
    /// An empty program on the shared genlock grid: no source selected.
    pub fn new() -> Self {
        Self {
            fps: GENLOCK_GRID_FPS,
            segments: Vec::new(),
            last: None,
            pending: BTreeMap::new(),
            from_pending: BTreeMap::new(),
            windows: Vec::new(),
            spec: TransitionSpec::cut(SpecSource::Fallback),
            counters: TransitionCounters::default(),
            last_offer: HashMap::new(),
            queue: SubmitQueue::new(PROGRAM_QUEUE_BOUND),
            health: ProgramHealth::default(),
        }
    }

    /// The source that owns boundary `stamp_100ns`, if any.
    pub fn owner_of(&self, stamp_100ns: i64) -> Option<i64> {
        self.segments
            .iter()
            .rev()
            .find(|&&(first, _)| first <= stamp_100ns)
            .map(|&(_, pid)| pid)
    }

    /// Whether `pid` owns, or may still own, a program boundary (#215: or
    /// still contributes to a window) — the cheap pre-check that lets every
    /// other source skip the offer entirely.
    pub fn is_candidate(&self, pid: i64) -> bool {
        self.segments.iter().any(|&(_, p)| p == pid)
            || self.windows.iter().any(|w| w.from == Some(pid))
    }

    /// #215: the transition every later cut uses (the follow task keeps it in
    /// step with cg OBS and the settings). Returns whether it changed.
    pub fn set_transition(&mut self, spec: TransitionSpec) -> bool {
        let changed = self.spec != spec;
        self.spec = spec;
        changed
    }

    /// #215: whether the engine must keep `pid` playing although its OBS scene
    /// left program (see [`Hold`]). `None` = pause it now, as before. A window
    /// still waiting for its cue holds its outgoing source to the latest end
    /// the window can reach (the cue gate's wait included).
    pub fn hold_for(&self, pid: i64) -> Option<Hold> {
        let end = self
            .windows
            .iter()
            .filter(|w| w.from == Some(pid))
            .map(|w| w.end_100ns)
            .max();
        match end {
            Some(end) => Some(Hold::Until(strict_next_boundary_100ns(end, self.fps))),
            None => (self.selected() == Some(pid)).then_some(Hold::OnProgram),
        }
    }

    /// The selected source (the latest cut's target).
    pub fn selected(&self) -> Option<i64> {
        self.segments.last().map(|&(_, pid)| pid)
    }

    /// Select `pid` as the source of EVERY boundary (the persisted selection at
    /// startup — there is nothing to cut away from).
    pub fn select_initial(&mut self, pid: i64) {
        self.segments = vec![(i64::MIN, pid)];
    }

    /// Cut the program to `pid`. The cut boundary is the next boundary after
    /// the newest stamp the involved sources (the program's segments + `pid` +
    /// every window's `from`) or the program itself reached — `now_100ns` (the
    /// caller's clock) only when nothing was seen — plus [`CUT_LEAD_SLOTS`].
    /// The previous owner keeps every stamp before it. A later cut replaces one
    /// recorded on the same or a later boundary; a cut back to the source that
    /// still owns that boundary cancels the pending cut. #215: the cut opens a
    /// transition window of the current spec ([`Self::set_transition`]; a Cut
    /// = zero boundaries), fading out of the source ON AIR: a waiting or
    /// frozen window whose span reaches the new boundary kept its outgoing
    /// source there, and the new cut freezes a waiting one (it never opens;
    /// `Window::holds_on_air`). Returns `false` (nothing recorded) when `pid`
    /// is already the selected source.
    pub fn cut(&mut self, pid: i64, now_100ns: i64) -> bool {
        if self.selected() == Some(pid) {
            return false;
        }
        // Cut in the sources' stamp domain (module doc): the newest stamp the
        // involved sources or the program reached; the caller's clock only as
        // the fallback.
        let seen = self
            .segments
            .iter()
            .map(|&(_, p)| p)
            .chain(std::iter::once(pid))
            .chain(self.windows.iter().filter_map(|w| w.from))
            .filter_map(|p| self.last_offer.get(&p).copied())
            .chain(self.last)
            .max();
        let from = seen.unwrap_or(now_100ns);
        let mut boundary = strict_next_boundary_100ns(from, self.fps);
        for _ in 0..CUT_LEAD_SLOTS {
            boundary = strict_next_boundary_100ns(boundary, self.fps);
        }
        self.segments.retain(|&(first, _)| first < boundary);
        // #215: the source on air, read BEFORE the windows this cut replaces
        // are dropped: a waiting one cut ON the boundary kept its outgoing
        // source on program (one cut after it never did, `on_air` skips it).
        let outgoing = self.on_air(boundary);
        // A window that has not started at the new cut boundary is replaced
        // (or cancelled); a running one ends where the new cut starts, and one
        // still waiting for its cue is frozen there when its span reaches it
        // (`Window::truncate`). Its outgoing source's frames are all older
        // than `boundary`: the cut is placed after the newest stamp of every
        // window's `from` too.
        self.windows.retain(|w| w.cut_100ns < boundary);
        for w in &mut self.windows {
            if w.truncate(boundary) {
                info!(
                    from = ?w.from,
                    to = w.to,
                    "program transition: a later cut froze the fade still waiting for its cue"
                );
            }
        }
        // A cut back to the source that still owns the boundary (A→B→A inside
        // one slot) just cancels the pending cut; a cut back to the source a
        // waiting cue kept on air takes the boundaries over with no window.
        if outgoing != Some(pid) {
            self.segments.push((boundary, pid));
            let window = Window::cued(outgoing, pid, boundary, &self.spec);
            self.windows.push(window);
        } else if self.selected() != Some(pid) {
            self.segments.push((boundary, pid));
        }
        self.health.cuts += 1;
        self.prune();
        true
    }

    /// Every paced source reports its progress here on every boundary (via
    /// [`program_copy`]), program candidate or not, so a source that is cut to
    /// is already known to be live. Returns whether `pid` can own a boundary.
    pub fn touch(&mut self, pid: i64, stamp_100ns: i64) -> bool {
        self.last_offer.insert(pid, stamp_100ns);
        self.is_candidate(pid)
    }

    /// A source offers its boundary job (right after its own submit). Records
    /// the source's progress, keeps the job only when the source owns the
    /// boundary and the boundary is still open, then forwards whatever is
    /// contiguous. It reads no clock (module doc): the submit thread's wall is
    /// not the stamps' wall, so time-based misses are the sender's call.
    pub fn offer(&mut self, pid: i64, job: SubmitJob) -> OfferOutcome {
        let stamp = job.video_tc_100ns;
        self.last_offer.insert(pid, stamp);
        // #215: inside a window the outgoing source contributes too.
        let from_side = self.window_at(stamp).is_some_and(|w| w.from == Some(pid));
        if !from_side && self.owner_of(stamp) != Some(pid) {
            return OfferOutcome::NotOwner;
        }
        let late = self.last.is_some_and(|l| stamp <= l);
        let waiting = if from_side {
            &mut self.from_pending
        } else {
            &mut self.pending
        };
        if late || waiting.contains_key(&stamp) {
            self.health.late_dropped += 1;
            return OfferOutcome::Late;
        }
        waiting.insert(stamp, job);
        self.release_inner(None);
        OfferOutcome::Accepted
    }

    /// Whether boundary `boundary_100ns` (not delivered yet) counts as MISSED at
    /// `now_100ns` — never before it is reached, then the four cases of the
    /// module doc.
    pub fn fill_due(&self, boundary_100ns: i64, now_100ns: i64) -> bool {
        match self.owner_of(boundary_100ns) {
            Some(owner) => self.source_missed(owner, boundary_100ns, Some(now_100ns)),
            None => now_100ns >= boundary_100ns, // never before it is reached
        }
    }

    /// Whether `src`'s pair for `boundary_100ns` counts as missed (#215: per
    /// source, for both sides of a window boundary). With a clock: never
    /// before the boundary is reached, then the source passed it, is absent,
    /// or the grace ran out. Without one (the offer path): only when it passed.
    fn source_missed(&self, src: i64, boundary_100ns: i64, now_100ns: Option<i64>) -> bool {
        let offered = self.last_offer.get(&src).copied();
        let next = strict_next_boundary_100ns(boundary_100ns, self.fps);
        let passed = offered.is_some_and(|o| o >= next);
        let Some(now) = now_100ns else {
            return passed;
        };
        if now < boundary_100ns {
            return false; // never fill a boundary before it is reached
        }
        let Some(offered) = offered.filter(|_| !passed) else {
            return true; // passed it, or never offered anything
        };
        now - offered > PROGRAM_LIVE_WINDOW_100NS
            || now >= boundary_100ns + PROGRAM_FILL_GRACE_SLOTS * interval_100ns(self.fps)
    }

    /// Whether the owner of `boundary_100ns` already touched or offered a
    /// LATER stamp — one source works in stamp order, so the boundary will
    /// never come (a coalesce gap on that source). Needs no clock.
    pub fn owner_passed(&self, boundary_100ns: i64) -> bool {
        self.owner_of(boundary_100ns)
            .is_some_and(|owner| self.source_missed(owner, boundary_100ns, None))
    }

    /// The `SP-program` sender's per-boundary check on its own wall: release,
    /// in stamp order, every boundary that is ready at `now_100ns` — the owned
    /// frame when it is here, the standby pair when the boundary is missed
    /// ([`fill_due`](Self::fill_due)), a resync when more than 8 slots were
    /// missed. Stops at the first boundary that is neither here nor missed yet.
    pub fn release(&mut self, now_100ns: i64) {
        self.release_inner(Some(now_100ns));
    }

    /// [`release`](Self::release) with `now = None` is the clock-free offer
    /// path: a boundary counts as missed only when its owner is already past it
    /// (or the reorder buffer overflows), and "how far behind" is measured
    /// against the waiting frame instead of a clock.
    fn release_inner(&mut self, now_100ns: Option<i64>) {
        let floor_now = now_100ns.map(|n| floor_boundary_100ns(n, self.fps));
        for _ in 0..MAX_RELEASE_STEPS {
            let first_pending = self.first_waiting();
            let Some(expected) = self
                .last
                .map(|last| strict_next_boundary_100ns(last, self.fps))
                .or(first_pending)
                .or(floor_now)
            else {
                return;
            };
            let missed = match self.window_at(expected) {
                // #215: a window boundary carries BOTH sources' pairs: mixed,
                // or held while the cue waits (`program_bus_window.rs`).
                Some(w) => match self.window_step(&w, expected, now_100ns) {
                    WindowStep::Next => continue,
                    WindowStep::Missed(missed) => missed,
                },
                None => {
                    if let Some(job) = self.pending.remove(&expected) {
                        self.health.forwarded += 1;
                        self.commit(ProgramJob::Source(job));
                        continue;
                    }
                    match now_100ns {
                        Some(now) => self.fill_due(expected, now),
                        None => self.owner_passed(expected),
                    }
                }
            };
            if !self.overflowing() && !missed {
                return;
            }
            let Some(horizon) = floor_now.or(first_pending) else {
                return;
            };
            if lag_slots_100ns(expected, horizon, self.fps) > GENLOCK_MAX_CATCHUP_INTERVALS {
                let target = first_pending.map_or(horizon, |first| first.min(horizon));
                self.last = Some(floor_boundary_100ns(target - 1, self.fps));
                self.health.resyncs += 1;
                continue;
            }
            self.health.filled += 1;
            self.commit(ProgramJob::Standby {
                stamp_100ns: expected,
            });
        }
    }

    /// #215: more waiting jobs than the reorder bound, on either side.
    fn overflowing(&self) -> bool {
        self.pending.len() > PROGRAM_PENDING_BOUND
            || self.from_pending.len() > PROGRAM_PENDING_BOUND
    }

    /// The oldest waiting job's stamp, on either side.
    fn first_waiting(&self) -> Option<i64> {
        let to = self.pending.keys().next().copied();
        let from = self.from_pending.keys().next().copied();
        to.into_iter().chain(from).min()
    }

    /// Queue one boundary for the `SP-program` sender and advance.
    fn commit(&mut self, job: ProgramJob) {
        self.last = Some(job.stamp_100ns());
        if let HandoffOutcome::Coalesced { .. } = self.queue.offer(job) {
            self.health.coalesced += 1;
        }
        self.prune();
    }

    /// Forget segments that can no longer own a boundary still to come.
    fn prune(&mut self) {
        let Some(last) = self.last else {
            return;
        };
        let next = strict_next_boundary_100ns(last, self.fps);
        let dead = self
            .segments
            .iter()
            .skip(1)
            .filter(|&&(first, _)| first <= next)
            .count();
        self.segments.drain(..dead);
        // #215: a window whose last boundary is served is done.
        let before = self.windows.len();
        self.windows.retain(|w| w.end_100ns > next);
        self.counters.transitions_done += (before - self.windows.len()) as u64;
    }

    /// The next queued program boundary for the sender.
    pub fn take(&mut self) -> Option<ProgramJob> {
        self.queue.take()
    }

    /// Number of program boundaries queued for the sender.
    pub fn queued(&self) -> usize {
        self.queue.depth()
    }

    /// The sender submitted the pair stamped `stamp_100ns`.
    pub fn record_submitted(&mut self, stamp_100ns: i64) {
        self.health.submitted += 1;
        self.health.last_stamp_100ns = stamp_100ns;
    }

    /// Latest `SP-program` receiver connection count.
    pub fn set_connections(&mut self, n: i32) {
        self.health.connections = n;
    }

    /// The program state for the API.
    pub fn status(&self) -> ProgramStatus {
        ProgramStatus {
            ndi_name: PROGRAM_NDI_NAME,
            source: self.selected(),
            previous: self.segments.iter().rev().nth(1).map(|&(_, pid)| pid),
            cut_boundary_100ns: self
                .segments
                .last()
                .map(|&(first, _)| first)
                .filter(|&first| first != i64::MIN),
            health: self.health,
            transition: TransitionStatus {
                kind: self.spec.kind,
                duration_ms: self.spec.duration_ms,
                n_slots: self.spec.n_slots,
                source: self.spec.source,
                active: self
                    .windows
                    .iter()
                    .find(|w| w.n_slots > 0 && w.cue != Cue::Frozen)
                    .map(|w| ActiveWindow::of(w, self.last)),
                counters: self.counters,
            },
        }
    }
}

/// What the `SP-program` sender thread got from [`ProgramBus::take_timeout`].
pub enum Take {
    Job(ProgramJob),
    /// The wait timed out with nothing queued — check for missed boundaries.
    Idle,
    /// The bus was stopped and the queue is drained.
    Stopped,
}

struct BusState {
    core: ProgramCore,
    stop: bool,
}

/// The thread-safe program bus: [`ProgramCore`] behind a `Mutex`, plus a
/// `Condvar` that wakes the `SP-program` sender thread when a boundary is
/// queued. Every method holds the lock for µs (no SDK call under it).
pub struct ProgramBus {
    state: Mutex<BusState>,
    ready: Condvar,
    /// #210: the program's VBAN audio output, fed by the `SP-program` sender
    /// thread and reported under `vban` on `GET /api/v1/program`.
    vban: Arc<VbanOut>,
    /// #212: the NDI input "OBS manuál" (source id `PROGRAM_INPUT_ID`): its
    /// settings, stop flag and telemetry (`input` on `GET /api/v1/program`).
    input: Arc<NdiInputShared>,
    /// #213: the Companion remote control's telemetry (`remote` on
    /// `GET /api/v1/program`).
    remote: Arc<RemoteShared>,
    /// #215: the OBS-follow telemetry (`follow` on `GET /api/v1/program`).
    follow: Arc<FollowShared>,
    /// #213: serializes [`persist_and_cut`] — the API and the remote control
    /// can cut concurrently, and the persisted source must be the one cut last.
    cut_serial: tokio::sync::Mutex<()>,
    /// #221: what is on air, published on every cut and on the startup
    /// selection (`program_on_air.rs`).
    on_air: watch::Sender<OnAir>,
    /// #221: one scene switch at a time, in arrival order, across every
    /// client (`program_switch::switch_scene`).
    switch_order: tokio::sync::Mutex<()>,
}

impl Default for ProgramBus {
    fn default() -> Self {
        Self::new()
    }
}

impl ProgramBus {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(BusState {
                core: ProgramCore::new(),
                stop: false,
            }),
            ready: Condvar::new(),
            vban: Arc::new(VbanOut::new()),
            input: Arc::new(NdiInputShared::default()),
            remote: Arc::new(RemoteShared::default()),
            follow: Arc::new(FollowShared::default()),
            cut_serial: tokio::sync::Mutex::new(()),
            on_air: watch::channel(OnAir::default()).0,
            switch_order: tokio::sync::Mutex::new(()),
        }
    }

    /// #210: the program's VBAN output.
    pub fn vban(&self) -> &Arc<VbanOut> {
        &self.vban
    }

    /// #212: the NDI input's shared state.
    pub fn input(&self) -> &Arc<NdiInputShared> {
        &self.input
    }

    /// #213: the remote control's telemetry.
    pub fn remote(&self) -> &Arc<RemoteShared> {
        &self.remote
    }

    /// #215: the OBS-follow telemetry.
    pub fn follow(&self) -> &Arc<FollowShared> {
        &self.follow
    }

    /// See [`ProgramCore::set_transition`].
    pub fn set_transition(&self, spec: TransitionSpec) -> bool {
        self.lock().core.set_transition(spec)
    }

    /// See [`ProgramCore::hold_for`].
    pub fn hold_for(&self, pid: i64) -> Option<Hold> {
        self.lock().core.hold_for(pid)
    }

    fn lock(&self) -> MutexGuard<'_, BusState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// See [`ProgramCore::is_candidate`].
    pub fn is_candidate(&self, pid: i64) -> bool {
        self.lock().core.is_candidate(pid)
    }

    /// See [`ProgramCore::touch`].
    pub fn touch(&self, pid: i64, stamp_100ns: i64) -> bool {
        self.lock().core.touch(pid, stamp_100ns)
    }

    /// See [`ProgramCore::offer`].
    pub fn offer(&self, pid: i64, job: SubmitJob) -> OfferOutcome {
        let outcome = self.lock().core.offer(pid, job);
        self.ready.notify_one(); // the waiter re-checks the queue itself
        outcome
    }

    /// Release missed boundaries (the sender thread's per-boundary check).
    pub fn release_due(&self, now_100ns: i64) {
        self.lock().core.release(now_100ns);
        self.ready.notify_one();
    }

    /// Cut the program to `pid` (see [`ProgramCore::cut`]), publish it as on
    /// air for `scene`, and return the new state. #221: EVERY cut is
    /// published, a cut to the source already on air too, under the state
    /// lock (so in cut order).
    pub fn cut(&self, pid: i64, now_100ns: i64, scene: Option<&str>) -> ProgramStatus {
        let mut st = self.lock();
        st.core.cut(pid, now_100ns);
        self.publish(pid, scene);
        st.core.status()
    }

    /// See [`ProgramCore::select_initial`]; published as on air for `scene`.
    pub fn select_initial(&self, pid: i64, scene: Option<&str>) {
        let mut st = self.lock();
        st.core.select_initial(pid);
        self.publish(pid, scene);
    }

    /// #221: publish `source` as on air for `scene` (`seq` + 1). Always
    /// `send_modify`: `send` drops the value while nobody subscribed yet.
    fn publish(&self, source: i64, scene: Option<&str>) {
        self.on_air
            .send_modify(|on_air| *on_air = on_air.next(source, scene));
    }

    /// #221: a receiver of what is on air; it has seen the current value, so
    /// `changed` waits for the next publication.
    pub fn on_air(&self) -> watch::Receiver<OnAir> {
        self.on_air.subscribe()
    }

    /// #221: what is on air now.
    pub fn on_air_now(&self) -> OnAir {
        self.on_air.borrow().clone()
    }

    /// #221: the order of the scene switches (`program_switch`).
    pub fn switch_order(&self) -> &tokio::sync::Mutex<()> {
        &self.switch_order
    }

    pub fn status(&self) -> ProgramStatus {
        self.lock().core.status()
    }

    /// Sender thread: the next queued boundary, waiting at most `wait` for one.
    pub fn take_timeout(&self, wait: Duration) -> Take {
        let st = self.lock();
        let (mut st, _) = self
            .ready
            .wait_timeout_while(st, wait, |s| s.core.queued() == 0 && !s.stop)
            .unwrap_or_else(|p| p.into_inner());
        match st.core.take() {
            Some(job) => Take::Job(job),
            None if st.stop => Take::Stopped,
            None => Take::Idle,
        }
    }

    /// See [`ProgramCore::record_submitted`].
    pub fn record_submitted(&self, stamp_100ns: i64) {
        self.lock().core.record_submitted(stamp_100ns);
    }

    /// See [`ProgramCore::set_connections`].
    pub fn set_connections(&self, n: i32) {
        self.lock().core.set_connections(n);
    }

    /// Stop the sender thread once the queue is drained (process shutdown).
    pub fn stop(&self) {
        self.lock().stop = true;
        self.ready.notify_all();
    }
}

/// A copy of a source's boundary job for the program — an `Arc` bump of the
/// frame and one ≤ 12.8 KB audio block — taken BEFORE the source's own submit
/// moves the frame into its holdover, and only when `pid` can own a program
/// boundary. Every source records its progress here on every boundary
/// ([`ProgramCore::touch`]); a source that cannot own one pays that one lock
/// and nothing else.
pub fn program_copy(bus: &ProgramBus, pid: i64, job: &SubmitJob) -> Option<SubmitJob> {
    bus.touch(pid, job.video_tc_100ns).then(|| job.clone())
}

static PROGRAM_BUS: OnceLock<Arc<ProgramBus>> = OnceLock::new();

/// Install the process-wide bus the paced submit threads offer to. Returns
/// `false` when one is already installed (the first one stays).
pub fn install(bus: Arc<ProgramBus>) -> bool {
    PROGRAM_BUS.set(bus).is_ok()
}

/// The process-wide bus, once installed at startup.
pub fn installed() -> Option<&'static Arc<ProgramBus>> {
    PROGRAM_BUS.get()
}

/// Persist the selected program source (a playlist id).
pub async fn persist_selected_source(pool: &SqlitePool, pid: i64) -> Result<(), sqlx::Error> {
    crate::db::models::set_setting(pool, SETTING_PROGRAM_SOURCE, &pid.to_string()).await
}

/// Persist `pid` as the selected source FIRST, then cut the program to it (a
/// failed write cuts nothing) and publish it as on air for `scene` (#221). The
/// one cut path of `POST /api/v1/program/cut` and the #213 remote control; two
/// cuts never interleave, so the persisted source is always the one on
/// program.
pub async fn persist_and_cut(
    pool: &SqlitePool,
    bus: &ProgramBus,
    pid: i64,
    scene: Option<&str>,
) -> Result<ProgramStatus, sqlx::Error> {
    let _serial = bus.cut_serial.lock().await;
    persist_selected_source(pool, pid).await?;
    Ok(bus.cut(pid, utc_now_100ns(), scene))
}

/// Restore the persisted program source into `bus` (startup). Returns the
/// restored source id; a missing or unreadable setting leaves the program on
/// its standby pair. The #212 NDI input (`PROGRAM_INPUT_ID`) is restored only
/// while it is active (enabled with a source) — otherwise it is not a source.
/// #221: published as on air with the playlist's catalog scene
/// (`scene_catalog::scene_of_source`; none for the input).
pub async fn restore_selected_source(pool: &SqlitePool, bus: &ProgramBus) -> Option<i64> {
    let raw = match crate::db::models::get_setting(pool, SETTING_PROGRAM_SOURCE).await {
        Ok(v) => v?,
        Err(e) => {
            warn!(%e, "program bus: reading the persisted source failed");
            return None;
        }
    };
    let pid = match raw.trim().parse::<i64>() {
        Ok(pid) => pid,
        Err(e) => {
            warn!(%e, raw = %raw, "program bus: persisted source is not a source id");
            return None;
        }
    };
    if pid == sp_core::config::PROGRAM_INPUT_ID && !input_active(pool).await {
        warn!(
            source = pid,
            "program bus: the persisted source is the NDI input, which is disabled or has no source — not restored"
        );
        return None;
    }
    let scene = crate::playback::scene_catalog::scene_of_source(pool, pid).await;
    bus.select_initial(pid, scene.as_deref());
    info!(source = pid, scene = ?scene, "program bus: restored the selected source");
    Some(pid)
}

/// #212: whether the stored settings make the NDI input a source (enabled
/// with a source name; an unreadable setting counts as not).
async fn input_active(pool: &SqlitePool) -> bool {
    crate::playback::ndi_input::load_input_settings(pool)
        .await
        .is_ok_and(|s| s.active())
}

#[cfg(test)]
#[path = "program_bus_tests.rs"]
mod tests;
#[cfg(test)]
#[path = "program_bus_tests_cue.rs"]
mod tests_cue;
#[cfg(test)]
#[path = "program_bus_tests_transition.rs"]
mod tests_transition;
