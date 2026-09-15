//! #157 poll-policy + retry-backoff tests — split from driver_tests.rs for the 1000-line cap.

use super::*;

// ---------------------------------------------------------------------------
// #157 — light /product liveness + demand-driven /composition refresh.
// ---------------------------------------------------------------------------

// -- FullRefreshReason::decide (pure poll policy) ---------------------------

// Signature: decide(now, last_full_ok, last_full_attempt, ttl, retry_after,
//                    forced, breaker_just_closed).
const T300: Duration = Duration::from_secs(300);
const T60: Duration = Duration::from_secs(60);

#[test]
fn decide_forced_is_command() {
    let now = Instant::now();
    assert_eq!(
        FullRefreshReason::decide(now, Some(now), None, T300, T60, true, false),
        Some(FullRefreshReason::Command),
        "a forced RefreshMapping must always yield a full refresh"
    );
}

#[test]
fn decide_breaker_closed_triggers_refresh() {
    let now = Instant::now();
    assert_eq!(
        FullRefreshReason::decide(now, Some(now), None, T300, T60, false, true),
        Some(FullRefreshReason::BreakerClosed),
        "a just-closed breaker resyncs the map even when the cache is fresh"
    );
}

#[test]
fn decide_never_refreshed_is_startup() {
    let now = Instant::now();
    assert_eq!(
        FullRefreshReason::decide(now, None, None, T300, T60, false, false),
        Some(FullRefreshReason::Startup),
        "no successful full refresh yet must trigger the first one"
    );
}

#[test]
fn decide_fresh_mapping_skips_refresh() {
    let base = Instant::now();
    assert_eq!(
        FullRefreshReason::decide(
            base + Duration::from_secs(10),
            Some(base),
            None,
            T300,
            T60,
            false,
            false,
        ),
        None,
        "the steady state (fresh cache, live REST) must only run the light probe"
    );
}

#[test]
fn decide_ttl_expiry_triggers_refresh() {
    let base = Instant::now();
    assert_eq!(
        FullRefreshReason::decide(
            base + Duration::from_secs(301),
            Some(base),
            None,
            T300,
            T60,
            false,
            false,
        ),
        Some(FullRefreshReason::Ttl),
        "a mapping older than the TTL must be refreshed"
    );
}

#[test]
fn decide_exactly_at_ttl_triggers_refresh() {
    // >= ttl semantics: exactly at the TTL boundary is stale enough to refresh.
    let base = Instant::now();
    assert_eq!(
        FullRefreshReason::decide(
            base + Duration::from_secs(300),
            Some(base),
            None,
            T300,
            T60,
            false,
            false,
        ),
        Some(FullRefreshReason::Ttl),
        "exactly at TTL must refresh (>=, not >)"
    );
}

#[test]
fn decide_command_wins_over_ttl_and_breaker() {
    let now = Instant::now();
    assert_eq!(
        FullRefreshReason::decide(now, None, None, T300, T60, true, true),
        Some(FullRefreshReason::Command),
        "a forced command takes precedence over every other reason"
    );
}

// -- retry backoff: never re-attempt a full refresh within retry_after of the
//    last attempt (success OR failure) — the saturated-Arena guard (#157 review).

#[test]
fn decide_startup_retry_blocked_inside_window() {
    let base = Instant::now();
    // A full refresh was attempted at `base` (and failed — last_full_ok None).
    assert_eq!(
        FullRefreshReason::decide(
            base + Duration::from_secs(30),
            None,
            Some(base),
            T300,
            T60,
            false,
            false,
        ),
        None,
        "a failed startup refresh must not be retried within the 60 s window"
    );
}

#[test]
fn decide_startup_retry_allowed_after_window() {
    let base = Instant::now();
    assert_eq!(
        FullRefreshReason::decide(
            base + Duration::from_secs(61),
            None,
            Some(base),
            T300,
            T60,
            false,
            false,
        ),
        Some(FullRefreshReason::Startup),
        "after the retry window a failed startup refresh may be retried"
    );
}

