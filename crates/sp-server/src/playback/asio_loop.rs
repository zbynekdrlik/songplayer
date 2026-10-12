//! #233 (PP's SoundGrid errors, the owner's ruling of 12.10.2026): an ASIO
//! output's LOOP CHECK. The Waves LV1 console loops PP's SoundGrid outputs
//! back to PP's SoundGrid inputs, so what an output sends must return
//! sample for sample, up to a constant gain (its polarity included) after
//! a constant round trip. The driver's own resync requests proved no
//! sufficient error detector (the ticket's trace comment); this compares
//! end to end and names every difference with its UTC time.
//!
//! - The glue (`asio_win.rs`, for an entry with `asio.loop_check`) taps
//!   every buffer switch into two rings ([`LoopTaps`]): what it wrote to
//!   the output's L/R (stereo f32, zeros for the frames the ring did not
//!   deliver, [`copy_played`]) and the driver's first `min(inputs, 8)`
//!   inputs (`asio_format::read_channel`). Both or neither: a full ring
//!   counts an overflow and drops that callback on both sides, so the two
//!   streams stay frame-aligned. Never a wait, a lock or an allocation.
//! - The analyzer runs on its own thread, `loop-<id>`
//!   ([`spawn_loop_thread`], started through the worker's seam, never on the
//!   worker or the driver's thread), draining the rings every 10 ms
//!   ([`LoopCore::tick`]).
//! - Searching: which input carries the sent L and which the sent R, the
//!   round trip to the exact frame (≤ 0.5 s) and each one's gain
//!   (`asio_loop_search.rs`, coarse to fine, its CPU bounded), once every
//!   0.5 s of returned audio, at the lags whose sent window has energy
//!   (over −50 dBFS); a lock needs the window to pass the judge's test.
//! - Locked: each 256-frame block is judged where its sent side has energy,
//!   else it is "blind" ([`verdict`]): a return 40 dB under what was
//!   expected is a `dropout`, a residual (return − gain · sent) over
//!   −40 dB of it a `mismatch`. A run of blocks of one kind is ONE event
//!   (its frames: the run's length); 8 bad blocks in a row search again,
//!   and a lock at another round trip is a `slip` of the difference in
//!   frames (positive: the return comes later, samples were inserted on
//!   the way; negative: lost).
//! - `no_return`: 10 s of sent energy with no lock.
//! - An overflow (the analyzer a second behind) drops the data around it
//!   on both sides and keeps the lock: no false event.
//!
//! An event's UTC time is when the analyzer drained it, less the frames
//! that came after the event's first block at the driver's rate: it lags
//! the real moment by the input latency and up to one driver buffer (a
//! buffer switch delivers an input buffer once it is full). One WARN per
//! event, at most one per 5 s with the count held back (`WarnLimiter`).
//! The counters count since the output was built, across its driver's
//! reopens; a reopen starts a new search (a new round trip is no slip).
//! Status: `outputs[i].asio.loop` ([`LoopStatus`]).
//!
//! Known limits: a strictly periodic program (a test tone) can lock a
//! period off, and the next sound that is not periodic re-locks as a
//! slip; a gain changed on the console reads as a mismatch run, then
//! locks again at the same round trip (no slip); a parked driver (a
//! callback that never returned) keeps its taps, so the analyzer keeps
//! its last state and judges nothing more.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::playback::audio_out_queue::lock;
use crate::playback::stat_window::WarnLimiter;
use crate::playback::vban_packet::clean_f32;

#[path = "asio_loop_search.rs"]
mod search;
pub use search::{DECIMATE, Found, Tap, WINDOW};

