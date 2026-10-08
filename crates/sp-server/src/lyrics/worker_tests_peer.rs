//! #229: the lyrics worker asks its peers first.

use super::*;
use crate::peer::kind::Job;
use crate::peer::rig::{SNV_KEY, TestNode};

const YT: &str = "aaaaaaaaaaa";

fn worker(node: &TestNode) -> LyricsWorker {
    let (events, _) = broadcast::channel(16);
    LyricsWorker::new_for_test(node.pool().clone(), node.cache().to_path_buf(), events)
        .with_peer(node.ex.clone())
}

async fn lyrics_state(node: &TestNode, id: i64) -> (i64, i64, Option<String>) {
    sqlx::query_as(
        "SELECT has_lyrics, lyrics_attempts, lyrics_next_attempt_at FROM videos WHERE id = ?",
    )
    .bind(id)
    .fetch_one(node.pool())
    .await
    .unwrap()
}

#[tokio::test]
async fn a_peers_lyrics_are_taken_by_the_lyrics_worker() {
    let _lk = crate::lyrics::heavy_slot::DUB_FLAG_SERIAL.lock().await;
    crate::lyrics::heavy_slot::set_dub_slot_wanted(false);
    let snv = TestNode::start("snv", Some(SNV_KEY)).await;
    let snv_id = snv.add_video(YT).await;
    snv.give_song(snv_id, YT, "Way Maker", "Sinach").await;
    snv.give_lyrics(snv_id, YT, "mtl+g35t").await;
    snv.hash_now().await;
    let pp = TestNode::start("pp", None).await;
    pp.set_peers(&[snv.as_peer(SNV_KEY)]).await;
    let id = pp.add_video(YT).await;
    pp.give_song(id, YT, "Way Maker", "Sinach").await;
    pp.audio_from(YT, "snv").await;
    let (events, mut rx) = broadcast::channel(16);
    LyricsWorker::new_for_test(pp.pool().clone(), pp.cache().to_path_buf(), events)
        .with_peer(pp.ex.clone())
        .process_next()
        .await;
    assert_eq!(lyrics_state(&pp, id).await.0, 1);
    assert!(pp.cache().join(format!("{YT}_lyrics.json")).exists());
    let mut completed = None;
    while let Ok(msg) = rx.try_recv() {
        if let ServerMsg::LyricsCompleted {
            video_id, source, ..
        } = msg
        {
            completed = Some((video_id, source));
        }
    }
    assert_eq!(
        completed,
        Some((id, "mtl+g35t".to_string())),
        "the dashboard hears of a peer's lyrics as of its own"
    );
}

/// #229 PP audit (comment 6054582866): PP's own track of a song SNV had
/// stands in for SNV's (`peer::standin`); once SNV has its lyrics, the
/// worker's tick takes them in its place and tells the dashboard.
#[tokio::test]
async fn the_worker_replaces_a_stand_in_with_the_peers_copy() {
    let _lk = crate::lyrics::heavy_slot::DUB_FLAG_SERIAL.lock().await;
    crate::lyrics::heavy_slot::set_dub_slot_wanted(false);
    let snv = TestNode::start("snv", Some(SNV_KEY)).await;
    let snv_id = snv.add_video(YT).await;
    snv.give_song(snv_id, YT, "Way Maker", "Sinach").await;
    let snv_json = snv.give_lyrics(snv_id, YT, "mtl+g35t").await;
    snv.hash_now().await;
    let pp = TestNode::start("pp", None).await;
    pp.set_peers(&[snv.as_peer(SNV_KEY)]).await;
    let id = pp.add_video(YT).await;
    pp.give_song(id, YT, "Way Maker", "Sinach").await;
    pp.audio_from(YT, "snv").await;
    pp.give_lyrics(id, YT, "gemini-3-5-transcribe").await;
    crate::db::models_peer::record_standin(pp.pool(), YT, "lyrics", "snv", 1_000, 0)
        .await
        .unwrap();
    let (events, mut rx) = broadcast::channel(16);
    LyricsWorker::new_for_test(pp.pool().clone(), pp.cache().to_path_buf(), events)
        .with_peer(pp.ex.clone())
        .process_next()
        .await;
    let json = std::fs::read(pp.cache().join(format!("{YT}_lyrics.json"))).unwrap();
    assert_eq!(json, snv_json);
    let mut completed = None;
    while let Ok(msg) = rx.try_recv() {
        if let ServerMsg::LyricsCompleted {
            video_id, source, ..
        } = msg
        {
            completed = Some((video_id, source));
        }
    }
    assert_eq!(completed, Some((id, "mtl+g35t".to_string())));
}

#[tokio::test]
async fn a_peer_with_the_lyrics_job_queued_defers_the_song() {
    let _lk = crate::lyrics::heavy_slot::DUB_FLAG_SERIAL.lock().await;
    crate::lyrics::heavy_slot::set_dub_slot_wanted(false);
    let snv = TestNode::start("snv", Some(SNV_KEY)).await;
    let snv_id = snv.add_video(YT).await;
    snv.give_song(snv_id, YT, "Way Maker", "Sinach").await;
    let pp = TestNode::start("pp", None).await;
    pp.set_peers(&[snv.as_peer(SNV_KEY)]).await;
    let id = pp.add_video(YT).await;
    pp.give_song(id, YT, "Way Maker", "Sinach").await;
    let snv_catalog = crate::peer::catalog::build(&snv.ex, "snv", None)
        .await
        .unwrap();
    assert!(
        snv_catalog.announces(YT, Job::Lyrics.makes()),
        "SNV has the song's lyrics queued"
    );
    worker(&pp).process_next().await;
    let (has, attempts, next) = lyrics_state(&pp, id).await;
    assert_eq!((has, attempts), (0, 0), "nothing ran here, no attempt");
    assert!(next.is_some(), "re-picked after the recheck");
}
