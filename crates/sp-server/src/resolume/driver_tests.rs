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