/// The driver inputs tapped at most (its first ones).
pub const MAX_LOOP_INPUTS: usize = 8;
/// −50 dBFS as a mean square: a sent block or window under it has no
/// energy to judge or search by.
pub const FLOOR_MS: f64 = 1e-5;
/// −40 dB as an energy ratio: a residual over it of what was expected is a
/// mismatch, a return under it a dropout.
pub const MISMATCH_RATIO: f64 = 1e-4;
/// The frames one verdict judges.
pub const BLOCK: usize = 256;
/// Bad blocks in a row that search again.
pub const RESEARCH_AFTER: u32 = 8;
/// The longest round trip searched, s.
pub const MAX_ROUND_TRIP_S: f64 = 0.5;
/// Returned audio between two search attempts, s.
pub const SEARCH_EVERY_S: f64 = 0.5;
/// Sent energy with no lock before the state reads `no_return`, s.
pub const NO_RETURN_S: f64 = 10.0;
/// The events the status lists, the newest last.
pub const RECENT_EVENTS: usize = 32;
/// Each ring holds this much at the driver's rate, s.
pub const RING_S: f64 = 1.0;
/// The analyzer drains the rings this often.
pub const LOOP_TICK: Duration = Duration::from_millis(10);
/// At most one event WARN per this much (100 ns).
const WARN_EVERY_100NS: i64 = 50_000_000;

/// `seconds` at `rate`, whole frames (rounded down).
pub fn frames_of(seconds: f64, rate: f64) -> u64 {
    (seconds * rate) as u64
}

/// `energy` (a sum of squares over `frames`) is over the −50 dBFS floor.
pub fn has_energy(energy: f64, frames: usize) -> bool {
    energy > FLOOR_MS * frames as f64
}

/// What a block (or a search window) of `frames` reads, per sent channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verdict {
    /// The sent side has no energy: nothing to judge.
    Blind,
    Ok,
    Mismatch,
    Dropout,
}

/// The judge: `sent` (Σ sent²), `back` (Σ return²), `residual`
/// (Σ (return − gain · sent)²) over `frames`. Blind under the floor; a
/// return at or under 40 dB below what was expected (gain² · sent) is a
/// dropout, a residual over that a mismatch.
pub fn verdict(frames: usize, sent: f64, back: f64, residual: f64, gain: f64) -> Verdict {
    if !has_energy(sent, frames) {
        return Verdict::Blind;
    }
    let allowed = gain * gain * sent * MISMATCH_RATIO;
    if back <= allowed {
        Verdict::Dropout
    } else if residual > allowed {
        Verdict::Mismatch
    } else {
        Verdict::Ok
    }
}

/// A gain in dB (its size; a polarity flip reads as its size).
pub fn gain_db(gain: f64) -> f64 {
    20.0 * gain.abs().log10()
}

/// The UTC time of a frame `behind` frames before the newest one the
/// analyzer drained at `now_ms` (see the module doc).
pub fn event_utc_ms(now_ms: i64, behind: u64, rate: f64) -> i64 {
    now_ms - (behind as f64 * 1000.0 / rate).round() as i64
}

/// The loop check's state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopState {
    /// Not checking: the entry has no `loop_check`, or its driver is not
    /// running.
    #[default]
    Off,
    Searching,
    Locked,
    /// Searching for 10 s of sent energy and nothing came back as sent.
    NoReturn,
}

/// What an event was.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopEventKind {
    Mismatch,
    Dropout,
    Slip,
}

/// One difference between what was sent and what returned.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct LoopEvent {
    pub kind: LoopEventKind,
    /// Its first block's UTC time, ms (see the module doc).
    pub utc_ms: i64,
    /// How long ago, s (as of the status).
    pub ago_s: f64,
    /// A mismatch or a dropout: the run's length; a slip: the round trip's
    /// change (positive = the return comes later).
    pub frames: i64,
}

impl LoopEvent {
    /// This event as of `now_ms`.
    fn aged(self, now_ms: i64) -> Self {
        Self {
            ago_s: (now_ms - self.utc_ms) as f64 / 1000.0,
            ..self
        }
    }
}

