//! #304: a job that may not run here (#229 item C: the lyrics while this
//! node's paid AI is off) and whose peer copy is made from another audio
//! than this node's (`peer::audio`: a song this node downloaded itself) was
//! held for good: nothing ever made this node's audio the peer's. Now this
//! node takes the PEER'S SONG first — its video + audio, the `download`
//! kind's pair (`decide::holds(Job::Download)`) whose audio is the one the
//! copy was made from — in place of its own, so the song here IS the
//! peer's and the copy is then taken by the existing rule, in the same pick.
//!
//! The pair is fetched as sha256-checked parts (`Exchange::fetch`: the
//! transfer slot, a Range resume, nothing transferred twice), then swapped
//! under `cache::SONG_FILES` over the names the row records: the names do
//! not change (the title stays), so every file named after the audio keeps
//! its name and no rename / re-link of the set is needed. The old files are
//! kept as `<name>.peer-old` links until the placed files are verified (their
//! sizes, the catalog's) and are put back on any failure (a name a player
//! holds: the parts stay for the next pick, no transfer). Then the pair's
//! origin is recorded (`peer_fetches`, as a fetched download records it) with
//! the audio's hash (`peer_hashes`), so the audio guard vouches for it from
//! now on: the swap never repeats for the same audio, only after the peer
//! itself replaced its song (one transfer per change). This node's stems of
//! the OLD audio are deleted and the rows that recorded them asked again
//! (`requeue_lost_stems`): the stems job then takes the peer's (the same
//! audio now) or separates the new audio here. Every failure is the hold it
//! was before (30 min, no attempt).

use std::path::{Path, PathBuf};

use tracing::{debug, info, warn};

use super::Exchange;
use super::ask::{FetchPlan, PeerStep};
use super::catalog::path_key;
use super::client::PeerError;
use super::decide::holds;
use super::hasher::stat;
use super::kind::{ArtifactKind, Job};
use super::wire::{Artifact, now_ms};
use crate::db::models_peer::{self, HashEntry};
use crate::downloader::cache::SONG_FILES;

/// The suffix of an old song file kept while the peer's takes its name.
const OLD_SUFFIX: &str = ".peer-old";

/// A song's video and audio files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Pair {
    pub(crate) video: PathBuf,
    pub(crate) audio: PathBuf,
}

/// The peer's video and audio artifacts of a song.
#[derive(Debug, Clone)]
struct PeerPair {
    video: Artifact,
    audio: Artifact,
}

/// `file`'s name while the peer's file takes it: `<name>.peer-old`.
pub(crate) fn old_path(file: &Path) -> PathBuf {
    let mut name = file.as_os_str().to_os_string();
    name.push(OLD_SUFFIX);
    PathBuf::from(name)
}

impl Exchange {
    /// Before `job` of row `video_id` is held because `plan`'s copy is made
    /// from another audio (the module doc): `None` = the song here is the
    /// peer's now, take the copy; else the step the hook returns (held).
    pub(crate) async fn take_peers_song(
        &self,
        job: Job,
        plan: &FetchPlan,
        video_id: i64,
        youtube_id: &str,
    ) -> Option<PeerStep> {
        let peer = plan.peer.name.as_str();
        let pair = match self.peers_pair(plan, youtube_id).await {
            Ok(pair) => pair,
            Err(why) => {
                debug!(youtube_id, peer, %why, "exchange: no song of the peer's to take - held");
                return Some(self.hold(job, video_id, youtube_id).await);
            }
        };
        let Some(own) = self.recorded_pair(video_id).await else {
            debug!(
                video_id,
                youtube_id, "exchange: the row records no song pair - held"
            );
            return Some(self.hold(job, video_id, youtube_id).await);
        };
        let parts = match self.fetch_pair(plan, &pair).await {
            Ok(parts) => parts,
            Err(e) => {
                return Some(
                    self.after_failed_fetch(job, video_id, youtube_id, peer, &e)
                        .await,
                );
            }
        };
        if let Err(why) = self
            .swap_song(video_id, youtube_id, peer, &own, &parts, &pair)
            .await
        {
            warn!(
                youtube_id,
                peer,
                why,
                "exchange: the peer's song could not replace this node's - held, asked again later"
            );
            return Some(self.hold(job, video_id, youtube_id).await);
        }
        info!(
            youtube_id,
            job = job.as_str(),
            peer,
            audio_bytes = pair.audio.size,
            video_bytes = pair.video.size,
            "exchange: this node's audio is not the peer's - the peer's video and audio replace it"
        );
        None
    }

