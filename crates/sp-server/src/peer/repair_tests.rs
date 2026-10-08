//! #229 `peer::repair::peer_title`, over two real nodes.

use super::*;
use crate::db::models_peer::fetch_record;
use crate::peer::rig::{SNV_KEY, TestNode};

const YT: &str = "aaaaaaaaaaa";

/// SNV holds the song under a provider's title, its pair hashed (so the
/// catalog lists the video and the audio before the title); PP asks SNV.
async fn snv_and_pp() -> (TestNode, TestNode) {
    let snv = TestNode::start("snv", Some(SNV_KEY)).await;
    let id = snv.add_video(YT).await;
    snv.give_song(id, YT, "Way Maker", "Sinach").await;
    snv.hash_now().await;
    let pp = TestNode::start("pp", None).await;
    pp.set_peers(&[snv.as_peer(SNV_KEY)]).await;
    (snv, pp)
}

#[tokio::test]
async fn a_peers_provider_title_is_taken_and_its_origin_recorded() {
    let (_snv, pp) = snv_and_pp().await;
    let taken = peer_title(&pp.ex, YT).await.unwrap();
    let title = &taken.title;
    assert_eq!(
        (
            title.song.as_str(),
            title.artist.as_str(),
            title.source,
            title.gemini_failed
        ),
        ("Way Maker", "Sinach", "gemini", false)
    );
    assert_eq!(
        fetch_record(pp.pool(), YT, "metadata").await.unwrap(),
        None,
        "recorded only once the repair wrote it"
    );
    crate::peer::download::record_title(&pp.ex, YT, &taken).await;
    let (node, version, sha) = fetch_record(pp.pool(), YT, "metadata")
        .await
        .unwrap()
        .unwrap();
    assert_eq!((node.as_str(), version), ("snv", 1));
    let catalog = pp.ex.client.catalog(&pp_peer(&pp).await).await.unwrap();
    let entry = catalog
        .artifacts
        .iter()
        .find(|a| a.kind == ArtifactKind::Metadata)
        .unwrap();
    assert_eq!(sha, entry.sha256, "the catalog's sha of the title");
}

#[tokio::test]
async fn a_peers_parser_title_is_not_taken() {
    let (snv, pp) = snv_and_pp().await;
    sqlx::query("UPDATE videos SET gemini_failed = 1, metadata_source = 'regex'")
        .execute(snv.pool())
        .await
        .unwrap();
    assert_eq!(peer_title(&pp.ex, YT).await, None);
    assert_eq!(fetch_record(pp.pool(), YT, "metadata").await.unwrap(), None);
}

#[tokio::test]
async fn a_peer_without_the_video_has_no_title() {
    let (_snv, pp) = snv_and_pp().await;
    assert_eq!(peer_title(&pp.ex, "bbbbbbbbbbb").await, None);
}

/// The row a peer answers must be the title its catalog lists: one read
/// between two changes of the title there is not taken.
#[tokio::test]
async fn a_title_that_no_longer_matches_the_catalog_is_not_taken() {
    let (snv, pp) = snv_and_pp().await;
    let peer = pp_peer(&pp).await;
    pp.ex.client.catalog(&peer).await.unwrap();
    sqlx::query("UPDATE videos SET song = 'Way Maker Live'")
        .execute(snv.pool())
        .await
        .unwrap();
    assert_eq!(
        peer_title(&pp.ex, YT).await,
        None,
        "the cached catalog is older"
    );
    assert_eq!(fetch_record(pp.pool(), YT, "metadata").await.unwrap(), None);
    pp.ex.client.forget_catalog("snv");
    assert_eq!(
        peer_title(&pp.ex, YT).await.unwrap().title.song,
        "Way Maker Live",
        "a fresh catalog matches"
    );
}

#[tokio::test]
async fn with_no_peers_no_title_is_asked() {
    let pp = TestNode::start("pp", None).await;
    assert_eq!(peer_title(&pp.ex, YT).await, None);
    assert!(pp.ex.client.last_reads().is_empty());
}

/// PP's configured peer `snv`.
async fn pp_peer(pp: &TestNode) -> crate::peer::config::PeerConfig {
    NodeConfig::load(pp.pool())
        .await
        .unwrap()
        .peer("snv")
        .unwrap()
        .clone()
}

/// #229 item A: a repaired video's wait for its peer's title ends (a later
/// re-queue of it would otherwise inherit a spent bound).
#[tokio::test]
async fn a_repaired_videos_wait_ends() {
    let (_snv, pp) = snv_and_pp().await;
    let now = crate::peer::wire::now_ms();
    crate::db::models_peer::start_wait(pp.pool(), YT, METADATA_WAIT, now)
        .await
        .unwrap();
    end_wait(&pp.ex, YT).await;
    let waited = crate::db::models_peer::waited(pp.pool(), YT, METADATA_WAIT, now)
        .await
        .unwrap();
    assert_eq!(waited, None);
    assert_eq!(METADATA_WAIT, "metadata");
}
