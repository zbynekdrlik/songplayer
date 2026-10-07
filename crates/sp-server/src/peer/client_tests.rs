//! #229 `peer::client`: wiremock stands in for Cloudflare Access + a peer;
//! the rig for a real node.

use std::time::{Duration, Instant};

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::peer::config::PeerConfig;
use crate::peer::kind::ArtifactKind;
use crate::peer::rig::{SNV_KEY, TestNode};

/// The test service token (never a real one).
const CF_ID: &str = "pp-client-example.access";
const CF_SECRET: &str = "pp-cf-secret-example";

/// A sha256 as 64 hex digits (built: the staging hook refuses a long hex
/// literal).
fn sha() -> String {
    "0123456789abcdef".repeat(4)
}

fn peer_at(base_url: &str, with_token: bool) -> PeerConfig {
    PeerConfig {
        name: "snv".into(),
        base_url: base_url.into(),
        key: SNV_KEY.into(),
        cf_client_id: with_token.then(|| CF_ID.to_string()),
        cf_client_secret: with_token.then(|| CF_SECRET.to_string()),
    }
}

fn catalog_body() -> String {
    let sha = sha();
    format!(
        r#"{{"node":"snv","artifacts":[
            {{"youtube_id":"aaaaaaaaaaa","kind":"audio","version":1,"size":3000,"sha256":"{sha}"}},
            {{"youtube_id":"aaaaaaaaaaa","kind":"dub","version":1,"size":9,"sha256":"{sha}"}}],
          "jobs":[]}}"#
    )
}

