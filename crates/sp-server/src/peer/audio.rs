//! #229: a peer's stems and lyrics are made from the PEER's audio, so this
//! node takes them only when its own audio is that very audio
//! (`decide::same_audio`): it fetched its video's audio from that peer at
//! the sha the peer's catalog lists now (`peer_fetches`) and the row's audio
//! file has that audio's size, or its own hash of the row's audio is that
//! sha (`peer_hashes`). Else the stems are separated, and the lyrics
//! processed, here: stems or line timings made from another encode (a song
//! this node downloaded itself, a different YouTube format) would drift
//! against the audio this node plays.
//!
//! The hash is this node's `peer_hashes` entry while it still holds, else
//! taken now at the hasher's rate and stored as the hasher would: a node
//! that does not serve (PP in phase 1) runs no hasher, and the audio phase 0
//! copied from SNV carries no fetch record. When this node cannot tell yet
//! (the peer lists no audio of the video now, a rename there not hashed
//! again; or no audio of the row is on disk here) the job waits like a
//! failed fetch, within the same 2 h bound. While this node's transfers are
//! paused nothing is read or hashed: the fetch would be refused anyway, so
//! the job waits out the pause as a refused fetch does.

use std::path::Path;

use tracing::{info, warn};

use super::Exchange;
use super::ask::{FetchPlan, PeerStep};
use super::client::PeerError;
use super::decide::{OwnAudio, same_audio};
use super::hasher::{HASH_BYTES_PER_S, hash_unchanged, stat};
use super::kind::{ArtifactKind, Job};
use crate::db::models_peer;

/// A row's audio file on disk: its size and this node's hash of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RowAudio {
    pub(crate) size: u64,
    pub(crate) hashed: Option<String>,
}

impl Exchange {
    /// Before a hook takes `plan`'s stems or lyrics (`job`) for row
    /// `video_id`: `None` = this node's audio IS the audio the peer lists
    /// now, take them. Else the step the hook returns instead: the job runs
    /// here (another audio, INFO), or waits, bounded like a failed fetch
    /// (this node's pause, or it cannot tell yet).
    pub(crate) async fn unless_peers_audio(
        &self,
        job: Job,
        plan: &FetchPlan,
        video_id: i64,
        youtube_id: &str,
    ) -> Option<PeerStep> {
        let peer = plan.peer.name.as_str();
        if self.transfers_paused().await {
            return Some(
                self.not_now(job, video_id, youtube_id, peer, PeerError::Paused)
                    .await,
            );
        }
        let Some(listed) = plan.peer_audio.as_ref() else {
            let why = PeerError::NotYet("the peer lists no audio of the video now".into());
            return Some(self.not_now(job, video_id, youtube_id, peer, why).await);
        };
        let Some(row) = self.row_audio(video_id).await else {
            let why = PeerError::NotYet("no audio of the row is on disk here".into());
            return Some(self.not_now(job, video_id, youtube_id, peer, why).await);
        };
        let fetched =
            models_peer::fetch_record(&self.pool, youtube_id, ArtifactKind::Audio.as_str())
                .await
                .inspect_err(
                    |e| warn!(youtube_id, %e, "exchange: reading the audio's origin failed"),
                )
                .ok()
                .flatten();
        let own = OwnAudio {
            fetched: fetched
                .as_ref()
                .map(|(node, _, sha)| (node.as_str(), sha.as_str())),
            size: row.size,
            hashed: row.hashed.as_deref(),
        };
        if same_audio(peer, listed, own) {
            return None;
        }
        info!(
            youtube_id,
            job = job.as_str(),
            peer,
            "exchange: the peer's copy is made from another audio than this node's - processing here"
        );
        Some(PeerStep::Local(Some(self.run_here(job, youtube_id).await)))
    }

    /// The peer's copy is not taken now (`why`): the job waits like a
    /// refused fetch (`after_failed_fetch`: no attempt, counted against the
    /// 2 h bound; this node's own pause rechecks every 5 min, never gives
    /// up).
    async fn not_now(
        &self,
        job: Job,
        video_id: i64,
        youtube_id: &str,
        peer: &str,
        why: PeerError,
    ) -> PeerStep {
        self.after_failed_fetch(job, video_id, youtube_id, peer, &why)
            .await
    }

    /// Row `video_id`'s current audio file: its size and this node's hash
    /// of it (its `peer_hashes` entry while that holds, `HashEntry::holds`,
    /// else hashed now). `None` for a row with no audio, or one not on disk.
    pub(crate) async fn row_audio(&self, video_id: i64) -> Option<RowAudio> {
        let audio: Option<Option<String>> =
            sqlx::query_scalar("SELECT audio_file_path FROM videos WHERE id = ?")
                .bind(video_id)
                .fetch_optional(&self.pool)
                .await
                .inspect_err(|e| warn!(video_id, %e, "exchange: reading the row's audio failed"))
                .ok()
                .flatten();
        let audio = audio.flatten()?;
        let (size, mtime_ms) = stat(Path::new(&audio)).await?;
        let stored = models_peer::hash_of(&self.pool, &audio)
            .await
            .inspect_err(|e| warn!(video_id, %e, "exchange: reading the audio's hash failed"))
            .ok()
            .flatten()
            .filter(|entry| entry.holds(size, mtime_ms));
        let hashed = match stored {
            Some(entry) => Some(entry.sha256),
            None => self.hash_now(Path::new(&audio), size, mtime_ms).await,
        };
        Some(RowAudio {
            size: u64::try_from(size).ok()?,
            hashed,
        })
    }

    /// Hash `audio` (found at `size` / `mtime_ms`) now, at the hasher's
    /// rate, and store the entry as the hasher would. `None` when it changed
    /// meanwhile or cannot be read.
    async fn hash_now(&self, audio: &Path, size: i64, mtime_ms: i64) -> Option<String> {
        let entry = hash_unchanged(audio, size, mtime_ms, HASH_BYTES_PER_S)
            .await
            .inspect_err(
                |e| warn!(audio = %audio.display(), %e, "exchange: hashing the audio failed"),
            )
            .ok()
            .flatten()?;
        if let Err(e) = models_peer::put_hash(&self.pool, &entry).await {
            warn!(audio = %audio.display(), %e, "exchange: storing the audio's hash failed");
        }
        Some(entry.sha256)
    }
}

#[cfg(test)]
#[path = "audio_tests.rs"]
mod tests;
