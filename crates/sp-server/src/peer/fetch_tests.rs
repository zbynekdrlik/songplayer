//! #229 `peer::fetch`: an artifact into `<cache>/peer/`, Range-resumed,
//! bounded by the catalog's size, sha-checked, one transfer per peer.

use std::path::{Path, PathBuf};
use std::time::Duration;

use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::peer::client::{PeerClient, PeerError};
use crate::peer::config::PeerConfig;
use crate::peer::hash::sha256_hex;
use crate::peer::kind::ArtifactKind;
use crate::peer::rig::{SNV_KEY, TestNode, bytes, set};
use crate::peer::wire::Artifact;

const YT: &str = "aaaaaaaaaaa";
const AUDIO_PATH: &str = "/api/v1/peer/artifact/aaaaaaaaaaa/audio";

fn body() -> Vec<u8> {
    bytes(1_000, 5)
}

fn artifact_of(data: &[u8]) -> Artifact {
    Artifact {
        youtube_id: YT.into(),
        kind: ArtifactKind::Audio,
        version: 1,
        size: data.len() as u64,
        sha256: sha256_hex(data),
        updated_at: None,
    }
}

fn peer_at(base_url: &str) -> PeerConfig {
    PeerConfig {
        name: "snv".into(),
        base_url: base_url.into(),
        key: SNV_KEY.into(),
        cf_client_id: None,
        cf_client_secret: None,
    }
}

fn part_of(dir: &Path, a: &Artifact) -> PathBuf {
    dir.join(part_name(a).unwrap())
}

#[test]
fn a_part_is_named_by_video_kind_and_sha() {
    let a = artifact_of(&body());
    let want = format!("{YT}_audio_{}.part", &a.sha256[..16]);
    assert_eq!(part_name(&a), Some(want));
    let bad_id = Artifact {
        youtube_id: "../x".into(),
        ..a.clone()
    };
    assert_eq!(part_name(&bad_id), None);
    let bad_sha = Artifact {
        sha256: "nothex".into(),
        ..a.clone()
    };
    assert_eq!(part_name(&bad_sha), None);
    let unknown = Artifact {
        kind: ArtifactKind::Unknown,
        ..a
    };
    assert_eq!(part_name(&unknown), None);
}

#[test]
fn content_range_names_its_first_byte() {
    assert_eq!(content_range_start("bytes 300-999/1000"), Some(300));
    assert_eq!(content_range_start("bytes 0-0/1"), Some(0));
    assert_eq!(content_range_start("bytes */1000"), None);
    assert_eq!(content_range_start("items 3-4/5"), None);
}

#[tokio::test]
async fn fetches_a_real_nodes_artifact_and_checks_its_sha() {
    let snv = TestNode::start("snv", Some(SNV_KEY)).await;
    let id = snv.add_video(YT).await;
    snv.give_song(id, YT, "Way Maker", "Sinach").await;
    snv.hash_now().await;
    let client = PeerClient::new();
    let peer = snv.as_peer(SNV_KEY);
    let c = client.read_catalog(&peer).await.unwrap();
    let audio = c
        .artifacts
        .iter()
        .find(|a| a.kind == ArtifactKind::Audio)
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let part = client.fetch(&peer, audio, dir.path()).await.unwrap();
    assert_eq!(part, part_of(dir.path(), audio));
    assert_eq!(std::fs::read(&part).unwrap(), bytes(3_000, 2));
}

/// The real peer API answers a resumed part with its 206: the client appends
/// from where the part stopped.
#[tokio::test]
async fn resumes_a_part_from_a_real_node() {
    let snv = TestNode::start("snv", Some(SNV_KEY)).await;
    let id = snv.add_video(YT).await;
    snv.give_song(id, YT, "Way Maker", "Sinach").await;
    snv.hash_now().await;
    let client = PeerClient::new();
    let peer = snv.as_peer(SNV_KEY);
    let c = client.read_catalog(&peer).await.unwrap();
    let video = c
        .artifacts
        .iter()
        .find(|a| a.kind == ArtifactKind::Video)
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(part_of(dir.path(), video), &bytes(2_000, 1)[..700]).unwrap();
    let part = client.fetch(&peer, video, dir.path()).await.unwrap();
    assert_eq!(std::fs::read(&part).unwrap(), bytes(2_000, 1));
}

