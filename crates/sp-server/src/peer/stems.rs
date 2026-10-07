//! #229: the stems job asks first. A peer's two stems are fetched as parts,
//! then placed under THIS node's audio name as the row records it AFTER the
//! transfer — read under `cache::SONG_FILES`, the lock a rename holds (#136,
//! `.claude/rules/song-files.md`) — and marked done. Only when this node's
//! audio IS the peer's (`peer::audio`): stems separated from another encode
//! are separated here instead. Nothing is transferred for a song with no
//! audio on disk here: `song_input::job_input` defers it with no attempt, as
//! for a local separation. The stem worker asks before its venv check (the
//! plan's decisions): a node with no lyrics venv still takes a peer's stems.

use std::path::PathBuf;
use std::sync::Arc;

use super::Exchange;
use super::ask::{Ask, FetchPlan, PeerStep};
use super::client::PeerError;
use super::kind::{ArtifactKind, Job};
use crate::db::models_stems::{StemJob, mark_stems_done};
use crate::downloader::cache::SONG_FILES;
use crate::song_input::{HeavyJob, job_input};

/// The stem worker's hook: fetch, wait or separate here.
pub async fn first(ex: Option<&Arc<Exchange>>, job: &StemJob) -> PeerStep {
    let Some(ex) = ex else {
        return PeerStep::Local(None);
    };
    let kind = Job::Stems;
    match ex.ask(kind, &job.youtube_id).await {
        Ask::Local(guard) => PeerStep::Local(Some(guard)),
        Ask::Wait { recheck, .. } => ex.defer(kind, job.video_id, recheck).await,
        Ask::Fetch(plan) => {
            if !ex
                .has_peers_audio(&plan, job.video_id, &job.youtube_id)
                .await
            {
                return ex
                    .run_here_on_own_audio(kind, &job.youtube_id, &plan.peer.name)
                    .await;
            }
            let input = job_input(
                &ex.pool,
                job.video_id,
                &job.audio_file_path,
                HeavyJob::Stems,
            )
            .await;
            if input.is_none() {
                return PeerStep::Deferred;
            }
            match adopt(ex, job, &plan).await {
                Ok(()) => {
                    ex.fetched(kind, &job.youtube_id, &plan.peer.name, &plan.artifacts)
                        .await;
                    PeerStep::Done
                }
                Err(e) => {
                    ex.after_failed_fetch(kind, job.video_id, &job.youtube_id, &plan.peer.name, &e)
                        .await
                }
            }
        }
    }
}

/// The peer's stems under the row's CURRENT audio name, done.
pub(crate) async fn adopt(ex: &Exchange, job: &StemJob, plan: &FetchPlan) -> Result<(), PeerError> {
    let vocals_part = ex
        .fetch(&plan.peer, plan.artifact(ArtifactKind::StemVocals)?)
        .await?;
    let instrumental_part = ex
        .fetch(&plan.peer, plan.artifact(ArtifactKind::StemInstrumental)?)
        .await?;
    let _files = SONG_FILES.lock().await;
    let audio: Option<Option<String>> =
        sqlx::query_scalar("SELECT audio_file_path FROM videos WHERE id = ?")
            .bind(job.video_id)
            .fetch_optional(&ex.pool)
            .await?;
    let audio = audio
        .flatten()
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .ok_or_else(|| PeerError::Io("the song has no audio on disk here".into()))?;
    let (vocals, instrumental) = crate::stems::stem_paths(&audio);
    tokio::fs::rename(&vocals_part, &vocals).await?;
    tokio::fs::rename(&instrumental_part, &instrumental).await?;
    mark_stems_done(
        &ex.pool,
        job.video_id,
        &vocals.to_string_lossy(),
        &instrumental.to_string_lossy(),
    )
    .await?;
    Ok(())
}

#[cfg(test)]
#[path = "stems_tests.rs"]
mod tests;
