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
use std::time::Duration;
use tokio::process::Command;

/// #162: how much longer a CPU heavy step is allowed to run than the GPU-sized
/// base timeout. Vocal isolation / stem separation / mtl were all timed for GPU
/// speed (~8× realtime); on the win-resolume CPU they measure ~3× realtime with
/// 6 threads and ~5–6× realtime under the cpu-idle 3-thread cap (2026-09-15).
/// A GPU-sized ceiling therefore kills a CPU job mid-run — it is deferred with
/// backoff, retried, killed again forever. ×4 gives a 10-min file 1280 s → 5120 s
/// (≈85 min), enough for ~6× realtime plus model load. Applied by
/// [`heavy_step_timeout`].
pub(crate) const CPU_TIMEOUT_MULTIPLIER: u32 = 4;

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
    /// cpu-idle: GPU untouched (the script runs `--force-cpu`, forcing CPU
    /// in-process — see [`apply`](Self::apply) for why we no longer hide the GPU
    /// via `CUDA_VISIBLE_DEVICES`), `BELOW_NORMAL_PRIORITY_CLASS` (never IDLE — see `cpu_idle`), thread cap = a QUARTER
    /// of the logical cores (#162 — minimal load, not speed). Cannot disturb the
    /// live wall.
    pub(crate) fn cpu_idle() -> Self {
        Self {
            device: Device::Cpu,
            // BELOW_NORMAL, not IDLE: a child CREATED in IDLE_PRIORITY_CLASS gets
            // the lowest page priority and the memory manager trims its working
            // set continuously while OBS/Arena churn — measured 0.02–0.23 cores
            // with ~200k page faults/s (win-resolume 2026-09-15). The same job
            // created BELOW_NORMAL runs at full speed; the 3-thread cap keeps the
            // load minimal, and OBS (High) always wins the CPU anyway.
            priority: Priority::BelowNormal,
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

    /// CPU-force CLI args this plan appends to the heavy Python subprocess's
    /// argv: a CPU plan → `["--force-cpu"]` (the script forces CPU in-process
    /// via `_force_cpu()`, keeping CUDA initialized so the NVIDIA driver never
    /// unloads and crashes — see [`apply`](Self::apply)); a GPU plan → `[]`.
    /// This replaces the removed `CUDA_VISIBLE_DEVICES="-1"` env write. mtl's
    /// `run.py` keeps its own `--no-cuda` flag; this is for the audio-separator
    /// scripts (`preprocess-vocals` / `separate`).
    pub fn script_cpu_args(&self) -> &'static [&'static str] {
        match self.device {
            Device::Cpu => &["--force-cpu"],
            Device::Gpu => &[],
        }
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
    #[cfg_attr(test, mutants::skip)] // `|` and `^` are equivalent for disjoint bit masks; the exact flag values are asserted by creation_flags_* tests
    pub(crate) fn creation_flags(&self) -> u32 {
        // winbase.h values.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
        let prio = match self.priority {
            Priority::BelowNormal => BELOW_NORMAL_PRIORITY_CLASS,
        };
        // The priority-class bits are disjoint from CREATE_NO_WINDOW, so `|` and
        // `^` yield identical values here — the assumption behind the skipped
        // equivalent mutant, stated in code.
        debug_assert!(
            CREATE_NO_WINDOW & prio == 0,
            "priority-class bits must be disjoint from CREATE_NO_WINDOW"
        );
        CREATE_NO_WINDOW | prio
    }

    /// Stamp this plan onto a subprocess `Command`:
    /// - cap CPU threads via `OMP|MKL|TORCH_NUM_THREADS` when `threads` is set.
    /// - on Windows, set the priority-class creation flags (replacing the old
    ///   inline `BELOW_NORMAL` at every heavy spawn).
    ///
    /// **CPU is forced via the script argv (`--force-cpu`, see
    /// [`script_cpu_args`](Self::script_cpu_args)), NOT via
    /// `CUDA_VISIBLE_DEVICES`.** Hiding the GPU with `CUDA_VISIBLE_DEVICES="-1"`
    /// CRASHES the box: torch + onnxruntime probe the CUDA driver, find no
    /// devices, the NVIDIA user-mode DLL unloads, and a later stray call into it
    /// kills the process — `0xc0000005` in `nvdxgdmal64.dll_unloaded`,
    /// nondeterministically (after chunk 1/23 or immediately), every isolation /
    /// separation (probed on win-resolume 2026-09-15). The scripts' in-process
    /// `_force_cpu()` (monkeypatch `torch.cuda.is_available`/`device_count` while
    /// CUDA stays initialized) forces CPU WITHOUT unloading the driver — 6/23
    /// chunks in 3 min, GPU untouched, wall fps nominal. So `apply` sets NO CUDA
    /// env at all; the CPU plan carries `--force-cpu` in argv instead (mtl keeps
    /// `--no-cuda`).
    ///
    /// The `#[cfg(windows)]` creation-flags branch is integration-only; the env
    /// branch is unit-tested via `cmd.as_std().get_envs()`.
    pub(crate) fn apply(&self, cmd: &mut Command) {
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

/// #162: the subprocess timeout for one heavy step under `plan`, from the
/// GPU-sized `base` ceiling. A GPU plan keeps `base`; a CPU plan (cpu-idle,
/// forced onto CPU while the wall plays) gets `base * CPU_TIMEOUT_MULTIPLIER`,
/// saturating at [`Duration::MAX`], because the CPU runs the step several times
/// slower than the GPU the base was sized for. Pure — unit-tested; every heavy
/// spawn site chooses its timeout through this.
pub(crate) fn heavy_step_timeout(plan: &HeavyStepPlan, base: Duration) -> Duration {
    if plan.is_gpu() {
        base
    } else {
        base.saturating_mul(CPU_TIMEOUT_MULTIPLIER)
    }
}

// ---------------------------------------------------------------------------
// #171 — STALL-based timeout for the RESUMABLE heavy audio steps.
//
// `heavy_step_timeout` above is a whole-song wall-clock ceiling (still used by
// mtl). It CANNOT bound the resumable isolation / stem-separation steps: on this
// CPU those run at ~8-14x realtime, so a legitimate 3-10 min song runs 40-77
// min and the whole-song ceiling kills it mid-run and discards the work (#171).
// Instead the scripts now write one segment WAV at a time into a work dir, and
// the Rust side kills the child only when NO new segment has appeared for
// `stall_timeout` — so a slow-but-progressing run is never killed and a hung one
// still dies. The whole-song figure survives only as an ETA in the log line.
// ---------------------------------------------------------------------------

/// Max gap between per-chunk progress writes before a resumable heavy child is
/// killed, on the CPU plan (a single ~30 s segment takes several minutes under
/// the 3-thread cap, so 15 min leaves generous headroom incl. model load).
pub(crate) const STALL_TIMEOUT_CPU_SECS: u64 = 900;

/// The GPU-plan stall window — the GPU is much faster, so a shorter window still
/// catches a genuine hang.
pub(crate) const STALL_TIMEOUT_GPU_SECS: u64 = 300;

/// Extra idle allowance before the FIRST per-chunk write of a run, covering the
/// one-time model load (two RoFormer checkpoints) + the first segment's
/// inference — on BOTH a fresh run and a resume (the models reload before the
/// next NEW segment). Without it a slow-but-healthy cold start (esp. a cold GPU
/// checkpoint under the tighter GPU window) would be mistaken for a stall and
/// killed, resumed, killed again — the exact kill-loop this ticket fixes. After
/// the first write of the run the plain [`stall_timeout`] applies.
pub(crate) const STALL_STARTUP_GRACE_SECS: u64 = 300;

/// The stall window (max gap between chunk-progress writes) for `plan`. Pure —
/// unit-tested.
pub(crate) fn stall_timeout(plan: &HeavyStepPlan) -> Duration {
    if plan.is_gpu() {
        Duration::from_secs(STALL_TIMEOUT_GPU_SECS)
    } else {
        Duration::from_secs(STALL_TIMEOUT_CPU_SECS)
    }
}

/// The idle limit before a resumable child is killed: [`stall_timeout`] once the
/// child has written at least one segment THIS run, plus
/// [`STALL_STARTUP_GRACE_SECS`] while it has not (model load + first segment).
/// Bounds BOTH a hang during model load AND a genuine mid-run stall, without
/// killing a slow-but-healthy cold start. Pure — unit-tested.
pub(crate) fn stall_limit(plan: &HeavyStepPlan, first_progress_seen: bool) -> Duration {
    let base = stall_timeout(plan);
    if first_progress_seen {
        base
    } else {
        base + Duration::from_secs(STALL_STARTUP_GRACE_SECS)
    }
}

/// True when `idle` (time since the last per-chunk progress write, or since the
/// child started when none has been written yet) has exceeded the plan's
/// [`stall_limit`] — the caller kills the resumable child, leaving its work dir
/// intact for the next resume. Pure — unit-tested.
pub(crate) fn stall_timeout_expired(
    idle: Duration,
    plan: &HeavyStepPlan,
    first_progress_seen: bool,
) -> bool {
    idle > stall_limit(plan, first_progress_seen)
}

/// Newest mtime among files directly in `dir` (the resumable segment files), or
/// `UNIX_EPOCH` when the dir is absent/empty/unreadable. I/O — `mutants::skip`.
#[cfg_attr(test, mutants::skip)]
fn newest_mtime(dir: &std::path::Path) -> std::time::SystemTime {
    let mut newest = std::time::SystemTime::UNIX_EPOCH;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if let Ok(m) = entry.metadata().and_then(|meta| meta.modified())
                && m > newest
            {
                newest = m;
            }
        }
    }
    newest
}

/// #171: wait for a resumable heavy child (`preprocess-vocals` / `separate`),
/// killing it ONLY when its `work_dir` has gone [`stall_timeout`]-stale — no new
/// segment file written for that long. Each segment write bumps the newest
/// mtime and resets the clock, so a slow-but-progressing CPU run is never
/// killed; a hung child still dies, and its work dir is LEFT INTACT so the next
/// pick resumes. `eta` is the whole-song estimate, logged only. Returns the exit
/// status on a normal exit, or `Err` on a stall. I/O orchestration —
/// `mutants::skip`; the decision core [`stall_timeout_expired`] is unit-tested.
#[cfg_attr(test, mutants::skip)]
pub(crate) async fn wait_with_stall_timeout(
    child: &mut tokio::process::Child,
    work_dir: &std::path::Path,
    plan: &HeavyStepPlan,
    label: &str,
    eta: Duration,
) -> anyhow::Result<std::process::ExitStatus> {
    let poll = Duration::from_secs(30);
    let mut last_progress = std::time::Instant::now();
    // Baseline the newest mtime so a RESUME (work dir already has segments) still
    // waits for a NEW segment this run — the models reload first either way, so
    // the startup grace applies until the first NEW write, not just on a fresh run.
    let mut last_newest = newest_mtime(work_dir);
    let mut first_progress_seen = false;
    tracing::debug!(
        "{label}: stall-bounded wait (stall={}s +{}s startup grace, eta~{}s) watching {}",
        stall_timeout(plan).as_secs(),
        STALL_STARTUP_GRACE_SECS,
        eta.as_secs(),
        work_dir.display()
    );
    loop {
        // A per-poll timeout, NOT `select!` with `child.wait()` in one arm and
        // `child.kill()` in another — that would need two simultaneous `&mut
        // child` borrows. On timeout the (cancel-safe) wait future is dropped,
        // freeing the borrow so we can kill.
        match tokio::time::timeout(poll, child.wait()).await {
            Ok(res) => return res.map_err(|e| anyhow::anyhow!("{label} wait failed: {e}")),
            Err(_) => {
                let newest = newest_mtime(work_dir);
                if newest > last_newest {
                    last_newest = newest;
                    last_progress = std::time::Instant::now();
                    first_progress_seen = true;
                }
                if stall_timeout_expired(last_progress.elapsed(), plan, first_progress_seen) {
                    let _ = child.kill().await;
                    return Err(anyhow::anyhow!(
                        "{label} stalled — no chunk progress for {}s \
                         (work dir preserved for resume)",
                        stall_limit(plan, first_progress_seen).as_secs()
                    ));
                }
            }
        }
    }
}

/// CPU-idle thread cap: a quarter of the logical cores, BOUNDED by the affinity
/// block the child is confined to, at least 1. Reads the environment
/// (`available_parallelism`) AND the live published affinity block
/// (`heavy_slot::current_affinity_block_cores`, the popcount of the resolved
/// affinity mask), so it is integration-only; the pure rule it delegates to
/// (`cpu_idle_threads_for`) is unit-tested.
#[cfg_attr(test, mutants::skip)]
fn cpu_idle_threads() -> usize {
    cpu_idle_threads_for(
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1),
        crate::lyrics::heavy_slot::current_affinity_block_cores(),
    )
}

