//! #217 addendum 3 — the one-param staleness probe after a liveness failure
//! the breaker did not see (a quick Arena relaunch with no push in it).

use std::time::{Duration, Instant};

use tokio::sync::broadcast;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::not_ready_tests::{arena, composition_fetches, composition_sequence, drain};
use super::relaunch_tests::composition;
use crate::resolume::driver::HostDriver;
use crate::resolume::{RecoveryEvent, SUBS_TOKEN};

fn secs(s: u64) -> Duration {
    Duration::from_secs(s)
}

/// `GET /api/v1/parameter/by-id/{param_id}` answers `status`.
async fn param_answers(server: &MockServer, param_id: i64, status: u16) {
    Mock::given(method("GET"))
        .and(path(format!("/api/v1/parameter/by-id/{param_id}")))
        .respond_with(ResponseTemplate::new(status))
        .mount(server)
        .await;
}

/// How many requests reached param `param_id` (these tests push nothing, so
/// every one is a probe).
async fn param_probes(server: &MockServer, param_id: i64) -> usize {
    let route = format!("/api/v1/parameter/by-id/{param_id}");
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == route)
        .count()
}

/// The text param `#sp-subs` is mapped to.
fn subs_param(driver: &HostDriver) -> Option<i64> {
    driver
        .clip_mapping
        .get(SUBS_TOKEN)
        .and_then(|clips| clips.first())
        .map(|clip| clip.text_param_id)
}

/// A driver whose startup refresh at `base` mapped `#sp-subs` on param 900,
/// then one failed liveness probe: a hiccup the breaker does not see.
async fn driver_after_a_hiccup(
    server: &MockServer,
    base: Instant,
) -> (HostDriver, broadcast::Receiver<RecoveryEvent>) {
    let (tx, rx) = broadcast::channel(16);
    let mut driver =
        HostDriver::new("127.0.0.1".into(), server.address().port()).with_recovery_channel(tx);
    driver.on_tick_at(base).await;
    assert_eq!(subs_param(&driver), Some(900), "the startup refresh");
    driver.apply_outcome(false);
    assert!(
        !driver.circuit_breaker_open,
        "one failure: the breaker stays closed"
    );
    (driver, rx)
}

/// The box (2026-09-27): Arena re-ids every param on relaunch. A relaunch
/// inside a hiccup shorter than three failed probes, with no push in it, kept
/// the dead ids until the 300 s TTL refresh, which fires no event. The first
/// ok probe after the failure asks Arena for one mapped param: a 404 is a
/// stale map, so the not-ready refresh runs on this tick and the new map
/// fires one RecoveryEvent (the engine re-syncs the title and the line).
#[tokio::test]
async fn a_liveness_flip_whose_param_probe_404s_refreshes_the_stale_map() {
    let server = arena().await;
    composition_sequence(
        &server,
        composition(&[(SUBS_TOKEN, 100, 900)]),
        composition(&[(SUBS_TOKEN, 200, 1900)]),
    )
    .await;
    param_answers(&server, 900, 404).await;
    let base = Instant::now();
    let (mut driver, mut rx) = driver_after_a_hiccup(&server, base).await;

    driver.on_tick_at(base + secs(10)).await;

    assert_eq!(
        param_probes(&server, 900).await,
        1,
        "one mapped param probed"
    );
    assert_eq!(
        composition_fetches(&server).await,
        2,
        "the 404 refreshed the map on this tick, not at the TTL"
    );
    assert_eq!(subs_param(&driver), Some(1900), "the relaunched clip's id");
    assert!(
        driver.not_ready_since.is_none(),
        "the clips are mapped: the episode is over"
    );
    assert_eq!(drain(&mut rx), 1, "one RecoveryEvent re-pushes the wall");
}

