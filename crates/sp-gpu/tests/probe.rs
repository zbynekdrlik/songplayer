//! #279 P0: the GPU capability probe (`sp_gpu::probe_gpu`, `probe_warp`,
//! `probe_on_listed_adapter`) on `windows-latest`.
//!
//! What WARP answers to the five steps is NOT documented: finding out is
//! the probe's job. So these tests assert only what this repo has already
//! proven on the runner (WARP makes BGRA devices on the Basic Render Driver,
//! `tests/warp.rs`; a device on the LISTED Basic Render Driver;
//! `pick_adapter` never takes it) and that every step comes back as a
//! definite answer: `Ok`, or an error naming its call, never a panic. The
//! answers themselves go straight to stderr, past libtest's capture
//! (`rust-workspace.md`), so the CI log of a passing run records them for
//! the epic: the main session reads WARP's verdict there.

#![cfg(windows)]

use std::io::Write;

use sp_gpu::probe::{GpuProbe, Verdict};
use sp_gpu::{GpuError, pick_adapter};

/// Write the probe's answers to stderr, past libtest's capture.
fn print(what: &str, probe: &GpuProbe) {
    let mut err = std::io::stderr().lock();
    let adapter = &probe.adapter;
    writeln!(
        err,
        "#279 P0 probe on {what}: adapter {:?} (vendor {:#06x}, device {:#06x}, \
         dedicated {} MiB, software {})",
        adapter.name,
        adapter.vendor_id,
        adapter.device_id,
        adapter.dedicated_video_memory / (1024 * 1024),
        adapter.is_software()
    )
    .unwrap();
    for (name, step) in probe.report.steps() {
        let answer = match step {
            Ok(()) => "ok".to_string(),
            Err(error) => format!("FAILED: {error}"),
        };
        writeln!(err, "  {name}: {answer}").unwrap();
    }
    let vram = match &probe.vram {
        Ok(vram) => format!("budget {} MiB, used {} MiB", vram.budget_mb, vram.used_mb),
        Err(error) => format!("FAILED: {error}"),
    };
    writeln!(err, "  vram: {vram}").unwrap();
    writeln!(
        err,
        "  verdict: {}",
        serde_json::to_string(&probe.report.verdict()).unwrap()
    )
    .unwrap();
    writeln!(
        err,
        "  report: {}",
        serde_json::to_string(&probe.report).unwrap()
    )
    .unwrap();
}

/// Every step is a definite answer, and the verdict is the one the steps
/// make: `gpu_resident_ok` when every step passed, else the first failed
/// one by name.
fn assert_definite(what: &str, probe: &GpuProbe) {
    let mut first_failed: Option<&'static str> = None;
    for (name, step) in probe.report.steps() {
        if let Err(error) = step {
            assert!(!error.call.is_empty(), "{what}: {name} names no call");
            assert!(!error.detail.is_empty(), "{what}: {name} says nothing");
            if first_failed.is_none() {
                first_failed = Some(name);
            }
        }
    }
    match (probe.report.verdict(), first_failed) {
        (Verdict::GpuResidentOk, None) => {}
        (Verdict::NeedsFallback { step, .. }, Some(name)) => {
            assert_eq!(step, name, "{what}: the first failed step");
        }
        (verdict, first) => panic!("{what}: verdict {verdict:?} against {first:?}"),
    }
    if let Err(error) = &probe.vram {
        assert!(!error.call.is_empty(), "{what}: vram names no call");
    }
}

/// The index of the Basic Render Driver in DXGI's list (always listed).
fn basic_render_driver(adapters: &[sp_gpu::AdapterInfo]) -> usize {
    adapters
        .iter()
        .position(|a| a.vendor_id == 0x1414 && a.device_id == 0x8c)
        .expect("DXGI always lists the Basic Render Driver")
}

#[test]
fn two_warp_devices_answer_every_step() {
    let probe = sp_gpu::probe_warp()
        .unwrap_or_else(|e| panic!("two WARP devices (proven by tests/warp.rs): {e}"));
    print("two WARP devices", &probe);
    assert_definite("WARP", &probe);
    // WARP's devices run on the Basic Render Driver (tests/warp.rs).
    assert!(probe.adapter.software_flag, "{:?}", probe.adapter);
    assert_eq!(
        (probe.adapter.vendor_id, probe.adapter.device_id),
        (0x1414, 0x8c)
    );
    assert_eq!(
        probe.adapters,
        sp_gpu::adapters().expect("DXGI lists its adapters"),
        "the probe lists DXGI's adapters"
    );
}

#[test]
fn the_listed_basic_render_driver_answers_every_step_the_way_probe_gpu_runs() {
    // windows-latest has no GPU, so `probe_gpu`'s device path (an explicit
    // DXGI adapter, D3D_DRIVER_TYPE_UNKNOWN) runs on the Basic Render Driver.
    // Only the path is the same: A is BGRA here, where `probe_gpu` makes it
    // with the video API (whether this driver makes that is not proven).
    let adapters = sp_gpu::adapters().expect("DXGI lists its adapters");
    let index = basic_render_driver(&adapters);
    let probe = sp_gpu::probe_on_listed_adapter(index).unwrap_or_else(|e| {
        panic!("two devices on the listed Basic Render Driver (tests/warp.rs): {e}")
    });
    print("the listed Basic Render Driver", &probe);
    assert_definite("listed adapter", &probe);
    assert_eq!(
        probe.adapter, adapters[index],
        "both devices on that adapter"
    );
}

#[test]
fn probe_gpu_takes_the_picked_adapter_or_reports_none() {
    let adapters = sp_gpu::adapters().expect("DXGI lists its adapters");
    match pick_adapter(&adapters) {
        None => {
            writeln!(
                std::io::stderr().lock(),
                "#279 P0 probe_gpu: no hardware adapter on this runner: {adapters:?}"
            )
            .unwrap();
            assert_eq!(sp_gpu::probe_gpu().err(), Some(GpuError::NoAdapter));
        }
        Some(index) => {
            let probe = sp_gpu::probe_gpu()
                .unwrap_or_else(|e| panic!("the picked adapter {:?}: {e}", adapters[index]));
            print("the picked adapter", &probe);
            assert_definite("picked adapter", &probe);
            assert_eq!(probe.adapter, adapters[index]);
        }
    }
}

#[test]
fn an_adapter_index_past_the_list_is_no_adapter() {
    let count = sp_gpu::adapters().expect("DXGI lists its adapters").len();
    assert_eq!(
        sp_gpu::probe_on_listed_adapter(count).err(),
        Some(GpuError::NoAdapter)
    );
}
