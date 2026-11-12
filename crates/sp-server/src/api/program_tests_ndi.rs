//! #279 P1b: `GET /api/v1/program` and the cut answer carry `ndi`, NDI
//! `SP-program`'s source and its delay line (`playback::ndi_feed`), next to
//! the other blocks. Through the real axum router.
//! Wired via `#[cfg(test)] #[path = "program_tests_ndi.rs"] mod tests_ndi;`.

use axum::http::StatusCode;
use sp_core::config::NdiSource;

use super::tests::{add_playlist, call};
use crate::api::routes::tests::test_state;
use crate::playback::frame_buf::SharedFrame;

/// #305 item 3: `ndi.discoverable` before the check began.
fn starting() -> serde_json::Value {
    serde_json::json!({
        "state": "starting",
        "since_ms": null,
        "seen_at_ms": null,
        "read_at_ms": null,
        "address": null,
        "on_dante": false,
        "reads": 0,
        "misses": 0,
        "expected": null,
        "reason": null,
    })
}

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
            "readback_late_events": [],
            "feed_coalesced": 0,
            "switches": 0,
            "readback_us_p99": 0,
            "uyvy_us_p99": 0,
            "discoverable": starting(),
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
        max.ndi.take_at(4, 10_000).is_some(),
        "boundary 4 sent: 2 and 3 go with it"
    );
    max.ndi.push_at(3, SharedFrame::new(vec![0; 4]), 10_250);
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
            "readback_late_events": [{ "utc_ms": 0, "behind_slots": 0, "late_us": 250 }],
            "feed_coalesced": 1,
            "switches": 1,
            "readback_us_p99": 1700,
            "uyvy_us_p99": 450,
            "discoverable": starting(),
        }),
        "the cut answer carries it too"
    );
}

/// #305 item 3: `ndi.discoverable` as the check reads it, and its
/// `degraded_reason` "SP-program not discoverable on the network" once it is
/// lost past the grace; the receiver reason keeps precedence, and a listing
/// again clears it.
#[tokio::test]
async fn get_program_reports_sp_program_s_discoverability() {
    use crate::playback::ndi_discover::At;
    use sp_ndi::FoundSource;
    use sp_ndi::test_util::MockNdiSourceFinder;
    let state = test_state().await;
    let discover = state.program_bus.max().ndi.discover.clone();
    let finder = MockNdiSourceFinder::default();
    // The route judges staleness on the real monotonic clock: the reads are
    // placed up to now (UTC values of the test's own).
    let mono = At::now().mono_ms;
    let at = |t: i64| At {
        mono_ms: mono - 41_000 + t,
        utc_ms: t,
    };
    discover.begin("RESOLUME-PP (SP-program)".into(), at(1_000));
    for t in [11_000, 21_000, 31_000] {
        discover.poll(&finder, &[], || at(t));
    }
    let (status, json) = call(state.clone(), "GET", "/api/v1/program", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["ndi"]["discoverable"]["state"], "not_discoverable");
    assert_eq!(json["ndi"]["discoverable"]["since_ms"], 31_000);
    assert_eq!(json["ndi"]["discoverable"]["misses"], 3);
    assert_eq!(
        json["degraded_reason"], "SP-program not discoverable on the network",
        "nothing on program: the source still must be findable"
    );

    // A source on program with no receiver: the receiver reason first.
    let pid = add_playlist(&state.pool, "slow").await;
    state.program_bus.select_initial(pid, None);
    state.program_bus.set_connections(0);
    let (_, json) = call(state.clone(), "GET", "/api/v1/program", None).await;
    assert_eq!(json["degraded_reason"], "no NDI receiver on SP-program");

    state.program_bus.set_connections(1);
    finder.set_sources(vec![FoundSource {
        name: "resolume-pp (SP-program)".into(),
        url: "10.77.8.201:5961".into(),
    }]);
    discover.poll(&finder, &[], || at(41_000));
    let (_, json) = call(state, "GET", "/api/v1/program", None).await;
    assert_eq!(json["degraded_reason"], serde_json::Value::Null);
    assert_eq!(
        json["ndi"]["discoverable"],
        serde_json::json!({
            "state": "discoverable",
            "since_ms": 41_000,
            "seen_at_ms": 41_000,
            "read_at_ms": 41_000,
            "address": "10.77.8.201:5961",
            "on_dante": false,
            "reads": 4,
            "misses": 0,
            "expected": "RESOLUME-PP (SP-program)",
            "reason": null,
        })
    );
}
