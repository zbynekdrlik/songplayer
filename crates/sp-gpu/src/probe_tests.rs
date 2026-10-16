//! Tests for the GPU capability probe's report and rules (#279 P0).

use super::{
    DXGI_ERROR_UNSUPPORTED, DXGI_ERROR_WAS_STILL_DRAWING, E_INVALIDARG, PROBE_CLEAR,
    PROBE_CLEAR_BYTES, PROBE_NV12_HEIGHT, PROBE_NV12_WIDTH, ProbeError, ProbeReport, ProbeStep,
    S_OK, Verdict, Vram, WAIT_ABANDONED, WAIT_TIMEOUT, acquire_step, content_step, feature_step,
    hresult_detail, pixel_step, probe_picture,
};
use crate::error::GpuError;

const FAIL: ProbeError = ProbeError {
    call: "OpenSharedResource1",
    hresult: E_INVALIDARG,
    detail: "invalid argument",
};

fn all_ok() -> ProbeReport {
    ProbeReport {
        extended_resource_sharing: Ok(()),
        nv12_shared_nthandle: Ok(()),
        nv12_keyed_mutex_roundtrip: Ok(()),
        nv12_srv_r8_r8g8: Ok(()),
        uyvy_target_and_staging_do_not_wait: Ok(()),
    }
}

fn error(call: &'static str) -> ProbeStep {
    Err(ProbeError::failed(call, DXGI_ERROR_UNSUPPORTED))
}

#[test]
fn every_step_passed_is_gpu_resident_ok() {
    assert_eq!(all_ok().verdict(), Verdict::GpuResidentOk);
}

/// Each step alone failing names itself; with several, the first in the
/// report's order is named (one case per step, so no step is skipped).
#[test]
fn the_verdict_names_the_first_failed_step_in_the_report_s_order() {
    let names = [
        "extended_resource_sharing",
        "nv12_shared_nthandle",
        "nv12_keyed_mutex_roundtrip",
        "nv12_srv_r8_r8g8",
        "uyvy_target_and_staging_do_not_wait",
    ];
    for (i, &name) in names.iter().enumerate() {
        let mut report = all_ok();
        let steps = [
            &mut report.extended_resource_sharing,
            &mut report.nv12_shared_nthandle,
            &mut report.nv12_keyed_mutex_roundtrip,
            &mut report.nv12_srv_r8_r8g8,
            &mut report.uyvy_target_and_staging_do_not_wait,
        ];
        // This step fails, and every step after it too.
        for (j, step) in steps.into_iter().enumerate() {
            if j >= i {
                *step = error(names[j]);
            }
        }
        assert_eq!(
            report.verdict(),
            Verdict::NeedsFallback {
                step: name,
                error: ProbeError::failed(name, DXGI_ERROR_UNSUPPORTED),
            },
            "{name}"
        );
    }
}

#[test]
fn the_steps_are_listed_with_their_json_names_in_order() {
    let report = ProbeReport {
        nv12_srv_r8_r8g8: Err(FAIL),
        ..all_ok()
    };
    let steps = report.steps();
    let names: Vec<&str> = steps.iter().map(|(name, _)| *name).collect();
    assert_eq!(
        names,
        [
            "extended_resource_sharing",
            "nv12_shared_nthandle",
            "nv12_keyed_mutex_roundtrip",
            "nv12_srv_r8_r8g8",
            "uyvy_target_and_staging_do_not_wait",
        ]
    );
    let failed: Vec<bool> = steps.iter().map(|(_, step)| step.is_err()).collect();
    assert_eq!(failed, [false, false, false, true, false]);
    // The JSON object's keys are the same names.
    let json = serde_json::to_value(&report).unwrap();
    let mut keys: Vec<String> = json.as_object().unwrap().keys().cloned().collect();
    keys.sort_unstable();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(keys, sorted);
}

