//! #279 P1b: `GET /api/v1/program` and the cut answer carry `ndi`, NDI
//! `SP-program`'s source and its delay line (`playback::ndi_feed`), next to
//! the other blocks. Through the real axum router.
//! Wired via `#[cfg(test)] #[path = "program_tests_ndi.rs"] mod tests_ndi;`.

use axum::http::StatusCode;
use sp_core::config::NdiSource;

use super::tests::{add_playlist, call};
use crate::api::routes::tests::test_state;
use crate::playback::frame_buf::SharedFrame;

#[tokio::test]
async fn get_program_reports_the_ndi_block() {
    let state = test_state().await;
    let (status, json) = call(state.clone(), "GET", "/api/v1/program", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        json["ndi"],
        serde_json::json!({
            "setting": "cpu",
            "source": "cpu",
            "delay_slots": 0,
            "fourcc": "NV12",
            "repeats": 0,
            "readback_late": 0,
            "feed_coalesced": 0,
            "switches": 0,
            "readback_us_p99": 0,
            "uyvy_us_p99": 0,
        }),
        "the default: the CPU canvas, nothing read back"
    );

    // The GPU readback, as the threads report it: a switch and a repeat,
    // a readback with its pass, a picture past the depth and a late one.
    let max = state.program_bus.max().clone();
    max.set_ndi_source(NdiSource::Gpu);
    max.ndi.record_boundary(NdiSource::Gpu, true, true);
    max.ndi.record_readback(1_700, Some(450));
    for k in 1..=4 {
        max.ndi.push(k, SharedFrame::new(vec![0; 4]));
    }
    assert!(
        max.ndi.take(4).is_some(),
        "boundary 4 sent: 2 and 3 go with it"
    );
    max.ndi.push(3, SharedFrame::new(vec![0; 4]));
    let pid = add_playlist(&state.pool, "slow").await;
    let (status, json) = call(
        state,
        "POST",
        "/api/v1/program/cut",
        Some(serde_json::json!({ "source": pid })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        json["ndi"],
        serde_json::json!({
            "setting": "gpu",
            "source": "gpu",
            "delay_slots": 1,
            "fourcc": "UYVY",
            "repeats": 1,
            "readback_late": 1,
            "feed_coalesced": 1,
            "switches": 1,
            "readback_us_p99": 1700,
            "uyvy_us_p99": 450,
        }),
        "the cut answer carries it too"
    );
}