#[tokio::test]
async fn resumes_a_part_with_a_range_request() {
    let data = body();
    let a = artifact_of(&data);
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(AUDIO_PATH))
        .and(header("range", "bytes=300-"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("content-range", "bytes 300-999/1000")
                .set_body_bytes(data[300..].to_vec()),
        )
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(part_of(dir.path(), &a), &data[..300]).unwrap();
    let part = PeerClient::new()
        .fetch(&peer_at(&server.uri()), &a, dir.path())
        .await
        .unwrap();
    assert_eq!(std::fs::read(part).unwrap(), data);
}

#[tokio::test]
async fn a_206_from_another_byte_is_refused_and_the_part_dropped() {
    let data = body();
    let a = artifact_of(&data);
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("content-range", "bytes 0-999/1000")
                .set_body_bytes(data.clone()),
        )
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(part_of(dir.path(), &a), &data[..300]).unwrap();
    let err = PeerClient::new()
        .fetch(&peer_at(&server.uri()), &a, dir.path())
        .await
        .unwrap_err();
    assert!(matches!(err, PeerError::BadResponse(_)), "{err:?}");
    assert!(!part_of(dir.path(), &a).exists());
}

#[tokio::test]
async fn a_server_that_ignores_range_restarts_the_part() {
    let data = body();
    let a = artifact_of(&data);
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(data.clone()))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(part_of(dir.path(), &a), &data[..300]).unwrap();
    let part = PeerClient::new()
        .fetch(&peer_at(&server.uri()), &a, dir.path())
        .await
        .unwrap();
    assert_eq!(std::fs::read(part).unwrap(), data);
}

#[tokio::test]
async fn a_complete_part_is_checked_without_a_request() {
    let data = body();
    let a = artifact_of(&data);
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(part_of(dir.path(), &a), &data).unwrap();
    let part = PeerClient::new()
        .fetch(&peer_at(&server.uri()), &a, dir.path())
        .await
        .unwrap();
    assert_eq!(std::fs::read(part).unwrap(), data);
}

#[tokio::test]
async fn a_part_longer_than_the_artifact_is_fetched_again() {
    let data = body();
    let a = artifact_of(&data);
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(data.clone()))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let mut longer = data.clone();
    longer.push(0);
    std::fs::write(part_of(dir.path(), &a), &longer).unwrap();
    let part = PeerClient::new()
        .fetch(&peer_at(&server.uri()), &a, dir.path())
        .await
        .unwrap();
    assert_eq!(std::fs::read(part).unwrap(), data);
}

#[tokio::test]
async fn one_byte_over_the_catalog_size_is_refused_and_dropped() {
    let data = body();
    let a = artifact_of(&data);
    let mut over = data.clone();
    over.push(9);
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(over))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let err = PeerClient::new()
        .fetch(&peer_at(&server.uri()), &a, dir.path())
        .await
        .unwrap_err();
    assert!(matches!(err, PeerError::BadResponse(_)), "{err:?}");
    assert!(!part_of(dir.path(), &a).exists());
}

#[tokio::test]
async fn a_short_body_keeps_the_part_for_the_next_attempt() {
    let data = body();
    let a = artifact_of(&data);
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(data[..700].to_vec()))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let err = PeerClient::new()
        .fetch(&peer_at(&server.uri()), &a, dir.path())
        .await
        .unwrap_err();
    assert!(matches!(err, PeerError::BadResponse(_)), "{err:?}");
    let kept = std::fs::read(part_of(dir.path(), &a)).unwrap();
    assert_eq!(kept, data[..700].to_vec());
}

