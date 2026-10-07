//! #229: the stem worker asks its peers first, before its venv check.

use super::*;
use crate::peer::rig::{SNV_KEY, TestNode};
use std::sync::Arc;
use tokio::sync::RwLock;

const YT: &str = "aaaaaaaaaaa";

/// A stem worker of `node`. With `venv`, its venv python "exists" (an empty
/// stub), so a tick gets past the venv check to the job; the fresh health
/// registry keeps the 60 s startup floor, so a Local job never starts a
/// separation here.
fn worker(node: &TestNode, venv: bool) -> StemWorker {
    if venv {
        let python = crate::lyrics::bootstrap::venv_python_path(node.cache());
        std::fs::create_dir_all(python.parent().unwrap()).unwrap();
        std::fs::write(&python, b"").unwrap();
    }
    StemWorker::new(
        node.pool().clone(),
        node.cache().to_path_buf(),
        Arc::new(crate::playback::ndi_health::NdiHealthRegistry::new()),
        Arc::new(RwLock::new(crate::obs::ObsState::default())),
    )
    .with_peer(node.ex.clone())
}

async fn status(node: &TestNode, id: i64) -> Option<String> {
    sqlx::query_scalar("SELECT stem_status FROM videos WHERE id = ?")
        .bind(id)
        .fetch_one(node.pool())
        .await
        .unwrap()
}

/// SNV with the song and its stems hashed; PP with the song, asking SNV.
async fn snv_and_pp() -> (TestNode, TestNode, i64) {
    let snv = TestNode::start("snv", Some(SNV_KEY)).await;
    let snv_id = snv.add_video(YT).await;
    snv.give_song(snv_id, YT, "Way Maker", "Sinach").await;
    snv.give_stems(snv_id).await;
    snv.hash_now().await;
    let pp = TestNode::start("pp", None).await;
    pp.set_peers(&[snv.as_peer(SNV_KEY)]).await;
    let id = pp.add_video(YT).await;
    pp.give_song(id, YT, "Way Maker", "Sinach").await;
    (snv, pp, id)
}

#[tokio::test]
async fn a_peers_stems_are_taken_by_the_stem_worker() {
    let _lk = crate::lyrics::heavy_slot::DUB_FLAG_SERIAL.lock().await;
    crate::lyrics::heavy_slot::set_dub_slot_wanted(false);
    let (_snv, pp, id) = snv_and_pp().await;
    worker(&pp, true).process_next().await;
    assert_eq!(status(&pp, id).await.as_deref(), Some("done"));
}

/// The plan's decisions: the stem fetch runs before the lyrics-venv check,
/// so a node with no venv still takes a peer's stems.
#[tokio::test]
async fn a_node_with_no_venv_still_takes_a_peers_stems() {
    let _lk = crate::lyrics::heavy_slot::DUB_FLAG_SERIAL.lock().await;
    crate::lyrics::heavy_slot::set_dub_slot_wanted(false);
    let (_snv, pp, id) = snv_and_pp().await;
    let w = worker(&pp, false);
    w.process_next().await;
    assert_eq!(status(&pp, id).await.as_deref(), Some("done"));
    assert!(
        w.warned_no_python
            .load(std::sync::atomic::Ordering::Relaxed),
        "the missing venv is still reported"
    );
}

/// A node with no venv separates nothing, so a row nobody else has is put
/// back (no attempt): the rows behind it reach their peer step too.
#[tokio::test]
async fn a_node_with_no_venv_reaches_the_rows_behind_the_head() {
    let _lk = crate::lyrics::heavy_slot::DUB_FLAG_SERIAL.lock().await;
    crate::lyrics::heavy_slot::set_dub_slot_wanted(false);
    let snv = TestNode::start("snv", Some(SNV_KEY)).await;
    let snv_id = snv.add_video(YT).await;
    snv.give_song(snv_id, YT, "Way Maker", "Sinach").await;
    snv.give_stems(snv_id).await;
    snv.hash_now().await;
    let pp = TestNode::start("pp", None).await;
    pp.set_peers(&[snv.as_peer(SNV_KEY)]).await;
    let head = pp.add_video("ccccccccccc").await;
    pp.give_song(head, "ccccccccccc", "Iny", "Zbor").await;
    let id = pp.add_video(YT).await;
    pp.give_song(id, YT, "Way Maker", "Sinach").await;
    let w = worker(&pp, false);
    w.process_next().await;
    w.process_next().await;
    assert_eq!(
        status(&pp, id).await.as_deref(),
        Some("done"),
        "the peer's stems of the second row"
    );
    let (head_status, attempts, next): (Option<String>, i64, Option<String>) = sqlx::query_as(
        "SELECT stem_status, stem_attempts, stem_next_attempt_at FROM videos WHERE id = ?",
    )
    .bind(head)
    .fetch_one(pp.pool())
    .await
    .unwrap();
    assert_eq!((head_status, attempts), (None, 0), "no attempt counted");
    assert!(next.is_some(), "the head is re-picked later");
}

#[tokio::test]
async fn with_no_peers_the_stem_worker_goes_on_as_before() {
    let _lk = crate::lyrics::heavy_slot::DUB_FLAG_SERIAL.lock().await;
    crate::lyrics::heavy_slot::set_dub_slot_wanted(false);
    let pp = TestNode::start("pp", None).await;
    let id = pp.add_video(YT).await;
    pp.give_song(id, YT, "Way Maker", "Sinach").await;
    let w = worker(&pp, true);
    w.process_next().await;
    assert_eq!(
        status(&pp, id).await,
        None,
        "pending: the startup floor holds the separation"
    );
    assert!(
        !w.warned_no_python
            .load(std::sync::atomic::Ordering::Relaxed),
        "a venv here: no missing-venv WARN"
    );
    let waits: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM peer_waits")
        .fetch_one(pp.pool())
        .await
        .unwrap();
    assert_eq!(waits, 0);
    let next: Option<String> =
        sqlx::query_scalar("SELECT stem_next_attempt_at FROM videos WHERE id = ?")
            .bind(id)
            .fetch_one(pp.pool())
            .await
            .unwrap();
    assert_eq!(next, None, "no defer");
}