/// `outputs[i].asio.loop`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LoopStatus {
    pub state: LoopState,
    /// The 0-based driver input that carries the sent L and R (while
    /// locked; `None` for a channel no input carries).
    pub inputs: [Option<u32>; 2],
    /// The round trip, frames at the driver's rate (while locked).
    pub delay_frames: Option<u64>,
    /// Each channel's gain on the way, dB (while locked).
    pub gain_db: [Option<f64>; 2],
    /// Seconds of blocks judged, and of blocks too quiet to judge.
    pub checked_s: f64,
    pub blind_s: f64,
    pub mismatches: u64,
    pub dropouts: u64,
    pub slips: u64,
    /// Buffer switches the taps dropped (the analyzer fell behind).
    pub overflows: u64,
    pub last_event: Option<LoopEvent>,
    /// The last [`RECENT_EVENTS`], oldest first.
    pub recent: Vec<LoopEvent>,
}

/// A lock: the round trip, each channel's tap, and the first returned
/// frame not judged yet.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Locked {
    delay: usize,
    taps: [Option<Tap>; 2],
    judged_to: u64,
}

/// A run of bad blocks of one kind, not recorded yet.
#[derive(Clone, Copy, Debug, PartialEq)]
struct OpenRun {
    kind: LoopEventKind,
    utc_ms: i64,
    blocks: u64,
}

/// The output's figures, kept across its driver's runs.
#[derive(Clone, Debug, Default, PartialEq)]
struct Totals {
    checked_s: f64,
    blind_s: f64,
    mismatches: u64,
    dropouts: u64,
    slips: u64,
    overflows: u64,
    recent: VecDeque<LoopEvent>,
    /// Events recorded since the core last took them (for the WARNs).
    fresh: Vec<LoopEvent>,
}

/// The analyzer (pure: data in, status out). Frames are counted from the
/// run's start, the same on both streams (the taps push both or neither).
#[derive(Default)]
pub struct LoopAnalyzer {
    running: bool,
    rate: f64,
    inputs: usize,
    max_lag: usize,
    search_every: u64,
    no_return_frames: u64,
    sent: [VecDeque<f32>; 2],
    sent_base: u64,
    sent_end: u64,
    recv: Vec<VecDeque<f32>>,
    recv_base: u64,
    recv_end: u64,
    lock: Option<Locked>,
    /// The last lock's round trip this run (a new lock elsewhere is a slip).
    prev_delay: Option<usize>,
    /// The start of the bad run that lost the lock (a slip's time).
    slip_utc: Option<i64>,
    last_attempt: u64,
    /// Sent frames with energy since the search began (`no_return`).
    loud_frames: u64,
    loud_acc: [f64; 2],
    loud_n: usize,
    run: Option<OpenRun>,
    streak: u32,
    streak_utc: i64,
    /// Ticks of data still to drop after an overflow.
    discard: u8,
    overflow_seen: u64,
    search_work: u64,
    totals: Totals,
}

impl LoopAnalyzer {
    /// A new run of the driver at `rate` with `inputs` tapped: the search
    /// starts afresh; the counters and the events go on.
    pub fn start_run(&mut self, rate: f64, inputs: usize) {
        self.end_run();
        let inputs = inputs.min(MAX_LOOP_INPUTS);
        let max_lag = frames_of(MAX_ROUND_TRIP_S, rate) as usize;
        *self = Self {
            running: true,
            rate,
            inputs,
            max_lag: max_lag - max_lag % DECIMATE,
            search_every: frames_of(SEARCH_EVERY_S, rate),
            no_return_frames: frames_of(NO_RETURN_S, rate),
            recv: vec![VecDeque::new(); inputs],
            search_work: self.search_work,
            totals: std::mem::take(&mut self.totals),
            ..Self::default()
        };
    }

    /// The run ended (its driver closed): a bad run so far is an event, and
    /// the state reads off.
    pub fn end_run(&mut self) {
        self.end_event_run();
        self.running = false;
        self.lock = None;
    }

