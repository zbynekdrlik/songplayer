//! #233: the outputs' settings task. Every [`OUTPUTS_SETTINGS_POLL`] it makes
//! one pass ([`tick`]): the one-time `vban_*` migration (`audio_out_migrate.rs`)
//! until it has run — a failed one is tried again on the next pass — then
//! the list (read leniently) and the network rate, applied: an entry
//! identical to a running one up to its name (and built for the same rate)
//! is KEPT — its thread, queue and frame counter run on, a new name only
//! relabels it; unless its thread could not start
//! (`RunningOutput::start_failed`): that one is rebuilt on every pass until
//! it starts — a new or changed one is built (its target resolved, its
//! thread started), a removed or changed one is discarded
//! (`OutputSink::discard`: its queue dropped, no push taken after it, its
//! thread exits after at most the block it already holds; only the
//! shutdown's `stop_all` drains). A stored value that is no list changes nothing (Review Focus
//! 3): what runs keeps running and the problem is named. A kept VBAN output
//! re-resolves its target every `VBAN_RESOLVE_EVERY`; a failed re-resolve
//! keeps the last good address. The thread starter is a parameter, so a unit
//! test on the Windows job starts no real thread.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use sp_core::audio_outputs::{OutputEntry, OutputType, effective_rate};
use sqlx::SqlitePool;
use tokio::sync::broadcast;
use tracing::{info, warn};

use crate::playback::audio_out::{AudioOutputs, OutputSink, RunningOutput};
use crate::playback::audio_out_config::{OutputsSettings, load};
use crate::playback::audio_out_migrate::{MigrationOutcome, migrate_vban_settings};
use crate::playback::vban_out::{VbanConfig, VbanOut, needs_resolve, resolve_dest};

/// How often the list is re-read (#210's VBAN settings poll).
pub const OUTPUTS_SETTINGS_POLL: Duration = Duration::from_secs(5);

/// Starts a built VBAN output's thread: [`start_vban_thread`] in production,
/// a recorder in the tests.
pub type StartThread = dyn Fn(&Arc<VbanOut>, &str) + Send + Sync;

/// What to do with one wanted entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Keep the running output at this index.
    Keep(usize),
    Build,
}

/// One step per wanted entry (settings order), and the running outputs to stop.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub steps: Vec<Step>,
    pub stop: Vec<usize>,
}

/// The rate an entry's output is built for.
pub fn build_rate(entry: &OutputEntry, network_rate: u32) -> u32 {
    match entry.kind {
        OutputType::Vban => effective_rate(entry.rate, network_rate),
    }
}

/// Whether a running output's entry is the wanted one up to its name: a new
/// name only relabels the output, its thread runs on.
pub fn same_but_name(running: &OutputEntry, wanted: &OutputEntry) -> bool {
    let relabelled = OutputEntry {
        name: wanted.name.clone(),
        ..running.clone()
    };
    relabelled == *wanted
}

/// Keep, build, stop. Ids are unique (validated), so at most one running
/// output matches a wanted entry. An output whose thread could not start is
/// never kept: it is rebuilt (retried) on every pass until it starts.
pub fn plan(running: &[RunningOutput], wanted: &[OutputEntry], network_rate: u32) -> Plan {
    let mut kept = vec![false; running.len()];
    let mut steps = Vec::with_capacity(wanted.len());
    for w in wanted {
        let rate = build_rate(w, network_rate);
        let same = (0..running.len()).find(|&i| {
            same_but_name(&running[i].entry, w)
                && running[i].built_rate == rate
                && !running[i].start_failed()
        });
        match same {
            Some(i) => {
                kept[i] = true;
                steps.push(Step::Keep(i));
            }
            None => steps.push(Step::Build),
        }
    }
    let stop = (0..running.len()).filter(|&i| !kept[i]).collect();
    Plan { steps, stop }
}

