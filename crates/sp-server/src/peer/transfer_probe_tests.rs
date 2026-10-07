//! #229 `peer::transfer_probe`: the PP gate's real transfer, over two real
//! nodes, and its route through `peer::router`.
//! (Repo convention: names the parent only imports are imported explicitly.)

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use sp_core::config::{SETTING_NODE_NAME, SETTING_PEER_TRANSFERS_PAUSED};
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::peer::Exchange;
use crate::peer::config::PeerConfig;
use crate::peer::hash::sha256_hex;
use crate::peer::kind::ArtifactKind;
use crate::peer::rig::{OTHER_KEY, SNV_KEY, TestNode, set};
use crate::peer::wire::{Artifact, Catalog};

const YT: &str = "aaaaaaaaaaa";

fn art(kind: ArtifactKind, size: u64, sha: char) -> Artifact {
    Artifact {
        youtube_id: YT.into(),
        kind,
        version: 1,
        size,
        sha256: sha.to_string().repeat(64),
        updated_at: None,
    }
}

fn catalog_of(artifacts: Vec<Artifact>) -> Catalog {
    Catalog {
        node: "snv".into(),
        artifacts,
        jobs: vec![],
    }
}

/// The smallest FILE: never a metadata entry (answered from a row, not the
/// file path), never an empty file; the first listed among equals.
#[test]
fn the_probe_picks_the_smallest_non_empty_file() {
    let c = catalog_of(vec![
        art(ArtifactKind::Metadata, 10, 'a'),
        art(ArtifactKind::Video, 0, 'b'),
        art(ArtifactKind::Audio, 300, 'c'),
        art(ArtifactKind::Lyrics, 200, 'd'),
        art(ArtifactKind::StemVocals, 200, 'e'),
        art(ArtifactKind::StemInstrumental, 250, 'f'),
    ]);
    assert_eq!(pick(&c), Ok(&c.artifacts[3]));
}

#[test]
fn a_catalog_with_no_file_artifact_gives_no_probe() {
    let only_metadata = catalog_of(vec![art(ArtifactKind::Metadata, 10, 'a')]);
    assert_eq!(
        pick(&only_metadata),
        Err("the catalog lists no file artifact".to_string())
    );
    assert!(pick(&catalog_of(vec![])).is_err());
}

#[test]
fn the_probe_transfers_at_most_64_mib() {
    let at_cap = catalog_of(vec![art(ArtifactKind::Audio, PROBE_MAX_BYTES, 'a')]);
    assert_eq!(pick(&at_cap), Ok(&at_cap.artifacts[0]));
    let over = catalog_of(vec![art(ArtifactKind::Audio, PROBE_MAX_BYTES + 1, 'a')]);
    assert_eq!(
        pick(&over),
        Err(format!(
            "its smallest file artifact is {} bytes, over the probe's {PROBE_MAX_BYTES}",
            PROBE_MAX_BYTES + 1
        ))
    );
    assert_eq!(PROBE_MAX_BYTES, 64 * 1024 * 1024);
}

/// SNV serving a song with its lyrics, hashed (the lyrics track is its
/// smallest file); PP asking SNV. Returns SNV's lyrics bytes.
async fn snv_and_pp() -> (TestNode, TestNode, Vec<u8>) {
    let snv = TestNode::start("snv", Some(SNV_KEY)).await;
    let id = snv.add_video(YT).await;
    snv.give_song(id, YT, "Way Maker", "Sinach").await;
    let json = snv.give_lyrics(id, YT, "mtl+g35t").await;
    snv.hash_now().await;
    let pp = TestNode::start("pp", None).await;
    pp.set_peers(&[snv.as_peer(SNV_KEY)]).await;
    (snv, pp, json)
}

fn entries(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir).unwrap().count()
}

/// A real transfer of SNV's smallest file: whole, its sha256 read back here,
/// in a temp dir that is gone afterwards; nothing lands in PP's cache.
#[tokio::test]
async fn a_probe_transfers_the_peers_smallest_file_outside_the_cache() {
    let (snv, pp, json) = snv_and_pp().await;
    let tmp = tempfile::tempdir().unwrap();
    let probe = pp
        .ex
        .probe_transfer(&snv.as_peer(SNV_KEY), tmp.path(), PROBE_MAX_TIME)
        .await;
    assert!(probe.ok, "{probe:?}");
    assert_eq!(probe.error, None);
    assert_eq!(
        (probe.name.as_str(), probe.base_url.as_str()),
        ("snv", snv.base_url.as_str())
    );
    let a = probe.artifact.as_ref().unwrap();
    assert_eq!((a.youtube_id.as_str(), a.kind), (YT, ArtifactKind::Lyrics));
    assert_eq!(a.size, json.len() as u64);
    assert_eq!(probe.bytes, Some(json.len() as u64));
    assert_eq!(probe.sha256, Some(sha256_hex(&json)));
    assert_eq!(probe.sha256.as_ref(), Some(&a.sha256));
    assert_eq!(entries(tmp.path()), 0, "the probe's temp dir is removed");
    assert_eq!(entries(pp.cache()), 0, "nothing written into the cache");
}

/// The gate never waits behind the workers' transfers: the probe does not
/// take the peer's transfer slot (held here for the whole probe).
#[tokio::test]
async fn a_probe_does_not_wait_for_the_peers_transfer_slot() {
    let (snv, pp, json) = snv_and_pp().await;
    let slot = pp.ex.client.slot("snv");
    let _held = slot.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let probe = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        pp.ex
            .probe_transfer(&snv.as_peer(SNV_KEY), tmp.path(), PROBE_MAX_TIME),
    )
    .await
    .expect("the probe waited for the held slot");
    assert!(probe.ok, "{probe:?}");
    assert_eq!(probe.bytes, Some(json.len() as u64));
}