/// Pure cpu-idle thread cap (#162 — minimal load, not speed): the quarter-cores
/// rule `cores / 4` BOUNDED by the affinity `block` the child runs on (the
/// popcount of the resolved affinity mask), at least 1. On the 24-core box the
/// #168 round-5 default confines the child to a 4-logical-core block, so the 6
/// threads the quarter rule would pick are capped to 4 — 6 torch threads on 4
/// logical cores oversubscribe. A wide `block` (an explicit whole-machine mask)
/// lets the quarter rule win. Extracted so it is deterministic in tests.
fn cpu_idle_threads_for(cores: usize, block: usize) -> usize {
    (cores / 4).min(block).max(1) // the #162 quarter rule, capped to the block
}

// ---------------------------------------------------------------------------
// Worker-side regime orchestration (thin I/O seam — reads the live setting +
// wall handles, drives the isolation step under the chosen plan). Kept here so
// worker.rs stays under the 1000-line CI cap (#162).
// ---------------------------------------------------------------------------

use crate::lyrics::idle_gate_abort::WallAbort;
use crate::lyrics::worker_outcome::SongOutcome;
use std::path::PathBuf;

/// Why a heavy lyrics step (isolation / mtl) deferred WITHOUT touching the
/// per-row backoff — both map to a no-penalty `SongOutcome` in `defer_heavy`.
#[derive(Debug)]
pub(crate) enum HeavyDefer {
    /// #161: the wall went busy mid-GPU-step (idle-only mode) → `WaitingForWall`.
    WallAbort(WallAbort),
    /// #162: free RAM/commit below `HEAVY_STEP_MIN_FREE_BYTES` before the step
    /// (the WARN is already logged where the headroom was read) → `WaitingForMemory`.
    Memory,
    /// #167: the engine started less than `HEAVY_STEP_STARTUP_FLOOR` ago — no
    /// heavy step runs yet so the wall pipelines come up on a quiet box. No
    /// backoff; the song is re-picked next tick.
    StartupGrace,
}

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
    ///   `Err(HeavyDefer::WallAbort)` so the caller defers the whole song.
    ///
    /// A pre-step low-memory reading (either mode) returns
    /// `Err(HeavyDefer::Memory)` BEFORE the slot — the song defers with no
    /// backoff and re-runs when memory frees (#162).
    #[cfg_attr(test, mutants::skip)]
    pub(crate) async fn isolate_with_regime(
        &self,
        row: &crate::db::models::VideoLyricsRow,
        gpu_mem: Option<&str>,
        mode: ProcessingMode,
    ) -> Result<Option<PathBuf>, HeavyDefer> {
        // #167: no heavy step for the first 60 s after engine start — the wall
        // pipelines must come up on a quiet box. No backoff; the song is re-picked
        // next tick.
        if let Some(reg) = self.ndi_health_registry.as_ref()
            && crate::lyrics::idle_gate::startup_floor_defers(reg.since_created())
        {
            tracing::info!(
                "lyrics_worker: heavy step isolation deferred (wall unknown — startup grace)"
            );
            return Err(HeavyDefer::StartupGrace);
        }
        // #162: memory-headroom guard BEFORE the slot (owner's order). Below the
        // 4 GiB floor → defer with no backoff (`WaitingForMemory`), re-check next
        // tick; the WARN with the numbers is logged in `heavy_step_memory_ok`.
        if crate::lyrics::heavy_slot::heavy_step_memory_defers("isolation") {
            return Err(HeavyDefer::Memory);
        }
        let activity = self.wall_activity().await;
        let plan = HeavyStepPlan::for_activity(mode, activity);
        let detail = self.wall_regime_detail(activity).await;
        let timeout =
            crate::lyrics::idle_gate_abort::isolation_step_timeout(&plan, row.duration_ms);
        tracing::info!(
            "lyrics_worker: heavy step isolation mode={} timeout={}s (wall {detail})",
            plan.label(),
            timeout.as_secs()
        );
        let result = match (mode, plan.is_gpu()) {
            (ProcessingMode::LowPriority, true) => {
                match self.isolate_vocals(row, gpu_mem, &plan, true).await {
                    Ok(v) => Ok(v),
                    Err(abort) => {
                        let cpu = HeavyStepPlan::cpu_idle();
                        let cpu_timeout = crate::lyrics::idle_gate_abort::isolation_step_timeout(
                            &cpu,
                            row.duration_ms,
                        );
                        tracing::info!(
                            "lyrics_worker: heavy step isolation re-run mode=cpu-idle \
                             timeout={}s after GPU abort ({})",
                            cpu_timeout.as_secs(),
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
        };
        result.map_err(HeavyDefer::WallAbort)
    }

    /// Map a [`HeavyDefer`] to its no-penalty `SongOutcome`: a `WallAbort`
    /// surfaces the "waiting — wall in use" badge (`WaitingForWall`); a `Memory`
    /// defer clears the in-flight marker so the selector re-picks next tick
    /// (`WaitingForMemory`). Neither touches the per-row backoff.
    #[cfg_attr(test, mutants::skip)]
    pub(crate) async fn defer_heavy(&self, d: HeavyDefer) -> SongOutcome {
        match d {
            HeavyDefer::WallAbort(abort) => {
                self.enter_wall_abort(&abort.detail).await;
                SongOutcome::WaitingForWall
            }
            HeavyDefer::Memory => {
                self.clear_processing().await;
                SongOutcome::WaitingForMemory
            }
            HeavyDefer::StartupGrace => {
                // #167: no-penalty defer, re-picked next tick once the 60 s floor
                // elapses. Surface the same "waiting — wall in use" badge (the
                // reading is UNKNOWN at startup, which reads as in-use).
                self.enter_wall_wait("startup grace — wall unknown").await;
                SongOutcome::WaitingForWall
            }
        }
    }
}

#[cfg(test)]
#[path = "heavy_plan_tests.rs"]
mod tests;