#[test]
fn decide_retry_exactly_at_window_allowed() {
    // At exactly retry_after the window has elapsed (`<`, not `<=`) — kills the
    // `< -> <=` boundary mutant.
    let base = Instant::now();
    assert_eq!(
        FullRefreshReason::decide(
            base + Duration::from_secs(60),
            None,
            Some(base),
            T300,
            T60,
            false,
            false,
        ),
        Some(FullRefreshReason::Startup),
        "exactly at the retry window the refresh is allowed (<, not <=)"
    );
}

#[test]
fn decide_ttl_retry_blocked_inside_window() {
    let base = Instant::now();
    // TTL has expired (last_full_ok at `base`, now 400 s later) but a full
    // refresh was attempted 10 s ago — back off.
    assert_eq!(
        FullRefreshReason::decide(
            base + Duration::from_secs(400),
            Some(base),
            Some(base + Duration::from_secs(390)),
            T300,
            T60,
            false,
            false,
        ),
        None,
        "an expired-TTL refresh still backs off within the retry window"
    );
}

#[test]
fn decide_ttl_retry_allowed_after_window() {
    let base = Instant::now();
    assert_eq!(
        FullRefreshReason::decide(
            base + Duration::from_secs(400),
            Some(base),
            Some(base + Duration::from_secs(330)),
            T300,
            T60,
            false,
            false,
        ),
        Some(FullRefreshReason::Ttl),
        "an expired-TTL refresh proceeds once the retry window has elapsed"
    );
}

#[test]
fn decide_command_immediate_despite_recent_attempt() {
    let base = Instant::now();
    // A forced command bypasses the retry window entirely.
    assert_eq!(
        FullRefreshReason::decide(
            base + Duration::from_secs(5),
            None,
            Some(base),
            T300,
            T60,
            true,
            false,
        ),
        Some(FullRefreshReason::Command),
        "a forced command refreshes immediately even inside the retry window"
    );
}

// -- apply_outcome (shared failure/breaker/recovery bookkeeping) ------------

#[test]
fn apply_outcome_returns_breaker_just_closed() {
    let mut driver = HostDriver::new("127.0.0.1".to_string(), 8090);
    // Simulate an open breaker after prior failures.
    driver.circuit_breaker_open = true;
    driver.consecutive_failures = 5;

    let closed = driver.apply_outcome(true);
    assert!(
        closed,
        "a success while the breaker is open must report breaker_just_closed"
    );
    assert!(
        !driver.circuit_breaker_open,
        "the breaker must be closed now"
    );
    assert_eq!(
        driver.consecutive_failures, 0,
        "failures must reset on success"
    );

    // A subsequent success does not re-close an already-closed breaker.
    let closed_again = driver.apply_outcome(true);
    assert!(
        !closed_again,
        "an already-closed breaker must not report breaker_just_closed again"
    );
}

// -- probe_liveness (light /product) ----------------------------------------

#[tokio::test]
async fn liveness_success_records_latency() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/product"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"name": "Arena"})),
        )
        .mount(&server)
        .await;
    let port = server.address().port();
    let mut driver = HostDriver::new("127.0.0.1".into(), port);

    let closed = driver.probe_liveness().await;
    assert!(
        !closed,
        "a clean first probe must not report a breaker close"
    );
    assert!(
        driver.last_refresh_ok,
        "a successful probe marks the host live"
    );
    assert_eq!(driver.consecutive_failures, 0);
    assert!(
        driver.product_latency_ms.is_some(),
        "a successful /product probe records its round-trip latency"
    );
}

#[tokio::test]
async fn probe_liveness_reports_breaker_close() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/product"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"name": "Arena"})),
        )
        .mount(&server)
        .await;
    let port = server.address().port();
    let mut driver = HostDriver::new("127.0.0.1".into(), port);
    // Simulate a previously-open breaker.
    driver.circuit_breaker_open = true;
    driver.consecutive_failures = 4;

    let closed = driver.probe_liveness().await;
    assert!(
        closed,
        "a successful /product probe while the breaker is open must report the close (reason d)"
    );
    assert!(!driver.circuit_breaker_open);
}

