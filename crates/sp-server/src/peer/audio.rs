//! #229: a peer's stems and lyrics are made from the PEER's audio, so this
//! node takes them only when its own audio is that very audio
//! (`decide::same_audio`): it fetched its video's audio from that peer at
//! the sha the peer's catalog lists now (`peer_fetches`) and the row's audio
//! file has that audio's size, or its own hash of the row's audio is that
//! sha (`peer_hashes`, while the file still has its hashed size and mtime).
//! Else the stems are separated, and the lyrics processed, here: stems or
//! line timings made from another encode (a song this node downloaded
//! itself, a different YouTube format) would drift against the audio this
//! node plays. PP hashes nothing while it does not serve (phase 1), so there
//! the fetch record is what counts. A peer that lists no audio of the video
//! right now (a rename there not hashed again yet) is waited for like a
//! failed fetch, within the same 2 h bound.

use std::path::Path;

use tracing::{info, warn};

use super::Exchange;
use super::ask::{FetchPlan, PeerStep};
use super::client::PeerError;
use super::decide::{OwnAudio, same_audio};
use super::hasher::stat;
use super::kind::{ArtifactKind, Job};
use crate::db::models_peer;

/// What this node has of a row's audio: its size on disk now and its own
/// hash while that still holds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RowAudio {
    pub(crate) size: Option<u64>,
    pub(crate) hashed: Option<String>,
}

impl Exchange {
    /// Before a hook takes `plan`'s stems or lyrics (`job`) for row
    /// `video_id`: `None` = this node's audio IS the audio the peer lists
    /// now, take them. Else the step the hook returns instead: the job runs
    /// here (another audio, INFO), or waits, bounded like a failed fetch
    /// (the peer lists no audio of the video now).
    pub(crate) async fn unless_peers_audio(
        &self,
        job: Job,
        plan: &FetchPlan,
        video_id: i64,
        youtube_id: &str,
    ) -> Option<PeerStep> {
        let peer = plan.peer.name.as_str();
        let Some(listed) = plan.peer_audio.as_ref() else {
            let why = PeerError::BadResponse("the peer lists no audio of the video now".into());
            return Some(
                self.after_failed_fetch(job, video_id, youtube_id, peer, &why)
                    .await,
            );
        };
        let fetched =
            models_peer::fetch_record(&self.pool, youtube_id, ArtifactKind::Audio.as_str())
                .await
                .inspect_err(
                    |e| warn!(youtube_id, %e, "exchange: reading the audio's origin failed"),
                )
                .ok()
                .flatten();
        let row = self.row_audio(video_id).await;
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

    /// Row `video_id`'s current audio: its size on disk and this node's
    /// `peer_hashes` sha of it while the file still has the size and mtime
    /// it was hashed at (`HashEntry::holds`). Nothing for a row with no
    /// audio, or one not on disk.
    async fn row_audio(&self, video_id: i64) -> RowAudio {
        let audio: Option<Option<String>> =
            sqlx::query_scalar("SELECT audio_file_path FROM videos WHERE id = ?")
                .bind(video_id)
                .fetch_optional(&self.pool)
                .await
                .inspect_err(|e| warn!(video_id, %e, "exchange: reading the row's audio failed"))
                .ok()
                .flatten();
        let Some(audio) = audio.flatten() else {
            return RowAudio::default();
        };
        let Some((size, mtime_ms)) = stat(Path::new(&audio)).await else {
            return RowAudio::default();
        };
        let hashed = models_peer::hash_of(&self.pool, &audio)
            .await
            .inspect_err(|e| warn!(video_id, %e, "exchange: reading the audio's hash failed"))
            .ok()
            .flatten()
            .filter(|entry| entry.holds(size, mtime_ms))
            .map(|entry| entry.sha256);
        RowAudio {
            size: u64::try_from(size).ok(),
            hashed,
        }
    }
}

#[cfg(test)]
#[path = "audio_tests.rs"]
mod tests;
