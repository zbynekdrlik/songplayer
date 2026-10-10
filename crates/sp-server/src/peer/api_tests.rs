//! #229 the peer API, driven through the real `peer::router`.

use axum::body::Body;
use axum::http::{HeaderMap, StatusCode};
use tower::ServiceExt;

use super::*;
use crate::peer::kind::{ArtifactKind, Job};
use crate::peer::rig::{OTHER_KEY, SNV_KEY, TestNode, bytes, set};
use crate::peer::wire::{Catalog, JobState, PeerVideo};

const YT: &str = "aaaaaaaaaaa";

async fn call(
    node: &TestNode,
    uri: &str,
    key: Option<&str>,
    range: Option<&str>,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut req = axum::http::Request::builder().uri(uri);
    if let Some(key) = key {
        req = req.header(PEER_KEY_HEADER, key);
    }
    if let Some(range) = range {
        req = req.header("range", range);
    }
    let resp = crate::peer::router(node.ex.clone())
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, headers, body)
}

/// SNV serving one hashed song (`Way Maker` / `Sinach`).
async fn snv_with_song() -> TestNode {
    let node = TestNode::start("snv", Some(SNV_KEY)).await;
    let id = node.add_video(YT).await;
    node.give_song(id, YT, "Way Maker", "Sinach").await;
    node.hash_now().await;
    node
}

async fn row_id(node: &TestNode) -> i64 {
    sqlx::query_scalar("SELECT id FROM videos WHERE youtube_id = ?")
        .bind(YT)
        .fetch_one(node.pool())
        .await
        .unwrap()
}

fn artifact(kind: &str) -> String {
    format!("/api/v1/peer/artifact/{YT}/{kind}")
}

#[test]
fn keys_match_only_the_same_key() {
    assert!(keys_match(SNV_KEY, SNV_KEY));
    assert!(!keys_match("", SNV_KEY));
    assert!(!keys_match(&SNV_KEY[..SNV_KEY.len() - 1], SNV_KEY));
    assert!(!keys_match(&format!("{SNV_KEY}x"), SNV_KEY));
    assert!(!keys_match(OTHER_KEY, SNV_KEY));
}

