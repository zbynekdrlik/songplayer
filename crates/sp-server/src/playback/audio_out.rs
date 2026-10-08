//! #233: the fan-out of the program's audio to its outputs. After the peak
//! limiter, where #210's VBAN hand-off sat (`ProgramOutput::serve`, before
//! MAX and the NDI submit), each boundary's block — ONE shared copy
//! (`ProgramBlock`) — is pushed into every running output's own bounded,
//! drop-oldest queue; every output has its own thread. A slow, blocked or
//! failed output can never delay the boundary, the NDI submit, MAX or another
//! output: a push takes the output's queue lock for µs and never waits.
//!
//! The list is swapped whole by the outputs task (`audio_out_task.rs`): the
//! program thread reads one `Arc` snapshot per boundary. `status()` is
//! `GET /api/v1/program` → `outputs[]`, in list order, disabled entries too.
//! Two transports: a VBAN output (`vban_out.rs`, one paced thread per
//! destination) and an ASIO output (#233 lane 3, `asio_out.rs`, one worker
//! per driver, its status under `asio` with a `note` when the driver's rate
//! is not the network's or its buffer is over a third of a grid slot).

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use sp_core::audio_outputs::{DEFAULT_NETWORK_RATE, OutputEntry, OutputType};
use sp_core::audio_outputs_save::{
    VBAN_CONVERTER, VBAN_NOT_BUILT, VBAN_NOT_STARTED, VBAN_RESOLVING, VBAN_UNRESOLVED,
};

use crate::playback::asio_out::{AsioOut, AsioStatus};
use crate::playback::asio_state::{Reason, buffer_note, rate_note};
use crate::playback::audio_out_block::ProgramBlock;
use crate::playback::audio_out_queue::lock;
use crate::playback::vban_out::{VbanOut, VbanStatus};
use crate::playback::vban_packet::VBAN_SEND_LATENCY_100NS;
use crate::playback::vban_rate::fft_delay_frames;

pub const STATE_RUNNING: &str = "running";
pub const STATE_OPENING: &str = "opening";
pub const STATE_WAITING: &str = "waiting";
pub const STATE_DISABLED: &str = "disabled";

/// Where an output's blocks go.
#[derive(Clone)]
pub enum OutputSink {
    Vban(Arc<VbanOut>),
    Asio(Arc<AsioOut>),
}

impl OutputSink {
    /// Hand the output one block (never waits).
    pub fn push(&self, block: ProgramBlock) {
        match self {
            Self::Vban(out) => out.push(block),
            Self::Asio(out) => out.push(block),
        }
    }

    /// Stop the output's thread once its queue is drained (shutdown).
    pub fn stop(&self) {
        match self {
            Self::Vban(out) => out.stop(),
            Self::Asio(out) => out.stop(),
        }
    }

    /// Stop the output's thread now, its queue dropped (a runtime replace
    /// or removal, `audio_out_task::apply`).
    pub fn discard(&self) {
        match self {
            Self::Vban(out) => out.discard(),
            Self::Asio(out) => out.discard(),
        }
    }

    /// Why the output's thread could not start, if it could not.
    fn start_error(&self) -> Option<String> {
        match self {
            Self::Vban(out) => out.start_error(),
            Self::Asio(out) => out.start_error(),
        }
    }
}

/// One entry of the list as it runs.
#[derive(Clone)]
pub struct RunningOutput {
    pub entry: OutputEntry,
    /// The rate it was built for (`audio_out_task::build_rate`).
    pub built_rate: u32,
    /// `None`: disabled, or it could not be built (`error`).
    pub sink: Option<OutputSink>,
    pub error: Option<String>,
}

/// `GET /api/v1/program` → `outputs[i]`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct OutputStatus {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub name: String,
    pub enabled: bool,
    /// running | opening | waiting | disabled.
    pub state: &'static str,
    pub reason: Option<String>,
    /// #233 release review: a waiting VBAN output's reason as a stable code
    /// (`vban_reason_code`), which the dashboard shows in Slovak.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<&'static str>,
    pub rate: u32,
    pub format: &'static str,
    pub channels: u32,
    pub delay_ms: u32,
    pub latency_ms: f64,
    pub blocks_sent: u64,
    pub blocks_dropped: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vban: Option<VbanStatus>,
    /// #233: a running ASIO driver whose rate is not the network's, or
    /// whose buffer is over a third of a grid slot (a WARN once).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asio: Option<AsioStatus>,
}

