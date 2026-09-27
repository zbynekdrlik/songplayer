//! #217 addendum — Arena's REST answers before its composition has loaded.
//! A composition with none of SongPlayer's clips is NOT READY: it is fetched
//! again on the next liveness tick, and the refresh that finally finds the
//! clips fires one `RecoveryEvent`, so the engine re-pushes the title and the
//! current line. Split from `driver_poll_tests.rs` for the 1000-line cap.

use std::time::{Duration, Instant};

use tokio::sync::broadcast;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::resolume::{RecoveryEvent, SUBS_TOKEN, TITLE_TOKEN};

/// A composition with one text clip per token, laid out like Arena's
/// `/composition` (`layers[].clips[].name.value` plus a `ParamText` param).
fn composition_with(tokens: &[&str]) -> serde_json::Value {
    let clips: Vec<serde_json::Value> = tokens
        .iter()
        .zip(0_i64..)
        .map(|(token, i)| {
            let clip_id = 100 + i;
            let param_id = 900 + i;
            serde_json::json!({
                "id": clip_id,
                "name": { "value": token },
                "video": { "sourceparams": { "Text": { "id": param_id, "valuetype": "ParamText" } } }
            })
        })
        .collect();
    serde_json::json!({ "layers": [{ "clips": clips }] })
}

/// An Arena whose light `/product` probe always answers.
async fn arena() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/product"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"name": "Arena"})),
        )
        .mount(&server)
        .await;
    server
}

/// `/composition` answers `first` once, then `then` on every later fetch.
/// The `first` mock is mounted first, so it wins while it has a use left.
async fn composition_sequence(
    server: &MockServer,
    first: serde_json::Value,
    then: serde_json::Value,
) {
    Mock::given(method("GET"))
        .and(path("/api/v1/composition"))
        .respond_with(ResponseTemplate::new(200).set_body_json(first))
        .up_to_n_times(1)
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/composition"))
        .respond_with(ResponseTemplate::new(200).set_body_json(then))
        .mount(server)
        .await;
}

async fn composition_fetches(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/api/v1/composition")
        .count()
}

/// How many `RecoveryEvent`s are waiting on the channel (they are sent
/// synchronously inside the driver call, so none is still in flight).
fn drain(rx: &mut broadcast::Receiver<RecoveryEvent>) -> usize {
    let mut n = 0;
    while rx.try_recv().is_ok() {
        n += 1;
    }
    n
}

// -- the regression: RED on the pre-#217 driver ---------------------------

/// Startup against an Arena whose composition has not loaded yet. The empty
/// composition is not a successful refresh; the next tick fetches again (not
/// the 300 s TTL later), and the fetch that finds `#sp-subs` fires exactly one
/// `RecoveryEvent`. After that the steady state (the light probe only)
/// resumes.
#[tokio::test]
async fn composition_not_loaded_at_startup_is_refetched_on_the_next_tick() {
    let server = arena().await;
    composition_sequence(
        &server,
        serde_json::json!({"layers": []}),
        composition_with(&[SUBS_TOKEN]),
    )
    .await;
    let (tx, mut rx) = broadcast::channel(16);
    let mut driver =
        HostDriver::new("127.0.0.1".into(), server.address().port()).with_recovery_channel(tx);
    let base = Instant::now();

    // Tick 1: the startup refresh gets the not-yet-loaded composition.
    driver.on_tick_at(base).await;
    assert_eq!(
        composition_fetches(&server).await,
        1,
        "the first tick runs the startup refresh"
    );
    assert!(
        driver.last_full_refresh_ok_at.is_none(),
        "a composition with no SongPlayer clip is not a successful refresh"
    );
    assert_eq!(
        drain(&mut rx),
        0,
        "nothing to re-push while the mapping is not ready"
    );

    // Tick 2, 10 s later: fetched again now.
    driver.on_tick_at(base + Duration::from_secs(10)).await;
    assert_eq!(
        composition_fetches(&server).await,
        2,
        "a not-ready mapping is fetched again on the next tick, not after the TTL"
    );
    assert!(
        driver.clip_mapping.contains_key(SUBS_TOKEN),
        "the second fetch maps the loaded #sp-subs clip"
    );
    assert_eq!(
        drain(&mut rx),
        1,
        "the refresh that finds the clips fires exactly one RecoveryEvent"
    );

    // Tick 3: the mapping is ready, so only the light probe runs.
    driver.on_tick_at(base + Duration::from_secs(20)).await;
    assert_eq!(
        composition_fetches(&server).await,
        2,
        "a ready mapping returns to the steady state"
    );
    assert_eq!(drain(&mut rx), 0, "no further RecoveryEvent");
}

/// The box incident (2026-09-27): Arena was closed for 26 min (breaker open,
/// clip cache evicted), then relaunched. Its REST answered first, so the
/// breaker-closed refresh got an empty composition, and the clips appeared a
/// few seconds later. The next tick must fetch again: neither the pre-restart
/// TTL stamp nor the 60 s retry window may hold it back. It also fires one
/// `RecoveryEvent` beyond the breaker-close one, so the title and line are
/// re-pushed to clips that now exist.
#[tokio::test]
async fn arena_relaunch_refetches_the_composition_until_its_clips_load() {
    let server = arena().await;
    composition_sequence(
        &server,
        serde_json::json!({"layers": []}),
        composition_with(&[TITLE_TOKEN, SUBS_TOKEN]),
    )
    .await;
    let (tx, mut rx) = broadcast::channel(16);
    let mut driver =
        HostDriver::new("127.0.0.1".into(), server.address().port()).with_recovery_channel(tx);
    let base = Instant::now();
    // Before the relaunch: a full refresh at `base`, then Arena went away
    // (three failed probes opened the breaker and evicted the clip cache).
    driver.last_full_refresh_ok_at = Some(base);
    driver.last_full_attempt_at = Some(base);
    driver.consecutive_failures = 3;
    driver.circuit_breaker_open = true;

    let relaunch = base + Duration::from_secs(1800);
    driver.on_tick_at(relaunch).await;
    assert!(
        !driver.circuit_breaker_open,
        "Arena's REST answering closes the breaker"
    );
    assert_eq!(
        composition_fetches(&server).await,
        1,
        "the breaker close runs one full refresh"
    );
    assert!(
        driver.clip_mapping.is_empty(),
        "the composition has not loaded yet"
    );
    assert_eq!(
        drain(&mut rx),
        1,
        "the breaker close fires its own RecoveryEvent"
    );

    driver.on_tick_at(relaunch + Duration::from_secs(10)).await;
    assert_eq!(
        composition_fetches(&server).await,
        2,
        "the next tick fetches again, not the TTL refresh 5 min later"
    );
    assert!(
        driver.clip_mapping.contains_key(TITLE_TOKEN)
            && driver.clip_mapping.contains_key(SUBS_TOKEN),
        "the loaded composition's #sp-title and #sp-subs clips are mapped"
    );
    assert_eq!(
        drain(&mut rx),
        1,
        "the clips arriving fire one more RecoveryEvent, so the engine re-pushes"
    );

    driver.on_tick_at(relaunch + Duration::from_secs(20)).await;
    assert_eq!(
        composition_fetches(&server).await,
        2,
        "a ready mapping returns to the steady state"
    );
}