    /// The peer's video + audio of `youtube_id` at a version this node
    /// takes (the plan's peer, its cached catalog), whose audio is the one
    /// the plan's copy was made from.
    async fn peers_pair(&self, plan: &FetchPlan, youtube_id: &str) -> Result<PeerPair, PeerError> {
        let catalog = self.client.catalog(&plan.peer).await?;
        let not_yet = |why: &str| PeerError::NotYet(why.into());
        let artifacts = holds(&catalog, Job::Download, youtube_id)
            .ok_or_else(|| not_yet("the peer lists no video and audio of the song"))?;
        let find = |kind| artifacts.iter().find(|a| a.kind == kind).cloned();
        let (Some(video), Some(audio)) = (find(ArtifactKind::Video), find(ArtifactKind::Audio))
        else {
            return Err(not_yet("the peer lists no video and audio of the song"));
        };
        let made_from = plan.peer_audio.as_ref().map(|a| a.sha256.as_str());
        if made_from != Some(audio.sha256.as_str()) {
            return Err(not_yet(
                "the peer's audio is not the one its copy was made from",
            ));
        }
        Ok(PeerPair { video, audio })
    }

    /// Row `video_id`'s recorded video and audio, both named.
    async fn recorded_pair(&self, video_id: i64) -> Option<Pair> {
        let row: Option<(Option<String>, Option<String>)> =
            sqlx::query_as("SELECT file_path, audio_file_path FROM videos WHERE id = ?")
                .bind(video_id)
                .fetch_optional(&self.pool)
                .await
                .inspect_err(|e| warn!(video_id, %e, "exchange: reading the row's files failed"))
                .ok()
                .flatten();
        let (Some(video), Some(audio)) = row? else {
            return None;
        };
        (!video.is_empty() && !audio.is_empty()).then(|| Pair {
            video: PathBuf::from(video),
            audio: PathBuf::from(audio),
        })
    }

    /// Both artifacts as verified parts.
    async fn fetch_pair(&self, plan: &FetchPlan, pair: &PeerPair) -> Result<Pair, PeerError> {
        let video = self.fetch(&plan.peer, &pair.video).await?;
        let audio = self.fetch(&plan.peer, &pair.audio).await?;
        Ok(Pair { video, audio })
    }

    /// Under `SONG_FILES`: the row must still record `own`; the parts take
    /// its names ([`replace_pair`]), then the origin, the audio's hash, the
    /// format and the old audio's stems are dealt with (the module doc).
    async fn swap_song(
        &self,
        video_id: i64,
        youtube_id: &str,
        peer: &str,
        own: &Pair,
        parts: &Pair,
        pair: &PeerPair,
    ) -> Result<(), String> {
        let _files = SONG_FILES.lock().await;
        if self.recorded_pair(video_id).await.as_ref() != Some(own) {
            return Err("the row's files changed meanwhile".into());
        }
        replace_pair(own, parts, (pair.video.size, pair.audio.size))?;
        self.record_origins(youtube_id, peer, &[pair.video.clone(), pair.audio.clone()])
            .await;
        self.store_audio_hash(&own.audio, &pair.audio.sha256).await;
        // #223 S9b: the files are the peer's; the format recorded for this
        // node's own download no longer describes them.
        if let Err(e) = crate::downloader::format::record(&self.pool, video_id, None).await {
            warn!(youtube_id, %e, "exchange: forgetting the replaced files' video format failed");
        }
        self.drop_old_stems(youtube_id, &own.audio).await;
        Ok(())
    }

    /// The placed audio's hash is the catalog's (the part was checked
    /// against it): stored as the hasher would, so the audio guard reads it.
    async fn store_audio_hash(&self, audio: &Path, sha256: &str) {
        let Some((size, mtime_ms)) = stat(audio).await else {
            warn!(audio = %audio.display(), "exchange: the placed audio could not be read");
            return;
        };
        let entry = HashEntry {
            path: path_key(audio),
            size,
            mtime_ms,
            sha256: sha256.to_string(),
            hashed_at_ms: now_ms(),
        };
        if let Err(e) = models_peer::put_hash(&self.pool, &entry).await {
            warn!(audio = %audio.display(), %e, "exchange: storing the audio's hash failed");
        }
    }