async fn serves_catalog(server: &MockServer, body: String, times: u64) {
    Mock::given(method("GET"))
        .and(path("/api/v1/peer/catalog"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .expect(times)
        .mount(server)
        .await;
}

#[test]
fn every_status_means_its_error() {
    assert_eq!(status_error(200), None);
    assert_eq!(status_error(206), None);
    for s in [300, 301, 302, 307, 399, 403] {
        assert_eq!(status_error(s), Some(PeerError::AccessRefused(s)), "{s}");
    }
    assert_eq!(status_error(401), Some(PeerError::KeyRefused));
    assert_eq!(status_error(404), Some(PeerError::NotFound));
    assert_eq!(status_error(503), Some(PeerError::Paused));
    for s in [204, 299, 400, 402, 416, 500, 502] {
        let bad = PeerError::BadResponse(format!("HTTP {s}"));
        assert_eq!(status_error(s), Some(bad), "{s}");
    }
}

#[test]
fn a_cached_catalog_is_fresh_for_the_ttl_only() {
    let t = Instant::now();
    assert!(fresh(t, t));
    assert!(fresh(t, t + CATALOG_TTL - Duration::from_nanos(1)));
    assert!(!fresh(t, t + CATALOG_TTL));
    assert_eq!(CATALOG_TTL, Duration::from_secs(60));
}

/// Review Focus 1: Access refuses a bad / missing service token with a 302 to
/// its login page. It is a refusal, never followed, never parsed.
#[tokio::test]
async fn a_cloudflare_login_redirect_is_access_refused_and_never_followed() {
    let server = MockServer::start().await;
    let login = format!("{}/cdn-cgi/access/login", server.uri());
    Mock::given(method("GET"))
        .and(path("/api/v1/peer/catalog"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", login.as_str()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/cdn-cgi/access/login"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>login</html>"))
        .expect(0)
        .mount(&server)
        .await;
    let client = PeerClient::new();
    let err = client
        .read_catalog(&peer_at(&server.uri(), true))
        .await
        .unwrap_err();
    assert_eq!(err, PeerError::AccessRefused(302));
    let read = &client.last_reads()["snv"];
    assert!(!read.ok);
    assert_eq!((read.artifacts, read.jobs), (0, 0));
    assert!(read.error.as_deref().unwrap().contains("Cloudflare Access"));
}

#[tokio::test]
async fn a_403_is_access_refused_and_a_401_the_key() {
    let cases: [(u16, PeerError); 2] = [
        (403, PeerError::AccessRefused(403)),
        (401, PeerError::KeyRefused),
    ];
    for (status, want) in cases {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(status))
            .mount(&server)
            .await;
        let peer = peer_at(&server.uri(), false);
        let err = PeerClient::new().read_catalog(&peer).await.unwrap_err();
        assert_eq!(err, want);
    }
}

#[tokio::test]
async fn the_key_and_the_service_token_travel_as_headers() {
    let server = MockServer::start().await;
    serves_catalog(&server, catalog_body(), 1).await;
    PeerClient::new()
        .read_catalog(&peer_at(&server.uri(), true))
        .await
        .unwrap();
    let req = &server.received_requests().await.unwrap()[0];
    let header = |name: &str| {
        req.headers
            .get(name)
            .map(|v| v.to_str().unwrap().to_string())
    };
    assert!(
        header("x-sp-peer-key").as_deref() == Some(SNV_KEY),
        "the key"
    );
    assert_eq!(header("cf-access-client-id").as_deref(), Some(CF_ID));
    assert!(
        header("cf-access-client-secret").as_deref() == Some(CF_SECRET),
        "the service token's secret"
    );
}

#[tokio::test]
async fn no_access_headers_without_a_token() {
    let server = MockServer::start().await;
    serves_catalog(&server, catalog_body(), 1).await;
    PeerClient::new()
        .read_catalog(&peer_at(&server.uri(), false))
        .await
        .unwrap();
    let req = &server.received_requests().await.unwrap()[0];
    assert!(req.headers.get("cf-access-client-id").is_none());
    assert!(req.headers.get("cf-access-client-secret").is_none());
    assert!(req.headers.get("x-sp-peer-key").is_some());
}

/// Review Focus 3, over HTTP.
#[tokio::test]
async fn a_newer_peers_catalog_keeps_what_this_node_knows() {
    let server = MockServer::start().await;
    serves_catalog(&server, catalog_body(), 1).await;
    let c = PeerClient::new()
        .read_catalog(&peer_at(&server.uri(), false))
        .await
        .unwrap();
    assert_eq!(c.artifacts.len(), 1);
    assert_eq!(c.artifacts[0].kind, ArtifactKind::Audio);
}

#[tokio::test]
async fn a_catalog_over_the_bound_is_refused_at_the_exact_byte() {
    let body = catalog_body();
    let server = MockServer::start().await;
    serves_catalog(&server, body.clone(), 2).await;
    let peer = peer_at(&server.uri(), false);
    let at_bound = PeerClient::with_max_catalog_bytes(body.len());
    assert!(at_bound.read_catalog(&peer).await.is_ok());
    let over = PeerClient::with_max_catalog_bytes(body.len() - 1);
    let err = over.read_catalog(&peer).await.unwrap_err();
    assert!(matches!(err, PeerError::BadResponse(_)), "{err:?}");
}

#[tokio::test]
async fn a_non_json_answer_is_a_bad_response_that_quotes_nothing() {
    let server = MockServer::start().await;
    serves_catalog(&server, "\"leaked-text\"".to_string(), 1).await;
    let err = PeerClient::new()
        .read_catalog(&peer_at(&server.uri(), false))
        .await
        .unwrap_err();
    let text = err.to_string();
    assert!(
        text.contains("line 1") && !text.contains("leaked-text"),
        "{text}"
    );
}

#[tokio::test]
async fn an_unreachable_peer_is_unreachable() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let err = PeerClient::new()
        .read_catalog(&peer_at(&base, false))
        .await
        .unwrap_err();
    assert!(matches!(err, PeerError::Unreachable(_)), "{err:?}");
    let text = err.to_string().to_lowercase();
    assert!(
        text.contains("connect"),
        "names the connect failure: {text}"
    );
    assert!(!text.contains("127.0.0.1"), "no URL: {text}");
}

#[tokio::test]
async fn the_catalog_is_read_once_per_ttl_until_forgotten() {
    let server = MockServer::start().await;
    serves_catalog(&server, catalog_body(), 2).await;
    let client = PeerClient::new();
    let peer = peer_at(&server.uri(), false);
    client.catalog(&peer).await.unwrap();
    client.catalog(&peer).await.unwrap();
    client.forget_catalog("snv");
    client.catalog(&peer).await.unwrap();
    // MockServer verifies `.expect(2)` when it drops.
}

#[tokio::test]
async fn a_failed_read_is_not_cached() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .expect(2)
        .mount(&server)
        .await;
    let client = PeerClient::new();
    let peer = peer_at(&server.uri(), false);
    assert!(client.catalog(&peer).await.is_err());
    assert!(client.catalog(&peer).await.is_err());
}

#[tokio::test]
async fn reads_a_real_nodes_catalog_and_video_row() {
    let snv = TestNode::start("snv", Some(SNV_KEY)).await;
    let id = snv.add_video("aaaaaaaaaaa").await;
    snv.give_song(id, "aaaaaaaaaaa", "Way Maker", "Sinach")
        .await;
    snv.hash_now().await;
    let client = PeerClient::new();
    let peer = snv.as_peer(SNV_KEY);
    let c = client.read_catalog(&peer).await.unwrap();
    assert_eq!(c.node, "snv");
    assert!(c.artifacts.iter().any(|a| a.kind == ArtifactKind::Video));
    let v = client.video(&peer, "aaaaaaaaaaa").await.unwrap();
    assert_eq!(v.metadata.song, "Way Maker");
    let read = &client.last_reads()["snv"];
    assert!(read.ok && read.error.is_none());
    assert_eq!(
        (read.artifacts, read.jobs),
        (c.artifacts.len(), c.jobs.len())
    );
    assert!(!read.at.is_empty());
    let missing = client.video(&peer, "bbbbbbbbbbb").await.unwrap_err();
    assert_eq!(missing, PeerError::NotFound);
}

#[tokio::test]
async fn a_bad_youtube_id_is_never_sent() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let err = PeerClient::new()
        .video(&peer_at(&server.uri(), false), "../x")
        .await
        .unwrap_err();
    assert!(matches!(err, PeerError::BadResponse(_)));
}

/// A peer's video row must be the one asked for (lanes 8-9 adopt from it).
#[tokio::test]
async fn a_video_row_of_another_video_is_refused() {
    let server = MockServer::start().await;
    let row = r#"{"metadata":{"youtube_id":"bbbbbbbbbbb","song":"S","artist":"A",
        "metadata_source":"gemini","gemini_failed":false},"duration_ms":null,"lyrics":null}"#;
    Mock::given(method("GET"))
        .and(path("/api/v1/peer/videos/aaaaaaaaaaa"))
        .respond_with(ResponseTemplate::new(200).set_body_string(row))
        .mount(&server)
        .await;
    let err = PeerClient::new()
        .video(&peer_at(&server.uri(), false), "aaaaaaaaaaa")
        .await
        .unwrap_err();
    assert!(matches!(err, PeerError::BadResponse(_)), "{err:?}");
}