/// A VBAN output's state: disabled, not built (the reason), its thread not
/// running yet, waiting for an address (the resolve error), or running.
pub fn vban_state(
    enabled: bool,
    build_error: Option<&str>,
    thread_running: bool,
    addressed: bool,
    resolve_error: Option<&str>,
) -> (&'static str, Option<String>) {
    if !enabled {
        return (STATE_DISABLED, None);
    }
    if let Some(e) = build_error {
        return (STATE_WAITING, Some(e.to_string()));
    }
    if !thread_running {
        return (STATE_OPENING, None);
    }
    if addressed {
        return (STATE_RUNNING, None);
    }
    let reason = resolve_error.unwrap_or("the address is not resolved yet");
    (STATE_WAITING, Some(reason.to_string()))
}

/// #233 release review: a waiting VBAN output's reason as a stable code:
/// its `cause` (`not_built`, `not_started`, `converter`), else a target that
/// does not resolve (`unresolved`) or is not resolved yet (`resolving`).
/// `None` unless the output waits.
pub fn vban_reason_code(
    state: &str,
    cause: Option<&'static str>,
    resolve_failed: bool,
) -> Option<&'static str> {
    if state != STATE_WAITING {
        return None;
    }
    Some(cause.unwrap_or(if resolve_failed {
        VBAN_UNRESOLVED
    } else {
        VBAN_RESOLVING
    }))
}

/// A VBAN output's latency from the boundary, ms: the send latency, the
/// delay, the rate converter's delay.
pub fn vban_latency_ms(delay_ms: u32, rate_hz: u32) -> f64 {
    VBAN_SEND_LATENCY_100NS as f64 / 10_000.0
        + f64::from(delay_ms)
        + fft_delay_frames(rate_hz) as f64 * 1_000.0 / f64::from(rate_hz)
}

impl RunningOutput {
    /// #233: its thread could not start (a failed UDP bind or spawn): the
    /// outputs task rebuilds it on its next pass instead of keeping it.
    pub fn start_failed(&self) -> bool {
        self.sink
            .as_ref()
            .is_some_and(|sink| sink.start_error().is_some())
    }

    /// This output's line of `outputs[]` on a network at `network_rate`.
    pub fn status(&self, network_rate: u32) -> OutputStatus {
        match (&self.sink, self.entry.kind) {
            (Some(OutputSink::Asio(out)), _) => self.asio_status(Some(out), network_rate),
            (Some(OutputSink::Vban(out)), _) => self.vban_status(Some(out)),
            (None, OutputType::Asio) => self.asio_status(None, network_rate),
            (None, OutputType::Vban) => self.vban_status(None),
        }
    }

    fn vban_status(&self, out: Option<&Arc<VbanOut>>) -> OutputStatus {
        let e = &self.entry;
        let st = out.map(|out| (out.status(), out.is_running()));
        // A build error, else why the thread could not start (#233), else
        // what the running thread cannot do (a refused rate converter), each
        // with its code (#233 release review).
        let not_started = out.and_then(|out| out.start_error());
        let fault = out.and_then(|out| out.fault());
        let (build_error, cause) = match (self.error.clone(), not_started, fault) {
            (Some(why), _, _) => (Some(why), Some(VBAN_NOT_BUILT)),
            (None, Some(why), _) => (Some(why), Some(VBAN_NOT_STARTED)),
            (None, None, Some(why)) => (Some(why), Some(VBAN_CONVERTER)),
            (None, None, None) => (None, None),
        };
        let addressed = st
            .as_ref()
            .is_some_and(|(s, _)| s.targets.iter().any(|t| t.addr.is_some()));
        let resolve_error = st
            .as_ref()
            .and_then(|(s, _)| s.targets.first())
            .and_then(|t| t.error.clone());
        let (state, reason) = vban_state(
            e.enabled,
            build_error.as_deref(),
            st.as_ref().is_some_and(|(_, running)| *running),
            addressed,
            resolve_error.as_deref(),
        );
        let telemetry = st.map(|(s, _)| s);
        OutputStatus {
            id: e.id.clone(),
            kind: e.kind.as_str(),
            name: e.name.clone(),
            enabled: e.enabled,
            state,
            reason,
            reason_code: vban_reason_code(state, cause, resolve_error.is_some()),
            rate: self.built_rate,
            format: e.vban.as_ref().map_or("int24", |v| v.format.as_str()),
            channels: 2,
            delay_ms: e.delay_ms,
            latency_ms: vban_latency_ms(e.delay_ms, self.built_rate),
            blocks_sent: telemetry.as_ref().map_or(0, |s| s.blocks_sent),
            blocks_dropped: telemetry.as_ref().map_or(0, |s| s.blocks_dropped),
            vban: telemetry,
            note: None,
            asio: None,
        }
    }

