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
pub(super) async fn arena() -> MockServer {
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
pub(super) async fn composition_sequence(
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

pub(super) async fn composition_fetches(server: &MockServer) -> usize {
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
pub(super) fn drain(rx: &mut broadcast::Receiver<RecoveryEvent>) -> usize {
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

// -- FullRefreshReason::decide with a NOT READY mapping (pure policy) ------

/// `decide` with the production TTL and retry windows, never forced.
fn decide_at(
    now: Instant,
    last_full_ok: Option<Instant>,
    last_full_attempt: Option<Instant>,
    not_ready_since: Option<Instant>,
    breaker_just_closed: bool,
) -> Option<FullRefreshReason> {
    FullRefreshReason::decide(
        now,
        last_full_ok,
        last_full_attempt,
        not_ready_since,
        false,
        FULL_REFRESH_TTL,
        FULL_REFRESH_RETRY,
        false,
        breaker_just_closed,
    )
}

fn secs(s: u64) -> Duration {
    Duration::from_secs(s)
}

#[test]
fn decide_not_ready_fires_on_every_tick_inside_the_first_two_minutes() {
    let base = Instant::now();
    // Every ~10 s tick from 10 s to 110 s after the first not-ready refresh,
    // with the previous tick's fetch 10 s ago (well inside the 60 s retry
    // window), fetches again. It does so both at startup (never a successful
    // refresh) and with a fresh TTL stamp that would otherwise mean "steady".
    for k in 1..=11u64 {
        let now = base + secs(10 * k);
        let attempt = Some(base + secs(10 * (k - 1)));
        assert_eq!(
            decide_at(now, None, attempt, Some(base), false),
            Some(FullRefreshReason::NotReady),
            "{} s into a not-ready episode (startup) must fetch again",
            10 * k
        );
        assert_eq!(
            decide_at(now, Some(base), attempt, Some(base), false),
            Some(FullRefreshReason::NotReady),
            "{} s into a not-ready episode must fetch again despite a fresh TTL stamp",
            10 * k
        );
    }
}

#[test]
fn decide_not_ready_after_an_arena_restart_beats_the_old_ttl_stamp() {
    // The box incident: the last good refresh is 30 min old, the breaker-closed
    // refresh 10 s ago got an empty composition. A TTL reason would wait out
    // the 60 s retry window; NotReady fetches now.
    let base = Instant::now();
    let relaunch = base + secs(1800);
    assert_eq!(
        decide_at(
            relaunch + secs(10),
            Some(base),
            Some(relaunch),
            Some(relaunch),
            false
        ),
        Some(FullRefreshReason::NotReady),
    );
}

#[test]
fn decide_not_ready_at_the_end_of_the_fast_window_uses_the_retry_window() {
    // Exactly 120 s after the first not-ready refresh the fast window is over
    // (`<`, not `<=`), so the 60 s retry window applies: the last attempt was
    // 10 s ago, so no fetch on this tick.
    let base = Instant::now();
    assert_eq!(
        decide_at(
            base + secs(120),
            None,
            Some(base + secs(110)),
            Some(base),
            false
        ),
        None,
    );
}

#[test]
fn decide_not_ready_after_the_fast_window_fetches_once_per_retry_window() {
    let base = Instant::now();
    let attempt = Some(base + secs(120));
    assert_eq!(
        decide_at(base + secs(179), Some(base), attempt, Some(base), false),
        None,
        "59 s after the last attempt the retry window still holds"
    );
    assert_eq!(
        decide_at(base + secs(180), Some(base), attempt, Some(base), false),
        Some(FullRefreshReason::NotReady),
        "60 s after the last attempt a still-not-ready mapping is fetched again"
    );
}

#[test]
fn decide_not_ready_after_a_failed_fetch_keeps_the_retry_window() {
    // Inside the fast window, but the last /composition attempt FAILED (the
    // #157 case): no every-tick refetch, the 60 s retry window applies.
    let base = Instant::now();
    let after_failed_fetch = |now| {
        FullRefreshReason::decide(
            now,
            None,
            Some(base),
            Some(base),
            true,
            FULL_REFRESH_TTL,
            FULL_REFRESH_RETRY,
            false,
            false,
        )
    };
    assert_eq!(
        after_failed_fetch(base + secs(10)),
        None,
        "10 s after a failed fetch the retry window holds, even in the fast window"
    );
    assert_eq!(
        after_failed_fetch(base + secs(60)),
        Some(FullRefreshReason::NotReady),
        "60 s after a failed fetch a not-ready mapping is fetched again"
    );
}

#[test]
fn decide_ready_mapping_keeps_the_steady_state() {
    let base = Instant::now();
    assert_eq!(
        decide_at(base + secs(10), Some(base), Some(base), None, false),
        None,
        "a ready, fresh mapping runs only the light probe"
    );
}

#[test]
fn decide_command_and_breaker_close_keep_their_precedence_over_not_ready() {
    let base = Instant::now();
    assert_eq!(
        FullRefreshReason::decide(
            base + secs(10),
            None,
            Some(base),
            Some(base),
            false,
            FULL_REFRESH_TTL,
            FULL_REFRESH_RETRY,
            true,
            true,
        ),
        Some(FullRefreshReason::Command),
        "a forced command wins"
    );
    assert_eq!(
        decide_at(base + secs(10), None, None, Some(base), true),
        Some(FullRefreshReason::BreakerClosed),
        "a just-closed breaker keeps its own reason"
    );
}

// -- has_songplayer_clips ----------------------------------------------------

#[test]
fn a_mapping_is_ready_only_with_one_of_songplayer_s_own_tokens() {
    assert!(
        !has_songplayer_clips(&parse_composition(&serde_json::json!({"layers": []}))),
        "an empty composition (Arena still loading) is not ready"
    );
    assert!(
        !has_songplayer_clips(&parse_composition(&composition_with(&[
            "#timer",
            "#bible-verse"
        ]))),
        "the operator's own tokens alone are not ready"
    );
    for token in crate::resolume::SONGPLAYER_TOKENS {
        assert!(
            has_songplayer_clips(&parse_composition(&composition_with(&["#timer", token]))),
            "{token} alone makes the mapping ready"
        );
    }
}

// -- the driver over wiremock ------------------------------------------------

/// A composition that never gets SongPlayer's clips (only the operator's own
/// tokens), polled for 5 minutes of ~10 s ticks: 1 startup fetch + 11 fast
/// fetches (10–110 s) + 3 in the 60 s retry window (170, 230, 290 s) = 15,
/// against 31 if every tick fetched. The test drives the ticks 10 s apart; the
/// real driver ticks every 2 s on the fast path (#217 addendum 2: 63 fetches,
/// `a_not_ready_episode_refetches_every_2_s_then_falls_back_after_120_s`).
#[tokio::test]
async fn a_composition_that_never_loads_is_fetched_15_times_in_five_minutes() {
    let server = arena().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/composition"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(composition_with(&["#timer", "#bible-verse"])),
        )
        .mount(&server)
        .await;
    let (tx, mut rx) = broadcast::channel(16);
    let mut driver =
        HostDriver::new("127.0.0.1".into(), server.address().port()).with_recovery_channel(tx);
    let base = Instant::now();

    for k in 0..=30u64 {
        driver.on_tick_at(base + secs(10 * k)).await;
    }

    assert_eq!(
        composition_fetches(&server).await,
        15,
        "1 startup + 11 fast (10..110 s) + 3 retry-window fetches (170, 230, 290 s)"
    );
    assert_eq!(
        drain(&mut rx),
        0,
        "a mapping that never gets ready never fires a RecoveryEvent"
    );
    assert!(
        driver.last_full_refresh_ok_at.is_none() && driver.last_full_refresh_ts.is_none(),
        "no refresh of a not-ready composition counts as successful"
    );
}

#[tokio::test]
async fn a_ready_composition_on_a_clean_start_fires_no_recovery_event() {
    let server = arena().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/composition"))
        .respond_with(ResponseTemplate::new(200).set_body_json(composition_with(&[SUBS_TOKEN])))
        .mount(&server)
        .await;
    let (tx, mut rx) = broadcast::channel(16);
    let mut driver =
        HostDriver::new("127.0.0.1".into(), server.address().port()).with_recovery_channel(tx);
    let base = Instant::now();

    driver.on_tick_at(base).await;

    assert_eq!(
        driver.last_full_refresh_ok_at,
        Some(base),
        "a ready startup refresh is a success, stamped on the tick's clock"
    );
    assert_eq!(
        drain(&mut rx),
        0,
        "nothing was lost, so a clean ready start re-pushes nothing"
    );
}

/// The ready refresh comes after a failure (e.g. a forced RefreshMapping
/// while the last probe had failed). The failure evicted nothing, so its
/// recovery fires no event of its own (#217 addendum 2); the ready transition
/// fires the one. Two re-pushes would restart the title fade (a visible
/// blink).
#[tokio::test]
async fn a_ready_refresh_after_a_failure_fires_exactly_one_recovery_event() {
    let server = arena().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/composition"))
        .respond_with(ResponseTemplate::new(200).set_body_json(composition_with(&[SUBS_TOKEN])))
        .mount(&server)
        .await;
    let (tx, mut rx) = broadcast::channel(16);
    let mut driver =
        HostDriver::new("127.0.0.1".into(), server.address().port()).with_recovery_channel(tx);
    let base = Instant::now();
    driver.not_ready_since = Some(base);
    driver.consecutive_failures = 1;

    driver.refresh_mapping(base + secs(10)).await.unwrap();

    assert!(
        driver.not_ready_since.is_none(),
        "the ready refresh ends the not-ready episode"
    );
    assert_eq!(
        drain(&mut rx),
        1,
        "one refresh fires exactly one RecoveryEvent"
    );
}

/// Arena goes away in the middle of a not-ready episode and comes back with
/// its composition loaded. The breaker close fires its own RecoveryEvent, and
/// the engine's re-push lands after the breaker-closed refresh maps the clips.
/// That refresh ends the episode the close started, but in the same step as
/// the close's event, so it must not fire a second one (a second re-push
/// restarts the title fade).
#[tokio::test]
async fn an_outage_during_a_not_ready_episode_fires_one_recovery_event_on_return() {
    let server = arena().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/composition"))
        .respond_with(ResponseTemplate::new(200).set_body_json(composition_with(&[SUBS_TOKEN])))
        .mount(&server)
        .await;
    let (tx, mut rx) = broadcast::channel(16);
    let mut driver =
        HostDriver::new("127.0.0.1".into(), server.address().port()).with_recovery_channel(tx);
    let base = Instant::now();
    driver.not_ready_since = Some(base);
    for _ in 0..3 {
        driver.apply_outcome(false);
    }
    assert!(
        driver.circuit_breaker_open,
        "three failures open the breaker"
    );
    assert!(
        driver.not_ready_since.is_none(),
        "opening the breaker ends the not-ready episode"
    );

    driver.on_tick_at(base + secs(100)).await;

    assert!(
        driver.clip_mapping.contains_key(SUBS_TOKEN),
        "the breaker-closed refresh maps the loaded clips"
    );
    assert_eq!(
        drain(&mut rx),
        1,
        "only the breaker close's RecoveryEvent, no second one from the same return"
    );
}

/// Review round 1: a probe failed during the not-ready episode (too few to
/// open the breaker). On the next tick the probe's recovery resets the failure
/// count, then the NotReady refresh on the same tick finds the clips. One
/// event for the tick, never two (a second ShowTitle restarts the title fade).
/// Before #217 addendum 2 the probe's recovery fired it and the ready
/// transition held back; now a bare failing→ok flip fires none, and the ready
/// transition fires the one.
#[tokio::test]
async fn a_probe_recovery_on_the_ready_tick_fires_one_recovery_event() {
    let server = arena().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/composition"))
        .respond_with(ResponseTemplate::new(200).set_body_json(composition_with(&[SUBS_TOKEN])))
        .mount(&server)
        .await;
    let (tx, mut rx) = broadcast::channel(16);
    let mut driver =
        HostDriver::new("127.0.0.1".into(), server.address().port()).with_recovery_channel(tx);
    let base = Instant::now();
    driver.not_ready_since = Some(base);
    driver.last_full_attempt_at = Some(base);
    driver.apply_outcome(false); // one failed probe: breaker still closed

    driver.on_tick_at(base + secs(10)).await;

    assert_eq!(
        composition_fetches(&server).await,
        1,
        "the not-ready mapping is fetched on this tick"
    );
    assert!(
        driver.not_ready_since.is_none(),
        "the clips arrived, the episode is over"
    );
    assert_eq!(
        drain(&mut rx),
        1,
        "one tick fires one RecoveryEvent: the ready transition's"
    );
}

/// An earlier tick's RecoveryEvent does not cover a later step. A forced
/// RefreshMapping that ends the not-ready episode fires its own event.
#[tokio::test]
async fn a_forced_refresh_that_finds_the_clips_fires_the_ready_event() {
    let server = arena().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/composition"))
        .respond_with(ResponseTemplate::new(200).set_body_json(composition_with(&[SUBS_TOKEN])))
        .mount(&server)
        .await;
    let (tx, mut rx) = broadcast::channel(16);
    let mut driver =
        HostDriver::new("127.0.0.1".into(), server.address().port()).with_recovery_channel(tx);
    let base = Instant::now();
    for _ in 0..3 {
        driver.apply_outcome(false); // Arena goes away: the breaker opens
    }
    driver.apply_outcome(true); // an earlier tick's breaker close
    assert_eq!(drain(&mut rx), 1, "the earlier recovery fired its event");
    driver.not_ready_since = Some(base); // the evicted map, as `on_tick_at` marks it

    driver.handle_command(ResolumeCommand::RefreshMapping).await;

    assert_eq!(
        drain(&mut rx),
        1,
        "the forced refresh that finds the clips fires the ready event"
    );
}

/// Arena goes away during a not-ready episode that started long ago, and comes
/// back still loading. The breaker close starts a new episode, so the relaunch
/// gets a FRESH 120 s fast window: the next tick fetches again instead of
/// waiting out the 60 s retry window.
#[tokio::test]
async fn an_outage_gives_the_relaunch_a_fresh_fast_window() {
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
    driver.not_ready_since = Some(base);
    for _ in 0..3 {
        driver.apply_outcome(false);
    }

    let relaunch = base + secs(1000);
    driver.on_tick_at(relaunch).await;
    assert_eq!(
        driver.not_ready_since,
        Some(relaunch),
        "the still-loading composition starts a new episode at the relaunch"
    );
    driver.on_tick_at(relaunch + secs(10)).await;

    assert_eq!(
        composition_fetches(&server).await,
        2,
        "breaker-closed refresh + the next tick's NotReady refetch"
    );
    assert_eq!(
        drain(&mut rx),
        2,
        "the breaker close's event, then one when the clips arrive"
    );
}

/// Review round 2: Arena comes back less than 60 s after the last full-refresh
/// attempt, so the retry window holds the breaker-closed refresh back. The
/// breaker had evicted the clip map, so that map has none of SongPlayer's
/// clips: it is NOT READY from the breaker close, and the next tick fetches it
/// instead of treating the empty map as the steady state until the TTL.
#[tokio::test]
async fn a_breaker_close_held_back_by_the_retry_window_is_refetched_on_the_next_tick() {
    let server = arena().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/composition"))
        .respond_with(ResponseTemplate::new(200).set_body_json(composition_with(&[SUBS_TOKEN])))
        .mount(&server)
        .await;
    let (tx, mut rx) = broadcast::channel(16);
    let mut driver =
        HostDriver::new("127.0.0.1".into(), server.address().port()).with_recovery_channel(tx);
    let base = Instant::now();

    driver.on_tick_at(base).await; // startup refresh: mapped
    for _ in 0..3 {
        driver.apply_outcome(false); // Arena goes away: breaker opens, map evicted
    }

    driver.on_tick_at(base + secs(40)).await;
    assert_eq!(
        composition_fetches(&server).await,
        1,
        "40 s after the startup attempt the retry window holds the breaker-closed refresh"
    );
    assert_eq!(
        drain(&mut rx),
        1,
        "the breaker close fires its own RecoveryEvent"
    );
    assert_eq!(
        driver.not_ready_since,
        Some(base + secs(40)),
        "the evicted map is not ready from the breaker close"
    );

    driver.on_tick_at(base + secs(50)).await;
    assert_eq!(
        composition_fetches(&server).await,
        2,
        "the next tick fetches the evicted map, not the TTL refresh at 300 s"
    );
    assert!(
        driver.clip_mapping.contains_key(SUBS_TOKEN),
        "the clips are mapped again"
    );
    assert_eq!(
        drain(&mut rx),
        1,
        "the refresh that maps the clips fires one RecoveryEvent, so the wall is re-pushed"
    );
}

/// Review round 3: after a breaker close `/composition` keeps FAILING (Arena's
/// REST answers `/product` but chokes on the 14 MB composition while it
/// loads). Not ready means "answered without SongPlayer's clips"; a failed
/// fetch is the #157 case, so it waits out the 60 s retry window instead of
/// taking the every-tick fast path. 31 ticks, 10 s apart, from the relaunch:
/// fetches at 0, 60, 120, 180, 240, 300 s = 6, not 15. The only event is
/// the breaker close's: a failed fetch followed by an ok probe is a bare
/// failing→ok flip, no recovery (#217 addendum 2). Before, each failed fetch
/// made the next probe fire the #157 `was_failing` event, 6 in all.
#[tokio::test]
async fn a_failing_composition_after_a_breaker_close_keeps_the_retry_window() {
    let server = arena().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/composition"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let (tx, mut rx) = broadcast::channel(64);
    let mut driver =
        HostDriver::new("127.0.0.1".into(), server.address().port()).with_recovery_channel(tx);
    let base = Instant::now();
    driver.last_full_refresh_ok_at = Some(base);
    driver.last_full_attempt_at = Some(base);
    driver.consecutive_failures = 3;
    driver.circuit_breaker_open = true;

    let relaunch = base + secs(1800);
    for k in 0..=30u64 {
        driver.on_tick_at(relaunch + secs(10 * k)).await;
    }

    assert_eq!(
        composition_fetches(&server).await,
        6,
        "a failing /composition is fetched once per 60 s window (0, 60, ..., 300 s)"
    );
    assert_eq!(
        drain(&mut rx),
        1,
        "only the breaker close fires: a failed fetch then an ok probe is no recovery"
    );
}

/// Review round 4: the first `/composition` after a breaker close FAILS, the
/// next one ANSWERS without clips (Arena still loading), then the clips load.
/// The answered fetch must clear `last_full_attempt_failed`, so the fast path
/// comes back and the next tick maps the clips (R+70 s), instead of waiting
/// out another 60 s retry window (R+120 s).
#[tokio::test]
async fn an_answered_fetch_after_a_failed_one_restores_the_fast_path() {
    let server = arena().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/composition"))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/composition"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"layers": []})))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/composition"))
        .respond_with(ResponseTemplate::new(200).set_body_json(composition_with(&[SUBS_TOKEN])))
        .mount(&server)
        .await;
    let (tx, mut rx) = broadcast::channel(16);
    let mut driver =
        HostDriver::new("127.0.0.1".into(), server.address().port()).with_recovery_channel(tx);
    let base = Instant::now();
    driver.last_full_refresh_ok_at = Some(base);
    driver.last_full_attempt_at = Some(base);
    driver.consecutive_failures = 3;
    driver.circuit_breaker_open = true;
    let relaunch = base + secs(1800);

    // R: the breaker close fires its event; its refresh fails (500).
    // R+10: the probe answers again, no event (#217 addendum 2); the retry
    // window holds.
    let mut events = Vec::new();
    for k in 0..=6u64 {
        driver.on_tick_at(relaunch + secs(10 * k)).await;
        events.push(drain(&mut rx));
    }
    assert_eq!(
        composition_fetches(&server).await,
        2,
        "the failed fetch at R, then the retry-window fetch at R+60 that answers empty"
    );
    assert_eq!(
        events,
        [1, 0, 0, 0, 0, 0, 0],
        "RecoveryEvents per tick R..R+60: only the close, a failed fetch then an ok probe is none"
    );

    driver.on_tick_at(relaunch + secs(70)).await;
    assert_eq!(
        composition_fetches(&server).await,
        3,
        "the answered fetch restored the fast path: fetched again on the next tick"
    );
    assert!(
        driver.clip_mapping.contains_key(SUBS_TOKEN),
        "the clips are mapped at R+70 s"
    );
    assert_eq!(
        drain(&mut rx),
        1,
        "the refresh that maps the clips fires one RecoveryEvent"
    );
}
