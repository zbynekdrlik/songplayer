//! #147: the per-boundary trace of `SP-program` — the instrument for the
//! clumped arrival strih's FIFO sees (design record 6051091817).
//!
//! strih's receiver sees `SP-program`'s frames arrive in clumps: many empty
//! 33 ms ticks, a shed frame when a clump overfills, mostly around an
//! on-air song start. The sender already reads five instants per boundary
//! (`program_output_timing::BoundaryMarks`), but `health.timing` keeps only
//! each figure's worst over 60–120 s, so the per-boundary SEQUENCE (a job
//! taken 30 ms late, then one taken 2 ms late: two frames ~5 ms apart on the
//! wire) shows nowhere. This module keeps that sequence:
//!
//! - [`ProgramTrace`]: a preallocated ring of the last [`TRACE_CAPACITY`]
//!   boundaries (10 min at 30 fps). ONE writer, the `SP-program` sender
//!   thread ([`TraceWriter`], claimed once), writes each record after the
//!   boundary's NDI submit returned: no allocation, no lock, no log. A reader
//!   (the API, the minute summary) never blocks it: every slot is a seqlock
//!   of atomics, and a slot written over while it is read is skipped.
//! - [`TraceRecord`]: the boundary, its wire stamp, the UTC (ms) of its
//!   submit return, the four other instants as µs after the boundary, the
//!   source the bus says it shows, what it was ([`TraceKind`]), whether its
//!   pair was the source's own content, and the video id of a new on-air
//!   song on its first live boundary ([`ProgramTrace::mark_song`]).
//! - [`TraceSpan`]: the window `GET /api/v1/program/trace` asks for, at most
//!   [`TRACE_MAX_SPAN_MS`] (2 min) long; a longer one is clamped and says so.
//! - [`ClumpFlags`]: the clump detector. A boundary taken more than one slot
//!   late ([`CLUMP_LATE_US`]), or submitted less than [`CLUMP_CLOSE_US`]
//!   after the boundary before it, is a clump boundary.
//! - [`MinuteLog`]: what the once-a-minute task (`program_trace_log.rs`)
//!   logs: one INFO line, only for a minute that held a clump boundary.
//!
//! Everything here is pure (atomics, no thread, no clock) and Linux-tested.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering, fence};
use std::sync::{Arc, Mutex, PoisonError, TryLockError};

use serde::Serialize;

use crate::playback::fleet_shift::{shift_100ns, wire_stamp_100ns};
use crate::playback::program_bus::ProgramJob;
use crate::playback::program_output_timing::BoundaryMarks;

/// Boundaries the trace holds: 10 min at the 30 fps grid.
pub const TRACE_CAPACITY: usize = 18_000;

/// The longest window one `GET /api/v1/program/trace` answers (ms, 2 min).
pub const TRACE_MAX_SPAN_MS: i64 = 120_000;

/// A boundary whose job was taken more than this after the boundary (µs,
/// one grid slot) is a clump boundary (`late`).
pub const CLUMP_LATE_US: i64 = 33_333;

/// A boundary submitted less than this after the boundary before it (µs) is
/// a clump boundary (`close`): two frames that close on the wire land in one
/// receiver tick.
pub const CLUMP_CLOSE_US: i64 = 10_000;

/// A song mark not taken within this many boundaries (10 s) is dropped: its
/// song's first live boundary went out without it.
pub const SONG_MARK_MAX_BOUNDARIES: u64 = 300;

/// What `None` (no source, no song) is stored as.
const NONE: i64 = i64::MIN;

/// Words per record ([`encode`]).
const WORDS: usize = 12;

/// What a traced boundary was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TraceKind {
    /// A forwarded pair of the source on program before it.
    Source,
    /// A forwarded pair of another source than the boundary before (a
    /// cut's first boundary, or the program's first).
    Cut,
    /// The program's standby pair for a missed boundary.
    Fill,
    /// A boundary of a transition window: both sides mixed.
    Fade,
}

