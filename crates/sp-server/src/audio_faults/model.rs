//! #257: what the audio fault ledger reads and writes — a path's counters,
//! events, states and meters as one tick reads them ([`Probe`]), and the row
//! it writes ([`FaultRow`]). Pure data, no I/O.

/// How a row weighs (`audio_faults.severity`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    /// Samples lost, inserted or wrong.
    Loss,
    /// Late or squeezed, the samples still whole.
    Risk,
    /// A path's (re)start: no fault, it bounds "clean since".
    Info,
    /// A loop check's checked / blind time (ms): no fault.
    Meter,
}

impl Severity {
    /// The stored text.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Loss => "loss",
            Self::Risk => "risk",
            Self::Info => "info",
            Self::Meter => "meter",
        }
    }
}

/// The row kinds (`audio_faults.kind`), one vocabulary for the ledger, the
/// API and the gate.
pub mod kinds {
    /// A path seen for the first time, or rebuilt (severity info): its
    /// counters' baseline.
    pub const START: &str = "start";
    /// A path rebuilt while it counted (an ASIO host restarted, an entry or
    /// a pipeline was rebuilt): its audio broke off there (a loss, 1).
    pub const INTERRUPTED: &str = "interrupted";
    /// Program: a boundary of a real source filled with standby (or a
    /// fade's missing side): `health.filled_owned`.
    pub const FILLED: &str = "filled";
    /// Program / ASIO output / NDI input / pipeline: a resync.
    pub const RESYNC: &str = "resync";
    /// Program: an owned frame that came after its boundary went out.
    pub const LATE_DROPPED: &str = "late_dropped";
    /// Program: a boundary the sender's full queue dropped.
    pub const COALESCED: &str = "coalesced";
    /// Program: a block handed to VBAN after its first packet was due.
    pub const VBAN_FEED_LATE: &str = "vban_feed_late";
    /// Program: frames the limiter engaged on outside a fade.
    pub const LIMITED: &str = "limited";
    /// Program: `SP-program` boundaries taken over a slot late (the trace).
    pub const NDI_LATE: &str = "ndi_late";
    /// Output: blocks dropped (a full queue, an ASIO host that is down).
    pub const BLOCKS_DROPPED: &str = "blocks_dropped";
    /// VBAN output: blocks sent as silence.
    pub const BLOCKS_SUBSTITUTED: &str = "blocks_substituted";
    /// VBAN output: datagrams whose send failed.
    pub const SEND_ERROR: &str = "send_error";
    /// VBAN output: one packet sent over 10 ms late (an event).
    pub const LATE_PACKET: &str = "late_packet";
    /// ASIO output: an underrun. Pipeline: a block cut short mid-song (the
    /// decode fell behind the cushion, #257 `underruns_mid`).
    pub const UNDERRUN: &str = "underrun";
    /// Pipeline: a block cut short before a song's (a seek's, a resume's)
    /// first full block: its audio did not start on time (#257).
    pub const UNDERRUN_START: &str = "underrun_start";
    /// Pipeline: a song that ended before the decoder reached its audio's end
    /// (a decode error, a producer gone, audio past the last frame's
    /// read-ahead): the rest of its audio never played (#257).
    pub const UNDERRUN_END: &str = "underrun_end";
    /// ASIO output: a hard re-centre.
    pub const HARD_RECENTRE: &str = "hard_recentre";
    /// ASIO output: a close (a reset, a rate change, a stall).
    pub const RESET: &str = "reset";
    /// ASIO output: frames the ring had no room for. Pipeline: the times
    /// its audio grid dropped its oldest samples over the 2 s cap.
    pub const OVERFLOW: &str = "overflow";
    /// ASIO output: a buffer switch over two periods late.
    pub const LATE_CALLBACK: &str = "late_callback";
    /// ASIO output: a buffer switch under a quarter period after the one
    /// before (a catch-up burst).
    pub const FAST_CALLBACK: &str = "fast_callback";
    /// ASIO output: the driver reported a CPU overload.
    pub const OVERLOAD: &str = "overload";
    /// Dante output: blocks the device's pump never ran (#285).
    pub const PUMP_MISSED: &str = "pump_missed";
    /// Dante output: blocks the pump ran late.
    pub const PUMP_LATE: &str = "pump_late";
    /// Dante output: blocks that took the pump longer than a block.
    pub const PUMP_OVERRUN: &str = "pump_overrun";
    /// ASIO output: its host process went down (a state).
    pub const HOST_DOWN: &str = "host_down";
    /// Loop check: a returned block unlike the one sent (an event).
    pub const MISMATCH: &str = "mismatch";
    /// Loop check: a returned block missing (an event).
    pub const DROPOUT: &str = "dropout";
    /// Loop check: the round trip moved (an event).
    pub const SLIP: &str = "slip";
    /// Loop check: nothing came back (a state).
    pub const NO_RETURN: &str = "no_return";
    /// Loop check: ms judged (a meter).
    pub const CHECKED: &str = "checked";
    /// Loop check: ms too quiet to judge (a meter).
    pub const BLIND: &str = "blind";
    /// Pipeline: grid slots nobody serviced between two scopes.
    pub const UNSERVICED: &str = "unserviced";
    /// NDI input: a boundary offered as standby (silence) with no source:
    /// no receiver, disconnected, no video yet.
    pub const NO_SOURCE: &str = "no_source";
    /// NDI input: a boundary offered as standby (silence), the source's
    /// format unsupported.
    pub const UNSUPPORTED: &str = "unsupported";
}