    /// One drain: `received` (interleaved, `inputs` a frame), `sent`
    /// (stereo), the taps' overflow count this run, at `now_ms` (UTC).
    pub fn ingest(&mut self, received: &[f32], sent: &[f32], overflow: u64, now_ms: i64) {
        if !self.running {
            return;
        }
        if overflow != self.overflow_seen {
            self.totals.overflows += overflow - self.overflow_seen;
            self.overflow_seen = overflow;
            // This drain and the next: the gap is in one of them.
            self.discard = 2;
        }
        if self.discard > 0 {
            self.discard -= 1;
            self.skip(received.len() / self.inputs.max(1), sent.len() / 2);
            return;
        }
        self.append(received, sent);
        if self.lock.is_some() {
            self.judge(now_ms);
        } else {
            self.count_loud(sent);
            self.maybe_search(now_ms);
        }
        self.trim();
    }

    /// The events recorded since the last call (for their WARNs).
    pub fn take_fresh(&mut self) -> Vec<LoopEvent> {
        std::mem::take(&mut self.totals.fresh)
    }

    /// The multiply-adds every search so far took.
    pub fn search_work(&self) -> u64 {
        self.search_work
    }

    /// The status as of `now_ms`.
    pub fn status(&self, now_ms: i64) -> LoopStatus {
        let taps = self.lock.map_or([None; 2], |l| l.taps);
        let t = &self.totals;
        LoopStatus {
            state: self.state(),
            inputs: taps.map(|tap| tap.map(|x| x.input as u32)),
            delay_frames: self.lock.map(|l| l.delay as u64),
            gain_db: taps.map(|tap| tap.map(|x| gain_db(x.gain))),
            checked_s: t.checked_s,
            blind_s: t.blind_s,
            mismatches: t.mismatches,
            dropouts: t.dropouts,
            slips: t.slips,
            overflows: t.overflows,
            last_event: t.recent.back().map(|e| e.aged(now_ms)),
            recent: t.recent.iter().map(|e| e.aged(now_ms)).collect(),
        }
    }

    fn state(&self) -> LoopState {
        if !self.running {
            LoopState::Off
        } else if self.lock.is_some() {
            LoopState::Locked
        } else if self.loud_frames >= self.no_return_frames {
            LoopState::NoReturn
        } else {
            LoopState::Searching
        }
    }

    fn append(&mut self, received: &[f32], sent: &[f32]) {
        if self.inputs > 0 {
            for frame in received.chunks_exact(self.inputs) {
                for (ring, &v) in self.recv.iter_mut().zip(frame) {
                    ring.push_back(v);
                }
            }
            self.recv_end += (received.len() / self.inputs) as u64;
        }
        for frame in sent.chunks_exact(2) {
            self.sent[0].push_back(frame[0]);
            self.sent[1].push_back(frame[1]);
        }
        self.sent_end += (sent.len() / 2) as u64;
    }

    /// Frames dropped around an overflow: counted on, never compared (the
    /// lock stays; [`Self::judge`] resumes where both sides are whole).
    fn skip(&mut self, received: usize, sent: usize) {
        self.end_event_run();
        self.streak = 0;
        self.recv_end += received as u64;
        self.sent_end += sent as u64;
        for ring in self.sent.iter_mut().chain(self.recv.iter_mut()) {
            ring.clear();
        }
        self.recv_base = self.recv_end;
        self.sent_base = self.sent_end;
    }

    /// Keep what a search or the judge can still read.
    fn trim(&mut self) {
        let keep = self
            .recv_end
            .saturating_sub((WINDOW + BLOCK) as u64)
            .max(self.recv_base);
        let n = (keep - self.recv_base) as usize;
        for ring in &mut self.recv {
            ring.drain(..n);
        }
        self.recv_base = keep;
        let keep = self
            .recv_end
            .saturating_sub((WINDOW + BLOCK + self.max_lag) as u64)
            .clamp(self.sent_base, self.sent_end);
        let n = (keep - self.sent_base) as usize;
        for ring in &mut self.sent {
            ring.drain(..n);
        }
        self.sent_base = keep;
    }

