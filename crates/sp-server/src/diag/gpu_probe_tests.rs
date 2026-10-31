//! #279 P0: the probe request's shapes, the answer and the run, on Linux
//! with made-up probes (the route's own tests: `api/diag_tests_probe.rs`).

use std::path::PathBuf;

use sp_gpu::probe::{
    DXGI_ERROR_UNSUPPORTED, GpuProbe, ProbeError, ProbeReport, Verdict, Vram, WAIT_TIMEOUT,
};
use sp_gpu::{AdapterInfo, GpuError};

use super::{AdapterJson, BenchAsk, ProbeAnswer, ProbeOutcome, ask, run};
use crate::diag::decode_bench::{BenchMode, DecodeBench};

const GIB: u64 = 1024 * 1024 * 1024;

fn rtx() -> AdapterInfo {
    AdapterInfo {
        name: "NVIDIA GeForce RTX 3070 Ti".into(),
        vendor_id: 0x10de,
        device_id: 0x2482,
        dedicated_video_memory: 8 * GIB,
        software_flag: false,
    }
}

fn basic_render_driver() -> AdapterInfo {
    AdapterInfo {
        name: "Microsoft Basic Render Driver".into(),
        vendor_id: 0x1414,
        device_id: 0x8c,
        dedicated_video_memory: 0,
        software_flag: true,
    }
}

fn report(roundtrip: Result<(), ProbeError>) -> ProbeReport {
    ProbeReport {
        extended_resource_sharing: Ok(()),
        nv12_shared_nthandle: Ok(()),
        nv12_keyed_mutex_roundtrip: roundtrip,
        nv12_srv_r8_r8g8: Ok(()),
        uyvy_target_and_staging_do_not_wait: Ok(()),
    }
}

fn timeout() -> ProbeError {
    ProbeError::failed("AcquireSync (B, key 1, 0 ms)", WAIT_TIMEOUT)
}

fn probe(roundtrip: Result<(), ProbeError>, vram: Result<Vram, ProbeError>) -> GpuProbe {
    GpuProbe {
        adapter: rtx(),
        adapters: vec![rtx(), basic_render_driver()],
        report: report(roundtrip),
        vram,
    }
}

#[test]
fn a_probe_asks_nothing_else_and_a_decode_run_names_its_file_and_seconds() {
    assert_eq!(ask(true, None, None, false, false), Ok(BenchAsk::Probe));
    let refused = Err("a probe takes no file, seconds, hw or resident");
    assert_eq!(ask(true, Some("x.mp4"), None, false, false), refused);
    assert_eq!(ask(true, None, Some(5), false, false), refused);
    assert_eq!(ask(true, None, None, true, false), refused);
    assert_eq!(ask(true, None, None, false, true), refused);
    let decode = |mode| -> Result<BenchAsk<'static>, &'static str> {
        Ok(BenchAsk::Decode {
            file: "x.mp4",
            seconds: 5,
            mode,
        })
    };
    assert_eq!(
        ask(false, Some("x.mp4"), Some(5), false, false),
        decode(BenchMode::Software)
    );
    assert_eq!(
        ask(false, Some("x.mp4"), Some(5), true, false),
        decode(BenchMode::Hardware)
    );
    // #279 P3a: a GPU-resident run, never together with hw.
    assert_eq!(
        ask(false, Some("x.mp4"), Some(5), false, true),
        decode(BenchMode::Resident)
    );
    let two = Err("hw and resident are two modes: send one of them");
    assert_eq!(ask(false, Some("x.mp4"), Some(5), true, true), two);
    assert_eq!(ask(false, None, None, true, true), two);
    let missing = Err("file and seconds are required (or \"probe\": true)");
    assert_eq!(ask(false, Some("x.mp4"), None, false, false), missing);
    assert_eq!(ask(false, None, Some(5), false, false), missing);
    assert_eq!(ask(false, None, None, false, false), missing);
    assert_eq!(ask(false, None, None, false, true), missing);
}

#[test]
fn an_adapter_is_named_with_its_memory_in_mib() {
    assert_eq!(
        AdapterJson::of(&rtx()),
        AdapterJson {
            name: "NVIDIA GeForce RTX 3070 Ti".into(),
            vendor_id: 0x10de,
            device_id: 0x2482,
            vram_mb: 8_192,
            software: false,
        }
    );
    assert!(AdapterJson::of(&basic_render_driver()).software);
}

