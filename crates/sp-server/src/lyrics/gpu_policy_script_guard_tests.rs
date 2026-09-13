//! Structural guards (#154): the deployed GPU Python scripts must carry the
//! WDDM below-normal scheduling-priority call and the per-process VRAM cap, and
//! must NOT override the separator's model parameters (owner directive
//! 2026-09-13: separation/alignment QUALITY must not change — only GPU priority
//! and VRAM headroom). CRLF-normalised for the Windows CI checkout.
//!
//! These scripts are deployed to win-resolume verbatim via `include_str!`
//! (`worker.rs::ensure_script`), so guarding the committed source is guarding
//! what actually runs on the box.

const WORKER_PY: &str = include_str!("../../../../scripts/lyrics_worker.py");
const MTL_RUN_PY: &str =
    include_str!("../../../../eval/lyrics/aligners/lyrics_alignment_mtl/run.py");

fn norm(s: &str) -> String {
    s.replace("\r\n", "\n")
}

#[test]
fn worker_py_sets_wddm_below_normal_priority() {
    let src = norm(WORKER_PY);
    assert!(
        src.contains("D3DKMTSetProcessSchedulingPriorityClass"),
        "lyrics_worker.py must call the WDDM scheduling-priority API"
    );
    assert!(
        src.contains("D3DKMT_SCHEDULINGPRIORITYCLASS_BELOW_NORMAL"),
        "must name the BELOW_NORMAL (=1) priority class"
    );
}

#[test]
fn worker_py_caps_vram_fraction() {
    let src = norm(WORKER_PY);
    assert!(
        src.contains("set_per_process_memory_fraction"),
        "lyrics_worker.py must cap the per-process CUDA memory fraction"
    );
    assert!(
        src.contains("LYRICS_GPU_MEM_FRACTION"),
        "must read the VRAM cap from the LYRICS_GPU_MEM_FRACTION env var"
    );
}

#[test]
fn worker_py_has_cuda_oom_cpu_fallback() {
    let src = norm(WORKER_PY);
    assert!(
        src.contains("gpu_polite("),
        "cmd_preprocess_vocals must call gpu_polite()"
    );
    assert!(
        src.contains("_is_cuda_oom") && src.contains("_force_cpu"),
        "a CUDA OOM must retry on CPU (identical model + parameters)"
    );
    assert!(
        src.contains("force_cpu=True"),
        "the OOM retry must force the isolation onto CPU"
    );
}

#[test]
fn worker_py_does_not_override_separator_model_params() {
    let src = norm(WORKER_PY);
    // Owner directive 2026-09-13: separation quality must not change, so the
    // separator's model-footprint knobs stay at their defaults.
    assert!(
        !src.contains("mdxc_params"),
        "must NOT override mdxc_params (separator model parameters)"
    );
    assert!(
        !src.contains("segment_size"),
        "must NOT override segment_size (separator model parameter)"
    );
}

#[test]
fn mtl_run_py_sets_wddm_priority_and_vram_cap() {
    let src = norm(MTL_RUN_PY);
    assert!(
        src.contains("D3DKMTSetProcessSchedulingPriorityClass"),
        "run.py must call the WDDM scheduling-priority API when CUDA is used"
    );
    assert!(
        src.contains("set_per_process_memory_fraction"),
        "run.py must cap the per-process CUDA memory fraction"
    );
    assert!(
        src.contains("gpu_polite("),
        "run.py must call gpu_polite() before CUDA alignment"
    );
}