impl TraceKind {
    /// The kind's name in the API's rows.
    pub fn label(self) -> &'static str {
        match self {
            TraceKind::Source => "src",
            TraceKind::Cut => "cut",
            TraceKind::Fill => "fill",
            TraceKind::Fade => "fade",
        }
    }

    fn from_code(code: u64) -> Self {
        match code {
            1 => TraceKind::Cut,
            2 => TraceKind::Fill,
            3 => TraceKind::Fade,
            _ => TraceKind::Source,
        }
    }
}

/// What a program job alone says about its boundary: forwarded, filled or
/// faded, and whether its pair is the source's own content (`live`: a
/// forwarded live pair, a fade whose incoming pair is live).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JobShape {
    /// [`TraceKind::Source`], [`TraceKind::Fill`] or [`TraceKind::Fade`]; the
    /// writer tells a cut from a forward.
    pub kind: TraceKind,
    pub live: bool,
}

impl JobShape {
    /// The shape of `job`, read before the sender consumes it.
    pub fn of(job: &ProgramJob) -> Self {
        let (kind, live) = match job {
            ProgramJob::Source(job) => (TraceKind::Source, job.live),
            ProgramJob::Standby { .. } => (TraceKind::Fill, false),
            ProgramJob::Mix(mix) => (TraceKind::Fade, mix.to.as_ref().is_some_and(|j| j.live)),
        };
        Self { kind, live }
    }
}

/// One traced boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TraceRecord {
    /// Records written before this one: its place in the trace.
    pub index: u64,
    /// The boundary on the sender's timeline (100 ns).
    pub stamp_100ns: i64,
    /// Its wire stamp, what a receiver sees (100 ns, under the K it was
    /// sent with).
    pub wire_100ns: i64,
    /// UTC of the submit return (ms): the fleet label of that reading.
    pub utc_ms: i64,
    /// The job taken, its audio handed to the outputs, its video side
    /// started, its NDI submit returned: µs after the boundary.
    pub taken_us: i64,
    pub fed_us: i64,
    pub submit_start_us: i64,
    pub submitted_us: i64,
    /// The source the bus says the boundary shows (`None`: none selected).
    pub source: Option<i64>,
    pub kind: TraceKind,
    /// The pair is the source's own content ([`JobShape::live`]).
    pub live: bool,
    /// The video id of a new on-air song, on its first live boundary.
    pub song: Option<i64>,
}

impl TraceRecord {
    /// The submit return on the sender's timeline (100 ns).
    pub fn submitted_at_100ns(&self) -> i64 {
        self.stamp_100ns + self.submitted_us * 10
    }
}

/// µs from the boundary `stamp_100ns` to the instant `at_100ns` (negative
/// before it).
fn us_from(stamp_100ns: i64, at_100ns: i64) -> i64 {
    (at_100ns - stamp_100ns) / 10
}

fn opt_word(value: Option<i64>) -> u64 {
    value.unwrap_or(NONE) as u64
}

fn opt_of(word: u64) -> Option<i64> {
    let value = word as i64;
    (value != NONE).then_some(value)
}

/// A record as the words its slot stores.
fn encode(r: &TraceRecord) -> [u64; WORDS] {
    [
        r.index,
        r.stamp_100ns as u64,
        r.wire_100ns as u64,
        r.utc_ms as u64,
        r.taken_us as u64,
        r.fed_us as u64,
        r.submit_start_us as u64,
        r.submitted_us as u64,
        opt_word(r.source),
        r.kind as u64,
        u64::from(r.live),
        opt_word(r.song),
    ]
}

/// The record a slot's words hold.
fn decode(w: [u64; WORDS]) -> TraceRecord {
    TraceRecord {
        index: w[0],
        stamp_100ns: w[1] as i64,
        wire_100ns: w[2] as i64,
        utc_ms: w[3] as i64,
        taken_us: w[4] as i64,
        fed_us: w[5] as i64,
        submit_start_us: w[6] as i64,
        submitted_us: w[7] as i64,
        source: opt_of(w[8]),
        kind: TraceKind::from_code(w[9]),
        live: w[10] != 0,
        song: opt_of(w[11]),
    }
}

/// One record's slot: a seqlock of atomics. Its sequence is odd while the
/// writer fills it and grows by 2 per record, so a reader that saw the same
/// even sequence before and after reading the words read one whole record.
struct Slot {
    seq: AtomicU64,
    words: [AtomicU64; WORDS],
}

