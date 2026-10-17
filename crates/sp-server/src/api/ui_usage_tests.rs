//! #256: `POST /api/v1/ui-usage` and `GET /api/v1/ui-usage/summary`
//! through the real router.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sp_core::config::{PROGRAM_BLANK_ID, PROGRAM_INPUT_ID};
use sp_core::ui_usage::{BATCH_REFUSAL, DAYS_REFUSAL};
use sqlx::SqlitePool;
use tower::ServiceExt;

use crate::AppState;
use crate::api::routes::tests::{app, test_state};

const DAY: i64 = 86_400_000;
const HOUR: i64 = 3_600_000;
const JSON: &str = "application/json";

/// One stored row: `(ts_ms, page, control, kind, on_air_source)`.
type Stored = (i64, String, String, String, Option<i64>);

async fn post(state: &AppState, content_type: &str, body: String) -> (StatusCode, String) {
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/ui-usage")
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(body))
        .unwrap();
    let resp = app(state.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

async fn get(state: &AppState, uri: &str) -> (StatusCode, String) {
    let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
    let resp = app(state.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 22)
        .await
        .unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

async fn summary(state: &AppState, uri: &str) -> serde_json::Value {
    let (status, body) = get(state, uri).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    serde_json::from_str(&body).unwrap()
}

/// A batch body of `(ts_ms, page, control, kind)` events.
fn batch(events: &[(i64, &str, &str, &str)]) -> String {
    let events: Vec<serde_json::Value> = events
        .iter()
        .map(|&(ts_ms, page, control, kind)| {
            serde_json::json!({"ts_ms": ts_ms, "page": page, "control": control, "kind": kind})
        })
        .collect();
    serde_json::json!({ "events": events }).to_string()
}

/// POST `body` as the dashboard's fetch does; it must be stored.
async fn store(state: &AppState, body: String) {
    let (status, text) = post(state, JSON, body).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{text}");
    assert_eq!(text, "", "a 204 has no body");
}

async fn rows(pool: &SqlitePool) -> Vec<Stored> {
    sqlx::query_as(
        "SELECT ts_ms, page, control, kind, on_air_source FROM ui_usage ORDER BY ts_ms, rowid",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

fn stored(ts_ms: i64, page: &str, control: &str, kind: &str, source: Option<i64>) -> Stored {
    (
        ts_ms,
        page.to_string(),
        control.to_string(),
        kind.to_string(),
        source,
    )
}

#[tokio::test]
async fn a_batch_is_stored_with_the_program_source_on_air() {
    let state = test_state().await;
    state.program_bus.select_initial(7, Some("sp-fast"));
    store(
        &state,
        batch(&[
            (1_000, "/", "nav-settings", "click"),
            (1_001, "/settings", "", "page"),
        ]),
    )
    .await;
    assert_eq!(
        rows(&state.pool).await,
        [
            stored(1_000, "/", "nav-settings", "click", Some(7)),
            stored(1_001, "/settings", "", "page", Some(7)),
        ]
    );
}

#[tokio::test]
async fn a_batch_before_any_program_source_stores_none() {
    let state = test_state().await;
    store(&state, batch(&[(5, "/live", "player-skip", "click")])).await;
    assert_eq!(
        rows(&state.pool).await,
        [stored(5, "/live", "player-skip", "click", None)]
    );
}

/// `navigator.sendBeacon` sends its string as `text/plain`; it is a batch
/// like any other.
#[tokio::test]
async fn a_beacon_sent_as_plain_text_is_stored() {
    let state = test_state().await;
    state.program_bus.select_initial(PROGRAM_INPUT_ID, None);
    let body = batch(&[(9, "/settings", "<form>", "submit")]);
    let (status, text) = post(&state, "text/plain;charset=UTF-8", body).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{text}");
    assert_eq!(
        rows(&state.pool).await,
        [stored(9, "/settings", "<form>", "submit", Some(-1))]
    );
}

#[tokio::test]
async fn a_full_batch_at_its_bounds_is_stored_whole() {
    let state = test_state().await;
    let wide = "č".repeat(120);
    let events: Vec<(i64, &str, &str, &str)> = (0..200)
        .map(|ts| (ts, wide.as_str(), wide.as_str(), "change"))
        .collect();
    store(&state, batch(&events)).await;
    let kept = rows(&state.pool).await;
    assert_eq!(kept.len(), 200);
    assert_eq!(kept[199], stored(199, &wide, &wide, "change", None));
}

#[tokio::test]
async fn a_batch_out_of_bounds_is_refused_whole_and_nothing_is_written() {
    let long = "p".repeat(121);
    let over: Vec<(i64, &str, &str, &str)> = (0..201).map(|ts| (ts, "/", "x", "click")).collect();
    let bodies = [
        batch(&over),
        batch(&[(1, "/", "x", "click"), (2, long.as_str(), "x", "click")]),
        batch(&[(1, "/", long.as_str(), "click")]),
        batch(&[(1, "/", "x", "hover")]),
        "not json".to_string(),
        r#"{"events":[{"ts_ms":1,"page":"/","kind":"click"}]}"#.to_string(),
        "{}".to_string(),
    ];
    for body in bodies {
        let state = test_state().await;
        let (status, text) = post(&state, JSON, body.clone()).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{}",
            &body[..body.len().min(80)]
        );
        assert_eq!(text, BATCH_REFUSAL);
        assert!(rows(&state.pool).await.is_empty(), "nothing written");
    }
}

/// Per control on its page: the count, the distinct days with a use, the
/// first and the last use and the uses while a PLAYLIST was on air — not
/// before the first selection, not the NDI input, not Blank. The stamps
/// sit around noon UTC, so their days are the same in any time zone the
/// test runs in (`localtime`).
#[tokio::test]
async fn the_summary_counts_uses_days_and_playlist_on_air_per_control() {
    let state = test_state().await;
    let now = chrono::Utc::now().timestamp_millis();
    let base = (now / DAY - 3) * DAY + 12 * HOUR;
    // Nothing selected yet.
    store(
        &state,
        batch(&[(base - HOUR, "/live", "nav-lyrics", "click")]),
    )
    .await;
    state.program_bus.select_initial(7, Some("sp-fast"));
    store(
        &state,
        batch(&[
            (base, "/live", "nav-lyrics", "click"),
            (base + 30 * 60_000, "/live", "nav-lyrics", "click"),
            (base + DAY, "/live", "nav-lyrics", "click"),
            (base + 2 * DAY, "/lyrics", "", "page"),
        ]),
    )
    .await;
    state.program_bus.select_initial(PROGRAM_INPUT_ID, None);
    store(
        &state,
        batch(&[(base + 2 * DAY + HOUR, "/live", "nav-lyrics", "click")]),
    )
    .await;
    state.program_bus.select_initial(PROGRAM_BLANK_ID, None);
    store(
        &state,
        batch(&[(base + 2 * DAY + 2 * HOUR, "/live", "nav-lyrics", "click")]),
    )
    .await;

    let answer = summary(&state, "/api/v1/ui-usage/summary?days=30").await;
    let answered_now = answer["now_ms"].as_i64().unwrap();
    assert!(answered_now >= now, "{answer}");
    assert_eq!(answer["days"], 30);
    assert_eq!(answer["from_ms"], answered_now - 30 * DAY);
    assert_eq!(answer["events"], 7);
    assert_eq!(answer["first_ms"], base - HOUR);
    assert_eq!(
        answer["rows"],
        serde_json::json!([
            {
                "page": "/live", "control": "nav-lyrics", "kind": "click",
                "count": 6, "days_used": 3,
                "first_ms": base - HOUR, "last_ms": base + 2 * DAY + 2 * HOUR,
                "on_air": 3
            },
            {
                "page": "/lyrics", "control": "", "kind": "page",
                "count": 1, "days_used": 1,
                "first_ms": base + 2 * DAY, "last_ms": base + 2 * DAY,
                "on_air": 1
            }
        ])
    );
}

#[tokio::test]
async fn the_summary_reads_only_its_window_and_by_default_all_400_days() {
    let state = test_state().await;
    let now = chrono::Utc::now().timestamp_millis();
    store(
        &state,
        batch(&[
            (now - 10 * DAY, "/", "nav-live", "click"),
            (now - HOUR, "/", "nav-live", "click"),
            (now - 399 * DAY, "/", "nav-settings", "click"),
        ]),
    )
    .await;
    let week = summary(&state, "/api/v1/ui-usage/summary?days=7").await;
    assert_eq!(week["events"], 1, "{week}");
    assert_eq!(week["rows"][0]["control"], "nav-live");
    assert_eq!(week["rows"][0]["first_ms"], now - HOUR);
    let all = summary(&state, "/api/v1/ui-usage/summary").await;
    assert_eq!(all["days"], 400);
    assert_eq!(all["events"], 3, "{all}");
    assert_eq!(all["rows"][0]["control"], "nav-live", "most used first");
    assert_eq!(all["rows"][1]["control"], "nav-settings");
}

#[tokio::test]
async fn a_summary_with_no_use_says_so() {
    let state = test_state().await;
    let answer = summary(&state, "/api/v1/ui-usage/summary?days=1").await;
    assert_eq!(answer["events"], 0);
    assert_eq!(answer["first_ms"], serde_json::Value::Null);
    assert_eq!(answer["rows"], serde_json::json!([]));
}

#[tokio::test]
async fn days_out_of_range_is_a_400_with_a_fixed_text() {
    let state = test_state().await;
    for bad in ["0", "401", "abc", "-1", "", "%2B5"] {
        let (status, body) = get(&state, &format!("/api/v1/ui-usage/summary?days={bad}")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
        assert_eq!(body, DAYS_REFUSAL);
    }
}
