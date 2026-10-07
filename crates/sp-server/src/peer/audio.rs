//! #229: a peer's stems and lyrics are made from the PEER's audio, so this
//! node takes them only when its own audio is that very audio
//! (`decide::same_audio`): it fetched its audio from that peer at the sha
//! the peer's catalog lists now (`peer_fetches`), or its own hash of the
//! row's audio is that sha (`peer_hashes`, while the file still has its
//! hashed size and mtime). Else the stems are separated, and the lyrics
//! processed, here: stems or line timings made from another encode (a song
//! this node downloaded itself, a different YouTube format) would drift
//! against the audio this node plays. PP hashes nothing while it does not
//! serve (phase 1), so there the fetch record is what counts.

use std::path::Path;

use tracing::{info, warn};

use super::Exchange;
use super::ask::{FetchPlan, PeerStep};
use super::decide::same_audio;
use super::hasher::stat;
use super::kind::{ArtifactKind, Job};
use crate::db::models_peer;

impl Exchange {
    /// Row `video_id`'s audio IS the audio `plan`'s peer lists now: that
    /// peer's stems and lyrics of `youtube_id` fit this node.
    pub(crate) async fn has_peers_audio(
        &self,
        plan: &FetchPlan,
        video_id: i64,
        youtube_id: &str,
    ) -> bool {
        let fetched =
            models_peer::fetch_record(&self.pool, youtube_id, ArtifactKind::Audio.as_str())
                .await
                .inspect_err(
                    |e| warn!(youtube_id, %e, "exchange: reading the audio's origin failed"),
                )
                .ok()
                .flatten();
        let hashed = self.audio_hash(video_id).await;
        same_audio(
            &plan.peer.name,
            plan.peer_audio.as_deref(),
            fetched
                .as_ref()
                .map(|(node, _, sha)| (node.as_str(), sha.as_str())),
            hashed.as_deref(),
        )
    }

    /// `job` of `youtube_id` runs here: `peer` made its copy from another
    /// audio than this node's.
    pub(crate) async fn run_here_on_own_audio(
        &self,
        job: Job,
        youtube_id: &str,
        peer: &str,
    ) -> PeerStep {
        info!(
            youtube_id,
            job = job.as_str(),
            peer,
            "exchange: the peer's copy is made from another audio than this node's - processing here"
        );
        PeerStep::Local(Some(self.run_here(job, youtube_id).await))
    }

    /// This node's sha256 of row `video_id`'s current audio: its
    /// `peer_hashes` entry while the file still has the size and mtime it
    /// was hashed at (`HashEntry::holds`); `None` without one.
    async fn audio_hash(&self, video_id: i64) -> Option<String> {
        let audio: Option<Option<String>> =
            sqlx::query_scalar("SELECT audio_file_path FROM videos WHERE id = ?")
                .bind(video_id)
                .fetch_optional(&self.pool)
                .await
                .inspect_err(|e| warn!(video_id, %e, "exchange: reading the row's audio failed"))
                .ok()
                .flatten();
        let audio = audio.flatten()?;
        let entry = models_peer::hash_of(&self.pool, &audio)
            .await
            .inspect_err(|e| warn!(video_id, %e, "exchange: reading the audio's hash failed"))
            .ok()
            .flatten()?;
        let (size, mtime_ms) = stat(Path::new(&audio)).await?;
        entry.holds(size, mtime_ms).then_some(entry.sha256)
    }
}

#[cfg(test)]
#[path = "audio_tests.rs"]
mod tests;