impl Slot {
    fn new() -> Self {
        Self {
            seq: AtomicU64::new(0),
            words: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    /// The writer starts a record: the sequence turns odd. Returns the even
    /// sequence it had, for [`close`](Self::close).
    fn open(&self) -> u64 {
        let seq = self.seq.load(Ordering::Relaxed);
        self.seq.store(seq + 1, Ordering::Relaxed);
        fence(Ordering::Release);
        seq
    }

    fn fill(&self, words: [u64; WORDS]) {
        for (slot, word) in self.words.iter().zip(words) {
            slot.store(word, Ordering::Relaxed);
        }
    }

    /// The record opened at `seq` is whole: the sequence is even again.
    fn close(&self, seq: u64) {
        self.seq.store(seq + 2, Ordering::Release);
    }

    /// The slot's words; `None` while the writer fills it or when it wrote
    /// the slot again while they were read. `between` runs after the words
    /// are read, before the sequence is read again (the tests' writer).
    fn load_with(&self, between: impl FnOnce()) -> Option<[u64; WORDS]> {
        let seq = self.seq.load(Ordering::Acquire);
        if seq % 2 == 1 {
            return None;
        }
        let words = std::array::from_fn(|i| self.words[i].load(Ordering::Relaxed));
        fence(Ordering::Acquire);
        between();
        (self.seq.load(Ordering::Relaxed) == seq).then_some(words)
    }
}

/// A song that started on air, waiting for its first live boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SongMark {
    source: i64,
    video_id: i64,
    /// Records written when it was marked.
    at: u64,
}

/// The ring of the last boundaries `SP-program` sent (module doc).
pub struct ProgramTrace {
    slots: Box<[Slot]>,
    /// Records written so far: the next record's index.
    written: AtomicU64,
    /// A [`TraceWriter`] is alive.
    writer: AtomicBool,
    /// The song mark: set by the engine, taken by the writer, which only
    /// ever `try_lock`s it (it never waits on the engine).
    mark: Mutex<Option<SongMark>>,
}

impl Default for ProgramTrace {
    fn default() -> Self {
        Self::new()
    }
}

impl ProgramTrace {
    /// The production trace: [`TRACE_CAPACITY`] records.
    pub fn new() -> Self {
        Self::with_capacity(TRACE_CAPACITY)
    }

    /// A trace of `capacity` records (at least 1), allocated now.
    pub fn with_capacity(capacity: usize) -> Self {
        assert!(capacity >= 1, "a trace holds at least one record");
        Self {
            slots: (0..capacity).map(|_| Slot::new()).collect(),
            written: AtomicU64::new(0),
            writer: AtomicBool::new(false),
            mark: Mutex::new(None),
        }
    }

    /// Records the ring holds at most.
    pub fn capacity(&self) -> u64 {
        self.slots.len() as u64
    }

    /// Records written since the trace was built.
    pub fn written(&self) -> u64 {
        self.written.load(Ordering::Acquire)
    }

    /// Records the ring holds now.
    pub fn held(&self) -> u64 {
        self.written().min(self.capacity())
    }

    /// The trace's one writer; `None` while another is alive.
    pub fn writer(self: &Arc<Self>) -> Option<TraceWriter> {
        let claimed =
            self.writer
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire);
        claimed.ok()?;
        Some(TraceWriter {
            trace: self.clone(),
            last_source: None,
        })
    }

    /// The engine: `source`'s song `video_id` started on air. The writer
    /// puts it on `source`'s next live boundary (within
    /// [`SONG_MARK_MAX_BOUNDARIES`]); a later mark replaces it.
    pub fn mark_song(&self, source: i64, video_id: i64) {
        let at = self.written();
        let mut mark = self.mark.lock().unwrap_or_else(PoisonError::into_inner);
        *mark = Some(SongMark {
            source,
            video_id,
            at,
        });
    }

    /// Record `index`, if the ring still holds it whole.
    pub fn read(&self, index: u64) -> Option<TraceRecord> {
        self.read_with(index, || {})
    }