    /// While searching: count the sent frames with energy, a block at a
    /// time (`no_return`).
    fn count_loud(&mut self, sent: &[f32]) {
        for frame in sent.chunks_exact(2) {
            for (acc, &v) in self.loud_acc.iter_mut().zip(frame) {
                *acc += f64::from(v) * f64::from(v);
            }
            self.loud_n += 1;
            if self.loud_n == BLOCK {
                if self.loud_acc.iter().any(|&e| has_energy(e, BLOCK)) {
                    self.loud_frames += BLOCK as u64;
                }
                self.loud_acc = [0.0; 2];
                self.loud_n = 0;
            }
        }
    }

    /// A search attempt over the last [`WINDOW`] returned frames, every
    /// [`SEARCH_EVERY_S`] of them; its range is what the sent history holds.
    fn maybe_search(&mut self, now_ms: i64) {
        let end = self.recv_end;
        let w = WINDOW as u64;
        if end < self.last_attempt + self.search_every
            || end < self.recv_base + w
            || end > self.sent_end
        {
            return;
        }
        let start = end - w;
        let Some(room) = start.checked_sub(self.sent_base) else {
            return;
        };
        let room = room as usize;
        let max_lag = self.max_lag.min(room - room % DECIMATE);
        self.last_attempt = end;
        for ring in self.sent.iter_mut().chain(self.recv.iter_mut()) {
            ring.make_contiguous();
        }
        let (s0, r0) = (room - max_lag, (start - self.recv_base) as usize);
        let mut work = 0;
        let found = {
            let span = s0..s0 + WINDOW + max_lag;
            let sent = [0usize, 1].map(|c| &self.sent[c].as_slices().0[span.clone()]);
            let recv: Vec<&[f32]> = self
                .recv
                .iter()
                .map(|ring| &ring.as_slices().0[r0..r0 + WINDOW])
                .collect();
            search::search(sent, &recv, max_lag, &mut work)
        };
        self.search_work += work;
        if let Some(found) = found {
            self.lock_on(found, end, now_ms);
        }
    }

    fn lock_on(&mut self, found: Found, end: u64, now_ms: i64) {
        if let Some(prev) = self.prev_delay
            && prev != found.delay
        {
            let utc = self.slip_utc.unwrap_or(now_ms);
            self.record(LoopEventKind::Slip, utc, found.delay as i64 - prev as i64);
        }
        self.prev_delay = Some(found.delay);
        self.slip_utc = None;
        self.loud_frames = 0;
        self.lock = Some(Locked {
            delay: found.delay,
            taps: found.taps,
            judged_to: end,
        });
    }

    /// Judge every whole block both sides hold (the sent side is at least
    /// as far as the returned one: the taps push it first and the core
    /// drains it last, so its frames `delay` earlier are always there).
    fn judge(&mut self, now_ms: i64) {
        let whole = self.recv_end.min(self.sent_end);
        while let Some(lock) = self.lock {
            let delay = lock.delay as u64;
            let j = lock
                .judged_to
                .max(self.recv_base)
                .max(self.sent_base + delay);
            let next = j + BLOCK as u64;
            if next > whole {
                self.lock = Some(Locked {
                    judged_to: j,
                    ..lock
                });
                return;
            }
            let verdict = (0..2)
                .filter_map(|c| lock.taps[c].map(|tap| self.block_verdict(c, tap, j, delay)))
                .max()
                .unwrap_or(Verdict::Blind);
            self.lock = Some(Locked {
                judged_to: next,
                ..lock
            });
            let utc = event_utc_ms(now_ms, self.recv_end - j, self.rate);
            self.note(verdict, utc);
        }
    }

