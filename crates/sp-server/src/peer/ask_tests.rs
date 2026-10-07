//! #229 `Exchange::ask`, over two real nodes.

use super::*;
use crate::db::models_peer::{fetch_record, start_wait, waited};
use crate::peer::client::PeerError;
use crate::peer::decide::MAX_PEER_WAIT;
use crate::peer::kind::{ArtifactKind, Job};
use crate::peer::rig::{SNV_KEY, TestNode};
use crate::peer::wire::now_ms;
use std::time::Duration;

const YT: &str = "aaaaaaaaaaa";
const OTHER: &str = "bbbbbbbbbbb";

/// SNV serving one hashed song; PP asking SNV.
async fn snv_and_pp() -> (TestNode, TestNode) {
    let snv = TestNode::start("snv", Some(SNV_KEY)).await;
    let id = snv.add_video(YT).await;
    snv.give_song(id, YT, "Way Maker", "Sinach").await;
    snv.hash_now().await;
    let pp = TestNode::start("pp", None).await;
    pp.set_peers(&[snv.as_peer(SNV_KEY)]).await;
    (snv, pp)
}

async fn wait_rows(node: &TestNode) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM peer_waits")
        .fetch_one(node.pool())
        .await
        .unwrap()
}

async fn wait_start(node: &TestNode, youtube_id: &str, job: &str) -> Option<i64> {
    sqlx::query_scalar("SELECT since_ms FROM peer_waits WHERE youtube_id = ? AND job = ?")
        .bind(youtube_id)
        .bind(job)
        .fetch_optional(node.pool())
        .await
        .unwrap()
}

#[tokio::test]
async fn with_no_peers_the_job_runs_here_announced_and_nothing_is_written() {
    let pp = TestNode::start("pp", None).await;
    let Ask::Local(guard) = pp.ex.ask(Job::Stems, YT).await else {
        panic!("expected Local")
    };
    assert_eq!(pp.ex.board.snapshot("pp").len(), 2, "both stems announced");
    drop(guard);
    assert!(pp.ex.board.snapshot("pp").is_empty());
    assert_eq!(wait_rows(&pp).await, 0);
}

/// The tie-break for queued jobs (design record 6035823568): a node waits
/// only for a peer listed in its own `peers`. SNV lists none in phase 1, so a
/// job PP runs (or has queued) never holds SNV back.
#[tokio::test]
async fn a_node_never_waits_for_a_peer_it_does_not_list() {
    let pp = TestNode::start("pp", Some(SNV_KEY)).await;
    let _running = pp.ex.announce(YT, Job::Stems);
    let snv = TestNode::start("snv", None).await;
    let Ask::Local(_guard) = snv.ex.ask(Job::Stems, YT).await else {
        panic!("expected Local")
    };
    assert_eq!(wait_rows(&snv).await, 0);
    assert!(
        snv.ex.client.last_reads().is_empty(),
        "no peer was read at all"
    );
}

#[tokio::test]
async fn a_peer_that_has_it_is_fetched_from() {
    let (_snv, pp) = snv_and_pp().await;
    let Ask::Fetch(plan) = pp.ex.ask(Job::Download, YT).await else {
        panic!("expected Fetch")
    };
    assert_eq!(plan.peer.name, "snv");
    assert_eq!(plan.artifacts.len(), 2, "the video and the audio");
    assert_eq!(plan.artifact(ArtifactKind::Video).unwrap().size, 2_000);
    assert_eq!(plan.artifact(ArtifactKind::Audio).unwrap().size, 3_000);
    assert!(plan.artifact(ArtifactKind::Lyrics).is_err());
    assert_eq!(wait_rows(&pp).await, 0);
    assert!(
        pp.ex.board.snapshot("pp").is_empty(),
        "a fetch is not announced as a job run here"
    );
}

#[tokio::test]
async fn a_peer_running_the_job_is_waited_for() {
    let (snv, pp) = snv_and_pp().await;
    let _running = snv.ex.announce(OTHER, Job::Stems);
    let Ask::Wait { peer, recheck } = pp.ex.ask(Job::Stems, OTHER).await else {
        panic!("expected Wait")
    };
    assert_eq!((peer.as_str(), recheck), ("snv", Duration::from_secs(120)));
    assert!(wait_start(&pp, OTHER, "stems").await.is_some());
    assert!(pp.ex.board.snapshot("pp").is_empty(), "nothing runs here");
}

/// The 2 h bound counts from the FIRST wait, and the recheck backs off with it.
#[tokio::test]
async fn a_wait_keeps_its_first_start_and_backs_off() {
    let (snv, pp) = snv_and_pp().await;
    let _running = snv.ex.announce(OTHER, Job::Stems);
    let ten_min_ago = now_ms() - 600_000;
    start_wait(pp.pool(), OTHER, "stems", ten_min_ago)
        .await
        .unwrap();
    let Ask::Wait { recheck, .. } = pp.ex.ask(Job::Stems, OTHER).await else {
        panic!("expected Wait")
    };
    assert_eq!(recheck.as_secs(), 150, "a quarter of the 10 min waited");
    assert_eq!(
        wait_start(&pp, OTHER, "stems").await,
        Some(ten_min_ago),
        "the first start is kept"
    );
}