    /// [`read`](Self::read), with `between` run inside the slot's read.
    fn read_with(&self, index: u64, between: impl FnOnce()) -> Option<TraceRecord> {
        if index >= self.written() {
            return None;
        }
        let slot = &self.slots[(index % self.capacity()) as usize];
        let record = decode(slot.load_with(between)?);
        (record.index == index).then_some(record)
    }

    /// The records held from index `first` on, oldest first.
    pub fn records_from(&self, first: u64) -> Vec<TraceRecord> {
        self.held_from(first).collect()
    }

    fn held_from(&self, first: u64) -> impl Iterator<Item = TraceRecord> + '_ {
        let end = self.written();
        let start = first.max(end.saturating_sub(self.capacity()));
        (start..end).filter_map(|index| self.read(index))
    }

    /// The records whose submit returned inside `span`, oldest first.
    pub fn window(&self, span: &TraceSpan) -> Vec<TraceRecord> {
        self.held_from(0)
            .filter(|r| span.contains(r.utc_ms))
            .collect()
    }

    /// The oldest record held.
    pub fn oldest(&self) -> Option<TraceRecord> {
        self.held_from(0).next()
    }

    /// The newest record.
    pub fn newest(&self) -> Option<TraceRecord> {
        self.written().checked_sub(1).and_then(|i| self.read(i))
    }

    /// The writer: put `record` in its slot (its index is [`written`](Self::written)).
    fn push(&self, record: &TraceRecord) {
        let slot = &self.slots[(record.index % self.capacity()) as usize];
        let seq = slot.open();
        slot.fill(encode(record));
        slot.close(seq);
        self.written.store(record.index + 1, Ordering::Release);
    }
}

/// The trace's one writer (the `SP-program` sender thread). Dropping it
/// lets another be claimed.
pub struct TraceWriter {
    trace: Arc<ProgramTrace>,
    /// The source of the last record written (a cut is a forward of another).
    last_source: Option<i64>,
}

impl TraceWriter {
    /// Write the boundary served at `marks`: its `shape`, the `source` the
    /// bus says it shows, under the fleet shift `k` (K_F, for its wire stamp
    /// and its UTC). A forward of another source than the record before is a
    /// [`TraceKind::Cut`]; a live record of the marked song's source takes
    /// the song mark.
    pub fn record(&mut self, marks: &BoundaryMarks, source: Option<i64>, shape: JobShape, k: i64) {
        let index = self.trace.written();
        let kind = match shape.kind {
            TraceKind::Source if source != self.last_source => TraceKind::Cut,
            kind => kind,
        };
        self.last_source = source;
        let stamp = marks.stamp_100ns;
        let record = TraceRecord {
            index,
            stamp_100ns: stamp,
            wire_100ns: wire_stamp_100ns(stamp, k),
            utc_ms: (marks.submitted_100ns + shift_100ns(k)).div_euclid(10_000),
            taken_us: us_from(stamp, marks.taken_100ns),
            fed_us: us_from(stamp, marks.fed_100ns),
            submit_start_us: us_from(stamp, marks.submit_start_100ns),
            submitted_us: us_from(stamp, marks.submitted_100ns),
            source,
            kind,
            live: shape.live,
            song: self.take_song(source, shape.live, index),
        };
        self.trace.push(&record);
    }

    /// The marked song, when record `index` (of `source`, `live`) is its
    /// first live boundary. A mark older than [`SONG_MARK_MAX_BOUNDARIES`]
    /// is dropped. The mark is only `try_lock`ed: while the engine sets it,
    /// the next boundary takes it.
    fn take_song(&self, source: Option<i64>, live: bool, index: u64) -> Option<i64> {
        let mut slot = match self.trace.mark.try_lock() {
            Ok(slot) => slot,
            Err(TryLockError::Poisoned(slot)) => slot.into_inner(),
            Err(TryLockError::WouldBlock) => return None,
        };
        let mark = (*slot)?;
        if index.saturating_sub(mark.at) > SONG_MARK_MAX_BOUNDARIES {
            *slot = None;
            return None;
        }
        if !live || source != Some(mark.source) {
            return None;
        }
        *slot = None;
        Some(mark.video_id)
    }
}

