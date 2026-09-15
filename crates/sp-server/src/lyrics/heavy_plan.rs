//! #162 — CPU/GPU **priority regime** for the heavy lyrics/stem subprocesses.
//!
//! Replaces the #154 idle-ONLY gate (which STOPPED all heavy work while the
//! wall played) with a per-heavy-step PLAN. Owner ruling (2026-09-15):
//! processing must keep running during playback, at a priority that cannot
//! disturb the wall — *stopping is not the solution*.
//!
//! Pure decision core: [`ProcessingMode`] + [`HeavyStepPlan::for_activity`]
//! decide device / priority / thread-cap from the mode and the live
//! [`WallActivity`]; the thin [`HeavyStepPlan::apply`] seam stamps the resulting
//! env (`CUDA_VISIBLE_DEVICES` / `OMP|MKL|TORCH_NUM_THREADS`) + Windows
//! priority-class creation flags onto a subprocess `Command`. The worker-side
//! regime orchestration (`impl LyricsWorker`) is added alongside the wiring.

use crate::lyrics::idle_gate::WallActivity;
use tokio::process::Command;

/// The single operator switch — replaces the removed `lyrics_gate_when_playing`
/// boolean (#162, `MIGRATION_V25`). DEFAULT [`LowPriority`](ProcessingMode::LowPriority):
/// keep processing during playback at reduced priority. [`IdleOnly`](ProcessingMode::IdleOnly)
/// is the pre-#162 behaviour, kept ONLY as an operator option — never the
/// default (the owner never approved idle-only as the default).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProcessingMode {
    #[default]
    LowPriority,
    IdleOnly,
}

impl ProcessingMode {
    /// Parse the `lyrics_processing_mode` setting. Unset / unrecognised →
    /// `LowPriority` (the default). The migration of the removed
    /// `lyrics_gate_when_playing` boolean is a no-op at READ time: BOTH old
    /// values fold to `LowPriority` (see `MIGRATION_V25`), so an absent
    /// `lyrics_processing_mode` correctly yields the low-priority default
    /// whatever the stale boolean once was.
    pub(crate) fn from_setting(raw: Option<&str>) -> Self {
        match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            Some("idle-only") | Some("idle_only") | Some("idle") => Self::IdleOnly,
            _ => Self::LowPriority,
        }
    }
}

/// Which processor a heavy step runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Device {
    Cpu,
    Gpu,
}

/// Windows scheduling priority class for a heavy step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    Idle,
    BelowNormal,
}

/// How ONE heavy subprocess (vocal isolation / mtl align / stem separation)
/// should run right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeavyStepPlan {
    pub device: Device,
    pub priority: Priority,
    /// CPU thread cap applied via `OMP|MKL|TORCH_NUM_THREADS`. `Some(n)` in
    /// cpu-idle mode; `None` leaves the env unset (GPU path — today's
    /// behaviour, full parallelism).
    pub threads: Option<usize>,
}

impl HeavyStepPlan {
    /// cpu-idle: GPU untouched (`CUDA_VISIBLE_DEVICES=""`), `IDLE_PRIORITY_CLASS`,
    /// thread cap = half the logical cores. Cannot disturb the live wall.
    pub(crate) fn cpu_idle() -> Self {
        Self {
            device: Device::Cpu,
            priority: Priority::Idle,
            threads: Some(cpu_idle_threads()),
        }
    }

    /// gpu: `BELOW_NORMAL_PRIORITY_CLASS`, no thread cap — the pre-#162 idle
    /// behaviour, used when the wall is idle (fast) or in idle-only mode.
    pub(crate) fn gpu_below_normal() -> Self {
        Self {
            device: Device::Gpu,
            priority: Priority::BelowNormal,
            threads: None,
        }
    }

    /// The plan for running ONE heavy step under `mode` with the wall in
    /// `activity`.
    ///
    /// - `LowPriority` + wall in use → **cpu-idle** (GPU untouched, so the wall
    ///   never stutters — the whole point of #162).
    /// - everything else — `LowPriority` while the wall is idle, or `IdleOnly`
    ///   (which only ever RUNS when idle, deferring otherwise) → **gpu**.
    pub(crate) fn for_activity(mode: ProcessingMode, activity: WallActivity) -> Self {
        match mode {
            ProcessingMode::LowPriority if activity.in_use() => Self::cpu_idle(),
            _ => Self::gpu_below_normal(),
        }
    }

