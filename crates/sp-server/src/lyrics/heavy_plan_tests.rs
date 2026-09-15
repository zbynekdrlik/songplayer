//! #162 pure tests for the CPU/GPU priority-regime decision core.
//!
//! `for_activity` picks the plan (cpu-idle on a busy wall in low-priority, gpu
//! otherwise). `apply` stamps thread caps + Windows priority-class flags but
//! NEVER a CUDA env: hiding the GPU via `CUDA_VISIBLE_DEVICES="-1"` crashed the
//! NVIDIA driver on the box (`nvdxgdmal64.dll_unloaded`, 0xc0000005 —
//! win-resolume 2026-09-15), so CPU is forced via the `--force-cpu` script arg
//! (`script_cpu_args`) instead. The tests pin both invariants.

use super::*;
use crate::lyrics::idle_gate::WallActivity;
use std::ffi::OsStr;

fn playing() -> WallActivity {
    WallActivity {
        any_playing: true,
        obs_streaming: false,
        obs_recording: false,
        known: true,
    }
}

fn idle() -> WallActivity {
    WallActivity::default()
}

// ---- ProcessingMode::from_setting ----------------------------------------

#[test]
fn mode_defaults_to_low_priority_when_unset() {
    assert_eq!(
        ProcessingMode::from_setting(None),
        ProcessingMode::LowPriority
    );
    assert_eq!(ProcessingMode::default(), ProcessingMode::LowPriority);
}

#[test]
fn mode_parses_idle_only_tokens() {
    for v in ["idle-only", "idle_only", "IDLE-ONLY", "  idle  "] {
        assert_eq!(
            ProcessingMode::from_setting(Some(v)),
            ProcessingMode::IdleOnly,
            "{v:?} should parse as idle-only"
        );
    }
}

#[test]
fn mode_low_priority_for_explicit_and_unknown() {
    for v in ["low-priority", "low_priority", "garbage", ""] {
        assert_eq!(
            ProcessingMode::from_setting(Some(v)),
            ProcessingMode::LowPriority,
            "{v:?} should parse as (or fall back to) low-priority"
        );
    }
}

// ---- for_activity — the CORE decision (RED fails the first) --------------

#[test]
fn low_priority_playing_is_cpu_idle() {
    let p = HeavyStepPlan::for_activity(ProcessingMode::LowPriority, playing());
    assert_eq!(p.device, Device::Cpu, "low-priority + wall in use → CPU");
    assert_eq!(
        p.priority,
        Priority::BelowNormal,
        "low-priority + wall in use → BELOW_NORMAL class (IDLE at creation starves the child: working set trimmed, 0.02 cores)"
    );
    assert!(p.threads.is_some(), "cpu-idle plan caps threads");
    assert!(!p.is_gpu());
    assert_eq!(p.label(), "cpu-idle");
}

#[test]
fn low_priority_idle_is_gpu_below_normal() {
    let p = HeavyStepPlan::for_activity(ProcessingMode::LowPriority, idle());
    assert_eq!(
        p.device,
        Device::Gpu,
        "low-priority + idle wall → GPU (fast)"
    );
    assert_eq!(p.priority, Priority::BelowNormal);
    assert_eq!(p.threads, None);
    assert!(p.is_gpu());
    assert_eq!(p.label(), "gpu");
}

#[test]
fn idle_only_is_always_gpu() {
    // idle-only never runs on CPU — it DEFERS on a busy wall, so whenever its
    // plan is consulted (it actually runs) the wall is idle → GPU.
    assert_eq!(
        HeavyStepPlan::for_activity(ProcessingMode::IdleOnly, playing()).device,
        Device::Gpu
    );
    assert_eq!(
        HeavyStepPlan::for_activity(ProcessingMode::IdleOnly, idle()).device,
        Device::Gpu
    );
}

// ---- heavy_step_timeout (pure, CPU ×4 scaling) ---------------------------

#[test]
fn cpu_plan_timeout_is_four_times_base() {
    // A CPU plan runs several times slower than the GPU the base was sized for,
    // so it gets CPU_TIMEOUT_MULTIPLIER × the base (#162).
    let base = std::time::Duration::from_secs(1280);
    assert_eq!(
        heavy_step_timeout(&HeavyStepPlan::cpu_idle(), base),
        std::time::Duration::from_secs(1280 * 4),
        "a cpu-idle plan scales the GPU-sized base by ×4"
    );
}

#[test]
fn gpu_plan_timeout_is_base() {
    // The GPU plan keeps the base ceiling unchanged.
    let base = std::time::Duration::from_secs(1280);
    assert_eq!(
        heavy_step_timeout(&HeavyStepPlan::gpu_below_normal(), base),
        base,
        "a GPU plan is unscaled — the base was sized for GPU speed"
    );
}

#[test]
fn cpu_plan_timeout_saturates() {
    // ×4 of a near-max base saturates at Duration::MAX instead of overflowing.
    assert_eq!(
        heavy_step_timeout(&HeavyStepPlan::cpu_idle(), std::time::Duration::MAX),
        std::time::Duration::MAX,
        "the ×4 scale saturates rather than overflowing"
    );
}

// ---- creation_flags (pure, Windows constants) ----------------------------

