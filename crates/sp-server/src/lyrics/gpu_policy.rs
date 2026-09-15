//! GPU-politeness plumbing (#154).
//!
//! The vocal-isolation subprocess (`scripts/lyrics_worker.py::
//! cmd_preprocess_vocals`) and the forced-alignment subprocess
//! (`eval/lyrics/aligners/lyrics_alignment_mtl/run.py`) both run on the SHARED
//! win-resolume event PC, alongside the live Media Foundation video decoder and
//! OBS/Resolume. Under load they starve the decoder of VRAM + GPU scheduling and
//! drop playback frames (#144/#147 receiver-side audit).
//!
//! The Python side (a) drops its WDDM GPU scheduling priority to BELOW_NORMAL
//! and (b) caps its per-process CUDA memory fraction. This module is the Rust
//! seam: it turns the operator-tunable `lyrics_gpu_mem_fraction` DB setting into
//! the child-process env the Python workers read.
//!
//! **Quality is never traded for headroom (owner directive 2026-09-13: "nedegraduj
//! kvalitu, rýchlosť je nepodstatná").** Only the VRAM cap and the scheduling
//! priority change — the separator's / aligner's model parameters are left
//! untouched. On a CUDA OOM under the cap the Python side re-runs the SAME model
//! on CPU (identical output, only slower).
//!
//! **Secondary to the idle gate (`idle_gate.rs`, #154).** The 2026-09-14 box
//! crash proved priority + cap alone are insufficient (cap 0.4 → CPU fallback →
//! still stutters). The PRIMARY mechanism is now the idle gate: heavy stages run
//! only while the wall is idle. This module stays as defence in depth for the
//! bounded one-stage window the gate cannot avoid (the wall going busy DURING an
//! isolation that started while idle — the running subprocess is not killed).

/// Default per-process CUDA memory fraction when `lyrics_gpu_mem_fraction` is
/// unset. 0.7 ≈ 5.6 GB on the box's 8 GB card — enough for the roformer while
/// leaving headroom for the live MF decoder.
pub const DEFAULT_GPU_MEM_FRACTION: f64 = 0.7;

/// Clamp floor — below this the roformer will not load.
pub const MIN_GPU_MEM_FRACTION: f64 = 0.2;

/// Clamp ceiling — above this no headroom is left for the decoder.
pub const MAX_GPU_MEM_FRACTION: f64 = 0.95;

/// Env var the Python GPU workers read for the VRAM cap.
pub const ENV_GPU_MEM_FRACTION: &str = "LYRICS_GPU_MEM_FRACTION";

/// Child-process env pairs derived from the (already-read) raw
/// `lyrics_gpu_mem_fraction` DB setting. Pure — no I/O.
///
/// A missing / unparseable / non-finite / out-of-range value falls back to the
/// clamped [`DEFAULT_GPU_MEM_FRACTION`]. Only the VRAM cap is carried — the
/// separator's own model parameters are never touched.
pub fn env_for_child(setting: Option<&str>) -> Vec<(String, String)> {
    let frac = setting
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|f| f.is_finite())
        .unwrap_or(DEFAULT_GPU_MEM_FRACTION)
        .clamp(MIN_GPU_MEM_FRACTION, MAX_GPU_MEM_FRACTION);
    vec![(ENV_GPU_MEM_FRACTION.to_string(), format!("{frac}"))]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Return the single VRAM-cap env value produced for `setting`.
    fn frac_of(setting: Option<&str>) -> String {
        let env = env_for_child(setting);
        assert_eq!(env.len(), 1, "exactly one env pair (the VRAM cap)");
        let (k, v) = &env[0];
        assert_eq!(k, ENV_GPU_MEM_FRACTION);
        v.clone()
    }

    #[test]
    fn default_is_0_7_when_unset() {
        assert_eq!(frac_of(None), "0.7");
    }

    #[test]
    fn honours_override() {
        assert_eq!(frac_of(Some("0.85")), "0.85");
        assert_eq!(frac_of(Some("0.5")), "0.5");
    }

    #[test]
    fn trims_whitespace() {
        assert_eq!(frac_of(Some("  0.6\n")), "0.6");
    }

    #[test]
    fn clamps_below_floor() {
        assert_eq!(frac_of(Some("0.05")), "0.2");
    }

    #[test]
    fn clamps_above_ceiling() {
        assert_eq!(frac_of(Some("1.5")), "0.95");
    }

    #[test]
    fn garbage_falls_back_to_default() {
        assert_eq!(frac_of(Some("not-a-number")), "0.7");
        assert_eq!(frac_of(Some("")), "0.7");
    }

    #[test]
    fn non_finite_falls_back_to_default() {
        assert_eq!(frac_of(Some("inf")), "0.7");
        assert_eq!(frac_of(Some("nan")), "0.7");
    }

    #[test]
    fn env_var_name_is_stable() {
        assert_eq!(ENV_GPU_MEM_FRACTION, "LYRICS_GPU_MEM_FRACTION");
    }
}

#[cfg(test)]
#[path = "gpu_policy_script_guard_tests.rs"]
mod script_guard_tests;
