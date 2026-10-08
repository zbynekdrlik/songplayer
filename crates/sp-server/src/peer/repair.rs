//! #229: the metadata repair asks first. A video in this node's repair queue
//! (a parser title) takes a peer's provider or operator title
//! (`download::PeerTitle::of`, metadata version ≥ 1) when the peer's catalog
//! lists the video's metadata at such a version and the peer's row matches
//! that entry (the sha256 of `PeerMetadata::to_bytes`, as the catalog
//! computes it): no provider is called here. Else this node's providers repair
//! it as before. The repair's own locked rename + record
//! (`ReprocessWorker::apply_title`) writes the title; only once it did is the
//! origin recorded in `peer_fetches` (`download::record_title`).
//!
//! Item A: with no peer's title yet, the repair WAITS (≤ 2 h, `peer_waits`
//! job `metadata`) while the listed peer this node took the song's audio
//! from still lists that audio, or cannot be read ([`waits_for_peer`]):
//! that peer names the song too (its download, its own repair), so this
//! node's providers (paid AI) never redo it; past the bound they repair it.

use tracing::warn;

use super::Exchange;
use super::config::NodeConfig;
use super::decide::{gives_up, listed_audio};
use super::download::PeerTitle;
use super::kind::{ArtifactKind, acceptable};
use super::wire::now_ms;
use crate::db::models_peer;

/// The `peer_waits` job of a repair waiting for its peer's title.
pub const METADATA_WAIT: &str = "metadata";

/// Whether the repair of `youtube_id` waits for a peer's title (the module
/// doc): this node took the song's audio from a peer it lists, and that
/// peer lists the same audio (its sha256) or cannot be read, and the wait
/// has not reached the 2 h bound (`decide::gives_up`). A wait starts at its
/// first answer `true`; [`end_wait`] ends it.
pub async fn waits_for_peer(ex: &Exchange, youtube_id: &str) -> bool {
    let Some((node, sha256)) = ex.song_from(youtube_id).await else {
        return false;
    };
    let Ok(cfg) = NodeConfig::load(&ex.pool).await else {
        return false;
    };
    let Some(peer) = cfg.peer(&node) else {
        return false;
    };
    let holds = match ex.client.catalog(peer).await {
        Ok(catalog) => listed_audio(&catalog, youtube_id).is_some_and(|a| a.sha256 == sha256),
        Err(_) => true,
    };
    if !holds {
        return false;
    }
    let now = now_ms();
    if let Err(e) = models_peer::start_wait(&ex.pool, youtube_id, METADATA_WAIT, now).await {
        warn!(youtube_id, %e, "exchange: recording the repair's wait failed");
    }
    let waited = models_peer::waited(&ex.pool, youtube_id, METADATA_WAIT, now)
        .await
        .inspect_err(|e| warn!(youtube_id, %e, "exchange: reading the repair's wait failed"))
        .ok()
        .flatten()
        .unwrap_or_default();
    !gives_up(waited)
}

/// The repair of `youtube_id` waits no more (it was repaired).
pub async fn end_wait(ex: &Exchange, youtube_id: &str) {
    if let Err(e) = models_peer::end_wait(&ex.pool, youtube_id, METADATA_WAIT).await {
        warn!(youtube_id, %e, "exchange: ending the repair's wait failed");
    }
}

/// A peer's title for `youtube_id`, or `None`: the repair then waits while
/// the peer the song came from holds it (`waits_for_peer`), else asks this
/// node's providers (held while paid AI is off).
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
