//! #257: `GET /api/v1/audio/faults` through the real router, over the
//! ledger's own rows (`audio_faults::store::insert`).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use crate::AppState;
use crate::api::routes::tests::{app, test_state};
use crate::audio_faults::answer::{HOURS_REFUSAL, MAX_EVENTS};
use crate::audio_faults::model::{FaultRow, Severity, kinds};
use crate::audio_faults::{store, utc_now_ms};

async fn get(state: AppState, uri: &str) -> (StatusCode, String) {
    let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
    let resp = app(state).oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 22)
        .await
        .unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

async fn json(state: AppState, uri: &str) -> serde_json::Value {
    let (status, body) = get(state, uri).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    serde_json::from_str(&body).unwrap()
}

fn fault(ts_ms: i64, path: &str, kind: &'static str, severity: Severity, amount: i64) -> FaultRow {
    FaultRow {
        ts_ms,
        path: path.to_string(),
        kind,
        severity,
        amount,
        detail: format!("{path} name"),
    }
}

/// The path named `path` in the answer.
fn path<'a>(answer: &'a serde_json::Value, path: &str) -> &'a serde_json::Value {
    answer["paths"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["path"] == path)
        .unwrap_or_else(|| panic!("no {path} in {answer}"))
}

#[tokio::test]
async fn an_empty_ledger_answers_the_program_clean_for_the_default_day() {
    let state = test_state().await;
    let before = utc_now_ms();
    let answer = json(state, "/api/v1/audio/faults").await;
    let now = answer["now_ms"].as_i64().unwrap();
    assert!(now >= before, "{answer}");
    assert_eq!(answer["hours"], 24);
    assert_eq!(answer["from_ms"], now - 86_400_000);
    assert_eq!(answer["worst"], serde_json::Value::Null);
    assert_eq!(answer["truncated"], false);
    assert_eq!(answer["kept_from_ms"], now - 7_776_000_000);
    assert_eq!(
        answer["clean_since_ms"], now,
        "a ledger with no row claims no clean time"
    );
    let program = path(&answer, "program");
    assert_eq!(program["detail"], "SP-program");
    assert_eq!(program["live"], true);
    assert_eq!(program["clean_since_ms"], now);
    assert_eq!(program["counts"], serde_json::json!({}));
    assert_eq!(program["events"], serde_json::json!([]));
}

