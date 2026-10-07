//! #229: the metadata repair asks first. A video in this node's repair queue
//! (a parser title) takes a peer's provider or operator title
//! (`download::adopted_title`, metadata version ≥ 1) when the peer's catalog
//! lists the video's metadata and the peer's row matches that entry (the
//! sha256 of `PeerMetadata::to_bytes`, as the catalog computes it): no
//! provider is called here, and the origin goes to `peer_fetches`. Else this
//! node's providers repair it as before. The repair's own locked rename +
//! record (`ReprocessWorker::apply_title`) writes the title.

use tracing::{info, warn};

use super::Exchange;
use super::config::NodeConfig;
use super::download::adopted_title;
use super::hash::sha256_hex;
use super::kind::ArtifactKind;
use super::wire::now_ms;
use crate::db::models_peer;
use crate::metadata::manual::DownloadTitle;

/// A peer's title for `youtube_id`, or `None` (ask this node's providers).
/// The peers are asked in their configured order; the first one with a title
/// this node takes wins.
pub async fn peer_title(ex: &Exchange, youtube_id: &str) -> Option<DownloadTitle> {
    let cfg = NodeConfig::load(&ex.pool).await.ok()?;
    if !cfg.asking() {
        return None;
    }
    for peer in &cfg.peers {
        let Ok(catalog) = ex.client.catalog(peer).await else {
            continue;
        };
        let Some(entry) = catalog
            .artifacts
            .iter()
            .find(|a| a.youtube_id == youtube_id && a.kind == ArtifactKind::Metadata)
        else {
            continue;
        };
        let Ok(video) = ex.client.video(peer, youtube_id).await else {
            continue;
        };
        let sha = sha256_hex(&video.metadata.to_bytes());
        if sha != entry.sha256 {
            warn!(
                youtube_id,
                peer = %peer.name,
                "exchange: a peer's title does not match its catalog - asking again later"
            );
            continue;
        }
        let Some(title) = adopted_title(&video.metadata) else {
            continue;
        };
        let recorded = models_peer::record_fetch(
            &ex.pool,
            youtube_id,
            ArtifactKind::Metadata.as_str(),
            &peer.name,
            video.metadata.version(),
            &sha,
            now_ms(),
        )
        .await;
        if let Err(e) = recorded {
            warn!(youtube_id, %e, "exchange: recording a fetch failed");
        }
        info!(
            youtube_id,
            source = %format!("peer:{}", peer.name),
            "exchange: the metadata repair takes a peer's title"
        );
        return Some(title);
    }
    None
}

#[cfg(test)]
#[path = "repair_tests.rs"]
mod tests;
