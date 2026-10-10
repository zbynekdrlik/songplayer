//! #228: `POST /api/v1/program/burn`, and `burn_on` / `burned_boundaries` /
//! `on_air_item` on `GET /api/v1/program`, through the real router. Wired
//! via `#[cfg(test)] #[path = "program_tests_burn.rs"] mod tests_burn;` in
//! `api/program.rs`.

use axum::http::StatusCode;
use serde_json::json;

use super::tests::{add_playlist, call};
use crate::api::routes::tests::test_state;
use crate::playback::program_item::ItemStatus;

#[tokio::test]
async fn the_burn_is_off_and_no_item_is_on_air_at_the_start() {
    let state = test_state().await;
    let (status, json) = call(state, "GET", "/api/v1/program", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["burn_on"], json!(false));
    assert_eq!(json["burned_boundaries"], json!(0));
    assert!(json["on_air_item"].is_null(), "{json}");
}

#[tokio::test]
async fn the_burn_route_switches_the_burn_on_and_off() {
    let state = test_state().await;
    let on = Some(json!({ "on": true }));
    let (status, json) = call(state.clone(), "POST", "/api/v1/program/burn", on).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json, json!({ "burn_on": true }));
    assert!(state.program_bus.item().burn_on(), "the sender's switch");
    let (_, json) = call(state.clone(), "GET", "/api/v1/program", None).await;
    assert_eq!(json["burn_on"], json!(true));

    let off = Some(json!({ "on": false }));
    let (status, json) = call(state.clone(), "POST", "/api/v1/program/burn", off).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json, json!({ "burn_on": false }));
    let (_, json) = call(state, "GET", "/api/v1/program", None).await;
    assert_eq!(json["burn_on"], json!(false));
}

#[tokio::test]
async fn a_burn_request_without_its_switch_is_refused() {
    let state = test_state().await;
    let body = Some(json!({ "burn": true }));
    let (status, _) = call(state.clone(), "POST", "/api/v1/program/burn", body).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(!state.program_bus.item().burn_on());
}

#[tokio::test]
async fn the_burned_boundaries_are_counted_on_the_answer() {
    let state = test_state().await;
    state.program_bus.item().count_burned();
    state.program_bus.item().count_burned();
    let (_, json) = call(state, "GET", "/api/v1/program", None).await;
    assert_eq!(json["burned_boundaries"], json!(2));
}

/// The item the sender put on the wire, with its video's YouTube id and
/// title from the store.
#[tokio::test]
async fn the_on_air_item_carries_its_video_s_id_and_title() {
    let state = test_state().await;
    let pid = add_playlist(&state.pool, "test").await;
    let video_id = sqlx::query(
        "INSERT INTO videos (playlist_id, youtube_id, title, normalized) \
         VALUES (?, 'measure-v01', 'camera-box meracie video v1 (128 s)', 1)",
    )
    .bind(pid)
    .execute(&state.pool)
    .await
    .unwrap()
    .last_insert_rowid();
    let item = ItemStatus {
        playlist_id: pid,
        video_id,
        started_at_utc_ns: 1_760_000_000_000_000_000,
        position_ms: 60_033,
        frame: 1_801,
        frame_utc_ns: 1_760_000_060_033_333_300,
    };
    state.program_bus.item().publish(Some(item));
    let (_, json) = call(state, "GET", "/api/v1/program", None).await;
    let on_air = &json["on_air_item"];
    assert_eq!(on_air["playlist_id"], json!(pid));
    assert_eq!(on_air["video_id"], json!(video_id));
    assert_eq!(on_air["youtube_id"], json!("measure-v01"));
    assert_eq!(
        on_air["title"],
        json!("camera-box meracie video v1 (128 s)")
    );
    assert_eq!(
        on_air["started_at_utc_ns"].as_i64(),
        Some(1_760_000_000_000_000_000)
    );
    assert_eq!(on_air["position_ms"], json!(60_033));
    assert_eq!(on_air["frame"], json!(1_801));
    assert_eq!(
        on_air["frame_utc_ns"].as_i64(),
        Some(1_760_000_060_033_333_300)
    );
}

/// The cut answer carries the same fields.
#[tokio::test]
async fn the_cut_answer_carries_the_burn_and_the_item_too() {
    let state = test_state().await;
    state.program_bus.item().set_burn(true);
    let body = Some(json!({ "source": -2 }));
    let (status, json) = call(state, "POST", "/api/v1/program/cut", body).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["burn_on"], json!(true));
    assert!(
        json.get("on_air_item").is_some_and(|v| v.is_null()),
        "{json}"
    );
}

/// An item whose video row is gone still shows, with no id and no title.
#[tokio::test]
async fn an_item_whose_video_is_gone_shows_no_id_and_no_title() {
    let state = test_state().await;
    let item = ItemStatus {
        playlist_id: 3,
        video_id: 999,
        started_at_utc_ns: 1,
        position_ms: 2,
        frame: 3,
        frame_utc_ns: 4,
    };
    state.program_bus.item().publish(Some(item));
    let (_, json) = call(state, "GET", "/api/v1/program", None).await;
    let on_air = &json["on_air_item"];
    assert_eq!(on_air["video_id"], json!(999));
    assert!(on_air["youtube_id"].is_null());
    assert!(on_air["title"].is_null());
}
