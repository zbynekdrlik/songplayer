//! Unit tests for HostDriver — extracted via #[path] to keep driver.rs under the 1000-line file-size cap.

use super::*;

fn sample_composition() -> serde_json::Value {
    serde_json::json!({
        "layers": [
            {
                "clips": [
                    {
                        "id": 100,
                        "name": { "value": "Title #song-name-a" },
                        "video": {
                            "sourceparams": {
                                "Text": { "id": 200, "valuetype": "ParamText" }
                            }
                        }
                    },
                    {
                        "id": 101,
                        "name": { "value": "Title #song-name-b" },
                        "video": {
                            "sourceparams": {
                                "Text": { "id": 201, "valuetype": "ParamText" }
                            }
                        }
                    },
                    {
                        "id": 102,
                        "name": { "value": "Artist #artist-name-a" },
                        "video": {
                            "sourceparams": {
                                "Text": { "id": 202, "valuetype": "ParamText" }
                            }
                        }
                    },
                    {
                        "id": 103,
                        "name": { "value": "Artist #artist-name-b" },
                        "video": {
                            "sourceparams": {
                                "Text": { "id": 203, "valuetype": "ParamText" }
                            }
                        }
                    },
                    {
                        "id": 104,
                        "name": { "value": "Clear #song-clear" },
                        "video": {
                            "sourceparams": {
                                "Text": { "id": 204, "valuetype": "ParamText" }
                            }
                        }
                    }
                ]
            }
        ]
    })
}

#[test]
fn clip_discovery_parses_tokens() {
    let comp = sample_composition();
    let mapping = parse_composition(&comp);

    assert_eq!(mapping.len(), 5);

    let song_a = &mapping["#song-name-a"];
    assert_eq!(song_a.len(), 1);
    assert_eq!(song_a[0].clip_id, 100);
    assert_eq!(song_a[0].text_param_id, 200);

    let song_b = &mapping["#song-name-b"];
    assert_eq!(song_b.len(), 1);
    assert_eq!(song_b[0].clip_id, 101);
    assert_eq!(song_b[0].text_param_id, 201);

    let artist_a = &mapping["#artist-name-a"];
    assert_eq!(artist_a.len(), 1);
    assert_eq!(artist_a[0].clip_id, 102);
    assert_eq!(artist_a[0].text_param_id, 202);

    let artist_b = &mapping["#artist-name-b"];
    assert_eq!(artist_b.len(), 1);
    assert_eq!(artist_b[0].clip_id, 103);
    assert_eq!(artist_b[0].text_param_id, 203);

    let clear = &mapping["#song-clear"];
    assert_eq!(clear.len(), 1);
    assert_eq!(clear[0].clip_id, 104);
    assert_eq!(clear[0].text_param_id, 204);
}

#[test]
fn clip_discovery_ignores_clips_without_tokens() {
    let comp = serde_json::json!({
        "layers": [{
            "clips": [{
                "id": 1,
                "name": { "value": "No tokens here" },
                "video": {
                    "sourceparams": {
                        "Text": { "id": 10, "valuetype": "ParamText" }
                    }
                }
            }]
        }]
    });

    let mapping = parse_composition(&comp);
    assert!(mapping.is_empty());
}

#[test]
fn clip_discovery_ignores_clips_without_text_param() {
    let comp = serde_json::json!({
        "layers": [{
            "clips": [{
                "id": 1,
                "name": { "value": "Has #token" },
                "video": {
                    "sourceparams": {}
                }
            }]
        }]
    });

    let mapping = parse_composition(&comp);
    assert!(mapping.is_empty());
}

#[test]
fn clip_discovery_handles_multiple_tokens_per_clip() {
    let comp = serde_json::json!({
        "layers": [{
            "clips": [{
                "id": 50,
                "name": { "value": "Multi #tag-one #tag-two" },
                "video": {
                    "sourceparams": {
                        "Text": { "id": 500, "valuetype": "ParamText" }
                    }
                }
            }]
        }]
    });

    let mapping = parse_composition(&comp);
    assert_eq!(mapping.len(), 2);
    assert_eq!(mapping["#tag-one"][0].clip_id, 50);
    assert_eq!(mapping["#tag-two"][0].clip_id, 50);
}

#[test]
fn clip_discovery_empty_composition() {
    let comp = serde_json::json!({});
    let mapping = parse_composition(&comp);
    assert!(mapping.is_empty());

    let comp2 = serde_json::json!({ "layers": [] });
    let mapping2 = parse_composition(&comp2);
    assert!(mapping2.is_empty());
}