/// Apply one read of the settings (see the module doc).
pub async fn apply(
    outputs: &AudioOutputs,
    settings: OutputsSettings,
    resolved: &mut HashMap<String, Instant>,
    start: &StartThread,
) {
    if settings.not_a_list {
        outputs.set_problems(settings.problems);
        return;
    }
    let running = outputs.running();
    let plan = plan(&running, &settings.entries, settings.network_rate);
    for &i in &plan.stop {
        resolved.remove(&running[i].entry.id);
    }
    let mut next = Vec::with_capacity(settings.entries.len());
    for (entry, step) in settings.entries.iter().zip(&plan.steps) {
        let output = match *step {
            Step::Keep(i) => {
                let mut kept = running[i].clone();
                kept.entry = entry.clone();
                refresh_vban(&kept, resolved).await;
                kept
            }
            Step::Build => build(entry, settings.network_rate, resolved, start).await,
        };
        next.push(output);
    }
    outputs.replace(next);
    for &i in &plan.stop {
        if let Some(sink) = &running[i].sink {
            sink.discard();
        }
        log_stopped(&running[i].entry);
    }
    outputs.set_network_rate(settings.network_rate);
    outputs.set_problems(settings.problems);
}

/// An entry's output: disabled = listed with no sink; a VBAN entry gets its
/// output resolved and its thread started; a build error is kept on it.
async fn build(
    entry: &OutputEntry,
    network_rate: u32,
    resolved: &mut HashMap<String, Instant>,
    start: &StartThread,
) -> RunningOutput {
    let built_rate = build_rate(entry, network_rate);
    let mut output = RunningOutput {
        entry: entry.clone(),
        built_rate,
        sink: None,
        error: None,
    };
    if !entry.enabled {
        return output;
    }
    match entry.kind {
        OutputType::Vban => match (VbanOut::for_entry(entry, network_rate), entry.vban.clone()) {
            (Ok(out), Some(dest)) => {
                let out = Arc::new(out);
                out.set_config(resolve_dest(dest, true, Vec::new()).await);
                resolved.insert(entry.id.clone(), Instant::now());
                warn_unresolved(&entry.id, &out.config());
                start(&out, &entry.id);
                log_started(entry, built_rate);
                output.sink = Some(OutputSink::Vban(out));
            }
            (Err(e), _) => output.error = Some(e),
            (Ok(_), None) => output.error = Some("not a VBAN entry".into()),
        },
    }
    output
}

/// A kept VBAN output's DNS, on #210's cadence (`needs_resolve`).
async fn refresh_vban(output: &RunningOutput, resolved: &mut HashMap<String, Instant>) {
    let (Some(OutputSink::Vban(out)), Some(dest)) = (&output.sink, output.entry.vban.clone())
    else {
        return;
    };
    let since = resolved.get(&output.entry.id).map(Instant::elapsed);
    if !needs_resolve(false, true, since) {
        return;
    }
    let cfg = resolve_dest(dest, true, out.config().targets.clone()).await;
    warn_unresolved(&output.entry.id, &cfg);
    out.set_config(cfg);
    resolved.insert(output.entry.id.clone(), Instant::now());
}

/// What the task carries from one pass to the next.
#[derive(Debug, Default)]
pub struct TaskState {
    resolved: HashMap<String, Instant>,
    reported: Vec<String>,
    migrated: bool,
}

/// One pass of the task: the migration until it has run, then the list read
/// and applied. A pass that cannot read the settings changes nothing.
pub async fn tick(
    pool: &SqlitePool,
    outputs: &AudioOutputs,
    state: &mut TaskState,
    start: &StartThread,
) {
    if !state.migrated {
        match migrate_vban_settings(pool).await {
            Ok(outcome) => {
                log_migration(&outcome);
                state.migrated = true;
            }
            Err(e) => warn!(
                %e,
                "audio outputs: the vban_* migration failed — tried again on the next pass"
            ),
        }
    }
    match load(pool).await {
        Ok(settings) => {
            warn_new_problems(&settings.problems, &state.reported);
            state.reported = settings.problems.clone();
            apply(outputs, settings, &mut state.resolved, start).await;
        }
        Err(e) => warn!(%e, "audio outputs: reading the settings failed"),
    }
}

