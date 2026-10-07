//! #229: the metadata repair takes a peer's title before it asks the providers.

use super::*;
use crate::downloader::cache::{audio_filename, video_filename};
use crate::peer::rig::{SNV_KEY, TestNode, bytes, counting_chain};
use std::sync::Arc;
use std::sync::atomic::Ordering;

const YT: &str = "aaaaaaaaaaa";

/// PP holds the song under a parser title (`_gf` files, in the repair queue).
async fn pp_with_a_parser_title(pp: &TestNode) -> i64 {
    let id = pp.add_video(YT).await;
    let video = pp
        .cache()
        .join(video_filename("Guess", "Unknown", YT, true));
    let audio = pp
        .cache()
        .join(audio_filename("Guess", "Unknown", YT, true));
    std::fs::write(&video, bytes(2_000, 1)).unwrap();
    std::fs::write(&audio, bytes(3_000, 2)).unwrap();
    crate::db::models::mark_video_processed_pair(
        pp.pool(),
        id,
        "Guess",
        "Unknown",
        "regex",
        true,
        &video.to_string_lossy(),
        &audio.to_string_lossy(),
    )
    .await
    .unwrap();
    id
}

async fn title(node: &TestNode, id: i64) -> (String, String, Option<String>, i64) {
    sqlx::query_as("SELECT song, artist, metadata_source, gemini_failed FROM videos WHERE id = ?")
        .bind(id)
        .fetch_one(node.pool())
        .await
        .unwrap()
}

/// SNV holds the song under a provider's title; PP asks SNV.
async fn snv_and_pp() -> (TestNode, TestNode) {
    let snv = TestNode::start("snv", Some(SNV_KEY)).await;
    let snv_id = snv.add_video(YT).await;
    snv.give_song(snv_id, YT, "Way Maker", "Sinach").await;
    let pp = TestNode::start("pp", None).await;
    pp.set_peers(&[snv.as_peer(SNV_KEY)]).await;
    (snv, pp)
}

#[tokio::test]
async fn a_peers_provider_title_repairs_the_row_with_no_provider_call() {
    let (_snv, pp) = snv_and_pp().await;
    let id = pp_with_a_parser_title(&pp).await;
    let (chain, calls) = counting_chain();
    let mut worker =
        ReprocessWorker::new(pp.pool().clone(), Arc::new(chain), pp.cache().to_path_buf())
            .with_peer(pp.ex.clone());
    assert_eq!(worker.process_all().await.unwrap(), 1);
    assert_eq!(
        title(&pp, id).await,
        (
            "Way Maker".into(),
            "Sinach".into(),
            Some("gemini".into()),
            0
        )
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(
        pp.cache()
            .join(audio_filename("Way Maker", "Sinach", YT, false))
            .exists(),
        "renamed after the title"
    );
    assert!(
        !pp.cache()
            .join(audio_filename("Guess", "Unknown", YT, true))
            .exists()
    );
    let record = crate::db::models_peer::fetch_record(pp.pool(), YT, "metadata")
        .await
        .unwrap();
    assert_eq!(record.unwrap().0, "snv");
}

/// A peer's title costs no provider call, so the providers' rate-limit
/// cooldown and this video's backoff never hold it back.
#[tokio::test]
async fn a_peers_title_is_taken_during_the_providers_cooldown() {
    let (_snv, pp) = snv_and_pp().await;
    let id = pp_with_a_parser_title(&pp).await;
    let (chain, calls) = counting_chain();
    let mut worker =
        ReprocessWorker::new(pp.pool().clone(), Arc::new(chain), pp.cache().to_path_buf())
            .with_peer(pp.ex.clone());
    worker.cooldown_until = Some(Instant::now() + Duration::from_secs(600));
    worker
        .per_video_backoff
        .insert(id, (Instant::now() + Duration::from_secs(600), 2));
    let row = ReprocessRow {
        id,
        youtube_id: YT.into(),
        title: "t".into(),
    };
    assert!(matches!(
        worker.reprocess_one(&row).await.unwrap(),
        ReprocessOutcome::Success
    ));
    assert_eq!(title(&pp, id).await.0, "Way Maker");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(
        !worker.per_video_backoff.contains_key(&id),
        "a repaired video leaves the backoff"
    );
}

#[tokio::test]
async fn a_peers_parser_title_leaves_the_repair_to_the_providers() {
    let (snv, pp) = snv_and_pp().await;
    sqlx::query("UPDATE videos SET song = 'Guess', artist = 'Unknown', gemini_failed = 1, metadata_source = 'regex'")
        .execute(snv.pool())
        .await
        .unwrap();
    let id = pp_with_a_parser_title(&pp).await;
    let (chain, calls) = counting_chain();
    let mut worker =
        ReprocessWorker::new(pp.pool().clone(), Arc::new(chain), pp.cache().to_path_buf())
            .with_peer(pp.ex.clone());
    worker.process_all().await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(title(&pp, id).await.0, "Chain Song");
    assert_eq!(
        crate::db::models_peer::fetch_record(pp.pool(), YT, "metadata")
            .await
            .unwrap(),
        None
    );
}