#[tokio::test]
async fn after_two_hours_the_job_runs_here_and_the_wait_ends() {
    let (snv, pp) = snv_and_pp().await;
    let _running = snv.ex.announce(OTHER, Job::Stems);
    let long_ago = now_ms() - i64::try_from(MAX_PEER_WAIT.as_millis()).unwrap() - 1_000;
    start_wait(pp.pool(), OTHER, "stems", long_ago)
        .await
        .unwrap();
    let Ask::Local(_guard) = pp.ex.ask(Job::Stems, OTHER).await else {
        panic!("expected Local")
    };
    assert_eq!(wait_rows(&pp).await, 0);
    assert_eq!(
        pp.ex.board.snapshot("pp").len(),
        2,
        "the job is announced here"
    );
}

#[tokio::test]
async fn an_unreachable_peer_is_waited_for() {
    let pp = TestNode::start("pp", None).await;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let gone = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let mut snv = pp.as_peer(SNV_KEY);
    snv.name = "snv".into();
    snv.base_url = gone;
    pp.set_peers(&[snv]).await;
    let Ask::Wait { peer, .. } = pp.ex.ask(Job::Lyrics, YT).await else {
        panic!("expected Wait")
    };
    assert_eq!(peer, "snv");
    assert!(wait_start(&pp, YT, "lyrics").await.is_some());
}

#[tokio::test]
async fn settings_that_do_not_hold_run_the_job_here() {
    let pp = TestNode::start("pp", None).await;
    crate::db::models::set_setting(pp.pool(), "peers", "not a list")
        .await
        .unwrap();
    let Ask::Local(_guard) = pp.ex.ask(Job::Lyrics, YT).await else {
        panic!("expected Local")
    };
    assert_eq!(wait_rows(&pp).await, 0);
    assert_eq!(pp.ex.board.snapshot("pp").len(), 1, "the lyrics job");
}

#[tokio::test]
async fn nobody_having_it_ends_an_earlier_wait() {
    let (_snv, pp) = snv_and_pp().await;
    start_wait(pp.pool(), "ccccccccccc", "lyrics", now_ms() - 1_000)
        .await
        .unwrap();
    let Ask::Local(_guard) = pp.ex.ask(Job::Lyrics, "ccccccccccc").await else {
        panic!("expected Local")
    };
    assert_eq!(wait_rows(&pp).await, 0);
}

#[tokio::test]
async fn a_failed_fetch_waits_and_a_done_one_records_its_origin() {
    let (_snv, pp) = snv_and_pp().await;
    let recheck = pp
        .ex
        .fetch_failed(Job::Download, YT, "snv", &PeerError::NotFound)
        .await;
    assert_eq!(recheck, Duration::from_secs(120));
    assert_eq!(wait_rows(&pp).await, 1);
    assert!(
        waited(pp.pool(), YT, "download", now_ms())
            .await
            .unwrap()
            .is_some()
    );
    let Ask::Fetch(plan) = pp.ex.ask(Job::Download, YT).await else {
        panic!("a peer's copy is taken while the job waits")
    };
    pp.ex
        .fetched(Job::Download, YT, "snv", &plan.artifacts)
        .await;
    assert_eq!(wait_rows(&pp).await, 0);
    let (node, version, sha) = fetch_record(pp.pool(), YT, "audio").await.unwrap().unwrap();
    assert_eq!((node.as_str(), version), ("snv", 1));
    assert_eq!(sha, plan.artifact(ArtifactKind::Audio).unwrap().sha256);
    let (node, _, sha) = fetch_record(pp.pool(), YT, "video").await.unwrap().unwrap();
    assert_eq!(node, "snv");
    assert_eq!(sha, plan.artifact(ArtifactKind::Video).unwrap().sha256);
}

#[tokio::test]
async fn a_failed_fetch_backs_off_from_the_first_wait() {
    let (_snv, pp) = snv_and_pp().await;
    start_wait(pp.pool(), YT, "download", now_ms() - 2_400_000)
        .await
        .unwrap();
    let recheck = pp
        .ex
        .fetch_failed(Job::Stems, YT, "snv", &PeerError::NotFound)
        .await;
    assert_eq!(recheck, Duration::from_secs(120), "another job's wait");
    let recheck = pp
        .ex
        .fetch_failed(Job::Download, YT, "snv", &PeerError::NotFound)
        .await;
    assert_eq!(recheck.as_secs(), 600, "a quarter of the 40 min waited");
}