#[tokio::test]
async fn a_sha_mismatch_drops_the_part_and_the_cached_catalog() {
    let data = body();
    let mut lying = artifact_of(&data);
    lying.sha256 = sha256_hex(b"something else");
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(AUDIO_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(data.clone()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/peer/catalog"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"node":"snv"}"#))
        .expect(2)
        .mount(&server)
        .await;
    let client = PeerClient::new();
    let peer = peer_at(&server.uri());
    client.catalog(&peer).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let err = client.fetch(&peer, &lying, dir.path()).await.unwrap_err();
    assert!(matches!(err, PeerError::ShaMismatch { .. }), "{err:?}");
    assert!(!part_of(dir.path(), &lying).exists());
    client.catalog(&peer).await.unwrap(); // read again: the mismatch forgot the cache
}

/// A part of an older copy (another sha) is dropped, a part of another kind
/// kept; a fetch with no part to resume asks for the whole file (no Range).
#[tokio::test]
async fn a_part_of_an_older_copy_is_dropped_other_kinds_kept() {
    let data = body();
    let a = artifact_of(&data);
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(data.clone()))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let older = dir.path().join(format!("{YT}_audio_ffffffffffffffff.part"));
    let other_kind = dir.path().join(format!("{YT}_video_ffffffffffffffff.part"));
    std::fs::write(&older, b"old").unwrap();
    std::fs::write(&other_kind, b"video part").unwrap();
    PeerClient::new()
        .fetch(&peer_at(&server.uri()), &a, dir.path())
        .await
        .unwrap();
    assert!(!older.exists());
    assert!(other_kind.exists());
    let req = &server.received_requests().await.unwrap()[0];
    assert!(req.headers.get("range").is_none(), "nothing to resume");
}

#[tokio::test]
async fn one_transfer_at_a_time_per_peer() {
    let data = body();
    let a = artifact_of(&data);
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(data.clone()))
        .mount(&server)
        .await;
    let client = std::sync::Arc::new(PeerClient::new());
    let dir = tempfile::tempdir().unwrap();
    let slot = client.slot("snv");
    let held = slot.lock().await;
    let (c2, peer, dir2, a2) = (
        client.clone(),
        peer_at(&server.uri()),
        dir.path().to_path_buf(),
        a.clone(),
    );
    let task = tokio::spawn(async move { c2.fetch(&peer, &a2, &dir2).await });
    let waited = tokio::time::timeout(Duration::from_secs(1), async {
        while server.received_requests().await.unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(
        waited.is_err(),
        "no request while another transfer holds the peer"
    );
    drop(held);
    task.await.unwrap().unwrap();
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_paused_node_fetches_nothing() {
    let pp = TestNode::start("pp", None).await;
    set(pp.pool(), "peer_transfers_paused", "true").await;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let err = pp
        .ex
        .fetch(&peer_at(&server.uri()), &artifact_of(&body()))
        .await
        .unwrap_err();
    assert_eq!(err, PeerError::Paused);
    assert_eq!(pp.ex.parts_dir(), pp.cache().join("peer"));
}

#[tokio::test]
async fn a_node_fetches_into_its_parts_dir() {
    let snv = TestNode::start("snv", Some(SNV_KEY)).await;
    let id = snv.add_video(YT).await;
    snv.give_song(id, YT, "Way Maker", "Sinach").await;
    snv.hash_now().await;
    let pp = TestNode::start("pp", None).await;
    let peer = snv.as_peer(SNV_KEY);
    let c = pp.ex.client.read_catalog(&peer).await.unwrap();
    let audio = c
        .artifacts
        .iter()
        .find(|a| a.kind == ArtifactKind::Audio)
        .unwrap();
    let part = pp.ex.fetch(&peer, audio).await.unwrap();
    assert_eq!(part.parent(), Some(pp.cache().join("peer").as_path()));
    assert_eq!(std::fs::read(part).unwrap(), bytes(3_000, 2));
}