#[test]
fn parse_composition_collects_multiple_clips_per_token() {
    let comp = serde_json::json!({
        "layers": [
            {
                "clips": [
                    {
                        "id": 100,
                        "name": { "value": "Title A #sp-title" },
                        "video": { "sourceparams": { "Text": { "id": 200, "valuetype": "ParamText" } } }
                    },
                    {
                        "id": 101,
                        "name": { "value": "Title B #sp-title" },
                        "video": { "sourceparams": { "Text": { "id": 201, "valuetype": "ParamText" } } }
                    }
                ]
            },
            {
                "clips": [
                    {
                        "id": 102,
                        "name": { "value": "Other Layer #sp-title" },
                        "video": { "sourceparams": { "Text": { "id": 202, "valuetype": "ParamText" } } }
                    }
                ]
            }
        ]
    });

    let mapping = parse_composition(&comp);
    let clips = mapping.get("#sp-title").expect("must have #sp-title entry");
    assert_eq!(clips.len(), 3, "expected 3 clips, got: {clips:?}");

    let ids: Vec<i64> = clips.iter().map(|c| c.clip_id).collect();
    assert!(ids.contains(&100));
    assert!(ids.contains(&101));
    assert!(ids.contains(&102));
}

#[test]
fn clip_discovery_uses_param_text_valuetype() {
    let comp = serde_json::json!({
        "layers": [{
            "clips": [{
                "id": 1683810383769_i64,
                "name": { "value": "#sp-title" },
                "video": {
                    "sourceparams": {
                        "Text": {
                            "id": 1775761488634_i64,
                            "valuetype": "ParamText",
                            "value": "Hello"
                        }
                    }
                }
            }]
        }]
    });
    let mapping = parse_composition(&comp);
    assert_eq!(mapping.len(), 1);
    let clips = &mapping["#sp-title"];
    assert_eq!(clips.len(), 1);
    assert_eq!(clips[0].clip_id, 1683810383769);
    assert_eq!(clips[0].text_param_id, 1775761488634);
}

#[test]
fn host_driver_new() {
    let driver = HostDriver::new("192.168.1.10".to_string(), 8080);
    assert!(driver.clip_mapping.is_empty());
    assert!(driver.endpoint_cache.is_none());
}

#[test]
fn is_ip_literal_detects_ipv4() {
    assert!(is_ip_literal("192.168.1.10"));
    assert!(is_ip_literal("127.0.0.1"));
    assert!(is_ip_literal("10.77.9.201"));
    assert!(!is_ip_literal("resolume.lan"));
    assert!(!is_ip_literal("my-host.local"));
}

#[test]
fn resolved_endpoint_ip_literal_no_host_header() {
    let ep = ResolvedEndpoint::from_ip("192.168.1.10", 8090);
    assert_eq!(ep.base_url, "http://192.168.1.10:8090");
    assert!(ep.host_header.is_none());
}

#[test]
fn resolved_endpoint_hostname_has_host_header() {
    let ep = ResolvedEndpoint::from_resolved("10.77.9.201", "resolume.lan", 8090);
    assert_eq!(ep.base_url, "http://10.77.9.201:8090");
    assert_eq!(ep.host_header.as_deref(), Some("resolume.lan:8090"));
}

#[test]
fn resolved_endpoint_expiry() {
    let ep = ResolvedEndpoint::from_ip("192.168.1.10", 8090);
    // Freshly created endpoint should not be expired.
    assert!(!ep.is_expired());
}

/// Verify TTL expiry using a synthetic future `now`. This avoids the
/// Windows `Instant::now() - Duration` underflow problem by going
/// forward in time rather than backward.
#[test]
fn resolved_endpoint_expires_after_ttl() {
    let ep = ResolvedEndpoint::from_ip("192.168.1.10", 8090);
    let future = ep.resolved_at + Duration::from_secs(301);
    assert!(
        ep.is_expired_at(future),
        "endpoint aged 301s should be expired (TTL=300s)"
    );
}

/// Boundary test: just under TTL is NOT expired.
#[test]
fn resolved_endpoint_just_under_ttl_is_not_expired() {
    let ep = ResolvedEndpoint::from_ip("192.168.1.10", 8090);
    let future = ep.resolved_at + (RESOLUTION_TTL - Duration::from_millis(1));
    assert!(
        !ep.is_expired_at(future),
        "endpoint just under TTL should not be expired"
    );
}

