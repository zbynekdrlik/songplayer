//! #162 pure tests for the CPU/GPU priority-regime decision core.
//!
//! RED (TIER-0 pattern): `HeavyStepPlan::for_activity`'s low-priority busy
//! branch ships the WRONG plan (gpu), so `low_priority_playing_is_cpu_idle` and
//! `cpu_plan_hides_cuda_and_caps_threads` FAIL until the GREEN fix returns
//! `Self::cpu_idle()`. The other tests pin the surrounding invariants.

use super::*;
use crate::lyrics::idle_gate::WallActivity;
use std::ffi::OsStr;

fn playing() -> WallActivity {
    WallActivity {
        any_playing: true,
        obs_streaming: false,
        obs_recording: false,
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
        Priority::Idle,
        "low-priority + wall in use → IDLE class"
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

// ---- creation_flags (pure, Windows constants) ----------------------------

#[test]
fn creation_flags_cpu_idle_is_create_no_window_plus_idle_class() {
    // CREATE_NO_WINDOW 0x08000000 | IDLE_PRIORITY_CLASS 0x40
    assert_eq!(HeavyStepPlan::cpu_idle().creation_flags(), 0x0800_0040);
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
fn cpu_idle_threads_is_half_cores_at_least_one() {
    assert_eq!(cpu_idle_threads_for(8), 4);
    assert_eq!(cpu_idle_threads_for(16), 8);
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
async fn cpu_plan_hides_cuda_and_caps_threads() {
    // Derived through the full decision path so a wrong `for_activity` (RED)
    // fails here too, not only in the pure device assert above.
    let plan = HeavyStepPlan::for_activity(ProcessingMode::LowPriority, playing());
    let mut cmd = tokio::process::Command::new("true");
    plan.apply(&mut cmd);

    assert_eq!(
        env_of(&cmd, "CUDA_VISIBLE_DEVICES"),
        Some(Some(std::ffi::OsString::from("-1"))),
        "cpu-idle plan must hide the GPU with CUDA_VISIBLE_DEVICES=\"-1\" \
         (Windows drops an empty value, leaving the GPU visible)"
    );
    for k in ["OMP_NUM_THREADS", "MKL_NUM_THREADS", "TORCH_NUM_THREADS"] {
        assert!(env_of(&cmd, k).is_some(), "cpu-idle plan must cap {k}");
    }
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
