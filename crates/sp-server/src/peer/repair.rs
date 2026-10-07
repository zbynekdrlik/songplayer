//! #229: the metadata repair asks first. A video in this node's repair queue
//! (a parser title) takes a peer's provider or operator title
//! (`download::adopted_title`, metadata version ≥ 1) when the peer's catalog
//! lists the video's metadata at such a version and the peer's row matches
//! that entry (the sha256 of `PeerMetadata::to_bytes`, as the catalog
//! computes it): no provider is called here. Else this node's providers repair
//! it as before. The repair's own locked rename + record
//! (`ReprocessWorker::apply_title`) writes the title; only once it did is the
//! origin recorded in `peer_fetches` ([`record`]).

use tracing::{info, warn};

use super::Exchange;
use super::config::NodeConfig;
use super::download::adopted_title;
use super::hash::sha256_hex;
use super::kind::{ArtifactKind, acceptable};
use super::wire::now_ms;
use crate::db::models_peer;
use crate::metadata::manual::DownloadTitle;

/// A peer's title for a video, with where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerTitle {
    pub title: DownloadTitle,
    /// The peer's configured name.
    pub peer: String,
    /// The title's metadata version (`kind::metadata_version`).
    pub version: u32,
    /// The sha256 of the title's canonical bytes (the catalog's).
    pub sha256: String,
}

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
        let sha256 = sha256_hex(&video.metadata.to_bytes());
        if sha256 != entry.sha256 {
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
        return Some(PeerTitle {
            title,
            peer: peer.name.clone(),
            version: video.metadata.version(),
            sha256,
        });
    }
    None
}

/// `youtube_id`'s title came from `t.peer`: recorded once the repair wrote it.
pub async fn record(ex: &Exchange, youtube_id: &str, t: &PeerTitle) {
    let recorded = models_peer::record_fetch(
        &ex.pool,
        youtube_id,
        ArtifactKind::Metadata.as_str(),
        &t.peer,
        t.version,
        &t.sha256,
        now_ms(),
    )
    .await;
    if let Err(e) = recorded {
        warn!(youtube_id, %e, "exchange: recording a fetch failed");
    }
    info!(
        youtube_id,
        source = %format!("peer:{}", t.peer),
        "exchange: the metadata repair took a peer's title"
    );
}

#[cfg(test)]
#[path = "repair_tests.rs"]
mod tests;