    /// #233: an ASIO output's line: the driver's rate and sample type, its
    /// state and reason from the worker (a build error or a failed spawn
    /// first), the notes while it runs.
    fn asio_status(&self, out: Option<&Arc<AsioOut>>, network_rate: u32) -> OutputStatus {
        let e = &self.entry;
        let snap = out.map(|out| out.snapshot());
        let failed = self.error.clone().or(out.and_then(|out| out.start_error()));
        let (state, reason) = match (&snap, failed) {
            _ if !e.enabled => (STATE_DISABLED, None),
            (_, Some(why)) => (STATE_WAITING, Some(why)),
            (Some(s), None) => (s.state, s.reason.as_ref().map(Reason::text)),
            (None, None) => (STATE_OPENING, None),
        };
        let status = snap.as_ref().map(|s| s.status.clone());
        let rate = status.as_ref().map_or(0, |s| s.driver_rate);
        let notes: Vec<String> = [
            rate_note(rate, network_rate),
            status
                .as_ref()
                .and_then(|s| buffer_note(s.buffer_frames, rate)),
        ]
        .into_iter()
        .flatten()
        .collect();
        let running = state == STATE_RUNNING;
        OutputStatus {
            id: e.id.clone(),
            kind: e.kind.as_str(),
            name: e.name.clone(),
            enabled: e.enabled,
            state,
            reason,
            reason_code: None,
            rate,
            format: status.as_ref().map_or("", |s| s.sample_type),
            channels: 2,
            delay_ms: e.delay_ms,
            latency_ms: status.as_ref().map_or(0.0, |s| s.latency_ms),
            blocks_sent: snap.as_ref().map_or(0, |s| s.blocks_sent),
            blocks_dropped: snap.as_ref().map_or(0, |s| s.blocks_dropped),
            vban: None,
            note: (running && !notes.is_empty()).then(|| notes.join("; ")),
            asio: status,
        }
    }
}

/// The program's audio outputs (owned by `ProgramBus`).
pub struct AudioOutputs {
    list: Mutex<Arc<Vec<RunningOutput>>>,
    network_rate: AtomicU32,
    problems: Mutex<Vec<String>>,
}

impl Default for AudioOutputs {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioOutputs {
    /// No output, the default network rate, no problem.
    pub fn new() -> Self {
        Self {
            list: Mutex::new(Arc::new(Vec::new())),
            network_rate: AtomicU32::new(DEFAULT_NETWORK_RATE),
            problems: Mutex::new(Vec::new()),
        }
    }

    /// Hand `block` to every running output (an `Arc` bump each; never waits).
    pub fn offer(&self, block: &ProgramBlock) {
        let list = self.running();
        for sink in list.iter().filter_map(|o| o.sink.as_ref()) {
            sink.push(block.clone());
        }
    }

    /// The list as it runs now (a snapshot).
    pub fn running(&self) -> Arc<Vec<RunningOutput>> {
        lock(&self.list).clone()
    }

    /// Swap the whole list (the outputs task).
    pub fn replace(&self, list: Vec<RunningOutput>) {
        *lock(&self.list) = Arc::new(list);
    }

    /// Stop every output's thread (process shutdown).
    pub fn stop_all(&self) {
        for sink in self.running().iter().filter_map(|o| o.sink.as_ref()) {
            sink.stop();
        }
    }

    /// The network rate the list was last applied with.
    pub fn network_rate(&self) -> u32 {
        self.network_rate.load(Ordering::Relaxed)
    }

    pub fn set_network_rate(&self, rate: u32) {
        self.network_rate.store(rate, Ordering::Relaxed);
    }

    /// The stored entries this version could not run (named).
    pub fn problems(&self) -> Vec<String> {
        lock(&self.problems).clone()
    }

    pub fn set_problems(&self, problems: Vec<String>) {
        *lock(&self.problems) = problems;
    }

    /// `outputs[]`, in list order, on the network rate the list was applied
    /// with.
    pub fn status(&self) -> Vec<OutputStatus> {
        let network_rate = self.network_rate();
        self.running()
            .iter()
            .map(|o| o.status(network_rate))
            .collect()
    }

    /// Tests: one VBAN output as the whole fan-out (#210's test seam).
    #[cfg(test)]
    pub fn single_vban(out: Arc<VbanOut>) -> Self {
        let outputs = Self::new();
        outputs.replace(vec![tests::running_vban("out-1", out)]);
        outputs
    }
}

#[cfg(test)]
#[path = "audio_out_tests.rs"]
pub(crate) mod tests;