#[tokio::test]
async fn a_node_that_does_not_serve_answers_404_whatever_the_key() {
    let node = TestNode::start("snv", None).await;
    let audio = artifact("audio");
    for uri in ["/api/v1/peer/catalog", audio.as_str()] {
        let (status, _, _) = call(&node, uri, Some(SNV_KEY), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
    }
}

#[tokio::test]
async fn a_key_without_a_node_name_does_not_serve() {
    let node = TestNode::start("snv", Some(SNV_KEY)).await;
    set(node.pool(), "node_name", "").await;
    let (status, _, _) = call(&node, "/api/v1/peer/catalog", Some(SNV_KEY), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn settings_that_do_not_hold_do_not_serve() {
    let node = TestNode::start("snv", Some(SNV_KEY)).await;
    set(node.pool(), "node_name", "Not A Name").await;
    let (status, _, _) = call(&node, "/api/v1/peer/catalog", Some(SNV_KEY), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_request_without_the_key_or_with_another_is_refused() {
    let node = snv_with_song().await;
    for uri in ["/api/v1/peer/catalog", "/api/v1/peer/videos/aaaaaaaaaaa"] {
        for key in [None, Some(OTHER_KEY), Some("")] {
            let (status, _, body) = call(&node, uri, key, None).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri} {key:?}");
            assert!(body.is_empty());
        }
    }
    let (status, _, _) = call(&node, &artifact("audio"), None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn the_catalog_lists_the_hashed_song_and_the_jobs() {
    let node = snv_with_song().await;
    let _job = node.ex.announce("bbbbbbbbbbb", Job::Stems);
    let (status, headers, body) = call(&node, "/api/v1/peer/catalog", Some(SNV_KEY), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["cache-control"], "no-store");
    let c: Catalog = serde_json::from_slice(&body).unwrap();
    assert_eq!(c.node, "snv");
    let audio = c
        .artifacts
        .iter()
        .find(|a| a.kind == ArtifactKind::Audio)
        .unwrap();
    assert_eq!(
        audio.sha256,
        crate::peer::hash::sha256_hex(&bytes(3_000, 2))
    );
    let running = c.jobs.iter().filter(|j| j.state == JobState::Running);
    assert_eq!(running.count(), 2, "the stems job announces both stems");
    assert!(
        c.jobs
            .iter()
            .any(|j| j.youtube_id == YT && j.state == JobState::Queued),
        "the song's lyrics and stems wait in this node's queues"
    );
}

#[tokio::test]
async fn since_must_be_a_time_and_filters_the_files() {
    let node = snv_with_song().await;
    let bad = "/api/v1/peer/catalog?since=yesterday";
    let (status, _, _) = call(&node, bad, Some(SNV_KEY), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let earlier = "/api/v1/peer/catalog?since=2000-01-01T00:00:00Z";
    let (status, _, body) = call(&node, earlier, Some(SNV_KEY), None).await;
    assert_eq!(status, StatusCode::OK);
    let c: Catalog = serde_json::from_slice(&body).unwrap();
    assert_eq!(c.artifacts.len(), 3, "video, audio, metadata");
    let later = "/api/v1/peer/catalog?since=2999-01-01T00:00:00Z";
    let (status, _, body) = call(&node, later, Some(SNV_KEY), None).await;
    assert_eq!(status, StatusCode::OK);
    let c: Catalog = serde_json::from_slice(&body).unwrap();
    assert!(c.artifacts.iter().all(|a| a.kind == ArtifactKind::Metadata));
    assert_eq!(c.artifacts.len(), 1);
}

/// The pause holds every peer route (the design record), after the key: a
/// caller without it learns nothing.
#[tokio::test]
async fn a_paused_node_answers_503_retry_after() {
    let node = snv_with_song().await;
    set(node.pool(), "peer_transfers_paused", "true").await;
    let (video, audio) = (format!("/api/v1/peer/videos/{YT}"), artifact("audio"));
    for uri in ["/api/v1/peer/catalog", video.as_str(), audio.as_str()] {
        let (status, headers, _) = call(&node, uri, Some(SNV_KEY), None).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{uri}");
        assert_eq!(headers["retry-after"], "600");
        assert_eq!(headers["cache-control"], "no-store");
    }
    let (status, _, _) = call(&node, &artifact("audio"), None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// #230: a held background pauses the exchange like `peer_transfers_paused`
/// (every peer route 503); the release serves again.
#[tokio::test]
async fn a_held_background_answers_503_until_released() {
    let node = snv_with_song().await;
    crate::background_hold::hold_for_a_minute(node.pool()).await;
    let (status, headers, _) = call(&node, "/api/v1/peer/catalog", Some(SNV_KEY), None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(headers["retry-after"], "600");
    crate::background_hold::end_hold(node.pool()).await;
    let (status, _, _) = call(&node, "/api/v1/peer/catalog", Some(SNV_KEY), None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn a_videos_row_carries_its_title_and_lyrics() {
    let node = snv_with_song().await;
    let id = row_id(&node).await;
    node.give_lyrics(id, YT, "mtl+g35t").await;
    sqlx::query(
        "UPDATE videos SET lyrics_reference = 1, lyrics_translation_version = 2, \
         lyrics_translation_gender = 'm', lyrics_alignment_model = 'mtl', \
         duration_ms = 241000 WHERE id = ?",
    )
    .bind(id)
    .execute(node.pool())
    .await
    .unwrap();
    let uri = format!("/api/v1/peer/videos/{YT}");
    let (status, headers, body) = call(&node, &uri, Some(SNV_KEY), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["cache-control"], "no-store");
    let v: PeerVideo = serde_json::from_slice(&body).unwrap();
    let m = &v.metadata;
    assert_eq!(
        (m.song.as_str(), m.artist.as_str()),
        ("Way Maker", "Sinach")
    );
    assert_eq!(m.metadata_source.as_deref(), Some("gemini"));
    assert!(!m.gemini_failed);
    assert_eq!(v.duration_ms, Some(241_000));
    let l = v.lyrics.unwrap();
    assert_eq!(l.source, "mtl+g35t");
    assert_eq!(l.pipeline_version, crate::lyrics::LYRICS_PIPELINE_VERSION);
    assert_eq!(l.alignment_model.as_deref(), Some("mtl"));
    assert!(l.reference);
    assert_eq!(
        (l.translation_version, l.translation_gender.as_deref()),
        (2, Some("m"))
    );
}

#[tokio::test]
async fn a_row_without_lyrics_carries_none_and_no_reference() {
    let node = snv_with_song().await;
    let uri = format!("/api/v1/peer/videos/{YT}");
    let (status, _, body) = call(&node, &uri, Some(SNV_KEY), None).await;
    assert_eq!(status, StatusCode::OK);
    let v: PeerVideo = serde_json::from_slice(&body).unwrap();
    assert_eq!((v.lyrics, v.duration_ms), (None, None));
}

#[tokio::test]
async fn an_unknown_video_is_404_and_a_bad_id_400() {
    let node = snv_with_song().await;
    let unknown = "/api/v1/peer/videos/bbbbbbbbbbb";
    let (status, _, _) = call(&node, unknown, Some(SNV_KEY), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let short = "/api/v1/peer/videos/short";
    let (status, _, _) = call(&node, short, Some(SNV_KEY), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Review Focus 5: whichever row of the video asked for the dub.
#[tokio::test]
async fn a_dubbed_videos_row_carries_no_lyrics() {
    let node = snv_with_song().await;
    let id = row_id(&node).await;
    node.give_lyrics(id, YT, "mtl+g35t").await;
    let other = node.add_video_to(2, YT).await;
    sqlx::query("UPDATE videos SET dub_requested = 1 WHERE id = ?")
        .bind(other)
        .execute(node.pool())
        .await
        .unwrap();
    let uri = format!("/api/v1/peer/videos/{YT}");
    let (_, _, body) = call(&node, &uri, Some(SNV_KEY), None).await;
    let v: PeerVideo = serde_json::from_slice(&body).unwrap();
    assert_eq!(v.lyrics, None);
}

#[tokio::test]
async fn an_artifact_is_served_whole() {
    let node = snv_with_song().await;
    let (status, headers, body) = call(&node, &artifact("audio"), Some(SNV_KEY), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["cache-control"], "no-store");
    assert_eq!(body, bytes(3_000, 2));
    let (_, _, video) = call(&node, &artifact("video"), Some(SNV_KEY), None).await;
    assert_eq!(video, bytes(2_000, 1));
}

#[tokio::test]
async fn a_range_request_gets_the_rest_of_the_file() {
    let node = snv_with_song().await;
    let range = Some("bytes=10-");
    let (status, headers, body) = call(&node, &artifact("audio"), Some(SNV_KEY), range).await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(headers["content-range"], "bytes 10-2999/3000");
    assert_eq!(headers["cache-control"], "no-store");
    assert_eq!(body, bytes(3_000, 2)[10..].to_vec());
}

#[tokio::test]
async fn stems_and_lyrics_are_served_once_there() {
    let node = snv_with_song().await;
    let id = row_id(&node).await;
    let (_, instrumental) = node.give_stems(id).await;
    let json = node.give_lyrics(id, YT, "mtl+g35t").await;
    let (status, _, body) = call(&node, &artifact("stem_instrumental"), Some(SNV_KEY), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, std::fs::read(instrumental).unwrap());
    let (status, _, body) = call(&node, &artifact("lyrics"), Some(SNV_KEY), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json);
}

#[tokio::test]
async fn an_unknown_or_absent_kind_is_404_and_a_bad_id_400() {
    let node = snv_with_song().await;
    for kind in ["dub", "unknown", "stem_vocals", "lyrics"] {
        let (status, _, _) = call(&node, &artifact(kind), Some(SNV_KEY), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{kind}");
    }
    let bad = "/api/v1/peer/artifact/x/audio";
    let (status, _, _) = call(&node, bad, Some(SNV_KEY), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let none = "/api/v1/peer/artifact/bbbbbbbbbbb/metadata";
    let (status, _, _) = call(&node, none, Some(SNV_KEY), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn metadata_is_served_as_its_canonical_bytes() {
    let node = snv_with_song().await;
    let (status, headers, body) = call(&node, &artifact("metadata"), Some(SNV_KEY), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "application/json");
    assert_eq!(headers["cache-control"], "no-store");
    let (_, _, cat) = call(&node, "/api/v1/peer/catalog", Some(SNV_KEY), None).await;
    let c: Catalog = serde_json::from_slice(&cat).unwrap();
    let listed = c
        .artifacts
        .iter()
        .find(|a| a.kind == ArtifactKind::Metadata)
        .unwrap();
    assert_eq!(crate::peer::hash::sha256_hex(&body), listed.sha256);
}

/// Review Focus 5.
#[tokio::test]
async fn the_lyrics_of_a_dubbed_video_are_not_served() {
    let node = snv_with_song().await;
    let id = row_id(&node).await;
    node.give_lyrics(id, YT, "mtl+g35t").await;
    let (status, _, _) = call(&node, &artifact("lyrics"), Some(SNV_KEY), None).await;
    assert_eq!(status, StatusCode::OK, "plain lyrics are served");
    sqlx::query("UPDATE videos SET dub_requested = 1 WHERE id = ?")
        .bind(id)
        .execute(node.pool())
        .await
        .unwrap();
    let (status, _, _) = call(&node, &artifact("lyrics"), Some(SNV_KEY), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// The upload cap: `peer_serve_max_mbps` paces the body (1 Mbit/s = 125 000
/// B/s: the 3 000-byte audio takes ≥ 24 ms, a lower bound only).
#[tokio::test]
async fn an_artifact_is_sent_at_the_upload_cap() {
    let node = snv_with_song().await;
    set(node.pool(), "peer_serve_max_mbps", "1").await;
    let started = std::time::Instant::now();
    let (status, _, body) = call(&node, &artifact("audio"), Some(SNV_KEY), None).await;
    let took = started.elapsed();
    assert_eq!((status, body.len()), (StatusCode::OK, 3_000));
    assert!(took >= std::time::Duration::from_millis(23), "{took:?}");
}

/// The video's duration from any of its rows (the first one may have none).
#[tokio::test]
async fn a_videos_duration_is_read_from_any_row() {
    let node = snv_with_song().await;
    let other = node.add_video_to(2, YT).await;
    sqlx::query("UPDATE videos SET duration_ms = 241000 WHERE id = ?")
        .bind(other)
        .execute(node.pool())
        .await
        .unwrap();
    let uri = format!("/api/v1/peer/videos/{YT}");
    let (_, _, body) = call(&node, &uri, Some(SNV_KEY), None).await;
    let v: PeerVideo = serde_json::from_slice(&body).unwrap();
    assert_eq!(v.duration_ms, Some(241_000));
}
