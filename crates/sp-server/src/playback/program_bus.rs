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
//! **Order.** Two sources offer from two submit threads, so the new source's
//! first frame can arrive before the old source's last one. The bus keeps a
//! small stamp-ordered reorder buffer and releases strictly one boundary after
//! the other. A boundary nobody delivers is MISSED and gets the program's own
//! standby pair (the #147 NV12 black + one silent block), so the program keeps a
//! constant cadence. A boundary is declared missed, once it has been reached,
//! when (see [`ProgramCore::fill_due`]):
//!
//! - it has no owner (no source selected yet);
//! - its owner already offered a LATER stamp (a coalesce gap on that source —
//!   one source offers in stamp order, so the boundary will never come);
//! - its owner has offered nothing for [`PROGRAM_LIVE_WINDOW_100NS`] (an absent
//!   source is filled on time, not 100 ms late);
//! - [`PROGRAM_FILL_GRACE_SLOTS`] slots after the boundary (a stalled source).
//!
//! A frame that arrives for a boundary already served is dropped and counted
//! (`late_dropped`). More than [`GENLOCK_MAX_CATCHUP_INTERVALS`] missed slots in
//! a row RESYNC (like the pacer) instead of bursting old black frames.
//!
//! This file is the PURE, Linux-tested decision layer ([`ProgramCore`]) plus
//! its `Mutex`/`Condvar` wrapper ([`ProgramBus`]) and the settings persistence
//! of the selected source. The `SP-program` sender + its thread live in
//! `program_output.rs`; the offer hook sits in the paced submit thread
//! (`pipeline_paced_submit.rs`).

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use serde::Serialize;
use sp_core::genlock::{
    GENLOCK_GRID_FPS, GENLOCK_MAX_CATCHUP_INTERVALS, UNITS_PER_SECOND, floor_boundary_100ns,
    interval_100ns, lag_slots_100ns, strict_next_boundary_100ns,
};
use sqlx::SqlitePool;
use tracing::{info, warn};

use crate::playback::submit_handoff::{HandoffOutcome, SubmitJob, SubmitQueue};

/// The program output's NDI source name.
pub const PROGRAM_NDI_NAME: &str = "SP-program";

/// DB setting that persists the selected program source (a playlist id).
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

/// One program boundary for the `SP-program` sender.
pub enum ProgramJob {
    /// A source's own boundary job, forwarded unchanged (same `Arc` frame, same
    /// audio block, same stamps).
    Source(SubmitJob),
    /// The program's own standby pair for a missed boundary (#147 NV12 black +
    /// one silent block), stamped on that boundary.
    Standby { stamp_100ns: i64 },
}

