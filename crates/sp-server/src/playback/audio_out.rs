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

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use serde::Serialize;
use sp_core::audio_outputs::{DEFAULT_NETWORK_RATE, OutputEntry, OutputType};

use crate::playback::audio_out_block::ProgramBlock;
use crate::playback::vban_out::{VbanOut, VbanStatus};
use crate::playback::vban_packet::VBAN_SEND_LATENCY_100NS;
use crate::playback::vban_rate::fft_delay_frames;

pub const STATE_RUNNING: &str = "running";
pub const STATE_OPENING: &str = "opening";
pub const STATE_WAITING: &str = "waiting";
pub const STATE_DISABLED: &str = "disabled";

/// Where an output's blocks go (lane 3 adds `Asio`).
#[derive(Clone)]
pub enum OutputSink {
    Vban(Arc<VbanOut>),
}

impl OutputSink {
    /// Hand the output one block (never waits).
    pub fn push(&self, block: ProgramBlock) {
        match self {
            Self::Vban(out) => out.push(block),
        }
    }

    /// Stop the output's thread once its queue is drained.
    pub fn stop(&self) {
        match self {
            Self::Vban(out) => out.stop(),
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
    pub rate: u32,
    pub format: &'static str,
    pub channels: u32,
    pub delay_ms: u32,
    pub latency_ms: f64,
    pub blocks_sent: u64,
    pub blocks_dropped: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vban: Option<VbanStatus>,
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

/// A VBAN output's latency from the boundary, ms: the send latency, the
/// delay, the rate converter's delay.
pub fn vban_latency_ms(delay_ms: u32, rate_hz: u32) -> f64 {
    VBAN_SEND_LATENCY_100NS as f64 / 10_000.0
        + f64::from(delay_ms)
        + fft_delay_frames(rate_hz) as f64 * 1_000.0 / f64::from(rate_hz)
}

impl RunningOutput {
    /// This output's line of `outputs[]`.
    pub fn status(&self) -> OutputStatus {
        let e = &self.entry;
        match e.kind {
            OutputType::Vban => {
                let st = self
                    .sink
                    .as_ref()
                    .map(|OutputSink::Vban(out)| (out.status(), out.is_running()));
                let addressed = st
                    .as_ref()
                    .is_some_and(|(s, _)| s.targets.iter().any(|t| t.addr.is_some()));
                let resolve_error = st
                    .as_ref()
                    .and_then(|(s, _)| s.targets.first())
                    .and_then(|t| t.error.clone());
                let (state, reason) = vban_state(
                    e.enabled,
                    self.error.as_deref(),
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
                    rate: self.built_rate,
                    format: e.vban.as_ref().map_or("int24", |v| v.format.as_str()),
                    channels: 2,
                    delay_ms: e.delay_ms,
                    latency_ms: vban_latency_ms(e.delay_ms, self.built_rate),
                    blocks_sent: telemetry.as_ref().map_or(0, |s| s.blocks_sent),
                    blocks_dropped: telemetry.as_ref().map_or(0, |s| s.blocks_dropped),
                    vban: telemetry,
                }
            }
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
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

    /// `outputs[]`, in list order.
    pub fn status(&self) -> Vec<OutputStatus> {
        self.running().iter().map(RunningOutput::status).collect()
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