#[tokio::test]
async fn hours_out_of_range_is_a_400_with_a_fixed_text() {
    for bad in ["0", "2161", "abc", "-1", "%2B5"] {
        let state = test_state().await;
        let (status, body) = get(state, &format!("/api/v1/audio/faults?hours={bad}")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
        assert_eq!(body, HOURS_REFUSAL, "{bad}");
    }
    let state = test_state().await;
    let answer = json(state, "/api/v1/audio/faults?hours=2160").await;
    assert_eq!(answer["hours"], 2160);
}

#[tokio::test]
async fn the_window_holds_its_rows_counts_and_the_last_loss() {
    let state = test_state().await;
    let now = utc_now_ms();
    let rows = [
        fault(
            now - 2 * 3_600_000,
            "out:2",
            kinds::UNDERRUN,
            Severity::Loss,
            9,
        ),
        fault(now - 50_000, "out:2", kinds::START, Severity::Info, 1),
        fault(now - 40_000, "out:2", kinds::UNDERRUN, Severity::Loss, 2),
        fault(now - 30_000, "out:2", kinds::UNDERRUN, Severity::Loss, 3),
        fault(
            now - 20_000,
            "out:2",
            kinds::LATE_CALLBACK,
            Severity::Risk,
            1,
        ),
        fault(now - 10_000, "loop:2", kinds::MISMATCH, Severity::Loss, 256),
        fault(
            now - 5_000,
            "loop:2",
            kinds::CHECKED,
            Severity::Meter,
            61_000,
        ),
    ];
    store::insert(&state.pool, &rows).await.unwrap();
    let answer = json(state, "/api/v1/audio/faults?hours=1").await;
    let out = path(&answer, "out:2");
    assert_eq!(out["detail"], "out:2 name");
    assert_eq!(out["live"], false);
    assert_eq!(out["clean_since_ms"], now - 30_000);
    assert_eq!(out["last_loss_ms"], now - 30_000);
    assert_eq!(out["last_risk_ms"], now - 20_000);
    assert_eq!(
        out["counts"],
        serde_json::json!({"underrun": 5, "late_callback": 1}),
        "the row two hours back is outside the window"
    );
    assert_eq!(
        out["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| (e["kind"].as_str().unwrap(), e["amount"].as_i64().unwrap()))
            .collect::<Vec<_>>(),
        vec![
            ("start", 1),
            ("underrun", 2),
            ("underrun", 3),
            ("late_callback", 1)
        ],
        "oldest first"
    );
    let looped = path(&answer, "loop:2");
    assert_eq!(looped["counts"], serde_json::json!({"mismatch": 1}));
    assert_eq!(
        looped["loop"],
        serde_json::json!({"checked_s": 61.0, "blind_s": 0.0})
    );
    assert_eq!(
        looped["events"].as_array().unwrap().len(),
        1,
        "a meter is no event"
    );
    assert_eq!(answer["worst"], "loop:2", "the most recent loss");
}

/// #257 (main session's ruling): a planned restart is no fault, so a
/// path's clean time and the chain's look beyond the asked window: the
/// newest loss the ledger keeps, else its first start.
#[tokio::test]
async fn clean_since_looks_beyond_the_window_and_across_restarts() {
    let state = test_state().await;
    let now = utc_now_ms();
    const DAY: i64 = 86_400_000;
    let rows = [
        fault(now - 10 * DAY, "program", kinds::START, Severity::Info, 1),
        fault(now - 10 * DAY, "out:b", kinds::START, Severity::Info, 1),
        fault(now - 3 * DAY, "program", kinds::FILLED, Severity::Loss, 2),
        fault(
            now - 2 * DAY,
            "out:b",
            kinds::LATE_CALLBACK,
            Severity::Risk,
            1,
        ),
        fault(now - 3_600_000, "program", kinds::START, Severity::Info, 1),
        fault(now - 3_600_000, "out:b", kinds::START, Severity::Info, 1),
        fault(
            now - 91 * DAY,
            "out:old",
            kinds::UNDERRUN,
            Severity::Loss,
            1,
        ),
    ];
    store::insert(&state.pool, &rows).await.unwrap();
    let answer = json(state, "/api/v1/audio/faults?hours=2").await;
    assert_eq!(
        path(&answer, "program")["clean_since_ms"],
        now - 3 * DAY,
        "the loss three days back, across the restart an hour ago"
    );
    assert_eq!(
        path(&answer, "program")["last_loss_ms"],
        serde_json::Value::Null
    );
    assert_eq!(
        path(&answer, "out:b")["clean_since_ms"],
        now - 10 * DAY,
        "no loss: its FIRST start kept"
    );
    assert_eq!(
        answer["clean_since_ms"],
        now - 3 * DAY,
        "the chain: the newest loss kept; a loss past the 90 days is gone"
    );
    assert_eq!(
        answer["worst"],
        serde_json::Value::Null,
        "no loss in the window"
    );
}

#[tokio::test]
async fn over_500_events_answer_the_newest_500_and_say_so() {
    let state = test_state().await;
    let now = utc_now_ms();
    let rows: Vec<FaultRow> = (0..=MAX_EVENTS as i64)
        .map(|i| {
            fault(
                now - 100_000 + i,
                "out:1",
                kinds::UNDERRUN,
                Severity::Loss,
                1,
            )
        })
        .collect();
    store::insert(&state.pool, &rows).await.unwrap();
    let answer = json(state, "/api/v1/audio/faults?hours=1").await;
    assert_eq!(answer["truncated"], true);
    let out = path(&answer, "out:1");
    let events = out["events"].as_array().unwrap();
    assert_eq!(events.len(), MAX_EVENTS);
    assert_eq!(
        events[0]["ts_ms"],
        now - 100_000 + 1,
        "the oldest one left out"
    );
    assert_eq!(out["counts"]["underrun"], 501, "the counts hold every row");
}

#[tokio::test]
async fn exactly_500_events_are_not_truncated() {
    let state = test_state().await;
    let now = utc_now_ms();
    let rows: Vec<FaultRow> = (0..MAX_EVENTS as i64)
        .map(|i| {
            fault(
                now - 100_000 + i,
                "out:1",
                kinds::UNDERRUN,
                Severity::Loss,
                1,
            )
        })
        .collect();
    store::insert(&state.pool, &rows).await.unwrap();
    let answer = json(state, "/api/v1/audio/faults?hours=1").await;
    assert_eq!(answer["truncated"], false);
    assert_eq!(
        path(&answer, "out:1")["events"].as_array().unwrap().len(),
        MAX_EVENTS
    );
}

#[tokio::test]
async fn a_ledger_that_cannot_be_read_is_a_500_naming_why() {
    let state = test_state().await;
    state.pool.close().await;
    let (status, body) = get(state, "/api/v1/audio/faults").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        body.starts_with("the audio fault ledger could not be read: "),
        "{body}"
    );
}
