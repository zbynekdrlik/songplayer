//! The transition every program cut uses, kept in step with the operator's
//! settings (#215; #221 L5, design record 5873773896 §1g).
//!
//! #221 L5 deleted the OBS follow (`program_follow.rs`: `SP-program` cut to
//! cg OBS's program scene, and cg OBS's scene transition chose the spec).
//! SongPlayer is the master switcher now, so the only source of the spec is
//! Nastavenia. This task is the half of the follow task that survives:
//!
//! - `program_transition`: `fade` / `cut` (trimmed); anything else, no
//!   value, or the retired `obs` is the default — a Fade of
//!   `program_transition_ms`, `source: fallback`
//!   (`program_transition::effective_spec`);
//! - `program_transition_ms`: `sp_core::config::program_transition_ms` (the
//!   one parse the Nastavenia form uses too);
//! - re-read every [`TRANSITION_SETTINGS_POLL`] (and at once at start), so a
//!   save applies within 5 s; the spec goes on the bus with
//!   `ProgramBus::set_transition`, and a change is logged. An unreadable
//!   store keeps the spec in force (WARN).
//!
//! Every cut (the dashboard's, the #213 remote control's) uses the spec on
//! the bus; the bus starts on a hard Cut (`SpecSource::Fallback`) until the
//! first read.

use std::sync::Arc;
use std::time::Duration;

use sp_core::config::{
    SETTING_PROGRAM_TRANSITION, SETTING_PROGRAM_TRANSITION_MS, program_transition_ms,
};
use sqlx::SqlitePool;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use crate::playback::program_bus::ProgramBus;
use crate::playback::program_transition::{TransitionMode, TransitionSpec, effective_spec};

/// How often the task re-reads the settings.
pub const TRANSITION_SETTINGS_POLL: Duration = Duration::from_secs(5);

/// The stored transition settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransitionSettings {
    /// `program_transition`; `None` = none chosen (the default fade).
    pub mode: Option<TransitionMode>,
    /// `program_transition_ms`.
    pub ms: u32,
}

impl TransitionSettings {
    /// The spec every cut uses under these settings.
    pub fn spec(&self) -> TransitionSpec {
        effective_spec(self.mode, self.ms)
    }
}

/// Read the transition settings.
pub async fn load_transition_settings(
    pool: &SqlitePool,
) -> Result<TransitionSettings, sqlx::Error> {
    use crate::db::models::get_setting;
    let mode = TransitionMode::parse(
        get_setting(pool, SETTING_PROGRAM_TRANSITION)
            .await?
            .as_deref(),
    );
    let ms = program_transition_ms(
        get_setting(pool, SETTING_PROGRAM_TRANSITION_MS)
            .await?
            .as_deref(),
    );
    Ok(TransitionSettings { mode, ms })
}

/// One read: put the stored settings' spec on `bus` and return it. An
/// unreadable store changes nothing (WARN) and returns `None`.
pub async fn apply_transition_settings(
    pool: &SqlitePool,
    bus: &ProgramBus,
) -> Option<TransitionSpec> {
    match load_transition_settings(pool).await {
        Ok(settings) => {
            let spec = settings.spec();
            let changed = bus.set_transition(spec);
            log_spec(changed, &spec);
            Some(spec)
        }
        Err(e) => {
            warn!(%e, "program transition: reading the settings failed — the transition in force stays");
            None
        }
    }
}

/// The log line of one read: INFO when the spec changed. Logging only.
#[cfg_attr(test, mutants::skip)]
fn log_spec(changed: bool, spec: &TransitionSpec) {
    if changed {
        info!(
            kind = ?spec.kind,
            duration_ms = spec.duration_ms,
            n_slots = spec.n_slots,
            source = ?spec.source,
            "program transition: every cut now uses this transition"
        );
    } else {
        debug!(kind = ?spec.kind, duration_ms = spec.duration_ms, "program transition: unchanged");
    }
}

/// Start the task (called once from `PlaybackEngine::start_program`).
#[cfg_attr(test, mutants::skip)] // orchestration glue; the task itself is tested
pub fn start_transition_settings(
    pool: SqlitePool,
    bus: Arc<ProgramBus>,
    shutdown: &broadcast::Sender<()>,
) {
    let rx = shutdown.subscribe();
    tokio::spawn(run_transition_settings_task(
        pool,
        bus,
        rx,
        TRANSITION_SETTINGS_POLL,
    ));
}

/// Keep the bus's transition spec in step with the settings: read them at
/// once, then every `poll`, until shutdown.
pub async fn run_transition_settings_task(
    pool: SqlitePool,
    bus: Arc<ProgramBus>,
    mut shutdown: broadcast::Receiver<()>,
    poll: Duration,
) {
    let mut tick = tokio::time::interval(poll);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            _ = shutdown.recv() => break,
            _ = tick.tick() => {
                apply_transition_settings(&pool, &bus).await;
            }
        }
    }
    info!("program transition: settings task stopped");
}

#[cfg(test)]
#[path = "program_transition_settings_tests.rs"]
mod tests;