#[test]
fn the_answer_carries_the_verdict_every_step_the_adapters_and_the_memory() {
    let answer = ProbeAnswer::of(probe(
        Err(timeout()),
        Ok(Vram {
            budget_mb: 7_000,
            used_mb: 1_200,
        }),
    ));
    assert_eq!(
        serde_json::to_value(&answer).unwrap(),
        serde_json::json!({
            "verdict": {
                "kind": "needs_fallback",
                "step": "nv12_keyed_mutex_roundtrip",
                "error": {
                    "call": "AcquireSync (B, key 1, 0 ms)",
                    "hresult": "0x00000102",
                    "detail": "timed out",
                },
            },
            "probe": {
                "extended_resource_sharing": {"ok": true},
                "nv12_shared_nthandle": {"ok": true},
                "nv12_keyed_mutex_roundtrip": {
                    "ok": false,
                    "error": {
                        "call": "AcquireSync (B, key 1, 0 ms)",
                        "hresult": "0x00000102",
                        "detail": "timed out",
                    },
                },
                "nv12_srv_r8_r8g8": {"ok": true},
                "uyvy_target_and_staging_do_not_wait": {"ok": true},
            },
            "adapter": {
                "name": "NVIDIA GeForce RTX 3070 Ti",
                "vendor_id": 0x10de,
                "device_id": 0x2482,
                "vram_mb": 8192,
                "software": false,
            },
            "adapters": [
                {
                    "name": "NVIDIA GeForce RTX 3070 Ti",
                    "vendor_id": 0x10de,
                    "device_id": 0x2482,
                    "vram_mb": 8192,
                    "software": false,
                },
                {
                    "name": "Microsoft Basic Render Driver",
                    "vendor_id": 0x1414,
                    "device_id": 0x8c,
                    "vram_mb": 0,
                    "software": true,
                },
            ],
            "vram": {"budget_mb": 7000, "used_mb": 1200},
            "vram_error": null,
        })
    );
}

#[test]
fn a_memory_that_could_not_be_read_says_why() {
    let error = ProbeError::failed("IDXGIAdapter3", DXGI_ERROR_UNSUPPORTED);
    let answer = ProbeAnswer::of(probe(Ok(()), Err(error)));
    assert_eq!(answer.verdict, Verdict::GpuResidentOk);
    assert_eq!((answer.vram, answer.vram_error), (None, Some(error)));
}

#[test]
fn the_log_line_names_the_verdict_the_adapter_and_each_step() {
    let failing = ProbeAnswer::of(probe(Err(timeout()), Err(timeout())));
    assert_eq!(
        failing.summary(),
        "verdict=needs_fallback at nv12_keyed_mutex_roundtrip \
         adapter=\"NVIDIA GeForce RTX 3070 Ti\" extended_resource_sharing=ok \
         nv12_shared_nthandle=ok nv12_keyed_mutex_roundtrip=[AcquireSync (B, key 1, 0 ms): \
         timed out (HRESULT 0x00000102)] nv12_srv_r8_r8g8=ok \
         uyvy_target_and_staging_do_not_wait=ok"
    );
    let passing = ProbeAnswer::of(probe(Ok(()), Err(timeout())));
    assert!(
        passing
            .summary()
            .starts_with("verdict=gpu_resident_ok adapter="),
        "{}",
        passing.summary()
    );
}

fn bench() -> DecodeBench {
    DecodeBench::new(PathBuf::from("unused"))
}

#[tokio::test]
async fn a_probe_answers_and_frees_the_bench() {
    let bench = bench();
    let slot = bench.try_start().expect("free");
    let outcome = run(slot, || Ok(probe(Ok(()), Err(timeout())))).await;
    let ProbeOutcome::Answer(answer) = outcome else {
        panic!("an answer: {outcome:?}");
    };
    assert_eq!(answer.verdict, Verdict::GpuResidentOk);
    assert_eq!(answer.adapters.len(), 2);
    assert!(bench.try_start().is_some(), "the bench is free again");
}

#[tokio::test]
async fn no_direct3d_is_unsupported_and_any_other_error_a_failure() {
    let bench = bench();
    let outcome = run(bench.try_start().unwrap(), || Err(GpuError::Unsupported)).await;
    assert!(matches!(outcome, ProbeOutcome::Unsupported), "{outcome:?}");
    let outcome = run(bench.try_start().unwrap(), || Err(GpuError::NoAdapter)).await;
    let ProbeOutcome::Failed(why) = outcome else {
        panic!("a failure: {outcome:?}");
    };
    assert_eq!(
        why,
        format!("the GPU probe failed: {}", GpuError::NoAdapter),
        "it names the error"
    );
    assert!(bench.try_start().is_some(), "the bench is free again");
}

#[tokio::test]
async fn a_panicking_probe_is_a_failure_and_frees_the_bench() {
    let bench = bench();
    let outcome = run(bench.try_start().unwrap(), || panic!("injected")).await;
    let ProbeOutcome::Failed(why) = outcome else {
        panic!("a failure: {outcome:?}");
    };
    assert!(why.starts_with("the GPU probe's thread failed"), "{why}");
    assert!(bench.try_start().is_some(), "the bench is free again");
}