    pub(crate) fn is_gpu(&self) -> bool {
        matches!(self.device, Device::Gpu)
    }

    /// Short label for the per-step INFO log: `cpu-idle` | `gpu`.
    pub(crate) fn label(&self) -> &'static str {
        match self.device {
            Device::Cpu => "cpu-idle",
            Device::Gpu => "gpu",
        }
    }

    /// Windows process-creation flags for this plan: `CREATE_NO_WINDOW` always
    /// (hide the console), OR'd with the priority class. Pure — unit-tested
    /// off-Windows. Only CALLED inside the `#[cfg(windows)]` branch of `apply`
    /// (and by the pure tests), so it is dead code in the non-Windows lib build.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn creation_flags(&self) -> u32 {
        // winbase.h values.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
        const IDLE_PRIORITY_CLASS: u32 = 0x0000_0040;
        let prio = match self.priority {
            Priority::Idle => IDLE_PRIORITY_CLASS,
            Priority::BelowNormal => BELOW_NORMAL_PRIORITY_CLASS,
        };
        CREATE_NO_WINDOW | prio
    }

    /// Stamp this plan onto a subprocess `Command`:
    /// - CPU path → `CUDA_VISIBLE_DEVICES=""` so the child's torch reports no
    ///   CUDA device and builds every model on CPU — byte-identical to the
    ///   existing in-process OOM→CPU fallback (`_force_cpu` in the scripts),
    ///   only slower. (mtl's `run.py` additionally takes `--no-cuda`, which the
    ///   caller passes for a CPU plan.)
    /// - cap CPU threads via `OMP|MKL|TORCH_NUM_THREADS` when `threads` is set.
    /// - on Windows, set the priority-class creation flags (replacing the old
    ///   inline `BELOW_NORMAL` at every heavy spawn).
    ///
    /// The `#[cfg(windows)]` creation-flags branch is integration-only; the env
    /// branch is unit-tested via `cmd.as_std().get_envs()`.
    pub(crate) fn apply(&self, cmd: &mut Command) {
        if matches!(self.device, Device::Cpu) {
            cmd.env("CUDA_VISIBLE_DEVICES", "");
        }
        if let Some(n) = self.threads {
            let n = n.to_string();
            cmd.env("OMP_NUM_THREADS", &n);
            cmd.env("MKL_NUM_THREADS", &n);
            cmd.env("TORCH_NUM_THREADS", &n);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(self.creation_flags());
        }
    }
}

/// CPU-idle thread cap: half the logical cores, at least 1. Reads the
/// environment (`available_parallelism`) so it is integration-only; the pure
/// rule it delegates to (`cpu_idle_threads_for`) is unit-tested.
#[cfg_attr(test, mutants::skip)]
fn cpu_idle_threads() -> usize {
    cpu_idle_threads_for(
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1),
    )
}

/// Pure half-cores rule (extracted so it is deterministic in tests).
fn cpu_idle_threads_for(cores: usize) -> usize {
    (cores / 2).max(1)
}

// ---------------------------------------------------------------------------
// Worker-side regime orchestration (thin I/O seam — reads the live setting +
// wall handles, drives the isolation step under the chosen plan). Kept here so
// worker.rs stays under the 1000-line CI cap (#162).
// ---------------------------------------------------------------------------

use crate::lyrics::idle_gate_abort::WallAbort;
use std::path::PathBuf;

impl crate::lyrics::worker::LyricsWorker {
    /// The live `lyrics_processing_mode` operator setting (default low-priority),
    /// read each tick so a dashboard flip takes effect within one worker poll.
    #[cfg_attr(test, mutants::skip)]
    pub(crate) async fn processing_mode(&self) -> ProcessingMode {
        let raw = crate::db::models::get_setting(&self.pool, "lyrics_processing_mode")
            .await
            .ok()
            .flatten();
        ProcessingMode::from_setting(raw.as_deref())
    }