/// Boundary test: exactly at TTL is NOT expired (strict-greater semantics).
/// This is the only test that distinguishes `>` from `>=` in is_expired_at —
/// the `is_expired_at(now)` refactor lets us construct the boundary exactly,
/// which `Instant::now()`-based tests could not.
#[test]
fn resolved_endpoint_at_exactly_ttl_is_not_expired() {
    let ep = ResolvedEndpoint::from_ip("192.168.1.10", 8090);
    let exact_ttl = ep.resolved_at + RESOLUTION_TTL;
    assert!(
        !ep.is_expired_at(exact_ttl),
        "endpoint at exactly TTL boundary must NOT be expired (>, not >=)"
    );
}

/// Boundary test: just over TTL IS expired.
#[test]
fn resolved_endpoint_just_over_ttl_is_expired() {
    let ep = ResolvedEndpoint::from_ip("192.168.1.10", 8090);
    let future = ep.resolved_at + (RESOLUTION_TTL + Duration::from_millis(1));
    assert!(
        ep.is_expired_at(future),
        "endpoint just over TTL should be expired"
    );
}

#[tokio::test]
async fn cached_endpoint_returns_none_until_ensure_endpoint_called() {
    let driver = HostDriver::new("127.0.0.1".to_string(), 1);
    assert!(
        driver.cached_endpoint().is_none(),
        "no endpoint cached before ensure_endpoint"
    );
}

#[tokio::test]
async fn ensure_endpoint_populates_cache_for_ip_literal() {
    let mut driver = HostDriver::new("127.0.0.1".to_string(), 8090);
    driver.ensure_endpoint().await.unwrap();
    let cached = driver.cached_endpoint().expect("endpoint should be cached");
    assert_eq!(cached.base_url, "http://127.0.0.1:8090");
    assert!(
        cached.host_header.is_none(),
        "IP literal should not need a Host header override"
    );
}

/// Wiremock test that exercises `refresh_mapping` against a real HTTP
/// server returning a composition. Kills the `Ok(())` mutant by asserting
/// that the mapping was actually populated from the response.
#[tokio::test]
async fn refresh_mapping_populates_clip_mapping_from_composition() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let composition = serde_json::json!({
        "layers": [{
            "clips": [{
                "id": 555,
                "name": { "value": "#sp-title" },
                "video": {
                    "sourceparams": {
                        "Text": { "id": 999, "valuetype": "ParamText" }
                    }
                }
            }]
        }]
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/composition"))
        .respond_with(ResponseTemplate::new(200).set_body_json(composition))
        .mount(&server)
        .await;

    let url = server.uri();
    let stripped = url.trim_start_matches("http://");
    let parts: Vec<&str> = stripped.split(':').collect();
    let host = parts[0].to_string();
    let port: u16 = parts[1].parse().unwrap();

    let mut driver = HostDriver::new(host, port);
    assert!(driver.clip_mapping.is_empty());

    driver.refresh_mapping().await.unwrap();

    let clips = driver
        .clip_mapping
        .get("#sp-title")
        .expect("#sp-title should be populated");
    assert_eq!(clips.len(), 1);
    assert_eq!(clips[0].clip_id, 555);
    assert_eq!(clips[0].text_param_id, 999);
}

/// Verify the cache is reused when not expired (kills the `delete !` mutant
/// at the `if !cached.is_expired()` check).
#[tokio::test]
async fn endpoint_returns_cached_value_on_subsequent_calls() {
    let mut driver = HostDriver::new("127.0.0.1".to_string(), 8090);
    let ep1 = driver.endpoint().await.unwrap();
    let ep2 = driver.endpoint().await.unwrap();
    // Same resolved_at means we got the cached value, not a fresh resolve.
    assert_eq!(ep1.resolved_at, ep2.resolved_at);
    assert_eq!(ep1.base_url, ep2.base_url);
}

#[tokio::test]
async fn recovery_event_fires_on_success_after_failure() {
    let server = wiremock::MockServer::start().await;
    // First request fails, subsequent succeed
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(wiremock::ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({"layers": []})),
        )
        .mount(&server)
        .await;
    let port = server.address().port();

    let (tx, mut rx) = tokio::sync::broadcast::channel(8);
    let mut driver = HostDriver::new("127.0.0.1".into(), port).with_recovery_channel(tx);

    let _ = driver.refresh_mapping().await; // fails
    let _ = driver.refresh_mapping().await; // succeeds → RecoveryEvent

    let event = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
        .await
        .expect("RecoveryEvent should arrive")
        .expect("channel open");
    assert_eq!(event.host, "127.0.0.1");
}