/// Whether one row of `kind` is ONE occurrence (an event or a state: its
/// amount is a magnitude, or 1), not a counter's step (its amount is the
/// count).
pub fn row_counted(kind: &str) -> bool {
    matches!(
        kind,
        kinds::LATE_PACKET
            | kinds::HOST_DOWN
            | kinds::MISMATCH
            | kinds::DROPOUT
            | kinds::SLIP
            | kinds::NO_RETURN
    )
}

/// How many faults `rows` rows of `kind` holding `amount` in total are.
pub fn occurrences(kind: &str, rows: i64, amount: i64) -> i64 {
    if row_counted(kind) { rows } else { amount }
}

/// One `audio_faults` row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FaultRow {
    /// UTC, ms: the tick that saw it, or an event's own time.
    pub ts_ms: i64,
    pub path: String,
    pub kind: &'static str,
    pub severity: Severity,
    /// A counter's step, an event's magnitude (frames, µs), 1 for a state or
    /// a start, ms for a meter.
    pub amount: i64,
    /// The path's name as shown (an output's name, a pipeline's NDI name).
    pub detail: String,
}

/// A counter that only grows while its path runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Counter {
    pub kind: &'static str,
    pub severity: Severity,
    pub value: u64,
}

impl Counter {
    pub fn loss(kind: &'static str, value: u64) -> Self {
        Self {
            kind,
            severity: Severity::Loss,
            value,
        }
    }

    pub fn risk(kind: &'static str, value: u64) -> Self {
        Self {
            kind,
            severity: Severity::Risk,
            value,
        }
    }
}

/// An event with its own UTC time, kept in a short list by its path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event {
    pub kind: &'static str,
    pub severity: Severity,
    pub utc_ms: i64,
    pub amount: i64,
}

/// A state that is a fault while it holds: one row when it starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StateFlag {
    pub kind: &'static str,
    pub severity: Severity,
    pub active: bool,
}

/// A time that only grows while its path runs (ms).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Meter {
    pub kind: &'static str,
    pub value_ms: u64,
}

/// One audio path as a tick reads it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Probe {
    /// `program`, `input`, `pipeline:<playlist id>`, `out:<output id>`,
    /// `loop:<output id>`.
    pub path: String,
    pub detail: String,
    /// Grows while the path runs: lower than at the last tick = rebuilt.
    pub run: u64,
    /// Its faults count now (a pipeline or the NDI input on program, an
    /// output running). A tick records a step only when the path was gated
    /// in at the tick before (`ledger.rs`).
    pub gated_in: bool,
    pub counters: Vec<Counter>,
    pub events: Vec<Event>,
    pub states: Vec<StateFlag>,
    pub meters: Vec<Meter>,
}