impl Drop for TraceWriter {
    fn drop(&mut self) {
        self.trace.writer.store(false, Ordering::Release);
    }
}

/// The window `[from, to)` (UTC ms) of a trace query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TraceSpan {
    pub from_utc_ms: i64,
    pub to_utc_ms: i64,
    /// The query asked for more than [`TRACE_MAX_SPAN_MS`]: `to` was moved to
    /// `from` + the maximum.
    pub clamped: bool,
}

impl TraceSpan {
    /// The window a query asks for: `to` defaults to `now_ms`, `from` to
    /// [`TRACE_MAX_SPAN_MS`] before `to`. One longer than the maximum ends
    /// the maximum after `from`. A `from` after `to` is refused.
    pub fn resolve(from: Option<i64>, to: Option<i64>, now_ms: i64) -> Result<Self, &'static str> {
        let to = to.unwrap_or(now_ms);
        let from = from.unwrap_or_else(|| to.saturating_sub(TRACE_MAX_SPAN_MS));
        if from > to {
            return Err("from_utc_ms is after to_utc_ms");
        }
        let clamped = to.saturating_sub(from) > TRACE_MAX_SPAN_MS;
        let to = if clamped {
            from.saturating_add(TRACE_MAX_SPAN_MS)
        } else {
            to
        };
        Ok(Self {
            from_utc_ms: from,
            to_utc_ms: to,
            clamped,
        })
    }

    /// Whether `utc_ms` is inside the window.
    pub fn contains(&self, utc_ms: i64) -> bool {
        (self.from_utc_ms..self.to_utc_ms).contains(&utc_ms)
    }
}

/// The clump detector's verdict on one record.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClumpFlags {
    /// Taken more than [`CLUMP_LATE_US`] after its boundary.
    pub late: bool,
    /// Submitted less than [`CLUMP_CLOSE_US`] after the record before it.
    pub close: bool,
}

impl ClumpFlags {
    /// The flags of `record`, `prev` being the record before it.
    pub fn of(prev: Option<&TraceRecord>, record: &TraceRecord) -> Self {
        let gap_us = prev.map(|p| (record.submitted_at_100ns() - p.submitted_at_100ns()) / 10);
        Self {
            late: record.taken_us > CLUMP_LATE_US,
            close: gap_us.is_some_and(|gap| gap < CLUMP_CLOSE_US),
        }
    }
}

/// The detector over consecutive records: each one against the one before.
#[derive(Clone, Copy, Debug, Default)]
pub struct ClumpScan {
    prev: Option<TraceRecord>,
}

impl ClumpScan {
    /// A scan whose first record follows `prev`.
    pub fn after(prev: Option<TraceRecord>) -> Self {
        Self { prev }
    }

    /// The flags of the next record.
    pub fn next(&mut self, record: &TraceRecord) -> ClumpFlags {
        let flags = ClumpFlags::of(self.prev.as_ref(), record);
        self.prev = Some(*record);
        flags
    }
}

/// Counts over a run of records.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ClumpCounts {
    pub boundaries: u64,
    /// Records flagged `late` / `close`.
    pub late: u64,
    pub close: u64,
    /// Records that carry a song start.
    pub songs: u64,
}

impl ClumpCounts {
    fn add(&mut self, record: &TraceRecord, flags: ClumpFlags) {
        self.boundaries += 1;
        self.late += u64::from(flags.late);
        self.close += u64::from(flags.close);
        self.songs += u64::from(record.song.is_some());
    }
}

/// The names of a row's columns, in order.
pub const TRACE_COLUMNS: [&str; 12] = [
    "utc_ms",
    "wire_100ns",
    "source",
    "kind",
    "live",
    "taken_us",
    "fed_us",
    "submit_start_us",
    "submitted_us",
    "late",
    "close",
    "song",
];

/// One record as a row of [`TRACE_COLUMNS`] (a JSON array).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TraceRow(
    i64,
    i64,
    Option<i64>,
    &'static str,
    u8,
    i64,
    i64,
    i64,
    i64,
    u8,
    u8,
    Option<i64>,
);