    /// One block of sent channel `channel` against its tap.
    fn block_verdict(&self, channel: usize, tap: Tap, j: u64, delay: u64) -> Verdict {
        let s0 = (j - delay - self.sent_base) as usize;
        let r0 = (j - self.recv_base) as usize;
        let (sent, back) = (&self.sent[channel], &self.recv[tap.input]);
        let (mut ss, mut rr, mut ee) = (0.0, 0.0, 0.0);
        for t in 0..BLOCK {
            let s = f64::from(sent[s0 + t]);
            let r = f64::from(back[r0 + t]);
            let e = r - tap.gain * s;
            ss += s * s;
            rr += r * r;
            ee += e * e;
        }
        verdict(BLOCK, ss, rr, ee, tap.gain)
    }

    /// A judged block's verdict at `utc_ms`: the counters, the runs, the
    /// events, a new search after [`RESEARCH_AFTER`] bad ones in a row.
    fn note(&mut self, verdict: Verdict, utc_ms: i64) {
        let seconds = BLOCK as f64 / self.rate;
        let kind = match verdict {
            Verdict::Blind => {
                self.totals.blind_s += seconds;
                self.calm();
                return;
            }
            Verdict::Ok => {
                self.totals.checked_s += seconds;
                self.calm();
                return;
            }
            Verdict::Mismatch => LoopEventKind::Mismatch,
            Verdict::Dropout => LoopEventKind::Dropout,
        };
        self.totals.checked_s += seconds;
        if self.streak == 0 {
            self.streak_utc = utc_ms;
        }
        self.streak += 1;
        let same_kind = self.run.is_some_and(|run| run.kind == kind);
        if let Some(run) = self.run.as_mut().filter(|_| same_kind) {
            run.blocks += 1;
        } else {
            self.end_event_run();
            self.run = Some(OpenRun {
                kind,
                utc_ms,
                blocks: 1,
            });
        }
        if self.streak >= RESEARCH_AFTER {
            self.end_event_run();
            self.slip_utc = Some(self.streak_utc);
            self.lock = None;
            self.streak = 0;
            self.loud_frames = 0;
            self.last_attempt = self.recv_end;
        }
    }

    /// A block that is not bad: the bad run so far is an event.
    fn calm(&mut self) {
        self.end_event_run();
        self.streak = 0;
    }

    fn end_event_run(&mut self) {
        if let Some(run) = self.run.take() {
            let frames = run.blocks * BLOCK as u64;
            self.record(run.kind, run.utc_ms, frames as i64);
        }
    }

    fn record(&mut self, kind: LoopEventKind, utc_ms: i64, frames: i64) {
        let t = &mut self.totals;
        match kind {
            LoopEventKind::Mismatch => t.mismatches += 1,
            LoopEventKind::Dropout => t.dropouts += 1,
            LoopEventKind::Slip => t.slips += 1,
        }
        let event = LoopEvent {
            kind,
            utc_ms,
            ago_s: 0.0,
            frames,
        };
        t.recent.push_back(event);
        if t.recent.len() > RECENT_EVENTS {
            t.recent.pop_front();
        }
        t.fresh.push(event);
    }

    /// (sent, returned) frames held, per channel.
    #[cfg(test)]
    pub(crate) fn held(&self) -> (usize, usize) {
        (
            self.sent[0].len(),
            self.recv.first().map_or(0, VecDeque::len),
        )
    }
}

/// What the outputs played for one buffer switch, as the tap sends it:
/// `delivered` (the ring's interleaved stereo frames) as the encoder saw
/// them (`clean_f32`), then silence for the frames it did not deliver.
pub fn copy_played(delivered: &[f32], played: &mut [f32]) {
    let mut rest = played.iter_mut();
    for (&x, dst) in delivered.iter().zip(rest.by_ref()) {
        *dst = clean_f32(x);
    }
    for dst in rest {
        *dst = 0.0;
    }
}

/// The glue's ends of a run's rings (one per driver start).
pub struct LoopTaps {
    sent: rtrb::Producer<f32>,
    received: rtrb::Producer<f32>,
    overflow: Arc<AtomicU64>,
}

