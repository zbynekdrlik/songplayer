//! #229: the metadata repair asks first. A video in this node's repair queue
//! (a parser title) takes a peer's provider or operator title
//! (`download::PeerTitle::of`, metadata version ≥ 1) when the peer's catalog
//! lists the video's metadata at such a version and the peer's row matches
//! that entry (the sha256 of `PeerMetadata::to_bytes`, as the catalog
//! computes it): no provider is called here. Else this node's providers repair
//! it as before. The repair's own locked rename + record
//! (`ReprocessWorker::apply_title`) writes the title; only once it did is the
//! origin recorded in `peer_fetches` (`download::record_title`).

use tracing::warn;

use super::Exchange;
use super::config::NodeConfig;
use super::download::PeerTitle;
use super::kind::{ArtifactKind, acceptable};

/// A peer's title for `youtube_id`, or `None` (ask this node's providers).
/// The peers are asked in their configured order; the first one with a title
/// this node takes wins.
pub async fn peer_title(ex: &Exchange, youtube_id: &str) -> Option<PeerTitle> {
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
        // A parser's title there is no better than one made here: no request.
        if !acceptable(ArtifactKind::Metadata, entry.version) {
            continue;
        }
        let Ok(video) = ex.client.video(peer, youtube_id).await else {
            continue;
        };
        let Some(taken) = PeerTitle::of(&peer.name, &video.metadata) else {
            continue;
        };
        if taken.sha256 != entry.sha256 {
            warn!(
                youtube_id,
                peer = %peer.name,
                "exchange: a peer's title does not match its catalog - asking again later"
            );
            continue;
        }
        return Some(taken);
    }
    None
}

#[cfg(test)]
#[path = "repair_tests.rs"]
mod tests;