    /// Loop-level defer decision. Only `IdleOnly` mode defers on a busy wall
    /// (the pre-#162 gate + idle-settle hysteresis); `LowPriority` NEVER defers
    /// — it runs every heavy step at reduced priority instead. Returns the
    /// activity snapshot for the log/badge either way.
    #[cfg_attr(test, mutants::skip)]
    pub(crate) async fn loop_should_defer(&self, mode: ProcessingMode) -> (bool, WallActivity) {
        match mode {
            ProcessingMode::IdleOnly => self.wall_gate_should_defer().await,
            ProcessingMode::LowPriority => (false, self.wall_activity().await),
        }
    }

    /// Human detail for the per-step regime log: the playing NDI output's name
    /// when the wall is in use, else the generic reason or `idle`.
    #[cfg_attr(test, mutants::skip)]
    pub(crate) async fn wall_regime_detail(&self, activity: WallActivity) -> String {
        if !activity.in_use() {
            return "idle".to_string();
        }
        if activity.any_playing
            && let Some(reg) = &self.ndi_health_registry
            && let Some(name) = reg
                .snapshots()
                .iter()
                .find(|s| {
                    matches!(
                        s.state,
                        crate::playback::ndi_health::PlaybackStateLabel::Playing
                    )
                })
                .map(|s| s.ndi_name.clone())
        {
            return format!("{name} Playing");
        }
        activity.reason().unwrap_or("wall in use").to_string()
    }

    /// The dashboard worker-state suffix for a heavy step under this mode +
    /// activity. `LowPriority` while the wall is in use → ` (cpu, wall in use)`
    /// (surfaced through the existing stage string); otherwise empty. The
    /// idle-only "waiting — wall in use" badge is unchanged and lives on its own
    /// deferral path (`enter_wall_wait`).
    pub(crate) fn stage_regime_suffix(
        mode: ProcessingMode,
        activity: WallActivity,
    ) -> &'static str {
        if mode == ProcessingMode::LowPriority && activity.in_use() {
            " (cpu, wall in use)"
        } else {
            ""
        }
    }

    /// Run vocal isolation under the #162 priority regime.
    ///
    /// - `LowPriority` + wall idle → GPU with the abort watcher armed; if the
    ///   wall goes busy mid-isolation the GPU child is killed and isolation
    ///   re-runs IMMEDIATELY on CPU (no defer, no idle wait).
    /// - `LowPriority` + wall in use → CPU-idle, abort watcher OFF (a CPU/IDLE
    ///   job cannot disturb the wall, so it is never aborted).
    /// - `IdleOnly` → GPU with the abort watcher armed; an abort surfaces as
    ///   `WallAbort` so the caller defers the whole song (today's semantics).
    #[cfg_attr(test, mutants::skip)]
    pub(crate) async fn isolate_with_regime(
        &self,
        row: &crate::db::models::VideoLyricsRow,
        gpu_mem: Option<&str>,
        mode: ProcessingMode,
    ) -> Result<Option<PathBuf>, WallAbort> {
        let activity = self.wall_activity().await;
        let plan = HeavyStepPlan::for_activity(mode, activity);
        let detail = self.wall_regime_detail(activity).await;
        tracing::info!(
            "lyrics_worker: heavy step isolation mode={} (wall {detail})",
            plan.label()
        );
        match (mode, plan.is_gpu()) {
            (ProcessingMode::LowPriority, true) => {
                match self.isolate_vocals(row, gpu_mem, &plan, true).await {
                    Ok(v) => Ok(v),
                    Err(abort) => {
                        let cpu = HeavyStepPlan::cpu_idle();
                        tracing::info!(
                            "lyrics_worker: heavy step isolation re-run mode=cpu-idle \
                             after GPU abort ({})",
                            abort.detail
                        );
                        // abort_enabled=false → runs to completion, never Err.
                        self.isolate_vocals(row, gpu_mem, &cpu, false).await
                    }
                }
            }
            (ProcessingMode::LowPriority, false) => {
                self.isolate_vocals(row, gpu_mem, &plan, false).await
            }
            (ProcessingMode::IdleOnly, _) => self.isolate_vocals(row, gpu_mem, &plan, true).await,
        }
    }
}

#[cfg(test)]
#[path = "heavy_plan_tests.rs"]
mod tests;