impl LoopTaps {
    /// One buffer switch: what the outputs played (stereo) and what the
    /// inputs returned (interleaved) — both, or neither and an overflow
    /// (the two streams stay frame-aligned). Never waits.
    pub fn push(&mut self, sent: &[f32], received: &[f32]) {
        if self.sent.slots() >= sent.len() && self.received.slots() >= received.len() {
            let _ = self.sent.push_entire_slice(sent);
            let _ = self.received.push_entire_slice(received);
        } else {
            self.overflow.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// The analyzer's ends of a run's rings.
pub struct LoopFeed {
    sent: rtrb::Consumer<f32>,
    received: rtrb::Consumer<f32>,
    overflow: Arc<AtomicU64>,
    /// The driver's rate.
    pub rate: f64,
    /// The inputs the glue taps (the driver's start says how many).
    pub inputs: usize,
}

impl LoopFeed {
    /// This feed with the inputs the device's start tapped.
    pub fn with_inputs(self, inputs: u32) -> Self {
        Self {
            inputs: (inputs as usize).min(MAX_LOOP_INPUTS),
            ..self
        }
    }

    /// What the rings hold now and the overflows so far (tests).
    #[cfg(test)]
    pub(crate) fn drained(&mut self) -> (Vec<f32>, Vec<f32>, u64) {
        (
            drain(&mut self.sent, 1),
            drain(&mut self.received, 1),
            self.overflow.load(Ordering::Relaxed),
        )
    }
}

/// A run's two rings, [`RING_S`] at `rate` each (the received one sized
/// for [`MAX_LOOP_INPUTS`]): the glue's ends and the analyzer's.
pub fn loop_rings(rate: f64) -> (LoopTaps, LoopFeed) {
    let frames = frames_of(RING_S, rate).max(1) as usize;
    let (sent, sent_out) = rtrb::RingBuffer::new(frames * 2);
    let (received, received_out) = rtrb::RingBuffer::new(frames * MAX_LOOP_INPUTS);
    let overflow = Arc::new(AtomicU64::new(0));
    (
        LoopTaps {
            sent,
            received,
            overflow: overflow.clone(),
        },
        LoopFeed {
            sent: sent_out,
            received: received_out,
            overflow,
            rate,
            inputs: 0,
        },
    )
}

/// Every whole `whole`-sample frame `ring` holds now.
fn drain(ring: &mut rtrb::Consumer<f32>, whole: usize) -> Vec<f32> {
    let n = ring.slots();
    let mut out = vec![0.0; n - n % whole];
    let _ = ring.pop_partial_slice(&mut out);
    out
}

/// The analyzer thread's state: the run's rings, the analyzer, the WARNs'
/// limit.
pub struct LoopCore {
    id: String,
    feed: Option<LoopFeed>,
    analyzer: LoopAnalyzer,
    warns: WarnLimiter,
}

impl LoopCore {
    pub fn new(id: &str) -> Self {
        Self {
            id: id.to_string(),
            feed: None,
            analyzer: LoopAnalyzer::default(),
            warns: WarnLimiter::default(),
        }
    }

    /// One tick at `now_ms` (UTC): a new run's rings (a driver start), what
    /// the rings hold, then the status. A run whose taps are gone (its
    /// driver closed) ends once drained.
    pub fn tick(&mut self, new_feed: Option<LoopFeed>, now_ms: i64) -> LoopStatus {
        if let Some(feed) = new_feed {
            self.analyzer.start_run(feed.rate, feed.inputs);
            self.feed = Some(feed);
        }
        if let Some(feed) = self.feed.as_mut() {
            let gone = feed.sent.is_abandoned();
            let received = drain(&mut feed.received, feed.inputs.max(1));
            let sent = drain(&mut feed.sent, 2);
            let overflow = feed.overflow.load(Ordering::Relaxed);
            self.analyzer.ingest(&received, &sent, overflow, now_ms);
            if gone {
                self.analyzer.end_run();
                self.feed = None;
            }
        }
        for e in self.analyzer.take_fresh() {
            let at = e.utc_ms.saturating_mul(10_000);
            if let Some(held_back) = self.warns.admit(at, WARN_EVERY_100NS) {
                log_event(&self.id, &e, held_back);
            }
        }
        self.analyzer.status(now_ms)
    }

    /// The analyzer (tests).
    #[cfg(test)]
    pub(crate) fn analyzer(&self) -> &LoopAnalyzer {
        &self.analyzer
    }
}

/// An event's WARN (`held_back`: those since the last WARN).
#[cfg_attr(test, mutants::skip)] // logging only; the decision is WarnLimiter's (tested)
fn log_event(id: &str, e: &LoopEvent, held_back: u64) {
    let utc = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(e.utc_ms)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_default();
    warn!(
        id,
        kind = ?e.kind,
        %utc,
        frames = e.frames,
        held_back,
        "asio loop check: what returned is not what was sent (a mismatch or a dropout over the run's frames, or a slip of the round trip by its frames)"
    );
}

/// What the analyzer thread starts with ([`LoopLink::start`]).
pub struct LoopThread {
    pub id: String,
    /// A run's rings, at each driver start.
    pub feeds: Receiver<LoopFeed>,
    /// Where the thread publishes its status each tick.
    pub status: Arc<Mutex<LoopStatus>>,
}

/// Starts an output's analyzer thread: [`spawn_loop_thread`] in the
/// worker, a recorder in the tests (a unit test never gets a live thread).
pub type StartLoop = Box<dyn Fn(LoopThread)>;

/// The worker's side of its output's analyzer thread: it hands each run's
/// rings over and reads the status. Dropped with the worker, which ends the
/// thread.
pub struct LoopLink {
    feeds: Sender<LoopFeed>,
    status: Arc<Mutex<LoopStatus>>,
}

impl LoopLink {
    /// Start the analyzer thread of output `id` through `start`.
    pub fn start(id: &str, start: &dyn Fn(LoopThread)) -> Self {
        let (feeds, rx) = channel();
        let status = Arc::new(Mutex::new(LoopStatus::default()));
        start(LoopThread {
            id: id.to_string(),
            feeds: rx,
            status: status.clone(),
        });
        Self { feeds, status }
    }

    /// A driver start's rings, for the analyzer (a thread that never
    /// started drops them; the status then stays off).
    pub fn run(&self, feed: LoopFeed) {
        let _ = self.feeds.send(feed);
    }

    /// The analyzer's last status.
    pub fn status(&self) -> LoopStatus {
        lock(&self.status).clone()
    }
}

/// The analyzer thread `loop-<id>` (an ordinary thread: it judges what the
/// rings already hold and is never on the audio's path).
#[cfg_attr(test, mutants::skip)] // an OS thread around LoopCore::tick (tested tick by tick)
pub fn spawn_loop_thread(t: LoopThread) {
    let name = format!("loop-{}", t.id);
    let spawned = std::thread::Builder::new()
        .name(name)
        .spawn(move || run_loop_thread(t));
    if let Err(e) = spawned {
        warn!(%e, "asio loop check: spawning the analyzer thread failed - the loop status stays off");
    }
}

/// The analyzer thread's loop: a tick every [`LOOP_TICK`] (or at a new
/// run's rings), until the worker drops its link.
#[cfg_attr(test, mutants::skip)] // a blocking loop around LoopCore::tick (tested)
fn run_loop_thread(t: LoopThread) {
    let mut core = LoopCore::new(&t.id);
    loop {
        let feed = match t.feeds.recv_timeout(LOOP_TICK) {
            Ok(feed) => Some(feed),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        let status = core.tick(feed, chrono::Utc::now().timestamp_millis());
        *lock(&t.status) = status;
    }
}

#[cfg(test)]
#[path = "asio_loop_tests.rs"]
mod tests;