#[test]
fn a_report_serializes_each_step_as_ok_or_its_error() {
    let report = ProbeReport {
        nv12_keyed_mutex_roundtrip: Err(ProbeError::failed("AcquireSync (B, key 1)", WAIT_TIMEOUT)),
        ..all_ok()
    };
    assert_eq!(
        serde_json::to_value(&report).unwrap(),
        serde_json::json!({
            "extended_resource_sharing": {"ok": true},
            "nv12_shared_nthandle": {"ok": true},
            "nv12_keyed_mutex_roundtrip": {
                "ok": false,
                "error": {
                    "call": "AcquireSync (B, key 1)",
                    "hresult": "0x00000102",
                    "detail": "timed out",
                },
            },
            "nv12_srv_r8_r8g8": {"ok": true},
            "uyvy_target_and_staging_do_not_wait": {"ok": true},
        })
    );
}

#[test]
fn the_verdict_serializes_with_its_kind() {
    assert_eq!(
        serde_json::to_value(Verdict::GpuResidentOk).unwrap(),
        serde_json::json!({"kind": "gpu_resident_ok"})
    );
    assert_eq!(
        serde_json::to_value(Verdict::NeedsFallback {
            step: "nv12_shared_nthandle",
            error: FAIL,
        })
        .unwrap(),
        serde_json::json!({
            "kind": "needs_fallback",
            "step": "nv12_shared_nthandle",
            "error": {
                "call": "OpenSharedResource1",
                "hresult": "0x80070057",
                "detail": "invalid argument",
            },
        })
    );
}

#[test]
fn a_known_hresult_is_named_and_any_other_reads_failed() {
    assert_eq!(
        hresult_detail(DXGI_ERROR_WAS_STILL_DRAWING),
        "still drawing (DO_NOT_WAIT)"
    );
    assert_eq!(
        hresult_detail(DXGI_ERROR_UNSUPPORTED),
        "unsupported by the driver"
    );
    assert_eq!(hresult_detail(E_INVALIDARG), "invalid argument");
    assert_eq!(hresult_detail(WAIT_TIMEOUT), "timed out");
    assert_eq!(hresult_detail(WAIT_ABANDONED), "abandoned");
    assert_eq!(hresult_detail(0x8000_4005), "failed");
    // The constants are the SDK's.
    assert_eq!(
        (DXGI_ERROR_WAS_STILL_DRAWING, DXGI_ERROR_UNSUPPORTED),
        (0x887A_000A, 0x887A_0004)
    );
    assert_eq!(
        (E_INVALIDARG, WAIT_TIMEOUT, WAIT_ABANDONED, S_OK),
        (0x8007_0057, 0x102, 0x80, 0)
    );
}

#[test]
fn an_error_says_its_call_detail_and_hresult() {
    assert_eq!(
        ProbeError::failed("Map (staging, DO_NOT_WAIT)", DXGI_ERROR_WAS_STILL_DRAWING).to_string(),
        "Map (staging, DO_NOT_WAIT): still drawing (DO_NOT_WAIT) (HRESULT 0x887a000a)"
    );
    assert_eq!(
        ProbeError::refused("CheckFeatureSupport", "reports FALSE").to_string(),
        "CheckFeatureSupport: reports FALSE (HRESULT 0x00000000)"
    );
}

#[test]
fn a_crate_error_keeps_its_call_and_hresult() {
    let api = GpuError::Api {
        call: "CreateTexture2D",
        hresult: E_INVALIDARG,
    };
    assert_eq!(
        ProbeError::from_gpu(&api),
        ProbeError::failed("CreateTexture2D", E_INVALIDARG)
    );
    let lost = GpuError::DeviceLost {
        call: "GetData",
        hresult: 0x887A_0005,
    };
    assert_eq!(
        ProbeError::from_gpu(&lost),
        ProbeError {
            call: "GetData",
            hresult: 0x887A_0005,
            detail: "failed",
        }
    );
    assert_eq!(
        ProbeError::from_gpu(&GpuError::NoObject {
            call: "CreateQuery"
        }),
        ProbeError::refused("CreateQuery", "gave no object")
    );
    assert_eq!(
        ProbeError::from_gpu(&GpuError::Timeout(10_000)),
        ProbeError::refused("the GPU wait", "not done within the bound")
    );
    assert_eq!(
        ProbeError::from_gpu(&GpuError::NoAdapter),
        ProbeError::refused("the probe", "failed outside a Direct3D call")
    );
}