/// Windows: the output's paced thread (#210's, MMCSS "Pro Audio").
#[cfg_attr(test, mutants::skip)] // OS thread spawn; Windows-only, like #210's
pub fn start_vban_thread(out: &Arc<VbanOut>, id: &str) {
    #[cfg(windows)]
    crate::playback::vban_out::spawn_vban_thread(out.clone(), id.to_string());
    #[cfg(not(windows))]
    let _ = (out, id);
}

#[cfg_attr(test, mutants::skip)] // logging only
fn warn_unresolved(id: &str, cfg: &VbanConfig) {
    for t in cfg.targets.iter().filter(|t| t.error.is_some()) {
        warn!(
            id,
            spec = %t.spec,
            error = t.error.as_deref().unwrap_or_default(),
            kept = ?t.addr,
            "audio output: resolving a VBAN target failed"
        );
    }
}

#[cfg_attr(test, mutants::skip)] // logging only; the problems are tested through `outputs_problems`
fn warn_new_problems(problems: &[String], reported: &[String]) {
    for p in problems.iter().filter(|p| !reported.contains(p)) {
        warn!(problem = %p, "audio outputs: a stored entry is skipped");
    }
}

#[cfg_attr(test, mutants::skip)] // logging only
fn log_started(entry: &OutputEntry, rate: u32) {
    info!(
        id = %entry.id,
        name = %entry.name,
        kind = entry.kind.as_str(),
        rate,
        delay_ms = entry.delay_ms,
        "audio output: started"
    );
}

#[cfg_attr(test, mutants::skip)] // logging only
fn log_stopped(entry: &OutputEntry) {
    info!(id = %entry.id, name = %entry.name, "audio output: stopped");
}

#[cfg_attr(test, mutants::skip)] // logging only; the decision is migrate_vban_settings' (tested)
fn log_migration(outcome: &MigrationOutcome) {
    match outcome {
        MigrationOutcome::Nothing => {}
        MigrationOutcome::Migrated(m) => {
            let ids: Vec<&str> = m.entries.iter().map(|e| e.id.as_str()).collect();
            info!(
                ?ids,
                "audio outputs: #210's VBAN settings became entries (48 kHz INT24, unchanged; \
                 the vban_* keys stay for a rollback)"
            );
            for s in &m.skipped {
                warn!(spec = %s, "audio outputs: a #210 VBAN target was not migrated");
            }
        }
    }
}

/// Spawn [`run_outputs_task`] with the real thread starter.
#[cfg_attr(test, mutants::skip)] // task spawn
pub fn start_outputs(
    pool: SqlitePool,
    outputs: Arc<AudioOutputs>,
    shutdown: &broadcast::Sender<()>,
) {
    let start: Arc<StartThread> = Arc::new(start_vban_thread);
    tokio::spawn(run_outputs_task(pool, outputs, shutdown.subscribe(), start));
}

/// The task: a [`tick`] every 5 s until shutdown, then stop every output.
#[cfg_attr(test, mutants::skip)] // a timer loop around tick (tested)
pub async fn run_outputs_task(
    pool: SqlitePool,
    outputs: Arc<AudioOutputs>,
    mut shutdown: broadcast::Receiver<()>,
    start: Arc<StartThread>,
) {
    let mut state = TaskState::default();
    loop {
        tick(&pool, &outputs, &mut state, start.as_ref()).await;
        tokio::select! {
            _ = shutdown.recv() => break,
            _ = tokio::time::sleep(OUTPUTS_SETTINGS_POLL) => {}
        }
    }
    outputs.stop_all();
    info!("audio outputs: settings task stopped");
}

#[cfg(test)]
#[path = "audio_out_task_tests.rs"]
mod tests;