/// The probe's 404 opens the existing not-ready episode: while Arena is still
/// loading its composition the driver ticks every 2 s, and the tick that
/// maps the clips fires the one RecoveryEvent. The next ok tick is no flip:
/// nothing is probed again.
#[tokio::test]
async fn a_param_probe_404_while_arena_still_loads_opens_the_2_s_not_ready_episode() {
    let server = arena().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/composition"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(composition(&[(SUBS_TOKEN, 100, 900)])),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    composition_sequence(
        &server,
        serde_json::json!({"layers": []}),
        composition(&[(SUBS_TOKEN, 200, 1900)]),
    )
    .await;
    param_answers(&server, 900, 404).await;
    let base = Instant::now();
    let (mut driver, mut rx) = driver_after_a_hiccup(&server, base).await;

    driver.on_tick_at(base + secs(10)).await;
    assert_eq!(
        driver.not_ready_since,
        Some(base + secs(10)),
        "the probe's 404 opened the episode"
    );
    assert_eq!(composition_fetches(&server).await, 2, "refreshed at once");
    assert_eq!(
        driver.tick_period(base + secs(10), secs(10)),
        secs(2),
        "Arena is still loading: the episode ticks every 2 s"
    );
    assert_eq!(drain(&mut rx), 0, "nothing mapped yet, nothing to re-push");

    driver.on_tick_at(base + secs(12)).await;
    assert_eq!(composition_fetches(&server).await, 3);
    assert_eq!(subs_param(&driver), Some(1900));
    assert_eq!(drain(&mut rx), 1, "the ready map fires the RecoveryEvent");
    assert_eq!(
        param_probes(&server, 900).await,
        1,
        "the 2 s tick followed an ok one: no flip, no second probe"
    );
}

/// A param Arena still answers: the map is valid, and a 14 MB refresh would
/// be pure load (the design's rejected alternative).
#[tokio::test]
async fn a_liveness_flip_whose_param_probe_answers_200_refreshes_nothing() {
    let server = arena().await;
    let mapped = composition(&[(SUBS_TOKEN, 100, 900)]);
    composition_sequence(&server, mapped.clone(), mapped).await;
    param_answers(&server, 900, 200).await;
    let base = Instant::now();
    let (mut driver, mut rx) = driver_after_a_hiccup(&server, base).await;

    driver.on_tick_at(base + secs(10)).await;

    assert_eq!(
        param_probes(&server, 900).await,
        1,
        "the flip probed one mapped param"
    );
    assert_eq!(
        composition_fetches(&server).await,
        1,
        "the param answered: no refresh"
    );
    assert!(driver.not_ready_since.is_none());
    assert_eq!(drain(&mut rx), 0);
}

/// No failure since the last ok probe: nothing to check. A breaker close
/// resyncs the map on its own (the breaker-closed refresh), so it is not
/// probed either. The param answers 404 here, so a probe would refresh.
#[tokio::test]
async fn only_a_failure_the_breaker_did_not_see_is_probed() {
    let server = arena().await;
    let mapped = composition(&[(SUBS_TOKEN, 100, 900)]);
    composition_sequence(&server, mapped.clone(), mapped).await;
    param_answers(&server, 900, 404).await;
    let (tx, _rx) = broadcast::channel(16);
    let mut driver =
        HostDriver::new("127.0.0.1".into(), server.address().port()).with_recovery_channel(tx);
    let base = Instant::now();
    driver.on_tick_at(base).await;

    driver.on_tick_at(base + secs(10)).await;
    assert_eq!(param_probes(&server, 900).await, 0, "a steady tick");
    assert_eq!(composition_fetches(&server).await, 1);

    for _ in 0..3 {
        driver.apply_outcome(false);
    }
    assert!(
        driver.circuit_breaker_open,
        "three failures open the breaker"
    );
    driver.on_tick_at(base + secs(20)).await;
    assert!(!driver.circuit_breaker_open, "the ok probe closed it");
    assert_eq!(
        param_probes(&server, 900).await,
        0,
        "the breaker close resyncs the map itself"
    );
}

/// A failure inside an open not-ready episode: the episode refreshes anyway.
/// A probe would only restamp its start and stretch its 120 s fast window.
#[tokio::test]
async fn an_open_not_ready_episode_is_not_probed() {
    let server = arena().await;
    composition_sequence(
        &server,
        composition(&[(SUBS_TOKEN, 100, 900)]),
        serde_json::json!({"layers": []}),
    )
    .await;
    param_answers(&server, 900, 404).await;
    let base = Instant::now();
    let (mut driver, _rx) = driver_after_a_hiccup(&server, base).await;
    driver.not_ready_since = Some(base + secs(5));

    driver.on_tick_at(base + secs(10)).await;

    assert_eq!(param_probes(&server, 900).await, 0);
    assert_eq!(
        driver.not_ready_since,
        Some(base + secs(5)),
        "the episode keeps its start"
    );
    assert_eq!(
        composition_fetches(&server).await,
        2,
        "the episode's own refetch ran"
    );
}
