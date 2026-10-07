//! #229: the PP gate's real transfer, `POST /api/v1/exchange/probe/transfer`
//! (`peer::lan`, LAN, no key, like the catalog probe). For each configured
//! peer: its catalog read now, its smallest FILE artifact ([`pick`]: a
//! `metadata` entry is answered from a row, never through the file path
//! the gate is about), at most [`PROBE_MAX_BYTES`], fetched through this
//! node's own client (the same request, Range resume, size bound and sha
//! check as an adoption, `PeerClient::fetch_unslotted`) into a temp dir
//! OUTSIDE the cache, which is removed when the probe ends. It does not take
//! the peer's transfer slot: its part is its own and small, and the gate
//! must not wait behind the workers' queued transfers. The bytes that
//! arrived are counted and hashed again here, so the gate reads a real
//! transfer, not a catalog. Refused while this node's transfers are paused;
//! one probe at a time (the route answers 409 to a second).

use std::path::Path;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tracing::info;

use super::Exchange;
use super::config::PeerConfig;
use super::hash::sha256_file;
use super::kind::ArtifactKind;
use super::wire::{Artifact, Catalog};

/// The largest artifact a probe transfers: 64 MiB (a lyrics track is a few
/// hundred KiB, a stem tens of MiB).
pub const PROBE_MAX_BYTES: u64 = 67_108_864;

/// The longest a probe of one peer takes, the catalog read included: under
/// the PP gate's 330 s request bound, so the gate reads the probe's own
/// failure, and a trickling transfer never holds the probe lock for long.
pub const PROBE_MAX_TIME: Duration = Duration::from_secs(300);

/// One peer's transfer probe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferProbe {
    pub name: String,
    pub base_url: String,
    /// The artifact arrived whole and sha-checked.
    pub ok: bool,
    /// The catalog's entry of the artifact transferred; `None` when none
    /// was picked.
    pub artifact: Option<Artifact>,
    /// What arrived, read back here: its byte count and sha256.
    pub bytes: Option<u64>,
    pub sha256: Option<String>,
    pub latency_ms: u64,
    pub error: Option<String>,
}

/// The artifact a probe transfers from `catalog`: the smallest non-empty
/// file artifact (the first listed among equals), or why none.
pub fn pick(catalog: &Catalog) -> Result<&Artifact, String> {
    let smallest = catalog
        .artifacts
        .iter()
        .filter(|a| a.kind != ArtifactKind::Metadata && a.size > 0)
        .min_by_key(|a| a.size)
        .ok_or_else(|| "the catalog lists no file artifact".to_string())?;
    if smallest.size > PROBE_MAX_BYTES {
        return Err(format!(
            "its smallest file artifact is {} bytes, over the probe's {PROBE_MAX_BYTES}",
            smallest.size
        ));
    }
    Ok(smallest)
}

impl Exchange {
    /// Transfer one artifact of `peer` into a temp dir under `tmp_parent`
    /// (the OS temp dir in production) and read it back, within `max_time`
    /// ([`PROBE_MAX_TIME`] from the route): a transfer that trickles past it
    /// fails, its temp dir removed, and the probe lock is free again.
    pub async fn probe_transfer(
        &self,
        peer: &PeerConfig,
        tmp_parent: &Path,
        max_time: Duration,
    ) -> TransferProbe {
        let started = Instant::now();
        let mut probe = TransferProbe {
            name: peer.name.clone(),
            base_url: peer.base_url.clone(),
            ok: false,
            artifact: None,
            bytes: None,
            sha256: None,
            latency_ms: 0,
            error: None,
        };
        let done = tokio::time::timeout(max_time, self.transfer_one(peer, tmp_parent, &mut probe))
            .await
            .unwrap_or_else(|_| Err(format!("the probe took over {max_time:?}")));
        probe.latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        probe.ok = done.is_ok();
        probe.error = done.err();
        info!(
            peer = %probe.name,
            ok = probe.ok,
            bytes = probe.bytes,
            latency_ms = probe.latency_ms,
            error = probe.error.as_deref(),
            "exchange: transfer probe"
        );
        probe
    }

    async fn transfer_one(
        &self,
        peer: &PeerConfig,
        tmp_parent: &Path,
        probe: &mut TransferProbe,
    ) -> Result<(), String> {
        if self.transfers_paused().await {
            return Err("transfers are paused on this node (peer_transfers_paused)".into());
        }
        let catalog = self
            .client
            .read_catalog(peer)
            .await
            .map_err(|e| format!("reading the catalog failed: {e}"))?;
        let artifact = pick(&catalog)?.clone();
        probe.artifact = Some(artifact.clone());
        let dir = tempfile::Builder::new()
            .prefix("songplayer-peer-probe-")
            .tempdir_in(tmp_parent)
            .map_err(|e| format!("local: a temp dir: {e}"))?;
        let part = self
            .client
            .fetch_unslotted(peer, &artifact, dir.path())
            .await
            .map_err(|e| e.to_string())?;
        let read_back = |e: std::io::Error| format!("local: reading the part back: {e}");
        let bytes = tokio::fs::metadata(&part).await.map_err(read_back)?.len();
        probe.bytes = Some(bytes);
        probe.sha256 = Some(sha256_file(&part, 0).await.map_err(read_back)?);
        Ok(())
    }
}

#[cfg(test)]
#[path = "transfer_probe_tests.rs"]
mod tests;