#[test]
fn a_feature_reported_false_is_refused() {
    assert_eq!(feature_step("CheckFeatureSupport", true), Ok(()));
    assert_eq!(
        feature_step("CheckFeatureSupport", false),
        Err(ProbeError {
            call: "CheckFeatureSupport",
            hresult: 0,
            detail: "reports FALSE",
        })
    );
}

/// Only `S_OK` holds the key: a timeout and an abandoned key are success
/// codes the probe must still call failures.
#[test]
fn only_s_ok_holds_the_key() {
    assert_eq!(acquire_step("AcquireSync (B, key 1)", S_OK), Ok(()));
    for hresult in [WAIT_TIMEOUT, WAIT_ABANDONED, E_INVALIDARG] {
        assert_eq!(
            acquire_step("AcquireSync (B, key 1)", hresult),
            Err(ProbeError::failed("AcquireSync (B, key 1)", hresult)),
            "{hresult:#x}"
        );
    }
}

#[test]
fn the_mapped_texel_must_be_the_cleared_colour() {
    assert_eq!(PROBE_CLEAR, [1.0, 0.0, 1.0, 1.0]);
    assert_eq!(
        pixel_step("Map", Some(PROBE_CLEAR_BYTES), PROBE_CLEAR_BYTES),
        Ok(())
    );
    assert_eq!(
        pixel_step("Map", Some([255, 0, 254, 255]), PROBE_CLEAR_BYTES),
        Err(ProbeError::refused(
            "Map",
            "read another colour than the clear"
        ))
    );
    assert_eq!(
        pixel_step("Map", None, PROBE_CLEAR_BYTES),
        Err(ProbeError::refused("Map", "mapped no memory"))
    );
}

#[test]
fn video_memory_is_floored_to_whole_mib() {
    let mib = 1024 * 1024;
    assert_eq!(
        Vram::from_bytes(7_000 * mib + mib - 1, 1_500 * mib + 3),
        Vram {
            budget_mb: 7_000,
            used_mb: 1_500,
        }
    );
    assert_eq!(
        serde_json::to_value(Vram::from_bytes(8 * mib, 2 * mib)).unwrap(),
        serde_json::json!({"budget_mb": 8, "used_mb": 2})
    );
}

/// A's picture: 256×144 NV12 packed (36 864 luma + 18 432 chroma bytes),
/// byte `i` = `i mod 251`.
#[test]
fn the_probe_picture_numbers_its_bytes_mod_251() {
    assert_eq!((PROBE_NV12_WIDTH, PROBE_NV12_HEIGHT), (256, 144));
    let picture = probe_picture();
    assert_eq!(picture.len(), 55_296);
    assert_eq!(
        (picture[0], picture[250], picture[251], picture[252]),
        (0, 250, 0, 1)
    );
    assert_eq!(picture[55_295], 75, "55 295 mod 251");
}

/// B must read back exactly the bytes A wrote.
#[test]
fn b_must_read_back_what_a_wrote() {
    const CALL: &str = "the shared NV12 as B read it back";
    assert_eq!(content_step(CALL, Some(probe_picture())), Ok(()));
    let mut stale = probe_picture();
    stale[40_000] ^= 1;
    assert_eq!(
        content_step(CALL, Some(stale)),
        Err(ProbeError::refused(CALL, "read other bytes than A wrote"))
    );
    assert_eq!(
        content_step(CALL, Some(vec![0; 55_296])),
        Err(ProbeError::refused(CALL, "read other bytes than A wrote")),
        "an untouched (zeroed) texture"
    );
    assert_eq!(
        content_step(CALL, None),
        Err(ProbeError::refused(CALL, "mapped no whole picture"))
    );
}
