//! #209 `GET /api/v1/program` + `POST /api/v1/program/cut`, through the real
//! axum router. Shares `test_state`/`app` with `routes_tests.rs`.
//! Wired via `#[cfg(test)] #[path = "program_tests.rs"] mod tests;`.

use crate::api::routes::tests::{app, test_state};
use crate::playback::program_bus::{
    PROGRAM_NDI_NAME, ProgramBus, SETTING_PROGRAM_SOURCE, restore_selected_source,
};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

async fn add_playlist(pool: &sqlx::SqlitePool, name: &str) -> i64 {
    sqlx::query("INSERT INTO playlists (name, youtube_url, ndi_output_name) VALUES (?, ?, ?)")
        .bind(name)
        .bind(format!("https://youtube.com/playlist?list={name}"))
        .bind(format!("SP-{name}"))
        .execute(pool)
        .await
        .unwrap()
        .last_insert_rowid()
}

async fn call(
    state: crate::AppState,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let mut req = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(v) => {
            req = req.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let resp = app(state).oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

#[tokio::test]
async fn get_program_reports_no_source_before_any_cut() {
    let state = test_state().await;
    let (status, json) = call(state, "GET", "/api/v1/program", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["ndi_name"], PROGRAM_NDI_NAME);
    assert!(json["source"].is_null());
    assert!(json["previous"].is_null());
    assert!(json["cut_boundary_100ns"].is_null());
    assert_eq!(json["health"]["forwarded"], 0);
    assert_eq!(json["health"]["filled"], 0);
    assert_eq!(json["health"]["cuts"], 0);
}

#[tokio::test]
async fn cut_selects_the_source_get_reports_it_and_it_persists() {
    let state = test_state().await;
    let slow = add_playlist(&state.pool, "slow").await;
    let fast = add_playlist(&state.pool, "fast").await;

    let (status, json) = call(
        state.clone(),
        "POST",
        "/api/v1/program/cut",
        Some(serde_json::json!({ "source": slow })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["source"], slow);
    assert!(json["cut_boundary_100ns"].as_i64().is_some_and(|b| b > 0));
    assert_eq!(json["health"]["cuts"], 1);

    let (status, json) = call(
        state.clone(),
        "POST",
        "/api/v1/program/cut",
        Some(serde_json::json!({ "source": fast })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["source"], fast);
    assert_eq!(json["health"]["cuts"], 2);

    let (status, json) = call(state.clone(), "GET", "/api/v1/program", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["source"], fast);
    assert_eq!(json["health"]["cuts"], 2);
    assert_eq!(state.program_bus.status().source, Some(fast));

    // Persisted: a restart restores the selected source.
    let stored = crate::db::models::get_setting(&state.pool, SETTING_PROGRAM_SOURCE)
        .await
        .unwrap();
    assert_eq!(stored, Some(fast.to_string()));
    let restarted = ProgramBus::new();
    assert_eq!(
        restore_selected_source(&state.pool, &restarted).await,
        Some(fast)
    );
    assert_eq!(restarted.status().source, Some(fast));
}

#[tokio::test]
async fn a_cut_away_from_a_restored_source_reports_it_as_previous() {
    let state = test_state().await;
    let slow = add_playlist(&state.pool, "slow").await;
    let fast = add_playlist(&state.pool, "fast").await;
    state.program_bus.select_initial(slow); // as restored at startup
    let (status, json) = call(
        state.clone(),
        "POST",
        "/api/v1/program/cut",
        Some(serde_json::json!({ "source": fast })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["source"], fast);
    assert_eq!(
        json["previous"], slow,
        "slow owns every boundary before the cut"
    );
    let (_, json) = call(state, "GET", "/api/v1/program", None).await;
    assert_eq!(json["previous"], slow);
}

#[tokio::test]
async fn cut_to_an_unknown_playlist_is_404_and_changes_nothing() {
    let state = test_state().await;
    let (status, _) = call(
        state.clone(),
        "POST",
        "/api/v1/program/cut",
        Some(serde_json::json!({ "source": 4242 })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(state.program_bus.status().source, None);
    assert_eq!(
        crate::db::models::get_setting(&state.pool, SETTING_PROGRAM_SOURCE)
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn cut_without_a_source_is_rejected() {
    let state = test_state().await;
    let (status, _) = call(
        state.clone(),
        "POST",
        "/api/v1/program/cut",
        Some(serde_json::json!({ "src": 1 })),
    )
    .await;
    assert!(status.is_client_error(), "got {status}");
    assert_eq!(state.program_bus.status().health.cuts, 0);
}
