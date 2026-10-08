//! #147: the program trace's once-a-minute clump summary (design record
//! 6051091817, item 4).
//!
//! Every [`TRACE_LOG_EVERY`] the task sums the boundaries the `SP-program`
//! sender wrote into the trace since the summary before
//! (`program_trace::MinuteLog`) and writes ONE INFO line, only when one of
//! them was a clump boundary (its job taken over a slot late, or its submit
//! under 10 ms after the boundary before), with the counts and the minute's
//! UTC span. So at most one line a minute, and never a line on the sender's
//! boundary path: the task only reads the trace, which never blocks the
//! sender.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast;
use tokio::time::MissedTickBehavior;
use tracing::info;

use crate::playback::program_output_timing::utc_label;
use crate::playback::program_trace::{MinuteLog, MinuteSummary, ProgramTrace};

/// How often the summary runs.
pub const TRACE_LOG_EVERY: Duration = Duration::from_secs(60);

/// Start the task (called once from `PlaybackEngine::start_program`).
#[cfg_attr(test, mutants::skip)] // orchestration glue; the task itself is tested
pub fn start(trace: Arc<ProgramTrace>, shutdown: &broadcast::Sender<()>) {
    let rx = shutdown.subscribe();
    tokio::spawn(run_trace_log(trace, rx, TRACE_LOG_EVERY, log_clump_minute));
}

/// Every `every`, the first one `every` after the start, sum the records the
/// trace got since the last summary and hand a minute that held a clump
/// boundary to `write` (production: its INFO line), until shutdown.
pub async fn run_trace_log(
    trace: Arc<ProgramTrace>,
    mut shutdown: broadcast::Receiver<()>,
    every: Duration,
    mut write: impl FnMut(&MinuteSummary),
) {
    let mut log = MinuteLog::default();
    let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            _ = shutdown.recv() => break,
            _ = tick.tick() => {
                if let Some(summary) = log.minute(&trace) {
                    write(&summary);
                }
            }
        }
    }
}

/// The minute's INFO line. Logging only.
#[cfg_attr(test, mutants::skip)]
fn log_clump_minute(s: &MinuteSummary) {
    info!(
        boundaries = s.counts.boundaries,
        late = s.counts.late,
        close = s.counts.close,
        songs = s.counts.songs,
        from_utc = %utc_label(s.first_utc_ms.saturating_mul(10_000)),
        to_utc = %utc_label(s.last_utc_ms.saturating_mul(10_000)),
        "program trace: a minute held clumped boundaries (a job taken over a slot late, or a submit under 10 ms after the boundary before) — GET /api/v1/program/trace for its rows"
    );
}

#[cfg(test)]
#[path = "program_trace_log_tests.rs"]
mod tests;