    /// The stems named after `audio` were separated from the audio it
    /// replaced: deleted, their peer records forgotten, and every row of
    /// `youtube_id` that recorded them asked again.
    async fn drop_old_stems(&self, youtube_id: &str, audio: &Path) {
        let (vocals, instrumental) = crate::stems::stem_paths(audio);
        for stem in [&vocals, &instrumental] {
            remove_quietly(stem);
        }
        let stems = [ArtifactKind::StemVocals, ArtifactKind::StemInstrumental];
        if let Err(e) = models_peer::forget_fetches(&self.pool, youtube_id, &stems).await {
            warn!(youtube_id, %e, "exchange: forgetting a peer's stems failed");
        }
        let rows: Vec<i64> = sqlx::query_scalar(
            "SELECT id FROM videos WHERE youtube_id = ? AND audio_file_path = ? \
             AND (stem_status = 'done' OR vocals_file_path IS NOT NULL \
                  OR instrumental_file_path IS NOT NULL)",
        )
        .bind(youtube_id)
        .bind(path_key(audio))
        .fetch_all(&self.pool)
        .await
        .inspect_err(|e| warn!(youtube_id, %e, "exchange: reading the rows' stems failed"))
        .unwrap_or_default();
        for id in rows {
            if let Err(e) = crate::db::models_stems::requeue_lost_stems(&self.pool, id).await {
                warn!(video_id = id, %e, "exchange: asking for the stems again failed");
            }
        }
    }
}

/// The file half of the swap: each of `own` linked as `<name>.peer-old`,
/// `parts` renamed over `own`, the placed files' sizes checked against
/// `sizes` (video, audio), then the links removed. On any failure the old
/// files are back under their names (a placed video goes back into its
/// part, so the next pick transfers nothing) and the reason is returned.
pub(crate) fn replace_pair(own: &Pair, parts: &Pair, sizes: (u64, u64)) -> Result<(), String> {
    let old = Pair {
        video: old_path(&own.video),
        audio: old_path(&own.audio),
    };
    link_old(&own.video, &old.video)?;
    if let Err(e) = link_old(&own.audio, &old.audio) {
        remove_quietly(&old.video);
        return Err(e);
    }
    let unlink = |old: &Pair| {
        remove_quietly(&old.video);
        remove_quietly(&old.audio);
    };
    if let Err(e) = std::fs::rename(&parts.video, &own.video) {
        unlink(&old);
        return Err(format!("the video's name: {e}"));
    }
    if let Err(e) = std::fs::rename(&parts.audio, &own.audio) {
        let _ = std::fs::rename(&own.video, &parts.video);
        restore(&old.video, &own.video);
        remove_quietly(&old.audio);
        return Err(format!("the audio's name: {e}"));
    }
    let placed = |file: &Path, size: u64| std::fs::metadata(file).is_ok_and(|m| m.len() == size);
    if !(placed(&own.video, sizes.0) && placed(&own.audio, sizes.1)) {
        restore(&old.video, &own.video);
        restore(&old.audio, &own.audio);
        return Err("the placed files are not the catalog's size".into());
    }
    unlink(&old);
    Ok(())
}

/// `file` linked as `old` (a stale `old` removed first).
fn link_old(file: &Path, old: &Path) -> Result<(), String> {
    gone(std::fs::remove_file(old)).map_err(|e| format!("the stale {}: {e}", old.display()))?;
    std::fs::hard_link(file, old).map_err(|e| format!("the link {}: {e}", old.display()))
}

/// The old file back under `file`'s name (WARNed when it cannot be).
fn restore(old: &Path, file: &Path) {
    if let Err(e) = std::fs::rename(old, file) {
        warn!(file = %file.display(), %e, "exchange: an old song file could not be put back");
    }
}

/// `file` removed; a failure WARNed (an old file or link left behind).
fn remove_quietly(file: &Path) {
    if let Err(e) = gone(std::fs::remove_file(file)) {
        warn!(file = %file.display(), %e, "exchange: an old song file was not removed");
    }
}

/// A removal's result where a file that was not there counts as removed.
pub(crate) fn gone(removed: std::io::Result<()>) -> std::io::Result<()> {
    match removed {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[cfg(test)]
#[path = "song_swap_tests.rs"]
mod tests;