impl ProgramJob {
    /// The boundary (video timecode, 100 ns) this job is stamped on.
    pub fn stamp_100ns(&self) -> i64 {
        match self {
            ProgramJob::Source(job) => job.video_tc_100ns,
            ProgramJob::Standby { stamp_100ns } => *stamp_100ns,
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
}

/// The pure program-bus decision layer: ownership, the reorder buffer, the
/// standby fill, and the bounded queue to the `SP-program` sender. No clock and
/// no threads — every call takes `now_100ns` explicitly.
pub struct ProgramCore {
    fps: i64,
    /// `(first_stamp, pid)`, ascending by `first_stamp`.
    segments: Vec<(i64, i64)>,
    /// The last boundary committed to the program queue.
    last: Option<i64>,
    /// Owned source jobs waiting for an earlier boundary, keyed by stamp.
    pending: BTreeMap<i64, SubmitJob>,
    /// The last stamp each source offered (its liveness + progress).
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

    /// Whether `pid` owns, or may still own, a program boundary — the cheap
    /// pre-check that lets every other source skip the offer entirely.
    pub fn is_candidate(&self, pid: i64) -> bool {
        self.segments.iter().any(|&(_, p)| p == pid)
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
    /// `now_100ns` plus [`CUT_LEAD_SLOTS`]; the previous owner keeps every stamp
    /// before it. A later cut replaces one recorded on the same or a later
    /// boundary. Returns `false` (nothing recorded) when `pid` is already the
    /// selected source.
    pub fn cut(&mut self, pid: i64, now_100ns: i64) -> bool {
        if self.selected() == Some(pid) {
            return false;
        }
        let mut boundary = strict_next_boundary_100ns(now_100ns, self.fps);
        for _ in 0..CUT_LEAD_SLOTS {
            boundary = strict_next_boundary_100ns(boundary, self.fps);
        }
        self.segments.retain(|&(first, _)| first < boundary);
        self.segments.push((boundary, pid));
        self.health.cuts += 1;
        self.prune();
        true
    }

    /// A source offers its boundary job (right after its own submit). Records
    /// the source's progress, keeps the job only when the source owns the
    /// boundary and the boundary is still open, then releases whatever is ready.
    pub fn offer(&mut self, pid: i64, job: SubmitJob, now_100ns: i64) -> OfferOutcome {
        let stamp = job.video_tc_100ns;
        self.last_offer.insert(pid, stamp);
        if self.owner_of(stamp) != Some(pid) {
            return OfferOutcome::NotOwner;
        }
        if self.last.is_some_and(|l| stamp <= l) || self.pending.contains_key(&stamp) {
            self.health.late_dropped += 1;
            return OfferOutcome::Late;
        }
        self.pending.insert(stamp, job);
        self.release(now_100ns);
        OfferOutcome::Accepted
    }

    /// Whether boundary `boundary_100ns` (not delivered yet) counts as MISSED at
    /// `now_100ns` — never before it is reached, then the four cases of the
    /// module doc.
    pub fn fill_due(&self, boundary_100ns: i64, now_100ns: i64) -> bool {
        if now_100ns < boundary_100ns {
            return false; // never fill a boundary before it is reached
        }
        let Some(owner) = self.owner_of(boundary_100ns) else {
            return true;
        };
        let Some(&offered) = self.last_offer.get(&owner) else {
            return true;
        };
        if offered >= strict_next_boundary_100ns(boundary_100ns, self.fps) {
            return true;
        }
        if now_100ns - offered > PROGRAM_LIVE_WINDOW_100NS {
            return true;
        }
        now_100ns >= boundary_100ns + PROGRAM_FILL_GRACE_SLOTS * interval_100ns(self.fps)
    }

    /// Release, in stamp order, every boundary that is ready at `now_100ns`:
    /// the owned frame when it is here, the standby pair when the boundary is
    /// missed, a resync when more than 8 slots were missed. Stops at the first
    /// boundary that is neither here nor missed yet.
    pub fn release(&mut self, now_100ns: i64) {
        let floor_now = floor_boundary_100ns(now_100ns, self.fps);
        for _ in 0..MAX_RELEASE_STEPS {
            let expected = match self.last {
                Some(last) => strict_next_boundary_100ns(last, self.fps),
                None => self.pending.keys().next().copied().unwrap_or(floor_now),
            };
            if let Some(job) = self.pending.remove(&expected) {
                self.health.forwarded += 1;
                self.commit(ProgramJob::Source(job));
                continue;
            }
            let overflow = self.pending.len() > PROGRAM_PENDING_BOUND;
            if !overflow && !self.fill_due(expected, now_100ns) {
                return;
            }
            if lag_slots_100ns(expected, floor_now, self.fps) > GENLOCK_MAX_CATCHUP_INTERVALS {
                let target = self
                    .pending
                    .keys()
                    .next()
                    .map_or(floor_now, |&first| first.min(floor_now));
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
        }
    }

    fn lock(&self) -> MutexGuard<'_, BusState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// See [`ProgramCore::is_candidate`].
    pub fn is_candidate(&self, pid: i64) -> bool {
        self.lock().core.is_candidate(pid)
    }

    /// See [`ProgramCore::offer`].
    pub fn offer(&self, pid: i64, job: SubmitJob, now_100ns: i64) -> OfferOutcome {
        let outcome = self.lock().core.offer(pid, job, now_100ns);
        self.ready.notify_one(); // the waiter re-checks the queue itself
        outcome
    }

    /// Release missed boundaries (the sender thread's per-boundary check).
    pub fn release_due(&self, now_100ns: i64) {
        self.lock().core.release(now_100ns);
        self.ready.notify_one();
    }

    /// Cut the program to `pid` (see [`ProgramCore::cut`]) and return the new
    /// state.
    pub fn cut(&self, pid: i64, now_100ns: i64) -> ProgramStatus {
        let mut st = self.lock();
        st.core.cut(pid, now_100ns);
        st.core.status()
    }

    /// See [`ProgramCore::select_initial`].
    pub fn select_initial(&self, pid: i64) {
        self.lock().core.select_initial(pid);
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
/// boundary (every other source pays one lock and nothing else).
pub fn program_copy(bus: &ProgramBus, pid: i64, job: &SubmitJob) -> Option<SubmitJob> {
    bus.is_candidate(pid).then(|| job.clone())
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

/// Restore the persisted program source into `bus` (startup). Returns the
/// restored playlist id; a missing or unreadable setting leaves the program on
/// its standby pair.
pub async fn restore_selected_source(pool: &SqlitePool, bus: &ProgramBus) -> Option<i64> {
    let raw = match crate::db::models::get_setting(pool, SETTING_PROGRAM_SOURCE).await {
        Ok(v) => v?,
        Err(e) => {
            warn!(%e, "program bus: reading the persisted source failed");
            return None;
        }
    };
    match raw.trim().parse::<i64>() {
        Ok(pid) => {
            bus.select_initial(pid);
            info!(source = pid, "program bus: restored the selected source");
            Some(pid)
        }
        Err(e) => {
            warn!(%e, raw = %raw, "program bus: persisted source is not a playlist id");
            None
        }
    }
}

#[cfg(test)]
#[path = "program_bus_tests.rs"]
mod tests;
