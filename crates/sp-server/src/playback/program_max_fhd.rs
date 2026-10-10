//! #239: the `SP-program` Spout sender next to `SP-program-MAX`: its
//! setting, its enable rule and its telemetry. A child module of
//! `program_max.rs` (it reads the hand-off's private queue and stats).
//!
//! The FHD program goes out over Spout too, as `SP-program` (Arena lists
//! `SPOUT_SP-program`): the same native picture(s) and weight as MAX's
//! boundary, aspect-fitted into a 1920×1080 target, the picture the NDI
//! `SP-program` carries. It rides MAX's `program-max` thread, queue and
//! pacing (`program_max_worker.rs`): a second compositor (`sp-gpu`'s
//! `Compositor::with_size`) and a second sender (`SpoutSender::new_fhd`),
//! built, backed off and rebuilt on their own, sent right after MAX at the
//! same paced instant. So it needs MAX: with `program_max_enabled` off it is
//! off too ([`fhd_off_reason`] = [`FHD_OFF_MAX`]).
//!
//! The setting `program_spout_fhd_enabled` (`sp_core::config`, ON unless it
//! says `"false"`) is applied with MAX's (`apply_fhd_setting`, every
//! `MAX_SETTINGS_POLL`); the telemetry is [`FhdStatus`], served as
//! `max.fhd` on `GET /api/v1/program`.

use std::sync::MutexGuard;

use serde::Serialize;
use sp_core::config::{SETTING_PROGRAM_SPOUT_FHD_ENABLED, program_spout_fhd_enabled};
use sp_gpu::{ComposeStats, SPOUT_FHD_SENDER_NAME, SpoutSendStats};
use sqlx::SqlitePool;
use tracing::{info, warn};

use super::{MAX_NOT_RUNNING, MaxOut, MaxPhase, Window, state_label};

/// Why the FHD sender is off: `program_max_enabled` is off (the FHD sender
/// rides MAX's thread).
pub const FHD_OFF_MAX: &str = "max_off";

/// Why the FHD sender is off: its own setting `program_spout_fhd_enabled`.
pub const FHD_OFF_SETTING: &str = "setting_off";

/// Why the FHD sender does not run, or `None` when both switches let it:
/// MAX off first (it needs MAX's thread, whatever its own setting says),
/// then its own setting. The one rule the worker ([`MaxOut::fhd_wanted`])
/// and the telemetry share.
pub fn fhd_off_reason(fhd_setting: bool, max_enabled: bool) -> Option<&'static str> {
    if !max_enabled {
        Some(FHD_OFF_MAX)
    } else if !fhd_setting {
        Some(FHD_OFF_SETTING)
    } else {
        None
    }
}

/// `GET /api/v1/program` → `max.fhd` (#239).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FhdStatus {
    /// The setting `program_spout_fhd_enabled`, as last applied.
    pub enabled: bool,
    /// `running`, `off`, `unsupported` or `error: <why>` (MAX's
    /// `state_label` over the FHD sender's own phase; `off` while
    /// [`reason`](Self::reason) names a switch).
    pub state: String,
    /// Why it is off: [`FHD_OFF_MAX`] or [`FHD_OFF_SETTING`]; `None` while
    /// both switches are on.
    pub reason: Option<&'static str>,
    /// The Spout sender name (Arena: `SPOUT_<name>`).
    pub spout_name: &'static str,
    /// The size Spout's registry lists the sender at (what a receiver
    /// opens), read once after its first boundary went out; 0×0 before
    /// that, and once the sender is dropped.
    pub listed_width: u32,
    pub listed_height: u32,
    /// Boundaries composed and sent to Spout as `SP-program`.
    pub submitted: u64,
    /// Boundaries it took that did not go out (a failed build or frame, a
    /// backoff).
    pub failed: u64,
    /// `SP-program` senders refused (the name taken, not listed, not
    /// registered): each waits a backoff before a new one.
    pub sender_backoffs: u64,
    /// The p99 over the last `MAX_STAT_WINDOW` sent frames, µs: the plane
    /// uploads (onto the FHD compositor's own device: its extra cost next to
    /// MAX), the draw until the GPU finished it, Spout's `SendTexture`.
    pub upload_us_p99: u64,
    pub draw_us_p99: u64,
    pub send_us_p99: u64,
}

/// What the `program-max` thread reports about the FHD sender, under its
/// own lock (never held with the queue's or MAX's stats lock).
pub(super) struct FhdStats {
    phase: MaxPhase,
    submitted: u64,
    failed: u64,
    sender_backoffs: u64,
    upload: Window,
    draw: Window,
    send: Window,
    listed: Option<(u32, u32)>,
}

impl FhdStats {
    /// No thread yet, nothing sent.
    pub(super) fn new() -> Self {
        Self {
            phase: MaxPhase::Failed(MAX_NOT_RUNNING.to_string()),
            submitted: 0,
            failed: 0,
            sender_backoffs: 0,
            upload: Window::default(),
            draw: Window::default(),
            send: Window::default(),
            listed: None,
        }
    }
}

