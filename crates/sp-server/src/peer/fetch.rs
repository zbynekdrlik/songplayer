//! #229: one artifact from a peer into `<cache>/peer/` — a subdir the cache
//! self-heal and the phase-0 copy never touch. The part is named by the
//! artifact's sha, so a part of an older copy is never resumed into a newer
//! one; it resumes with `Range: bytes=N-` (a 206 must start at N, a 200
//! restarts), stops at the catalog's size, and is sha-checked at the end (a
//! mismatch drops it and the cached catalog). One transfer at a time per
//! peer; the caller fetches one job's artifacts from one peer, so two
//! transfers never write the same part. The caller renames the verified part
//! into place (`peer::{download, stems, lyrics}`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::io::AsyncWriteExt;
use tracing::info;

use super::Exchange;
use super::client::{PeerClient, PeerError, lock, status_error, unreachable_err};
use super::config::PeerConfig;
use super::hash::sha256_file;
use super::kind::ArtifactKind;
use super::wire::{Artifact, is_sha256_hex};
use crate::downloader::cache::is_valid_video_id;

/// `<yt>_<kind>_<first 16 sha digits>.part`, or `None` for an artifact this
/// node cannot name safely.
pub fn part_name(a: &Artifact) -> Option<String> {
    let nameable = is_valid_video_id(&a.youtube_id)
        && is_sha256_hex(&a.sha256)
        && a.kind != ArtifactKind::Unknown;
    nameable.then(|| {
        format!(
            "{}_{}_{}.part",
            a.youtube_id,
            a.kind.as_str(),
            &a.sha256[..16]
        )
    })
}

/// The first byte of a `Content-Range: bytes <first>-<last>/<size>`.
pub fn content_range_start(value: &str) -> Option<u64> {
    value
        .strip_prefix("bytes ")?
        .split('-')
        .next()?
        .trim()
        .parse()
        .ok()
}

impl PeerClient {
    /// The transfer slot of `peer` (one transfer at a time per peer).
    pub(crate) fn slot(&self, peer: &str) -> Arc<tokio::sync::Mutex<()>> {
        Arc::clone(lock(&self.slots).entry(peer.to_string()).or_default())
    }

    /// Artifact `a` of `peer` as a verified part in `parts_dir`.
    pub async fn fetch(
        &self,
        peer: &PeerConfig,
        a: &Artifact,
        parts_dir: &Path,
    ) -> Result<PathBuf, PeerError> {
        let name = part_name(a)
            .ok_or_else(|| PeerError::BadResponse("an artifact this node cannot name".into()))?;
        let slot = self.slot(&peer.name);
        let _turn = slot.lock().await;
        tokio::fs::create_dir_all(parts_dir).await?;
        drop_older_parts(parts_dir, a, &name).await;
        let part = parts_dir.join(&name);
        let mut have = tokio::fs::metadata(&part)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        if have > a.size {
            tokio::fs::remove_file(&part).await?;
            have = 0;
        }
        if have < a.size {
            self.download(peer, a, &part, have).await?;
        }
        let got = sha256_file(&part, 0).await?;
        if got != a.sha256 {
            let _ = tokio::fs::remove_file(&part).await;
            self.forget_catalog(&peer.name);
            return Err(PeerError::ShaMismatch {
                expected: a.sha256.clone(),
                got,
            });
        }
        Ok(part)
    }

    async fn download(
        &self,
        peer: &PeerConfig,
        a: &Artifact,
        part: &Path,
        have: u64,
    ) -> Result<(), PeerError> {
        let path = format!("/api/v1/peer/artifact/{}/{}", a.youtube_id, a.kind.as_str());
        let mut req = self.get(peer, &path);
        if have > 0 {
            req = req.header(reqwest::header::RANGE, format!("bytes={have}-"));
        }
        let mut resp = req.send().await.map_err(unreachable_err)?;
        let status = resp.status().as_u16();
        if let Some(e) = status_error(status) {
            return Err(e);
        }
        let resumed = status == 206;
        if resumed {
            let start = resp
                .headers()
                .get(reqwest::header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .and_then(content_range_start);
            if start != Some(have) {
                let _ = tokio::fs::remove_file(part).await;
                return Err(PeerError::BadResponse(format!(
                    "asked from byte {have}, got {start:?}"
                )));
            }
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(resumed)
            .truncate(!resumed)
            .open(part)
            .await?;
        let mut written = if resumed { have } else { 0 };
        while let Some(chunk) = resp.chunk().await.map_err(unreachable_err)? {
            written += chunk.len() as u64;
            if written > a.size {
                drop(file);
                let _ = tokio::fs::remove_file(part).await;
                return Err(PeerError::BadResponse(format!(
                    "more than the catalog's {} bytes",
                    a.size
                )));
            }
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        if written != a.size {
            return Err(PeerError::BadResponse(format!(
                "{written} of {} bytes (the part is kept; the next attempt resumes)",
                a.size
            )));
        }
        info!(
            peer = %peer.name,
            youtube_id = %a.youtube_id,
            kind = a.kind.as_str(),
            resumed_from = if resumed { have } else { 0 },
            bytes = a.size,
            "exchange: fetched an artifact"
        );
        Ok(())
    }
}

/// Every other part of the same video and kind (an older copy's) is dropped.
async fn drop_older_parts(dir: &Path, a: &Artifact, keep: &str) {
    let prefix = format!("{}_{}_", a.youtube_id, a.kind.as_str());
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(&prefix) && name.ends_with(".part") && name != keep {
            let _ = tokio::fs::remove_file(entry.path()).await;
        }
    }
}

impl Exchange {
    /// Where fetched artifacts wait as parts.
    pub(crate) fn parts_dir(&self) -> PathBuf {
        self.cache_dir.join("peer")
    }

    /// Artifact `a` of `peer`, a verified part; refused while transfers are
    /// paused (a transfer already running finishes).
    pub async fn fetch(&self, peer: &PeerConfig, a: &Artifact) -> Result<PathBuf, PeerError> {
        if self.transfers_paused().await {
            return Err(PeerError::Paused);
        }
        self.client.fetch(peer, a, &self.parts_dir()).await
    }
}

#[cfg(test)]
#[path = "fetch_tests.rs"]
mod tests;
