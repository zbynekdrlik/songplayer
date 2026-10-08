//! #147: `GET /api/v1/program/trace` through the real axum router: the
//! window's rows in their columns, the clamp, the defaults, and a refused
//! query that never repeats what was sent.
//! Wired via `#[cfg(test)] #[path = "program_tests_trace.rs"] mod tests_trace;`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use sp_core::genlock::{GENLOCK_GRID_FPS, grid_boundary_100ns};
use tower::ServiceExt;

use crate::api::routes::tests::{app, test_state};
use crate::playback::program_output_timing::BoundaryMarks;
use crate::playback::program_trace::{JobShape, TraceKind};

const SOURCE: i64 = 7;

/// The `k`-th grid boundary after 2025-10-08 00:13:20 UTC.
fn b(k: i64) -> i64 {
    grid_boundary_100ns(1_759_882_400 * GENLOCK_GRID_FPS + k, GENLOCK_GRID_FPS)
}

/// `GET uri`: the status and the body as text.
async fn get(state: crate::AppState, uri: &str) -> (StatusCode, String) {
    let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
    let resp = app(state).oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

/// The program's sender served boundaries 0..3 on time (taken 1 ms after,
/// submitted 4 ms after), all of `SOURCE`.
fn three_boundaries(state: &crate::AppState) {
    let trace = state.program_bus.trace();
    let mut writer = trace.writer().expect("no sender runs in the test state");
    let live = JobShape {
        kind: TraceKind::Source,
        live: true,
    };
    for k in 0..3 {
        let stamp = b(k);
        let marks = BoundaryMarks {
            stamp_100ns: stamp,
            taken_100ns: stamp + 10_000,
            fed_100ns: stamp + 12_000,
            submit_start_100ns: stamp + 15_000,
            submitted_100ns: stamp + 40_000,
        };
        writer.record(&marks, Some(SOURCE), live, 0);
    }
}

#[tokio::test]
async fn the_trace_answers_its_window_as_rows_of_its_columns() {
    let state = test_state().await;
    three_boundaries(&state);
    let utc = |k: i64| (b(k) + 40_000) / 10_000;
    let uri = format!(
        "/api/v1/program/trace?from_utc_ms={}&to_utc_ms={}",
        utc(1),
        utc(2) + 1
    );
    let (status, body) = get(state, &uri).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["from_utc_ms"], utc(1));
    assert_eq!(json["to_utc_ms"], utc(2) + 1);
    assert_eq!(json["clamped"], false);
    assert_eq!(json["max_span_ms"], 120_000);
    assert_eq!(json["capacity"], 18_000);
    assert_eq!(json["held"], 3);
    assert_eq!(json["oldest_utc_ms"], utc(0));
    assert_eq!(json["newest_utc_ms"], utc(2));
    assert_eq!(
        json["columns"],
        serde_json::json!([
            "utc_ms",
            "wire_100ns",
            "source",
            "kind",
            "live",
            "taken_us",
            "fed_us",
            "submit_start_us",
            "submitted_us",
            "late",
            "close",
            "song"
        ])
    );
    let row = |k: i64| {
        serde_json::json!([
            utc(k),
            b(k),
            SOURCE,
            "src",
            1,
            1_000,
            1_200,
            1_500,
            4_000,
            0,
            0,
            null
        ])
    };
    assert_eq!(json["rows"], serde_json::json!([row(1), row(2)]));
    assert_eq!(
        json["clumps"],
        serde_json::json!({"boundaries": 2, "late": 0, "close": 0, "songs": 0})
    );
}

/// No bounds: the two minutes up to now. A window over two minutes ends two
/// minutes after its `from`, and says so.
#[tokio::test]
async fn the_trace_window_defaults_to_two_minutes_and_is_clamped_to_two() {
    let state = test_state().await;
    let (status, body) = get(state.clone(), "/api/v1/program/trace").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    let (from, to) = (json["from_utc_ms"].as_i64(), json["to_utc_ms"].as_i64());
    assert_eq!(from.zip(to).map(|(f, t)| t - f), Some(120_000));
    assert_eq!(json["rows"], serde_json::json!([]));

    let (status, body) = get(
        state,
        "/api/v1/program/trace?from_utc_ms=1000&to_utc_ms=1759882400000",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        (json["to_utc_ms"].as_i64(), json["clamped"].as_bool()),
        (Some(121_000), Some(true))
    );
}

/// A bound that is not an integer, or a `from` after its `to`: 400 with a
/// fixed text that never repeats what was sent.
#[tokio::test]
async fn a_trace_query_that_is_not_two_integers_is_refused_without_echoing_it() {
    let state = test_state().await;
    let (status, body) = get(
        state.clone(),
        "/api/v1/program/trace?from_utc_ms=%3Cscript%3Ealert(1)%3C%2Fscript%3E",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, "from_utc_ms must be an integer (UTC ms)");
    let (status, body) = get(state.clone(), "/api/v1/program/trace?to_utc_ms=12.5").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, "to_utc_ms must be an integer (UTC ms)");
    let (status, body) = get(state, "/api/v1/program/trace?from_utc_ms=9&to_utc_ms=8").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, "from_utc_ms is after to_utc_ms");
}