#[tokio::test]
async fn product_failure_increments_and_third_opens_breaker() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/product"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let port = server.address().port();
    let mut driver = HostDriver::new("127.0.0.1".into(), port);

    driver.probe_liveness().await;
    assert_eq!(driver.consecutive_failures, 1);
    assert!(!driver.circuit_breaker_open);
    assert!(
        driver.product_latency_ms.is_none(),
        "a failed probe clears the latency reading"
    );

    driver.probe_liveness().await;
    assert_eq!(driver.consecutive_failures, 2);
    assert!(!driver.circuit_breaker_open);

    driver.probe_liveness().await;
    assert_eq!(driver.consecutive_failures, 3);
    assert!(
        driver.circuit_breaker_open,
        "a 3rd consecutive /product failure opens the breaker"
    );
}

// -- on_tick_at (liveness + demand-driven full refresh) ---------------------

#[tokio::test]
async fn steady_state_polls_product_not_composition() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/product"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"name": "Arena"})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/composition"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"layers": []})))
        .mount(&server)
        .await;
    let port = server.address().port();
    let mut driver = HostDriver::new("127.0.0.1".into(), port);

    // Startup does exactly one full refresh; steady ticks only probe /product.
    driver
        .run_full_refresh(FullRefreshReason::Startup, Instant::now())
        .await;
    for _ in 0..5 {
        driver.on_tick_at(Instant::now()).await;
    }

    let reqs = server.received_requests().await.unwrap();
    let product = reqs
        .iter()
        .filter(|r| r.url.path() == "/api/v1/product")
        .count();
    let composition = reqs
        .iter()
        .filter(|r| r.url.path() == "/api/v1/composition")
        .count();
    assert_eq!(product, 5, "one /product probe per steady tick");
    assert_eq!(
        composition, 1,
        "only the startup /composition — never one per steady tick (the #157 fix)"
    );
}

#[tokio::test]
async fn ttl_expiry_triggers_one_composition_refresh() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/product"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"name": "Arena"})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/composition"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"layers": []})))
        .mount(&server)
        .await;
    let port = server.address().port();
    let mut driver = HostDriver::new("127.0.0.1".into(), port);

    driver
        .run_full_refresh(FullRefreshReason::Startup, Instant::now())
        .await;
    let t0 = driver
        .last_full_refresh_ok_at
        .expect("startup refresh must record its monotonic timestamp");

    // A tick well within the TTL: only the light probe, no full refresh.
    driver.on_tick_at(t0 + Duration::from_secs(10)).await;
    // A tick just past the TTL: exactly one more /composition.
    driver
        .on_tick_at(t0 + FULL_REFRESH_TTL + Duration::from_secs(1))
        .await;

    let reqs = server.received_requests().await.unwrap();
    let composition = reqs
        .iter()
        .filter(|r| r.url.path() == "/api/v1/composition")
        .count();
    let product = reqs
        .iter()
        .filter(|r| r.url.path() == "/api/v1/product")
        .count();
    assert_eq!(composition, 2, "startup + exactly one TTL-driven refresh");
    assert_eq!(product, 2, "two ticks → two liveness probes");
}

#[tokio::test]
async fn dead_rest_tick_skips_composition() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/product"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/composition"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"layers": []})))
        .mount(&server)
        .await;
    let port = server.address().port();
    let mut driver = HostDriver::new("127.0.0.1".into(), port);

    driver.on_tick_at(Instant::now()).await;

    let reqs = server.received_requests().await.unwrap();
    let composition = reqs
        .iter()
        .filter(|r| r.url.path() == "/api/v1/composition")
        .count();
    let product = reqs
        .iter()
        .filter(|r| r.url.path() == "/api/v1/product")
        .count();
    assert_eq!(
        composition, 0,
        "a failed /product probe must never pile the heavy /composition onto a dead REST"
    );
    assert_eq!(product, 1, "the tick still runs its liveness probe");
}