/// SNV's file changed after it was hashed: the catalog's sha no longer
/// matches the bytes, so the transfer fails loudly and leaves nothing.
#[tokio::test]
async fn a_transfer_that_does_not_match_the_catalog_fails() {
    let (snv, pp, json) = snv_and_pp().await;
    let mut other = json.clone();
    other[0] ^= 0xff;
    std::fs::write(snv.cache().join(format!("{YT}_lyrics.json")), &other).unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let probe = pp
        .ex
        .probe_transfer(&snv.as_peer(SNV_KEY), tmp.path(), PROBE_MAX_TIME)
        .await;
    assert!(!probe.ok);
    let error = probe.error.unwrap();
    assert!(error.contains("sha256 mismatch"), "{error}");
    assert_eq!(probe.artifact.map(|a| a.kind), Some(ArtifactKind::Lyrics));
    assert_eq!((probe.bytes, probe.sha256), (None, None));
    assert_eq!(entries(tmp.path()), 0);
}

#[tokio::test]
async fn a_probe_names_a_refused_key() {
    let (snv, pp, _) = snv_and_pp().await;
    let tmp = tempfile::tempdir().unwrap();
    let probe = pp
        .ex
        .probe_transfer(&snv.as_peer(OTHER_KEY), tmp.path(), PROBE_MAX_TIME)
        .await;
    assert!(!probe.ok);
    assert_eq!(probe.artifact, None);
    let error = probe.error.unwrap();
    assert!(error.starts_with("reading the catalog failed: "), "{error}");
    assert!(
        error.contains("peer key") && !error.contains(OTHER_KEY),
        "{error}"
    );
}

/// This node's own pause: no transfer, not even a catalog read.
#[tokio::test]
async fn a_probe_waits_for_nothing_while_this_nodes_transfers_are_paused() {
    let (snv, pp, _) = snv_and_pp().await;
    set(pp.pool(), SETTING_PEER_TRANSFERS_PAUSED, "true").await;
    let tmp = tempfile::tempdir().unwrap();
    let probe = pp
        .ex
        .probe_transfer(&snv.as_peer(SNV_KEY), tmp.path(), PROBE_MAX_TIME)
        .await;
    assert!(!probe.ok);
    assert!(probe.error.as_deref().unwrap().contains("paused"));
    assert_eq!(probe.artifact, None);
    assert!(pp.ex.client.last_reads().is_empty(), "no catalog read");
}

/// A peer whose artifact never arrives within the probe's bound: the probe
/// fails naming the bound, its temp dir is gone, and its catalog entry is
/// still reported.
#[tokio::test]
async fn a_probe_past_its_time_bound_fails_and_leaves_nothing() {
    let server = MockServer::start().await;
    let body = b"stalled track".to_vec();
    let listed = Artifact {
        youtube_id: YT.into(),
        kind: ArtifactKind::Lyrics,
        version: crate::lyrics::LYRICS_PIPELINE_VERSION,
        size: body.len() as u64,
        sha256: sha256_hex(&body),
        updated_at: None,
    };
    let catalog = catalog_of(vec![listed.clone()]);
    Mock::given(method("GET"))
        .and(path("/api/v1/peer/catalog"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&catalog))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/v1/peer/artifact/{YT}/lyrics")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(body)
                .set_delay(Duration::from_secs(30)),
        )
        .mount(&server)
        .await;
    let pp = TestNode::start("pp", None).await;
    let peer = PeerConfig {
        name: "snv".into(),
        base_url: server.uri(),
        key: SNV_KEY.into(),
        cf_client_id: None,
        cf_client_secret: None,
    };
    let tmp = tempfile::tempdir().unwrap();
    let bound = Duration::from_millis(500);
    let probe = pp.ex.probe_transfer(&peer, tmp.path(), bound).await;
    assert!(!probe.ok);
    assert_eq!(probe.error.as_deref(), Some("the probe took over 500ms"));
    assert_eq!(probe.artifact, Some(listed));
    assert_eq!(entries(tmp.path()), 0, "the temp dir is removed");
    assert_eq!(PROBE_MAX_TIME, Duration::from_secs(300));
}

async fn post_transfer(ex: &Arc<Exchange>) -> (StatusCode, String) {
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/exchange/probe/transfer")
        .body(Body::empty())
        .unwrap();
    let resp = crate::peer::router(ex.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

#[tokio::test]
async fn the_route_probes_every_peer_with_no_secret_in_its_answer() {
    let (_snv, pp, json) = snv_and_pp().await;
    let (status, body) = post_transfer(&pp.ex).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body.contains(SNV_KEY), "the answer holds the key");
    let results: Vec<TransferProbe> = serde_json::from_str(&body).unwrap();
    assert_eq!(results.len(), 1);
    assert!(results[0].ok, "{body}");
    assert_eq!(results[0].bytes, Some(json.len() as u64));
}

#[tokio::test]
async fn a_second_probe_while_one_runs_is_refused() {
    let (_snv, pp, _) = snv_and_pp().await;
    let running = pp.ex.transfer_probe.lock().await;
    let (status, body) = post_transfer(&pp.ex).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body.contains("already running"), "{body}");
    drop(running);
    assert_eq!(post_transfer(&pp.ex).await.0, StatusCode::OK);
}

#[tokio::test]
async fn the_route_refuses_settings_that_do_not_hold() {
    let pp = TestNode::start("pp", None).await;
    set(pp.pool(), SETTING_NODE_NAME, "PP").await;
    let (status, body) = post_transfer(&pp.ex).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body.contains("node_name"), "{body}");
}