impl MaxOut {
    fn lock_fhd(&self) -> MutexGuard<'_, FhdStats> {
        self.fhd.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Apply `program_spout_fhd_enabled`; returns whether it changed. The
    /// thread drops the FHD sender at its next boundary once it is off.
    pub fn set_fhd_enabled(&self, on: bool) -> bool {
        let mut queue = self.lock_queue();
        let changed = queue.fhd_enabled != on;
        queue.fhd_enabled = on;
        changed
    }

    /// The setting `program_spout_fhd_enabled`, as last applied.
    pub fn fhd_enabled(&self) -> bool {
        self.lock_queue().fhd_enabled
    }

    /// Whether the thread runs the FHD sender: its setting on AND MAX on
    /// ([`fhd_off_reason`]).
    pub fn fhd_wanted(&self) -> bool {
        let queue = self.lock_queue();
        fhd_off_reason(queue.fhd_enabled, queue.enabled).is_none()
    }

    /// The thread attached (`Running`) or ended (not running): the FHD
    /// sender's phase follows MAX's, and an `Unsupported` platform keeps it.
    pub(super) fn set_fhd_thread_phase(&self, phase: MaxPhase) {
        let mut fhd = self.lock_fhd();
        if fhd.phase != MaxPhase::Unsupported {
            fhd.phase = phase;
        }
    }

    /// No Direct3D / Spout here: the FHD sender does nothing either.
    pub(super) fn set_fhd_unsupported(&self) {
        self.lock_fhd().phase = MaxPhase::Unsupported;
    }

    /// An FHD boundary went out: its costs, and `running`.
    pub fn record_fhd_sent(&self, compose: ComposeStats, send: SpoutSendStats) {
        let mut fhd = self.lock_fhd();
        fhd.submitted += 1;
        fhd.upload.push(compose.upload_us);
        fhd.draw.push(compose.draw_us);
        fhd.send.push(send.send_us);
        fhd.phase = MaxPhase::Running;
    }

    /// An FHD boundary did not go out because of `why`.
    pub fn record_fhd_failed(&self, why: &str) {
        let mut fhd = self.lock_fhd();
        fhd.failed += 1;
        fhd.phase = MaxPhase::Failed(why.to_string());
    }

    /// An FHD boundary was skipped while its sender waits out a backoff
    /// (the state keeps the failure that started it).
    pub fn record_fhd_skipped(&self) {
        self.lock_fhd().failed += 1;
    }

    /// A refused `SP-program` sender: a new one after the backoff.
    pub fn record_fhd_sender_backoff(&self) {
        self.lock_fhd().sender_backoffs += 1;
    }

    /// Where Spout's registry lists the FHD sender (`None`: not read yet,
    /// or the sender is gone).
    pub fn record_fhd_listed(&self, listed: Option<(u32, u32)>) {
        self.lock_fhd().listed = listed;
    }

    /// What [`record_fhd_listed`](Self::record_fhd_listed) last recorded.
    pub fn fhd_listed(&self) -> Option<(u32, u32)> {
        self.lock_fhd().listed
    }

    /// The `max.fhd` block for the switches `fhd_setting` and `max_enabled`
    /// (read by the caller under the queue lock). The windows are copied
    /// under the lock and sorted after it.
    pub(super) fn fhd_status(&self, fhd_setting: bool, max_enabled: bool) -> FhdStatus {
        let (phase, counts, listed, windows) = {
            let fhd = self.lock_fhd();
            (
                fhd.phase.clone(),
                [fhd.submitted, fhd.failed, fhd.sender_backoffs],
                fhd.listed,
                [fhd.upload.clone(), fhd.draw.clone(), fhd.send.clone()],
            )
        };
        let [upload, draw, send] = windows;
        let [submitted, failed, sender_backoffs] = counts;
        let reason = fhd_off_reason(fhd_setting, max_enabled);
        let (listed_width, listed_height) = listed.unwrap_or_default();
        FhdStatus {
            enabled: fhd_setting,
            state: state_label(&phase, reason.is_none()),
            reason,
            spout_name: SPOUT_FHD_SENDER_NAME,
            listed_width,
            listed_height,
            submitted,
            failed,
            sender_backoffs,
            upload_us_p99: upload.p99(),
            draw_us_p99: draw.p99(),
            send_us_p99: send.p99(),
        }
    }
}

/// Read `program_spout_fhd_enabled` (ON unless it says `"false"`).
pub async fn load_fhd_enabled(pool: &SqlitePool) -> Result<bool, sqlx::Error> {
    let raw = crate::db::models::get_setting(pool, SETTING_PROGRAM_SPOUT_FHD_ENABLED).await?;
    Ok(program_spout_fhd_enabled(raw.as_deref()))
}

/// Apply the stored `program_spout_fhd_enabled` to `max` (an unreadable one
/// changes nothing).
pub(super) async fn apply_fhd_setting(pool: &SqlitePool, max: &MaxOut) {
    match load_fhd_enabled(pool).await {
        Ok(on) => {
            if max.set_fhd_enabled(on) {
                info!(
                    enabled = on,
                    "program max: the SP-program Spout setting applied"
                );
            }
        }
        Err(e) => warn!(%e, "program max: reading the SP-program Spout setting failed"),
    }
}

#[cfg(test)]
#[path = "program_max_fhd_tests.rs"]
mod tests;