#[test]
fn creation_flags_cpu_idle_is_create_no_window_plus_below_normal_class() {
    // CREATE_NO_WINDOW 0x08000000 | BELOW_NORMAL_PRIORITY_CLASS 0x4000 — never
    // IDLE: a child created in IDLE class is starved by working-set trimming.
    assert_eq!(HeavyStepPlan::cpu_idle().creation_flags(), 0x0800_4000);
}

#[test]
fn creation_flags_gpu_is_create_no_window_plus_below_normal_class() {
    // CREATE_NO_WINDOW 0x08000000 | BELOW_NORMAL_PRIORITY_CLASS 0x4000
    assert_eq!(
        HeavyStepPlan::gpu_below_normal().creation_flags(),
        0x0800_4000
    );
}

// ---- thread cap (pure) ---------------------------------------------------

#[test]
fn cpu_idle_threads_is_quarter_cores_at_least_one() {
    // #162: a QUARTER of the cores (minimal load, not speed) — 3 on the 12-core box.
    assert_eq!(cpu_idle_threads_for(12), 3);
    assert_eq!(cpu_idle_threads_for(8), 2);
    assert_eq!(cpu_idle_threads_for(16), 4);
    assert_eq!(cpu_idle_threads_for(3), 1);
    assert_eq!(cpu_idle_threads_for(1), 1);
    assert_eq!(cpu_idle_threads_for(0), 1);
}

// ---- stage_regime_suffix (dashboard badge) -------------------------------

#[test]
fn stage_suffix_only_for_low_priority_while_in_use() {
    use crate::lyrics::worker::LyricsWorker;
    assert_eq!(
        LyricsWorker::stage_regime_suffix(ProcessingMode::LowPriority, playing()),
        " (cpu, wall in use)",
        "low-priority + wall in use shows the cpu regime suffix"
    );
    assert_eq!(
        LyricsWorker::stage_regime_suffix(ProcessingMode::LowPriority, idle()),
        "",
        "low-priority + idle → no suffix (GPU, fast)"
    );
    assert_eq!(
        LyricsWorker::stage_regime_suffix(ProcessingMode::IdleOnly, playing()),
        "",
        "idle-only never shows the cpu suffix — it defers with the waiting badge"
    );
}

// ---- apply() spawn-env (RED fails the cpu one) ---------------------------

fn env_of(cmd: &tokio::process::Command, key: &str) -> Option<Option<std::ffi::OsString>> {
    cmd.as_std()
        .get_envs()
        .find(|(k, _)| *k == OsStr::new(key))
        .map(|(_, v)| v.map(|s| s.to_owned()))
}

#[tokio::test]
async fn cpu_plan_never_touches_cuda_env_and_caps_threads() {
    // #162: apply() must NEVER set CUDA_VISIBLE_DEVICES. Hiding the GPU that way
    // crashed the NVIDIA driver on the box; CPU is forced via --force-cpu argv
    // (see script_cpu_args), not env. Assert NO CUDA env on BOTH plans.
    let cpu = HeavyStepPlan::for_activity(ProcessingMode::LowPriority, playing());
    let gpu = HeavyStepPlan::for_activity(ProcessingMode::LowPriority, idle());
    for plan in [cpu, gpu] {
        let mut cmd = tokio::process::Command::new("true");
        plan.apply(&mut cmd);
        assert!(
            env_of(&cmd, "CUDA_VISIBLE_DEVICES").is_none(),
            "apply() must never set CUDA_VISIBLE_DEVICES (it crashed the driver, #162)"
        );
    }
    // The cpu-idle plan still caps threads.
    let mut cmd = tokio::process::Command::new("true");
    cpu.apply(&mut cmd);
    for k in ["OMP_NUM_THREADS", "MKL_NUM_THREADS", "TORCH_NUM_THREADS"] {
        assert!(env_of(&cmd, k).is_some(), "cpu-idle plan must cap {k}");
    }
}

#[test]
fn script_cpu_args_only_for_cpu_plan() {
    assert_eq!(
        HeavyStepPlan::cpu_idle().script_cpu_args(),
        &["--force-cpu"],
        "a CPU plan forces the script onto CPU via --force-cpu"
    );
    assert!(
        HeavyStepPlan::gpu_below_normal()
            .script_cpu_args()
            .is_empty(),
        "a GPU plan passes no CPU-force arg"
    );
    // Through the full decision path too.
    assert_eq!(
        HeavyStepPlan::for_activity(ProcessingMode::LowPriority, playing()).script_cpu_args(),
        &["--force-cpu"]
    );
    assert!(
        HeavyStepPlan::for_activity(ProcessingMode::LowPriority, idle())
            .script_cpu_args()
            .is_empty()
    );
}

#[tokio::test]
async fn gpu_plan_leaves_cuda_and_threads_unset() {
    let plan = HeavyStepPlan::for_activity(ProcessingMode::LowPriority, idle());
    let mut cmd = tokio::process::Command::new("true");
    plan.apply(&mut cmd);

    assert!(
        env_of(&cmd, "CUDA_VISIBLE_DEVICES").is_none(),
        "gpu plan must NOT hide the GPU"
    );
    assert!(
        env_of(&cmd, "OMP_NUM_THREADS").is_none(),
        "gpu plan must not cap threads"
    );
}