#[tokio::test]
async fn no_recovery_event_on_clean_first_success() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({"layers": []})),
        )
        .mount(&server)
        .await;
    let port = server.address().port();
    let (tx, mut rx) = tokio::sync::broadcast::channel(8);
    let mut driver = HostDriver::new("127.0.0.1".into(), port).with_recovery_channel(tx);

    let _ = driver.refresh_mapping().await;

    let result = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await;
    assert!(
        result.is_err(),
        "no event should fire on clean first success"
    );
}

#[tokio::test]
async fn circuit_breaker_evicts_clip_map_after_threshold_failures() {
    // wiremock server that returns 503 every time
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(wiremock::ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let port = server.address().port();
    let mut driver = HostDriver::new("127.0.0.1".into(), port);
    // Pretend the cache has clips from a prior successful refresh
    driver.clip_mapping.insert("#sp-title".into(), vec![]);

    // Three consecutive failures should trip the breaker and evict
    for _ in 0..3 {
        let _ = driver.refresh_mapping().await;
    }

    assert!(driver.circuit_breaker_open, "circuit should be open");
    assert!(driver.clip_mapping.is_empty(), "cache should be evicted");
}

#[tokio::test]
async fn single_failure_does_not_trip_circuit() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(wiremock::ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    // Subsequent requests should 404 by default; we only test the first failure
    let port = server.address().port();
    let mut driver = HostDriver::new("127.0.0.1".into(), port);

    let _ = driver.refresh_mapping().await;

    assert_eq!(driver.consecutive_failures, 1);
    assert!(!driver.circuit_breaker_open);
}

// ---------------------------------------------------------------------------
// #157 — light /product liveness + demand-driven /composition refresh.
// ---------------------------------------------------------------------------

// -- FullRefreshReason::decide (pure poll policy) ---------------------------

#[test]
fn decide_forced_is_command() {
    let now = Instant::now();
    assert_eq!(
        FullRefreshReason::decide(now, Some(now), Duration::from_secs(300), true, false),
        Some(FullRefreshReason::Command),
        "a forced RefreshMapping must always yield a full refresh"
    );
}

#[test]
fn decide_breaker_closed_triggers_refresh() {
    let now = Instant::now();
    assert_eq!(
        FullRefreshReason::decide(now, Some(now), Duration::from_secs(300), false, true),
        Some(FullRefreshReason::BreakerClosed),
        "a just-closed breaker resyncs the map even when the cache is fresh"
    );
}

#[test]
fn decide_never_refreshed_is_startup() {
    let now = Instant::now();
    assert_eq!(
        FullRefreshReason::decide(now, None, Duration::from_secs(300), false, false),
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
            Duration::from_secs(300),
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
            Duration::from_secs(300),
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
            Duration::from_secs(300),
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
        FullRefreshReason::decide(now, None, Duration::from_secs(300), true, true),
        Some(FullRefreshReason::Command),
        "a forced command takes precedence over every other reason"
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
    assert!(!driver.circuit_breaker_open, "the breaker must be closed now");
    assert_eq!(driver.consecutive_failures, 0, "failures must reset on success");

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
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"name": "Arena"})))
        .mount(&server)
        .await;
    let port = server.address().port();
    let mut driver = HostDriver::new("127.0.0.1".into(), port);

    let closed = driver.probe_liveness().await;
    assert!(!closed, "a clean first probe must not report a breaker close");
    assert!(driver.last_refresh_ok, "a successful probe marks the host live");
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
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"name": "Arena"})))
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
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"name": "Arena"})))
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
    driver.run_full_refresh(FullRefreshReason::Startup).await;
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
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"name": "Arena"})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/composition"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"layers": []})))
        .mount(&server)
        .await;
    let port = server.address().port();
    let mut driver = HostDriver::new("127.0.0.1".into(), port);

    driver.run_full_refresh(FullRefreshReason::Startup).await;
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
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"name": "Arena"})))
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
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"name": "Arena"})))
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
    driver.run_full_refresh(FullRefreshReason::Startup).await;

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
        snap.clips_by_token.get(crate::resolume::TITLE_TOKEN).copied(),
        Some(1),
        "the mapped #sp-title clip must be counted"
    );
    assert_eq!(
        snap.clips_by_token.get(crate::resolume::SUBS_TOKEN).copied(),
        Some(0),
        "an unmapped token must report zero clips"
    );
}
