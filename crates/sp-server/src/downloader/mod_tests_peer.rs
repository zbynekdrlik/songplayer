//! #229: the download worker asks its peers first. Its tools are missing on
//! purpose: a local download fails (one attempt counted), so a row that ends
//! normalized with no attempt was never downloaded here.

use super::tools::ToolPaths;
use super::*;
use crate::metadata::ProviderChain;
use crate::peer::kind::Job;
use crate::peer::rig::{SNV_KEY, TestNode};
use std::sync::Arc;
use tokio::sync::broadcast;

const YT: &str = "aaaaaaaaaaa";

fn worker(node: &TestNode) -> DownloadWorker {
    let tools = ToolPaths {
        ytdlp: node.cache().join("no-yt-dlp"),
        ffmpeg: node.cache().join("no-ffmpeg"),
        python: None,
        deno: None,
    };
    let (events, _) = broadcast::channel(8);
    DownloadWorker::new(
        node.pool().clone(),
        tools,
        node.cache().to_path_buf(),
        node.cache().to_path_buf(),
        Arc::new(ProviderChain::new(vec![])),
        events,
        Arc::new(tokio::sync::Mutex::new(())),
    )
    .with_peer(node.ex.clone())
}

async fn state(node: &TestNode, id: i64) -> (i64, i64) {
    sqlx::query_as("SELECT normalized, download_attempts FROM videos WHERE id = ?")
        .bind(id)
        .fetch_one(node.pool())
        .await
        .unwrap()
}

#[tokio::test]
async fn a_download_a_peer_has_is_fetched_and_never_run() {
    let snv = TestNode::start("snv", Some(SNV_KEY)).await;
    let snv_id = snv.add_video(YT).await;
    snv.give_song(snv_id, YT, "Way Maker", "Sinach").await;
    snv.hash_now().await;
    let pp = TestNode::start("pp", None).await;
    pp.set_peers(&[snv.as_peer(SNV_KEY)]).await;
    let id = pp.add_video(YT).await;
    let w = worker(&pp);
    let mut events = w.event_tx.subscribe();
    assert!(w.process_next().await);
    assert_eq!(state(&pp, id).await, (1, 0));
    assert_eq!(events.try_recv().unwrap(), format!("processed:{YT}"));
    assert!(
        events.try_recv().is_err(),
        "no downloading: event for a fetched pair"
    );
}

#[tokio::test]
async fn with_nobody_having_it_the_worker_runs_its_own_download() {
    let pp = TestNode::start("pp", None).await;
    let id = pp.add_video(YT).await;
    let w = worker(&pp);
    let mut events = w.event_tx.subscribe();
    assert!(!w.process_next().await, "yt-dlp is missing here");
    assert_eq!(
        state(&pp, id).await,
        (0, 1),
        "the local path ran and failed once"
    );
    assert_eq!(events.try_recv().unwrap(), format!("downloading:{YT}"));
}

#[tokio::test]
async fn a_peer_downloading_it_defers_the_row_without_an_attempt() {
    let snv = TestNode::start("snv", Some(SNV_KEY)).await;
    let _running = snv.ex.announce(YT, Job::Download);
    let pp = TestNode::start("pp", None).await;
    pp.set_peers(&[snv.as_peer(SNV_KEY)]).await;
    let id = pp.add_video(YT).await;
    let w = worker(&pp);
    let mut events = w.event_tx.subscribe();
    assert!(!w.process_next().await);
    assert_eq!(state(&pp, id).await, (0, 0));
    assert!(
        events.try_recv().is_err(),
        "no downloading:/processed: event for a deferred row"
    );
    assert!(
        fetch_next_unprocessed(pp.pool()).await.unwrap().is_none(),
        "the deferred row is not picked again at once"
    );
}