impl TraceRow {
    fn of(r: &TraceRecord, flags: ClumpFlags) -> Self {
        Self(
            r.utc_ms,
            r.wire_100ns,
            r.source,
            r.kind.label(),
            u8::from(r.live),
            r.taken_us,
            r.fed_us,
            r.submit_start_us,
            r.submitted_us,
            u8::from(flags.late),
            u8::from(flags.close),
            r.song,
        )
    }
}

/// `GET /api/v1/program/trace`'s answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TraceAnswer {
    pub from_utc_ms: i64,
    pub to_utc_ms: i64,
    pub clamped: bool,
    pub max_span_ms: i64,
    pub capacity: u64,
    pub held: u64,
    pub oldest_utc_ms: Option<i64>,
    pub newest_utc_ms: Option<i64>,
    pub clumps: ClumpCounts,
    pub columns: [&'static str; 12],
    pub rows: Vec<TraceRow>,
}

impl TraceAnswer {
    /// The answer to a query of `from` / `to` (UTC ms as text, as sent) at
    /// `now_ms`. A value that is not an integer, or a `from` after `to`, is
    /// refused with a fixed text (never what was sent).
    pub fn build(
        trace: &ProgramTrace,
        from: Option<&str>,
        to: Option<&str>,
        now_ms: i64,
    ) -> Result<Self, &'static str> {
        let from = parse_ms(from, "from_utc_ms must be an integer (UTC ms)")?;
        let to = parse_ms(to, "to_utc_ms must be an integer (UTC ms)")?;
        let span = TraceSpan::resolve(from, to, now_ms)?;
        let records = trace.window(&span);
        let before = records.first().and_then(|r| r.index.checked_sub(1));
        let mut scan = ClumpScan::after(before.and_then(|i| trace.read(i)));
        let mut clumps = ClumpCounts::default();
        let rows = records
            .iter()
            .map(|r| {
                let flags = scan.next(r);
                clumps.add(r, flags);
                TraceRow::of(r, flags)
            })
            .collect();
        Ok(Self {
            from_utc_ms: span.from_utc_ms,
            to_utc_ms: span.to_utc_ms,
            clamped: span.clamped,
            max_span_ms: TRACE_MAX_SPAN_MS,
            capacity: trace.capacity(),
            held: trace.held(),
            oldest_utc_ms: trace.oldest().map(|r| r.utc_ms),
            newest_utc_ms: trace.newest().map(|r| r.utc_ms),
            clumps,
            columns: TRACE_COLUMNS,
            rows,
        })
    }
}

/// An optional integer query value; `refusal` when it is not one.
fn parse_ms(value: Option<&str>, refusal: &'static str) -> Result<Option<i64>, &'static str> {
    value
        .map(|v| v.parse::<i64>().map_err(|_| refusal))
        .transpose()
}

/// A minute that held a clump boundary: what its INFO line says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MinuteSummary {
    pub counts: ClumpCounts,
    /// UTC (ms) of its first and last record.
    pub first_utc_ms: i64,
    pub last_utc_ms: i64,
}

/// The once-a-minute summary: the records written since the last call,
/// each one against the record before it (the minute before's last too).
#[derive(Debug, Default)]
pub struct MinuteLog {
    /// The first record not summed yet.
    next: u64,
    scan: ClumpScan,
}

impl MinuteLog {
    /// Sum the records written since the last call; `Some` only when one of
    /// them is a clump boundary (the minute's INFO line).
    pub fn minute(&mut self, trace: &ProgramTrace) -> Option<MinuteSummary> {
        let records = trace.records_from(self.next);
        let (first, last) = (records.first()?, records.last()?);
        self.next = last.index + 1;
        let mut counts = ClumpCounts::default();
        for record in &records {
            let flags = self.scan.next(record);
            counts.add(record, flags);
        }
        let clumped = counts.late + counts.close > 0;
        clumped.then_some(MinuteSummary {
            counts,
            first_utc_ms: first.utc_ms,
            last_utc_ms: last.utc_ms,
        })
    }
}

#[cfg(test)]
#[path = "program_trace_tests.rs"]
mod tests;
#[cfg(test)]
#[path = "program_trace_tests_bus.rs"]
mod tests_bus;