#[tokio::test]
async fn refresh_mapping_command_forces_composition() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/product"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"name": "Arena"})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/composition"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"layers": []})))
        .mount(&server)
        .await;
    let port = server.address().port();
    let mut driver = HostDriver::new("127.0.0.1".into(), port);
    // Pretend the cache is fresh so a plain tick would NOT refresh.
    driver.last_full_refresh_ok_at = Some(Instant::now());

    driver.handle_command(ResolumeCommand::RefreshMapping).await;

    let reqs = server.received_requests().await.unwrap();
    let composition = reqs
        .iter()
        .filter(|r| r.url.path() == "/api/v1/composition")
        .count();
    assert_eq!(
        composition, 1,
        "RefreshMapping forces exactly one /composition regardless of TTL freshness"
    );
}

// -- health snapshot carries the new #157 fields ----------------------------

#[tokio::test]
async fn health_snapshot_carries_new_fields() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/product"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"name": "Arena"})),
        )
        .mount(&server)
        .await;
    let composition = serde_json::json!({
        "layers": [{
            "clips": [{
                "id": 1,
                "name": { "value": "#sp-title" },
                "video": { "sourceparams": { "Text": { "id": 9, "valuetype": "ParamText" } } }
            }]
        }]
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/composition"))
        .respond_with(ResponseTemplate::new(200).set_body_json(composition))
        .mount(&server)
        .await;
    let port = server.address().port();

    let initial = crate::resolume::HostHealthSnapshot {
        host: "127.0.0.1".to_string(),
        last_refresh_ts: None,
        last_refresh_ok: false,
        consecutive_failures: 0,
        circuit_breaker_open: false,
        product_latency_ms: None,
        last_full_refresh_ts: None,
        clips_by_token: std::collections::BTreeMap::new(),
    };
    let (tx, rx) = tokio::sync::watch::channel(initial);
    let mut driver = HostDriver::new("127.0.0.1".into(), port).with_health_channel(tx);

    driver.probe_liveness().await;
    driver
        .run_full_refresh(FullRefreshReason::Startup, Instant::now())
        .await;

    let snap = rx.borrow().clone();
    assert!(snap.last_refresh_ok);
    assert!(
        snap.product_latency_ms.is_some(),
        "snapshot must carry the /product round-trip latency"
    );
    assert!(
        snap.last_full_refresh_ts.is_some(),
        "snapshot must carry the last successful full-refresh timestamp"
    );
    assert_eq!(
        snap.clips_by_token
            .get(crate::resolume::TITLE_TOKEN)
            .copied(),
        Some(1),
        "the mapped #sp-title clip must be counted"
    );
    assert_eq!(
        snap.clips_by_token
            .get(crate::resolume::SUBS_TOKEN)
            .copied(),
        Some(0),
        "an unmapped token must report zero clips"
    );
}

// -- retry backoff throttles a failing /composition (the saturated-Arena bug) --

#[tokio::test]
async fn full_refresh_retry_backoff_throttles_composition() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // Liveness answers, but /composition keeps failing (500) — the saturated
    // Arena state this lane exists for. The heavy fetch must be throttled to
    // once per retry window, NOT retried on every ~10 s tick (#157 review).
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/product"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"name": "Arena"})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/composition"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let port = server.address().port();
    let mut driver = HostDriver::new("127.0.0.1".into(), port);

    let base = Instant::now();
    // Tick at t: first attempt (never refreshed → Startup) → /composition #1 (fails).
    driver.on_tick_at(base).await;
    // Ticks at t+10s and t+20s: inside the 60 s retry window → suppressed.
    driver.on_tick_at(base + Duration::from_secs(10)).await;
    driver.on_tick_at(base + Duration::from_secs(20)).await;
    // Tick at t+61s: window elapsed → a second /composition attempt.
    driver.on_tick_at(base + Duration::from_secs(61)).await;

    let reqs = server.received_requests().await.unwrap();
    let composition = reqs
        .iter()
        .filter(|r| r.url.path() == "/api/v1/composition")
        .count();
    let product = reqs
        .iter()
        .filter(|r| r.url.path() == "/api/v1/product")
        .count();
    assert_eq!(
        composition, 2,
        "a failing /composition is retried once per 60 s window, not every tick (#157)"
    );
    assert_eq!(product, 4, "every tick still runs its light /product probe");
}
