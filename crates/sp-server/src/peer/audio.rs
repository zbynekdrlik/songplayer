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
//! The fetch record is read first: it reads no file. Only when it does not
//! vouch is the row's audio hashed: its `peer_hashes` entry while that still
//! holds, else hashed now at the hasher's rate and stored as the hasher
//! would (a node that does not serve, PP in phase 1, runs no hasher, and the
//! audio phase 0 copied from SNV carries no fetch record). When this node
//! cannot tell yet (the peer lists no audio of the video now, a rename there
//! not hashed again; no audio of the row is on disk here; or its audio
//! cannot be hashed now) the job waits like a failed fetch, within the same
//! 2 h bound. While this node's
//! transfers are paused nothing is read or hashed: the fetch would be
//! refused anyway, so the job waits out the pause as a refused fetch does.

use std::path::Path;

use tracing::{info, warn};

use super::Exchange;
use super::ask::{FetchPlan, PeerStep};
use super::client::PeerError;
use super::decide::{OwnAudio, same_audio};
use super::hasher::{HASH_BYTES_PER_S, hash_unchanged, stat};
use super::kind::{ArtifactKind, Job};
use crate::db::models_peer;

/// Whether a peer's stems or lyrics fit this node's audio
/// ([`Exchange::audio_verdict`]).
#[derive(Debug)]
pub(crate) enum AudioVerdict {
    /// This node's audio IS the audio the peer lists now: take them.
    Same,
    /// Another audio: made here instead.
    Other,
    /// Not now (`why`): this node's transfers are paused, or it cannot tell
    /// yet.
    NotNow(PeerError),
}

/// A row's audio file on disk now: its path as the row records it, its
/// size and its mtime (`hasher::stat`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct RowAudio {
    path: String,
    size: i64,
    mtime_ms: i64,
}

impl Exchange {
    /// Before a hook takes `plan`'s stems or lyrics (`job`) for row
    /// `video_id`: `None` = this node's audio IS the audio the peer lists
    /// now, take them. Else the step the hook returns instead: the job runs
    /// here (another audio, INFO), or waits like a failed fetch: this node's
    /// pause (5 min rechecks, never gives up), or it cannot tell yet (within
    /// the 2 h bound).
    pub(crate) async fn unless_peers_audio(
        &self,
        job: Job,
        plan: &FetchPlan,
        video_id: i64,
        youtube_id: &str,
    ) -> Option<PeerStep> {
        let peer = plan.peer.name.as_str();
        match self.audio_verdict(plan, video_id, youtube_id).await {
            AudioVerdict::Same => None,
            AudioVerdict::NotNow(why) => {
                Some(self.not_now(job, video_id, youtube_id, peer, why).await)
            }
            AudioVerdict::Other => {
                info!(
                    youtube_id,
                    job = job.as_str(),
                    peer,
                    "exchange: the peer's copy is made from another audio than this node's - processing here"
                );
                Some(PeerStep::Local(Some(self.run_here(job, youtube_id).await)))
            }
        }
    }

    /// Whether `plan`'s stems or lyrics fit row `video_id`'s audio (the
    /// module doc), with no side effect but an on-demand hash it stores: the
    /// hooks (`unless_peers_audio`) and a stand-in's supersede
    /// (`peer::standin`) act on it. This node's own pause is checked first:
    /// nothing is read or hashed then.
    pub(crate) async fn audio_verdict(
        &self,
        plan: &FetchPlan,
        video_id: i64,
        youtube_id: &str,
    ) -> AudioVerdict {
        let peer = plan.peer.name.as_str();
        if self.transfers_paused().await {
            return AudioVerdict::NotNow(PeerError::Paused);
        }
        let Some(listed) = plan.peer_audio.as_ref() else {
            let why = PeerError::NotYet("the peer lists no audio of the video now".into());
            return AudioVerdict::NotNow(why);
        };
        let Some(row) = self.row_audio(video_id).await else {
            let why = PeerError::NotYet("no audio of the row is on disk here".into());
            return AudioVerdict::NotNow(why);
        };
        let fetched =
            models_peer::fetch_record(&self.pool, youtube_id, ArtifactKind::Audio.as_str())
                .await
                .inspect_err(
                    |e| warn!(youtube_id, %e, "exchange: reading the audio's origin failed"),
                )
                .ok()
                .flatten();
        let mut own = OwnAudio {
            fetched: fetched
                .as_ref()
                .map(|(node, _, sha)| (node.as_str(), sha.as_str())),
            size: u64::try_from(row.size).unwrap_or_default(),
            hashed: None,
        };
        // The record reads no file: the audio is hashed only when it does not
        // vouch for it. A hash that cannot be taken now (the file changed
        // meanwhile, a rename here; or it cannot be read) is no verdict.
        let hashed = if same_audio(peer, listed, own) {
            None
        } else {
            let Some(sha) = self.audio_sha(&row).await else {
                let why = PeerError::NotYet("this node's audio could not be hashed now".into());
                return AudioVerdict::NotNow(why);
            };
            Some(sha)
        };
        own.hashed = hashed.as_deref();
        if same_audio(peer, listed, own) {
            AudioVerdict::Same
        } else {
            AudioVerdict::Other
        }
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

    /// Row `video_id`'s current audio file on disk; `None` for a row with no
    /// audio, or one not on disk.
    async fn row_audio(&self, video_id: i64) -> Option<RowAudio> {
        let audio: Option<Option<String>> =
            sqlx::query_scalar("SELECT audio_file_path FROM videos WHERE id = ?")
                .bind(video_id)
                .fetch_optional(&self.pool)
                .await
                .inspect_err(|e| warn!(video_id, %e, "exchange: reading the row's audio failed"))
                .ok()
                .flatten();
        let path = audio.flatten()?;
        let (size, mtime_ms) = stat(Path::new(&path)).await?;
        Some(RowAudio {
            path,
            size,
            mtime_ms,
        })
    }

    /// This node's sha256 of `row`'s audio: its `peer_hashes` entry while
    /// that still holds (`HashEntry::holds`), else hashed now at the
    /// hasher's rate and stored as the hasher would. `None` when the file
    /// changed meanwhile or cannot be read.
    async fn audio_sha(&self, row: &RowAudio) -> Option<String> {
        let stored = models_peer::hash_of(&self.pool, &row.path)
            .await
            .inspect_err(
                |e| warn!(audio = %row.path, %e, "exchange: reading the audio's hash failed"),
            )
            .ok()
            .flatten()
            .filter(|entry| entry.holds(row.size, row.mtime_ms));
        if let Some(entry) = stored {
            return Some(entry.sha256);
        }
        let entry = hash_unchanged(
            Path::new(&row.path),
            row.size,
            row.mtime_ms,
            HASH_BYTES_PER_S,
        )
        .await
        .inspect_err(|e| warn!(audio = %row.path, %e, "exchange: hashing the audio failed"))
        .ok()
        .flatten()?;
        if let Err(e) = models_peer::put_hash(&self.pool, &entry).await {
            warn!(audio = %row.path, %e, "exchange: storing the audio's hash failed");
        }
        Some(entry.sha256)
    }
}

#[cfg(test)]
#[path = "audio_tests.rs"]
mod tests;
